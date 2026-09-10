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

use crate::sql::{
    AggFunc, CmpOp, Expr, FromItem, InsertValue, IsolationLevel, JoinKind, Literal, SelectItem,
    SelectStmt, Stmt, WhereCond, WhereRhs,
};
use crate::storage::{ColType, Engine, RowVersion, Snapshot, Value, WriteOp, row_visible};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct ExecError {
    pub code: &'static str,
    pub message: String,
}

fn exec_err(code: &'static str, message: impl Into<String>) -> ExecError {
    ExecError {
        code,
        message: message.into(),
    }
}

/// Per-statement execution context: the snapshot to read from, the
/// acting transaction's xid, its isolation level, and the write log that
/// receives every mutation (for undo and commit-time WAL records).
pub struct StmtCtx<'a> {
    pub snap: &'a Snapshot,
    pub own: u64,
    pub level: IsolationLevel,
    pub writes: &'a mut Vec<WriteOp>,
}

/// Outcome of executing one statement.
#[derive(Debug)]
pub enum ExecResult {
    /// Rows to return: (column name, column type) + row values.
    Select {
        columns: Vec<(String, ColType)>,
        rows: Vec<Vec<Value>>,
    },
    /// Tag for CommandComplete, e.g. "INSERT 0 2".
    Command { tag: String },
}

pub fn execute(eng: &mut Engine, ctx: &mut StmtCtx, stmt: &Stmt) -> Result<ExecResult, ExecError> {
    match stmt {
        Stmt::CreateTable { name, columns } => exec_create(eng, ctx, name, columns),
        Stmt::Insert {
            table,
            columns,
            rows,
        } => exec_insert(eng, ctx, table, columns, rows),
        Stmt::Select(sel) => {
            // FOR UPDATE locks (from every query level that asked for
            // them) are collected during the scan and acquired here, while
            // the engine lock is still held — so they are atomic with the
            // read. Never waited on: see try_row_lock.
            let mut lock_ids: Vec<(String, u64)> = Vec::new();
            let out = {
                let mut q = Q {
                    eng,
                    snap: ctx.snap,
                    own: ctx.own,
                    depth: 0,
                    lock_ids: &mut lock_ids,
                };
                run_select(&mut q, sel, &[])?
            };
            if !lock_ids.is_empty() {
                acquire_row_locks(eng, ctx.own, &lock_ids)?;
            }
            Ok(ExecResult::Select {
                columns: out.columns,
                rows: out.rows,
            })
        }
        Stmt::DropTable { if_exists, name } => exec_drop(eng, ctx, name, *if_exists),
        Stmt::Update {
            table,
            sets,
            where_,
        } => exec_update(eng, ctx, table, sets, where_),
        Stmt::Delete { table, where_ } => exec_delete(eng, ctx, table, where_),
        // Transaction control / checkpoint / vacuum never reach the
        // executor (server.rs intercepts them); reaching here is a bug in
        // the session layer.
        Stmt::Begin { .. }
        | Stmt::Commit
        | Stmt::Rollback
        | Stmt::Savepoint { .. }
        | Stmt::RollbackTo { .. }
        | Stmt::Release { .. }
        | Stmt::Checkpoint
        | Stmt::Vacuum { .. } => Err(exec_err(
            "25001",
            "transaction control statements must go through the session",
        )),
    }
}

/// Take every collected FOR UPDATE lock for `own`. A row locked by
/// another *active* transaction fails the whole statement with 40001 —
/// we never block waiting for a lock (documented deviation from Postgres,
/// which would wait).
fn acquire_row_locks(eng: &mut Engine, own: u64, ids: &[(String, u64)]) -> Result<(), ExecError> {
    for (table, id) in ids {
        if let Err(_holder) = eng.try_row_lock(*id, own) {
            return Err(exec_err(
                "40001",
                format!("could not obtain lock on row in relation \"{}\"", table),
            ));
        }
    }
    Ok(())
}

fn exec_create(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    columns: &[(String, ColType)],
) -> Result<ExecResult, ExecError> {
    if eng.db.find_table(name, ctx.snap, ctx.own).is_some() {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    for (i, (col, _)) in columns.iter().enumerate() {
        if columns[..i].iter().any(|(c, _)| c == col) {
            return Err(exec_err(
                "42701",
                format!("column \"{}\" specified more than once", col),
            ));
        }
    }
    // NOTE: a concurrent uncommitted CREATE of the same name is allowed
    // here (it is invisible to us); the commit-time check in server.rs
    // rejects the second committer with 40001.
    eng.db
        .tables
        .entry(name.to_string())
        .or_default()
        .push(crate::storage::Table::new(columns.to_vec(), ctx.own));
    ctx.writes.push(WriteOp::CreateTable {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: "CREATE TABLE".to_string(),
    })
}

/// Coerce an INSERT literal to the target column type.
fn coerce_literal(lit: &Literal, col_type: &ColType, col_name: &str) -> Result<Value, ExecError> {
    match (lit, col_type) {
        (Literal::Null, _) => Ok(Value::Null),
        (Literal::Int(i), ColType::Int) => Ok(Value::Int(*i)),
        (Literal::Int(i), ColType::Float) => Ok(Value::Float(*i as f64)),
        (Literal::Float(f), ColType::Float) => Ok(Value::Float(*f)),
        (Literal::Text(s), ColType::Text) => Ok(Value::Text(s.clone())),
        (Literal::Bool(b), ColType::Bool) => Ok(Value::Bool(*b)),
        _ => Err(exec_err(
            "42804",
            format!(
                "column \"{}\" is of type {} but expression is of type {}",
                col_name,
                col_type.sql_name(),
                lit.type_name()
            ),
        )),
    }
}

/// Coerce an evaluated UPDATE value to the target column type.
fn coerce_value(v: Value, col_type: &ColType, col_name: &str) -> Result<Value, ExecError> {
    match (&v, col_type) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Int(i), ColType::Int) => Ok(Value::Int(*i)),
        (Value::Int(i), ColType::Float) => Ok(Value::Float(*i as f64)),
        (Value::Float(f), ColType::Float) => Ok(Value::Float(*f)),
        (Value::Text(s), ColType::Text) => Ok(Value::Text(s.clone())),
        (Value::Bool(b), ColType::Bool) => Ok(Value::Bool(*b)),
        _ => Err(exec_err(
            "42804",
            format!(
                "column \"{}\" is of type {} but expression is of type {}",
                col_name,
                col_type.sql_name(),
                v.type_name()
            ),
        )),
    }
}

fn exec_insert(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    columns: &Option<Vec<String>>,
    rows: &[Vec<InsertValue>],
) -> Result<ExecResult, ExecError> {
    // Validate everything before mutating (statement atomicity).
    let new_rows: Vec<Vec<Value>> = {
        let t = eng
            .db
            .find_table(table, ctx.snap, ctx.own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let targets: Vec<usize> = match columns {
            Some(names) => names
                .iter()
                .map(|n| {
                    t.column_index(n).ok_or_else(|| {
                        exec_err(
                            "42703",
                            format!("column \"{}\" of relation \"{}\" does not exist", n, table),
                        )
                    })
                })
                .collect::<Result<_, _>>()?,
            None => (0..t.columns.len()).collect(),
        };
        let ncols = t.columns.len();
        let mut built = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != targets.len() {
                return Err(exec_err(
                    "42601",
                    format!(
                        "INSERT has {} expressions but {} target columns",
                        row.len(),
                        targets.len()
                    ),
                ));
            }
            let mut values = vec![Value::Null; ncols];
            for (v, &ci) in row.iter().zip(targets.iter()) {
                let lit = match v {
                    InsertValue::Lit(l) => l,
                    InsertValue::Param(n) => {
                        return Err(exec_err("42P02", format!("there is no parameter ${}", n)));
                    }
                };
                let (cname, ctype) = &t.columns[ci];
                values[ci] = coerce_literal(lit, ctype, cname)?;
            }
            built.push(values);
        }
        built
    };
    // Apply: each row becomes a version owned by this transaction,
    // invisible to everyone else until we commit.
    let n = new_rows.len();
    let mut ids = Vec::with_capacity(n);
    for _ in 0..n {
        ids.push(eng.alloc_row_id());
    }
    let t = eng
        .db
        .find_table_mut(table, ctx.snap, ctx.own)
        .expect("table still visible; engine lock held throughout");
    for (values, id) in new_rows.into_iter().zip(ids.iter()) {
        t.push_version(RowVersion {
            id: *id,
            values,
            xmin: ctx.own,
            xmax: 0,
        });
        ctx.writes.push(WriteOp::InsertRow {
            table: table.to_string(),
            row_id: *id,
        });
    }
    Ok(ExecResult::Command {
        tag: format!("INSERT 0 {}", n),
    })
}

/// `WHERE col = literal` comparison (UPDATE/DELETE only; SELECT uses full
/// predicates since v0.6). NULL never matches (SQL semantics).
/// Returns Err(42883) when the operand types are incompatible, like Postgres.
fn value_matches(value: &Value, lit: &Literal) -> Result<bool, ExecError> {
    match (value, lit) {
        (Value::Null, _) | (_, Literal::Null) => Ok(false),
        (Value::Int(a), Literal::Int(b)) => Ok(a == b),
        (Value::Float(a), Literal::Float(b)) => Ok(a == b),
        (Value::Int(a), Literal::Float(b)) => Ok((*a as f64) == *b),
        (Value::Float(a), Literal::Int(b)) => Ok(*a == (*b as f64)),
        (Value::Text(a), Literal::Text(b)) => Ok(a == b),
        (Value::Bool(a), Literal::Bool(b)) => Ok(a == b),
        _ => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} = {}",
                value.type_name(),
                lit.type_name()
            ),
        )),
    }
}

/// Does this row version satisfy all WHERE conditions? (UPDATE/DELETE.)
fn row_matches_where(
    t: &crate::storage::Table,
    values: &[Value],
    where_: &[WhereCond],
) -> Result<bool, ExecError> {
    row_matches_where_cols(&t.columns, values, where_)
}

