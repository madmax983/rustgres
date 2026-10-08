// v1.78 mechanical split: moved verbatim from src/exec.rs (4478-9073).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

pub(crate) fn assign_err(col_name: &str, col_type: &ColType, from: &str) -> ExecError {
    exec_err(
        "42804",
        format!(
            "column \"{}\" is of type {} but expression is of type {}",
            col_name,
            // v0.35: show the typmod for character columns.
            col_type.typmod_display(),
            from
        ),
    )
}

/// Coerce an INSERT literal to the target column type. SQL literals
/// start life as Postgres' "unknown" type, so int/float/text literals
/// coerce generously (with range checks); typed literals (DATE '...'
/// etc.) and anything else go through the strict value path.
pub(crate) fn coerce_literal(
    lit: &Literal,
    col_type: &ColType,
    col_name: &str,
) -> Result<Value, ExecError> {
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
            // v0.60: a numeric(p,s) target applies PG19's typmod
            // (round to scale, overflow check).
            ColType::Numeric(tm) => crate::storage::Numeric::parse(s)
                .map_err(|_| {
                    exec_err(
                        "22P02",
                        format!("invalid input syntax for type numeric: {:?}", s),
                    )
                })
                .and_then(|n| apply_numeric_typmod(n, *tm))
                .map(Value::Numeric),
            ColType::Float4 => s
                .parse::<f64>()
                .map(|f| Value::Float4(f as f32))
                .map_err(|_| assign_err(col_name, col_type, lit.type_name())),
            ColType::Float => s
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| assign_err(col_name, col_type, lit.type_name())),
            ColType::Text => Ok(Value::text(s.as_str())),
            _ => Err(assign_err(col_name, col_type, lit.type_name())),
        },
        // Unknown-type text literal: through the type's input function
        // (so INSERT INTO d VALUES ('2026-01-01') works for dates).
        // v0.35: character targets use assignment (input-function)
        // semantics: blank-tolerant truncation, 22001 on non-blank excess.
        Literal::Text(s) => {
            let v = Value::text(s.clone());
            let r = match col_type {
                ColType::Char(_) | ColType::Varchar(_) => eval_assign_cast(&v, col_type),
                _ => eval_cast(&v, *col_type),
            };
            r.map_err(|e| {
                if e.code == "42846" {
                    assign_err(col_name, col_type, lit.type_name())
                } else {
                    e
                }
            })
        }
        // Unknown-type boolean literal.
        Literal::Bool(b) => match col_type {
            ColType::Bool => Ok(Value::Bool(*b)),
            ColType::Text => Ok(Value::text(if *b { "true" } else { "false" })),
            _ => Err(assign_err(col_name, col_type, lit.type_name())),
        },
        // Typed literals (DATE '...', BYTEA '...', ...): strict.
        other => coerce_value(other.clone().into_value(), col_type, col_name),
    }
}

/// Integer literal (unknown type) to a column: any int kind with a
/// range check, float/numeric widening, or text rendering.
pub(crate) fn coerce_int_lit(
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
        ColType::Text => Ok(Value::text(i.to_string())),
        _ => range(i),
    }
}

/// Float literal (unknown type) to a column.
pub(crate) fn coerce_float_lit(
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
        ColType::Numeric(tm) => Numeric::from_f64(f)
            .map_err(|_| exec_err("22003", "value out of range for type numeric"))
            .and_then(|n| apply_numeric_typmod(n, *tm))
            .map(Value::Numeric),
        ColType::Text => Ok(Value::text(Value::Float(f).to_text().unwrap_or_default())),
        _ => Err(assign_err(col_name, col_type, from)),
    }
}

/// v0.60: PG19 `apply_typmod` for assignment to `numeric(p,s)`
/// (src/backend/utils/adt/numeric.c): round to the declared scale,
/// then error 22003 ("numeric field overflow") when the value needs
/// more than precision - scale digits left of the decimal point.
pub(crate) fn apply_numeric_typmod(
    n: Numeric,
    tm: Option<(u32, i32)>,
) -> Result<Numeric, ExecError> {
    let (p, s) = match tm {
        None => return Ok(n),
        Some(t) => t,
    };
    n.apply_typmod(p, s).map_err(|e| {
        let detail = match e {
            crate::storage::TypmodError::Infinite => format!(
                "a field with precision {}, scale {} cannot hold an infinite value",
                p, s
            ),
            crate::storage::TypmodError::Overflow => {
                let bound = if (p as i64) - (s as i64) > 0 {
                    format!("10^{}", (p as i64) - (s as i64))
                } else {
                    "1".to_string()
                };
                format!(
                    "a field with precision {}, scale {} must round to an absolute value less than {}",
                    p, s, bound
                )
            }
        };
        exec_err("22003", format!("numeric field overflow: {}", detail))
    })
}

/// Numeric literal (unknown type, only via params) to a column.
pub(crate) fn coerce_numeric_lit(
    n: &Numeric,
    col_type: &ColType,
    col_name: &str,
) -> Result<Value, ExecError> {
    match col_type {
        ColType::Numeric(tm) => apply_numeric_typmod(n.clone(), *tm).map(Value::Numeric),
        ColType::Float4 => Ok(Value::Float4(n.to_f64() as f32)),
        ColType::Float => Ok(Value::Float(n.to_f64())),
        ColType::Text => Ok(Value::text(n.to_text())),
        _ => Err(assign_err(col_name, col_type, "numeric")),
    }
}

/// Coerce an evaluated UPDATE value to the target column type: same
/// type, numeric widening, or text through the type's input function.
/// Narrowing a computed value needs an explicit cast (like Postgres).
pub(crate) fn coerce_value(
    v: Value,
    col_type: &ColType,
    col_name: &str,
) -> Result<Value, ExecError> {
    if v == Value::Null || v.col_type() == *col_type {
        return Ok(v);
    }
    // v0.81: a Record value (from ROW(...) or a named-composite cast)
    // assigns into a Composite column directly — the fields were
    // already coerced at cast time.
    if matches!(v, Value::Record(_)) && matches!(col_type, ColType::Composite) {
        return Ok(v);
    }
    // v0.65: narrowing integer assignments (PG's assignment casts, e.g.
    // nextval()'s bigint into a serial integer column). Overflow is
    // PG19's 22003, not a type mismatch.
    match (&v, col_type) {
        (Value::BigInt(i), ColType::SmallInt) => {
            return i16::try_from(*i)
                .map(Value::SmallInt)
                .map_err(|_| exec_err("22003", "smallint out of range"));
        }
        (Value::BigInt(i), ColType::Int) => {
            return i32::try_from(*i)
                .map(|x| Value::Int(x as i64))
                .map_err(|_| exec_err("22003", "integer out of range"));
        }
        (Value::Int(i), ColType::SmallInt) => {
            return i16::try_from(*i)
                .map(Value::SmallInt)
                .map_err(|_| exec_err("22003", "smallint out of range"));
        }
        _ => {}
    }
    let widened = match (&v, col_type) {
        (Value::SmallInt(i), ColType::Int) => Some(Value::Int(*i as i64)),
        (Value::SmallInt(i), ColType::BigInt) => Some(Value::BigInt(*i as i64)),
        (Value::SmallInt(i), ColType::Float4) => Some(Value::Float4(*i as f32)),
        (Value::SmallInt(i), ColType::Float) => Some(Value::Float(*i as f64)),
        (Value::SmallInt(i), ColType::Numeric(_)) => {
            Some(Value::Numeric(Numeric::from_i64(*i as i64)))
        }
        (Value::Int(i), ColType::BigInt) => Some(Value::BigInt(*i)),
        (Value::Int(i), ColType::Float4) => Some(Value::Float4(*i as f32)),
        (Value::Int(i), ColType::Float) => Some(Value::Float(*i as f64)),
        (Value::Int(i), ColType::Numeric(_)) => Some(Value::Numeric(Numeric::from_i64(*i))),
        (Value::BigInt(i), ColType::Float) => Some(Value::Float(*i as f64)),
        (Value::BigInt(i), ColType::Numeric(_)) => Some(Value::Numeric(Numeric::from_i64(*i))),
        (Value::Float4(f), ColType::Float) => Some(Value::Float(*f as f64)),
        (Value::Float4(f), ColType::Numeric(_)) => {
            Numeric::from_f64(*f as f64).ok().map(Value::Numeric)
        }
        (Value::Float(f), ColType::Numeric(_)) => Numeric::from_f64(*f).ok().map(Value::Numeric),
        // v0.60: assigning a numeric value to numeric(p,s) applies PG19's typmod.
        (Value::Numeric(n), ColType::Numeric(_)) => Some(Value::Numeric(n.clone())),
        (Value::Numeric(n), ColType::Float4) => Some(Value::Float4(n.to_f64() as f32)),
        (Value::Numeric(n), ColType::Float) => Some(Value::Float(n.to_f64())),
        _ => None,
    };
    if let Some(w) = widened {
        // v0.60: assignment to numeric(p,s) applies PG19's typmod
        // (round to scale, then check precision).
        if let (Value::Numeric(n), ColType::Numeric(tm)) = (&w, col_type) {
            return apply_numeric_typmod(n.clone(), *tm).map(Value::Numeric);
        }
        return Ok(w);
    }
    // Text goes through the type's input function (assignment cast).
    // v0.35: character targets use assignment semantics; bpchar values
    // coerce like text.
    if matches!(v, Value::Text(_) | Value::BpChar(_))
        || matches!(col_type, ColType::Char(_) | ColType::Varchar(_))
    {
        return eval_assign_cast(&v, col_type).map_err(|e| {
            if e.code == "42846" {
                assign_err(col_name, col_type, &v.type_name())
            } else {
                e
            }
        });
    }
    Err(assign_err(col_name, col_type, &v.type_name()))
}

/// v0.10: materialize a DML statement's WITH list. DML bodies cannot
/// reference the CTEs (except through subqueries in UPDATE's SET/WHERE or
/// the RETURNING list), but the CTEs are still evaluated — like Postgres,
/// which runs them for their side effects and validation.
pub(crate) fn materialize_dml_ctes(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    with: &[CteDef],
) -> Result<Vec<Rc<CteBinding>>, ExecError> {
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: ctx.read_only,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: None,
        write: Some(qwrite_from_ctx(
            &mut *ctx.writes,
            ctx.write_xid,
            ctx.level,
            ctx.default_toast_compression,
        )),
    };
    materialize_ctes(&mut q, with)?;
    Ok(q.ctes)
}

/// v0.10: output column names/types for a RETURNING list, resolved
/// against the target table's columns. `qual` is the visible qualifier
/// for column references; it differs from `table` only when DELETE
/// carries an alias (v0.65) or UPDATE carries one (v0.76). `extra` holds
/// the UPDATE ... FROM / DELETE ... USING source schemas (v0.76, PG19:
/// RETURNING may reference those tables); they are flattened ahead of
/// the target schema into a single scope, exactly like the runtime's
/// combined evaluation schemas, so ambiguity (42702) and resolution
/// agree between Describe and execution.
/// v1.22: `describe_returning` now expands `RETURNING *` /
/// `RETURNING qual.*` (PG19) to the target table's columns, in order,
/// and returns the expanded item list alongside the column descriptors
/// so execution projects the same list.
pub(crate) fn describe_returning(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    table: &str,
    qual: &str,
    extra: &[Vec<QCol>],
    returning: &[SelectItem],
) -> Result<(Vec<(String, ColType)>, Vec<SelectItem>), ExecError> {
    let t = eng
        .db
        .find_table(table, snap, &[own], session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
    // v1.22: expand `*` / `qual.*` to explicit column references.
    let mut expanded: Vec<SelectItem> = Vec::with_capacity(returning.len());
    for item in returning {
        match item {
            SelectItem::All => {
                for (n, _) in &t.columns {
                    expanded.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(qual.to_string()),
                            name: n.clone(),
                        },
                        alias: None,
                    });
                }
            }
            SelectItem::AllOf(q) if q == qual => {
                for (n, _) in &t.columns {
                    expanded.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(qual.to_string()),
                            name: n.clone(),
                        },
                        alias: None,
                    });
                }
            }
            SelectItem::AllOf(_) => {
                return Err(exec_err(
                    "42601",
                    "RETURNING qual.* for a non-target table is not supported",
                ));
            }
            SelectItem::Expr { .. } => expanded.push(item.clone()),
        }
    }
    let mut combined: Vec<QCol> = Vec::new();
    for s in extra {
        combined.extend(s.iter().cloned());
    }
    combined.extend(t.columns.iter().map(|(n, ty)| QCol {
        qual: qual.to_string(),
        name: n.clone(),
        ty: *ty,

        hidden: false,
        src_ord: 0,
    }));
    let schemas: Vec<Vec<QCol>> = vec![combined];
    let refs: Vec<&[QCol]> = schemas.iter().map(|s| s.as_slice()).collect();
    let mut out = Vec::new();
    for item in &expanded {
        if let SelectItem::Expr { expr, alias } = item {
            let ty = expr_type(eng, snap, own, session, &refs, &[], &[], expr)?;
            let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
            out.push((name, ty));
        }
    }
    Ok((out, expanded))
}

/// v0.10: evaluate a RETURNING list against one affected row.
pub(crate) fn project_returning(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    scopes: &[(&[QCol], &[Value])],
    // v1.40: per-row provenance for RETURNING system columns (see
    // `eval_dml_expr`). One entry per range in the scope; the
    // FROM/USING range (if any) carries row_id = u64::MAX.
    prov: &[RowProv],
    returning: &[SelectItem],
    ctes: &[Rc<CteBinding>],
    // v1.33: statement write context (see `eval_dml_expr`).
    mut write: Option<QWrite<'_>>,
) -> Result<Vec<Value>, ExecError> {
    let mut out = Vec::with_capacity(returning.len());
    for item in returning {
        if let SelectItem::Expr { expr, .. } = item {
            // v1.33: reborrow the write context per row (it is consumed
            // by the move into `eval_dml_expr`).
            let w = write.as_mut().map(QWrite::reborrow);
            out.push(eval_dml_expr(
                eng,
                snap,
                own,
                session,
                role,
                scopes,
                Some(prov),
                expr,
                ctes,
                None,
                w,
            )?);
        }
    }
    Ok(out)
}

/// v0.10: resolved `ON CONFLICT` arbiter.
pub(crate) struct UpsertPlan {
    /// (index name, key column positions) for every arbitrating unique
    /// index, in deterministic order.
    pub(crate) indexes: Vec<(String, Vec<usize>)>,
    /// Target column positions for DO UPDATE SET, in SET order.
    pub(crate) set_cols: Vec<usize>,
    pub(crate) action: ConflictAction,
    /// v0.71: the constraint behind each arbiter index, for mapping a
    /// partitioned target's arbiter to each leaf's backing index
    /// (`{leaf}_{constraint}`, the v0.70 naming convention). `None`
    /// for standalone unique indexes, which have no per-leaf
    /// enforcement in this engine.
    pub(crate) con_names: Vec<Option<String>>,
    /// v0.71: key column NAMES for each arbiter (parent column order).
    /// A partitioned target's arbiter maps to the leaf unique index
    /// with the same key column names — this covers PARTITION OF
    /// children (`{leaf}_{constraint}`) and ATTACHed partitions (whose
    /// indexes keep their own names).
    pub(crate) key_names: Vec<Vec<String>>,
    /// v0.71: whether the arbiter was explicit (columns or constraint
    /// name). DO NOTHING without an arbiter arbitrates every usable
    /// leaf constraint instead of the parent's index list.
    pub(crate) explicit_arbiter: bool,
}

pub(crate) fn plan_upsert(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    oc: &OnConflict,
    meta: &TableMeta,
) -> Result<UpsertPlan, ExecError> {
    // (index name, key column positions, key column names, backing
    // constraint name), sorted. v0.71: the constraint name maps a
    // partitioned target's arbiter to each leaf's backing index.
    // v0.22: temp tables have no backing indexes; their PRIMARY KEY /
    // UNIQUE constraints arbitrate ON CONFLICT directly.
    let t = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{table}\" does not exist")))?;
    // v0.71: partition children name constraint backing indexes
    // `{table}_{constraint}` (v0.70 convention); the constraint name is
    // the index name with that prefix stripped. Parents and plain
    // tables keep the historical `{constraint}` name.
    let is_child = t.partition.as_ref().is_some_and(|p| p.parent.is_some());
    let mut unique: Vec<(String, Vec<usize>, Vec<String>, Option<String>)> =
        if eng.db.is_temp_table(ctx.session, table) {
            let mut out = Vec::new();
            let mut push = |u: &UniqueDef| {
                let cols: Vec<usize> = u.cols.iter().filter_map(|c| t.column_index(c)).collect();
                if cols.len() == u.cols.len() {
                    out.push((u.name.clone(), cols, u.cols.clone(), Some(u.name.clone())));
                }
            };
            if let Some(pk) = &t.pkey {
                push(pk);
            }
            for u in &t.uniques {
                push(u);
            }
            out
        } else {
            eng.db
                .visible_indexes_for(table, ctx.snap, &ctx.all_xids, ctx.session)
                .into_iter()
                .filter(|ix| ix.def.unique && ix.def.planner_usable)
                .map(|ix| {
                    // v0.71: only constraint-backed indexes (internal)
                    // carry a constraint name for ON CONFLICT ON
                    // CONSTRAINT and partitioned leaf mapping. A
                    // standalone CREATE UNIQUE INDEX has no constraint
                    // and no per-leaf backing index.
                    let con = if !ix.def.internal {
                        None
                    } else if is_child {
                        ix.def
                            .name
                            .strip_prefix(&format!("{table}_"))
                            .map(|s| s.to_string())
                    } else {
                        Some(ix.def.name.clone())
                    };
                    (
                        ix.def.name.clone(),
                        ix.def.cols.clone(),
                        ix.def.col_names.clone(),
                        con,
                    )
                })
                .collect()
        };
    unique.sort_by(|a, b| a.0.cmp(&b.0));
    let no_arbiter = || {
        exec_err(
            "42P10",
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        )
    };
    let explicit_arbiter = !matches!(oc.arbiter, ConflictArbiter::None);
    let resolved: Vec<(String, Vec<usize>, Option<String>)> = match &oc.arbiter {
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
                .map(|(n, cols, _, con)| (n.clone(), cols.clone(), con.clone()))
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
                .find(|(_, _, cn, _)| {
                    let mut have: Vec<&str> = cn.iter().map(|s| s.as_str()).collect();
                    have.sort_unstable();
                    have == want
                })
                .map(|(n, cols, _, con)| vec![(n.clone(), cols.clone(), con.clone())])
                .ok_or_else(no_arbiter)?
        }
        // v0.71: match the constraint name as well as the index name, so
        // ON CONFLICT ON CONSTRAINT works against sub-partitioned
        // intermediates (whose backing indexes are `{table}_{constraint}`).
        // v0.71: only actual constraints match (con.is_some()); a
        // standalone CREATE UNIQUE INDEX is not a constraint (42P10).
        ConflictArbiter::Constraint(name) => unique
            .iter()
            .find(|(_, _, _, con)| con.as_deref() == Some(name.as_str()))
            .map(|(n, cols, _, con)| vec![(n.clone(), cols.clone(), con.clone())])
            .ok_or_else(no_arbiter)?,
    };
    let indexes: Vec<(String, Vec<usize>)> = resolved
        .iter()
        .map(|(n, cols, _)| (n.clone(), cols.clone()))
        .collect();
    let con_names: Vec<Option<String>> = resolved.iter().map(|(_, _, con)| con.clone()).collect();
    let key_names: Vec<Vec<String>> = resolved
        .iter()
        .map(|(_, cols, _)| {
            cols.iter()
                .map(|&p| meta.columns[p].0.clone())
                .collect::<Vec<_>>()
        })
        .collect();
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
        con_names,
        key_names,
        explicit_arbiter,
    })
}

