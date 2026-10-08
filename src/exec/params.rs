// v1.78 mechanical split: moved verbatim from src/exec.rs (53164-54637).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// v0.2: parameters for the extended query protocol (v0.6: new AST)
// ---------------------------------------------------------------------------

pub(crate) fn pin_param(out: &mut [Option<ColType>], p: u32, t: ColType) -> Result<(), ExecError> {
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
pub(crate) fn hint_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
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
                    prov: None,
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
            hint_type(eng, snap, own, session, schemas, left),
            hint_type(eng, snap, own, session, schemas, right),
        )
        .ok(),
        Expr::Cast { to, .. } => Some(*to),
        // v0.53: `-x` hints like the old `0 - x` desugar.
        Expr::Neg(x) => combine_arith_types(
            ArithOp::Sub,
            Some(ColType::Int),
            hint_type(eng, snap, own, session, schemas, x),
        )
        .ok(),
        Expr::Concat(..) => Some(ColType::Text),
        Expr::Extract { .. } => Some(ColType::Numeric(None)),
        Expr::Func { name, args } => {
            func_result_type(name, args, eng, snap, own, session, schemas, &[], &[]).ok()
        }
        Expr::Agg {
            func,
            arg,
            distinct: _,
            arg2,
            ..
        } => agg_result_type(
            eng,
            snap,
            own,
            session,
            schemas,
            &[],
            &[],
            *func,
            arg.as_deref(),
            arg2.as_deref(),
        )
        .ok(),
        Expr::ScalarSub(sub) => {
            let cols = describe_select(eng, snap, own, session, sub, &[], &[]).ok()?;
            if cols.len() == 1 {
                Some(cols[0].1.clone())
            } else {
                None
            }
        }
        // v1.11: array constructors hint their array type (PG19's
        // array_expr): element hints combine via select_common_type,
        // mirroring execution's array_ctor_from_vals. A nested row is
        // itself an ArrayCtor whose hint is already an array type;
        // ArrayElem::of flattens it to the innermost element, like PG.
        // All-unknown (e.g. ARRAY[]) resolves to text[], like PG.
        Expr::ArrayCtor { elems, .. } => {
            let mut acc: Option<ColType> = None;
            for e in elems {
                if let Some(t) = hint_type(eng, snap, own, session, schemas, e) {
                    acc = Some(match acc {
                        Some(a) => common_supertype("ARRAY", &a, &t).ok()?,
                        None => t,
                    });
                }
            }
            let elem_ty = acc.unwrap_or(ColType::Text);
            Some(ColType::Array(ArrayElem::of(&elem_ty)))
        }
        // v1.11: row constructors hint record (PG19's row_expr), so
        // VALUES/UNION columns holding ROW(...) describe correctly
        // instead of defaulting to text.
        Expr::Row(_) => Some(ColType::Record),
        // Predicates are boolean, but that never usefully pins a param.
        _ => None,
    }
}

