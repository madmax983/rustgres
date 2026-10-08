// v1.78 mechanical split: moved verbatim from src/exec.rs (33133-35927).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// Aggregation (v0.6)
// ---------------------------------------------------------------------------

/// v0.94: canonical exact-numeric encoding for [`value_key`], via
/// `Numeric::hash_key`: (special, negative, decimal magnitude digits,
/// scale). Equal numerics (per `Numeric::cmp`) share one encoding —
/// `1::int`, `1::bigint`, `1.0::numeric` group together — and NaN /
/// +Infinity / -Infinity stay distinct, matching PG19's hash_numeric
/// (trailing zeros omitted from the hash input; specials hashed by
/// kind).
pub(crate) fn value_key_numeric(out: &mut Vec<u8>, n: &crate::storage::Numeric) {
    let (special, neg, mag, scale) = n.hash_key();
    out.push(special);
    out.push(neg as u8);
    // v1.70: `mag` avoids a `BigUint` entirely for the common case
    // (see `Numeric::hash_key`/`NumKeyMag`); both arms render the exact
    // same digit string for equal magnitudes (no leading zeros, "0" for
    // zero), so the encoded bytes are unchanged either way.
    let digits = match &mag {
        crate::storage::NumKeyMag::Small(v) => v.to_string(),
        crate::storage::NumKeyMag::Big(b) => b.to_decimal_string(),
    };
    out.extend_from_slice(&(digits.len() as u64).to_be_bytes());
    out.extend_from_slice(digits.as_bytes());
    out.extend_from_slice(&scale.to_be_bytes());
}

/// Append a canonical byte key for a value (for GROUP BY / DISTINCT).
/// Floats hash canonically so NaN groups with NaN and -0.0 with 0.0,
/// like Postgres (PG19 hashfloat4/hashfloat8).
/// Hash key for GROUP BY / DISTINCT. Exact numerics (int2/int4/int8/
/// numeric) canonicalize via `Numeric::hash_key` — (special, sign,
/// magnitude, scale) with trailing zeros stripped — so `1::int`,
/// `1::bigint` and `1.0::numeric` group together, and NaN/+Inf/-Inf
/// stay distinct (PG19 hash_numeric semantics).
pub(crate) fn value_key(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0),
        Value::SmallInt(i) => {
            out.push(8);
            value_key_numeric(out, &crate::storage::Numeric::new(*i as i128, 0));
        }
        Value::Int(i) => {
            out.push(8);
            value_key_numeric(out, &crate::storage::Numeric::new(*i as i128, 0));
        }
        Value::BigInt(i) => {
            out.push(8);
            value_key_numeric(out, &crate::storage::Numeric::new(*i as i128, 0));
        }
        Value::Numeric(n) => {
            out.push(8);
            value_key_numeric(out, n);
        }
        Value::Float4(f) => {
            out.push(2);
            // v0.94: canonicalized like the hash-join key: -0.0
            // groups with 0.0, all NaNs group together (PG19
            // hashfloat4).
            out.extend_from_slice(&canon_float_key(*f as f64).to_be_bytes());
        }
        Value::Float(f) => {
            out.push(2);
            // v0.94: see Float4.
            out.extend_from_slice(&canon_float_key(*f).to_be_bytes());
        }
        Value::Text(s) => {
            out.push(3);
            out.extend_from_slice(&(s.len() as u64).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        // v0.35: bpchar groups by its significant (rtrimmed) content, so
        // 'ab'::char(3) and 'ab'::text group together, like PG's
        // trailing-space-insensitive bpchar equality.
        Value::BpChar(s) => {
            out.push(3);
            let t = crate::storage::rtrim_spaces(s);
            out.extend_from_slice(&(t.len() as u64).to_be_bytes());
            out.extend_from_slice(t.as_bytes());
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
        // v1.39: bit strings group by (bitlen, bytes) — the bit
        // length matters because trailing bits are padding.
        Value::BitString(b) => {
            out.push(18);
            out.extend_from_slice(&b.bitlen.to_be_bytes());
            out.extend_from_slice(&b.bytes);
        }
        Value::Uuid(u) => {
            out.push(13);
            out.extend_from_slice(u);
        }
        // v0.64: pg_lsn groups by its u64 value.
        Value::PgLsn(lsn) => {
            out.push(15);
            out.extend_from_slice(&lsn.to_be_bytes());
        }
        // v1.40: tid groups by (block, offset).
        Value::Tid(b, o) => {
            out.push(19);
            out.extend_from_slice(&b.to_be_bytes());
            out.extend_from_slice(&o.to_be_bytes());
        }
        // v0.36: the one-byte "char" value groups by its byte.
        Value::SingleChar(b) => {
            out.push(14);
            out.push(*b);
        }
        // v0.73: records group by their field values (names do not
        // participate, like PG's record equality).
        Value::Record(fields) => {
            out.push(16);
            out.extend_from_slice(&(fields.len() as u64).to_be_bytes());
            for (_, f) in fields {
                value_key(f, out);
            }
        }
        // v0.79: arrays group by element type, dims and element keys
        // (PG's hash_array mixes the same inputs). NULL elements hash
        // distinctly from the string "NULL", like every other type.
        Value::Array(a) => {
            out.push(17);
            out.push(a.elem as u8);
            out.extend_from_slice(&(a.dims.len() as u64).to_be_bytes());
            for d in &a.dims {
                out.extend_from_slice(&d.to_be_bytes());
            }
            for l in &a.lower {
                out.extend_from_slice(&l.to_be_bytes());
            }
            for e in &a.elems {
                value_key(e, out);
            }
        }
    }
}

/// Hash-group the rows, then evaluate the select list + HAVING per group.
/// With no GROUP BY and no input rows there is still exactly one (empty)
/// group, so `SELECT count(*)` returns 0 rather than no rows — like
/// Postgres. FOR UPDATE never reaches here (rejected in validation).
/// v0.78: resolve GROUP BY ordinals to select-list expressions, per
/// grouping set. PG19's parse analysis treats an integer literal in
/// GROUP BY as an ordinal reference to the select list (`GROUP BY 1`
/// groups by the first select item), never as a constant. Out-of-range
/// ordinals — and ordinals pointing at `*` items, whose star expansion
/// is unsupported in grouping keys — are 42803 ("GROUP BY position N
/// is not in select list"). With no GROUP BY at all there is a single
/// empty set (the v0.47 grand-total behavior).
pub(crate) fn resolve_grouping_sets(stmt: &SelectStmt) -> Result<Vec<Vec<Expr>>, ExecError> {
    fn resolve_one(stmt: &SelectStmt, set: &[Expr]) -> Result<Vec<Expr>, ExecError> {
        let mut out = Vec::with_capacity(set.len());
        for g in set {
            let ordinal = match g {
                Expr::Literal(Literal::Int(n) | Literal::BigInt(n)) => usize::try_from(*n).ok(),
                _ => None,
            };
            match ordinal {
                Some(n) if n >= 1 => match stmt.items.get(n - 1) {
                    Some(SelectItem::Expr { expr, .. }) => out.push(expr.clone()),
                    _ => {
                        return Err(exec_err(
                            "42803",
                            format!("GROUP BY position {n} is not in select list"),
                        ));
                    }
                },
                Some(n) => {
                    return Err(exec_err(
                        "42803",
                        format!("GROUP BY position {n} is not in select list"),
                    ));
                }
                None => out.push(g.clone()),
            }
        }
        Ok(out)
    }

    if stmt.group_by.is_empty() {
        return Ok(vec![Vec::new()]);
    }
    stmt.group_by
        .iter()
        .map(|set| resolve_one(stmt, set))
        .collect()
}

/// v0.47: evaluate one GROUP BY key expression for a single input row,
/// expanding a top-level SRF call to its output values (PG19's
/// ProjectSet-below-Aggregate: one value per fanned-out row). Non-SRF
/// keys yield exactly one value.
pub(crate) fn eval_group_key_expanded(
    q: &mut Q,
    scopes: &[Scope],
    key: &Expr,
) -> Result<Vec<Value>, ExecError> {
    if let Expr::Func { name, args } = key {
        if is_builtin_srf(name) {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval_expr(q, scopes, a)?);
            }
            return eval_srf_vals(name, &vals);
        }
    }
    Ok(vec![eval_expr(q, scopes, key)?])
}