/// v0.10: current values of one row version, by id.
pub(crate) fn row_values_by_id(eng: &Engine, ctx: &StmtCtx, table: &str, id: u64) -> Option<Row> {
    let t = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)?;
    let pos = t.row_pos(id)?;
    Some(t.rows[pos].values.clone())
}

/// v0.10: serialize an arbiter key for same-statement conflict tracking.
pub(crate) fn arbiter_key(values: &[Value], key_cols: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    for &c in key_cols {
        value_key(&values[c], &mut out);
        out.push(0xff);
    }
    out
}

/// v0.71: route one candidate row to its leaf for ON CONFLICT
/// arbitration (PG19: tuple routing happens before the arbiter index is
/// mapped to the leaf). Mirrors `route_partition_inserts` for a single
/// row: a leaf validates its bound; a sub-partitioned intermediate
/// validates its bound then routes to descendant leaves.
pub(crate) fn find_row_leaf(
    eng: &mut Engine,
    ctx: &StmtCtx,
    table: &str,
    values: &Row,
) -> Result<String, ExecError> {
    let (pinfo, table_cols) = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("target still visible; engine lock held throughout");
        match &t.partition {
            Some(p) => (p.clone(), t.columns.clone()),
            None => return Ok(table.to_string()),
        }
    };
    if pinfo.bound.is_some() {
        check_leaf_bound(eng, ctx, table, &pinfo, &table_cols, &values[..])?;
        if pinfo.children.is_empty() {
            return Ok(table.to_string());
        }
        // Sub-partitioned intermediate: fall through and route to
        // descendant leaves.
    }
    find_partition_leaf(eng, ctx, table, &pinfo, &table_cols, &values[..])
}

/// v0.71: remap a parent-order row to a leaf's column order (the inverse
/// of `remap_row_to_parent`).
pub(crate) fn remap_parent_to_leaf(
    parent_cols: &[(String, ColType)],
    leaf_cols: &[(String, ColType)],
    values: &[Value],
) -> Vec<Value> {
    leaf_cols
        .iter()
        .map(|(ln, _)| {
            parent_cols
                .iter()
                .position(|(pn, _)| pn == ln)
                .map(|i| values[i].clone())
                .unwrap_or(Value::Null)
        })
        .collect()
}

/// v0.71: per-leaf ON CONFLICT resolution for a partitioned target.
pub(crate) struct LeafUpsertCtx {
    /// The leaf's column list (for parent<->leaf remapping).
    pub(crate) cols: Vec<(String, ColType)>,
    /// The plan's explicit arbiter mapped to this leaf's backing
    /// indexes: (leaf index name, leaf key positions).
    pub(crate) arbiters: Vec<(String, Vec<usize>)>,
    /// Every unique index on the leaf, for DO NOTHING without an
    /// arbiter (PG19: the individual leaf partitions' constraints are
    /// considered, not the whole hierarchy's).
    pub(crate) all_unique: Vec<(String, Vec<usize>)>,
}

