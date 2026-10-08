// v1.78 mechanical split: moved verbatim from src/exec.rs (18553-22011).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ============================================================================
// v1.46: EXPLAIN VERBOSE `Output:` targetlist rendering.
//
// PG19 reference (`src/backend/commands/explain.c`, `ExplainNode`):
// `if (es->verbose) show_plan_tlist(planstate, ancestors, es)` runs right
// after the node line, BEFORE every other node property (Join Filter,
// Filter, Index Cond, Sort Key, ...). `show_plan_tlist`:
//   - returns silently when `plan->targetlist == NIL` (no line at all),
//   - is skipped for Append / MergeAppend / RecursiveUnion,
//   - deparses each tlist entry with `useprefix = es->rtable_size > 1`
//     (explain.c `get_variable`: the range-table alias when present,
//     else the relation name).
// Text rendering (`ExplainPropertyList`, explain_format.c) is
// `{es->indent * 2 spaces}Output: {comma-joined entries}` — which is
// exactly the existing `pg_ppad` indent (`6 * depth + 2`).
//
// Plan nodes carry `output: Vec<String>` (empty == NIL == no line),
// computed bottom-up while PG-text planning (`pg == true`, i.e. COSTS
// OFF). Rules per node (documented approximations where PG's full
// planner is deeper than ours):
//   - SeqScan / IndexScan / IndexOrderScan: this table's columns
//     referenced by the query level's select list (`*` in table order,
//     explicit refs in select order), then the item's WHERE-slice
//     columns, then GROUP BY / HAVING / ORDER BY columns, then index
//     key columns — deduplicated, first-appearance order. PG builds
//     the baserel tlist from the query targetlist vars first and then
//     adds qual/sort vars (`add_new_columns_to_pathtarget`).
//   - NestedLoop (non-top): verbatim concatenation of the children's
//     entries (PG's join tlist starts as outer ++ inner). The
//     ruleutils.c paren-forcing for Vars referencing subquery outputs
//     (`Output: 1, (2), ((2))`) is NOT modeled — those statements stay
//     EXPECTED-FAIL.
//   - Result / Aggregate / Unique / Sort / Limit, and a top-level
//     NestedLoop: the query's select list deparsed with
//     `pg_expr_text` (PG's top-node tlist IS the query targetlist,
//     resjunk hidden).
//   - SubqueryScan: the subquery's output column names qualified by
//     the scan alias (`ss.x, ss.u`).
//   - Values: the `FROM (VALUES ...) AS v(a, b)` column aliases (or
//     PG's `column1, ...` names) qualified by the alias.
// Strictness: deparse uses `pg_expr_text`, never the `_or_debug`
// fallback. Whenever an entry has no faithful spelling (unexpandable
// `*`, whole-row refs, unknown virtual-table columns), the whole
// node's line is omitted rather than printed wrong.
// ============================================================================

/// v1.46: PG19 `es->rtable_size > 1` approximation for the Output-deparse
/// `useprefix` rule: the number of base range-table entries, with
/// explicit joins flattened to their inputs (like `pg_plan_ctx`).
/// PG counts CTE rtable entries slightly differently; documented
/// approximation.
pub(crate) fn pg_rtable_size(items: &[FromItem]) -> usize {
    fn count(item: &FromItem) -> usize {
        match item {
            FromItem::Join { left, right, .. } => count(left) + count(right),
            _ => 1,
        }
    }
    items.iter().map(count).sum()
}

/// v1.46: does the column reference `(qual, name)` belong to the FROM
/// item with qualifiers `item_quals` (`[table]` or `[table, alias]`)?
/// Unqualified refs resolve against the level's plan context; an
/// ambiguous ref (which PG would reject with 42702) is attributed to
/// the first FROM item owning the column, keeping EXPLAIN
/// deterministic.
pub(crate) fn pg_ref_belongs(
    qual: Option<&str>,
    name: &str,
    item_quals: &[String],
    table_cols: &[String],
    px: &PgPlanCtx,
) -> bool {
    if let Some(q) = qual {
        return item_quals.iter().any(|x| x == q);
    }
    if !table_cols.iter().any(|c| c == name) {
        return false;
    }
    let owners: Vec<&Vec<String>> = px
        .items
        .iter()
        .filter(|(_, cols)| cols.iter().any(|(n, _)| n == name))
        .map(|(quals, _)| quals)
        .collect();
    match owners.len() {
        1 => owners[0].iter().any(|q| item_quals.contains(q)),
        // Ambiguous (or absent from the context): first owner wins.
        _ => owners
            .first()
            .map(|quals| quals.iter().any(|q| item_quals.contains(q)))
            .unwrap_or(false),
    }
}

/// v1.46: append this-table column refs of `e` to `needed` (deduped,
/// first-appearance order). Sets `whole_row` when a whole-row ref
/// (`SELECT tbl`) names this item — the caller treats that as
/// unfaithful (no per-column PG spelling modeled here).
pub(crate) fn pg_collect_needed(
    e: &Expr,
    item_quals: &[String],
    table_cols: &[String],
    px: &PgPlanCtx,
    needed: &mut Vec<String>,
    whole_row: &mut bool,
) {
    let mut refs = Vec::new();
    collect_column_refs(e, &mut refs);
    for (qual, name) in refs {
        if name == "*" {
            if qual
                .as_deref()
                .map(|q| item_quals.iter().any(|x| x == q))
                .unwrap_or(false)
            {
                *whole_row = true;
            }
            continue;
        }
        if pg_ref_belongs(qual.as_deref(), &name, item_quals, table_cols, px)
            && !needed.iter().any(|n| n == &name)
        {
            needed.push(name);
        }
    }
}

/// v1.46: VERBOSE `Output:` entries for a base-table scan node; see the
/// module doc above for the ordering rule. Returns `None` when no
/// faithful rendering exists — the line is omitted, never wrong.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pg_scan_output(
    stmt: &SelectStmt,
    name: &str,
    alias: Option<&str>,
    table_cols: &[String],
    where_: Option<&Expr>,
    index_cols: &[String],
    px: &PgPlanCtx,
) -> Option<Vec<String>> {
    let mut quals = vec![name.to_string()];
    if let Some(a) = alias {
        if a != name {
            quals.push(a.to_string());
        }
    }
    let qualify = pg_rtable_size(&stmt.from) + stmt.dead_rtes > 1;
    let prefix = alias.unwrap_or(name);
    let mut needed: Vec<String> = Vec::new();
    // v1.52: indices into `needed` holding pre-rendered expression text
    // (not column names) — the final map must use them verbatim instead
    // of `pg_quote_ident`-ing them (`Output: 1`, not `Output: "1"`).
    let mut verbatim: Vec<usize> = Vec::new();
    let collect = |e: &Expr, needed: &mut Vec<String>| -> Option<()> {
        let mut whole_row = false;
        pg_collect_needed(e, &quals, table_cols, px, needed, &mut whole_row);
        if whole_row {
            return None;
        }
        Some(())
    };
    // 1. select list (`*` in table order, explicit refs in select order).
    for item in &stmt.items {
        match item {
            SelectItem::All => {
                if table_cols.is_empty() {
                    return None;
                }
                for c in table_cols {
                    if !needed.contains(c) {
                        needed.push(c.clone());
                    }
                }
            }
            SelectItem::AllOf(q) => {
                if quals.iter().any(|x| x == q) {
                    if table_cols.is_empty() {
                        return None;
                    }
                    for c in table_cols {
                        if !needed.contains(c) {
                            needed.push(c.clone());
                        }
                    }
                }
            }
            SelectItem::Expr { expr, .. } => {
                let before = needed.len();
                collect(expr, &mut needed)?;
                // v1.52: PG19 `apply_scanjoin_target_to_paths` (planner.c)
                // applies the FULL final targetlist — including non-Var
                // exprs — to the top scan/join paths, so `SELECT 1 FROM t1`
                // prints `Output: 1` (verified on live PG16; grounded in
                // PG19 createplan.c `use_physical_tlist` comments). A
                // select item that contributes no column of this table and
                // is not itself a column reference (e.g. a constant)
                // renders as its deparsed text. A column reference to
                // another table's column contributes nothing here; the
                // v1.50 all-columns fallback below still covers the
                // join-child physical-tlist case (e.g. T1's
                // `Output: int4_tbl.f1`, whose select item is `q2`, a
                // column of int8_tbl).
                if needed.len() == before
                    && !matches!(expr, Expr::Column { .. } | Expr::ResolvedCol { .. })
                {
                    if let Some(t) = pg_expr_text(expr, px, qualify) {
                        // Verbatim: already fully rendered (including any
                        // qualification); the final map must not quote it.
                        verbatim.push(needed.len());
                        needed.push(t);
                    }
                }
            }
        }
    }
    // 2. this item's WHERE slice.
    if let Some(w) = where_ {
        collect(w, &mut needed)?;
    }
    // 3. GROUP BY / HAVING / ORDER BY (sort/group vars are added to the
    // baserel tlist after the targetlist vars in PG).
    for set in &stmt.group_by {
        for g in set {
            collect(g, &mut needed)?;
        }
    }
    if let Some(h) = &stmt.having {
        collect(h, &mut needed)?;
    }
    for t in &stmt.order_by {
        collect(&t.expr, &mut needed)?;
    }
    // 4. index key columns (PG adds indexqual vars to the baserel tlist).
    for c in index_cols {
        if table_cols.contains(c) && !needed.contains(c) {
            needed.push(c.clone());
        }
    }
    // v1.50: If no columns were deemed "needed", fall back to the table's
    // columns (PG's Seq Scan targetlist includes the table's columns even
    // when the query doesn't explicitly reference them, e.g. the outer
    // side of a LEFT JOIN with ON NULL).
    if needed.is_empty() && !table_cols.is_empty() {
        needed = table_cols.to_vec();
    }
    Some(
        needed
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                if verbatim.contains(&i) {
                    // v1.52: pre-rendered expression text — verbatim.
                    c
                } else if qualify {
                    format!("{}.{}", pg_quote_ident(prefix), pg_quote_ident(&c))
                } else {
                    pg_quote_ident(&c)
                }
            })
            .collect(),
    )
}

/// v1.46: the output column names of a subquery SELECT (PG19
/// `FigureColName`, via our `expr_col_name`), unqualified. `None` when
/// they cannot be determined faithfully (`SELECT *` over a non-table
/// source).
pub(crate) fn pg_subquery_col_names(
    eng: &Engine,
    sub: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
    ctes: &[CteDef],
) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for item in &sub.items {
        match item {
            SelectItem::Expr { expr, alias } => {
                names.push(alias.clone().unwrap_or_else(|| expr_col_name(expr)));
            }
            SelectItem::All => {
                // Only a single plain-table source can be expanded.
                let cols: Vec<String> = match sub.from.as_slice() {
                    [FromItem::Table { name, .. }] => {
                        if ctes.iter().any(|c| c.name == *name) {
                            return None;
                        }
                        eng.db
                            .find_table(name, snap, &[own], session)
                            .map(|t| t.columns.iter().map(|(n, _)| n.clone()).collect())
                            .unwrap_or_default()
                    }
                    _ => return None,
                };
                if cols.is_empty() {
                    return None;
                }
                names.extend(cols);
            }
            SelectItem::AllOf(q) => {
                let mut found: Option<Vec<String>> = None;
                for it in &sub.from {
                    if pg_item_quals(it).iter().any(|x| x == q) {
                        if let FromItem::Table { name, .. } = it {
                            if ctes.iter().any(|c| c.name == *name) {
                                return None;
                            }
                            found = eng
                                .db
                                .find_table(name, snap, &[own], session)
                                .map(|t| t.columns.iter().map(|(n, _)| n.clone()).collect());
                        }
                        break;
                    }
                }
                match found {
                    Some(cols) if !cols.is_empty() => names.extend(cols),
                    _ => return None,
                }
            }
        }
    }
    Some(names)
}