/// v0.78: dispatcher for grouped aggregation. Plain `GROUP BY` (or no
/// GROUP BY) keeps the exact legacy single-set path; grouping-set
/// syntax (`()`, `ROLLUP`, `CUBE`, `GROUPING SETS`, `DISTINCT`) runs one
/// aggregation per set, with bare select-list columns not in the current
/// set evaluating to NULL (PG19 semantics). Non-trivial expressions over
/// unbound columns still raise 42803, like PG.
/// v1.26: PG19 LATERAL aggregate rule (`parse_agg.c`
/// `check_agglevels_and_constraints`). A LATERAL subquery is transformed
/// with the outer pstate's `p_expr_kind = EXPR_KIND_FROM_SUBSELECT`
/// (`parse_clause.c` `transformRangeSubselect`), and an aggregate call
/// whose argument references *any* outer-level variable (`min_varlevel
/// >= 1`) walks up to that pstate — PG19 rejects it with 42803
/// ("aggregate functions are not allowed in FROM clause of their own
/// query level"). Runs once at the top of `exec_agg` for the lateral
/// subquery's own level (the innermost marker whose lateral level is
/// this query, i.e. `marker.depth + 1 == q.depth`). Only aggregates at
/// this level are examined — subqueries are their own levels and get
/// their own check when their `exec_agg` runs (the marker lookup finds
/// the same marker for nested levels, and `ns_base` stays valid because
/// scopes are only appended). An argument resolving to any scope before
/// this level's own FROM scope (`si < ns_base + 1`, where `own` sits at
/// `ns_base + 1`) triggers the error. Unqualified columns resolve
/// innermost-first, so genuinely local aggregates are unaffected.
pub(crate) fn check_lateral_agg_args(
    q: &Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
) -> Result<(), ExecError> {
    let marker = LATERAL_NS.with(|s| {
        s.borrow()
            .iter()
            .rev()
            .find(|(_, d)| *d + 1 == q.depth)
            .copied()
    });
    let Some((ns_base, _)) = marker else {
        return Ok(());
    };
    // This query level's scope chain: the lateral-visible scopes, then
    // this level's own FROM scope. Resolution is schema-only, so a dummy
    // row suffices (same approach as `is_group_bound`).
    let own = Scope {
        schema,
        row: &[],
        prov: None,
    };
    let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
    scopes.extend_from_slice(outer);
    scopes.push(own);
    // Mirror `is_agg_query`'s aggregate set: select items, HAVING,
    // ORDER BY, DISTINCT ON. `expr_walk` never descends into subquery
    // levels, so only this level's aggregates are seen.
    let mut aggs: Vec<&Expr> = Vec::new();
    for i in &stmt.items {
        if let SelectItem::Expr { expr, .. } = i {
            expr_walk(expr, &mut |x| {
                if matches!(x, Expr::Agg { .. }) {
                    aggs.push(x);
                }
            });
        }
    }
    if let Some(h) = &stmt.having {
        expr_walk(h, &mut |x| {
            if matches!(x, Expr::Agg { .. }) {
                aggs.push(x);
            }
        });
    }
    for o in &stmt.order_by {
        expr_walk(&o.expr, &mut |x| {
            if matches!(x, Expr::Agg { .. }) {
                aggs.push(x);
            }
        });
    }
    for e in &stmt.distinct_on {
        expr_walk(e, &mut |x| {
            if matches!(x, Expr::Agg { .. }) {
                aggs.push(x);
            }
        });
    }
    for agg in aggs {
        let Expr::Agg {
            arg, arg2, filter, ..
        } = agg
        else {
            continue;
        };
        // v1.29: FILTER is evaluated per input row like the aggregate
        // arguments, so its columns get the same lateral-level check.
        for a in [arg.as_deref(), arg2.as_deref(), filter.as_deref()]
            .into_iter()
            .flatten()
        {
            let mut cols: Vec<(Option<&str>, &str)> = Vec::new();
            expr_walk(a, &mut |c| {
                if let Expr::Column { table, name } = c {
                    cols.push((table.as_deref(), name.as_str()));
                }
            });
            for (table, name) in cols {
                // Unresolvable here is another error's business (42703
                // surfaces from evaluation); only a successful
                // outer-level resolution triggers 42803. `own` (this
                // level's FROM scope) sits at `ns_base + 1`; anything
                // before it is an outer query level.
                let Ok((si, _)) = resolve_col(&scopes, table, name) else {
                    continue;
                };
                if si < ns_base + 1 {
                    return Err(exec_err(
                        "42803",
                        "aggregate functions are not allowed in FROM clause of their own query level",
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn exec_agg(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
    rows: &[QRow],
    out_cols: &[(String, ColType)],
    // v0.10: deduplicated window specs for this query level.
    windows: &[ExecWindow],
) -> Result<Vec<OutRow>, ExecError> {
    // v1.26: PG19 LATERAL aggregate rule — an aggregate in a LATERAL
    // subquery whose argument references the parent query level is
    // 42803, raised before any grouping work.
    check_lateral_agg_args(q, outer, stmt, schema)?;
    let sets = resolve_grouping_sets(stmt)?;
    // v0.80: `GROUPING(...)` analysis — fewer than 32 arguments, each
    // matching a grouping expression of this query level (PG19
    // parse_agg.c). Validated once per statement, before any per-set
    // aggregation.
    validate_grouping_calls(stmt, schema, &sets)?;
    if !stmt.group_by_sets {
        debug_assert!(sets.len() <= 1);
        let set = sets.into_iter().next().unwrap_or_default();
        return exec_agg_one(q, outer, stmt, &set, schema, rows, out_cols, windows, None);
    }
    let mut out = Vec::new();
    for set in &sets {
        let null_items = unbound_bare_columns(stmt, schema, set);
        out.extend(exec_agg_one(
            q,
            outer,
            stmt,
            set,
            schema,
            rows,
            out_cols,
            windows,
            Some(&null_items),
        )?);
    }
    Ok(out)
}

/// v0.78: whether a bare column reference (qual, name) is bound by one
/// of the grouping key expressions — the binding half of
/// `grouped_col_value`, without the 42803 error. Column resolution is
/// schema-only, so a dummy scope with an empty row suffices.
pub(crate) fn is_group_bound(group_keys: &[Expr], schema: &[QCol], qual: &str, name: &str) -> bool {
    let gscope = Scope {
        schema,
        row: &[],
        prov: None,
    };
    let qual_opt = if qual.is_empty() { None } else { Some(qual) };
    let Ok((rsi, rci)) = resolve_col(&[gscope], qual_opt, name) else {
        return false;
    };
    for g in group_keys {
        match g {
            Expr::Column {
                table: gq,
                name: gn,
            } => {
                if let Ok((gsi, gci)) = resolve_col(&[gscope], gq.as_deref(), gn) {
                    if gsi == rsi && gci == rci {
                        return true;
                    }
                }
            }
            _ => {
                let probe = Expr::Column {
                    table: qual_opt.map(|s| s.to_string()),
                    name: name.to_string(),
                };
                if *g == probe {
                    return true;
                }
            }
        }
    }
    false
}

/// v0.78: select-item indices whose bare-column projection must yield
/// NULL for the given grouping set (PG19: grouping-set queries show
/// unbound columns as NULL). Only top-level `SelectItem::Expr` with a
/// bare `Expr::Column`, and the individual columns of `*` / `qual.*`
/// expansions, qualify; non-trivial expressions keep the 42803 check.
/// `SelectItem::Expr` entries are keyed by item index; `*`/`qual.*` by
/// `usize::MAX - schema_index` (disjoint from item indices).
pub(crate) fn unbound_bare_columns(
    stmt: &SelectStmt,
    schema: &[QCol],
    group_keys: &[Expr],
) -> std::collections::HashSet<usize> {
    let mut null_items = std::collections::HashSet::new();
    for (i, item) in stmt.items.iter().enumerate() {
        match item {
            SelectItem::Expr { expr, .. } => {
                if let Expr::Column { table, name } = expr {
                    if !is_group_bound(group_keys, schema, table.as_deref().unwrap_or(""), name) {
                        null_items.insert(i);
                    }
                }
            }
            SelectItem::All => {
                for (ci, c) in schema.iter().enumerate() {
                    if c.hidden {
                        continue;
                    }
                    if !is_group_bound(group_keys, schema, &c.qual, &c.name) {
                        null_items.insert(usize::MAX - ci);
                    }
                }
            }
            SelectItem::AllOf(qual) => {
                for i in qual_star_order(schema, qual) {
                    let c = &schema[i];
                    if !is_group_bound(group_keys, schema, qual, &c.name) {
                        null_items.insert(usize::MAX - i);
                    }
                }
            }
        }
    }
    null_items
}

pub(crate) fn exec_agg_one(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    // v0.78: the grouping key expressions for this one grouping set.
    group_keys: &[Expr],
    schema: &[QCol],
    rows: &[QRow],
    out_cols: &[(String, ColType)],
    // v0.10: deduplicated window specs for this query level.
    windows: &[ExecWindow],
    // v0.78: `Some` only for grouping-set queries: select-item indices
    // (and star columns) that must project NULL instead of 42803.
    null_items: Option<&std::collections::HashSet<usize>>,
) -> Result<Vec<OutRow>, ExecError> {
    // Group rows by their GROUP BY key, remembering first-seen order.
    // v0.47: GROUP BY ordinals resolve to select-list expressions (PG19
    // parse analysis: `GROUP BY 1` groups by the first select item);
    // v0.78: resolved per grouping set by the caller.
    // v0.47: positions of top-level SRF calls in the grouping keys. When
    // non-empty, input rows fan out per PG19's ProjectSet-below-Aggregate
    // before grouping, and aggregates fold the expanded rows.
    // v0.47: membership test for the SRF key positions, indexed by group
    // key position.
    let mut is_srf_key = vec![false; group_keys.len()];
    for (i, g) in group_keys.iter().enumerate() {
        if matches!(g, Expr::Func { name, .. } if is_builtin_srf(name)) {
            is_srf_key[i] = true;
        }
    }
    // v0.47: when any grouping key is an SRF, input rows fan out per
    // PG19's ProjectSet-below-Aggregate before grouping, and aggregates
    // fold the expanded rows.
    let has_srf_keys = is_srf_key.iter().any(|&b| b);
    // Expanded working rows (materialized only when SRF keys exist) and
    // the per-row GROUP BY key values.
    let mut xrows: Vec<QRow> = Vec::new();
    let mut xkeys: Vec<Vec<Value>> = Vec::new();
    for row in rows.iter() {
        let frame = Scope {
            schema,
            row: &row.cells,
            // v1.13: tableoid in GROUP BY keys needs row provenance.
            prov: Some(&row.prov),
        };
        let scopes_storage: Vec<Scope>;
        let scopes: &[Scope] = if outer.is_empty() {
            std::slice::from_ref(&frame)
        } else {
            let mut buf: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            buf.extend_from_slice(outer);
            buf.push(frame);
            scopes_storage = buf;
            &scopes_storage
        };
        if !has_srf_keys {
            // GROUP BY exprs cannot contain aggregates (validated).
            let mut key_vals = Vec::with_capacity(group_keys.len());
            for g in group_keys {
                key_vals.push(eval_expr(q, scopes, g)?);
            }
            xkeys.push(key_vals);
        } else {
            // v0.47: SRF-in-GROUP-BY expansion (PG19 nodeProjectSet.c
            // `ExecProjectSRF`): each input row yields one row per SRF
            // element; the width is the max SRF width, exhausted SRFs
            // pad NULL, and an all-empty SRF set drops the row. Plain
            // key expressions repeat their value on every fanned-out row.
            let mut cols: Vec<Vec<Value>> = Vec::with_capacity(group_keys.len());
            let mut width = 0;
            for (ki, g) in group_keys.iter().enumerate() {
                let vals = eval_group_key_expanded(q, scopes, g)?;
                if is_srf_key[ki] {
                    width = width.max(vals.len());
                }
                cols.push(vals);
            }
            for k in 0..width {
                let mut key_vals = Vec::with_capacity(group_keys.len());
                for (ki, vals) in cols.iter().enumerate() {
                    if is_srf_key[ki] {
                        key_vals.push(vals.get(k).cloned().unwrap_or(Value::Null));
                    } else {
                        key_vals.push(vals[0].clone());
                    }
                }
                xrows.push(QRow {
                    cells: row.cells.clone(),
                    prov: row.prov.clone(),
                });
                xkeys.push(key_vals);
            }
        }
    }
    let rows_eff: &[QRow] = if !has_srf_keys { rows } else { &xrows };
    // Group rows by their GROUP BY key, remembering first-seen order.
    let mut group_index: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut groups: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
    // v0.79: reused scratch buffer for the per-row key encoding -- only a
    // first-seen group's key needs its own allocation (cloned into
    // `group_index`); every repeat lookup reuses this same buffer instead
    // of allocating and growing a fresh `Vec<u8>` per row.
    let mut key_bytes: Vec<u8> = Vec::new();
    for (i, key_vals) in xkeys.iter().enumerate() {
        key_bytes.clear();
        for v in key_vals {
            value_key(v, &mut key_bytes);
        }
        match group_index.get(key_bytes.as_slice()) {
            Some(&gi) => groups[gi].1.push(i),
            None => {
                group_index.insert(key_bytes.clone(), groups.len());
                groups.push((key_vals.clone(), vec![i]));
            }
        }
    }
    if groups.is_empty() && group_keys.is_empty() {
        groups.push((Vec::new(), Vec::new()));
    }
    // v0.10: HAVING is evaluated BEFORE windows (Postgres: windows see
    // only groups surviving HAVING). Collect surviving group indices.
    let mut surviving = Vec::with_capacity(groups.len());
    for (gi, (key_vals, idxs)) in groups.iter().enumerate() {
        let first: &[Value] = match idxs.first() {
            Some(&i) => &rows_eff[i].cells,
            None => &[],
        };
        let gscope = Scope {
            schema,
            row: first,
            prov: None,
        };
        let keep = match &stmt.having {
            None => true,
            Some(h) => eval_grouped_bool(
                q, outer, gscope, schema, rows_eff, idxs, key_vals, group_keys, h,
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
            q, outer, schema, rows_eff, &groups, &surviving, group_keys, windows,
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

            hidden: false,
            src_ord: 0,
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
        let (first, first_prov): (&[Value], Option<&[RowProv]>) = match idxs.first() {
            Some(&i) => (&rows_eff[i].cells, Some(&rows_eff[i].prov)),
            None => (&[], None),
        };
        let gscope = Scope {
            schema,
            row: first,
            prov: first_prov,
        };
        // v0.78: grouping-set NULL substitution. `null_items` holds the
        // select-item indices (and `usize::MAX - ci` for star columns)
        // whose bare columns are not in this grouping set: PG projects
        // them as NULL instead of raising 42803.
        let is_null_item = |key: usize| -> bool { null_items.is_some_and(|s| s.contains(&key)) };
        // v1.27: PG19 ProjectSet above the aggregate — an SRF call
        // anywhere in the target list (top-level or nested, e.g.
        // `generate_series(1,50)/10`) fans each grouped row out (the
        // plain path's `expand_srf`, v0.32), unless the SRF is a GROUP
        // BY key (v0.47 reads those from key_vals) or windows are
        // present (plain-path rule: windowed queries keep the scalar
        // reading).
        let expand_srf = windows.is_empty()
            && stmt.items.iter().enumerate().any(|(ii, item)| {
                if is_null_item(ii) {
                    return false;
                }
                match item {
                    SelectItem::Expr { expr, .. } => {
                        if group_keys.iter().any(|g| g == expr) {
                            return false;
                        }
                        let mut slots = Vec::new();
                        collect_target_srfs(q.eng, expr, group_keys, &mut slots);
                        !slots.is_empty()
                    }
                    _ => false,
                }
            });
        // Rows produced by this group: exactly one normally, or the SRF
        // fan-out (possibly zero rows when every SRF is empty). Each
        // fanned row carries its SRF bindings for ORDER BY terms that
        // name an SRF call textually (PG evaluates ORDER BY after the
        // ProjectSet).
        let group_rows: Vec<(Vec<Value>, Vec<(Expr, Value)>)> = if expand_srf {
            project_group_expanded(
                q,
                outer,
                stmt,
                schema,
                gscope,
                rows_eff,
                idxs,
                key_vals,
                group_keys,
                &is_null_item,
            )?
        } else {
            let mut cells = Vec::new();
            for (ii, item) in stmt.items.iter().enumerate() {
                cells.extend(grouped_project_item(
                    q,
                    outer,
                    gscope,
                    schema,
                    rows_eff,
                    idxs,
                    key_vals,
                    group_keys,
                    &is_null_item,
                    ii,
                    item,
                )?);
            }
            vec![(cells, Vec::new())]
        };
        for (cells, row_srf) in group_rows {
            q.srf_vals = row_srf;
            let sort_keys = if stmt.order_by.is_empty() {
                None
            } else {
                let mut fallback = |e: &Expr| {
                    eval_grouped(
                        q, outer, gscope, schema, rows_eff, idxs, key_vals, group_keys, e,
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
                cells: Row::new(cells),
                prov: Vec::new(),
                full: None,
                sort_keys,
                // v0.10: ORDER BY terms with windows need the group index.
                // v1.27: no windows when expanding (see expand_srf above).
                win_idx: if windows.is_empty() { None } else { Some(gi) },
                // v1.28: grouped sort keys are precomputed above with the
                // row's SRF bindings live — nothing to rebind later.
                srf: Vec::new(),
            });
        }
        // v1.27: the fan-out bindings are per-row; never leak them past
        // the group's rows.
        q.srf_vals = Vec::new();
    }
    Ok(out_rows)
}

/// v1.27: project one select-list item for a grouped row, returning its
/// column values (stars expand to several). Shared by the scalar grouped
/// projection and by the SRF fan-out below (for the non-SRF items).
#[allow(clippy::too_many_arguments)]
pub(crate) fn grouped_project_item(
    q: &mut Q,
    outer: &[Scope],
    gscope: Scope,
    schema: &[QCol],
    rows_eff: &[QRow],
    idxs: &[usize],
    key_vals: &[Value],
    group_keys: &[Expr],
    is_null_item: &dyn Fn(usize) -> bool,
    ii: usize,
    item: &SelectItem,
) -> Result<Vec<Value>, ExecError> {
    let mut vals = Vec::new();
    match item {
        SelectItem::All => {
            // v0.23: hidden columns are skipped by `*`.
            for (ci, c) in schema.iter().enumerate().filter(|(_, c)| !c.hidden) {
                if is_null_item(usize::MAX - ci) {
                    vals.push(Value::Null);
                } else {
                    vals.push(grouped_col_value(
                        gscope,
                        group_keys,
                        key_vals,
                        c.qual.as_str(),
                        &c.name,
                    )?);
                }
            }
        }
        SelectItem::AllOf(qual) => {
            // v0.23: expand in the qualifier's source-column order.
            let idx = qual_star_order(schema, qual);
            if idx.is_empty() {
                return Err(exec_err(
                    "42P01",
                    format!("missing FROM-clause entry for table \"{}\"", qual),
                ));
            }
            for i in idx {
                let c = &schema[i];
                if is_null_item(usize::MAX - i) {
                    vals.push(Value::Null);
                } else {
                    vals.push(grouped_col_value(
                        gscope,
                        group_keys,
                        key_vals,
                        qual.as_str(),
                        &c.name,
                    )?);
                }
            }
        }
        SelectItem::Expr { expr, .. } => {
            if is_null_item(ii) {
                vals.push(Value::Null);
            } else {
                vals.push(eval_grouped(
                    q, outer, gscope, schema, rows_eff, idxs, key_vals, group_keys, expr,
                )?);
            }
        }
    }
    Ok(vals)
}

/// v1.27: one SRF occurrence collected from a select item — PG19's
/// ProjectSet target-list scan finds SRF calls nested anywhere in the
/// target list (not just top-level), e.g. `generate_series(1,50)/10`.
/// Each textual occurrence fans out separately; the fan-out zips them
/// with NULL padding. Shared by the grouped path (ProjectSet over Agg)
/// and, since v1.28, the plain path (ProjectSet over the scan).
pub(crate) struct SrfSlot {
    pub(crate) name: String,
    pub(crate) args: Vec<Expr>,
    /// The exact call expression; `eval_grouped`/`eval_expr` match it
    /// structurally against `q.srf_vals`.
    pub(crate) call: Expr,
}

/// v1.27: collect the SRF calls to fan out for one select item. Skips:
/// an SRF call that is itself a GROUP BY key (v0.47 reads those from
/// key_vals), SRFs under aggregate/window calls (those keep the existing
/// below-Agg evaluation path), and SRFs inside subqueries (their own
/// query level handles them, with a fresh Q). v1.28: shared with the
/// plain path, which passes empty group keys.
pub(crate) fn collect_target_srfs(
    eng: &Engine,
    e: &Expr,
    group_keys: &[Expr],
    out: &mut Vec<SrfSlot>,
) {
    match e {
        // Pruned: aggregates and windows keep their existing
        // evaluation paths; subqueries are their own query level.
        // v1.30: ordered-set aggregates prune like plain aggregates.
        Expr::Agg { .. } | Expr::WithinGroup { .. } | Expr::Window { .. } => {}
        Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::InSub { .. }
        | Expr::Quantified { .. }
        | Expr::Exists { .. } => {}
        Expr::Func { name, args } if is_srf(eng, name) => {
            if !group_keys.iter().any(|g| g == e) {
                out.push(SrfSlot {
                    name: name.clone(),
                    args: args.clone(),
                    call: e.clone(),
                });
            }
            // Pruned: no SRF collection inside SRF arguments.
        }
        Expr::NamedArg { expr, .. } => collect_target_srfs(eng, expr, group_keys, out),
        Expr::Column { .. }
        | Expr::ResolvedCol { .. }
        | Expr::Literal(_)
        | Expr::Param(_)
        | Expr::WholeRow { .. } => {}
        Expr::Arith { left, right, .. }
        | Expr::Concat(left, right)
        | Expr::Cmp { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::IsDistinctFrom { left, right, .. } => {
            collect_target_srfs(eng, left, group_keys, out);
            collect_target_srfs(eng, right, group_keys, out);
        }
        Expr::Cast { expr, .. }
        | Expr::CastNamed { expr, .. }
        | Expr::FieldAccess { expr, .. }
        | Expr::Not(expr)
        | Expr::BitNot(expr)
        | Expr::Neg(expr)
        | Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Extract { from: expr, .. } => collect_target_srfs(eng, expr, group_keys, out),
        Expr::Row(elems) => {
            for el in elems {
                collect_target_srfs(eng, el, group_keys, out);
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            collect_target_srfs(eng, expr, group_keys, out);
            collect_target_srfs(eng, pattern, group_keys, out);
            if let Some(esc) = escape {
                collect_target_srfs(eng, esc, group_keys, out);
            }
        }
        Expr::Regex { expr, pattern, .. } => {
            collect_target_srfs(eng, expr, group_keys, out);
            collect_target_srfs(eng, pattern, group_keys, out);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_target_srfs(eng, expr, group_keys, out);
            collect_target_srfs(eng, low, group_keys, out);
            collect_target_srfs(eng, high, group_keys, out);
        }
        Expr::Func { args, .. } => {
            for a in args {
                collect_target_srfs(eng, a, group_keys, out);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            if let Some(o) = operand {
                collect_target_srfs(eng, o, group_keys, out);
            }
            for (k, r) in whens {
                collect_target_srfs(eng, k, group_keys, out);
                collect_target_srfs(eng, r, group_keys, out);
            }
            if let Some(el) = else_ {
                collect_target_srfs(eng, el, group_keys, out);
            }
        }
        Expr::ArrayCtor { elems, .. } => {
            for el in elems {
                collect_target_srfs(eng, el, group_keys, out);
            }
        }
        Expr::Subscript { array, indices, .. } => {
            collect_target_srfs(eng, array, group_keys, out);
            for i in indices {
                collect_target_srfs(eng, i, group_keys, out);
            }
        }
        Expr::Slice { array, bounds, .. } => {
            collect_target_srfs(eng, array, group_keys, out);
            for (lo, hi) in bounds {
                if let Some(b) = lo {
                    collect_target_srfs(eng, b, group_keys, out);
                }
                if let Some(b) = hi {
                    collect_target_srfs(eng, b, group_keys, out);
                }
            }
        }
        Expr::UserOp { left, right, .. } => {
            collect_target_srfs(eng, left, group_keys, out);
            collect_target_srfs(eng, right, group_keys, out);
        }
    }
}

/// v1.27: PG19 ProjectSet above the aggregate (`nodeProjectSet.c`
/// `ExecProjectSRF`). Each grouped row fans out to one output row per
/// SRF element; several SRFs zip with NULL padding for the exhausted
/// ones; an all-empty SRF set drops the grouped row (`hasresult`
/// false). SRF calls are found nested anywhere in the target list
/// (PG19's target-list SRF scan): each select item is evaluated per
/// fanned row with its SRF calls bound to that row's values
/// (`q.srf_vals`, intercepted in `eval_grouped`'s `Expr::Func` arm).
/// SRF arguments evaluate once per group in grouped context
/// (aggregates fold the group, bare columns must be group-bound).
/// Returns the output rows with each row's SRF bindings (the caller
/// rebinds them for ORDER BY terms that name an SRF call textually —
/// PG evaluates ORDER BY after the ProjectSet).
#[allow(clippy::too_many_arguments)]
pub(crate) fn project_group_expanded(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
    gscope: Scope,
    rows_eff: &[QRow],
    idxs: &[usize],
    key_vals: &[Value],
    group_keys: &[Expr],
    is_null_item: &dyn Fn(usize) -> bool,
) -> Result<Vec<(Vec<Value>, Vec<(Expr, Value)>)>, ExecError> {
    let saved_srf = std::mem::take(&mut q.srf_vals);
    // Collect the SRF fan-out slots across the select items. A whole
    // item that is a GROUP BY key (or a grouping-set NULL item) is not
    // scanned: it reads from key_vals / NULL like a plain column.
    let mut slots: Vec<SrfSlot> = Vec::new();
    for (ii, item) in stmt.items.iter().enumerate() {
        if let SelectItem::Expr { expr, .. } = item {
            if !is_null_item(ii) && !group_keys.iter().any(|g| g == expr) {
                collect_target_srfs(q.eng, expr, group_keys, &mut slots);
            }
        }
    }
    // SRF arguments evaluate once per group, in grouped context; the
    // SRF then runs on those values (this is what the old 42883 arm in
    // `eval_grouped` refused to do).
    let mut fans: Vec<Vec<Value>> = Vec::with_capacity(slots.len());
    for slot in &slots {
        let mut avals = Vec::with_capacity(slot.args.len());
        for a in &slot.args {
            avals.push(eval_grouped(
                q, outer, gscope, schema, rows_eff, idxs, key_vals, group_keys, a,
            )?);
        }
        fans.push(if is_builtin_srf(&slot.name) {
            eval_srf_vals(&slot.name, &avals)?
        } else {
            let mut chained: Vec<Scope> = outer.to_vec();
            chained.push(gscope);
            eval_user_srf_vals(q, &chained, &slot.name, &avals)?
        });
    }
    // The fan-out width comes from the SRF columns only — a row is
    // produced only when at least one SRF yields a value. Plain columns
    // repeat and never create rows on their own.
    let width = fans.iter().map(Vec::len).max().unwrap_or(0);
    let mut rows = Vec::with_capacity(width);
    for k in 0..width {
        // Bind this row's SRF values (NULL-padded past exhaustion).
        q.srf_vals = slots
            .iter()
            .zip(&fans)
            .map(|(s, fan)| (s.call.clone(), fan.get(k).cloned().unwrap_or(Value::Null)))
            .collect();
        let mut cells = Vec::new();
        for (ii, item) in stmt.items.iter().enumerate() {
            cells.extend(grouped_project_item(
                q,
                outer,
                gscope,
                schema,
                rows_eff,
                idxs,
                key_vals,
                group_keys,
                is_null_item,
                ii,
                item,
            )?);
        }
        rows.push((cells, q.srf_vals.clone()));
    }
    q.srf_vals = saved_srf;
    Ok(rows)
}

/// A bare column in a grouped query: it must be group-bound — either one
/// of the GROUP BY expressions (same column, or structurally identical)
/// — else 42803, like Postgres.
pub(crate) fn grouped_col_value(
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
pub(crate) fn eval_grouped(
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
    // v0.80: `GROUPING(...)` — PG19's grouping-set mask. The i-th
    // argument (left to right) contributes bit (n-1-i): 1 when the
    // argument is absent from the current grouping set, 0 when it is
    // present. Arguments were validated against the union of all sets in
    // `exec_agg`, so only the current set matters here.
    if let Expr::Func { name, args } = e {
        if name == "grouping" {
            let n = args.len();
            let mut mask: i64 = 0;
            for (i, a) in args.iter().enumerate() {
                if !group_by.iter().any(|k| grouping_arg_matches(schema, a, k)) {
                    mask |= 1i64 << (n - 1 - i);
                }
            }
            return Ok(Value::Int(mask));
        }
    }
    // v0.95: PG19 parse_agg.c — check whole-expression structural
    // equality against GROUP BY before descending into the expression.
    // `SELECT lower(c) ... GROUP BY lower(c)` is legal; without this the
    // evaluator descends into `lower(c)` and rejects the bare `c` with
    // 42803.
    for (i, g) in group_by.iter().enumerate() {
        if e == g {
            return Ok(key_vals[i].clone());
        }
    }
    match e {
        // v0.95: a NamedArg that reaches evaluation unwraps to its value
        // (function-call paths resolve named args first).
        Expr::NamedArg { expr, .. } => eval_grouped(
            q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
        ),
        Expr::Agg {
            func,
            arg,
            distinct,
            arg2,
            agg_order_by,
            filter,
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
            agg_order_by,
            filter.as_deref(),
        ),
        // v1.30: PG19 ordered-set aggregate (WITHIN GROUP).
        Expr::WithinGroup {
            func,
            direct_args,
            within_order_by,
            filter,
        } => eval_within_group_agg(
            q,
            outer,
            gscope,
            schema,
            rows,
            idxs,
            *func,
            direct_args,
            within_order_by,
            filter.as_deref(),
        ),
        Expr::Column { table, name } => {
            let qual = table.as_deref().unwrap_or("");
            // v0.73: PG19 whole-row fallback for a bare range name (see
            // eval_expr); evaluates against the group's first row.
            if qual.is_empty()
                && matches!(
                    resolve_col(&[gscope], None, name),
                    Err(ref e) if e.code == "42703"
                )
                && gscope.schema.iter().any(|c| c.qual == *name)
            {
                let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                scopes.extend_from_slice(outer);
                scopes.push(gscope);
                return eval_wholerow(&scopes, name);
            }
            // v1.13: `tableoid` in grouped evaluation — resolve from the
            // group's first row provenance (the key was computed per-row).
            if name == "tableoid"
                && matches!(
                    resolve_col(&[gscope], table.as_deref(), name),
                    Err(ref e) if e.code == "42703"
                )
            {
                let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                scopes.extend_from_slice(outer);
                scopes.push(gscope);
                // If tableoid is a GROUP BY key, use the computed key value.
                for (i, g) in group_by.iter().enumerate() {
                    if *g
                        == (Expr::Column {
                            table: table.clone(),
                            name: name.clone(),
                        })
                    {
                        return Ok(key_vals[i].clone());
                    }
                }
                return eval_tableoid(q, &scopes, table.as_deref());
            }
            // v1.17: `xmin`/`xmax` in grouped evaluation — resolve from
            // the group's first row provenance, like `tableoid`.
            if (name == "xmin" || name == "xmax")
                && matches!(
                    resolve_col(&[gscope], table.as_deref(), name),
                    Err(ref e) if e.code == "42703"
                )
            {
                let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                scopes.extend_from_slice(outer);
                scopes.push(gscope);
                // If xmin/xmax is a GROUP BY key, use the computed key value.
                for (i, g) in group_by.iter().enumerate() {
                    if *g
                        == (Expr::Column {
                            table: table.clone(),
                            name: name.clone(),
                        })
                    {
                        return Ok(key_vals[i].clone());
                    }
                }
                return eval_xmin_xmax(q, &scopes, table.as_deref(), name);
            }
            grouped_col_value(gscope, group_by, key_vals, qual, name)
        }
        // v0.73: whole-row refs evaluate against the group's first row,
        // like correlated subqueries do here.
        Expr::WholeRow { qual } => {
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(gscope);
            eval_wholerow(&scopes, qual)
        }
        // Resolved columns never reach grouped evaluation (GROUP BY /
        // select-list expressions are resolved per row at runtime).
        Expr::ResolvedCol { .. } => Err(exec_err(
            "XX000",
            "internal error: resolved column in grouped evaluation",
        )),
        // Subqueries are their own query level: evaluate normally, with
        // the group's first row available for correlation.
        // v1.22: `IN (subquery)` / quantified comparisons may hold
        // aggregates in the left side at THIS level (e.g.
        // `(1 = ANY(array_agg(f1))) = ANY (SELECT ...)`); PG computes
        // them with the group and the SubPlan references the value, so
        // evaluate the left side in grouped context first.
        Expr::InSub { expr, sub, neg } if contains_agg(expr) => {
            let lv = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(gscope);
            eval_in_value(q, &scopes, lv, sub, *neg)
        }
        Expr::Quantified {
            left,
            op,
            quant,
            sub,
        } if contains_agg(left) => {
            let lv = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, left,
            )?;
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(gscope);
            eval_quantified_value(q, &scopes, lv, op, *quant, sub)
        }
        Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::InSub { .. }
        | Expr::Quantified { .. }
        | Expr::UserOp { .. }
        | Expr::Exists { .. } => {
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
        Expr::Cast { expr, to, .. } => {
            if let Some(v) = cast_empty_array_ctor(expr, to) {
                return Ok(v);
            }
            let v = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            eval_cast(&v, *to)
        }
        // v0.81: `ROW(a, b, ...)` — evaluates to a composite record with
        // PG's `f1`, `f2`, ... field names.
        // v1.43: `ROW(qual.*, ...)` — PG19 expands the star into the
        // row's field list (flat, sequential f-names), not a nested
        // whole-row value. The grouped scope shape mirrors the
        // `WholeRow` arm below (outer frames, then the group scope).
        Expr::Row(elems) => {
            let mut fields = Vec::new();
            for e in elems {
                if let Expr::WholeRow { qual } = e {
                    let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                    scopes.extend_from_slice(outer);
                    scopes.push(gscope);
                    for v in wholerow_flat_values(&scopes, qual)? {
                        fields.push((format!("f{}", fields.len() + 1), v));
                    }
                } else {
                    let v =
                        eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, e)?;
                    fields.push((format!("f{}", fields.len() + 1), v));
                }
            }
            Ok(Value::Record(fields))
        }
        // v0.81: `(expr).field` — composite field access.
        Expr::FieldAccess { expr, field } => {
            let v = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            match v {
                Value::Record(fields) => fields
                    .into_iter()
                    .find(|(n, _)| n == field)
                    .map(|(_, v)| v)
                    .ok_or_else(|| {
                        exec_err("42703", format!("column \"{}\" not found in record", field))
                    }),
                Value::Null => Ok(Value::Null),
                _ => Err(exec_err(
                    "42809",
                    "cannot access field of non-composite value".to_string(),
                )),
            }
        }
        // v0.81: `expr::named_composite` — resolves the type name against
        // the catalog (42704 if undefined), then coerces the record.
        Expr::CastNamed { expr, name } => {
            let v = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            eval_cast_named(q, &v, name)
        }
        Expr::Concat(a, b) => {
            let va = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, a)?;
            let vb = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, b)?;
            eval_concat(&va, &vb)
        }
        // v0.79: real array expressions (operands evaluate in the
        // grouped scope, like every other scalar expression here).
        Expr::ArrayCtor { elems, nested } => {
            let mut vals = Vec::with_capacity(elems.len());
            for e in elems {
                vals.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, e,
                )?);
            }
            array_ctor_from_vals(vals, *nested, false)
        }
        Expr::Subscript { array, indices } => {
            let va = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, array,
            )?;
            let mut idx_vals = Vec::with_capacity(indices.len());
            for i in indices {
                idx_vals.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, i,
                )?);
            }
            eval_subscript_vals(&va, &idx_vals)
        }
        Expr::Slice { array, bounds } => {
            let va = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, array,
            )?;
            let mut bound_vals = Vec::with_capacity(bounds.len());
            for (l, u) in bounds {
                let lo = match l {
                    Some(l) => Some(eval_grouped(
                        q, outer, gscope, schema, rows, idxs, key_vals, group_by, l,
                    )?),
                    None => None,
                };
                let hi = match u {
                    Some(u) => Some(eval_grouped(
                        q, outer, gscope, schema, rows, idxs, key_vals, group_by, u,
                    )?),
                    None => None,
                };
                bound_vals.push((lo, hi));
            }
            eval_slice_vals(&va, &bound_vals)
        }
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => {
            let va = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            let vb = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, pattern,
            )?;
            let ve = match escape {
                Some(e) => Some(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, e,
                )?),
                None => None,
            };
            eval_like(&va, &vb, ve.as_ref(), *not, *ilike)
        }
        // v0.68: regex match operators.
        Expr::Regex {
            expr,
            pattern,
            not,
            case_insensitive,
        } => {
            let va = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, expr,
            )?;
            let vb = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, pattern,
            )?;
            eval_regex_match(&va, &vb, *not, *case_insensitive)
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
            // v1.27: PG19 ProjectSet-over-Agg — an SRF call collected
            // for fan-out evaluates to its current row's value.
            // `project_group_expanded` sets `q.srf_vals` per fanned row;
            // subqueries run with a fresh Q (empty `srf_vals`), so this
            // only fires for the item's own expression tree.
            if let Some((_, v)) = q.srf_vals.iter().find(|(call, _)| call == e) {
                return Ok(v.clone());
            }
            // v0.95: resolve `name => expr` named arguments to positional
            // order (PG19). Zero-cost when absent.
            let __owned: Vec<Expr>;
            let args: &[Expr] = if args.iter().any(|a| matches!(a, Expr::NamedArg { .. })) {
                __owned = resolve_named_args(q.eng, name, args)?;
                &__owned
            } else {
                args
            };
            // v0.97: pg_typeof static typing (same as the scalar
            // `eval_func` path): domain-typed arguments report the
            // domain name. Grouped scopes chain outer scopes plus the
            // grouping scope, like the regclass coercion below.
            if name == "pg_typeof" && args.len() == 1 {
                let gsc = Scope {
                    schema: gscope.schema,
                    row: gscope.row,
                    prov: gscope.prov,
                };
                let mut chained: Vec<Scope> = outer.to_vec();
                chained.push(gsc);
                if let Some(dname) = pg_typeof_domain_name(q, &chained, &args[0]) {
                    // PG19 still evaluates the argument; only the
                    // reported name is static.
                    eval_grouped(
                        q, outer, gscope, schema, rows, idxs, key_vals, group_by, &args[0],
                    )?;
                    return Ok(Value::text(dname));
                }
                let v = eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, &args[0],
                )?;
                return Ok(Value::text(v.type_name()));
            }
            // v0.47: an SRF in the select list that is also a GROUP BY
            // key reads the group's key value. PG19 expands such SRFs
            // below the aggregate (ProjectSet under Agg), so the
            // targetlist reference is the grouped value, not a fresh
            // scalar call.
            if is_builtin_srf(name) {
                for (i, g) in group_by.iter().enumerate() {
                    if e == g {
                        return Ok(key_vals[i].clone());
                    }
                }
            }
            // v0.37: pg_column_compression and pg_relation_size need
            // engine/provenance access; they cannot go through the pure
            // eval_func_vals path.
            if name == "pg_column_compression" {
                if args.len() != 1 {
                    return Err(exec_err(
                        "42883",
                        "function pg_column_compression() does not exist".to_string(),
                    ));
                }
                // Build scopes from gscope + outer for the type lookup.
                // Grouped values are always computed inline, so there is
                // no base row to attribute (use_prov = false).
                let mut scopes = Vec::with_capacity(outer.len() + 1);
                scopes.extend_from_slice(outer);
                scopes.push(gscope);
                let v = eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, &args[0],
                )?;
                return pg_column_compression_value(q, &scopes, &args[0], &v, false);
            }
            if name == "pg_relation_size" {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    let v =
                        eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, a)?;
                    vals.push(v);
                }
                check_builtin_arity(name, &vals)?;
                return eval_pg_relation_size(q, &vals);
            }
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                // v0.90: `VARIADIC expr` — expand the array into individual
                // args (grouped path). A NULL array makes the call NULL.
                if let Some(inner) = is_variadic_marker(a) {
                    let v = eval_grouped(
                        q, outer, gscope, schema, rows, idxs, key_vals, group_by, inner,
                    )?;
                    match expand_variadic_value(v)? {
                        // v1.24: PG19 text_format() treats VARIADIC NULL
                        // as a zero-length array; every other variadic
                        // builtin makes the whole call NULL.
                        None if variadic_null_expands_empty(name) => {
                            continue;
                        }
                        None => return Ok(Value::Null),
                        Some(expanded) => {
                            for ev in expanded {
                                vals.push(normalize_func_arg(name, ev));
                            }
                            continue;
                        }
                    }
                }
                let v = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, a)?;
                vals.push(normalize_func_arg(name, v));
            }
            // v0.9: sequence functions need engine access; they cannot
            // go through the pure eval_func_vals path.
            // v0.98: lastval() also needs engine access (session state).
            if matches!(name.as_str(), "nextval" | "currval" | "setval" | "lastval") {
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
            // v0.91: hidden `__any_all_array` builtin (desugared from
            // `expr op ANY|ALL|SOME (array_expr)`); needs engine access
            // for user-defined operators. Build scopes like
            // pg_column_compression above.
            if name == "__any_all_array" {
                let mut scopes = Vec::with_capacity(outer.len() + 1);
                scopes.extend_from_slice(outer);
                scopes.push(gscope);
                return eval_any_all_array(q, &scopes, &vals);
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
            // v0.38: regclass/oid binary coercion (below). The grouped
            // scope chain is outer scopes plus the grouping scope.
            let gsc = Scope {
                schema: gscope.schema,
                row: gscope.row,
                prov: gscope.prov,
            };
            let mut chained: Vec<Scope> = outer.to_vec();
            chained.push(gsc);
            let (va, vb) = coerce_regclass_cmp(q, &chained, left, right, va, vb)?;
            // v0.57: name-vs-unknown-literal truncation (PG19 namein).
            let (va, vb) = coerce_name_cmp(&chained, left, right, va, vb);
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
        Expr::BitNot(x) => {
            let v = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, x)?;
            eval_bitnot_val(&v)
        }
        Expr::Neg(x) => {
            let v = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, x)?;
            eval_neg_val(&v)
        }
        // v0.55: CASE in the grouped path (e.g. aggregates inside arms).
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            let mut schemas: Vec<&[QCol]> = outer.iter().map(|s| s.schema).collect();
            schemas.push(gscope.schema);
            let ty = case_eval_type(q.eng, q.snap, q.own, q.session, &schemas, whens, else_)?;
            eval_case(operand, whens, else_, ty, |e| {
                eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, e)
            })
        }
        Expr::IsNull { expr: x, neg } => {
            let v = eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, x)?;
            Ok(Value::Bool((v == Value::Null) != *neg))
        }
        // v0.48: `IS [NOT] DISTINCT FROM` in the grouped path.
        Expr::IsDistinctFrom { left, right, neg } => {
            let l = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, left,
            )?;
            let r = eval_grouped(
                q, outer, gscope, schema, rows, idxs, key_vals, group_by, right,
            )?;
            eval_is_distinct_from(l, r, *neg)
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