pub(crate) fn infer_expr(
    e: &Expr,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    out: &mut [Option<ColType>],
) -> Result<(), ExecError> {
    match e {
        Expr::Arith { left, right, .. } => {
            infer_expr(left, eng, snap, own, session, schemas, out)?;
            infer_expr(right, eng, snap, own, session, schemas, out)?;
            // One side a param, the other side typed: pin the param.
            for (p_side, o_side) in [(left, right), (right, left)] {
                if let Expr::Param(p) = **p_side {
                    if let Some(t) = hint_type(eng, snap, own, session, schemas, o_side) {
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
            infer_expr(left, eng, snap, own, session, schemas, out)?;
            infer_expr(right, eng, snap, own, session, schemas, out)?;
            // `col = $N` pins the param to the column's type.
            if let Expr::Param(p) = **left {
                if let Some(t) = hint_type(eng, snap, own, session, schemas, right) {
                    pin_param(out, p, t)?;
                }
            }
            if let Expr::Param(p) = **right {
                if let Some(t) = hint_type(eng, snap, own, session, schemas, left) {
                    pin_param(out, p, t)?;
                }
            }
            Ok(())
        }
        Expr::And(a, b) | Expr::Or(a, b) => {
            infer_expr(a, eng, snap, own, session, schemas, out)?;
            infer_expr(b, eng, snap, own, session, schemas, out)
        }
        Expr::Not(x) | Expr::BitNot(x) | Expr::IsNull { expr: x, .. } => {
            infer_expr(x, eng, snap, own, session, schemas, out)
        }
        Expr::Neg(x) => {
            // v0.53: unary minus pins a param like the old `0 - x`
            // desugar did — the param takes the `0` side's type.
            if let Expr::Param(p) = **x {
                pin_param(out, p, ColType::Int)?;
            }
            infer_expr(x, eng, snap, own, session, schemas, out)
        }
        // v0.55: recurse into every CASE arm; pin `$N` WHEN keys to
        // the simple-CASE operand's type and `$N` results to a sibling
        // result arm's type.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                infer_expr(o, eng, snap, own, session, schemas, out)?;
            }
            for (k, r) in whens {
                infer_expr(k, eng, snap, own, session, schemas, out)?;
                infer_expr(r, eng, snap, own, session, schemas, out)?;
            }
            if let Some(e) = else_ {
                infer_expr(e, eng, snap, own, session, schemas, out)?;
            }
            if let Some(o) = operand {
                if let Some(t) = hint_type(eng, snap, own, session, schemas, o) {
                    for (k, _) in whens {
                        if let Expr::Param(p) = **k {
                            pin_param(out, p, t.clone())?;
                        }
                    }
                }
            }
            let mut results: Vec<&Expr> = Vec::with_capacity(whens.len() + 1);
            for (_, r) in whens.iter() {
                results.push(r);
            }
            if let Some(e) = else_.as_deref() {
                results.push(e);
            }
            for r in &results {
                if let Expr::Param(p) = **r {
                    for s in &results {
                        if std::ptr::eq(*r, *s) {
                            continue;
                        }
                        if let Some(t) = hint_type(eng, snap, own, session, schemas, s) {
                            pin_param(out, p, t)?;
                            break;
                        }
                    }
                }
            }
            Ok(())
        }
        Expr::IsDistinctFrom { left, right, .. } => {
            infer_expr(left, eng, snap, own, session, schemas, out)?;
            infer_expr(right, eng, snap, own, session, schemas, out)
        }
        Expr::Agg { arg, .. } => {
            if let Some(a) = arg {
                infer_expr(a, eng, snap, own, session, schemas, out)?;
            }
            Ok(())
        }
        Expr::ScalarSub(sub) => infer_select(sub, eng, snap, own, session, schemas, out),
        Expr::InSub { expr, sub, .. } => {
            infer_expr(expr, eng, snap, own, session, schemas, out)?;
            infer_select(sub, eng, snap, own, session, schemas, out)?;
            // `$N IN (SELECT col ...)` pins the param to the column type.
            if let Expr::Param(p) = **expr {
                if let Ok(cols) = describe_select(eng, snap, own, session, sub, &[], &[]) {
                    if cols.len() == 1 {
                        pin_param(out, p, cols[0].1.clone())?;
                    }
                }
            }
            Ok(())
        }
        Expr::Exists { sub, .. } => infer_select(sub, eng, snap, own, session, schemas, out),
        _ => Ok(()),
    }
}

pub(crate) fn infer_from(
    f: &FromItem,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    out: &mut [Option<ColType>],
) -> Result<(), ExecError> {
    match f {
        FromItem::Table { .. } => Ok(()),
        // v0.99: derived tables may be correlated to enclosing scopes
        // (PG19); a superset is fine for param pinning, which only fires
        // on unambiguous matches.
        FromItem::Derived { sub, .. } => infer_select(sub, eng, snap, own, session, schemas, out),
        // v0.14: VALUES rows are uncorrelated constants.
        FromItem::Values { rows, .. } => {
            for row in rows {
                for e in row {
                    infer_expr(e, eng, snap, own, session, &[], out)?;
                }
            }
            Ok(())
        }
        FromItem::Join {
            left, right, on, ..
        } => {
            infer_from(left, eng, snap, own, session, schemas, out)?;
            infer_from(right, eng, snap, own, session, schemas, out)?;
            // ON params resolve against the enclosing query's combined
            // schemas (a superset is fine: pinning only fires on
            // unambiguous matches).
            if let Some(p) = on {
                infer_expr(p, eng, snap, own, session, schemas, out)?;
            }
            Ok(())
        }
        // v0.32: table-function args are uncorrelated (no LATERAL).
        FromItem::Function { args, .. } => {
            for a in args {
                infer_expr(a, eng, snap, own, session, &[], out)?;
            }
            Ok(())
        }
    }
}

/// `outer` is the enclosing query's schema chain (for correlated
/// subqueries); this query's own FROM schemas are appended after it.
pub(crate) fn infer_select(
    s: &SelectStmt,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    outer: &[&[QCol]],
    out: &mut [Option<ColType>],
) -> Result<(), ExecError> {
    // v0.44: recurse through set-operation branches.
    if let Some(root) = &s.set_op {
        infer_select(&root.left, eng, snap, own, session, outer, out)?;
        for b in &root.chain {
            infer_select(&b.right, eng, snap, own, session, outer, out)?;
        }
        return Ok(());
    }
    // Unknown tables are skipped (execution reports them).
    // v0.10: this level's own CTEs are visible to the FROM clause.
    let visible: Vec<CteDef> = s.with.clone();
    let own_schemas = from_schemas(eng, snap, own, session, &s.from, &visible, &[], outer, &[])
        .unwrap_or_default();
    let mut refs: Vec<&[QCol]> = Vec::with_capacity(outer.len() + own_schemas.len());
    refs.extend_from_slice(outer);
    refs.extend(own_schemas.iter().map(|s| s.as_slice()));
    for f in &s.from {
        infer_from(f, eng, snap, own, session, &refs, out)?;
    }
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            infer_expr(expr, eng, snap, own, session, &refs, out)?;
        }
    }
    if let Some(w) = &s.where_ {
        infer_expr(w, eng, snap, own, session, &refs, out)?;
    }
    for g in s.group_by.iter().flatten() {
        infer_expr(g, eng, snap, own, session, &refs, out)?;
    }
    if let Some(h) = &s.having {
        infer_expr(h, eng, snap, own, session, &refs, out)?;
    }
    for o in &s.order_by {
        infer_expr(&o.expr, eng, snap, own, session, &refs, out)?;
    }
    Ok(())
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
    session: u64,
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
        if let Some(t) = eng.db.find_table(table, snap, &[own], session) {
            // Unknown column names are skipped here; execution reports them.
            // v0.84: targets are InsertTarget (name + indirection); the
            // indirection path is ignored for type inference.
            let targets: Vec<usize> = match columns {
                Some(names) => names
                    .iter()
                    .filter_map(|n| t.column_index(&n.name))
                    .collect(),
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
        infer_ctes(with, eng, snap, own, session, &mut out);
        infer_on_conflict(on_conflict, table, eng, snap, own, session, &mut out);
        infer_returning(
            returning,
            table,
            table,
            &[],
            eng,
            snap,
            own,
            session,
            &mut out,
        );
    }
    if let Stmt::Update {
        table,
        alias,
        sets,
        from,
        where_,
        with,
        returning,
        ..
    } = stmt
    {
        // v0.76: SET/WHERE see the UPDATE ... FROM tables too (PG19); the
        // alias (if any) is the visible qualifier. A bad FROM table must
        // not break Bind (from_schemas' unwrap_or_default rule).
        let uqual = alias.as_deref().unwrap_or(table);
        let uextra: Vec<Vec<QCol>> =
            from_schemas(eng, snap, own, session, from, with, &[], &[], &[]).unwrap_or_default();
        if let Some(t) = eng.db.find_table(table, snap, &[own], session) {
            let mut combined: Vec<QCol> = uextra.iter().flatten().cloned().collect();
            combined.extend(t.columns.iter().map(|(n, ty)| QCol {
                qual: uqual.to_string(),
                name: n.clone(),
                ty: *ty,

                hidden: false,
                src_ord: 0,
            }));
            let schemas: Vec<Vec<QCol>> = vec![combined];
            let refs: Vec<&[QCol]> = schemas.iter().map(|s| s.as_slice()).collect();
            for (col, expr) in sets {
                if let Expr::Param(p) = expr {
                    if let Some(i) = t.column_index(col) {
                        pin_param(&mut out, *p, t.columns[i].1.clone())?;
                    }
                } else {
                    infer_expr(expr, eng, snap, own, session, &refs, &mut out)?;
                }
            }
            // v0.22: WHERE is a full expression now; infer its params
            // against the target table's schema (this also preserves the
            // old `col = $N` pinning via infer_expr's Cmp arm).
            if let Some(w) = where_ {
                infer_expr(w, eng, snap, own, session, &refs, &mut out)?;
            }
        }
        // v0.10: CTE bodies and RETURNING list.
        infer_ctes(with, eng, snap, own, session, &mut out);
        infer_returning(
            returning, table, uqual, &uextra, eng, snap, own, session, &mut out,
        );
    }
    match stmt {
        Stmt::Select(sel) => {
            infer_select(sel, eng, snap, own, session, &[], &mut out)?;
        }
        Stmt::Delete {
            table,
            alias,
            using,
            where_,
            with,
            returning,
            ..
        } => {
            // v0.22: WHERE is a full expression; infer its params against
            // the target table's schema. v0.65: the alias (if any) is
            // the visible qualifier. v0.76: DELETE ... USING tables are
            // visible too (PG19).
            let qual = alias.as_deref().unwrap_or(table);
            let dextra: Vec<Vec<QCol>> =
                from_schemas(eng, snap, own, session, using, with, &[], &[], &[])
                    .unwrap_or_default();
            if let Some(w) = where_ {
                if let Some(t) = eng.db.find_table(table, snap, &[own], session) {
                    let mut combined: Vec<QCol> = dextra.iter().flatten().cloned().collect();
                    combined.extend(t.columns.iter().map(|(n, ty)| QCol {
                        qual: qual.to_string(),
                        name: n.clone(),
                        ty: *ty,

                        hidden: false,
                        src_ord: 0,
                    }));
                    let schemas: Vec<Vec<QCol>> = vec![combined];
                    let refs: Vec<&[QCol]> = schemas.iter().map(|s| s.as_slice()).collect();
                    infer_expr(w, eng, snap, own, session, &refs, &mut out)?;
                }
            }
            // v0.10: CTE bodies and RETURNING list.
            infer_ctes(with, eng, snap, own, session, &mut out);
            infer_returning(
                returning, table, qual, &dextra, eng, snap, own, session, &mut out,
            );
        }
        _ => {}
    }
    Ok(out)
}