/// v0.71: build the per-leaf upsert context: map the parent arbiter
/// index to the corresponding leaf index (PG19), like
/// `get_partition_parent(indexOid, true)` inverted. A standalone
/// unique index on the parent has no per-leaf enforcement in this
/// engine, so an explicit arbiter that maps to one is an honest 42P10.
pub(crate) fn leaf_upsert_ctx(
    eng: &Engine,
    ctx: &StmtCtx,
    plan: &UpsertPlan,
    leaf: &str,
) -> Result<LeafUpsertCtx, ExecError> {
    let cols = {
        let lt = eng
            .db
            .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("leaf still visible; engine lock held throughout");
        lt.columns.clone()
    };
    let no_arbiter = || {
        exec_err(
            "42P10",
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        )
    };
    let mut arbiters = Vec::with_capacity(plan.key_names.len());
    if plan.explicit_arbiter {
        for (key_names, con) in plan.key_names.iter().zip(plan.con_names.iter()) {
            // v0.71: map the parent arbiter to the leaf unique index with
            // the same key column names. This covers PARTITION OF
            // children (v0.70 `{leaf}_{constraint}` naming) and ATTACHed
            // partitions (which keep their own index names). A standalone
            // parent unique index (con == None) has no per-leaf backing
            // index in this engine.
            let found = con.as_ref().and_then(|_| {
                eng.db
                    .visible_indexes_for(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                    .into_iter()
                    .filter(|ix| ix.def.unique && ix.def.planner_usable)
                    .find(|ix| ix.def.col_names == *key_names)
                    .map(|ix| (ix.def.name.clone(), ix.def.cols.clone()))
            });
            match found {
                Some(pair) => arbiters.push(pair),
                None => return Err(no_arbiter()),
            }
        }
    }
    let all_unique: Vec<(String, Vec<usize>)> = eng
        .db
        .visible_indexes_for(leaf, ctx.snap, &ctx.all_xids, ctx.session)
        .into_iter()
        .filter(|ix| ix.def.unique && ix.def.planner_usable)
        .map(|ix| (ix.def.name.clone(), ix.def.cols.clone()))
        .collect();
    Ok(LeafUpsertCtx {
        cols,
        arbiters,
        all_unique,
    })
}

/// v0.71: name a leaf backing index (`{leaf}_{constraint}`) by its
/// constraint in 23505 messages, like Postgres names the constraint
/// rather than the partition's index.
pub(crate) fn leaf_constraint_name(
    eng: &Engine,
    ctx: &StmtCtx,
    leaf: &str,
    index_name: &str,
) -> String {
    if let Some(con) = index_name.strip_prefix(&format!("{leaf}_")) {
        if let Some(t) = eng
            .db
            .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
        {
            let known = t.pkey.as_ref().is_some_and(|p| p.name == con)
                || t.uniques.iter().any(|u| u.name == con);
            if known {
                return con.to_string();
            }
        }
    }
    index_name.to_string()
}

/// v0.37: name of the toast table for a main-table OID, as PG names it
/// (`pg_toast.pg_toast_<oid>`).
pub(crate) fn toast_table_name(oid: u32) -> String {
    format!("pg_toast.pg_toast_{}", oid)
}

/// v0.37: find the visible toast table's name by its OID.
pub(crate) fn toast_table_name_by_oid(
    eng: &Engine,
    oid: u32,
    snap: &crate::storage::Snapshot,
    own: u64,
) -> Option<String> {
    eng.db
        .tables
        .iter()
        .filter_map(|(n, vs)| {
            vs.iter()
                .find(|v| v.oid == oid && crate::storage::table_visible(v, snap, &[own]))
                .map(|_| n.clone())
        })
        .next()
}

/// v0.37: ensure the toast table exists for `table_name`, creating it
/// (with its own OID) on first use, like PG's `toast_save_datum`
/// creating the toast relation lazily. Returns the toast table name.
/// v0.37: build the toast table value for a main-table OID. PG names it
/// `pg_toast.pg_toast_<oid>` and creates it at CREATE TABLE; toast
/// tables are never themselves toasted (all-PLAIN storage).
pub(crate) fn new_toast_table(toast_oid: u32, owner: &str, xmin: u64) -> Table {
    let mut tt = Table::new(
        vec![
            ("chunk_id".to_string(), ColType::Int),
            ("chunk_seq".to_string(), ColType::Int),
            ("chunk_data".to_string(), ColType::Bytea),
        ],
        xmin,
    );
    tt.oid = toast_oid;
    tt.owner = owner.to_string();
    // Toast tables are never themselves toasted.
    tt.col_storage = vec![
        toast_storage::PLAIN,
        toast_storage::PLAIN,
        toast_storage::PLAIN,
    ];
    tt
}

/// v0.37: create the toast table for a main table that has toastable
/// columns but no toast table yet, and link it via `toast_relid`. PG
/// creates the toast table at CREATE TABLE; for ALTER ADD COLUMN we
/// create it when the first toastable column appears. The toast
/// table's own CREATE is staged as a write op (like the main table's),
/// so ROLLBACK and WAL replay both see it. No-op when the table
/// already has one, has no toastable columns, or is a temp table
/// (temp tables are session-local and skip TOAST; values stay inline).
pub(crate) fn ensure_toast_table_eager(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table_name: &str,
    main_table: &mut Table,
) {
    if main_table.toast_relid != 0 {
        return;
    }
    if !main_table.columns.iter().any(|(_, ty)| ty.is_toastable()) {
        return;
    }
    if eng
        .db
        .temp_tables
        .get(&ctx.session)
        .is_some_and(|m| m.contains_key(table_name))
    {
        return;
    }
    let toast_oid = eng.db.alloc_oid();
    let toast_name = toast_table_name(main_table.oid);
    let tt = new_toast_table(toast_oid, &ctx.role, ctx.own);
    eng.db
        .tables
        .entry(toast_name.clone())
        .or_default()
        .push(tt);
    main_table.toast_relid = toast_oid;
    ctx.writes.push(WriteOp::CreateTable { name: toast_name });
}

pub(crate) fn ensure_toast_table(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table_name: &str,
) -> Result<String, ExecError> {
    let (oid, toast_relid) = {
        let t = eng
            .db
            .find_table(table_name, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| {
                exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", table_name),
                )
            })?;
        (t.oid, t.toast_relid)
    };
    if toast_relid != 0 {
        // Toast table already exists; find its name by OID.
        if let Some(name) = toast_table_name_by_oid(eng, toast_relid, ctx.snap, ctx.own) {
            return Ok(name);
        }
        // Fell through: toast_relid points nowhere (shouldn't happen).
    }
    // Create the toast table: (chunk_id oid, chunk_seq int4, chunk_data bytea).
    let toast_name = toast_table_name(oid);
    let toast_oid = eng.db.alloc_oid();
    let tt = new_toast_table(toast_oid, &ctx.role, ctx.own);
    eng.db
        .tables
        .entry(toast_name.clone())
        .or_default()
        .push(tt);
    // Record the toast table OID on the main table.
    {
        let t = eng
            .db
            .find_table_mut(table_name, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible");
        t.toast_relid = toast_oid;
    }
    ctx.writes.push(WriteOp::CreateTable {
        name: toast_name.clone(),
    });
    // v1.05: the reltoastrelid link is a catalog mutation like any
    // other — stage it as its own write op so ROLLBACK restores the
    // previous link and the WAL replays it (RGSWAL19). Without this the
    // main table's toast_relid would be lost on crash recovery.
    ctx.writes.push(WriteOp::SetToastRelid {
        table: table_name.to_string(),
        toast_relid: toast_oid,
        prev: 0,
    });
    Ok(toast_name)
}

/// v0.37: apply TOAST to a freshly inserted row. Plans storage
/// via `toast::plan_toast`, allocates value ids, records `toast_info`,
/// writes out-of-line chunks to the toast table, and stamps the row's
/// `toast` flags. Values stay detoasted in the main row (the flags are
/// metadata); chunks are derived data for `pg_class.reltoastrelid`.
pub(crate) fn toast_new_row(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table_name: &str,
    row_id: u64,
    values: &Row,
) -> Result<(), ExecError> {
    toast_row_impl(eng, ctx, table_name, row_id, values, None)
}

/// v1.05: apply TOAST to an UPDATE's new row version, following PG19's
/// `heap_update` → `heap_toast_insert_or_update(relation, newtup,
/// &oldtup)` (`heaptoast.c` / `toast_helper.c`, REL_19_STABLE):
/// - columns whose datum is unchanged keep the old toast value id
///   (PG's `TOASTCOL_IGNORE` fast path) — no new chunks are written
///   and the old ones are not deleted;
/// - changed columns are re-toasted with fresh value ids, and every
///   old value id not carried forward has its chunks deleted
///   (`toast_tuple_cleanup` → `toast_delete_datum`).
///
/// `old_values`/`old_toast` are the superseded version's detoasted
/// values and per-column toast flags in table column order. Both the
/// new chunks and the deletions are staged as `WriteOp`s, so ROLLBACK
/// restores the pre-update state exactly and the WAL replays it.
pub(crate) fn toast_update_row(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table_name: &str,
    row_id: u64,
    old_values: &Row,
    old_toast: &[u32],
    values: &Row,
) -> Result<(), ExecError> {
    toast_row_impl(
        eng,
        ctx,
        table_name,
        row_id,
        values,
        Some((old_values, old_toast)),
    )
}

pub(crate) fn toast_row_impl(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table_name: &str,
    row_id: u64,
    values: &Row,
    old: Option<(&Row, &[u32])>,
) -> Result<(), ExecError> {
    // v0.37: temp tables are session-local and skip TOAST (values stay
    // inline, always correct); they never get a toast table, so there
    // is nothing to spill to.
    if eng
        .db
        .temp_tables
        .get(&ctx.session)
        .is_some_and(|m| m.contains_key(table_name))
    {
        return Ok(());
    }
    // v0.41: compression uses the session's default_toast_compression
    // GUC unless the column has an explicit COMPRESSION method.
    const COMPRESS_OK: bool = true;
    // Phase 1: plan with a shared borrow. v1.05: updates also compute
    // the unchanged-column reuse mask (PG19 TOASTCOL_IGNORE).
    let (plan, reuse) = {
        let t = eng
            .db
            .find_table(table_name, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| {
                exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", table_name),
                )
            })?;
        let reuse: Vec<Option<bool>> = match &old {
            Some((old_values, old_toast)) => {
                crate::toast::toast_update_reuse(old_values, old_toast, values, &t.toast_info)
            }
            None => vec![None; values.len()],
        };
        let plan = crate::toast::plan_toast(
            t,
            values,
            COMPRESS_OK,
            ctx.default_toast_compression,
            &reuse,
        );
        (plan, reuse)
    };
    // v1.05: a reused column's preset plan is External, so the all-Plain
    // early return must not fire when any column is reused (phase 2
    // still has to stamp the carried-forward flags). It also must not
    // fire on UPDATE when the old row had out-of-line values: phase 4
    // has to delete those chunks transactionally (a toasted -> inline
    // UPDATE would otherwise leave them for the non-WAL-logged vacuum,
    // resurrecting them on crash recovery).
    let has_old_toast = old.is_some_and(|(_, old_toast)| old_toast.iter().any(|v| *v != 0));
    if !has_old_toast
        && reuse.iter().all(|r| r.is_none())
        && plan.iter().all(|p| *p == crate::toast::ToastPlan::Plain)
    {
        return Ok(());
    }
    // Phase 2: allocate value ids, record toast_info, collect chunks,
    // and stamp the row's flags (mutable table borrow).
    let (chunks, doomed): (Vec<(u32, Vec<u8>)>, Vec<u32>) = {
        let t = eng
            .db
            .find_table_mut(table_name, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible");
        let mut flags = vec![0u32; values.len()];
        let mut chunks = Vec::new();
        for (i, p) in plan.iter().enumerate() {
            // v1.05: unchanged column — keep the old value id, write no
            // chunks, record no toast_info (PG19 TOASTCOL_IGNORE).
            if reuse.get(i).copied().flatten().is_some() {
                flags[i] = old
                    .and_then(|(_, old_toast)| old_toast.get(i).copied())
                    .unwrap_or(0);
                continue;
            }
            if *p == crate::toast::ToastPlan::Plain {
                continue;
            }
            let vid = t.next_value_id;
            t.next_value_id += 1;
            let compressed = matches!(
                p,
                crate::toast::ToastPlan::Compressed | crate::toast::ToastPlan::CompressedExternal
            );
            // v0.41: the column's explicit method, else the session GUC
            // (PG resolves invalid/default attcompression at write time).
            let method = t
                .col_compression
                .get(i)
                .copied()
                .flatten()
                .unwrap_or(ctx.default_toast_compression);
            crate::toast::record_toast_info(t, vid, compressed, method);
            flags[i] = vid;
            if let Some(bytes) = crate::toast::out_of_line_bytes(values, i, *p, COMPRESS_OK, method)
            {
                chunks.push((vid, bytes));
            }
        }
        if let Some(pos) = t.row_pos(row_id) {
            if let Some(r) = t.rows.get_mut(pos) {
                r.toast = flags;
            }
        }
        // v1.05: old value ids not carried into the new version lose
        // their chunks (PG19 toast_tuple_cleanup → toast_delete_datum).
        let doomed: Vec<u32> = match &old {
            Some((_, old_toast)) => old_toast
                .iter()
                .enumerate()
                .filter(|(i, vid)| **vid != 0 && reuse.get(*i).copied().flatten().is_none())
                .map(|(_, vid)| *vid)
                .collect(),
            None => Vec::new(),
        };
        (chunks, doomed)
    };
    // Phase 3: write chunks to the toast table.
    // Pre-allocate chunk row ids (the table borrow below conflicts).
    let n_chunks: usize = chunks
        .iter()
        .map(|(_, b)| crate::toast::chunk_bytes(b).len())
        .sum();
    let mut chunk_ids = Vec::with_capacity(n_chunks);
    for _ in 0..n_chunks {
        chunk_ids.push(eng.alloc_row_id());
    }
    // v0.37: ensure the toast table exists if the table has any
    // toastable columns (like PG, which creates it at CREATE TABLE).
    // This sets reltoastrelid even when no chunks are needed yet.
    let has_toastable = {
        let t = eng
            .db
            .find_table(table_name, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible");
        t.columns.iter().any(|(_, ty)| ty.is_toastable())
    };
    if has_toastable {
        let toast_name = ensure_toast_table(eng, ctx, table_name)?;
        if !chunks.is_empty() {
            let tt = eng
                .db
                .find_table_mut(&toast_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("toast table just created");
            let mut id_iter = chunk_ids.into_iter();
            for (vid, bytes) in chunks {
                for (seq, chunk) in crate::toast::chunk_bytes(&bytes).iter().enumerate() {
                    let cid = id_iter.next().expect("pre-allocated");
                    tt.push_version(RowVersion::plain(
                        cid,
                        Row::new(vec![
                            Value::Int(vid as i64),
                            Value::Int(seq as i64),
                            Value::Bytea(chunk.to_vec()),
                        ]),
                        ctx.write_xid,
                    ));
                    // v0.37: chunks are real rows — stage the write op so
                    // ROLLBACK removes them and the WAL replays them.
                    ctx.writes.push(WriteOp::InsertRow {
                        table: toast_name.clone(),
                        row_id: cid,
                    });
                }
            }
        }
    }
    // Phase 4 (v1.05): delete the superseded value ids' chunks, like
    // PG19's toast_tuple_cleanup at heap_update time. Transactional and
    // WAL-logged (toast_delete_chunks stages DeleteRow ops), so unlike
    // the old commit-time vacuum reap the deletions survive crashes
    // and stay invisible to concurrent snapshots until commit.
    if !doomed.is_empty() {
        toast_delete_chunks(eng, ctx, table_name, &doomed)?;
    }
    Ok(())
}

/// v0.39: delete the out-of-line toast chunks for the given value ids.
/// PostgreSQL's `heap_delete` calls `heap_toast_delete` immediately when
/// the deleted tuple has external attributes (heapam.c); the chunk rows
/// are staged as `WriteOp::DeleteRow` so the deletions are transactional
/// (WAL-logged, restored on ROLLBACK by the DeleteRow undo). A no-op when
/// the table has no toast table or none of the value ids is toasted.
pub(crate) fn toast_delete_chunks(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table_name: &str,
    vids: &[u32],
) -> Result<(), ExecError> {
    if vids.is_empty() {
        return Ok(());
    }
    let toast_name = {
        let t = match eng
            .db
            .find_table(table_name, ctx.snap, &ctx.all_xids, ctx.session)
        {
            Some(t) => t,
            None => return Ok(()),
        };
        if t.toast_relid == 0 {
            return Ok(());
        }
        match toast_table_name_by_oid(eng, t.toast_relid, ctx.snap, ctx.own) {
            Some(n) => n,
            None => return Ok(()),
        }
    };
    // Collect the live chunk rows first; the mutation below needs the
    // table mutably.
    let chunk_rows: Vec<(u64, u64)> =
        match eng
            .db
            .find_table(&toast_name, ctx.snap, &ctx.all_xids, ctx.session)
        {
            Some(tt) => tt
                .rows
                .iter()
                .filter(|r| r.xmax == 0)
                .filter_map(|r| match r.values.first() {
                    Some(Value::Int(vid)) if vids.contains(&(*vid as u32)) => Some((r.id, r.xmax)),
                    _ => None,
                })
                .collect(),
            None => return Ok(()),
        };
    if chunk_rows.is_empty() {
        return Ok(());
    }
    let tt = eng
        .db
        .find_table_mut(&toast_name, ctx.snap, &ctx.all_xids, ctx.session)
        .expect("toast table still visible; engine lock held throughout");
    for (cid, prev_xmax) in chunk_rows {
        let Some(pos) = tt.row_pos(cid) else {
            continue;
        };
        tt.rows[pos].xmax = ctx.write_xid;
        ctx.writes.push(WriteOp::DeleteRow {
            table: toast_name.clone(),
            row_id: cid,
            prev_xmax,
        });
    }
    Ok(())
}

/// v0.48: apply pre-built `(row id, row)` inserts to a table: push row
/// versions, record undo write ops, TOAST wide values, and maintain
/// indexes. Shared by INSERT and CREATE TABLE AS.
pub(crate) fn apply_row_inserts(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    inserts: &[(u64, Row)],
) -> Result<(), ExecError> {
    {
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible; engine lock held throughout");
        for (id, values) in inserts {
            t.push_version(RowVersion::plain(*id, values.clone(), ctx.write_xid));
            ctx.writes.push(WriteOp::InsertRow {
                table: table.to_string(),
                row_id: *id,
            });
        }
    }
    // v0.37: TOAST the new rows (separate borrows inside).
    for (id, values) in inserts {
        toast_new_row(eng, ctx, table, *id, values)?;
    }
    for (id, values) in inserts {
        eng.db.index_insert_row(table, *id, values, ctx.session);
    }
    Ok(())
}

/// v0.48: `CREATE TABLE ... AS <query>` (PG19 createas.c / intorel.c).
/// The query runs first — parse analysis and execution must succeed
/// before anything is created (statement atomicity). The table's
/// columns are inferred from the query's output names and types;
/// explicit aliases rename them positionally. Completes with
/// `SELECT <n>`, like PostgreSQL.
///
/// Honest deviation: `WITH NO DATA` still executes the query and
/// discards the rows (Postgres skips execution); observable only via
/// volatile functions in the query.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_table_as(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col_aliases: &[String],
    select: &SelectStmt,
    temp: bool,
    if_not_exists: bool,
    with_data: bool,
) -> Result<ExecResult, ExecError> {
    // Existence check first, like CREATE TABLE (42P07).
    if eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .is_some()
        || eng.db.find_view(name, ctx.snap, ctx.own).is_some()
    {
        if if_not_exists {
            // PG: NOTICE "relation already exists, skipping", tag SELECT 0.
            return Ok(ExecResult::Command {
                tag: "SELECT 0".to_string(),
            });
        }
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    // Run the query before creating anything.
    let mut lock_ids: Vec<(String, u64)> = Vec::new();
    let out = {
        let mut q = Q {
            eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: ctx.read_only,
            depth: 0,
            lock_ids: &mut lock_ids,
            ctes: Vec::new(),
            wctx: None,
            srf_vals: Vec::new(),
            priv_scopes: Vec::new(),
            hashed_exists: Rc::new(RefCell::new(HashMap::new())),
            immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
            plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
            hashed_in: Rc::new(RefCell::new(Vec::new())),
            pending_updates: None,
            write: Some(qwrite_from_ctx(
                &mut *ctx.writes,
                ctx.write_xid,
                ctx.level,
                ctx.default_toast_compression,
            )),
        };
        run_select(&mut q, select, &[])?
    };
    if !lock_ids.is_empty() {
        acquire_row_locks(eng, ctx.own, &lock_ids)?;
    }
    // Column names: explicit aliases rename positionally.
    let mut columns: Vec<(String, ColType)> = out.columns;
    if !col_aliases.is_empty() {
        if col_aliases.len() != columns.len() {
            return Err(exec_err(
                "42601",
                format!(
                    "table \"{}\" has {} columns available but {} specified",
                    name,
                    columns.len(),
                    col_aliases.len()
                ),
            ));
        }
        for ((n, _), a) in columns.iter_mut().zip(col_aliases.iter()) {
            *n = a.clone();
        }
    }
    // Duplicate names are 42701 — except repeated `?column?`, which
    // Postgres permits for unnamed expression outputs.
    {
        let mut seen = std::collections::HashSet::new();
        for (n, _) in &columns {
            if n != "?column?" && !seen.insert(n) {
                return Err(exec_err(
                    "42701",
                    format!("column \"{}\" specified more than once", n),
                ));
            }
        }
    }
    let ncols = columns.len();
    let def = crate::sql::TableDef {
        columns,
        composite_types: vec![None; ncols],
        domain_types: vec![None; ncols],
        domain_elem: vec![false; ncols],
        not_null: vec![false; ncols],
        defaults: vec![None; ncols],
        serial: vec![None; ncols],
        compression: vec![None; ncols],
        storage: vec![None; ncols],
        checks: Vec::new(),
        uniques: Vec::new(),
        pkey: None,
        fks: Vec::new(),
        partition: None,
        likes: Vec::new(),
        reloptions: Vec::new(),
        inherits: Vec::new(),
        deferred_not_null: Vec::new(),
    };
    // CTAS definitions never carry constraints; the effective def is discarded.
    let _ = create_table_from_def(eng, ctx, name, &def, temp)?;
    let n = if with_data { out.rows.len() } else { 0 };
    if with_data {
        let mut inserts: Vec<(u64, Row)> = Vec::with_capacity(out.rows.len());
        for row in out.rows {
            inserts.push((eng.alloc_row_id(), row));
        }
        apply_row_inserts(eng, ctx, name, &inserts)?;
    }
    Ok(ExecResult::Command {
        tag: format!("SELECT {}", n),
    })
}

/// v0.84: one evaluated indirection step on an INSERT target
/// (index expressions are evaluated once per row).
pub(crate) enum ResolvedIndirection {
    Index(Vec<i64>),
    Field(String),
}

/// v0.84: evaluate the index expressions of an INSERT indirection path.
/// PG19 coerces `A_Indices` to int4 via assignment coercion
/// (`array_index_to_i64`); a NULL subscript is 2202E.
pub(crate) fn resolve_insert_indirection(
    q: &mut Q,
    indir: &[InsertIndirection],
) -> Result<Vec<ResolvedIndirection>, ExecError> {
    let mut out = Vec::with_capacity(indir.len());
    for step in indir {
        match step {
            InsertIndirection::Index(exprs) => {
                let mut idxs = Vec::with_capacity(exprs.len());
                for e in exprs {
                    let v = eval_expr(q, &[], e)?;
                    if matches!(v, Value::Null) {
                        return Err(exec_err(
                            "2202E",
                            "array subscript in assignment must not be null",
                        ));
                    }
                    idxs.push(array_index_to_i64(&v)?);
                }
                out.push(ResolvedIndirection::Index(idxs));
            }
            InsertIndirection::Field(f) => out.push(ResolvedIndirection::Field(f.clone())),
            InsertIndirection::Slice => {
                return Err(exec_err(
                    "0A000",
                    "slice assignment in INSERT target list is not supported yet",
                ));
            }
        }
    }
    Ok(out)
}

/// v0.84: reverse of `ArrayElem::of` — the `ColType` of an array's
/// elements for INSERT indirection recursion. Typmods do not survive
/// the round trip (numeric/Char/Varchar lose (p,s)/n); assignment
/// coercion at the leaf still applies the value-level checks.
pub(crate) fn array_elem_coltype(e: ArrayElem) -> ColType {
    match e {
        ArrayElem::Bool => ColType::Bool,
        ArrayElem::Bytea => ColType::Bytea,
        ArrayElem::Bit => ColType::Bit, // v1.39
        ArrayElem::Tid => ColType::Tid, // v1.40
        ArrayElem::SingleChar => ColType::SingleChar,
        ArrayElem::Name => ColType::Name,
        ArrayElem::SmallInt => ColType::SmallInt,
        ArrayElem::Int => ColType::Int,
        ArrayElem::Text => ColType::Text,
        ArrayElem::Char => ColType::Char(None),
        ArrayElem::Varchar => ColType::Varchar(None),
        ArrayElem::BigInt => ColType::BigInt,
        ArrayElem::Float4 => ColType::Float4,
        ArrayElem::Float => ColType::Float,
        ArrayElem::Date => ColType::Date,
        ArrayElem::Timestamp => ColType::Timestamp,
        ArrayElem::Timestamptz => ColType::Timestamptz,
        ArrayElem::Numeric => ColType::Numeric(None),
        ArrayElem::Uuid => ColType::Uuid,
        ArrayElem::Regclass => ColType::Regclass,
        ArrayElem::Json => ColType::Json,
        ArrayElem::Record => ColType::Composite,
        ArrayElem::PgLsn => ColType::PgLsn,
        ArrayElem::Xid => ColType::Xid,
    }
}

/// v0.84: walk an INSERT indirection path from the column type to the
/// leaf target type. PG19 coerces the assigned value to the type at the
/// END of the indirection (`transformAssignedExpr`), so the row builder
/// coerces to this leaf type before the structural assignment.
/// Returns the leaf `ColType` plus the composite type name when the leaf
/// is (or is an array of) a named composite.
pub(crate) fn insert_leaf_type(
    eng: &Engine,
    ctype: &ColType,
    composite: Option<&str>,
    indir: &[InsertIndirection],
    col_name: &str,
    table: &str,
) -> Result<(ColType, Option<String>), ExecError> {
    let mut ct = ctype.clone();
    let mut comp = composite.map(|s| s.to_string());
    for step in indir {
        match step {
            InsertIndirection::Index(_) => match ct {
                ColType::Array(e) => {
                    if e != ArrayElem::Record {
                        comp = None;
                    }
                    ct = array_elem_coltype(e);
                }
                _ => {
                    return Err(exec_err(
                        "42804",
                        format!(
                            "column \"{}\" of relation \"{}\" cannot be subscripted",
                            col_name, table
                        ),
                    ));
                }
            },
            InsertIndirection::Field(fname) => {
                let tname = match ct {
                    ColType::Composite => comp.clone(),
                    _ => None,
                }
                .ok_or_else(|| {
                    exec_err(
                        "42804",
                        format!(
                            "column \"{}\" of relation \"{}\" is not a composite type",
                            col_name, table
                        ),
                    )
                })?;
                let fields = eng
                    .db
                    .types
                    .get(&tname)
                    .and_then(|st| st.composite.clone())
                    .ok_or_else(|| {
                        exec_err("42704", format!("type \"{}\" does not exist", tname))
                    })?;
                let (_, ftype, nested) =
                    fields.iter().find(|(n, _, _)| n == fname).ok_or_else(|| {
                        exec_err(
                            "42703",
                            format!("column \"{}\" of type \"{}\" does not exist", fname, tname),
                        )
                    })?;
                ct = ftype.clone();
                comp = nested.clone();
            }
            InsertIndirection::Slice => {
                return Err(exec_err(
                    "0A000",
                    "slice assignment in INSERT target list is not supported yet",
                ));
            }
        }
    }
    Ok((ct, comp))
}

/// v0.84: execute PG19 `transformAssignmentIndirection` — assign `val`
/// into `base` following `steps`. `base` is the column value accumulated
/// so far: `Value::Null` for a fresh INSERT row, because PG19 builds the
/// new column value from a NULL constant of the column type
/// (parse_target.c `transformAssignedExpr`), not from any existing
/// tuple. `val` is already coerced to the leaf type. Only 1-D array
/// expansion is supported; deeper shapes are an honest 0A000.
pub(crate) fn assign_insert_indirection(
    eng: &Engine,
    base: Value,
    steps: &[ResolvedIndirection],
    val: Value,
    ctype: &ColType,
    composite: Option<&str>,
    col_name: &str,
) -> Result<Value, ExecError> {
    let Some((first, rest)) = steps.split_first() else {
        return Ok(val);
    };
    match first {
        ResolvedIndirection::Index(idxs) => {
            let elem = match ctype {
                ColType::Array(e) => *e,
                _ => {
                    return Err(exec_err(
                        "42804",
                        format!("cannot subscript type {}", ctype.sql_name()),
                    ));
                }
            };
            if idxs.len() != 1 {
                return Err(exec_err(
                    "0A000",
                    "multi-dimensional subscript in INSERT target is not supported yet",
                ));
            }
            let mut arr = match base {
                Value::Null => ArrayVal {
                    elem,
                    dims: Vec::new(),
                    lower: Vec::new(),
                    elems: Vec::new(),
                },
                Value::Array(a) => *a,
                _ => {
                    return Err(exec_err(
                        "42804",
                        format!("cannot subscript type {}", base.type_name()),
                    ));
                }
            };
            // PG19 `array_set_element` bound expansion for 1-D arrays
            // (arbitrary lower bounds allowed, gaps fill with NULL).
            let idx = idxs[0];
            if arr.dims.is_empty() {
                arr.dims.push(1);
                arr.lower.push(idx as i32);
                arr.elems.push(Value::Null);
            } else {
                if arr.dims.len() != 1 {
                    return Err(exec_err(
                        "0A000",
                        "multi-dimensional array assignment in INSERT target is not supported yet",
                    ));
                }
                let l = arr.lower[0] as i64;
                let n = arr.dims[0] as i64;
                if idx < l {
                    let prepend = (l - idx) as usize;
                    let mut grown = Vec::with_capacity(arr.elems.len() + prepend);
                    grown.extend(std::iter::repeat(Value::Null).take(prepend));
                    grown.extend(arr.elems.drain(..));
                    arr.elems = grown;
                    arr.lower[0] = idx as i32;
                    arr.dims[0] += prepend as i32;
                } else if idx >= l + n {
                    let append = (idx - l - n + 1) as usize;
                    arr.elems
                        .extend(std::iter::repeat(Value::Null).take(append));
                    arr.dims[0] += append as i32;
                }
            }
            let pos = (idx - arr.lower[0] as i64) as usize;
            let elem_ct = array_elem_coltype(elem);
            let elem_comp = if elem == ArrayElem::Record {
                composite
            } else {
                None
            };
            let cur_elem = std::mem::replace(&mut arr.elems[pos], Value::Null);
            arr.elems[pos] =
                assign_insert_indirection(eng, cur_elem, rest, val, &elem_ct, elem_comp, col_name)?;
            Ok(Value::Array(Box::new(arr)))
        }
        ResolvedIndirection::Field(fname) => {
            let tname = match ctype {
                ColType::Composite => composite,
                _ => None,
            }
            .ok_or_else(|| {
                exec_err(
                    "42804",
                    format!(
                        "cannot access field of non-composite type {}",
                        ctype.sql_name()
                    ),
                )
            })?;
            let fields = eng
                .db
                .types
                .get(tname)
                .and_then(|st| st.composite.clone())
                .ok_or_else(|| exec_err("42704", format!("type \"{}\" does not exist", tname)))?;
            let (ftype, nested) = fields
                .iter()
                .find(|(n, _, _)| n == fname)
                .map(|(_, t, n)| (t.clone(), n.clone()))
                .ok_or_else(|| {
                    exec_err(
                        "42703",
                        format!("column \"{}\" of type \"{}\" does not exist", fname, tname),
                    )
                })?;
            let mut rec = match base {
                Value::Null => fields
                    .iter()
                    .map(|(n, _, _)| (n.clone(), Value::Null))
                    .collect::<Vec<_>>(),
                Value::Record(r) => r,
                _ => {
                    return Err(exec_err(
                        "42804",
                        format!("cannot access field of type {}", base.type_name()),
                    ));
                }
            };
            let ri = rec.iter().position(|(n, _)| n == fname).ok_or_else(|| {
                exec_err(
                    "42703",
                    format!("column \"{}\" of type \"{}\" does not exist", fname, tname),
                )
            })?;
            let cur = std::mem::replace(&mut rec[ri].1, Value::Null);
            rec[ri].1 = assign_insert_indirection(
                eng,
                cur,
                rest,
                val,
                &ftype,
                nested.as_deref(),
                col_name,
            )?;
            Ok(Value::Record(rec))
        }
    }
}

pub(crate) fn exec_insert(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    columns: &Option<Vec<InsertTarget>>,
    rows: &[Vec<InsertValue>],
    select: &Option<SelectStmt>,
    with: &[CteDef],
    on_conflict: &Option<OnConflict>,
    returning: &[SelectItem],
) -> Result<ExecResult, ExecError> {
    // v0.11: INSERT needs INSERT privilege on the table — or, with an
    // explicit column list, on each listed column (like PostgreSQL).
    // v0.84: privilege checks apply to the root column of each target
    // (PG checks the column, not the indirection path).
    if let Some(targets) = columns {
        let names: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
        require_column_privs(
            eng,
            ctx,
            table,
            &names,
            crate::storage::PRIV_INSERT,
            "INSERT",
        )?;
    } else {
        require_table_priv(eng, ctx, table, crate::storage::PRIV_INSERT, "INSERT")?;
    }
    // v0.12/v0.84: duplicate target columns are 42701 in PostgreSQL —
    // but only for whole-column duplicates. PG19
    // (transformInsertTargetList) allows repeated *partial* (indirection)
    // targets on the same column (`INSERT INTO t (f[1], f[2])`), and
    // rejects a whole-column target mixed with any other assignment to
    // the same column: `column "x" specified more than once`.
    if let Some(targets) = columns {
        let mut whole = std::collections::HashSet::new();
        let mut partial = std::collections::HashSet::new();
        for t in targets {
            if t.indirection.is_empty() {
                if !whole.insert(&t.name) || partial.contains(&t.name) {
                    return Err(exec_err(
                        "42701",
                        format!("column \"{}\" specified more than once", t.name),
                    ));
                }
            } else if whole.contains(&t.name) {
                return Err(exec_err(
                    "42701",
                    format!("column \"{}\" specified more than once", t.name),
                ));
            } else {
                partial.insert(&t.name);
            }
        }
    }
    // v0.10: WITH materialization (validated; plain INSERT cannot reference
    // the CTEs, but subqueries in RETURNING/ON CONFLICT can).
    let ctes = materialize_dml_ctes(eng, &mut *ctx, with)?;
    // v0.10: INSERT...SELECT: run the SELECT and convert rows to insert
    // values. The CTEs are already materialized above.
    let select_rows: Option<Vec<Row>> = if let Some(sel) = select {
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: ctx.read_only,
            depth: 0,
            lock_ids: &mut lock_ids,
            ctes: ctes.clone(),
            wctx: None,
            srf_vals: Vec::new(),
            priv_scopes: Vec::new(),
            hashed_exists: Rc::new(RefCell::new(HashMap::new())),
            immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
            plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
            hashed_in: Rc::new(RefCell::new(Vec::new())),
            pending_updates: None,
            write: Some(qwrite_from_ctx(
                &mut *ctx.writes,
                ctx.write_xid,
                ctx.level,
                ctx.default_toast_compression,
            )),
        };
        let out = run_select(&mut q, sel, &[])?;
        Some(out.rows)
    } else {
        None
    };
    // v0.10: resolve the ON CONFLICT arbiter before building rows (needs
    // the table metadata).
    let (meta_for_upsert, partitioned_upsert) = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        // v0.71: PG19 implements ON CONFLICT against partitioned tables
        // (route each row to its leaf, then arbitrate per leaf — see the
        // upsert loop below). Partitioned *temporary* tables stay
        // rejected: their constraints have no backing indexes to map.
        if on_conflict.is_some()
            && t.partition.is_some()
            && eng.db.is_temp_table(ctx.session, table)
        {
            return Err(exec_err(
                "0A000",
                "ON CONFLICT on a partitioned temporary table is not supported yet",
            ));
        }
        // A partitioned target that is not a leaf (a parent, a
        // sub-partitioned intermediate, or a partitioned table with no
        // children yet): rows route to leaves before conflict
        // arbitration. True leaves keep the single-table path.
        // (v0.72: `is_partitioned` is the honest discriminator — a
        // childless partitioned table is not a leaf.)
        let partitioned = t.partition.as_ref().is_some_and(|p| p.is_partitioned);
        (TableMeta::of(t), partitioned)
    };
    let upsert: Option<UpsertPlan> = match on_conflict {
        None => None,
        Some(oc) => Some(plan_upsert(eng, ctx, table, oc, &meta_for_upsert)?),
    };
    // Validate everything before mutating (statement atomicity).
    let new_rows: Vec<Row> = {
        let meta = {
            let t = eng
                .db
                .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                .ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", table))
                })?;
            TableMeta::of(t)
        };
        let ncols = meta.columns.len();
        // v0.65: PG's first-N rule (INSERT reference): with no column
        // list, a row shorter than the table targets the first N
        // columns; the rest take defaults. More expressions than
        // columns is still 42601.
        // v0.84: targets keep their indirection slice for the row
        // builder (`&[]` = whole column on the no-list path).
        let targets: Vec<(usize, &[InsertIndirection])> = match columns {
            Some(tgts) => tgts
                .iter()
                .map(|t| {
                    meta.columns
                        .iter()
                        .position(|(c, _)| c == &t.name)
                        .map(|idx| (idx, t.indirection.as_slice()))
                        .ok_or_else(|| {
                            exec_err(
                                "42703",
                                format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    t.name, table
                                ),
                            )
                        })
                })
                .collect::<Result<_, _>>()?,
            // v0.65: PG's first-N rule (INSERT reference): with no column
            // list, a row shorter than the table targets the first N
            // columns; the rest take defaults. More expressions than
            // columns is still 42601.
            None => {
                let width = if let Some(srows) = &select_rows {
                    srows.first().map(|r| r.len()).unwrap_or(0)
                } else {
                    rows.first().map(|r| r.len()).unwrap_or(0)
                };
                if width > ncols {
                    return Err(exec_err(
                        "42601",
                        "INSERT has more expressions than target columns".to_string(),
                    ));
                }
                (0..width)
                    .map(|i| (i, &[][..] as &[InsertIndirection]))
                    .collect()
            }
        };
        let mut built: Vec<Row> = Vec::new();
        // v0.10: INSERT...SELECT: validate column count and use the
        // SELECT's rows directly (already Values).
        if let Some(srows) = select_rows {
            // v0.84: indirection index expressions are row-independent —
            // resolve them once. Only pays for a Q when some target
            // actually carries indirection.
            let resolved: Vec<Vec<ResolvedIndirection>> =
                if targets.iter().any(|(_, indir)| !indir.is_empty()) {
                    let mut lock_ids = Vec::new();
                    let mut q = Q {
                        eng,
                        snap: ctx.snap,
                        own: ctx.own,
                        all_xids: ctx.all_xids.clone(),
                        session: ctx.session,
                        role: ctx.role,
                        read_only: ctx.read_only,
                        depth: 0,
                        lock_ids: &mut lock_ids,
                        ctes: ctes.clone(),
                        wctx: None,
                        srf_vals: Vec::new(),
                        priv_scopes: Vec::new(),
                        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
                        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
                        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
                        hashed_in: Rc::new(RefCell::new(Vec::new())),
                        pending_updates: None,
                        write: Some(qwrite_from_ctx(
                            &mut *ctx.writes,
                            ctx.write_xid,
                            ctx.level,
                            ctx.default_toast_compression,
                        )),
                    };
                    targets
                        .iter()
                        .map(|(_, indir)| resolve_insert_indirection(&mut q, indir))
                        .collect::<Result<Vec<_>, _>>()?
                } else {
                    targets.iter().map(|_| Vec::new()).collect()
                };
            for cells in srows {
                if cells.len() != targets.len() {
                    // v0.65: same width rule as VALUES above.
                    let msg = if columns.is_some() {
                        format!(
                            "INSERT has {} expressions but {} target columns",
                            cells.len(),
                            targets.len(),
                        )
                    } else {
                        "VALUES lists must all be the same length".to_string()
                    };
                    return Err(exec_err("42601", msg));
                }
                let mut values = vec![Value::Null; ncols];
                let mut explicit = vec![false; ncols];
                for (j, v) in cells.iter().enumerate() {
                    let (ti, indir) = targets[j];
                    let (cname, ctype) = &meta.columns[ti];
                    if !indir.is_empty() {
                        // v0.84: indirection target in INSERT...SELECT.
                        // SELECT rows are Values, so coerce to the leaf
                        // type (assignment coercion), then build from a
                        // NULL base like the VALUES path.
                        let comp = meta.composite_types.get(ti).and_then(|o| o.as_deref());
                        let (leaf_ct, _) = insert_leaf_type(eng, ctype, comp, indir, cname, table)?;
                        let coerced = match &leaf_ct {
                            ColType::Numeric(tm) => match v {
                                Value::Numeric(n) => {
                                    apply_numeric_typmod(n.clone(), *tm).map(Value::Numeric)?
                                }
                                _ => coerce_value(v.clone(), &leaf_ct, cname)?,
                            },
                            _ => coerce_value(v.clone(), &leaf_ct, cname)?,
                        };
                        let cur = std::mem::replace(&mut values[ti], Value::Null);
                        values[ti] = assign_insert_indirection(
                            eng,
                            cur,
                            &resolved[j],
                            coerced,
                            ctype,
                            comp,
                            cname,
                        )?;
                    } else {
                        // v0.60: INSERT...SELECT applies the numeric(p,s)
                        // typmod like any other assignment (PG's
                        // apply_typmod); the SELECT's rows are Values, not
                        // pre-coerced to the target type.
                        values[ti] = match ctype {
                            ColType::Numeric(tm) => match v {
                                Value::Numeric(n) => {
                                    apply_numeric_typmod(n.clone(), *tm).map(Value::Numeric)?
                                }
                                _ => v.clone(),
                            },
                            _ => v.clone(),
                        };
                    }
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
                built.push(Row::new(values));
            }
        } else {
            // v0.24: expression VALUES (`INSERT ... VALUES (repeat(...))`)
            // need an evaluation context; build it once for all rows.
            let mut lock_ids = Vec::new();
            let mut q = Q {
                eng,
                snap: ctx.snap,
                own: ctx.own,
                all_xids: ctx.all_xids.clone(),
                session: ctx.session,
                role: ctx.role,
                read_only: ctx.read_only,
                depth: 0,
                lock_ids: &mut lock_ids,
                ctes: ctes.clone(),
                wctx: None,
                srf_vals: Vec::new(),
                priv_scopes: Vec::new(),
                hashed_exists: Rc::new(RefCell::new(HashMap::new())),
                immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
                plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
                hashed_in: Rc::new(RefCell::new(Vec::new())),
                pending_updates: None,
                write: Some(qwrite_from_ctx(
                    &mut *ctx.writes,
                    ctx.write_xid,
                    ctx.level,
                    ctx.default_toast_compression,
                )),
            };
            built = Vec::with_capacity(rows.len());
            // v0.72: expand top-level set-returning calls in VALUES
            // (PG19 ROWS FROM: `VALUES (generate_series(1,3))` is three
            // rows). Width is unchanged by expansion, so the
            // targets/width checks above still apply.
            let rows = expand_insert_srf_rows(&mut q, rows)?;
            for row in &rows {
                if row.len() != targets.len() {
                    // v0.65: explicit lists need the exact count; with no
                    // list the first row set the width, so a mismatch means
                    // ragged VALUES lists (PG: 42601 "VALUES lists must all
                    // be the same length").
                    let msg = if columns.is_some() {
                        format!(
                            "INSERT has {} expressions but {} target columns",
                            row.len(),
                            targets.len()
                        )
                    } else {
                        "VALUES lists must all be the same length".to_string()
                    };
                    return Err(exec_err("42601", msg));
                }
                let mut values = vec![Value::Null; ncols];
                let mut explicit = vec![false; ncols];
                for (v, &(ci, indir)) in row.iter().zip(targets.iter()) {
                    let (cname, ctype) = &meta.columns[ci];
                    // v0.84: indirection targets (`f2[1]`, `f3.if2`).
                    // PG19 rejects DEFAULT into an indirection path
                    // with 0A000 before evaluating any default, coerces
                    // the value to the type at the END of the path, and
                    // builds the column value from a NULL base.
                    if !indir.is_empty() {
                        if matches!(v, InsertValue::Default) {
                            let msg = match &indir[0] {
                                InsertIndirection::Index(_) => {
                                    "cannot set an array element to DEFAULT"
                                }
                                _ => "cannot set a subfield to DEFAULT",
                            };
                            return Err(exec_err("0A000", msg));
                        }
                        let comp = meta.composite_types.get(ci).and_then(|o| o.as_deref());
                        let (leaf_ct, _leaf_comp) =
                            insert_leaf_type(q.eng, ctype, comp, indir, cname, table)?;
                        let steps = resolve_insert_indirection(&mut q, indir)?;
                        let raw = match v {
                            InsertValue::Lit(l) => coerce_literal(l, &leaf_ct, cname)?,
                            InsertValue::Param(n) => {
                                return Err(exec_err(
                                    "42P02",
                                    format!("there is no parameter ${}", n),
                                ));
                            }
                            InsertValue::Expr(e) => {
                                let val = eval_expr(&mut q, &[], e)?;
                                coerce_value(val, &leaf_ct, cname)?
                            }
                            InsertValue::Default => {
                                unreachable!("v0.84: DEFAULT with indirection rejected above")
                            }
                        };
                        let cur = std::mem::replace(&mut values[ci], Value::Null);
                        values[ci] =
                            assign_insert_indirection(q.eng, cur, &steps, raw, ctype, comp, cname)?;
                    } else {
                        values[ci] = match v {
                            InsertValue::Lit(l) => coerce_literal(l, ctype, cname)?,
                            InsertValue::Param(n) => {
                                return Err(exec_err(
                                    "42P02",
                                    format!("there is no parameter ${}", n),
                                ));
                            }
                            // v0.24: general expressions in VALUES — evaluate,
                            // then coerce to the column type like PG's
                            // assignment cast.
                            InsertValue::Expr(e) => {
                                let val = eval_expr(&mut q, &[], e)?;
                                coerce_value(val, ctype, cname)?
                            }
                            // v0.9: DEFAULT in VALUES applies the column default.
                            // v0.85: falls back to the domain's DEFAULT (PG19).
                            InsertValue::Default => {
                                let dd = domain_default(q.eng, &meta, ci).cloned();
                                match meta.defaults[ci].as_ref().or(dd.as_ref()) {
                                    Some(d) => eval_default(
                                        q.eng,
                                        ctx.snap,
                                        ctx.own,
                                        ctx.session,
                                        ctx.role,
                                        d,
                                        ctype,
                                        cname,
                                    )?,
                                    None => Value::Null,
                                }
                            }
                        };
                        // v0.85: bare ROW() into a composite (or
                        // domain-over-composite) column — coerce by
                        // position (PG19 assignment coercion).
                        let comp = meta.composite_types.get(ci).and_then(|o| o.as_deref());
                        let assigned = std::mem::replace(&mut values[ci], Value::Null);
                        values[ci] = coerce_assign_composite(q.eng, assigned, comp)?;
                    }
                    explicit[ci] = true;
                }
                // v0.9: fill defaults for columns not mentioned.
                for (i, d) in meta.defaults.iter().enumerate() {
                    if !explicit[i] {
                        // v0.85: fall back to the domain's DEFAULT (PG19).
                        let dd = domain_default(q.eng, &meta, i).cloned();
                        if let Some(d) = d.as_ref().or(dd.as_ref()) {
                            let (cname, ctype) = &meta.columns[i];
                            values[i] = eval_default(
                                q.eng,
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
                    q.eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    &meta,
                    table,
                    &values,
                )?;
                built.push(Row::new(values));
            }
        } // end else (VALUES path)
        // v0.8: statement-atomic UNIQUE enforcement — every row is checked
        // against the indexes and against earlier rows of this statement
        // before any version is pushed. v0.10: skipped when there is an
        // ON CONFLICT clause — conflicts are resolved per row instead.
        // v0.71: a partitioned target enforces each leaf's unique indexes
        // (the parent's own indexes hold no rows).
        if upsert.is_none() {
            let is_partitioned_parent = eng
                .db
                .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                .is_some_and(|t| t.partition.as_ref().is_some_and(|p| p.parent.is_none()));
            if is_partitioned_parent {
                check_partitioned_insert_unique(eng, ctx, table, &built)?;
            } else {
                check_insert_unique(&eng.db, table, &built, ctx.snap, &ctx.all_xids, ctx.session)?;
            }
        }
        // v0.9: child-side foreign keys. Rows inserted earlier in the same
        // statement are visible to later rows (self-references).
        for (i, values) in built.iter().enumerate() {
            check_fk_child_row(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                &meta,
                table,
                values,
                &built[..i],
                None,
            )?;
        }
        built
    };
    // v1.00: fire BEFORE INSERT FOR EACH ROW triggers on the target
    // table before ON CONFLICT planning and partition routing. PG19
    // fires the target's triggers first; the partition router fires
    // each leaf's triggers after routing (see route_partition_inserts).
    // Suppressed rows (RETURN NULL) never reach the arbiter or the
    // router.
    let new_rows: Vec<Row> = {
        let target_cols: Vec<(String, ColType)> = {
            let t = eng
                .db
                .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("target still visible");
            t.columns.clone()
        };
        let fired: Vec<Row> =
            fire_before_insert_triggers(eng, ctx, table, &target_cols, &new_rows)?
                .into_iter()
                .flatten()
                .collect();
        // v1.00: PG validates CHECK/NOT NULL after BEFORE triggers fire,
        // so re-validate the post-trigger rows (a trigger may have
        // broken a constraint, e.g. the mlparted11_trig fixture).
        if fired.len() != new_rows.len()
            || fired
                .iter()
                .zip(new_rows.iter())
                .any(|(a, b)| a[..] != b[..])
        {
            let meta = {
                let t = eng
                    .db
                    .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("target still visible");
                TableMeta::of(t)
            };
            for row in &fired {
                check_row_constraints(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    &meta,
                    table,
                    &row[..],
                )?;
            }
        }
        fired
    };
    // v0.10: plan the per-row ON CONFLICT resolution. No mutation happens
    // here, so a failed row still leaves the statement atomic. Tracks:
    // - inserts: (new row id, values) to insert,
    // - updates: (target table, conflict row id, prev xmax, new values)
    //   for DO UPDATE,
    // - ret_rows: RETURNING source rows in statement order (inserted values
    //   or updated new values; skipped rows contribute nothing).
    // Same-statement conflicts are detected via `key_map`: (index name,
    // key bytes) -> row id of a planned insert.
    let mut inserts: Vec<(u64, Row)> = Vec::new();
    let mut updates: Vec<(String, u64, u64, Row)> = Vec::new();
    let mut ret_rows: Vec<Row> = Vec::new();
    // v1.40: per-row provenance for RETURNING system columns, parallel
    // to ret_rows. `table` is the leaf holding the post-DML version
    // (partition routing); `qual` is the target's range qualifier.
    let mut ret_prov: Vec<RowProv> = Vec::new();
    // v1.40: upsert DO UPDATE rows whose new version id is allocated
    // after planning: (ret_prov index, updates index).
    let mut ret_prov_fixups: Vec<(usize, usize)> = Vec::new();
    if let Some(plan) = &upsert {
        let mut key_map: HashMap<(String, Vec<u8>), u64> = HashMap::new();
        // Latest planned values per row id (planned inserts and the new
        // values of planned updates), for chained same-statement conflicts.
        // Stored in the row's home table column order (the leaf's, for a
        // partitioned target).
        let mut latest: HashMap<u64, Row> = HashMap::new();
        // v0.71: per-leaf upsert contexts for a partitioned target, and
        // the home leaf of each planned insert (for the DO UPDATE
        // cross-partition check on same-statement conflicts).
        let mut leaf_ctxs: HashMap<String, LeafUpsertCtx> = HashMap::new();
        // v0.71: PG19 deterministic DO UPDATE — one row cannot be
        // affected twice by the same statement (21000). Tracks
        // (conflict-search table, row id) of completed DO UPDATEs.
        let mut do_updated: std::collections::HashSet<(String, u64)> =
            std::collections::HashSet::new();
        let mut planned_leaf: HashMap<u64, String> = HashMap::new();
        // Schemas for DO UPDATE evaluation: excluded first, target last
        // (unqualified columns resolve to the target, like Postgres).
        let mk_schemas = || {
            let tgt: Vec<QCol> = meta_for_upsert
                .columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: table.to_string(),
                    name: n.clone(),
                    ty: *ty,

                    hidden: false,
                    src_ord: 0,
                })
                .collect();
            let excl: Vec<QCol> = meta_for_upsert
                .columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: "excluded".to_string(),
                    name: n.clone(),
                    ty: *ty,

                    hidden: false,
                    src_ord: 0,
                })
                .collect();
            (excl, tgt)
        };
        for values in &new_rows {
            // v0.71: partitioned target — route the row to its leaf first
            // (PG19: tuple routing precedes ON CONFLICT arbitration), then
            // arbitrate in the leaf's column order and index space.
            // `ctable`/`cvalues`/`arbiters` are the conflict-search target:
            // the leaf for a partitioned target, else the table itself.
            let (ctable, cvalues, arbiters): (String, Vec<Value>, Vec<(String, Vec<usize>)>) =
                if partitioned_upsert {
                    let leaf = find_row_leaf(eng, ctx, table, values)?;
                    if !leaf_ctxs.contains_key(&leaf) {
                        let lu = leaf_upsert_ctx(eng, ctx, plan, &leaf)?;
                        leaf_ctxs.insert(leaf.clone(), lu);
                    }
                    let lu = &leaf_ctxs[&leaf];
                    let cvalues =
                        remap_parent_to_leaf(&meta_for_upsert.columns, &lu.cols, &values[..]);
                    let arbiters = if plan.explicit_arbiter {
                        lu.arbiters.clone()
                    } else {
                        lu.all_unique.clone()
                    };
                    (leaf, cvalues, arbiters)
                } else {
                    (table.to_string(), values.to_vec(), plan.indexes.clone())
                };
            // 1. Conflict with a table row?
            let mut conflict: Option<u64> = None;
            for (iname, _) in &arbiters {
                if let Some(id) = eng.db.unique_conflict_row(
                    &ctable,
                    iname,
                    &cvalues,
                    None,
                    ctx.snap,
                    &ctx.all_xids,
                    ctx.session,
                ) {
                    conflict = Some(id);
                    break;
                }
            }
            // 2. Conflict with a row planned earlier in this statement?
            if conflict.is_none() {
                for (iname, kcols) in &arbiters {
                    // NULL key parts never conflict.
                    if kcols.iter().any(|&c| cvalues[c] == Value::Null) {
                        continue;
                    }
                    if let Some(id) = key_map.get(&(iname.clone(), arbiter_key(&cvalues, kcols))) {
                        conflict = Some(*id);
                        break;
                    }
                }
            }
            let Some(tid) = conflict else {
                // No conflict: insert.
                let id = eng.alloc_row_id();
                for (iname, kcols) in &arbiters {
                    if kcols.iter().any(|&c| cvalues[c] == Value::Null) {
                        continue;
                    }
                    key_map.insert((iname.clone(), arbiter_key(&cvalues, kcols)), id);
                }
                if partitioned_upsert {
                    planned_leaf.insert(id, ctable.clone());
                }
                latest.insert(id, Row::new(cvalues));
                // Inserts stay in target (parent) column order: the
                // partition router remaps them per leaf.
                inserts.push((id, values.clone()));
                ret_rows.push(values.clone());
                // v1.40: ctable is the leaf for a partitioned target,
                // else the table itself.
                ret_prov.push(RowProv {
                    qual: table.to_string(),
                    table: ctable.clone(),
                    row_id: id,
                });
                continue;
            };
            match &plan.action {
                ConflictAction::DoNothing => {
                    // Skipped: contributes no row and no RETURNING output.
                }
                ConflictAction::DoUpdate { sets, where_ } => {
                    // v0.71: PG19 deterministic DO UPDATE — a row
                    // affected once by this statement cannot be affected
                    // again (21000).
                    if !do_updated.insert((ctable.clone(), tid)) {
                        return Err(exec_err(
                            "21000",
                            "ON CONFLICT DO UPDATE command cannot affect row a second time",
                        ));
                    }
                    // Target values in conflict-target order: latest
                    // planned, else the table row.
                    let target_values: Row = match latest.get(&tid) {
                        Some(v) => v.clone(),
                        None => row_values_by_id(eng, ctx, &ctable, tid)
                            .ok_or_else(|| exec_err("XX000", "upsert conflict target vanished"))?,
                    };
                    // Only real table rows need the concurrency checks;
                    // planned rows are ours.
                    let is_planned = latest.contains_key(&tid);
                    if !is_planned {
                        let t = eng
                            .db
                            .find_table(&ctable, ctx.snap, &ctx.all_xids, ctx.session)
                            .expect("table still visible; engine lock held throughout");
                        let pos = t.row_pos(tid).expect("conflict target still present");
                        let r = &t.rows[pos];
                        check_write_conflict(eng, r.xmax, ctx.level)?;
                        check_row_lock(eng, &ctable, tid, ctx.own)?;
                    }
                    // v0.71: SET/WHERE evaluate against parent-order rows
                    // (the schemas name the target's columns); remap the
                    // leaf-order target up for a partitioned target.
                    let target_parent: Vec<Value> = if partitioned_upsert {
                        let lu = &leaf_ctxs[&ctable];
                        remap_row_to_parent(&lu.cols, &meta_for_upsert.columns, &target_values[..])
                    } else {
                        target_values.to_vec()
                    };
                    let (excl_schema, tgt_schema) = mk_schemas();
                    let frames: Vec<(&[QCol], &[Value])> = vec![
                        (&excl_schema, &values[..]),
                        (&tgt_schema, &target_parent[..]),
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
                            None,
                            w,
                            &ctes,
                            None,
                            Some(qwrite_from_ctx(
                                &mut *ctx.writes,
                                ctx.write_xid,
                                ctx.level,
                                ctx.default_toast_compression,
                            )),
                        )?;
                        if v != Value::Bool(true) {
                            continue;
                        }
                    }
                    let mut new_values = target_parent.clone();
                    for ((_, expr), &ci) in sets.iter().zip(plan.set_cols.iter()) {
                        let v = eval_dml_expr(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            &frames,
                            None,
                            expr,
                            &ctes,
                            None,
                            Some(qwrite_from_ctx(
                                &mut *ctx.writes,
                                ctx.write_xid,
                                ctx.level,
                                ctx.default_toast_compression,
                            )),
                        )?;
                        let (cname, ctype) = &meta_for_upsert.columns[ci];
                        new_values[ci] = coerce_value(v, ctype, cname)?;
                        // v0.85: positional record coercion for
                        // composite/domain-over-composite targets.
                        let comp = meta_for_upsert
                            .composite_types
                            .get(ci)
                            .and_then(|o| o.as_deref());
                        let assigned = std::mem::replace(&mut new_values[ci], Value::Null);
                        new_values[ci] = coerce_assign_composite(eng, assigned, comp)?;
                    }
                    // v0.71: PG19 forbids ON CONFLICT DO UPDATE from moving
                    // the row to a different partition (0A000). An
                    // unroutable result is likewise rejected: it would
                    // appear in a different partition than the original.
                    if partitioned_upsert {
                        let home: &str = if is_planned {
                            planned_leaf.get(&tid).expect("planned row has a home leaf")
                        } else {
                            &ctable
                        };
                        let new_leaf =
                            find_row_leaf(eng, ctx, table, &Row::new(new_values.clone()))?;
                        if new_leaf != home {
                            return Err(exec_err_detail(
                                "0A000",
                                "invalid ON UPDATE specification",
                                "The result tuple would appear in a different partition than the original tuple.",
                            ));
                        }
                    }
                    // Store and index in conflict-target order.
                    let store_values: Vec<Value> = if partitioned_upsert {
                        let lu = &leaf_ctxs[&ctable];
                        remap_parent_to_leaf(&meta_for_upsert.columns, &lu.cols, &new_values)
                    } else {
                        new_values.clone()
                    };
                    // Same validations as a plain UPDATE row.
                    if let Some(vname) = eng.db.unique_violation(
                        &ctable,
                        &store_values,
                        Some(tid),
                        ctx.snap,
                        &ctx.all_xids,
                        ctx.session,
                    ) {
                        let vname = if partitioned_upsert {
                            leaf_constraint_name(eng, ctx, &ctable, &vname)
                        } else {
                            vname
                        };
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
                        ctx.session,
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
                            .find_table(&ctable, ctx.snap, &ctx.all_xids, ctx.session)
                            .expect("table still visible; engine lock held throughout");
                        t.rows[t.row_pos(tid).expect("conflict target still present")].xmax
                    };
                    // Refresh the same-statement key map when the key changed.
                    for (iname, kcols) in &arbiters {
                        let old_null = kcols.iter().any(|&c| target_values[c] == Value::Null);
                        let new_null = kcols.iter().any(|&c| store_values[c] == Value::Null);
                        if !old_null {
                            key_map.remove(&(iname.clone(), arbiter_key(&target_values, kcols)));
                        }
                        if !new_null {
                            key_map.insert((iname.clone(), arbiter_key(&store_values, kcols)), tid);
                        }
                    }
                    let new_values = Row::new(new_values);
                    // latest tracks the home-table order; updates carry
                    // their target table; RETURNING stays in target order.
                    let store_row = Row::new(store_values);
                    latest.insert(tid, store_row.clone());
                    updates.push((ctable.clone(), tid, prev_xmax, store_row));
                    ret_rows.push(new_values);
                    // v1.40: the new version id is allocated with the
                    // other update_ids below; record the fixup.
                    ret_prov_fixups.push((ret_prov.len(), updates.len() - 1));
                    ret_prov.push(RowProv {
                        qual: table.to_string(),
                        table: ctable.clone(),
                        row_id: u64::MAX,
                    });
                }
            }
        }
    } else {
        // No ON CONFLICT: every candidate is inserted.
        for values in &new_rows {
            let id = eng.alloc_row_id();
            let values = values.clone();
            inserts.push((id, values.clone()));
            ret_rows.push(values);
            // v1.40: the leaf is resolved by route_partition_inserts
            // below; `table` is overwritten with it.
            ret_prov.push(RowProv {
                qual: table.to_string(),
                table: table.to_string(),
                row_id: id,
            });
        }
    }
    let n = inserts.len() + updates.len();
    // Every row inserted/updated below pushes exactly one WriteOp onto
    // ctx.writes; reserving for the known total up front avoids the
    // repeated grow-and-copy of pushing into it unsized (measured via
    // dhat on benches/profile_insert.py: this was the single largest
    // allocation site in the workload, ~26% of bytes allocated).
    ctx.writes.reserve(n);
    // v0.69: route to partition leaves if the target is partitioned.
    // (Statement-atomic: routing happens before any mutation.)
    let routed = route_partition_inserts(eng, ctx, table, &inserts)?;
    // v1.40: resolve the partition leaf holding each inserted row for
    // RETURNING system columns (`tableoid` reports the leaf, like PG).
    // Upsert DO UPDATE rows are not routed and keep their planned leaf.
    // Row-version ids are server-assigned counters, not attacker-
    // controlled input, and this map is rebuilt and fully probed once
    // per INSERT statement (one entry, one lookup, per row) — same
    // SipHash-is-overkill rationale as `Table::row_index`.
    let leaf_of: std::collections::HashMap<u64, &str, crate::fxhash::FxBuildHasher> = routed
        .iter()
        .flat_map(|(leaf, rows)| rows.iter().map(|(id, _)| (*id, leaf.as_str())))
        .collect();
    for rp in &mut ret_prov {
        if let Some(leaf) = leaf_of.get(&rp.row_id) {
            rp.table = leaf.to_string();
        }
    }
    // Apply inserts (versions, TOAST, index maintenance).
    for (leaf, leaf_inserts) in &routed {
        apply_row_inserts(eng, ctx, leaf, leaf_inserts)?;
    }
    // Apply DO UPDATEs: delete old version + insert new version.
    // Pre-allocate the new row ids (the table borrow below conflicts).
    // v0.71: each update carries its target table (the leaf, for a
    // partitioned target).
    let mut update_ids = Vec::with_capacity(updates.len());
    for _ in 0..updates.len() {
        update_ids.push(eng.alloc_row_id());
    }
    let mut indexed: Vec<(String, u64, Row, Row, Vec<u32>)> = Vec::with_capacity(updates.len());
    for ((utab, old_id, prev_xmax, new_values), new_id) in updates.iter().zip(update_ids) {
        let t = eng
            .db
            .find_table_mut(utab, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible; engine lock held throughout");
        let pos = t
            .row_pos(*old_id)
            .expect("row version still present; engine lock held throughout");
        // v0.13: UpdateRow carries the old values for logical decoding.
        // v1.05: the old toast flags ride along too, for PG19 update
        // toast lifecycle (reuse unchanged pointers, delete the rest).
        let old_values = t.rows[pos].values.clone();
        let old_toast = t.rows[pos].toast.clone();
        t.rows[pos].xmax = ctx.write_xid;
        t.push_version(RowVersion::plain(new_id, new_values.clone(), ctx.write_xid));
        ctx.writes.push(WriteOp::UpdateRow {
            table: utab.clone(),
            old_id: *old_id,
            new_id,
            prev_xmax: *prev_xmax,
            old_values: old_values.clone(),
        });
        indexed.push((
            utab.clone(),
            new_id,
            new_values.clone(),
            old_values,
            old_toast,
        ));
    }
    // v1.05: TOAST the updated rows with PG19 update semantics —
    // unchanged columns reuse their toast pointers; superseded chunks
    // are deleted transactionally (not left for vacuum).
    for (utab, new_id, new_values, old_values, old_toast) in &indexed {
        toast_update_row(eng, ctx, utab, *new_id, old_values, old_toast, new_values)?;
    }
    for (utab, new_id, new_values, _, _) in &indexed {
        eng.db
            .index_insert_row(utab, *new_id, new_values, ctx.session);
    }
    // v1.40: fill in the new version ids for upsert DO UPDATE rows'
    // RETURNING provenance (allocated with the other update_ids).
    for (ret_idx, upd_idx) in &ret_prov_fixups {
        let (utab, new_id, _, _, _) = &indexed[*upd_idx];
        let rp = &mut ret_prov[*ret_idx];
        rp.table = utab.clone();
        rp.row_id = *new_id;
    }
    // v0.10: RETURNING.
    let (ret_cols, ret_out): (Vec<(String, ColType)>, Vec<Row>) = if returning.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let (cols, returning_expanded) = describe_returning(
            eng,
            ctx.snap,
            ctx.own,
            ctx.session,
            table,
            table,
            &[],
            returning,
        )?;
        let schema: Vec<QCol> = meta_for_upsert
            .columns
            .iter()
            .map(|(n, ty)| QCol {
                qual: table.to_string(),
                name: n.clone(),
                ty: *ty,

                hidden: false,
                src_ord: 0,
            })
            .collect();
        let mut out_rows = Vec::with_capacity(ret_rows.len());
        // v1.40: per-row provenance drives RETURNING system columns.
        for (values, rp) in ret_rows.iter().zip(ret_prov.iter()) {
            out_rows.push(Row::new(project_returning(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &[(&schema, values)],
                std::slice::from_ref(rp),
                &returning_expanded,
                &ctes,
                Some(qwrite_from_ctx(
                    &mut *ctx.writes,
                    ctx.write_xid,
                    ctx.level,
                    ctx.default_toast_compression,
                )),
            )?));
        }
        (cols, out_rows)
    };
    Ok(ExecResult::Dml {
        tag: format!("INSERT 0 {}", n),
        columns: ret_cols,
        rows: ret_out,
    })
}

