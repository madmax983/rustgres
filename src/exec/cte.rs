// v1.78 mechanical split: moved verbatim from src/exec.rs (25974-27423).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// v0.10: Common Table Expressions
// ---------------------------------------------------------------------------

/// Materialize every CTE in definition order, pushing one binding each.
/// CTE bodies are uncorrelated (like Postgres): they see sibling CTEs
/// defined earlier, never the outer query's scopes.
pub(crate) fn materialize_ctes(q: &mut Q, ctes: &[CteDef]) -> Result<(), ExecError> {
    for cte in ctes {
        let binding = eval_cte(q, cte)?;
        q.ctes.push(Rc::new(binding));
    }
    Ok(())
}

pub(crate) fn eval_cte(q: &mut Q, cte: &CteDef) -> Result<CteBinding, ExecError> {
    match &cte.body {
        CteBody::Simple(sel) => {
            let out = run_select(q, sel, &[])?;
            cte_binding(cte, out.columns, out.rows)
        }
        CteBody::Union { left, right, all } => eval_recursive_cte(q, cte, left, right, *all),
        // v1.39: data-modifying CTE body — run the DML through the
        // shared nested-DML helper (v1.33 `run_func_dml` pattern); its
        // RETURNING rows become the CTE's rows. A sibling-CTE reference
        // inside the DML body fails closed (42P01) via the existing
        // name resolution, which only sees the outer query's bindings.
        CteBody::Dml(stmt) => {
            let out = run_nested_dml(q, stmt, &format!("CTE \"{}\"", cte.name))?;
            cte_binding(cte, out.columns, out.rows)
        }
    }
}

pub(crate) fn cte_binding(
    cte: &CteDef,
    columns: Vec<(String, ColType)>,
    rows: Vec<Row>,
) -> Result<CteBinding, ExecError> {
    check_col_alias_arity(&cte.name, columns.len(), &cte.col_aliases)?;
    let schema: Vec<QCol> = columns
        .into_iter()
        .enumerate()
        .map(|(i, (name, ty))| QCol {
            qual: cte.name.clone(),
            name: cte.col_aliases.get(i).cloned().unwrap_or(name),
            ty,

            hidden: false,
            src_ord: 0,
        })
        .collect();
    let rows = rows
        .into_iter()
        .map(|cells| QRow {
            cells,
            prov: Vec::new(),
        })
        .collect();
    Ok(CteBinding {
        name: cte.name.clone(),
        schema,
        rows,
    })
}