fn row_matches_where_cols(
    columns: &[(String, ColType)],
    values: &[Value],
    where_: &[WhereCond],
) -> Result<bool, ExecError> {
    for w in where_ {
        let i = columns
            .iter()
            .position(|(n, _)| n == &w.col)
            .ok_or_else(|| exec_err("42703", format!("column \"{}\" does not exist", w.col)))?;
        let lit = match &w.rhs {
            WhereRhs::Lit(l) => l,
            WhereRhs::Param(n) => {
                return Err(exec_err("42P02", format!("there is no parameter ${}", n)));
            }
        };
        if !value_matches(&values[i], lit)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// v0.6: a row locked by another active transaction (SELECT ... FOR
/// UPDATE) cannot be written by us — fail fast with 40001 instead of
/// blocking, like Postgres' NOWAIT but without the wait option.
fn check_row_lock(eng: &Engine, table: &str, row_id: u64, own: u64) -> Result<(), ExecError> {
    if let Some(h) = eng.row_lock_holder(row_id) {
        if h != own && eng.txns.active.contains(&h) {
            return Err(exec_err(
                "40001",
                format!("could not obtain lock on row in relation \"{}\"", table),
            ));
        }
    }
    Ok(())
}

/// Write-write conflict check for UPDATE/DELETE/DROP: the version was
/// visible in our snapshot, but its `xmax` is now set by another
/// transaction that committed *after* our snapshot was taken. (If it had
/// committed before, the version would not be visible to us at all.)
/// REPEATABLE READ and SERIALIZABLE fail with 40001, like Postgres;
/// READ COMMITTED cannot reach this — its per-statement snapshot already
/// excludes such versions.
fn check_write_conflict(eng: &Engine, xmax: u64, level: IsolationLevel) -> Result<(), ExecError> {
    if xmax == 0 {
        return Ok(());
    }
    if eng.xid_committed(xmax) {
        // The row was deleted/updated by a transaction that committed
        // after our snapshot was taken. READ COMMITTED cannot reach this
        // (its per-statement snapshot already excludes such versions);
        // REPEATABLE READ and SERIALIZABLE fail with 40001, like Postgres.
        return match level {
            IsolationLevel::ReadCommitted => Ok(()),
            _ => Err(exec_err(
                "40001",
                "could not serialize access due to concurrent update",
            )),
        };
    }
    // xmax belongs to a concurrent *uncommitted* transaction: no row
    // locking in v0.5, so this is last-writer-wins (documented).
    Ok(())
}

fn exec_update(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    sets: &[(String, Expr)],
    where_: &[WhereCond],
) -> Result<ExecResult, ExecError> {
    // Plan first (validate + conflict-check), mutate after: a failed
    // UPDATE leaves no trace (statement atomicity).
    let plan: Vec<(u64, u64, Vec<Value>)> = {
        let t = eng
            .db
            .find_table(table, ctx.snap, ctx.own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let set_cols: Vec<usize> = sets
            .iter()
            .map(|(name, _)| {
                t.column_index(name).ok_or_else(|| {
                    exec_err(
                        "42703",
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            name, table
                        ),
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        let columns = t.columns.clone();
        let schema: Vec<QCol> = columns
            .iter()
            .map(|(n, ty)| QCol {
                qual: String::new(),
                name: n.clone(),
                ty: ty.clone(),
            })
            .collect();
        // Copy the visible rows' data out; the borrow of `t` ends here so
        // SET expressions can run against `eng` below.
        let vis: Vec<(u64, u64, Vec<Value>)> = t
            .rows
            .iter()
            .filter(|r| row_visible(r, ctx.snap, ctx.own))
            .map(|r| (r.id, r.xmax, r.values.clone()))
            .collect();
        let mut plan = Vec::new();
        // SET expressions are evaluated with a scratch query context;
        // subqueries in SET correlate to the row being updated.
        for (id, xmax, values) in &vis {
            check_write_conflict(eng, *xmax, ctx.level)?;
            if !row_matches_where_cols(&columns, values, where_)? {
                continue;
            }
            // Only rows we actually write conflict with FOR UPDATE locks —
            // merely scanning a locked row is fine, like Postgres.
            check_row_lock(eng, table, *id, ctx.own)?;
            let mut new_values = values.clone();
            for ((_, expr), &ci) in sets.iter().zip(set_cols.iter()) {
                let v = eval_update_expr(eng, ctx.snap, ctx.own, &schema, values, expr)?;
                let (cname, ctype) = &columns[ci];
                new_values[ci] = coerce_value(v, ctype, cname)?;
            }
            plan.push((*id, *xmax, new_values));
        }
        plan
    };
    // Apply: UPDATE = delete old version + insert new version.
    let n = plan.len();
    let mut new_ids = Vec::with_capacity(n);
    for _ in 0..n {
        new_ids.push(eng.alloc_row_id());
    }
    let t = eng
        .db
        .find_table_mut(table, ctx.snap, ctx.own)
        .expect("table still visible; engine lock held throughout");
    for ((old_id, prev_xmax, new_values), new_id) in plan.into_iter().zip(new_ids) {
        let pos = t
            .row_pos(old_id)
            .expect("row version still present; engine lock held throughout");
        t.rows[pos].xmax = ctx.own;
        ctx.writes.push(WriteOp::DeleteRow {
            table: table.to_string(),
            row_id: old_id,
            prev_xmax,
        });
        t.push_version(RowVersion {
            id: new_id,
            values: new_values,
            xmin: ctx.own,
            xmax: 0,
        });
        ctx.writes.push(WriteOp::InsertRow {
            table: table.to_string(),
            row_id: new_id,
        });
    }
    Ok(ExecResult::Command {
        tag: format!("UPDATE {}", n),
    })
}

fn exec_delete(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    where_: &[WhereCond],
) -> Result<ExecResult, ExecError> {
    // Plan first for statement atomicity (WHERE type errors must not
    // leave half the rows deleted).
    let plan: Vec<(u64, u64)> = {
        let t = eng
            .db
            .find_table(table, ctx.snap, ctx.own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let mut plan = Vec::new();
        for rv in t.rows.iter().filter(|r| row_visible(r, ctx.snap, ctx.own)) {
            check_write_conflict(eng, rv.xmax, ctx.level)?;
            if row_matches_where(t, &rv.values, where_)? {
                // Only rows we actually delete conflict with FOR UPDATE
                // locks — merely scanning a locked row is fine.
                check_row_lock(eng, table, rv.id, ctx.own)?;
                plan.push((rv.id, rv.xmax));
            }
        }
        plan
    };
    let n = plan.len();
    let t = eng
        .db
        .find_table_mut(table, ctx.snap, ctx.own)
        .expect("table still visible; engine lock held throughout");
    for (id, prev_xmax) in plan {
        let pos = t
            .row_pos(id)
            .expect("row version still present; engine lock held throughout");
        t.rows[pos].xmax = ctx.own;
        ctx.writes.push(WriteOp::DeleteRow {
            table: table.to_string(),
            row_id: id,
            prev_xmax,
        });
    }
    Ok(ExecResult::Command {
        tag: format!("DELETE {}", n),
    })
}

fn exec_drop(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    // Find the visible version first (immutable) for the conflict check,
    // then mutate. A DROP of a table dropped by a not-yet-visible
    // transaction behaves like the row case (40001 under RR/SERIALIZABLE).
    let prev_xmax = {
        let t = eng.db.find_table(name, ctx.snap, ctx.own);
        match t {
            None if if_exists => {
                return Ok(ExecResult::Command {
                    tag: "DROP TABLE".to_string(),
                });
            }
            None => {
                return Err(exec_err(
                    "42P01",
                    format!("table \"{}\" does not exist", name),
                ));
            }
            Some(t) => {
                if t.dropped_xmax != 0
                    && t.dropped_xmax != ctx.own
                    && eng.xid_committed(t.dropped_xmax)
                {
                    return Err(exec_err(
                        "40001",
                        "could not serialize access due to concurrent update",
                    ));
                }
                t.dropped_xmax
            }
        }
    };
    let t = eng
        .db
        .find_table_mut(name, ctx.snap, ctx.own)
        .expect("table still visible; engine lock held throughout");
    t.dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropTable {
        name: name.to_string(),
        prev_xmax,
    });
    Ok(ExecResult::Command {
        tag: "DROP TABLE".to_string(),
    })
}

// ---------------------------------------------------------------------------
// v0.6 query engine
// ---------------------------------------------------------------------------

/// One column of a query's working schema: qualifier (table alias, or the
/// table name when there is no alias; "" for no-FROM / output rows),
/// column name, and type.
#[derive(Clone, Debug)]
pub struct QCol {
    pub qual: String,
    pub name: String,
    pub ty: ColType,
}

/// One working row: cell values parallel to the schema, plus provenance —
/// (table name, row-version id) of each contributing base-table row —
/// used by SELECT ... FOR UPDATE.
#[derive(Clone, Debug, Default)]
pub struct QRow {
    pub cells: Vec<Value>,
    pub prov: Vec<(String, u64)>,
}

/// One scope in the column-resolution chain: a FROM source's schema+row,
/// or an outer query's row for correlated subqueries. Scopes are searched
/// innermost-first, like Postgres.
#[derive(Clone, Copy)]
struct Scope<'a> {
    schema: &'a [QCol],
    row: &'a [Value],
}

/// Resolve `qual.name` / `name` across the scope chain (innermost first).
/// Unqualified names fall through to outer scopes (correlation); qualified
/// names must resolve in the innermost scope that contains the qualifier.
fn resolve_col(
    scopes: &[Scope],
    qual: Option<&str>,
    name: &str,
) -> Result<(usize, usize), ExecError> {
    for (si, sc) in scopes.iter().enumerate().rev() {
        match qual {
            Some(q) => {
                if !sc.schema.iter().any(|c| c.qual == q) {
                    continue; // qualifier not in this scope: try outward
                }
                let mut found = None;
                for (ci, c) in sc.schema.iter().enumerate() {
                    if c.qual == q && c.name == name {
                        if found.is_some() {
                            return Err(exec_err(
                                "42702",
                                format!("column reference \"{}.{}\" is ambiguous", q, name),
                            ));
                        }
                        found = Some(ci);
                    }
                }
                return found.map(|ci| (si, ci)).ok_or_else(|| {
                    exec_err("42703", format!("column {}.{} does not exist", q, name))
                });
            }
            None => {
                let mut found = None;
                for (ci, c) in sc.schema.iter().enumerate() {
                    if c.name == name {
                        if found.is_some() {
                            return Err(exec_err(
                                "42702",
                                format!("column reference \"{}\" is ambiguous", name),
                            ));
                        }
                        found = Some(ci);
                    }
                }
                if let Some(ci) = found {
                    return Ok((si, ci));
                }
                // No match here: an outer scope may provide it (correlation).
            }
        }
    }
    Err(exec_err(
        "42703",
        format!("column \"{}\" does not exist", name),
    ))
}

/// Pre-resolve every column reference at this query level of `pred` into a
/// positional `ResolvedCol`, for one fixed scope shape (`schemas`,
/// outermost first). Callgrind showed per-pair name resolution
/// (`resolve_col` + string compares) costing ~20% of all instructions in a
/// join workload — the schemas don't change across the loop, so resolving
/// once up front removes it from the hot path.
///
/// Subqueries are their own query level and are left untouched: their inner
/// references resolve at runtime, still able to correlate against the live
/// outer frames. Errors are identical to per-row evaluation because the
/// same `resolve_col` does the work — against prototype scopes with empty
/// rows, since resolution only inspects schemas. Like per-row evaluation,
/// callers must only resolve when the loop would actually evaluate the
/// predicate (non-empty inputs), so error timing is unchanged.
fn resolve_predicate_columns(pred: &Expr, schemas: &[&[QCol]]) -> Result<Expr, ExecError> {
    // Prototype scopes: resolve_col never touches `row`, only `schema`
    // (expr_type already uses this trick).
    let scopes: Vec<Scope> = schemas
        .iter()
        .map(|s| Scope {
            schema: s,
            row: &[],
        })
        .collect();
    resolve_predicate_columns_in(pred, &scopes)
}

fn resolve_predicate_columns_in(pred: &Expr, scopes: &[Scope]) -> Result<Expr, ExecError> {
    let r = |e: &Expr| resolve_predicate_columns_in(e, scopes);
    match pred {
        Expr::Column { table, name } => {
            let (frame, idx) = resolve_col(scopes, table.as_deref(), name)?;
            Ok(Expr::ResolvedCol { frame, idx })
        }
        // Leaves and already-resolved nodes pass through (idempotent).
        Expr::ResolvedCol { .. } | Expr::Literal(_) | Expr::Param(_) => Ok(pred.clone()),
        // Separate query levels: runtime resolution (correlation intact).
        Expr::ScalarSub(_) | Expr::InSub { .. } | Expr::Exists { .. } => Ok(pred.clone()),
        Expr::Add(a, b) => Ok(Expr::Add(Box::new(r(a)?), Box::new(r(b)?))),
        Expr::Cmp { op, left, right } => Ok(Expr::Cmp {
            op: *op,
            left: Box::new(r(left)?),
            right: Box::new(r(right)?),
        }),
        Expr::And(a, b) => Ok(Expr::And(Box::new(r(a)?), Box::new(r(b)?))),
        Expr::Or(a, b) => Ok(Expr::Or(Box::new(r(a)?), Box::new(r(b)?))),
        Expr::Not(x) => Ok(Expr::Not(Box::new(r(x)?))),
        Expr::IsNull { expr, neg } => Ok(Expr::IsNull {
            expr: Box::new(r(expr)?),
            neg: *neg,
        }),
        // Can't occur in a JOIN ON (rejected by validation), but resolve
        // the argument rather than choke if one ever arrives.
        Expr::Agg { func, arg } => Ok(Expr::Agg {
            func: *func,
            arg: arg.as_ref().map(|a| r(a).map(Box::new)).transpose()?,
        }),
    }
}

/// Per-query evaluation state threaded through the v0.6 engine.
struct Q<'a, 'b> {
    eng: &'a mut Engine,
    snap: &'b Snapshot,
    own: u64,
    /// Subquery nesting depth (0 = top level).
    depth: usize,
    /// Sink for (table, row-version id) pairs named by FOR UPDATE, at any
    /// query level. The top-level `execute` acquires them all at once.
    lock_ids: &'a mut Vec<(String, u64)>,
}

struct SelectOut {
    columns: Vec<(String, ColType)>,
    rows: Vec<Vec<Value>>,
}

/// One projected output row mid-pipeline: projected cells, provenance for
/// FOR UPDATE, and (only when an ORDER BY may need it) the full
/// pre-projection cells.
struct OutRow {
    cells: Vec<Value>,
    prov: Vec<(String, u64)>,
    full: Vec<Value>,
    /// Precomputed ORDER BY keys. Aggregated queries fill this in during
    /// exec_agg, where group-level ORDER BY expressions (aggregates, GROUP
    /// BY columns not in the select list) can still be evaluated; plain
    /// queries compute keys later in apply_order.
    sort_keys: Option<Vec<Value>>,
}

fn run_select(q: &mut Q, stmt: &SelectStmt, outer: &[Scope]) -> Result<SelectOut, ExecError> {
    if let Some(n) = stmt.limit {
        if n < 0 {
            return Err(exec_err("2201W", "LIMIT must not be negative"));
        }
    }
    if let Some(n) = stmt.offset {
        if n < 0 {
            return Err(exec_err("2201W", "OFFSET must not be negative"));
        }
    }
    validate_select(stmt)?;
    // Column metadata first, so names/types are identical between
    // Describe and execution.
    let out_cols = describe_select(&*q.eng, q.snap, q.own, stmt)?;
    let (schema, rows) = build_from(q, outer, &stmt.from, stmt.where_.as_ref(), stmt.for_update)?;
    let rows = apply_where(q, outer, &schema, rows, stmt.where_.as_ref())?;
    let agg = is_agg_query(stmt);
    // Non-aggregated queries with ORDER BY keep the full row around: an
    // ORDER BY term may reference a non-projected column (v0.1 behavior).
    let keep_full = !agg && !stmt.distinct && !stmt.order_by.is_empty();
    let mut orows: Vec<OutRow> = if agg {
        exec_agg(q, outer, stmt, &schema, &rows, &out_cols)?
    } else {
        let mut v = Vec::with_capacity(rows.len());
        for r in rows {
            let full = if keep_full {
                r.cells.clone()
            } else {
                Vec::new()
            };
            // project_row takes the row by value: plain `SELECT *` moves
            // it through with zero copies, and provenance moves rather
            // than cloning.
            let (cells, prov) = project_row(q, outer, stmt, &schema, r)?;
            v.push(OutRow {
                cells,
                prov,
                full,
                sort_keys: None,
            });
        }
        v
    };
    if stmt.distinct {
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        orows.retain(|o| {
            let mut k = Vec::new();
            for v in &o.cells {
                value_key(v, &mut k);
            }
            seen.insert(k)
        });
    }
    if !stmt.order_by.is_empty() {
        apply_order(q, outer, stmt, &schema, &out_cols, &mut orows)?;
    }
    if let Some(n) = stmt.offset {
        let n = (n as usize).min(orows.len());
        orows.drain(..n);
    }
    if let Some(n) = stmt.limit {
        orows.truncate(n as usize);
    }
    if stmt.for_update {
        for o in &orows {
            for (t, id) in &o.prov {
                if !q.lock_ids.iter().any(|(_, x)| x == id) {
                    q.lock_ids.push((t.clone(), *id));
                }
            }
        }
    }
    Ok(SelectOut {
        columns: out_cols,
        rows: orows.into_iter().map(|o| o.cells).collect(),
    })
}

/// Reject query shapes we deliberately do not support, before doing any
/// work. Aggregates are illegal in WHERE / JOIN ON / GROUP BY (42803,
/// like Postgres); FOR UPDATE is illegal with DISTINCT / GROUP BY /
/// aggregates (0A000, like Postgres).
fn validate_select(stmt: &SelectStmt) -> Result<(), ExecError> {
    if let Some(w) = &stmt.where_ {
        if contains_agg(w) {
            return Err(exec_err(
                "42803",
                "aggregates are not allowed in WHERE clause",
            ));
        }
        validate_expr(w)?;
    }
    for g in &stmt.group_by {
        if contains_agg(g) {
            return Err(exec_err("42803", "aggregates are not allowed in GROUP BY"));
        }
        validate_expr(g)?;
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            validate_expr(expr)?;
        }
    }
    for f in &stmt.from {
        validate_from(f)?;
    }
    if let Some(h) = &stmt.having {
        validate_expr(h)?;
    }
    for o in &stmt.order_by {
        validate_expr(&o.expr)?;
    }
    if stmt.for_update {
        if stmt.distinct {
            return Err(exec_err(
                "0A000",
                "FOR UPDATE is not allowed with DISTINCT clause",
            ));
        }
        if !stmt.group_by.is_empty() || stmt.having.is_some() {
            return Err(exec_err(
                "0A000",
                "FOR UPDATE is not allowed with GROUP BY clause",
            ));
        }
        if is_agg_query(stmt) {
            return Err(exec_err(
                "0A000",
                "FOR UPDATE is not allowed with aggregate functions",
            ));
        }
    }
    Ok(())
}

fn validate_from(f: &FromItem) -> Result<(), ExecError> {
    match f {
        FromItem::Table { .. } => Ok(()),
        FromItem::Derived { sub, .. } => validate_select(sub),
        FromItem::Join {
            left, right, on, ..
        } => {
            validate_from(left)?;
            validate_from(right)?;
            if let Some(p) = on {
                if contains_agg(p) {
                    return Err(exec_err(
                        "42803",
                        "aggregates are not allowed in JOIN conditions",
                    ));
                }
                validate_expr(p)?;
            }
            Ok(())
        }
    }
}

/// Recurse into subqueries so nested SELECTs get validated too.
fn validate_expr(e: &Expr) -> Result<(), ExecError> {
    match e {
        Expr::ScalarSub(s) => validate_select(s),
        Expr::InSub { expr, sub, .. } => {
            validate_expr(expr)?;
            validate_select(sub)
        }
        Expr::Exists { sub, .. } => validate_select(sub),
        Expr::Add(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            validate_expr(a)?;
            validate_expr(b)
        }
        Expr::Cmp { left, right, .. } => {
            validate_expr(left)?;
            validate_expr(right)
        }
        Expr::Not(x) | Expr::IsNull { expr: x, .. } => validate_expr(x),
        Expr::Agg { arg, .. } => {
            if let Some(a) = arg {
                validate_expr(a)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Does this expression contain an aggregate at *this* query level?
/// Subqueries are their own level and are not descended into.
fn contains_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg { .. } => true,
        Expr::Column { .. } | Expr::ResolvedCol { .. } | Expr::Literal(_) | Expr::Param(_) => false,
        Expr::Add(a, b) | Expr::And(a, b) | Expr::Or(a, b) => contains_agg(a) || contains_agg(b),
        Expr::Cmp { left, right, .. } => contains_agg(left) || contains_agg(right),
        Expr::Not(x) | Expr::IsNull { expr: x, .. } => contains_agg(x),
        Expr::InSub { expr, .. } => contains_agg(expr),
        // ScalarSub / Exists are separate query levels.
        Expr::ScalarSub(_) | Expr::Exists { .. } => false,
    }
}

/// A query is aggregated when it has GROUP BY / HAVING, or an aggregate
/// anywhere at its own level (select list, HAVING, or ORDER BY — the
/// Postgres rule).
fn is_agg_query(stmt: &SelectStmt) -> bool {
    !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt.items.iter().any(|i| match i {
            SelectItem::Expr { expr, .. } => contains_agg(expr),
            _ => false,
        })
        || stmt.order_by.iter().any(|o| contains_agg(&o.expr))
}

// ---------------------------------------------------------------------------
// FROM
// ---------------------------------------------------------------------------

/// Build the joined working table: schema + one QRow per joined row.
/// `outer` is the enclosing query's scope chain (for correlated
/// subqueries in ON / derived tables never see it — no LATERAL).
fn build_from(
    q: &mut Q,
    outer: &[Scope],
    from: &[FromItem],
    where_: Option<&Expr>,
    need_prov: bool,
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    if from.is_empty() {
        // No FROM: exactly one empty row (SELECT 1, SELECT count(*), ...).
        return Ok((Vec::new(), vec![QRow::default()]));
    }
    if from.len() == 1 {
        // Fast path: a single FROM item needs no cross product — filter
        // its rows in place and return them untouched, without the
        // per-row cells/prov rebuild the general loop below performs.
        // (Identical to one loop iteration with an empty accumulator.)
        let (s2, r2) = build_source(q, outer, &from[0], where_, need_prov)?;
        let quals: HashSet<String> = s2.iter().map(|c| c.qual.clone()).collect();
        let no_quals = HashSet::new();
        let push = pushdown_for(where_, &quals, &no_quals, &s2, &[]);
        let r2 = filter_rows(q, outer, &s2, r2, &push)?;
        return Ok((s2, r2));
    }
    let mut acc_schema: Vec<QCol> = Vec::new();
    let mut acc_rows = vec![QRow::default()];
    for item in from {
        let (s2, mut r2) = build_source(q, outer, item, where_, need_prov)?;
        // Comma joins are inner joins: a WHERE conjunct that mentions only
        // this item's columns can filter its rows before the cross product.
        // (Qualified refs must name a qualifier from this item and from no
        // earlier item; unqualified refs must resolve here and nowhere
        // earlier — anything ambiguous stays for the post-join WHERE, which
        // raises the same error it always did.)
        let quals: HashSet<String> = s2.iter().map(|c| c.qual.clone()).collect();
        let acc_quals: HashSet<String> = acc_schema.iter().map(|c| c.qual.clone()).collect();
        let push = pushdown_for(where_, &quals, &acc_quals, &s2, &acc_schema);
        r2 = filter_rows(q, outer, &s2, r2, &push)?;
        let mut schema = Vec::with_capacity(acc_schema.len() + s2.len());
        schema.extend(acc_schema.iter().cloned());
        schema.extend(s2.iter().cloned());
        let mut rows = Vec::new();
        for a in &acc_rows {
            for b in &r2 {
                let mut cells = Vec::with_capacity(a.cells.len() + b.cells.len());
                cells.extend(a.cells.iter().cloned());
                cells.extend(b.cells.iter().cloned());
                let mut prov = Vec::with_capacity(a.prov.len() + b.prov.len());
                prov.extend(a.prov.iter().cloned());
                prov.extend(b.prov.iter().cloned());
                rows.push(QRow { cells, prov });
            }
        }
        acc_schema = schema;
        acc_rows = rows;
    }
    Ok((acc_schema, acc_rows))
}

/// AND-flatten a WHERE predicate into its top-level conjuncts. Splitting
/// is sound for filtering: a row survives `WHERE (a AND b)` iff it
/// survives both `a` and `b` (three-valued AND is TRUE iff both are TRUE).
fn split_conjuncts(e: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    let mut stack = vec![e];
    while let Some(x) = stack.pop() {
        match x {
            Expr::And(a, b) => {
                stack.push(a);
                stack.push(b);
            }
            _ => out.push(x),
        }
    }
    out
}

/// Collect every column reference in `e`. Returns false when `e` contains
/// anything we refuse to push below a join — aggregates and subqueries
/// stay above, conservatively (their correlation is someone else's problem).
fn pushable_columns(e: &Expr, cols: &mut Vec<(Option<String>, String)>) -> bool {
    match e {
        Expr::Column { table, name } => {
            cols.push((table.clone(), name.clone()));
            true
        }
        Expr::Literal(_) | Expr::Param(_) => true,
        Expr::Add(a, b) => pushable_columns(a, cols) && pushable_columns(b, cols),
        Expr::Cmp { left, right, .. } => {
            pushable_columns(left, cols) && pushable_columns(right, cols)
        }
        Expr::And(a, b) | Expr::Or(a, b) => pushable_columns(a, cols) && pushable_columns(b, cols),
        Expr::Not(x) => pushable_columns(x, cols),
        Expr::IsNull { expr: x, .. } => pushable_columns(x, cols),
        _ => false,
    }
}

/// The WHERE conjuncts that mention only `own`'s columns and may therefore
/// filter `own`'s rows before the join. A qualified ref must name a
/// qualifier of `own` and of no table in `other_quals`; an unqualified ref
/// must resolve to a column of `own` and of no column of `other`. Anything
/// ambiguous is left for the post-join WHERE, which reports it exactly as
/// before — pushdown never changes which queries error.
fn pushdown_for<'a>(
    where_: Option<&'a Expr>,
    own_quals: &HashSet<String>,
    other_quals: &HashSet<String>,
    own: &[QCol],
    other: &[QCol],
) -> Vec<&'a Expr> {
    let Some(w) = where_ else {
        return Vec::new();
    };
    split_conjuncts(w)
        .into_iter()
        .filter(|c| {
            let mut cols = Vec::new();
            if !pushable_columns(c, &mut cols) {
                return false;
            }
            cols.iter().all(|(q, name)| match q {
                Some(qq) => own_quals.contains(qq) && !other_quals.contains(qq),
                None => {
                    own.iter().any(|c| c.name == *name) && !other.iter().any(|c| c.name == *name)
                }
            })
        })
        .collect()
}

/// Keep the rows for which every conjunct is TRUE (same check_bool the
/// post-join WHERE uses).
fn filter_rows(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: Vec<QRow>,
    conjuncts: &[&Expr],
) -> Result<Vec<QRow>, ExecError> {
    if conjuncts.is_empty() {
        return Ok(rows);
    }
    let mut out = Vec::new();
    for row in rows {
        let mut keep = true;
        for c in conjuncts {
            let frame = Scope {
                schema,
                row: &row.cells,
            };
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(frame);
            if !check_bool(eval_expr(q, &scopes, c)?, "WHERE")? {
                keep = false;
                break;
            }
        }
        if keep {
            out.push(row);
        }
    }
    Ok(out)
}

/// Every `(qualifier, name)` column reference in an expression, at any
/// depth — including inside subqueries. Conservative: a ref that merely
/// looks ambiguous at the join level disables the fast path even if an
/// inner scope would shadow it; the slow path keeps exact old semantics.
fn collect_column_refs(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        Expr::Column { table, name } => out.push((table.clone(), name.clone())),
        // Already resolved (unambiguous by construction): nothing to collect.
        Expr::ResolvedCol { .. } => {}
        Expr::Literal(_) | Expr::Param(_) => {}
        Expr::Add(a, b) => {
            collect_column_refs(a, out);
            collect_column_refs(b, out);
        }
        Expr::Cmp { left, right, .. } => {
            collect_column_refs(left, out);
            collect_column_refs(right, out);
        }
        Expr::And(a, b) | Expr::Or(a, b) => {
            collect_column_refs(a, out);
            collect_column_refs(b, out);
        }
        Expr::Not(x) => collect_column_refs(x, out),
        Expr::IsNull { expr: x, .. } => collect_column_refs(x, out),
        Expr::Agg { arg, .. } => {
            if let Some(x) = arg {
                collect_column_refs(x, out);
            }
        }
        Expr::ScalarSub(s) => collect_stmt_refs(s, out),
        Expr::InSub { expr, sub, .. } => {
            collect_column_refs(expr, out);
            collect_stmt_refs(sub, out);
        }
        Expr::Exists { sub, .. } => collect_stmt_refs(sub, out),
    }
}

fn collect_stmt_refs(s: &SelectStmt, out: &mut Vec<(Option<String>, String)>) {
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_column_refs(expr, out);
        }
    }
    for f in &s.from {
        collect_from_refs(f, out);
    }
    if let Some(w) = &s.where_ {
        collect_column_refs(w, out);
    }
    for g in &s.group_by {
        collect_column_refs(g, out);
    }
    if let Some(h) = &s.having {
        collect_column_refs(h, out);
    }
    for o in &s.order_by {
        collect_column_refs(&o.expr, out);
    }
}

fn collect_from_refs(f: &FromItem, out: &mut Vec<(Option<String>, String)>) {
    match f {
        FromItem::Join {
            left, right, on, ..
        } => {
            collect_from_refs(left, out);
            collect_from_refs(right, out);
            if let Some(p) = on {
                collect_column_refs(p, out);
            }
        }
        FromItem::Derived { sub, .. } => collect_stmt_refs(sub, out),
        FromItem::Table { .. } => {}
    }
}

/// True when a JOIN's ON predicate can be evaluated with two separate
/// frames (left row / right row) instead of a combined row buffer: no
/// column reference is ambiguous across the two sides. When this holds,
/// the old single-frame code could never have raised 42702 on this
/// predicate, so the two-frame evaluation is exactly equivalent — and it
/// allocates nothing per row pair in the common case. Anything doubtful
/// takes the slow path (the old combined-frame code, unchanged).
fn join_fast_path(on: &Expr, lschema: &[QCol], rschema: &[QCol]) -> bool {
    let mut refs = Vec::new();
    collect_column_refs(on, &mut refs);
    refs.iter().all(|(q, name)| {
        let count = |s: &[QCol]| {
            s.iter()
                .filter(|c| c.name == *name && q.as_deref().map_or(true, |qq| c.qual == qq))
                .count()
        };
        count(lschema) + count(rschema) < 2
    })
}

/// Combine a kept join pair: concatenated cells + provenance.
fn combine_rows(l: &QRow, r: &QRow) -> QRow {
    let mut cells = Vec::with_capacity(l.cells.len() + r.cells.len());
    cells.extend(l.cells.iter().cloned());
    cells.extend(r.cells.iter().cloned());
    let mut prov = Vec::with_capacity(l.prov.len() + r.prov.len());
    prov.extend(l.prov.iter().cloned());
    prov.extend(r.prov.iter().cloned());
    QRow { cells, prov }
}

/// LEFT JOIN null-extension for an unmatched left row.
fn left_null_row(l: &QRow, rschema: &[QCol]) -> QRow {
    let mut cells = l.cells.clone();
    cells.extend(rschema.iter().map(|_| Value::Null));
    QRow {
        cells,
        prov: l.prov.clone(),
    }
}

fn build_source(
    q: &mut Q,
    outer: &[Scope],
    item: &FromItem,
    where_: Option<&Expr>,
    // True when this statement is FOR UPDATE: base-table rows must carry
    // their (table, row-version id) provenance so the outer FOR UPDATE
    // pass can lock them. Otherwise provenance stays empty and costs
    // nothing per row.
    need_prov: bool,
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    match item {
        FromItem::Table { name, alias } => {
            let qual = alias.clone().unwrap_or_else(|| name.clone());
            // Borrow ends before any recursive call below: everything is
            // cloned out of the table.
            let (schema, rows) = {
                let t = q.eng.db.find_table(name, q.snap, q.own).ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", name))
                })?;
                let schema: Vec<QCol> = t
                    .columns
                    .iter()
                    .map(|(n, ty)| QCol {
                        qual: qual.clone(),
                        name: n.clone(),
                        ty: ty.clone(),
                    })
                    .collect();
                let rows: Vec<QRow> = t
                    .rows
                    .iter()
                    .filter(|r| row_visible(r, q.snap, q.own))
                    .map(|r| QRow {
                        cells: r.values.clone(),
                        prov: if need_prov {
                            vec![(name.clone(), r.id)]
                        } else {
                            Vec::new()
                        },
                    })
                    .collect();
                (schema, rows)
            };
            Ok((schema, rows))
        }
        FromItem::Derived { sub, alias } => {
            // Derived tables are uncorrelated (no LATERAL support): they
            // never see the outer scope chain.
            let out = {
                let mut sub_q = Q {
                    eng: &mut *q.eng,
                    snap: q.snap,
                    own: q.own,
                    depth: q.depth + 1,
                    lock_ids: &mut *q.lock_ids,
                };
                run_select(&mut sub_q, sub, &[])?
            };
            let schema: Vec<QCol> = out
                .columns
                .into_iter()
                .map(|(n, ty)| QCol {
                    qual: alias.clone(),
                    name: n,
                    ty,
                })
                .collect();
            let rows: Vec<QRow> = out
                .rows
                .into_iter()
                .map(|cells| QRow {
                    cells,
                    prov: Vec::new(),
                })
                .collect();
            Ok((schema, rows))
        }
        FromItem::Join {
            left,
            kind,
            right,
            on,
        } => {
            let (lschema, lrows) = build_source(q, outer, left, where_, need_prov)?;
            let (rschema, rrows) = build_source(q, outer, right, where_, need_prov)?;
            let mut schema = Vec::with_capacity(lschema.len() + rschema.len());
            schema.extend(lschema.iter().cloned());
            schema.extend(rschema.iter().cloned());
            // Predicate pushdown, inner joins only below nested joins:
            // each side is filtered here only when it is a leaf source
            // (table or derived table), so a conjunct is never pushed at
            // two levels. For INNER/CROSS both sides filter; for LEFT only
            // the preserved (left) side — filtering the right side of a
            // LEFT JOIN would change NULL-extension. The full WHERE still
            // applies after the join, so this is purely an optimization.
            let lquals: HashSet<String> = lschema.iter().map(|c| c.qual.clone()).collect();
            let rquals: HashSet<String> = rschema.iter().map(|c| c.qual.clone()).collect();
            let lrows = if matches!(
                left.as_ref(),
                FromItem::Table { .. } | FromItem::Derived { .. }
            ) {
                let push = pushdown_for(where_, &lquals, &rquals, &lschema, &rschema);
                filter_rows(q, outer, &lschema, lrows, &push)?
            } else {
                lrows
            };
            let rrows = if *kind != JoinKind::Left
                && matches!(
                    right.as_ref(),
                    FromItem::Table { .. } | FromItem::Derived { .. }
                ) {
                let push = pushdown_for(where_, &rquals, &lquals, &rschema, &lschema);
                filter_rows(q, outer, &rschema, rrows, &push)?
            } else {
                rrows
            };
            // Three execution paths for the nested loop. Fast path: no ON
            // column ref is ambiguous across the two sides, so two
            // separate frames resolve exactly like the old single
            // combined frame (which could never raise 42702 here) — and
            // no combined row is built per pair. The slow path is the old
            // combined-frame code, unchanged; it only triggers for
            // ambiguous ON predicates (which raise 42702 on the first
            // pair) — correctness is never at risk.
            let mut rows = Vec::new();
            let is_cross = *kind == JoinKind::Cross;
            let on_pred: Option<&Expr> = if is_cross { None } else { on.as_ref() };
            let fast = is_cross || on_pred.map_or(false, |p| join_fast_path(p, &lschema, &rschema));
            // Pre-resolve the ON predicate's column references once: the
            // scope shape is fixed for the whole loop, so per-pair name
            // resolution is pure overhead (~20% of join instructions per
            // Callgrind). The prototype schema order matches the runtime
            // scope order below (outer frames, then left, then right).
            // Only resolve when the loop would actually evaluate the
            // predicate (both sides non-empty): with an empty input the
            // loop body never runs, and per-row evaluation would never
            // have raised a resolution error either.
            let mut proto_schemas: Vec<&[QCol]> = Vec::with_capacity(outer.len() + 2);
            for s in outer {
                proto_schemas.push(s.schema);
            }
            proto_schemas.push(&lschema);
            proto_schemas.push(&rschema);
            let on_resolved: Option<Expr> = match on_pred {
                Some(p) if fast && !lrows.is_empty() && !rrows.is_empty() => {
                    Some(resolve_predicate_columns(p, &proto_schemas)?)
                }
                _ => None,
            };
            let on_fast: Option<&Expr> = on_resolved.as_ref().or(on_pred);
            if fast && outer.is_empty() {
                // Hot path: zero allocation per row pair. The two frames
                // live in a stack array; Scope is Copy.
                for l in &lrows {
                    let fl = Scope {
                        schema: &lschema,
                        row: &l.cells,
                    };
                    let mut matched = false;
                    for r in &rrows {
                        let fr = Scope {
                            schema: &rschema,
                            row: &r.cells,
                        };
                        let scopes = [fl, fr];
                        let keep = match on_fast {
                            None => true,
                            Some(p) => check_bool(eval_expr(q, &scopes, p)?, "join condition")?,
                        };
                        if keep {
                            matched = true;
                            rows.push(combine_rows(l, r));
                        }
                    }
                    if !matched && *kind == JoinKind::Left {
                        rows.push(left_null_row(l, &rschema));
                    }
                }
            } else if fast {
                // Correlated join (rare): one small scope Vec per pair.
                for l in &lrows {
                    let mut matched = false;
                    for r in &rrows {
                        let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 2);
                        scopes.extend_from_slice(outer);
                        scopes.push(Scope {
                            schema: &lschema,
                            row: &l.cells,
                        });
                        scopes.push(Scope {
                            schema: &rschema,
                            row: &r.cells,
                        });
                        let keep = match on_fast {
                            None => true,
                            Some(p) => check_bool(eval_expr(q, &scopes, p)?, "join condition")?,
                        };
                        if keep {
                            matched = true;
                            rows.push(combine_rows(l, r));
                        }
                    }
                    if !matched && *kind == JoinKind::Left {
                        rows.push(left_null_row(l, &rschema));
                    }
                }
            } else {
                // Slow path: original combined-frame evaluation.
                for l in &lrows {
                    let mut matched = false;
                    for r in &rrows {
                        let mut cells = Vec::with_capacity(l.cells.len() + r.cells.len());
                        cells.extend(l.cells.iter().cloned());
                        cells.extend(r.cells.iter().cloned());
                        let keep = match on_pred {
                            None => true,
                            Some(p) => {
                                let frame = Scope {
                                    schema: &schema,
                                    row: &cells,
                                };
                                let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                                scopes.extend_from_slice(outer);
                                scopes.push(frame);
                                check_bool(eval_expr(q, &scopes, p)?, "join condition")?
                            }
                        };
                        if keep {
                            matched = true;
                            let mut prov = Vec::with_capacity(l.prov.len() + r.prov.len());
                            prov.extend(l.prov.iter().cloned());
                            prov.extend(r.prov.iter().cloned());
                            rows.push(QRow { cells, prov });
                        }
                    }
                    if !matched && *kind == JoinKind::Left {
                        rows.push(left_null_row(l, &rschema));
                    }
                }
            }
            Ok((schema, rows))
        }
    }
}

