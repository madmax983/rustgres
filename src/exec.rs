//! Executor: runs the parsed AST against in-memory storage.
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

use crate::sql::{Expr, InsertValue, Literal, SelectItem, Stmt, WhereCond, WhereRhs};
use crate::storage::{ColType, Database, Table, Value};

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

/// Outcome of executing one statement.
pub enum ExecResult {
    /// Rows to return: (column name, column type) + row values.
    Select {
        columns: Vec<(String, ColType)>,
        rows: Vec<Vec<Value>>,
    },
    /// Tag for CommandComplete, e.g. "INSERT 0 2".
    Command { tag: String },
}

pub fn execute(db: &mut Database, stmt: &Stmt) -> Result<ExecResult, ExecError> {
    match stmt {
        Stmt::CreateTable { name, columns } => exec_create(db, name, columns),
        Stmt::Insert {
            table,
            columns,
            rows,
        } => exec_insert(db, table, columns, rows),
        Stmt::Select {
            items,
            table,
            where_,
            limit,
        } => exec_select(db, items, table, where_, *limit),
        Stmt::DropTable { if_exists, name } => exec_drop(db, name, *if_exists),
        // Transaction control never reaches the executor (server.rs
        // intercepts it); reaching here is a bug in the session layer.
        Stmt::Begin
        | Stmt::Commit
        | Stmt::Rollback
        | Stmt::Savepoint { .. }
        | Stmt::RollbackTo { .. }
        | Stmt::Release { .. } => Err(exec_err(
            "25001",
            "transaction control statements must go through the session",
        )),
    }
}

fn exec_create(
    db: &mut Database,
    name: &str,
    columns: &[(String, ColType)],
) -> Result<ExecResult, ExecError> {
    if db.tables.contains_key(name) {
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
    db.tables.insert(
        name.to_string(),
        Table {
            columns: columns.to_vec(),
            rows: Vec::new(),
        },
    );
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

fn exec_insert(
    db: &mut Database,
    table: &str,
    columns: &Option<Vec<String>>,
    rows: &[Vec<InsertValue>],
) -> Result<ExecResult, ExecError> {
    let t = db.tables.get_mut(table).ok_or_else(|| {
        exec_err("42P01", format!("relation \"{}\" does not exist", table))
    })?;
    // Resolve the target column indexes up front.
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
    let mut new_rows = Vec::with_capacity(rows.len());
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
            // Parameters are substituted before execution; a leftover
            // Param here means it was never bound.
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
        new_rows.push(values);
    }
    let n = new_rows.len();
    t.rows.extend(new_rows);
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
fn expr_desc(e: &Expr, t: Option<&Table>) -> Result<(String, ColType), ExecError> {
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
    t: Option<&Table>,
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
    t: Option<&Table>,
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

fn exec_select(
    db: &mut Database,
    items: &[SelectItem],
    table: &Option<String>,
    where_: &[WhereCond],
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
            Ok(ExecResult::Select {
                columns,
                rows: vec![row],
            })
        }
        Some(tname) => {
            let t = db.tables.get(tname).ok_or_else(|| {
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
            // Filter + project.
            let mut rows = Vec::new();
            'rowloop: for r in &t.rows {
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
                    if !value_matches(&r[i], lit)? {
                        continue 'rowloop;
                    }
                }
                let mut out = Vec::with_capacity(columns.len());
                for item in items {
                    match item {
                        SelectItem::All => out.extend(r.iter().cloned()),
                        SelectItem::Expr(e) => out.push(eval_expr(e, Some(t), Some(r))?),
                    }
                }
                rows.push(out);
            }
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

fn exec_drop(db: &mut Database, name: &str, if_exists: bool) -> Result<ExecResult, ExecError> {
    if db.tables.remove(name).is_none() && !if_exists {
        return Err(exec_err(
            "42P01",
            format!("table \"{}\" does not exist", name),
        ));
    }
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
fn add_side_hint(e: &Expr, tbl: Option<&Table>) -> Option<ColType> {
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
    tbl: Option<&Table>,
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

/// Infer each `$N`'s type from usage context (one entry per param, 1-based).
/// `WHERE col = $N` pins the column's type; `$N + <typed>` pins the other
/// side's type; `$N + $M` with no other info defaults both to integer.
/// In `INSERT ... VALUES ($N, ...)`, a param takes its target column's type.
/// Params with no constraint stay None (callers default them to text).
pub fn infer_param_types(
    stmt: &Stmt,
    db: &Database,
) -> Result<Vec<Option<ColType>>, ExecError> {
    let n = stmt.max_param();
    let mut out: Vec<Option<ColType>> = vec![None; n];
    if let Stmt::Insert { table, columns, rows } = stmt {
        if let Some(t) = db.tables.get(table) {
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
    if let Stmt::Select { items, table, where_, .. } = stmt {
        let tbl = match table {
            Some(name) => db.tables.get(name),
            None => None,
        };
        for w in where_ {
            if let WhereRhs::Param(p) = w.rhs {
                if let Some(t) = tbl {
                    if let Some(i) = t.column_index(&w.col) {
                        pin_param(&mut out, p, t.columns[i].1.clone())?;
                    }
                }
            }
        }
        for item in items {
            if let SelectItem::Expr(e) = item {
                infer_expr(e, tbl, &mut out)?;
            }
        }
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
    db: &Database,
) -> Result<Vec<ColType>, ExecError> {
    let inferred = infer_param_types(stmt, db)?;
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
    db: &Database,
) -> Result<Vec<Option<Value>>, ExecError> {
    let types = resolve_param_types(stmt, declared, db)?;
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
            for w in where_ {
                if let WhereRhs::Param(p) = w.rhs {
                    w.rhs = WhereRhs::Lit(param_literal(p, params)?);
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
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
    db: &Database,
) -> Result<Option<Vec<(String, ColType)>>, ExecError> {
    match stmt {
        Stmt::Select { .. } => {
            let eff = resolve_param_types(stmt, declared, db)?;
            let mut s = stmt.clone();
            let dummy: Vec<Option<Value>> =
                eff.iter().map(|t| Some(dummy_value(t))).collect();
            subst_params(&mut s, &dummy)?;
            if let Stmt::Select { items, table, .. } = &s {
                let tbl = match table {
                    Some(name) => Some(db.tables.get(name).ok_or_else(|| {
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
