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
//! locking in v0.5; documented in the README).

use crate::sql::{
    Expr, InsertValue, IsolationLevel, Literal, OrderTerm, SelectItem, Stmt, WhereCond,
    WhereRhs,
};
use std::cmp::Ordering;
use crate::storage::{
    row_visible, ColType, Engine, RowVersion, Snapshot, Value, WriteOp,
};

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

pub fn execute(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    stmt: &Stmt,
) -> Result<ExecResult, ExecError> {
    match stmt {
        Stmt::CreateTable { name, columns } => exec_create(eng, ctx, name, columns),
        Stmt::Insert {
            table,
            columns,
            rows,
        } => exec_insert(eng, ctx, table, columns, rows),
        Stmt::Select {
            items,
            table,
            where_,
            order_by,
            limit,
        } => exec_select(eng, ctx, items, table, where_, order_by, *limit),
        Stmt::DropTable { if_exists, name } => exec_drop(eng, ctx, name, *if_exists),
        Stmt::Update { table, sets, where_ } => exec_update(eng, ctx, table, sets, where_),
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
    eng.db.tables.entry(name.to_string()).or_default().push(
        crate::storage::Table::new(columns.to_vec(), ctx.own),
    );
    ctx.writes.push(WriteOp::CreateTable {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: "CREATE TABLE".to_string(),
    })
}

/// Coerce an INSERT literal to the target column type.
fn coerce_literal(
    lit: &Literal,
    col_type: &ColType,
    col_name: &str,
) -> Result<Value, ExecError> {
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
fn coerce_value(
    v: Value,
    col_type: &ColType,
    col_name: &str,
) -> Result<Value, ExecError> {
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
        let t = eng.db.find_table(table, ctx.snap, ctx.own).ok_or_else(|| {
            exec_err("42P01", format!("relation \"{}\" does not exist", table))
        })?;
        let targets: Vec<usize> = match columns {
            Some(names) => names
                .iter()
                .map(|n| {
                    t.column_index(n).ok_or_else(|| {
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
                        return Err(exec_err(
                            "42P02",
                            format!("there is no parameter ${}", n),
                        ));
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

/// `WHERE col = literal` comparison. NULL never matches (SQL semantics).
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

/// Does this row version satisfy all WHERE conditions?
fn row_matches_where(
    t: &crate::storage::Table,
    values: &[Value],
    where_: &[WhereCond],
) -> Result<bool, ExecError> {
    for w in where_ {
        let i = t.column_index(&w.col).ok_or_else(|| {
            exec_err("42703", format!("column \"{}\" does not exist", w.col))
        })?;
        let lit = match &w.rhs {
            WhereRhs::Lit(l) => l,
            WhereRhs::Param(n) => {
                return Err(exec_err(
                    "42P02",
                    format!("there is no parameter ${}", n),
                ))
            }
        };
        if !value_matches(&values[i], lit)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Write-write conflict check for UPDATE/DELETE/DROP: the version was
/// visible in our snapshot, but its `xmax` is now set by another
/// transaction that committed *after* our snapshot was taken. (If it had
/// committed before, the version would not be visible to us at all.)
/// REPEATABLE READ and SERIALIZABLE fail with 40001, like Postgres;
/// READ COMMITTED cannot reach this — its per-statement snapshot already
/// excludes such versions.
fn check_write_conflict(eng: &Engine, v: &RowVersion, level: IsolationLevel) -> Result<(), ExecError> {
    if v.xmax == 0 {
        return Ok(());
    }
    if eng.xid_committed(v.xmax) {
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
        let t = eng.db.find_table(table, ctx.snap, ctx.own).ok_or_else(|| {
            exec_err("42P01", format!("relation \"{}\" does not exist", table))
        })?;
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
        let mut plan = Vec::new();
        for rv in t.rows.iter().filter(|r| row_visible(r, ctx.snap, ctx.own)) {
            check_write_conflict(eng, rv, ctx.level)?;
            if !row_matches_where(t, &rv.values, where_)? {
                continue;
            }
            let mut new_values = rv.values.clone();
            for ((_, expr), &ci) in sets.iter().zip(set_cols.iter()) {
                let v = eval_expr(expr, Some(t), Some(&rv.values))?;
                let (cname, ctype) = &t.columns[ci];
                new_values[ci] = coerce_value(v, ctype, cname)?;
            }
            plan.push((rv.id, rv.xmax, new_values));
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
        let t = eng.db.find_table(table, ctx.snap, ctx.own).ok_or_else(|| {
            exec_err("42P01", format!("relation \"{}\" does not exist", table))
        })?;
        let mut plan = Vec::new();
        for rv in t.rows.iter().filter(|r| row_visible(r, ctx.snap, ctx.own)) {
            check_write_conflict(eng, rv, ctx.level)?;
            if row_matches_where(t, &rv.values, where_)? {
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

// ---------------------------------------------------------------------------
// v0.2: expressions
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
            ))
        }
    };
    Ok((ty, v))
}

/// (output name, output type) of a select-list expression, without row data.
/// Parameters must already be substituted; a leftover `Param` is 42P02.
fn expr_desc(
    e: &Expr,
    t: Option<&crate::storage::Table>,
) -> Result<(String, ColType), ExecError> {
    match e {
        Expr::Column(name) => {
            let tbl = t.ok_or_else(|| {
                exec_err("42601", "syntax error: column reference requires FROM")
            })?;
            let i = tbl.column_index(name).ok_or_else(|| {
                exec_err("42703", format!("column \"{}\" does not exist", name))
            })?;
            Ok((name.clone(), tbl.columns[i].1.clone()))
        }
        Expr::Literal(lit) => Ok(("?column?".to_string(), lit.col_type())),
        Expr::Param(n) => Err(exec_err(
            "42P02",
            format!("there is no parameter ${}", n),
        )),
        Expr::Add(a, b) => {
            let ta = add_operand_desc_type(a, t)?;
            let tb = add_operand_desc_type(b, t)?;
            Ok(("?column?".to_string(), combine_add_types(ta, tb)?))
        }
    }
}

/// Operand type of `+` for the description pass; NULL contributes nothing.
fn add_operand_desc_type(
    e: &Expr,
    t: Option<&crate::storage::Table>,
) -> Result<Option<ColType>, ExecError> {
    match e {
        Expr::Literal(Literal::Null) => Ok(None),
        Expr::Literal(lit) => Ok(Some(lit.col_type())),
        Expr::Param(n) => Err(exec_err(
            "42P02",
            format!("there is no parameter ${}", n),
        )),
        Expr::Column(_) => expr_desc(e, t).map(|(_, ty)| Some(ty)),
        Expr::Add(a, b) => {
            let ta = add_operand_desc_type(a, t)?;
            let tb = add_operand_desc_type(b, t)?;
            Ok(Some(combine_add_types(ta, tb)?))
        }
    }
}

/// Evaluate one select-list expression against a row (params substituted).
fn eval_expr(
    e: &Expr,
    t: Option<&crate::storage::Table>,
    row: Option<&[Value]>,
) -> Result<Value, ExecError> {
    match e {
        Expr::Column(name) => {
            let r = row.ok_or_else(|| {
                exec_err("42601", "syntax error: column reference requires FROM")
            })?;
            // t is Some whenever row is Some (both come from the FROM table).
            let tbl = t.expect("table present when row present");
            let i = tbl.column_index(name).ok_or_else(|| {
                exec_err("42703", format!("column \"{}\" does not exist", name))
            })?;
            Ok(r[i].clone())
        }
        Expr::Literal(lit) => Ok(lit.clone().into_value()),
        Expr::Param(n) => Err(exec_err(
            "42P02",
            format!("there is no parameter ${}", n),
        )),
        Expr::Add(a, b) => {
            let va = eval_expr(a, t, row)?;
            let vb = eval_expr(b, t, row)?;
            eval_add(&va, &vb).map(|(_, v)| v)
        }
    }
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
            })
        }
        (_, Value::Null) => {
            return Ok(if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            })
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
            ))
        }
    };
    Ok(if desc { ord.reverse() } else { ord })
}

/// Evaluate one ORDER BY term against a row. An integer literal is a
/// 1-based position in the select list, like PostgreSQL; anything else is a
/// normal expression over the row's columns.
fn sort_key_value(
    expr: &Expr,
    table: Option<&crate::storage::Table>,
    row: Option<&[Value]>,
    items: &[SelectItem],
) -> Result<Value, ExecError> {
    if let Expr::Literal(Literal::Int(n)) = expr {
        let idx = *n as usize;
        if idx >= 1 {
            let mut pos = 0;
            for item in items {
                match item {
                    SelectItem::All => {
                        let t = table.ok_or_else(|| {
                            exec_err("42601", "ORDER BY position with SELECT * needs FROM")
                        })?;
                        let width = t.columns.len();
                        if idx > pos && idx <= pos + width {
                            let row = row.ok_or_else(|| {
                                exec_err("42601", "ORDER BY position needs a row")
                            })?;
                            return Ok(row[idx - pos - 1].clone());
                        }
                        pos += width;
                    }
                    SelectItem::Expr(e) => {
                        pos += 1;
                        if pos == idx {
                            return Ok(eval_expr(e, table, row)?);
                        }
                    }
                }
            }
            return Err(exec_err(
                "42601",
                format!(
                    "ORDER BY position {} is not in the select list",
                    n
                ),
            ));
        }
    }
    eval_expr(expr, table, row)
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

fn exec_select(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    items: &[SelectItem],
    table: &Option<String>,
    where_: &[WhereCond],
    order_by: &[OrderTerm],
    limit: Option<i64>,
) -> Result<ExecResult, ExecError> {
    if let Some(n) = limit {
        if n < 0 {
            return Err(exec_err("2201W", "LIMIT must not be negative"));
        }
    }
    match table {
        None => {
            // `SELECT <exprs>` with no FROM: exactly one row.
            if !where_.is_empty() {
                return Err(exec_err("42601", "WHERE requires FROM"));
            }
            let mut columns = Vec::new();
            let mut row = Vec::new();
            for item in items {
                match item {
                    SelectItem::All => {
                        return Err(exec_err(
                            "42601",
                            "syntax error: SELECT * requires FROM",
                        ));
                    }
                    SelectItem::Expr(e) => {
                        let (name, ty) = expr_desc(e, None)?;
                        let v = eval_expr(e, None, None)?;
                        columns.push((name, ty));
                        row.push(v);
                    }
                }
            }
            // One row: nothing to sort, but evaluate the keys so bad
            // column references still error like PostgreSQL.
            for o in order_by {
                let _ = sort_key_value(&o.expr, None, None, items)?;
            }
            Ok(ExecResult::Select {
                columns,
                rows: vec![row],
            })
        }
        Some(tname) => {
            let t = eng.db.find_table(tname, ctx.snap, ctx.own).ok_or_else(|| {
                exec_err("42P01", format!("relation \"{}\" does not exist", tname))
            })?;
            // Resolve output columns up front (so bad columns error even on
            // empty tables, like v0.1).
            let mut columns = Vec::new();
            for item in items {
                match item {
                    SelectItem::All => {
                        for (n, ty) in &t.columns {
                            columns.push((n.clone(), ty.clone()));
                        }
                    }
                    SelectItem::Expr(e) => {
                        let (n, ty) = expr_desc(e, Some(t))?;
                        columns.push((n, ty));
                    }
                }
            }
            // Filter + project over the versions visible in our snapshot.
            // Sort keys are computed against the full row (pre-projection),
            // like PostgreSQL, so ORDER BY can reference non-selected columns.
            let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
            for rv in t.rows.iter().filter(|r| row_visible(r, ctx.snap, ctx.own)) {
                if !row_matches_where(t, &rv.values, where_)? {
                    continue;
                }
                let mut keys = Vec::with_capacity(order_by.len());
                for o in order_by {
                    keys.push(sort_key_value(&o.expr, Some(t), Some(&rv.values), items)?);
                }
                let mut out = Vec::with_capacity(columns.len());
                for item in items {
                    match item {
                        SelectItem::All => out.extend(rv.values.iter().cloned()),
                        SelectItem::Expr(e) => {
                            out.push(eval_expr(e, Some(t), Some(&rv.values))?)
                        }
                    }
                }
                keyed.push((keys, out));
            }
            if !order_by.is_empty() {
                let mut cmp_err: Option<ExecError> = None;
                keyed.sort_by(|a, b| {
                    if cmp_err.is_some() {
                        return Ordering::Equal;
                    }
                    for (i, o) in order_by.iter().enumerate() {
                        match compare_values(&a.0[i], &b.0[i], o.desc) {
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
            }
            let mut rows: Vec<Vec<Value>> =
                keyed.into_iter().map(|(_, out)| out).collect();
            if let Some(n) = limit {
                rows.truncate(n as usize);
            }
            Ok(ExecResult::Select {
                columns,
                rows,
            })
        }
    }
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
            None if if_exists => return Ok(ExecResult::Command {
                tag: "DROP TABLE".to_string(),
            }),
            None => {
                return Err(exec_err(
                    "42P01",
                    format!("table \"{}\" does not exist", name),
                ))
            }
            Some(t) => {
                if t.dropped_xmax != 0 && t.dropped_xmax != ctx.own && eng.xid_committed(t.dropped_xmax)
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
// v0.2: parameters for the extended query protocol
// ---------------------------------------------------------------------------

fn pin_param(
    out: &mut [Option<ColType>],
    p: u32,
    t: ColType,
) -> Result<(), ExecError> {
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

/// A hint type for one side of `+` (None = param or NULL: no constraint).
fn add_side_hint(e: &Expr, tbl: Option<&crate::storage::Table>) -> Option<ColType> {
    match e {
        Expr::Literal(Literal::Null) | Expr::Param(_) => None,
        Expr::Literal(lit) => Some(lit.col_type()),
        Expr::Column(c) => tbl
            .and_then(|t| t.column_index(c))
            .map(|i| tbl.unwrap().columns[i].1.clone()),
        Expr::Add(a, b) => {
            combine_add_types(add_side_hint(a, tbl), add_side_hint(b, tbl)).ok()
        }
    }
}

fn infer_expr(
    e: &Expr,
    tbl: Option<&crate::storage::Table>,
    out: &mut [Option<ColType>],
) -> Result<(), ExecError> {
    if let Expr::Add(a, b) = e {
        infer_expr(a, tbl, out)?;
        infer_expr(b, tbl, out)?;
        // One side a param, the other side typed: pin the param.
        match (&**a, &**b) {
            (Expr::Param(p), other) => {
                if let Some(t) = add_side_hint(other, tbl) {
                    pin_param(out, *p, t)?;
                }
            }
            (other, Expr::Param(p)) => {
                if let Some(t) = add_side_hint(other, tbl) {
                    pin_param(out, *p, t)?;
                }
            }
            _ => {}
        }
        // Both sides params with no other info: default to integer
        // (documented v0.2 inference rule for `$1 + $2`).
        if matches!(&**a, Expr::Param(_)) && matches!(&**b, Expr::Param(_)) {
            for p in [match &**a {
                Expr::Param(p) => *p,
                _ => unreachable!(),
            }, match &**b {
                Expr::Param(p) => *p,
                _ => unreachable!(),
            }] {
                let i = (p - 1) as usize;
                if out[i].is_none() {
                    out[i] = Some(ColType::Int);
                }
            }
        }
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
    if let Stmt::Insert { table, columns, rows } = stmt {
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
    if let Stmt::Update { table, sets, where_ } = stmt {
        if let Some(t) = eng.db.find_table(table, snap, own) {
            for (col, expr) in sets {
                if let Expr::Param(p) = expr {
                    if let Some(i) = t.column_index(col) {
                        pin_param(&mut out, *p, t.columns[i].1.clone())?;
                    }
                } else {
                    infer_expr(expr, Some(t), &mut out)?;
                }
            }
            infer_where(where_, Some(t), &mut out)?;
        }
    }
    match stmt {
        Stmt::Select { table, where_, items, .. } => {
            let tbl = infer_table(eng, table, snap, own);
            infer_where(where_, tbl, &mut out)?;
            for item in items {
                if let SelectItem::Expr(e) = item {
                    infer_expr(e, tbl, &mut out)?;
                }
            }
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
            format!(
                "unsupported type OID {} for parameter ${}",
                oid, param_no
            ),
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
                ))
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
            exec_err("08P01", "bind message supplies fewer parameters than required")
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
            let s = std::str::from_utf8(bytes).map_err(|_| {
                exec_err("22021", "invalid byte sequence for encoding \"UTF8\"")
            })?;
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
        Stmt::Select { items, where_, .. } => {
            for item in items {
                if let SelectItem::Expr(e) = item {
                    subst_expr(e, params)?;
                }
            }
            subst_where(where_, params)
        }
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
        Expr::Add(a, b) => {
            subst_expr(a, params)?;
            subst_expr(b, params)?;
        }
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
        Stmt::Select { .. } => {
            let eff = resolve_param_types(stmt, declared, eng, snap, own)?;
            let mut s = stmt.clone();
            let dummy: Vec<Option<Value>> =
                eff.iter().map(|t| Some(dummy_value(t))).collect();
            subst_params(&mut s, &dummy)?;
            if let Stmt::Select { items, table, .. } = &s {
                let tbl = match table {
                    Some(name) => Some(eng.db.find_table(name, snap, own).ok_or_else(|| {
                        exec_err("42P01", format!("relation \"{}\" does not exist", name))
                    })?),
                    None => None,
                };
                let mut cols = Vec::new();
                for item in items {
                    match item {
                        SelectItem::All => {
                            let t = tbl.ok_or_else(|| {
                                exec_err("42601", "syntax error: SELECT * requires FROM")
                            })?;
                            for (n, ty) in &t.columns {
                                cols.push((n.clone(), ty.clone()));
                            }
                        }
                        SelectItem::Expr(e) => {
                            let (n, ty) = expr_desc(e, tbl)?;
                            cols.push((n, ty));
                        }
                    }
                }
                Ok(Some(cols))
            } else {
                unreachable!("subst_params cannot change the statement kind")
            }
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Table;

    fn engine() -> Engine {
        let mut eng = Engine::new();
        eng.db.tables.insert(
            "t".to_string(),
            vec![Table::new(
                vec![
                    ("a".to_string(), ColType::Int),
                    ("b".to_string(), ColType::Text),
                ],
                1,
            )],
        );
        eng.txns.next_xid = 5;
        eng
    }

    fn ctx<'a>(
        snap: &'a Snapshot,
        writes: &'a mut Vec<WriteOp>,
    ) -> StmtCtx<'a> {
        StmtCtx {
            snap,
            own: 4,
            level: IsolationLevel::ReadCommitted,
            writes,
        }
    }

    fn insert_stmt() -> Stmt {
        Stmt::Insert {
            table: "t".to_string(),
            columns: None,
            rows: vec![
                vec![
                    InsertValue::Lit(Literal::Int(1)),
                    InsertValue::Lit(Literal::Text("x".into())),
                ],
                vec![
                    InsertValue::Lit(Literal::Int(2)),
                    InsertValue::Lit(Literal::Text("y".into())),
                ],
            ],
        }
    }

    #[test]
    fn update_marks_xmax_and_appends() {
        let mut eng = engine();
        let snap = eng.take_snapshot();
        let mut writes = Vec::new();
        {
            let mut c = ctx(&snap, &mut writes);
            execute(&mut eng, &mut c, &insert_stmt()).unwrap();
        }
        // Commit the inserts so a second xid can see them.
        eng.txns.active.clear();
        let snap3 = Snapshot {
            active: vec![],
            next_xid: 6,
        };
        let mut writes2 = Vec::new();
        let mut c3 = StmtCtx {
            snap: &snap3,
            own: 4,
            level: IsolationLevel::ReadCommitted,
            writes: &mut writes2,
        };
        let upd = Stmt::Update {
            table: "t".to_string(),
            sets: vec![("a".to_string(), Expr::Literal(Literal::Int(11)))],
            where_: vec![WhereCond {
                col: "a".to_string(),
                rhs: WhereRhs::Lit(Literal::Int(1)),
            }],
        };
        match execute(&mut eng, &mut c3, &upd).unwrap() {
            ExecResult::Command { tag } => assert_eq!(tag, "UPDATE 1"),
            _ => panic!("expected command"),
        }
        let t = &eng.db.tables["t"][0];
        assert_eq!(t.rows.len(), 3); // 2 originals + 1 new version
        let new: Vec<_> = t.rows.iter().filter(|r| r.xmin == 4 && r.xmax == 0).collect();
        // one new version from the update (the two inserts also have xmin 4)
        assert!(new.iter().any(|r| r.values[0] == Value::Int(11)));
        let old = t.rows.iter().find(|r| r.values[0] == Value::Int(1)).unwrap();
        assert_eq!(old.xmax, 4);
    }

    #[test]
    fn delete_sets_xmax() {
        let mut eng = engine();
        let snap = eng.take_snapshot();
        let mut writes = Vec::new();
        {
            let mut c = ctx(&snap, &mut writes);
            execute(&mut eng, &mut c, &insert_stmt()).unwrap();
        }
        let snap2 = Snapshot {
            active: vec![],
            next_xid: 6,
        };
        let del = Stmt::Delete {
            table: "t".to_string(),
            where_: vec![WhereCond {
                col: "b".to_string(),
                rhs: WhereRhs::Lit(Literal::Text("y".into())),
            }],
        };
        let mut writes2 = Vec::new();
        let mut c = StmtCtx {
            snap: &snap2,
            own: 4,
            level: IsolationLevel::ReadCommitted,
            writes: &mut writes2,
        };
        match execute(&mut eng, &mut c, &del).unwrap() {
            ExecResult::Command { tag } => assert_eq!(tag, "DELETE 1"),
            _ => panic!("expected command"),
        }
        // The deleted version is invisible to a fresh snapshot...
        let t = &eng.db.tables["t"][0];
        let dead = t.rows.iter().find(|r| r.values[1] == Value::Text("y".into())).unwrap();
        assert_eq!(dead.xmax, 4);
        // ...but still visible to the deleter's own snapshot rules? No:
        // xmax == own hides it from self too.
        assert!(!row_visible(dead, &snap2, 4));
    }

    #[test]
    fn write_conflict_rr_is_40001() {
        let mut eng = engine();
        // Committed row (xmin=1).
        eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
            id: 1,
            values: vec![Value::Int(1), Value::Text("x".into())],
            xmin: 1,
            xmax: 0,
        });
        // Snapshot taken by xid 4 while deleter 7 is still active.
        let snap = Snapshot {
            active: vec![4, 7],
            next_xid: 8,
        };
        // Deleter 7 commits (leaves active).
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = 7;
        eng.txns.next_xid = 8;
        let del = Stmt::Delete {
            table: "t".to_string(),
            where_: vec![],
        };
        let mut writes = Vec::new();
        let mut c = StmtCtx {
            snap: &snap,
            own: 4,
            level: IsolationLevel::RepeatableRead,
            writes: &mut writes,
        };
        let err = execute(&mut eng, &mut c, &del).unwrap_err();
        assert_eq!(err.code, "40001");
    }
}