fn apply_where(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: Vec<QRow>,
    pred: Option<&Expr>,
) -> Result<Vec<QRow>, ExecError> {
    let Some(pred) = pred else {
        return Ok(rows);
    };
    let mut out = Vec::new();
    for row in rows {
        let keep = {
            let frame = Scope {
                schema,
                row: &row.cells,
            };
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(frame);
            check_bool(eval_expr(q, &scopes, pred)?, "WHERE")?
        };
        if keep {
            out.push(row);
        }
    }
    Ok(out)
}

/// A predicate result keeps the row iff it is TRUE (NULL counts as false,
/// SQL three-valued logic); a non-boolean is 42804, like Postgres.
fn check_bool(v: Value, what: &str) -> Result<bool, ExecError> {
    match v {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        other => Err(exec_err(
            "42804",
            format!(
                "argument of {} must be type boolean, not type {}",
                what,
                other.type_name()
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// Projection (non-aggregated queries)
// ---------------------------------------------------------------------------

/// Project one joined row to output cells + provenance.
fn project_row(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
    row: QRow,
) -> Result<(Vec<Value>, Vec<(String, u64)>), ExecError> {
    // Fast path: plain `SELECT *` moves the row through untouched — no
    // scope chain, no per-row allocation at all.
    if stmt.items.len() == 1 && matches!(stmt.items[0], SelectItem::All) {
        return Ok((row.cells, row.prov));
    }
    // Only expression items need a scope chain; star-only shapes don't
    // pay for one. `row` is owned, so provenance moves without cloning.
    let scopes: Vec<Scope> = if stmt
        .items
        .iter()
        .any(|i| matches!(i, SelectItem::Expr { .. }))
    {
        let mut s: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
        s.extend_from_slice(outer);
        s.push(Scope {
            schema,
            row: &row.cells,
        });
        s
    } else {
        Vec::new()
    };
    let mut cells = Vec::new();
    for item in &stmt.items {
        match item {
            SelectItem::All => cells.extend(row.cells.iter().cloned()),
            SelectItem::AllOf(qual) => {
                let mut any = false;
                for (c, v) in schema.iter().zip(row.cells.iter()) {
                    if c.qual == *qual {
                        cells.push(v.clone());
                        any = true;
                    }
                }
                if !any {
                    return Err(exec_err(
                        "42P01",
                        format!("missing FROM-clause entry for table \"{}\"", qual),
                    ));
                }
            }
            SelectItem::Expr { expr, .. } => cells.push(eval_expr(q, &scopes, expr)?),
        }
    }
    Ok((cells, row.prov))
}

// ---------------------------------------------------------------------------
// Aggregation (v0.6)
// ---------------------------------------------------------------------------

/// Append a canonical byte key for a value (for GROUP BY / DISTINCT).
/// Floats use their bit pattern so NaN groups with NaN, like Postgres.
fn value_key(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0),
        Value::Int(i) => {
            out.push(1);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::Float(f) => {
            out.push(2);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        Value::Text(s) => {
            out.push(3);
            out.extend_from_slice(&(s.len() as u64).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bool(b) => {
            out.push(4);
            out.push(*b as u8);
        }
    }
}

/// Hash-group the rows, then evaluate the select list + HAVING per group.
/// With no GROUP BY and no input rows there is still exactly one (empty)
/// group, so `SELECT count(*)` returns 0 rather than no rows — like
/// Postgres. FOR UPDATE never reaches here (rejected in validation).
fn exec_agg(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
    rows: &[QRow],
    out_cols: &[(String, ColType)],
) -> Result<Vec<OutRow>, ExecError> {
    // Group rows by their GROUP BY key, remembering first-seen order.
    let mut group_index: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut groups: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let (key_vals, key_bytes) = {
            let frame = Scope {
                schema,
                row: &row.cells,
            };
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(frame);
            let mut key_vals = Vec::with_capacity(stmt.group_by.len());
            let mut key_bytes = Vec::new();
            for g in &stmt.group_by {
                // GROUP BY exprs cannot contain aggregates (validated).
                let v = eval_expr(q, &scopes, g)?;
                value_key(&v, &mut key_bytes);
                key_vals.push(v);
            }
            (key_vals, key_bytes)
        };
        match group_index.get(&key_bytes) {
            Some(&gi) => groups[gi].1.push(i),
            None => {
                group_index.insert(key_bytes, groups.len());
                groups.push((key_vals, vec![i]));
            }
        }
    }
    if groups.is_empty() && stmt.group_by.is_empty() {
        groups.push((Vec::new(), Vec::new()));
    }
    // ORDER BY resolution needs the output schema + select-item positions;
    // the per-group fallback evaluates group-level expressions (aggregates
    // and GROUP BY columns, like Postgres) while all group context is alive.
    let out_qcols: Vec<QCol> = out_cols
        .iter()
        .map(|(n, ty)| QCol {
            qual: String::new(),
            name: n.clone(),
            ty: ty.clone(),
        })
        .collect();
    let item_pos = select_item_positions(stmt, schema);
    let mut out_rows = Vec::new();
    for (key_vals, idxs) in &groups {
        // Correlated subqueries inside HAVING / the select list see the
        // first row of the group. Any correlated column must be group-bound
        // for the query to be valid, so every row in the group agrees on it.
        let first: &[Value] = match idxs.first() {
            Some(&i) => &rows[i].cells,
            None => &[],
        };
        let gscope = Scope { schema, row: first };
        if let Some(h) = &stmt.having {
            if !eval_grouped_bool(
                q,
                outer,
                gscope,
                schema,
                rows,
                idxs,
                key_vals,
                &stmt.group_by,
                h,
            )? {
                continue;
            }
        }
        let mut cells = Vec::new();
        for item in &stmt.items {
            match item {
                SelectItem::All => {
                    for c in schema.iter() {
                        cells.push(grouped_col_value(
                            gscope,
                            &stmt.group_by,
                            key_vals,
                            c.qual.as_str(),
                            &c.name,
                        )?);
                    }
                }
                SelectItem::AllOf(qual) => {
                    let mut any = false;
                    for c in schema.iter().filter(|c| c.qual == *qual) {
                        cells.push(grouped_col_value(
                            gscope,
                            &stmt.group_by,
                            key_vals,
                            qual.as_str(),
                            &c.name,
                        )?);
                        any = true;
                    }
                    if !any {
                        return Err(exec_err(
                            "42P01",
                            format!("missing FROM-clause entry for table \"{}\"", qual),
                        ));
                    }
                }
                SelectItem::Expr { expr, .. } => cells.push(eval_grouped(
                    q,
                    outer,
                    gscope,
                    schema,
                    rows,
                    idxs,
                    key_vals,
                    &stmt.group_by,
                    expr,
                )?),
            }
        }
        let sort_keys = if stmt.order_by.is_empty() {
            None
        } else {
            let mut fallback = |e: &Expr| {
                eval_grouped(
                    q,
                    outer,
                    gscope,
                    schema,
                    rows,
                    idxs,
                    key_vals,
                    &stmt.group_by,
                    e,
                )
            };
            let mut keys = Vec::with_capacity(stmt.order_by.len());
            for term in &stmt.order_by {
                keys.push(order_key(
                    stmt,
                    &out_qcols,
                    &item_pos,
                    &cells,
                    &term.expr,
                    &mut fallback,
                )?);
            }
            Some(keys)
        };
        out_rows.push(OutRow {
            cells,
            prov: Vec::new(),
            full: Vec::new(),
            sort_keys,
        });
    }
    Ok(out_rows)
}

/// A bare column in a grouped query: it must be group-bound — either one
/// of the GROUP BY expressions (same column, or structurally identical)
/// — else 42803, like Postgres.
fn grouped_col_value(
    gscope: Scope,
    group_by: &[Expr],
    key_vals: &[Value],
    qual: &str,
    name: &str,
) -> Result<Value, ExecError> {
    let qual_opt = if qual.is_empty() { None } else { Some(qual) };
    let (rsi, rci) = resolve_col(&[gscope], qual_opt, name)?;
    for (i, g) in group_by.iter().enumerate() {
        match g {
            Expr::Column {
                table: gq,
                name: gn,
            } => {
                if let Ok((gsi, gci)) = resolve_col(&[gscope], gq.as_deref(), gn) {
                    if gsi == rsi && gci == rci {
                        return Ok(key_vals[i].clone());
                    }
                }
            }
            _ => {
                let probe = Expr::Column {
                    table: qual_opt.map(|s| s.to_string()),
                    name: name.to_string(),
                };
                if *g == probe {
                    return Ok(key_vals[i].clone());
                }
            }
        }
    }
    Err(exec_err(
        "42803",
        format!(
            "column \"{}\" must appear in the GROUP BY clause or be used in an aggregate function",
            name
        ),
    ))
}

/// Evaluate a select-list / HAVING expression for one group: aggregates
/// fold the group's rows, bare columns must be group-bound, and
/// subqueries evaluate with the group's first row as correlation scope.
fn eval_grouped(
    q: &mut Q,
    outer: &[Scope],
    gscope: Scope,
    schema: &[QCol],
    rows: &[QRow],
    idxs: &[usize],
    key_vals: &[Value],
    group_by: &[Expr],
    e: &Expr,
) -> Result<Value, ExecError> {
    match e {
        Expr::Agg { func, arg } => {
            eval_agg_func(q, outer, schema, rows, idxs, *func, arg.as_deref())
        }
        Expr::Column { table, name } => grouped_col_value(
            gscope,
            group_by,
            key_vals,
            table.as_deref().unwrap_or(""),
            name,
        ),
        // Resolved columns never reach grouped evaluation (GROUP BY /
        // select-list expressions are resolved per row at runtime).
        Expr::ResolvedCol { .. } => Err(exec_err(
            "XX000",
            "internal error: resolved column in grouped evaluation",
        )),
        // Subqueries are their own query level: evaluate normally, with
        // the group's first row available for correlation.
        Expr::ScalarSub(_) | Expr::InSub { .. } | Expr::Exists { .. } => {
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(gscope);
            eval_expr(q, &scopes, e)
        }
        Expr::Literal(lit) => Ok(lit.clone().into_value()),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Add(a, b) => {
            let va = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, a)?;
            let vb = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, b)?;
            eval_add(&va, &vb).map(|(_, v)| v)
        }
        Expr::Cmp { op, left, right } => {
            let va = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, left,
            )?;
            let vb = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, right,
            )?;
            eval_cmp_vals(*op, &va, &vb)
        }
        Expr::And(a, b) => {
            let va = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, a)?;
            let vb = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, b)?;
            eval_and_vals(&va, &vb)
        }
        Expr::Or(a, b) => {
            let va = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, a)?;
            let vb = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, b)?;
            eval_or_vals(&va, &vb)
        }
        Expr::Not(x) => {
            let v = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, x)?;
            eval_not_val(&v)
        }
        Expr::IsNull { expr: x, neg } => {
            let v = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, x)?;
            Ok(Value::Bool((v == Value::Null) != *neg))
        }
    }
}