pub(crate) fn eval_grouped_bool(
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
pub(crate) fn sum_vals(vals: &[Value]) -> Result<Value, ExecError> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    // v0.76: PG19 widening — sum(smallint/int) -> bigint,
    // sum(bigint) -> numeric. (v0.6's "widest input kind" rule is retired.)
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
    } else if cat == NumCat::Big {
        // v0.76: sum(bigint) -> numeric (PG19).
        let mut acc = Numeric::zero();
        for v in vals {
            let n = to_numeric_opt(v)
                .ok_or_else(|| exec_err("22003", "value out of range for numeric"))?;
            acc = acc
                .checked_add(&n)
                .ok_or_else(|| exec_err("22003", "numeric field overflow"))?;
        }
        Ok(Value::Numeric(acc))
    } else {
        // v0.76: sum(smallint/int) -> bigint (PG19).
        let mut acc: i128 = 0;
        for v in vals {
            acc = acc
                .checked_add(to_i128(v))
                .ok_or_else(|| exec_err("22003", "integer out of range"))?;
        }
        fit_int_result(NumCat::Big, acc)
    }
}

/// v0.10: `avg()` over non-null values (shared by grouped aggregates
/// and windowed aggregates).
pub(crate) fn avg_vals(vals: &[Value]) -> Result<Value, ExecError> {
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

/// v0.64: sample/population variance and stddev over non-null values,
/// following PG19's `numeric_stddev_internal` exactly. The transition
/// accumulates N, sumX, sumX2 plus NaN/±Infinity counts; the final
/// computes `N*sumX2 - sumX*sumX`, guards `<= 0` to exactly zero
/// (roundoff — with exact arithmetic this only hits constant inputs),
/// divides by `N*(N-1)` (sample) or `N*N` (population) at PG19's
/// `select_div_scale`, and takes the square root at the same rscale
/// for stddev. Sample aggregates return NULL when the total input
/// count (NaNs and infinities count, like PG) is <= 1; any NaN or
/// infinite input yields NaN.
pub(crate) fn variance_vals(
    vals: &[Value],
    name: &str,
    sample: bool,
    want_variance: bool,
) -> Result<Value, ExecError> {
    // PG has separate float8 aggregates; route any float input through
    // the f64 path (same formula, NaN/inf input -> NaN).
    if vals
        .iter()
        .any(|v| matches!(v, Value::Float(_) | Value::Float4(_)))
    {
        return variance_float_vals(vals, sample, want_variance);
    }
    let overflow = || exec_err("22003", "numeric field overflow");
    let mut n: i64 = 0;
    let mut total: i64 = 0;
    let mut nan_c: i64 = 0;
    let mut pinf_c: i64 = 0;
    let mut ninf_c: i64 = 0;
    let mut sum_x = BigDec::zero();
    let mut sum_x2 = BigDec::zero();
    // v0.64: PG19 keeps the transition sums as arbitrary-precision
    // intermediates — only the *final* result is subject to the numeric
    // digit limit. (The "sum of squares would overflow but variance
    // does not" regression vector squares 9e131071, whose 262144-digit
    // square must not 22003 here.) Accumulate on BigDec and track the
    // max input dscale for PG's rscale rules.
    let mut max_dscale: i32 = 0;
    for v in vals {
        let xv = to_numeric_opt(v).ok_or_else(|| {
            exec_err(
                "42883",
                format!("function {}({}) does not exist", name, v.type_name()),
            )
        })?;
        total += 1;
        match xv.special {
            NumericSpecial::NaN => nan_c += 1,
            NumericSpecial::PosInf => pinf_c += 1,
            NumericSpecial::NegInf => ninf_c += 1,
            NumericSpecial::Finite => {
                n += 1;
                max_dscale = max_dscale.max(xv.dscale);
                // PG squares at rscale X.dscale*2 — exact, since the
                // true square carries exactly twice the input's dscale.
                let x = BigDec::from_numeric(&xv).ok_or_else(overflow)?;
                let x2 = x.mul_exact(&x).ok_or_else(overflow)?;
                sum_x = sum_x.add(&x);
                sum_x2 = sum_x2.add(&x2);
            }
        }
    }
    if total == 0 {
        return Ok(Value::Null);
    }
    if sample && total <= 1 {
        return Ok(Value::Null);
    }
    if nan_c > 0 || pinf_c > 0 || ninf_c > 0 {
        return Ok(Value::Numeric(Numeric::nan()));
    }
    let n_bd = BigDec::from_i64(n);
    // Both products are exact (PG rounds them at sumX.dscale*2, which
    // the true products never exceed: sumX*sumX carries exactly
    // 2*sumX.dscale digits, N*sumX2 carries sumX2.dscale <= 2*max).
    let sum_x_sq = sum_x.mul_exact(&sum_x).ok_or_else(overflow)?;
    let n_sum_x2 = n_bd.mul_exact(&sum_x2).ok_or_else(overflow)?;
    let numer_bd = n_sum_x2.sub(&sum_x_sq);
    // PG's roundoff guard: with exact arithmetic the numerator is only
    // <= 0 for constant inputs, whose variance is exactly zero.
    if numer_bd.cmp(&BigDec::zero()) != Ordering::Greater {
        return Ok(Value::Numeric(Numeric::zero()));
    }
    // The numerator as a Numeric for PG19's select_div_scale (its dscale
    // is 2*max_dscale by PG's sub_var rule). from_bigdec enforces PG's
    // final-result digit limit, like make_result's weight check.
    let numer =
        Numeric::from_bigdec(&numer_bd, max_dscale.saturating_mul(2)).ok_or_else(overflow)?;
    let denom_n = if sample { n * (n - 1) } else { n * n };
    let denom = Numeric::from_i64(denom_n);
    let rscale = Numeric::div_scale_for(&numer, &denom);
    let mut res = numer.div_at_scale(&denom, rscale).ok_or_else(overflow)?;
    if !want_variance {
        // PG applies sqrt_var at the division's rscale.
        let b = BigDec::from_numeric(&res).ok_or_else(overflow)?;
        let s = b.sqrt_round(rscale).ok_or_else(overflow)?;
        res = Numeric::from_bigdec(&s, rscale).ok_or_else(overflow)?;
    }
    Ok(Value::Numeric(res))
}

/// v0.64: float8-style variance/stddev (PG's float8 aggregates) for
/// float4/float8 inputs: same N*sumX2-sumX^2 formula in f64, NaN or
/// infinite input -> NaN, sample N<=1 -> NULL.
pub(crate) fn variance_float_vals(
    vals: &[Value],
    sample: bool,
    want_variance: bool,
) -> Result<Value, ExecError> {
    let mut n = 0i64;
    let mut sum = 0.0f64;
    let mut sum2 = 0.0f64;
    let mut bad = false;
    for v in vals {
        let x = to_f64v(v);
        n += 1;
        if !x.is_finite() {
            bad = true;
            continue;
        }
        sum += x;
        sum2 += x * x;
    }
    if n == 0 || (sample && n <= 1) {
        return Ok(Value::Null);
    }
    if bad {
        return Ok(Value::Float(f64::NAN));
    }
    let nf = n as f64;
    let numer = nf * sum2 - sum * sum;
    if numer <= 0.0 {
        return Ok(Value::Float(0.0));
    }
    let denom = if sample { nf * (nf - 1.0) } else { nf * nf };
    let var = numer / denom;
    Ok(Value::Float(if want_variance { var } else { var.sqrt() }))
}

/// v0.64: `bool_and` — true iff every non-null input is true, NULL when
/// there are no non-null inputs (Postgres semantics).
pub(crate) fn bool_and_vals(vals: &[Value]) -> Result<Value, ExecError> {
    let mut any = false;
    for v in vals {
        match v {
            Value::Bool(b) => {
                any = true;
                if !b {
                    return Ok(Value::Bool(false));
                }
            }
            _ => {
                return Err(exec_err(
                    "42883",
                    format!("function bool_and({}) does not exist", v.type_name()),
                ));
            }
        }
    }
    Ok(if any { Value::Bool(true) } else { Value::Null })
}

// ---------------------------------------------------------------------------
// v1.30: PG19 ordered-set aggregates (WITHIN GROUP)
// ---------------------------------------------------------------------------

/// v1.30: is this `ColType` in PG's numeric category (implicitly
/// coercible to float8, like PG19's `select_common_type` unification
/// for WITHIN GROUP)?
pub(crate) fn within_group_numeric(t: &ColType) -> bool {
    matches!(
        t,
        ColType::SmallInt
            | ColType::Int
            | ColType::BigInt
            | ColType::Numeric(_)
            | ColType::Float4
            | ColType::Float
    )
}

/// v1.30: PG19 parse-time check for hypothetical-set aggregates —
/// each direct arg must unify with its sort column
/// (`select_common_type(..., "WITHIN GROUP")` → 42804 "WITHIN GROUP
/// types %s and %s cannot be matched"). Approximated: identical types
/// unify; numerics unify with numerics (PG's implicit int→float8
/// etc.); anything else must match exactly.
pub(crate) fn within_group_check_arg_types(
    q: &mut Q,
    schema: &[QCol],
    outer: &[Scope],
    direct_args: &[Expr],
    within_order_by: &[OrderTerm],
) -> Result<(), ExecError> {
    let schemas: &[&[QCol]] = std::slice::from_ref(&schema);
    let outer_schemas: Vec<&[QCol]> = outer.iter().map(|s| s.schema).collect();
    for (a, o) in direct_args.iter().zip(within_order_by.iter()) {
        // Unwrap named-notation args (eval_expr unwraps them too).
        let a = match a {
            Expr::NamedArg { expr, .. } => expr.as_ref(),
            other => other,
        };
        // PG19: an untyped NULL literal coerces to the sort column's
        // type (like the percentile fractions).
        if matches!(a, Expr::Literal(Literal::Null)) {
            continue;
        }
        let ta = expr_type(
            &mut *q.eng,
            q.snap,
            q.own,
            q.session,
            schemas,
            &outer_schemas,
            &[],
            a,
        )?;
        let ts = expr_type(
            &mut *q.eng,
            q.snap,
            q.own,
            q.session,
            schemas,
            &outer_schemas,
            &[],
            &o.expr,
        )?;
        if ta == ts || (within_group_numeric(&ta) && within_group_numeric(&ts)) {
            continue;
        }
        return Err(exec_err(
            "42804",
            format!(
                "WITHIN GROUP types {} and {} cannot be matched",
                ta.sql_name(),
                ts.sql_name()
            ),
        ));
    }
    Ok(())
}

/// v1.30: validate one percentile fraction (PG19
/// `orderedsetaggs.c`): NULL → None (a NULL fraction yields NULL);
/// out-of-[0,1] or NaN → 2201W "percentile value %g is not between 0
/// and 1"; a non-numeric fraction → 42883 (PG19 has no such
/// signature).
pub(crate) fn percentile_fraction_value(agg: &str, f: &Value) -> Result<Option<f64>, ExecError> {
    match f {
        Value::Null => Ok(None),
        Value::SmallInt(_)
        | Value::Int(_)
        | Value::BigInt(_)
        | Value::Numeric(_)
        | Value::Float4(_)
        | Value::Float(_) => {
            let p = to_f64v(f);
            if p < 0.0 || p > 1.0 || p.is_nan() {
                return Err(exec_err(
                    "2201W",
                    format!("percentile value {p} is not between 0 and 1"),
                ));
            }
            Ok(Some(p))
        }
        other => Err(exec_err(
            "42883",
            format!("function {}({}) does not exist", agg, other.type_name()),
        )),
    }
}

/// v1.30: is this Value in PG's numeric category (acceptable as a
/// `percentile_cont` sort input, matching `within_group_numeric`)?
pub(crate) fn within_group_numeric_value(v: &Value) -> bool {
    matches!(
        v,
        Value::SmallInt(_)
            | Value::Int(_)
            | Value::BigInt(_)
            | Value::Numeric(_)
            | Value::Float4(_)
            | Value::Float(_)
    )
}
/// v1.30: `percentile_cont` final step (PG19
/// `percentile_cont_final_common` with `float8_lerp`).
/// `direct` is the fraction (scalar) or fractions (array);
/// `vals` the sorted non-null sort values. PG19:
/// `first_row = floor(p*(N-1))`, `second_row = ceil(p*(N-1))`,
/// linear interpolation between them; NULL fraction → NULL; no rows
/// → NULL; NULL/empty fractions array → NULL / empty array (same
/// shape as the input); NULL elements → NULL elements.
pub(crate) fn percentile_cont_final(direct: &Value, vals: &[Value]) -> Result<Value, ExecError> {
    // No regular rows → NULL, even for the array form.
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    // PG19's float8 variant: a non-numeric sort value has no such
    // signature (42883). (The static check exempts Text because
    // untyped NULL literals type as Text; real text fails here.)
    if vals.iter().any(|v| !within_group_numeric_value(v)) {
        return Err(exec_err(
            "42883",
            "function percentile_cont(float8) does not exist".to_string(),
        ));
    }
    let n = vals.len() as f64;
    let one = |p: f64| -> Value {
        let x = p * (n - 1.0);
        let first = x.floor() as usize;
        let second = x.ceil() as usize;
        if first == second {
            return vals[first].clone();
        }
        // PG19 float8_lerp: lo + proportion * (hi - lo).
        let proportion = x - first as f64;
        let lo = to_f64v(&vals[first]);
        let hi = to_f64v(&vals[second]);
        Value::Float(lo + proportion * (hi - lo))
    };
    match direct {
        Value::Array(a) => {
            let mut elems = Vec::with_capacity(a.elems.len());
            for f in &a.elems {
                match percentile_fraction_value("percentile_cont", f)? {
                    None => elems.push(Value::Null),
                    Some(p) => elems.push(one(p)),
                }
            }
            // PG19: "We make the output array the same shape as the
            // input".
            Ok(Value::Array(Box::new(ArrayVal {
                elem: ArrayElem::Float,
                dims: a.dims.clone(),
                lower: a.lower.clone(),
                elems,
            })))
        }
        f => match percentile_fraction_value("percentile_cont", f)? {
            None => Ok(Value::Null),
            Some(p) => Ok(one(p)),
        },
    }
}

/// v1.30: `percentile_disc` final step (PG19
/// `percentile_disc_final`): `rownum = ceil(p*N)` (1-based); NULL
/// fraction → NULL; no rows → NULL. Array form mirrors
/// `percentile_cont`'s shape handling.
pub(crate) fn percentile_disc_final(
    direct: &Value,
    vals: &[Value],
    elem: ArrayElem,
) -> Result<Value, ExecError> {
    // No regular rows → NULL, even for the array form.
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    let n = vals.len() as f64;
    let one = |p: f64| -> Value {
        // Smallest K (1-based) with K/N >= p.
        let rownum = (p * n).ceil() as usize;
        vals[rownum.max(1) - 1].clone()
    };
    match direct {
        Value::Array(a) => {
            let mut elems = Vec::with_capacity(a.elems.len());
            for f in &a.elems {
                match percentile_fraction_value("percentile_disc", f)? {
                    None => elems.push(Value::Null),
                    Some(p) => elems.push(one(p)),
                }
            }
            Ok(Value::Array(Box::new(ArrayVal {
                elem,
                dims: a.dims.clone(),
                lower: a.lower.clone(),
                elems,
            })))
        }
        f => match percentile_fraction_value("percentile_disc", f)? {
            None => Ok(Value::Null),
            Some(p) => Ok(one(p)),
        },
    }
}

/// v1.30: `mode()` final step (PG19 `mode_final`): most frequent
/// non-null sort value; ties break to the first in sort order (the
/// first longest run of the stably-sorted input).
pub(crate) fn mode_final(vals: &[Value]) -> Result<Value, ExecError> {
    let mut best: Option<&Value> = None;
    let mut best_n = 0usize;
    let mut i = 0;
    while i < vals.len() {
        let mut n = 1;
        // Adjacent in sort order ⟺ equal under the sort's value
        // comparison (compare_values, desc/NULLS irrelevant for
        // equality).
        while i + n < vals.len()
            && compare_values(&vals[i + n], &vals[i], false, None)? == Ordering::Equal
        {
            n += 1;
        }
        // Strict `>`: the first longest run wins ties.
        if n > best_n {
            best_n = n;
            best = Some(&vals[i]);
        }
        i += n;
    }
    Ok(best.cloned().unwrap_or(Value::Null))
}

/// v1.30: PG19 `ExecQual` equality-operator semantics for
/// hypothetical `dense_rank` peer detection
/// (`execTuplesMatchPrepare` over the sort columns): two key tuples
/// are duplicates iff every column pair is non-null and canonically
/// equal — NULL never equals, not even NULL.
pub(crate) fn within_group_keys_dup(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        match (x, y) {
            (Value::Null, _) | (_, Value::Null) => return false,
            _ => {
                let mut ka = Vec::new();
                let mut kb = Vec::new();
                value_key(x, &mut ka);
                value_key(y, &mut kb);
                if ka != kb {
                    return false;
                }
            }
        }
    }
    true
}

