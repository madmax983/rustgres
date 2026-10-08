//! Executor: runs the parsed AST against MVCC storage (v0.5).
//!
//! Every statement runs under the engine lock with a snapshot and its
//! transaction's xid. Reads filter row/table versions through
//! `row_visible`/`table_visible`; writes append new row versions (INSERT),
//! mark `xmax` (DELETE), or do both (UPDATE), recording each change in
//! the statement context's write log for undo and WAL.
//!
//! Errors carry a Postgres SQLSTATE code so the server can emit a
//! proper ErrorResponse.
//!
//! v0.2 additions: `+` expression evaluation, `$N` parameter type
//! inference (`infer_param_types`), text-format parameter parsing
//! (`bind_params`), parameter substitution (`subst_params`), and
//! result-column typing for Describe (`describe_columns`).

//! v0.3: transaction control statements (Begin/Commit/...) are parsed into
//! the AST but never reach `execute` — the session layer in server.rs
//! intercepts them. Their match arms below are defensive only.
//!
//! v0.5: MVCC execution. UPDATE/DELETE are new. Write-write conflicts:
//! if a row version visible in our snapshot was deleted/updated by a
//! transaction that committed after our snapshot was taken,
//! REPEATABLE READ and SERIALIZABLE fail with 40001 ("could not serialize
//! access due to concurrent update"), like Postgres. READ COMMITTED
//! takes a fresh snapshot per statement, so the conflicting version is
//! simply not visible and the statement operates on the newest committed
//! data. Concurrent *uncommitted* writes are last-writer-wins (no row
//! locking in v0.5), documented in the README.
//!
//! v0.6: query engine. SELECT is a real pipeline now: FROM sources
//! (tables, derived tables, INNER/LEFT/CROSS joins with ON), general
//! WHERE/ON/HAVING predicates (AND/OR/NOT, comparisons, IS NULL,
//! IN/EXISTS subqueries — correlated subqueries supported), hash-based
//! GROUP BY with aggregates (COUNT/SUM/AVG/MIN/MAX), DISTINCT, ORDER BY
//! (output names, aliases, positions, and — for plain queries —
//! non-projected columns), OFFSET, and SELECT ... FOR UPDATE row locks.
// v1.78 mechanical split: moved verbatim from src/exec.rs (1-38, 61-270).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

#[derive(Debug)]
pub struct ExecError {
    pub code: &'static str,
    pub message: String,
    /// v0.71: optional PG-style DETAIL line, sent as the `D` field of the
    /// ErrorResponse. `None` for the vast majority of errors.
    pub detail: Option<String>,
}

pub(crate) fn exec_err(code: &'static str, message: impl Into<String>) -> ExecError {
    ExecError {
        detail: None,
        code,
        message: message.into(),
    }
}

/// v0.71: error with a PG-style DETAIL line.
pub(crate) fn exec_err_detail(
    code: &'static str,
    message: impl Into<String>,
    detail: impl Into<String>,
) -> ExecError {
    ExecError {
        detail: Some(detail.into()),
        code,
        message: message.into(),
    }
}

/// Convert a SQL-layer validation error into an execution error.
pub(crate) fn sql_err(e: SqlError) -> ExecError {
    // SqlError.code is &'static str; ExecError.code is too, but the borrow
    // checker can't see that — map to a static fallback.
    let _ = e.code;
    exec_err("42601", e.message)
}

/// Per-statement execution context: the snapshot to read from, the
/// acting transaction's xid, its isolation level, and the write log that
/// receives every mutation (for undo and commit-time WAL records).
pub struct StmtCtx<'a> {
    pub snap: &'a Snapshot,
    pub own: u64,
    /// v1.21: xid to stamp on NEW row versions (xmin/xmax) and DDL MVCC
    /// fields. Inside a savepoint scope this is the scope's sub-xid
    /// (PG19's subtransaction xid); otherwise it equals `own`. `own`
    /// (the top-level xid) remains the visibility identity.
    pub write_xid: u64,
    /// v1.21: every xid owned by the transaction (top + sub-xids), for
    /// visibility checks. A transaction must see rows stamped with any
    /// of its sub-xids, even under RepeatableRead where the snapshot
    /// predates the sub-xid allocation.
    pub all_xids: Vec<u64>,
    pub level: IsolationLevel,
    pub writes: &'a mut Vec<WriteOp>,
    /// v0.9: server-assigned session id, for session-local `currval`.
    pub session: u64,
    /// v0.11: authenticated role executing this statement (lowercased).
    /// Owners, grants, and privilege checks all key off this.
    pub role: &'a str,
    /// v0.17: statement runs read-only — sequence advances (nextval,
    /// setval) fail with 25006. Set by the server from the effective
    /// transaction mode.
    pub read_only: bool,
    /// v0.41: session's `default_toast_compression` GUC, consulted by
    /// TOAST for columns without an explicit `COMPRESSION` method.
    /// Set by the server from the session, like `read_only`.
    pub default_toast_compression: crate::storage::ToastCompression,
    /// v1.00: out-of-band messages (e.g. `RAISE NOTICE` from trigger
    /// bodies) collected during execution. The server drains these
    /// after a successful statement and sends them as NoticeResponse
    /// ('N') messages before the completion tag.
    pub notices: Vec<String>,
}

