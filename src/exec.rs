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
    AggFunc, ArithOp, CmpOp, Expr, FromItem, InsertValue, IsolationLevel, JoinKind, Literal,
    SelectItem, SelectStmt, Stmt, WhereCond, WhereRhs,
};
use crate::storage::{
    ColType, Engine, Numeric, RowVersion, Snapshot, Value, WriteOp, row_visible,
};
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
fn coerce_int_lit(i: i128, col_type: &ColType, col_name: &str, _from: &str) -> Result<Value, ExecError> {
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
        ColType::Text => Ok(Value::Text(
            Value::Float(f).to_text().unwrap_or_default(),
        )),
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
        (Value::SmallInt(i), ColType::Numeric) => Some(Value::Numeric(Numeric::from_i64(*i as i64))),
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
        (Value::Float(a), Literal::Decimal(b)) => {
            Ok(*a == b.parse::<f64>().unwrap_or(f64::NAN))
        }
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
        Expr::Like {
            expr, pattern, ..
        } => {
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
        Expr::Like {
            expr, pattern, ..
        } => contains_agg(expr) || contains_agg(pattern),
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
        Expr::Arith { left, right, .. } => {
            pushable_columns(left, cols) && pushable_columns(right, cols)
        }
        Expr::Concat(a, b) => pushable_columns(a, cols) && pushable_columns(b, cols),
        Expr::Cast { expr, .. } => pushable_columns(expr, cols),
        Expr::Like {
            expr, pattern, ..
        } => pushable_columns(expr, cols) && pushable_columns(pattern, cols),
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
        Expr::Like {
            expr, pattern, ..
        } => {
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
            let va = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, a,
            )?;
            let vb = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, b,
            )?;
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
        AggFunc::Sum => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            // v0.6 rule kept: sum returns the widest input kind among
            // the rows (documented deviation: Postgres widens int->bigint).
            let mut cat = NumCat::Small;
            for v in &vals {
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
                for v in &vals {
                    let n = to_numeric_opt(v).ok_or_else(|| {
                        exec_err("22003", "value out of range for numeric")
                    })?;
                    acc = acc
                        .checked_add(&n)
                        .ok_or_else(|| exec_err("22003", "numeric field overflow"))?;
                }
                Ok(Value::Numeric(acc))
            } else if cat >= NumCat::Real {
                let mut acc = 0.0;
                for v in &vals {
                    acc += to_f64v(v);
                }
                Ok(if cat == NumCat::Real {
                    Value::Float4(acc as f32)
                } else {
                    Value::Float(acc)
                })
            } else {
                let mut acc: i128 = 0;
                for v in &vals {
                    acc = acc.checked_add(to_i128(v)).ok_or_else(|| {
                        exec_err("22003", "integer out of range")
                    })?;
                }
                let icat = if cat == NumCat::Small {
                    NumCat::Int
                } else {
                    cat
                };
                fit_int_result(icat, acc)
            }
        }
        AggFunc::Avg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            // v0.6 rule kept: avg always returns double precision.
            let mut sum = 0.0;
            for v in &vals {
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
        NumCat::Small => Ok(Value::SmallInt(i16::try_from(r).map_err(|_| {
            exec_err("22003", "smallint out of range")
        })?)),
        NumCat::Int => Ok(Value::Int(i32::try_from(r).map_err(|_| ovf())? as i64)),
        _ => Ok(Value::BigInt(i64::try_from(r).map_err(|_| ovf())?)),
    }
}