fn eval_grouped_bool(
    q: &mut Q,
    outer: &[Scope],
    gscope: Scope,
    schema: &[QCol],
    rows: &[QRow],
    idxs: &[usize],
    key_vals: &[Value],
    group_by: &[Expr],
    e: &Expr,
) -> Result<bool, ExecError> {
    let v = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, e)?;
    check_bool(v, "HAVING")
}

/// The aggregate functions over one group's rows.
fn eval_agg_func(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: &[QRow],
    idxs: &[usize],
    func: AggFunc,
    arg: Option<&Expr>,
) -> Result<Value, ExecError> {
    if func == AggFunc::Count && arg.is_none() {
        return Ok(Value::Int(idxs.len() as i64));
    }
    let a = arg.expect("non-COUNT aggregates take an argument");
    let mut vals: Vec<Value> = Vec::new();
    for &i in idxs {
        let frame = Scope {
            schema,
            row: &rows[i].cells,
        };
        let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
        scopes.extend_from_slice(outer);
        scopes.push(frame);
        // Aggregate arguments cannot nest aggregates (the parser allows
        // the syntax; fail like Postgres rather than recursing forever).
        let v = eval_expr(q, &scopes, a)?;
        if v != Value::Null {
            vals.push(v);
        }
    }
    match func {
        AggFunc::Count => Ok(Value::Int(vals.len() as i64)),
        AggFunc::Sum => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut int_sum: i64 = 0;
            let mut float_sum: f64 = 0.0;
            let mut is_float = false;
            for v in &vals {
                match v {
                    Value::Int(i) => {
                        if is_float {
                            float_sum += *i as f64;
                        } else {
                            int_sum = int_sum
                                .checked_add(*i)
                                .ok_or_else(|| exec_err("22003", "integer out of range"))?;
                        }
                    }
                    Value::Float(f) => {
                        if !is_float {
                            is_float = true;
                            float_sum = int_sum as f64;
                        }
                        float_sum += *f;
                    }
                    other => {
                        return Err(exec_err(
                            "42883",
                            format!("function sum({}) does not exist", other.type_name()),
                        ));
                    }
                }
            }
            Ok(if is_float {
                Value::Float(float_sum)
            } else {
                Value::Int(int_sum)
            })
        }
        AggFunc::Avg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut sum = 0.0;
            for v in &vals {
                match v {
                    Value::Int(i) => sum += *i as f64,
                    Value::Float(f) => sum += *f,
                    other => {
                        return Err(exec_err(
                            "42883",
                            format!("function avg({}) does not exist", other.type_name()),
                        ));
                    }
                }
            }
            Ok(Value::Float(sum / vals.len() as f64))
        }
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<&Value> = None;
            for v in &vals {
                match best {
                    None => best = Some(v),
                    Some(b) => {
                        let ord = cmp_ordering(v, b, CmpOp::Lt)?.expect("non-null values compare");
                        let better = if func == AggFunc::Min {
                            ord == Ordering::Less
                        } else {
                            ord == Ordering::Greater
                        };
                        if better {
                            best = Some(v);
                        }
                    }
                }
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
    }
}