thread_local! {
    /// v1.02: statement-scoped notice sink. `RAISE NOTICE` inside
    /// plpgsql *function* bodies has no `&mut StmtCtx` in reach (the
    /// call chain is `execute -> run_select -> eval_expr ->
    /// call_user_function -> run_func_body -> run_plpgsql_body ->
    /// run_plpgsql_stmts`, which only carries `&mut Q`), so notices
    /// are pushed here and `execute()` drains them into
    /// `StmtCtx.notices` on exit — the existing v1.00 delivery path
    /// (server.rs sends them as NoticeResponse before
    /// CommandComplete). Thread-per-connection makes the thread-local
    /// statement-scoped in practice; nested `execute()` calls (the
    /// COPY TO helper) save/restore it. `None` outside a statement
    /// (unit tests calling the runner directly): notices are dropped.
    pub(crate) static NOTICE_SINK: RefCell<Option<std::rc::Rc<RefCell<Vec<String>>>>> =
        const { RefCell::new(None) };

    /// v1.26: LATERAL namespace markers (PG19 `scanNameSpaceForRefname` /
    /// `check_agglevels_and_constraints`, PG19's parse-time LATERAL
    /// rules). Each entry is `(ns_base, depth)`:
    /// * `ns_base` — index into the current `&[Scope]` chain where the
    ///   lateral namespace starts: the lateral-visible scopes
    ///   (textually-preceding FROM items, incl. the immediate left row)
    ///   plus anything the right side pushes afterwards (its own FROM
    ///   items, nested subquery scopes). A qualified reference resolving
    ///   at or past `ns_base` must name a qualifier that occurs exactly
    ///   once in `scopes[ns_base..]`, else PG19 raises 42P09
    ///   (`table reference "x" is ambiguous`).
    /// * `depth` — `q.depth` when the marker was pushed. Scopes are only
    ///   ever appended while a marker is live, so indices stay valid; a
    ///   marker whose base lies past a rebuilt shorter chain is ignored.
    pub(crate) static LATERAL_NS: RefCell<Vec<(usize, usize)>> =
        const { RefCell::new(Vec::new()) };

    /// v1.26: cross-nest LATERAL inheritance. `eval_cross_nest_join`
    /// evaluates its right subtree once per left row with the
    /// textually-preceding FROM scopes appended to `outer`; a lateral
    /// item nested in that subtree (at the same `q.depth` — reached via
    /// `build_source`, not via a subquery boundary) extends its marker
    /// bases over the trailing `usize` scopes. `None` outside a
    /// cross-nest right subtree. Saved/restored on nesting.
    pub(crate) static LATERAL_INHERIT: RefCell<Option<(usize, usize)>> =
        const { RefCell::new(None) };
}

/// v1.26: pushes a LATERAL namespace marker; pops it on drop, so every
/// return path (including `?`) restores the stack.
pub(crate) struct LateralNsGuard;
impl Drop for LateralNsGuard {
    fn drop(&mut self) {
        LATERAL_NS.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// v1.26: restores the previous `LATERAL_INHERIT` value on drop.
pub(crate) struct LateralInheritGuard {
    pub(crate) prev: Option<(usize, usize)>,
}
impl Drop for LateralInheritGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        LATERAL_INHERIT.with(|s| {
            *s.borrow_mut() = prev;
        });
    }
}

/// v1.26: compute and push the LATERAL namespace marker for a lateral
/// join whose per-row scopes are `outer[..outer_len] +
/// prefix[..prefix_len] + [left row]`. A pending cross-nest inheritance
/// applies only at the same query level (reached via `build_source`,
/// not across a subquery boundary): the enclosing cross-nest join
/// appended its preceding scopes as the *first* `n` entries of this
/// join's `prefix`, and they are lateral-visible here. The inheritance
/// is left in place for sibling lateral items evaluated later in the
/// same right subtree.
pub(crate) fn push_lateral_ns(depth: usize, outer_len: usize, prefix_len: usize) -> LateralNsGuard {
    let inherit = LATERAL_INHERIT.with(|s| {
        let cur = *s.borrow();
        match cur {
            Some((n, d)) if d == depth && n <= prefix_len => cur,
            _ => None,
        }
    });
    // The first `n` prefix scopes are lateral-visible (appended by the
    // enclosing cross-nest join at the same query level); the namespace
    // starts before them.
    let ns_base = match inherit {
        Some((n, _)) => outer_len + prefix_len - n,
        None => outer_len + prefix_len,
    };
    LATERAL_NS.with(|s| {
        s.borrow_mut().push((ns_base, depth));
    });
    LateralNsGuard
}

/// v1.02: push a notice into the statement-scoped sink (no-op when no
/// statement is running).
pub(crate) fn push_notice(msg: String) {
    NOTICE_SINK.with(|s| {
        if let Some(sink) = s.borrow().as_ref() {
            sink.borrow_mut().push(msg);
        }
    });
}

/// Outcome of executing one statement.
#[derive(Debug)]
pub enum ExecResult {
    /// Rows to return: (column name, column type) + row values.
    Select {
        columns: Vec<(String, ColType)>,
        rows: Vec<Row>,
    },
    /// EXPLAIN output: same shape as Select but completes with the
    /// "EXPLAIN" tag, like Postgres.
    Explain {
        columns: Vec<(String, ColType)>,
        rows: Vec<Row>,
    },
    /// Tag for CommandComplete, e.g. "INSERT 0 2".
    Command { tag: String },
    /// v0.10: INSERT/UPDATE/DELETE — always carries the completion tag;
    /// with RETURNING it also carries the result rows (empty otherwise,
    /// behaving exactly like Command on the wire).
    Dml {
        tag: String,
        columns: Vec<(String, ColType)>,
        rows: Vec<Row>,
    },
}