/// Date arithmetic. Ok(None) = not date/time operands (caller falls
/// through to numeric handling).
fn eval_datetime_arith(
    op: ArithOp,
    a: &Value,
    b: &Value,
) -> Result<Option<Value>, ExecError> {
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
        (Value::Timestamp(_), Value::Timestamp(_))
            | (Value::Timestamptz(_), Value::Timestamptz(_))
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
        format!(
            "cannot cast type {} to {}",
            from.type_name(),
            to
        ),
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
            let r = n.to_i64().ok_or_else(|| exec_err("22003", "numeric out of range"))?;
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
                    format!(
                        "invalid input syntax for type double precision: {:?}",
                        s
                    ),
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
        Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "on" | "1" => Ok(true),
            "false" | "f" | "no" | "n" | "off" | "0" => Ok(false),
            _ => Err(exec_err(
                "22P02",
                format!("invalid input syntax for type boolean: {:?}", s),
            )),
        },
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
            Value::Text(s) => crate::storage::parse_uuid(s)
                .map(Value::Uuid)
                .map_err(|_| {
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
            && (toks[ti] == PatTok::Any
                || matches!(toks[ti], PatTok::Lit(c) if c == s[si]))
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
        "upper" | "lower" | "length" | "char_length" | "character_length" | "abs"
        | "floor" | "ceil" | "ceiling" | "sqrt" => n == 1,
        "round" => n == 1 || n == 2,
        "substring" => n == 2 || n == 3,
        "power" | "mod" | "position" | "date_trunc" | "nullif" => n == 2,
        "replace" | "split_part" | "trim" => n == 3,
        "now" | "current_date" | "current_timestamp" => n == 0,
        "coalesce" | "greatest" | "least" => n >= 1,
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
    match name {
        "upper" | "lower" | "length" | "char_length" | "character_length" | "substring"
        | "trim" | "position" | "replace" | "split_part" => eval_str_func(name, vals),
        "abs" | "round" | "floor" | "ceil" | "ceiling" | "sqrt" | "power" | "mod" => {
            eval_math_func(name, vals)
        }
        "now" | "current_date" | "current_timestamp" | "date_trunc" => {
            eval_datetime_func(name, vals)
        }
        "coalesce" | "nullif" | "greatest" | "least" => eval_cond_func(name, vals),
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
            let idx = if n > 0 {
                n - 1
            } else {
                parts.len() as i64 + n
            };
            let r = if idx >= 0 {
                parts.get(idx as usize).copied().unwrap_or("")
            } else {
                ""
            };
            Ok(Value::Text(r.to_string()))
        }
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// Shared `power(a, b)` / `a ^ b` implementation. `bad` builds the
/// type-mismatch error (42883), which differs between the function and
/// operator spellings.
fn eval_power_op(a: &Value, b: &Value, bad: impl Fn(&Value) -> ExecError) -> Result<Value, ExecError> {
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
            let n = to_numeric_opt(v)
                .ok_or_else(|| func_arg_err(name, v))?;
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
                Value::Float4(f) => Ok(Value::Float4(if is_floor {
                    f.floor()
                } else {
                    f.ceil()
                })),
                Value::Float(f) => Ok(Value::Float(if is_floor {
                    f.floor()
                } else {
                    f.ceil()
                })),
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
) -> Result<ColType, ExecError> {
    let arg0 = || expr_type(eng, snap, own, schemas, &args[0]);
    match name {
        "upper" | "lower" | "substring" | "trim" | "replace" | "split_part" => Ok(ColType::Text),
        "length" | "char_length" | "character_length" | "position" => Ok(ColType::Int),
        "abs" => arg0(),
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
                match expr_type(eng, snap, own, schemas, a)? {
                    ColType::Float4 | ColType::Float => return Ok(ColType::Float),
                    _ => {}
                }
            }
            Ok(ColType::Numeric)
        }
        "now" | "current_timestamp" => Ok(ColType::Timestamptz),
        "current_date" => Ok(ColType::Date),
        "date_trunc" => match expr_type(eng, snap, own, schemas, &args[1])? {
            ColType::Timestamptz => Ok(ColType::Timestamptz),
            _ => Ok(ColType::Timestamp),
        },
        "coalesce" | "nullif" | "greatest" | "least" => arg0(),
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
        Expr::Arith { op, left, right } => {
            let ta = arith_operand_type(eng, snap, own, schemas, left, *op)?;
            let tb = arith_operand_type(eng, snap, own, schemas, right, *op)?;
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
        Expr::Func { name, args } => func_result_type(name, args, eng, snap, own, schemas),
        Expr::Extract { .. } => Ok(ColType::Numeric),
        Expr::Agg {
            func,
            arg,
            distinct: _,
            arg2,
        } => agg_result_type(eng, snap, own, schemas, *func, arg.as_deref(), arg2.as_deref()),
        Expr::ScalarSub(sub) => {
            let cols = describe_select(eng, snap, own, sub)?;
            if cols.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            Ok(cols[1 - 1].clone().1)
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
    e: &Expr,
    op: ArithOp,
) -> Result<Option<ColType>, ExecError> {
    match e {
        Expr::Literal(Literal::Null) => Ok(None),
        Expr::Literal(lit) => Ok(Some(lit.col_type())),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Column { .. } | Expr::ResolvedCol { .. } => {
            Ok(Some(expr_type(eng, snap, own, schemas, e)?))
        }
        Expr::Arith {
            op: inner,
            left,
            right,
        } => {
            let ta = arith_operand_type(eng, snap, own, schemas, left, *inner)?;
            let tb = arith_operand_type(eng, snap, own, schemas, right, *inner)?;
            Ok(Some(combine_arith_types(*inner, ta, tb)?))
        }
        Expr::Agg { .. } | Expr::ScalarSub(_) => Ok(Some(expr_type(eng, snap, own, schemas, e)?)),
        Expr::Cast { to, .. } => Ok(Some(*to)),
        Expr::Func { .. } => Ok(Some(expr_type(eng, snap, own, schemas, e)?)),
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
            let is_int_kind = |t: &ColType| {
                matches!(t, ColType::SmallInt | ColType::Int | ColType::BigInt)
            };
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
    func: AggFunc,
    arg: Option<&Expr>,
    arg2: Option<&Expr>,
) -> Result<ColType, ExecError> {
    match func {
        // Postgres count() returns bigint.
        AggFunc::Count => Ok(ColType::BigInt),
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
                // v0.6 rule kept (documented): sum returns the
                // argument type; Postgres would widen int->bigint.
                Ok(t)
            }
        },
        AggFunc::Min | AggFunc::Max => {
            let a = arg.expect("min/max always take an argument");
            expr_type(eng, snap, own, schemas, a)
        }
        AggFunc::StringAgg => {
            // Delimiter should be text-ish; be permissive here (the
            // executor coerces via casts) and just require an argument.
            let a = arg.expect("string_agg always takes arguments");
            let t = expr_type(eng, snap, own, schemas, a)?;
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
            func_result_type(name, args, eng, snap, own, schemas).ok()
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
            *func,
            arg.as_deref(),
            arg2.as_deref(),
        )
        .ok(),
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
        Expr::Like {
            expr, pattern, ..
        } => {
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