// ---------------------------------------------------------------------------
// ORDER BY
// ---------------------------------------------------------------------------

/// Output-column positions of each select item (None for `*` expansions),
/// for matching ORDER BY terms against the select list structurally.
fn select_item_positions(stmt: &SelectStmt, schema: &[QCol]) -> Vec<Option<usize>> {
    let mut pos = 0;
    let mut out = Vec::new();
    for item in &stmt.items {
        match item {
            SelectItem::All => {
                pos += schema.len();
                out.push(None);
            }
            SelectItem::AllOf(qual) => {
                pos += schema.iter().filter(|c| c.qual == *qual).count();
                out.push(None);
            }
            SelectItem::Expr { .. } => {
                out.push(Some(pos));
                pos += 1;
            }
        }
    }
    out
}

fn apply_order(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
    out_cols: &[(String, ColType)],
    orows: &mut Vec<OutRow>,
) -> Result<(), ExecError> {
    let out_qcols: Vec<QCol> = out_cols
        .iter()
        .map(|(n, ty)| QCol {
            qual: String::new(),
            name: n.clone(),
            ty: ty.clone(),
        })
        .collect();
    let item_pos = select_item_positions(stmt, schema);
    // Sort keys per row; an evaluation error aborts the sort (captured
    // like the v0.5 implementation, since sort_by can't return Results).
    // Aggregated queries carry keys precomputed in exec_agg, where the
    // group context is still alive.
    let mut keys: Vec<Vec<Value>> = Vec::with_capacity(orows.len());
    let mut key_err: Option<ExecError> = None;
    for o in orows.iter() {
        if key_err.is_some() {
            break;
        }
        let row_keys = match &o.sort_keys {
            Some(k) => k.clone(),
            None => {
                let mut fallback = |e: &Expr| -> Result<Value, ExecError> {
                    if stmt.distinct {
                        // DISTINCT queries can only sort by their output.
                        return Err(exec_err(
                            "42703",
                            "ORDER BY expression must appear in the select list",
                        ));
                    }
                    // Fall back to the full pre-projection row.
                    let frame = Scope {
                        schema,
                        row: &o.full,
                    };
                    let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                    scopes.extend_from_slice(outer);
                    scopes.push(frame);
                    eval_expr(q, &scopes, e)
                };
                let mut row_keys = Vec::with_capacity(stmt.order_by.len());
                for term in &stmt.order_by {
                    match order_key(
                        stmt,
                        &out_qcols,
                        &item_pos,
                        &o.cells,
                        &term.expr,
                        &mut fallback,
                    ) {
                        Ok(v) => row_keys.push(v),
                        Err(e) => {
                            key_err = Some(e);
                            break;
                        }
                    }
                }
                row_keys
            }
        };
        keys.push(row_keys);
    }
    if let Some(e) = key_err {
        return Err(e);
    }
    let mut idx: Vec<usize> = (0..orows.len()).collect();
    let mut cmp_err: Option<ExecError> = None;
    idx.sort_by(|&a, &b| {
        if cmp_err.is_some() {
            return Ordering::Equal;
        }
        for (i, term) in stmt.order_by.iter().enumerate() {
            match compare_values(&keys[a][i], &keys[b][i], term.desc) {
                Ok(Ordering::Equal) => continue,
                Ok(ord) => return ord,
                Err(e) => {
                    cmp_err = Some(e);
                    return Ordering::Equal;
                }
            }
        }
        Ordering::Equal
    });
    if let Some(e) = cmp_err {
        return Err(e);
    }
    let mut sorted = Vec::with_capacity(orows.len());
    for i in idx {
        sorted.push(OutRow {
            cells: std::mem::take(&mut orows[i].cells),
            prov: std::mem::take(&mut orows[i].prov),
            full: std::mem::take(&mut orows[i].full),
            sort_keys: std::mem::take(&mut orows[i].sort_keys),
        });
    }
    *orows = sorted;
    Ok(())
}

/// One ORDER BY key. Resolution order, Postgres-style:
/// 1. a positive integer literal = 1-based position in the select list;
/// 2. an expression structurally identical to a select-list expression;
/// 3. an unqualified name matching an output column (covers aliases);
/// 4. the caller-supplied fallback: for plain non-DISTINCT queries, any
///    expression over the FROM row (the v0.1-v0.5 behavior); for aggregated
///    queries, a group-level expression (aggregates / GROUP BY columns);
///    for DISTINCT queries, an error — output only.
fn order_key(
    stmt: &SelectStmt,
    out_qcols: &[QCol],
    item_pos: &[Option<usize>],
    cells: &[Value],
    expr: &Expr,
    fallback: &mut dyn FnMut(&Expr) -> Result<Value, ExecError>,
) -> Result<Value, ExecError> {
    if let Expr::Literal(Literal::Int(n)) = expr {
        if *n >= 1 {
            let idx = *n as usize;
            return cells.get(idx - 1).cloned().ok_or_else(|| {
                exec_err(
                    "42601",
                    format!("ORDER BY position {} is not in the select list", n),
                )
            });
        }
    }
    for (ii, item) in stmt.items.iter().enumerate() {
        if let SelectItem::Expr { expr: e, .. } = item {
            if e == expr {
                let pos = item_pos[ii].expect("expression items have positions");
                return Ok(cells[pos].clone());
            }
        }
    }
    if let Expr::Column { table: None, name } = expr {
        let mut found = None;
        for (i, c) in out_qcols.iter().enumerate() {
            if c.name == *name {
                if found.is_some() {
                    return Err(exec_err(
                        "42702",
                        format!("ORDER BY \"{}\" is ambiguous", name),
                    ));
                }
                found = Some(i);
            }
        }
        if let Some(i) = found {
            return Ok(cells[i].clone());
        }
    }
    fallback(expr)
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// Evaluate one expression against the scope chain (params substituted).
fn eval_expr(q: &mut Q, scopes: &[Scope], e: &Expr) -> Result<Value, ExecError> {
    match e {
        Expr::Column { table, name } => {
            let (si, ci) = resolve_col(scopes, table.as_deref(), name)?;
            Ok(scopes[si].row[ci].clone())
        }
        // Pre-resolved by resolve_predicate_columns for a fixed scope
        // shape: direct positional fetch, no name lookup. The positions
        // were resolved against these exact schemas, so the indexes hold.
        Expr::ResolvedCol { frame, idx } => Ok(scopes[*frame].row[*idx].clone()),
        Expr::Literal(lit) => Ok(lit.clone().into_value()),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Add(a, b) => {
            let va = eval_expr(q, scopes, a)?;
            let vb = eval_expr(q, scopes, b)?;
            eval_add(&va, &vb).map(|(_, v)| v)
        }
        Expr::Cmp { op, left, right } => {
            let va = eval_expr(q, scopes, left)?;
            let vb = eval_expr(q, scopes, right)?;
            eval_cmp_vals(*op, &va, &vb)
        }
        Expr::And(a, b) => {
            let va = eval_expr(q, scopes, a)?;
            let vb = eval_expr(q, scopes, b)?;
            eval_and_vals(&va, &vb)
        }
        Expr::Or(a, b) => {
            let va = eval_expr(q, scopes, a)?;
            let vb = eval_expr(q, scopes, b)?;
            eval_or_vals(&va, &vb)
        }
        Expr::Not(x) => {
            let v = eval_expr(q, scopes, x)?;
            eval_not_val(&v)
        }
        Expr::IsNull { expr: x, neg } => {
            let v = eval_expr(q, scopes, x)?;
            Ok(Value::Bool((v == Value::Null) != *neg))
        }
        // Aggregates only evaluate in the grouped path; reaching the
        // row-level evaluator is a validation bug (validate_select keeps
        // them out of WHERE/ON/GROUP BY, and is_agg_query routes select
        // lists with aggregates to exec_agg).
        Expr::Agg { .. } => Err(exec_err("42803", "aggregates not allowed in this context")),
        Expr::ScalarSub(sub) => {
            let out = {
                let mut sub_q = Q {
                    eng: &mut *q.eng,
                    snap: q.snap,
                    own: q.own,
                    depth: q.depth + 1,
                    lock_ids: &mut *q.lock_ids,
                };
                run_select(&mut sub_q, sub, scopes)?
            };
            if out.columns.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            if out.rows.len() > 1 {
                return Err(exec_err(
                    "21000",
                    "more than one row returned by a subquery used as an expression",
                ));
            }
            Ok(out
                .rows
                .first()
                .map(|r| r[0].clone())
                .unwrap_or(Value::Null))
        }
        Expr::InSub { expr, sub, neg } => eval_in(q, scopes, expr, sub, *neg),
        Expr::Exists { sub, neg } => {
            let out = {
                let mut sub_q = Q {
                    eng: &mut *q.eng,
                    snap: q.snap,
                    own: q.own,
                    depth: q.depth + 1,
                    lock_ids: &mut *q.lock_ids,
                };
                run_select(&mut sub_q, sub, scopes)?
            };
            let exists = !out.rows.is_empty();
            Ok(Value::Bool(if *neg { !exists } else { exists }))
        }
    }
}

/// `[NOT] IN (subquery)` with SQL three-valued logic: TRUE if any equal
/// value, else NULL if NULLs were involved, else FALSE.
fn eval_in(
    q: &mut Q,
    scopes: &[Scope],
    e: &Expr,
    sub: &SelectStmt,
    neg: bool,
) -> Result<Value, ExecError> {
    let v = eval_expr(q, scopes, e)?;
    let out = {
        let mut sub_q = Q {
            eng: &mut *q.eng,
            snap: q.snap,
            own: q.own,
            depth: q.depth + 1,
            lock_ids: &mut *q.lock_ids,
        };
        run_select(&mut sub_q, sub, scopes)?
    };
    if out.columns.len() != 1 {
        return Err(exec_err("42601", "subquery must return only one column"));
    }
    let mut saw_null = false;
    let mut found = false;
    for row in &out.rows {
        match cmp_ordering(&v, &row[0], CmpOp::Eq)? {
            Some(Ordering::Equal) => {
                found = true;
                break;
            }
            Some(_) => {}
            None => saw_null = true,
        }
    }
    let result = if found {
        Some(true)
    } else if v == Value::Null || saw_null {
        None
    } else {
        Some(false)
    };
    let result = if neg { not3(result) } else { result };
    Ok(match result {
        Some(b) => Value::Bool(b),
        None => Value::Null,
    })
}

fn not3(v: Option<bool>) -> Option<bool> {
    v.map(|b| !b)
}

/// Compare two values: None when either side is NULL (SQL semantics).
/// Numeric types compare numerically (int/float mix), text byte-wise (no
/// collations yet), bools false < true. Mismatched non-null types are
/// 42883, like Postgres.
fn cmp_ordering(a: &Value, b: &Value, op: CmpOp) -> Result<Option<Ordering>, ExecError> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Ok(None),
        (Value::Int(x), Value::Int(y)) => Ok(Some(x.cmp(y))),
        (Value::Float(x), Value::Float(y)) => Ok(Some(x.total_cmp(y))),
        (Value::Int(x), Value::Float(y)) => Ok(Some((*x as f64).total_cmp(y))),
        (Value::Float(x), Value::Int(y)) => Ok(Some(x.total_cmp(&(*y as f64)))),
        (Value::Text(x), Value::Text(y)) => Ok(Some(x.cmp(y))),
        (Value::Bool(x), Value::Bool(y)) => Ok(Some(x.cmp(y))),
        _ => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                a.type_name(),
                op.sql(),
                b.type_name()
            ),
        )),
    }
}