/// v0.10: best-effort parameter inference inside CTE bodies. Failures are
/// swallowed (params stay unpinned and default to text) because bodies can
/// reference sibling CTEs that only fully resolve at execution time.
pub(crate) fn infer_ctes(
    ctes: &[CteDef],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    out: &mut [Option<ColType>],
) {
    for cte in ctes {
        match &cte.body {
            CteBody::Simple(s) => {
                let _ = infer_select(s, eng, snap, own, session, &[], out);
            }
            CteBody::Union { left, .. } => {
                let _ = infer_select(left, eng, snap, own, session, &[], out);
            }
            // v1.39: no DML param inference yet — params inside a
            // data-modifying CTE body stay unpinned (best-effort, per
            // the fn docs).
            CteBody::Dml(_) => {}
        }
    }
}

/// v0.10: best-effort inference for a RETURNING list against the target
/// table's columns.
pub(crate) fn infer_returning(
    returning: &[SelectItem],
    table: &str,
    qual: &str,
    extra: &[Vec<QCol>],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    out: &mut [Option<ColType>],
) {
    if returning.is_empty() {
        return;
    }
    if let Some(t) = eng.db.find_table(table, snap, &[own], session) {
        // v0.76: UPDATE ... FROM / DELETE ... USING — RETURNING may
        // reference those tables (PG19); flatten their schemas ahead of
        // the target's, like the describe path.
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
        for item in returning {
            if let SelectItem::Expr { expr, .. } = item {
                let _ = infer_expr(expr, eng, snap, own, session, &refs, out);
            }
        }
    }
}