/// v1.30: hypothetical-set aggregate final step. `keyed` is the
/// filter-passing input rows stably sorted by the WITHIN GROUP keys
/// (NULLs kept — PG19's `ordered_set_transition_multi` never skips
/// NULLs); `direct` holds the hypothetical row's keys. Follows PG19's
/// `hypothetical_rank_common` / `hypothetical_dense_rank_final`:
/// - `rank` = 1 + #(rows strictly less) (hypothetical sorts ahead
///   of peers, flag -1)
/// - `dense_rank` = 1 + #(distinct key groups strictly less)
/// - `percent_rank` = (rank-1)/rowcount (0.0 when there are no rows)
/// - `cume_dist` = (1 + #(strictly less) + #(peers))/(rowcount+1)
///   (hypothetical sorts behind peers, flag +1; 1.0 with no rows)
#[allow(clippy::cast_possible_wrap)]
pub(crate) fn hypothetical_final(
    func: OrderedSetAgg,
    direct: &[Value],
    keyed: &[(Vec<Value>, usize)],
    within_order_by: &[OrderTerm],
) -> Result<Value, ExecError> {
    let rowcount = keyed.len() as i64;
    let mut less = 0i64;
    let mut peers = 0i64;
    let mut groups_less = 0i64;
    let mut prev: Option<&Vec<Value>> = None;
    // The input is sorted, so every strictly-less row precedes every
    // peer-or-greater row; one pass classifies all three counts.
    for (keys, _) in keyed {
        let ord = compare_window_keys(keys, direct, within_order_by)?;
        match ord {
            Ordering::Less => {
                less += 1;
                let dup = prev.is_some_and(|p| within_group_keys_dup(p, keys));
                if !dup {
                    groups_less += 1;
                }
            }
            Ordering::Equal => {
                peers += 1;
            }
            Ordering::Greater => {}
        }
        prev = Some(keys);
    }
    match func {
        OrderedSetAgg::Rank => Ok(Value::BigInt(1 + less)),
        OrderedSetAgg::DenseRank => Ok(Value::BigInt(1 + groups_less)),
        OrderedSetAgg::PercentRank => {
            if rowcount == 0 {
                Ok(Value::Float(0.0))
            } else {
                Ok(Value::Float(less as f64 / rowcount as f64))
            }
        }
        OrderedSetAgg::CumeDist => Ok(Value::Float(
            (1 + less + peers) as f64 / (rowcount + 1) as f64,
        )),
        _ => unreachable!("plain ordered-set aggregates are handled by the caller"),
    }
}