fn eval_cmp_vals(op: CmpOp, a: &Value, b: &Value) -> Result<Value, ExecError> {
    match cmp_ordering(a, b, op)? {
        None => Ok(Value::Null),
        Some(ord) => Ok(Value::Bool(match op {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
        })),
    }
}

/// AND/OR/NOT operands must be boolean (or NULL); anything else is 42804.
fn as_bool3(v: &Value, what: &str) -> Result<Option<bool>, ExecError> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(exec_err(
            "42804",
            format!(
                "{} requires boolean operands, not {}",
                what,
                other.type_name()
            ),
        )),
    }
}

fn bool3(v: Option<bool>) -> Value {
    match v {
        Some(b) => Value::Bool(b),
        None => Value::Null,
    }
}

fn eval_and_vals(a: &Value, b: &Value) -> Result<Value, ExecError> {
    let (a, b) = (as_bool3(a, "AND")?, as_bool3(b, "AND")?);
    Ok(bool3(match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }))
}

fn eval_or_vals(a: &Value, b: &Value) -> Result<Value, ExecError> {
    let (a, b) = (as_bool3(a, "OR")?, as_bool3(b, "OR")?);
    Ok(bool3(match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }))
}

fn eval_not_val(v: &Value) -> Result<Value, ExecError> {
    Ok(bool3(not3(as_bool3(v, "NOT")?)))
}

/// Evaluate a SET expression for UPDATE: the row being updated is the
/// correlation scope, so subqueries in SET can reference it.
fn eval_update_expr(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    schema: &[QCol],
    values: &[Value],
    e: &Expr,
) -> Result<Value, ExecError> {
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap,
        own,
        depth: 0,
        lock_ids: &mut lock_ids,
    };
    let frame = Scope {
        schema,
        row: values,
    };
    let v = eval_expr(&mut q, &[frame], e)?;
    // FOR UPDATE inside an UPDATE's SET subquery locks nothing: UPDATE is
    // not SELECT, so there is no statement-level lock flow to hand the
    // collected ids to (documented).
    Ok(v)
}

// ---------------------------------------------------------------------------
// v0.2: arithmetic (kept)
// ---------------------------------------------------------------------------

/// A value's type for `+` resolution; NULL contributes no constraint.
fn add_operand_value_type(v: &Value) -> Option<ColType> {
    match v {
        Value::Null => None,
        Value::Int(_) => Some(ColType::Int),
        Value::Float(_) => Some(ColType::Float),
        Value::Text(_) => Some(ColType::Text),
        Value::Bool(_) => Some(ColType::Bool),
    }
}

/// Result type of `a + b` given operand types (None = NULL/unknown side).
fn combine_add_types(a: Option<ColType>, b: Option<ColType>) -> Result<ColType, ExecError> {
    match (a, b) {
        (None, None) => Ok(ColType::Int),
        (None, Some(t)) | (Some(t), None) => Ok(t),
        (Some(ColType::Int), Some(ColType::Int)) => Ok(ColType::Int),
        (Some(ColType::Float), _) | (_, Some(ColType::Float)) => Ok(ColType::Float),
        (Some(x), Some(y)) => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} + {}",
                x.sql_name(),
                y.sql_name()
            ),
        )),
    }
}

fn eval_add(a: &Value, b: &Value) -> Result<(ColType, Value), ExecError> {
    let ty = combine_add_types(add_operand_value_type(a), add_operand_value_type(b))?;
    let v = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Int(x), Value::Int(y)) => Value::Int(
            x.checked_add(*y)
                .ok_or_else(|| exec_err("22003", "integer out of range"))?,
        ),
        (Value::Float(x), Value::Float(y)) => Value::Float(x + y),
        (Value::Int(x), Value::Float(y)) => Value::Float(*x as f64 + y),
        (Value::Float(x), Value::Int(y)) => Value::Float(x + *y as f64),
        _ => {
            return Err(exec_err(
                "42883",
                format!(
                    "operator does not exist: {} + {}",
                    a.type_name(),
                    b.type_name()
                ),
            ));
        }
    };
    Ok((ty, v))
}

/// Compare two values for ORDER BY. Numeric types compare numerically
/// (int/float mix), text compares byte-wise (no collation support yet),
/// bools order false < true. Mismatched non-null types are an error, like
/// PostgreSQL. NULL placement follows PostgreSQL defaults: NULLS LAST for
/// ASC, NULLS FIRST for DESC.
fn compare_values(a: &Value, b: &Value, desc: bool) -> Result<Ordering, ExecError> {
    let nulls_first = desc;
    let ord = match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            return Ok(if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        (_, Value::Null) => {
            return Ok(if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            });
        }
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).total_cmp(y),
        (Value::Float(x), Value::Int(y)) => x.total_cmp(&(*y as f64)),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => {
            return Err(exec_err(
                "42804",
                format!(
                    "ORDER BY cannot compare {} with {}",
                    value_type_name(a),
                    value_type_name(b)
                ),
            ));
        }
    };
    Ok(if desc { ord.reverse() } else { ord })
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "integer",
        Value::Float(_) => "float",
        Value::Text(_) => "text",
        Value::Bool(_) => "boolean",
        Value::Null => "null",
    }
}

// ---------------------------------------------------------------------------
// Result-column typing (Describe + execution agree via describe_select)
// ---------------------------------------------------------------------------

/// Working schemas of the FROM clause: one entry per source, in order.
/// Unknown tables are an error here (42P01); parameter inference uses
/// `unwrap_or_default` instead, so a bad table doesn't break Bind.
fn from_schemas(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    from: &[FromItem],
) -> Result<Vec<Vec<QCol>>, ExecError> {
    let mut out = Vec::new();
    for item in from {
        from_schema_item(eng, snap, own, item, &mut out)?;
    }
    Ok(out)
}

fn from_schema_item(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    item: &FromItem,
    out: &mut Vec<Vec<QCol>>,
) -> Result<(), ExecError> {
    match item {
        FromItem::Table { name, alias } => {
            let t = eng.db.find_table(name, snap, own).ok_or_else(|| {
                exec_err("42P01", format!("relation \"{}\" does not exist", name))
            })?;
            let qual = alias.clone().unwrap_or_else(|| name.clone());
            out.push(
                t.columns
                    .iter()
                    .map(|(n, ty)| QCol {
                        qual: qual.clone(),
                        name: n.clone(),
                        ty: ty.clone(),
                    })
                    .collect(),
            );
            Ok(())
        }
        FromItem::Derived { sub, alias } => {
            let cols = describe_select(eng, snap, own, sub)?;
            out.push(
                cols.into_iter()
                    .map(|(n, ty)| QCol {
                        qual: alias.clone(),
                        name: n,
                        ty,
                    })
                    .collect(),
            );
            Ok(())
        }
        FromItem::Join { left, right, .. } => {
            from_schema_item(eng, snap, own, left, out)?;
            from_schema_item(eng, snap, own, right, out)
        }
    }
}

/// (name, type) of every output column. Used by Describe and by execution
/// itself, so the two can never disagree.
fn describe_select(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    stmt: &SelectStmt,
) -> Result<Vec<(String, ColType)>, ExecError> {
    let schemas = from_schemas(eng, snap, own, &stmt.from)?;
    let refs: Vec<&[QCol]> = schemas.iter().map(|s| s.as_slice()).collect();
    let mut out = Vec::new();
    for item in &stmt.items {
        match item {
            SelectItem::All => {
                for s in &schemas {
                    for c in s {
                        out.push((c.name.clone(), c.ty.clone()));
                    }
                }
            }
            SelectItem::AllOf(qual) => {
                let mut any = false;
                for s in &schemas {
                    for c in s {
                        if c.qual == *qual {
                            out.push((c.name.clone(), c.ty.clone()));
                            any = true;
                        }
                    }
                }
                if !any {
                    return Err(exec_err(
                        "42P01",
                        format!("missing FROM-clause entry for table \"{}\"", qual),
                    ));
                }
            }
            SelectItem::Expr { expr, alias } => {
                let ty = expr_type(eng, snap, own, &refs, expr)?;
                let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
                out.push((name, ty));
            }
        }
    }
    Ok(out)
}

/// Default output column name when there is no alias: the column name for
/// bare refs, the function name for aggregates, `?column?` otherwise —
/// like Postgres.
fn expr_col_name(e: &Expr) -> String {
    match e {
        Expr::Column { name, .. } => name.clone(),
        Expr::Agg { func, .. } => func.name().to_string(),
        _ => "?column?".to_string(),
    }
}

fn expr_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    e: &Expr,
) -> Result<ColType, ExecError> {
    match e {
        Expr::Column { table, name } => {
            let scopes: Vec<Scope> = schemas
                .iter()
                .map(|s| Scope {
                    schema: s,
                    row: &[],
                })
                .collect();
            let (si, ci) = resolve_col(&scopes, table.as_deref(), name)?;
            Ok(schemas[si][ci].ty.clone())
        }
        // Pre-resolved: the type lives at the same position in the schemas.
        Expr::ResolvedCol { frame, idx } => schemas
            .get(*frame)
            .and_then(|s| s.get(*idx))
            .map(|c| c.ty.clone())
            .ok_or_else(|| exec_err("XX000", "internal error: resolved column out of range")),
        Expr::Literal(lit) => Ok(lit.col_type()),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Add(a, b) => {
            let ta = add_operand_type(eng, snap, own, schemas, a)?;
            let tb = add_operand_type(eng, snap, own, schemas, b)?;
            combine_add_types(ta, tb)
        }
        Expr::Cmp { .. }
        | Expr::And(_, _)
        | Expr::Or(_, _)
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::InSub { .. }
        | Expr::Exists { .. } => Ok(ColType::Bool),
        Expr::Agg { func, arg } => agg_result_type(eng, snap, own, schemas, *func, arg.as_deref()),
        Expr::ScalarSub(sub) => {
            let cols = describe_select(eng, snap, own, sub)?;
            if cols.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            Ok(cols[1 - 1].clone().1)
        }
    }
}

/// Operand type of `+` for the description pass; NULL contributes nothing.
fn add_operand_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    e: &Expr,
) -> Result<Option<ColType>, ExecError> {
    match e {
        Expr::Literal(Literal::Null) => Ok(None),
        Expr::Literal(lit) => Ok(Some(lit.col_type())),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Column { .. } | Expr::ResolvedCol { .. } => {
            Ok(Some(expr_type(eng, snap, own, schemas, e)?))
        }
        Expr::Add(a, b) => {
            let ta = add_operand_type(eng, snap, own, schemas, a)?;
            let tb = add_operand_type(eng, snap, own, schemas, b)?;
            Ok(Some(combine_add_types(ta, tb)?))
        }
        Expr::Agg { .. } | Expr::ScalarSub(_) => Ok(Some(expr_type(eng, snap, own, schemas, e)?)),
        // Boolean / predicate expressions can't be added.
        _ => Err(exec_err(
            "42883",
            "operator does not exist: boolean + integer",
        )),
    }
}