/// v0.22: UPDATE/DELETE graduated to full predicate expressions (see
/// `exec_update`/`exec_delete`); the legacy `WHERE col = literal`
/// matcher is retired. NULL never matches (SQL semantics), which the
/// expression evaluator already implements via three-valued logic.

/// v0.6: a row locked by another active transaction (SELECT ... FOR
/// UPDATE) cannot be written by us — fail fast with 40001 instead of
/// blocking, like Postgres' NOWAIT but without the wait option.
pub(crate) fn check_row_lock(
    eng: &Engine,
    table: &str,
    row_id: u64,
    own: u64,
) -> Result<(), ExecError> {
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
pub(crate) fn check_write_conflict(
    eng: &Engine,
    xmax: u64,
    level: IsolationLevel,
) -> Result<(), ExecError> {
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

// --- v0.70: partitioned UPDATE/DELETE ---------------------------------------
// In PostgreSQL, UPDATE/DELETE naming a partitioned table operate on every
// partition. This engine stores rows only in leaves, so a partitioned
// target expands to its leaf tables. Leaf rows are remapped to the parent's
// column order for WHERE/SET/RETURNING evaluation (leaves may reorder
// columns, e.g. ATTACH with a column list); storage, indexes, constraints
// and FK cascades use each leaf's own order and metadata. An UPDATE that
// changes the partition key moves the row: the old version is deleted from
// its leaf and the new version is inserted into the routed leaf.

/// Leaf tables under `root` with their column lists: every descendant
/// with no children of its own. Built on the SELECT path's
/// `collect_partition_leaves` traversal.
pub(crate) fn partition_leaves(
    eng: &Engine,
    ctx: &StmtCtx,
    root: &str,
) -> Vec<(String, Vec<(String, ColType)>)> {
    collect_partition_leaves(&eng.db, root, ctx.snap, ctx.own, ctx.session)
        .into_iter()
        .filter_map(|name| {
            eng.db
                .find_table(&name, ctx.snap, &ctx.all_xids, ctx.session)
                .map(|t| (name, t.columns.clone()))
        })
        .collect()
}

/// Reorder `values` from `from_cols` order into `to_cols` order, matching by
/// column name. Missing columns become NULL (defensive; PARTITION OF
/// children always carry the full column set).
pub(crate) fn reorder_row(
    from_cols: &[(String, ColType)],
    to_cols: &[(String, ColType)],
    values: &[Value],
) -> Vec<Value> {
    let pos: std::collections::HashMap<&str, usize> = from_cols
        .iter()
        .enumerate()
        .map(|(i, (n, _))| (n.as_str(), i))
        .collect();
    to_cols
        .iter()
        .map(|(n, _)| {
            pos.get(n.as_str())
                .and_then(|&i| values.get(i))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect()
}

pub(crate) fn exec_update(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    sets: &[(String, Expr)],
    from: &[FromItem],
    where_: &Option<Expr>,
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
    // v1.20: PostgreSQL rejects UPDATE...FROM when the target table's
    // name appears among the FROM items (42712 "table name specified
    // more than once"), even under LATERAL or with an alias — the check
    // is on the relation name, not the alias. Without this we would
    // wrongly succeed (e.g. `update xx1 ... from xx1, lateral ...`).
    // Note: parse_from folds comma-separated items into a single
    // Cross Join, so walk the Join tree to find plain Table items.
    if !from.is_empty() {
        fn collect_table_names(item: &FromItem, out: &mut Vec<String>) {
            match item {
                FromItem::Table { name, .. } => {
                    out.push(name.rsplit('.').next().unwrap_or(name).to_string());
                }
                FromItem::Join { left, right, .. } => {
                    collect_table_names(left, out);
                    collect_table_names(right, out);
                }
                _ => {}
            }
        }
        let target = table.rsplit('.').next().unwrap_or(table);
        let mut names = Vec::new();
        for item in from {
            collect_table_names(item, &mut names);
        }
        for from_name in names {
            if from_name.eq_ignore_ascii_case(target) {
                return Err(exec_err(
                    "42712",
                    format!("table name \"{}\" specified more than once", from_name),
                ));
            }
        }
    }
    // v0.10: WITH materialization; the CTEs are visible to subqueries in
    // SET/WHERE and in the RETURNING list.
    let ctes = materialize_dml_ctes(eng, &mut *ctx, with)?;
    // v0.89: statement-local UPDATE overlay — rows already planned by
    // this UPDATE, as (destination leaf table, row-version id, new
    // values in destination order). Volatile SQL function bodies called
    // from SET/WHERE see these instead of the statement snapshot
    // (PG19); stable/immutable functions and plain subqueries do not.
    // This is a plan, not a mutation: storage is untouched until the
    // whole UPDATE validates, so statement atomicity is preserved.
    let pending: Rc<RefCell<Vec<(String, u64, Row)>>> = Rc::new(RefCell::new(Vec::new()));
    // Plan first (validate + conflict-check), mutate after: a failed
    // UPDATE leaves no trace (statement atomicity).
    // v0.70: a partitioned target expands to its leaves. The plan holds
    // in-place updates as (dst leaf, old id, prev xmax, new values in dst
    // order); rows whose partition key changed are planned as moves
    // (src leaf, old id, prev xmax, dst leaf, new values in dst order).
    // `ret_new` carries every new row in parent order for RETURNING.
    let (plan, moves, ret_new, ret_from, ret_prov_ids, from_schema): (
        Vec<(String, u64, u64, Row)>,
        Vec<(String, u64, u64, String, Row)>,
        Vec<Row>,
        Vec<Option<Row>>,
        // v1.40: (old row id, destination leaf) per ret_new row.
        Vec<(u64, String)>,
        Option<Vec<QCol>>,
    ) = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let meta = TableMeta::of(t);
        let partitioned = t.partition.is_some();
        let pinfo = t.partition.clone();
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
        // v0.85: composite type names for positional record coercion on
        // UPDATE assignment.
        let composite_types = meta.composite_types.clone();
        // v0.22: the target table name is the qualifier (was empty), so
        // `tbl.col` references resolve in SET and WHERE, as in PostgreSQL.
        // v0.76: an alias makes it the visible qualifier (PG's alias clause).
        let qual = alias.as_deref().unwrap_or(table);
        let schema: Vec<QCol> = columns
            .iter()
            .map(|(n, ty)| QCol {
                qual: qual.to_string(),
                name: n.clone(),
                ty: *ty,

                hidden: false,
                src_ord: 0,
            })
            .collect();
        // Copy the visible rows' data out; the borrow of `t` ends here so
        // SET expressions can run against `eng` below. v0.70: a
        // partitioned target scans every leaf; rows are remapped to the
        // parent's column order so the parent-qualified schema evaluates
        // correctly (leaves may reorder columns).
        let mut vis: Vec<(String, Vec<(String, ColType)>, u64, u64, Row)> = Vec::new();
        // v0.70: the tables to scan — every leaf for a partitioned
        // target, else the named table itself.
        let leaves: Vec<(String, Vec<(String, ColType)>)> = if partitioned {
            let mut ls = partition_leaves(eng, ctx, table);
            if ls.is_empty() {
                ls.push((table.to_string(), columns.clone()));
            }
            ls
        } else {
            vec![(table.to_string(), columns.clone())]
        };
        // Leaf column orders, for routing and remapping below.
        let leaf_cols_of = leaves.clone();
        for (leaf, leaf_cols) in &leaves {
            let lt = eng
                .db
                .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("leaf still visible; engine lock held throughout");
            for r in lt
                .rows
                .iter()
                .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
            {
                let pv = if *leaf == table {
                    r.values.clone()
                } else {
                    Row::new(reorder_row(leaf_cols, &columns, &r.values))
                };
                vis.push((leaf.clone(), leaf_cols.clone(), r.id, r.xmax, pv));
            }
        }
        let mut plan: Vec<(String, u64, u64, Row)> = Vec::new();
        let mut moves: Vec<(String, u64, u64, String, Row)> = Vec::new();
        let mut ret_new: Vec<Row> = Vec::new();
        // v1.40: (old row id, destination leaf) per ret_new row, for
        // RETURNING system columns. The new version id is allocated
        // in the apply loop below.
        let mut ret_prov_ids: Vec<(u64, String)> = Vec::new();
        // v0.76: UPDATE ... FROM — build the FROM items once (like SELECT's
        // FROM and DELETE's USING). Each target row is tested against the
        // cross product; SET/WHERE/RETURNING see the combined scopes with
        // the target frame last (unqualified refs resolve to the target).
        let from_data: Option<(Vec<QCol>, Vec<QRow>)> = if from.is_empty() {
            None
        } else {
            let mut lock_ids = Vec::new();
            let mut q = Q {
                eng: &mut *eng,
                snap: ctx.snap,
                own: ctx.own,
                all_xids: ctx.all_xids.clone(),
                session: ctx.session,
                role: ctx.role,
                read_only: ctx.read_only,
                depth: 0,
                lock_ids: &mut lock_ids,
                ctes: ctes.clone(),
                wctx: None,
                srf_vals: Vec::new(),
                priv_scopes: Vec::new(),
                hashed_exists: Rc::new(RefCell::new(HashMap::new())),
                immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
                plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
                hashed_in: Rc::new(RefCell::new(Vec::new())),
                pending_updates: None,
                write: Some(qwrite_from_ctx(
                    &mut *ctx.writes,
                    ctx.write_xid,
                    ctx.level,
                    ctx.default_toast_compression,
                )),
            };
            let (fschema, frows) = build_from(&mut q, &[], from, None, false, None, None)?;
            Some((fschema, frows))
        };
        // For RETURNING with FROM: the FROM row used for each updated target
        // row, parallel to `ret_new` (None when no FROM clause).
        let mut ret_from: Vec<Option<Row>> = Vec::new();
        // (leaf, old values in leaf order) parallel to `plan`, for the FK
        // cascade below.
        let mut old_leaf_vals: Vec<(String, Row)> = Vec::new();
        // (src leaf, old values in src order) parallel to `moves`.
        let mut move_old_vals: Vec<(String, Row)> = Vec::new();
        // New values (with row id) per destination leaf, for
        // self-referencing FK child checks.
        let mut new_by_dst: std::collections::HashMap<String, Vec<(u64, Row)>> =
            std::collections::HashMap::new();
        // SET expressions are evaluated with a scratch query context;
        // subqueries in SET correlate to the row being updated.
        for (leaf, leaf_cols, id, xmax, values) in &vis {
            check_write_conflict(eng, *xmax, ctx.level)?;
            // v0.76: UPDATE ... FROM — test each target row against the
            // FROM cross product. The SET expressions evaluate against the
            // matching combination; if several FROM rows match, the last
            // one wins (PG picks an arbitrary match; we take the last for
            // determinism). Without FROM, the old single-scope logic runs.
            let (row_matches, from_row): (bool, Option<Row>) = match &from_data {
                None => {
                    // v0.22: full predicate expression (was `col = literal`
                    // only). NULL or false skips the row, like SELECT's WHERE.
                    let m = match where_ {
                        None => true,
                        Some(pred) => {
                            let v = eval_update_expr(
                                eng,
                                ctx.snap,
                                ctx.own,
                                ctx.session,
                                ctx.role,
                                &schema,
                                values,
                                pred,
                                &ctes,
                                Some(pending.clone()),
                                Some(qwrite_from_ctx(
                                    &mut *ctx.writes,
                                    ctx.write_xid,
                                    ctx.level,
                                    ctx.default_toast_compression,
                                )),
                            )?;
                            v == Value::Bool(true)
                        }
                    };
                    (m, None)
                }
                Some((fschema, frows)) => {
                    let mut matched = false;
                    let mut last_frow: Option<Row> = None;
                    for frow in frows {
                        let m = match where_ {
                            None => true,
                            Some(pred) => {
                                // v0.76: combine FROM + target into a single
                                // schema so unqualified refs resolve to
                                // either (FROM first, then target).
                                let mut cschema = fschema.clone();
                                cschema.extend(schema.iter().cloned());
                                let mut cvalues = frow.cells.clone().into_cells();
                                cvalues.extend(values.iter().cloned());
                                let v = eval_update_expr(
                                    eng,
                                    ctx.snap,
                                    ctx.own,
                                    ctx.session,
                                    ctx.role,
                                    &cschema,
                                    &cvalues,
                                    pred,
                                    &ctes,
                                    Some(pending.clone()),
                                    Some(qwrite_from_ctx(
                                        &mut *ctx.writes,
                                        ctx.write_xid,
                                        ctx.level,
                                        ctx.default_toast_compression,
                                    )),
                                )?;
                                v == Value::Bool(true)
                            }
                        };
                        if m {
                            matched = true;
                            last_frow = Some(frow.cells.clone());
                        }
                    }
                    (matched, last_frow)
                }
            };
            if !row_matches {
                continue;
            }
            // Only rows we actually write conflict with FOR UPDATE locks —
            // merely scanning a locked row is fine, like Postgres.
            check_row_lock(eng, leaf, *id, ctx.own)?;
            let mut new_values = values.to_vec();
            for ((_, expr), &ci) in sets.iter().zip(set_cols.iter()) {
                let v = match &from_data {
                    None => eval_update_expr(
                        eng,
                        ctx.snap,
                        ctx.own,
                        ctx.session,
                        ctx.role,
                        &schema,
                        values,
                        expr,
                        &ctes,
                        Some(pending.clone()),
                        Some(qwrite_from_ctx(
                            &mut *ctx.writes,
                            ctx.write_xid,
                            ctx.level,
                            ctx.default_toast_compression,
                        )),
                    )?,
                    Some((fschema, _)) => {
                        let frow = from_row.as_ref().expect("matched row has FROM data");
                        // v0.76: combined schema (FROM first, then target).
                        let mut cschema = fschema.clone();
                        cschema.extend(schema.iter().cloned());
                        let mut cvalues = frow.clone().into_cells();
                        cvalues.extend(values.iter().cloned());
                        eval_update_expr(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            &cschema,
                            &cvalues,
                            expr,
                            &ctes,
                            Some(pending.clone()),
                            Some(qwrite_from_ctx(
                                &mut *ctx.writes,
                                ctx.write_xid,
                                ctx.level,
                                ctx.default_toast_compression,
                            )),
                        )?
                    }
                };
                let (cname, ctype) = &columns[ci];
                new_values[ci] = coerce_value(v, ctype, cname)?;
                // v0.85: positional record coercion for
                // composite/domain-over-composite targets.
                let comp = composite_types.get(ci).and_then(|o| o.as_deref());
                let assigned = std::mem::replace(&mut new_values[ci], Value::Null);
                new_values[ci] = coerce_assign_composite(eng, assigned, comp)?;
            }
            // v0.70: route the new row through the partitioned target. A
            // changed partition key moves the row to another leaf
            // (delete old version + insert new), like PostgreSQL.
            let (dst, dst_cols) = if partitioned {
                let p = pinfo.as_ref().expect("partitioned target keeps its info");
                let dl = find_partition_leaf(eng, ctx, table, p, &columns, &new_values)?;
                let dc = leaf_cols_of
                    .iter()
                    .find(|(n, _)| n == &dl)
                    .map(|(_, c)| c.clone())
                    .unwrap_or_else(|| columns.clone());
                (dl, dc)
            } else {
                (leaf.clone(), leaf_cols.clone())
            };
            let new_dst = reorder_row(&columns, &dst_cols, &new_values);
            let dst_meta = {
                let dt = eng
                    .db
                    .find_table(&dst, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("destination leaf visible; engine lock held throughout");
                TableMeta::of(dt)
            };
            // v0.8: UNIQUE enforcement against the destination's indexes.
            // The old version is excluded (it is being replaced); the
            // check runs before any mutation, keeping the statement
            // atomic.
            if let Some(vname) = eng.db.unique_violation(
                &dst,
                &new_dst,
                Some(*id),
                ctx.snap,
                &ctx.all_xids,
                ctx.session,
            ) {
                return Err(exec_err(
                    "23505",
                    format!(
                        "duplicate key value violates unique constraint \"{}\"",
                        vname
                    ),
                ));
            }
            // v0.9: NOT NULL + CHECK on the new row (destination metadata).
            check_row_constraints(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &dst_meta,
                &dst,
                &new_dst,
            )?;
            new_by_dst
                .entry(dst.clone())
                .or_default()
                .push((*id, Row::new(new_dst.clone())));
            ret_new.push(Row::new(new_values.clone()));
            // v1.40: `dst` is the leaf holding the new version (the
            // source `leaf` when the row does not move).
            ret_prov_ids.push((*id, dst.clone()));
            // v0.76: remember the FROM row for RETURNING (None without FROM).
            ret_from.push(from_row.clone());
            let old_src = Row::new(reorder_row(&columns, leaf_cols, &values.to_vec()));
            if dst == *leaf {
                old_leaf_vals.push((leaf.clone(), old_src));
                // v0.89: record the planned row in the statement-local
                // overlay so volatile function bodies later in this
                // UPDATE see it (PG19); nothing is written yet.
                pending
                    .borrow_mut()
                    .push((leaf.clone(), *id, Row::new(new_dst.clone())));
                plan.push((leaf.clone(), *id, *xmax, Row::new(new_dst)));
            } else {
                move_old_vals.push((leaf.clone(), old_src));
                // v0.89: same for rows moving between partitions,
                // keyed by the destination leaf.
                pending
                    .borrow_mut()
                    .push((dst.clone(), *id, Row::new(new_dst.clone())));
                moves.push((leaf.clone(), *id, *xmax, dst, Row::new(new_dst)));
            }
        }
        // v0.9: child-side FK checks for the new rows, per destination
        // leaf. Self-references see the statement's own new versions;
        // each row's old version is excluded from the parent scan.
        for (dst, new_rows) in &new_by_dst {
            let dst_meta = {
                let dt = eng
                    .db
                    .find_table(dst, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("destination leaf visible; engine lock held throughout");
                TableMeta::of(dt)
            };
            let self_new: Vec<Row> = new_rows.iter().map(|(_, nv)| nv.clone()).collect();
            for (rid, nv) in new_rows {
                check_fk_child_row(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    &dst_meta,
                    dst,
                    nv,
                    &self_new,
                    Some(*rid),
                )?;
            }
        }
        // v0.9: parent-side FK actions (RESTRICT / CASCADE / SET NULL /
        // SET DEFAULT), planned before any mutation. Grouped by the leaf
        // holding the old version; moves plan their delete side.
        let mut cascade = FkCascade::default();
        {
            let mut by_src: std::collections::HashMap<&str, Vec<(u64, Row, Option<Row>)>> =
                std::collections::HashMap::new();
            for ((_, id, _, nv), (src, ov)) in plan.iter().zip(old_leaf_vals.iter()) {
                by_src
                    .entry(src.as_str())
                    .or_default()
                    .push((*id, ov.clone(), Some(nv.clone())));
            }
            for ((src, id, _, _, _), (_, ov)) in moves.iter().zip(move_old_vals.iter()) {
                by_src
                    .entry(src.as_str())
                    .or_default()
                    .push((*id, ov.clone(), None));
            }
            for (src, changed) in &by_src {
                let src_meta = {
                    let st = eng
                        .db
                        .find_table(src, ctx.snap, &ctx.all_xids, ctx.session)
                        .expect("source leaf visible; engine lock held throughout");
                    TableMeta::of(st)
                };
                plan_fk_cascade(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    ctx.level,
                    src,
                    &src_meta,
                    changed,
                    0,
                    &mut cascade,
                )?;
            }
        }
        // v0.8: pairwise unique check — two rows updated to the same unique
        // key in one statement (the index still holds only old entries).
        // v0.9: extended over cascaded updates, grouped by table. v0.70:
        // grouped by destination leaf, including moved rows.
        {
            let mut by_table: HashMap<&str, Vec<(u64, u64, Row)>> = HashMap::new();
            for (dst, id, xmax, nv) in &plan {
                by_table
                    .entry(dst.as_str())
                    .or_default()
                    .push((*id, *xmax, nv.clone()));
            }
            for (_, id, xmax, dst, nv) in &moves {
                by_table
                    .entry(dst.as_str())
                    .or_default()
                    .push((*id, *xmax, nv.clone()));
            }
            for (t, id, xmax, nv) in &cascade.updates {
                by_table
                    .entry(t.as_str())
                    .or_default()
                    .push((*id, *xmax, nv.clone()));
            }
            for (t, p) in &by_table {
                check_update_unique_pairs(&eng.db, t, p, ctx.snap, &ctx.all_xids, ctx.session)?;
            }
        }
        // Apply the cascade after the unique checks pass.
        apply_fk_cascade(eng, ctx, cascade)?;
        let from_schema = from_data.map(|(s, _)| s);
        (plan, moves, ret_new, ret_from, ret_prov_ids, from_schema)
    };
    // Apply: UPDATE = delete old version + insert new version, per leaf.
    // v0.70: moved rows are deleted from their source leaf and inserted
    // into the routed destination leaf.
    let n = plan.len() + moves.len();
    // v1.40: old -> new version id, for RETURNING system columns.
    let mut new_id_of: Vec<(u64, u64)> = Vec::with_capacity(n);
    {
        let mut by_leaf: std::collections::HashMap<String, Vec<(u64, u64, Row)>> =
            std::collections::HashMap::new();
        for (dst, old_id, prev_xmax, new_values) in plan {
            by_leaf
                .entry(dst)
                .or_default()
                .push((old_id, prev_xmax, new_values));
        }
        // Deterministic leaf order for the WAL log.
        let mut leaves: Vec<String> = by_leaf.keys().cloned().collect();
        leaves.sort_unstable();
        for leaf in &leaves {
            let rows = &by_leaf[leaf.as_str()];
            let mut new_ids = Vec::with_capacity(rows.len());
            for _ in 0..rows.len() {
                new_ids.push(eng.alloc_row_id());
            }
            // The table borrow ends before index maintenance (both need
            // `eng.db` mutably); collect the new versions' keys meanwhile.
            // v1.05: also carry each superseded version's values and toast
            // flags, so toast_update_row can reuse unchanged toast
            // pointers and delete superseded chunks (PG19
            // heap_toast_insert_or_update).
            let mut indexed: Vec<(u64, Row, Row, Vec<u32>)> = Vec::with_capacity(rows.len());
            {
                let t = eng
                    .db
                    .find_table_mut(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("table still visible; engine lock held throughout");
                for ((old_id, prev_xmax, new_values), new_id) in rows.iter().cloned().zip(new_ids) {
                    // v1.40: remember the new version id for RETURNING
                    // system columns.
                    new_id_of.push((old_id, new_id));
                    let pos = t
                        .row_pos(old_id)
                        .expect("row version still present; engine lock held throughout");
                    // v0.13: UpdateRow carries the old values for logical decoding.
                    let old_values = t.rows[pos].values.clone();
                    let old_toast = t.rows[pos].toast.clone();
                    t.rows[pos].xmax = ctx.write_xid;
                    t.push_version(RowVersion::plain(new_id, new_values.clone(), ctx.write_xid));
                    ctx.writes.push(WriteOp::UpdateRow {
                        table: leaf.clone(),
                        old_id,
                        new_id,
                        prev_xmax,
                        old_values: old_values.clone(),
                    });
                    indexed.push((new_id, new_values, old_values, old_toast));
                }
            }
            // v1.05: TOAST the updated rows with PG19 update semantics —
            // unchanged columns reuse their toast pointers; superseded
            // chunks are deleted transactionally (not left for vacuum).
            for (new_id, new_values, old_values, old_toast) in &indexed {
                toast_update_row(eng, ctx, leaf, *new_id, old_values, old_toast, new_values)?;
            }
            // v0.8: index the new versions (old versions' entries stay; the
            // version chain's xmax makes them invisible).
            for (new_id, new_values, _, _) in &indexed {
                eng.db
                    .index_insert_row(leaf, *new_id, new_values, ctx.session);
            }
        }
    }
    // v0.70: moved rows — delete the old version from the source leaf,
    // insert the new version into the routed destination leaf.
    {
        let mut by_src: std::collections::HashMap<String, Vec<(u64, u64)>> =
            std::collections::HashMap::new();
        let mut by_dst: std::collections::HashMap<String, Vec<(u64, Row)>> =
            std::collections::HashMap::new();
        for (src, old_id, prev_xmax, dst, new_values) in moves {
            let new_id = eng.alloc_row_id();
            // v1.40: remember the new version id for RETURNING system
            // columns.
            new_id_of.push((old_id, new_id));
            by_src.entry(src).or_default().push((old_id, prev_xmax));
            by_dst.entry(dst).or_default().push((new_id, new_values));
        }
        let mut srcs: Vec<String> = by_src.keys().cloned().collect();
        srcs.sort_unstable();
        // v1.05: collect each deleted source version's toast value ids —
        // PG19 models a partition move as heap_delete + heap_insert, so
        // the source version's chunks are deleted eagerly (heap_delete →
        // heap_toast_delete), not left for vacuum.
        let mut doomed_by_src: std::collections::HashMap<String, Vec<u32>> =
            std::collections::HashMap::new();
        for src in &srcs {
            let t = eng
                .db
                .find_table_mut(src, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("source leaf visible; engine lock held throughout");
            for (old_id, prev_xmax) in &by_src[src.as_str()] {
                let pos = t
                    .row_pos(*old_id)
                    .expect("row version still present; engine lock held throughout");
                doomed_by_src
                    .entry(src.clone())
                    .or_default()
                    .extend(t.rows[pos].toast.iter().copied().filter(|v| *v != 0));
                t.rows[pos].xmax = ctx.write_xid;
                ctx.writes.push(WriteOp::DeleteRow {
                    table: src.clone(),
                    row_id: *old_id,
                    prev_xmax: *prev_xmax,
                });
            }
        }
        // v1.05: delete the moved-away versions' chunks transactionally.
        for src in &srcs {
            if let Some(doomed) = doomed_by_src.get(src) {
                if !doomed.is_empty() {
                    toast_delete_chunks(eng, ctx, src, doomed)?;
                }
            }
        }
        let mut dsts: Vec<String> = by_dst.keys().cloned().collect();
        dsts.sort_unstable();
        for dst in &dsts {
            apply_row_inserts(eng, ctx, dst, &by_dst[dst.as_str()])?;
        }
    }
    // v0.10: RETURNING evaluates against the NEW row values. v0.70:
    // `ret_new` holds them in parent column order (leaves may reorder).
    let (ret_cols, ret_out): (Vec<(String, ColType)>, Vec<Row>) = if returning.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        // v0.76: UPDATE ... FROM — RETURNING may reference the FROM tables
        // (PG19); describe against the FROM schemas plus the target (with
        // the alias as the visible qualifier), mirroring the runtime's
        // combined evaluation schema below.
        let rqual = alias.as_deref().unwrap_or(table);
        let extra: Vec<Vec<QCol>> = from_schema
            .as_ref()
            .map(|s| vec![s.clone()])
            .unwrap_or_default();
        let (cols, returning_expanded) = describe_returning(
            eng,
            ctx.snap,
            ctx.own,
            ctx.session,
            table,
            rqual,
            &extra,
            returning,
        )?;
        let schema: Vec<QCol> = {
            let t = eng
                .db
                .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("table still visible; engine lock held throughout");
            t.columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: rqual.to_string(),
                    name: n.clone(),
                    ty: *ty,

                    hidden: false,
                    src_ord: 0,
                })
                .collect()
        };
        // v1.40: per-row provenance for RETURNING system columns —
        // the new version's (destination leaf, new row id). PG evaluates
        // UPDATE ... RETURNING against the new row version.
        let new_id_of: std::collections::HashMap<u64, u64> = new_id_of.into_iter().collect();
        let ret_prov: Vec<RowProv> = ret_prov_ids
            .iter()
            .map(|(old_id, dst)| RowProv {
                qual: rqual.to_string(),
                table: dst.clone(),
                // Defensive: every planned row is applied, so the id is
                // always present; u64::MAX keeps system columns a clean
                // 42703 if that ever drifts.
                row_id: new_id_of.get(old_id).copied().unwrap_or(u64::MAX),
            })
            .collect();
        // v1.40: the FROM range's qualifier, for the ambiguity check on
        // unqualified system-column references (its row ids are not
        // tracked, so its system columns stay 42703).
        let from_qual: Option<String> = from_schema
            .as_ref()
            .and_then(|fs| fs.first().map(|c| c.qual.clone()));
        let mut out_rows = Vec::with_capacity(ret_new.len());
        for ((new_values, frow_opt), rp) in ret_new.iter().zip(ret_from.iter()).zip(ret_prov.iter())
        {
            // v0.76: UPDATE ... FROM — RETURNING sees the FROM columns too.
            // Combine into a single schema (FROM first, then target) so
            // unqualified refs resolve.
            let (cschema, cvalues): (Vec<QCol>, Vec<Value>) = match (from_schema.as_ref(), frow_opt)
            {
                (Some(fs), Some(fv)) => {
                    let mut cs = fs.clone();
                    cs.extend(schema.iter().cloned());
                    let mut cv = fv.clone().into_cells();
                    cv.extend(new_values.iter().cloned());
                    (cs, cv)
                }
                _ => (schema.clone(), new_values.to_vec()),
            };
            let scopes: Vec<(&[QCol], &[Value])> = vec![(&cschema, &cvalues)];
            // v1.40: provenance for the scope's ranges — the FROM range
            // (if any) first, then the target. The FROM entry is a
            // sentinel: only its qualifier participates.
            let prov: Vec<RowProv> = match (&from_qual, frow_opt) {
                (Some(fq), Some(_)) => vec![
                    RowProv {
                        qual: fq.clone(),
                        table: String::new(),
                        row_id: u64::MAX,
                    },
                    rp.clone(),
                ],
                _ => vec![rp.clone()],
            };
            out_rows.push(Row::new(project_returning(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &scopes,
                prov.as_slice(),
                &returning_expanded,
                &ctes,
                Some(qwrite_from_ctx(
                    &mut *ctx.writes,
                    ctx.write_xid,
                    ctx.level,
                    ctx.default_toast_compression,
                )),
            )?));
        }
        (cols, out_rows)
    };
    Ok(ExecResult::Dml {
        tag: format!("UPDATE {}", n),
        columns: ret_cols,
        rows: ret_out,
    })
}

pub(crate) fn exec_delete(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    using: &[FromItem],
    where_: &Option<Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
) -> Result<ExecResult, ExecError> {
    // v0.11: DELETE needs DELETE privilege on the table.
    require_table_priv(eng, ctx, table, crate::storage::PRIV_DELETE, "DELETE")?;
    // v0.65: an alias makes it the visible qualifier in WHERE/RETURNING
    // (PG's alias clause); storage and privileges use the real name.
    let qual = alias.as_deref().unwrap_or(table);
    // v0.10: WITH materialization (validated; plain DELETE cannot reference
    // the CTEs, but the RETURNING list can via subqueries).
    let ctes = materialize_dml_ctes(eng, &mut *ctx, with)?;
    // Plan first for statement atomicity (WHERE type errors must not
    // leave half the rows deleted). v0.70: a partitioned target scans
    // every leaf; the plan records (leaf, id, xmax, values in parent
    // order for RETURNING, values in leaf order for FK cascades, toast).
    let (plan, ret_using, using_schema): (
        Vec<(String, u64, u64, Row, Row, Vec<u32>)>,
        Vec<Option<Row>>,
        Option<Vec<QCol>>,
    ) = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let meta = TableMeta::of(t);
        let partitioned = t.partition.is_some();
        let columns = meta.columns.clone();
        // v0.22: the WHERE clause is a full predicate expression
        // (like SELECT's). The schema carries the target table name
        // as qualifier so `tbl.col` references resolve, as in
        // PostgreSQL's DELETE. Visible rows are copied out first so the
        // borrow of `t` ends before predicate evaluation, which needs
        // `&mut eng` (subqueries).
        let schema: Vec<QCol> = meta
            .columns
            .iter()
            .map(|(n, ty)| QCol {
                qual: qual.to_string(),
                name: n.clone(),
                ty: *ty,

                hidden: false,
                src_ord: 0,
            })
            .collect();
        let vis: Vec<(String, u64, u64, Row, Row, Vec<u32>)> = {
            // (leaf, id, xmax, values parent order, values leaf order, toast)
            let mut v = Vec::new();
            // v0.70: partitioned targets scan every leaf, remapping rows
            // to the parent's column order for WHERE evaluation.
            let targets: Vec<(String, Vec<(String, ColType)>)> = if partitioned {
                let mut ls = partition_leaves(eng, ctx, table);
                if ls.is_empty() {
                    ls.push((table.to_string(), columns.clone()));
                }
                ls
            } else {
                vec![(table.to_string(), columns.clone())]
            };
            for (leaf, leaf_cols) in &targets {
                let lt = eng
                    .db
                    .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("leaf still visible; engine lock held throughout");
                for r in lt
                    .rows
                    .iter()
                    .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
                {
                    let pv = if *leaf == table {
                        r.values.clone()
                    } else {
                        Row::new(reorder_row(leaf_cols, &columns, &r.values))
                    };
                    v.push((
                        leaf.clone(),
                        r.id,
                        r.xmax,
                        pv,
                        r.values.clone(),
                        r.toast.clone(),
                    ));
                }
            }
            v
        };
        let mut plan = Vec::new();
        // v0.76: the USING row matched for each deleted target row,
        // parallel to `plan` (None without USING) — RETURNING may
        // reference USING columns (PG19).
        let mut plan_using: Vec<Option<Row>> = Vec::new();
        // v0.74: DELETE ... USING — evaluate the USING from-items once
        // (like SELECT's FROM); each target row is tested against the
        // cross product, and deleted if any combination satisfies WHERE.
        let using_data: Option<(Vec<QCol>, Vec<QRow>)> = if using.is_empty() {
            None
        } else {
            let mut lock_ids = Vec::new();
            let mut q = Q {
                eng: &mut *eng,
                snap: ctx.snap,
                own: ctx.own,
                all_xids: ctx.all_xids.clone(),
                session: ctx.session,
                role: ctx.role,
                read_only: ctx.read_only,
                depth: 0,
                lock_ids: &mut lock_ids,
                ctes: ctes.clone(),
                wctx: None,
                srf_vals: Vec::new(),
                priv_scopes: Vec::new(),
                hashed_exists: Rc::new(RefCell::new(HashMap::new())),
                immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
                plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
                hashed_in: Rc::new(RefCell::new(Vec::new())),
                pending_updates: None,
                write: Some(qwrite_from_ctx(
                    &mut *ctx.writes,
                    ctx.write_xid,
                    ctx.level,
                    ctx.default_toast_compression,
                )),
            };
            let (uschema, urows) = build_from(&mut q, &[], using, None, false, None, None)?;
            Some((uschema, urows))
        };
        for (leaf, id, xmax, values, leaf_values, _toast) in &vis {
            check_write_conflict(eng, *xmax, ctx.level)?;
            let (row_matches, using_row): (bool, Option<Row>) = match where_ {
                // v0.76: without WHERE every target row is deleted (v0.74
                // behavior) — but only when USING produced at least one
                // row (PG19 cross-product semantics); RETURNING sees the
                // first USING row, if any.
                None => match using_data.as_ref() {
                    Some((_, urows)) if urows.is_empty() => (false, None),
                    Some((_, urows)) => (true, urows.first().map(|u| u.cells.clone())),
                    None => (true, None),
                },
                Some(pred) => {
                    if let Some((uschema, urows)) = &using_data {
                        // Target frame last: unqualified refs resolve to
                        // the target table (eval_dml_expr convention).
                        let mut matched = false;
                        let mut matched_urow: Option<Row> = None;
                        for urow in urows {
                            let v = eval_dml_expr(
                                eng,
                                ctx.snap,
                                ctx.own,
                                ctx.session,
                                ctx.role,
                                &[(uschema, &urow.cells), (&schema, values)],
                                None,
                                pred,
                                &ctes,
                                None,
                                Some(qwrite_from_ctx(
                                    &mut *ctx.writes,
                                    ctx.write_xid,
                                    ctx.level,
                                    ctx.default_toast_compression,
                                )),
                            )?;
                            if v == Value::Bool(true) {
                                matched = true;
                                // v0.76: remember the USING row for
                                // RETURNING (first match wins here; PG
                                // picks an arbitrary match).
                                matched_urow = Some(urow.cells.clone());
                                break;
                            }
                        }
                        (matched, matched_urow)
                    } else {
                        let v = eval_update_expr(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            &schema,
                            values,
                            pred,
                            &ctes,
                            None,
                            Some(qwrite_from_ctx(
                                &mut *ctx.writes,
                                ctx.write_xid,
                                ctx.level,
                                ctx.default_toast_compression,
                            )),
                        )?;
                        (v == Value::Bool(true), None)
                    }
                }
            };
            if row_matches {
                // Only rows we actually delete conflict with FOR UPDATE
                // locks — merely scanning a locked row is fine.
                check_row_lock(eng, leaf, *id, ctx.own)?;
                plan.push((
                    leaf.clone(),
                    *id,
                    *xmax,
                    values.clone(),
                    leaf_values.clone(),
                    _toast.clone(),
                ));
                // v0.76: parallel USING row for RETURNING.
                plan_using.push(using_row);
            }
        }
        // v0.9: parent-side FK actions for the deleted rows, grouped by
        // the leaf holding each row (v0.70).
        let mut cascade = FkCascade::default();
        {
            let mut by_leaf: std::collections::HashMap<&str, Vec<(u64, Row, Option<Row>)>> =
                std::collections::HashMap::new();
            for (leaf, id, _, _, leaf_values, _) in &plan {
                by_leaf
                    .entry(leaf.as_str())
                    .or_default()
                    .push((*id, leaf_values.clone(), None));
            }
            for (leaf, changed) in &by_leaf {
                let leaf_meta = {
                    let lt = eng
                        .db
                        .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                        .expect("leaf visible; engine lock held throughout");
                    TableMeta::of(lt)
                };
                plan_fk_cascade(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    ctx.level,
                    leaf,
                    &leaf_meta,
                    changed,
                    0,
                    &mut cascade,
                )?;
            }
        }
        apply_fk_cascade(eng, ctx, cascade)?;
        // v0.76: USING schema for RETURNING (None without USING).
        let using_schema = using_data.map(|(s, _)| s);
        (plan, plan_using, using_schema)
    };
    let n = plan.len();
    // v0.10: DELETE RETURNING evaluates against the OLD row values —
    // collect them before the plan is consumed by the apply loop.
    let ret_vals: Vec<Row> = plan.iter().map(|(_, _, _, v, _, _)| v.clone()).collect();
    // v0.39: the deleted versions' toast value ids, for chunk cleanup below.
    let mut vids_by_leaf: std::collections::HashMap<String, Vec<u32>> =
        std::collections::HashMap::new();
    for (leaf, _, _, _, _, toast) in &plan {
        vids_by_leaf
            .entry(leaf.clone())
            .or_default()
            .extend(toast.iter().copied().filter(|v| *v != 0));
    }
    // v0.70: apply per leaf.
    {
        let mut by_leaf: std::collections::HashMap<&str, Vec<(u64, u64)>> =
            std::collections::HashMap::new();
        for (leaf, id, prev_xmax, _, _, _) in &plan {
            by_leaf
                .entry(leaf.as_str())
                .or_default()
                .push((*id, *prev_xmax));
        }
        let mut leaves: Vec<&str> = by_leaf.keys().copied().collect();
        leaves.sort_unstable();
        for leaf in leaves {
            let t = eng
                .db
                .find_table_mut(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("table still visible; engine lock held throughout");
            for (id, prev_xmax) in &by_leaf[leaf] {
                let pos = t
                    .row_pos(*id)
                    .expect("row version still present; engine lock held throughout");
                t.rows[pos].xmax = ctx.write_xid;
                ctx.writes.push(WriteOp::DeleteRow {
                    table: leaf.to_string(),
                    row_id: *id,
                    prev_xmax: *prev_xmax,
                });
            }
        }
    }
    // v0.39: deleting a row version deletes its out-of-line toast chunks
    // too (PG19 heap_delete calls heap_toast_delete immediately). Staged
    // as WriteOps so the chunk deletions are transactional and WAL-logged;
    // ROLLBACK restores them via the DeleteRow undo. v0.70: per leaf.
    {
        let mut leaves: Vec<String> = vids_by_leaf.keys().cloned().collect();
        leaves.sort_unstable();
        for leaf in &leaves {
            toast_delete_chunks(eng, ctx, leaf, &vids_by_leaf[leaf.as_str()])?;
        }
    }
    // v0.10: RETURNING.
    let (ret_cols, ret_out): (Vec<(String, ColType)>, Vec<Row>) = if returning.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        // v0.76: DELETE ... USING — RETURNING may reference the USING
        // tables (PG19); describe against the USING schemas plus the
        // target, mirroring the runtime's combined evaluation schema.
        let extra: Vec<Vec<QCol>> = using_schema
            .as_ref()
            .map(|s| vec![s.clone()])
            .unwrap_or_default();
        let (cols, returning_expanded) = describe_returning(
            eng,
            ctx.snap,
            ctx.own,
            ctx.session,
            table,
            qual,
            &extra,
            returning,
        )?;
        let schema: Vec<QCol> = {
            let t = eng
                .db
                .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("table still visible; engine lock held throughout");
            t.columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: qual.to_string(),
                    name: n.clone(),
                    ty: *ty,

                    hidden: false,
                    src_ord: 0,
                })
                .collect()
        };
        // v1.40: per-row provenance for RETURNING system columns —
        // the deleted version's (leaf, old row id). PG evaluates
        // DELETE ... RETURNING against the old row version (its xmax is
        // the deleting xid). ret_vals is parallel to plan.
        let ret_prov: Vec<RowProv> = plan
            .iter()
            .map(|(leaf, id, _, _, _, _)| RowProv {
                qual: qual.to_string(),
                table: leaf.clone(),
                row_id: *id,
            })
            .collect();
        // v1.40: the USING range's qualifier, for the ambiguity check
        // on unqualified system-column references (its row ids are not
        // tracked, so its system columns stay 42703).
        let using_qual: Option<String> = using_schema
            .as_ref()
            .and_then(|us| us.first().map(|c| c.qual.clone()));
        let mut out_rows = Vec::with_capacity(ret_vals.len());
        for ((values, urow_opt), rp) in ret_vals.iter().zip(ret_using.iter()).zip(ret_prov.iter()) {
            // v0.76: DELETE ... USING — RETURNING sees the USING columns
            // too. Combine into a single schema (USING first, then
            // target), mirroring UPDATE ... FROM.
            let (cschema, cvalues): (Vec<QCol>, Vec<Value>) =
                match (using_schema.as_ref(), urow_opt) {
                    (Some(us), Some(uv)) => {
                        let mut cs = us.clone();
                        cs.extend(schema.iter().cloned());
                        let mut cv = uv.clone().into_cells();
                        cv.extend(values.iter().cloned());
                        (cs, cv)
                    }
                    _ => (schema.clone(), values.to_vec()),
                };
            // v1.40: provenance for the scope's ranges — the USING range
            // (if any) first, then the target. The USING entry is a
            // sentinel: only its qualifier participates.
            let prov: Vec<RowProv> = match (&using_qual, urow_opt) {
                (Some(uq), Some(_)) => vec![
                    RowProv {
                        qual: uq.clone(),
                        table: String::new(),
                        row_id: u64::MAX,
                    },
                    rp.clone(),
                ],
                _ => vec![rp.clone()],
            };
            out_rows.push(Row::new(project_returning(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &[(&cschema, &cvalues)],
                prov.as_slice(),
                &returning_expanded,
                &ctes,
                Some(qwrite_from_ctx(
                    &mut *ctx.writes,
                    ctx.write_xid,
                    ctx.level,
                    ctx.default_toast_compression,
                )),
            )?));
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
pub(crate) fn exec_truncate(
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
            for (child, _) in fks_referencing(eng, ctx.snap, ctx.own, ctx.session, &t) {
                if !targets.contains(&child) {
                    targets.push(child);
                }
            }
            i += 1;
        }
    } else {
        for t in tables {
            let refs = fks_referencing(eng, ctx.snap, ctx.own, ctx.session, t);
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
        if eng
            .db
            .find_table(t, ctx.snap, &ctx.all_xids, ctx.session)
            .is_none()
        {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", t),
            ));
        }
    }
    // v0.69: expand partitioned tables to their descendant leaves
    // (PG's TRUNCATE propagates to partitions).
    let mut expanded: Vec<String> = Vec::new();
    for t in &targets {
        let leaves = collect_partition_leaves(&eng.db, t, ctx.snap, ctx.own, ctx.session);
        // If it's not partitioned, collect_partition_leaves returns [t].
        // If it is, it returns the leaves. We want the leaves in both
        // cases (for a non-partitioned table, the "leaf" is itself).
        expanded.extend(leaves);
    }
    // Deduplicate (a table might appear via CASCADE and as a partition).
    expanded.sort();
    expanded.dedup();
    let targets = expanded;
    // Delete every visible row, staging the same WriteOps as DELETE so
    // ROLLBACK / ROLLBACK TO SAVEPOINT restore the table exactly.
    // v0.37: TRUNCATE also clears each table's toast data, like PG.
    let mut delete_targets: Vec<String> = targets.clone();
    for t in &targets {
        let tr = eng
            .db
            .find_table(t, ctx.snap, &ctx.all_xids, ctx.session)
            .map(|tt| tt.toast_relid)
            .unwrap_or(0);
        if tr != 0 {
            if let Some(tn) = toast_table_name_by_oid(eng, tr, ctx.snap, ctx.own) {
                delete_targets.push(tn);
            }
        }
    }
    for table in &delete_targets {
        let plan: Vec<(u64, u64)> = {
            let t = eng
                .db
                .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("table checked above; engine lock held throughout");
            let mut plan = Vec::new();
            for rv in t
                .rows
                .iter()
                .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
            {
                check_write_conflict(eng, rv.xmax, ctx.level)?;
                check_row_lock(eng, table, rv.id, ctx.own)?;
                plan.push((rv.id, rv.xmax));
            }
            plan
        };
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table checked above; engine lock held throughout");
        for (id, prev_xmax) in plan {
            let pos = t
                .row_pos(id)
                .expect("row version still present; engine lock held throughout");
            t.rows[pos].xmax = ctx.write_xid;
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

pub(crate) fn exec_drop(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    // v0.72: PG resolves every name before dropping, and a partition
    // dropped implicitly with its parent is not an error when also
    // named explicitly (performMultipleDeletions deletes each object
    // once). Track statement-level drops and skip repeats.
    let mut dropped: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in names {
        if dropped.contains(name) {
            continue;
        }
        drop_one_table(eng, ctx, name, if_exists, cascade, &mut dropped)?;
    }
    Ok(ExecResult::Command {
        tag: "DROP TABLE".to_string(),
    })
}

/// v0.65: drop a serial backing sequence when its table is dropped
/// (PG's OWNED BY / DEPENDENCY_AUTO behavior). Owned sequences are
/// dropped unconditionally with their table, like PG — even if another
/// table's DEFAULT references the sequence (that default simply breaks,
/// as in PG). No-op when already gone.
pub(crate) fn drop_serial_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    seq: &str,
) -> Result<(), ExecError> {
    if eng.db.find_sequence(seq, ctx.snap, ctx.own).is_none() {
        return Ok(());
    }
    // Same drop mechanics as exec_drop_sequence (no separate ownership
    // check: the sequence was created with the table by the same role).
    let versions = eng.db.sequences.get_mut(seq).expect("visible above");
    let cur = versions
        .iter_mut()
        .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
        .expect("visible above");
    let prev = cur.clone();
    cur.dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropSequence {
        name: seq.to_string(),
        seq: prev,
    });
    Ok(())
}

/// v0.65: collect the names of sequences explicitly owned by a table's
/// serial columns (PG's DEPENDENCY_AUTO). Never infers from nextval()
/// default text: user sequences referenced by explicit DEFAULT
/// nextval() have `owned_by = None` and survive DROP TABLE.
/// `temp_session` isolates temp-table sequences by session.
pub(crate) fn owned_seqs_of(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    temp_session: Option<u64>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (name, versions) in &eng.db.sequences {
        if let Some(s) = versions
            .iter()
            .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
        {
            if let Some((t, _, sess)) = &s.owned_by {
                if t == table && *sess == temp_session {
                    out.push(name.clone());
                }
            }
        }
    }
    out.sort();
    out
}

/// Drop a single table with dependency handling.
pub(crate) fn drop_one_table(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_exists: bool,
    cascade: bool,
    dropped: &mut std::collections::HashSet<String>,
) -> Result<(), ExecError> {
    // v0.11: only the owner (or a superuser) may drop a table.
    require_table_owner(eng, ctx, name)?;
    // v0.69: recursive DROP for partitioned tables (PG drops all
    // partitions). Do this before the temp/permanent split so temp
    // partitioned tables work too.
    let children: Vec<String> = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .and_then(|t| t.partition.as_ref())
        .map(|p| p.children.clone())
        .unwrap_or_default();
    for child in &children {
        // Recurse (the child may itself be partitioned).
        drop_one_table(eng, ctx, child, false, cascade, dropped)?;
    }
    // v0.22: DROP resolves a session-local temp table first (PostgreSQL
    // semantics): dropping it reveals any permanent table of the same
    // name. The permanent table is untouched.
    let temp_prev: Option<Table> = eng
        .db
        .temp_tables
        .get_mut(&ctx.session)
        .and_then(|tmps| tmps.remove(name));
    if let Some(prev) = temp_prev {
        // Clean up the now-empty session map.
        if eng
            .db
            .temp_tables
            .get(&ctx.session)
            .is_some_and(|tmps| tmps.is_empty())
        {
            eng.db.temp_tables.remove(&ctx.session);
        }
        // v0.65: drop the temp table's explicitly-owned serial
        // sequences (session-isolated via owned_by).
        let serial_seqs = owned_seqs_of(eng, ctx, name, Some(ctx.session));
        // v0.70: detach a dropped temp partition child from its
        // parent's children list (a permanent parent gets a versioned,
        // undoable link update; a temp parent is session-local).
        let temp_part_parent = prev.partition.as_ref().and_then(|p| p.parent.clone());
        ctx.writes.push(WriteOp::DropTempTable {
            session: ctx.session,
            name: name.to_string(),
            prev: Some(prev),
        });
        if let Some(parent_name) = temp_part_parent {
            if eng.db.is_temp_table(ctx.session, &parent_name) {
                if let Some(pp) =
                    eng.db
                        .find_table_mut(&parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                {
                    if let Some(pi) = pp.partition.as_mut() {
                        pi.children.retain(|c| c != name);
                    }
                }
            } else {
                parent_link_child(eng, ctx, &parent_name, name, false);
            }
        }
        for seq_name in &serial_seqs {
            drop_serial_sequence(eng, ctx, seq_name)?;
        }
        // Drop the temp table's indexes with it (statement-atomic via
        // their own write ops, like the permanent path below).
        // v0.22: temp tables never own global indexes (their constraints
        // are enforced by scan and CREATE INDEX on them is rejected), so
        // there is nothing to drop here. Dropping "the temp table's
        // indexes" by table name would delete a same-named permanent
        // table's indexes — that block was removed for exactly this
        // reason.
        // v0.72: record the statement-level drop (multi-name DROP).
        dropped.insert(name.to_string());
        return Ok(());
    }
    // Find the visible version first (immutable) for the conflict check,
    // then mutate. A DROP of a table dropped by a not-yet-visible
    // transaction behaves like the row case (40001 under RR/SERIALIZABLE).
    let prev_xmax = {
        let t = eng
            .db
            .find_table(name, ctx.snap, &ctx.all_xids, ctx.session);
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
            .find(|t| crate::storage::table_visible(t, ctx.snap, &ctx.all_xids))
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
        if eng
            .db
            .find_table(tname, ctx.snap, &ctx.all_xids, ctx.session)
            .is_some()
        {
            alter_drop_constraint_internal(eng, ctx, tname, fk_name)?;
        }
    }
    // v0.65: collect explicitly-owned serial sequences first (PG's
    // DEPENDENCY_AUTO; never inferred from defaults). Done before the
    // mutable table borrow below.
    let serial_seqs = owned_seqs_of(eng, ctx, name, None);
    let t = eng
        .db
        .find_table_mut(name, ctx.snap, &ctx.all_xids, ctx.session)
        .expect("table still visible; engine lock held throughout");
    // v0.37: dropping a table drops its toast table too (like PG).
    let toast_relid = t.toast_relid;
    // v0.70: remember the partition parent (if any) so the dropped
    // child can be detached from its children list below.
    let part_parent = t.partition.as_ref().and_then(|p| p.parent.clone());
    t.dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropTable {
        name: name.to_string(),
        prev_xmax,
    });
    // v0.70: detach a dropped partition child from its parent's
    // children list (transactionally, so ROLLBACK re-links it). The
    // recursive DROP above already dropped this table's own children.
    if let Some(parent_name) = part_parent {
        if eng.db.is_temp_table(ctx.session, &parent_name) {
            if let Some(pp) =
                eng.db
                    .find_table_mut(&parent_name, ctx.snap, &ctx.all_xids, ctx.session)
            {
                if let Some(pi) = pp.partition.as_mut() {
                    pi.children.retain(|c| c != name);
                }
            }
        } else {
            parent_link_child(eng, ctx, &parent_name, name, false);
        }
    }
    // v0.65: dropping a table drops its owned serial sequences.
    for seq_name in &serial_seqs {
        drop_serial_sequence(eng, ctx, seq_name)?;
    }
    if toast_relid != 0 {
        // Find the toast table's name by OID (visible version).
        if let Some(tn) = toast_table_name_by_oid(eng, toast_relid, ctx.snap, ctx.own) {
            let tt_prev_xmax = eng
                .db
                .find_table(&tn, ctx.snap, &ctx.all_xids, ctx.session)
                .map(|tt| tt.dropped_xmax)
                .unwrap_or(0);
            if let Some(tt) = eng
                .db
                .find_table_mut(&tn, ctx.snap, &ctx.all_xids, ctx.session)
            {
                tt.dropped_xmax = ctx.own;
                ctx.writes.push(WriteOp::DropTable {
                    name: tn,
                    prev_xmax: tt_prev_xmax,
                });
            }
        }
    }
    // v0.8: dropping a table drops its indexes with it. Each index drop
    // is its own write op (snapshotting the definition) so ROLLBACK
    // restores them and the WAL replays them.
    let idx_names: Vec<String> = eng
        .db
        .visible_indexes_for(name, ctx.snap, &ctx.all_xids, ctx.session)
        .iter()
        .map(|ix| ix.def.name.clone())
        .collect();
    for iname in idx_names {
        drop_index_internal(eng, ctx, &iname)?;
    }
    // v0.72: record the statement-level drop (multi-name DROP).
    dropped.insert(name.to_string());
    Ok(())
}