/// v0.10: best-effort inference for ON CONFLICT DO UPDATE expressions
/// against the target table's columns.
pub(crate) fn infer_on_conflict(
    on_conflict: &Option<OnConflict>,
    table: &str,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    out: &mut [Option<ColType>],
) {
    if let Some(OnConflict {
        action: ConflictAction::DoUpdate { sets, .. },
        ..
    }) = on_conflict
    {
        if let Some(t) = eng.db.find_table(table, snap, &[own], session) {
            let schemas: Vec<Vec<QCol>> = vec![
                t.columns
                    .iter()
                    .map(|(n, ty)| QCol {
                        qual: String::new(),
                        name: n.clone(),
                        ty: *ty,

                        hidden: false,
                        src_ord: 0,
                    })
                    .collect(),
            ];
            let refs: Vec<&[QCol]> = schemas.iter().map(|s| s.as_slice()).collect();
            for (_, expr) in sets {
                let _ = infer_expr(expr, eng, snap, own, session, &refs, out);
            }
        }
    }
}

pub(crate) fn oid_to_coltype(oid: i32, param_no: usize) -> Result<Option<ColType>, ExecError> {
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
    session: u64,
) -> Result<Vec<ColType>, ExecError> {
    let inferred = infer_param_types(stmt, eng, snap, own, session)?;
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

    session: u64,
) -> Result<Vec<Option<Value>>, ExecError> {
    let types = resolve_param_types(stmt, declared, eng, snap, own, session)?;
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

pub(crate) fn parse_param_value(bytes: &[u8], t: &ColType, n: usize) -> Result<Value, ExecError> {
    let bad = |msg: String| {
        exec_err(
            "22P02",
            format!("invalid input syntax for type {}: {}", t.sql_name(), msg),
        )
    };
    match t {
        // v0.78: array parameters arrive as the `{...}` literal text —
        // the same representation values are carried in.
        ColType::Text | ColType::Array(_) => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| exec_err("22021", "invalid byte sequence for encoding \"UTF8\""))?;
            Ok(Value::text(s))
        }
        // v1.39: bit-string parameters go through PG19 `bit_in`
        // semantics (22P02 on a bad digit).
        ColType::Bit => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| exec_err("22021", "invalid byte sequence for encoding \"UTF8\""))?;
            match crate::storage::BitString::parse_input(s) {
                Ok(b) => Ok(Value::BitString(b)),
                Err(msg) => Err(bad(msg)),
            }
        }
        // v0.35: parameter input goes through the type's input function,
        // i.e. assignment semantics (blank-tolerant, 22001 on excess).
        ColType::Char(n) => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| exec_err("22021", "invalid byte sequence for encoding \"UTF8\""))?;
            eval_char_assign(s, *n, true)
        }
        ColType::Varchar(n) => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| exec_err("22021", "invalid byte sequence for encoding \"UTF8\""))?;
            eval_char_assign(s, *n, false)
        }
        // v0.36: parameter input for "char" goes through charin (total:
        // first byte wins, empty is NUL; never errors).
        ColType::SingleChar => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| exec_err("22021", "invalid byte sequence for encoding \"UTF8\""))?;
            Ok(Value::SingleChar(crate::storage::char_in(s)))
        }
        // v0.57: parameter input for `name` goes through namein:
        // silent truncation at 63 bytes (NAMEDATALEN-1).
        ColType::Name => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| exec_err("22021", "invalid byte sequence for encoding \"UTF8\""))?;
            Ok(Value::text(crate::storage::truncate_name(s)))
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
        // v1.40: parameter input for tid goes through tidin `(b,o)`
        // (PG19 `tidin`).
        ColType::Tid => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            let s = s.trim();
            let inner = s
                .strip_prefix('(')
                .and_then(|t| t.strip_suffix(')'))
                .ok_or_else(|| bad(format!("\"{s}\"")))?;
            let (bs, os) = inner
                .split_once(',')
                .ok_or_else(|| bad(format!("\"{s}\"")))?;
            let b: u32 = bs.trim().parse().map_err(|_| bad(format!("\"{s}\"")))?;
            let o: u32 = os.trim().parse().map_err(|_| bad(format!("\"{s}\"")))?;
            Ok(Value::Tid(b, o))
        }
        // v1.17: parameter input for xid goes through xidin (unsigned
        // decimal, like PG).
        ColType::Xid => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            s.trim()
                .parse::<u64>()
                .map(|w| Value::Int(w as i64))
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
        ColType::Numeric(..) => {
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
        // v0.64: pg_lsn input (text format HIGH/LOW hex).
        ColType::PgLsn => {
            let s = std::str::from_utf8(bytes).map_err(|_| bad("not valid UTF-8".into()))?;
            let s = s.trim();
            let parts: Vec<&str> = s.split('/').collect();
            if parts.len() != 2 {
                return Err(bad(format!("\"{}\"", s)));
            }
            let high = u64::from_str_radix(parts[0], 16).map_err(|_| bad(format!("\"{}\"", s)))?;
            let low = u64::from_str_radix(parts[1], 16).map_err(|_| bad(format!("\"{}\"", s)))?;
            Ok(Value::PgLsn((high << 32) | low))
        }
        // v0.37: regclass input not supported via binary protocol.
        ColType::Regclass => Err(bad("invalid input syntax for type regclass".into())),
        // v0.73: record input is not supported (no composite input
        // function); json accepts its text form.
        ColType::Record => Err(bad("invalid input syntax for type record".into())),
        // v0.81: no composite input function either — same as record.
        ColType::Composite => Err(bad("invalid input syntax for type composite".into())),
        ColType::Json => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| exec_err("22021", "invalid byte sequence for encoding \"UTF8\""))?;
            Ok(Value::text(s))
        }
    }
    .map_err(|e| {
        // Keep the parameter number in the message for debuggability.
        ExecError {
            detail: None,
            code: e.code,
            message: format!("parameter ${}: {}", n, e.message),
        }
    })
}