fn numeric_agg_arg(func: &str, t: &ColType) -> Result<(), ExecError> {
    match t {
        ColType::Int | ColType::Float => Ok(()),
        _ => Err(exec_err(
            "42883",
            format!("function {}({}) does not exist", func, t.sql_name()),
        )),
    }
}

fn agg_result_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    func: AggFunc,
    arg: Option<&Expr>,
) -> Result<ColType, ExecError> {
    match func {
        AggFunc::Count => Ok(ColType::Int),
        AggFunc::Avg => {
            if let Some(a) = arg {
                numeric_agg_arg("avg", &expr_type(eng, snap, own, schemas, a)?)?;
            }
            Ok(ColType::Float)
        }
        AggFunc::Sum => match arg {
            // Only COUNT takes `*`; the parser guarantees it.
            None => Ok(ColType::Int),
            Some(a) => {
                let t = expr_type(eng, snap, own, schemas, a)?;
                numeric_agg_arg("sum", &t)?;
                Ok(t)
            }
        },
        AggFunc::Min | AggFunc::Max => {
            let a = arg.expect("min/max always take an argument");
            expr_type(eng, snap, own, schemas, a)
        }
    }
}

// ---------------------------------------------------------------------------
// v0.2: parameters for the extended query protocol (v0.6: new AST)
// ---------------------------------------------------------------------------

fn pin_param(out: &mut [Option<ColType>], p: u32, t: ColType) -> Result<(), ExecError> {
    let i = (p - 1) as usize;
    match &out[i] {
        Some(existing) if *existing != t => Err(exec_err(
            "42804",
            format!("parameter ${} has conflicting inferred types", p),
        )),
        _ => {
            out[i] = Some(t);
            Ok(())
        }
    }
}

/// Best-effort static type of an expression for parameter inference.
/// Returns None when the type isn't pinned down (params, NULLs); never
/// fails — execution reports the real errors.
fn hint_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    e: &Expr,
) -> Option<ColType> {
    match e {
        Expr::Literal(Literal::Null) | Expr::Param(_) => None,
        Expr::Literal(lit) => Some(lit.col_type()),
        Expr::Column { .. } | Expr::ResolvedCol { .. } => {
            let scopes: Vec<Scope> = schemas
                .iter()
                .map(|s| Scope {
                    schema: s,
                    row: &[],
                })
                .collect();
            let (si, ci) = match e {
                Expr::Column { table, name } => {
                    resolve_col(&scopes, table.as_deref(), name).ok()?
                }
                Expr::ResolvedCol { frame, idx } => (*frame, *idx),
                _ => return None,
            };
            // A resolved position may exceed these schemas (inference runs
            // on shapes the resolver never saw); treat as unknown, not fatal.
            let c = schemas.get(si)?.get(ci)?;
            Some(c.ty.clone())
        }
        Expr::Add(a, b) => combine_add_types(
            hint_type(eng, snap, own, schemas, a),
            hint_type(eng, snap, own, schemas, b),
        )
        .ok(),
        Expr::Agg { func, arg } => {
            agg_result_type(eng, snap, own, schemas, *func, arg.as_deref()).ok()
        }
        Expr::ScalarSub(sub) => {
            let cols = describe_select(eng, snap, own, sub).ok()?;
            if cols.len() == 1 {
                Some(cols[0].1.clone())
            } else {
                None
            }
        }
        // Predicates are boolean, but that never usefully pins a param.
        _ => None,
    }
}

fn infer_expr(
    e: &Expr,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    out: &mut [Option<ColType>],
) -> Result<(), ExecError> {
    match e {
        Expr::Add(a, b) => {
            infer_expr(a, eng, snap, own, schemas, out)?;
            infer_expr(b, eng, snap, own, schemas, out)?;
            // One side a param, the other side typed: pin the param.
            for (p_side, o_side) in [(a, b), (b, a)] {
                if let Expr::Param(p) = **p_side {
                    if let Some(t) = hint_type(eng, snap, own, schemas, o_side) {
                        pin_param(out, p, t)?;
                    }
                }
            }
            // Both sides params with no other info: default to integer
            // (documented v0.2 inference rule for `$1 + $2`).
            if matches!(**a, Expr::Param(_)) && matches!(**b, Expr::Param(_)) {
                for p_side in [a, b] {
                    if let Expr::Param(p) = **p_side {
                        let i = (p as usize) - 1;
                        if out[i].is_none() {
                            out[i] = Some(ColType::Int);
                        }
                    }
                }
            }
            Ok(())
        }
        Expr::Cmp { left, right, .. } => {
            infer_expr(left, eng, snap, own, schemas, out)?;
            infer_expr(right, eng, snap, own, schemas, out)?;
            // `col = $N` pins the param to the column's type.
            if let Expr::Param(p) = **left {
                if let Some(t) = hint_type(eng, snap, own, schemas, right) {
                    pin_param(out, p, t)?;
                }
            }
            if let Expr::Param(p) = **right {
                if let Some(t) = hint_type(eng, snap, own, schemas, left) {
                    pin_param(out, p, t)?;
                }
            }
            Ok(())
        }
        Expr::And(a, b) | Expr::Or(a, b) => {
            infer_expr(a, eng, snap, own, schemas, out)?;
            infer_expr(b, eng, snap, own, schemas, out)
        }
        Expr::Not(x) | Expr::IsNull { expr: x, .. } => infer_expr(x, eng, snap, own, schemas, out),
        Expr::Agg { arg, .. } => {
            if let Some(a) = arg {
                infer_expr(a, eng, snap, own, schemas, out)?;
            }
            Ok(())
        }
        Expr::ScalarSub(sub) => infer_select(sub, eng, snap, own, schemas, out),
        Expr::InSub { expr, sub, .. } => {
            infer_expr(expr, eng, snap, own, schemas, out)?;
            infer_select(sub, eng, snap, own, schemas, out)?;
            // `$N IN (SELECT col ...)` pins the param to the column type.
            if let Expr::Param(p) = **expr {
                if let Ok(cols) = describe_select(eng, snap, own, sub) {
                    if cols.len() == 1 {
                        pin_param(out, p, cols[0].1.clone())?;
                    }
                }
            }
            Ok(())
        }
        Expr::Exists { sub, .. } => infer_select(sub, eng, snap, own, schemas, out),
        _ => Ok(()),
    }
}

fn infer_from(
    f: &FromItem,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    out: &mut [Option<ColType>],
) -> Result<(), ExecError> {
    match f {
        FromItem::Table { .. } => Ok(()),
        // Derived tables are uncorrelated (no LATERAL): no outer schemas.
        FromItem::Derived { sub, .. } => infer_select(sub, eng, snap, own, &[], out),
        FromItem::Join {
            left, right, on, ..
        } => {
            infer_from(left, eng, snap, own, schemas, out)?;
            infer_from(right, eng, snap, own, schemas, out)?;
            // ON params resolve against the enclosing query's combined
            // schemas (a superset is fine: pinning only fires on
            // unambiguous matches).
            if let Some(p) = on {
                infer_expr(p, eng, snap, own, schemas, out)?;
            }
            Ok(())
        }
    }
}

/// `outer` is the enclosing query's schema chain (for correlated
/// subqueries); this query's own FROM schemas are appended after it.
fn infer_select(
    s: &SelectStmt,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    outer: &[&[QCol]],
    out: &mut [Option<ColType>],
) -> Result<(), ExecError> {
    // Unknown tables are skipped (execution reports them).
    let own_schemas = from_schemas(eng, snap, own, &s.from).unwrap_or_default();
    let mut refs: Vec<&[QCol]> = Vec::with_capacity(outer.len() + own_schemas.len());
    refs.extend_from_slice(outer);
    refs.extend(own_schemas.iter().map(|s| s.as_slice()));
    for f in &s.from {
        infer_from(f, eng, snap, own, &refs, out)?;
    }
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            infer_expr(expr, eng, snap, own, &refs, out)?;
        }
    }
    if let Some(w) = &s.where_ {
        infer_expr(w, eng, snap, own, &refs, out)?;
    }
    for g in &s.group_by {
        infer_expr(g, eng, snap, own, &refs, out)?;
    }
    if let Some(h) = &s.having {
        infer_expr(h, eng, snap, own, &refs, out)?;
    }
    for o in &s.order_by {
        infer_expr(&o.expr, eng, snap, own, &refs, out)?;
    }
    Ok(())
}

fn infer_where(
    where_: &[WhereCond],
    tbl: Option<&crate::storage::Table>,
    out: &mut [Option<ColType>],
) -> Result<(), ExecError> {
    for w in where_ {
        if let WhereRhs::Param(p) = w.rhs {
            if let Some(t) = tbl {
                if let Some(i) = t.column_index(&w.col) {
                    pin_param(out, p, t.columns[i].1.clone())?;
                }
            }
        }
    }
    Ok(())
}

/// Look up the table visible to (`snap`, `own`) for parameter inference.
/// Unknown tables are skipped (execution reports them); this mirrors the
/// old "resolve against the session's visible database" behavior.
fn infer_table<'e>(
    eng: &'e Engine,
    name: &Option<String>,
    snap: &Snapshot,
    own: u64,
) -> Option<&'e crate::storage::Table> {
    match name {
        Some(n) => eng.db.find_table(n, snap, own),
        None => None,
    }
}

/// Infer each `$N`'s type from usage context (one entry per param, 1-based).
/// `WHERE col = $N` pins the column's type; `$N + <typed>` pins the other
/// side's type; `$N + $M` with no other info defaults both to integer.
/// In `INSERT ... VALUES ($N, ...)` and `UPDATE ... SET col = $N`, a param
/// takes its target column's type.
/// Params with no constraint stay None (callers default them to text).
pub fn infer_param_types(
    stmt: &Stmt,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
) -> Result<Vec<Option<ColType>>, ExecError> {
    let n = stmt.max_param();
    let mut out: Vec<Option<ColType>> = vec![None; n];
    if let Stmt::Insert {
        table,
        columns,
        rows,
    } = stmt
    {
        if let Some(t) = eng.db.find_table(table, snap, own) {
            // Unknown column names are skipped here; execution reports them.
            let targets: Vec<usize> = match columns {
                Some(names) => names.iter().filter_map(|n| t.column_index(n)).collect(),
                None => (0..t.columns.len()).collect(),
            };
            for row in rows {
                for (v, &ci) in row.iter().zip(targets.iter()) {
                    if let InsertValue::Param(p) = v {
                        if let Some((_, ty)) = t.columns.get(ci) {
                            pin_param(&mut out, *p, ty.clone())?;
                        }
                    }
                }
            }
        }
    }
    if let Stmt::Update {
        table,
        sets,
        where_,
    } = stmt
    {
        if let Some(t) = eng.db.find_table(table, snap, own) {
            let schemas: Vec<Vec<QCol>> = vec![
                t.columns
                    .iter()
                    .map(|(n, ty)| QCol {
                        qual: String::new(),
                        name: n.clone(),
                        ty: ty.clone(),
                    })
                    .collect(),
            ];
            let refs: Vec<&[QCol]> = schemas.iter().map(|s| s.as_slice()).collect();
            for (col, expr) in sets {
                if let Expr::Param(p) = expr {
                    if let Some(i) = t.column_index(col) {
                        pin_param(&mut out, *p, t.columns[i].1.clone())?;
                    }
                } else {
                    infer_expr(expr, eng, snap, own, &refs, &mut out)?;
                }
            }
            infer_where(where_, Some(t), &mut out)?;
        }
    }
    match stmt {
        Stmt::Select(sel) => {
            infer_select(sel, eng, snap, own, &[], &mut out)?;
        }
        Stmt::Delete { table, where_ } => {
            let tbl = infer_table(eng, &Some(table.clone()), snap, own);
            infer_where(where_, tbl, &mut out)?;
        }
        _ => {}
    }
    Ok(out)
}

fn oid_to_coltype(oid: i32, param_no: usize) -> Result<Option<ColType>, ExecError> {
    match oid {
        0 => Ok(None), // unknown: infer
        23 => Ok(Some(ColType::Int)),
        25 => Ok(Some(ColType::Text)),
        16 => Ok(Some(ColType::Bool)),
        701 => Ok(Some(ColType::Float)),
        _ => Err(exec_err(
            "42804",
            format!("unsupported type OID {} for parameter ${}", oid, param_no),
        )),
    }
}

/// Effective type of every `$N`: declared OID wins, else inferred, else text.
pub fn resolve_param_types(
    stmt: &Stmt,
    declared: &[i32],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
) -> Result<Vec<ColType>, ExecError> {
    let inferred = infer_param_types(stmt, eng, snap, own)?;
    let n = stmt.max_param();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let oid = declared.get(i).copied().unwrap_or(0);
        let dt = oid_to_coltype(oid, i + 1)?;
        match (dt, &inferred[i]) {
            (Some(d), Some(f)) if d != *f => {
                return Err(exec_err(
                    "42804",
                    format!(
                        "parameter ${} is of type {} but expression is of type {}",
                        i + 1,
                        d.sql_name(),
                        f.sql_name()
                    ),
                ));
            }
            (Some(d), _) => out.push(d),
            (None, Some(f)) => out.push(f.clone()),
            (None, None) => out.push(ColType::Text),
        }
    }
    Ok(out)
}

/// Parse text-format Bind parameters into typed values (`None` = SQL NULL).
/// Failures are 22P02 (bad literal) like Postgres.
pub fn bind_params(
    stmt: &Stmt,
    declared: &[i32],
    raw: &[Option<Vec<u8>>],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
) -> Result<Vec<Option<Value>>, ExecError> {
    let types = resolve_param_types(stmt, declared, eng, snap, own)?;
    let mut out = Vec::with_capacity(types.len());
    for (i, t) in types.iter().enumerate() {
        let r = raw.get(i).ok_or_else(|| {
            exec_err(
                "08P01",
                "bind message supplies fewer parameters than required",
            )
        })?;
        match r {
            None => out.push(None),
            Some(bytes) => out.push(Some(parse_param_value(bytes, t, i + 1)?)),
        }
    }
    Ok(out)
}

