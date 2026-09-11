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

use crate::index::{Index, IndexDef, IndexKey, index_key_cmp};
use crate::sql::{
    AggFunc, AlterAction, ArithOp, CheckDef, CmpOp, ConflictAction, ConflictArbiter, CteBody,
    CteDef, DefaultExpr, Expr, FkAction, FkDef, FrameBound, FromItem, InsertValue, IsolationLevel,
    JoinKind, Literal, OnConflict, OrderTerm, SelectItem, SelectStmt, SequenceOpts, SqlError, Stmt,
    TableDef, UniqueDef, WhereCond, WhereRhs, WindowFrame, WindowFunc, collect_col_refs,
    collect_table_refs, parse_statement, validate_constraint_expr,
};
use crate::storage::{
    ColStats, ColType, Database, Engine, Numeric, RowVersion, Sequence, Snapshot, Table,
    TableStats, Value, ViewDef, WriteOp, row_visible,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::rc::Rc;

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

/// Convert a SQL-layer validation error into an execution error.
fn sql_err(e: SqlError) -> ExecError {
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
}

/// Outcome of executing one statement.
#[derive(Debug)]
pub enum ExecResult {
    /// Rows to return: (column name, column type) + row values.
    Select {
        columns: Vec<(String, ColType)>,
        rows: Vec<Vec<Value>>,
    },
    /// EXPLAIN output: same shape as Select but completes with the
    /// "EXPLAIN" tag, like Postgres.
    Explain {
        columns: Vec<(String, ColType)>,
        rows: Vec<Vec<Value>>,
    },
    /// Tag for CommandComplete, e.g. "INSERT 0 2".
    Command { tag: String },
    /// v0.10: INSERT/UPDATE/DELETE — always carries the completion tag;
    /// with RETURNING it also carries the result rows (empty otherwise,
    /// behaving exactly like Command on the wire).
    Dml {
        tag: String,
        columns: Vec<(String, ColType)>,
        rows: Vec<Vec<Value>>,
    },
}

// ---------------------------------------------------------------------------
// v0.11: privilege checks. Denials are SQLSTATE 42501
// (insufficient_privilege), like PostgreSQL.
// ---------------------------------------------------------------------------

/// Require superuser (for role management).
fn require_superuser(eng: &Engine, ctx: &StmtCtx) -> Result<(), ExecError> {
    if crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        Ok(())
    } else {
        Err(exec_err(
            "42501",
            "permission denied: must be superuser to manage roles".to_string(),
        ))
    }
}

/// Require `privs` on a table. Missing tables are left alone so the
/// caller still reports 42P01 (like PostgreSQL, existence is checked
/// before privileges).
fn require_table_priv(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    privs: u32,
    priv_name: &str,
) -> Result<(), ExecError> {
    if let Some(t) = eng.db.find_table(table, ctx.snap, ctx.own) {
        let have = crate::storage::table_privs(&eng.db, ctx.role, t, ctx.snap, ctx.own);
        if have & privs != privs {
            return Err(exec_err(
                "42501",
                format!(
                    "permission denied for table \"{}\" (needs {})",
                    table, priv_name
                ),
            ));
        }
    }
    Ok(())
}

/// v0.11: require `privs` on each of `columns`, satisfied by a
/// table-level grant or a column-level grant. Unknown columns are
/// skipped — the statement's own validation reports them as 42703.
fn require_column_privs(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    columns: &[String],
    privs: u32,
    priv_name: &str,
) -> Result<(), ExecError> {
    if let Some(t) = eng.db.find_table(table, ctx.snap, ctx.own) {
        let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
        for c in columns {
            if !t.columns.iter().any(|(n, _)| n == c) {
                continue;
            }
            let have = crate::storage::column_privs_in(
                &eng.db, ctx.role, t, c, &closure, ctx.snap, ctx.own,
            );
            if have & privs != privs {
                return Err(exec_err(
                    "42501",
                    format!(
                        "permission denied for column \"{}\" of table \"{}\" (needs {})",
                        c, table, priv_name
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Require owner-or-superuser on a table (DDL).
fn require_table_owner(eng: &Engine, ctx: &StmtCtx, table: &str) -> Result<(), ExecError> {
    if let Some(t) = eng.db.find_table(table, ctx.snap, ctx.own) {
        if t.owner != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!("permission denied: must be owner of table \"{}\"", table),
            ));
        }
    }
    Ok(())
}

/// Require owner-or-superuser on a sequence (DDL).
fn require_seq_owner(eng: &Engine, ctx: &StmtCtx, seq: &str) -> Result<(), ExecError> {
    if let Some(s) = eng.db.find_sequence(seq, ctx.snap, ctx.own) {
        if s.owner != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!("permission denied: must be owner of sequence \"{}\"", seq),
            ));
        }
    }
    Ok(())
}

/// Require owner-or-superuser on a view (DDL).
fn require_view_owner(eng: &Engine, ctx: &StmtCtx, view: &str) -> Result<(), ExecError> {
    if let Some(v) = eng.db.find_view(view, ctx.snap, ctx.own) {
        if v.owner != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!("permission denied: must be owner of view \"{}\"", view),
            ));
        }
    }
    Ok(())
}

pub fn execute(eng: &mut Engine, ctx: &mut StmtCtx, stmt: &Stmt) -> Result<ExecResult, ExecError> {
    match stmt {
        Stmt::CreateTable { name, def } => exec_create(eng, ctx, name, def),
        Stmt::Insert {
            table,
            columns,
            rows,
            select,
            with,
            on_conflict,
            returning,
        } => exec_insert(
            eng,
            ctx,
            table,
            columns,
            rows,
            select,
            with,
            on_conflict,
            returning,
        ),
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
                    session: ctx.session,
                    role: ctx.role,
                    read_only: ctx.read_only,
                    depth: 0,
                    lock_ids: &mut lock_ids,
                    ctes: Vec::new(),
                    wctx: None,
                    priv_scopes: Vec::new(),
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
        Stmt::DropTable {
            if_exists,
            names,
            cascade,
        } => exec_drop(eng, ctx, names, *if_exists, *cascade),
        // --- v0.9: constraints / ALTER / views / sequences
        Stmt::AlterTable { name, action } => exec_alter(eng, ctx, name, action),
        Stmt::CreateView {
            name,
            query,
            col_aliases,
            or_replace,
        } => exec_create_view(eng, ctx, name, query, col_aliases, *or_replace),
        Stmt::DropView {
            names,
            if_exists,
            cascade,
        } => exec_drop_view(eng, ctx, names, *if_exists, *cascade),
        Stmt::CreateSequence {
            name,
            if_not_exists,
            opts,
        } => exec_create_sequence(eng, ctx, name, *if_not_exists, opts),
        Stmt::AlterSequence { name, opts } => exec_alter_sequence(eng, ctx, name, opts),
        Stmt::DropSequence { names, if_exists } => exec_drop_sequence(eng, ctx, names, *if_exists),
        // --- v0.11: roles and privileges ---
        Stmt::CreateRole {
            name,
            login,
            superuser,
            password,
            connlimit,
            valid_until,
        } => exec_create_role(
            eng,
            ctx,
            name,
            *login,
            *superuser,
            password,
            *connlimit,
            valid_until,
        ),
        Stmt::AlterRole {
            name,
            login,
            superuser,
            password,
            connlimit,
            valid_until,
        } => exec_alter_role(
            eng,
            ctx,
            name,
            *login,
            *superuser,
            password,
            *connlimit,
            valid_until,
        ),
        Stmt::DropRole { names, if_exists } => exec_drop_role(eng, ctx, names, *if_exists),
        Stmt::Grant {
            privs,
            object,
            grantees,
        } => exec_grant_revoke(eng, ctx, privs, object, grantees, false),
        Stmt::Revoke {
            privs,
            object,
            grantees,
        } => exec_grant_revoke(eng, ctx, privs, object, grantees, true),
        Stmt::GrantRole { roles, grantees } => exec_grant_role(eng, ctx, roles, grantees),
        Stmt::RevokeRole { roles, grantees } => exec_revoke_role(eng, ctx, roles, grantees),
        // --- v0.8: indexes / EXPLAIN / ANALYZE
        Stmt::CreateIndex {
            name,
            table,
            columns,
            unique,
            if_not_exists,
        } => exec_create_index(eng, ctx, name, table, columns, *unique, *if_not_exists),
        Stmt::DropIndex { name, if_exists } => exec_drop_index(eng, ctx, name, *if_exists),
        Stmt::Explain { stmt } => exec_explain(eng, ctx, stmt),
        Stmt::Analyze { table } => exec_analyze(eng, ctx, table),
        Stmt::Update {
            table,
            sets,
            where_,
            with,
            returning,
        } => exec_update(eng, ctx, table, sets, where_, with, returning),
        Stmt::Delete {
            table,
            where_,
            with,
            returning,
        } => exec_delete(eng, ctx, table, where_, with, returning),
        // v0.16: TRUNCATE is executor-level (transactional row removal).
        Stmt::Truncate {
            tables,
            restart_identity,
            cascade,
        } => exec_truncate(eng, ctx, tables, *restart_identity, *cascade),
        // v0.16: cursor statements are session-level (server.rs owns the
        // cursor map); reaching the executor is a bug in the session
        // layer.
        Stmt::Declare { .. } | Stmt::Fetch { .. } | Stmt::Close { .. } | Stmt::Move { .. } => {
            Err(exec_err(
                "XX000",
                "internal error: cursor statement reached the statement executor",
            ))
        }
        // v0.10: COPY is handled by the server layer (it needs the raw
        // frontend messages); reaching the executor is a bug.
        Stmt::Copy { .. } => Err(exec_err(
            "XX000",
            "internal error: COPY reached the statement executor",
        )),
        // Transaction control / checkpoint / vacuum never reach the
        // executor (server.rs intercepts them); reaching here is a bug in
        // the session layer.
        Stmt::Begin { .. }
        | Stmt::Commit { .. }
        | Stmt::Rollback { .. }
        | Stmt::Savepoint { .. }
        | Stmt::RollbackTo { .. }
        | Stmt::Release { .. }
        | Stmt::Checkpoint
        | Stmt::Vacuum { .. }
        // v0.17: SET/SHOW/RESET and transaction-characteristic statements
        // are intercepted by server.rs; reaching here is a session bug.
        | Stmt::SetTransaction { .. }
        | Stmt::SetSessionCharacteristics { .. }
        | Stmt::Set { .. }
        | Stmt::Show { .. }
        | Stmt::Reset { .. } => Err(exec_err(
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
    def: &TableDef,
) -> Result<ExecResult, ExecError> {
    if eng.db.find_table(name, ctx.snap, ctx.own).is_some()
        || eng.db.find_view(name, ctx.snap, ctx.own).is_some()
    {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    // NOTE: a concurrent uncommitted CREATE of the same name is allowed
    // here (it is invisible to us); the commit-time check in server.rs
    // rejects the second committer with 40001.
    let mut t = Table::with_def(def, ctx.own);
    // v0.11: the creating role owns the table.
    t.owner = ctx.role.to_string();
    eng.db.tables.entry(name.to_string()).or_default().push(t);
    ctx.writes.push(WriteOp::CreateTable {
        name: name.to_string(),
    });
    // Validate foreign keys after the table exists so self-references
    // resolve. On failure the statement aborts and undo removes the
    // table (statement atomicity).
    for fk in &def.fks {
        validate_fk_def(eng, ctx, name, fk)?;
    }
    // Backing unique indexes for PRIMARY KEY / UNIQUE constraints. The
    // table is empty, so no duplicate check is needed.
    if let Some(pk) = &def.pkey {
        create_constraint_index(eng, ctx, name, &pk.name, &pk.cols, true)?;
    }
    for u in &def.uniques {
        create_constraint_index(eng, ctx, name, &u.name, &u.cols, true)?;
    }
    Ok(ExecResult::Command {
        tag: "CREATE TABLE".to_string(),
    })
}

/// Validate a foreign-key definition: the referenced table exists, the
/// referenced columns resolve, and they form the parent's primary key or
/// a unique constraint (SQLSTATE 42830, like Postgres).
fn validate_fk_def(
    eng: &Engine,
    ctx: &StmtCtx,
    child_table: &str,
    fk: &FkDef,
) -> Result<(), ExecError> {
    let child = eng
        .db
        .find_table(child_table, ctx.snap, ctx.own)
        .ok_or_else(|| {
            exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", child_table),
            )
        })?;
    for c in &fk.cols {
        if child.column_index(c).is_none() {
            return Err(exec_err(
                "42703",
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    c, child_table
                ),
            ));
        }
    }
    let parent = eng
        .db
        .find_table(&fk.ref_table, ctx.snap, ctx.own)
        .ok_or_else(|| {
            exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", fk.ref_table),
            )
        })?;
    let ref_cols: Vec<String> = if fk.ref_cols.is_empty() {
        match &parent.pkey {
            Some(pk) => pk.cols.clone(),
            None => {
                return Err(exec_err(
                    "42830",
                    format!(
                        "there is no primary key for referenced table \"{}\"",
                        fk.ref_table
                    ),
                ));
            }
        }
    } else {
        fk.ref_cols.clone()
    };
    if fk.cols.len() != ref_cols.len() {
        return Err(exec_err(
            "42830",
            format!(
                "number of referencing and referenced columns for foreign key \"{}\" must be the same",
                fk.name
            ),
        ));
    }
    for c in &ref_cols {
        if parent.column_index(c).is_none() {
            return Err(exec_err(
                "42703",
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    c, fk.ref_table
                ),
            ));
        }
    }
    let is_key = parent
        .pkey
        .as_ref()
        .map(|pk| pk.cols == ref_cols)
        .unwrap_or(false)
        || parent.uniques.iter().any(|u| u.cols == ref_cols);
    if !is_key {
        return Err(exec_err(
            "42830",
            format!(
                "there is no unique constraint matching given keys for referenced table \"{}\"",
                fk.ref_table
            ),
        ));
    }
    Ok(())
}

/// Build the backing unique index for a PRIMARY KEY / UNIQUE constraint
/// (`internal` marks it as constraint-owned). Checks for duplicate keys
/// among live row versions, like CREATE UNIQUE INDEX.
fn create_constraint_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    cname: &str,
    cols: &[String],
    internal: bool,
) -> Result<(), ExecError> {
    if eng.db.find_index(cname, ctx.snap, ctx.own).is_some() {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", cname),
        ));
    }
    let t = eng
        .db
        .find_table(table, ctx.snap, ctx.own)
        .expect("table still visible; engine lock held throughout");
    let mut seen = Vec::with_capacity(cols.len());
    for c in cols {
        let pos = t.column_index(c).ok_or_else(|| {
            exec_err(
                "42703",
                format!("column \"{}\" of relation \"{}\" does not exist", c, table),
            )
        })?;
        if seen.contains(&pos) {
            return Err(exec_err(
                "42701",
                format!("column \"{}\" specified more than once", c),
            ));
        }
        seen.push(pos);
    }
    let mut ix = Index::new(IndexDef {
        name: cname.to_string(),
        table: table.to_string(),
        cols: seen,
        col_names: cols.to_vec(),
        unique: true,
        internal,
        created_xmin: ctx.own,
        dropped_xmax: 0,
    });
    for r in &t.rows {
        let key = ix.key_for(&r.values);
        ix.insert(key, r.id);
    }
    // Duplicate check among versions that could still become visible
    // (mirrors CREATE UNIQUE INDEX).
    for (key, ids) in &ix.tree {
        if key.0.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        if ids.len() > 1 {
            return Err(exec_err(
                "23505",
                format!(
                    "duplicate key value violates unique constraint \"{}\"",
                    cname
                ),
            ));
        }
    }
    eng.db.indexes.insert(cname.to_string(), ix);
    ctx.writes.push(WriteOp::CreateIndex {
        name: cname.to_string(),
    });
    Ok(())
}

// ============================================================================
// v0.9: constraint enforcement (NOT NULL / CHECK / FOREIGN KEY / DEFAULT).
// ============================================================================

/// Owned copy of a table's constraint metadata, so checks can run while
/// `eng` is mutably borrowed for expression evaluation.
#[derive(Clone)]
struct TableMeta {
    columns: Vec<(String, ColType)>,
    not_null: Vec<bool>,
    defaults: Vec<Option<DefaultExpr>>,
    checks: Vec<CheckDef>,
    pkey: Option<UniqueDef>,
    fks: Vec<FkDef>,
}

impl TableMeta {
    fn of(t: &Table) -> Self {
        TableMeta {
            columns: t.columns.clone(),
            not_null: t.not_null.clone(),
            defaults: t.defaults.clone(),
            checks: t.checks.clone(),
            pkey: t.pkey.clone(),
            fks: t.fks.clone(),
        }
    }

    fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(n, _)| n == name)
    }
}

/// Evaluate a column DEFAULT to a value of the column's type.
fn eval_default(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    d: &DefaultExpr,
    ctype: &ColType,
    cname: &str,
) -> Result<Value, ExecError> {
    match d {
        DefaultExpr::Lit(lit) => coerce_literal(lit, ctype, cname),
        DefaultExpr::Nextval(seq) => {
            let v = seq_nextval(eng, snap, own, session, role, seq)?;
            coerce_value(Value::BigInt(v), ctype, cname)
        }
        DefaultExpr::Expr(e) => {
            let mut lock_ids = Vec::new();
            let mut q = Q {
                eng,
                snap,
                own,
                session,
                role,
                // v0.17: DML-only helper — INSERT/UPDATE/DELETE are
                // statement-blocked when read-only, so this is false.
                read_only: false,
                depth: 0,
                lock_ids: &mut lock_ids,
                ctes: Vec::new(),
                wctx: None,
                priv_scopes: Vec::new(),
            };
            let v = eval_expr(&mut q, &[], e)?;
            coerce_value(v, ctype, cname)
        }
    }
}

/// NOT NULL + CHECK validation for a fully-built row (INSERT / UPDATE /
/// cascaded writes). SQLSTATEs 23502 / 23514, like Postgres.
fn check_row_constraints(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    meta: &TableMeta,
    table: &str,
    values: &[Value],
) -> Result<(), ExecError> {
    for (i, (name, _)) in meta.columns.iter().enumerate() {
        if meta.not_null[i] && matches!(values[i], Value::Null) {
            return Err(exec_err(
                "23502",
                format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    name, table
                ),
            ));
        }
    }
    if meta.checks.is_empty() {
        return Ok(());
    }
    let schema: Vec<QCol> = meta
        .columns
        .iter()
        .map(|(n, ty)| QCol {
            qual: String::new(),
            name: n.clone(),
            ty: ty.clone(),
        })
        .collect();
    for check in &meta.checks {
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng,
            snap,
            own,
            session,
            role,
            // v0.17: DML-only helper — INSERT/UPDATE/DELETE are
            // statement-blocked when read-only, so this is false.
            read_only: false,
            depth: 0,
            lock_ids: &mut lock_ids,
            ctes: Vec::new(),
            wctx: None,
            priv_scopes: Vec::new(),
        };
        let frame = Scope {
            schema: &schema,
            row: values,
        };
        let v = eval_expr(&mut q, &[frame], &check.expr)?;
        // Postgres CHECK passes on TRUE or NULL; only FALSE fails it.
        if matches!(v, Value::Bool(false)) {
            return Err(exec_err(
                "23514",
                format!(
                    "new row for relation \"{}\" violates check constraint \"{}\"",
                    table, check.name
                ),
            ));
        }
    }
    Ok(())
}

/// Resolve an FK's referenced column names (empty = parent's primary key).
fn fk_ref_cols(parent_meta: &TableMeta, fk: &FkDef) -> Result<Vec<String>, ExecError> {
    if fk.ref_cols.is_empty() {
        match &parent_meta.pkey {
            Some(pk) => Ok(pk.cols.clone()),
            None => Err(exec_err(
                "42830",
                format!(
                    "there is no primary key for referenced table \"{}\"",
                    fk.ref_table
                ),
            )),
        }
    } else {
        Ok(fk.ref_cols.clone())
    }
}

/// Child-side foreign-key check for one new row (INSERT / UPDATE /
/// cascaded SET NULL / SET DEFAULT). `self_new` holds the statement's own
/// new rows on the child table (self-references); `ignore_id` excludes the
/// row version being replaced (self-referencing UPDATE).
///
/// MATCH SIMPLE semantics: a NULL in any referencing column skips the
/// check. Violation is SQLSTATE 23503.
fn check_fk_child_row(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    child_meta: &TableMeta,
    child_table: &str,
    values: &[Value],
    self_new: &[Vec<Value>],
    ignore_id: Option<u64>,
) -> Result<(), ExecError> {
    for fk in &child_meta.fks {
        let child_pos: Vec<usize> = fk
            .cols
            .iter()
            .map(|c| {
                child_meta
                    .column_index(c)
                    .expect("fk columns validated at DDL")
            })
            .collect();
        let key: Vec<Value> = child_pos.iter().map(|&i| values[i].clone()).collect();
        if key.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        let parent = eng
            .db
            .find_table(&fk.ref_table, snap, own)
            .expect("fk parent validated at DDL time");
        let parent_meta = TableMeta::of(parent);
        let ref_cols = fk_ref_cols(&parent_meta, fk)?;
        let parent_pos: Vec<usize> = ref_cols
            .iter()
            .map(|c| {
                parent
                    .column_index(c)
                    .expect("fk ref cols validated at DDL")
            })
            .collect();
        let matches_key = |vals: &[Value]| {
            parent_pos
                .iter()
                .zip(key.iter())
                .all(|(&pi, kv)| vals[pi] == *kv)
        };
        let self_ref = fk.ref_table == child_table;
        let found = parent
            .rows
            .iter()
            .filter(|r| row_visible(r, snap, own))
            .filter(|r| !(self_ref && Some(r.id) == ignore_id))
            .any(|r| matches_key(&r.values))
            || (self_ref && self_new.iter().any(|v| matches_key(v)));
        if !found {
            return Err(exec_err(
                "23503",
                format!(
                    "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                    child_table, fk.name
                ),
            ));
        }
    }
    Ok(())
}

/// Every (child table, FK) pair in the database whose referenced table is
/// `parent`, using the versions visible to (`snap`, `own`).
fn fks_referencing(eng: &Engine, snap: &Snapshot, own: u64, parent: &str) -> Vec<(String, FkDef)> {
    let mut out = Vec::new();
    for (name, versions) in &eng.db.tables {
        if let Some(t) = versions
            .iter()
            .find(|t| crate::storage::table_visible(t, snap, own))
        {
            for fk in &t.fks {
                if fk.ref_table == parent {
                    out.push((name.clone(), fk.clone()));
                }
            }
        }
    }
    out
}

/// Cascaded writes collected while planning parent-side FK actions.
#[derive(Default)]
struct FkCascade {
    /// (table, row id, prev xmax) to delete.
    deletes: Vec<(String, u64, u64)>,
    /// (table, row id, prev xmax, new values) to update.
    updates: Vec<(String, u64, u64, Vec<Value>)>,
}

impl FkCascade {
    fn contains(&self, table: &str, id: u64) -> bool {
        self.deletes.iter().any(|(t, i, _)| t == table && *i == id)
            || self
                .updates
                .iter()
                .any(|(t, i, _, _)| t == table && *i == id)
    }
}

/// Plan the child-side effects of deleting/updating parent rows.
///
/// `changed` holds (row id, old values, new values or None for delete) for
/// the parent table. RESTRICT violations fail with 23503; CASCADE / SET
/// NULL / SET DEFAULT collect into `out` (recursively, depth-limited).
fn plan_fk_cascade(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    level: IsolationLevel,
    parent_table: &str,
    parent_meta: &TableMeta,
    changed: &[(u64, Vec<Value>, Option<Vec<Value>>)],
    depth: u8,
    out: &mut FkCascade,
) -> Result<(), ExecError> {
    if depth > 16 {
        return Err(exec_err(
            "54000",
            "foreign key cascade depth exceeded".to_string(),
        ));
    }
    for (child_table, fk) in fks_referencing(eng, snap, own, parent_table) {
        let ref_cols = fk_ref_cols(parent_meta, &fk)?;
        let parent_pos: Vec<usize> = ref_cols
            .iter()
            .map(|c| {
                parent_meta
                    .column_index(c)
                    .expect("fk ref cols validated at DDL")
            })
            .collect();
        // Child metadata (owned; the borrow ends before recursion).
        let child_meta = {
            let t = eng
                .db
                .find_table(&child_table, snap, own)
                .expect("child table visible; engine lock held");
            TableMeta::of(t)
        };
        let child_pos: Vec<usize> = fk
            .cols
            .iter()
            .map(|c| {
                child_meta
                    .column_index(c)
                    .expect("fk cols validated at DDL")
            })
            .collect();
        for (pid, old_values, new_values) in changed {
            let _ = pid;
            let old_key: Vec<Value> = parent_pos.iter().map(|&i| old_values[i].clone()).collect();
            let new_key: Option<Vec<Value>> = new_values
                .as_ref()
                .map(|nv| parent_pos.iter().map(|&i| nv[i].clone()).collect());
            if let Some(nk) = &new_key {
                if nk == &old_key {
                    continue; // key unchanged: nothing to do
                }
            }
            let act = if new_values.is_some() {
                fk.on_update
            } else {
                fk.on_delete
            };
            // Visible child rows referencing the old key.
            let refs: Vec<(u64, u64, Vec<Value>)> = {
                let t = eng
                    .db
                    .find_table(&child_table, snap, own)
                    .expect("child table visible; engine lock held");
                t.rows
                    .iter()
                    .filter(|r| row_visible(r, snap, own))
                    .filter(|r| {
                        child_pos
                            .iter()
                            .zip(old_key.iter())
                            .all(|(&ci, kv)| r.values[ci] == *kv)
                    })
                    .map(|r| (r.id, r.xmax, r.values.clone()))
                    .collect()
            };
            for (cid, cxmax, cvalues) in refs {
                if out.contains(&child_table, cid) {
                    continue; // already handled by this cascade
                }
                match act {
                    FkAction::Restrict => {
                        return Err(exec_err(
                            "23503",
                            format!(
                                "{} on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                                if new_values.is_some() {
                                    "update"
                                } else {
                                    "delete"
                                },
                                parent_table,
                                fk.name,
                                child_table,
                            ),
                        ));
                    }
                    FkAction::Cascade => {
                        check_write_conflict(eng, cxmax, level)?;
                        check_row_lock(eng, &child_table, cid, own)?;
                        if let Some(nk) = &new_key {
                            // ON UPDATE CASCADE: move the child key.
                            let mut nv = cvalues.clone();
                            for (&ci, kv) in child_pos.iter().zip(nk.iter()) {
                                nv[ci] = kv.clone();
                            }
                            check_row_constraints(
                                eng,
                                snap,
                                own,
                                session,
                                role,
                                &child_meta,
                                &child_table,
                                &nv,
                            )?;
                            if let Some(vname) =
                                eng.db
                                    .unique_violation(&child_table, &nv, Some(cid), snap, own)
                            {
                                return Err(exec_err(
                                    "23505",
                                    format!(
                                        "duplicate key value violates unique constraint \"{}\"",
                                        vname
                                    ),
                                ));
                            }
                            // The child's own FKs must still hold.
                            check_fk_child_row(
                                eng,
                                snap,
                                own,
                                &child_meta,
                                &child_table,
                                &nv,
                                &[],
                                Some(cid),
                            )?;
                            out.updates
                                .push((child_table.clone(), cid, cxmax, nv.clone()));
                            let child_meta2 = child_meta.clone();
                            plan_fk_cascade(
                                eng,
                                snap,
                                own,
                                session,
                                role,
                                level,
                                &child_table,
                                &child_meta2,
                                &[(cid, cvalues.clone(), Some(nv))],
                                depth + 1,
                                out,
                            )?;
                        } else {
                            // ON DELETE CASCADE.
                            out.deletes.push((child_table.clone(), cid, cxmax));
                            let child_meta2 = child_meta.clone();
                            plan_fk_cascade(
                                eng,
                                snap,
                                own,
                                session,
                                role,
                                level,
                                &child_table,
                                &child_meta2,
                                &[(cid, cvalues.clone(), None)],
                                depth + 1,
                                out,
                            )?;
                        }
                    }
                    FkAction::SetNull | FkAction::SetDefault => {
                        check_write_conflict(eng, cxmax, level)?;
                        check_row_lock(eng, &child_table, cid, own)?;
                        let mut nv = cvalues.clone();
                        for &ci in &child_pos {
                            nv[ci] = match act {
                                FkAction::SetNull => Value::Null,
                                _ => {
                                    let (cname, ctype) = &child_meta.columns[ci];
                                    match &child_meta.defaults[ci] {
                                        Some(d) => eval_default(
                                            eng, snap, own, session, role, d, ctype, cname,
                                        )?,
                                        None => Value::Null,
                                    }
                                }
                            };
                        }
                        check_row_constraints(
                            eng,
                            snap,
                            own,
                            session,
                            role,
                            &child_meta,
                            &child_table,
                            &nv,
                        )?;
                        if let Some(vname) =
                            eng.db
                                .unique_violation(&child_table, &nv, Some(cid), snap, own)
                        {
                            return Err(exec_err(
                                "23505",
                                format!(
                                    "duplicate key value violates unique constraint \"{}\"",
                                    vname
                                ),
                            ));
                        }
                        check_fk_child_row(
                            eng,
                            snap,
                            own,
                            &child_meta,
                            &child_table,
                            &nv,
                            &[],
                            Some(cid),
                        )?;
                        out.updates
                            .push((child_table.clone(), cid, cxmax, nv.clone()));
                        let child_meta2 = child_meta.clone();
                        plan_fk_cascade(
                            eng,
                            snap,
                            own,
                            session,
                            role,
                            level,
                            &child_table,
                            &child_meta2,
                            &[(cid, cvalues.clone(), Some(nv))],
                            depth + 1,
                            out,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Apply a planned FK cascade: deletes then updates, with index
/// maintenance and write logging, like exec_update/exec_delete do.
fn apply_fk_cascade(eng: &mut Engine, ctx: &mut StmtCtx, out: FkCascade) -> Result<(), ExecError> {
    // Deletes.
    for (table, id, prev_xmax) in &out.deletes {
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, ctx.own)
            .expect("table still visible; engine lock held throughout");
        let pos = t
            .row_pos(*id)
            .expect("row version still present; engine lock held throughout");
        t.rows[pos].xmax = ctx.own;
        ctx.writes.push(WriteOp::DeleteRow {
            table: table.clone(),
            row_id: *id,
            prev_xmax: *prev_xmax,
        });
    }
    // Updates = delete old version + insert new version.
    let mut new_ids = Vec::with_capacity(out.updates.len());
    for _ in 0..out.updates.len() {
        new_ids.push(eng.alloc_row_id());
    }
    let mut indexed: Vec<(String, u64, Vec<Value>)> = Vec::with_capacity(out.updates.len());
    for ((table, old_id, prev_xmax, new_values), new_id) in out.updates.iter().zip(new_ids) {
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, ctx.own)
            .expect("table still visible; engine lock held throughout");
        let pos = t
            .row_pos(*old_id)
            .expect("row version still present; engine lock held throughout");
        // v0.13: capture the old values for the UpdateRow op (the
        // logical decoder needs them); values never change in place.
        let old_values = t.rows[pos].values.clone();
        t.rows[pos].xmax = ctx.own;
        t.push_version(RowVersion {
            id: new_id,
            values: new_values.clone(),
            xmin: ctx.own,
            xmax: 0,
        });
        ctx.writes.push(WriteOp::UpdateRow {
            table: table.clone(),
            old_id: *old_id,
            new_id,
            prev_xmax: *prev_xmax,
            old_values,
        });
        indexed.push((table.clone(), new_id, new_values.clone()));
    }
    for (table, new_id, new_values) in &indexed {
        eng.db.index_insert_row(table, *new_id, new_values);
    }
    Ok(())
}

fn assign_err(col_name: &str, col_type: &ColType, from: &str) -> ExecError {
    exec_err(
        "42804",
        format!(
            "column \"{}\" is of type {} but expression is of type {}",
            col_name,
            col_type.sql_name(),
            from
        ),
    )
}

/// Coerce an INSERT literal to the target column type. SQL literals
/// start life as Postgres' "unknown" type, so int/float/text literals
/// coerce generously (with range checks); typed literals (DATE '...'
/// etc.) and anything else go through the strict value path.
fn coerce_literal(lit: &Literal, col_type: &ColType, col_name: &str) -> Result<Value, ExecError> {
    match lit {
        Literal::Null => Ok(Value::Null),
        Literal::SmallInt(i) => coerce_int_lit(*i as i128, col_type, col_name, lit.type_name()),
        Literal::Int(i) => coerce_int_lit(*i as i128, col_type, col_name, lit.type_name()),
        Literal::BigInt(i) => coerce_int_lit(*i as i128, col_type, col_name, lit.type_name()),
        Literal::Real(f) => coerce_float_lit(*f as f64, col_type, col_name, lit.type_name()),
        Literal::Float(f) => coerce_float_lit(*f, col_type, col_name, lit.type_name()),
        Literal::Numeric(n) => coerce_numeric_lit(n, col_type, col_name),
        // Decimal literal text: parse exactly for numeric targets so
        // high-precision decimals don't round-trip through f64.
        Literal::Decimal(s) => match col_type {
            ColType::Numeric => crate::storage::Numeric::parse(s)
                .map(Value::Numeric)
                .map_err(|_| {
                    exec_err(
                        "22P02",
                        format!("invalid input syntax for type numeric: {:?}", s),
                    )
                }),
            ColType::Float4 => s
                .parse::<f64>()
                .map(|f| Value::Float4(f as f32))
                .map_err(|_| assign_err(col_name, col_type, lit.type_name())),
            ColType::Float => s
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| assign_err(col_name, col_type, lit.type_name())),
            ColType::Text => Ok(Value::Text(s.clone())),
            _ => Err(assign_err(col_name, col_type, lit.type_name())),
        },
        // Unknown-type text literal: through the type's input function
        // (so INSERT INTO d VALUES ('2026-01-01') works for dates).
        Literal::Text(s) => eval_cast(&Value::Text(s.clone()), *col_type).map_err(|e| {
            if e.code == "42846" {
                assign_err(col_name, col_type, lit.type_name())
            } else {
                e
            }
        }),
        // Unknown-type boolean literal.
        Literal::Bool(b) => match col_type {
            ColType::Bool => Ok(Value::Bool(*b)),
            ColType::Text => Ok(Value::Text(b.to_string())),
            _ => Err(assign_err(col_name, col_type, lit.type_name())),
        },
        // Typed literals (DATE '...', BYTEA '...', ...): strict.
        other => coerce_value(other.clone().into_value(), col_type, col_name),
    }
}

/// Integer literal (unknown type) to a column: any int kind with a
/// range check, float/numeric widening, or text rendering.
fn coerce_int_lit(
    i: i128,
    col_type: &ColType,
    col_name: &str,
    _from: &str,
) -> Result<Value, ExecError> {
    let range = |i: i128| {
        coerce_value(
            Value::BigInt(i64::try_from(i).map_err(|_| exec_err("22003", "integer out of range"))?),
            col_type,
            col_name,
        )
    };
    match col_type {
        ColType::SmallInt => i16::try_from(i)
            .map(Value::SmallInt)
            .map_err(|_| exec_err("22003", "smallint out of range")),
        ColType::Int => i32::try_from(i)
            .map(|w| Value::Int(w as i64))
            .map_err(|_| exec_err("22003", "integer out of range")),
        ColType::BigInt => i64::try_from(i)
            .map(Value::BigInt)
            .map_err(|_| exec_err("22003", "bigint out of range")),
        ColType::Text => Ok(Value::Text(i.to_string())),
        _ => range(i),
    }
}

/// Float literal (unknown type) to a column.
fn coerce_float_lit(
    f: f64,
    col_type: &ColType,
    col_name: &str,
    from: &str,
) -> Result<Value, ExecError> {
    match col_type {
        ColType::Float4 => {
            if f.abs() > f32::MAX as f64 {
                return Err(exec_err("22003", "value out of range for type real"));
            }
            Ok(Value::Float4(f as f32))
        }
        ColType::Float => Ok(Value::Float(f)),
        ColType::Numeric => Numeric::from_f64(f)
            .map(Value::Numeric)
            .map_err(|_| exec_err("22003", "value out of range for type numeric")),
        ColType::Text => Ok(Value::Text(Value::Float(f).to_text().unwrap_or_default())),
        _ => Err(assign_err(col_name, col_type, from)),
    }
}

/// Numeric literal (unknown type, only via params) to a column.
fn coerce_numeric_lit(n: &Numeric, col_type: &ColType, col_name: &str) -> Result<Value, ExecError> {
    match col_type {
        ColType::Numeric => Ok(Value::Numeric(n.clone())),
        ColType::Float4 => Ok(Value::Float4(n.to_f64() as f32)),
        ColType::Float => Ok(Value::Float(n.to_f64())),
        ColType::Text => Ok(Value::Text(n.to_text())),
        _ => Err(assign_err(col_name, col_type, "numeric")),
    }
}

/// Coerce an evaluated UPDATE value to the target column type: same
/// type, numeric widening, or text through the type's input function.
/// Narrowing a computed value needs an explicit cast (like Postgres).
fn coerce_value(v: Value, col_type: &ColType, col_name: &str) -> Result<Value, ExecError> {
    if v == Value::Null || v.col_type() == *col_type {
        return Ok(v);
    }
    let widened = match (&v, col_type) {
        (Value::SmallInt(i), ColType::Int) => Some(Value::Int(*i as i64)),
        (Value::SmallInt(i), ColType::BigInt) => Some(Value::BigInt(*i as i64)),
        (Value::SmallInt(i), ColType::Float4) => Some(Value::Float4(*i as f32)),
        (Value::SmallInt(i), ColType::Float) => Some(Value::Float(*i as f64)),
        (Value::SmallInt(i), ColType::Numeric) => {
            Some(Value::Numeric(Numeric::from_i64(*i as i64)))
        }
        (Value::Int(i), ColType::BigInt) => Some(Value::BigInt(*i)),
        (Value::Int(i), ColType::Float4) => Some(Value::Float4(*i as f32)),
        (Value::Int(i), ColType::Float) => Some(Value::Float(*i as f64)),
        (Value::Int(i), ColType::Numeric) => Some(Value::Numeric(Numeric::from_i64(*i))),
        (Value::BigInt(i), ColType::Float) => Some(Value::Float(*i as f64)),
        (Value::BigInt(i), ColType::Numeric) => Some(Value::Numeric(Numeric::from_i64(*i))),
        (Value::Float4(f), ColType::Float) => Some(Value::Float(*f as f64)),
        (Value::Float4(f), ColType::Numeric) => {
            Numeric::from_f64(*f as f64).ok().map(Value::Numeric)
        }
        (Value::Float(f), ColType::Numeric) => Numeric::from_f64(*f).ok().map(Value::Numeric),
        (Value::Numeric(n), ColType::Float4) => Some(Value::Float4(n.to_f64() as f32)),
        (Value::Numeric(n), ColType::Float) => Some(Value::Float(n.to_f64())),
        _ => None,
    };
    if let Some(w) = widened {
        return Ok(w);
    }
    // Text goes through the type's input function (assignment cast).
    if matches!(v, Value::Text(_)) {
        return eval_cast(&v, *col_type).map_err(|e| {
            if e.code == "42846" {
                assign_err(col_name, col_type, v.type_name())
            } else {
                e
            }
        });
    }
    Err(assign_err(col_name, col_type, v.type_name()))
}

/// v0.10: materialize a DML statement's WITH list. DML bodies cannot
/// reference the CTEs (except through subqueries in UPDATE's SET/WHERE or
/// the RETURNING list), but the CTEs are still evaluated — like Postgres,
/// which runs them for their side effects and validation.
fn materialize_dml_ctes(
    eng: &mut Engine,
    ctx: &StmtCtx,
    with: &[CteDef],
) -> Result<Vec<Rc<CteBinding>>, ExecError> {
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        session: ctx.session,
        role: ctx.role,
        read_only: ctx.read_only,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        priv_scopes: Vec::new(),
    };
    materialize_ctes(&mut q, with)?;
    Ok(q.ctes)
}

/// v0.10: output column names/types for a RETURNING list, resolved
/// against the target table's columns.
fn describe_returning(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    table: &str,
    returning: &[SelectItem],
) -> Result<Vec<(String, ColType)>, ExecError> {
    let t = eng
        .db
        .find_table(table, snap, own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
    let schemas: Vec<Vec<QCol>> = vec![
        t.columns
            .iter()
            .map(|(n, ty)| QCol {
                qual: table.to_string(),
                name: n.clone(),
                ty: ty.clone(),
            })
            .collect(),
    ];
    let refs: Vec<&[QCol]> = schemas.iter().map(|s| s.as_slice()).collect();
    let mut out = Vec::new();
    for item in returning {
        match item {
            SelectItem::Expr { expr, alias } => {
                let ty = expr_type(eng, snap, own, &refs, &[], expr)?;
                let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
                out.push((name, ty));
            }
            SelectItem::All | SelectItem::AllOf(_) => {
                return Err(exec_err(
                    "42601",
                    "RETURNING * is not supported; list the columns explicitly",
                ));
            }
        }
    }
    Ok(out)
}

/// v0.10: evaluate a RETURNING list against one affected row.
fn project_returning(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    schema: &[QCol],
    values: &[Value],
    returning: &[SelectItem],
    ctes: &[Rc<CteBinding>],
) -> Result<Vec<Value>, ExecError> {
    let mut out = Vec::with_capacity(returning.len());
    for item in returning {
        if let SelectItem::Expr { expr, .. } = item {
            out.push(eval_dml_expr(
                eng,
                snap,
                own,
                session,
                role,
                &[(schema, values)],
                expr,
                ctes,
            )?);
        }
    }
    Ok(out)
}

/// v0.10: resolved `ON CONFLICT` arbiter.
struct UpsertPlan {
    /// (index name, key column positions) for every arbitrating unique
    /// index, in deterministic order.
    indexes: Vec<(String, Vec<usize>)>,
    /// Target column positions for DO UPDATE SET, in SET order.
    set_cols: Vec<usize>,
    action: ConflictAction,
}

fn plan_upsert(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    oc: &OnConflict,
    meta: &TableMeta,
) -> Result<UpsertPlan, ExecError> {
    // (index name, key column positions, key column names), sorted.
    let mut unique: Vec<(String, Vec<usize>, Vec<String>)> = eng
        .db
        .visible_indexes_for(table, ctx.snap, ctx.own)
        .into_iter()
        .filter(|ix| ix.def.unique)
        .map(|ix| {
            (
                ix.def.name.clone(),
                ix.def.cols.clone(),
                ix.def.col_names.clone(),
            )
        })
        .collect();
    unique.sort_by(|a, b| a.0.cmp(&b.0));
    let no_arbiter = || {
        exec_err(
            "42P10",
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        )
    };
    let indexes: Vec<(String, Vec<usize>)> = match &oc.arbiter {
        ConflictArbiter::None => {
            if let ConflictAction::DoUpdate { .. } = &oc.action {
                return Err(exec_err(
                    "42601",
                    "ON CONFLICT DO UPDATE requires inference specification or constraint name",
                ));
            }
            // DO NOTHING without an arbiter: every unique index arbitrates.
            unique
                .iter()
                .map(|(n, cols, _)| (n.clone(), cols.clone()))
                .collect()
        }
        ConflictArbiter::Columns(cols) => {
            for c in cols {
                if meta.column_index(c).is_none() {
                    return Err(exec_err(
                        "42703",
                        format!("column \"{}\" of relation \"{}\" does not exist", c, table),
                    ));
                }
            }
            let mut want: Vec<&str> = cols.iter().map(|s| s.as_str()).collect();
            want.sort_unstable();
            unique
                .iter()
                .find(|(_, _, cn)| {
                    let mut have: Vec<&str> = cn.iter().map(|s| s.as_str()).collect();
                    have.sort_unstable();
                    have == want
                })
                .map(|(n, cols, _)| vec![(n.clone(), cols.clone())])
                .ok_or_else(no_arbiter)?
        }
        ConflictArbiter::Constraint(name) => unique
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(n, cols, _)| vec![(n.clone(), cols.clone())])
            .ok_or_else(no_arbiter)?,
    };
    if indexes.is_empty() {
        // No unique index to arbitrate: DO NOTHING degrades to a plain
        // insert; DO UPDATE (already rejected without an arbiter) would be
        // meaningless.
        if let ConflictAction::DoUpdate { .. } = &oc.action {
            return Err(no_arbiter());
        }
    }
    // Validate DO UPDATE's target columns once.
    let mut set_cols = Vec::new();
    if let ConflictAction::DoUpdate { sets, .. } = &oc.action {
        // v0.12: duplicate SET targets are 42701 in PostgreSQL.
        let mut seen = std::collections::HashSet::new();
        for (name, _) in sets {
            if !seen.insert(name) {
                return Err(exec_err(
                    "42701",
                    format!("multiple assignments to same column \"{}\"", name),
                ));
            }
            set_cols.push(meta.column_index(name).ok_or_else(|| {
                exec_err(
                    "42703",
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        name, table
                    ),
                )
            })?);
        }
    }
    Ok(UpsertPlan {
        indexes,
        set_cols,
        action: oc.action.clone(),
    })
}

/// v0.10: locate an upsert conflict for a candidate row: returns the
/// conflicting row version's id. Checks the table's unique indexes, like
/// Postgres' speculative insertion.
fn find_upsert_conflict(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    plan: &UpsertPlan,
    values: &[Value],
) -> Option<u64> {
    for (iname, _) in &plan.indexes {
        if let Some(id) = eng
            .db
            .unique_conflict_row(table, iname, values, None, ctx.snap, ctx.own)
        {
            return Some(id);
        }
    }
    None
}

/// v0.10: current values of one row version, by id.
fn row_values_by_id(eng: &Engine, ctx: &StmtCtx, table: &str, id: u64) -> Option<Vec<Value>> {
    let t = eng.db.find_table(table, ctx.snap, ctx.own)?;
    let pos = t.row_pos(id)?;
    Some(t.rows[pos].values.clone())
}

/// v0.10: serialize an arbiter key for same-statement conflict tracking.
fn arbiter_key(values: &[Value], key_cols: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    for &c in key_cols {
        value_key(&values[c], &mut out);
        out.push(0xff);
    }
    out
}

fn exec_insert(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    columns: &Option<Vec<String>>,
    rows: &[Vec<InsertValue>],
    select: &Option<SelectStmt>,
    with: &[CteDef],
    on_conflict: &Option<OnConflict>,
    returning: &[SelectItem],
) -> Result<ExecResult, ExecError> {
    // v0.11: INSERT needs INSERT privilege on the table — or, with an
    // explicit column list, on each listed column (like PostgreSQL).
    if let Some(cols) = columns {
        require_column_privs(eng, ctx, table, cols, crate::storage::PRIV_INSERT, "INSERT")?;
    } else {
        require_table_priv(eng, ctx, table, crate::storage::PRIV_INSERT, "INSERT")?;
    }
    // v0.12: duplicate target columns are 42701 in PostgreSQL
    // ("multiple assignments to same column"), not silently accepted.
    if let Some(cols) = columns {
        let mut seen = std::collections::HashSet::new();
        for c in cols {
            if !seen.insert(c) {
                return Err(exec_err(
                    "42701",
                    format!("multiple assignments to same column \"{}\"", c),
                ));
            }
        }
    }
    // v0.10: WITH materialization (validated; plain INSERT cannot reference
    // the CTEs, but subqueries in RETURNING/ON CONFLICT can).
    let ctes = materialize_dml_ctes(eng, ctx, with)?;
    // v0.10: INSERT...SELECT: run the SELECT and convert rows to insert
    // values. The CTEs are already materialized above.
    let select_rows: Option<Vec<Vec<Value>>> = if let Some(sel) = select {
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng,
            snap: ctx.snap,
            own: ctx.own,
            session: ctx.session,
            role: ctx.role,
            read_only: ctx.read_only,
            depth: 0,
            lock_ids: &mut lock_ids,
            ctes: ctes.clone(),
            wctx: None,
            priv_scopes: Vec::new(),
        };
        let out = run_select(&mut q, sel, &[])?;
        Some(out.rows)
    } else {
        None
    };
    // v0.10: resolve the ON CONFLICT arbiter before building rows (needs
    // the table metadata).
    let meta_for_upsert = {
        let t = eng
            .db
            .find_table(table, ctx.snap, ctx.own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        TableMeta::of(t)
    };
    let upsert: Option<UpsertPlan> = match on_conflict {
        None => None,
        Some(oc) => Some(plan_upsert(eng, ctx, table, oc, &meta_for_upsert)?),
    };
    // Validate everything before mutating (statement atomicity).
    let new_rows: Vec<Vec<Value>> = {
        let meta = {
            let t = eng.db.find_table(table, ctx.snap, ctx.own).ok_or_else(|| {
                exec_err("42P01", format!("relation \"{}\" does not exist", table))
            })?;
            TableMeta::of(t)
        };
        let targets: Vec<usize> = match columns {
            Some(names) => names
                .iter()
                .map(|n| {
                    meta.columns
                        .iter()
                        .position(|(c, _)| c == n)
                        .ok_or_else(|| {
                            exec_err(
                                "42703",
                                format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    n, table
                                ),
                            )
                        })
                })
                .collect::<Result<_, _>>()?,
            None => (0..meta.columns.len()).collect(),
        };
        let ncols = meta.columns.len();
        let mut built: Vec<Vec<Value>> = Vec::new();
        // v0.10: INSERT...SELECT: validate column count and use the
        // SELECT's rows directly (already Values).
        if let Some(srows) = select_rows {
            for cells in srows {
                if cells.len() != targets.len() {
                    return Err(exec_err(
                        "42601",
                        format!(
                            "INSERT has {} expressions but {} target columns",
                            cells.len(),
                            targets.len(),
                        ),
                    ));
                }
                let mut values = vec![Value::Null; ncols];
                let mut explicit = vec![false; ncols];
                for (j, v) in cells.into_iter().enumerate() {
                    let ti = targets[j];
                    values[ti] = v;
                    explicit[ti] = true;
                }
                // Fill defaults, check constraints (same as VALUES path).
                for (i, d) in meta.defaults.iter().enumerate() {
                    if !explicit[i] {
                        if let Some(d) = d {
                            let (cname, ctype) = &meta.columns[i];
                            values[i] = eval_default(
                                eng,
                                ctx.snap,
                                ctx.own,
                                ctx.session,
                                ctx.role,
                                d,
                                ctype,
                                cname,
                            )?;
                        }
                    }
                }
                check_row_constraints(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    &meta,
                    table,
                    &values,
                )?;
                built.push(values);
            }
        } else {
            built = Vec::with_capacity(rows.len());
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
                let mut explicit = vec![false; ncols];
                for (v, &ci) in row.iter().zip(targets.iter()) {
                    let (cname, ctype) = &meta.columns[ci];
                    values[ci] = match v {
                        InsertValue::Lit(l) => coerce_literal(l, ctype, cname)?,
                        InsertValue::Param(n) => {
                            return Err(exec_err("42P02", format!("there is no parameter ${}", n)));
                        }
                        // v0.9: DEFAULT in VALUES applies the column default.
                        InsertValue::Default => match &meta.defaults[ci] {
                            Some(d) => eval_default(
                                eng,
                                ctx.snap,
                                ctx.own,
                                ctx.session,
                                ctx.role,
                                d,
                                ctype,
                                cname,
                            )?,
                            None => Value::Null,
                        },
                    };
                    explicit[ci] = true;
                }
                // v0.9: fill defaults for columns not mentioned.
                for (i, d) in meta.defaults.iter().enumerate() {
                    if !explicit[i] {
                        if let Some(d) = d {
                            let (cname, ctype) = &meta.columns[i];
                            values[i] = eval_default(
                                eng,
                                ctx.snap,
                                ctx.own,
                                ctx.session,
                                ctx.role,
                                d,
                                ctype,
                                cname,
                            )?;
                        }
                    }
                }
                // v0.9: NOT NULL + CHECK.
                check_row_constraints(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    &meta,
                    table,
                    &values,
                )?;
                built.push(values);
            }
        } // end else (VALUES path)
        // v0.8: statement-atomic UNIQUE enforcement — every row is checked
        // against the indexes and against earlier rows of this statement
        // before any version is pushed. v0.10: skipped when there is an
        // ON CONFLICT clause — conflicts are resolved per row instead.
        if upsert.is_none() {
            check_insert_unique(&eng.db, table, &built, ctx.snap, ctx.own)?;
        }
        // v0.9: child-side foreign keys. Rows inserted earlier in the same
        // statement are visible to later rows (self-references).
        for (i, values) in built.iter().enumerate() {
            check_fk_child_row(
                eng,
                ctx.snap,
                ctx.own,
                &meta,
                table,
                values,
                &built[..i],
                None,
            )?;
        }
        built
    };
    // v0.10: plan the per-row ON CONFLICT resolution. No mutation happens
    // here, so a failed row still leaves the statement atomic. Tracks:
    // - inserts: (new row id, values) to insert,
    // - updates: (conflict row id, prev xmax, new values) for DO UPDATE,
    // - ret_rows: RETURNING source rows in statement order (inserted values
    //   or updated new values; skipped rows contribute nothing).
    // Same-statement conflicts are detected via `key_map`: (index name,
    // key bytes) -> row id of a planned insert.
    let mut inserts: Vec<(u64, Vec<Value>)> = Vec::new();
    let mut updates: Vec<(u64, u64, Vec<Value>)> = Vec::new();
    let mut ret_rows: Vec<Vec<Value>> = Vec::new();
    if let Some(plan) = &upsert {
        let mut key_map: HashMap<(String, Vec<u8>), u64> = HashMap::new();
        // Latest planned values per row id (planned inserts and the new
        // values of planned updates), for chained same-statement conflicts.
        let mut latest: HashMap<u64, Vec<Value>> = HashMap::new();
        // Schemas for DO UPDATE evaluation: excluded first, target last
        // (unqualified columns resolve to the target, like Postgres).
        let mk_schemas = || {
            let tgt: Vec<QCol> = meta_for_upsert
                .columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: table.to_string(),
                    name: n.clone(),
                    ty: ty.clone(),
                })
                .collect();
            let excl: Vec<QCol> = meta_for_upsert
                .columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: "excluded".to_string(),
                    name: n.clone(),
                    ty: ty.clone(),
                })
                .collect();
            (excl, tgt)
        };
        for values in &new_rows {
            // 1. Conflict with a table row?
            let mut conflict: Option<u64> = find_upsert_conflict(eng, ctx, table, plan, values);
            // 2. Conflict with a row planned earlier in this statement?
            if conflict.is_none() {
                for (iname, kcols) in &plan.indexes {
                    // NULL key parts never conflict.
                    if kcols.iter().any(|&c| values[c] == Value::Null) {
                        continue;
                    }
                    if let Some(id) = key_map.get(&(iname.clone(), arbiter_key(values, kcols))) {
                        conflict = Some(*id);
                        break;
                    }
                }
            }
            let Some(tid) = conflict else {
                // No conflict: insert.
                let id = eng.alloc_row_id();
                for (iname, kcols) in &plan.indexes {
                    if kcols.iter().any(|&c| values[c] == Value::Null) {
                        continue;
                    }
                    key_map.insert((iname.clone(), arbiter_key(values, kcols)), id);
                }
                latest.insert(id, values.clone());
                inserts.push((id, values.clone()));
                ret_rows.push(values.clone());
                continue;
            };
            match &plan.action {
                ConflictAction::DoNothing => {
                    // Skipped: contributes no row and no RETURNING output.
                }
                ConflictAction::DoUpdate { sets, where_ } => {
                    // Target values: latest planned, else the table row.
                    let target_values: Vec<Value> = match latest.get(&tid) {
                        Some(v) => v.clone(),
                        None => row_values_by_id(eng, ctx, table, tid)
                            .ok_or_else(|| exec_err("XX000", "upsert conflict target vanished"))?,
                    };
                    // Only real table rows need the concurrency checks;
                    // planned rows are ours.
                    let is_planned = latest.contains_key(&tid);
                    if !is_planned {
                        let t = eng
                            .db
                            .find_table(table, ctx.snap, ctx.own)
                            .expect("table still visible; engine lock held throughout");
                        let pos = t.row_pos(tid).expect("conflict target still present");
                        let r = &t.rows[pos];
                        check_write_conflict(eng, r.xmax, ctx.level)?;
                        check_row_lock(eng, table, tid, ctx.own)?;
                    }
                    let (excl_schema, tgt_schema) = mk_schemas();
                    let frames: Vec<(&[QCol], &[Value])> = vec![
                        (&excl_schema, &values[..]),
                        (&tgt_schema, &target_values[..]),
                    ];
                    // Optional DO UPDATE ... WHERE: false means skip.
                    if let Some(w) = where_ {
                        let v = eval_dml_expr(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            &frames,
                            w,
                            &ctes,
                        )?;
                        if v != Value::Bool(true) {
                            continue;
                        }
                    }
                    let mut new_values = target_values.clone();
                    for ((_, expr), &ci) in sets.iter().zip(plan.set_cols.iter()) {
                        let v = eval_dml_expr(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            &frames,
                            expr,
                            &ctes,
                        )?;
                        let (cname, ctype) = &meta_for_upsert.columns[ci];
                        new_values[ci] = coerce_value(v, ctype, cname)?;
                    }
                    // Same validations as a plain UPDATE row.
                    if let Some(vname) =
                        eng.db
                            .unique_violation(table, &new_values, Some(tid), ctx.snap, ctx.own)
                    {
                        return Err(exec_err(
                            "23505",
                            format!(
                                "duplicate key value violates unique constraint \"{}\"",
                                vname
                            ),
                        ));
                    }
                    check_row_constraints(
                        eng,
                        ctx.snap,
                        ctx.own,
                        ctx.session,
                        ctx.role,
                        &meta_for_upsert,
                        table,
                        &new_values,
                    )?;
                    check_fk_child_row(
                        eng,
                        ctx.snap,
                        ctx.own,
                        &meta_for_upsert,
                        table,
                        &new_values,
                        &[],
                        Some(tid),
                    )?;
                    // prev xmax for the WAL undo record.
                    let prev_xmax: u64 = if is_planned {
                        0
                    } else {
                        let t = eng
                            .db
                            .find_table(table, ctx.snap, ctx.own)
                            .expect("table still visible; engine lock held throughout");
                        t.rows[t.row_pos(tid).expect("conflict target still present")].xmax
                    };
                    // Refresh the same-statement key map when the key changed.
                    for (iname, kcols) in &plan.indexes {
                        let old_null = kcols.iter().any(|&c| target_values[c] == Value::Null);
                        let new_null = kcols.iter().any(|&c| new_values[c] == Value::Null);
                        if !old_null {
                            key_map.remove(&(iname.clone(), arbiter_key(&target_values, kcols)));
                        }
                        if !new_null {
                            key_map.insert((iname.clone(), arbiter_key(&new_values, kcols)), tid);
                        }
                    }
                    latest.insert(tid, new_values.clone());
                    updates.push((tid, prev_xmax, new_values.clone()));
                    ret_rows.push(new_values);
                }
            }
        }
    } else {
        // No ON CONFLICT: every candidate is inserted.
        for values in &new_rows {
            let id = eng.alloc_row_id();
            inserts.push((id, values.clone()));
            ret_rows.push(values.clone());
        }
    }
    let n = inserts.len() + updates.len();
    // Apply inserts.
    {
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, ctx.own)
            .expect("table still visible; engine lock held throughout");
        for (id, values) in &inserts {
            t.push_version(RowVersion {
                id: *id,
                values: values.clone(),
                xmin: ctx.own,
                xmax: 0,
            });
            ctx.writes.push(WriteOp::InsertRow {
                table: table.to_string(),
                row_id: *id,
            });
        }
    }
    for (id, values) in &inserts {
        eng.db.index_insert_row(table, *id, values);
    }
    // Apply DO UPDATEs: delete old version + insert new version.
    // Pre-allocate the new row ids (the table borrow below conflicts).
    let mut update_ids = Vec::with_capacity(updates.len());
    for _ in 0..updates.len() {
        update_ids.push(eng.alloc_row_id());
    }
    let mut indexed: Vec<(u64, Vec<Value>)> = Vec::with_capacity(updates.len());
    {
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, ctx.own)
            .expect("table still visible; engine lock held throughout");
        for ((old_id, prev_xmax, new_values), new_id) in updates.iter().zip(update_ids) {
            let pos = t
                .row_pos(*old_id)
                .expect("row version still present; engine lock held throughout");
            // v0.13: UpdateRow carries the old values for logical decoding.
            let old_values = t.rows[pos].values.clone();
            t.rows[pos].xmax = ctx.own;
            t.push_version(RowVersion {
                id: new_id,
                values: new_values.clone(),
                xmin: ctx.own,
                xmax: 0,
            });
            ctx.writes.push(WriteOp::UpdateRow {
                table: table.to_string(),
                old_id: *old_id,
                new_id,
                prev_xmax: *prev_xmax,
                old_values,
            });
            indexed.push((new_id, new_values.clone()));
        }
    }
    for (new_id, new_values) in &indexed {
        eng.db.index_insert_row(table, *new_id, new_values);
    }
    // v0.10: RETURNING.
    let (ret_cols, ret_out): (Vec<(String, ColType)>, Vec<Vec<Value>>) = if returning.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let cols = describe_returning(eng, ctx.snap, ctx.own, table, returning)?;
        let schema: Vec<QCol> = meta_for_upsert
            .columns
            .iter()
            .map(|(n, ty)| QCol {
                qual: table.to_string(),
                name: n.clone(),
                ty: ty.clone(),
            })
            .collect();
        let mut out_rows = Vec::with_capacity(ret_rows.len());
        for values in &ret_rows {
            out_rows.push(project_returning(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &schema,
                values,
                returning,
                &ctes,
            )?);
        }
        (cols, out_rows)
    };
    Ok(ExecResult::Dml {
        tag: format!("INSERT 0 {}", n),
        columns: ret_cols,
        rows: ret_out,
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
        (Value::Float(a), Literal::Decimal(b)) => Ok(*a == b.parse::<f64>().unwrap_or(f64::NAN)),
        (Value::Int(a), Literal::Decimal(b)) => {
            Ok((*a as f64) == b.parse::<f64>().unwrap_or(f64::NAN))
        }
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
    with: &[CteDef],
    returning: &[SelectItem],
) -> Result<ExecResult, ExecError> {
    // v0.11: UPDATE needs UPDATE on each SET column — a table-level
    // grant or a column-level grant (like PostgreSQL).
    require_column_privs(
        eng,
        ctx,
        table,
        &sets.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        crate::storage::PRIV_UPDATE,
        "UPDATE",
    )?;
    // v0.12: duplicate SET targets are 42701 in PostgreSQL
    // ("multiple assignments to same column"), not silently accepted.
    {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in sets {
            if !seen.insert(name) {
                return Err(exec_err(
                    "42701",
                    format!("multiple assignments to same column \"{}\"", name),
                ));
            }
        }
    }
    // v0.10: WITH materialization; the CTEs are visible to subqueries in
    // SET/WHERE and in the RETURNING list.
    let ctes = materialize_dml_ctes(eng, ctx, with)?;
    // Plan first (validate + conflict-check), mutate after: a failed
    // UPDATE leaves no trace (statement atomicity).
    let plan: Vec<(u64, u64, Vec<Value>)> = {
        let t = eng
            .db
            .find_table(table, ctx.snap, ctx.own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let meta = TableMeta::of(t);
        let set_cols: Vec<usize> = sets
            .iter()
            .map(|(name, _)| {
                meta.column_index(name).ok_or_else(|| {
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
        let columns = meta.columns.clone();
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
        let vis: Vec<(u64, u64, Vec<Value>)> = {
            let t = eng
                .db
                .find_table(table, ctx.snap, ctx.own)
                .expect("table still visible; engine lock held throughout");
            t.rows
                .iter()
                .filter(|r| row_visible(r, ctx.snap, ctx.own))
                .map(|r| (r.id, r.xmax, r.values.clone()))
                .collect()
        };
        let mut plan = Vec::new();
        let mut old_vals = Vec::new();
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
                let v = eval_update_expr(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    &schema,
                    values,
                    expr,
                    &ctes,
                )?;
                let (cname, ctype) = &columns[ci];
                new_values[ci] = coerce_value(v, ctype, cname)?;
            }
            // v0.8: UNIQUE enforcement against the indexes. The old
            // version is excluded (it is being replaced); the check runs
            // before any mutation, keeping the statement atomic.
            if let Some(vname) =
                eng.db
                    .unique_violation(table, &new_values, Some(*id), ctx.snap, ctx.own)
            {
                return Err(exec_err(
                    "23505",
                    format!(
                        "duplicate key value violates unique constraint \"{}\"",
                        vname
                    ),
                ));
            }
            // v0.9: NOT NULL + CHECK on the new row.
            check_row_constraints(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &meta,
                table,
                &new_values,
            )?;
            old_vals.push(values.clone());
            plan.push((*id, *xmax, new_values));
        }
        // v0.9: child-side FK checks for the new rows. Self-references see
        // the statement's own new versions; each row's old version is
        // excluded from the parent scan.
        {
            let self_new: Vec<Vec<Value>> = plan.iter().map(|(_, _, nv)| nv.clone()).collect();
            for (i, (_, _, nv)) in plan.iter().enumerate() {
                check_fk_child_row(
                    eng,
                    ctx.snap,
                    ctx.own,
                    &meta,
                    table,
                    nv,
                    &self_new,
                    Some(plan[i].0),
                )?;
            }
        }
        // v0.9: parent-side FK actions (RESTRICT / CASCADE / SET NULL /
        // SET DEFAULT), planned before any mutation.
        let mut cascade = FkCascade::default();
        {
            let changed: Vec<(u64, Vec<Value>, Option<Vec<Value>>)> = plan
                .iter()
                .zip(old_vals.iter())
                .map(|((id, _, nv), ov)| (*id, ov.clone(), Some(nv.clone())))
                .collect();
            plan_fk_cascade(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                ctx.level,
                table,
                &meta,
                &changed,
                0,
                &mut cascade,
            )?;
        }
        // v0.8: pairwise unique check — two rows updated to the same unique
        // key in one statement (the index still holds only old entries).
        // v0.9: extended over cascaded updates, grouped by table.
        {
            let mut by_table: HashMap<&str, Vec<(u64, u64, Vec<Value>)>> = HashMap::new();
            by_table
                .entry(table)
                .or_default()
                .extend(plan.iter().cloned());
            for (t, id, xmax, nv) in &cascade.updates {
                by_table
                    .entry(t.as_str())
                    .or_default()
                    .push((*id, *xmax, nv.clone()));
            }
            for (t, p) in &by_table {
                check_update_unique_pairs(&eng.db, t, p, ctx.snap, ctx.own)?;
            }
        }
        // Apply the cascade after the unique checks pass.
        apply_fk_cascade(eng, ctx, cascade)?;
        plan
    };
    // Apply: UPDATE = delete old version + insert new version.
    let n = plan.len();
    let mut new_ids = Vec::with_capacity(n);
    for _ in 0..n {
        new_ids.push(eng.alloc_row_id());
    }
    // The table borrow ends before index maintenance (both need `eng.db`
    // mutably); collect the new versions' keys meanwhile.
    let mut indexed: Vec<(u64, Vec<Value>)> = Vec::with_capacity(n);
    {
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, ctx.own)
            .expect("table still visible; engine lock held throughout");
        for ((old_id, prev_xmax, new_values), new_id) in plan.into_iter().zip(new_ids) {
            let pos = t
                .row_pos(old_id)
                .expect("row version still present; engine lock held throughout");
            // v0.13: UpdateRow carries the old values for logical decoding.
            let old_values = t.rows[pos].values.clone();
            t.rows[pos].xmax = ctx.own;
            t.push_version(RowVersion {
                id: new_id,
                values: new_values.clone(),
                xmin: ctx.own,
                xmax: 0,
            });
            ctx.writes.push(WriteOp::UpdateRow {
                table: table.to_string(),
                old_id,
                new_id,
                prev_xmax,
                old_values,
            });
            indexed.push((new_id, new_values));
        }
    }
    // v0.8: index the new versions (old versions' entries stay; the
    // version chain's xmax makes them invisible).
    for (new_id, new_values) in &indexed {
        eng.db.index_insert_row(table, *new_id, new_values);
    }
    // v0.10: RETURNING evaluates against the NEW row values.
    let (ret_cols, ret_out): (Vec<(String, ColType)>, Vec<Vec<Value>>) = if returning.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let cols = describe_returning(eng, ctx.snap, ctx.own, table, returning)?;
        let schema: Vec<QCol> = {
            let t = eng
                .db
                .find_table(table, ctx.snap, ctx.own)
                .expect("table still visible; engine lock held throughout");
            t.columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: table.to_string(),
                    name: n.clone(),
                    ty: ty.clone(),
                })
                .collect()
        };
        let mut out_rows = Vec::with_capacity(indexed.len());
        for (_, new_values) in &indexed {
            out_rows.push(project_returning(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &schema,
                new_values,
                returning,
                &ctes,
            )?);
        }
        (cols, out_rows)
    };
    Ok(ExecResult::Dml {
        tag: format!("UPDATE {}", n),
        columns: ret_cols,
        rows: ret_out,
    })
}

fn exec_delete(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    where_: &[WhereCond],
    with: &[CteDef],
    returning: &[SelectItem],
) -> Result<ExecResult, ExecError> {
    // v0.11: DELETE needs DELETE privilege on the table.
    require_table_priv(eng, ctx, table, crate::storage::PRIV_DELETE, "DELETE")?;
    // v0.10: WITH materialization (validated; plain DELETE cannot reference
    // the CTEs, but the RETURNING list can via subqueries).
    let ctes = materialize_dml_ctes(eng, ctx, with)?;
    // Plan first for statement atomicity (WHERE type errors must not
    // leave half the rows deleted).
    let plan: Vec<(u64, u64, Vec<Value>)> = {
        let t = eng
            .db
            .find_table(table, ctx.snap, ctx.own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let meta = TableMeta::of(t);
        let mut plan = Vec::new();
        for rv in t.rows.iter().filter(|r| row_visible(r, ctx.snap, ctx.own)) {
            check_write_conflict(eng, rv.xmax, ctx.level)?;
            if row_matches_where_cols(&meta.columns, &rv.values, where_)? {
                // Only rows we actually delete conflict with FOR UPDATE
                // locks — merely scanning a locked row is fine.
                check_row_lock(eng, table, rv.id, ctx.own)?;
                plan.push((rv.id, rv.xmax, rv.values.clone()));
            }
        }
        // v0.9: parent-side FK actions for the deleted rows.
        let mut cascade = FkCascade::default();
        {
            let changed: Vec<(u64, Vec<Value>, Option<Vec<Value>>)> = plan
                .iter()
                .map(|(id, _, values)| (*id, values.clone(), None))
                .collect();
            plan_fk_cascade(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                ctx.level,
                table,
                &meta,
                &changed,
                0,
                &mut cascade,
            )?;
        }
        apply_fk_cascade(eng, ctx, cascade)?;
        plan
    };
    let n = plan.len();
    // v0.10: DELETE RETURNING evaluates against the OLD row values —
    // collect them before the plan is consumed by the apply loop.
    let ret_vals: Vec<Vec<Value>> = plan.iter().map(|(_, _, v)| v.clone()).collect();
    let t = eng
        .db
        .find_table_mut(table, ctx.snap, ctx.own)
        .expect("table still visible; engine lock held throughout");
    for (id, prev_xmax, _) in plan {
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
    // v0.10: RETURNING.
    let (ret_cols, ret_out): (Vec<(String, ColType)>, Vec<Vec<Value>>) = if returning.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let cols = describe_returning(eng, ctx.snap, ctx.own, table, returning)?;
        let schema: Vec<QCol> = {
            let t = eng
                .db
                .find_table(table, ctx.snap, ctx.own)
                .expect("table still visible; engine lock held throughout");
            t.columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: table.to_string(),
                    name: n.clone(),
                    ty: ty.clone(),
                })
                .collect()
        };
        let mut out_rows = Vec::with_capacity(ret_vals.len());
        for values in &ret_vals {
            out_rows.push(project_returning(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &schema,
                values,
                returning,
                &ctes,
            )?);
        }
        (cols, out_rows)
    };
    Ok(ExecResult::Dml {
        tag: format!("DELETE {}", n),
        columns: ret_cols,
        rows: ret_out,
    })
}

// --- v0.16: TRUNCATE ---------------------------------------------------------
fn exec_truncate(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    tables: &[String],
    restart_identity: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    if restart_identity {
        // Sequence ownership is not tracked by the engine yet; refusing
        // is more honest than silently not resetting.
        return Err(exec_err(
            "0A000",
            "TRUNCATE ... RESTART IDENTITY is not supported yet",
        ));
    }
    // Resolve the table closure first (statement atomicity: a missing
    // table or FK violation fails before anything is removed).
    let mut targets: Vec<String> = tables.to_vec();
    if cascade {
        // Like Postgres, CASCADE pulls in every table holding a foreign
        // key that references a truncated table (transitively).
        let mut i = 0;
        while i < targets.len() {
            let t = targets[i].clone();
            for (child, _) in fks_referencing(eng, ctx.snap, ctx.own, &t) {
                if !targets.contains(&child) {
                    targets.push(child);
                }
            }
            i += 1;
        }
    } else {
        for t in tables {
            let refs = fks_referencing(eng, ctx.snap, ctx.own, t);
            if let Some((child, _)) = refs.first() {
                return Err(exec_err(
                    "2BP01",
                    format!(
                        "cannot truncate a table referenced in a foreign key constraint (\"{}\" references \"{}\")",
                        child, t
                    ),
                ));
            }
        }
    }
    for t in &targets {
        require_table_priv(eng, ctx, t, crate::storage::PRIV_TRUNCATE, "TRUNCATE")?;
        if eng.db.find_table(t, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", t),
            ));
        }
    }
    // Delete every visible row, staging the same WriteOps as DELETE so
    // ROLLBACK / ROLLBACK TO SAVEPOINT restore the table exactly.
    for table in &targets {
        let plan: Vec<(u64, u64)> = {
            let t = eng
                .db
                .find_table(table, ctx.snap, ctx.own)
                .expect("table checked above; engine lock held throughout");
            let mut plan = Vec::new();
            for rv in t.rows.iter().filter(|r| row_visible(r, ctx.snap, ctx.own)) {
                check_write_conflict(eng, rv.xmax, ctx.level)?;
                check_row_lock(eng, table, rv.id, ctx.own)?;
                plan.push((rv.id, rv.xmax));
            }
            plan
        };
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, ctx.own)
            .expect("table checked above; engine lock held throughout");
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
    }
    Ok(ExecResult::Command {
        tag: "TRUNCATE TABLE".to_string(),
    })
}

fn exec_drop(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        drop_one_table(eng, ctx, name, if_exists, cascade)?;
    }
    Ok(ExecResult::Command {
        tag: "DROP TABLE".to_string(),
    })
}

/// Drop a single table with dependency handling.
fn drop_one_table(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_exists: bool,
    cascade: bool,
) -> Result<(), ExecError> {
    // v0.11: only the owner (or a superuser) may drop a table.
    require_table_owner(eng, ctx, name)?;
    // Find the visible version first (immutable) for the conflict check,
    // then mutate. A DROP of a table dropped by a not-yet-visible
    // transaction behaves like the row case (40001 under RR/SERIALIZABLE).
    let prev_xmax = {
        let t = eng.db.find_table(name, ctx.snap, ctx.own);
        match t {
            None if if_exists => return Ok(()),
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
    // Dependency scan: FKs from other tables, and views.
    let mut dep_fks: Vec<(String, String)> = Vec::new();
    for (tname, vs) in &eng.db.tables {
        if tname == name {
            continue;
        }
        let Some(ot) = vs
            .iter()
            .find(|t| crate::storage::table_visible(t, ctx.snap, ctx.own))
        else {
            continue;
        };
        for fk in &ot.fks {
            if fk.ref_table == name {
                dep_fks.push((tname.clone(), fk.name.clone()));
            }
        }
    }
    let mut dep_views: Vec<String> = Vec::new();
    for (vname, vs) in &eng.db.views {
        if vs.iter().any(|v| {
            crate::storage::view_visible(v, ctx.snap, ctx.own) && v.deps.iter().any(|d| d == name)
        }) {
            dep_views.push(vname.clone());
        }
    }
    if !cascade && (!dep_fks.is_empty() || !dep_views.is_empty()) {
        let first = dep_fks
            .first()
            .map(|(_, f)| f.as_str())
            .or(dep_views.first().map(|s| s.as_str()))
            .unwrap_or("?");
        return Err(exec_err(
            "2BP01",
            format!("cannot drop table {} because {} depends on it", name, first),
        ));
    }
    // CASCADE: drop dependent views and FKs first.
    for v in &dep_views {
        drop_view_internal(eng, ctx, v)?;
    }
    for (tname, fk_name) in &dep_fks {
        // The referencing table may itself have been dropped by an
        // earlier CASCADE in this same statement.
        if eng.db.find_table(tname, ctx.snap, ctx.own).is_some() {
            alter_drop_constraint_internal(eng, ctx, tname, fk_name)?;
        }
    }
    let t = eng
        .db
        .find_table_mut(name, ctx.snap, ctx.own)
        .expect("table still visible; engine lock held throughout");
    t.dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropTable {
        name: name.to_string(),
        prev_xmax,
    });
    // v0.8: dropping a table drops its indexes with it. Each index drop
    // is its own write op (snapshotting the definition) so ROLLBACK
    // restores them and the WAL replays them.
    let idx_names: Vec<String> = eng
        .db
        .visible_indexes_for(name, ctx.snap, ctx.own)
        .iter()
        .map(|ix| ix.def.name.clone())
        .collect();
    for iname in idx_names {
        drop_index_internal(eng, ctx, &iname)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// v0.8 indexes: DDL, unique enforcement, planner, EXPLAIN, ANALYZE
// ---------------------------------------------------------------------------

/// 23505 error constructor.
fn unique_violation_err(index: &str) -> ExecError {
    exec_err(
        "23505",
        format!(
            "duplicate key value violates unique constraint \"{}\"",
            index
        ),
    )
}

/// Statement-atomic UNIQUE check for INSERT: every candidate row is checked
/// against the indexes and against earlier rows of the same statement
/// before any version is pushed.
fn check_insert_unique(
    db: &Database,
    table: &str,
    rows: &[Vec<Value>],
    snap: &Snapshot,
    own: u64,
) -> Result<(), ExecError> {
    let uniques: Vec<&Index> = db
        .visible_indexes_for(table, snap, own)
        .into_iter()
        .filter(|ix| ix.def.unique)
        .collect();
    for (i, values) in rows.iter().enumerate() {
        for ix in &uniques {
            let key = ix.key_for(values);
            if key.0.iter().any(|v| matches!(v, Value::Null)) {
                continue; // NULLs never conflict
            }
            if rows[..i].iter().any(|prev| ix.key_for(prev) == key) {
                return Err(unique_violation_err(&ix.def.name));
            }
        }
        if let Some(name) = db.unique_violation(table, values, None, snap, own) {
            return Err(unique_violation_err(&name));
        }
    }
    Ok(())
}

/// Pairwise UNIQUE check for UPDATE's planned rows: the index still holds
/// only the old entries at plan time, so new-vs-new conflicts need an
/// explicit pass.
fn check_update_unique_pairs(
    db: &Database,
    table: &str,
    plan: &[(u64, u64, Vec<Value>)],
    snap: &Snapshot,
    own: u64,
) -> Result<(), ExecError> {
    let uniques: Vec<&Index> = db
        .visible_indexes_for(table, snap, own)
        .into_iter()
        .filter(|ix| ix.def.unique)
        .collect();
    for (i, (_, _, values)) in plan.iter().enumerate() {
        for ix in &uniques {
            let key = ix.key_for(values);
            if key.0.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            if plan[..i].iter().any(|(_, _, prev)| ix.key_for(prev) == key) {
                return Err(unique_violation_err(&ix.def.name));
            }
        }
    }
    Ok(())
}

fn exec_create_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    table: &str,
    columns: &[String],
    unique: bool,
    if_not_exists: bool,
) -> Result<ExecResult, ExecError> {
    // v0.11: indexing a table needs its owner (or a superuser).
    require_table_owner(eng, ctx, table)?;
    if eng.db.find_index(name, ctx.snap, ctx.own).is_some() {
        if if_not_exists {
            return Ok(ExecResult::Command {
                tag: "CREATE INDEX".to_string(),
            });
        }
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    // Resolve the table and columns (immutable borrows only).
    {
        let t = eng
            .db
            .find_table(table, ctx.snap, ctx.own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let mut seen = Vec::with_capacity(columns.len());
        for c in columns {
            let pos = t.column_index(c).ok_or_else(|| {
                exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", c, table),
                )
            })?;
            if seen.contains(&pos) {
                return Err(exec_err(
                    "42701",
                    format!("column \"{}\" specified more than once", c),
                ));
            }
            seen.push(pos);
        }
        if seen.is_empty() {
            return Err(exec_err(
                "42601",
                "syntax error: index requires at least one column".to_string(),
            ));
        }
    }
    // Build + backfill from every version of the visible table version;
    // visibility is resolved at scan time, so uncommitted and dead
    // versions get entries too (like Postgres' heap/index split).
    let t = eng
        .db
        .find_table(table, ctx.snap, ctx.own)
        .expect("table still visible; engine lock held throughout");
    let cols: Vec<usize> = columns
        .iter()
        .map(|c| t.column_index(c).expect("columns resolved above"))
        .collect();
    let mut ix = Index::new(IndexDef {
        name: name.to_string(),
        table: table.to_string(),
        cols,
        col_names: columns.to_vec(),
        unique,
        internal: false,
        created_xmin: ctx.own,
        dropped_xmax: 0,
    });
    for r in &t.rows {
        let key = ix.key_for(&r.values);
        ix.insert(key, r.id);
    }
    if unique {
        // A duplicate among versions that could still become visible
        // fails the CREATE, like Postgres. Conservative: versions deleted
        // only by still-active transactions (or by us) count as live.
        for (key, ids) in &ix.tree {
            if key.0.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            let mut live = 0u32;
            for &id in ids {
                let dead = match t.row_pos(id) {
                    Some(pos) => {
                        let r = &t.rows[pos];
                        r.xmax != 0 && r.xmax != ctx.own && eng.xid_committed(r.xmax)
                    }
                    None => true, // vacuumed away: cannot conflict
                };
                if !dead {
                    live += 1;
                    if live >= 2 {
                        return Err(unique_violation_err(name));
                    }
                }
            }
        }
    }
    // `t`'s borrow ends at its last use above; the insert below needs
    // `eng.db` mutably.
    eng.db.indexes.insert(name.to_string(), ix);
    ctx.writes.push(WriteOp::CreateIndex {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: "CREATE INDEX".to_string(),
    })
}

fn exec_drop_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    // Snapshot the definition first: the write log's undo restores it on
    // ROLLBACK, and the WAL replays the drop on commit.
    // v0.11: dropping an index needs its table's owner (or a superuser).
    if let Some(ix) = eng.db.find_index(name, ctx.snap, ctx.own) {
        require_table_owner(eng, ctx, &ix.def.table.clone())?;
    }
    let snapshot = match eng.db.find_index(name, ctx.snap, ctx.own) {
        Some(ix) => ix.clone(),
        None => {
            if if_exists {
                return Ok(ExecResult::Command {
                    tag: "DROP INDEX".to_string(),
                });
            }
            return Err(exec_err(
                "42P01",
                format!("index \"{}\" does not exist", name),
            ));
        }
    };
    eng.db
        .find_index_mut(name, ctx.snap, ctx.own)
        .expect("index still present; engine lock held throughout")
        .def
        .dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropIndex {
        name: name.to_string(),
        index: snapshot,
    });
    Ok(ExecResult::Command {
        tag: "DROP INDEX".to_string(),
    })
}

// --- v0.8 planner ----------------------------------------------------------

/// One usable index bound extracted from a WHERE conjunct.
#[derive(Clone)]
enum IndexBoundKind {
    Eq,
    /// Greater-than; bool = inclusive.
    Gt(bool),
    /// Less-than; bool = inclusive.
    Lt(bool),
}

/// Try to read one WHERE conjunct as bounds on this table's columns.
/// `None` when the conjunct is not indexable — never an error, the
/// residual filter handles it.
fn conjunct_bounds(
    e: &Expr,
    qual: &str,
    table_name: &str,
    columns: &[(String, ColType)],
) -> Option<Vec<(usize, IndexBoundKind, Value)>> {
    // Resolve `Column op Literal` in either order, normalizing the
    // comparison direction.
    let from_cmp =
        |op: &CmpOp, left: &Expr, right: &Expr| -> Option<(usize, IndexBoundKind, Value)> {
            let (col_side, lit_side, flip) = match (left, right) {
                (Expr::Column { .. }, Expr::Literal(_)) => (left, right, false),
                (Expr::Literal(_), Expr::Column { .. }) => (right, left, true),
                _ => return None,
            };
            let (cq, cname) = match col_side {
                Expr::Column { table, name } => (table, name),
                _ => return None,
            };
            if let Some(cq) = cq {
                if cq != qual && cq != table_name {
                    return None;
                }
            }
            let lit = match lit_side {
                Expr::Literal(l) => l,
                _ => return None,
            };
            let pos = columns.iter().position(|(n, _)| n == cname)?;
            let (_, ctype) = &columns[pos];
            // The literal is coerced to the column type, exactly like an
            // INSERT value; anything uncoercible stays a sequential scan.
            let v = coerce_literal(lit, ctype, cname).ok()?;
            let kind = match (op, flip) {
                (CmpOp::Eq, _) => IndexBoundKind::Eq,
                (CmpOp::Gt, false) | (CmpOp::Lt, true) => IndexBoundKind::Gt(false),
                (CmpOp::Ge, false) | (CmpOp::Le, true) => IndexBoundKind::Gt(true),
                (CmpOp::Lt, false) | (CmpOp::Gt, true) => IndexBoundKind::Lt(false),
                (CmpOp::Le, false) | (CmpOp::Ge, true) => IndexBoundKind::Lt(true),
                _ => return None, // <> is never indexable
            };
            Some((pos, kind, v))
        };
    match e {
        Expr::Cmp { op, left, right } => from_cmp(op, left, right).map(|b| vec![b]),
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            if *neg {
                return None;
            }
            let (cq, cname) = match &**expr {
                Expr::Column { table, name } => (table, name),
                _ => return None,
            };
            if let Some(cq) = cq {
                if cq != qual && cq != table_name {
                    return None;
                }
            }
            let lo = match &**low {
                Expr::Literal(l) => l,
                _ => return None,
            };
            let hi = match &**high {
                Expr::Literal(l) => l,
                _ => return None,
            };
            let pos = columns.iter().position(|(n, _)| n == cname)?;
            let (_, ctype) = &columns[pos];
            let lo_v = coerce_literal(lo, ctype, cname).ok()?;
            let hi_v = coerce_literal(hi, ctype, cname).ok()?;
            Some(vec![
                (pos, IndexBoundKind::Gt(true), lo_v),
                (pos, IndexBoundKind::Lt(true), hi_v),
            ])
        }
        _ => None,
    }
}

/// Planned access path for one base-table scan.
#[derive(Clone, Debug)]
enum AccessPath {
    SeqScan,
    IndexScan {
        index: String,
        /// Equality values for the leading index columns.
        prefix: Vec<Value>,
        /// Optional range on the next index column: (value, inclusive).
        lo: Option<(Value, bool)>,
        hi: Option<(Value, bool)>,
        /// Human-readable condition for EXPLAIN.
        cond: String,
    },
}

/// Choose an access path for one base-table source. The index scan's row
/// set is always a *superset* of the true matches (same-type bounds under
/// the index's own ordering); the executor still applies the full residual
/// predicate and MVCC visibility, so a wrong choice costs speed, never
/// correctness.
fn plan_access_path(
    db: &Database,
    t: &Table,
    table_name: &str,
    qual: &str,
    where_: Option<&Expr>,
    snap: &Snapshot,
    own: u64,
) -> AccessPath {
    let w = match where_ {
        Some(w) => w,
        None => return AccessPath::SeqScan,
    };
    // Gather per-column bounds from the AND-conjuncts.
    let mut col_bounds: HashMap<usize, Vec<(IndexBoundKind, Value)>> = HashMap::new();
    for c in split_conjuncts(w) {
        if let Some(bs) = conjunct_bounds(c, qual, table_name, &t.columns) {
            for (pos, kind, v) in bs {
                col_bounds.entry(pos).or_default().push((kind, v));
            }
        }
    }
    if col_bounds.is_empty() {
        return AccessPath::SeqScan;
    }
    // Prefer the longest equality prefix; a point lookup beats a range,
    // and more bound columns beat fewer.
    let mut best: Option<AccessPath> = None;
    let mut best_score = (0usize, 0usize, false);
    for ix in db.visible_indexes_for(table_name, snap, own) {
        let mut prefix: Vec<Value> = Vec::new();
        let mut cond_parts: Vec<String> = Vec::new();
        let mut i = 0;
        while i < ix.def.cols.len() {
            let cp = ix.def.cols[i];
            let eq_val = col_bounds
                .get(&cp)
                .and_then(|bs| bs.iter().find(|(k, _)| matches!(k, IndexBoundKind::Eq)));
            match eq_val {
                Some((_, v)) => {
                    cond_parts.push(format!(
                        "{} = {}",
                        ix.def.col_names[i],
                        value_to_text_cast(v)
                    ));
                    prefix.push(v.clone());
                    i += 1;
                }
                None => break,
            }
        }
        // Optional range on the next column — or on the leading column
        // when there is no equality prefix at all. When several bounds
        // exist for one side the first wins — a wider range is still a
        // correct superset.
        let (mut lo, mut hi): (Option<(Value, bool)>, Option<(Value, bool)>) = (None, None);
        if i < ix.def.cols.len() {
            if let Some(bs) = col_bounds.get(&ix.def.cols[i]) {
                for (k, v) in bs {
                    match k {
                        IndexBoundKind::Gt(incl) if lo.is_none() => {
                            let op = if *incl { ">=" } else { ">" };
                            cond_parts.push(format!(
                                "{} {} {}",
                                ix.def.col_names[i],
                                op,
                                value_to_text_cast(v)
                            ));
                            lo = Some((v.clone(), *incl));
                        }
                        IndexBoundKind::Lt(incl) if hi.is_none() => {
                            let op = if *incl { "<=" } else { "<" };
                            cond_parts.push(format!(
                                "{} {} {}",
                                ix.def.col_names[i],
                                op,
                                value_to_text_cast(v)
                            ));
                            hi = Some((v.clone(), *incl));
                        }
                        _ => {}
                    }
                }
            }
        }
        if i == 0 && lo.is_none() && hi.is_none() {
            continue; // leading column unbound: index not usable
        }
        let is_point = i == ix.def.cols.len() && lo.is_none() && hi.is_none();
        let bound_cols = i + usize::from(lo.is_some() || hi.is_some());
        let score = (i, bound_cols, is_point);
        if score > best_score {
            best_score = score;
            best = Some(AccessPath::IndexScan {
                index: ix.def.name.clone(),
                prefix,
                lo,
                hi,
                cond: format!("({})", cond_parts.join(" AND ")),
            });
        }
    }
    best.unwrap_or(AccessPath::SeqScan)
}

/// Row-version ids from `ix` matching an equality `prefix` plus an
/// optional range on the next key component, in ascending key order.
///
/// The scan starts at the short prefix key: every key with this prefix
/// sorts after it and before any key with a greater prefix (lexicographic
/// order), so the matching keys form the contiguous run up to the first
/// prefix break. Within the run the range component is monotonic, so the
/// high bound stops the scan and the low bound only skips the head.
fn index_scan_ids(
    ix: &Index,
    prefix: &[Value],
    lo: Option<(&Value, bool)>,
    hi: Option<(&Value, bool)>,
) -> Vec<u64> {
    let start = IndexKey(
        prefix
            .iter()
            .cloned()
            .chain(lo.map(|(v, _)| (*v).clone()))
            .collect(),
    );
    let mut out = Vec::new();
    let p = prefix.len();
    let has_range = lo.is_some() || hi.is_some();
    for (k, ids) in ix.tree.range((Bound::Included(&start), Bound::Unbounded)) {
        if k.0.len() < p
            || k.0
                .iter()
                .zip(prefix.iter())
                // Prefix equality in *index* ordering (Numeric 5 = 5.0).
                .any(|(a, b)| index_key_cmp(a, b) != Ordering::Equal)
        {
            break;
        }
        if has_range {
            // A range always sits on column p < ncols here (a full-prefix
            // point lookup never takes this path).
            let c = &k.0[p];
            if let Some((lv, incl)) = lo {
                match index_key_cmp(c, lv) {
                    Ordering::Less => continue,
                    Ordering::Equal if !incl => continue,
                    _ => {}
                }
            }
            if let Some((hv, incl)) = hi {
                match index_key_cmp(c, hv) {
                    Ordering::Greater => break,
                    Ordering::Equal if !incl => break,
                    _ => {}
                }
            }
        }
        out.extend(ids.iter().copied());
    }
    out
}

/// Hint for an index-ordered scan: `SELECT ... FROM t ORDER BY <index cols>`
/// with no WHERE — rows stream out of the index in ORDER BY order and the
/// sort step is skipped.
#[derive(Clone, Debug)]
struct OrderHint {
    index: String,
    desc: bool,
}

/// ORDER BY term rendering for EXPLAIN.
fn order_term_text(t: &OrderTerm) -> String {
    let e = format!("{:?}", t.expr);
    if t.desc { format!("{} DESC", e) } else { e }
}

fn plan_order_scan(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    stmt: &SelectStmt,
) -> Option<OrderHint> {
    // Only the simplest shape: single base table, no filter, no
    // aggregation / DISTINCT / grouping.
    if stmt.from.len() != 1 || stmt.where_.is_some() || stmt.distinct {
        return None;
    }
    if is_agg_query(stmt) || !stmt.group_by.is_empty() || stmt.having.is_some() {
        return None;
    }
    if stmt.order_by.is_empty() {
        return None;
    }
    let (table_name, qual) = match &stmt.from[0] {
        FromItem::Table { name, alias } => {
            (name.as_str(), alias.clone().unwrap_or_else(|| name.clone()))
        }
        FromItem::Derived { .. } | FromItem::Join { .. } | FromItem::Values { .. } => return None,
    };
    let t = eng.db.find_table(table_name, snap, own)?;
    // ORDER BY alias safety: an unqualified ORDER BY column that matches a
    // select-list output name resolves to the *output* value (which may be
    // an expression), not the raw column — unless the select item is the
    // identical expression. Qualified terms always read the column.
    for term in &stmt.order_by {
        if let Expr::Column { table: None, name } = &term.expr {
            for item in &stmt.items {
                match item {
                    SelectItem::Expr { expr: e, alias } => {
                        let out_name = alias.clone().unwrap_or_else(|| expr_col_name(e));
                        if out_name == *name && e != &term.expr {
                            return None;
                        }
                    }
                    SelectItem::All | SelectItem::AllOf(_) => {}
                }
            }
        }
    }
    // Every term must be a plain column of this table, all in the same
    // direction, with default NULL placement (NULLs sort high in the
    // index: NULLS LAST for ASC, NULLS FIRST for DESC — Postgres'
    // defaults, matching compare_values).
    let desc = stmt.order_by[0].desc;
    let mut cols: Vec<String> = Vec::new();
    for term in &stmt.order_by {
        if term.desc != desc {
            return None;
        }
        if term.nulls_first.unwrap_or(desc) != desc {
            return None;
        }
        match &term.expr {
            Expr::Column { table, name } => {
                if let Some(q) = table {
                    if q != &qual && q != table_name {
                        return None;
                    }
                }
                if !t.columns.iter().any(|(n, _)| n == name) {
                    return None;
                }
                cols.push(name.clone());
            }
            _ => return None,
        }
    }
    // An index whose columns are exactly this sequence (indexes are
    // ascending; a DESC query scans it backwards).
    // An index whose leading columns are exactly this sequence satisfies
    // the ordering (indexes are ascending; a DESC query scans backwards).
    // A composite index also satisfies a prefix: (a,b) order implies
    // a-order.
    for ix in eng.db.visible_indexes_for(table_name, snap, own) {
        if ix.def.col_names.len() >= cols.len() && ix.def.col_names[..cols.len()] == cols[..] {
            return Some(OrderHint {
                index: ix.def.name.clone(),
                desc,
            });
        }
    }
    None
}

/// Rows of one base table in index key order (for ORDER BY ... LIMIT with
/// no WHERE). NULL placement matches the ORDER BY defaults: ascending
/// scans yield NULLS LAST, descending scans NULLS FIRST. Stops after
/// `limit` *visible* rows when set — the caller guarantees no residual
/// filter, DISTINCT, or aggregation can remove rows afterwards.
fn index_order_rows(
    ix: &Index,
    t: &Table,
    table_name: &str,
    desc: bool,
    snap: &Snapshot,
    own: u64,
    need_prov: bool,
    limit: Option<usize>,
) -> Vec<QRow> {
    let mut rows = Vec::new();
    // Walk the buckets in index order, resolving each id to its visible
    // heap version, and stop as soon as `limit` visible rows exist.
    let mut iter: Box<dyn Iterator<Item = (&IndexKey, &Vec<u64>)>> = if desc {
        Box::new(ix.tree.iter().rev())
    } else {
        Box::new(ix.tree.iter())
    };
    'scan: for (_, ids) in iter.by_ref() {
        for &id in ids {
            if let Some(n) = limit {
                if rows.len() >= n {
                    break 'scan;
                }
            }
            if let Some(pos) = t.row_pos(id) {
                let r = &t.rows[pos];
                if row_visible(r, snap, own) {
                    rows.push(QRow {
                        cells: r.values.clone(),
                        prov: if need_prov {
                            vec![(table_name.to_string(), r.id)]
                        } else {
                            Vec::new()
                        },
                    });
                }
            }
        }
    }
    rows
}

// --- EXPLAIN ---------------------------------------------------------------

/// EXPLAIN plan node: mirrors the executor's access-path decisions
/// without executing anything.
#[derive(Clone, Debug)]
enum PlanNode {
    Result {
        rows: u64,
        filter: Option<String>,
    },
    SeqScan {
        table: String,
        filter: Option<String>,
        rows: u64,
    },
    IndexScan {
        table: String,
        index: String,
        cond: String,
        filter: Option<String>,
        rows: u64,
    },
    IndexOrderScan {
        table: String,
        index: String,
        order: String,
        rows: u64,
    },
    NestedLoop {
        filter: Option<String>,
        rows: u64,
        outer: Box<PlanNode>,
        inner: Box<PlanNode>,
    },
    Aggregate {
        rows: u64,
        child: Box<PlanNode>,
    },
    Unique {
        rows: u64,
        child: Box<PlanNode>,
    },
    Sort {
        keys: String,
        rows: u64,
        child: Box<PlanNode>,
    },
    Limit {
        n: String,
        rows: u64,
        child: Box<PlanNode>,
    },
    SubqueryScan {
        alias: String,
        rows: u64,
        child: Box<PlanNode>,
    },
    /// v0.14: `(VALUES ...)` table source.
    Values {
        rows: u64,
    },
}

impl PlanNode {
    fn rows(&self) -> u64 {
        match self {
            PlanNode::Result { rows, .. }
            | PlanNode::SeqScan { rows, .. }
            | PlanNode::IndexScan { rows, .. }
            | PlanNode::IndexOrderScan { rows, .. }
            | PlanNode::NestedLoop { rows, .. }
            | PlanNode::Aggregate { rows, .. }
            | PlanNode::Unique { rows, .. }
            | PlanNode::Sort { rows, .. }
            | PlanNode::Limit { rows, .. }
            | PlanNode::SubqueryScan { rows, .. } => *rows,
            PlanNode::Values { rows, .. } => *rows,
        }
    }

    /// Attach a residual filter to a scan-like node.
    fn set_filter(&mut self, f: String) {
        match self {
            PlanNode::Result { filter, .. }
            | PlanNode::SeqScan { filter, .. }
            | PlanNode::IndexScan { filter, .. }
            | PlanNode::NestedLoop { filter, .. } => *filter = Some(f),
            _ => {}
        }
    }
}

/// Estimated live row count: ANALYZE stats when present, else a visible
/// count under this snapshot.
fn est_rel_rows(db: &Database, table: &str, snap: &Snapshot, own: u64) -> u64 {
    if let Some(ts) = db.stats.get(table) {
        return ts.reltuples.round().max(0.0) as u64;
    }
    db.find_table(table, snap, own)
        .map(|t| t.rows.iter().filter(|r| row_visible(r, snap, own)).count() as u64)
        .unwrap_or(0)
}

/// Equality selectivity from ANALYZE stats: MCV frequency when the value
/// is most-common, else the uniform remainder.
fn est_eq_sel(ts: Option<&TableStats>, col: &str, v: &Value) -> f64 {
    let cs = match ts.and_then(|t| t.cols.get(col)) {
        Some(c) => c,
        None => return 0.1,
    };
    if let Some((_, f)) = cs
        .mcv
        .iter()
        .find(|(mv, _)| index_key_cmp(mv, v) == Ordering::Equal)
    {
        return f.clamp(0.0001, 1.0);
    }
    let mcv_total: f64 = cs.mcv.iter().map(|(_, f)| f).sum();
    let rest = (1.0 - cs.null_frac - mcv_total).max(0.0);
    let nd = (cs.n_distinct - cs.mcv.len() as f64).max(1.0);
    (rest / nd).clamp(0.0001, 1.0)
}

/// Range selectivity from the histogram bounds: the fraction of
/// equal-count intervals overlapping [lo, hi].
fn est_range_sel(
    ts: Option<&TableStats>,
    col: &str,
    lo: Option<&Value>,
    hi: Option<&Value>,
) -> f64 {
    let cs = match ts.and_then(|t| t.cols.get(col)) {
        Some(c) => c,
        None => return 0.1,
    };
    if cs.hist_bounds.len() >= 2 {
        let iv = cs.hist_bounds.len() - 1;
        let mut overlap = 0u32;
        for i in 0..iv {
            let b0 = &cs.hist_bounds[i];
            let b1 = &cs.hist_bounds[i + 1];
            let above_lo = lo.map_or(true, |lv| index_key_cmp(b1, lv) != Ordering::Less);
            let below_hi = hi.map_or(true, |hv| index_key_cmp(b0, hv) != Ordering::Greater);
            if above_lo && below_hi {
                overlap += 1;
            }
        }
        return (overlap as f64 / iv as f64).clamp(0.0001, 1.0);
    }
    0.1
}

fn est_index_rows(
    db: &Database,
    table: &str,
    ix: &Index,
    prefix: &[Value],
    lo: Option<&Value>,
    hi: Option<&Value>,
    snap: &Snapshot,
    own: u64,
) -> u64 {
    let rel = est_rel_rows(db, table, snap, own) as f64;
    if rel == 0.0 {
        return 0;
    }
    let ts = db.stats.get(table);
    let mut sel = 1.0f64;
    for (i, v) in prefix.iter().enumerate() {
        sel *= est_eq_sel(ts, &ix.def.col_names[i], v);
    }
    if lo.is_some() || hi.is_some() {
        sel *= est_range_sel(ts, &ix.def.col_names[prefix.len()], lo, hi);
    }
    (rel * sel).round().max(1.0) as u64
}

fn plan_from_item(
    eng: &Engine,
    item: &FromItem,
    where_: Option<&Expr>,
    snap: &Snapshot,
    own: u64,
) -> Result<PlanNode, ExecError> {
    match item {
        FromItem::Table { name, alias } => {
            // v0.9: information_schema virtual tables plan as scans.
            if name == "information_schema.tables" || name == "information_schema.columns" {
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    filter: None,
                    rows: 100,
                });
            }
            // v0.9: views plan as a plain scan; build_source expands the
            // stored SELECT (no index access into views).
            if eng.db.find_view(name, snap, own).is_some() {
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    filter: None,
                    rows: 1000,
                });
            }
            if name == "pg_stats" && eng.db.find_table(name, snap, own).is_none() {
                let rows: u64 = eng.db.stats.values().map(|ts| ts.cols.len() as u64).sum();
                return Ok(PlanNode::SeqScan {
                    table: "pg_stats".to_string(),
                    filter: None,
                    rows,
                });
            }
            // v0.11: role catalogs are virtual.
            if matches!(
                name.as_str(),
                "pg_authid" | "pg_roles" | "pg_user" | "pg_auth_members"
            ) && eng.db.find_table(name, snap, own).is_none()
            {
                let rows = virtual_role_catalog_rows(&eng.db, name, snap, own);
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    filter: None,
                    rows,
                });
            }
            // v0.13: pg_replication_slots is virtual too; the estimate is
            // the live slot count.
            if name.as_str() == "pg_replication_slots"
                && eng.db.find_table(name, snap, own).is_none()
            {
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    filter: None,
                    rows: eng.repl_slots.len() as u64,
                });
            }
            let t = eng.db.find_table(name, snap, own).ok_or_else(|| {
                exec_err("42P01", format!("relation \"{}\" does not exist", name))
            })?;
            let qual = alias.clone().unwrap_or_else(|| name.clone());
            let rel_rows = est_rel_rows(&eng.db, name, snap, own);
            match plan_access_path(&eng.db, t, name, &qual, where_, snap, own) {
                AccessPath::SeqScan => Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    filter: None,
                    rows: rel_rows,
                }),
                AccessPath::IndexScan {
                    index,
                    prefix,
                    lo,
                    hi,
                    cond,
                } => {
                    let ix = eng
                        .db
                        .indexes
                        .get(&index)
                        .expect("planned index still present; engine lock held throughout");
                    let rows = est_index_rows(
                        &eng.db,
                        name,
                        ix,
                        &prefix,
                        lo.as_ref().map(|(v, _)| v),
                        hi.as_ref().map(|(v, _)| v),
                        snap,
                        own,
                    );
                    Ok(PlanNode::IndexScan {
                        table: name.clone(),
                        index,
                        cond,
                        filter: None,
                        rows,
                    })
                }
            }
        }
        FromItem::Derived { sub, alias } => {
            let child = plan_select(eng, sub, snap, own)?;
            let rows = child.rows();
            Ok(PlanNode::SubqueryScan {
                alias: alias.clone(),
                rows,
                child: Box::new(child),
            })
        }
        // v0.14: VALUES rows are uncorrelated constants.
        FromItem::Values { rows, .. } => Ok(PlanNode::Values {
            rows: rows.len() as u64,
        }),
        // The executor runs joins as nested loops; the filter attaches to
        // the outermost Nested Loop node at the top level.
        FromItem::Join { left, right, .. } => {
            let outer = plan_from_item(eng, left, None, snap, own)?;
            let inner = plan_from_item(eng, right, None, snap, own)?;
            let rows = outer.rows().saturating_mul(inner.rows());
            Ok(PlanNode::NestedLoop {
                filter: None,
                rows,
                outer: Box::new(outer),
                inner: Box::new(inner),
            })
        }
    }
}

fn plan_select(
    eng: &Engine,
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
) -> Result<PlanNode, ExecError> {
    // 1. FROM → access paths. A single base table with a usable
    // ORDER BY ... LIMIT hint becomes an index-order scan.
    let mut ordered = false;
    let mut node = if stmt.from.is_empty() {
        PlanNode::Result {
            rows: 1,
            filter: None,
        }
    } else if stmt.from.len() == 1 {
        if matches!(&stmt.from[0], FromItem::Table { .. }) {
            if let Some(hint) = plan_order_scan(eng, snap, own, stmt) {
                let name = match &stmt.from[0] {
                    FromItem::Table { name, .. } => name.clone(),
                    _ => unreachable!(),
                };
                ordered = true;
                PlanNode::IndexOrderScan {
                    rows: est_rel_rows(&eng.db, &name, snap, own),
                    table: name,
                    index: hint.index,
                    order: stmt
                        .order_by
                        .iter()
                        .map(order_term_text)
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            } else {
                plan_from_item(eng, &stmt.from[0], stmt.where_.as_ref(), snap, own)?
            }
        } else {
            plan_from_item(eng, &stmt.from[0], stmt.where_.as_ref(), snap, own)?
        }
    } else {
        let mut items = stmt.from.iter();
        let mut node = plan_from_item(eng, items.next().unwrap(), stmt.where_.as_ref(), snap, own)?;
        for item in items {
            let inner = plan_from_item(eng, item, stmt.where_.as_ref(), snap, own)?;
            let rows = node.rows().saturating_mul(inner.rows());
            node = PlanNode::NestedLoop {
                filter: None,
                rows,
                outer: Box::new(node),
                inner: Box::new(inner),
            };
        }
        node
    };
    // 2. WHERE → residual filter (the index cond is shown separately).
    if let Some(w) = &stmt.where_ {
        node.set_filter(format!("{:?}", w));
    }
    // 3. Aggregation / DISTINCT.
    if is_agg_query(stmt) {
        let rows = if stmt.group_by.is_empty() {
            1
        } else {
            node.rows()
        };
        node = PlanNode::Aggregate {
            rows,
            child: Box::new(node),
        };
    } else if stmt.distinct {
        let rows = node.rows();
        node = PlanNode::Unique {
            rows,
            child: Box::new(node),
        };
    }
    // 4. ORDER BY → Sort, unless the index-order scan provides it.
    if !stmt.order_by.is_empty() && !ordered {
        let keys = stmt
            .order_by
            .iter()
            .map(order_term_text)
            .collect::<Vec<_>>()
            .join(", ");
        let rows = node.rows();
        node = PlanNode::Sort {
            keys,
            rows,
            child: Box::new(node),
        };
    }
    // 5. OFFSET / LIMIT.
    if stmt.limit.is_some() || stmt.offset.is_some() {
        let n = match (stmt.offset, stmt.limit) {
            (Some(o), Some(l)) => format!("{} OFFSET {}", l, o),
            (Some(o), None) => format!("ALL OFFSET {}", o),
            (None, Some(l)) => format!("{}", l),
            (None, None) => unreachable!(),
        };
        let mut rows = node.rows();
        if let Some(l) = stmt.limit {
            rows = rows.min(l.max(0) as u64);
        }
        node = PlanNode::Limit {
            n,
            rows,
            child: Box::new(node),
        };
    }
    Ok(node)
}

fn render_plan(node: &PlanNode, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    match node {
        PlanNode::Result { rows, filter } => {
            out.push(format!("{}Result (rows={})", pad, rows));
            if let Some(f) = filter {
                out.push(format!("{}  Filter: {}", pad, f));
            }
        }
        PlanNode::SeqScan {
            table,
            filter,
            rows,
        } => {
            out.push(format!("{}Seq Scan on {} (rows={})", pad, table, rows));
            if let Some(f) = filter {
                out.push(format!("{}  Filter: {}", pad, f));
            }
        }
        PlanNode::IndexScan {
            table,
            index,
            cond,
            filter,
            rows,
        } => {
            out.push(format!(
                "{}Index Scan using {} on {} (rows={})",
                pad, index, table, rows
            ));
            out.push(format!("{}  Index Cond: {}", pad, cond));
            if let Some(f) = filter {
                out.push(format!("{}  Filter: {}", pad, f));
            }
        }
        PlanNode::IndexOrderScan {
            table,
            index,
            order,
            rows,
        } => {
            out.push(format!(
                "{}Index Scan using {} on {} (rows={})",
                pad, index, table, rows
            ));
            out.push(format!("{}  Order: {}", pad, order));
        }
        PlanNode::NestedLoop {
            filter,
            rows,
            outer,
            inner,
        } => {
            out.push(format!("{}Nested Loop (rows={})", pad, rows));
            if let Some(f) = filter {
                out.push(format!("{}  Filter: {}", pad, f));
            }
            render_plan(outer, depth + 1, out);
            render_plan(inner, depth + 1, out);
        }
        PlanNode::Aggregate { rows, child } => {
            out.push(format!("{}Aggregate (rows={})", pad, rows));
            render_plan(child, depth + 1, out);
        }
        PlanNode::Unique { rows, child } => {
            out.push(format!("{}Unique (rows={})", pad, rows));
            render_plan(child, depth + 1, out);
        }
        PlanNode::Sort { keys, rows, child } => {
            out.push(format!("{}Sort (rows={})", pad, rows));
            out.push(format!("{}  Sort Key: {}", pad, keys));
            render_plan(child, depth + 1, out);
        }
        PlanNode::Limit { n, rows, child } => {
            out.push(format!("{}Limit {} (rows={})", pad, n, rows));
            render_plan(child, depth + 1, out);
        }
        PlanNode::SubqueryScan { alias, rows, child } => {
            out.push(format!("{}Subquery Scan on {} (rows={})", pad, alias, rows));
            render_plan(child, depth + 1, out);
        }
        PlanNode::Values { rows } => {
            out.push(format!("{}Values (rows={})", pad, rows));
        }
    }
}

fn exec_explain(eng: &mut Engine, ctx: &mut StmtCtx, stmt: &Stmt) -> Result<ExecResult, ExecError> {
    let sel = match stmt {
        Stmt::Select(s) => s,
        _ => {
            return Err(exec_err(
                "0A000",
                "EXPLAIN only supports SELECT statements".to_string(),
            ));
        }
    };
    // Planning only inspects definitions and statistics — nothing runs.
    let plan = plan_select(&*eng, sel, ctx.snap, ctx.own)?;
    let mut lines = Vec::new();
    render_plan(&plan, 0, &mut lines);
    Ok(ExecResult::Explain {
        columns: vec![("QUERY PLAN".to_string(), ColType::Text)],
        rows: lines.into_iter().map(|l| vec![Value::Text(l)]).collect(),
    })
}

/// v0.14: best-effort `Value` -> `ColType` for VALUES column typing
/// (first non-NULL value wins; all-NULL columns describe as TEXT).
fn value_coltype(v: &Value) -> ColType {
    match v {
        Value::SmallInt(_) => ColType::SmallInt,
        Value::Int(_) => ColType::Int,
        Value::BigInt(_) => ColType::BigInt,
        Value::Float4(_) => ColType::Float4,
        Value::Float(_) => ColType::Float,
        Value::Numeric(_) => ColType::Numeric,
        Value::Text(_) => ColType::Text,
        Value::Bool(_) => ColType::Bool,
        Value::Date(_) => ColType::Date,
        Value::Timestamp(_) => ColType::Timestamp,
        Value::Timestamptz(_) => ColType::Timestamptz,
        Value::Bytea(_) => ColType::Bytea,
        Value::Uuid(_) => ColType::Uuid,
        Value::Null => ColType::Text,
    }
}

// --- ANALYZE -----------------------------------------------------------------

/// Compute ANALYZE statistics for one table over the snapshot's visible
/// rows. Distinct counts are exact (not estimated); the MCV list and
/// histogram bounds derive from the same pass.
fn analyze_table(db: &Database, table: &str, snap: &Snapshot, own: u64) -> TableStats {
    let t = db
        .find_table(table, snap, own)
        .expect("table checked visible by caller");
    let vis: Vec<&RowVersion> = t
        .rows
        .iter()
        .filter(|r| row_visible(r, snap, own))
        .collect();
    let total = vis.len() as f64;
    let mut cols = HashMap::new();
    for (ci, (cname, _)) in t.columns.iter().enumerate() {
        let mut nulls = 0u64;
        let mut counts: HashMap<Vec<u8>, (Value, u64)> = HashMap::new();
        for r in &vis {
            let v = &r.values[ci];
            if matches!(v, Value::Null) {
                nulls += 1;
                continue;
            }
            let mut k = Vec::new();
            value_key(v, &mut k);
            let e = counts.entry(k).or_insert_with(|| (v.clone(), 0));
            e.1 += 1;
        }
        // Most common values: top 100 by count (ties by index order).
        let mut by_freq: Vec<(Value, u64)> =
            counts.values().map(|(v, c)| (v.clone(), *c)).collect();
        by_freq.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| index_key_cmp(&a.0, &b.0)));
        let mcv: Vec<(Value, f64)> = by_freq
            .iter()
            .take(100)
            .map(|(v, c)| (v.clone(), if total > 0.0 { *c as f64 / total } else { 0.0 }))
            .collect();
        // Histogram bounds: up to 101 evenly spaced distinct values in
        // index order (5 and 5.0 merge — index ordering, not byte order).
        let mut distinct: Vec<Value> = counts.values().map(|(v, _)| v.clone()).collect();
        distinct.sort_by(index_key_cmp);
        distinct.dedup_by(|a, b| index_key_cmp(a, b) == Ordering::Equal);
        let n_distinct = distinct.len() as f64;
        let hist_bounds: Vec<Value> = if distinct.len() <= 101 {
            distinct
        } else {
            (0..101)
                .map(|i| distinct[i * (distinct.len() - 1) / 100].clone())
                .collect()
        };
        cols.insert(
            cname.clone(),
            ColStats {
                null_frac: if total > 0.0 {
                    nulls as f64 / total
                } else {
                    0.0
                },
                n_distinct,
                mcv,
                hist_bounds,
            },
        );
    }
    TableStats {
        reltuples: total,
        cols,
    }
}

fn exec_analyze(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &Option<String>,
) -> Result<ExecResult, ExecError> {
    // v0.11: ANALYZE requires ownership (or superuser), like PostgreSQL.
    let is_owner = |eng: &Engine, n: &str| {
        eng.db
            .find_table(n, ctx.snap, ctx.own)
            .map(|t| {
                t.owner == ctx.role
                    || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            })
            .unwrap_or(false)
    };
    let names: Vec<String> = match table {
        Some(n) => {
            if eng.db.find_table(n, ctx.snap, ctx.own).is_none() {
                return Err(exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", n),
                ));
            }
            if !is_owner(eng, n) {
                return Err(exec_err(
                    "42501",
                    format!("permission denied: must be owner of table \"{}\"", n),
                ));
            }
            vec![n.clone()]
        }
        // No table named: analyze the tables this role owns (like PG,
        // which only touches tables the user may maintain).
        None => eng
            .db
            .tables
            .keys()
            .filter(|n| is_owner(eng, n))
            .cloned()
            .collect(),
    };
    // Statistics are not transactional (like Postgres): they are computed
    // and stored without write ops.
    for n in names {
        let stats = analyze_table(&eng.db, &n, ctx.snap, ctx.own);
        eng.db.stats.insert(n, stats);
    }
    Ok(ExecResult::Command {
        tag: "ANALYZE".to_string(),
    })
}

/// The pg_stats system catalog table (virtual): one row per analyzed table
/// column. A real table named pg_stats takes precedence (checked by the
/// caller).
fn pg_stats_schema() -> Vec<QCol> {
    [
        ("schemaname", ColType::Text),
        ("tablename", ColType::Text),
        ("attname", ColType::Text),
        ("null_frac", ColType::Float),
        ("n_distinct", ColType::Float),
        ("most_common_vals", ColType::Text),
        ("most_common_freqs", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "pg_stats".to_string(),
        name: n.to_string(),
        ty,
    })
    .collect()
}

fn pg_stats_scan(db: &Database) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_stats_schema();
    let mut table_names: Vec<&String> = db.stats.keys().collect();
    table_names.sort();
    let mut rows = Vec::new();
    for tn in table_names {
        let ts = &db.stats[tn];
        let mut col_names: Vec<&String> = ts.cols.keys().collect();
        col_names.sort();
        for cn in col_names {
            let cs = &ts.cols[cn];
            let vals = cs
                .mcv
                .iter()
                .map(|(v, _)| value_to_text_cast(v))
                .collect::<Vec<_>>()
                .join(",");
            let freqs = cs
                .mcv
                .iter()
                .map(|(_, f)| format!("{:.4}", f))
                .collect::<Vec<_>>()
                .join(",");
            rows.push(QRow {
                cells: vec![
                    Value::Text("public".to_string()),
                    Value::Text(tn.clone()),
                    Value::Text((*cn).clone()),
                    Value::Float(cs.null_frac),
                    Value::Float(cs.n_distinct),
                    Value::Text(format!("{{{}}}", vals)),
                    Value::Text(format!("{{{}}}", freqs)),
                ],
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}

// ---------------------------------------------------------------------------
// v0.11: role catalogs (virtual). A real table by the same name takes
// precedence, like pg_stats. `pg_authid` masks password verifiers from
// non-superusers, like PostgreSQL.
// ---------------------------------------------------------------------------

fn qcol(qual: &str, name: &str, ty: ColType) -> QCol {
    QCol {
        qual: qual.to_string(),
        name: name.to_string(),
        ty,
    }
}

fn pg_authid_schema() -> Vec<QCol> {
    vec![
        qcol("pg_authid", "rolname", ColType::Text),
        qcol("pg_authid", "rolsuper", ColType::Bool),
        qcol("pg_authid", "rolinherit", ColType::Bool),
        qcol("pg_authid", "rolcreaterole", ColType::Bool),
        qcol("pg_authid", "rolcreatedb", ColType::Bool),
        qcol("pg_authid", "rolcanlogin", ColType::Bool),
        qcol("pg_authid", "rolconnlimit", ColType::Int),
        qcol("pg_authid", "rolpassword", ColType::Text),
        qcol("pg_authid", "rolvaliduntil", ColType::Timestamp),
    ]
}

fn pg_roles_schema() -> Vec<QCol> {
    vec![
        qcol("pg_roles", "rolname", ColType::Text),
        qcol("pg_roles", "rolsuper", ColType::Bool),
        qcol("pg_roles", "rolinherit", ColType::Bool),
        qcol("pg_roles", "rolcreaterole", ColType::Bool),
        qcol("pg_roles", "rolcreatedb", ColType::Bool),
        qcol("pg_roles", "rolcanlogin", ColType::Bool),
        qcol("pg_roles", "rolconnlimit", ColType::Int),
        qcol("pg_roles", "rolvaliduntil", ColType::Timestamp),
    ]
}

fn pg_user_schema() -> Vec<QCol> {
    vec![
        qcol("pg_user", "usename", ColType::Text),
        qcol("pg_user", "usesysid", ColType::Int),
        qcol("pg_user", "usecreatedb", ColType::Bool),
        qcol("pg_user", "usesuper", ColType::Bool),
        qcol("pg_user", "usecatupd", ColType::Bool),
        qcol("pg_user", "passwd", ColType::Text),
        qcol("pg_user", "valuntil", ColType::Timestamp),
        qcol("pg_user", "useconfig", ColType::Text), // text[] has no ColType; always NULL
    ]
}

/// Rows for pg_auth_members: one row per live membership edge whose
/// group role still exists. Oids are deterministic FNV-1a stand-ins
/// (see name_oid); admin_option is always false (WITH ADMIN OPTION is
/// parsed but not tracked).
fn pg_auth_members_rows(db: &Database, snap: &Snapshot, own: u64) -> Vec<QRow> {
    let mut names: Vec<&String> = db.roles.keys().collect();
    names.sort();
    let mut rows = Vec::new();
    for member_name in names {
        let Some(r) = db.roles[member_name]
            .iter()
            .find(|r| crate::storage::role_visible(r, snap, own))
        else {
            continue;
        };
        for m in &r.memberships {
            // Skip edges whose group role is gone (DROP ROLE cleans these
            // up; this is belt-and-braces for concurrent snapshots).
            if db.find_role(&m.role, snap, own).is_none() {
                continue;
            }
            rows.push(QRow {
                cells: vec![
                    Value::Int(name_oid(&m.role) as i64),
                    Value::Int(name_oid(member_name) as i64),
                    Value::Int(name_oid(&m.grantor) as i64),
                    Value::Bool(false),
                ],
                prov: Vec::new(),
            });
        }
    }
    rows
}

/// Rows for pg_authid / pg_roles / pg_user. `kind` selects the column
/// layout. Roles are snapshot-filtered like every other catalog.
fn pg_auth_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    role: &str,
    kind: &str,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = match kind {
        "pg_roles" => pg_roles_schema(),
        "pg_user" => pg_user_schema(),
        "pg_auth_members" => pg_auth_members_schema(),
        _ => pg_authid_schema(),
    };
    if kind == "pg_auth_members" {
        return (schema, pg_auth_members_rows(db, snap, own));
    }
    let viewer_super = crate::storage::is_superuser_snap(db, role, snap, own);
    let mut names: Vec<&String> = db.roles.keys().collect();
    names.sort();
    let mut rows = Vec::new();
    for rn in names {
        let Some(r) = db.roles[rn]
            .iter()
            .find(|r| crate::storage::role_visible(r, snap, own))
        else {
            continue;
        };
        let valid_until = match &r.valid_until {
            None => Value::Null,
            Some(vu) => match crate::datetime::parse_timestamp(vu) {
                Ok(t) => Value::Timestamp(t),
                Err(_) => Value::Null,
            },
        };
        let passwd = match &r.password {
            None => Value::Null,
            Some(v) => {
                if viewer_super {
                    Value::Text(v.encode())
                } else {
                    Value::Text("********".to_string())
                }
            }
        };
        let cells = match kind {
            "pg_roles" => vec![
                Value::Text(r.name.clone()),
                Value::Bool(r.superuser),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(r.can_login),
                Value::Int(r.connlimit as i64),
                valid_until.clone(),
            ],
            "pg_user" => vec![
                Value::Text(r.name.clone()),
                // No stable oids in rustgres; hash the name for a
                // deterministic stand-in.
                Value::Int(name_oid(&r.name) as i64),
                Value::Bool(false),
                Value::Bool(r.superuser),
                Value::Bool(false),
                passwd,
                valid_until.clone(),
                Value::Null,
            ],
            _ => vec![
                Value::Text(r.name.clone()),
                Value::Bool(r.superuser),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(r.can_login),
                Value::Int(r.connlimit as i64),
                passwd,
                valid_until,
            ],
        };
        rows.push(QRow {
            cells,
            prov: Vec::new(),
        });
    }
    (schema, rows)
}

fn pg_auth_members_schema() -> Vec<QCol> {
    vec![
        qcol("pg_auth_members", "roleid", ColType::Int),
        qcol("pg_auth_members", "member", ColType::Int),
        qcol("pg_auth_members", "grantor", ColType::Int),
        qcol("pg_auth_members", "admin_option", ColType::Bool),
    ]
}

/// Estimated row count for the planner over virtual role catalogs.
fn virtual_role_catalog_rows(db: &Database, name: &str, snap: &Snapshot, own: u64) -> u64 {
    if name == "pg_auth_members" {
        let mut n = 0u64;
        for vs in db.roles.values() {
            if let Some(r) = vs
                .iter()
                .find(|r| crate::storage::role_visible(r, snap, own))
            {
                n += r.memberships.len() as u64;
            }
        }
        return n;
    }
    db.roles.len() as u64
}

// ---------------------------------------------------------------------------
// v0.13: pg_replication_slots (virtual). Mirrors PostgreSQL's view over the
// live slot map: slot_name, plugin, slot_type, active, restart_lsn,
// confirmed_flush_lsn. LSNs render in PostgreSQL's `pg_lsn` text form.
// A real table by the same name takes precedence, like the other virtual
// catalogs.
// ---------------------------------------------------------------------------

fn pg_replication_slots_schema() -> Vec<QCol> {
    vec![
        qcol("pg_replication_slots", "slot_name", ColType::Text),
        qcol("pg_replication_slots", "plugin", ColType::Text),
        qcol("pg_replication_slots", "slot_type", ColType::Text),
        qcol("pg_replication_slots", "active", ColType::Bool),
        qcol("pg_replication_slots", "restart_lsn", ColType::Text),
        qcol("pg_replication_slots", "confirmed_flush_lsn", ColType::Text),
    ]
}

fn pg_replication_slots_rows(eng: &Engine) -> Vec<QRow> {
    let mut names: Vec<&String> = eng.repl_slots.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let s = &eng.repl_slots[n];
            QRow {
                cells: vec![
                    Value::Text(s.name.clone()),
                    Value::Text(s.plugin.clone()),
                    Value::Text(s.slot_type.clone()),
                    Value::Bool(s.active),
                    Value::Text(crate::repl::format_lsn(s.restart_lsn)),
                    Value::Text(crate::repl::format_lsn(s.confirmed_flush_lsn)),
                ],
                prov: Vec::new(),
            }
        })
        .collect()
}

/// Deterministic stand-in oid for pg_user.usesysid (FNV-1a of the name).
fn name_oid(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
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
        Expr::Arith { op, left, right } => Ok(Expr::Arith {
            op: *op,
            left: Box::new(r(left)?),
            right: Box::new(r(right)?),
        }),
        Expr::Concat(a, b) => Ok(Expr::Concat(Box::new(r(a)?), Box::new(r(b)?))),
        Expr::Cast { expr, to } => Ok(Expr::Cast {
            expr: Box::new(r(expr)?),
            to: *to,
        }),
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
        } => Ok(Expr::Like {
            expr: Box::new(r(expr)?),
            pattern: Box::new(r(pattern)?),
            not: *not,
            ilike: *ilike,
        }),
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => Ok(Expr::Between {
            expr: Box::new(r(expr)?),
            low: Box::new(r(low)?),
            high: Box::new(r(high)?),
            neg: *neg,
        }),
        Expr::IsBool { expr, neg, val } => Ok(Expr::IsBool {
            expr: Box::new(r(expr)?),
            neg: *neg,
            val: *val,
        }),
        Expr::Func { name, args } => Ok(Expr::Func {
            name: name.clone(),
            args: args.iter().map(r).collect::<Result<Vec<_>, _>>()?,
        }),
        Expr::Extract { field, from } => Ok(Expr::Extract {
            field: field.clone(),
            from: Box::new(r(from)?),
        }),
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
        Expr::Agg {
            func,
            arg,
            distinct,
            arg2,
        } => Ok(Expr::Agg {
            func: *func,
            arg: arg.as_ref().map(|a| r(a).map(Box::new)).transpose()?,
            distinct: *distinct,
            arg2: arg2.as_ref().map(|a| r(a).map(Box::new)).transpose()?,
        }),
        // v0.10: resolve columns inside window inputs.
        Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            wid,
        } => Ok(Expr::Window {
            func: func.clone(),
            args: args.iter().map(|a| r(a)).collect::<Result<Vec<_>, _>>()?,
            distinct: *distinct,
            partition_by: partition_by
                .iter()
                .map(|p| r(p))
                .collect::<Result<Vec<_>, _>>()?,
            order_by: order_by
                .iter()
                .map(|o| {
                    Ok(OrderTerm {
                        expr: r(&o.expr)?,
                        desc: o.desc,
                        nulls_first: o.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            frame: frame.clone(),
            wid: *wid,
        }),
    }
}

/// Per-query evaluation state threaded through the v0.6 engine.
struct Q<'a, 'b> {
    eng: &'a mut Engine,
    snap: &'b Snapshot,
    own: u64,
    /// v0.9: server-assigned session id, for session-local `currval`.
    session: u64,
    /// Subquery nesting depth (0 = top level).
    depth: usize,
    /// Sink for (table, row-version id) pairs named by FOR UPDATE, at any
    /// query level. The top-level `execute` acquires them all at once.
    lock_ids: &'a mut Vec<(String, u64)>,
    /// v0.10: materialized CTE bindings visible at this query level
    /// (innermost last). Shared by reference-counting so subqueries
    /// inherit them cheaply.
    ctes: Vec<Rc<CteBinding>>,
    /// v0.10: active window-function evaluation context. Set by the
    /// window pre-pass before projection; `Expr::Window` evaluates by
    /// looking up `values[wid][row]`.
    wctx: Option<WindowCtx>,
    /// v0.11: acting role, for privilege checks during scans and
    /// sequence-function evaluation.
    role: &'a str,
    /// v0.17: session is read-only — nextval/setval fail with 25006.
    read_only: bool,
    /// v0.11: qualifier -> real table name per query level (innermost
    /// last), for the column-privilege pre-pass. `None` = not a base
    /// table (view/CTE/derived); those are checked at their own level.
    priv_scopes: Vec<Vec<(String, Option<String>)>>,
}

/// v0.10: a materialized Common Table Expression: name, output schema and
/// rows. Recursive CTEs hold the fixpoint result.
#[derive(Clone, Debug)]
struct CteBinding {
    name: String,
    schema: Vec<QCol>,
    rows: Vec<QRow>,
}

/// v0.10: window-function evaluation state for one query level.
/// `values[wid]` holds one value per input row (non-agg path) or per
/// group (agg path); `row` is the input row / group currently being
/// projected.
#[derive(Clone, Debug, Default)]
struct WindowCtx {
    values: Vec<Vec<Value>>,
    row: usize,
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
    /// v0.10: pre-projection input row index, for ORDER BY terms that
    /// contain window functions (set only when the query uses windows).
    win_idx: Option<usize>,
}

/// v0.10: `WITH [RECURSIVE] ...` — materialize every CTE of this query
/// level (in definition order, so later CTEs see earlier ones), run the
/// inner query, then pop the bindings. The bindings live on the shared
/// query context, so nested subqueries, derived tables and views see them
/// too; sibling and outer levels are unaffected after the pop.
fn run_select(q: &mut Q, stmt: &SelectStmt, outer: &[Scope]) -> Result<SelectOut, ExecError> {
    let base = q.ctes.len();
    if !stmt.with.is_empty() {
        materialize_ctes(q, &stmt.with)?;
    }
    // v0.11: column privileges. Push this level's qualifier map, check
    // every column this level reads, then pop (balanced across view
    // expansion, which reuses `q`).
    q.priv_scopes.push(priv_scope_for(q, stmt));
    let chk = check_select_col_privs(q, stmt);
    q.priv_scopes.pop();
    chk?;
    let out = run_select_inner(q, stmt, outer);
    q.ctes.truncate(base);
    // v0.10: the window context is per query level; never leak it.
    q.wctx = None;
    out
}

/// v0.11: qualifier -> real-table map for one query level (see
/// `Q::priv_scopes`). CTEs shadow real tables; views and derived
/// tables map to `None` (checked at their own query level).
fn priv_scope_for(q: &Q, stmt: &SelectStmt) -> Vec<(String, Option<String>)> {
    fn walk(q: &Q, item: &FromItem, out: &mut Vec<(String, Option<String>)>) {
        match item {
            FromItem::Table { name, alias } => {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let is_cte = q.ctes.iter().rev().any(|b| b.name == *name);
                let real = if is_cte {
                    None
                } else {
                    q.eng
                        .db
                        .find_table(name, q.snap, q.own)
                        .map(|_| name.clone())
                };
                out.push((qual, real));
            }
            FromItem::Derived { alias, .. } => out.push((alias.clone(), None)),
            FromItem::Values { alias, .. } => out.push((alias.clone(), None)),
            FromItem::Join { left, right, .. } => {
                walk(q, left, out);
                walk(q, right, out);
            }
        }
    }
    let mut out = Vec::new();
    for item in &stmt.from {
        walk(q, item, &mut out);
    }
    out
}

/// v0.11: column-level SELECT enforcement. Every column read at this
/// query level must be covered by table-level SELECT or a column-level
/// SELECT grant. Unresolvable references (unknown tables, view/CTE
/// columns) are skipped here: unknown relations fail later as 42P01,
/// and views/CTEs/derived tables are checked at their own level when
/// expanded. Correlated references resolve against outer levels via
/// `q.priv_scopes` (innermost last, current level included).
fn check_select_col_privs(q: &Q, stmt: &SelectStmt) -> Result<(), ExecError> {
    use crate::sql::SelectItem;
    let mut refs: Vec<(Option<String>, String)> = Vec::new();
    let mut star = false;
    let mut star_quals: Vec<String> = Vec::new();
    for item in &stmt.items {
        match item {
            SelectItem::All => star = true,
            SelectItem::AllOf(qual) => star_quals.push(qual.clone()),
            SelectItem::Expr { expr, .. } => crate::sql::collect_col_refs(expr, &mut refs),
        }
    }
    if let Some(w) = &stmt.where_ {
        crate::sql::collect_col_refs(w, &mut refs);
    }
    for g in &stmt.group_by {
        crate::sql::collect_col_refs(g, &mut refs);
    }
    if let Some(h) = &stmt.having {
        crate::sql::collect_col_refs(h, &mut refs);
    }
    for o in &stmt.order_by {
        crate::sql::collect_col_refs(&o.expr, &mut refs);
    }

    // All (table, column) pairs this level needs SELECT on.
    let mut needed: Vec<(String, String)> = Vec::new();
    let levels: Vec<&Vec<(String, Option<String>)>> = q.priv_scopes.iter().collect();
    // Does `table` currently have `col`?
    let has_col = |table: &str, col: &str| {
        q.eng
            .db
            .find_table(table, q.snap, q.own)
            .map(|t| t.columns.iter().any(|(n, _)| n == col))
            .unwrap_or(false)
    };
    if star {
        if let Some(level) = levels.last() {
            for (_, real) in level.iter() {
                if let Some(t) = real {
                    if let Some(tab) = q.eng.db.find_table(t, q.snap, q.own) {
                        for (cn, _) in &tab.columns {
                            needed.push((t.clone(), cn.clone()));
                        }
                    }
                }
            }
        }
    }
    for sq in &star_quals {
        if let Some((t, _)) = resolve_priv_qual(&levels, Some(sq)) {
            if let Some(tab) = q.eng.db.find_table(&t, q.snap, q.own) {
                for (cn, _) in &tab.columns {
                    needed.push((t.clone(), cn.clone()));
                }
            }
        }
    }
    for (qual, col) in &refs {
        for (t, _) in resolve_priv_ref(&levels, qual.as_deref(), col, &has_col) {
            needed.push((t, col.clone()));
        }
    }
    let closure = crate::storage::role_closure(&q.eng.db, q.role, q.snap, q.own);
    for (t, c) in &needed {
        let tab = q
            .eng
            .db
            .find_table(t, q.snap, q.own)
            .expect("resolved from a live table above");
        let have =
            crate::storage::column_privs_in(&q.eng.db, q.role, tab, c, &closure, q.snap, q.own);
        if have & crate::storage::PRIV_SELECT != crate::storage::PRIV_SELECT {
            return Err(exec_err(
                "42501",
                format!("permission denied for column \"{}\" of table \"{}\"", c, t),
            ));
        }
    }
    Ok(())
}

/// Resolve a qualified name to its table: the innermost level
/// containing the qualifier wins. Returns `None` when the qualifier
/// names a view/CTE/derived table (checked at its own level) or is
/// unknown (fails later as 42P01).
fn resolve_priv_qual(
    levels: &[&Vec<(String, Option<String>)>],
    qual: Option<&str>,
) -> Option<(String, ())> {
    let qual = qual?;
    for level in levels.iter().rev() {
        if let Some((_, real)) = level.iter().find(|(qn, _)| qn == qual) {
            return real.clone().map(|t| (t, ()));
        }
    }
    None
}

/// Resolve a column reference to the (table, column) pairs it may read,
/// mirroring the executor's innermost-first scope resolution:
/// a qualified ref binds to the innermost level holding the qualifier;
/// an unqualified ref binds to the innermost level with a table
/// carrying that column (all such tables, if several — the query is
/// ambiguous and fails anyway).
fn resolve_priv_ref(
    levels: &[&Vec<(String, Option<String>)>],
    qual: Option<&str>,
    col: &str,
    has_col: &dyn Fn(&str, &str) -> bool,
) -> Vec<(String, ())> {
    if qual.is_some() {
        return resolve_priv_qual(levels, qual).into_iter().collect();
    }
    for level in levels.iter().rev() {
        let mut found = Vec::new();
        for (_, real) in level.iter() {
            if let Some(t) = real {
                if has_col(t, col) {
                    found.push((t.clone(), ()));
                }
            }
        }
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// v0.10: Common Table Expressions
// ---------------------------------------------------------------------------

/// Materialize every CTE in definition order, pushing one binding each.
/// CTE bodies are uncorrelated (like Postgres): they see sibling CTEs
/// defined earlier, never the outer query's scopes.
fn materialize_ctes(q: &mut Q, ctes: &[CteDef]) -> Result<(), ExecError> {
    for cte in ctes {
        let binding = eval_cte(q, cte)?;
        q.ctes.push(Rc::new(binding));
    }
    Ok(())
}

fn eval_cte(q: &mut Q, cte: &CteDef) -> Result<CteBinding, ExecError> {
    match &cte.body {
        CteBody::Simple(sel) => {
            let out = run_select(q, sel, &[])?;
            Ok(cte_binding(cte, out.columns, out.rows))
        }
        CteBody::Union { left, right, all } => eval_recursive_cte(q, cte, left, right, *all),
    }
}

fn cte_binding(cte: &CteDef, columns: Vec<(String, ColType)>, rows: Vec<Vec<Value>>) -> CteBinding {
    let schema: Vec<QCol> = columns
        .into_iter()
        .enumerate()
        .map(|(i, (name, ty))| QCol {
            qual: cte.name.clone(),
            name: cte.col_aliases.get(i).cloned().unwrap_or(name),
            ty,
        })
        .collect();
    let rows = rows
        .into_iter()
        .map(|cells| QRow {
            cells,
            prov: Vec::new(),
        })
        .collect();
    CteBinding {
        name: cte.name.clone(),
        schema,
        rows,
    }
}

/// v0.10: `WITH RECURSIVE`: iterative fixpoint. The seed (non-recursive
/// term) is evaluated once; then the recursive term is re-evaluated with
/// the CTE name bound to the previous iteration's *new* rows until an
/// iteration produces nothing new. UNION (distinct) deduplicates across
/// iterations, so cyclic graphs terminate; UNION ALL keeps duplicates.
/// A safety cap aborts runaway UNION ALL recursion (Postgres would loop
/// forever).
fn eval_recursive_cte(
    q: &mut Q,
    cte: &CteDef,
    left: &SelectStmt,
    right: &SelectStmt,
    all: bool,
) -> Result<CteBinding, ExecError> {
    let seed = run_select(q, left, &[])?;
    let width = seed.columns.len();
    let mut binding = cte_binding(cte, seed.columns, seed.rows);
    let seed_types: Vec<ColType> = binding.schema.iter().map(|c| c.ty.clone()).collect();
    // v0.10: non-recursive UNION in WITH RECURSIVE: evaluate the right
    // side once (it doesn't reference the CTE) and union.
    if !stmt_refs_table(right, &cte.name) {
        let other = run_select(q, right, &[])?;
        if other.columns.len() != width {
            return Err(exec_err(
                "42601",
                format!(
                    "recursive CTE \"{}\": right side returns {} columns, expected {}",
                    cte.name,
                    other.columns.len(),
                    width
                ),
            ));
        }
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        if !all {
            for r in &binding.rows {
                let mut k = Vec::new();
                for v in &r.cells {
                    value_key(v, &mut k);
                }
                seen.insert(k);
            }
        }
        for cells in other.rows {
            let mut coerced = Vec::with_capacity(cells.len());
            for (v, ty) in cells.into_iter().zip(seed_types.iter()) {
                coerced.push(coerce_value(v, ty, &cte.name)?);
            }
            if all {
                binding.rows.push(QRow {
                    cells: coerced,
                    prov: Vec::new(),
                });
            } else {
                let mut k = Vec::new();
                for v in &coerced {
                    value_key(v, &mut k);
                }
                if seen.insert(k) {
                    binding.rows.push(QRow {
                        cells: coerced,
                        prov: Vec::new(),
                    });
                }
            }
        }
        return Ok(binding);
    }
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    if !all {
        for r in &binding.rows {
            let mut k = Vec::new();
            for v in &r.cells {
                value_key(v, &mut k);
            }
            seen.insert(k);
        }
    }
    let mut working: Vec<QRow> = binding.rows.clone();
    // Cap iterations: UNION ALL over a cyclic graph never reaches a
    // fixpoint.
    for _ in 0..10_000 {
        // Bind the CTE name to the previous iteration's new rows.
        q.ctes.push(Rc::new(CteBinding {
            name: cte.name.clone(),
            schema: binding.schema.clone(),
            rows: working,
        }));
        let delta = run_select(q, right, &[]);
        q.ctes.pop();
        let delta = delta?;
        if delta.columns.len() != width {
            return Err(exec_err(
                "42601",
                format!(
                    "recursive CTE \"{}\": recursive term returns {} columns, expected {}",
                    cte.name,
                    delta.columns.len(),
                    width
                ),
            ));
        }
        let mut new_rows: Vec<QRow> = Vec::new();
        for cells in delta.rows {
            // Coerce the recursive term's cells to the seed's column
            // types (like UNION's type resolution, simplified).
            let mut coerced = Vec::with_capacity(cells.len());
            for (v, ty) in cells.into_iter().zip(seed_types.iter()) {
                coerced.push(coerce_value(v, ty, &cte.name)?);
            }
            let row = QRow {
                cells: coerced,
                prov: Vec::new(),
            };
            if all {
                new_rows.push(row);
            } else {
                let mut k = Vec::new();
                for v in &row.cells {
                    value_key(v, &mut k);
                }
                if seen.insert(k) {
                    new_rows.push(row);
                }
            }
        }
        if new_rows.is_empty() {
            return Ok(binding);
        }
        binding.rows.extend(new_rows.clone());
        working = new_rows;
    }
    Err(exec_err(
        "54001",
        format!(
            "recursive CTE \"{}\" exceeded the 10000-iteration limit (cyclic UNION ALL?)",
            cte.name
        ),
    ))
}

/// v0.10: validate one query level's CTEs: duplicate names (also caught by
/// the parser), and the recursive-CTE shape rules — the non-recursive
/// term must not reference the CTE, the recursive term must.
fn validate_ctes(ctes: &[CteDef]) -> Result<(), ExecError> {
    for cte in ctes {
        if !cte.recursive {
            continue;
        }
        let (left, right) = match &cte.body {
            CteBody::Union { left, right, .. } => (left.as_ref(), right.as_ref()),
            // v0.10: Postgres allows a non-recursive body in WITH
            // RECURSIVE; skip the recursion checks.
            CteBody::Simple(_) => continue,
        };
        if stmt_refs_table(left, &cte.name) {
            return Err(exec_err(
                "42601",
                format!(
                    "recursive CTE \"{}\": the non-recursive term must not reference the CTE",
                    cte.name
                ),
            ));
        }
        if !stmt_refs_table(right, &cte.name) {
            // v0.10: Postgres allows a non-recursive UNION in WITH
            // RECURSIVE; treat it as a single union (no iteration).
            return Ok(());
        }
    }
    Ok(())
}

/// True when the SELECT's FROM clause (descending into derived tables)
/// names the given relation.
fn stmt_refs_table(sel: &SelectStmt, name: &str) -> bool {
    sel.from.iter().any(|f| from_refs_table(f, name))
}

fn from_refs_table(f: &FromItem, name: &str) -> bool {
    match f {
        FromItem::Table { name: n, .. } => n == name,
        FromItem::Derived { sub, .. } => stmt_refs_table(sub, name),
        FromItem::Values { .. } => false,
        FromItem::Join { left, right, .. } => {
            from_refs_table(left, name) || from_refs_table(right, name)
        }
    }
}

fn run_select_inner(q: &mut Q, stmt: &SelectStmt, outer: &[Scope]) -> Result<SelectOut, ExecError> {
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
    let out_cols = describe_select(&*q.eng, q.snap, q.own, stmt, &q.ctes)?;
    // v0.8: when the whole query is a plain single-table SELECT whose
    // ORDER BY matches an index, rows stream out of the index in ORDER BY
    // order and the sort step below is skipped.
    let order_hint = plan_order_scan(&*q.eng, q.snap, q.own, stmt);
    // v0.10: collect window specs early — the early-limit optimization
    // is unsafe with windows (LIMIT must apply after windows are
    // computed over the complete input).
    let windows = collect_windows(stmt);
    let has_windows = !windows.is_empty();
    // v0.8: OFFSET+LIMIT row budget lets the index-order scan stop as
    // soon as enough visible rows are collected. Safe only together
    // with the order hint: there is no residual filter, DISTINCT, or
    // aggregation between the scan and the final truncation.
    let early_limit = if has_windows {
        None
    } else {
        order_hint.as_ref().and(match (stmt.offset, stmt.limit) {
            (_, Some(l)) => Some(stmt.offset.unwrap_or(0).max(0).saturating_add(l.max(0)) as usize),
            _ => None,
        })
    };
    let (schema, rows) = build_from(
        q,
        outer,
        &stmt.from,
        stmt.where_.as_ref(),
        stmt.for_update,
        order_hint.as_ref(),
        early_limit,
    )?;
    let rows = apply_where(q, outer, &schema, rows, stmt.where_.as_ref())?;
    // v0.10: window functions — stamp each Expr::Window with its index
    // (clone only when needed). `windows` was collected above.
    let owned_stmt: SelectStmt;
    let stmt: &SelectStmt = if windows.is_empty() {
        stmt
    } else {
        let mut s = stmt.clone();
        assign_window_ids(&mut s, &windows);
        owned_stmt = s;
        &owned_stmt
    };
    let agg = is_agg_query(stmt);
    // v0.10: precompute window values for the non-aggregated case
    // (the aggregated case is handled inside exec_agg, where groups
    // are available).
    if !windows.is_empty() && !agg {
        let inputs = gather_window_inputs_plain(q, outer, &schema, &rows, &windows)?;
        install_windows(q, &windows, &inputs)?;
    }
    // Non-aggregated queries with ORDER BY keep the full row around: an
    // ORDER BY term may reference a non-projected column (v0.1 behavior).
    let keep_full = !agg && !stmt.distinct && !stmt.order_by.is_empty();
    let mut orows: Vec<OutRow> = if agg {
        exec_agg(q, outer, stmt, &schema, &rows, &out_cols, &windows)?
    } else {
        let mut v = Vec::with_capacity(rows.len());
        for (ri, r) in rows.into_iter().enumerate() {
            let full = if keep_full {
                r.cells.clone()
            } else {
                Vec::new()
            };
            // v0.10: point the window context at this input row before
            // projecting (windows were precomputed above).
            if let Some(wctx) = q.wctx.as_mut() {
                wctx.row = ri;
            }
            // project_row takes the row by value: plain `SELECT *` moves
            // it through with zero copies, and provenance moves rather
            // than cloning.
            let (cells, prov) = project_row(q, outer, stmt, &schema, r)?;
            v.push(OutRow {
                cells,
                prov,
                full,
                sort_keys: None,
                // v0.10: ORDER BY terms with windows need the input
                // row index.
                win_idx: if windows.is_empty() { None } else { Some(ri) },
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
    if !stmt.order_by.is_empty() && order_hint.is_none() {
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
    // v0.10: CTE shape rules (recursive UNION discipline).
    validate_ctes(&stmt.with)?;
    // v0.10: window-function placement rules.
    validate_windows(stmt)?;
    if let Some(w) = &stmt.where_ {
        if contains_agg(w) {
            return Err(exec_err(
                "42803",
                "aggregates are not allowed in WHERE clause",
            ));
        }
        if contains_window(w) {
            return Err(exec_err(
                "42803",
                "window functions are not allowed in WHERE clause",
            ));
        }
        validate_expr(w)?;
    }
    for g in &stmt.group_by {
        if contains_agg(g) {
            return Err(exec_err("42803", "aggregates are not allowed in GROUP BY"));
        }
        if contains_window(g) {
            return Err(exec_err(
                "42803",
                "window functions are not allowed in GROUP BY",
            ));
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
        if contains_window(h) {
            return Err(exec_err(
                "42803",
                "window functions are not allowed in HAVING",
            ));
        }
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
        FromItem::Values { rows, .. } => {
            for row in rows {
                for e in row {
                    validate_expr(e)?;
                }
            }
            Ok(())
        }
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
        Expr::Arith { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Concat(left, right) => {
            validate_expr(left)?;
            validate_expr(right)
        }
        Expr::Cmp { left, right, .. } => {
            validate_expr(left)?;
            validate_expr(right)
        }
        Expr::Like { expr, pattern, .. } => {
            validate_expr(expr)?;
            validate_expr(pattern)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_expr(expr)?;
            validate_expr(low)?;
            validate_expr(high)
        }
        Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => {
            validate_expr(x)
        }
        Expr::Cast { expr, .. } => validate_expr(expr),
        Expr::Func { args, .. } => {
            for a in args {
                validate_expr(a)?;
            }
            Ok(())
        }
        Expr::Extract { from, .. } => validate_expr(from),
        Expr::Agg { arg, arg2, .. } => {
            if let Some(a) = arg {
                validate_expr(a)?;
            }
            if let Some(a) = arg2 {
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
        Expr::Arith { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Concat(left, right) => contains_agg(left) || contains_agg(right),
        Expr::Cmp { left, right, .. } => contains_agg(left) || contains_agg(right),
        Expr::Like { expr, pattern, .. } => contains_agg(expr) || contains_agg(pattern),
        Expr::Between {
            expr, low, high, ..
        } => contains_agg(expr) || contains_agg(low) || contains_agg(high),
        Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => {
            contains_agg(x)
        }
        Expr::Cast { expr, .. } => contains_agg(expr),
        Expr::Func { args, .. } => args.iter().any(contains_agg),
        Expr::Extract { from, .. } => contains_agg(from),
        Expr::InSub { expr, .. } => contains_agg(expr),
        // ScalarSub / Exists are separate query levels.
        Expr::ScalarSub(_) | Expr::Exists { .. } => false,
        // v0.10: a window counts as an aggregate when any of its input
        // expressions does (e.g. `sum(x) OVER (...)`).
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(contains_agg)
                || partition_by.iter().any(contains_agg)
                || order_by.iter().any(|o| contains_agg(&o.expr))
        }
    }
}

/// v0.10: true when the expression contains a window function (at any
/// depth, but not crossing into subquery levels).
fn contains_window(e: &Expr) -> bool {
    match e {
        Expr::Window { .. } => true,
        Expr::Column { .. } | Expr::ResolvedCol { .. } | Expr::Literal(_) | Expr::Param(_) => false,
        Expr::Arith { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Concat(left, right) => contains_window(left) || contains_window(right),
        Expr::Cmp { left, right, .. } => contains_window(left) || contains_window(right),
        Expr::Like { expr, pattern, .. } => contains_window(expr) || contains_window(pattern),
        Expr::Between {
            expr, low, high, ..
        } => contains_window(expr) || contains_window(low) || contains_window(high),
        Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => {
            contains_window(x)
        }
        Expr::Cast { expr, .. } => contains_window(expr),
        Expr::Func { args, .. } => args.iter().any(contains_window),
        Expr::Agg { arg, arg2, .. } => {
            arg.as_deref().map(contains_window).unwrap_or(false)
                || arg2.as_deref().map(contains_window).unwrap_or(false)
        }
        Expr::Extract { from, .. } => contains_window(from),
        Expr::InSub { expr, .. } => contains_window(expr),
        // Subqueries are separate query levels.
        Expr::ScalarSub(_) | Expr::Exists { .. } => false,
    }
}

/// v0.10: COPY support — data layer for the server's COPY protocol.
// ---------------------------------------------------------------------------

/// v0.10: resolve the column count for a COPY target (explicit column
/// list, or the table's full width).
pub fn copy_ncols(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    table: &str,
    columns: &Option<Vec<String>>,
) -> Result<usize, ExecError> {
    if let Some(cols) = columns {
        // Validate the names while we're at it.
        let t = eng
            .db
            .find_table(table, snap, own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let meta = TableMeta::of(t);
        for n in cols {
            if !meta.columns.iter().any(|(c, _)| c == n) {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", n, table),
                ));
            }
        }
        Ok(cols.len())
    } else {
        let t = eng
            .db
            .find_table(table, snap, own)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        Ok(TableMeta::of(t).columns.len())
    }
}

/// v0.10: COPY TO STDOUT — run `SELECT <cols> FROM <table>` and return
/// the (name, type) columns and the visible rows.
pub fn copy_to_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    columns: &Option<Vec<String>>,
) -> Result<(Vec<(String, ColType)>, Vec<Vec<Value>>), ExecError> {
    // Validate the table/columns first (clean 42P01/42703 errors).
    copy_ncols(eng, ctx.snap, ctx.own, table, columns)?;
    let col_list = match columns {
        Some(cols) => cols
            .iter()
            .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", "),
        None => "*".to_string(),
    };
    let sql = format!(
        "SELECT {} FROM \"{}\"",
        col_list,
        table.replace('"', "\"\"")
    );
    let stmt = crate::sql::parse_statement(&sql).map_err(|e| ExecError {
        code: e.code,
        message: e.message,
    })?;
    match execute(eng, ctx, &stmt)? {
        ExecResult::Select { columns, rows } => Ok((columns, rows)),
        _ => Err(ExecError {
            code: "XX000",
            message: "internal error: COPY TO did not return rows".to_string(),
        }),
    }
}

/// v0.10: COPY FROM STDIN — insert pre-parsed rows. Returns the row count.
pub fn copy_from_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    columns: &Option<Vec<String>>,
    rows: Vec<Vec<crate::copy::CopyField>>,
) -> Result<u64, ExecError> {
    use crate::copy::CopyField;
    use crate::sql::Literal;
    let insert_rows: Vec<Vec<InsertValue>> = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|f| match f {
                    CopyField::Text(s) => InsertValue::Lit(Literal::Text(s)),
                    CopyField::Null => InsertValue::Lit(Literal::Null),
                })
                .collect()
        })
        .collect();
    let n = insert_rows.len() as u64;
    // Reuse the full INSERT path: coercion, defaults, constraints,
    // unique indexes, foreign keys, WAL — atomically.
    let _ = exec_insert(
        eng,
        ctx,
        table,
        columns,
        &insert_rows,
        &None,
        &[],
        &None,
        &[],
    )?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// v0.10: Window functions
// ---------------------------------------------------------------------------

/// v0.10: an executable window specification, deduplicated across the
/// query level. `wid` indexes the precomputed values in the query
/// context (`Q.wctx`).
#[derive(Clone, Debug, PartialEq)]
struct ExecWindow {
    func: WindowFunc,
    args: Vec<Expr>,
    distinct: bool,
    partition_by: Vec<Expr>,
    order_by: Vec<OrderTerm>,
    frame: WindowFrame,
}

/// v0.10: validate every window function at this query level: arity,
/// no nested windows, no window inside an aggregate argument, and
/// frame discipline.
fn validate_windows(stmt: &SelectStmt) -> Result<(), ExecError> {
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            validate_window_expr(expr, false)?;
        }
    }
    for o in &stmt.order_by {
        validate_window_expr(&o.expr, false)?;
    }
    Ok(())
}

/// v0.10: `in_window` tracks whether we are inside a window's input
/// expressions (nested windows are forbidden); `in_agg` tracks
/// aggregate arguments (windows are forbidden there).
fn validate_window_expr(e: &Expr, in_agg: bool) -> Result<(), ExecError> {
    match e {
        Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            ..
        } => {
            if in_agg {
                return Err(exec_err(
                    "42803",
                    "window functions are not allowed in aggregate arguments",
                ));
            }
            check_window_arity(func, args.len())?;
            if *distinct {
                return Err(exec_err(
                    "0A000",
                    "DISTINCT is not supported in window functions",
                ));
            }
            for a in args {
                if contains_window(a) {
                    return Err(exec_err("42803", "window functions cannot be nested"));
                }
                validate_window_expr(a, false)?;
            }
            for p in partition_by {
                if contains_window(p) {
                    return Err(exec_err("42803", "window functions cannot be nested"));
                }
                validate_window_expr(p, false)?;
            }
            for o in order_by {
                if contains_window(&o.expr) {
                    return Err(exec_err("42803", "window functions cannot be nested"));
                }
                validate_window_expr(&o.expr, false)?;
            }
            check_window_frame(frame, order_by.len())?;
            // Frame bounds must be constant.
            Ok(())
        }
        Expr::Agg { arg, arg2, .. } => {
            if let Some(a) = arg {
                validate_window_expr(a, true)?;
            }
            if let Some(a) = arg2 {
                validate_window_expr(a, true)?;
            }
            Ok(())
        }
        Expr::Arith { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Concat(left, right) => {
            validate_window_expr(left, in_agg)?;
            validate_window_expr(right, in_agg)
        }
        Expr::Cmp { left, right, .. } => {
            validate_window_expr(left, in_agg)?;
            validate_window_expr(right, in_agg)
        }
        Expr::Like { expr, pattern, .. } => {
            validate_window_expr(expr, in_agg)?;
            validate_window_expr(pattern, in_agg)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_window_expr(expr, in_agg)?;
            validate_window_expr(low, in_agg)?;
            validate_window_expr(high, in_agg)
        }
        Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => {
            validate_window_expr(x, in_agg)
        }
        Expr::Cast { expr, .. } => validate_window_expr(expr, in_agg),
        Expr::Func { args, .. } => {
            for a in args {
                validate_window_expr(a, in_agg)?;
            }
            Ok(())
        }
        Expr::Extract { from, .. } => validate_window_expr(from, in_agg),
        Expr::InSub { expr, .. } => validate_window_expr(expr, in_agg),
        Expr::Column { .. }
        | Expr::ResolvedCol { .. }
        | Expr::Literal(_)
        | Expr::Param(_)
        | Expr::ScalarSub(_)
        | Expr::Exists { .. } => Ok(()),
    }
}

/// v0.10: argument counts per window function (Postgres arities).
fn check_window_arity(func: &WindowFunc, n: usize) -> Result<(), ExecError> {
    let ok = match func {
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => n == 0,
        WindowFunc::Ntile => n == 1,
        WindowFunc::Lag | WindowFunc::Lead => n >= 1 && n <= 3,
        WindowFunc::FirstValue | WindowFunc::LastValue => n == 1,
        WindowFunc::NthValue => n == 2,
        WindowFunc::Agg(f) => check_agg_window_arity(f, n),
    };
    if !ok {
        return Err(exec_err(
            "42883",
            format!("wrong number of arguments to window function (got {})", n),
        ));
    }
    Ok(())
}

/// v0.10: aggregate arities when used as window functions.
fn check_agg_window_arity(f: &AggFunc, n: usize) -> bool {
    match f {
        AggFunc::Count => n <= 1,
        AggFunc::Sum | AggFunc::Avg | AggFunc::Min | AggFunc::Max => n == 1,
        AggFunc::StringAgg => false,
    }
}

/// v0.10: frame discipline (Postgres restrictions).
fn check_window_frame(frame: &WindowFrame, order_len: usize) -> Result<(), ExecError> {
    match frame {
        WindowFrame::Default => Ok(()),
        WindowFrame::Rows { .. } => Ok(()),
        WindowFrame::Range { start, end } => {
            if has_offset_bound(start) || has_offset_bound(end) {
                if order_len != 1 {
                    return Err(exec_err(
                        "0A000",
                        "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY expression",
                    ));
                }
            }
            Ok(())
        }
    }
}

/// v0.10: true for `N PRECEDING` / `N FOLLOWING` bounds.
fn has_offset_bound(b: &FrameBound) -> bool {
    matches!(b, FrameBound::Preceding(_) | FrameBound::Following(_))
}

/// v0.10: collect the deduplicated window specifications used at this
/// query level (SELECT list and ORDER BY).
fn collect_windows(stmt: &SelectStmt) -> Vec<ExecWindow> {
    let mut out: Vec<ExecWindow> = Vec::new();
    let mut visit = |e: &Expr| {
        if let Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            ..
        } = e
        {
            let w = ExecWindow {
                func: func.clone(),
                args: args.clone(),
                distinct: *distinct,
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: frame.clone(),
            };
            if !out.contains(&w) {
                out.push(w);
            }
        }
    };
    // Walk select items and ORDER BY terms (windows are validated to
    // live only there).
    fn walk(e: &Expr, visit: &mut impl FnMut(&Expr)) {
        match e {
            Expr::Window { .. } => visit(e),
            Expr::Arith { left, right, .. }
            | Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::Concat(left, right) => {
                walk(left, visit);
                walk(right, visit);
            }
            Expr::Cmp { left, right, .. } => {
                walk(left, visit);
                walk(right, visit);
            }
            Expr::Like { expr, pattern, .. } => {
                walk(expr, visit);
                walk(pattern, visit);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                walk(expr, visit);
                walk(low, visit);
                walk(high, visit);
            }
            Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => {
                walk(x, visit)
            }
            Expr::Cast { expr, .. } => walk(expr, visit),
            Expr::Func { args, .. } => {
                for a in args {
                    walk(a, visit);
                }
            }
            Expr::Agg { arg, arg2, .. } => {
                if let Some(a) = arg {
                    walk(a, visit);
                }
                if let Some(a) = arg2 {
                    walk(a, visit);
                }
            }
            Expr::Extract { from, .. } => walk(from, visit),
            Expr::InSub { expr, .. } => walk(expr, visit),
            _ => {}
        }
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk(expr, &mut visit);
        }
    }
    for o in &stmt.order_by {
        walk(&o.expr, &mut visit);
    }
    out
}

/// v0.10: stamp each `Expr::Window` with its deduplicated index.
fn assign_window_ids(stmt: &mut SelectStmt, windows: &[ExecWindow]) {
    fn stamp(e: &mut Expr, windows: &[ExecWindow]) {
        match e {
            Expr::Window {
                func,
                args,
                distinct,
                partition_by,
                order_by,
                frame,
                wid,
            } => {
                let w = ExecWindow {
                    func: func.clone(),
                    args: args.clone(),
                    distinct: *distinct,
                    partition_by: partition_by.clone(),
                    order_by: order_by.clone(),
                    frame: frame.clone(),
                };
                *wid = windows.iter().position(|x| x == &w).unwrap_or(0);
            }
            Expr::Arith { left, right, .. }
            | Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::Concat(left, right) => {
                stamp(left, windows);
                stamp(right, windows);
            }
            Expr::Cmp { left, right, .. } => {
                stamp(left, windows);
                stamp(right, windows);
            }
            Expr::Like { expr, pattern, .. } => {
                stamp(expr, windows);
                stamp(pattern, windows);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                stamp(expr, windows);
                stamp(low, windows);
                stamp(high, windows);
            }
            Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => {
                stamp(x, windows)
            }
            Expr::Cast { expr, .. } => stamp(expr, windows),
            Expr::Func { args, .. } => {
                for a in args {
                    stamp(a, windows);
                }
            }
            Expr::Agg { arg, arg2, .. } => {
                if let Some(a) = arg {
                    stamp(a, windows);
                }
                if let Some(a) = arg2 {
                    stamp(a, windows);
                }
            }
            Expr::Extract { from, .. } => stamp(from, windows),
            Expr::InSub { expr, .. } => stamp(expr, windows),
            _ => {}
        }
    }
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            stamp(expr, windows);
        }
    }
    for o in &mut stmt.order_by {
        stamp(&mut o.expr, windows);
    }
}

/// v0.10: precomputed input values for one window: partition keys,
/// order keys, and argument values, one entry per input row.
struct WindowInput {
    part_keys: Vec<Vec<Value>>,
    order_keys: Vec<Vec<Value>>,
    arg_vals: Vec<Vec<Value>>,
}

/// v0.10: compare two order-key vectors using the window's ORDER BY
/// terms (direction + null placement).
fn compare_window_keys(
    a: &[Value],
    b: &[Value],
    order_by: &[OrderTerm],
) -> Result<Ordering, ExecError> {
    for (i, o) in order_by.iter().enumerate() {
        let av = a.get(i).unwrap_or(&Value::Null);
        let bv = b.get(i).unwrap_or(&Value::Null);
        let ord = compare_values(av, bv, o.desc, o.nulls_first)?;
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// v0.10: peer test for ranking and RANGE frames: all order keys equal
/// (NULL = NULL for peer grouping, like Postgres).
fn window_keys_equal(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        let eq = match (x, y) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            _ => {
                let mut ka = Vec::new();
                let mut kb = Vec::new();
                value_key(x, &mut ka);
                value_key(y, &mut kb);
                ka == kb
            }
        };
        if !eq {
            return false;
        }
    }
    true
}

/// v0.10: resolve a ROWS frame bound to an inclusive row position.
fn rows_bound(b: &FrameBound, pos: usize, n: usize) -> usize {
    match b {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::Preceding(k) => pos.saturating_sub((*k).min(usize::MAX as u64) as usize),
        FrameBound::CurrentRow => pos,
        FrameBound::Following(k) => pos
            .saturating_add((*k).min(usize::MAX as u64) as usize)
            .min(n.saturating_sub(1)),
        FrameBound::UnboundedFollowing => n.saturating_sub(1),
    }
}

/// v0.10: resolve a window frame to an inclusive (start, end) position
/// range within the ordered partition.
fn resolve_frame(
    spec: &ExecWindow,
    input: &WindowInput,
    idxs: &[usize],
    pos: usize,
) -> Result<(usize, usize), ExecError> {
    let n = idxs.len();
    if n == 0 {
        return Ok((0, 0));
    }
    let order_len = spec.order_by.len();
    // Effective frame (Postgres default rule).
    let (is_range, start, end): (bool, FrameBound, FrameBound) = match &spec.frame {
        WindowFrame::Default => {
            if order_len > 0 {
                (true, FrameBound::UnboundedPreceding, FrameBound::CurrentRow)
            } else {
                (
                    false,
                    FrameBound::UnboundedPreceding,
                    FrameBound::UnboundedFollowing,
                )
            }
        }
        WindowFrame::Rows { start, end } => (false, start.clone(), end.clone()),
        WindowFrame::Range { start, end } => (true, start.clone(), end.clone()),
    };
    if !is_range {
        let s = rows_bound(&start, pos, n);
        let e = rows_bound(&end, pos, n);
        // v0.10: an inverted span is empty (not silently reversed).
        if s > e {
            return Ok((1, 0));
        }
        return Ok((s, e));
    }
    // RANGE mode without ORDER BY: no ordering values exist, so every
    // row is a peer of every other; resolve positionally like ROWS.
    let key_at = |p: usize| -> &[Value] { &input.order_keys[idxs[p]] };
    if order_len == 0 {
        let s = rows_bound(&start, pos, n);
        let e = rows_bound(&end, pos, n);
        if s > e {
            return Ok((1, 0));
        }
        return Ok((s, e));
    }
    // RANGE mode: the frame is all rows whose single order key lies in
    // the [lo, hi] value interval derived from the bounds (Postgres
    // value-based RANGE semantics; CURRENT ROW expands to peers via the
    // interval). NULL order keys never match.
    if order_len > 1
        && matches!(
            (&start, &end),
            (FrameBound::Preceding(_), _)
                | (FrameBound::Following(_), _)
                | (_, FrameBound::Preceding(_))
                | (_, FrameBound::Following(_))
        )
    {
        return Err(exec_err(
            "0A000",
            "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY expression",
        ));
    }
    let cur = key_at(pos).first().unwrap_or(&Value::Null).clone();
    // v0.10: RANGE without offsets uses peer semantics (works for any
    // orderable type, like Postgres); offsets require numeric.
    let has_offset = has_offset_bound(&start) || has_offset_bound(&end);
    if !has_offset {
        // Peer-based: expand CURRENT ROW to the peer group.
        // Use the ORDER BY direction for the comparison.
        let (desc, nulls_first) = spec
            .order_by
            .first()
            .map(|o| (o.desc, o.nulls_first))
            .unwrap_or((false, None));
        let is_peer = |a: &Value, b: &Value| -> bool {
            // NULLs are peers of each other (they sort together).
            match (a, b) {
                (Value::Null, Value::Null) => true,
                (Value::Null, _) | (_, Value::Null) => false,
                _ => matches!(
                    compare_values(a, b, desc, nulls_first),
                    Ok(std::cmp::Ordering::Equal)
                ),
            }
        };
        let bound_pos = |b: &FrameBound, is_start: bool| -> usize {
            match b {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::UnboundedFollowing => n - 1,
                FrameBound::CurrentRow => {
                    if is_start {
                        // First peer at or before pos.
                        let mut s = pos;
                        while s > 0 && is_peer(key_at(s - 1).first().unwrap_or(&Value::Null), &cur)
                        {
                            s -= 1;
                        }
                        s
                    } else {
                        // Last peer at or after pos.
                        let mut e = pos;
                        while e + 1 < n
                            && is_peer(key_at(e + 1).first().unwrap_or(&Value::Null), &cur)
                        {
                            e += 1;
                        }
                        e
                    }
                }
                // Unreachable: has_offset is false.
                FrameBound::Preceding(_) | FrameBound::Following(_) => pos,
            }
        };
        let s = bound_pos(&start, true);
        let e = bound_pos(&end, false);
        // An inverted span (e.g. start after end) is empty.
        if s > e {
            return Ok((1, 0));
        }
        return Ok((s, e));
    }
    // RANGE with offsets: numeric interval semantics.
    if matches!(cur, Value::Null) {
        return Err(exec_err(
            "0A000",
            "RANGE offset requires a numeric ORDER BY expression",
        ));
    }
    let to_f = |v: &Value| -> Result<f64, ExecError> {
        match v {
            Value::Int(i) => Ok(*i as f64),
            Value::BigInt(i) => Ok(*i as f64),
            Value::Float(f) => Ok(*f),
            Value::Numeric(n) => Ok(n.to_f64()),
            _ => Err(exec_err(
                "0A000",
                "RANGE offset requires a numeric ORDER BY expression",
            )),
        }
    };
    let cur_f = if matches!(cur, Value::Null) {
        f64::NAN
    } else {
        to_f(&cur)?
    };
    let bound_val = |b: &FrameBound| -> Result<f64, ExecError> {
        match b {
            FrameBound::UnboundedPreceding => Ok(f64::NEG_INFINITY),
            FrameBound::UnboundedFollowing => Ok(f64::INFINITY),
            FrameBound::CurrentRow => Ok(cur_f),
            FrameBound::Preceding(k) => Ok(cur_f - (*k as f64)),
            FrameBound::Following(k) => Ok(cur_f + (*k as f64)),
        }
    };
    // Note: for the start bound, PRECEDING gives lo; for the end bound,
    // FOLLOWING gives hi. (A start FOLLOWING / end PRECEDING was
    // rejected by the parser's frame sanity check.)
    let lo = bound_val(&start)?;
    let hi = bound_val(&end)?;
    let mut s = n;
    let mut e = n;
    for p in 0..n {
        let v = key_at(p).first().unwrap_or(&Value::Null);
        if matches!(v, Value::Null) {
            continue;
        }
        let f = to_f(v)?;
        if f >= lo && f <= hi {
            if s == n {
                s = p;
            }
            e = p;
        }
    }
    if s == n {
        return Ok((1, 0)); // empty frame
    }
    Ok((s, e))
}

/// v0.10: gather window inputs (partition keys, order keys, argument
/// values) for the non-aggregated case: one entry per filtered row.
fn gather_window_inputs_plain(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: &[QRow],
    specs: &[ExecWindow],
) -> Result<Vec<WindowInput>, ExecError> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let mut part_keys = Vec::with_capacity(rows.len());
        let mut order_keys = Vec::with_capacity(rows.len());
        let mut arg_vals = Vec::with_capacity(rows.len());
        for r in rows {
            let frame = Scope {
                schema,
                row: &r.cells,
            };
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(frame);
            // Window inputs cannot contain window functions (validated)
            // or aggregates (that would make this an aggregate query).
            let mut pk = Vec::with_capacity(spec.partition_by.len());
            for p in &spec.partition_by {
                pk.push(eval_expr(q, &scopes, p)?);
            }
            let mut ok = Vec::with_capacity(spec.order_by.len());
            for o in &spec.order_by {
                ok.push(eval_expr(q, &scopes, &o.expr)?);
            }
            let mut av = Vec::with_capacity(spec.args.len());
            for a in &spec.args {
                av.push(eval_expr(q, &scopes, a)?);
            }
            part_keys.push(pk);
            order_keys.push(ok);
            arg_vals.push(av);
        }
        out.push(WindowInput {
            part_keys,
            order_keys,
            arg_vals,
        });
    }
    Ok(out)
}

/// v0.10: gather window inputs for the aggregated case: one entry per
/// group, evaluated with the group's context (aggregates and GROUP BY
/// keys via `eval_grouped`).
fn gather_window_inputs_grouped(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: &[QRow],
    groups: &[(Vec<Value>, Vec<usize>)],
    // v0.10: indices of groups surviving HAVING (windows see only these,
    // like Postgres).
    surviving: &[usize],
    group_by: &[Expr],
    specs: &[ExecWindow],
) -> Result<Vec<WindowInput>, ExecError> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let mut part_keys = Vec::with_capacity(surviving.len());
        let mut order_keys = Vec::with_capacity(surviving.len());
        let mut arg_vals = Vec::with_capacity(surviving.len());
        for &gi in surviving {
            let (key_vals, idxs) = &groups[gi];
            let first: &[Value] = match idxs.first() {
                Some(&i) => &rows[i].cells,
                None => &[],
            };
            let gscope = Scope { schema, row: first };
            let mut pk = Vec::with_capacity(spec.partition_by.len());
            for p in &spec.partition_by {
                pk.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, p,
                )?);
            }
            let mut ok = Vec::with_capacity(spec.order_by.len());
            for o in &spec.order_by {
                ok.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, &o.expr,
                )?);
            }
            let mut av = Vec::with_capacity(spec.args.len());
            for a in &spec.args {
                av.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, a,
                )?);
            }
            part_keys.push(pk);
            order_keys.push(ok);
            arg_vals.push(av);
        }
        out.push(WindowInput {
            part_keys,
            order_keys,
            arg_vals,
        });
    }
    Ok(out)
}

/// v0.10: compute every window's value vector and install the query's
/// window context.
fn install_windows(
    q: &mut Q,
    specs: &[ExecWindow],
    inputs: &[WindowInput],
) -> Result<(), ExecError> {
    let mut values = Vec::with_capacity(specs.len());
    for (spec, input) in specs.iter().zip(inputs.iter()) {
        values.push(compute_window_values(spec, input)?);
    }
    q.wctx = Some(WindowCtx { values, row: 0 });
    Ok(())
}

/// v0.10: compute one window's value for every input row.
fn compute_window_values(spec: &ExecWindow, input: &WindowInput) -> Result<Vec<Value>, ExecError> {
    let nrows = input.arg_vals.len();
    let mut result = vec![Value::Null; nrows];
    if nrows == 0 {
        return Ok(result);
    }
    // Partition rows by partition-key.
    let mut parts: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    let mut part_order: Vec<Vec<u8>> = Vec::new();
    for i in 0..nrows {
        let mut k = Vec::new();
        for v in &input.part_keys[i] {
            value_key(v, &mut k);
        }
        if !parts.contains_key(&k) {
            part_order.push(k.clone());
        }
        parts.entry(k).or_default().push(i);
    }
    for pk in &part_order {
        let mut idxs = parts[pk].clone();
        // Order within the partition (stable: ties keep input order).
        if !spec.order_by.is_empty() {
            let mut err: Option<ExecError> = None;
            idxs.sort_by(|&a, &b| {
                if err.is_some() {
                    return Ordering::Equal;
                }
                match compare_window_keys(
                    &input.order_keys[a],
                    &input.order_keys[b],
                    &spec.order_by,
                ) {
                    Ok(o) => o,
                    Err(e) => {
                        err = Some(e);
                        Ordering::Equal
                    }
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
        let vals = compute_partition(spec, input, &idxs)?;
        for (j, &row_idx) in idxs.iter().enumerate() {
            result[row_idx] = vals[j].clone();
        }
    }
    Ok(result)
}

/// v0.10: compute a window function over one ordered partition.
/// `idxs` are input-row indices in partition order; returns one value
/// per position.
fn compute_partition(
    spec: &ExecWindow,
    input: &WindowInput,
    idxs: &[usize],
) -> Result<Vec<Value>, ExecError> {
    let n = idxs.len();
    let mut out = vec![Value::Null; n];
    // Peer groups (for rank/dense_rank and RANGE): consecutive rows
    // with equal order keys.
    let mut peer_id = vec![0usize; n];
    if !spec.order_by.is_empty() && n > 0 {
        let mut p = 0;
        for i in 1..n {
            if !window_keys_equal(&input.order_keys[idxs[i]], &input.order_keys[idxs[i - 1]]) {
                p += 1;
            }
            peer_id[i] = p;
        }
    }
    let arg = |pos: usize, k: usize| -> Value {
        input
            .arg_vals
            .get(idxs[pos])
            .and_then(|v| v.get(k))
            .cloned()
            .unwrap_or(Value::Null)
    };
    match &spec.func {
        WindowFunc::RowNumber => {
            for (j, v) in out.iter_mut().enumerate() {
                *v = Value::BigInt(j as i64 + 1);
            }
        }
        WindowFunc::Rank => {
            for j in 0..n {
                // 1 + rows before the first peer.
                let mut first = j;
                while first > 0 && peer_id[first - 1] == peer_id[j] {
                    first -= 1;
                }
                out[j] = Value::BigInt(first as i64 + 1);
            }
        }
        WindowFunc::DenseRank => {
            for j in 0..n {
                out[j] = Value::BigInt(peer_id[j] as i64 + 1);
            }
        }
        WindowFunc::Ntile => {
            let k_val = arg(0, 0);
            let k = match k_val {
                Value::Int(i) => i as usize,
                Value::BigInt(i) => i.max(0) as usize,
                Value::Null => return Err(exec_err("22004", "ntile argument must not be null")),
                _ => return Err(exec_err("22003", "ntile argument must be an integer")),
            };
            if k == 0 {
                return Err(exec_err("22003", "ntile argument must be positive"));
            }
            // As evenly as possible, larger buckets first (Postgres).
            let base = n / k;
            let rem = n % k;
            let mut pos = 0;
            for b in 0..k {
                let size = base + if b < rem { 1 } else { 0 };
                for _ in 0..size {
                    if pos < n {
                        out[pos] = Value::Int((b + 1) as i64);
                    }
                    pos += 1;
                }
            }
        }
        WindowFunc::Lag | WindowFunc::Lead => {
            let is_lag = matches!(spec.func, WindowFunc::Lag);
            for j in 0..n {
                let off_val = if input.arg_vals.get(idxs[j]).map(|v| v.len()).unwrap_or(0) > 1 {
                    arg(j, 1)
                } else {
                    Value::Int(1)
                };
                let off: i64 = match off_val {
                    Value::Int(i) => i as i64,
                    Value::BigInt(i) => i,
                    Value::Null => {
                        return Err(exec_err("22004", "lag/lead offset must not be null"));
                    }
                    _ => return Err(exec_err("22003", "lag/lead offset must be an integer")),
                };
                if off < 0 {
                    return Err(exec_err("22003", "lag/lead offset must not be negative"));
                }
                let target = if is_lag {
                    (j as i64) - off
                } else {
                    (j as i64) + off
                };
                out[j] = if target >= 0 && (target as usize) < n {
                    arg(target as usize, 0)
                } else if input.arg_vals.get(idxs[j]).map(|v| v.len()).unwrap_or(0) > 2 {
                    arg(j, 2)
                } else {
                    Value::Null
                };
            }
        }
        WindowFunc::FirstValue => {
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j)?;
                out[j] = if s <= e { arg(s, 0) } else { Value::Null };
            }
        }
        WindowFunc::LastValue => {
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j)?;
                out[j] = if s <= e { arg(e, 0) } else { Value::Null };
            }
        }
        WindowFunc::NthValue => {
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j)?;
                let nth = match arg(j, 1) {
                    Value::Int(i) => i as i64,
                    Value::BigInt(i) => i,
                    Value::Null => {
                        return Err(exec_err("22004", "nth_value argument must not be null"));
                    }
                    _ => return Err(exec_err("22003", "nth_value argument must be an integer")),
                };
                out[j] = if nth >= 1 && s <= e && (s as i64) + nth - 1 <= e as i64 {
                    arg((s as i64 + nth - 1) as usize, 0)
                } else {
                    Value::Null
                };
            }
        }
        WindowFunc::Agg(f) => {
            // count(*) counts rows (NULLs included); other aggregates
            // skip NULL inputs, like their grouped counterparts.
            let count_star = *f == AggFunc::Count && spec.args.is_empty();
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j)?;
                let mut vals: Vec<Value> = Vec::new();
                if s <= e {
                    for p in s..=e {
                        let v = arg(p, 0);
                        if count_star || !matches!(v, Value::Null) {
                            vals.push(v);
                        }
                    }
                }
                out[j] = eval_window_agg(*f, &vals)?;
            }
        }
    }
    Ok(out)
}

/// v0.10: evaluate a windowed aggregate over frame values.
fn eval_window_agg(f: AggFunc, vals: &[Value]) -> Result<Value, ExecError> {
    match f {
        AggFunc::Count => {
            // count(x): non-null inputs; count(*): all rows. (NULLs are
            // filtered by the caller for count(x).)
            Ok(Value::BigInt(vals.len() as i64))
        }
        AggFunc::Sum => sum_vals(vals),
        AggFunc::Avg => avg_vals(vals),
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<&Value> = None;
            for v in vals {
                match best {
                    None => best = Some(v),
                    Some(b) => {
                        let ord = cmp_ordering(v, b, CmpOp::Lt)?.expect("non-null values compare");
                        let better = if f == AggFunc::Min {
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
        AggFunc::StringAgg => Err(exec_err(
            "0A000",
            "string_agg is not supported as a window function",
        )),
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
    // v0.8: index-order scan hint for the single-table fast path; always
    // None when `from` has more than one item.
    order_hint: Option<&OrderHint>,
    // v0.8: OFFSET+LIMIT row budget for early termination of the
    // index-order scan (only meaningful together with `order_hint`).
    early_limit: Option<usize>,
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
        let (s2, r2) = build_source(
            q,
            outer,
            &from[0],
            where_,
            need_prov,
            order_hint,
            early_limit,
        )?;
        let quals: HashSet<String> = s2.iter().map(|c| c.qual.clone()).collect();
        let no_quals = HashSet::new();
        let push = pushdown_for(where_, &quals, &no_quals, &s2, &[]);
        let r2 = filter_rows(q, outer, &s2, r2, &push)?;
        return Ok((s2, r2));
    }
    let mut acc_schema: Vec<QCol> = Vec::new();
    let mut acc_rows = vec![QRow::default()];
    for item in from {
        let (s2, mut r2) = build_source(q, outer, item, where_, need_prov, None, None)?;
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
        Expr::Arith { left, right, .. } => {
            pushable_columns(left, cols) && pushable_columns(right, cols)
        }
        Expr::Concat(a, b) => pushable_columns(a, cols) && pushable_columns(b, cols),
        Expr::Cast { expr, .. } => pushable_columns(expr, cols),
        Expr::Like { expr, pattern, .. } => {
            pushable_columns(expr, cols) && pushable_columns(pattern, cols)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            pushable_columns(expr, cols)
                && pushable_columns(low, cols)
                && pushable_columns(high, cols)
        }
        Expr::Func { args, .. } => args.iter().all(|a| pushable_columns(a, cols)),
        Expr::Extract { from, .. } => pushable_columns(from, cols),
        Expr::Cmp { left, right, .. } => {
            pushable_columns(left, cols) && pushable_columns(right, cols)
        }
        Expr::And(a, b) | Expr::Or(a, b) => pushable_columns(a, cols) && pushable_columns(b, cols),
        Expr::Not(x) => pushable_columns(x, cols),
        Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => pushable_columns(x, cols),
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
        Expr::Arith { left, right, .. } => {
            collect_column_refs(left, out);
            collect_column_refs(right, out);
        }
        Expr::Concat(a, b) => {
            collect_column_refs(a, out);
            collect_column_refs(b, out);
        }
        Expr::Cast { expr, .. } => collect_column_refs(expr, out),
        Expr::Like { expr, pattern, .. } => {
            collect_column_refs(expr, out);
            collect_column_refs(pattern, out);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_column_refs(expr, out);
            collect_column_refs(low, out);
            collect_column_refs(high, out);
        }
        Expr::IsBool { expr: x, .. } => collect_column_refs(x, out),
        Expr::Func { args, .. } => {
            for a in args {
                collect_column_refs(a, out);
            }
        }
        Expr::Extract { from, .. } => collect_column_refs(from, out),
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
        Expr::Agg { arg, arg2, .. } => {
            if let Some(x) = arg {
                collect_column_refs(x, out);
            }
            if let Some(x) = arg2 {
                collect_column_refs(x, out);
            }
        }
        Expr::ScalarSub(s) => collect_stmt_refs(s, out),
        Expr::InSub { expr, sub, .. } => {
            collect_column_refs(expr, out);
            collect_stmt_refs(sub, out);
        }
        Expr::Exists { sub, .. } => collect_stmt_refs(sub, out),
        // v0.10: collect column refs from window inputs.
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                collect_column_refs(a, out);
            }
            for p in partition_by {
                collect_column_refs(p, out);
            }
            for o in order_by {
                collect_column_refs(&o.expr, out);
            }
        }
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
        FromItem::Values { rows, .. } => {
            for row in rows {
                for e in row {
                    collect_column_refs(e, out);
                }
            }
        }
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
    // v0.8: when the whole query is a plain single-table SELECT whose
    // ORDER BY matches an index, the table streams out of the index in
    // ORDER BY order and run_select skips the sort. Only ever set on the
    // fast path (single FROM item).
    order_hint: Option<&OrderHint>,
    // v0.8: row budget for early termination of the index-order scan.
    early_limit: Option<usize>,
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    match item {
        FromItem::Table { name, alias } => {
            let qual = alias.clone().unwrap_or_else(|| name.clone());
            // v0.10: CTEs shadow everything (like Postgres).
            if let Some(b) = q.ctes.iter().rev().find(|b| b.name == *name) {
                let schema: Vec<QCol> = b
                    .schema
                    .iter()
                    .map(|c| QCol {
                        qual: qual.clone(),
                        name: c.name.clone(),
                        ty: c.ty.clone(),
                    })
                    .collect();
                return Ok((schema, b.rows.clone()));
            }
            // v0.9: information_schema virtual tables.
            if name == "information_schema.tables" {
                let (schema, rows) = info_tables_scan(&q.eng.db, q.snap, q.own);
                let schema: Vec<QCol> = schema
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                return Ok((schema, rows));
            }
            if name == "information_schema.columns" {
                let (schema, rows) = info_columns_scan(&q.eng.db, q.snap, q.own);
                let schema: Vec<QCol> = schema
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                return Ok((schema, rows));
            }
            // v0.9: a view name expands to its stored SELECT. Views are
            // checked before tables (a table would have blocked CREATE VIEW).
            if let Some(view) = q.eng.db.find_view(name, q.snap, q.own).cloned() {
                let stmt = parse_statement(&view.query).map_err(sql_err)?;
                let select = match stmt {
                    Stmt::Select(s) => s,
                    _ => {
                        return Err(exec_err(
                            "0A000",
                            format!("view \"{}\" query is not a SELECT", name),
                        ));
                    }
                };
                // Guard against runaway recursion (e.g. a view recreated
                // over itself via OR REPLACE races).
                if q.depth > 16 {
                    return Err(exec_err(
                        "54001",
                        "view recursion limit exceeded".to_string(),
                    ));
                }
                q.depth += 1;
                let out = run_select(q, &select, outer);
                q.depth -= 1;
                let out = out?;
                let schema: Vec<QCol> = out
                    .columns
                    .into_iter()
                    .enumerate()
                    .map(|(i, (cn, ty))| QCol {
                        qual: qual.clone(),
                        name: view.col_aliases.get(i).cloned().unwrap_or(cn),
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
                return Ok((schema, rows));
            }
            // v0.8: the pg_stats system catalog is virtual — a real table
            // by that name takes precedence.
            if name == "pg_stats" && q.eng.db.find_table(name, q.snap, q.own).is_none() {
                let schema: Vec<QCol> = pg_stats_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                let (_, rows) = pg_stats_scan(&q.eng.db);
                return Ok((schema, rows));
            }
            // v0.11: the role catalogs are virtual too.
            if matches!(
                name.as_str(),
                "pg_authid" | "pg_roles" | "pg_user" | "pg_auth_members"
            ) && q.eng.db.find_table(name, q.snap, q.own).is_none()
            {
                let (schema, rows) = pg_auth_scan(&q.eng.db, q.snap, q.own, q.role, name);
                let schema: Vec<QCol> = schema
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                return Ok((schema, rows));
            }
            // v0.13: pg_replication_slots is virtual too (cluster-global
            // slot map, not a table).
            if name.as_str() == "pg_replication_slots"
                && q.eng.db.find_table(name, q.snap, q.own).is_none()
            {
                let schema: Vec<QCol> = pg_replication_slots_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                let rows = pg_replication_slots_rows(q.eng);
                return Ok((schema, rows));
            }
            // v0.11: scanning a real table needs SELECT (and UPDATE when
            // the statement is FOR UPDATE, like PostgreSQL). Existence is
            // still reported as 42P01 when the table is missing.
            {
                let (eng, snap, own, role) = (&q.eng, q.snap, q.own, q.role);
                if let Some(t) = eng.db.find_table(name, snap, own) {
                    let have = crate::storage::table_privs(&eng.db, role, t, snap, own);
                    // v0.11: table-level SELECT, or any column-level
                    // SELECT grant — the run_select pre-pass enforces
                    // the precise per-column rule.
                    let select_ok = have & crate::storage::PRIV_SELECT
                        == crate::storage::PRIV_SELECT
                        || crate::storage::has_col_priv(
                            &eng.db,
                            role,
                            t,
                            crate::storage::PRIV_SELECT,
                            snap,
                            own,
                        );
                    let update_ok = !need_prov
                        || have & crate::storage::PRIV_UPDATE == crate::storage::PRIV_UPDATE;
                    if !select_ok || !update_ok {
                        return Err(exec_err(
                            "42501",
                            format!(
                                "permission denied for table \"{}\" (needs {})",
                                name,
                                if need_prov {
                                    "SELECT, UPDATE"
                                } else {
                                    "SELECT"
                                },
                            ),
                        ));
                    }
                }
            }
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
                // v0.8: the planner may replace the sequential scan with
                // an index scan (WHERE bounds on a leading index prefix)
                // or an index-order scan (ORDER BY). Either way the full
                // residual predicate and MVCC visibility are still
                // applied afterwards, so a plan only ever costs speed.
                let db = &q.eng.db;
                let snap = q.snap;
                let own = q.own;
                let rows: Vec<QRow> = if let Some(hint) = order_hint {
                    let ix = db
                        .indexes
                        .get(&hint.index)
                        .expect("planned index still present; engine lock held throughout");
                    index_order_rows(ix, t, name, hint.desc, snap, own, need_prov, early_limit)
                } else {
                    match plan_access_path(db, t, name, &qual, where_, snap, own) {
                        AccessPath::SeqScan => t
                            .rows
                            .iter()
                            .filter(|r| row_visible(r, snap, own))
                            .map(|r| QRow {
                                cells: r.values.clone(),
                                prov: if need_prov {
                                    vec![(name.clone(), r.id)]
                                } else {
                                    Vec::new()
                                },
                            })
                            .collect(),
                        AccessPath::IndexScan {
                            index,
                            prefix,
                            lo,
                            hi,
                            ..
                        } => {
                            let ix = db
                                .indexes
                                .get(&index)
                                .expect("planned index still present; engine lock held throughout");
                            let ids = index_scan_ids(
                                ix,
                                &prefix,
                                lo.as_ref().map(|(v, b)| (v, *b)),
                                hi.as_ref().map(|(v, b)| (v, *b)),
                            );
                            let mut rows = Vec::with_capacity(ids.len());
                            for id in ids {
                                if let Some(pos) = t.row_pos(id) {
                                    let r = &t.rows[pos];
                                    if row_visible(r, snap, own) {
                                        rows.push(QRow {
                                            cells: r.values.clone(),
                                            prov: if need_prov {
                                                vec![(name.clone(), r.id)]
                                            } else {
                                                Vec::new()
                                            },
                                        });
                                    }
                                }
                            }
                            rows
                        }
                    }
                };
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
                    session: q.session,
                    role: q.role,
                    read_only: q.read_only,
                    depth: q.depth + 1,
                    lock_ids: &mut *q.lock_ids,
                    ctes: q.ctes.clone(),
                    wctx: None,
                    priv_scopes: q.priv_scopes.clone(),
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
        // v0.14: `(VALUES (e, ...) [, ...]) [AS] alias`. VALUES is
        // uncorrelated, so every row evaluates with no scopes. Columns
        // are named `column1`, `column2`, ... like PostgreSQL.
        FromItem::Values { rows, alias } => {
            let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
            let mut eval_rows: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
            for row in rows {
                if row.len() != ncols {
                    return Err(exec_err(
                        "42601",
                        format!(
                            "VALUES lists must all be the same length ({} vs {})",
                            ncols,
                            row.len()
                        ),
                    ));
                }
                let mut cells = Vec::with_capacity(ncols);
                for e in row {
                    cells.push(eval_expr(q, &[], e)?);
                }
                eval_rows.push(cells);
            }
            let schema: Vec<QCol> = (0..ncols)
                .map(|i| {
                    let ty = eval_rows
                        .iter()
                        .map(|r| &r[i])
                        .find(|v| !matches!(v, Value::Null))
                        .map(value_coltype)
                        .unwrap_or(ColType::Text);
                    QCol {
                        qual: alias.clone(),
                        name: format!("column{}", i + 1),
                        ty,
                    }
                })
                .collect();
            let rows = eval_rows
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
            let (lschema, lrows) = build_source(q, outer, left, where_, need_prov, None, None)?;
            let (rschema, rrows) = build_source(q, outer, right, where_, need_prov, None, None)?;
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
/// Hash key for GROUP BY / DISTINCT. Exact numerics (int2/int4/int8/
/// numeric) canonicalize to a (tag, unscaled, scale) triple and floats to
/// f64 bits, so `1::int`, `1::bigint` and `1.0::numeric` group together —
/// like Postgres' common-type resolution for grouping.
fn value_key(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0),
        Value::SmallInt(i) => {
            out.push(8);
            out.extend_from_slice(&(*i as i128).to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
        }
        Value::Int(i) => {
            out.push(8);
            out.extend_from_slice(&(*i as i128).to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
        }
        Value::BigInt(i) => {
            out.push(8);
            out.extend_from_slice(&(*i as i128).to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
        }
        Value::Numeric(n) => {
            out.push(8);
            out.extend_from_slice(&n.unscaled.to_be_bytes());
            out.extend_from_slice(&n.scale.to_be_bytes());
        }
        Value::Float4(f) => {
            out.push(2);
            out.extend_from_slice(&(*f as f64).to_bits().to_be_bytes());
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
        Value::Date(d) => {
            out.push(9);
            out.extend_from_slice(&d.to_be_bytes());
        }
        Value::Timestamp(m) => {
            out.push(10);
            out.extend_from_slice(&m.to_be_bytes());
        }
        Value::Timestamptz(m) => {
            out.push(11);
            out.extend_from_slice(&m.to_be_bytes());
        }
        Value::Bytea(b) => {
            out.push(12);
            out.extend_from_slice(&(b.len() as u64).to_be_bytes());
            out.extend_from_slice(b);
        }
        Value::Uuid(u) => {
            out.push(13);
            out.extend_from_slice(u);
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
    // v0.10: deduplicated window specs for this query level.
    windows: &[ExecWindow],
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
    // v0.10: HAVING is evaluated BEFORE windows (Postgres: windows see
    // only groups surviving HAVING). Collect surviving group indices.
    let mut surviving = Vec::with_capacity(groups.len());
    for (gi, (key_vals, idxs)) in groups.iter().enumerate() {
        let first: &[Value] = match idxs.first() {
            Some(&i) => &rows[i].cells,
            None => &[],
        };
        let gscope = Scope { schema, row: first };
        let keep = match &stmt.having {
            None => true,
            Some(h) => eval_grouped_bool(
                q,
                outer,
                gscope,
                schema,
                rows,
                idxs,
                key_vals,
                &stmt.group_by,
                h,
            )?,
        };
        if keep {
            surviving.push(gi);
        }
    }
    // v0.10: precompute window values over the surviving groups (one
    // input row per group).
    if !windows.is_empty() {
        let inputs = gather_window_inputs_grouped(
            q,
            outer,
            schema,
            rows,
            &groups,
            &surviving,
            &stmt.group_by,
            windows,
        )?;
        install_windows(q, windows, &inputs)?;
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
    // v0.10: iterate surviving groups only (HAVING already applied).
    // `wctx.row` is the position in the surviving list, matching the
    // window input order.
    for (fgi, &gi) in surviving.iter().enumerate() {
        let (key_vals, idxs) = &groups[gi];
        // v0.10: point the window context at this group.
        if let Some(wctx) = q.wctx.as_mut() {
            wctx.row = fgi;
        }
        // Correlated subqueries inside the select list see the first row
        // of the group. Any correlated column must be group-bound for the
        // query to be valid, so every row in the group agrees on it.
        let first: &[Value] = match idxs.first() {
            Some(&i) => &rows[i].cells,
            None => &[],
        };
        let gscope = Scope { schema, row: first };
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
            // v0.10: ORDER BY terms with windows need the group index.
            win_idx: if windows.is_empty() { None } else { Some(gi) },
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
        Expr::Agg {
            func,
            arg,
            distinct,
            arg2,
        } => eval_agg_func(
            q,
            outer,
            schema,
            rows,
            idxs,
            *func,
            arg.as_deref(),
            *distinct,
            arg2.as_deref(),
        ),
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
        Expr::Arith { op, left, right } => {
            let va = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, left,
            )?;
            let vb = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, right,
            )?;
            eval_arith(*op, &va, &vb)
        }
        Expr::Cast { expr, to } => {
            let v = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            eval_cast(&v, *to)
        }
        Expr::Concat(a, b) => {
            let va = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, a)?;
            let vb = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, b)?;
            eval_concat(&va, &vb)
        }
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
        } => {
            let va = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            let vb = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, pattern,
            )?;
            eval_like(&va, &vb, *not, *ilike)
        }
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            let v = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            let lo = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, low,
            )?;
            let hi = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, high,
            )?;
            eval_between(&v, &lo, &hi, *neg)
        }
        Expr::IsBool { expr, neg, val } => {
            let v = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            eval_is_bool(&v, *neg, *val)
        }
        Expr::Func { name, args } => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, a,
                )?);
            }
            // v0.9: sequence functions need engine access; they cannot
            // go through the pure eval_func_vals path.
            if matches!(name.as_str(), "nextval" | "currval" | "setval") {
                check_builtin_arity(name, &vals)?;
                let (eng, snap, own, session) = (&mut *q.eng, &*q.snap, q.own, q.session);
                return eval_sequence_func(
                    eng,
                    snap,
                    own,
                    session,
                    q.role,
                    q.read_only,
                    name,
                    &vals,
                );
            }
            // Grouped context: no correlated subqueries inside function
            // args here (subqueries take the eval_expr path); dispatch on
            // pre-evaluated values.
            eval_func_vals(name, &vals)
        }
        Expr::Extract { field, from } => {
            let v = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, from,
            )?;
            eval_extract(field, &v)
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
        // v0.10: a top-level window in the select list (or ORDER BY
        // fallback) reads its precomputed value from the query's
        // window context. Nested windows never reach here: validation
        // rejects them, and window inputs are gathered separately.
        Expr::Window { wid, .. } => {
            let wctx = q
                .wctx
                .as_ref()
                .ok_or_else(|| exec_err("XX000", "internal error: window without context"))?;
            wctx.values
                .get(*wid)
                .and_then(|v| v.get(wctx.row))
                .cloned()
                .ok_or_else(|| exec_err("XX000", "internal error: window value missing"))
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
/// v0.10: `sum()` over non-null values (shared by grouped aggregates
/// and windowed aggregates).
fn sum_vals(vals: &[Value]) -> Result<Value, ExecError> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    // v0.6 rule kept: sum returns the widest input kind among
    // the rows (documented deviation: Postgres widens int->bigint).
    let mut cat = NumCat::Small;
    for v in vals {
        match num_cat(v) {
            Some(c) => cat = cat.max(c),
            None => {
                return Err(exec_err(
                    "42883",
                    format!("function sum({}) does not exist", v.type_name()),
                ));
            }
        }
    }
    if cat == NumCat::Numeric {
        let mut acc = Numeric::zero();
        for v in vals {
            let n = to_numeric_opt(v)
                .ok_or_else(|| exec_err("22003", "value out of range for numeric"))?;
            acc = acc
                .checked_add(&n)
                .ok_or_else(|| exec_err("22003", "numeric field overflow"))?;
        }
        Ok(Value::Numeric(acc))
    } else if cat >= NumCat::Real {
        let mut acc = 0.0;
        for v in vals {
            acc += to_f64v(v);
        }
        Ok(if cat == NumCat::Real {
            Value::Float4(acc as f32)
        } else {
            Value::Float(acc)
        })
    } else {
        let mut acc: i128 = 0;
        for v in vals {
            acc = acc
                .checked_add(to_i128(v))
                .ok_or_else(|| exec_err("22003", "integer out of range"))?;
        }
        let icat = if cat == NumCat::Small {
            NumCat::Int
        } else {
            cat
        };
        fit_int_result(icat, acc)
    }
}

/// v0.10: `avg()` over non-null values (shared by grouped aggregates
/// and windowed aggregates).
fn avg_vals(vals: &[Value]) -> Result<Value, ExecError> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    // v0.6 rule kept: avg always returns double precision.
    let mut sum = 0.0;
    for v in vals {
        match num_cat(v) {
            Some(_) => sum += to_f64v(v),
            None => {
                return Err(exec_err(
                    "42883",
                    format!("function avg({}) does not exist", v.type_name()),
                ));
            }
        }
    }
    Ok(Value::Float(sum / vals.len() as f64))
}

fn eval_agg_func(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: &[QRow],
    idxs: &[usize],
    func: AggFunc,
    arg: Option<&Expr>,
    distinct: bool,
    arg2: Option<&Expr>,
) -> Result<Value, ExecError> {
    if func == AggFunc::Count && arg.is_none() {
        return Ok(Value::BigInt(idxs.len() as i64));
    }
    let a = arg.expect("non-COUNT aggregates take an argument");
    let mut vals: Vec<Value> = Vec::new();
    // string_agg evaluates (value, delimiter) per row; the delimiter
    // may be NULL per-row (then it defaults to "") while a NULL value
    // still skips the row, like Postgres.
    let mut delims: Vec<Value> = Vec::new();
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
        if func == AggFunc::StringAgg {
            let d = eval_expr(q, &scopes, arg2.expect("string_agg takes a delimiter"))?;
            if v == Value::Null {
                continue;
            }
            vals.push(v);
            delims.push(d);
        } else if v != Value::Null {
            vals.push(v);
        }
    }
    // DISTINCT: dedupe on the canonical grouping key (NULLs already
    // removed above, so this matches Postgres' "DISTINCT treats NULLs
    // as equal" trivially). For string_agg, dedupe on the value alone,
    // like Postgres deduplicates the input rows.
    if distinct {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut kept: Vec<usize> = Vec::new();
        for (i, v) in vals.iter().enumerate() {
            let mut k = Vec::new();
            value_key(v, &mut k);
            if !seen.contains(&k) {
                seen.push(k);
                kept.push(i);
            }
        }
        let new_vals: Vec<Value> = kept.iter().map(|&i| vals[i].clone()).collect();
        vals = new_vals;
        // delims is only populated for string_agg; other aggregates
        // have no second argument to deduplicate.
        if !delims.is_empty() {
            let new_delims: Vec<Value> = kept.iter().map(|&i| delims[i].clone()).collect();
            delims = new_delims;
        }
    }
    match func {
        AggFunc::Count => Ok(Value::BigInt(vals.len() as i64)),
        AggFunc::Sum => sum_vals(&vals),
        AggFunc::Avg => avg_vals(&vals),
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
        AggFunc::StringAgg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut out = String::new();
            for (i, v) in vals.iter().enumerate() {
                let s = match v {
                    Value::Text(t) => t.clone(),
                    other => {
                        return Err(exec_err(
                            "42883",
                            format!("function string_agg({}) does not exist", other.type_name()),
                        ));
                    }
                };
                if i > 0 {
                    // NULL delimiter = no separator (Postgres rule).
                    match &delims[i] {
                        Value::Null => {}
                        Value::Text(d) => out.push_str(d),
                        other => {
                            return Err(exec_err(
                                "42883",
                                format!(
                                    "function string_agg(text, {}) does not exist",
                                    other.type_name()
                                ),
                            ));
                        }
                    }
                }
                out.push_str(&s);
            }
            Ok(Value::Text(out))
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
                    // v0.10: ORDER BY terms containing windows evaluate
                    // against the precomputed window values; point the
                    // context at this row's pre-projection index.
                    if contains_window(e) {
                        if let Some(wi) = o.win_idx {
                            if let Some(wctx) = q.wctx.as_mut() {
                                wctx.row = wi;
                            }
                        }
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
            match compare_values(&keys[a][i], &keys[b][i], term.desc, term.nulls_first) {
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
            win_idx: orows[i].win_idx,
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
        Expr::Arith { op, left, right } => {
            let va = eval_expr(q, scopes, left)?;
            let vb = eval_expr(q, scopes, right)?;
            eval_arith(*op, &va, &vb)
        }
        Expr::Cast { expr, to } => {
            let v = eval_expr(q, scopes, expr)?;
            eval_cast(&v, *to)
        }
        Expr::Concat(a, b) => {
            let va = eval_expr(q, scopes, a)?;
            let vb = eval_expr(q, scopes, b)?;
            eval_concat(&va, &vb)
        }
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
        } => {
            let va = eval_expr(q, scopes, expr)?;
            let vb = eval_expr(q, scopes, pattern)?;
            eval_like(&va, &vb, *not, *ilike)
        }
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            let v = eval_expr(q, scopes, expr)?;
            let lo = eval_expr(q, scopes, low)?;
            let hi = eval_expr(q, scopes, high)?;
            eval_between(&v, &lo, &hi, *neg)
        }
        Expr::IsBool { expr, neg, val } => {
            let v = eval_expr(q, scopes, expr)?;
            eval_is_bool(&v, *neg, *val)
        }
        Expr::Func { name, args } => eval_func(q, scopes, name, args),
        Expr::Extract { field, from } => {
            let v = eval_expr(q, scopes, from)?;
            eval_extract(field, &v)
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
                    session: q.session,
                    role: q.role,
                    read_only: q.read_only,
                    depth: q.depth + 1,
                    lock_ids: &mut *q.lock_ids,
                    ctes: q.ctes.clone(),
                    wctx: None,
                    priv_scopes: q.priv_scopes.clone(),
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
                    session: q.session,
                    role: q.role,
                    read_only: q.read_only,
                    depth: q.depth + 1,
                    lock_ids: &mut *q.lock_ids,
                    ctes: q.ctes.clone(),
                    wctx: None,
                    priv_scopes: q.priv_scopes.clone(),
                };
                run_select(&mut sub_q, sub, scopes)?
            };
            let exists = !out.rows.is_empty();
            Ok(Value::Bool(if *neg { !exists } else { exists }))
        }
        // v0.10: window functions are precomputed per query level
        // (Q.wctx) before projection; here we just look the value up.
        Expr::Window { wid, .. } => {
            let wctx = q
                .wctx
                .as_ref()
                .ok_or_else(|| exec_err("XX000", "internal error: window without context"))?;
            wctx.values
                .get(*wid)
                .and_then(|v| v.get(wctx.row))
                .cloned()
                .ok_or_else(|| exec_err("XX000", "internal error: window value missing"))
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
            session: q.session,
            role: q.role,
            read_only: q.read_only,
            depth: q.depth + 1,
            lock_ids: &mut *q.lock_ids,
            ctes: q.ctes.clone(),
            wctx: None,
            priv_scopes: q.priv_scopes.clone(),
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
/// Exact numerics (int2/int4/int8/numeric) compare exactly; float
/// kinds compare by total_cmp; mixed exact/float goes through f64 (a
/// documented precision caveat). Text is byte-wise (no collations
/// yet), bools false < true, dates/times/bytea/uuid compare naturally.
/// Mismatched non-null types are 42883, like Postgres.
/// Date as a timestamp (midnight) for mixed date/timestamp comparisons.
fn date_as_ts(d: i32) -> i64 {
    d as i64 * 86_400_000_000
}

fn cmp_ordering(a: &Value, b: &Value, op: CmpOp) -> Result<Option<Ordering>, ExecError> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Ok(None),
        (x, y) if is_exact_numeric(x) && is_exact_numeric(y) => {
            Ok(Some(exact_numeric(x).cmp(&exact_numeric(y))))
        }
        (Value::Float4(x), Value::Float4(y)) => Ok(Some((*x as f64).total_cmp(&(*y as f64)))),
        (Value::Float4(x), Value::Float(y)) => Ok(Some((*x as f64).total_cmp(y))),
        (Value::Float(x), Value::Float4(y)) => Ok(Some(x.total_cmp(&(*y as f64)))),
        (Value::Float(x), Value::Float(y)) => Ok(Some(x.total_cmp(y))),
        (x, y @ (Value::Float4(_) | Value::Float(_))) if is_exact_numeric(x) => {
            Ok(Some(exact_to_f64(x).total_cmp(&float_val(y))))
        }
        (x @ (Value::Float4(_) | Value::Float(_)), y) if is_exact_numeric(y) => {
            Ok(Some(float_val(x).total_cmp(&exact_to_f64(y))))
        }
        (Value::Text(x), Value::Text(y)) => Ok(Some(x.cmp(y))),
        (Value::Bool(x), Value::Bool(y)) => Ok(Some(x.cmp(y))),
        (Value::Date(x), Value::Date(y)) => Ok(Some(x.cmp(y))),
        (Value::Timestamp(x), Value::Timestamp(y)) => Ok(Some(x.cmp(y))),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => Ok(Some(x.cmp(y))),
        // Postgres casts date up to timestamp/timestamptz for mixed
        // comparisons (date is midnight).
        (Value::Date(d), Value::Timestamp(t)) => Ok(Some(date_as_ts(*d).cmp(t))),
        (Value::Timestamp(t), Value::Date(d)) => Ok(Some(t.cmp(&date_as_ts(*d)))),
        (Value::Date(d), Value::Timestamptz(t)) => Ok(Some(date_as_ts(*d).cmp(t))),
        (Value::Timestamptz(t), Value::Date(d)) => Ok(Some(t.cmp(&date_as_ts(*d)))),
        // Postgres casts timestamp up to timestamptz (session zone; v0.7
        // is UTC-only so this is exact).
        (Value::Timestamp(t), Value::Timestamptz(z)) => Ok(Some(t.cmp(z))),
        (Value::Timestamptz(z), Value::Timestamp(t)) => Ok(Some(z.cmp(t))),
        (Value::Bytea(x), Value::Bytea(y)) => Ok(Some(x.cmp(y))),
        (Value::Uuid(x), Value::Uuid(y)) => Ok(Some(x.cmp(y))),
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
    session: u64,
    role: &str,
    schema: &[QCol],
    values: &[Value],
    e: &Expr,
    ctes: &[Rc<CteBinding>],
) -> Result<Value, ExecError> {
    eval_dml_expr(eng, snap, own, session, role, &[(schema, values)], e, ctes)
}

/// v0.10: evaluate a DML expression (UPDATE SET, RETURNING, ON CONFLICT
/// DO UPDATE) against one or more explicit frames. Frames are ordered
/// outermost-first: unqualified column references resolve to the LAST
/// frame (like nested scopes), so callers put the target table last.
fn eval_dml_expr(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    frames: &[(&[QCol], &[Value])],
    e: &Expr,
    ctes: &[Rc<CteBinding>],
) -> Result<Value, ExecError> {
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap,
        own,
        session,
        role,
        // v0.17: DML-only helper — INSERT/UPDATE/DELETE are
        // statement-blocked when read-only, so this is false.
        read_only: false,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: ctes.to_vec(),
        wctx: None,
        priv_scopes: Vec::new(),
    };
    let scopes: Vec<Scope> = frames
        .iter()
        .map(|(schema, row)| Scope { schema, row })
        .collect();
    let v = eval_expr(&mut q, &scopes, e)?;
    // FOR UPDATE inside an UPDATE's SET subquery locks nothing: UPDATE is
    // not SELECT, so there is no statement-level lock flow to hand the
    // collected ids to (documented).
    Ok(v)
}

// ---------------------------------------------------------------------------
// v0.2: arithmetic (kept)
// ---------------------------------------------------------------------------

/// A value's type for `+` resolution; NULL contributes no constraint.
// ---------------------------------------------------------------------------
// v0.7 arithmetic, casts, operators, and built-in functions
// ---------------------------------------------------------------------------

/// Numeric category for promotion. Declaration order IS the promotion
/// lattice: smallint < integer < bigint < real < double precision <
/// numeric. (Postgres resolves real+int to double, and
/// anything+numeric to numeric; int2+int2 resolves to int4.)
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NumCat {
    Small,
    Int,
    Big,
    Real,
    Double,
    Numeric,
}

fn num_cat(v: &Value) -> Option<NumCat> {
    match v {
        Value::SmallInt(_) => Some(NumCat::Small),
        Value::Int(_) => Some(NumCat::Int),
        Value::BigInt(_) => Some(NumCat::Big),
        Value::Float4(_) => Some(NumCat::Real),
        Value::Float(_) => Some(NumCat::Double),
        Value::Numeric(_) => Some(NumCat::Numeric),
        _ => None,
    }
}

fn op_err(op: ArithOp, a: &Value, b: &Value) -> ExecError {
    exec_err(
        "42883",
        format!(
            "operator does not exist: {} {} {}",
            a.type_name(),
            op.sql(),
            b.type_name()
        ),
    )
}

/// Exact numeric -> Numeric. Floats that don't fit (NaN/Inf) -> None.
fn to_numeric_opt(v: &Value) -> Option<Numeric> {
    match v {
        Value::SmallInt(i) => Some(Numeric::new(*i as i128, 0)),
        Value::Int(i) => Some(Numeric::new(*i as i128, 0)),
        Value::BigInt(i) => Some(Numeric::new(*i as i128, 0)),
        Value::Numeric(n) => Some(n.clone()),
        Value::Float4(f) => Numeric::from_f64(*f as f64).ok(),
        Value::Float(f) => Numeric::from_f64(*f).ok(),
        _ => None,
    }
}

fn to_i128(v: &Value) -> i128 {
    match v {
        Value::SmallInt(i) => *i as i128,
        Value::Int(i) => *i as i128,
        Value::BigInt(i) => *i as i128,
        _ => 0,
    }
}

fn to_f64v(v: &Value) -> f64 {
    match v {
        Value::SmallInt(i) => *i as f64,
        Value::Int(i) => *i as f64,
        Value::BigInt(i) => *i as f64,
        Value::Numeric(n) => n.to_f64(),
        Value::Float4(f) => *f as f64,
        Value::Float(f) => *f,
        _ => f64::NAN,
    }
}

/// `+ - * / %` with Postgres-ish numeric promotion. NULL propagates.
/// Date arithmetic is handled first: date +/- integer-kind -> date,
/// date - date -> integer days. No intervals in v0.7, so timestamp
/// arithmetic (other than comparisons) is 42883.
fn eval_arith(op: ArithOp, a: &Value, b: &Value) -> Result<Value, ExecError> {
    if a == &Value::Null || b == &Value::Null {
        return Ok(Value::Null);
    }
    if let Some(v) = eval_datetime_arith(op, a, b)? {
        return Ok(v);
    }
    if op == ArithOp::Pow {
        // Postgres `^`: exact numeric power for integer exponents,
        // float8 when either side is floating.
        return eval_power_op(a, b, |w| op_err(op, a, w));
    }
    let (ca, cb) = match (num_cat(a), num_cat(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(op_err(op, a, b)),
    };
    let cat = ca.max(cb);
    if op == ArithOp::Mod && matches!(cat, NumCat::Real | NumCat::Double) {
        // Postgres defines % only for the exact numeric types
        // (smallint/int/bigint/numeric), not for real/double.
        return Err(op_err(op, a, b));
    }
    // Postgres resolves smallint <op> smallint to integer.
    let icat = if ca == NumCat::Small && cb == NumCat::Small {
        NumCat::Int
    } else {
        cat
    };
    match cat {
        NumCat::Numeric => {
            let x = to_numeric_opt(a)
                .ok_or_else(|| exec_err("22003", "value out of range for numeric"))?;
            let y = to_numeric_opt(b)
                .ok_or_else(|| exec_err("22003", "value out of range for numeric"))?;
            if matches!(op, ArithOp::Div | ArithOp::Mod) && y.is_zero() {
                return Err(exec_err("22012", "division by zero"));
            }
            let r = match op {
                ArithOp::Add => x.checked_add(&y),
                ArithOp::Sub => x.checked_sub(&y),
                ArithOp::Mul => x.checked_mul(&y),
                ArithOp::Div => x.checked_div(&y),
                ArithOp::Mod => x.checked_rem(&y),
                ArithOp::Pow => unreachable!("^ is handled before the category dispatch"),
            };
            r.map(Value::Numeric)
                .ok_or_else(|| exec_err("22003", "numeric field overflow"))
        }
        NumCat::Double | NumCat::Real => {
            let x = to_f64v(a);
            let y = to_f64v(b);
            if op == ArithOp::Div && y == 0.0 {
                // Postgres raises 22012 even for float division.
                return Err(exec_err("22012", "division by zero"));
            }
            let r = match op {
                ArithOp::Add => x + y,
                ArithOp::Sub => x - y,
                ArithOp::Mul => x * y,
                ArithOp::Div => x / y,
                ArithOp::Mod => unreachable!("rejected above"),
                ArithOp::Pow => unreachable!("^ is handled before the category dispatch"),
            };
            Ok(if cat == NumCat::Real {
                Value::Float4(r as f32)
            } else {
                Value::Float(r)
            })
        }
        _ => {
            let x = to_i128(a);
            let y = to_i128(b);
            if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0 {
                return Err(exec_err("22012", "division by zero"));
            }
            let r = match op {
                ArithOp::Add => x.checked_add(y),
                ArithOp::Sub => x.checked_sub(y),
                ArithOp::Mul => x.checked_mul(y),
                // i128::MIN / -1 is the only overflow here.
                ArithOp::Div => x.checked_div(y),
                ArithOp::Mod => x.checked_rem(y),
                ArithOp::Pow => unreachable!("^ is handled before the category dispatch"),
            };
            let r = r.ok_or_else(|| exec_err("22003", "integer out of range"))?;
            fit_int_result(icat, r)
        }
    }
}

/// Fit an i128 arithmetic result into the resolved integer kind.
fn fit_int_result(icat: NumCat, r: i128) -> Result<Value, ExecError> {
    let ovf = || exec_err("22003", "integer out of range");
    match icat {
        NumCat::Small => Ok(Value::SmallInt(
            i16::try_from(r).map_err(|_| exec_err("22003", "smallint out of range"))?,
        )),
        NumCat::Int => Ok(Value::Int(i32::try_from(r).map_err(|_| ovf())? as i64)),
        _ => Ok(Value::BigInt(i64::try_from(r).map_err(|_| ovf())?)),
    }
}

/// Date arithmetic. Ok(None) = not date/time operands (caller falls
/// through to numeric handling).
fn eval_datetime_arith(op: ArithOp, a: &Value, b: &Value) -> Result<Option<Value>, ExecError> {
    fn int_days(v: &Value) -> Option<i64> {
        match v {
            Value::SmallInt(i) => Some(*i as i64),
            Value::Int(i) => Some(*i),
            Value::BigInt(i) => Some(*i),
            _ => None,
        }
    }
    let is_dt = |v: &Value| {
        matches!(
            v,
            Value::Date(_) | Value::Timestamp(_) | Value::Timestamptz(_)
        )
    };
    if !is_dt(a) && !is_dt(b) {
        return Ok(None);
    }
    // date +/- integer-kind -> date
    if let Value::Date(d) = a {
        if let Some(days) = int_days(b) {
            match op {
                ArithOp::Add | ArithOp::Sub => {
                    let delta = if op == ArithOp::Add { days } else { -days };
                    let r = (*d as i64)
                        .checked_add(delta)
                        .and_then(|r| i32::try_from(r).ok())
                        .ok_or_else(|| exec_err("22008", "datetime field overflow"))?;
                    return Ok(Some(Value::Date(r)));
                }
                _ => return Err(op_err(op, a, b)),
            }
        }
    }
    // integer + date -> date (commutative with date + integer; only
    // addition commutes — `int - date` is undefined, like Postgres).
    if let Value::Date(d) = b {
        if let Some(days) = int_days(a) {
            if op == ArithOp::Add {
                let r = (*d as i64)
                    .checked_add(days)
                    .and_then(|r| i32::try_from(r).ok())
                    .ok_or_else(|| exec_err("22008", "datetime field overflow"))?;
                return Ok(Some(Value::Date(r)));
            }
            return Err(op_err(op, a, b));
        }
    }
    // date - date -> integer days
    if let (Value::Date(d1), Value::Date(d2)) = (a, b) {
        return match op {
            ArithOp::Sub => Ok(Some(Value::Int(*d1 as i64 - *d2 as i64))),
            _ => Err(op_err(op, a, b)),
        };
    }
    // timestamp - timestamp would be an interval; v0.7 has none.
    if matches!(
        (a, b),
        (Value::Timestamp(_), Value::Timestamp(_)) | (Value::Timestamptz(_), Value::Timestamptz(_))
    ) {
        return Err(exec_err(
            "42883",
            "operator does not exist: timestamp - timestamp (intervals are not supported in v0.7)",
        ));
    }
    Err(op_err(op, a, b))
}

// ---------------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------------

fn cast_err(from: &Value, to: &str) -> ExecError {
    exec_err(
        "42846",
        format!("cannot cast type {} to {}", from.type_name(), to),
    )
}

/// Cast a value to an integer kind. Floats and numerics round half away
/// from zero (like Postgres); text must be plain integer syntax.
fn cast_to_int(v: &Value) -> Result<i128, ExecError> {
    match v {
        Value::SmallInt(i) => Ok(*i as i128),
        Value::Int(i) => Ok(*i as i128),
        Value::BigInt(i) => Ok(*i as i128),
        Value::Numeric(n) => {
            let r = n
                .to_i64()
                .ok_or_else(|| exec_err("22003", "numeric out of range"))?;
            Ok(r as i128)
        }
        Value::Float4(f) => float_to_int(*f as f64),
        Value::Float(f) => float_to_int(*f),
        Value::Text(s) => parse_int_text(s),
        Value::Bool(b) => Ok(*b as i128),
        other => Err(cast_err(other, "integer")),
    }
}

fn float_to_int(f: f64) -> Result<i128, ExecError> {
    if !f.is_finite() {
        return Err(exec_err("22003", "integer out of range"));
    }
    // Half away from zero.
    let r = (f.abs() + 0.5).floor().copysign(f);
    if r < i128::MIN as f64 || r > i128::MAX as f64 {
        return Err(exec_err("22003", "integer out of range"));
    }
    Ok(r as i128)
}

/// Postgres integer input: optional sign, digits, surrounding
/// whitespace. Anything else (decimal points, exponents) is 22P02.
fn parse_int_text(s: &str) -> Result<i128, ExecError> {
    let t = s.trim();
    let digits = t.strip_prefix('+').unwrap_or(t);
    let digits = digits.strip_prefix('-').map(|d| d).unwrap_or(digits);
    let neg = t.starts_with('-');
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(exec_err(
            "22P02",
            format!("invalid input syntax for type integer: {:?}", s),
        ));
    }
    let mut r: i128 = 0;
    for c in digits.bytes() {
        r = r
            .checked_mul(10)
            .and_then(|r| r.checked_add((c - b'0') as i128))
            .ok_or_else(|| exec_err("22003", "value overflows integer"))?;
    }
    if neg {
        r = -r;
    }
    Ok(r)
}

fn cast_to_f64(v: &Value) -> Result<f64, ExecError> {
    match v {
        Value::SmallInt(i) => Ok(*i as f64),
        Value::Int(i) => Ok(*i as f64),
        Value::BigInt(i) => Ok(*i as f64),
        Value::Numeric(n) => Ok(n.to_f64()),
        Value::Float4(f) => Ok(*f as f64),
        Value::Float(f) => Ok(*f),
        Value::Bool(b) => Ok(*b as i32 as f64),
        Value::Text(s) => {
            let t = s.trim();
            match t.parse::<f64>() {
                Ok(f) => {
                    // Rust parses "1e999" as inf; Postgres rejects it.
                    if f.is_infinite()
                        && !t.eq_ignore_ascii_case("inf")
                        && !t.eq_ignore_ascii_case("infinity")
                        && !t.eq_ignore_ascii_case("+inf")
                        && !t.eq_ignore_ascii_case("+infinity")
                        && !t.eq_ignore_ascii_case("-inf")
                        && !t.eq_ignore_ascii_case("-infinity")
                    {
                        Err(exec_err(
                            "22003",
                            format!("value {:?} is out of range for type double precision", s),
                        ))
                    } else {
                        Ok(f)
                    }
                }
                Err(_) => Err(exec_err(
                    "22P02",
                    format!("invalid input syntax for type double precision: {:?}", s),
                )),
            }
        }
        other => Err(cast_err(other, "double precision")),
    }
}

fn cast_to_numeric(v: &Value) -> Result<Numeric, ExecError> {
    match v {
        Value::SmallInt(i) => Ok(Numeric::new(*i as i128, 0)),
        Value::Int(i) => Ok(Numeric::new(*i as i128, 0)),
        Value::BigInt(i) => Ok(Numeric::new(*i as i128, 0)),
        Value::Numeric(n) => Ok(n.clone()),
        Value::Float4(f) => Numeric::from_f64(*f as f64)
            .map_err(|_| exec_err("22003", "value out of range for type numeric")),
        Value::Float(f) => Numeric::from_f64(*f)
            .map_err(|_| exec_err("22003", "value out of range for type numeric")),
        Value::Bool(b) => Ok(Numeric::new(*b as i128, 0)),
        Value::Text(s) => match Numeric::parse(s) {
            Ok(n) => Ok(n),
            Err(crate::storage::NumericParseError::Syntax) => Err(exec_err(
                "22P02",
                format!("invalid input syntax for type numeric: {:?}", s),
            )),
            Err(crate::storage::NumericParseError::Overflow) => {
                Err(exec_err("22003", "value overflows numeric"))
            }
        },
        other => Err(cast_err(other, "numeric")),
    }
}

fn cast_to_bool(v: &Value) -> Result<bool, ExecError> {
    match v {
        Value::Bool(b) => Ok(*b),
        // v0.14: PostgreSQL boolean input accepts unambiguous prefixes of
        // true/false/yes/no/on/off (e.g. 'of' -> false, 'tru' -> true);
        // a bare 'o' is ambiguous (on/off) and rejected, like PG.
        Value::Text(s) => {
            let l = s.trim().to_ascii_lowercase();
            let parsed = match l.chars().next() {
                Some('t') if "true".starts_with(l.as_str()) => Some(true),
                Some('f') if "false".starts_with(l.as_str()) => Some(false),
                Some('y') if "yes".starts_with(l.as_str()) => Some(true),
                Some('n') if "no".starts_with(l.as_str()) => Some(false),
                Some('o') => {
                    let on = "on".starts_with(l.as_str());
                    let off = "off".starts_with(l.as_str());
                    match (on, off) {
                        (true, false) => Some(true),
                        (false, true) => Some(false),
                        _ => None, // ambiguous ('o') or invalid
                    }
                }
                Some('1') if l.len() == 1 => Some(true),
                Some('0') if l.len() == 1 => Some(false),
                _ => None,
            };
            parsed.ok_or_else(|| {
                exec_err(
                    "22P02",
                    format!("invalid input syntax for type boolean: {:?}", s),
                )
            })
        }
        // v0.14: PostgreSQL casts integers to boolean (0 -> false, else true).
        Value::SmallInt(i) => Ok(*i != 0),
        Value::Int(i) => Ok(*i != 0),
        Value::BigInt(i) => Ok(*i != 0),
        other => Err(cast_err(other, "boolean")),
    }
}

/// Text rendering for casts and `||`: like the wire format, except
/// booleans render as `true`/`false` (Postgres cast output).
fn value_to_text_cast(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        other => other.to_text().unwrap_or_default(),
    }
}

fn eval_cast(v: &Value, to: ColType) -> Result<Value, ExecError> {
    if v == &Value::Null {
        return Ok(Value::Null);
    }
    if v.col_type() == to {
        return Ok(v.clone());
    }
    match to {
        ColType::Text => Ok(Value::Text(value_to_text_cast(v))),
        ColType::Bool => cast_to_bool(v).map(Value::Bool),
        ColType::SmallInt => {
            let i = cast_to_int(v)?;
            i16::try_from(i)
                .map(Value::SmallInt)
                .map_err(|_| exec_err("22003", "smallint out of range"))
        }
        ColType::Int => {
            let i = cast_to_int(v)?;
            i32::try_from(i)
                .map(|w| Value::Int(w as i64))
                .map_err(|_| exec_err("22003", "integer out of range"))
        }
        ColType::BigInt => {
            let i = cast_to_int(v)?;
            i64::try_from(i)
                .map(Value::BigInt)
                .map_err(|_| exec_err("22003", "bigint out of range"))
        }
        ColType::Float4 => {
            let f = cast_to_f64(v)?;
            if f.abs() > f32::MAX as f64 {
                return Err(exec_err("22003", "value out of range for type real"));
            }
            Ok(Value::Float4(f as f32))
        }
        ColType::Float => cast_to_f64(v).map(Value::Float),
        ColType::Numeric => cast_to_numeric(v).map(Value::Numeric),
        ColType::Date => match v {
            Value::Text(s) => crate::datetime::parse_date(s)
                .map(Value::Date)
                .map_err(|e| exec_err("22P02", e)),
            // Timestamps truncate to their UTC date (v0.7 is always UTC).
            Value::Timestamp(m) | Value::Timestamptz(m) => {
                Ok(Value::Date((m.div_euclid(86_400_000_000)) as i32))
            }
            other => Err(cast_err(other, "date")),
        },
        ColType::Timestamp => match v {
            Value::Text(s) => crate::datetime::parse_timestamp(s)
                .map(Value::Timestamp)
                .map_err(|e| exec_err("22P02", e)),
            Value::Date(d) => (*d as i64)
                .checked_mul(86_400_000_000)
                .map(Value::Timestamp)
                .ok_or_else(|| exec_err("22008", "datetime field overflow")),
            // Timestamptz -> timestamp keeps the instant; v0.7 has no
            // session timezone so this is the UTC wall-clock time.
            Value::Timestamptz(m) => Ok(Value::Timestamp(*m)),
            other => Err(cast_err(other, "timestamp without time zone")),
        },
        ColType::Timestamptz => match v {
            Value::Text(s) => crate::datetime::parse_timestamptz(s)
                .map(Value::Timestamptz)
                .map_err(|e| exec_err("22P02", e)),
            Value::Date(d) => (*d as i64)
                .checked_mul(86_400_000_000)
                .map(Value::Timestamptz)
                .ok_or_else(|| exec_err("22008", "datetime field overflow")),
            // Timestamp -> timestamptz assumes UTC (documented: v0.7
            // has no session timezone setting).
            Value::Timestamp(m) => Ok(Value::Timestamptz(*m)),
            other => Err(cast_err(other, "timestamp with time zone")),
        },
        ColType::Bytea => match v {
            Value::Text(s) => crate::storage::parse_bytea(s)
                .map(Value::Bytea)
                .map_err(|_| {
                    exec_err(
                        "22P02",
                        format!("invalid input syntax for type bytea: {:?}", s),
                    )
                }),
            other => Err(cast_err(other, "bytea")),
        },
        ColType::Uuid => match v {
            Value::Text(s) => crate::storage::parse_uuid(s).map(Value::Uuid).map_err(|_| {
                exec_err(
                    "22P02",
                    format!("invalid input syntax for type uuid: {:?}", s),
                )
            }),
            other => Err(cast_err(other, "uuid")),
        },
    }
}

// ---------------------------------------------------------------------------
// ||, LIKE, BETWEEN, IS TRUE/FALSE/UNKNOWN
// ---------------------------------------------------------------------------

/// `||`: bytea||bytea -> bytea; anything else coerces to text
/// (Postgres' anynonarray || text behavior). NULL propagates.
fn eval_concat(a: &Value, b: &Value) -> Result<Value, ExecError> {
    if a == &Value::Null || b == &Value::Null {
        return Ok(Value::Null);
    }
    match (a, b) {
        (Value::Bytea(x), Value::Bytea(y)) => {
            let mut r = Vec::with_capacity(x.len() + y.len());
            r.extend_from_slice(x);
            r.extend_from_slice(y);
            Ok(Value::Bytea(r))
        }
        _ => {
            let mut s = value_to_text_cast(a);
            s.push_str(&value_to_text_cast(b));
            Ok(Value::Text(s))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatTok {
    Lit(char),
    Any,  // `_`
    Star, // `%`
}

/// Tokenize a LIKE pattern; backslash escapes the next character.
fn tokenize_like(pat: &[char]) -> Vec<PatTok> {
    let mut toks = Vec::new();
    let mut i = 0;
    while i < pat.len() {
        if pat[i] == '\\' && i + 1 < pat.len() {
            toks.push(PatTok::Lit(pat[i + 1]));
            i += 2;
        } else if pat[i] == '%' {
            toks.push(PatTok::Star);
            i += 1;
        } else if pat[i] == '_' {
            toks.push(PatTok::Any);
            i += 1;
        } else {
            toks.push(PatTok::Lit(pat[i]));
            i += 1;
        }
    }
    toks
}

/// Classic backtracking LIKE matcher over tokenized pattern.
fn match_like(s: &[char], toks: &[PatTok]) -> bool {
    let (mut si, mut ti) = (0, 0);
    let mut star_ti: Option<usize> = None;
    let mut star_si = 0;
    while si < s.len() {
        if ti < toks.len()
            && (toks[ti] == PatTok::Any || matches!(toks[ti], PatTok::Lit(c) if c == s[si]))
        {
            si += 1;
            ti += 1;
        } else if ti < toks.len() && toks[ti] == PatTok::Star {
            star_ti = Some(ti);
            star_si = si;
            ti += 1;
        } else if let Some(st) = star_ti {
            ti = st + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while ti < toks.len() && toks[ti] == PatTok::Star {
        ti += 1;
    }
    ti == toks.len()
}

fn eval_like(a: &Value, pattern: &Value, not: bool, ilike: bool) -> Result<Value, ExecError> {
    match (a, pattern) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(s), Value::Text(p)) => {
            let (s, p) = if ilike {
                (s.to_lowercase(), p.to_lowercase())
            } else {
                (s.clone(), p.clone())
            };
            let sc: Vec<char> = s.chars().collect();
            let pc: Vec<char> = p.chars().collect();
            let m = match_like(&sc, &tokenize_like(&pc));
            Ok(Value::Bool(if not { !m } else { m }))
        }
        _ => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                a.type_name(),
                if ilike { "~~*" } else { "~~" },
                pattern.type_name()
            ),
        )),
    }
}

/// `x BETWEEN lo AND hi` = `x >= lo AND x <= hi`; NULL in any operand
/// -> NULL; type mismatches surface as 42883 from the comparisons.
fn eval_between(v: &Value, lo: &Value, hi: &Value, neg: bool) -> Result<Value, ExecError> {
    if v == &Value::Null || lo == &Value::Null || hi == &Value::Null {
        return Ok(Value::Null);
    }
    let ge = eval_cmp_vals(CmpOp::Ge, v, lo)?;
    let le = eval_cmp_vals(CmpOp::Le, v, hi)?;
    match (ge, le) {
        (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(if neg { !(a && b) } else { a && b })),
        // Unreachable: non-null inputs compare to bools or raise.
        _ => Ok(Value::Null),
    }
}

/// `IS [NOT] TRUE/FALSE/UNKNOWN`. Non-boolean input is 42804.
fn eval_is_bool(v: &Value, neg: bool, val: Option<bool>) -> Result<Value, ExecError> {
    let matched = match (v, val) {
        (Value::Null, None) => true,
        (Value::Bool(b), Some(w)) => *b == w,
        (Value::Null, Some(_)) | (Value::Bool(_), None) => false,
        (other, _) => {
            return Err(exec_err(
                "42804",
                format!(
                    "argument of IS must be type boolean, not type {}",
                    other.type_name()
                ),
            ));
        }
    };
    Ok(Value::Bool(matched != neg))
}

// ---------------------------------------------------------------------------
// EXTRACT
// ---------------------------------------------------------------------------

fn eval_extract(field: &str, v: &Value) -> Result<Value, ExecError> {
    if v == &Value::Null {
        return Ok(Value::Null);
    }
    let field = field.to_ascii_lowercase();
    let r = match v {
        Value::Timestamp(m) | Value::Timestamptz(m) => crate::datetime::extract(&field, *m),
        Value::Date(d) => crate::datetime::extract_date(&field, *d),
        other => {
            return Err(exec_err(
                "42883",
                format!(
                    "function extract({} from {}) does not exist",
                    field,
                    other.type_name()
                ),
            ));
        }
    };
    match r {
        Ok(f) => Numeric::from_f64(f)
            .map(Value::Numeric)
            .map_err(|_| exec_err("22003", "value out of range for type numeric")),
        Err(e) => Err(exec_err("22023", e)),
    }
}

// ---------------------------------------------------------------------------
// Built-in scalar functions
// ---------------------------------------------------------------------------

fn func_arg_err(fname: &str, v: &Value) -> ExecError {
    exec_err(
        "42883",
        format!("function {}({}) does not exist", fname, v.type_name()),
    )
}

/// Strict text argument (no implicit casts, like Postgres' LIKE).
fn str_arg<'a>(fname: &str, v: &'a Value) -> Result<Option<&'a str>, ExecError> {
    match v {
        Value::Null => Ok(None),
        Value::Text(s) => Ok(Some(s)),
        other => Err(func_arg_err(fname, other)),
    }
}

/// Strict integer-kind argument.
fn int_arg(fname: &str, v: &Value) -> Result<Option<i64>, ExecError> {
    match v {
        Value::Null => Ok(None),
        Value::SmallInt(i) => Ok(Some(*i as i64)),
        Value::Int(i) => Ok(Some(*i)),
        Value::BigInt(i) => Ok(Some(*i)),
        other => Err(func_arg_err(fname, other)),
    }
}

fn eval_func(q: &mut Q, scopes: &[Scope], name: &str, args: &[Expr]) -> Result<Value, ExecError> {
    // v0.9: sequence functions need engine + snapshot + session access;
    // they cannot go through the pure eval_func_vals path.
    if matches!(name, "nextval" | "currval" | "setval") {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(eval_expr(q, scopes, a)?);
        }
        check_builtin_arity(name, &vals)?;
        let (eng, snap, own, session) = (&mut *q.eng, &*q.snap, q.own, q.session);
        return eval_sequence_func(eng, snap, own, session, q.role, q.read_only, name, &vals);
    }
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_expr(q, scopes, a)?);
    }
    eval_func_vals(name, &vals)
}

/// Validate argument counts for scalar built-ins. A wrong count is
/// 42883 (undefined_function), like Postgres — never an index panic.
fn check_builtin_arity(name: &str, vals: &[Value]) -> Result<(), ExecError> {
    let n = vals.len();
    let ok = match name {
        "upper" | "lower" | "length" | "char_length" | "character_length" | "abs" | "floor"
        | "ceil" | "ceiling" | "sqrt" => n == 1,
        "round" => n == 1 || n == 2,
        "substring" | "substr" => n == 2 || n == 3,
        "power" | "mod" | "position" | "date_trunc" | "nullif" => n == 2,
        "replace" | "split_part" | "trim" => n == 3,
        // v0.16: new string/math built-ins.
        "concat" => true, // concat() with no args is '' (Postgres).
        "concat_ws" => n >= 1,
        "to_hex" | "to_oct" | "to_bin" | "sign" | "reverse" => n == 1,
        "left" | "right" => n == 2,
        "now" | "current_date" | "current_timestamp" => n == 0,
        // v0.17: transaction/clock timestamps.
        "clock_timestamp" | "statement_timestamp" | "transaction_timestamp" => n == 0,
        // v0.17: version() takes no arguments.
        "version" => n == 0,
        // v0.17: date/time built-in batch.
        "date_part" | "to_char" | "timezone" => n == 2,
        "to_date" => n == 2,
        "to_timestamp" => n == 1 || n == 2, // (float8) or (text, text)
        "make_date" => n == 3,
        "make_timestamp" => n == 6,
        "coalesce" | "greatest" | "least" => n >= 1,
        // v0.14: PostgreSQL internal operator-function aliases (pg_regress).
        "booleq" | "boolne" | "int4eq" | "texteq" => n == 2,
        // v0.9: sequence functions.
        "nextval" | "currval" => n == 1,
        "setval" => n == 2 || n == 3,
        // EXTRACT and friends validate their own shapes; unknown names
        // fall through to the dispatch below which raises 42883.
        _ => true,
    };
    if ok {
        return Ok(());
    }
    let sig = vals
        .iter()
        .map(|v| v.type_name())
        .collect::<Vec<_>>()
        .join(", ");
    Err(exec_err(
        "42883",
        format!("function {}({}) does not exist", name, sig),
    ))
}

/// Dispatch on pre-evaluated argument values (used by the grouped path).
fn eval_func_vals(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    check_builtin_arity(name, vals)?;
    // v0.16: `substr` is a true alias of `substring` (same semantics).
    let name = if name == "substr" { "substring" } else { name };
    match name {
        "upper" | "lower" | "length" | "char_length" | "character_length" | "substring"
        | "trim" | "position" | "replace" | "split_part" | "concat" | "concat_ws" | "to_hex"
        | "to_oct" | "to_bin" | "left" | "right" | "reverse" => eval_str_func(name, vals),
        "abs" | "round" | "floor" | "ceil" | "ceiling" | "sqrt" | "power" | "mod" | "sign" => {
            eval_math_func(name, vals)
        }
        "now"
        | "current_date"
        | "current_timestamp"
        | "date_trunc"
        | "date_part"
        | "to_date"
        | "to_timestamp"
        | "to_char"
        | "make_date"
        | "make_timestamp"
        | "timezone"
        | "clock_timestamp"
        | "statement_timestamp"
        | "transaction_timestamp" => eval_datetime_func(name, vals),
        // v0.17: version() reports our own version, not a PG version we
        // claim to be (see SERVER_VERSION in server.rs).
        "version" => Ok(Value::Text(format!(
            "rustgres {} (PostgreSQL-compatible, protocol 3.0)",
            crate::server::SERVER_VERSION
        ))),
        "coalesce" | "nullif" | "greatest" | "least" => eval_cond_func(name, vals),
        // v0.14: PostgreSQL internal operator-function aliases (pg_regress
        // conformance): booleq(x,y) ≡ x = y, boolne(x,y) ≡ x <> y, etc.
        "booleq" | "int4eq" | "texteq" => eval_cmp_vals(CmpOp::Eq, &vals[0], &vals[1]),
        "boolne" => eval_cmp_vals(CmpOp::Ne, &vals[0], &vals[1]),
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

fn eval_str_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    match name {
        "upper" => Ok(match str_arg(name, &vals[0])? {
            None => Value::Null,
            Some(s) => Value::Text(s.to_uppercase()),
        }),
        "lower" => Ok(match str_arg(name, &vals[0])? {
            None => Value::Null,
            Some(s) => Value::Text(s.to_lowercase()),
        }),
        "length" | "char_length" | "character_length" => Ok(match str_arg(name, &vals[0])? {
            None => Value::Null,
            Some(s) => Value::Int(s.chars().count() as i64),
        }),
        "substring" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let start = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let len = if vals.len() > 2 {
                match int_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(n) => {
                        if n < 0 {
                            return Err(exec_err("22011", "negative substring length not allowed"));
                        }
                        Some(n)
                    }
                }
            } else {
                None
            };
            // 1-based; start < 1 shifts the window (Postgres rule).
            let chars: Vec<char> = s.chars().collect();
            let total = chars.len() as i64;
            let from = start.max(1);
            let upto = match len {
                Some(n) => start + n,
                None => total + 1,
            };
            let lo = (from - 1).max(0).min(total) as usize;
            let hi = (upto - 1).max(0).min(total) as usize;
            let (lo, hi) = (lo.min(hi), hi);
            Ok(Value::Text(chars[lo..hi].iter().collect()))
        }
        "trim" => {
            // Parser encodes trim as (spec, chars, str); the 1-arg form
            // arrives as ("both", " ", str).
            let (spec, ch, s) = (&vals[0], &vals[1], &vals[2]);
            let s = match str_arg(name, s)? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let spec = match spec {
                Value::Text(t) => t.as_str(),
                _ => return Err(exec_err("22023", "invalid trim specification")),
            };
            if !matches!(spec, "leading" | "trailing" | "both") {
                return Err(exec_err("22023", "invalid trim specification"));
            }
            let ch = match str_arg(name, ch)? {
                None => " ",
                Some(c) => c,
            };
            let set: Vec<char> = ch.chars().collect();
            let t = |c: char| set.contains(&c);
            let r = match spec {
                "leading" => s.trim_start_matches(t).to_string(),
                "trailing" => s.trim_end_matches(t).to_string(),
                _ => s.trim_matches(t).to_string(),
            };
            Ok(Value::Text(r))
        }
        "position" => {
            let sub = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let s = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            if sub.is_empty() {
                return Ok(Value::Int(1));
            }
            let sc: Vec<char> = s.chars().collect();
            let nc: Vec<char> = sub.chars().collect();
            let pos = sc
                .windows(nc.len())
                .position(|w| w == nc.as_slice())
                .map(|i| i as i64 + 1)
                .unwrap_or(0);
            Ok(Value::Int(pos))
        }
        "replace" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let from = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let to = match str_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            // Postgres: empty search string leaves the input unchanged.
            if from.is_empty() {
                return Ok(Value::Text(s.to_string()));
            }
            Ok(Value::Text(s.replace(from, to)))
        }
        "split_part" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let delim = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let n = match int_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            if n == 0 {
                return Err(exec_err(
                    "22023",
                    "field position must be greater than zero",
                ));
            }
            let parts: Vec<&str> = if delim.is_empty() {
                // Split into characters (Postgres behavior).
                let mut v: Vec<&str> = Vec::new();
                let mut i = 0;
                for c in s.chars() {
                    let l = c.len_utf8();
                    v.push(&s[i..i + l]);
                    i += l;
                }
                v
            } else {
                s.split(delim).collect()
            };
            let idx = if n > 0 { n - 1 } else { parts.len() as i64 + n };
            let r = if idx >= 0 {
                parts.get(idx as usize).copied().unwrap_or("")
            } else {
                ""
            };
            Ok(Value::Text(r.to_string()))
        }
        // --- v0.16: missing built-ins (pg_regress 42883 cluster) -----------
        "concat" => {
            // NULL arguments are ignored; every other type is coerced via
            // its text output, like Postgres.
            let mut out = String::new();
            for v in vals {
                if let Some(t) = v.to_text() {
                    out.push_str(&t);
                }
            }
            Ok(Value::Text(out))
        }
        "concat_ws" => {
            // A NULL separator makes the whole result NULL; NULL
            // arguments are skipped (no stray separators), like Postgres.
            let sep = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let mut out = String::new();
            let mut first = true;
            for v in &vals[1..] {
                if let Some(t) = v.to_text() {
                    if !first {
                        out.push_str(sep);
                    }
                    out.push_str(&t);
                    first = false;
                }
            }
            Ok(Value::Text(out))
        }
        "to_hex" | "to_oct" | "to_bin" => {
            // Postgres width rule: int2/int4 render negatives as 32-bit
            // two's complement, int8 as 64-bit two's complement; the
            // width comes from the *type* of the argument, not its range
            // (so -1234::bigint is 64-bit). Non-negative values render
            // with no leading zeros.
            let (wide, v): (bool, i64) = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::SmallInt(i) => (false, *i as i64),
                Value::Int(i) => (false, *i),
                Value::BigInt(i) => (true, *i),
                other => return Err(func_arg_err(name, other)),
            };
            let r = if wide {
                let u = v as u64;
                match name {
                    "to_hex" => format!("{:x}", u),
                    "to_oct" => format!("{:o}", u),
                    _ => format!("{:b}", u),
                }
            } else {
                let u = (v as i32) as u32;
                match name {
                    "to_hex" => format!("{:x}", u),
                    "to_oct" => format!("{:o}", u),
                    _ => format!("{:b}", u),
                }
            };
            Ok(Value::Text(r))
        }
        "left" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let n = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            // Negative n drops the last |n| characters (Postgres rule).
            let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
            Ok(Value::Text(chars[..take as usize].iter().collect()))
        }
        "right" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let n = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            // Negative n drops the first |n| characters (Postgres rule).
            let skip = if n >= 0 {
                (len - n).max(0)
            } else {
                (-n).min(len)
            };
            Ok(Value::Text(chars[skip as usize..].iter().collect()))
        }
        "reverse" => Ok(match str_arg(name, &vals[0])? {
            None => Value::Null,
            Some(s) => Value::Text(s.chars().rev().collect()),
        }),
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// Shared `power(a, b)` / `a ^ b` implementation. `bad` builds the
/// type-mismatch error (42883), which differs between the function and
/// operator spellings.
fn eval_power_op(
    a: &Value,
    b: &Value,
    bad: impl Fn(&Value) -> ExecError,
) -> Result<Value, ExecError> {
    if matches!(a, Value::Float4(_) | Value::Float(_))
        || matches!(b, Value::Float4(_) | Value::Float(_))
    {
        return Ok(Value::Float(to_f64v(a).powf(to_f64v(b))));
    }
    let base = to_numeric_opt(a).ok_or_else(|| bad(a))?;
    let exp = to_numeric_opt(b).ok_or_else(|| bad(b))?;
    // Integer exponents are exact (like Postgres' numeric
    // power); anything else goes through f64.
    if exp.scale == 0 {
        if let Ok(e) = i64::try_from(exp.unscaled) {
            if let Some(n) = base.pow(e) {
                return Ok(Value::Numeric(n));
            }
        }
    }
    let f = base.to_f64().powf(exp.to_f64());
    if f.is_nan() {
        return Err(exec_err(
            "2201F",
            "a negative number raised to a non-integer power yields a non-real result",
        ));
    }
    if f.is_infinite() {
        return Err(exec_err("22003", "value out of range for type numeric"));
    }
    Numeric::from_f64(f)
        .map(Value::Numeric)
        .map_err(|_| exec_err("22003", "value out of range for type numeric"))
}

fn eval_math_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    let v = &vals[0];
    if v == &Value::Null || (vals.len() > 1 && vals[1] == Value::Null) {
        return Ok(Value::Null);
    }
    match name {
        "abs" => match v {
            Value::SmallInt(i) => i
                .checked_abs()
                .map(Value::SmallInt)
                .ok_or_else(|| exec_err("22003", "smallint out of range")),
            Value::Int(i) => i
                .checked_abs()
                .map(Value::Int)
                .ok_or_else(|| exec_err("22003", "integer out of range")),
            Value::BigInt(i) => i
                .checked_abs()
                .map(Value::BigInt)
                .ok_or_else(|| exec_err("22003", "bigint out of range")),
            Value::Numeric(n) => Ok(Value::Numeric(n.abs())),
            Value::Float4(f) => Ok(Value::Float4(f.abs())),
            Value::Float(f) => Ok(Value::Float(f.abs())),
            other => Err(func_arg_err(name, other)),
        },
        "round" => {
            // Postgres: round(numeric) -> numeric, round(float8) ->
            // numeric, round(numeric, int) -> numeric.
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            if vals.len() == 1 {
                n.round_to(0)
                    .map(Value::Numeric)
                    .ok_or_else(|| exec_err("22003", "numeric field overflow"))
            } else {
                let s = int_arg(name, &vals[1])?.unwrap_or(0);
                round_scale(&n, s)
                    .map(Value::Numeric)
                    .ok_or_else(|| exec_err("22003", "numeric field overflow"))
            }
        }
        "floor" | "ceil" | "ceiling" => {
            let is_floor = name == "floor";
            match v {
                Value::Float4(f) => Ok(Value::Float4(if is_floor { f.floor() } else { f.ceil() })),
                Value::Float(f) => Ok(Value::Float(if is_floor { f.floor() } else { f.ceil() })),
                other => {
                    let n = to_numeric_opt(other).ok_or_else(|| func_arg_err(name, other))?;
                    let r = if is_floor { n.floor() } else { n.ceil() };
                    r.map(Value::Numeric)
                        .ok_or_else(|| exec_err("22003", "numeric field overflow"))
                }
            }
        }
        "sqrt" => {
            if matches!(v, Value::Float4(_) | Value::Float(_)) {
                // Postgres: sqrt(float8) -> float8, NaN for negatives.
                return Ok(Value::Float(to_f64v(v).sqrt()));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            match n.sqrt() {
                Some(r) => Ok(Value::Numeric(r)),
                // Postgres numeric.c: ERRCODE_INVALID_ARGUMENT_FOR_POWER_FUNCTION.
                None => Err(exec_err(
                    "2201F",
                    "cannot take square root of a negative number",
                )),
            }
        }
        "power" => eval_power_op(v, &vals[1], |w| func_arg_err(name, w)),
        "mod" => {
            // Postgres resolves mod() to numeric.
            let a = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            let b = to_numeric_opt(&vals[1]).ok_or_else(|| func_arg_err(name, &vals[1]))?;
            if b.is_zero() {
                return Err(exec_err("22012", "division by zero"));
            }
            a.checked_rem(&b)
                .map(Value::Numeric)
                .ok_or_else(|| exec_err("22003", "numeric field overflow"))
        }
        // v0.16: sign() returns -1/0/1 in the input's own type, like
        // Postgres (NaN stays NaN for floats).
        "sign" => match v {
            Value::SmallInt(i) => Ok(Value::SmallInt(i.signum())),
            Value::Int(i) => Ok(Value::Int(i.signum())),
            Value::BigInt(i) => Ok(Value::BigInt(i.signum())),
            Value::Numeric(n) => {
                let s = match n.cmp(&Numeric::new(0, 0)) {
                    std::cmp::Ordering::Less => -1i128,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                };
                Ok(Value::Numeric(Numeric::new(s, 0)))
            }
            Value::Float4(f) => Ok(Value::Float4(f.signum())),
            Value::Float(f) => Ok(Value::Float(f.signum())),
            other => Err(func_arg_err(name, other)),
        },
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// round(n, s) for negative s (round_to only takes u32 scales).
fn round_scale(n: &Numeric, s: i64) -> Option<Numeric> {
    if s >= 0 {
        return n.round_to(s as u32);
    }
    let mag = 10i128.checked_pow((-s) as u32)?;
    let shifted = Numeric::new(n.unscaled.checked_div(mag)?, 0);
    let rounded = shifted.round_to(0)?;
    Some(Numeric::new(rounded.unscaled.checked_mul(mag)?, 0))
}

/// Map a format-string error to its SQLSTATE: unsupported pattern
/// elements are 0A000, bad input values are 22008.
fn fmt_exec_err(e: crate::datetime::FmtErr) -> ExecError {
    match e {
        crate::datetime::FmtErr::Unsupported(msg) => exec_err("0A000", msg),
        crate::datetime::FmtErr::Invalid(msg) => exec_err("22008", msg),
    }
}

fn eval_datetime_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    match name {
        "now" | "current_timestamp" => Ok(Value::Timestamptz(crate::datetime::now_micros())),
        "current_date" => Ok(Value::Date(crate::datetime::today_days())),
        "date_trunc" => {
            let field = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s.to_ascii_lowercase(),
            };
            let v = &vals[1];
            if v == &Value::Null {
                return Ok(Value::Null);
            }
            let (micros, is_tz) = match v {
                Value::Date(d) => (
                    (*d as i64)
                        .checked_mul(86_400_000_000)
                        .ok_or_else(|| exec_err("22008", "datetime field overflow"))?,
                    false,
                ),
                Value::Timestamp(m) => (*m, false),
                Value::Timestamptz(m) => (*m, true),
                other => return Err(func_arg_err(name, other)),
            };
            match crate::datetime::date_trunc(&field, micros) {
                Ok(m) => Ok(if is_tz {
                    Value::Timestamptz(m)
                } else {
                    Value::Timestamp(m)
                }),
                Err(e) => Err(exec_err("22023", e)),
            }
        }
        // v0.17: function form of EXTRACT; identical semantics (and
        // identical numeric result) to `extract(field FROM x)`.
        "date_part" => {
            let field = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            eval_extract(field, &vals[1])
        }
        // v0.17: parse with a format string (documented pattern subset
        // in datetime.rs); unsupported patterns are 0A000, bad input
        // is 22008.
        "to_date" => {
            let input = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let fmt = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            match crate::datetime::to_date_parsed(input, fmt) {
                Ok(d) => Ok(Value::Date(d)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        "to_timestamp" => {
            if vals.len() == 1 {
                // to_timestamp(float8): Unix epoch seconds -> timestamptz.
                let v = &vals[0];
                if v == &Value::Null {
                    return Ok(Value::Null);
                }
                let secs = match v {
                    Value::SmallInt(_)
                    | Value::Int(_)
                    | Value::BigInt(_)
                    | Value::Numeric(_)
                    | Value::Float4(_)
                    | Value::Float(_) => to_f64v(v),
                    other => return Err(func_arg_err(name, other)),
                };
                let micros = secs * 1_000_000.0;
                if !micros.is_finite() || micros.abs() >= i64::MAX as f64 {
                    return Err(exec_err("22008", "timestamp out of range"));
                }
                return Ok(Value::Timestamptz(micros.round() as i64));
            }
            let input = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let fmt = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            match crate::datetime::to_timestamp_parsed(input, fmt) {
                Ok(m) => Ok(Value::Timestamp(m)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        // v0.17: format a date/timestamp/timestamptz with the same
        // documented pattern subset; renders in UTC (server is
        // UTC-only).
        "to_char" => {
            let v = &vals[0];
            if v == &Value::Null {
                return Ok(Value::Null);
            }
            let fmt = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let (days, tod) = match v {
                Value::Date(d) => (i64::from(*d), 0),
                Value::Timestamp(m) | Value::Timestamptz(m) => crate::datetime::split_micros(*m),
                other => return Err(func_arg_err(name, other)),
            };
            match crate::datetime::format_with_pattern(days, tod, fmt) {
                Ok(s) => Ok(Value::Text(s)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        // v0.17: make_date(y, m, d) -> date; out-of-range parts are 22008.
        "make_date" => {
            let y = match int_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let m = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let d = match int_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            match crate::datetime::make_date_checked(y, m, d) {
                Ok(days) => Ok(Value::Date(days)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        // v0.17: make_timestamp(y, mo, d, h, mi, seconds float8) ->
        // timestamp. There is no TIME type in the engine, so
        // make_time() stays 42883 (documented).
        "make_timestamp" => {
            let mut ints = [0i64; 5];
            for (i, slot) in ints.iter_mut().enumerate() {
                *slot = match int_arg(name, &vals[i])? {
                    None => return Ok(Value::Null),
                    Some(n) => n,
                };
            }
            let secs_v = &vals[5];
            if secs_v == &Value::Null {
                return Ok(Value::Null);
            }
            let secs = match secs_v {
                Value::SmallInt(_)
                | Value::Int(_)
                | Value::BigInt(_)
                | Value::Numeric(_)
                | Value::Float4(_)
                | Value::Float(_) => to_f64v(secs_v),
                other => return Err(func_arg_err(name, other)),
            };
            let [y, mo, d, h, mi] = ints;
            match crate::datetime::make_timestamp_checked(y, mo, d, h, mi, secs) {
                Ok(m) => Ok(Value::Timestamp(m)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        // v0.17: timezone(text, timestamptz) -> timestamp and
        // timezone(text, timestamp) -> timestamptz. The server is
        // UTC-only, so only 'UTC' (case-insensitive) is accepted;
        // anything else is 0A000 (documented).
        "timezone" => {
            let zone = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            if !zone.eq_ignore_ascii_case("utc") {
                return Err(exec_err(
                    "0A000",
                    format!("time zone {zone:?} is not supported (server is UTC-only)"),
                ));
            }
            let v = &vals[1];
            if v == &Value::Null {
                return Ok(Value::Null);
            }
            match v {
                Value::Timestamptz(m) => Ok(Value::Timestamp(*m)),
                Value::Timestamp(m) => Ok(Value::Timestamptz(*m)),
                other => Err(func_arg_err(name, other)),
            }
        }
        // v0.17: the engine does not pin transaction-start time, so
        // statement_timestamp() and transaction_timestamp() return the
        // execution time, exactly like clock_timestamp() — a documented
        // deviation from Postgres, where now()/transaction_timestamp()
        // are frozen at transaction start.
        "clock_timestamp" | "statement_timestamp" | "transaction_timestamp" => {
            Ok(Value::Timestamptz(crate::datetime::now_micros()))
        }
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

fn eval_cond_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    match name {
        "coalesce" => Ok(vals
            .iter()
            .find(|v| **v != Value::Null)
            .cloned()
            .unwrap_or(Value::Null)),
        "nullif" => {
            let (a, b) = (&vals[0], &vals[1]);
            if a == &Value::Null || b == &Value::Null {
                return Ok(a.clone());
            }
            match cmp_ordering(a, b, CmpOp::Eq)? {
                Some(Ordering::Equal) => Ok(Value::Null),
                _ => Ok(a.clone()),
            }
        }
        // Postgres: GREATEST/LEAST ignore NULLs; all-NULL -> NULL.
        "greatest" | "least" => {
            let want_greatest = name == "greatest";
            let mut best: Option<&Value> = None;
            for v in vals {
                if v == &Value::Null {
                    continue;
                }
                match best {
                    None => best = Some(v),
                    Some(cur) => {
                        let ord = cmp_ordering(cur, v, CmpOp::Eq)?.ok_or_else(|| {
                            exec_err("XX000", "internal error: null in greatest/least")
                        })?;
                        let take = if want_greatest {
                            ord == Ordering::Less
                        } else {
                            ord == Ordering::Greater
                        };
                        if take {
                            best = Some(v);
                        }
                    }
                }
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// Result-column type of a built-in function call (Describe path).
fn func_result_type(
    name: &str,
    args: &[Expr],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    ctes: &[CteDef],
) -> Result<ColType, ExecError> {
    let arg0 = || expr_type(eng, snap, own, schemas, ctes, &args[0]);
    match name {
        "upper" | "lower" | "substring" | "substr" | "trim" | "replace" | "split_part"
        | "concat" | "concat_ws" | "to_hex" | "to_oct" | "to_bin" | "left" | "right"
        | "reverse" => Ok(ColType::Text),
        "length" | "char_length" | "character_length" | "position" => Ok(ColType::Int),
        "abs" | "sign" => arg0(),
        "round" | "mod" => Ok(ColType::Numeric),
        "floor" | "ceil" | "ceiling" => match arg0()? {
            ColType::Float4 => Ok(ColType::Float4),
            ColType::Float => Ok(ColType::Float),
            _ => Ok(ColType::Numeric),
        },
        // Any float argument -> Float, else Numeric (documented:
        // Postgres returns numeric for sqrt(real)).
        "sqrt" | "power" => {
            for a in args {
                match expr_type(eng, snap, own, schemas, ctes, a)? {
                    ColType::Float4 | ColType::Float => return Ok(ColType::Float),
                    _ => {}
                }
            }
            Ok(ColType::Numeric)
        }
        "now" | "current_timestamp" => Ok(ColType::Timestamptz),
        // v0.17: transaction/clock timestamps are timestamptz.
        "clock_timestamp" | "statement_timestamp" | "transaction_timestamp" => {
            Ok(ColType::Timestamptz)
        }
        "current_date" => Ok(ColType::Date),
        "date_trunc" => match expr_type(eng, snap, own, schemas, ctes, &args[1])? {
            ColType::Timestamptz => Ok(ColType::Timestamptz),
            _ => Ok(ColType::Timestamp),
        },
        // v0.17: date/time built-in batch.
        "date_part" => Ok(ColType::Numeric),
        "to_date" | "make_date" => Ok(ColType::Date),
        "to_timestamp" => Ok(ColType::Timestamptz),
        "to_char" => Ok(ColType::Text),
        // v0.17: version() returns text.
        "version" => Ok(ColType::Text),
        "make_timestamp" => Ok(ColType::Timestamp),
        "timezone" => match expr_type(eng, snap, own, schemas, ctes, &args[1])? {
            ColType::Timestamptz => Ok(ColType::Timestamp),
            ColType::Timestamp => Ok(ColType::Timestamptz),
            // Other inputs are a runtime 42883; Describe still needs a
            // type, so fall back to timestamptz.
            _ => Ok(ColType::Timestamptz),
        },
        "coalesce" | "nullif" | "greatest" | "least" => arg0(),
        // v0.9: sequence functions return bigint (INT here).
        "nextval" | "currval" | "setval" => Ok(ColType::Int),
        // v0.14: PostgreSQL internal operator-function aliases return boolean.
        "booleq" | "boolne" | "int4eq" | "texteq" => Ok(ColType::Bool),
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// Compare two values for ORDER BY. Exact numerics (int2/int4/int8/
/// numeric) compare exactly; float4/float8 compare by `total_cmp`;
/// mixed exact/float goes through f64 (a documented precision caveat).
/// Text compares byte-wise (no collation support yet), bools order
/// false < true, dates/times/bytea/uuid compare naturally. Mismatched
/// non-null types are an error, like PostgreSQL. NULL placement follows
/// PostgreSQL defaults unless overridden: NULLS LAST for ASC,
/// NULLS FIRST for DESC.
fn compare_values(
    a: &Value,
    b: &Value,
    desc: bool,
    nulls_first: Option<bool>,
) -> Result<Ordering, ExecError> {
    let nulls_first = nulls_first.unwrap_or(desc);
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
        (x, y) if is_exact_numeric(x) && is_exact_numeric(y) => {
            exact_numeric(x).cmp(&exact_numeric(y))
        }
        (Value::Float4(x), Value::Float4(y)) => (*x as f64).total_cmp(&(*y as f64)),
        (Value::Float4(x), Value::Float(y)) => (*x as f64).total_cmp(y),
        (Value::Float(x), Value::Float4(y)) => x.total_cmp(&(*y as f64)),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (x, y @ (Value::Float4(_) | Value::Float(_))) if is_exact_numeric(x) => {
            (exact_to_f64(x)).total_cmp(&float_val(y))
        }
        (x @ (Value::Float4(_) | Value::Float(_)), y) if is_exact_numeric(y) => {
            float_val(x).total_cmp(&exact_to_f64(y))
        }
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x.cmp(y),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
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

/// Exact numeric kinds: int2/int4/int8/numeric.
fn is_exact_numeric(v: &Value) -> bool {
    matches!(
        v,
        Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) | Value::Numeric(_)
    )
}

/// Lift an exact numeric to a canonical Numeric for comparison.
fn exact_numeric(v: &Value) -> Numeric {
    match v {
        Value::SmallInt(i) => Numeric::new(*i as i128, 0),
        Value::Int(i) => Numeric::new(*i as i128, 0),
        Value::BigInt(i) => Numeric::new(*i as i128, 0),
        Value::Numeric(n) => n.clone(),
        _ => Numeric::zero(),
    }
}

fn exact_to_f64(v: &Value) -> f64 {
    match v {
        Value::SmallInt(i) => *i as f64,
        Value::Int(i) => *i as f64,
        Value::BigInt(i) => *i as f64,
        Value::Numeric(n) => n.to_f64(),
        _ => f64::NAN,
    }
}

fn float_val(v: &Value) -> f64 {
    match v {
        Value::Float4(f) => *f as f64,
        Value::Float(f) => *f,
        _ => f64::NAN,
    }
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::SmallInt(_) => "smallint",
        Value::Int(_) => "integer",
        Value::BigInt(_) => "bigint",
        Value::Float4(_) => "real",
        Value::Float(_) => "float",
        Value::Numeric(_) => "numeric",
        Value::Text(_) => "text",
        Value::Bool(_) => "boolean",
        Value::Date(_) => "date",
        Value::Timestamp(_) => "timestamp",
        Value::Timestamptz(_) => "timestamptz",
        Value::Bytea(_) => "bytea",
        Value::Uuid(_) => "uuid",
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
    visible: &[CteDef],
    bindings: &[Rc<CteBinding>],
) -> Result<Vec<Vec<QCol>>, ExecError> {
    let mut out = Vec::new();
    for item in from {
        from_schema_item(eng, snap, own, item, &mut out, visible, bindings)?;
    }
    Ok(out)
}

fn from_schema_item(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    item: &FromItem,
    out: &mut Vec<Vec<QCol>>,
    visible: &[CteDef],
    bindings: &[Rc<CteBinding>],
) -> Result<(), ExecError> {
    match item {
        FromItem::Table { name, alias } => {
            // v0.10: materialized CTE bindings (e.g. the recursive CTE
            // currently being evaluated) shadow everything.
            if let Some(b) = bindings.iter().rev().find(|b| b.name == *name) {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                out.push(
                    b.schema
                        .iter()
                        .map(|c| QCol {
                            qual: qual.clone(),
                            name: c.name.clone(),
                            ty: c.ty.clone(),
                        })
                        .collect(),
                );
                return Ok(());
            }
            // v0.10: CTEs shadow everything (like Postgres). `visible`
            // holds outer scopes' CTEs plus this query's own WITH list;
            // only CTEs before this one (outer + earlier siblings) are
            // visible to its body — a CTE never sees itself or later
            // siblings.
            if let Some(pos) = visible.iter().rposition(|c| c.name == *name) {
                let schema = describe_cte(eng, snap, own, &visible[pos], &visible[..pos])?;
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                out.push(
                    schema
                        .into_iter()
                        .map(|mut c| {
                            c.qual = qual.clone();
                            c
                        })
                        .collect(),
                );
                return Ok(());
            }
            // v0.9: information_schema virtual tables.
            if name == "information_schema.tables" {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = info_tables_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(schema);
                return Ok(());
            }
            // v0.13: pg_replication_slots is virtual too.
            if name.as_str() == "pg_replication_slots"
                && eng.db.find_table(name, snap, own).is_none()
            {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = pg_replication_slots_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(schema);
                return Ok(());
            }
            if name == "information_schema.columns" {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = info_columns_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(schema);
                return Ok(());
            }
            // v0.9: views describe as their stored SELECT's schema.
            if let Some(view) = eng.db.find_view(name, snap, own) {
                let stmt = parse_statement(&view.query).map_err(sql_err)?;
                let select = match stmt {
                    Stmt::Select(s) => s,
                    _ => {
                        return Err(exec_err(
                            "0A000",
                            format!("view \"{}\" query is not a SELECT", name),
                        ));
                    }
                };
                let cols = describe_select(eng, snap, own, &select, &[])?;
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = cols
                    .into_iter()
                    .enumerate()
                    .map(|(i, (cn, ty))| QCol {
                        qual: qual.clone(),
                        name: view.col_aliases.get(i).cloned().unwrap_or(cn),
                        ty,
                    })
                    .collect();
                out.push(schema);
                return Ok(());
            }
            // v0.8: the pg_stats system catalog is virtual — a real table
            // by that name takes precedence.
            if name == "pg_stats" && eng.db.find_table(name, snap, own).is_none() {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = pg_stats_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(schema);
                return Ok(());
            }
            // v0.11: the role catalogs are virtual too.
            if matches!(
                name.as_str(),
                "pg_authid" | "pg_roles" | "pg_user" | "pg_auth_members"
            ) && eng.db.find_table(name, snap, own).is_none()
            {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = match name.as_str() {
                    "pg_roles" => pg_roles_schema(),
                    "pg_user" => pg_user_schema(),
                    "pg_auth_members" => pg_auth_members_schema(),
                    _ => pg_authid_schema(),
                }
                .into_iter()
                .map(|mut c| {
                    c.qual = qual.clone();
                    c
                })
                .collect();
                out.push(schema);
                return Ok(());
            }
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
            let cols = describe_select_outer(eng, snap, own, sub, visible, &[])?;
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
        // v0.14: VALUES columns are `column1`, ... typed from the first
        // row's expressions (all-NULL columns describe as TEXT).
        FromItem::Values { rows, alias } => {
            let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
            let first = rows.first().cloned().unwrap_or_default();
            out.push(
                (0..ncols)
                    .map(|i| QCol {
                        qual: alias.clone(),
                        name: format!("column{}", i + 1),
                        ty: first
                            .get(i)
                            .and_then(|e| hint_type(eng, snap, own, &[], e))
                            .unwrap_or(ColType::Text),
                    })
                    .collect(),
            );
            Ok(())
        }
        FromItem::Join { left, right, .. } => {
            from_schema_item(eng, snap, own, left, out, visible, bindings)?;
            from_schema_item(eng, snap, own, right, out, visible, bindings)
        }
    }
}

/// v0.10: output schema of one CTE for the Describe path. `earlier` holds
/// the CTEs visible to the body (outer scopes + earlier siblings — never
/// the CTE itself). A recursive CTE describes as its non-recursive term.
fn describe_cte(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    cte: &CteDef,
    earlier: &[CteDef],
) -> Result<Vec<QCol>, ExecError> {
    let body = match &cte.body {
        CteBody::Simple(s) => s,
        CteBody::Union { left, .. } => left,
    };
    let cols = describe_select_outer(eng, snap, own, body, earlier, &[])?;
    Ok(cols
        .into_iter()
        .enumerate()
        .map(|(i, (name, ty))| QCol {
            qual: cte.name.clone(),
            name: cte.col_aliases.get(i).cloned().unwrap_or(name),
            ty,
        })
        .collect())
}

/// (name, type) of every output column. Used by Describe and by execution
/// itself, so the two can never disagree.
fn describe_select(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    stmt: &SelectStmt,
    bindings: &[Rc<CteBinding>],
) -> Result<Vec<(String, ColType)>, ExecError> {
    describe_select_outer(eng, snap, own, stmt, &[], bindings)
}

/// v0.10: `outer` holds the CTE definitions visible from enclosing query
/// levels (innermost last); the query's own WITH list is appended, so a
/// CTE body only sees outer CTEs and earlier siblings.
fn describe_select_outer(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    stmt: &SelectStmt,
    outer: &[CteDef],
    bindings: &[Rc<CteBinding>],
) -> Result<Vec<(String, ColType)>, ExecError> {
    let mut visible: Vec<CteDef> = outer.to_vec();
    visible.extend(stmt.with.iter().cloned());
    let schemas = from_schemas(eng, snap, own, &stmt.from, &visible, bindings)?;
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
                let ty = expr_type(eng, snap, own, &refs, &visible, expr)?;
                let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
                out.push((name, ty));
            }
        }
    }
    Ok(out)
}

/// Default output column name when there is no alias: the column name for
/// bare refs, the function name for aggregates, the type name for casts
/// (v0.14, like Postgres), `?column?` otherwise.
fn expr_col_name(e: &Expr) -> String {
    match e {
        Expr::Column { name, .. } => name.clone(),
        Expr::Agg { func, .. } => func.name().to_string(),
        Expr::Cast { to, .. } => to.pg_typname().to_string(),
        _ => "?column?".to_string(),
    }
}

fn expr_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    ctes: &[CteDef],
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
        Expr::Arith { op, left, right } => {
            let ta = arith_operand_type(eng, snap, own, schemas, ctes, left, *op)?;
            let tb = arith_operand_type(eng, snap, own, schemas, ctes, right, *op)?;
            combine_arith_types(*op, ta, tb)
        }
        Expr::Cast { to, .. } => Ok(*to),
        Expr::Concat(..) => Ok(ColType::Text),
        Expr::Cmp { .. }
        | Expr::And(_, _)
        | Expr::Or(_, _)
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::IsBool { .. }
        | Expr::Like { .. }
        | Expr::Between { .. }
        | Expr::InSub { .. }
        | Expr::Exists { .. } => Ok(ColType::Bool),
        Expr::Func { name, args } => func_result_type(name, args, eng, snap, own, schemas, ctes),
        Expr::Extract { .. } => Ok(ColType::Numeric),
        Expr::Agg {
            func,
            arg,
            distinct: _,
            arg2,
        } => agg_result_type(
            eng,
            snap,
            own,
            schemas,
            ctes,
            *func,
            arg.as_deref(),
            arg2.as_deref(),
        ),
        Expr::ScalarSub(sub) => {
            let cols = describe_select_outer(eng, snap, own, sub, ctes, &[])?;
            if cols.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            Ok(cols[1 - 1].clone().1)
        }
        // v0.10: window function result types.
        Expr::Window { func, args, .. } => {
            window_result_type(eng, snap, own, schemas, ctes, func, args)
        }
    }
}

/// v0.10: result type of a window function.
fn window_result_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    ctes: &[CteDef],
    func: &WindowFunc,
    args: &[Expr],
) -> Result<ColType, ExecError> {
    match func {
        // Postgres returns bigint for the ranking functions, integer
        // for ntile.
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => Ok(ColType::BigInt),
        WindowFunc::Ntile => Ok(ColType::Int),
        // lag/lead/first_value/last_value/nth_value: type of the value
        // argument.
        WindowFunc::Lag
        | WindowFunc::Lead
        | WindowFunc::FirstValue
        | WindowFunc::LastValue
        | WindowFunc::NthValue => {
            let a = args
                .first()
                .ok_or_else(|| exec_err("42883", "window function requires an argument"))?;
            expr_type(eng, snap, own, schemas, ctes, a)
        }
        WindowFunc::Agg(f) => {
            let arg = args.first();
            let arg2 = args.get(1);
            agg_result_type(eng, snap, own, schemas, ctes, *f, arg, arg2)
        }
    }
}

/// Operand type of an arithmetic operator for the description pass;
/// NULL contributes nothing.
fn arith_operand_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    schemas: &[&[QCol]],
    ctes: &[CteDef],
    e: &Expr,
    op: ArithOp,
) -> Result<Option<ColType>, ExecError> {
    match e {
        Expr::Literal(Literal::Null) => Ok(None),
        Expr::Literal(lit) => Ok(Some(lit.col_type())),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Column { .. } | Expr::ResolvedCol { .. } => {
            Ok(Some(expr_type(eng, snap, own, schemas, ctes, e)?))
        }
        Expr::Arith {
            op: inner,
            left,
            right,
        } => {
            let ta = arith_operand_type(eng, snap, own, schemas, ctes, left, *inner)?;
            let tb = arith_operand_type(eng, snap, own, schemas, ctes, right, *inner)?;
            Ok(Some(combine_arith_types(*inner, ta, tb)?))
        }
        Expr::Agg { .. } | Expr::ScalarSub(_) => {
            Ok(Some(expr_type(eng, snap, own, schemas, ctes, e)?))
        }
        Expr::Cast { to, .. } => Ok(Some(*to)),
        Expr::Func { .. } => Ok(Some(expr_type(eng, snap, own, schemas, ctes, e)?)),
        // Boolean / predicate expressions can't be arithmetic operands.
        _ => Err(exec_err(
            "42883",
            format!("operator does not exist: boolean {} integer", op.sql()),
        )),
    }
}

/// Rank in the numeric promotion lattice (v0.7):
/// smallint < integer < bigint < real < double precision < numeric.
/// (Postgres resolves real+int to double and anything+numeric to
/// numeric; int2+int2 resolves to int4 — see the special case below.)
fn numeric_rank(t: &ColType) -> Option<u8> {
    match t {
        ColType::SmallInt => Some(0),
        ColType::Int => Some(1),
        ColType::BigInt => Some(2),
        ColType::Float4 => Some(3),
        ColType::Float => Some(4),
        ColType::Numeric => Some(5),
        _ => None,
    }
}

fn rank_type(rank: u8) -> ColType {
    match rank {
        0 => ColType::SmallInt,
        1 => ColType::Int,
        2 => ColType::BigInt,
        3 => ColType::Float4,
        4 => ColType::Float,
        _ => ColType::Numeric,
    }
}

/// Result type of `a <op> b` given operand types (None = NULL/unknown
/// side). Date arithmetic: date +/- integer-kind -> date, date - date
/// -> integer. Anything else mismatched is 42883.
fn combine_arith_types(
    op: ArithOp,
    a: Option<ColType>,
    b: Option<ColType>,
) -> Result<ColType, ExecError> {
    let op_err = |x: &ColType, y: &ColType| {
        exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                x.sql_name(),
                op.sql(),
                y.sql_name()
            ),
        )
    };
    match (a, b) {
        (None, None) => Ok(ColType::Int),
        (None, Some(t)) | (Some(t), None) => Ok(t),
        (Some(x), Some(y)) => {
            // Date arithmetic (v0.7): date +/- int-kind -> date,
            // date - date -> integer (days). No intervals in v0.7.
            let is_int_kind =
                |t: &ColType| matches!(t, ColType::SmallInt | ColType::Int | ColType::BigInt);
            match (&x, &y) {
                (ColType::Date, y) if is_int_kind(y) => match op {
                    ArithOp::Add | ArithOp::Sub => Ok(ColType::Date),
                    _ => Err(op_err(&x, &y)),
                },
                // `int + date` commutes; `int - date` is undefined.
                (x, ColType::Date) if is_int_kind(x) => match op {
                    ArithOp::Add => Ok(ColType::Date),
                    _ => Err(op_err(&x, &y)),
                },
                (ColType::Date, ColType::Date) => match op {
                    ArithOp::Sub => Ok(ColType::Int),
                    _ => Err(op_err(&x, &y)),
                },
                (x, y) => match (numeric_rank(x), numeric_rank(y)) {
                    (Some(rx), Some(ry)) => {
                        if op == ArithOp::Pow {
                            // Postgres `^`: numeric for exact inputs,
                            // float8 when either side is floating.
                            return Ok(if rx.max(ry) >= 3 {
                                ColType::Float
                            } else {
                                ColType::Numeric
                            });
                        }
                        if op == ArithOp::Mod && matches!(rx.max(ry), 3 | 4) {
                            // Postgres defines % only for the exact numeric
                            // types (not real/double).
                            return Err(op_err(x, y));
                        }
                        // int2 <op> int2 -> int4, like Postgres.
                        if rx == 0 && ry == 0 {
                            return Ok(ColType::Int);
                        }
                        Ok(rank_type(rx.max(ry)))
                    }
                    _ => Err(op_err(x, y)),
                },
            }
        }
    }
}

fn numeric_agg_arg(func: &str, t: &ColType) -> Result<(), ExecError> {
    match t {
        ColType::SmallInt
        | ColType::Int
        | ColType::BigInt
        | ColType::Float4
        | ColType::Float
        | ColType::Numeric => Ok(()),
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
    ctes: &[CteDef],
    func: AggFunc,
    arg: Option<&Expr>,
    arg2: Option<&Expr>,
) -> Result<ColType, ExecError> {
    match func {
        // Postgres count() returns bigint.
        AggFunc::Count => Ok(ColType::BigInt),
        AggFunc::Avg => {
            if let Some(a) = arg {
                numeric_agg_arg("avg", &expr_type(eng, snap, own, schemas, ctes, a)?)?;
            }
            Ok(ColType::Float)
        }
        AggFunc::Sum => match arg {
            // Only COUNT takes `*`; the parser guarantees it.
            None => Ok(ColType::Int),
            Some(a) => {
                let t = expr_type(eng, snap, own, schemas, ctes, a)?;
                numeric_agg_arg("sum", &t)?;
                // v0.6 rule kept (documented): sum returns the
                // argument type; Postgres would widen int->bigint.
                Ok(t)
            }
        },
        AggFunc::Min | AggFunc::Max => {
            let a = arg.expect("min/max always take an argument");
            expr_type(eng, snap, own, schemas, ctes, a)
        }
        AggFunc::StringAgg => {
            // Delimiter should be text-ish; be permissive here (the
            // executor coerces via casts) and just require an argument.
            let a = arg.expect("string_agg always takes arguments");
            let t = expr_type(eng, snap, own, schemas, ctes, a)?;
            if !matches!(t, ColType::Text) {
                return Err(exec_err(
                    "42883",
                    format!("function string_agg({}) does not exist", t.sql_name()),
                ));
            }
            let _ = arg2;
            Ok(ColType::Text)
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
        Expr::Arith { op, left, right } => combine_arith_types(
            *op,
            hint_type(eng, snap, own, schemas, left),
            hint_type(eng, snap, own, schemas, right),
        )
        .ok(),
        Expr::Cast { to, .. } => Some(*to),
        Expr::Concat(..) => Some(ColType::Text),
        Expr::Extract { .. } => Some(ColType::Numeric),
        Expr::Func { name, args } => {
            func_result_type(name, args, eng, snap, own, schemas, &[]).ok()
        }
        Expr::Agg {
            func,
            arg,
            distinct: _,
            arg2,
        } => agg_result_type(
            eng,
            snap,
            own,
            schemas,
            &[],
            *func,
            arg.as_deref(),
            arg2.as_deref(),
        )
        .ok(),
        Expr::ScalarSub(sub) => {
            let cols = describe_select(eng, snap, own, sub, &[]).ok()?;
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
        Expr::Arith { left, right, .. } => {
            infer_expr(left, eng, snap, own, schemas, out)?;
            infer_expr(right, eng, snap, own, schemas, out)?;
            // One side a param, the other side typed: pin the param.
            for (p_side, o_side) in [(left, right), (right, left)] {
                if let Expr::Param(p) = **p_side {
                    if let Some(t) = hint_type(eng, snap, own, schemas, o_side) {
                        pin_param(out, p, t)?;
                    }
                }
            }
            // Both sides params with no other info: default to integer
            // (documented v0.2 inference rule for `$1 + $2`).
            if matches!(**left, Expr::Param(_)) && matches!(**right, Expr::Param(_)) {
                for p_side in [left, right] {
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
                if let Ok(cols) = describe_select(eng, snap, own, sub, &[]) {
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
        // v0.14: VALUES rows are uncorrelated constants.
        FromItem::Values { rows, .. } => {
            for row in rows {
                for e in row {
                    infer_expr(e, eng, snap, own, &[], out)?;
                }
            }
            Ok(())
        }
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
    // v0.10: this level's own CTEs are visible to the FROM clause.
    let visible: Vec<CteDef> = s.with.clone();
    let own_schemas = from_schemas(eng, snap, own, &s.from, &visible, &[]).unwrap_or_default();
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
        with,
        on_conflict,
        returning,
        ..
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
        // v0.10: CTE bodies, ON CONFLICT expressions and RETURNING list.
        infer_ctes(with, eng, snap, own, &mut out);
        infer_on_conflict(on_conflict, table, eng, snap, own, &mut out);
        infer_returning(returning, table, eng, snap, own, &mut out);
    }
    if let Stmt::Update {
        table,
        sets,
        where_,
        with,
        returning,
        ..
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
        // v0.10: CTE bodies and RETURNING list.
        infer_ctes(with, eng, snap, own, &mut out);
        infer_returning(returning, table, eng, snap, own, &mut out);
    }
    match stmt {
        Stmt::Select(sel) => {
            infer_select(sel, eng, snap, own, &[], &mut out)?;
        }
        Stmt::Delete {
            table,
            where_,
            with,
            returning,
            ..
        } => {
            let tbl = infer_table(eng, &Some(table.clone()), snap, own);
            infer_where(where_, tbl, &mut out)?;
            // v0.10: CTE bodies and RETURNING list.
            infer_ctes(with, eng, snap, own, &mut out);
            infer_returning(returning, table, eng, snap, own, &mut out);
        }
        _ => {}
    }
    Ok(out)
}

/// v0.10: best-effort parameter inference inside CTE bodies. Failures are
/// swallowed (params stay unpinned and default to text) because bodies can
/// reference sibling CTEs that only fully resolve at execution time.
fn infer_ctes(
    ctes: &[CteDef],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    out: &mut [Option<ColType>],
) {
    for cte in ctes {
        let body: &SelectStmt = match &cte.body {
            CteBody::Simple(s) => s,
            CteBody::Union { left, .. } => left,
        };
        let _ = infer_select(body, eng, snap, own, &[], out);
    }
}

/// v0.10: best-effort inference for a RETURNING list against the target
/// table's columns.
fn infer_returning(
    returning: &[SelectItem],
    table: &str,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    out: &mut [Option<ColType>],
) {
    if returning.is_empty() {
        return;
    }
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
        for item in returning {
            if let SelectItem::Expr { expr, .. } = item {
                let _ = infer_expr(expr, eng, snap, own, &refs, out);
            }
        }
    }
}

/// v0.10: best-effort inference for ON CONFLICT DO UPDATE expressions
/// against the target table's columns.
fn infer_on_conflict(
    on_conflict: &Option<OnConflict>,
    table: &str,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    out: &mut [Option<ColType>],
) {
    if let Some(OnConflict {
        action: ConflictAction::DoUpdate { sets, .. },
        ..
    }) = on_conflict
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
            for (_, expr) in sets {
                let _ = infer_expr(expr, eng, snap, own, &refs, out);
            }
        }
    }
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
                .parse::<i32>()
                .map(|w| Value::Int(w as i64))
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::SmallInt => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            s.trim()
                .parse::<i16>()
                .map(Value::SmallInt)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::BigInt => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            s.trim()
                .parse::<i64>()
                .map(Value::BigInt)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Float => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            s.trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Float4 => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            s.trim()
                .parse::<f32>()
                .map(Value::Float4)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Numeric => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            crate::storage::Numeric::parse(s.trim())
                .map(Value::Numeric)
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
        ColType::Date => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            crate::datetime::parse_date(s.trim())
                .map(Value::Date)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Timestamp => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            crate::datetime::parse_timestamp(s.trim())
                .map(Value::Timestamp)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Timestamptz => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            crate::datetime::parse_timestamptz(s.trim())
                .map(Value::Timestamptz)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Bytea => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            crate::storage::parse_bytea(s.trim())
                .map(Value::Bytea)
                .map_err(|_| bad(format!("\"{}\"", s)))
        }
        ColType::Uuid => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            crate::storage::parse_uuid(s.trim())
                .map(Value::Uuid)
                .map_err(|_| bad(format!("\"{}\"", s)))
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
        Stmt::Insert {
            rows,
            select,
            with,
            on_conflict,
            returning,
            ..
        } => {
            subst_ctes(with, params)?;
            for row in rows {
                for v in row {
                    if let InsertValue::Param(p) = v {
                        *v = InsertValue::Lit(param_literal(*p, params)?);
                    }
                }
            }
            if let Some(sel) = select {
                subst_select(sel, params)?;
            }
            subst_on_conflict(on_conflict, params)?;
            subst_returning(returning, params)?;
            Ok(())
        }
        Stmt::Select(sel) => subst_select(sel, params),
        Stmt::Update {
            sets,
            where_,
            with,
            returning,
            ..
        } => {
            subst_ctes(with, params)?;
            for (_, e) in sets {
                subst_expr(e, params)?;
            }
            subst_where(where_, params)?;
            subst_returning(returning, params)
        }
        Stmt::Delete {
            where_,
            with,
            returning,
            ..
        } => {
            subst_ctes(with, params)?;
            subst_where(where_, params)?;
            subst_returning(returning, params)
        }
        _ => Ok(()),
    }
}

/// v0.10: substitute parameters inside every CTE body.
fn subst_ctes(ctes: &mut [CteDef], params: &[Option<Value>]) -> Result<(), ExecError> {
    for cte in ctes {
        match &mut cte.body {
            CteBody::Simple(s) => subst_select(s, params)?,
            CteBody::Union { left, right, .. } => {
                subst_select(left, params)?;
                subst_select(right, params)?;
            }
        }
    }
    Ok(())
}

/// v0.10: substitute parameters inside a RETURNING list.
fn subst_returning(
    returning: &mut [SelectItem],
    params: &[Option<Value>],
) -> Result<(), ExecError> {
    for item in returning {
        if let SelectItem::Expr { expr, .. } = item {
            subst_expr(expr, params)?;
        }
    }
    Ok(())
}

/// v0.10: substitute parameters inside ON CONFLICT.
fn subst_on_conflict(
    on_conflict: &mut Option<OnConflict>,
    params: &[Option<Value>],
) -> Result<(), ExecError> {
    if let Some(OnConflict {
        action: ConflictAction::DoUpdate { sets, where_ },
        ..
    }) = on_conflict
    {
        for (_, e) in sets {
            subst_expr(e, params)?;
        }
        if let Some(w) = where_ {
            subst_expr(w, params)?;
        }
    }
    Ok(())
}

fn subst_select(s: &mut SelectStmt, params: &[Option<Value>]) -> Result<(), ExecError> {
    // v0.10: CTE bodies.
    subst_ctes(&mut s.with, params)?;
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
        FromItem::Values { rows, .. } => {
            for row in rows {
                for e in row {
                    subst_expr(e, params)?;
                }
            }
            Ok(())
        }
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
        Some(Value::SmallInt(i)) => Literal::SmallInt(*i),
        Some(Value::Int(i)) => Literal::Int(*i),
        Some(Value::BigInt(i)) => Literal::BigInt(*i),
        Some(Value::Float4(f)) => Literal::Real(*f),
        Some(Value::Float(f)) => Literal::Float(*f),
        Some(Value::Numeric(n)) => Literal::Numeric(n.clone()),
        Some(Value::Text(s)) => Literal::Text(s.clone()),
        Some(Value::Bool(b)) => Literal::Bool(*b),
        Some(Value::Date(d)) => Literal::Date(*d),
        Some(Value::Timestamp(m)) => Literal::Timestamp(*m),
        Some(Value::Timestamptz(m)) => Literal::Timestamptz(*m),
        Some(Value::Bytea(b)) => Literal::Bytea(b.clone()),
        Some(Value::Uuid(u)) => Literal::Uuid(*u),
        Some(Value::Null) => Literal::Null,
    })
}

fn subst_expr(e: &mut Expr, params: &[Option<Value>]) -> Result<(), ExecError> {
    match e {
        Expr::Param(p) => {
            *e = Expr::Literal(param_literal(*p, params)?);
        }
        Expr::Arith { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Concat(left, right) => {
            subst_expr(left, params)?;
            subst_expr(right, params)?;
        }
        Expr::Cmp { left, right, .. } => {
            subst_expr(left, params)?;
            subst_expr(right, params)?;
        }
        Expr::Like { expr, pattern, .. } => {
            subst_expr(expr, params)?;
            subst_expr(pattern, params)?;
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            subst_expr(expr, params)?;
            subst_expr(low, params)?;
            subst_expr(high, params)?;
        }
        Expr::Not(x) | Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => {
            subst_expr(x, params)?
        }
        Expr::Cast { expr, .. } => subst_expr(expr, params)?,
        Expr::Func { args, .. } => {
            for a in args {
                subst_expr(a, params)?;
            }
        }
        Expr::Extract { from, .. } => subst_expr(from, params)?,
        Expr::Agg { arg, arg2, .. } => {
            if let Some(a) = arg {
                subst_expr(a, params)?;
            }
            if let Some(a) = arg2 {
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
        ColType::SmallInt => Value::SmallInt(0),
        ColType::Int => Value::Int(0),
        ColType::BigInt => Value::BigInt(0),
        ColType::Float4 => Value::Float4(0.0),
        ColType::Float => Value::Float(0.0),
        ColType::Numeric => Value::Numeric(Numeric::zero()),
        ColType::Text => Value::Text(String::new()),
        ColType::Bool => Value::Bool(false),
        ColType::Date => Value::Date(0),
        ColType::Timestamp => Value::Timestamp(0),
        ColType::Timestamptz => Value::Timestamptz(0),
        ColType::Bytea => Value::Bytea(Vec::new()),
        ColType::Uuid => Value::Uuid([0; 16]),
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
            Ok(Some(describe_select(eng, snap, own, &s, &[])?))
        }
        // v0.10: DML with RETURNING describes the RETURNING list; without
        // it there are no result columns (like a plain command tag).
        Stmt::Insert {
            table, returning, ..
        }
        | Stmt::Update {
            table, returning, ..
        }
        | Stmt::Delete {
            table, returning, ..
        } => {
            if returning.is_empty() {
                Ok(None)
            } else {
                Ok(Some(describe_returning(eng, snap, own, table, returning)?))
            }
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
            session: 0,
            role: "postgres",
            read_only: false,
        };
        execute(eng, &mut ctx, &stmt)
    }

    fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
        match r {
            ExecResult::Select { rows, .. } | ExecResult::Explain { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                        .collect()
                })
                .collect(),
            ExecResult::Command { tag } => vec![vec![tag]],
            ExecResult::Dml { tag, .. } => vec![vec![tag]],
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
            session: 0,
            role: "postgres",
            read_only: false,
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
            session: 0,
            role: "postgres",
            read_only: false,
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
            ExecResult::Dml { tag, .. } => assert_eq!(tag, "UPDATE 1"),
            _ => panic!("expected command"),
        }
        let rows = rows_of(run(&mut eng, "SELECT name FROM users WHERE id = 1").unwrap());
        assert_eq!(rows, vec![vec!["ann2".to_string()]]);
    }

    // v0.16: new string/numeric built-ins.
    #[test]
    fn v16_string_and_numeric_functions() {
        let mut eng = engine();
        let one = |eng: &mut Engine, sql: &str| -> String {
            rows_of(run(eng, sql).unwrap())[0][0].clone()
        };
        // substr: 1-based, Unicode-aware.
        assert_eq!(one(&mut eng, "SELECT substr('hello', 2, 3)"), "ell");
        assert_eq!(one(&mut eng, "SELECT substr('héllo', 2, 3)"), "éll");
        assert_eq!(one(&mut eng, "SELECT substr('hello', -2, 4)"), "h");
        assert_eq!(one(&mut eng, "SELECT substr('hello', 99)"), "");
        // concat / concat_ws: NULL handling.
        assert_eq!(one(&mut eng, "SELECT concat('a', NULL, 1, true)"), "a1t");
        assert_eq!(
            one(&mut eng, "SELECT concat_ws(',', 'a', NULL, 'b')"),
            "a,b"
        );
        assert_eq!(one(&mut eng, "SELECT concat_ws(',', 'a', 'b')"), "a,b");
        assert_eq!(
            rows_of(run(&mut eng, "SELECT concat_ws(NULL, 'a')").unwrap())[0][0],
            "NULL"
        );
        // to_hex / to_oct / to_bin: int4 width vs bigint width (PG parity).
        assert_eq!(one(&mut eng, "SELECT to_hex(-1234)"), "fffffb2e");
        assert_eq!(one(&mut eng, "SELECT to_hex(255)"), "ff");
        assert_eq!(one(&mut eng, "SELECT to_oct(-1234)"), "37777775456");
        assert_eq!(
            one(&mut eng, "SELECT to_bin(-1234)"),
            "11111111111111111111101100101110"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_hex(-1234::bigint)"),
            "fffffffffffffb2e"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_oct(-1234::bigint)"),
            "1777777777777777775456"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_bin(-1234::bigint)"),
            "1111111111111111111111111111111111111111111111111111101100101110"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_bin(9223372036854775807::bigint)"),
            "111111111111111111111111111111111111111111111111111111111111111"
        );
        // sign across int / numeric / float.
        assert_eq!(one(&mut eng, "SELECT sign(-5)"), "-1");
        assert_eq!(one(&mut eng, "SELECT sign(0)"), "0");
        assert_eq!(one(&mut eng, "SELECT sign(2.5)"), "1");
        assert_eq!(one(&mut eng, "SELECT sign(-2.5::float8)"), "-1");
        assert_eq!(one(&mut eng, "SELECT sign(0.0::numeric)"), "0");
        // left / right with negative and oversized counts.
        assert_eq!(one(&mut eng, "SELECT left('abcdef', 2)"), "ab");
        assert_eq!(one(&mut eng, "SELECT left('abcdef', -2)"), "abcd");
        assert_eq!(one(&mut eng, "SELECT left('abcdef', 99)"), "abcdef");
        assert_eq!(one(&mut eng, "SELECT right('abcdef', 2)"), "ef");
        assert_eq!(one(&mut eng, "SELECT right('abcdef', -2)"), "cdef");
        assert_eq!(one(&mut eng, "SELECT left('héllo', 2)"), "hé");
        // reverse, Unicode-aware.
        assert_eq!(one(&mut eng, "SELECT reverse('abc')"), "cba");
        assert_eq!(one(&mut eng, "SELECT reverse('héllo')"), "olléh");
        assert_eq!(
            rows_of(run(&mut eng, "SELECT reverse(NULL)").unwrap())[0][0],
            "NULL"
        );
    }

    // v0.17: date/time built-in batch.
    #[test]
    fn v17_datetime_functions() {
        let mut eng = engine();
        let one = |eng: &mut Engine, sql: &str| -> String {
            rows_of(run(eng, sql).unwrap())[0][0].clone()
        };
        let err_code =
            |eng: &mut Engine, sql: &str| -> &'static str { run(eng, sql).unwrap_err().code };
        // date_part: function form of extract, numeric result.
        assert_eq!(
            one(&mut eng, "SELECT date_part('year', DATE '2026-09-11')"),
            "2026"
        );
        assert_eq!(
            one(
                &mut eng,
                "SELECT date_part('month', TIMESTAMP '2026-09-11 13:25:01')"
            ),
            "9"
        );
        assert_eq!(
            one(
                &mut eng,
                "SELECT date_part('hour', TIMESTAMPTZ '2026-09-11 13:25:01+00')"
            ),
            "13"
        );
        assert_eq!(
            one(&mut eng, "SELECT date_part('dow', DATE '2026-09-11')"),
            "5"
        ); // Friday
        assert_eq!(
            one(&mut eng, "SELECT date_part('quarter', DATE '2026-09-11')"),
            "3"
        );
        assert_eq!(
            one(&mut eng, "SELECT date_part('epoch', DATE '1970-01-02')"),
            "86400"
        );
        assert_eq!(
            rows_of(run(&mut eng, "SELECT date_part('year', NULL::date)").unwrap())[0][0],
            "NULL"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT date_part('bogus', DATE '2026-09-11')"),
            "22023"
        );
        assert_eq!(err_code(&mut eng, "SELECT date_part('year', 42)"), "42883");
        assert_eq!(err_code(&mut eng, "SELECT date_part('year')"), "42883");
        // to_date with format strings.
        assert_eq!(
            one(&mut eng, "SELECT to_date('2026-01-15', 'YYYY-MM-DD')"),
            "2026-01-15"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_date('15/01/2026', 'DD/MM/YYYY')"),
            "2026-01-15"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_date('09-11-26', 'MM-DD-YY')"),
            "2026-09-11"
        );
        assert_eq!(
            rows_of(run(&mut eng, "SELECT to_date(NULL, 'YYYY-MM-DD')").unwrap())[0][0],
            "NULL"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT to_date('2026-13-01', 'YYYY-MM-DD')"),
            "22008"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT to_date('2026-02-30', 'YYYY-MM-DD')"),
            "22008"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT to_date('2026/01/15', 'YYYY-MM-DD')"),
            "22008"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT to_date('2026-01-15', 'YYYY-MM-QQ')"),
            "0A000"
        );
        // to_timestamp with format strings.
        assert_eq!(
            one(
                &mut eng,
                "SELECT to_timestamp('2026-09-11 13:25:01', 'YYYY-MM-DD HH24:MI:SS')"
            ),
            "2026-09-11 13:25:01"
        );
        assert_eq!(
            one(
                &mut eng,
                "SELECT to_timestamp('2026-09-11 01:25 PM', 'YYYY-MM-DD HH12:MI AM')"
            ),
            "2026-09-11 13:25:00"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_timestamp('2026-09-11', 'YYYY-MM-DD')"),
            "2026-09-11 00:00:00"
        );
        assert_eq!(
            err_code(
                &mut eng,
                "SELECT to_timestamp('2026-09-11 25:00', 'YYYY-MM-DD HH24:MI')"
            ),
            "22008"
        );
        // to_timestamp(float8): epoch seconds -> timestamptz.
        assert_eq!(
            one(&mut eng, "SELECT to_timestamp(0)"),
            "1970-01-01 00:00:00+00"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_timestamp(86400.5)"),
            "1970-01-02 00:00:00.5+00"
        );
        assert_eq!(
            one(&mut eng, "SELECT to_timestamp(-1)"),
            "1969-12-31 23:59:59+00"
        );
        assert_eq!(
            rows_of(run(&mut eng, "SELECT to_timestamp(NULL::float8)").unwrap())[0][0],
            "NULL"
        );
        assert_eq!(err_code(&mut eng, "SELECT to_timestamp('x')"), "42883");
        // to_char formatting.
        assert_eq!(
            one(&mut eng, "SELECT to_char(DATE '2026-09-11', 'YYYY/MM/DD')"),
            "2026/09/11"
        );
        assert_eq!(
            one(
                &mut eng,
                "SELECT to_char(TIMESTAMP '2026-09-11 13:25:01', 'DD-MM-YYYY HH24:MI:SS')"
            ),
            "11-09-2026 13:25:01"
        );
        assert_eq!(
            one(
                &mut eng,
                "SELECT to_char(TIMESTAMP '2026-09-11 13:25:01', 'HH12:MI AM')"
            ),
            "01:25 PM"
        );
        assert_eq!(
            one(
                &mut eng,
                "SELECT to_char(TIMESTAMPTZ '2026-09-11 13:25:01+00', 'YYYY-MM-DD')"
            ),
            "2026-09-11"
        );
        assert_eq!(
            rows_of(run(&mut eng, "SELECT to_char(NULL::date, 'YYYY')").unwrap())[0][0],
            "NULL"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT to_char(DATE '2026-09-11', 'QQ')"),
            "0A000"
        );
        assert_eq!(err_code(&mut eng, "SELECT to_char(42, 'YYYY')"), "42883");
        // make_date / make_timestamp.
        assert_eq!(one(&mut eng, "SELECT make_date(2026, 9, 11)"), "2026-09-11");
        assert_eq!(one(&mut eng, "SELECT make_date(2024, 2, 29)"), "2024-02-29");
        assert_eq!(err_code(&mut eng, "SELECT make_date(2026, 13, 1)"), "22008");
        assert_eq!(err_code(&mut eng, "SELECT make_date(2026, 2, 30)"), "22008");
        assert_eq!(err_code(&mut eng, "SELECT make_date(2023, 2, 29)"), "22008");
        assert_eq!(
            rows_of(run(&mut eng, "SELECT make_date(2026, NULL, 1)").unwrap())[0][0],
            "NULL"
        );
        assert_eq!(
            one(&mut eng, "SELECT make_timestamp(2026, 9, 11, 13, 25, 1.5)"),
            "2026-09-11 13:25:01.5"
        );
        assert_eq!(
            one(&mut eng, "SELECT make_timestamp(2026, 1, 1, 0, 0, 0)"),
            "2026-01-01 00:00:00"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT make_timestamp(2026, 1, 1, 24, 0, 0)"),
            "22008"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT make_timestamp(2026, 1, 1, 0, 0, 60)"),
            "22008"
        );
        assert_eq!(err_code(&mut eng, "SELECT make_time(1, 2, 3)"), "42883");
        // timezone(): UTC-only.
        assert_eq!(
            one(
                &mut eng,
                "SELECT timezone('UTC', TIMESTAMPTZ '2026-09-11 13:25:01+00')"
            ),
            "2026-09-11 13:25:01"
        );
        assert_eq!(
            one(
                &mut eng,
                "SELECT timezone('utc', TIMESTAMP '2026-09-11 13:25:01')"
            ),
            "2026-09-11 13:25:01+00"
        );
        assert_eq!(
            err_code(
                &mut eng,
                "SELECT timezone('America/Chicago', TIMESTAMPTZ '2026-09-11 13:25:01+00')"
            ),
            "0A000"
        );
        assert_eq!(
            err_code(&mut eng, "SELECT timezone('UTC', DATE '2026-09-11')"),
            "42883"
        );
        // clock/statement/transaction timestamps return timestamptz now.
        let ts = one(&mut eng, "SELECT clock_timestamp()");
        assert!(ts.len() >= 19, "clock_timestamp() returned {ts:?}");
        let ts2 = one(&mut eng, "SELECT statement_timestamp()");
        assert!(ts2.len() >= 19);
        let ts3 = one(&mut eng, "SELECT transaction_timestamp()");
        assert!(ts3.len() >= 19);
        assert_eq!(err_code(&mut eng, "SELECT clock_timestamp(1)"), "42883");
        // Deliberately unimplemented: 42883.
        assert_eq!(err_code(&mut eng, "SELECT age(now())"), "42883");
        assert_eq!(err_code(&mut eng, "SELECT make_interval(1)"), "42883");
        assert_eq!(
            err_code(&mut eng, "SELECT date_bin('1 day', now(), now())"),
            "42883"
        );
        assert_eq!(err_code(&mut eng, "SELECT justify_days(now())"), "42883");
    }

    // v0.16: cursor_window positioning semantics (Postgres rules).
    #[test]
    fn v16_cursor_window() {
        use crate::server::cursor_window_for_test;
        use crate::sql::FetchDir;
        // 4 rows; cursor starts before the first row (-1).
        assert_eq!(
            cursor_window_for_test(&FetchDir::Forward(Some(1)), -1, 4),
            (0, 1, 0)
        );
        assert_eq!(
            cursor_window_for_test(&FetchDir::Forward(Some(2)), 0, 4),
            (1, 3, 2)
        );
        assert_eq!(
            cursor_window_for_test(&FetchDir::Forward(None), 2, 4),
            (3, 4, 3)
        );
        // Past the end: empty, parked after the last row.
        assert_eq!(
            cursor_window_for_test(&FetchDir::Forward(Some(1)), 3, 4),
            (0, 0, 4)
        );
        // BACKWARD excludes the current row and lands on the first returned.
        assert_eq!(
            cursor_window_for_test(&FetchDir::Backward(Some(3)), 4, 4),
            (1, 4, 1)
        );
        // RELATIVE is relative to the current row (cursor on row 2).
        assert_eq!(
            cursor_window_for_test(&FetchDir::Relative(-1), 1, 4),
            (0, 1, 0)
        );
        assert_eq!(
            cursor_window_for_test(&FetchDir::Absolute(2), -1, 4),
            (1, 2, 1)
        );
        assert_eq!(
            cursor_window_for_test(&FetchDir::Absolute(-1), -1, 4),
            (3, 4, 3)
        );
        // ABSOLUTE 0: before the first row, no rows.
        assert_eq!(
            cursor_window_for_test(&FetchDir::Absolute(0), 2, 4),
            (0, 0, -1)
        );
        assert_eq!(cursor_window_for_test(&FetchDir::First, 2, 4), (0, 1, 0));
        assert_eq!(cursor_window_for_test(&FetchDir::Last, 2, 4), (3, 4, 3));
    }
}

// ============================================================================
// v0.9: sequences.
// ============================================================================

/// Resolve CREATE/ALTER SEQUENCE options to concrete parameters.
/// Postgres defaults: start 1, increment 1, minvalue 1, maxvalue 2^63-1,
/// no cycle — except descending sequences (increment < 0), which default
/// to start -1, minvalue -(2^63), maxvalue -1.
fn sequence_params(
    opts: &SequenceOpts,
    for_alter: bool,
) -> Result<(i64, i64, i64, i64, bool, Option<i64>), ExecError> {
    let bad = |m: &str| exec_err("22023", m.to_string());
    let increment = opts.increment.unwrap_or(1);
    if increment == 0 {
        return Err(bad("INCREMENT must not be zero"));
    }
    let descending = increment < 0;
    let (dfl_min, dfl_max, dfl_start) = if descending {
        (i64::MIN, -1, -1)
    } else {
        (1, i64::MAX, 1)
    };
    let min_value = opts.min_value.unwrap_or(dfl_min);
    let max_value = opts.max_value.unwrap_or(dfl_max);
    let start = opts.start.unwrap_or(dfl_start);
    let cycle = opts.cycle.unwrap_or(false);
    if min_value >= max_value {
        return Err(bad("MINVALUE must be less than MAXVALUE"));
    }
    if start < min_value || start > max_value {
        return Err(bad("START value out of bounds"));
    }
    if !for_alter {
        if let Some(r) = opts.restart {
            // CREATE SEQUENCE ... RESTART is a Postgres syntax error.
            let _ = r;
            return Err(exec_err(
                "42601",
                "syntax error: RESTART is not allowed in CREATE SEQUENCE".to_string(),
            ));
        }
    }
    let restart = match opts.restart {
        None => None,
        Some(v) if v == SequenceOpts::RESTART_SENTINEL => Some(start),
        Some(v) => {
            if v < min_value || v > max_value {
                return Err(bad("RESTART value out of bounds"));
            }
            Some(v)
        }
    };
    Ok((start, increment, min_value, max_value, cycle, restart))
}

fn exec_create_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_not_exists: bool,
    opts: &SequenceOpts,
) -> Result<ExecResult, ExecError> {
    if eng.db.find_sequence(name, ctx.snap, ctx.own).is_some() {
        if if_not_exists {
            return Ok(ExecResult::Command {
                tag: "CREATE SEQUENCE".to_string(),
            });
        }
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    let (start, increment, min_value, max_value, cycle, _) = sequence_params(opts, false)?;
    let mut seq = Sequence::new(
        name.to_string(),
        start,
        increment,
        min_value,
        max_value,
        cycle,
        ctx.own,
    );
    // v0.11: the creating role owns the sequence.
    seq.owner = ctx.role.to_string();
    eng.db
        .sequences
        .entry(name.to_string())
        .or_default()
        .push(seq);
    ctx.writes.push(WriteOp::CreateSequence {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: "CREATE SEQUENCE".to_string(),
    })
}

fn exec_alter_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    opts: &SequenceOpts,
) -> Result<ExecResult, ExecError> {
    // v0.11: only the owner (or a superuser) may alter a sequence.
    require_seq_owner(eng, ctx, name)?;
    let live = eng
        .db
        .find_sequence(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    // Merge: ALTER supplies only the options it changes.
    let merged = SequenceOpts {
        start: opts.start.or(Some(live.start)),
        increment: opts.increment.or(Some(live.increment)),
        min_value: opts.min_value.or(Some(live.min_value)),
        max_value: opts.max_value.or(Some(live.max_value)),
        cycle: opts.cycle.or(Some(live.cycle)),
        restart: opts.restart,
    };
    let (start, increment, min_value, max_value, cycle, restart) = sequence_params(&merged, true)?;
    let prev = live.clone();
    let versions = eng.db.sequences.get_mut(name).expect("visible above");
    let cur = versions
        .iter_mut()
        .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
        .expect("visible above");
    cur.dropped_xmax = ctx.own;
    let mut next = Sequence::new(
        name.to_string(),
        start,
        increment,
        min_value,
        max_value,
        cycle,
        ctx.own,
    );
    // ALTER SEQUENCE changes parameters, not the position: carry the
    // current value and is_called forward (Postgres behavior).
    next.current = prev.current;
    next.is_called = prev.is_called;
    // v0.11: ALTER SEQUENCE preserves owner and grants.
    next.owner = prev.owner.clone();
    next.acl = prev.acl.clone();
    if let Some(r) = restart {
        // RESTART is setval(r, true): the next nextval advances past r.
        next.current = Some(r);
        next.is_called = true;
        // A RESTART counts as an advance for WAL purposes.
        if !eng.seq_advanced.contains(&name.to_string()) {
            eng.seq_advanced.push(name.to_string());
        }
    }
    eng.db
        .sequences
        .get_mut(name)
        .expect("visible above")
        .push(next);
    ctx.writes.push(WriteOp::AlterSequence {
        name: name.to_string(),
        prev,
    });
    Ok(ExecResult::Command {
        tag: "ALTER SEQUENCE".to_string(),
    })
}

fn exec_drop_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        // v0.11: only the owner (or a superuser) may drop a sequence.
        require_seq_owner(eng, ctx, name)?;
        let live = eng.db.find_sequence(name, ctx.snap, ctx.own);
        match live {
            None if if_exists => continue,
            None => {
                return Err(exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", name),
                ));
            }
            Some(_) => {}
        }
        // Postgres refuses to drop a sequence owned by a column default
        // (dependent). v0.9 tracks the dependency: DEFAULT nextval('s').
        let mut dependent: Option<(String, String)> = None;
        for (tname, vs) in &eng.db.tables {
            let Some(t) = vs
                .iter()
                .find(|t| crate::storage::table_visible(t, ctx.snap, ctx.own))
            else {
                continue;
            };
            for (i, d) in t.defaults.iter().enumerate() {
                if matches!(d, Some(DefaultExpr::Nextval(s)) if s == name) {
                    dependent = Some((tname.clone(), t.columns[i].0.clone()));
                    break;
                }
            }
            if dependent.is_some() {
                break;
            }
        }
        if let Some((t, c)) = dependent {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop sequence {} because column {}.{} has a default depending on it",
                    name, t, c
                ),
            ));
        }
        let versions = eng.db.sequences.get_mut(name).expect("visible above");
        let cur = versions
            .iter_mut()
            .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
            .expect("visible above");
        let prev = cur.clone();
        cur.dropped_xmax = ctx.own;
        ctx.writes.push(WriteOp::DropSequence {
            name: name.clone(),
            seq: prev,
        });
    }
    Ok(ExecResult::Command {
        tag: "DROP SEQUENCE".to_string(),
    })
}

/// Advance a sequence, returning the new value. Non-transactional, like
/// Postgres: the advance survives abort (undo is a no-op) and is staged
/// for commit-time WAL logging via `eng.seq_advanced`.
pub fn seq_nextval(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    name: &str,
) -> Result<i64, ExecError> {
    // v0.11: nextval needs USAGE on the sequence.
    if let Some(s) = eng.db.find_sequence(name, snap, own) {
        let have = crate::storage::sequence_privs(&eng.db, role, s, snap, own);
        if have & crate::storage::PRIV_USAGE == 0 {
            return Err(exec_err(
                "42501",
                format!("permission denied for sequence \"{}\"", name),
            ));
        }
    }
    let cur = eng
        .db
        .find_sequence_mut(name, snap, own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    // Postgres semantics via is_called: a fresh sequence (or one reset by
    // setval(v,false)) returns its current value without advancing; once
    // called, each nextval advances by the increment.
    let next = match cur.current {
        None => cur.start,
        Some(v) if !cur.is_called => v,
        Some(v) => {
            let n = v.checked_add(cur.increment).ok_or_else(|| {
                exec_err(
                    "55000",
                    format!(
                        "nextval: reached {} value of sequence \"{}\"",
                        if cur.increment > 0 {
                            "maximum"
                        } else {
                            "minimum"
                        },
                        name
                    ),
                )
            })?;
            let over = if cur.increment > 0 {
                n > cur.max_value
            } else {
                n < cur.min_value
            };
            if over {
                if cur.cycle {
                    if cur.increment > 0 {
                        cur.min_value
                    } else {
                        cur.max_value
                    }
                } else {
                    return Err(exec_err(
                        "55000",
                        format!(
                            "nextval: reached {} value of sequence \"{}\" ({})",
                            if cur.increment > 0 {
                                "maximum"
                            } else {
                                "minimum"
                            },
                            name,
                            if cur.increment > 0 {
                                cur.max_value
                            } else {
                                cur.min_value
                            },
                        ),
                    ));
                }
            } else {
                n
            }
        }
    };
    cur.current = Some(next);
    cur.is_called = true;
    eng.seq_currval.insert((session, name.to_string()), next);
    if !eng.seq_advanced.contains(&name.to_string()) {
        eng.seq_advanced.push(name.to_string());
    }
    Ok(next)
}

pub fn seq_currval(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    name: &str,
) -> Result<i64, ExecError> {
    // v0.11: currval needs USAGE on the sequence.
    match eng.db.find_sequence(name, snap, own) {
        None => {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", name),
            ));
        }
        Some(s) => {
            let have = crate::storage::sequence_privs(&eng.db, role, s, snap, own);
            if have & crate::storage::PRIV_USAGE == 0 {
                return Err(exec_err(
                    "42501",
                    format!("permission denied for sequence \"{}\"", name),
                ));
            }
        }
    }
    eng.seq_currval
        .get(&(session, name.to_string()))
        .copied()
        .ok_or_else(|| {
            exec_err(
                "55000",
                format!(
                    "currval of sequence \"{}\" is not yet defined in this session",
                    name
                ),
            )
        })
}

pub fn seq_setval(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    name: &str,
    value: i64,
    is_called: bool,
) -> Result<i64, ExecError> {
    // v0.11: setval needs USAGE on the sequence.
    if let Some(s) = eng.db.find_sequence(name, snap, own) {
        let have = crate::storage::sequence_privs(&eng.db, role, s, snap, own);
        if have & crate::storage::PRIV_USAGE == 0 {
            return Err(exec_err(
                "42501",
                format!("permission denied for sequence \"{}\"", name),
            ));
        }
    }
    let cur = eng
        .db
        .find_sequence_mut(name, snap, own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    cur.current = Some(value);
    // setval(v, true): is_called=true, next nextval returns v + increment.
    // setval(v, false): is_called=false, next nextval returns v itself.
    cur.is_called = is_called;
    if is_called {
        eng.seq_currval.insert((session, name.to_string()), value);
    } else {
        eng.seq_currval.remove(&(session, name.to_string()));
    }
    if !eng.seq_advanced.contains(&name.to_string()) {
        eng.seq_advanced.push(name.to_string());
    }
    Ok(value)
}

/// Dispatch for the nextval/currval/setval SQL functions (called from
/// eval_func with pre-evaluated args).

// ============================================================================
// v0.11: roles and privileges
// ============================================================================

fn exec_create_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    login: bool,
    superuser: bool,
    password: &Option<String>,
    connlimit: Option<i32>,
    valid_until: &Option<String>,
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    if eng.db.find_role(name, ctx.snap, ctx.own).is_some() {
        return Err(exec_err(
            "42710",
            format!("role \"{}\" already exists", name),
        ));
    }
    if let Some(vu) = valid_until {
        crate::storage::check_valid_until(vu).map_err(|e| exec_err("22008", e))?;
    }
    let role = crate::storage::Role {
        name: name.to_string(),
        password: password
            .as_ref()
            .map(|pw| crate::crypto::verifier_for_password(pw)),
        can_login: login,
        superuser,
        connlimit: connlimit.unwrap_or(-1),
        memberships: Vec::new(),
        valid_until: valid_until.clone(),
        created_xmin: ctx.own,
        dropped_xmax: 0,
    };
    eng.db.roles.entry(name.to_string()).or_default().push(role);
    ctx.writes.push(WriteOp::CreateRole {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: "CREATE ROLE".to_string(),
    })
}

fn exec_alter_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    login: Option<bool>,
    superuser: Option<bool>,
    password: &Option<Option<String>>,
    connlimit: Option<i32>,
    valid_until: &Option<String>,
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    let live = eng
        .db
        .find_role(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42704", format!("role \"{}\" does not exist", name)))?
        .clone();
    // The bootstrap superuser cannot be de-superusered or renamed away
    // into uselessness: refuse to remove SUPERUSER from `postgres`.
    // (Documented deviation: real PostgreSQL lets you do this.)
    if live.name == "postgres" && superuser == Some(false) {
        return Err(exec_err(
            "42501",
            "permission denied: cannot remove superuser from role \"postgres\"".to_string(),
        ));
    }
    let prev = live.clone();
    let cur = eng
        .db
        .find_role_mut(name, ctx.snap, ctx.own)
        .expect("visible above");
    cur.dropped_xmax = ctx.own;
    let versions = eng.db.roles.get_mut(name).expect("visible above");
    let mut next = live;
    if let Some(l) = login {
        next.can_login = l;
    }
    if let Some(s) = superuser {
        next.superuser = s;
    }
    if let Some(pw) = password {
        next.password = pw.as_ref().map(|p| crate::crypto::verifier_for_password(p));
    }
    if let Some(c) = connlimit {
        next.connlimit = c;
    }
    if let Some(vu) = valid_until {
        crate::storage::check_valid_until(vu).map_err(|e| exec_err("22008", e))?;
        next.valid_until = crate::storage::normalize_valid_until(vu);
    }
    next.created_xmin = ctx.own;
    next.dropped_xmax = 0;
    versions.push(next);
    ctx.writes.push(WriteOp::AlterRole {
        name: name.to_string(),
        prev,
    });
    Ok(ExecResult::Command {
        tag: "ALTER ROLE".to_string(),
    })
}

/// Swap the live version of role `name` for a mutated copy, WAL-logged
/// as AlterRole. The caller must have verified the role exists and is
/// visible under (snap, own). Returns the previous live version.
fn alter_role_swap(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    mutate: impl FnOnce(&mut crate::storage::Role),
) {
    let prev = eng
        .db
        .find_role(name, ctx.snap, ctx.own)
        .expect("role visible")
        .clone();
    let cur = eng
        .db
        .find_role_mut(name, ctx.snap, ctx.own)
        .expect("role visible");
    cur.dropped_xmax = ctx.own;
    let versions = eng.db.roles.get_mut(name).expect("role visible");
    let mut next = prev.clone();
    mutate(&mut next);
    next.created_xmin = ctx.own;
    next.dropped_xmax = 0;
    versions.push(next);
    ctx.writes.push(WriteOp::AlterRole {
        name: name.to_string(),
        prev,
    });
}

/// Would `GRANT role TO grantee` create a membership cycle? True when
/// `grantee` is already (transitively) a member of `role`.
fn membership_would_cycle(
    db: &crate::storage::Database,
    role: &str,
    grantee: &str,
    snap: &crate::storage::Snapshot,
    own: u64,
) -> bool {
    let mut seen = vec![role.to_string()];
    let mut i = 0;
    while i < seen.len() {
        let n = seen[i].clone();
        i += 1;
        if n == grantee {
            return true;
        }
        if let Some(r) = db.find_role(&n, snap, own) {
            for m in &r.memberships {
                if !seen.iter().any(|x| x == &m.role) {
                    seen.push(m.role.clone());
                }
            }
        }
    }
    false
}

/// GRANT role [, ...] TO role [, ...]: role membership. Members inherit
/// the group's GRANTed privileges. Superuser-only (PostgreSQL also
/// allows ADMIN OPTION holders; rustgres has no CREATEROLE yet).
fn exec_grant_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    roles: &[String],
    grantees: &[String],
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    for grantee in grantees {
        if eng.db.find_role(grantee, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", grantee),
            ));
        }
        for role in roles {
            if eng.db.find_role(role, ctx.snap, ctx.own).is_none() {
                return Err(exec_err(
                    "42704",
                    format!("role \"{}\" does not exist", role),
                ));
            }
            if role == grantee {
                return Err(exec_err(
                    "42501",
                    "a role cannot be a member of itself".to_string(),
                ));
            }
            if membership_would_cycle(&eng.db, role, grantee, ctx.snap, ctx.own) {
                return Err(exec_err(
                    "42501",
                    format!(
                        "granting \"{}\" to \"{}\" would create a membership cycle",
                        role, grantee
                    ),
                ));
            }
            let already = eng
                .db
                .find_role(grantee, ctx.snap, ctx.own)
                .map(|r| r.memberships.iter().any(|m| &m.role == role))
                .unwrap_or(false);
            if already {
                continue;
            }
            let grantor = ctx.role.to_string();
            let role_name = role.clone();
            alter_role_swap(eng, ctx, grantee, move |r| {
                r.memberships.push(crate::storage::RoleMembership {
                    role: role_name.clone(),
                    grantor: grantor.clone(),
                });
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "GRANT".to_string(),
    })
}

/// REVOKE role [, ...] FROM role [, ...]: remove membership.
fn exec_revoke_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    roles: &[String],
    grantees: &[String],
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    for grantee in grantees {
        if eng.db.find_role(grantee, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", grantee),
            ));
        }
        for role in roles {
            if eng.db.find_role(role, ctx.snap, ctx.own).is_none() {
                return Err(exec_err(
                    "42704",
                    format!("role \"{}\" does not exist", role),
                ));
            }
            let has = eng
                .db
                .find_role(grantee, ctx.snap, ctx.own)
                .map(|r| r.memberships.iter().any(|m| &m.role == role))
                .unwrap_or(false);
            if !has {
                continue;
            }
            let role_name = role.clone();
            alter_role_swap(eng, ctx, grantee, move |r| {
                r.memberships.retain(|m| m.role != role_name);
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "REVOKE".to_string(),
    })
}

fn exec_drop_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    for name in names {
        let live = eng.db.find_role(name, ctx.snap, ctx.own).cloned();
        let Some(role) = live else {
            if if_exists {
                continue;
            }
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", name),
            ));
        };
        // The bootstrap superuser is the safety net: it cannot be dropped.
        if role.name == "postgres" {
            return Err(exec_err(
                "42501",
                "permission denied: cannot drop role \"postgres\"".to_string(),
            ));
        }
        // Like PostgreSQL (2BP01): refuse while the role still owns objects.
        let mut owned = Vec::new();
        for (tn, vs) in &eng.db.tables {
            if vs.iter().any(|t| {
                crate::storage::table_visible(t, ctx.snap, ctx.own) && t.owner == role.name
            }) {
                owned.push(format!("table {}", tn));
            }
        }
        for (vn, vs) in &eng.db.views {
            if vs
                .iter()
                .any(|v| crate::storage::view_visible(v, ctx.snap, ctx.own) && v.owner == role.name)
            {
                owned.push(format!("view {}", vn));
            }
        }
        for (sn, vs) in &eng.db.sequences {
            if vs
                .iter()
                .any(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own) && s.owner == role.name)
            {
                owned.push(format!("sequence {}", sn));
            }
        }
        if !owned.is_empty() {
            return Err(exec_err(
                "2BP01",
                format!(
                    "role \"{}\" cannot be dropped because it owns {}",
                    role.name,
                    owned.join(", ")
                ),
            ));
        }
        // Grants *to* the dropped role linger but are inert (documented
        // deviation from PostgreSQL, which drops them).
        let cur = eng
            .db
            .find_role_mut(name, ctx.snap, ctx.own)
            .expect("visible above");
        cur.dropped_xmax = ctx.own;
        ctx.writes.push(WriteOp::DropRole {
            name: name.to_string(),
            prev: role,
        });
        // Membership edges referencing the dropped role are removed from
        // every remaining role (PostgreSQL drops them from pg_auth_members).
        let affected: Vec<String> = eng
            .db
            .roles
            .iter()
            .filter(|(rn, vs)| {
                *rn != name
                    && vs.iter().any(|r| {
                        crate::storage::role_visible(r, ctx.snap, ctx.own)
                            && r.memberships.iter().any(|m| &m.role == name)
                    })
            })
            .map(|(rn, _)| rn.clone())
            .collect();
        for rn in affected {
            let dropped = name.clone();
            alter_role_swap(eng, ctx, &rn, move |r| {
                r.memberships.retain(|m| m.role != dropped);
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP ROLE".to_string(),
    })
}

/// Apply a GRANT or REVOKE (revoke = true). Only superusers and object
/// owners may grant; the grantee roles must exist.
#[allow(clippy::too_many_arguments)]
fn exec_grant_revoke(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    privs: &[crate::sql::PrivSpec],
    object: &crate::sql::GrantObject,
    grantees: &[String],
    revoke: bool,
) -> Result<ExecResult, ExecError> {
    use crate::sql::{GrantObject, PrivSpec, Privilege};
    // All grantees must be existing roles.
    for g in grantees {
        if eng.db.find_role(g, ctx.snap, ctx.own).is_none() {
            return Err(exec_err("42704", format!("role \"{}\" does not exist", g)));
        }
    }
    // Column lists are only meaningful on tables.
    if !matches!(object, GrantObject::Table(_)) && privs.iter().any(|s| !s.columns.is_empty()) {
        return Err(exec_err(
            "0A000",
            "column lists are only allowed on tables".to_string(),
        ));
    }
    // Collapse the whole-object privileges to a bitmask, validating
    // applicability; column-restricted specs are handled separately.
    let mut bits = 0u32;
    let mut col_specs: Vec<&PrivSpec> = Vec::new();
    for s in privs {
        let p = s.priv_;
        if !s.columns.is_empty() {
            col_specs.push(s);
            continue;
        }
        match (p, object) {
            (Privilege::All, GrantObject::Table(_)) => bits |= crate::storage::PRIV_ALL_TABLE,
            (Privilege::All, GrantObject::Sequence(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::All, GrantObject::Database) => bits |= crate::storage::PRIV_CONNECT,
            (Privilege::Usage, GrantObject::Sequence(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::Connect, GrantObject::Database) => bits |= crate::storage::PRIV_CONNECT,
            (
                Privilege::Select
                | Privilege::Insert
                | Privilege::Update
                | Privilege::Delete
                | Privilege::Truncate
                | Privilege::References
                | Privilege::Trigger,
                GrantObject::Table(_),
            ) => bits |= p.bits(),
            _ => {
                return Err(exec_err(
                    "0A000",
                    format!("invalid privilege {:?} for {:?}", p, object),
                ));
            }
        }
    }
    let verb = if revoke { "REVOKE" } else { "GRANT" };
    match object {
        GrantObject::Table(name) => {
            let t = eng.db.find_table(name, ctx.snap, ctx.own).ok_or_else(|| {
                exec_err("42P01", format!("relation \"{}\" does not exist", name))
            })?;
            if t.owner != ctx.role
                && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            {
                return Err(exec_err(
                    "42501",
                    format!(
                        "permission denied: must be owner of table \"{}\" to {}",
                        name,
                        verb.to_lowercase()
                    ),
                ));
            }
            let mut next = t.clone();
            apply_acl_delta(&mut next.acl, grantees, bits, revoke, false);
            // v0.11: column-level grants.
            if !col_specs.is_empty() {
                let cols: Vec<String> = next.columns.iter().map(|(n, _)| n.clone()).collect();
                for s in &col_specs {
                    match (s.priv_, object) {
                        (
                            Privilege::Select
                            | Privilege::Insert
                            | Privilege::Update
                            | Privilege::References,
                            GrantObject::Table(_),
                        ) => {}
                        _ => {
                            return Err(exec_err(
                                "0A000",
                                format!("invalid privilege {:?} for {:?}", s.priv_, object),
                            ));
                        }
                    }
                    for c in &s.columns {
                        if !cols.iter().any(|n| n == c) {
                            return Err(exec_err(
                                "42703",
                                format!("column \"{}\" of relation \"{}\" does not exist", c, name),
                            ));
                        }
                    }
                    apply_col_acl_delta(
                        &mut next.col_acl,
                        grantees,
                        s.priv_.bits(),
                        &s.columns,
                        revoke,
                    );
                }
            }
            alter_swap(eng, ctx, name, None, next, None)?;
        }
        GrantObject::Sequence(name) => {
            let s = eng
                .db
                .find_sequence(name, ctx.snap, ctx.own)
                .ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", name))
                })?;
            if s.owner != ctx.role
                && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            {
                return Err(exec_err(
                    "42501",
                    format!(
                        "permission denied: must be owner of sequence \"{}\" to {}",
                        name,
                        verb.to_lowercase()
                    ),
                ));
            }
            // Version-swap like ALTER SEQUENCE so the grant is transactional.
            let prev = s.clone();
            let versions = eng.db.sequences.get_mut(name).expect("visible above");
            let cur = versions
                .iter_mut()
                .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
                .expect("visible above");
            cur.dropped_xmax = ctx.own;
            let mut next = prev.clone();
            apply_acl_delta(&mut next.acl, grantees, bits, revoke, false);
            next.created_xmin = ctx.own;
            next.dropped_xmax = 0;
            versions.push(next);
            ctx.writes.push(WriteOp::AlterSequence {
                name: name.to_string(),
                prev,
            });
        }
        GrantObject::Database => {
            require_superuser(eng, ctx)?;
            let prev = eng.db.db_acl.clone();
            apply_acl_delta(&mut eng.db.db_acl, grantees, bits, revoke, true);
            ctx.writes.push(WriteOp::DbAcl { prev });
        }
    }
    Ok(ExecResult::Command {
        tag: verb.to_string(),
    })
}

/// Merge grant/revoke bits into an ACL vector.
///
/// When `keep_zero` is false, entries left with no privileges are pruned
/// (fine for tables/sequences, whose default is deny). When true, zero-priv
/// entries are kept as explicit deny markers (needed for the database ACL,
/// whose default is allow: `db_connect_allowed` treats an entry without the
/// CONNECT bit as a revoked role).
fn apply_acl_delta(
    acl: &mut Vec<crate::storage::AclEntry>,
    grantees: &[String],
    bits: u32,
    revoke: bool,
    keep_zero: bool,
) {
    for g in grantees {
        if let Some(e) = acl.iter_mut().find(|e| e.role == *g) {
            if revoke {
                e.privs &= !bits;
            } else {
                e.privs |= bits;
            }
        } else {
            // No entry yet. A grant creates one; a revoke creates an
            // explicit deny marker only when keep_zero (database ACL).
            // Otherwise revoking a never-granted privilege is a no-op,
            // like PostgreSQL's warning (we stay silent).
            if !revoke || keep_zero {
                acl.push(crate::storage::AclEntry {
                    role: g.clone(),
                    privs: if revoke { 0 } else { bits },
                });
            }
        }
    }
    if !keep_zero {
        acl.retain(|e| e.privs != 0);
    }
}

/// Merge column grant/revoke bits into a table's `col_acl` vector.
/// Entries are keyed by (role, column-set): a grant unions the column
/// list, a revoke removes the columns (dropping the entry when its
/// column list empties out).
fn apply_col_acl_delta(
    col_acl: &mut Vec<crate::storage::ColAclEntry>,
    grantees: &[String],
    bits: u32,
    columns: &[String],
    revoke: bool,
) {
    for g in grantees {
        if revoke {
            // Remove the columns from every entry for this role whose
            // privs overlap; drop emptied entries.
            for e in col_acl.iter_mut().filter(|e| e.role == *g) {
                if e.privs & bits != 0 {
                    e.columns.retain(|c| !columns.contains(c));
                    if e.columns.is_empty() {
                        e.privs = 0;
                    } else {
                        // Partial column revoke: keep the entry but drop
                        // the revoked bits only when no columns remain
                        // for them. Simpler and safe: clear the bits —
                        // remaining columns keep other granted bits.
                        // (We track one privs mask per entry; revoking
                        // one privilege's columns while another's remain
                        // is approximated by keeping the entry.)
                    }
                }
            }
            col_acl.retain(|e| e.privs != 0 && !e.columns.is_empty());
        } else if let Some(e) = col_acl.iter_mut().find(|e| e.role == *g && e.privs == bits) {
            for c in columns {
                if !e.columns.contains(c) {
                    e.columns.push(c.clone());
                }
            }
        } else {
            col_acl.push(crate::storage::ColAclEntry {
                role: g.clone(),
                privs: bits,
                columns: columns.to_vec(),
            });
        }
    }
}

/// ALTER TABLE name OWNER TO new_owner.
fn alter_owner_to(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    new_owner: &str,
) -> Result<ExecResult, ExecError> {
    require_table_owner(eng, ctx, name)?;
    if eng.db.find_role(new_owner, ctx.snap, ctx.own).is_none() {
        return Err(exec_err(
            "42704",
            format!("role \"{}\" does not exist", new_owner),
        ));
    }
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let mut next = t;
    next.owner = new_owner.to_string();
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

fn eval_sequence_func(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    read_only: bool,
    name: &str,
    vals: &[Value],
) -> Result<Value, ExecError> {
    let seq_name = |v: &Value| -> Result<String, ExecError> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            // regclass input: Postgres accepts nextval('seq').
            _ => Err(exec_err(
                "42883",
                format!("function {}: sequence name must be text", name),
            )),
        }
    };
    match name {
        "nextval" => {
            // v0.17: advancing a sequence is a write — blocked read-only.
            if read_only {
                return Err(exec_err(
                    "25006",
                    "cannot execute nextval() in a read-only transaction".to_string(),
                ));
            }
            let s = seq_name(&vals[0])?;
            Ok(Value::BigInt(seq_nextval(
                eng, snap, own, session, role, &s,
            )?))
        }
        "currval" => {
            let s = seq_name(&vals[0])?;
            Ok(Value::BigInt(seq_currval(
                eng, snap, own, session, role, &s,
            )?))
        }
        "setval" => {
            // v0.17: like nextval, setting a sequence is a write.
            if read_only {
                return Err(exec_err(
                    "25006",
                    "cannot execute setval() in a read-only transaction".to_string(),
                ));
            }
            let s = seq_name(&vals[0])?;
            let v = match vals[1] {
                Value::Int(i) => i as i64,
                Value::BigInt(i) => i,
                Value::SmallInt(i) => i as i64,
                _ => {
                    return Err(exec_err(
                        "42883",
                        "setval: value must be an integer".to_string(),
                    ));
                }
            };
            let is_called = match vals.get(2) {
                None => true,
                Some(Value::Bool(b)) => *b,
                _ => {
                    return Err(exec_err(
                        "42883",
                        "setval: is_called must be boolean".to_string(),
                    ));
                }
            };
            Ok(Value::BigInt(seq_setval(
                eng, snap, own, session, role, &s, v, is_called,
            )?))
        }
        _ => unreachable!(),
    }
}

// ============================================================================
// v0.9: ALTER TABLE.
// ============================================================================

/// Swap in an altered table version: mark the live version dropped by us,
/// push the new one, and log WriteOp::AlterTable for undo/WAL.
/// `new_rows`: Some for ADD/DROP COLUMN (rows rewritten with new ids, WAL-
/// logged as InsertRow); None for pure-metadata alters (rows move over).
fn alter_swap(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    renamed_to: Option<String>,
    mut next: Table,
    new_rows: Option<Vec<RowVersion>>,
) -> Result<(), ExecError> {
    let prev = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    next.created_xmin = ctx.own;
    next.dropped_xmax = 0;
    let rewrite = new_rows.is_some();
    let mut log_ids = Vec::new();
    if let Some(rows) = new_rows {
        log_ids.extend(rows.iter().map(|r| r.id));
        next.rows = rows;
        // v0.9: the row ids changed — rebuild the id->position index.
        next.rebuild_row_index();
    }
    let target = renamed_to.clone().unwrap_or_else(|| name.to_string());
    if renamed_to.is_some() {
        let mut versions = eng.db.tables.remove(name).unwrap_or_default();
        {
            let old_live = versions
                .iter_mut()
                .find(|t| crate::storage::table_visible(t, ctx.snap, ctx.own))
                .expect("visible above");
            old_live.dropped_xmax = ctx.own;
            if !rewrite {
                next.rows = std::mem::take(&mut old_live.rows);
                // v0.9: the old version's rows were moved; its index is stale.
                old_live.rebuild_row_index();
                // v0.9: next got the old rows (same ids/order); its cloned
                // index is still valid, but rebuild to be safe.
                next.rebuild_row_index();
            }
        }
        versions.push(next);
        eng.db.tables.insert(target.clone(), versions);
    } else {
        let versions = eng.db.tables.get_mut(name).expect("visible above");
        let old_live = versions
            .iter_mut()
            .find(|t| crate::storage::table_visible(t, ctx.snap, ctx.own))
            .expect("visible above");
        old_live.dropped_xmax = ctx.own;
        if !rewrite {
            next.rows = std::mem::take(&mut old_live.rows);
            // v0.9: the old version's rows were moved; its index is stale.
            old_live.rebuild_row_index();
            // v0.9: next got the old rows (same ids/order); its cloned
            // index is still valid, but rebuild to be safe.
            next.rebuild_row_index();
        }
        versions.push(next);
    }
    ctx.writes.push(WriteOp::AlterTable {
        name: name.to_string(),
        prev,
        renamed_to,
        rewrite_rows: rewrite,
    });
    for id in log_ids {
        ctx.writes.push(WriteOp::InsertRow {
            table: target.clone(),
            row_id: id,
        });
    }
    Ok(())
}

/// Rename column references inside a CHECK/DEFAULT expression (for
/// ALTER TABLE ... RENAME COLUMN). Constraint expressions are simple
/// (no subqueries/aggregates), so a shallow walk suffices.
fn rename_col_in_expr(e: &mut Expr, old: &str, new: &str) {
    match e {
        Expr::Column { table, name } => {
            if table.is_none() && name == old {
                *name = new.to_string();
            }
        }
        Expr::Arith { left, right, .. } => {
            rename_col_in_expr(left, old, new);
            rename_col_in_expr(right, old, new);
        }
        Expr::Cast { expr, .. } => rename_col_in_expr(expr, old, new),
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            rename_col_in_expr(a, old, new);
            rename_col_in_expr(b, old, new);
        }
        Expr::Not(a) => rename_col_in_expr(a, old, new),
        Expr::Like { expr, pattern, .. } => {
            rename_col_in_expr(expr, old, new);
            rename_col_in_expr(pattern, old, new);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            rename_col_in_expr(expr, old, new);
            rename_col_in_expr(low, old, new);
            rename_col_in_expr(high, old, new);
        }
        Expr::IsBool { expr, .. } | Expr::IsNull { expr, .. } => rename_col_in_expr(expr, old, new),
        Expr::Extract { from, .. } => rename_col_in_expr(from, old, new),
        Expr::Cmp { left, right, .. } => {
            rename_col_in_expr(left, old, new);
            rename_col_in_expr(right, old, new);
        }
        Expr::Func { args, .. } => {
            for a in args {
                rename_col_in_expr(a, old, new);
            }
        }
        Expr::Literal(_) | Expr::Param(_) | Expr::ResolvedCol { .. } | Expr::Agg { .. } => {}
        Expr::ScalarSub(_) | Expr::InSub { .. } | Expr::Exists { .. } => {}
        // v0.10: windows cannot appear in constraints; nothing to rename.
        Expr::Window { .. } => {}
    }
}

fn exec_alter(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    action: &AlterAction,
) -> Result<ExecResult, ExecError> {
    // v0.11: every ALTER TABLE needs owner-or-superuser.
    require_table_owner(eng, ctx, name)?;
    match action {
        AlterAction::AddColumn {
            name: col,
            col_type,
            not_null,
            default,
            checks,
            uniques,
            pkey,
            fks,
        } => alter_add_column(
            eng, ctx, name, col, col_type, *not_null, default, checks, uniques, pkey, fks,
        ),
        AlterAction::DropColumn { name: col, cascade } => {
            alter_drop_column(eng, ctx, name, col, *cascade)
        }
        AlterAction::AddConstraint {
            check,
            unique,
            pkey,
            fk,
        } => alter_add_constraint(eng, ctx, name, check, unique, pkey, fk),
        AlterAction::DropConstraint { name: con, cascade } => {
            alter_drop_constraint(eng, ctx, name, con, *cascade)
        }
        AlterAction::AlterColumnSetDefault { name: col, default } => {
            alter_set_default(eng, ctx, name, col, Some(default.clone()))
        }
        AlterAction::AlterColumnDropDefault { name: col } => {
            alter_set_default(eng, ctx, name, col, None)
        }
        AlterAction::RenameColumn { old, new } => alter_rename_column(eng, ctx, name, old, new),
        AlterAction::RenameTo { new_name } => alter_rename_to(eng, ctx, name, new_name),
        // v0.11
        AlterAction::OwnerTo { new_owner } => alter_owner_to(eng, ctx, name, new_owner),
    }
}

#[allow(clippy::too_many_arguments)]
fn alter_add_column(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    col_type: &ColType,
    not_null: bool,
    default: &Option<DefaultExpr>,
    checks: &[CheckDef],
    uniques: &[UniqueDef],
    pkey: &Option<UniqueDef>,
    fks: &[FkDef],
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    if t.column_index(col).is_some() {
        return Err(exec_err(
            "42701",
            format!("column \"{}\" of relation \"{}\" already exists", col, name),
        ));
    }
    // Validate the default and check expressions up front.
    if let Some(d) = default {
        if let DefaultExpr::Expr(e) = d {
            validate_constraint_expr(e, "DEFAULT").map_err(sql_err)?;
        }
    }
    for c in checks {
        validate_constraint_expr(&c.expr, "CHECK").map_err(sql_err)?;
    }
    let has_rows = t.rows.iter().any(|r| row_visible(r, ctx.snap, ctx.own));
    if not_null && default.is_none() && has_rows {
        return Err(exec_err(
            "23502",
            format!("column \"{}\" contains null values", col),
        ));
    }
    let mut next = t.clone();
    next.columns.push((col.to_string(), col_type.clone()));
    next.not_null.push(not_null);
    next.defaults.push(default.clone());
    next.checks.extend(checks.iter().cloned());
    next.uniques.extend(uniques.iter().cloned());
    if let Some(pk) = pkey {
        if next.pkey.is_some() {
            return Err(exec_err(
                "42P16",
                "multiple primary keys for table".to_string(),
            ));
        }
        next.pkey = Some(pk.clone());
    }
    // Validate FKs against the new table shape.
    let next_meta = TableMeta::of(&next);
    for fk in fks {
        validate_fk_def(eng, ctx, name, fk)?;
    }
    next.fks.extend(fks.iter().cloned());
    // Rewrite every row with the new column value (default or NULL).
    // Reuse the old row ids so existing indexes stay valid; the WAL
    // replays them as InsertRows. Copy the visible rows out first; the
    // borrow of `t` must end before eval_default.
    let old_cols = t.columns.len();
    let old_rows: Vec<(u64, Vec<Value>)> = t
        .rows
        .iter()
        .filter(|r| row_visible(r, ctx.snap, ctx.own))
        .map(|r| (r.id, r.values.clone()))
        .collect();
    let mut new_rows = Vec::with_capacity(old_rows.len());
    for (old_id, values) in old_rows {
        let dv = match default {
            Some(d) => eval_default(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                d,
                col_type,
                col,
            )?,
            None => Value::Null,
        };
        let mut nv = Vec::with_capacity(old_cols + 1);
        nv.extend_from_slice(&values);
        // Defensive: tolerate rows narrower than the old schema (shouldn't happen).
        while nv.len() < old_cols {
            nv.push(Value::Null);
        }
        nv.push(dv);
        // New CHECK constraints must hold for existing rows.
        check_row_constraints(
            eng,
            ctx.snap,
            ctx.own,
            ctx.session,
            ctx.role,
            &next_meta,
            name,
            &nv,
        )?;
        new_rows.push(RowVersion {
            id: old_id,
            values: nv,
            xmin: ctx.own,
            xmax: 0,
        });
    }
    // Backing indexes for new unique/pkey constraints.
    let mut new_indexes: Vec<(String, Vec<String>)> = Vec::new();
    for u in uniques {
        new_indexes.push((u.name.clone(), u.cols.clone()));
    }
    if let Some(pk) = pkey {
        new_indexes.push((pk.name.clone(), pk.cols.clone()));
    }
    alter_swap(eng, ctx, name, None, next, Some(new_rows))?;
    for (ix_name, cols) in new_indexes {
        create_constraint_index(eng, ctx, name, &ix_name, &cols, true)?;
    }
    Ok(ExecResult::Command {
        tag: format!("ALTER TABLE"),
    })
}

fn alter_drop_column(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let ci = t.column_index(col).ok_or_else(|| {
        exec_err(
            "42703",
            format!("column \"{}\" of relation \"{}\" does not exist", col, name),
        )
    })?;
    // Dependency scan.
    let mut dep_constraints: Vec<String> = Vec::new();
    for c in &t.checks {
        let mut refs = Vec::new();
        collect_col_refs(&c.expr, &mut refs);
        if refs.iter().any(|(_, r)| r == col) {
            dep_constraints.push(format!("constraint \"{}\"", c.name));
        }
    }
    for u in &t.uniques {
        if u.cols.iter().any(|c| c == col) {
            dep_constraints.push(format!("constraint \"{}\"", u.name));
        }
    }
    if let Some(pk) = &t.pkey {
        if pk.cols.iter().any(|c| c == col) {
            dep_constraints.push(format!("constraint \"{}\"", pk.name));
        }
    }
    for fk in &t.fks {
        if fk.cols.iter().any(|c| c == col) {
            dep_constraints.push(format!("constraint \"{}\"", fk.name));
        }
    }
    // Other tables' FKs referencing this column.
    let mut dep_fks: Vec<(String, String)> = Vec::new();
    for (tname, vs) in &eng.db.tables {
        if tname == name {
            continue;
        }
        let Some(ot) = vs
            .iter()
            .find(|t| crate::storage::table_visible(t, ctx.snap, ctx.own))
        else {
            continue;
        };
        for fk in &ot.fks {
            if fk.ref_table != name {
                continue;
            }
            // Empty ref_cols = references our pkey.
            let ref_cols: Vec<String> = if fk.ref_cols.is_empty() {
                t.pkey.as_ref().map(|p| p.cols.clone()).unwrap_or_default()
            } else {
                fk.ref_cols.clone()
            };
            if ref_cols.iter().any(|c| c == col) {
                dep_fks.push((tname.clone(), fk.name.clone()));
            }
        }
    }
    // Indexes on the column.
    let mut dep_indexes: Vec<String> = Vec::new();
    for (ix_name, ix) in &eng.db.indexes {
        if ix.def.table != name {
            continue;
        }
        if ix.def.col_names.iter().any(|c| c == col) {
            dep_indexes.push(ix_name.clone());
        }
    }
    // Views reading the table.
    let mut dep_views: Vec<String> = Vec::new();
    for (vname, vs) in &eng.db.views {
        if vs.iter().any(|v| {
            crate::storage::view_visible(v, ctx.snap, ctx.own) && v.deps.iter().any(|d| d == name)
        }) {
            dep_views.push(vname.clone());
        }
    }
    if !cascade
        && (!dep_constraints.is_empty()
            || !dep_fks.is_empty()
            || !dep_indexes.is_empty()
            || !dep_views.is_empty())
    {
        let first = dep_constraints
            .first()
            .map(|s| s.as_str())
            .or(dep_fks.first().map(|(_, f)| f.as_str()))
            .or(dep_indexes.first().map(|s| s.as_str()))
            .or(dep_views.first().map(|s| s.as_str()))
            .unwrap_or("?");
        return Err(exec_err(
            "2BP01",
            format!(
                "cannot drop column {} of table {} because {} depends on it",
                col, name, first
            ),
        ));
    }
    // Snapshot the table state and end `t`'s borrow before CASCADE
    // mutations (they need `eng` mutably).
    let mut next = t.clone();
    let old_rows: Vec<(u64, Vec<Value>)> = t
        .rows
        .iter()
        .filter(|r| row_visible(r, ctx.snap, ctx.own))
        .map(|r| (r.id, r.values.clone()))
        .collect();
    let _ = t;
    // CASCADE: drop dependent objects.
    // Views first (they only read).
    for v in &dep_views {
        drop_view_internal(eng, ctx, v)?;
    }
    // This table's dependent constraints and their backing indexes.
    let drop_idx_for: Vec<String> = next
        .uniques
        .iter()
        .filter(|u| u.cols.iter().any(|c| c == col))
        .map(|u| u.name.clone())
        .chain(
            next.pkey
                .iter()
                .filter(|p| p.cols.iter().any(|c| c == col))
                .map(|p| p.name.clone()),
        )
        .collect();
    next.checks.retain(|c| {
        let mut refs = Vec::new();
        collect_col_refs(&c.expr, &mut refs);
        !refs.iter().any(|(_, r)| r == col)
    });
    next.uniques.retain(|u| !u.cols.iter().any(|c| c == col));
    if next
        .pkey
        .as_ref()
        .is_some_and(|p| p.cols.iter().any(|c| c == col))
    {
        next.pkey = None;
    }
    next.fks.retain(|fk| !fk.cols.iter().any(|c| c == col));
    for ix in drop_idx_for {
        // v0.9: DROP the backing index for the dropped UNIQUE/PK constraint.
        // Use direct map removal (bypasses MVCC visibility which may miss
        // the index due to snapshot timing).
        if let Some(index) = eng.db.indexes.remove(&ix) {
            ctx.writes.push(WriteOp::DropIndex {
                name: ix.clone(),
                index,
            });
        }
    }
    // Other tables' FKs referencing the column.
    for (tname, fk_name) in &dep_fks {
        alter_drop_constraint_internal(eng, ctx, tname, fk_name)?;
    }
    // Indexes on the column (non-constraint ones; constraint ones already dropped).
    for ix in &dep_indexes {
        if eng.db.indexes.contains_key(ix) {
            drop_index_internal(eng, ctx, ix)?;
        }
    }
    // Remaining indexes: shift positions after the dropped column.
    let mut index_defs: Vec<(String, Vec<usize>, Vec<String>)> = Vec::new();
    for (ix_name, ix) in &eng.db.indexes {
        if ix.def.table != name {
            continue;
        }
        index_defs.push((
            ix_name.clone(),
            ix.def.cols.clone(),
            ix.def.col_names.clone(),
        ));
    }
    // Now mutate the table shape.
    next.columns.remove(ci);
    next.not_null.remove(ci);
    next.defaults.remove(ci);
    // CHECK expressions reference columns by name; nothing to shift.
    // Rewrite rows without the column. Reuse old row ids so surviving
    // indexes stay valid.
    let mut new_rows = Vec::with_capacity(old_rows.len());
    for (old_id, mut values) in old_rows {
        if values.len() > ci {
            values.remove(ci);
        }
        new_rows.push(RowVersion {
            id: old_id,
            values,
            xmin: ctx.own,
            xmax: 0,
        });
    }
    alter_swap(eng, ctx, name, None, next, Some(new_rows))?;
    // Shift index positions: drop and re-create each surviving index with
    // positions adjusted, so undo (DropIndex+CreateIndex pair) and WAL
    // (new def logged) stay correct.
    for (ix_name, cols, col_names) in index_defs {
        let snapshot = eng.db.indexes.get(&ix_name).cloned().expect("found above");
        let unique = snapshot.def.unique;
        let internal = snapshot.def.internal;
        drop_index_internal(eng, ctx, &ix_name)?;
        let new_cols: Vec<usize> = cols
            .into_iter()
            .map(|p| if p > ci { p - 1 } else { p })
            .collect();
        let mut ix = Index::new(IndexDef {
            name: ix_name.clone(),
            table: name.to_string(),
            cols: new_cols,
            col_names,
            unique,
            internal,
            created_xmin: ctx.own,
            dropped_xmax: 0,
        });
        let t2 = eng
            .db
            .find_table(name, ctx.snap, ctx.own)
            .expect("just altered");
        for r in &t2.rows {
            let key = ix.key_for(&r.values);
            ix.insert(key, r.id);
        }
        eng.db.indexes.insert(ix_name.clone(), ix);
        ctx.writes.push(WriteOp::CreateIndex { name: ix_name });
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// Drop an index by name, logging WriteOp::DropIndex. Used by ALTER TABLE
/// CASCADE paths.
fn drop_index_internal(eng: &mut Engine, ctx: &mut StmtCtx, name: &str) -> Result<(), ExecError> {
    let snapshot = eng
        .db
        .find_index(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("index \"{}\" does not exist", name)))?
        .clone();
    eng.db
        .find_index_mut(name, ctx.snap, ctx.own)
        .expect("index still present")
        .def
        .dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropIndex {
        name: name.to_string(),
        index: snapshot,
    });
    Ok(())
}
/// True if a table already has a constraint with this name.
fn table_has_constraint(t: &Table, con: &str) -> bool {
    t.checks.iter().any(|c| c.name == con)
        || t.uniques.iter().any(|u| u.name == con)
        || t.pkey.as_ref().is_some_and(|p| p.name == con)
        || t.fks.iter().any(|f| f.name == con)
}

fn alter_add_constraint(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    check: &Option<CheckDef>,
    unique: &Option<UniqueDef>,
    pkey: &Option<UniqueDef>,
    fk: &Option<FkDef>,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let mut next = t.clone();
    let meta = TableMeta::of(&next);
    if let Some(c) = check {
        validate_constraint_expr(&c.expr, "CHECK").map_err(sql_err)?;
        if table_has_constraint(&next, &c.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", c.name),
            ));
        }
        let mut refs = Vec::new();
        collect_col_refs(&c.expr, &mut refs);
        for (_, r) in &refs {
            if meta.column_index(r).is_none() {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" does not exist", r),
                ));
            }
        }
        // Existing rows must satisfy the new CHECK.
        let mut probe = next.clone();
        probe.checks.push(c.clone());
        let probe_meta = TableMeta::of(&probe);
        let old_rows: Vec<Vec<Value>> = t
            .rows
            .iter()
            .filter(|r| row_visible(r, ctx.snap, ctx.own))
            .map(|r| r.values.clone())
            .collect();
        for values in &old_rows {
            check_row_constraints(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &probe_meta,
                name,
                values,
            )?;
        }
        next.checks.push(c.clone());
    }
    if let Some(u) = unique {
        if table_has_constraint(&next, &u.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", u.name),
            ));
        }
        for c in &u.cols {
            if next.column_index(c).is_none() {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", c, name),
                ));
            }
        }
        next.uniques.push(u.clone());
        alter_swap(eng, ctx, name, None, next, None)?;
        create_constraint_index(eng, ctx, name, &u.name, &u.cols, true)?;
        return Ok(ExecResult::Command {
            tag: "ALTER TABLE".to_string(),
        });
    }
    if let Some(pk) = pkey {
        if table_has_constraint(&next, &pk.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", pk.name),
            ));
        }
        if next.pkey.is_some() {
            return Err(exec_err(
                "42P16",
                "multiple primary keys for table".to_string(),
            ));
        }
        for c in &pk.cols {
            if next.column_index(c).is_none() {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", c, name),
                ));
            }
        }
        next.pkey = Some(pk.clone());
        alter_swap(eng, ctx, name, None, next, None)?;
        create_constraint_index(eng, ctx, name, &pk.name, &pk.cols, true)?;
        return Ok(ExecResult::Command {
            tag: "ALTER TABLE".to_string(),
        });
    }
    if let Some(f) = fk {
        if table_has_constraint(&next, &f.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", f.name),
            ));
        }
        validate_fk_def(eng, ctx, name, f)?;
        next.fks.push(f.clone());
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// Drop a single constraint by name on a table (used by DROP CONSTRAINT and
/// by CASCADE paths). Returns the dropped constraint's backing index name,
/// if any.
fn alter_drop_constraint_internal(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    con: &str,
) -> Result<(), ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let mut next = t.clone();
    let mut backing_index: Option<String> = None;
    let mut found = false;
    if let Some(pos) = next.checks.iter().position(|c| c.name == con) {
        next.checks.remove(pos);
        found = true;
    }
    if !found {
        if let Some(pos) = next.uniques.iter().position(|u| u.name == con) {
            backing_index = Some(next.uniques[pos].name.clone());
            next.uniques.remove(pos);
            found = true;
        }
    }
    if !found {
        if next.pkey.as_ref().is_some_and(|p| p.name == con) {
            backing_index = next.pkey.as_ref().map(|p| p.name.clone());
            next.pkey = None;
            found = true;
        }
    }
    if !found {
        if let Some(pos) = next.fks.iter().position(|f| f.name == con) {
            next.fks.remove(pos);
            found = true;
        }
    }
    if !found {
        return Err(exec_err(
            "42704",
            format!(
                "constraint \"{}\" of relation \"{}\" does not exist",
                con, name
            ),
        ));
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    if let Some(ix) = backing_index {
        if eng.db.find_index(&ix, ctx.snap, ctx.own).is_some() {
            drop_index_internal(eng, ctx, &ix)?;
        }
    }
    Ok(())
}

fn alter_drop_constraint(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    con: &str,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    // RESTRICT: refuse if other tables' FKs depend on this constraint
    // (unique/pkey backing an FK). CASCADE: drop those FKs too.
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let is_uniqueish =
        t.uniques.iter().any(|u| u.name == con) || t.pkey.as_ref().is_some_and(|p| p.name == con);
    if is_uniqueish {
        let mut dep_fks: Vec<(String, String)> = Vec::new();
        for (tname, vs) in &eng.db.tables {
            if tname == name {
                continue;
            }
            let Some(ot) = vs
                .iter()
                .find(|tt| crate::storage::table_visible(tt, ctx.snap, ctx.own))
            else {
                continue;
            };
            for fk in &ot.fks {
                if fk.ref_table == name {
                    dep_fks.push((tname.clone(), fk.name.clone()));
                }
            }
        }
        if !dep_fks.is_empty() && !cascade {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop constraint {} because other objects depend on it",
                    con
                ),
            ));
        }
        for (tname, fk_name) in &dep_fks {
            alter_drop_constraint_internal(eng, ctx, tname, fk_name)?;
        }
    }
    alter_drop_constraint_internal(eng, ctx, name, con)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

fn alter_set_default(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    default: Option<DefaultExpr>,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let ci = t.column_index(col).ok_or_else(|| {
        exec_err(
            "42703",
            format!("column \"{}\" of relation \"{}\" does not exist", col, name),
        )
    })?;
    if let Some(DefaultExpr::Expr(e)) = &default {
        validate_constraint_expr(e, "DEFAULT").map_err(sql_err)?;
    }
    let mut next = t.clone();
    next.defaults[ci] = default;
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

fn alter_rename_column(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    old: &str,
    new: &str,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    if t.column_index(old).is_none() {
        return Err(exec_err(
            "42703",
            format!("column \"{}\" of relation \"{}\" does not exist", old, name),
        ));
    }
    if t.column_index(new).is_some() {
        return Err(exec_err(
            "42701",
            format!("column \"{}\" of relation \"{}\" already exists", new, name),
        ));
    }
    // Views reference columns by name in stored SQL; refuse rather than
    // silently breaking them.
    for (vname, vs) in &eng.db.views {
        if vs.iter().any(|v| {
            crate::storage::view_visible(v, ctx.snap, ctx.own) && v.deps.iter().any(|d| d == name)
        }) {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot rename column {} because view {} depends on table {}",
                    old, vname, name
                ),
            ));
        }
    }
    let mut next = t.clone();
    let ci = next.column_index(old).expect("checked");
    next.columns[ci].0 = new.to_string();
    // Rename inside CHECK/DEFAULT expressions.
    for c in &mut next.checks {
        rename_col_in_expr(&mut c.expr, old, new);
    }
    for d in &mut next.defaults {
        if let Some(DefaultExpr::Expr(e)) = d {
            rename_col_in_expr(e, old, new);
        }
    }
    // Rename inside unique/pkey/fk column lists.
    for u in &mut next.uniques {
        for c in &mut u.cols {
            if c == old {
                *c = new.to_string();
            }
        }
    }
    if let Some(pk) = &mut next.pkey {
        for c in &mut pk.cols {
            if c == old {
                *c = new.to_string();
            }
        }
    }
    for fk in &mut next.fks {
        for c in &mut fk.cols {
            if c == old {
                *c = new.to_string();
            }
        }
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    // Update index col_names.
    let ix_names: Vec<String> = eng
        .db
        .indexes
        .iter()
        .filter(|(_, ix)| ix.def.table == name)
        .map(|(n, _)| n.clone())
        .collect();
    for ix_name in ix_names {
        if let Some(ix) = eng.db.indexes.get_mut(&ix_name) {
            for c in &mut ix.def.col_names {
                if c == old {
                    *c = new.to_string();
                }
            }
        }
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

fn alter_rename_to(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    new_name: &str,
) -> Result<ExecResult, ExecError> {
    if eng.db.find_table(new_name, ctx.snap, ctx.own).is_some()
        || eng.db.views.get(new_name).is_some_and(|vs| {
            vs.iter()
                .any(|v| crate::storage::view_visible(v, ctx.snap, ctx.own))
        })
    {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", new_name),
        ));
    }
    let t = eng
        .db
        .find_table(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    // Update FK ref_table in other tables that point at the old name.
    let mut ref_tables: Vec<String> = Vec::new();
    for (tname, vs) in &eng.db.tables {
        if tname == name {
            continue;
        }
        if let Some(ot) = vs
            .iter()
            .find(|tt| crate::storage::table_visible(tt, ctx.snap, ctx.own))
        {
            if ot.fks.iter().any(|fk| fk.ref_table == name) {
                ref_tables.push(tname.clone());
            }
        }
    }
    for tname in ref_tables {
        let ot = eng
            .db
            .find_table(&tname, ctx.snap, ctx.own)
            .expect("found above")
            .clone();
        let mut onext = ot.clone();
        for fk in &mut onext.fks {
            if fk.ref_table == name {
                fk.ref_table = new_name.to_string();
            }
        }
        alter_swap(eng, ctx, &tname, None, onext, None)?;
    }
    // Update view dependencies.
    let mut view_names: Vec<String> = Vec::new();
    for (vname, vs) in &eng.db.views {
        if vs.iter().any(|v| {
            crate::storage::view_visible(v, ctx.snap, ctx.own) && v.deps.iter().any(|d| d == name)
        }) {
            view_names.push(vname.clone());
        }
    }
    for vname in view_names {
        if let Some(vs) = eng.db.views.get_mut(&vname) {
            if let Some(v) = vs
                .iter_mut()
                .find(|v| crate::storage::view_visible(v, ctx.snap, ctx.own))
            {
                for d in &mut v.deps {
                    if d == name {
                        *d = new_name.to_string();
                    }
                }
            }
        }
    }
    alter_swap(eng, ctx, name, Some(new_name.to_string()), t, None)?;
    // Indexes were updated inside alter_swap; fix their table field via def.
    for ix in eng.db.indexes.values_mut() {
        if ix.def.table == name {
            ix.def.table = new_name.to_string();
        }
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

// ============================================================================
// v0.9: views.
// ============================================================================

fn exec_create_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    query: &str,
    col_aliases: &[String],
    or_replace: bool,
) -> Result<ExecResult, ExecError> {
    // A table by this name blocks the view (Postgres shares the namespace).
    if eng.db.find_table(name, ctx.snap, ctx.own).is_some() {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    let existing = eng.db.find_view(name, ctx.snap, ctx.own).is_some();
    if existing && !or_replace {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    if existing {
        // v0.11: OR REPLACE requires ownership of the existing view.
        require_view_owner(eng, ctx, name)?;
    }
    // The query must parse, and must not reference the view itself
    // (checked by the parser) or unknown relations.
    let stmt = parse_statement(query).map_err(sql_err)?;
    let select = match stmt {
        Stmt::Select(s) => s,
        _ => {
            return Err(exec_err(
                "42601",
                "CREATE VIEW query must be a SELECT".to_string(),
            ));
        }
    };
    let mut deps = Vec::new();
    collect_table_refs(&select, &mut deps);
    // Every referenced relation must exist (as a table or view).
    for d in &deps {
        let is_table = eng.db.find_table(d, ctx.snap, ctx.own).is_some();
        let is_view = eng.db.find_view(d, ctx.snap, ctx.own).is_some();
        if !is_table && !is_view {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", d),
            ));
        }
    }
    if existing {
        // OR REPLACE: drop the old definition first (same statement).
        drop_view_internal(eng, ctx, name)?;
    }
    let def = ViewDef {
        query: query.to_string(),
        col_aliases: col_aliases.to_vec(),
        deps,
        created_xmin: ctx.own,
        dropped_xmax: 0,
        // v0.11: the creating role owns the view.
        owner: ctx.role.to_string(),
    };
    eng.db.views.entry(name.to_string()).or_default().push(def);
    ctx.writes.push(WriteOp::CreateView {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: "CREATE VIEW".to_string(),
    })
}

fn exec_drop_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        // v0.11: only the owner (or a superuser) may drop a view.
        require_view_owner(eng, ctx, name)?;
        let exists = eng.db.find_view(name, ctx.snap, ctx.own).is_some();
        if !exists {
            if if_exists {
                continue;
            }
            return Err(exec_err(
                "42P01",
                format!("view \"{}\" does not exist", name),
            ));
        }
        // Other views depending on this one.
        let mut dependents: Vec<String> = Vec::new();
        for (vname, vs) in &eng.db.views {
            if vname == name {
                continue;
            }
            if vs.iter().any(|v| {
                crate::storage::view_visible(v, ctx.snap, ctx.own)
                    && v.deps.iter().any(|d| d == name)
            }) {
                dependents.push(vname.clone());
            }
        }
        if !dependents.is_empty() && !cascade {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop view {} because view {} depends on it",
                    name, dependents[0]
                ),
            ));
        }
        for dep in &dependents {
            drop_view_internal(eng, ctx, dep)?;
        }
        drop_view_internal(eng, ctx, name)?;
    }
    Ok(ExecResult::Command {
        tag: "DROP VIEW".to_string(),
    })
}

/// Drop a view by name, logging WriteOp::DropView. Used by DROP VIEW and
/// by CASCADE paths from DROP TABLE / DROP COLUMN.
fn drop_view_internal(eng: &mut Engine, ctx: &mut StmtCtx, name: &str) -> Result<(), ExecError> {
    let snapshot = eng
        .db
        .find_view(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("view \"{}\" does not exist", name)))?
        .clone();
    eng.db
        .find_view_mut(name, ctx.snap, ctx.own)
        .expect("view still present")
        .dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropView {
        name: name.to_string(),
        view: snapshot,
    });
    Ok(())
}

// ============================================================================
// v0.9: information_schema / pg_catalog virtual tables (`\d`-style
// introspection over the real catalog).
// ============================================================================

fn info_tables_schema() -> Vec<QCol> {
    [
        ("table_catalog", ColType::Text),
        ("table_schema", ColType::Text),
        ("table_name", ColType::Text),
        ("table_type", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "information_schema.tables".to_string(),
        name: n.to_string(),
        ty,
    })
    .collect()
}

fn info_tables_scan(db: &Database, snap: &Snapshot, own: u64) -> (Vec<QCol>, Vec<QRow>) {
    let schema = info_tables_schema();
    let mut rows = Vec::new();
    let mut names: Vec<String> = db
        .tables
        .iter()
        .filter(|(_, vs)| {
            vs.iter()
                .any(|t| crate::storage::table_visible(t, snap, own))
        })
        .map(|(n, _)| n.clone())
        .collect();
    names.sort();
    for tn in names {
        rows.push(QRow {
            cells: vec![
                Value::Text("rustgres".to_string()),
                Value::Text("public".to_string()),
                Value::Text(tn),
                Value::Text("BASE TABLE".to_string()),
            ],
            prov: Vec::new(),
        });
    }
    let mut vnames: Vec<String> = db
        .views
        .iter()
        .filter(|(_, vs)| {
            vs.iter()
                .any(|v| crate::storage::view_visible(v, snap, own))
        })
        .map(|(n, _)| n.clone())
        .collect();
    vnames.sort();
    for vn in vnames {
        rows.push(QRow {
            cells: vec![
                Value::Text("rustgres".to_string()),
                Value::Text("public".to_string()),
                Value::Text(vn),
                Value::Text("VIEW".to_string()),
            ],
            prov: Vec::new(),
        });
    }
    (schema, rows)
}

fn info_columns_schema() -> Vec<QCol> {
    [
        ("table_catalog", ColType::Text),
        ("table_schema", ColType::Text),
        ("table_name", ColType::Text),
        ("column_name", ColType::Text),
        ("ordinal_position", ColType::Int),
        ("column_default", ColType::Text),
        ("is_nullable", ColType::Text),
        ("data_type", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "information_schema.columns".to_string(),
        name: n.to_string(),
        ty,
    })
    .collect()
}

fn info_columns_scan(db: &Database, snap: &Snapshot, own: u64) -> (Vec<QCol>, Vec<QRow>) {
    let schema = info_columns_schema();
    let mut rows = Vec::new();
    let mut tables: Vec<(String, Table)> = db
        .tables
        .iter()
        .filter(|(_, vs)| {
            vs.iter()
                .any(|t| crate::storage::table_visible(t, snap, own))
        })
        .map(|(n, vs)| {
            let t = vs
                .iter()
                .find(|t| crate::storage::table_visible(t, snap, own))
                .expect("filtered")
                .clone();
            (n.clone(), t)
        })
        .collect();
    tables.sort_by(|a, b| a.0.cmp(&b.0));
    for (tn, t) in tables {
        for (i, (cn, ty)) in t.columns.iter().enumerate() {
            let default = match &t.defaults[i] {
                Some(d) => Value::Text(format!("{:?}", d)),
                None => Value::Null,
            };
            rows.push(QRow {
                cells: vec![
                    Value::Text("rustgres".to_string()),
                    Value::Text("public".to_string()),
                    Value::Text(tn.clone()),
                    Value::Text(cn.clone()),
                    Value::Int((i + 1) as i64),
                    default,
                    Value::Text(if t.not_null[i] { "NO" } else { "YES" }.to_string()),
                    Value::Text(format!("{:?}", ty)),
                ],
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}
