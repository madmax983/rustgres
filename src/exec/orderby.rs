// v1.78 mechanical split: moved verbatim from src/exec.rs (35928-36409).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// ORDER BY
// ---------------------------------------------------------------------------

/// Output-column positions of each select item (None for `*` expansions),
/// for matching ORDER BY terms against the select list structurally.
pub(crate) fn select_item_positions(stmt: &SelectStmt, schema: &[QCol]) -> Vec<Option<usize>> {
    let mut pos = 0;
    let mut out = Vec::new();
    for item in &stmt.items {
        match item {
            SelectItem::All => {
                // v0.23: hidden columns are skipped by `*`.
                pos += schema.iter().filter(|c| !c.hidden).count();
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

pub(crate) fn apply_order(
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

            hidden: false,
            src_ord: 0,
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
        // v1.28: PG19 evaluates ORDER BY after the ProjectSet — rebind
        // this fanned row's SRF values so an ORDER BY term naming an SRF
        // call textually sees the fanned value (PG19 allows SRFs in ORDER
        // BY per `check_srf_call_placement`). Non-fanned rows carry no
        // bindings; save/restore keeps any outer fan-out intact.
        let saved_srf = std::mem::replace(&mut q.srf_vals, o.srf.clone());
        let row_keys = match &o.sort_keys {
            Some(k) => k.clone(),
            None => {
                let mut fallback = |e: &Expr| -> Result<Value, ExecError> {
                    if stmt.distinct && stmt.distinct_on.is_empty() {
                        // DISTINCT queries can only sort by their output.
                        // v0.52: DISTINCT ON is exempt — its ORDER BY may
                        // name non-projected columns (PG allows
                        // `SELECT DISTINCT ON (a) a FROM t ORDER BY a, b`).
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
                        row: o.full.as_deref().unwrap_or(&[]),
                        // v1.17: ORDER BY terms may reference xmin/xmax;
                        // give the fallback scope the row provenance.
                        prov: Some(&o.prov),
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
        q.srf_vals = saved_srf;
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
            srf: std::mem::take(&mut orows[i].srf),
        });
    }
    *orows = sorted;
    Ok(())
}

/// v0.52: expand the SELECT list to one expression per output column, for
/// resolving ORDER BY ordinals in the DISTINCT ON prefix check. `*`
/// expands against the schema (skipping hidden columns, like projection);
/// `qual.*` uses the qualifier's source-column order. Returns None when
/// the ordinal is out of range or `*` cannot be expanded (no schema — the
/// EXPLAIN planner path).
pub(crate) fn nth_output_expr(
    items: &[SelectItem],
    schema: Option<&[QCol]>,
    n: i64,
) -> Option<Expr> {
    if n < 1 {
        return None;
    }
    let mut idx = 0i64;
    for item in items {
        match item {
            SelectItem::Expr { expr, .. } => {
                idx += 1;
                if idx == n {
                    return Some(expr.clone());
                }
            }
            SelectItem::All => {
                let schema = schema?;
                for c in schema.iter().filter(|c| !c.hidden) {
                    idx += 1;
                    if idx == n {
                        return Some(Expr::Column {
                            table: Some(c.qual.clone()),
                            name: c.name.clone(),
                        });
                    }
                }
            }
            SelectItem::AllOf(qual) => {
                let schema = schema?;
                for i in qual_star_order(schema, qual) {
                    idx += 1;
                    if idx == n {
                        let c = &schema[i];
                        return Some(Expr::Column {
                            table: Some(c.qual.clone()),
                            name: c.name.clone(),
                        });
                    }
                }
            }
        }
    }
    None
}

/// v0.52: resolve an ORDER BY term to the expression it denotes, for the
/// DISTINCT ON prefix check. Ordinals resolve to the nth output expression
/// (`*` expanded via the schema when available); an unqualified column
/// naming an explicit output alias resolves to the aliased expression (PG
/// prefers the output name). Anything else denotes itself. Returns None
/// when the term cannot be resolved statically.
pub(crate) fn resolve_distinct_order_term(
    items: &[SelectItem],
    schema: Option<&[QCol]>,
    expr: &Expr,
) -> Option<Expr> {
    if let Expr::Literal(Literal::Int(n)) = expr {
        return nth_output_expr(items, schema, *n);
    }
    if let Expr::Column { table: None, name } = expr {
        for item in items {
            if let SelectItem::Expr {
                expr: e,
                alias: Some(a),
            } = item
            {
                if a == name {
                    return Some(e.clone());
                }
            }
        }
    }
    Some(expr.clone())
}

/// v0.52: PG19 compares analyzed DISTINCT ON / ORDER BY expressions with
/// `equal()`. After parse analysis `don.a` and `a` are the same Var, so a
/// syntactic check must treat a qualified column as matching an
/// unqualified column with the same name (and vice versa). Anything else
/// needs structural equality. (Known gap: in a join, `t.a` also matches an
/// unqualified `a` that actually denotes `s.a`; PG would 42803 there while
/// this accepts. The mismatch is fail-safe — it can only sort by a term
/// the query already sorts by.)
pub(crate) fn distinct_exprs_match(d: &Expr, o: &Expr) -> bool {
    if d == o {
        return true;
    }
    match (d, o) {
        (
            Expr::Column {
                table: t1,
                name: n1,
            },
            Expr::Column {
                table: t2,
                name: n2,
            },
        ) if n1 == n2 => t1 == t2 || t1.is_none() || t2.is_none(),
        _ => false,
    }
}

/// v0.52: PG19 `transformDistinctOnClause` (parse_clause.c), as a syntactic
/// check. Verifies the DISTINCT ON expressions match the initial ORDER BY
/// expressions (42803: permutations and short prefixes are fine — missing
/// keys are appended implicitly; a key after a non-key term is an error)
/// and builds the *effective* sort order: the ORDER BY terms matching
/// DISTINCT ON keys (in ORDER BY order, each keeping its direction), then
/// any unmatched DISTINCT ON keys as implicit `ASC NULLS LAST` terms, then
/// the remaining ORDER BY tail. Also returns, per DISTINCT ON expression,
/// its index in the effective order, so the aggregate path can derive
/// group keys from the sort keys exec_agg precomputes. `schema` (None from
/// the EXPLAIN planner) is only needed to expand `*` for ORDER BY
/// ordinals.
pub(crate) fn check_distinct_on_order(
    stmt: &SelectStmt,
    schema: Option<&[QCol]>,
) -> Result<(Vec<OrderTerm>, Vec<usize>), ExecError> {
    let n = stmt.distinct_on.len();
    let resolved: Vec<Option<Expr>> = stmt
        .order_by
        .iter()
        .map(|t| resolve_distinct_order_term(&stmt.items, schema, &t.expr))
        .collect();
    let mut effective: Vec<OrderTerm> = Vec::new();
    let mut term_matched = vec![false; stmt.order_by.len()];
    let mut key_matched = vec![false; n];
    let mut key_pos = vec![usize::MAX; n];
    let mut seen_tail = false;
    for (i, (term, re)) in stmt.order_by.iter().zip(resolved.iter()).enumerate() {
        let mut hit = None;
        if let Some(re) = re {
            for (j, d) in stmt.distinct_on.iter().enumerate() {
                if !key_matched[j] && distinct_exprs_match(d, re) {
                    hit = Some(j);
                    break;
                }
            }
        }
        match hit {
            Some(j) => {
                // A DISTINCT ON key after a non-key ORDER BY term can
                // never be a prefix (PG19 42803).
                if seen_tail {
                    return Err(exec_err(
                        "42803",
                        "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
                    ));
                }
                key_matched[j] = true;
                key_pos[j] = effective.len();
                term_matched[i] = true;
                effective.push(term.clone());
            }
            None => {
                seen_tail = true;
            }
        }
    }
    // Unmatched DISTINCT ON keys become implicit sort keys (PG19 sorts by
    // them); a non-key ORDER BY term before them is 42803.
    for (j, d) in stmt.distinct_on.iter().enumerate() {
        if !key_matched[j] {
            if seen_tail {
                return Err(exec_err(
                    "42803",
                    "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
                ));
            }
            key_pos[j] = effective.len();
            effective.push(OrderTerm {
                expr: d.clone(),
                desc: false,
                nulls_first: None,
            });
        }
    }
    // The non-key ORDER BY tail keeps its relative order.
    for (i, term) in stmt.order_by.iter().enumerate() {
        if !term_matched[i] {
            effective.push(term.clone());
        }
    }
    Ok((effective, key_pos))
}

/// v0.52: `SELECT DISTINCT ON` — keep the first row of each group of rows
/// whose DISTINCT ON expressions are equal (NULLs group together, via the
/// canonical `value_key`, like the plain-DISTINCT filter). Callers sort by
/// the effective ORDER BY (distinct keys ++ tail) first — PG19 always
/// sorts, even with no user ORDER BY — so "first" is the ORDER BY winner.
/// Group keys are evaluated against the full pre-projection row, which
/// DISTINCT ON queries always retain. `distinct_pos` is Some on the
/// aggregate path: there is no pre-projection row, so the keys are derived
/// from the sort keys exec_agg precomputed for the effective ORDER BY
/// (each DISTINCT ON expression's index into them).
pub(crate) fn distinct_on_filter(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
    orows: &mut Vec<OutRow>,
    distinct_pos: Option<&[usize]>,
) -> Result<(), ExecError> {
    // Aggregate path: derive group keys from precomputed sort keys.
    if let Some(dpos) = distinct_pos {
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut kept: Vec<OutRow> = Vec::with_capacity(orows.len());
        for o in orows.drain(..) {
            let sk = o.sort_keys.as_ref().expect(
                "DISTINCT ON with aggregates precomputes sort keys for the effective ORDER BY",
            );
            let mut k = Vec::new();
            for &p in dpos {
                value_key(&sk[p], &mut k);
            }
            if seen.insert(k) {
                kept.push(o);
            }
        }
        *orows = kept;
        return Ok(());
    }
    // Plain path: evaluate the DISTINCT ON expressions against the full
    // pre-projection row. Window terms read their precomputed values via
    // the row's pre-projection index (like apply_order does).
    let mut keys: Vec<Vec<u8>> = Vec::with_capacity(orows.len());
    for o in orows.iter() {
        if let Some(wi) = o.win_idx {
            if let Some(wctx) = q.wctx.as_mut() {
                wctx.row = wi;
            }
        }
        let frame = Scope {
            schema,
            row: o.full.as_deref().unwrap_or(&[]),
            prov: None,
        };
        let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
        scopes.extend_from_slice(outer);
        scopes.push(frame);
        let mut k = Vec::new();
        for e in &stmt.distinct_on {
            value_key(&eval_expr(q, &scopes, e)?, &mut k);
        }
        keys.push(k);
    }
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut kept: Vec<OutRow> = Vec::with_capacity(orows.len());
    for (o, k) in orows.drain(..).zip(keys.into_iter()) {
        if seen.insert(k) {
            kept.push(o);
        }
    }
    *orows = kept;
    Ok(())
}

/// One ORDER BY key. Resolution order, Postgres-style:
/// 1. a positive integer literal = 1-based position in the select list;
/// 2. an expression structurally identical to a select-list expression;
/// 3. an unqualified name matching an output column (covers aliases);
/// 4. the caller-supplied fallback: for plain non-DISTINCT queries, any
///    expression over the FROM row (the v0.1-v0.5 behavior); for aggregated
///    queries, a group-level expression (aggregates / GROUP BY columns);
///    for plain DISTINCT queries, an error — output only (v0.52: DISTINCT
///    ON is exempt and may sort by non-projected columns, like Postgres).
pub(crate) fn order_key(
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