fn parse_param_value(bytes: &[u8], t: &ColType, n: usize) -> Result<Value, ExecError> {
    let bad = |msg: String| {
        exec_err(
            "22P02",
            format!("invalid input syntax for type {}: {}", t.sql_name(), msg),
        )
    };
    match t {
        ColType::Text => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| exec_err("22021", "invalid byte sequence for encoding \"UTF8\""))?;
            Ok(Value::Text(s.to_string()))
        }
        ColType::Int => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            s.trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Float => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            s.trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Bool => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            match s.trim().to_lowercase().as_str() {
                "t" | "true" | "1" | "yes" | "y" | "on" => Ok(Value::Bool(true)),
                "f" | "false" | "0" | "no" | "n" | "off" => Ok(Value::Bool(false)),
                _ => Err(bad(format!("\"{}\"", s))),
            }
        }
    }
    .map_err(|e| {
        // Keep the parameter number in the message for debuggability.
        ExecError {
            code: e.code,
            message: format!("parameter ${}: {}", n, e.message),
        }
    })
}

/// Replace every `$N` in the statement with its bound value's literal.
/// Unbound `$N` (no value supplied) is 42P02, like Postgres.
pub fn subst_params(stmt: &mut Stmt, params: &[Option<Value>]) -> Result<(), ExecError> {
    match stmt {
        Stmt::Insert { rows, .. } => {
            for row in rows {
                for v in row {
                    if let InsertValue::Param(p) = v {
                        *v = InsertValue::Lit(param_literal(*p, params)?);
                    }
                }
            }
            Ok(())
        }
        Stmt::Select(sel) => subst_select(sel, params),
        Stmt::Update { sets, where_, .. } => {
            for (_, e) in sets {
                subst_expr(e, params)?;
            }
            subst_where(where_, params)
        }
        Stmt::Delete { where_, .. } => subst_where(where_, params),
        _ => Ok(()),
    }
}

fn subst_select(s: &mut SelectStmt, params: &[Option<Value>]) -> Result<(), ExecError> {
    for item in &mut s.items {
        if let SelectItem::Expr { expr, .. } = item {
            subst_expr(expr, params)?;
        }
    }
    for f in &mut s.from {
        subst_from(f, params)?;
    }
    if let Some(w) = &mut s.where_ {
        subst_expr(w, params)?;
    }
    for g in &mut s.group_by {
        subst_expr(g, params)?;
    }
    if let Some(h) = &mut s.having {
        subst_expr(h, params)?;
    }
    for o in &mut s.order_by {
        subst_expr(&mut o.expr, params)?;
    }
    Ok(())
}

fn subst_from(f: &mut FromItem, params: &[Option<Value>]) -> Result<(), ExecError> {
    match f {
        FromItem::Table { .. } => Ok(()),
        FromItem::Derived { sub, .. } => subst_select(sub, params),
        FromItem::Join {
            left, right, on, ..
        } => {
            subst_from(left, params)?;
            subst_from(right, params)?;
            if let Some(p) = on {
                subst_expr(p, params)?;
            }
            Ok(())
        }
    }
}

fn subst_where(where_: &mut [WhereCond], params: &[Option<Value>]) -> Result<(), ExecError> {
    for w in where_ {
        if let WhereRhs::Param(p) = w.rhs {
            w.rhs = WhereRhs::Lit(param_literal(p, params)?);
        }
    }
    Ok(())
}

fn param_literal(p: u32, params: &[Option<Value>]) -> Result<Literal, ExecError> {
    let v = params
        .get((p - 1) as usize)
        .ok_or_else(|| exec_err("42P02", format!("there is no parameter ${}", p)))?;
    Ok(match v {
        None => Literal::Null,
        Some(Value::Int(i)) => Literal::Int(*i),
        Some(Value::Float(f)) => Literal::Float(*f),
        Some(Value::Text(s)) => Literal::Text(s.clone()),
        Some(Value::Bool(b)) => Literal::Bool(*b),
        Some(Value::Null) => Literal::Null,
    })
}

fn subst_expr(e: &mut Expr, params: &[Option<Value>]) -> Result<(), ExecError> {
    match e {
        Expr::Param(p) => {
            *e = Expr::Literal(param_literal(*p, params)?);
        }
        Expr::Add(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            subst_expr(a, params)?;
            subst_expr(b, params)?;
        }
        Expr::Cmp { left, right, .. } => {
            subst_expr(left, params)?;
            subst_expr(right, params)?;
        }
        Expr::Not(x) | Expr::IsNull { expr: x, .. } => subst_expr(x, params)?,
        Expr::Agg { arg, .. } => {
            if let Some(a) = arg {
                subst_expr(a, params)?;
            }
        }
        Expr::ScalarSub(s) => subst_select(s, params)?,
        Expr::InSub { expr, sub, .. } => {
            subst_expr(expr, params)?;
            subst_select(sub, params)?;
        }
        Expr::Exists { sub, .. } => subst_select(sub, params)?,
        _ => {}
    }
    Ok(())
}

fn dummy_value(t: &ColType) -> Value {
    match t {
        ColType::Int => Value::Int(0),
        ColType::Float => Value::Float(0.0),
        ColType::Text => Value::Text(String::new()),
        ColType::Bool => Value::Bool(false),
    }
}

/// Result columns for Describe: (name, type) per output column, or None
/// (→ NoData) for non-SELECT / empty statements. Parameters are replaced
/// with dummy values of their effective type so the normal description
/// path can type the output columns.
pub fn describe_columns(
    stmt: &Stmt,
    declared: &[i32],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
) -> Result<Option<Vec<(String, ColType)>>, ExecError> {
    match stmt {
        Stmt::Select(sel) => {
            let eff = resolve_param_types(stmt, declared, eng, snap, own)?;
            let mut s = sel.clone();
            let dummy: Vec<Option<Value>> = eff.iter().map(|t| Some(dummy_value(t))).collect();
            subst_select(&mut s, &dummy)?;
            Ok(Some(describe_select(eng, snap, own, &s)?))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parse_statement;
    use crate::storage::Table;

    /// Engine with two small tables, committed (next_xid past them).
    fn engine() -> Engine {
        let mut eng = Engine::new();
        // users(id int, name text): (1,'ann'), (2,'bob'), (3,'cid')
        let mut users = Table::new(
            vec![
                ("id".to_string(), ColType::Int),
                ("name".to_string(), ColType::Text),
            ],
            1,
        );
        for (i, id, name) in [(1, 1, "ann"), (2, 2, "bob"), (3, 3, "cid")] {
            users.push_version(RowVersion {
                id: i,
                values: vec![Value::Int(id), Value::Text(name.into())],
                xmin: 1,
                xmax: 0,
            });
        }
        // orders(id int, uid int, amt int): (1,1,10), (2,1,20), (3,2,5)
        let mut orders = Table::new(
            vec![
                ("id".to_string(), ColType::Int),
                ("uid".to_string(), ColType::Int),
                ("amt".to_string(), ColType::Int),
            ],
            1,
        );
        for (i, id, uid, amt) in [(4, 1, 1, 10), (5, 2, 1, 20), (6, 3, 2, 5)] {
            orders.push_version(RowVersion {
                id: i,
                values: vec![Value::Int(id), Value::Int(uid), Value::Int(amt)],
                xmin: 1,
                xmax: 0,
            });
        }
        eng.db.tables.insert("users".to_string(), vec![users]);
        eng.db.tables.insert("orders".to_string(), vec![orders]);
        eng.txns.next_xid = 10;
        eng.txns.next_row_id = 7;
        eng
    }

    /// Parse + execute a statement as one autocommit-ish step (own xid 9,
    /// fresh snapshot). Returns the rows as debug strings.
    fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
        let stmt = parse_statement(sql).map_err(|e| exec_err("42601", e.message))?;
        let snap = eng.take_snapshot();
        let mut writes = Vec::new();
        let mut ctx = StmtCtx {
            snap: &snap,
            own: 9,
            level: IsolationLevel::ReadCommitted,
            writes: &mut writes,
        };
        execute(eng, &mut ctx, &stmt)
    }

    fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
        match r {
            ExecResult::Select { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                        .collect()
                })
                .collect(),
            ExecResult::Command { tag } => vec![vec![tag]],
        }
    }

    #[test]
    fn inner_join_on_equality() {
        let mut eng = engine();
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT users.name, orders.amt FROM users JOIN orders ON users.id = orders.uid ORDER BY orders.amt",
            )
            .unwrap(),
        );
        assert_eq!(
            rows,
            vec![
                vec!["bob".to_string(), "5".to_string()],
                vec!["ann".to_string(), "10".to_string()],
                vec!["ann".to_string(), "20".to_string()],
            ]
        );
    }

    #[test]
    fn left_join_keeps_unmatched() {
        let mut eng = engine();
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT users.name, orders.amt FROM users LEFT JOIN orders ON users.id = orders.uid ORDER BY users.id, orders.amt",
            )
            .unwrap(),
        );
        assert_eq!(
            rows,
            vec![
                vec!["ann".to_string(), "10".to_string()],
                vec!["ann".to_string(), "20".to_string()],
                vec!["bob".to_string(), "5".to_string()],
                vec!["cid".to_string(), "NULL".to_string()],
            ]
        );
    }

    #[test]
    fn ambiguous_column_is_42702() {
        let mut eng = engine();
        let e = run(
            &mut eng,
            "SELECT id FROM users JOIN orders ON users.id = orders.uid",
        )
        .unwrap_err();
        assert_eq!(e.code, "42702");
    }

    #[test]
    fn aggregates_and_group_by() {
        let mut eng = engine();
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT uid, count(*), sum(amt), avg(amt), min(amt), max(amt) FROM orders GROUP BY uid ORDER BY uid",
            )
            .unwrap(),
        );
        assert_eq!(
            rows,
            vec![
                vec!["1", "2", "30", "15", "10", "20"],
                vec!["2", "1", "5", "5", "5", "5"],
            ]
            .into_iter()
            .map(|r: Vec<&str>| r.into_iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn having_filters_groups() {
        let mut eng = engine();
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT uid, count(*) FROM orders GROUP BY uid HAVING count(*) > 1",
            )
            .unwrap(),
        );
        assert_eq!(rows, vec![vec!["1".to_string(), "2".to_string()]]);
    }

    #[test]
    fn scalar_subquery_in_select() {
        let mut eng = engine();
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT name, (SELECT count(*) FROM orders WHERE orders.uid = users.id) FROM users ORDER BY id",
            )
            .unwrap(),
        );
        assert_eq!(
            rows,
            vec![
                vec!["ann".to_string(), "2".to_string()],
                vec!["bob".to_string(), "1".to_string()],
                vec!["cid".to_string(), "0".to_string()],
            ]
        );
    }

    #[test]
    fn in_subquery_and_exists() {
        let mut eng = engine();
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT name FROM users WHERE id IN (SELECT uid FROM orders) ORDER BY id",
            )
            .unwrap(),
        );
        assert_eq!(rows, vec![vec!["ann".to_string()], vec!["bob".to_string()]]);
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.uid = u.id AND o.amt > 15)",
            )
            .unwrap(),
        );
        assert_eq!(rows, vec![vec!["ann".to_string()]]);
    }

    #[test]
    fn derived_table_in_from() {
        let mut eng = engine();
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT t.uid, t.total FROM (SELECT uid, sum(amt) AS total FROM orders GROUP BY uid) t WHERE t.total > 10 ORDER BY t.uid",
            )
            .unwrap(),
        );
        assert_eq!(rows, vec![vec!["1".to_string(), "30".to_string()]]);
    }

    #[test]
    fn distinct_and_offset() {
        let mut eng = engine();
        let rows = rows_of(run(&mut eng, "SELECT DISTINCT uid FROM orders ORDER BY uid").unwrap());
        assert_eq!(rows, vec![vec!["1".to_string()], vec!["2".to_string()]]);
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT id FROM orders ORDER BY id LIMIT 2 OFFSET 1",
            )
            .unwrap(),
        );
        assert_eq!(rows, vec![vec!["2".to_string()], vec!["3".to_string()]]);
    }

    #[test]
    fn for_update_locks_and_conflicts() {
        let mut eng = engine();
        // Real transaction xids (registered in txns.active).
        let x1 = eng.begin_txn();
        let x2 = eng.begin_txn();
        // x1 locks users/1 via FOR UPDATE.
        let snap = eng.take_snapshot();
        let mut writes = Vec::new();
        let mut ctx = StmtCtx {
            snap: &snap,
            own: x1,
            level: IsolationLevel::ReadCommitted,
            writes: &mut writes,
        };
        let sel = parse_statement("SELECT * FROM users WHERE id = 1 FOR UPDATE").unwrap();
        execute(&mut eng, &mut ctx, &sel).unwrap();
        assert_eq!(eng.row_lock_holder(1), Some(x1));
        // x2 writing that row gets 40001 (fail fast, no waiting).
        let snap2 = eng.take_snapshot();
        let mut writes2 = Vec::new();
        let mut ctx2 = StmtCtx {
            snap: &snap2,
            own: x2,
            level: IsolationLevel::ReadCommitted,
            writes: &mut writes2,
        };
        let upd = parse_statement("UPDATE users SET name = 'x' WHERE id = 1").unwrap();
        let e = execute(&mut eng, &mut ctx2, &upd).unwrap_err();
        assert_eq!(e.code, "40001");
        // x2 can still write a different, unlocked row.
        let upd2 = parse_statement("UPDATE users SET name = 'y' WHERE id = 2").unwrap();
        execute(&mut eng, &mut ctx2, &upd2).unwrap();
        // Locks release on transaction end.
        eng.release_txn_locks(x1);
        eng.end_txn(x1);
        assert_eq!(eng.row_lock_holder(1), None);
        // ...and now x2's write to row 1 succeeds.
        let upd3 = parse_statement("UPDATE users SET name = 'z' WHERE id = 1").unwrap();
        execute(&mut eng, &mut ctx2, &upd3).unwrap();
        eng.end_txn(x2);
    }

    #[test]
    fn null_semantics_in_predicates() {
        let mut eng = engine();
        // cid has no orders: LEFT JOIN gives NULL amt; NULL comparisons
        // are not true.
        let rows = rows_of(
            run(
                &mut eng,
                "SELECT users.name FROM users LEFT JOIN orders ON users.id = orders.uid WHERE orders.amt > 100 OR orders.amt IS NULL ORDER BY users.id",
            )
            .unwrap(),
        );
        assert_eq!(rows, vec![vec!["cid".to_string()]]);
    }

    #[test]
    fn update_still_works() {
        let mut eng = engine();
        match run(&mut eng, "UPDATE users SET name = 'ann2' WHERE id = 1").unwrap() {
            ExecResult::Command { tag } => assert_eq!(tag, "UPDATE 1"),
            _ => panic!("expected command"),
        }
        let rows = rows_of(run(&mut eng, "SELECT name FROM users WHERE id = 1").unwrap());
        assert_eq!(rows, vec![vec!["ann2".to_string()]]);
    }
}