/// v1.30: evaluate a PG19 ordered-set aggregate (WITHIN GROUP) for one
/// group. Row gathering, FILTER handling, and the in-aggregate sort
/// mirror `eval_agg_func`; the final step follows PG19
/// `src/backend/utils/adt/orderedsetaggs.c`:
/// - FILTER gates the input rows before the WITHIN GROUP sort (PG19
///   builds the ordered-set sort from filter-passing rows).
/// - Direct arguments are evaluated once per group, against the
///   group's representative (first) input row — PG19's nodeAgg.c
///   evaluates them against each group's first tuple.
/// - The WITHIN GROUP sort keys are evaluated per input row and the
///   rows are stably sorted with `compare_window_keys` (the terms'
///   ASC/DESC and NULLS FIRST/LAST, exactly like PG19's tuplesort).
/// - Plain ordered-set aggregates skip NULL sort-key inputs
///   (`ordered_set_transition`); hypothetical-set aggregates keep
///   them (`ordered_set_transition_multi` never skips NULLs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn eval_within_group_agg(
    q: &mut Q,
    outer: &[Scope],
    gscope: Scope,
    schema: &[QCol],
    rows: &[QRow],
    idxs: &[usize],
    func: OrderedSetAgg,
    direct_args: &[Expr],
    within_order_by: &[OrderTerm],
    filter: Option<&Expr>,
) -> Result<Value, ExecError> {
    // Per-row FILTER predicate in row scope (mirrors eval_agg_func's
    // filter_ok closure).
    let filter_ok = |q: &mut Q, i: usize| -> Result<bool, ExecError> {
        let f = match filter {
            Some(f) => f,
            None => return Ok(true),
        };
        let frame = Scope {
            schema,
            row: &rows[i].cells,
            prov: None,
        };
        let mut buf: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
        buf.extend_from_slice(outer);
        buf.push(frame);
        // PG19 coerce_to_boolean with constructName "FILTER": non-TRUE
        // (incl. NULL) skips the row; non-boolean is 42804.
        check_bool(eval_expr(q, &buf, f)?, "FILTER")
    };

    // PG19 parse-time check for hypothetical-set aggregates (42804
    // "WITHIN GROUP types %s and %s cannot be matched"): each direct
    // arg must unify with its sort column. Runs before any direct-arg
    // evaluation, like PG's parse-time check.
    if func.is_hypothetical() {
        within_group_check_arg_types(q, schema, outer, direct_args, within_order_by)?;
    }

    // Direct args are evaluated once per group, against the group's
    // representative row (PG19 nodeAgg.c: each group's first tuple).
    // With no input rows PG19 never evaluates them (the transition
    // never runs): skip, since the final steps return fixed results
    // without `direct` then (and resolving a column against the
    // empty representative row would panic).
    let mut direct: Vec<Value> = Vec::with_capacity(direct_args.len());
    if !idxs.is_empty() {
        let mut rep_scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
        rep_scopes.extend_from_slice(outer);
        rep_scopes.push(gscope);
        for a in direct_args {
            direct.push(eval_expr(q, &rep_scopes, a)?);
        }
    }

    // FILTER gates the input rows (PG19 builds the ordered-set sort
    // from filter-passing rows).
    let mut visit: Vec<usize> = Vec::with_capacity(idxs.len());
    for &i in idxs {
        if filter_ok(q, i)? {
            visit.push(i);
        }
    }

    // Evaluate the WITHIN GROUP sort keys per input row, then stably
    // sort (PG19 sorts the aggregate's input before accumulation;
    // ties keep input row order). The sort_err pattern mirrors
    // eval_agg_func's in-aggregate ORDER BY.
    let mut keyed: Vec<(Vec<Value>, usize)> = Vec::with_capacity(visit.len());
    for &i in &visit {
        let frame = Scope {
            schema,
            row: &rows[i].cells,
            prov: None,
        };
        let mut buf: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
        buf.extend_from_slice(outer);
        buf.push(frame);
        let mut keys = Vec::with_capacity(within_order_by.len());
        for o in within_order_by {
            keys.push(eval_expr(q, &buf, &o.expr)?);
        }
        keyed.push((keys, i));
    }
    let mut sort_err: Option<ExecError> = None;
    keyed.sort_by(|(ak, _), (bk, _)| {
        if sort_err.is_some() {
            return Ordering::Equal;
        }
        match compare_window_keys(ak, bk, within_order_by) {
            Ok(o) => o,
            Err(e) => {
                sort_err = Some(e);
                Ordering::Equal
            }
        }
    });
    if let Some(e) = sort_err {
        return Err(e);
    }

    if func.is_hypothetical() {
        return hypothetical_final(func, &direct, &keyed, within_order_by);
    }

    // Plain ordered-set aggregates: PG19 `ordered_set_transition`
    // skips NULL inputs. (All three take exactly one sort key; the
    // parser enforces the arity.)
    let vals: Vec<Value> = keyed
        .iter()
        .map(|(keys, _)| keys[0].clone())
        .filter(|v| !matches!(v, Value::Null))
        .collect();

    match func {
        OrderedSetAgg::PercentileCont => {
            // No rows → NULL without touching the fraction (PG19's
            // finalfn returns NULL on an empty tuplestore; `direct`
            // was never evaluated for an empty group).
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let d = direct
                .first()
                .expect("percentile_cont takes one direct arg");
            percentile_cont_final(d, &vals)
        }
        OrderedSetAgg::PercentileDisc => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let d = direct
                .first()
                .expect("percentile_disc takes one direct arg");
            // The array form returns the sort type's array (PG19
            // builds it from the sort column's type). Derive the
            // element from the sort expression's static type.
            let schemas: &[&[QCol]] = std::slice::from_ref(&schema);
            let outer_schemas: Vec<&[QCol]> = outer.iter().map(|s| s.schema).collect();
            let sort_t = expr_type(
                &mut *q.eng,
                q.snap,
                q.own,
                q.session,
                schemas,
                &outer_schemas,
                &[],
                &within_order_by[0].expr,
            )?;
            percentile_disc_final(d, &vals, ArrayElem::of(&sort_t))
        }
        OrderedSetAgg::Mode => mode_final(&vals),
        _ => unreachable!("hypothetical-set aggregates are handled above"),
    }
}