/// v0.74: evaluate a SQL-level EXECUTE argument expression against an
/// empty scope (no tables). Used to bind `$N` in prepared statements.
pub fn eval_execute_arg(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    e: &Expr,
) -> Result<Value, ExecError> {
    eval_dml_expr(eng, snap, own, session, role, &[], None, e, &[], None, None)
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
                    match v {
                        InsertValue::Param(p) => {
                            *v = InsertValue::Lit(param_literal(*p, params)?);
                        }
                        // v0.24: substitute parameters inside VALUES
                        // expressions.
                        InsertValue::Expr(e) => subst_expr(e, params)?,
                        _ => {}
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
        // v1.03: EXPLAIN's inner SELECT can reference parameters
        // (function arguments / plpgsql variables) - substitute them
        // so FOR loops over EXPLAIN see current bindings.
        Stmt::Explain { stmt, .. } => subst_params(stmt, params),
        Stmt::Update {
            sets,
            from,
            where_,
            with,
            returning,
            ..
        } => {
            subst_ctes(with, params)?;
            // v0.76: parameters inside UPDATE ... FROM items (derived
            // tables, functions, VALUES).
            for f in from {
                subst_from(f, params)?;
            }
            for (_, e) in sets {
                subst_expr(e, params)?;
            }
            if let Some(w) = where_ {
                subst_expr(w, params)?;
            }
            subst_returning(returning, params)
        }
        Stmt::Delete {
            using,
            where_,
            with,
            returning,
            ..
        } => {
            subst_ctes(with, params)?;
            // v0.76: parameters inside DELETE ... USING items.
            for f in using {
                subst_from(f, params)?;
            }
            if let Some(w) = where_ {
                subst_expr(w, params)?;
            }
            subst_returning(returning, params)
        }
        _ => Ok(()),
    }
}