/// v0.10: `WITH RECURSIVE`: iterative fixpoint. The seed (non-recursive
/// term) is evaluated once; then the recursive term is re-evaluated with
/// the CTE name bound to the previous iteration's *new* rows until an
/// iteration produces nothing new. UNION (distinct) deduplicates across
/// iterations, so cyclic graphs terminate; UNION ALL keeps duplicates.
/// A safety cap aborts runaway UNION ALL recursion (Postgres would loop
/// forever).
pub(crate) fn eval_recursive_cte(
    q: &mut Q,
    cte: &CteDef,
    left: &SelectStmt,
    right: &SelectStmt,
    all: bool,
) -> Result<CteBinding, ExecError> {
    let seed = run_select(q, left, &[])?;
    let width = seed.columns.len();
    let mut binding = cte_binding(cte, seed.columns, seed.rows)?;
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
                for v in r.cells.iter() {
                    value_key(v, &mut k);
                }
                seen.insert(k);
            }
        }
        for cells in other.rows {
            let cells = cells.into_cells();
            let mut coerced = Vec::with_capacity(cells.len());
            for (v, ty) in cells.into_iter().zip(seed_types.iter()) {
                coerced.push(coerce_value(v, ty, &cte.name)?);
            }
            if all {
                binding.rows.push(QRow {
                    cells: Row::new(coerced),
                    prov: Vec::new(),
                });
            } else {
                let mut k = Vec::new();
                for v in &coerced {
                    value_key(v, &mut k);
                }
                if seen.insert(k) {
                    binding.rows.push(QRow {
                        cells: Row::new(coerced),
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
            for v in r.cells.iter() {
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
            let cells = cells.into_cells();
            let mut coerced = Vec::with_capacity(cells.len());
            for (v, ty) in cells.into_iter().zip(seed_types.iter()) {
                coerced.push(coerce_value(v, ty, &cte.name)?);
            }
            let row = QRow {
                cells: Row::new(coerced),
                prov: Vec::new(),
            };
            if all {
                new_rows.push(row);
            } else {
                let mut k = Vec::new();
                for v in row.cells.iter() {
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
pub(crate) fn validate_ctes(ctes: &[CteDef]) -> Result<(), ExecError> {
    for cte in ctes {
        if !cte.recursive {
            continue;
        }
        let (left, right) = match &cte.body {
            CteBody::Union { left, right, .. } => (left.as_ref(), right.as_ref()),
            // v0.10: Postgres allows a non-recursive body in WITH
            // RECURSIVE; skip the recursion checks.
            CteBody::Simple(_) => continue,
            // v1.39: recursive + DML is rejected at parse time (42P19),
            // so a DML body here is always non-recursive; skip.
            CteBody::Dml(_) => continue,
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
pub(crate) fn stmt_refs_table(sel: &SelectStmt, name: &str) -> bool {
    sel.from.iter().any(|f| from_refs_table(f, name))
        // v0.77: the recursive reference may hide inside an inner WITH
        // (e.g. `(WITH z AS NOT MATERIALIZED (SELECT * FROM x) ...)` as
        // the recursive term of `x`).
        || sel.with.iter().any(|c| cte_body_refs_table(&c.body, name))
}

pub(crate) fn cte_body_refs_table(body: &CteBody, name: &str) -> bool {
    match body {
        CteBody::Simple(s) => stmt_refs_table(s, name),
        CteBody::Union { left, right, .. } => {
            stmt_refs_table(left, name) || stmt_refs_table(right, name)
        }
        // v1.39: only used by the recursive-CTE shape checks, which a
        // DML body can never reach (parse-time 42P19).
        CteBody::Dml(_) => false,
    }
}

pub(crate) fn from_refs_table(f: &FromItem, name: &str) -> bool {
    match f {
        FromItem::Table { name: n, .. } => n == name,
        FromItem::Derived { sub, .. } => stmt_refs_table(sub, name),
        FromItem::Values { .. } => false,
        // v0.32: table functions reference no tables (args are
        // uncorrelated).
        FromItem::Function { .. } => false,
        FromItem::Join { left, right, .. } => {
            from_refs_table(left, name) || from_refs_table(right, name)
        }
    }
}

/// v1.14: collect the base table names reachable from FROM items (for
/// `FOR UPDATE OF` lock targeting). Derived subqueries are expanded
/// recursively; joins contribute their inner tables.
pub(crate) fn collect_base_tables(from: &[FromItem], out: &mut Vec<String>) {
    for item in from {
        match item {
            FromItem::Table { name, .. } => {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
            FromItem::Derived { sub, .. } => collect_base_tables(&sub.from, out),
            FromItem::Join { left, right, .. } => {
                collect_base_tables(std::slice::from_ref(left), out);
                collect_base_tables(std::slice::from_ref(right), out);
            }
            FromItem::Values { .. } | FromItem::Function { .. } => {}
        }
    }
}

pub(crate) fn run_select_inner(
    q: &mut Q,
    stmt: &SelectStmt,
    outer: &[Scope],
) -> Result<SelectOut, ExecError> {
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
    // v0.73: PG19 correlated references — the enclosing scopes' schemas
    // let a subquery's output describe resolve outer range/column refs.
    let outer_schemas: Vec<&[QCol]> = outer.iter().map(|s| s.schema).collect();
    let out_cols = describe_select(
        &*q.eng,
        q.snap,
        q.own,
        q.session,
        stmt,
        &q.ctes,
        &outer_schemas,
    )?;
    // v0.8: when the whole query is a plain single-table SELECT whose
    // ORDER BY matches an index, rows stream out of the index in ORDER BY
    // order and the sort step below is skipped.
    let order_hint = plan_order_scan(&*q.eng, q.snap, q.own, q.session, stmt);
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
    // v0.95: degenerate grouping (HAVING without GROUP BY or
    // aggregates) does not evaluate FROM/WHERE (PG19). Skip the WHERE
    // clause; the scan still produces rows for the single group.
    let degenerate = is_degenerate_grouping(stmt);
    let where_clause = if degenerate {
        None
    } else {
        stmt.where_.as_ref()
    };
    let (schema, rows) = build_from(
        q,
        outer,
        &stmt.from,
        where_clause,
        // v0.37: pg_column_compression needs row provenance too (anywhere
        // in the statement, including correlated subqueries).
        // v1.13: tableoid also needs per-row provenance (the OID varies
        // by partition leaf).
        // v1.17: xmin/xmax need per-row provenance (the MVCC header
        // varies by row version).
        stmt.for_update
            || stmt_uses_pg_column_compression(stmt)
            || stmt_uses_tableoid(stmt)
            || stmt_uses_xmin_xmax(stmt),
        order_hint.as_ref(),
        early_limit,
    )?;
    let rows = apply_where(q, outer, &schema, rows, where_clause)?;
    // v0.52: `SELECT DISTINCT ON` — PG19 parse analysis
    // (`transformDistinctOnClause`): check the ORDER BY prefix rule
    // (42803) and rewrite ORDER BY to the effective sort order (matching
    // key terms in ORDER BY order, then implicit ASC keys for any
    // unmatched DISTINCT ON expressions, then the non-key tail). PG always
    // sorts by the distinct keys, even with no user ORDER BY. Returns the
    // rewritten statement plus each DISTINCT ON expression's index in the
    // effective order (used by the aggregate path to derive group keys
    // from exec_agg's precomputed sort keys).
    let owned_distinct: SelectStmt;
    let distinct_pos: Option<Vec<usize>>;
    let stmt: &SelectStmt = if stmt.distinct_on.is_empty() {
        distinct_pos = None;
        stmt
    } else {
        let (effective, key_pos) = check_distinct_on_order(stmt, Some(&schema))?;
        let mut s = stmt.clone();
        s.order_by = effective;
        owned_distinct = s;
        distinct_pos = Some(key_pos);
        &owned_distinct
    };
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
    // v0.52: DISTINCT ON always keeps it — the group keys are evaluated
    // from the pre-projection row after sorting, and a deferred
    // SRF-in-targetlist expansion needs the input row back.
    let has_distinct_on = !stmt.distinct_on.is_empty();
    let keep_full = !agg && (has_distinct_on || (!stmt.distinct && !stmt.order_by.is_empty()));
    // v0.32: SRF-in-targetlist expansion (PG 19). A top-level
    // set-returning function call in the SELECT list fans each input
    // row out to one row per returned element. Only on the plain
    // projection path: aggregates and windowed queries keep the
    // scalar (first-row-or-NULL) reading. v0.52: with DISTINCT ON the
    // expansion is deferred until after the first-row-per-group filter
    // (PG's ProjectSet sits above Unique).
    // v1.28: SRF calls nested anywhere in the target list also fan out
    // (PG19's ProjectSet; the planner lifts them via
    // `split_pathtarget_at_srfs`), not just whole top-level SRF items.
    let expand_srf = windows.is_empty()
        && stmt.items.iter().any(|it| {
            matches!(
                it,
                SelectItem::Expr {
                    expr: Expr::Func { name, .. },
                    ..
                } if is_srf(q.eng, name)
            ) || item_has_nested_srf(q.eng, it)
        });
    // v1.28: whether the general nested-SRF path applies (computed once;
    // `project_row_expanded` must not re-walk the tree per row).
    let nested_srf = stmt.items.iter().any(|it| item_has_nested_srf(q.eng, it));
    let mut orows: Vec<OutRow> = if agg {
        exec_agg(q, outer, stmt, &schema, &rows, &out_cols, &windows)?
    } else {
        let mut v = Vec::with_capacity(rows.len());
        for (ri, r) in rows.into_iter().enumerate() {
            let full = keep_full.then(|| r.cells.clone());
            // v0.10: point the window context at this input row before
            // projecting (windows were precomputed above).
            if let Some(wctx) = q.wctx.as_mut() {
                wctx.row = ri;
            }
            if expand_srf && !has_distinct_on {
                for (cells, prov, srf) in
                    project_row_expanded(q, outer, stmt, &schema, r, out_cols.len(), nested_srf)?
                {
                    v.push(OutRow {
                        cells,
                        prov,
                        full: full.clone(),
                        sort_keys: None,
                        win_idx: None,
                        srf,
                    });
                }
                continue;
            }
            // project_row takes the row by value: plain `SELECT *` moves
            // it through with zero copies, and provenance moves rather
            // than cloning.
            let (cells, prov) =
                project_row(q, outer, stmt, &schema, r, out_cols.len(), has_distinct_on)?;
            v.push(OutRow {
                cells,
                prov,
                full,
                sort_keys: None,
                // v0.10: ORDER BY terms with windows need the input
                // row index.
                win_idx: if windows.is_empty() { None } else { Some(ri) },
                srf: Vec::new(),
            });
        }
        v
    };
    if stmt.distinct {
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        orows.retain(|o| {
            let mut k = Vec::new();
            for v in o.cells.iter() {
                value_key(v, &mut k);
            }
            seen.insert(k)
        });
    }
    if has_distinct_on {
        // v0.52: `SELECT DISTINCT ON`. PG19 always sorts by the effective
        // ORDER BY (distinct keys ++ tail — the rewrite above guarantees
        // the keys come first, so equal keys are adjacent) and keeps the
        // first row per group of equal distinct expressions (NULLs group
        // together). plan_order_scan bails on DISTINCT ON, so the
        // index-order path cannot skip this sort. On the aggregate path
        // the group keys come from exec_agg's precomputed sort keys.
        apply_order(q, outer, stmt, &schema, &out_cols, &mut orows)?;
        // Aggregate rows have no pre-projection row; their group keys
        // come from exec_agg's precomputed sort keys. Plain rows keep the
        // full row and evaluate the key expressions from it.
        let key_pos = if agg { distinct_pos.as_deref() } else { None };
        distinct_on_filter(q, outer, stmt, &schema, &mut orows, key_pos)?;
        if expand_srf && !agg {
            let mut expanded = Vec::with_capacity(orows.len());
            for o in orows {
                let row = QRow {
                    cells: o.full.expect("DISTINCT ON keeps full pre-projection rows"),
                    prov: o.prov,
                };
                for (cells, prov, srf) in
                    project_row_expanded(q, outer, stmt, &schema, row, out_cols.len(), nested_srf)?
                {
                    expanded.push(OutRow {
                        cells,
                        prov,
                        full: None,
                        sort_keys: None,
                        win_idx: None,
                        srf,
                    });
                }
            }
            orows = expanded;
        }
    } else if !stmt.order_by.is_empty() && order_hint.is_none() {
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
        // v1.14: `FOR UPDATE OF tbl [, ...]` locks only the listed tables'
        // rows; bare `FOR UPDATE` locks all. OF names are resolved to base
        // table names (subquery aliases expand to their inner tables).
        let targets: Option<Vec<String>> = if stmt.for_update_of.is_empty() {
            None
        } else {
            let mut tables = Vec::new();
            for name in &stmt.for_update_of {
                let mut stack: Vec<&FromItem> = stmt.from.iter().collect();
                while let Some(item) = stack.pop() {
                    match item {
                        FromItem::Table {
                            name: tname, alias, ..
                        } if alias.as_deref().unwrap_or(tname) == name => {
                            if !tables.contains(tname) {
                                tables.push(tname.clone());
                            }
                        }
                        FromItem::Derived { alias, sub, .. } if alias == name => {
                            collect_base_tables(&sub.from, &mut tables);
                        }
                        FromItem::Join {
                            left,
                            right,
                            kind,
                            alias,
                            using_alias,
                            ..
                        } if alias.is_none()
                            && using_alias.is_none()
                            && *kind == JoinKind::Cross =>
                        {
                            stack.push(left);
                            stack.push(right);
                        }
                        _ => {}
                    }
                }
            }
            Some(tables)
        };
        for o in &orows {
            for e in &o.prov {
                let t = &e.table;
                let id = e.row_id;
                if let Some(ref allowed) = targets {
                    if !allowed.iter().any(|a| a == t) {
                        continue;
                    }
                }
                if !q.lock_ids.iter().any(|(_, x)| *x == id) {
                    q.lock_ids.push((t.clone(), id));
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
pub(crate) fn validate_select(stmt: &SelectStmt) -> Result<(), ExecError> {
    // v0.10: CTE shape rules (recursive UNION discipline).
    validate_ctes(&stmt.with)?;
    // v0.44: validate every set-operation branch.
    if let Some(root) = &stmt.set_op {
        validate_select(&root.left)?;
        for b in &root.chain {
            validate_select(&b.right)?;
        }
        return Ok(());
    }
    // v0.10: window-function placement rules.
    validate_windows(stmt)?;
    if let Some(w) = &stmt.where_ {
        // v0.80: PG19 reports misplaced grouping operations with its own
        // 42803 text, not the aggregate one.
        if contains_grouping(w) {
            return Err(exec_err(
                "42803",
                "grouping operations are not allowed in WHERE",
            ));
        }
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
    for g in stmt.group_by.iter().flatten() {
        // v0.80: PG19's own 42803 text for misplaced grouping operations.
        if contains_grouping(g) {
            return Err(exec_err(
                "42803",
                "grouping operations are not allowed in GROUP BY",
            ));
        }
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
            // v0.80: `GROUPING(...)` nested inside an aggregate or window
            // call is rejected at analysis, like PG19 (42803).
            if let Some(what) = grouping_misplaced(expr) {
                return Err(exec_err(
                    "42803",
                    format!("grouping operations are not allowed in {what}"),
                ));
            }
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
        // v0.80: `GROUPING(...)` nested inside an aggregate or window
        // call is rejected at analysis, like PG19 (42803).
        if let Some(what) = grouping_misplaced(h) {
            return Err(exec_err(
                "42803",
                format!("grouping operations are not allowed in {what}"),
            ));
        }
    }
    for o in &stmt.order_by {
        validate_expr(&o.expr)?;
        // v0.80: `GROUPING(...)` nested inside an aggregate or window
        // call is rejected at analysis, like PG19 (42803).
        if let Some(what) = grouping_misplaced(&o.expr) {
            return Err(exec_err(
                "42803",
                format!("grouping operations are not allowed in {what}"),
            ));
        }
    }
    // v0.52: `SELECT DISTINCT ON` shape rules. Aggregates and window
    // functions are legal inside DISTINCT ON expressions (PG19 allows
    // `DISTINCT ON (count(*))` and `DISTINCT ON (rank() OVER (...))`; the
    // expressions are evaluated at group level / from window values like
    // any other). The ORDER BY prefix rule (42803) is enforced by
    // `check_distinct_on_order` in run_select_inner, which needs the FROM
    // schema to resolve ordinals over `*` — validate_select has no schema.
    if !stmt.distinct_on.is_empty() {
        for e in &stmt.distinct_on {
            validate_expr(e)?;
        }
    }
    if stmt.for_update {
        if stmt.distinct || !stmt.distinct_on.is_empty() {
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
        // v1.14: `FOR UPDATE OF tbl [, ...]` — each name must resolve to
        // a top-level FROM item; joins cannot be locked (PG19). A comma-
        // separated FROM list parses as CROSS JOINs, so we recurse into
        // unaliased CROSS joins to find plain tables.
        for name in &stmt.for_update_of {
            let mut found = false;
            let mut is_join = false;
            let mut stack: Vec<&FromItem> = stmt.from.iter().collect();
            while let Some(item) = stack.pop() {
                match item {
                    FromItem::Table {
                        name: tname, alias, ..
                    } => {
                        if alias.as_deref().unwrap_or(tname) == name {
                            found = true;
                            break;
                        }
                    }
                    FromItem::Derived { alias, .. } => {
                        if alias == name {
                            found = true;
                            break;
                        }
                    }
                    FromItem::Values { alias, .. } => {
                        if alias == name {
                            found = true;
                            break;
                        }
                    }
                    FromItem::Function { alias, .. } => {
                        if alias.as_deref() == Some(name.as_str()) {
                            found = true;
                            break;
                        }
                    }
                    FromItem::Join {
                        left,
                        right,
                        kind,
                        using_alias,
                        alias,
                        ..
                    } => {
                        if using_alias.as_deref() == Some(name.as_str())
                            || alias.as_deref() == Some(name.as_str())
                        {
                            found = true;
                            is_join = true;
                            break;
                        }
                        // Recurse into unaliased CROSS JOINs (comma-separated
                        // FROM items); an explicitly aliased join is opaque.
                        if alias.is_none() && using_alias.is_none() && *kind == JoinKind::Cross {
                            stack.push(left);
                            stack.push(right);
                        }
                    }
                }
            }
            if !found {
                return Err(exec_err(
                    "42P01",
                    format!(
                        "relation \"{}\" in FOR UPDATE clause not found in FROM clause",
                        name
                    ),
                ));
            }
            if is_join {
                return Err(exec_err("0A000", "FOR UPDATE cannot be applied to a join"));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_from(f: &FromItem) -> Result<(), ExecError> {
    match f {
        FromItem::Table { .. } => Ok(()),
        FromItem::Derived { sub, .. } => validate_select(sub),
        // v0.32: validate table-function args.
        FromItem::Function { args, .. } => {
            for a in args {
                validate_expr(a)?;
            }
            Ok(())
        }
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
                // v0.80: PG19's own 42803 text for misplaced grouping operations.
                if contains_grouping(p) {
                    return Err(exec_err(
                        "42803",
                        "grouping operations are not allowed in JOIN conditions",
                    ));
                }
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
pub(crate) fn validate_expr(e: &Expr) -> Result<(), ExecError> {
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
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => {
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
        Expr::Not(x)
        | Expr::BitNot(x)
        | Expr::Neg(x)
        | Expr::IsNull { expr: x, .. }
        | Expr::IsBool { expr: x, .. } => validate_expr(x),
        // v0.55: validate every CASE arm.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                validate_expr(o)?;
            }
            for (k, r) in whens {
                validate_expr(k)?;
                validate_expr(r)?;
            }
            if let Some(e) = else_ {
                validate_expr(e)?;
            }
            Ok(())
        }
        Expr::IsDistinctFrom { left, right, .. } => {
            validate_expr(left)?;
            validate_expr(right)
        }
        Expr::Cast { expr, .. } => validate_expr(expr),
        Expr::Func { args, .. } => {
            for a in args {
                validate_expr(a)?;
            }
            Ok(())
        }
        Expr::Extract { from, .. } => validate_expr(from),
        Expr::Agg {
            arg, arg2, filter, ..
        } => {
            if let Some(a) = arg {
                validate_expr(a)?;
            }
            if let Some(a) = arg2 {
                validate_expr(a)?;
            }
            // v1.29: FILTER is a per-row input like the arguments.
            if let Some(f) = filter {
                validate_expr(f)?;
            }
            Ok(())
        }
        // v1.30: ordered-set aggregate — direct args, WITHIN GROUP
        // sort keys, and FILTER are all per-row inputs.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            for a in direct_args {
                validate_expr(a)?;
            }
            for o in within_order_by {
                validate_expr(&o.expr)?;
            }
            if let Some(f) = filter {
                validate_expr(f)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Does this expression contain an aggregate at *this* query level?
/// Subqueries are their own level and are not descended into.
pub(crate) fn contains_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg { .. } | Expr::WithinGroup { .. } => true,
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => contains_agg(expr),
        Expr::Column { .. }
        | Expr::ResolvedCol { .. }
        | Expr::WholeRow { .. }
        | Expr::Literal(_)
        | Expr::Param(_) => false,
        Expr::Arith { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Concat(left, right) => contains_agg(left) || contains_agg(right),
        // v0.79: array operands can hide aggregates — recurse.
        Expr::ArrayCtor { elems, .. } => elems.iter().any(contains_agg),
        // v0.81: composite operands can hide aggregates — recurse.
        Expr::Row(elems) => elems.iter().any(contains_agg),
        Expr::CastNamed { expr, .. } | Expr::FieldAccess { expr, .. } => contains_agg(expr),
        Expr::Subscript { array, indices } => {
            contains_agg(array) || indices.iter().any(contains_agg)
        }
        Expr::Slice { array, bounds } => {
            contains_agg(array)
                || bounds.iter().any(|(l, u)| {
                    l.as_deref().map(contains_agg).unwrap_or(false)
                        || u.as_deref().map(contains_agg).unwrap_or(false)
                })
        }
        Expr::Cmp { left, right, .. } => contains_agg(left) || contains_agg(right),
        Expr::Like { expr, pattern, .. } => contains_agg(expr) || contains_agg(pattern),
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => contains_agg(expr) || contains_agg(pattern),
        Expr::Between {
            expr, low, high, ..
        } => contains_agg(expr) || contains_agg(low) || contains_agg(high),
        Expr::Not(x)
        | Expr::BitNot(x)
        | Expr::Neg(x)
        | Expr::IsNull { expr: x, .. }
        | Expr::IsBool { expr: x, .. } => contains_agg(x),
        // v0.55: an aggregate anywhere in a CASE (operand, WHEN key,
        // result, or ELSE) is an aggregate at this level.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_deref().is_some_and(contains_agg)
                || whens
                    .iter()
                    .any(|(k, r)| contains_agg(k) || contains_agg(r))
                || else_.as_deref().is_some_and(contains_agg)
        }
        Expr::IsDistinctFrom { left, right, .. } => contains_agg(left) || contains_agg(right),
        Expr::Cast { expr, .. } => contains_agg(expr),
        // v0.80: `GROUPING(...)` behaves like an aggregate for placement
        // and dispatch (PG19's transformGroupingFunc sets p_hasAggs).
        Expr::Func { name, args } => name == "grouping" || args.iter().any(contains_agg),
        Expr::Extract { from, .. } => contains_agg(from),
        Expr::InSub { expr, .. } => contains_agg(expr),
        // v0.87: quantified comparison (aggregate in `left` counts; the
        // subquery is its own level) and user operator.
        Expr::Quantified { left, .. } => contains_agg(left),
        Expr::UserOp { left, right, .. } => contains_agg(left) || contains_agg(right),
        // ScalarSub / ArraySubquery / Exists are separate query levels.
        Expr::ScalarSub(_) | Expr::ArraySubquery(_) | Expr::Exists { .. } => false,
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

/// v0.80: pre-order traversal of an expression tree, visiting every node
/// once. Subquery bodies (`ScalarSub`, the `sub` of `InSub`, `Exists`,
/// `ArraySubquery`) are separate query levels and are never descended
/// into — the same level discipline as `contains_agg`.
pub(crate) fn expr_walk<'a>(e: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
    let mut stack: Vec<&'a Expr> = vec![e];
    while let Some(x) = stack.pop() {
        visit(x);
        match x {
            Expr::Arith { left, right, .. }
            | Expr::Cmp { left, right, .. }
            | Expr::IsDistinctFrom { left, right, .. } => {
                stack.push(left);
                stack.push(right);
            }
            Expr::And(left, right) | Expr::Or(left, right) | Expr::Concat(left, right) => {
                stack.push(left);
                stack.push(right);
            }
            // v0.81: composite expressions — visit sub-expressions.
            Expr::CastNamed { expr, .. } | Expr::FieldAccess { expr, .. } => {
                stack.push(expr);
            }
            Expr::Row(elems) => {
                stack.extend(elems.iter());
            }
            Expr::Not(x) | Expr::BitNot(x) | Expr::Neg(x) => stack.push(x),
            Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => stack.push(x),
            Expr::Cast { expr: x, .. } | Expr::Extract { from: x, .. } => stack.push(x),
            Expr::Like { expr, pattern, .. } | Expr::Regex { expr, pattern, .. } => {
                stack.push(expr);
                stack.push(pattern);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                stack.push(expr);
                stack.push(low);
                stack.push(high);
            }
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                if let Some(o) = operand {
                    stack.push(o);
                }
                for (k, v) in whens {
                    stack.push(k);
                    stack.push(v);
                }
                if let Some(el) = else_ {
                    stack.push(el);
                }
            }
            Expr::Func { args, .. } => {
                for a in args {
                    stack.push(a);
                }
            }
            // v0.95: named args are transparent to inspection.
            Expr::NamedArg { expr, .. } => {
                stack.push(expr);
            }
            Expr::Agg {
                arg, arg2, filter, ..
            } => {
                if let Some(a) = arg {
                    stack.push(a);
                }
                if let Some(a) = arg2 {
                    stack.push(a);
                }
                // v1.29: FILTER is a per-row input like the arguments.
                if let Some(f) = filter {
                    stack.push(f);
                }
            }
            // v1.30: ordered-set aggregate — walk direct args, the
            // WITHIN GROUP sort keys, and the FILTER.
            Expr::WithinGroup {
                direct_args,
                within_order_by,
                filter,
                ..
            } => {
                for a in direct_args {
                    stack.push(a);
                }
                for o in within_order_by {
                    stack.push(&o.expr);
                }
                if let Some(f) = filter {
                    stack.push(f);
                }
            }
            Expr::ArrayCtor { elems, .. } => {
                for el in elems {
                    stack.push(el);
                }
            }
            Expr::Subscript { array, indices } => {
                stack.push(array);
                for i in indices {
                    stack.push(i);
                }
            }
            Expr::Slice { array, bounds } => {
                stack.push(array);
                for (l, u) in bounds {
                    if let Some(l) = l {
                        stack.push(l);
                    }
                    if let Some(u) = u {
                        stack.push(u);
                    }
                }
            }
            Expr::InSub { expr, .. } => stack.push(expr),
            // v0.87: quantified comparison visits `left` (subquery is its
            // own level); user operator visits both operands.
            Expr::Quantified { left, .. } => stack.push(left),
            Expr::UserOp { left, right, .. } => {
                stack.push(left);
                stack.push(right);
            }
            Expr::Window {
                args,
                partition_by,
                order_by,
                filter,
                ..
            } => {
                for a in args {
                    stack.push(a);
                }
                for p in partition_by {
                    stack.push(p);
                }
                for o in order_by {
                    stack.push(&o.expr);
                }
                // v1.29: FILTER on a windowed aggregate.
                if let Some(f) = filter {
                    stack.push(f);
                }
            }
            // Leaves, plus subquery levels (never descended into).
            Expr::Column { .. }
            | Expr::ResolvedCol { .. }
            | Expr::WholeRow { .. }
            | Expr::Literal(_)
            | Expr::Param(_)
            | Expr::ScalarSub(_)
            | Expr::ArraySubquery(_)
            | Expr::Exists { .. } => {}
        }
    }
}

/// v0.80: true for PG19's `GROUPING(...)` mask call, parsed as
/// `Func { name: "grouping" }`.
pub(crate) fn is_grouping_call(e: &Expr) -> bool {
    matches!(e, Expr::Func { name, .. } if name == "grouping")
}

/// v0.80: true when the expression contains a `GROUPING(...)` call at
/// this query level.
pub(crate) fn contains_grouping(e: &Expr) -> bool {
    let mut found = false;
    expr_walk(e, &mut |x| found = found || is_grouping_call(x));
    found
}

/// v0.80: argument lists of every `GROUPING(...)` call at this query
/// level, in pre-order.
pub(crate) fn collect_grouping_calls<'a>(e: &'a Expr, out: &mut Vec<&'a [Expr]>) {
    expr_walk(e, &mut |x| {
        if let Expr::Func { name, args } = x {
            if name == "grouping" {
                out.push(args.as_slice());
            }
        }
    });
}

/// v0.80: a `GROUPING(...)` nested inside an aggregate or window call at
/// this query level — PG19 rejects it at analysis (42803). Returns the
/// construct name for the error message.
pub(crate) fn grouping_misplaced(e: &Expr) -> Option<&'static str> {
    let mut hit: Option<&'static str> = None;
    expr_walk(e, &mut |x| {
        if hit.is_some() {
            return;
        }
        hit = match x {
            Expr::Agg { arg, arg2, .. } => {
                let bad = arg.as_deref().is_some_and(contains_grouping)
                    || arg2.as_deref().is_some_and(contains_grouping);
                bad.then_some("aggregate function calls")
            }
            Expr::Window {
                args,
                partition_by,
                order_by,
                ..
            } => {
                let bad = args.iter().any(contains_grouping)
                    || partition_by.iter().any(contains_grouping)
                    || order_by.iter().any(|o| contains_grouping(&o.expr));
                bad.then_some("window function calls")
            }
            _ => None,
        };
    });
    hit
}

/// v0.80: whether a `GROUPING(...)` argument denotes the same grouping
/// expression as a grouping key — PG19's `finalize_grouping_exprs` match
/// (equal Vars, else structurally equal expressions). Plain columns
/// resolve through the FROM schema, so `t.a`, `a`, and ordinal-expanded
/// keys compare by (range, column) rather than by text. Outer references
/// never resolve against this level's schema, so they never match —
/// like PG, which disallows them here.
pub(crate) fn grouping_arg_matches(schema: &[QCol], arg: &Expr, key: &Expr) -> bool {
    if let (
        Expr::Column {
            table: at,
            name: an,
        },
        Expr::Column {
            table: gt,
            name: gn,
        },
    ) = (arg, key)
    {
        let gscope = Scope {
            schema,
            row: &[],
            prov: None,
        };
        match (
            resolve_col(&[gscope], at.as_deref(), an),
            resolve_col(&[gscope], gt.as_deref(), gn),
        ) {
            (Ok((asi, aci)), Ok((gsi, gci))) => asi == gsi && aci == gci,
            _ => false,
        }
    } else {
        arg == key
    }
}

/// v0.80: PG19 `GROUPING(...)` analysis (parse_agg.c
/// `transformGroupingFunc` / `finalize_grouping_exprs`): every call takes
/// fewer than 32 arguments (54023), and every argument matches a grouping
/// expression of this query level — the union of all grouping sets
/// (42803).
pub(crate) fn validate_grouping_calls(
    stmt: &SelectStmt,
    schema: &[QCol],
    sets: &[Vec<Expr>],
) -> Result<(), ExecError> {
    let mut calls: Vec<&[Expr]> = Vec::new();
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_grouping_calls(expr, &mut calls);
        }
    }
    if let Some(h) = &stmt.having {
        collect_grouping_calls(h, &mut calls);
    }
    for o in &stmt.order_by {
        collect_grouping_calls(&o.expr, &mut calls);
    }
    for e in &stmt.distinct_on {
        collect_grouping_calls(e, &mut calls);
    }
    if calls.is_empty() {
        return Ok(());
    }
    for args in calls {
        if args.len() > 31 {
            return Err(exec_err(
                "54023",
                "GROUPING must have fewer than 32 arguments",
            ));
        }
        for a in args {
            let ok = sets
                .iter()
                .flatten()
                .any(|k| grouping_arg_matches(schema, a, k));
            if !ok {
                return Err(exec_err(
                    "42803",
                    "arguments to GROUPING must be grouping expressions of the associated query level",
                ));
            }
        }
    }
    Ok(())
}

/// v0.10: true when the expression contains a window function (at any
/// depth, but not crossing into subquery levels).
pub(crate) fn contains_window(e: &Expr) -> bool {
    match e {
        Expr::Window { .. } => true,
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => contains_window(expr),
        Expr::Column { .. }
        | Expr::ResolvedCol { .. }
        | Expr::WholeRow { .. }
        | Expr::Literal(_)
        | Expr::Param(_) => false,
        Expr::Arith { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Concat(left, right) => contains_window(left) || contains_window(right),
        // v0.79: array operands can hide window functions — recurse.
        Expr::ArrayCtor { elems, .. } => elems.iter().any(contains_window),
        // v0.81: composite operands can hide window functions — recurse.
        Expr::Row(elems) => elems.iter().any(contains_window),
        Expr::CastNamed { expr, .. } | Expr::FieldAccess { expr, .. } => contains_window(expr),
        Expr::Subscript { array, indices } => {
            contains_window(array) || indices.iter().any(contains_window)
        }
        Expr::Slice { array, bounds } => {
            contains_window(array)
                || bounds.iter().any(|(l, u)| {
                    l.as_deref().map(contains_window).unwrap_or(false)
                        || u.as_deref().map(contains_window).unwrap_or(false)
                })
        }
        Expr::Cmp { left, right, .. } => contains_window(left) || contains_window(right),
        Expr::Like { expr, pattern, .. } => contains_window(expr) || contains_window(pattern),
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => contains_window(expr) || contains_window(pattern),
        Expr::Between {
            expr, low, high, ..
        } => contains_window(expr) || contains_window(low) || contains_window(high),
        Expr::Not(x)
        | Expr::BitNot(x)
        | Expr::Neg(x)
        | Expr::IsNull { expr: x, .. }
        | Expr::IsBool { expr: x, .. } => contains_window(x),
        // v0.55: a window function anywhere in a CASE counts.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_deref().is_some_and(contains_window)
                || whens
                    .iter()
                    .any(|(k, r)| contains_window(k) || contains_window(r))
                || else_.as_deref().is_some_and(contains_window)
        }
        Expr::IsDistinctFrom { left, right, .. } => contains_window(left) || contains_window(right),
        Expr::Cast { expr, .. } => contains_window(expr),
        Expr::Func { args, .. } => args.iter().any(contains_window),
        Expr::Agg { arg, arg2, .. } => {
            arg.as_deref().map(contains_window).unwrap_or(false)
                || arg2.as_deref().map(contains_window).unwrap_or(false)
        }
        // v1.30: ordered-set aggregate — windows are forbidden in its
        // parts, but descend anyway for a faithful answer.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            direct_args.iter().any(contains_window)
                || within_order_by.iter().any(|o| contains_window(&o.expr))
                || filter.as_deref().map(contains_window).unwrap_or(false)
        }
        Expr::Extract { from, .. } => contains_window(from),
        Expr::InSub { expr, .. } => contains_window(expr),
        // v0.87: quantified comparison (window in `left` counts; the
        // subquery is its own level) and user operator.
        Expr::Quantified { left, .. } => contains_window(left),
        Expr::UserOp { left, right, .. } => contains_window(left) || contains_window(right),
        // Subqueries are separate query levels.
        Expr::ScalarSub(_) | Expr::ArraySubquery(_) | Expr::Exists { .. } => false,
    }
}