pub(crate) fn eval_agg_func(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: &[QRow],
    idxs: &[usize],
    func: AggFunc,
    arg: Option<&Expr>,
    distinct: bool,
    arg2: Option<&Expr>,
    // v0.92: ORDER BY inside the aggregate call (PG19 docs §4.2.7).
    order_by: &[OrderTerm],
    // v1.29: FILTER (WHERE ...) — only input rows where this is TRUE
    // feed the transition (PG19 nodeAgg.c folds aggfilter into the
    // transition expression). Evaluated per input row in row scope;
    // groups are still formed from all rows.
    filter: Option<&Expr>,
) -> Result<Value, ExecError> {
    // v1.29: per-row FILTER predicate in row scope (mirrors the scope
    // construction of the visit loop below).
    let filter_ok = |q: &mut Q, i: usize| -> Result<bool, ExecError> {
        let f = match filter {
            Some(f) => f,
            None => return Ok(true),
        };
        let frame = Scope {
            schema,
            row: &rows[i].cells,
            prov: None,
        };
        let scopes_storage: Vec<Scope>;
        let scopes: &[Scope] = if outer.is_empty() {
            std::slice::from_ref(&frame)
        } else {
            let mut buf: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            buf.extend_from_slice(outer);
            buf.push(frame);
            scopes_storage = buf;
            &scopes_storage
        };
        // PG19 coerce_to_boolean with constructName "FILTER": non-TRUE
        // (incl. NULL) skips the row; non-boolean is 42804.
        check_bool(eval_expr(q, scopes, f)?, "FILTER")
    };
    if func == AggFunc::Count && arg.is_none() {
        // v1.29: COUNT(*) with FILTER counts only the rows passing the
        // filter (PG19: the filter gates the transition input).
        if filter.is_some() {
            let mut n: i64 = 0;
            for &i in idxs {
                if filter_ok(q, i)? {
                    n += 1;
                }
            }
            return Ok(Value::BigInt(n));
        }
        return Ok(Value::BigInt(idxs.len() as i64));
    }
    let a = arg.expect("non-COUNT aggregates take an argument");
    // v1.29: FILTER applies at the aggregate's input — before the
    // in-aggregate ORDER BY sort (PG19 builds the sort from
    // filter-passing rows) and before DISTINCT dedup.
    let mut visit: Vec<usize> = idxs.to_vec();
    if filter.is_some() {
        let mut kept = Vec::with_capacity(visit.len());
        for &i in &visit {
            if filter_ok(q, i)? {
                kept.push(i);
            }
        }
        visit = kept;
    }
    // v0.92: ORDER BY inside the aggregate — evaluate the sort keys
    // per input row, then visit rows in sorted order (PG sorts the
    // aggregate's input before accumulation). Stable: ties keep input
    // row order.
    if !order_by.is_empty() {
        let mut keyed: Vec<(Vec<Value>, usize)> = Vec::with_capacity(idxs.len());
        for &i in &visit {
            let frame = Scope {
                schema,
                row: &rows[i].cells,
                prov: None,
            };
            let mut buf: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            buf.extend_from_slice(outer);
            buf.push(frame);
            let mut keys = Vec::with_capacity(order_by.len());
            for o in order_by {
                keys.push(eval_expr(q, &buf, &o.expr)?);
            }
            keyed.push((keys, i));
        }
        let mut sort_err: Option<ExecError> = None;
        keyed.sort_by(|(ak, _), (bk, _)| {
            if sort_err.is_some() {
                return Ordering::Equal;
            }
            match compare_window_keys(ak, bk, order_by) {
                Ok(o) => o,
                Err(e) => {
                    sort_err = Some(e);
                    Ordering::Equal
                }
            }
        });
        if let Some(e) = sort_err {
            return Err(e);
        }
        visit = keyed.into_iter().map(|(_, i)| i).collect();
    }
    // `visit.len() == idxs.len()` is an exact upper bound: the loop
    // below pushes at most one value per visited row (NULLs are
    // skipped, never adding to the count), so this avoids the 0->4->8->16
    // reallocation churn `Vec::new()` would otherwise pay per group.
    let mut vals: Vec<Value> = Vec::with_capacity(visit.len());
    // string_agg evaluates (value, delimiter) per row; the delimiter
    // may be NULL per-row (then it defaults to "") while a NULL value
    // still skips the row, like Postgres.
    let mut delims: Vec<Value> = Vec::new();
    for &i in &visit {
        let frame = Scope {
            schema,
            row: &rows[i].cells,
            prov: None,
        };
        let scopes_storage: Vec<Scope>;
        let scopes: &[Scope] = if outer.is_empty() {
            std::slice::from_ref(&frame)
        } else {
            let mut buf: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            buf.extend_from_slice(outer);
            buf.push(frame);
            scopes_storage = buf;
            &scopes_storage
        };
        // Aggregate arguments cannot nest aggregates (the parser allows
        // the syntax; fail like Postgres rather than recursing forever).
        let v = eval_expr(q, scopes, a)?;
        if func == AggFunc::StringAgg {
            let d = eval_expr(q, scopes, arg2.expect("string_agg takes a delimiter"))?;
            // v0.73: whole-row null test (see the general filter below).
            if crate::storage::value_is_null(&v) {
                continue;
            }
            vals.push(v);
            delims.push(d);
        // v0.73: a whole-row value counts as null iff every field is
        // null (PG19: `count(t.*)` skips null-extended outer-join rows).
        // v0.92: array_agg keeps NULL inputs — PG19 "Collects all the
        // input values, including nulls, into an array".
        } else if func == AggFunc::ArrayAgg || !crate::storage::value_is_null(&v) {
            vals.push(v);
        }
    }
    // DISTINCT: dedupe on the canonical grouping key. NULLs were
    // removed above for every aggregate except array_agg (which keeps
    // them per PG19); the NULL key is deterministic, so multiple NULLs
    // still collapse to one — Postgres' "DISTINCT treats NULLs as
    // equal". v0.92: Postgres deduplicates DISTINCT aggregate input
    // ROWS, so for string_agg the key is (value, delimiter), not the
    // value alone.
    if distinct {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut kept: Vec<usize> = Vec::new();
        for (i, v) in vals.iter().enumerate() {
            let mut k = Vec::new();
            value_key(v, &mut k);
            if func == AggFunc::StringAgg {
                if let Some(d) = delims.get(i) {
                    value_key(d, &mut k);
                }
            }
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
        AggFunc::BoolAnd => bool_and_vals(&vals),
        AggFunc::VarianceSamp => variance_vals(&vals, "variance", true, true),
        AggFunc::VariancePop => variance_vals(&vals, "var_pop", false, true),
        AggFunc::StddevSamp => variance_vals(&vals, "stddev", true, false),
        AggFunc::StddevPop => variance_vals(&vals, "stddev_pop", false, false),
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
                    // v0.35: PG coerces bpchar string_agg inputs to text.
                    Value::BpChar(t) => crate::storage::rtrim_spaces(t).into(),
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
            Ok(Value::text(out))
        }
        // v0.92: `array_agg(x)` — PG19 semantics live in
        // array_agg_final (NULLs kept for scalar input; 22004/2202E
        // errors for bad array inputs); zero input rows -> NULL.
        // `array_ctor_from_vals` implements PG's common-type
        // resolution and per-value coercion for ARRAY[...] literals.
        // The static argument type picks the overload (PG resolves
        // array_agg(anyarray) statically); the query schema lets
        // column references resolve to array types.
        AggFunc::ArrayAgg => array_agg_final(vals, expr_is_statically_array(a, Some(schema))),
    }
}