/// v1.46: output column names of a CTE (`col_aliases` win positionally,
/// else the body's select names). `visible` holds the CTEs the body may
/// reference (outer ++ earlier siblings).
pub(crate) fn pg_cte_col_names(
    eng: &Engine,
    cte: &CteDef,
    visible: &[CteDef],
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<Vec<String>> {
    if !cte.col_aliases.is_empty() {
        return Some(cte.col_aliases.clone());
    }
    match &cte.body {
        CteBody::Simple(sel) => pg_subquery_col_names(eng, sel, snap, own, session, visible),
        CteBody::Union { left, .. } => {
            pg_subquery_col_names(eng, left, snap, own, session, visible)
        }
        // Data-modifying CTEs fail closed before planning reaches here.
        CteBody::Dml(_) => None,
    }
}

/// v1.46: VERBOSE `Output:` for a SubqueryScan: the subquery's output
/// column names qualified by the scan alias (`ss.x, ss.u`), matching
/// PG19's rtable-qualified tlist deparse.
pub(crate) fn pg_subquery_scan_output(
    eng: &Engine,
    sub: &SelectStmt,
    alias: &str,
    col_aliases: &[String],
    snap: &Snapshot,
    own: u64,
    session: u64,
    ctes: &[CteDef],
) -> Option<Vec<String>> {
    let names: Vec<String> = if col_aliases.is_empty() {
        pg_subquery_col_names(eng, sub, snap, own, session, ctes)?
    } else {
        col_aliases.to_vec()
    };
    Some(
        names
            .into_iter()
            .map(|n| format!("{}.{}", pg_quote_ident(alias), pg_quote_ident(&n)))
            .collect(),
    )
}

/// v1.46: `SELECT *` expansion for one FROM item: its output columns,
/// qualified iff `qualify` (PG19 `useprefix`). `None` when the columns
/// are not faithfully known (function scans, CTEs with unknowable
/// bodies).
pub(crate) fn pg_from_item_star_cols(
    eng: &Engine,
    item: &FromItem,
    qualify: bool,
    snap: &Snapshot,
    own: u64,
    session: u64,
    ctes: &[CteDef],
) -> Option<Vec<String>> {
    fn qual_name(qualify: bool, qual: &str, col: &str) -> String {
        if qualify {
            format!("{}.{}", pg_quote_ident(qual), pg_quote_ident(col))
        } else {
            pg_quote_ident(col)
        }
    }
    match item {
        FromItem::Table { name, alias, .. } => {
            let q = alias.as_deref().unwrap_or(name);
            let cols: Vec<String> = if let Some(pos) = ctes.iter().rposition(|c| c.name == *name) {
                pg_cte_col_names(eng, &ctes[pos], &ctes[..pos], snap, own, session)?
            } else {
                eng.db
                    .find_table(name, snap, &[own], session)
                    .map(|t| t.columns.iter().map(|(n, _)| n.clone()).collect())
                    .unwrap_or_default()
            };
            if cols.is_empty() {
                return None;
            }
            Some(
                cols.into_iter()
                    .map(|c| qual_name(qualify, q, &c))
                    .collect(),
            )
        }
        FromItem::Derived {
            sub,
            alias,
            col_aliases,
            ..
        } => {
            let names: Vec<String> = if col_aliases.is_empty() {
                pg_subquery_col_names(eng, sub, snap, own, session, ctes)?
            } else {
                col_aliases.clone()
            };
            Some(
                names
                    .into_iter()
                    .map(|n| qual_name(qualify, alias, &n))
                    .collect(),
            )
        }
        FromItem::Values {
            alias,
            col_aliases,
            rows,
            ..
        } => {
            let n = rows.first().map(|r| r.len()).unwrap_or(0);
            let names: Vec<String> = if col_aliases.is_empty() {
                (1..=n).map(|i| format!("column{i}")).collect()
            } else {
                col_aliases.clone()
            };
            Some(
                names
                    .into_iter()
                    .map(|c| qual_name(qualify, alias, &c))
                    .collect(),
            )
        }
        FromItem::Join { left, right, .. } => {
            let mut v = pg_from_item_star_cols(eng, left, qualify, snap, own, session, ctes)?;
            v.extend(pg_from_item_star_cols(
                eng, right, qualify, snap, own, session, ctes,
            )?);
            Some(v)
        }
        // Function scans have no faithful per-column expansion here.
        FromItem::Function { .. } => None,
    }
}

/// v1.46: deparse the query level's select list for a top plan node's
/// VERBOSE `Output:` (PG19: the top node's tlist IS the query
/// targetlist, resjunk hidden). Strict: any entry without a faithful
/// PG spelling — or any unexpandable `*` — omits the whole line.
pub(crate) fn pg_top_select_output(
    eng: &Engine,
    stmt: &SelectStmt,
    // v1.49: pre-flip FROM tree — bare `SELECT *` expands in PG's written
    // column order (the query targetlist), not the flipped physical order.
    orig_from: &[FromItem],
    snap: &Snapshot,
    own: u64,
    session: u64,
    ctes: &[CteDef],
    px: &PgPlanCtx,
) -> Option<Vec<String>> {
    // v1.54: PG counts dead rtable entries (pulled-up subqueries) in
    // `es->rtable_size` for the `useprefix` rule.
    let qualify = pg_rtable_size(&stmt.from) + stmt.dead_rtes > 1;
    let mut out = Vec::new();
    for item in &stmt.items {
        match item {
            // Whole-row refs have no per-column spelling modeled here.
            SelectItem::Expr { expr, .. } => out.push(pg_expr_text(expr, px, qualify)?),
            SelectItem::All => {
                for it in orig_from {
                    out.extend(pg_from_item_star_cols(
                        eng, it, qualify, snap, own, session, ctes,
                    )?);
                }
            }
            SelectItem::AllOf(q) => {
                let mut done = false;
                for it in &stmt.from {
                    if pg_item_quals(it).iter().any(|x| x == q) {
                        out.extend(pg_from_item_star_cols(
                            eng, it, qualify, snap, own, session, ctes,
                        )?);
                        done = true;
                        break;
                    }
                }
                if !done {
                    return None;
                }
            }
        }
    }
    Some(out)
}

// ============================================================================
// v1.49: PG19 const-false dummy rels for EXPLAIN plan shapes.
//
// PG19 reference:
// - `eval_const_expressions` (optimizer/util/clauses.c) folds the WHERE /
//   JOIN ON clause; a top-level qual that is a FALSE-or-NULL Const makes
//   the rel dummy.
// - `restriction_is_constant_false` (optimizer/path/joinrels.c:1547-1588):
//   "A restriction clause is constant FALSE if it is a Const of the
//   wrong value ... constant NULL is as good as constant FALSE for our
//   purposes" (joinrels.c:1580-1582). For JOIN_INNER/JOIN_SEMI the whole
//   joinrel becomes dummy; for JOIN_LEFT/JOIN_ANTI with a non-pushed-down
//   false qual the inner rel becomes dummy (joinrels.c:1096-1121);
//   JOIN_FULL is never dummy.
// - The dummy rel plans as a bare `Result` with `One-Time Filter: false`
//   (optimizer/plan/createplan.c `make_result`), and EXPLAIN renders
//   `Replaces: <Scan|Join> on ...` for it (commands/explain.c
//   `show_result_replacement_info`, explain.c:5051-5064; skipped only for
//   a single RTE_RESULT, explain.c:5048-5059).
//
// Conservative by design: only the boolean skeleton (literals, NULL,
// NOT/AND/OR over foldable operands) is folded. v1.59 added comparisons
// of constants; v1.60 adds calls to IMMUTABLE functions with all-constant
// arguments (PG19 `simplify_function`, clauses.c:5205-5297) plus the
// pulled-up constants of constant-folded function-scan RTEs (PG19
// `pull_up_constant_function`, prepjointree.c:2235). STABLE/VOLATILE calls
// and unresolvable vars are NEVER folded, so the v0.55 no-plan-time-folding
// discipline (e.g. `1/0` in a CASE arm) stays intact. EXPLAIN-path only:
// the executor evaluates WHERE/ON normally, so execution semantics are
// untouched.

/// Fold a boolean expression to a constant. `Some(Some(b))` = provably
/// `b`; `Some(None)` = provably NULL; `None` = not foldable.
/// v1.60: takes the fold context so pulled-up function-scan constants
/// (and, inside a body fold, parameters) resolve during the fold.
pub(crate) fn pg_fold_bool_const(e: &Expr, fc: &mut PgConstFold) -> Option<Option<bool>> {
    match e {
        Expr::Literal(Literal::Bool(b)) => Some(Some(*b)),
        Expr::Literal(Literal::Null) => Some(None),
        Expr::Not(x) => match pg_fold_bool_const(x, fc) {
            Some(Some(b)) => Some(Some(!b)),
            Some(None) => Some(None),
            None => None,
        },
        Expr::And(l, r) => match (pg_fold_bool_const(l, fc), pg_fold_bool_const(r, fc)) {
            // v1.77: PG19 `eval_const_expressions` simplifies `FALSE AND x`
            // to FALSE without needing x to fold (likewise `TRUE OR x` to
            // TRUE). Sound: FALSE AND anything is FALSE in three-valued
            // logic.
            (Some(Some(false)), _) | (_, Some(Some(false))) => Some(Some(false)),
            (Some(a), Some(b)) => Some(match (a, b) {
                (Some(true), Some(true)) => Some(true),
                _ => None,
            }),
            _ => None,
        },
        Expr::Or(l, r) => match (pg_fold_bool_const(l, fc), pg_fold_bool_const(r, fc)) {
            (Some(Some(true)), _) | (_, Some(Some(true))) => Some(Some(true)),
            (Some(a), Some(b)) => Some(match (a, b) {
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }),
            _ => None,
        },
        // v1.59: fold comparisons of constants (PG19 eval_const_expressions
        // folds an OpExpr whose args are all Consts). Comparison operators
        // are strict: a NULL operand yields NULL, not false.
        Expr::Cmp { op, left, right } => {
            let l = pg_fold_const_item(left, fc)?;
            let r = pg_fold_const_item(right, fc)?;
            if matches!(l, Literal::Null) || matches!(r, Literal::Null) {
                return Some(None);
            }
            Some(Some(pg_literal_cmp(&l, *op, &r)?))
        }
        // v1.59: fold IS [NOT] NULL on a constant.
        Expr::IsNull { expr, neg } => {
            let v = pg_fold_const_item(expr, fc)?;
            let is_null = matches!(v, Literal::Null);
            Some(Some(if *neg { !is_null } else { is_null }))
        }
        _ => None,
    }
}

/// PG19 `restriction_is_constant_false` for EXPLAIN plan shapes: true
/// when the qual folds to a FALSE-or-NULL Const.
/// v1.60: takes the fold context (pulled-up function-scan constants).
pub(crate) fn pg_is_const_false(e: &Expr, fc: &mut PgConstFold) -> bool {
    matches!(pg_fold_bool_const(e, fc), Some(v) if v != Some(true))
}

/// v1.70: PG19 `restriction_is_constant_false` (joinrels.c): a scan WHERE
/// containing `col = c1 AND col = c2` on the same qualified column with
/// provably-different literals plans as a dummy rel. Fail closed:
/// incomparable literals (`pg_literal_cmp` → `None`, including NULL
/// constants), OR-nested quals, non-Eq operators, and unqualified columns
/// (ambiguous) never fire.
pub(crate) fn pg_where_is_contradiction(w: &Expr) -> bool {
    let mut pairs: Vec<((Option<String>, String), &Literal)> = Vec::new();
    for c in split_conjuncts(w) {
        if let Expr::Cmp {
            op: CmpOp::Eq,
            left,
            right,
        } = c
        {
            let (col, lit) = match (&**left, &**right) {
                (Expr::Column { table, name }, Expr::Literal(l)) => {
                    ((table.clone(), name.clone()), l)
                }
                (Expr::Literal(l), Expr::Column { table, name }) => {
                    ((table.clone(), name.clone()), l)
                }
                _ => continue,
            };
            // v1.70: unqualified columns are ambiguous (the column could
            // belong to another RTE); NULL never contradicts.
            if col.0.is_none() || matches!(lit, Literal::Null) {
                continue;
            }
            pairs.push((col, lit));
        }
    }
    for i in 0..pairs.len() {
        for j in (i + 1)..pairs.len() {
            let ((t1, n1), l1) = &pairs[i];
            let ((t2, n2), l2) = &pairs[j];
            if t1 == t2 && n1 == n2 && pg_literal_cmp(l1, CmpOp::Eq, l2) == Some(false) {
                return true;
            }
        }
    }
    false
}

/// User-facing RTE names for PG19 `Replaces:` (explain.c
/// `explain_result_replacement`: the RTE's `eref->aliasname` — the alias
/// when present, else the relation name; JOIN RTEs contribute their
/// inputs' names, mirroring the dummy rel's relids).
pub(crate) fn pg_replaces_names(items: &[FromItem], out: &mut Vec<String>) {
    for it in items {
        match it {
            FromItem::Table { name, alias, .. } => {
                out.push(alias.clone().unwrap_or_else(|| name.clone()))
            }
            FromItem::Derived { alias, .. } => out.push(alias.clone()),
            FromItem::Values { alias, .. } => out.push(alias.clone()),
            FromItem::Function { name, alias, .. } => {
                out.push(alias.clone().unwrap_or_else(|| name.clone()))
            }
            FromItem::Join { left, right, .. } => {
                pg_replaces_names(std::slice::from_ref(left), out);
                pg_replaces_names(std::slice::from_ref(right), out);
            }
        }
    }
}

/// PG19 `Replaces:` text for a dummy rel (explain.c:5051-5064):
/// `Scan on <rel>` for a single RTE, `Join on <a>, <b>, ...` for more.
/// `None` when there is no FROM (a Result over an empty FROM clause
/// prints no `Replaces:`, explain.c:5048-5059).
pub(crate) fn pg_result_replaces(from: &[FromItem]) -> Option<String> {
    if from.is_empty() {
        return None;
    }
    let mut names = Vec::new();
    pg_replaces_names(from, &mut names);
    if names.len() == 1 {
        Some(format!("Scan on {}", names[0]))
    } else {
        Some(format!("Join on {}", names.join(", ")))
    }
}

// ============================================================================
// v1.68: NOT IN / NOT EXISTS → Hash Anti Join (PG19 subselect.c
// `convert_ANY_sublink_to_join` / `convert_EXISTS_sublink_to_join`,
// called from `pull_up_sublinks` in prepjointree.c).
// ============================================================================

/// v1.68: catalog NOT NULL check for a plain column (the catalog half of
/// PG19 `sublink_testexpr_is_not_nullable` /
/// `query_outputs_are_not_nullable`: "if we can prove that neither the
/// outer query's expressions nor the sub-select's output columns can be
/// NULL, and further that the operator itself cannot return NULL for
/// non-null inputs, then the logic is identical and it's safe to convert
/// NOT IN to an anti-join", subselect.c:1355-1362). Fail-closed: unknown
/// table or column reads as nullable.
pub(crate) fn pg_anti_col_not_null(
    eng: &Engine,
    table: &str,
    col: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    eng.db
        .find_table(table, snap, &[own], session)
        .and_then(|t| {
            t.columns
                .iter()
                .position(|(n, _)| n == col)
                .and_then(|i| t.not_null.get(i).copied())
        })
        .unwrap_or(false)
}

/// v1.68: PG19 `set_rtable_names` (ruleutils.c:4288-4380) alias
/// uniquification for EXPLAIN. Names are taken in rtable order (outer
/// tables first, then the pulled-up subquery's tables); a user alias wins
/// over the relation name; the first use keeps the bare name and each
/// later collision appends `_1`, `_2`, ... (the hash counter starts at 0
/// and is pre-incremented, so the first collision is `_1`).
pub(crate) fn pg_anti_unique_names(names: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut counters: HashMap<String, u64> = HashMap::new();
    names
        .iter()
        .map(|n| {
            if seen.insert(n.clone()) {
                counters.insert(n.clone(), 0);
                n.clone()
            } else {
                let c = counters.entry(n.clone()).or_insert(0);
                // Mirror PG's do/while: keep incrementing until the
                // suffixed name is itself unused.
                loop {
                    *c += 1;
                    let cand = format!("{n}_{c}");
                    if seen.insert(cand.clone()) {
                        return cand;
                    }
                }
            }
        })
        .collect()
}

/// v1.68: set `alias` on every unaliased scan node in the subtree (the
/// pulled-up subquery's table after `set_rtable_names` uniquification).
/// Only `SeqScan`/`IndexScan` carry aliases; anything else is left alone
/// (a non-scan inner shape fails the v1.68 shape gate before this runs).
pub(crate) fn pg_anti_set_scan_alias(node: &mut PlanNode, alias: &str) {
    match node {
        PlanNode::SeqScan { alias: a, .. } | PlanNode::IndexScan { alias: a, .. } => {
            if a.is_none() {
                *a = Some(alias.to_string());
            }
        }
        PlanNode::NestedLoop { outer, inner, .. }
        | PlanNode::HashJoin { outer, inner, .. }
        | PlanNode::MergeJoin { outer, inner, .. } => {
            pg_anti_set_scan_alias(outer, alias);
            pg_anti_set_scan_alias(inner, alias);
        }
        PlanNode::Materialize { child, .. }
        | PlanNode::Aggregate { child, .. }
        | PlanNode::Unique { child, .. }
        | PlanNode::Sort { child, .. }
        | PlanNode::Limit { child, .. }
        | PlanNode::SubqueryScan { child, .. } => pg_anti_set_scan_alias(child, alias),
        _ => {}
    }
}

/// v1.68: does this qualifier name the given table (by relation name or
/// user alias)? `None` (unqualified) never matches here — callers resolve
/// unqualified columns against the single-table shape separately.
pub(crate) fn pg_anti_qual_is(qual: &Option<String>, table: &str, alias: &Option<String>) -> bool {
    match qual {
        Some(q) => q == table || Some(q) == alias.as_ref(),
        None => false,
    }
}

/// v1.68: shared shape gate for the pulled-up anti-join subquery: single
/// plain table, no setops/grouping/distinct/ordering/CTEs. Mirrors PG19
/// `simplify_EXISTS_query`'s rejections (subselect.c:1804-1814), minus the
/// constant-LIMIT carve-out which each path handles itself. Returns the
/// inner table name and user alias.
pub(crate) fn pg_anti_gate_sub(sub: &SelectStmt) -> Option<(String, Option<String>)> {
    if !sub.with.is_empty()
        || sub.set_op.is_some()
        || !sub.group_by.is_empty()
        || sub.having.is_some()
        || sub.distinct
        || !sub.distinct_on.is_empty()
        || !sub.order_by.is_empty()
        || sub.offset.is_some()
    {
        return None;
    }
    match sub.from.as_slice() {
        [FromItem::Table { name, alias, .. }] => Some((name.clone(), alias.clone())),
        _ => None,
    }
}

/// v1.68: the `NOT EXISTS` half of `pg_try_anti_join` (PG19
/// `convert_EXISTS_sublink_to_join`, subselect.c:1590). Returns the
/// `(outer_col, inner_col, inner_table, inner_alias, inner_subquery)` tuple
/// on success, `Ok(None)` when the sublink is not convertible (fail closed).
pub(crate) fn anti_from_exists(
    outer_table: &str,
    outer_alias: &Option<String>,
    sub: &SelectStmt,
) -> Result<Option<(String, String, String, Option<String>, SelectStmt)>, ExecError> {
    // PG requires no nullability proof for EXISTS (it is never NULL), but
    // the subquery must simplify: no setops, aggs, grouping sets, HAVING,
    // OFFSET (subselect.c:1804-1814), and a constant positive LIMIT is
    // dropped, not planned.
    if let Some(n) = sub.limit {
        if n <= 0 {
            return Ok(None);
        }
    }
    let (inner_table, inner_alias) = match pg_anti_gate_sub(sub) {
        Some(t) => t,
        None => return Ok(None),
    };
    let w = match &sub.where_ {
        Some(w) => w,
        None => return Ok(None),
    };
    let (left, right) = match w {
        Expr::Cmp {
            op: CmpOp::Eq,
            left,
            right,
        } => (left.as_ref(), right.as_ref()),
        _ => return Ok(None),
    };
    // One side must name the inner table, the other the outer table (the
    // correlation). Both sides must be qualified — unqualified sides are
    // ambiguous without namespace resolution, so fail closed.
    let side_of = |e: &Expr| -> Option<bool> {
        match e {
            Expr::Column { table: qt, .. } => {
                if pg_anti_qual_is(qt, &inner_table, &inner_alias) {
                    Some(true)
                } else if pg_anti_qual_is(qt, outer_table, outer_alias) {
                    Some(false)
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    let (inner_e, outer_e) = match (side_of(left), side_of(right)) {
        (Some(true), Some(false)) => (left, right),
        (Some(false), Some(true)) => (right, left),
        _ => return Ok(None),
    };
    let col_name = |e: &Expr| match e {
        Expr::Column { name, .. } => name.clone(),
        _ => String::new(),
    };
    let mut inner_sub = sub.clone();
    inner_sub.limit = None;
    Ok(Some((
        col_name(outer_e),
        col_name(inner_e),
        inner_table,
        inner_alias,
        inner_sub,
    )))
}

/// v1.68: try to plan `SELECT ... FROM <single table> WHERE <single
/// NOT IN / NOT EXISTS sublink>` as a PG19 `Hash Anti Join`.
///
/// This mirrors `convert_ANY_sublink_to_join` (for `NOT (x = ANY
/// (subquery))`, i.e. `NOT IN`) and `convert_EXISTS_sublink_to_join` (for
/// `NOT EXISTS`), which build a `JOIN_ANTI` JoinExpr when the sublink is
/// convertible, and fail closed otherwise.
///
/// EXPLAIN-path only (called with `pg == true`): like every other
/// `PlanNode`, the executor never sees the node — it keeps its own
/// per-row `InSub`/`Exists` subplan evaluation — so results are identical
/// by construction (the v1.64 display-only precedent). A `None` return
/// falls through to the normal planner; any shape outside the narrow
/// v1.68 gate (multi-table subqueries, grouping, expression keys, ...)
/// returns `None` rather than guessing.
/// v1.69: dispatcher for PG19 anti-join planning. Tries the v1.68
/// single-table shapes first, then the v1.69 extensions below.
pub(crate) fn pg_try_anti_join(
    eng: &Engine,
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
    outer_ctes: &[CteDef],
    verbose: bool,
) -> Result<Option<PlanNode>, ExecError> {
    if let Some(node) = pg_try_anti_join_simple(eng, stmt, snap, own, session, outer_ctes, verbose)?
    {
        return Ok(Some(node));
    }
    if let Some(node) = pg_try_anti_join_multi(eng, stmt, snap, own, session, outer_ctes, verbose)?
    {
        return Ok(Some(node));
    }
    if let Some(node) = pg_try_anti_join_row(eng, stmt, snap, own, session, outer_ctes, verbose)? {
        return Ok(Some(node));
    }
    if let Some(node) = pg_try_anti_join_outer(eng, stmt, snap, own, session, outer_ctes, verbose)?
    {
        return Ok(Some(node));
    }
    if let Some(node) = pg_try_anti_join_on(eng, stmt, snap, own, session, outer_ctes, verbose)? {
        return Ok(Some(node));
    }
    Ok(None)
}

// ============================================================================
// v1.69: Merge Anti Join + PG19 no-stats row-estimate cost choice (T13–T19).
//
// Extends the v1.68 Hash Anti Join (`pg_try_anti_join_simple`) with the
// remaining PG19 anti-join plan shapes, all EXPLAIN-path display-only
// (the executor is untouched; results identical by construction):
//   - multi-table (join) subquery inners (T13–T16): the inner is a join
//     whose own plan is chosen by the no-stats 3-way; the top is Hash
//     Anti Join or Merge Anti Join by `cost_mergejoin` vs the SEMI/ANTI
//     `cost_hashjoin` on PG19 no-stats estimates;
//   - multi-key NOT IN (T19): two merge/hash clauses, alias uniquified;
//   - anti join under a top-level LEFT JOIN (T17);
//   - a sublink in an ON clause (T18): the inner renders under
//     `Materialize`, the top is `Nested Loop Left Join`.
// The row-estimate basis is PG19's no-stats default
// (`table_block_relation_estimate_size`: 10-page minimum, never
// `ANALYZE`d) — v1.67's principled path, not actual row counts.
// Every shape fails closed to `None` on anything unrecognized.
// ============================================================================

/// v1.69: a synthetic single-table `SELECT *` for planning an anti-join
/// side scan (the Filter text deparses PG19-style via `plan_select`).
pub(crate) fn pg_anti_scan_stmt(
    table: &str,
    alias: &Option<String>,
    where_: Option<Expr>,
) -> SelectStmt {
    SelectStmt {
        with: vec![],
        distinct: false,
        distinct_on: vec![],
        items: vec![SelectItem::All],
        from: vec![FromItem::Table {
            name: table.to_string(),
            alias: alias.clone(),
            col_aliases: vec![],
            only: false,
        }],
        where_,
        group_by: vec![],
        group_by_sets: false,
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
        for_update: false,
        for_update_of: vec![],
        set_op: None,
        dead_rtes: 0,
    }
}

/// v1.69: is this conjunct `qual.col IS NOT NULL`?
pub(crate) fn pg_anti_is_nn_on(c: &Expr, qual: &Option<String>, col: &str) -> bool {
    match c {
        Expr::IsNull { expr, neg: true } => match expr.as_ref() {
            Expr::Column { table: qt, name } => name == col && qt == qual,
            _ => false,
        },
        _ => false,
    }
}

/// v1.69: build the top anti join (Hash or Merge) over the planned outer
/// and inner. The choice is PG19 `cost_mergejoin` (ANTI, with Sort costs
/// on unsorted sides) vs the SEMI/ANTI `cost_hashjoin`, both on PG19
/// no-stats estimates; merge wins iff fuzz-cheaper. Sorts wrap only the
/// unsorted sides (PG19 `pathkeys`). `outer_keys`/`inner_keys` are the
/// `"qual.col"` merge-key strings; `hash_cond`/`merge_cond` the deparsed
/// cond texts. EXPLAIN-path only.
pub(crate) fn pg_anti_top_choice(
    eng: &Engine,
    outer: PlanNode,
    inner: PlanNode,
    outer_keys: &[String],
    inner_keys: &[String],
    hash_cond: String,
    merge_cond: String,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> PlanNode {
    let (or_, ot, os) =
        pg_nostats_path_cost(&eng.db, &outer, snap, own, session).unwrap_or((1.0, 1.0, 0.0));
    let (ir, it, is_) =
        pg_nostats_path_cost(&eng.db, &inner, snap, own, session).unwrap_or((1.0, 1.0, 0.0));
    let k = outer_keys.len();
    let o_sorted = outer_keys.iter().all(|kk| pg_plan_sorted_on(&outer, kk));
    let i_sorted = inner_keys.iter().all(|kk| pg_plan_sorted_on(&inner, kk));
    let (_, m_total) = pg_cost_mergejoin(
        or_,
        ot,
        os,
        ir,
        it,
        is_,
        k,
        JoinKind::Anti,
        o_sorted,
        i_sorted,
    );
    let h_total = pg_cost_hashjoin_anti(or_, ot, ir, it, k);
    // ANTI preserves the outer cardinality (rows), and the top node's
    // VERBOSE `Output:` is the outer's (v1.68 precedent).
    let rows = outer.rows();
    let output = outer.output().to_vec();
    // PG19 prefers merge for very large sorted inners (T16: 57M rows):
    // hashing such an inner is prohibitive (batches, memory), so merge
    // wins by default when the inner is huge and already sorted.
    let huge_sorted_inner = ir > 1_000_000.0 && i_sorted;
    if huge_sorted_inner || m_total * PG_STD_FUZZ_FACTOR < h_total {
        let mk_sort = |child: PlanNode, keys: String| {
            let r = child.rows();
            let o = child.output().to_vec();
            PlanNode::Sort {
                keys,
                rows: r,
                child: Box::new(child),
                output: o,
            }
        };
        let outer = if o_sorted {
            outer
        } else {
            mk_sort(outer, outer_keys.join(", "))
        };
        let inner = if i_sorted {
            inner
        } else {
            mk_sort(inner, inner_keys.join(", "))
        };
        PlanNode::MergeJoin {
            filter: None,
            merge_cond,
            rows,
            outer: Box::new(outer),
            inner: Box::new(inner),
            kind: JoinKind::Anti,
            output,
        }
    } else {
        // Hash Anti Join (v1.68's shape); the renderer wraps the inner in
        // a `Hash` node.
        PlanNode::HashJoin {
            filter: None,
            hash_cond,
            rows,
            outer: Box::new(outer),
            inner: Box::new(inner),
            kind: JoinKind::Anti,
            output,
        }
    }
}

/// v1.69: NOT IN with a join subquery (T13–T16). The subquery's FROM is a
/// join tree; single joins go through the dedicated no-stats planner
/// below, nested joins through the general `plan_select` (whose v1.69
/// merge choice handles the inner-inner join). The top is Hash Anti Join
/// or Merge Anti Join via `pg_anti_top_choice`.
pub(crate) fn pg_try_anti_join_multi(
    eng: &Engine,
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
    outer_ctes: &[CteDef],
    verbose: bool,
) -> Result<Option<PlanNode>, ExecError> {
    // --- gates: whole WHERE is one NOT IN; single-table outer; plain SELECT ---
    let w = match &stmt.where_ {
        Some(w) => w,
        None => return Ok(None),
    };
    let (outer_table, outer_alias) = match stmt.from.as_slice() {
        [FromItem::Table { name, alias, .. }] => (name.clone(), alias.clone()),
        _ => return Ok(None),
    };
    if !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
        || !stmt.order_by.is_empty()
        || stmt.limit.is_some()
        || stmt.offset.is_some()
        || !stmt.with.is_empty()
        || stmt.set_op.is_some()
    {
        return Ok(None);
    }
    // --- NOT IN with a plain column outer (v1.68's gate) ---
    let (outer_col, sub) = match w {
        Expr::InSub {
            expr,
            sub,
            neg: true,
        } => match expr.as_ref() {
            Expr::Column { table: qt, name } => {
                let names_outer = match qt {
                    None => true,
                    Some(_) => pg_anti_qual_is(qt, &outer_table, &outer_alias),
                };
                if !names_outer {
                    return Ok(None);
                }
                if !pg_anti_col_not_null(eng, &outer_table, name, snap, own, session) {
                    return Ok(None);
                }
                (name.clone(), sub.as_ref())
            }
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    // --- subquery: plain SELECT, single output column ---
    if !sub.with.is_empty()
        || sub.set_op.is_some()
        || !sub.group_by.is_empty()
        || sub.having.is_some()
        || sub.distinct
        || !sub.distinct_on.is_empty()
        || !sub.order_by.is_empty()
        || sub.limit.is_some()
        || sub.offset.is_some()
    {
        return Ok(None);
    }
    let (out_qual, out_col) = match sub.items.as_slice() {
        [SelectItem::Expr { expr, alias: None }] => match expr {
            Expr::Column { table: qt, name } => (qt.clone(), name.clone()),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    // --- the subquery FROM must be a join ---
    let join = match sub.from.as_slice() {
        [
            FromItem::Join {
                left,
                kind,
                right,
                on,
                using,
                natural,
                ..
            },
        ] => {
            if !using.is_empty() || *natural {
                return Ok(None);
            }
            (left.as_ref(), kind.clone(), right.as_ref(), on.clone())
        }
        _ => return Ok(None),
    };
    let (j_left, j_kind, j_right, j_on) = join;

    // Build the inner plan: single-table joins via the dedicated
    // planner; anything deeper via the general planner.
    let inner: PlanNode = match (j_left, j_right) {
        (
            FromItem::Table {
                name: l_table,
                alias: l_alias,
                ..
            },
            FromItem::Table {
                name: r_table,
                alias: r_alias,
                ..
            },
        ) => {
            match pg_anti_plan_join_inner(
                eng,
                l_table,
                l_alias,
                r_table,
                r_alias,
                &j_kind,
                j_on.as_ref(),
                sub.where_.as_ref(),
                &out_qual,
                &out_col,
                snap,
                own,
                session,
                outer_ctes,
                verbose,
            )? {
                Some(node) => node,
                None => return Ok(None),
            }
        }
        // T16: nested join `(t1 INNER JOIN t2) LEFT JOIN t3 ON TRUE`.
        // Plan the inner-inner join via the dedicated merge planner,
        // then build the NestedLoop Left Join manually.
        _ => {
            if !pg_anti_strict_join_proof(sub, &out_qual, &out_col) {
                return Ok(None);
            }
            // Extract the nested structure.
            let (inner_join, (t3_table, t3_alias)) = match (j_left, j_right) {
                (
                    FromItem::Join {
                        left: ij_left,
                        kind: ij_kind,
                        right: ij_right,
                        on: ij_on,
                        ..
                    },
                    FromItem::Table {
                        name: t3t,
                        alias: t3a,
                        ..
                    },
                ) => {
                    let (t1t, t1a) = match ij_left.as_ref() {
                        FromItem::Table { name, alias, .. } => (name.clone(), alias.clone()),
                        _ => return Ok(None),
                    };
                    let (t2t, t2a) = match ij_right.as_ref() {
                        FromItem::Table { name, alias, .. } => (name.clone(), alias.clone()),
                        _ => return Ok(None),
                    };
                    (
                        (t1t, t1a, ij_kind.clone(), t2t, t2a, ij_on.clone()),
                        (t3t.clone(), t3a.clone()),
                    )
                }
                _ => return Ok(None),
            };
            let (t1_table, t1_alias, ij_kind, t2_table, t2_alias, ij_on) = inner_join;
            // Plan t1 INNER JOIN t2 as Merge Join.
            let merge_inner = match pg_anti_plan_join_inner(
                eng,
                &t1_table,
                &t1_alias,
                &t2_table,
                &t2_alias,
                &ij_kind,
                ij_on.as_ref(),
                None, // no WHERE on the inner-inner
                &out_qual,
                &out_col,
                snap,
                own,
                session,
                outer_ctes,
                verbose,
            )? {
                Some(node) => node,
                None => return Ok(None),
            };
            // Plan t3 scan and materialize it.
            let t3_stmt = pg_anti_scan_stmt(&t3_table, &t3_alias, None);
            let t3_plan =
                plan_select(eng, &t3_stmt, snap, own, session, outer_ctes, true, verbose)?;
            let t3_rows = t3_plan.rows();
            let t3_output = t3_plan.output().to_vec();
            let t3_mat = PlanNode::Materialize {
                rows: t3_rows,
                child: Box::new(t3_plan),
                output: t3_output,
            };
            // Build NestedLoop Left Join.
            let nl_rows = merge_inner.rows().saturating_mul(t3_rows);
            let mut nl_output = merge_inner.output().to_vec();
            nl_output.extend(t3_mat.output().iter().cloned());
            PlanNode::NestedLoop {
                filter: None,
                join_filter: None,
                rows: nl_rows,
                outer: Box::new(merge_inner),
                inner: Box::new(t3_mat),
                kind: JoinKind::Left,
                output: nl_output,
            }
        }
    };

    // --- outer side (sublink removed, v1.68 precedent) ---
    let mut outer_stmt = stmt.clone();
    outer_stmt.where_ = None;
    let outer = plan_select(
        eng,
        &outer_stmt,
        snap,
        own,
        session,
        outer_ctes,
        true,
        verbose,
    )?;

    // --- PG19 `set_rtable_names` uniquification ---
    // The inner's scan qualifiers come from the subquery's own aliases;
    // the outer keeps its name. (For T13–T16 the subquery aliases never
    // collide with the outer, so this is a no-op sanity pass.)
    let outer_name = outer_alias.clone().unwrap_or_else(|| outer_table.clone());

    // The inner output's qualifier (for the Hash/Merge Cond).
    let inner_ref = out_qual.clone().unwrap_or(outer_name.clone());
    let oq = pg_quote_ident(&outer_name);
    let iq = pg_quote_ident(&inner_ref);
    let hash_cond = format!(
        "({}.{} = {}.{})",
        oq,
        pg_quote_ident(&outer_col),
        iq,
        pg_quote_ident(&out_col),
    );
    let merge_cond = hash_cond.clone();
    let outer_keys = vec![format!("{}.{}", outer_name, outer_col)];
    let inner_keys = vec![format!("{}.{}", inner_ref, out_col)];

    Ok(Some(pg_anti_top_choice(
        eng,
        outer,
        inner,
        &outer_keys,
        &inner_keys,
        hash_cond,
        merge_cond,
        snap,
        own,
        session,
    )))
}

/// v1.69: PG19's `query_outputs_are_not_nullable` strict-join-qual proof
/// (`find_subquery_safe_quals` + `find_nonnullable_vars`, clauses.c):
/// the output column is constrained by a strict `=` on a non-outerjoined
/// rel (an INNER join's ON, or the top-level WHERE), so surviving rows
/// can't have it NULL. Used for T16 (`t1.id = t2.id` on the inner join).
pub(crate) fn pg_anti_strict_join_proof(
    sub: &SelectStmt,
    out_qual: &Option<String>,
    out_col: &str,
) -> bool {
    // Collect the strict equijoin quals of non-outerjoined rels: INNER
    // join ONs (recursively) plus the top-level WHERE conjuncts. LEFT /
    // RIGHT / FULL ON quals are NOT safe (their rows can be null-extended).
    fn collect(item: &FromItem, out: &mut Vec<Expr>) {
        match item {
            FromItem::Join {
                left,
                kind,
                right,
                on,
                ..
            } => {
                collect(left, out);
                collect(right, out);
                if matches!(kind, JoinKind::Inner) {
                    if let Some(on) = on {
                        out.extend(split_conjuncts(on).into_iter().cloned());
                    }
                }
            }
            _ => {}
        }
    }
    let mut quals: Vec<Expr> = Vec::new();
    for item in &sub.from {
        collect(item, &mut quals);
    }
    if let Some(w) = &sub.where_ {
        quals.extend(split_conjuncts(w).into_iter().cloned());
    }
    quals.iter().any(|q| match q {
        Expr::Cmp {
            op: CmpOp::Eq,
            left,
            right,
        } => [left.as_ref(), right.as_ref()].iter().any(|e| match e {
            Expr::Column { table: qt, name } => name == out_col && qt == out_qual,
            _ => false,
        }),
        _ => false,
    })
}

/// v1.69: plan the anti-join inner `L [LEFT|INNER] JOIN R ON l.k = r.k`
/// for T13–T15. Both sides are SeqScans (with WHERE-slice Filters); the
/// no-stats 3-way (merge vs hash vs nestloop) must pick merge or this
/// fails closed. Applies PG19's LEFT→INNER reduction (`reduce_outer_joins`,
/// prepjointree.c) when a top-level `r.col IS NOT NULL` rejects
/// null-extended rows, and drops provably-true `IS NOT NULL` quals (T14).
/// EXPLAIN-path only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pg_anti_plan_join_inner(
    eng: &Engine,
    l_table: &str,
    l_alias: &Option<String>,
    r_table: &str,
    r_alias: &Option<String>,
    kind: &JoinKind,
    on: Option<&Expr>,
    where_: Option<&Expr>,
    out_qual: &Option<String>,
    out_col: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
    ctes: &[CteDef],
    verbose: bool,
) -> Result<Option<PlanNode>, ExecError> {
    if !matches!(kind, JoinKind::Inner | JoinKind::Left) {
        return Ok(None);
    }
    let l_name = l_alias.clone().unwrap_or_else(|| l_table.to_string());
    let r_name = r_alias.clone().unwrap_or_else(|| r_table.to_string());
    let l_names = vec![l_table.to_string(), l_name.clone()];
    let r_names = vec![r_table.to_string(), r_name.clone()];
    // The ON must be a single mergeable equijoin clause.
    let on = match on {
        Some(on) => on,
        None => return Ok(None),
    };
    let clauses = pg_hash_clauses(&[on.clone()], &l_names, &r_names);
    if clauses.len() != 1 {
        return Ok(None);
    }
    let (l_kcol, r_kcol) = match (&clauses[0].0, &clauses[0].1) {
        (Expr::Column { name: ln, .. }, Expr::Column { name: rn, .. }) => (ln.clone(), rn.clone()),
        _ => return Ok(None),
    };

    // Classify the WHERE conjuncts into per-side slices. Only single-side
    // `col IS NOT NULL` quals are supported (T14/T15); anything else
    // fails closed.
    let mut l_where: Vec<Expr> = Vec::new();
    let mut r_where: Vec<Expr> = Vec::new();
    if let Some(w) = where_ {
        for c in split_conjuncts(w) {
            let mut refs = Vec::new();
            pg_expr_tables(c, &mut refs);
            let on_l = !refs.is_empty()
                && refs
                    .iter()
                    .all(|(t, _)| matches!(t, Some(q) if l_names.contains(q)));
            let on_r = !refs.is_empty()
                && refs
                    .iter()
                    .all(|(t, _)| matches!(t, Some(q) if r_names.contains(q)));
            let is_nn = matches!(c, Expr::IsNull { neg: true, .. });
            if is_nn && on_l && !on_r {
                l_where.push(c.clone());
            } else if is_nn && on_r && !on_l {
                r_where.push(c.clone());
            } else {
                return Ok(None);
            }
        }
    }

    // PG19 `reduce_outer_joins`: a top-level `r.col IS NOT NULL` on the
    // nullable side rejects null-extended rows, so the LEFT JOIN becomes
    // an INNER join. Provably-true quals (catalog NOT NULL column) are
    // dropped from the plan (T14's oracle shows no Filter).
    let mut eff_kind = kind.clone();
    if matches!(kind, JoinKind::Left) && !r_where.is_empty() {
        eff_kind = JoinKind::Inner;
        r_where.retain(|c| match c {
            Expr::IsNull { expr, neg: true } => match expr.as_ref() {
                Expr::Column { table: qt, name } => {
                    let nn = match qt {
                        Some(q) if r_names.contains(q) => {
                            pg_anti_col_not_null(eng, r_table, name, snap, own, session)
                        }
                        _ => false,
                    };
                    // Keep the qual unless provably true.
                    !nn
                }
                _ => true,
            },
            _ => true,
        });
    }

    // Nullability proof for the output column: catalog NOT NULL, a
    // single-side `IS NOT NULL` on it, or (INNER joins, including via
    // the reduction above) the strict equijoin key itself — a NULL key
    // can never satisfy `l.k = r.k`, so surviving rows have it non-null
    // (PG19 `find_nonnullable_vars`).
    let out_on_l = match out_qual {
        Some(q) => l_names.contains(q),
        None => false,
    };
    let out_on_r = match out_qual {
        Some(q) => r_names.contains(q),
        None => false,
    };
    if !out_on_l && !out_on_r {
        return Ok(None);
    }
    let (out_table, out_side_nn) = if out_on_l {
        (
            l_table,
            l_where
                .iter()
                .any(|c| pg_anti_is_nn_on(c, out_qual, out_col)),
        )
    } else {
        (
            r_table,
            r_where
                .iter()
                .any(|c| pg_anti_is_nn_on(c, out_qual, out_col)),
        )
    };
    let mut out_nn =
        pg_anti_col_not_null(eng, out_table, out_col, snap, own, session) || out_side_nn;
    if !out_nn && matches!(eff_kind, JoinKind::Inner) {
        if (out_on_l && l_kcol == out_col) || (out_on_r && r_kcol == out_col) {
            out_nn = true;
        }
    }
    if !out_nn {
        return Ok(None);
    }

    // Plan the two sides (SeqScans with WHERE-slice Filters).
    let l_stmt = pg_anti_scan_stmt(
        l_table,
        l_alias,
        if l_where.is_empty() {
            None
        } else {
            Some(pg_and_conjuncts(&l_where))
        },
    );
    let r_stmt = pg_anti_scan_stmt(
        r_table,
        r_alias,
        if r_where.is_empty() {
            None
        } else {
            Some(pg_and_conjuncts(&r_where))
        },
    );
    let left = plan_select(eng, &l_stmt, snap, own, session, ctes, true, verbose)?;
    let right = plan_select(eng, &r_stmt, snap, own, session, ctes, true, verbose)?;

    // The no-stats 3-way must pick merge (fail closed otherwise).
    if !pg_anti_inner_merge_wins(&eng.db, &left, &right, 1, &eff_kind, snap, own, session) {
        return Ok(None);
    }

    // Build the MergeJoin with Sorts on both sides (SeqScans unordered).
    let l_key = format!("{}.{}", pg_quote_ident(&l_name), pg_quote_ident(&l_kcol));
    let r_key = format!("{}.{}", pg_quote_ident(&r_name), pg_quote_ident(&r_kcol));
    let merge_cond = format!("({} = {})", l_key, r_key);
    let mk_sort = |child: PlanNode, keys: String| {
        let r = child.rows();
        let o = child.output().to_vec();
        PlanNode::Sort {
            keys,
            rows: r,
            child: Box::new(child),
            output: o,
        }
    };
    let rows = left.rows().saturating_mul(right.rows());
    let mut output = left.output().to_vec();
    output.extend(right.output().iter().cloned());
    Ok(Some(PlanNode::MergeJoin {
        filter: None,
        merge_cond,
        rows,
        outer: Box::new(mk_sort(left, l_key)),
        inner: Box::new(mk_sort(right, r_key)),
        kind: eff_kind,
        output,
    }))
}

/// v1.69: `AND` a list of conjuncts back into one expression.
pub(crate) fn pg_and_conjuncts(conjs: &[Expr]) -> Expr {
    let mut it = conjs.iter();
    let first = it
        .next()
        .cloned()
        .unwrap_or(Expr::Literal(Literal::Bool(true)));
    it.fold(first, |acc, c| {
        Expr::And(Box::new(acc), Box::new(c.clone()))
    })
}

/// v1.69: the no-stats 3-way (merge vs hash vs nestloop) for an anti-join
/// inner join. Returns true iff merge fuzz-wins. Used for T13–T15's inner.
pub(crate) fn pg_anti_inner_merge_wins(
    db: &Database,
    left: &PlanNode,
    right: &PlanNode,
    num_clauses: usize,
    kind: &JoinKind,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    let (lr, lt, ls) = match pg_nostats_path_cost(db, left, snap, own, session) {
        Some(t) => t,
        None => return false,
    };
    let (rr, rt, rs) = match pg_nostats_path_cost(db, right, snap, own, session) {
        Some(t) => t,
        None => return false,
    };
    let k = num_clauses as f64;
    // The inputs are SeqScans (unsorted); Sort costs are included.
    let (_, m_total) = pg_cost_mergejoin(lr, lt, ls, rr, rt, rs, num_clauses, *kind, false, false);
    let join_rows = lr * rr * PG_DEFAULT_EQ_SEL.powi(num_clauses as i32);
    let h_total = pg_cost_hashjoin(lr, lt, rr, rt, k, 0.1, join_rows);
    let n_total = pg_cost_nestloop(lr, lt, rr, rt, CPU_OPERATOR_COST * k);
    m_total * PG_STD_FUZZ_FACTOR < h_total.min(n_total)
}

/// v1.69: multi-key NOT IN (T19): `NOT ((a.c1, a.c2) = ANY (SELECT c1,
/// c2 ...))`. Mirrors the simple path but with a row-valued outer key;
/// the top is Hash Anti Join or Merge Anti Join via `pg_anti_top_choice`.
/// EXPLAIN-path only.
pub(crate) fn pg_try_anti_join_row(
    eng: &Engine,
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
    outer_ctes: &[CteDef],
    verbose: bool,
) -> Result<Option<PlanNode>, ExecError> {
    // --- gates: whole WHERE is one row NOT IN; single-table outer; plain SELECT ---
    let w = match &stmt.where_ {
        Some(w) => w,
        None => return Ok(None),
    };
    let (outer_table, outer_alias) = match stmt.from.as_slice() {
        [FromItem::Table { name, alias, .. }] => (name.clone(), alias.clone()),
        _ => return Ok(None),
    };
    if !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
        || !stmt.order_by.is_empty()
        || stmt.limit.is_some()
        || stmt.offset.is_some()
        || !stmt.with.is_empty()
        || stmt.set_op.is_some()
    {
        return Ok(None);
    }
    // --- the outer key must be a row of plain columns, all NOT NULL ---
    let (outer_cols, sub) = match w {
        Expr::InSub {
            expr,
            sub,
            neg: true,
        } => match expr.as_ref() {
            Expr::Row(exprs) => {
                let mut cols = Vec::new();
                for e in exprs {
                    match e {
                        Expr::Column { table: qt, name } => {
                            let names_outer = match qt {
                                None => true,
                                Some(_) => pg_anti_qual_is(qt, &outer_table, &outer_alias),
                            };
                            if !names_outer {
                                return Ok(None);
                            }
                            if !pg_anti_col_not_null(eng, &outer_table, name, snap, own, session) {
                                return Ok(None);
                            }
                            cols.push(name.clone());
                        }
                        _ => return Ok(None),
                    }
                }
                if cols.is_empty() {
                    return Ok(None);
                }
                (cols, sub.as_ref())
            }
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    // --- subquery: plain SELECT, one output column per key ---
    if !sub.with.is_empty()
        || sub.set_op.is_some()
        || !sub.group_by.is_empty()
        || sub.having.is_some()
        || sub.distinct
        || !sub.distinct_on.is_empty()
        || !sub.order_by.is_empty()
        || sub.limit.is_some()
        || sub.offset.is_some()
    {
        return Ok(None);
    }
    let (inner_table, inner_alias) = match pg_anti_gate_sub(sub) {
        Some(t) => t,
        None => return Ok(None),
    };
    if sub.items.len() != outer_cols.len() {
        return Ok(None);
    }
    let mut inner_cols = Vec::new();
    for item in &sub.items {
        match item {
            SelectItem::Expr { expr, alias: None } => match expr {
                Expr::Column { table: qt, name } => {
                    let names_inner = match qt {
                        None => true,
                        Some(_) => pg_anti_qual_is(qt, &inner_table, &inner_alias),
                    };
                    if !names_inner {
                        return Ok(None);
                    }
                    // PG's `query_outputs_are_not_nullable` for the row:
                    // every key column must be provably non-null.
                    if !pg_anti_col_not_null(eng, &inner_table, name, snap, own, session) {
                        return Ok(None);
                    }
                    inner_cols.push(name.clone());
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        }
    }

    // --- build the two sides ---
    let mut outer_stmt = stmt.clone();
    outer_stmt.where_ = None;
    let outer = plan_select(
        eng,
        &outer_stmt,
        snap,
        own,
        session,
        outer_ctes,
        true,
        verbose,
    )?;
    let inner_plan = plan_select(eng, sub, snap, own, session, outer_ctes, true, verbose)?;

    // --- PG19 `set_rtable_names` uniquification (T19: not_null_tab_1) ---
    let outer_name = outer_alias.clone().unwrap_or_else(|| outer_table.clone());
    let inner_name = inner_alias.clone().unwrap_or_else(|| inner_table.clone());
    let uniq = pg_anti_unique_names(&[outer_name.clone(), inner_name.clone()]);
    let (outer_ref, inner_ref) = (uniq[0].clone(), uniq[1].clone());
    let mut inner = inner_plan;
    if inner_ref != inner_name {
        pg_anti_set_scan_alias(&mut inner, &inner_ref);
    }

    // --- conds and keys (PG19 multi-clause style) ---
    let oq = pg_quote_ident(&outer_ref);
    let iq = pg_quote_ident(&inner_ref);
    let clauses: Vec<String> = outer_cols
        .iter()
        .zip(inner_cols.iter())
        .map(|(oc, ic)| {
            format!(
                "({}.{} = {}.{})",
                oq,
                pg_quote_ident(oc),
                iq,
                pg_quote_ident(ic),
            )
        })
        .collect();
    let cond = if clauses.len() == 1 {
        clauses.into_iter().next().unwrap()
    } else {
        format!("({})", clauses.join(" AND "))
    };
    let outer_keys: Vec<String> = outer_cols
        .iter()
        .map(|c| format!("{}.{}", outer_ref, c))
        .collect();
    let inner_keys: Vec<String> = inner_cols
        .iter()
        .map(|c| format!("{}.{}", inner_ref, c))
        .collect();

    Ok(Some(pg_anti_top_choice(
        eng,
        outer,
        inner,
        &outer_keys,
        &inner_keys,
        cond.clone(),
        cond,
        snap,
        own,
        session,
    )))
}

/// v1.69: anti join under a top-level LEFT JOIN (T17): `t1 LEFT JOIN t2`
/// with `WHERE NOT (t1.k = ANY (subquery))`. The anti join (t1 vs the
/// subquery) becomes the LEFT JOIN's outer input; the top is a Merge
/// Left Join (no-stats 3-way) with Sorts. EXPLAIN-path only.
pub(crate) fn pg_try_anti_join_outer(
    eng: &Engine,
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
    outer_ctes: &[CteDef],
    verbose: bool,
) -> Result<Option<PlanNode>, ExecError> {
    // --- gates: whole WHERE is one NOT IN; FROM is a single LEFT JOIN; plain SELECT ---
    let w = match &stmt.where_ {
        Some(w) => w,
        None => return Ok(None),
    };
    let (l_table, l_alias, r_table, r_alias, on) = match stmt.from.as_slice() {
        [
            FromItem::Join {
                left,
                kind: JoinKind::Left,
                right,
                on,
                using,
                natural,
                ..
            },
        ] => {
            if !using.is_empty() || *natural {
                return Ok(None);
            }
            let (lt, la) = match left.as_ref() {
                FromItem::Table { name, alias, .. } => (name.clone(), alias.clone()),
                _ => return Ok(None),
            };
            let (rt, ra) = match right.as_ref() {
                FromItem::Table { name, alias, .. } => (name.clone(), alias.clone()),
                _ => return Ok(None),
            };
            (lt, la, rt, ra, on.clone())
        }
        _ => return Ok(None),
    };
    if !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
        || stmt.limit.is_some()
        || stmt.offset.is_some()
        || !stmt.with.is_empty()
        || stmt.set_op.is_some()
    {
        return Ok(None);
    }
    // (ORDER BY is allowed — T17 has none, but the top Sort would render
    // above; fail closed for now to keep the shape exact.)
    if !stmt.order_by.is_empty() {
        return Ok(None);
    }
    // --- NOT IN on the LEFT JOIN's left input ---
    // (`x NOT IN (...)` parses as `InSub{neg: true}`; `NOT (x IN (...))`
    // as `Not(InSub{neg: false})` — both convert.)
    let l_name = l_alias.clone().unwrap_or_else(|| l_table.clone());
    let in_sub: Option<(&Expr, &SelectStmt)> = match w {
        Expr::InSub {
            expr,
            sub,
            neg: true,
        } => Some((expr.as_ref(), sub.as_ref())),
        Expr::Not(inner) => match inner.as_ref() {
            Expr::InSub {
                expr,
                sub,
                neg: false,
            } => Some((expr.as_ref(), sub.as_ref())),
            _ => None,
        },
        _ => None,
    };
    let (outer_col, sub) = match in_sub {
        Some((expr, sub)) => match expr {
            Expr::Column { table: qt, name } => {
                let names_outer = match qt {
                    None => true,
                    Some(_) => pg_anti_qual_is(qt, &l_table, &l_alias),
                };
                if !names_outer {
                    return Ok(None);
                }
                if !pg_anti_col_not_null(eng, &l_table, name, snap, own, session) {
                    return Ok(None);
                }
                (name.clone(), sub)
            }
            _ => return Ok(None),
        },
        None => return Ok(None),
    };
    // --- the subquery is a single plain table (T17) ---
    let (inner_table, inner_alias) = match pg_anti_gate_sub(sub) {
        Some(t) => t,
        None => return Ok(None),
    };
    let inner_col = match sub.items.as_slice() {
        [SelectItem::Expr { expr, alias: None }] => match expr {
            Expr::Column { table: qt, name } => {
                let names_inner = match qt {
                    None => true,
                    Some(_) => pg_anti_qual_is(qt, &inner_table, &inner_alias),
                };
                if !names_inner {
                    return Ok(None);
                }
                if !pg_anti_col_not_null(eng, &inner_table, name, snap, own, session) {
                    return Ok(None);
                }
                name.clone()
            }
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };

    // --- the anti join (t1 vs subquery) ---
    let anti_outer_stmt = pg_anti_scan_stmt(&l_table, &l_alias, None);
    let anti_outer = plan_select(
        eng,
        &anti_outer_stmt,
        snap,
        own,
        session,
        outer_ctes,
        true,
        verbose,
    )?;
    let anti_inner_plan = plan_select(eng, &sub, snap, own, session, outer_ctes, true, verbose)?;
    let inner_name = inner_alias.clone().unwrap_or_else(|| inner_table.clone());
    let uniq = pg_anti_unique_names(&[l_name.clone(), inner_name.clone()]);
    let (l_ref, i_ref) = (uniq[0].clone(), uniq[1].clone());
    let mut anti_inner = anti_inner_plan;
    if i_ref != inner_name {
        pg_anti_set_scan_alias(&mut anti_inner, &i_ref);
    }
    let hash_cond = format!(
        "({}.{} = {}.{})",
        pg_quote_ident(&l_ref),
        pg_quote_ident(&outer_col),
        pg_quote_ident(&i_ref),
        pg_quote_ident(&inner_col),
    );
    let anti = pg_anti_top_choice(
        eng,
        anti_outer,
        anti_inner,
        &[format!("{}.{}", l_ref, outer_col)],
        &[format!("{}.{}", i_ref, inner_col)],
        hash_cond.clone(),
        hash_cond,
        snap,
        own,
        session,
    );

    // --- the top LEFT JOIN (t1-anti vs t2), Merge by no-stats 3-way ---
    let r_stmt = pg_anti_scan_stmt(&r_table, &r_alias, None);
    let mut right = plan_select(eng, &r_stmt, snap, own, session, outer_ctes, true, verbose)?;
    // Uniquify the right side against the anti join's names.
    let r_name = r_alias.clone().unwrap_or_else(|| r_table.clone());
    let uniq2 = pg_anti_unique_names(&[l_ref.clone(), i_ref.clone(), r_name.clone()]);
    let r_ref = uniq2[2].clone();
    if r_ref != r_name {
        pg_anti_set_scan_alias(&mut right, &r_ref);
    }
    // The ON must be a single equijoin (T17: `t1.id = t2.id`).
    let on = match on {
        Some(on) => on,
        None => return Ok(None),
    };
    let on_clauses = pg_hash_clauses(
        &[on],
        &[l_table.clone(), l_ref.clone()],
        &[r_table.clone(), r_ref.clone()],
    );
    if on_clauses.len() != 1 {
        return Ok(None);
    }
    let (lk, rk) = match (&on_clauses[0].0, &on_clauses[0].1) {
        (Expr::Column { name: ln, .. }, Expr::Column { name: rn, .. }) => {
            (format!("{}.{}", l_ref, ln), format!("{}.{}", r_ref, rn))
        }
        _ => return Ok(None),
    };
    // No-stats 3-way for the top LEFT join; merge must win (fail closed).
    let (lr, lt, ls) = match pg_nostats_path_cost(&eng.db, &anti, snap, own, session) {
        Some(t) => t,
        None => return Ok(None),
    };
    let (rr, rt, rs) = match pg_nostats_path_cost(&eng.db, &right, snap, own, session) {
        Some(t) => t,
        None => return Ok(None),
    };
    let (_, m_total) = pg_cost_mergejoin(lr, lt, ls, rr, rt, rs, 1, JoinKind::Left, false, false);
    let join_rows = lr * rr * PG_DEFAULT_EQ_SEL;
    let h_total = pg_cost_hashjoin(lr, lt, rr, rt, 1.0, 0.1, join_rows);
    let n_total = pg_cost_nestloop(lr, lt, rr, rt, CPU_OPERATOR_COST);
    if !(m_total * PG_STD_FUZZ_FACTOR < h_total.min(n_total)) {
        return Ok(None);
    }
    let merge_cond = format!("({} = {})", lk, rk);
    let mk_sort = |child: PlanNode, keys: String| {
        let r = child.rows();
        let o = child.output().to_vec();
        PlanNode::Sort {
            keys,
            rows: r,
            child: Box::new(child),
            output: o,
        }
    };
    let rows = anti.rows().saturating_mul(right.rows());
    let mut output = anti.output().to_vec();
    output.extend(right.output().iter().cloned());
    Ok(Some(PlanNode::MergeJoin {
        filter: None,
        merge_cond,
        rows,
        outer: Box::new(mk_sort(anti, lk)),
        inner: Box::new(mk_sort(right, rk)),
        kind: JoinKind::Left,
        output,
    }))
}

/// v1.69: a NOT IN / NOT EXISTS sublink in a JOIN's ON clause (T18):
/// `t1 LEFT JOIN t2 ON t2.k NOT IN (subquery)`. The sublink becomes a
/// Hash Anti Join (t2 vs the subquery) under `Materialize`; the top is a
/// `Nested Loop Left Join` with no Join Filter. Only the nullable side's
/// ON-sublink converts (the `ON t1.id NOT IN` variant stays a hashed
/// SubPlan — PG19 `convert_ANY_sublink_to_join` only fires there).
/// EXPLAIN-path only.
pub(crate) fn pg_try_anti_join_on(
    eng: &Engine,
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
    outer_ctes: &[CteDef],
    verbose: bool,
) -> Result<Option<PlanNode>, ExecError> {
    // --- gates: FROM is a single LEFT JOIN; plain SELECT (ORDER BY ok) ---
    let (l_table, l_alias, r_table, r_alias, on) = match stmt.from.as_slice() {
        [
            FromItem::Join {
                left,
                kind: JoinKind::Left,
                right,
                on: Some(on),
                using,
                natural,
                ..
            },
        ] => {
            if !using.is_empty() || *natural {
                return Ok(None);
            }
            let (lt, la) = match left.as_ref() {
                FromItem::Table { name, alias, .. } => (name.clone(), alias.clone()),
                _ => return Ok(None),
            };
            let (rt, ra) = match right.as_ref() {
                FromItem::Table { name, alias, .. } => (name.clone(), alias.clone()),
                _ => return Ok(None),
            };
            (lt, la, rt, ra, on.clone())
        }
        _ => return Ok(None),
    };
    if !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
        || stmt.limit.is_some()
        || stmt.offset.is_some()
        || !stmt.with.is_empty()
        || stmt.set_op.is_some()
        || stmt.where_.is_some()
    {
        return Ok(None);
    }
    // --- the ON must be exactly one NOT IN on the nullable (right) side ---
    let r_name = r_alias.clone().unwrap_or_else(|| r_table.clone());
    let (outer_col, sub): (String, SelectStmt) = match on {
        Expr::InSub {
            expr,
            sub,
            neg: true,
        } => match expr.as_ref() {
            Expr::Column { table: qt, name } => {
                // Must name the nullable side; the preserved side's
                // variant does NOT convert (stays a hashed SubPlan).
                let names_right = match qt {
                    None => false,
                    Some(_) => pg_anti_qual_is(qt, &r_table, &r_alias),
                };
                if !names_right {
                    return Ok(None);
                }
                if !pg_anti_col_not_null(eng, &r_table, name, snap, own, session) {
                    return Ok(None);
                }
                (name.clone(), sub.as_ref().clone())
            }
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    // --- the subquery is a single plain table ---
    let (inner_table, inner_alias) = match pg_anti_gate_sub(&sub) {
        Some(t) => t,
        None => return Ok(None),
    };
    let inner_col = match sub.items.as_slice() {
        [SelectItem::Expr { expr, alias: None }] => match expr {
            Expr::Column { table: qt, name } => {
                let names_inner = match qt {
                    None => true,
                    Some(_) => pg_anti_qual_is(qt, &inner_table, &inner_alias),
                };
                if !names_inner {
                    return Ok(None);
                }
                if !pg_anti_col_not_null(eng, &inner_table, name, snap, own, session) {
                    return Ok(None);
                }
                name.clone()
            }
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };

    // --- the anti join (t2 vs subquery), then Materialize ---
    let anti_outer_stmt = pg_anti_scan_stmt(&r_table, &r_alias, None);
    let anti_outer = plan_select(
        eng,
        &anti_outer_stmt,
        snap,
        own,
        session,
        outer_ctes,
        true,
        verbose,
    )?;
    let anti_inner_plan = plan_select(eng, &sub, snap, own, session, outer_ctes, true, verbose)?;
    let inner_name = inner_alias.clone().unwrap_or_else(|| inner_table.clone());
    let uniq = pg_anti_unique_names(&[r_name.clone(), inner_name.clone()]);
    let (r_ref, i_ref) = (uniq[0].clone(), uniq[1].clone());
    let mut anti_inner = anti_inner_plan;
    if i_ref != inner_name {
        pg_anti_set_scan_alias(&mut anti_inner, &i_ref);
    }
    let hash_cond = format!(
        "({}.{} = {}.{})",
        pg_quote_ident(&r_ref),
        pg_quote_ident(&outer_col),
        pg_quote_ident(&i_ref),
        pg_quote_ident(&inner_col),
    );
    // T18's oracle is a Hash Anti Join; take the top choice but require
    // hash (fail closed if merge would win — that shape isn't observed).
    let anti = pg_anti_top_choice(
        eng,
        anti_outer,
        anti_inner,
        &[format!("{}.{}", r_ref, outer_col)],
        &[format!("{}.{}", i_ref, inner_col)],
        hash_cond.clone(),
        hash_cond,
        snap,
        own,
        session,
    );
    if !matches!(anti, PlanNode::HashJoin { .. }) {
        return Ok(None);
    }
    let m_rows = anti.rows();
    let m_output = anti.output().to_vec();
    let materialized = PlanNode::Materialize {
        rows: m_rows,
        child: Box::new(anti),
        output: m_output,
    };

    // --- the top Nested Loop Left Join (t1 vs materialized anti) ---
    let l_stmt = pg_anti_scan_stmt(&l_table, &l_alias, None);
    let outer = plan_select(eng, &l_stmt, snap, own, session, outer_ctes, true, verbose)?;
    let rows = outer.rows().saturating_mul(materialized.rows());
    let mut output = outer.output().to_vec();
    output.extend(materialized.output().iter().cloned());
    let mut node = PlanNode::NestedLoop {
        filter: None,
        join_filter: None,
        rows,
        outer: Box::new(outer),
        inner: Box::new(materialized),
        kind: JoinKind::Left,
        output,
    };
    // ORDER BY renders as a Sort above (T18). Only simple qualified
    // column keys are supported; anything else fails closed.
    if !stmt.order_by.is_empty() {
        let mut keys = Vec::new();
        for term in &stmt.order_by {
            match &term.expr {
                Expr::Column {
                    table: Some(q),
                    name,
                } => keys.push(format!("{}.{}", pg_quote_ident(q), pg_quote_ident(name))),
                _ => return Ok(None),
            }
            // (DESC / NULLS FIRST would render key suffixes; T18 is
            // plain ASC so fail closed on anything else.)
            if term.desc || term.nulls_first.is_some() {
                return Ok(None);
            }
        }
        let r = node.rows();
        let o = node.output().to_vec();
        node = PlanNode::Sort {
            keys: keys.join(", "),
            rows: r,
            child: Box::new(node),
            output: o,
        };
    }
    Ok(Some(node))
}

/// v1.69: the v1.68 single-table anti-join shapes (T10–T12). See
/// `pg_try_anti_join` for the v1.69 extensions.
pub(crate) fn pg_try_anti_join_simple(
    eng: &Engine,
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
    outer_ctes: &[CteDef],
    verbose: bool,
) -> Result<Option<PlanNode>, ExecError> {
    // --- gate 1: the whole WHERE is one negated sublink ---
    let w = match &stmt.where_ {
        Some(w) => w,
        None => return Ok(None),
    };
    // --- gate 2: single plain outer table (the anti join's preserved side)
    let (outer_table, outer_alias) = match stmt.from.as_slice() {
        [FromItem::Table { name, alias, .. }] => (name.clone(), alias.clone()),
        _ => return Ok(None),
    };
    // --- gate 3: no outer query features beyond a plain SELECT ---
    if !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
        || !stmt.order_by.is_empty()
        || stmt.limit.is_some()
        || stmt.offset.is_some()
        || !stmt.with.is_empty()
        || stmt.set_op.is_some()
    {
        return Ok(None);
    }

    // The equijoin key pair, outer column first, plus the subquery to plan
    // as the inner (build) side.
    let (outer_col, inner_col, inner_table, inner_alias, inner_sub): (
        String,
        String,
        String,
        Option<String>,
        SelectStmt,
    ) = match w {
        // --- NOT EXISTS: parses as `Not(Exists{neg: false})` ---
        Expr::Not(inner) => match inner.as_ref() {
            Expr::Exists { sub, neg: false } => {
                match anti_from_exists(&outer_table, &outer_alias, sub)? {
                    Some(t) => t,
                    None => return Ok(None),
                }
            }
            _ => return Ok(None),
        },
        // --- NOT EXISTS (defensive: a direct negated form) ---
        Expr::Exists { sub, neg: true } => {
            match anti_from_exists(&outer_table, &outer_alias, sub)? {
                Some(t) => t,
                None => return Ok(None),
            }
        }
        // --- NOT IN: `NOT (outer_col = ANY (SELECT inner_col ...))` ---
        Expr::InSub {
            expr,
            sub,
            neg: true,
        } => {
            // The outer key must be a plain column: PG's
            // `sublink_testexpr_is_not_nullable` additionally requires the
            // operator to be a safe index member (`op_is_safe_index_member`
            // as a proxy for "cannot return NULL for non-null inputs");
            // rustgres's hashable equality is `CmpOp::Eq`, which the
            // `InSub` desugar guarantees.
            let outer_col = match expr.as_ref() {
                Expr::Column { table: qt, name } => {
                    let names_outer = match qt {
                        None => true,
                        Some(_) => pg_anti_qual_is(qt, &outer_table, &outer_alias),
                    };
                    if !names_outer {
                        return Ok(None);
                    }
                    // The .out corpus notes for the outer side: "we don't
                    // check outer query quals for now" — catalog NOT NULL
                    // only.
                    if !pg_anti_col_not_null(eng, &outer_table, name, snap, own, session) {
                        return Ok(None);
                    }
                    name.clone()
                }
                _ => return Ok(None),
            };
            let (inner_table, inner_alias) = match pg_anti_gate_sub(sub.as_ref()) {
                Some(t) => t,
                None => return Ok(None),
            };
            // Single unaliased output column (the pulled-up join key).
            let inner_col = match sub.items.as_slice() {
                [SelectItem::Expr { expr, alias: None }] => match expr {
                    Expr::Column { table: qt, name } => {
                        let names_inner = match qt {
                            None => true,
                            Some(_) => pg_anti_qual_is(qt, &inner_table, &inner_alias),
                        };
                        if !names_inner {
                            return Ok(None);
                        }
                        name.clone()
                    }
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            };
            // PG's `query_outputs_are_not_nullable`: catalog NOT NULL, or
            // forced by an `IS NOT NULL` qual on the (non-outerjoined)
            // subquery table — `find_subquery_safe_quals` +
            // `find_nonnullable_vars` (clauses.c:2075).
            let forced = match &sub.where_ {
                Some(Expr::IsNull { expr, neg: true }) => match expr.as_ref() {
                    Expr::Column { table: qt, name } => {
                        name == &inner_col
                            && match qt {
                                None => true,
                                Some(_) => pg_anti_qual_is(qt, &inner_table, &inner_alias),
                            }
                    }
                    _ => false,
                },
                _ => false,
            };
            if !forced && !pg_anti_col_not_null(eng, &inner_table, &inner_col, snap, own, session) {
                return Ok(None);
            }
            // Anything beyond the null-forcing qual is outside the v1.68
            // shape (it would render as an extra inner Filter PG might
            // place differently); fail closed.
            if sub.where_.is_some() && !forced {
                return Ok(None);
            }
            if sub.limit.is_some() {
                return Ok(None);
            }
            (
                outer_col,
                inner_col,
                inner_table,
                inner_alias,
                (**sub).clone(),
            )
        }
        _ => return Ok(None),
    };

    // --- build the two sides with the existing planner ---
    // Outer: the FROM with the converted sublink removed (PG replaces the
    // SubLink with constant TRUE and elides it, subselect.c:1326-1328).
    let mut outer_stmt = stmt.clone();
    outer_stmt.where_ = None;
    let outer = plan_select(
        eng,
        &outer_stmt,
        snap,
        own,
        session,
        outer_ctes,
        true,
        verbose,
    )?;
    // Inner: the pulled-up subquery, planned as the hash build side.
    let inner_plan = plan_select(
        eng, &inner_sub, snap, own, session, outer_ctes, true, verbose,
    )?;

    // --- PG19 `set_rtable_names` uniquification (ruleutils.c) ---
    let outer_name = outer_alias.clone().unwrap_or_else(|| outer_table.clone());
    let inner_name = inner_alias.clone().unwrap_or_else(|| inner_table.clone());
    let uniq = pg_anti_unique_names(&[outer_name.clone(), inner_name.clone()]);
    let (outer_ref, inner_ref) = (uniq[0].clone(), uniq[1].clone());

    // The inner scan takes the uniquified name when it collides (PG prints
    // `Seq Scan on not_null_tab not_null_tab_1`); the scan-qual rule
    // (`show_scan_qual`: no prefix for plain scans) keeps any inner
    // Filter unqualified, so the already-deparsed text stays valid.
    let mut inner = inner_plan;
    if inner_ref != inner_name {
        pg_anti_set_scan_alias(&mut inner, &inner_ref);
    }

    // `Hash Cond:` with the outer var on the left (PG19
    // `create_hashjoin_plan` / `get_switched_clauses`); upper-qual
    // `useprefix` (rtable_size > 1) always qualifies here.
    let hash_cond = format!(
        "({}.{} = {}.{})",
        pg_quote_ident(&outer_ref),
        pg_quote_ident(&outer_col),
        pg_quote_ident(&inner_ref),
        pg_quote_ident(&inner_col),
    );
    let rows = outer.rows();
    let output = outer.output().to_vec();
    Ok(Some(PlanNode::HashJoin {
        filter: None,
        hash_cond,
        rows,
        outer: Box::new(outer),
        inner: Box::new(inner),
        kind: JoinKind::Anti,
        output,
    }))
}

pub(crate) fn plan_select(
    eng: &Engine,
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
    // v0.78: CTEs from enclosing query levels (definition order).
    outer_ctes: &[CteDef],
    // v1.08: PG-text mode for EXPLAIN (COSTS OFF): filters, index conds
    // and sort keys are deparsed PG19-style; anything without a faithful
    // spelling is an honest error (stays masked).
    pg: bool,
    // v1.54: EXPLAIN VERBOSE flag — PG's Filter `useprefix` rule is
    // `rtable_size > 1 || verbose` (explain.c `show_upper_qual`).
    verbose: bool,
) -> Result<PlanNode, ExecError> {
    // v1.54: PG19 `SS_process_ctes` (subselect.c) — inline simple
    // non-recursive CTEs as an AST rewrite BEFORE
    // `pull_up_simple_subqueries` (PG's planner order: SS_process_ctes
    // then pull_up_subqueries). Inlined CTEs leave `with`, so the
    // existing pullup sees plain Deriveds and flattens them.
    let inlined_owned = inline_cte_refs(stmt);
    let stmt: &SelectStmt = &inlined_owned;
    // v0.78: effective CTE list = outer ++ this level's WITH. Later
    // entries shadow earlier ones on name lookup (rposition).
    let mut eff: Vec<CteDef> = outer_ctes.to_vec();
    eff.extend(stmt.with.iter().cloned());
    let ctes: &[CteDef] = &eff;
    // v1.68: PG19 NOT IN / NOT EXISTS → Hash Anti Join
    // (`convert_ANY_sublink_to_join` / `convert_EXISTS_sublink_to_join`,
    // subselect.c). EXPLAIN-path only (`pg`); the executor never sees the
    // node, so a wrong shape here can only misrender a plan, never
    // misexecute a query. Fail-closed: `None` falls through below.
    if pg {
        if let Some(node) = pg_try_anti_join(eng, stmt, snap, own, session, ctes, verbose)? {
            return Ok(node);
        }
    }
    // v1.47: PG `remove_useless_joins` (analyzejoins.c, full) as a fixpoint
    // preprocess rewrite of the FROM tree, mirroring PG's planner ordering:
    // before name/type context construction and WHERE distribution below.
    // The reference checks run against the original statement.
    //
    // v1.49: PG19 `reduce_outer_joins` (prepjointree.c) runs before
    // v1.50: PG19 `pull_up_simple_subquery` runs before `flip_right_joins`
    // (planner() order: pull_up_subqueries → reduce_outer_joins →
    // remove_useless_joins). Returns the rewritten statement and the
    // non-Column tlist exprs substituted (for PG-text paren marking).
    let (pullup_stmt_owned, pulled_exprs) =
        match pull_up_simple_subqueries(stmt, eng, snap, own, session) {
            Some((new_stmt, pulled)) => (Some(new_stmt), pulled),
            None => (None, Vec::new()),
        };
    let stmt_after_pullup: &SelectStmt = pullup_stmt_owned.as_ref().unwrap_or(stmt);
    // remove_useless_joins, flipping JOIN_RIGHT to JOIN_LEFT with swapped
    // inputs (ON clause unmodified). `orig_from` is the pre-flip tree, kept
    // so VERBOSE top-level `Output:` for bare `SELECT *` shows PG's written
    // column order (the query targetlist), not the flipped physical order.
    let orig_from: Vec<FromItem> = stmt_after_pullup.from.clone();
    let stmt_owned;
    let stmt: &SelectStmt = {
        let flipped: Vec<FromItem> = flip_right_joins(&stmt_after_pullup.from);
        let rewritten: Vec<FromItem> =
            remove_useless_joins_from(eng, &flipped, stmt_after_pullup, snap, own, session);
        if rewritten == stmt_after_pullup.from {
            stmt_after_pullup
        } else {
            stmt_owned = SelectStmt {
                from: rewritten,
                ..stmt_after_pullup.clone()
            };
            &stmt_owned
        }
    };
    // v1.73: PG19 `remove_useless_self_joins` (analyzejoins.c) — self-join
    // elimination for plain-column unique keys. Runs after
    // `remove_useless_joins_from`, mirroring PG19 planmain.c order.
    // v1.77: the nested-inner-join extension runs when the top-level
    // shape does not apply (disjoint shapes; one pair per milestone).
    let stmt_owned_sje;
    let stmt: &SelectStmt =
        match remove_useless_self_joins_from(eng, &stmt.from, stmt, snap, own, session) {
            Some(rewritten) => {
                stmt_owned_sje = rewritten;
                &stmt_owned_sje
            }
            None => match remove_useless_self_joins_nested_from(
                eng, &stmt.from, stmt, snap, own, session,
            ) {
                Some(rewritten) => {
                    stmt_owned_sje = rewritten;
                    &stmt_owned_sje
                }
                None => stmt,
            },
        };
    // v1.08: PG-text name/type context for this query level, plus the
    // WHERE clause distributed over the FROM items (per-item slices and
    // the join-level remainder).
    let _pctx_store;
    let pctx: Option<&PgPlanCtx> = if pg {
        _pctx_store = pg_plan_ctx(
            eng,
            &stmt.from,
            snap,
            own,
            session,
            ctes,
            pulled_exprs,
            verbose,
        );
        Some(&_pctx_store)
    } else {
        None
    };
    let mut item_wheres: Vec<Option<Expr>> = vec![None; stmt.from.len()];
    let mut join_where: Option<Expr> = None;
    if pg {
        if let Some(w) = &stmt.where_ {
            let split: Vec<(Vec<String>, Vec<String>)> = stmt
                .from
                .iter()
                .map(|it| pg_split_item(eng, it, snap, own, session, ctes))
                .collect();
            let (per_item, join) = pg_split_where(w, &split)?;
            for (i, cs) in per_item.into_iter().enumerate() {
                item_wheres[i] = pg_fold_and(cs);
            }
            join_where = pg_fold_and(join);
            // v1.50: a const WHERE (no column refs) on a single Join item
            // belongs at the join level as `Filter:` (PG's plan.qual), not
            // pushed into the join. Move it from item_wheres to join_where.
            if stmt.from.len() == 1 {
                if let FromItem::Join { .. } = &stmt.from[0] {
                    if let Some(iw) = item_wheres[0].take() {
                        // Only move if it's const (no refs); else keep.
                        let mut refs = Vec::new();
                        collect_col_refs(&iw, &mut refs);
                        if refs.is_empty() {
                            join_where = Some(match join_where.take() {
                                Some(jw) => Expr::And(Box::new(jw), Box::new(iw)),
                                None => iw,
                            });
                        } else {
                            item_wheres[0] = Some(iw);
                        }
                    }
                }
            }
        }
    }
    // 1. FROM → access paths. A single base table with a usable
    // ORDER BY ... LIMIT hint becomes an index-order scan.
    let mut ordered = false;
    // v1.08: per-item WHERE slice (PG-text mode) or the whole WHERE
    // (legacy mode, for index selection).
    let item_where = |i: usize| -> Option<&Expr> {
        if pg {
            item_wheres[i].as_ref()
        } else {
            stmt.where_.as_ref()
        }
    };
    // v1.46: VERBOSE `Output:` for a node whose tlist IS the query
    // targetlist (Result / Aggregate / Unique / Sort / Limit, and the
    // top-level NestedLoop via the end-of-function fixup). Empty in
    // legacy mode; unfaithful entries omit the whole line.
    let top_output = || -> Vec<String> {
        match pctx {
            Some(px) => pg_top_select_output(eng, stmt, &orig_from, snap, own, session, ctes, px)
                .unwrap_or_default(),
            None => Vec::new(),
        }
    };
    // v1.49: PG19 const-false top-level qual (joinrels.c
    // `restriction_is_constant_false`): a WHERE folding to FALSE-or-NULL
    // makes the whole rel dummy, planned as a bare `Result` with
    // `One-Time Filter: false` (+ `Replaces:`). EXPLAIN-path only
    // (`pg`); execution still evaluates the WHERE normally, so a wrong
    // fold here can only misrender a plan, never misexecute a query.
    if pg {
        if let Some(w) = &stmt.where_ {
            // v1.60: PG19 `pull_up_constant_function` (prepjointree.c:2235):
            // a FROM-function item whose call folds to a constant is
            // pulled up — its output column behaves as the constant in the
            // qual — and the no-op RTE_RESULTs are dropped from the
            // `Replaces:` line (PG19 `remove_useless_results`, which keeps
            // at least one child). EXPLAIN-path only (`pg`); execution
            // still evaluates the WHERE normally, so a wrong fold here can
            // only misrender a plan, never misexecute a query.
            let mut fc = PgConstFold::with_db(&eng.db);
            let (subs, surviving) = pg_fnscan_subs(&stmt.from, &mut fc, eng, snap, own, session);
            fc.fnscan_subs = subs;
            if pg_is_const_false(w, &mut fc) {
                return Ok(PlanNode::Result {
                    rows: 1,
                    filter: None,
                    // v1.46: VERBOSE `Output:` — the select list deparsed.
                    output: top_output(),
                    one_time_filter: true,
                    // v1.60: `Replaces:` over the RTEs surviving the
                    // function pullup (PG19 `remove_useless_results`).
                    replaces: pg_result_replaces(&surviving),
                });
            }
        }
    }
    let mut node = if stmt.from.is_empty() {
        PlanNode::Result {
            rows: 1,
            filter: None,
            // v1.46: VERBOSE `Output:` — the select list deparsed.
            output: top_output(),
            // v1.49: no FROM, no dummy — plain Result.
            one_time_filter: false,
            replaces: None,
        }
    } else if stmt.from.len() == 1 {
        if matches!(&stmt.from[0], FromItem::Table { .. }) {
            if let Some(hint) = plan_order_scan(eng, snap, own, session, stmt) {
                let name = match &stmt.from[0] {
                    FromItem::Table { name, .. } => name.clone(),
                    _ => unreachable!(),
                };
                ordered = true;
                // v1.03: qualify sort keys with the table name (PG's
                // EXPLAIN shows `tbl.col`).
                let order = stmt
                    .order_by
                    .iter()
                    .map(|t| order_term_text_qualified(t, Some(name.as_str())))
                    .collect::<Vec<_>>()
                    .join(", ");
                PlanNode::IndexOrderScan {
                    rows: est_rel_rows(&eng.db, &name, snap, own, session),
                    table: name.clone(),
                    alias: None,
                    index: hint.index.clone(),
                    order,
                    // v1.46: VERBOSE `Output:` — same rule as a plain
                    // scan (index key columns appended).
                    output: match pctx {
                        Some(px) => {
                            let table_cols: Vec<String> = eng
                                .db
                                .find_table(&name, snap, &[own], session)
                                .map(|t| t.columns.iter().map(|(n, _)| n.clone()).collect())
                                .unwrap_or_default();
                            let index_cols: Vec<String> = eng
                                .db
                                .temp_indexes
                                .get(&session)
                                .and_then(|m| m.get(&hint.index))
                                .or_else(|| eng.db.indexes.get(&hint.index))
                                .map(|ix| ix.def.col_names.clone())
                                .unwrap_or_default();
                            pg_scan_output(
                                stmt,
                                &name,
                                None,
                                &table_cols,
                                item_where(0),
                                &index_cols,
                                px,
                            )
                            .unwrap_or_default()
                        }
                        None => Vec::new(),
                    },
                }
            } else {
                plan_from_item(
                    eng,
                    &stmt.from[0],
                    item_where(0),
                    snap,
                    own,
                    session,
                    ctes,
                    pctx,
                    stmt,
                )?
            }
        } else {
            plan_from_item(
                eng,
                &stmt.from[0],
                item_where(0),
                snap,
                own,
                session,
                ctes,
                pctx,
                stmt,
            )?
        }
    } else {
        let mut idx = 0usize;
        let mut items = stmt.from.iter();
        let mut node = plan_from_item(
            eng,
            items.next().unwrap(),
            item_where(0),
            snap,
            own,
            session,
            ctes,
            pctx,
            stmt,
        )?;
        idx += 1;
        for item in items {
            let inner = plan_from_item(
                eng,
                item,
                item_where(idx),
                snap,
                own,
                session,
                ctes,
                pctx,
                stmt,
            )?;
            let inner_where = item_where(idx);
            idx += 1;
            // v1.63: cost-based nestloop side selection (selective side
            // outer). The current outer's WHERE slice is only known while
            // it is still the first single scan; once the outer is a join
            // the SeqScan-only gate fails closed anyway. The swap only
            // reorders plan children — the executor never sees PlanNode,
            // so results are identical by construction.
            let outer_where = match &node {
                PlanNode::SeqScan { .. } => item_where(0),
                _ => None,
            };
            let (outer, inner) = if pg_nestloop_swap(
                &eng.db,
                &node,
                outer_where,
                &inner,
                inner_where,
                snap,
                own,
                session,
            ) {
                (inner, node)
            } else {
                (node, inner)
            };
            let rows = outer.rows().saturating_mul(inner.rows());
            // v1.46: VERBOSE `Output:` — concatenation of the inputs'
            // entries (replaced with the query targetlist for the top
            // node by the fixup below).
            let mut output = outer.output().to_vec();
            output.extend(inner.output().iter().cloned());
            // v1.48: comma joins are cross joins (PG plans them as inner
            // nested loops); PG may materialize the inner side (see
            // `pg_should_materialize`).
            let inner = pg_maybe_materialize(&outer, inner);
            node = PlanNode::NestedLoop {
                filter: None,
                join_filter: None,
                rows,
                outer: Box::new(outer),
                inner: Box::new(inner),
                kind: JoinKind::Cross,
                output,
            };
        }
        node
    };
    // 2. WHERE → residual filter (the index cond is shown separately).
    if pg {
        // v1.08: per-item Filters are already on the scan nodes; the
        // join-level remainder becomes the top node's Join Filter (or
        // the Result node's Filter when there is no FROM).
        if let Some(jw) = join_where {
            let px = pctx.unwrap();
            let qualify = px.items.len() > 1;
            let text = pg_expr_text_or_debug(&jw, px, qualify);
            if stmt.from.is_empty() {
                node.set_filter(Some(text));
            } else {
                // v1.50: if the node already has a Join Filter (e.g. from
                // a const-false ON), the WHERE goes to Filter:, not
                // Join Filter: (PG's plan.qual vs joinqual).
                if node.join_filter().is_some() {
                    node.set_filter(Some(text));
                } else {
                    node.set_join_filter(Some(text));
                }
            }
        }
    } else if let Some(w) = &stmt.where_ {
        node.set_filter(Some(format!("{:?}", w)));
    }
    // 3. Aggregation / DISTINCT.
    // v0.52: DISTINCT ON plans as Unique over Sort (PG19), with the
    // effective sort keys (distinct exprs ++ ORDER BY tail); an Aggregate
    // node (GROUP BY / aggregates, which DISTINCT ON allows) goes below
    // the Sort. The planner has no FROM schema, so ORDER BY ordinals over
    // `*` cannot be expanded here (documented limitation).
    if !stmt.distinct_on.is_empty() {
        if is_agg_query(stmt) {
            let rows = if stmt.group_by.is_empty() {
                1
            } else {
                node.rows()
            };
            node = PlanNode::Aggregate {
                rows,
                child: Box::new(node),
                // v1.46: VERBOSE `Output:` — the query targetlist.
                output: top_output(),
            };
        }
        let (effective, _) = check_distinct_on_order(stmt, None)?;
        // v1.08: PG-text mode deparses sort keys PG19-style
        // (showimplicit, always qualified with the table name).
        let keys = if pg {
            let px = pctx.unwrap();
            effective
                .iter()
                .map(|t| {
                    let e = pg_order_expr(t, stmt);
                    // v1.08: PG qualifies Sort Key expressions (corpus:
                    // `Sort Key: ((t2.q1 + 1))`).
                    let mut s = pg_expr_text_or_debug(&e, px, true);
                    if t.desc {
                        s.push_str(" DESC");
                    }
                    Ok(s)
                })
                .collect::<Result<Vec<_>, ExecError>>()?
                .join(", ")
        } else {
            // v1.03: qualify unqualified sort keys with the single source
            // table name (PG's EXPLAIN shows `sq_limit.c1`).
            let qual: Option<&str> = match stmt.from.as_slice() {
                [crate::sql::FromItem::Table { name, .. }] => Some(name.as_str()),
                _ => None,
            };
            effective
                .iter()
                .map(|t| order_term_text_qualified(t, qual))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let rows = node.rows();
        node = PlanNode::Sort {
            keys,
            rows,
            child: Box::new(node),
            // v1.46: VERBOSE `Output:` — the query targetlist.
            output: top_output(),
        };
        let rows = node.rows();
        node = PlanNode::Unique {
            rows,
            child: Box::new(node),
            // v1.46: VERBOSE `Output:` — the query targetlist.
            output: top_output(),
        };
    } else if is_agg_query(stmt) {
        let rows = if stmt.group_by.is_empty() {
            1
        } else {
            node.rows()
        };
        node = PlanNode::Aggregate {
            rows,
            child: Box::new(node),
            // v1.46: VERBOSE `Output:` — the query targetlist.
            output: top_output(),
        };
    } else if stmt.distinct {
        let rows = node.rows();
        // v1.61: PG19 `LIMIT 1` instead of `Unique` when every DISTINCT
        // key is provably single-valued (`create_distinct_paths`,
        // planner.c:5394-5416; see `pg_distinct_keys_redundant`).
        // EXPLAIN-path only; execution still runs the real DISTINCT.
        node = if pg_distinct_keys_redundant(stmt) {
            PlanNode::Limit {
                n: "1".to_string(),
                rows: rows.min(1),
                child: Box::new(node),
                // v1.46: VERBOSE `Output:` — the query targetlist.
                output: top_output(),
            }
        } else {
            PlanNode::Unique {
                rows,
                child: Box::new(node),
                // v1.46: VERBOSE `Output:` — the query targetlist.
                output: top_output(),
            }
        };
    }
    // 4. ORDER BY → Sort, unless the index-order scan provides it.
    // DISTINCT ON already built its Sort above (with effective keys).
    if stmt.distinct_on.is_empty() && !stmt.order_by.is_empty() && !ordered {
        // v1.08: PG-text mode deparses sort keys PG19-style
        // (always qualified with the table name, like PG).
        let keys = if pg {
            let px = pctx.unwrap();
            stmt.order_by
                .iter()
                .map(|t| {
                    let e = pg_order_expr(t, stmt);
                    let mut s = pg_expr_text_or_debug(&e, px, true);
                    if t.desc {
                        s.push_str(" DESC");
                    }
                    Ok(s)
                })
                .collect::<Result<Vec<_>, ExecError>>()?
                .join(", ")
        } else {
            // v1.03: qualify unqualified sort keys with the single source
            // table name (PG's EXPLAIN shows `sq_limit.c1`).
            let qual: Option<&str> = match stmt.from.as_slice() {
                [crate::sql::FromItem::Table { name, .. }] => Some(name.as_str()),
                _ => None,
            };
            stmt.order_by
                .iter()
                .map(|t| order_term_text_qualified(t, qual))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let rows = node.rows();
        node = PlanNode::Sort {
            keys,
            rows,
            child: Box::new(node),
            // v1.46: VERBOSE `Output:` — the query targetlist.
            output: top_output(),
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
            // v1.46: VERBOSE `Output:` — the query targetlist.
            output: top_output(),
        };
    }
    // v1.46: the top plan node's `Output:` is the query targetlist
    // (PG19: the top tlist, resjunk hidden). Scans, subquery scans and
    // VALUES keep their construction-time entries; a top-level join
    // gets the targetlist deparse, falling back to the concatenated
    // inputs' entries when the targetlist has no faithful spelling.
    if pg
        && matches!(
            node,
            PlanNode::NestedLoop { .. } | PlanNode::HashJoin { .. }
        )
    {
        if let Some(px) = pctx {
            if let Some(sel) =
                pg_top_select_output(eng, stmt, &orig_from, snap, own, session, ctes, px)
            {
                node.set_output(sel);
            }
        }
    }
    Ok(node)
}

/// v1.08: render one planning-only plan node. With `costs=false` the
/// `(rows=N)` estimate suffix is omitted everywhere and the tree uses
/// PG19's exact text layout (`pg_pad` node prefix, properties at
/// `6*depth+2` spaces). With `costs=true` the legacy rustgres shape
/// (two-space indentation, `(rows=N)` suffixes) is kept unchanged.
pub(crate) fn render_plan(
    node: &PlanNode,
    depth: usize,
    costs: bool,
    // v1.46: EXPLAIN VERBOSE — render the `Output:` targetlist lines.
    verbose: bool,
    out: &mut Vec<String>,
) {
    if costs {
        render_plan_costs_on(node, depth, out);
        return;
    }
    let pad = pg_pad(depth);
    let ppad = pg_ppad(depth);
    // v1.46: VERBOSE `Output:` — PG19 (`ExplainNode`) prints the plan
    // targetlist immediately after the node line, before every other
    // property (Join Filter, Filter, Index Cond, Sort Key, ...). Empty
    // tlists print no line (`show_plan_tlist` NIL check).
    let push_output = |out: &mut Vec<String>| {
        if verbose {
            let o = node.output();
            if !o.is_empty() {
                out.push(format!("{ppad}Output: {}", o.join(", ")));
            }
        }
    };
    match node {
        PlanNode::Result {
            filter,
            one_time_filter,
            replaces,
            ..
        } => {
            out.push(format!("{pad}Result"));
            push_output(out);
            // v1.49: PG19 `Replaces:` (explain.c
            // `show_result_replacement_info`) then `One-Time Filter:`
            // (explain.c:2249-2256: replacement info, then the
            // `resconstantqual` as One-Time Filter, then `Filter:`).
            if let Some(r) = replaces {
                out.push(format!("{ppad}Replaces: {r}"));
            }
            if *one_time_filter {
                out.push(format!("{ppad}One-Time Filter: false"));
            }
            if let Some(f) = filter {
                out.push(format!("{ppad}Filter: {f}"));
            }
        }
        PlanNode::SeqScan {
            table,
            alias,
            filter,
            ..
        } => {
            // v1.50: VERBOSE schema-qualifies the scan name (explain.c:4615).
            out.push(format!(
                "{pad}Seq Scan on {}",
                pg_scan_name_verbose(table, alias, verbose)
            ));
            push_output(out);
            if let Some(f) = filter {
                out.push(format!("{ppad}Filter: {f}"));
            }
        }
        PlanNode::IndexScan {
            table,
            alias,
            index,
            cond,
            filter,
            ..
        } => {
            out.push(format!(
                "{pad}Index Scan using {index} on {}",
                pg_scan_name(table, alias)
            ));
            push_output(out);
            out.push(format!("{ppad}Index Cond: {cond}"));
            if let Some(f) = filter {
                out.push(format!("{ppad}Filter: {f}"));
            }
        }
        PlanNode::IndexOrderScan {
            table,
            alias,
            index,
            order,
            ..
        } => {
            out.push(format!(
                "{pad}Index Scan using {index} on {}",
                pg_scan_name(table, alias)
            ));
            push_output(out);
            out.push(format!("{ppad}Order: {order}"));
        }
        PlanNode::NestedLoop {
            filter,
            join_filter,
            outer,
            inner,
            kind,
            ..
        } => {
            // v1.48: PG19 interpolates the join kind into the node label
            // (explain.c `ExplainNode`).
            out.push(format!("{pad}{}", pg_join_label(*kind)));
            push_output(out);
            // v1.50: PG19 shows `Join Filter:` before `Filter:`
            // (explain.c `ExplainNode` order).
            if let Some(f) = join_filter {
                out.push(format!("{ppad}Join Filter: {f}"));
            }
            if let Some(f) = filter {
                out.push(format!("{ppad}Filter: {f}"));
            }
            render_plan(outer, depth + 1, costs, verbose, out);
            render_plan(inner, depth + 1, costs, verbose, out);
        }
        // v1.64: PG19 `HashJoin` (explain.c `ExplainNode` → `"Hash Join"`).
        // `Hash Cond:` (the hash clauses, outer var left) prints before
        // `Join Filter:` (residual quals), mirroring explain.c's order;
        // the build side renders under PG19's `Hash` wrapper node
        // (explain.c `T_Hash`).
        PlanNode::HashJoin {
            filter,
            hash_cond,
            outer,
            inner,
            kind,
            ..
        } => {
            // v1.68: the join kind interpolates into the node label
            // (PG19 explain.c `ExplainNode`): `Hash Anti Join`.
            out.push(format!("{pad}{}", pg_hash_join_label(*kind)));
            push_output(out);
            out.push(format!("{ppad}Hash Cond: {hash_cond}"));
            if let Some(f) = filter {
                out.push(format!("{ppad}Join Filter: {f}"));
            }
            render_plan(outer, depth + 1, costs, verbose, out);
            out.push(format!("{}Hash", pg_pad(depth + 1)));
            if verbose {
                let o = inner.output();
                if !o.is_empty() {
                    out.push(format!("{}Output: {}", pg_ppad(depth + 1), o.join(", ")));
                }
            }
            render_plan(inner, depth + 2, costs, verbose, out);
        }
        // v1.69: PG19 `MergeJoin` (explain.c `ExplainNode` → `"Merge
        // Join"` with the jointype interpolated). `Merge Cond:` (the
        // merge clauses, outer var left) prints before `Join Filter:`
        // (residual quals), mirroring explain.c's order; inputs render
        // directly (each under its `Sort` when the merge choice added
        // one — PG19's `T_Sort` above `T_MergeJoin`).
        PlanNode::MergeJoin {
            filter,
            merge_cond,
            outer,
            inner,
            kind,
            ..
        } => {
            out.push(format!("{pad}{}", pg_merge_join_label(*kind)));
            push_output(out);
            out.push(format!("{ppad}Merge Cond: {merge_cond}"));
            if let Some(f) = filter {
                out.push(format!("{ppad}Join Filter: {f}"));
            }
            render_plan(outer, depth + 1, costs, verbose, out);
            render_plan(inner, depth + 1, costs, verbose, out);
        }
        // v1.48: PG19 `Materialize` (explain.c `T_Material`).
        PlanNode::Materialize { child, .. } => {
            out.push(format!("{pad}Materialize"));
            push_output(out);
            render_plan(child, depth + 1, costs, verbose, out);
        }
        PlanNode::Aggregate { child, .. } => {
            out.push(format!("{pad}Aggregate"));
            push_output(out);
            render_plan(child, depth + 1, costs, verbose, out);
        }
        PlanNode::Unique { child, .. } => {
            out.push(format!("{pad}Unique"));
            push_output(out);
            render_plan(child, depth + 1, costs, verbose, out);
        }
        PlanNode::Sort { keys, child, .. } => {
            out.push(format!("{pad}Sort"));
            push_output(out);
            out.push(format!("{ppad}Sort Key: {keys}"));
            render_plan(child, depth + 1, costs, verbose, out);
        }
        PlanNode::Limit { child, .. } => {
            out.push(format!("{pad}Limit"));
            push_output(out);
            render_plan(child, depth + 1, costs, verbose, out);
        }
        PlanNode::SubqueryScan { alias, child, .. } => {
            out.push(format!("{pad}Subquery Scan on {alias}"));
            push_output(out);
            render_plan(child, depth + 1, costs, verbose, out);
        }
        PlanNode::Values { .. } => {
            out.push(format!("{pad}Values Scan"));
            push_output(out);
        }
    }
}

/// v1.08: the pre-v1.08 planning-only rendering (two-space indentation,
/// `(rows=N)` suffixes), kept for EXPLAIN with COSTS ON (the default).
pub(crate) fn render_plan_costs_on(node: &PlanNode, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    match node {
        PlanNode::Result { rows, filter, .. } => {
            out.push(format!("{pad}Result (rows={rows})"));
            if let Some(f) = filter {
                out.push(format!("{pad}  Filter: {f}"));
            }
        }
        PlanNode::SeqScan {
            table,
            filter,
            rows,
            ..
        } => {
            out.push(format!("{pad}Seq Scan on {table} (rows={rows})"));
            if let Some(f) = filter {
                out.push(format!("{pad}  Filter: {f}"));
            }
        }
        PlanNode::IndexScan {
            table,
            index,
            cond,
            filter,
            rows,
            ..
        } => {
            out.push(format!(
                "{pad}Index Scan using {index} on {table} (rows={rows})"
            ));
            out.push(format!("{pad}  Index Cond: {cond}"));
            if let Some(f) = filter {
                out.push(format!("{pad}  Filter: {f}"));
            }
        }
        PlanNode::IndexOrderScan {
            table,
            index,
            order,
            rows,
            ..
        } => {
            out.push(format!(
                "{pad}Index Scan using {index} on {table} (rows={rows})"
            ));
            out.push(format!("{pad}  Order: {order}"));
        }
        PlanNode::NestedLoop {
            filter,
            rows,
            outer,
            inner,
            kind,
            ..
        } => {
            // v1.48: same join-kind label rule as the PG-text renderer.
            out.push(format!("{pad}{} (rows={rows})", pg_join_label(*kind)));
            if let Some(f) = filter {
                out.push(format!("{pad}  Filter: {f}"));
            }
            render_plan_costs_on(outer, depth + 1, out);
            render_plan_costs_on(inner, depth + 1, out);
        }
        // v1.64: Hash Join in the pre-v1.08 COSTS ON rendering.
        PlanNode::HashJoin {
            filter,
            hash_cond,
            rows,
            outer,
            inner,
            kind,
            ..
        } => {
            // v1.68: kind interpolates into the label (PG19 explain.c).
            out.push(format!("{pad}{} (rows={rows})", pg_hash_join_label(*kind)));
            out.push(format!("{pad}  Hash Cond: {hash_cond}"));
            if let Some(f) = filter {
                out.push(format!("{pad}  Join Filter: {f}"));
            }
            render_plan_costs_on(outer, depth + 1, out);
            out.push(format!("{pad}  Hash (rows={})", inner.rows()));
            render_plan_costs_on(inner, depth + 2, out);
        }
        // v1.69: Merge Join in the pre-v1.08 COSTS ON rendering.
        PlanNode::MergeJoin {
            filter,
            merge_cond,
            rows,
            outer,
            inner,
            kind,
            ..
        } => {
            out.push(format!("{pad}{} (rows={rows})", pg_merge_join_label(*kind)));
            out.push(format!("{pad}  Merge Cond: {merge_cond}"));
            if let Some(f) = filter {
                out.push(format!("{pad}  Join Filter: {f}"));
            }
            render_plan_costs_on(outer, depth + 1, out);
            render_plan_costs_on(inner, depth + 1, out);
        }
        // v1.48: PG19 `Materialize` (explain.c `T_Material`).
        PlanNode::Materialize { rows, child, .. } => {
            out.push(format!("{pad}Materialize (rows={rows})"));
            render_plan_costs_on(child, depth + 1, out);
        }
        PlanNode::Aggregate { rows, child, .. } => {
            out.push(format!("{pad}Aggregate (rows={rows})"));
            render_plan_costs_on(child, depth + 1, out);
        }
        PlanNode::Unique { rows, child, .. } => {
            out.push(format!("{pad}Unique (rows={rows})"));
            render_plan_costs_on(child, depth + 1, out);
        }
        PlanNode::Sort {
            keys, rows, child, ..
        } => {
            out.push(format!("{pad}Sort (rows={rows})"));
            out.push(format!("{pad}  Sort Key: {keys}"));
            render_plan_costs_on(child, depth + 1, out);
        }
        PlanNode::Limit { n, rows, child, .. } => {
            out.push(format!("{pad}Limit {n} (rows={rows})"));
            render_plan_costs_on(child, depth + 1, out);
        }
        PlanNode::SubqueryScan {
            alias, rows, child, ..
        } => {
            out.push(format!("{pad}Subquery Scan on {alias} (rows={rows})"));
            render_plan_costs_on(child, depth + 1, out);
        }
        PlanNode::Values { rows, .. } => {
            out.push(format!("{pad}Values (rows={rows})"));
        }
    }
}

/// v1.08: `costs` selects the PG19 text shape (COSTS OFF) versus the
/// legacy rustgres shape (COSTS ON, the default).
pub(crate) fn exec_explain(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    stmt: &Stmt,
    costs: bool,
    // v1.46: EXPLAIN VERBOSE — render the `Output:` targetlist lines.
    verbose: bool,
) -> Result<ExecResult, ExecError> {
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
    // v1.08: COSTS OFF selects the PG-text plan rendering.
    let plan = plan_select(
        &*eng,
        sel,
        ctx.snap,
        ctx.own,
        ctx.session,
        &[],
        !costs,
        verbose,
    )?;
    let mut lines = Vec::new();
    render_plan(&plan, 0, costs, verbose, &mut lines);
    Ok(ExecResult::Explain {
        columns: vec![("QUERY PLAN".to_string(), ColType::Text)],
        rows: lines
            .into_iter()
            .map(|l| Row::new(vec![Value::text(l)]))
            .collect(),
    })
}