/// v0.10: substitute parameters inside every CTE body.
pub(crate) fn subst_ctes(ctes: &mut [CteDef], params: &[Option<Value>]) -> Result<(), ExecError> {
    for cte in ctes {
        match &mut cte.body {
            CteBody::Simple(s) => subst_select(s, params)?,
            CteBody::Union { left, right, .. } => {
                subst_select(left, params)?;
                subst_select(right, params)?;
            }
            // v1.39: substitute inside the data-modifying statement.
            CteBody::Dml(stmt) => subst_params(stmt, params)?,
        }
    }
    Ok(())
}

/// v0.10: substitute parameters inside a RETURNING list.
pub(crate) fn subst_returning(
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
pub(crate) fn subst_on_conflict(
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

pub(crate) fn subst_select(s: &mut SelectStmt, params: &[Option<Value>]) -> Result<(), ExecError> {
    // v0.10: CTE bodies.
    subst_ctes(&mut s.with, params)?;
    // v0.44: set-operation branches and the root ORDER BY.
    if let Some(root) = s.set_op.as_mut() {
        subst_select(&mut root.left, params)?;
        for b in &mut root.chain {
            subst_select(&mut b.right, params)?;
        }
        for term in &mut root.order_by {
            subst_expr(&mut term.expr, params)?;
        }
        return Ok(());
    }
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
    for g in s.group_by.iter_mut().flatten() {
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

pub(crate) fn subst_from(f: &mut FromItem, params: &[Option<Value>]) -> Result<(), ExecError> {
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
        // v0.32: table-function args may hold $n parameters.
        FromItem::Function { args, .. } => {
            for a in args {
                subst_expr(a, params)?;
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

pub(crate) fn param_literal(p: u32, params: &[Option<Value>]) -> Result<Literal, ExecError> {
    let v = params
        .get((p - 1) as usize)
        .ok_or_else(|| exec_err("42P02", format!("there is no parameter ${}", p)))?;
    Ok(value_to_literal(v))
}

/// v0.72: losslessly lower a runtime `Value` to a `Literal`
/// (extracted from `param_literal`; also used to re-inject expanded
/// SRF outputs into INSERT VALUES rows).
pub(crate) fn value_to_literal(v: &Option<Value>) -> Literal {
    match v {
        None => Literal::Null,
        Some(Value::SmallInt(i)) => Literal::SmallInt(*i),
        Some(Value::Int(i)) => Literal::Int(*i),
        Some(Value::BigInt(i)) => Literal::BigInt(*i),
        Some(Value::Float4(f)) => Literal::Real(*f),
        Some(Value::Float(f)) => Literal::Float(*f),
        Some(Value::Numeric(n)) => Literal::Numeric(n.clone()),
        Some(Value::Text(s)) => Literal::Text(s.clone()),
        // v0.35: a bpchar parameter substitutes as its (padded) text;
        // downstream coercion re-applies any target typmod.
        Some(Value::BpChar(s)) => Literal::Text(s.clone()),
        // v0.36: a "char" parameter substitutes as its charout text;
        // downstream charin re-applies the input function (round-trips,
        // since charout is charin's fixed point for single bytes).
        Some(Value::SingleChar(b)) => Literal::Text(crate::storage::char_out(*b).into()),
        Some(Value::Bool(b)) => Literal::Bool(*b),
        Some(Value::Date(d)) => Literal::Date(*d),
        Some(Value::Timestamp(m)) => Literal::Timestamp(*m),
        Some(Value::Timestamptz(m)) => Literal::Timestamptz(*m),
        Some(Value::Bytea(b)) => Literal::Bytea(b.clone()),
        // v1.39: bit strings substitute losslessly as bit literals.
        Some(Value::BitString(b)) => Literal::BitString(b.clone()),
        Some(Value::Uuid(u)) => Literal::Uuid(*u),
        // v0.64: pg_lsn parameter substitutes as its text format.
        Some(Value::PgLsn(lsn)) => {
            Literal::Text(format!("{:X}/{:08X}", lsn >> 32, lsn & 0xFFFF_FFFF).into())
        }
        // v1.40: tid parameter substitutes as its `(b,o)` text format.
        Some(Value::Tid(b, o)) => Literal::Text(format!("({b},{o})").into()),
        // v0.73: records never arrive as parameters (parse_param_value
        // rejects the record type); substituting NULL is unreachable.
        Some(Value::Record(_)) => Literal::Null,
        // v0.79: an array parameter substitutes as its `{...}` literal
        // text (dims prefix included, so lower bounds round-trip);
        // downstream coercion re-parses it for the target type.
        Some(Value::Array(a)) => Literal::Text(a.to_literal().into()),
        Some(Value::Null) => Literal::Null,
    }
}

pub(crate) fn subst_expr(e: &mut Expr, params: &[Option<Value>]) -> Result<(), ExecError> {
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
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => {
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
        Expr::Not(x)
        | Expr::BitNot(x)
        | Expr::Neg(x)
        | Expr::IsNull { expr: x, .. }
        | Expr::IsBool { expr: x, .. } => subst_expr(x, params)?,
        Expr::IsDistinctFrom { left, right, .. } => {
            subst_expr(left, params)?;
            subst_expr(right, params)?;
        }
        Expr::Cast { expr, .. } => subst_expr(expr, params)?,
        // v0.86: named casts (e.g. `row($1,$2)::int8_tbl` in function bodies).
        Expr::CastNamed { expr, .. } => subst_expr(expr, params)?,
        // v0.86: parameters inside ROW(...) constructors (function
        // bodies like `select row($1,$2)::t`).
        Expr::Row(elems) => {
            for e in elems {
                subst_expr(e, params)?;
            }
        }
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
        // v1.30: ordered-set aggregate — params may hide in the
        // direct args, the WITHIN GROUP sort keys, or the FILTER.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            for a in direct_args {
                subst_expr(a, params)?;
            }
            for o in within_order_by {
                subst_expr(&mut o.expr, params)?;
            }
            if let Some(f) = filter {
                subst_expr(f, params)?;
            }
        }
        Expr::ScalarSub(s) => subst_select(s, params)?,
        Expr::InSub { expr, sub, .. } => {
            subst_expr(expr, params)?;
            subst_select(sub, params)?;
        }
        Expr::Exists { sub, .. } => subst_select(sub, params)?,
        // v1.02: the named-arg rewriter (`rewrite_func_arg_expr`)
        // descends through every scalar form, so the substitutor must
        // too — otherwise a `Param` produced by the rewrite inside one
        // of these forms would survive to eval and 42P02. Purely
        // additive: these positions previously fell through to `_`,
        // leaving the Param unsubstituted (always an eval error).
        Expr::FieldAccess { expr, .. } | Expr::NamedArg { expr, .. } => subst_expr(expr, params)?,
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(op) = operand {
                subst_expr(op, params)?;
            }
            for (k, v) in whens {
                subst_expr(k, params)?;
                subst_expr(v, params)?;
            }
            if let Some(el) = else_ {
                subst_expr(el, params)?;
            }
        }
        Expr::ArrayCtor { elems, .. } => {
            for e in elems {
                subst_expr(e, params)?;
            }
        }
        Expr::Subscript { array, indices } => {
            subst_expr(array, params)?;
            for i in indices {
                subst_expr(i, params)?;
            }
        }
        Expr::Slice { array, bounds } => {
            subst_expr(array, params)?;
            for (lo, hi) in bounds {
                if let Some(l) = lo {
                    subst_expr(l, params)?;
                }
                if let Some(h) = hi {
                    subst_expr(h, params)?;
                }
            }
        }
        Expr::UserOp { left, right, .. } => {
            subst_expr(left, params)?;
            subst_expr(right, params)?;
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args.iter_mut().chain(partition_by.iter_mut()) {
                subst_expr(a, params)?;
            }
            for o in order_by {
                subst_expr(&mut o.expr, params)?;
            }
        }
        Expr::Quantified { left, sub, .. } => {
            subst_expr(left, params)?;
            subst_select(sub, params)?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn dummy_value(t: &ColType) -> Value {
    match t {
        ColType::SmallInt => Value::SmallInt(0),
        ColType::Int => Value::Int(0),
        ColType::BigInt => Value::BigInt(0),
        ColType::Float4 => Value::Float4(0.0),
        ColType::Float => Value::Float(0.0),
        ColType::Numeric(..) => Value::Numeric(Numeric::zero()),
        ColType::Text => Value::text(""),
        ColType::Char(_) => Value::bpchar(""),       // v0.35
        ColType::Varchar(_) => Value::text(""),      // v0.35
        ColType::SingleChar => Value::SingleChar(0), // v0.36
        ColType::Bool => Value::Bool(false),
        ColType::Date => Value::Date(0),
        ColType::Timestamp => Value::Timestamp(0),
        ColType::Timestamptz => Value::Timestamptz(0),
        ColType::Bytea => Value::Bytea(Vec::new()),
        // v1.39: the empty bit string.
        ColType::Bit => Value::BitString(crate::storage::BitString {
            bitlen: 0,
            bytes: Vec::new(),
        }),
        ColType::Uuid => Value::Uuid([0; 16]),
        ColType::Regclass => Value::Text("".into()),
        ColType::Name => Value::text(""),  // v0.57
        ColType::PgLsn => Value::PgLsn(0), // v0.64
        // v1.17: xids are carried as Value::Int.
        ColType::Xid => Value::Int(0),
        ColType::Tid => Value::Tid(0, 0), // v1.40
        // v0.73: records never appear as parameter types in practice;
        // the empty record is the honest dummy.
        ColType::Record => Value::Record(Vec::new()),
        // v0.81: named composites likewise never appear as parameter
        // types; the empty record is the honest dummy here too.
        ColType::Composite => Value::Record(Vec::new()),
        ColType::Json => Value::text(""),
        // v0.78: the empty array literal is the honest dummy; values
        // are carried as the `{...}` literal text.
        ColType::Array(_) => Value::text("{}"),
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
    session: u64,
) -> Result<Option<Vec<(String, ColType)>>, ExecError> {
    match stmt {
        Stmt::Select(sel) => {
            let eff = resolve_param_types(stmt, declared, eng, snap, own, session)?;
            let mut s = sel.clone();
            let dummy: Vec<Option<Value>> = eff.iter().map(|t| Some(dummy_value(t))).collect();
            subst_select(&mut s, &dummy)?;
            Ok(Some(describe_select(
                eng,
                snap,
                own,
                session,
                &s,
                &[],
                &[],
            )?))
        }
        // v0.10: DML with RETURNING describes the RETURNING list; without
        // it there are no result columns (like a plain command tag).
        Stmt::Insert {
            table, returning, ..
        } => {
            if returning.is_empty() {
                Ok(None)
            } else {
                Ok(Some(
                    describe_returning(eng, snap, own, session, table, table, &[], returning)?.0,
                ))
            }
        }
        // v0.76: UPDATE ... FROM — RETURNING may reference the FROM
        // tables (PG19); the alias (if any) is the visible qualifier.
        Stmt::Update {
            table,
            alias,
            from,
            returning,
            with,
            ..
        } => {
            if returning.is_empty() {
                Ok(None)
            } else {
                let extra = from_schemas(eng, snap, own, session, from, with, &[], &[], &[])?;
                Ok(Some(
                    describe_returning(
                        eng,
                        snap,
                        own,
                        session,
                        table,
                        alias.as_deref().unwrap_or(table),
                        &extra,
                        returning,
                    )?
                    .0,
                ))
            }
        }
        // v0.65: DELETE's alias (if any) is the visible qualifier.
        // v0.76: DELETE ... USING — RETURNING may reference the USING
        // tables (PG19).
        Stmt::Delete {
            table,
            alias,
            using,
            returning,
            with,
            ..
        } => {
            if returning.is_empty() {
                Ok(None)
            } else {
                let extra = from_schemas(eng, snap, own, session, using, with, &[], &[], &[])?;
                Ok(Some(
                    describe_returning(
                        eng,
                        snap,
                        own,
                        session,
                        table,
                        alias.as_deref().unwrap_or(table),
                        &extra,
                        returning,
                    )?
                    .0,
                ))
            }
        }
        _ => Ok(None),
    }
}
