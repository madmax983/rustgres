// v1.78 mechanical split: moved verbatim from src/exec.rs (13888-16169).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ============================================================================
// v1.65: 3-way cross-join reordering — PG19 `standard_join_search`
// (allpaths.c) / `join_search_one_level` + `make_join_rel` (joinrels.c)
// in miniature, nestloop paths only.
//
// t5's remaining gap: the subquery already flattens (the parser folds
// comma lists into a single Cross Join item, so q0's 2-table subquery
// satisfies the pullup's `<= 1 FROM item` gate) and `q0.a = 1` is
// already pushed to n2's scan by `pg_split_where`. What remains is the
// join ORDER. PG builds joinrels level by level — all pairs, then all
// triples — and `make_join_rel` tries both input orders for JOIN_INNER
// via `add_paths_to_joinrel`, keeping the cheapest `cost_nestloop`
// path (`add_path` keeps the earlier-added path on fuzzy ties,
// `STD_FUZZ_FACTOR` = 1.01).
//
// This is that search for exactly three base-table leaves of a pure
// comma (Cross) product: all 3 pairs x both orders (level 2), then
// each pair joined with an allowed remaining leaf x both orders
// (level 3), cheapest wins. Row estimates reuse the v1.63
// `est_filtered_rows` machinery, scan costs the v1.63 `cost_seqscan`
// skeleton, and pair output rows use PG19's no-stats join-selectivity
// defaults (clausesel.c). A pair whose join clauses all link outside
// leaves extends only via those clauses (PG19
// `make_rels_by_clause_joins`); otherwise it extends cartesianly with
// every remaining leaf (PG19 `make_rels_by_clauseless_joins`).
//
// Level-3 ties are broken by preferring the cheaper inner pair, then
// the smaller pair — PG's empirical selective-first order (the t5
// oracle builds the inner pair from the filtered rel); the v1.64
// assessment sets the precedent of encoding PG's empirical order when
// the cost model near-ties.
//
// Soundness: like the v1.63 swap, this only reorders EXPLAIN plan
// children — the executor never sees `PlanNode` — so results are
// identical by construction. Fail closed: anything that is not a pure
// 3-table Cross product, any unresolvable table, any non-SeqScan leaf,
// or any planning error returns `None` and the existing written-order
// path runs untouched.
// ============================================================================

/// v1.65: join selectivity of one join conjunct under PG19's no-stats
/// defaults (clausesel.c): `eqsel` -> `DEFAULT_EQ_SEL` (0.005),
/// `neqsel` -> `1 - eqsel`, ordering ops -> `DEFAULT_INEQ_SEL` (1/3).
/// Anything else cannot make a pair look cheaper, only less
/// attractive (fail safe for the ordering decision).
pub(crate) fn pg_join_qual_sel(c: &Expr) -> f64 {
    if let Expr::Cmp { op, .. } = c {
        return match op {
            CmpOp::Eq => PG_DEFAULT_EQ_SEL,
            CmpOp::Ne => (1.0 - PG_DEFAULT_EQ_SEL).clamp(0.0, 1.0),
            CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => 1.0 / 3.0,
            _ => 1.0,
        };
    }
    1.0
}

/// v1.65: flatten one side of a comma join into base-table leaves.
/// Returns false (fail closed) on anything that is not a pure Cross
/// product over plain tables: outer joins, ON/USING/NATURAL joins,
/// derived tables, functions, VALUES, ...
pub(crate) fn pg_cross_leaf_one<'a>(it: &'a FromItem, out: &mut Vec<&'a FromItem>) -> bool {
    match it {
        FromItem::Table { .. } => {
            out.push(it);
            true
        }
        FromItem::Join {
            left,
            right,
            kind,
            on,
            using,
            natural,
            ..
        } if *kind == JoinKind::Cross && on.is_none() && using.is_empty() && !natural => {
            pg_cross_leaf_one(left, out) && pg_cross_leaf_one(right, out)
        }
        _ => false,
    }
}

/// v1.65: the leaf-index set a join conjunct references, mirroring
/// `pg_split_where`'s ownership rules (qualified refs match the leaf's
/// qualifier list; unqualified refs match the unique owning leaf's
/// columns). `None` when a ref is ambiguous or unresolvable — the
/// caller treats such conjuncts as covering every leaf (applied at the
/// top join; always correct, matching `pg_split_where`'s join-level
/// placement of the same conjuncts).
pub(crate) fn pg_conjunct_leaf_set(
    c: &Expr,
    splits: &[(Vec<String>, Vec<String>)],
) -> Option<Vec<usize>> {
    let mut refs = Vec::new();
    pg_expr_tables(c, &mut refs);
    let mut set = Vec::new();
    for (q, col) in &refs {
        let mut found = None;
        for (i, (qs, cs)) in splits.iter().enumerate() {
            let matches = match q {
                Some(qq) => qs.iter().any(|x| x == qq),
                None => cs.iter().any(|x| x == col),
            };
            if matches {
                if found.is_some() {
                    return None;
                }
                found = Some(i);
            }
        }
        let i = found?;
        if !set.contains(&i) {
            set.push(i);
        }
    }
    set.sort_unstable();
    Some(set)
}

/// v1.65: one leaf of the 3-way reorder: the planned scan plus its
/// cost-model inputs (`rows` after its WHERE slice, `scan` the
/// `cost_seqscan` skeleton — the v1.63 `pg_nestloop_swap` inputs).
pub(crate) struct PgCrossLeaf {
    pub(crate) node: PlanNode,
    pub(crate) rows: f64,
    pub(crate) scan: f64,
}

/// v1.66: one N-way DP joinrel — a leaf subset (bitmask) with its
/// cheapest left-deep nestloop build (PG19 `standard_join_search`
/// level-k rel, nestloop paths only). Level-2 rels are pairs (the
/// v1.65 `PgPair165`, now uniform with higher levels: `sub` is the
/// single outer leaf, `leaf` the inner leaf).
#[derive(Clone)]
pub(crate) struct PgJoinRel166 {
    /// estimated output rows (`rows(sub) * rows(leaf) * sel(quals)`)
    pub(crate) rows: f64,
    /// cheapest nestloop path cost to build this rel
    pub(crate) cost: f64,
    /// winning build: subrel mask and the newly joined leaf
    pub(crate) sub: u32,
    pub(crate) leaf: usize,
    /// true when the subrel is the outer side of the winning build
    pub(crate) sub_outer: bool,
    /// join quals newly coverable at this node (PG19
    /// `distribute_qual_to_rels`: attached at the lowest join covering
    /// all their tables)
    pub(crate) quals: Vec<Expr>,
    /// estimated rows of the inner side of the winning build (the
    /// v1.65 selective-first tie-break, generalized)
    pub(crate) inner_rows: f64,
}

/// v1.66: leaves a DP rel (member leaf set `members`) may join at the
/// next level (PG19 `join_search_one_level`): a rel with join clauses
/// linking an outside leaf extends only via those clauses (PG19
/// `make_rels_by_clause_joins`); otherwise it extends cartesianly with
/// every remaining leaf (PG19 `make_rels_by_clauseless_joins`).
/// Generalizes v1.65's `pg_pair_extensions` (which was pair-only) to
/// N-way rels; `n` is the total leaf count.
pub(crate) fn pg_subset_extensions(
    rem: &[(Expr, Vec<usize>)],
    members: &[usize],
    n: usize,
) -> Vec<usize> {
    let mut via_clause = Vec::new();
    for (_, s) in rem {
        if s.iter().any(|x| members.contains(x)) && !s.iter().all(|x| members.contains(x)) {
            for x in s {
                if !members.contains(x) && !via_clause.contains(x) {
                    via_clause.push(*x);
                }
            }
        }
    }
    if via_clause.is_empty() {
        (0..n).filter(|x| !members.contains(x)).collect()
    } else {
        via_clause
    }
}

/// v1.65: build one `NestedLoop` for the reordered tree — the same
/// construction as the existing Join branch and comma-join loop (rows
/// product, output concatenation, `pg_maybe_materialize`,
/// `JoinKind::Cross`). `quals` are the conjuncts newly coverable at
/// this node, deparsed with qualification like every join filter.
pub(crate) fn pg_build_cross_nl(
    outer: PlanNode,
    inner: PlanNode,
    quals: Vec<Expr>,
    px: &PgPlanCtx,
    // v1.72: EC-canonical join-filter rendering (PG19 equivclass.c
    // `generate_join_implied_equalities`) — the 3-way+ DP path's share of
    // v1.71's 2-way rule. `outer`/`inner` are the build-time sides (the
    // DP's chosen order IS the build order).
    ec: &mut PgEc,
    ec_split: &[(Vec<String>, Vec<String>)],
    outer_names: &[String],
    inner_names: &[String],
    ec_partial_outer: bool,
) -> PlanNode {
    let mut join_filter = match pg_fold_and(quals.clone()) {
        Some(e) => Some(pg_expr_text_or_debug(&e, px, true)),
        None => None,
    };
    // v1.72: rewrite to PG19 EC-canonical representatives
    // (first-orientation-outer-member = first-orientation-inner-member).
    // A partial index on the build-time-outer side flips the orientation
    // (generalizes v1.71's left-table rule). No-op when the text is
    // unchanged — zero rendering drift for untouched plans.
    let (ec_outer, ec_inner) = if ec_partial_outer {
        (inner_names, outer_names)
    } else {
        (outer_names, inner_names)
    };
    if let Some(t) = pg_ec_join_filter_text(&quals, ec, ec_outer, ec_inner, ec_split, px) {
        join_filter = Some(t);
    }
    let rows = outer.rows().saturating_mul(inner.rows());
    let mut output = outer.output().to_vec();
    output.extend(inner.output().iter().cloned());
    let inner = pg_maybe_materialize(&outer, inner);
    PlanNode::NestedLoop {
        filter: None,
        join_filter,
        rows,
        outer: Box::new(outer),
        inner: Box::new(inner),
        kind: JoinKind::Cross,
        output,
    }
}

/// v1.66: build the winning N-way DP tree — the same construction as
/// the v1.65 level-3 builder (`pg_build_cross_nl`), applied
/// recursively down the DP's winning decomposition chain. A
/// single-leaf subrel is the planned scan itself.
pub(crate) fn pg_build_cross_dp(
    rel: &PgJoinRel166,
    rels: &[Option<PgJoinRel166>],
    leaves: &[PgCrossLeaf],
    px: &PgPlanCtx,
    // v1.72: EC-canonical join-filter rendering state (PG19 equivclass.c).
    ec: &mut PgEc,
    ec_split: &[(Vec<String>, Vec<String>)],
    ec_partial: &[bool],
    leaf_names: &[Vec<String>],
) -> PlanNode {
    let sub_node = if rel.sub.count_ones() == 1 {
        leaves[rel.sub.trailing_zeros() as usize].node.clone()
    } else {
        let sub_rel = rels[rel.sub as usize]
            .as_ref()
            .expect("v1.66: DP subrel exists");
        pg_build_cross_dp(
            sub_rel, rels, leaves, px, ec, ec_split, ec_partial, leaf_names,
        )
    };
    let leaf_node = leaves[rel.leaf].node.clone();
    // v1.72: build-time sides for the EC orientation (the DP's chosen
    // outer/inner IS the build order, unlike the v1.63-swapped 2-way
    // path). Display-qualifier names per side (NOT pg_item_quals — the
    // table+alias pair would trip the self-join guard); a partial index
    // on any build-time-outer leaf flips the orientation.
    let (outer_mask, inner_mask) = if rel.sub_outer {
        (rel.sub, 1u32 << rel.leaf)
    } else {
        (1u32 << rel.leaf, rel.sub)
    };
    let mut outer_names: Vec<String> = Vec::new();
    let mut inner_names: Vec<String> = Vec::new();
    let mut ec_partial_outer = false;
    for i in 0..leaves.len() {
        if outer_mask & (1u32 << i) != 0 {
            outer_names.extend(leaf_names[i].iter().cloned());
            ec_partial_outer |= ec_partial[i];
        }
        if inner_mask & (1u32 << i) != 0 {
            inner_names.extend(leaf_names[i].iter().cloned());
        }
    }
    if rel.sub_outer {
        pg_build_cross_nl(
            sub_node,
            leaf_node,
            rel.quals.clone(),
            px,
            ec,
            ec_split,
            &outer_names,
            &inner_names,
            ec_partial_outer,
        )
    } else {
        pg_build_cross_nl(
            leaf_node,
            sub_node,
            rel.quals.clone(),
            px,
            ec,
            ec_split,
            &outer_names,
            &inner_names,
            ec_partial_outer,
        )
    }
}

/// v1.66: PG19 `standard_join_search` (allpaths.c) for N-way
/// comma-join products, nestloop paths only. Generalizes the v1.65
/// 3-leaf miniature to 3..=8 leaves: level 2 tries all pairs × both
/// orders; level k (3..=N) extends each level-(k-1) rel by one leaf per
/// PG's clause-vs-clauseless rule (`join_search_one_level`); costs via
/// `pg_cost_nestloop`; the winner minimizes the v1.65 lexicographic key
/// (top_cost, sub_cost, sub_rows, sub_inner_rows) — the absolute
/// `cost_nestloop` model first, then PG's empirical selective-first
/// order (the t5 oracle builds the inner pair from the filtered rel);
/// exact ties keep the written order (fail closed). Returns the
/// reordered plan, or `None` to keep the existing written-order path
/// (fail closed).
///
/// Bound: PG19 switches to GEQO at `geqo_threshold` (12) rels; rustgres
/// fails closed beyond 8 leaves — 2^8 DP states are trivial to explore,
/// and 8 is well clear of PG's heuristic threshold.
///
/// Join quals attach at the lowest nestloop covering all their tables
/// (PG19 `distribute_qual_to_rels`); per-leaf Filters are already on
/// the planned scans.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pg_reorder_cross_join(
    eng: &Engine,
    left: &FromItem,
    right: &FromItem,
    where_: Option<&Expr>,
    snap: &Snapshot,
    own: u64,
    session: u64,
    ctes: &[CteDef],
    px: &PgPlanCtx,
    stmt: &SelectStmt,
) -> Option<PlanNode> {
    let mut items: Vec<&FromItem> = Vec::new();
    if !(pg_cross_leaf_one(left, &mut items) && pg_cross_leaf_one(right, &mut items)) {
        return None;
    }
    let n = items.len();
    // v1.66: N-way DP for 3..=8 leaves (v1.65: exactly 3). Fewer than 3
    // leaves use the pre-existing paths; more than 8 fail closed to the
    // written order (PG19 would use GEQO past 12).
    if n < 3 || n > 8 {
        return None;
    }
    // WHERE distribution over the leaves (PG19 `distribute_qual_to_rels`,
    // via the existing `pg_split_where`: single-leaf conjuncts become
    // scan Filters, the rest stay at the join level).
    let splits: Vec<(Vec<String>, Vec<String>)> = items
        .iter()
        .map(|it| pg_split_item(eng, it, snap, own, session, ctes))
        .collect();
    let (per_leaf, remainder) = match where_ {
        Some(w) => pg_split_where(w, &splits).ok()?,
        None => (vec![Vec::new(); n], Vec::new()),
    };
    // v1.72: EC-canonical join-filter rendering (PG19 equivclass.c
    // `generate_join_implied_equalities`). One union-find over ALL WHERE
    // conjuncts in written order — single-leaf conjuncts seed the EC too
    // (e.g. t1.a = t1.b), exactly like v1.71's 2-way path. Shared by every
    // DP nestloop node below.
    let mut join_ec = PgEc::new();
    if let Some(w) = where_ {
        for c in pg_conjuncts(w) {
            pg_ec_union_conjunct(&mut join_ec, c, &splits);
        }
    }
    // v1.72: partial-index flags per leaf (PG19 `check_index_predicates`
    // early EC orientation; generalizes v1.71's left-table rule to the
    // DP's multi-leaf build-time sides).
    let ec_partial: Vec<bool> = items
        .iter()
        .map(|it| pg_item_has_partial_index(eng, it, snap, own, session))
        .collect();
    // v1.72: display qualifier per leaf (alias-or-table, PG19's RTE
    // alias) for the EC orientation sides. NOTE: this is NOT
    // `splits[i].0` (`pg_item_quals` returns table AND alias, which
    // would trip `pg_ec_join_filter_text`'s self-join guard); the 2-way
    // path uses `pg_replaces_names` for the same reason.
    let leaf_names: Vec<Vec<String>> = items
        .iter()
        .map(|it| {
            let mut v = Vec::new();
            pg_replaces_names(std::slice::from_ref(*it), &mut v);
            v
        })
        .collect();
    // Plan each leaf exactly as the written-order path would; gate on
    // plain SeqScan leaves over resolvable base tables (the v1.63
    // estimability gate, extended to N leaves). A planning error
    // falls back so the existing path can report it faithfully.
    let mut leaves: Vec<PgCrossLeaf> = Vec::with_capacity(n);
    for (it, cs) in items.iter().zip(per_leaf.iter()) {
        let lw = pg_fold_and(cs.clone());
        let node = plan_from_item(
            eng,
            it,
            lw.as_ref(),
            snap,
            own,
            session,
            ctes,
            Some(px),
            stmt,
        )
        .ok()?;
        let (table, alias) = match it {
            FromItem::Table { name, alias, .. } => (name.as_str(), alias.as_deref()),
            _ => return None,
        };
        if !matches!(node, PlanNode::SeqScan { .. }) {
            return None;
        }
        let t = eng.db.find_table(table, snap, &[own], session)?;
        let qual = alias.unwrap_or(table);
        let rows = est_filtered_rows(&eng.db, table, qual, t, node.rows() as f64, lw.as_ref());
        let scan = est_heap_pages(t) as f64 * SEQ_PAGE_COST + rows * CPU_TUPLE_COST;
        leaves.push(PgCrossLeaf { node, rows, scan });
    }
    // Leaf-index set per join-level conjunct; ambiguous, unresolvable
    // or const (empty) conjuncts cover every leaf (applied at the top
    // join), matching `pg_split_where`'s join-level placement.
    let all: Vec<usize> = (0..n).collect();
    let rem: Vec<(Expr, Vec<usize>)> = remainder
        .into_iter()
        .map(|c| {
            let mut set = pg_conjunct_leaf_set(&c, &splits).unwrap_or_else(|| all.clone());
            if set.is_empty() {
                set = all.clone();
            }
            (c, set)
        })
        .collect();
    // DP table: mask -> cheapest left-deep nestloop build. Level 2: all
    // pairs, both orders (PG19 `make_join_rel` tries both for JOIN_INNER;
    // `add_path` keeps the cheaper, ties keep the earlier-added =
    // written order, mirrored by strict `<`).
    let mut rels: Vec<Option<PgJoinRel166>> = vec![None; 1 << n];
    for i in 0..n {
        for j in (i + 1)..n {
            let quals: Vec<Expr> = rem
                .iter()
                .filter(|(_, s)| s.as_slice() == [i, j])
                .map(|(c, _)| c.clone())
                .collect();
            let sel: f64 = quals.iter().map(pg_join_qual_sel).product();
            let qp = CPU_OPERATOR_COST * quals.len() as f64;
            let (li, lj) = (&leaves[i], &leaves[j]);
            let cost_ij = pg_cost_nestloop(li.rows, li.scan, lj.rows, lj.scan, qp);
            let cost_ji = pg_cost_nestloop(lj.rows, lj.scan, li.rows, li.scan, qp);
            let (outer, inner, cost) = if cost_ji < cost_ij {
                (j, i, cost_ji)
            } else {
                (i, j, cost_ij)
            };
            let mask = (1u32 << i) | (1u32 << j);
            rels[mask as usize] = Some(PgJoinRel166 {
                rows: li.rows * lj.rows * sel,
                cost,
                sub: 1u32 << outer,
                leaf: inner,
                sub_outer: true,
                quals,
                inner_rows: leaves[inner].rows,
            });
        }
    }
    // Levels 3..=N: each level-(k-1) rel extends by one leaf per PG's
    // clause-vs-clauseless rule (`join_search_one_level`,
    // `make_rels_by_clause_joins` / `make_rels_by_clauseless_joins`).
    // `r` visits leaves in decreasing order so the (T, r) visit order
    // matches v1.65's pair order at N=3 (exact-tie determinism).
    for k in 3..=n {
        for mask in 0..(1u32 << n) {
            if mask.count_ones() as usize != k {
                continue;
            }
            let members: Vec<usize> = (0..n).filter(|i| mask & (1u32 << i) != 0).collect();
            let mut best_key: Option<(f64, f64, f64, f64)> = None;
            let mut best: Option<PgJoinRel166> = None;
            for &r in members.iter().rev() {
                let sub = mask ^ (1u32 << r);
                let Some(sub_rel) = rels[sub as usize].as_ref() else {
                    continue;
                };
                let sub_members: Vec<usize> = (0..n).filter(|i| sub & (1u32 << i) != 0).collect();
                if !pg_subset_extensions(&rem, &sub_members, n).contains(&r) {
                    continue;
                }
                // Join quals newly coverable at this node: conjuncts
                // covering a subset of `members` but not of `sub_members`
                // (PG19: the new joinrel's own restrictinfo — the lowest
                // join covering all their tables).
                let new_quals: Vec<Expr> = rem
                    .iter()
                    .filter(|(_, s)| {
                        s.iter().all(|x| members.contains(x))
                            && !s.iter().all(|x| sub_members.contains(x))
                    })
                    .map(|(c, _)| c.clone())
                    .collect();
                let sel: f64 = new_quals.iter().map(pg_join_qual_sel).product();
                let qp = CPU_OPERATOR_COST * new_quals.len() as f64;
                let lr = &leaves[r];
                let rows = sub_rel.rows * lr.rows * sel;
                let c_sub_outer =
                    pg_cost_nestloop(sub_rel.rows, sub_rel.cost, lr.rows, lr.scan, qp);
                let c_leaf_outer =
                    pg_cost_nestloop(lr.rows, lr.scan, sub_rel.rows, sub_rel.cost, qp);
                for (cost, sub_outer, inner_rows) in [
                    (c_sub_outer, true, lr.rows),
                    (c_leaf_outer, false, sub_rel.rows),
                ] {
                    let key = (cost, sub_rel.cost, sub_rel.rows, sub_rel.inner_rows);
                    let better = match best_key {
                        None => true,
                        Some(bk) => key < bk,
                    };
                    if better {
                        best_key = Some(key);
                        best = Some(PgJoinRel166 {
                            rows,
                            cost,
                            sub,
                            leaf: r,
                            sub_outer,
                            quals: new_quals.clone(),
                            inner_rows,
                        });
                    }
                }
            }
            rels[mask as usize] = best;
        }
    }
    // Build the winning tree from the full rel's decomposition chain.
    // A `None` full rel (no viable decomposition — unreachable for pure
    // comma products, but never assumed) fails closed to written order.
    let full = (1u32 << n) - 1;
    let top = rels[full as usize].as_ref()?;
    Some(pg_build_cross_dp(
        top,
        &rels,
        &leaves,
        px,
        &mut join_ec,
        &splits,
        &ec_partial,
        &leaf_names,
    ))
}

/// v0.78: plan a CTE body for EXPLAIN. `visible` holds the CTEs the body
/// may reference (outer levels ++ earlier siblings, in definition
/// order). UNION bodies (recursive CTEs) are estimated, not planned.
pub(crate) fn plan_cte_body(
    eng: &Engine,
    cte: &CteDef,
    visible: &[CteDef],
    snap: &Snapshot,
    own: u64,
    session: u64,
    // v1.08: PG-text mode for EXPLAIN (COSTS OFF).
    pg: bool,
    // v1.54: EXPLAIN VERBOSE flag (Filter `useprefix` rule).
    verbose: bool,
) -> Result<PlanNode, ExecError> {
    match &cte.body {
        CteBody::Simple(sel) => plan_select(eng, sel, snap, own, session, visible, pg, verbose),
        CteBody::Union { left, right, all } => {
            let l = plan_select(eng, left, snap, own, session, visible, pg, verbose)?;
            let r = plan_select(eng, right, snap, own, session, visible, pg, verbose)?;
            let rows = if *all {
                l.rows().saturating_add(r.rows())
            } else {
                l.rows().max(r.rows())
            };
            Ok(PlanNode::Values {
                rows,
                // v1.46: VERBOSE `Output:` (union CTE bodies are nominal).
                output: Vec::new(),
            })
        }
        // v1.39: EXPLAIN of a data-modifying CTE is not supported
        // (fail-closed 0A000; PG would show the ModifyTable plan).
        CteBody::Dml(_) => Err(exec_err(
            "0A000",
            "EXPLAIN of a data-modifying CTE is not supported",
        )),
    }
}

/// v1.47: do all key columns of some uniqueness guarantee on `table` appear
/// (case-insensitively) in `constrained`? Uniqueness proof for join removal
/// (PG19 `relation_has_unique_index_for`, indxpath.c). Covers PRIMARY KEY
/// and UNIQUE constraints plus standalone unique indexes that are
/// planner-usable: unique, non-partial, plain columns (PG's "indpred ==
/// NIL" + immediate — our constraints are all immediate; there is no
/// DEFERRABLE support). Multi-column keys qualify only when EVERY key
/// column is constrained by the join's equality conjuncts.
pub(crate) fn table_has_unique_for(
    eng: &Engine,
    table: &str,
    constrained: &HashSet<String>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    let db = &eng.db;
    let Some(t) = db.find_table(table, snap, &[own], session) else {
        return false;
    };
    let covers = |cols: &[String]| {
        !cols.is_empty()
            && cols
                .iter()
                .all(|c| constrained.iter().any(|x| x.eq_ignore_ascii_case(c)))
    };
    if t.pkey.as_ref().is_some_and(|pk| covers(&pk.cols)) {
        return true;
    }
    if t.uniques.iter().any(|u| covers(&u.cols)) {
        return true;
    }
    if !db.is_temp_table(session, table) {
        for ix in db.visible_indexes_for(table, snap, &[own], session) {
            if ix.def.unique && ix.def.planner_usable && covers(&ix.def.col_names) {
                return true;
            }
        }
    }
    false
}

/// v1.47: resolve a range qualifier to its underlying table name within
/// a FROM tree (aliases resolve to the real name; bare names stay as-is).
pub(crate) fn removal_qual_table(item: &FromItem, qual: &str) -> Option<String> {
    match item {
        FromItem::Table { name, alias, .. } => {
            if alias.as_deref() == Some(qual) || (alias.is_none() && name == qual) {
                Some(name.clone())
            } else {
                None
            }
        }
        FromItem::Join { left, right, .. } => {
            removal_qual_table(left, qual).or_else(|| removal_qual_table(right, qual))
        }
        FromItem::Derived { sub, alias, .. } => {
            if alias == qual {
                // Derived: no single underlying table; callers that need a
                // table type must use subquery_output_types instead.
                None
            } else {
                sub.from.iter().find_map(|f| removal_qual_table(f, qual))
            }
        }
        _ => None,
    }
}

/// v1.47: column type lookup for the join-removal type-compatibility check
/// (PG19 `equality_ops_are_compatible`, analyzejoins.c: the join's `=` must
/// agree with the equality the uniqueness proof relies on). `qual` is the
/// range qualifier as written; it is resolved against `scope` (the FROM
/// tree the qualifier is visible in) to find the real table.
pub(crate) fn removal_col_type(
    eng: &Engine,
    scope: &FromItem,
    qual: &str,
    col: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<ColType> {
    let table = removal_qual_table(scope, qual)?;
    let t = eng.db.find_table(&table, snap, &[own], session)?;
    let pos = t.column_index(col)?;
    t.columns.get(pos).map(|(_, ty)| *ty)
}

/// v1.47: are these two types' equalities compatible for join removal? Same
/// type is always fine; PG's integer btree opfamily gives consistent
/// equality semantics across int2/int4/int8 (covers the corpus's
/// `int8_tbl.q1 = int4_tbl.f1` case). Anything else fails closed.
pub(crate) fn removal_types_compatible(a: ColType, b: ColType) -> bool {
    if a == b {
        return true;
    }
    let is_int = |t: &ColType| matches!(t, ColType::SmallInt | ColType::Int | ColType::BigInt);
    is_int(&a) && is_int(&b)
}

/// v1.47: does `e` contain a subquery anywhere? Join-removal conjuncts must
/// be plain equalities (PG's mergejoinable restriction, which implies no
/// subplans); fail closed on `ScalarSub`/`ArraySubquery`/`InSub`/`Exists`/
/// `Quantified`, and on any unrecognized variant.
pub(crate) fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::InSub { .. }
        | Expr::Exists { .. }
        | Expr::Quantified { .. } => true,
        Expr::Column { .. } | Expr::Literal(_) | Expr::Param(_) | Expr::ResolvedCol { .. } => false,
        Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Cmp {
            left: a, right: b, ..
        }
        | Expr::UserOp {
            left: a, right: b, ..
        }
        | Expr::IsDistinctFrom {
            left: a, right: b, ..
        }
        | Expr::Concat(a, b) => expr_has_subquery(a) || expr_has_subquery(b),
        Expr::Not(x) | Expr::Neg(x) | Expr::BitNot(x) => expr_has_subquery(x),
        Expr::Func { args, .. } => args.iter().any(expr_has_subquery),
        Expr::Agg { arg, arg2, .. } => {
            arg.as_ref().is_some_and(|x| expr_has_subquery(x))
                || arg2.as_ref().is_some_and(|x| expr_has_subquery(x))
        }
        Expr::IsNull { expr, .. } | Expr::IsBool { expr, .. } => expr_has_subquery(expr),
        Expr::Between {
            expr, low, high, ..
        } => expr_has_subquery(expr) || expr_has_subquery(low) || expr_has_subquery(high),
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            operand.as_ref().is_some_and(|x| expr_has_subquery(x))
                || whens
                    .iter()
                    .any(|(w, t)| expr_has_subquery(w) || expr_has_subquery(t))
                || else_.as_ref().is_some_and(|x| expr_has_subquery(x))
        }
        Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => expr_has_subquery(expr),
        Expr::Row(es) => es.iter().any(expr_has_subquery),
        Expr::ArrayCtor { elems: es, .. } => es.iter().any(expr_has_subquery),
        // v1.74: the catch-all below historically false-positived on every
        // pure-expression variant (notably `Arith`), which disabled
        // self-join elimination for any restriction containing arithmetic
        // (v1.73's stmt-1 target). Recurse precisely: a subquery can only
        // hide in a subexpression.
        Expr::Arith { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::Extract { from, .. } => expr_has_subquery(from),
        Expr::Subscript { array, indices } => {
            expr_has_subquery(array) || indices.iter().any(expr_has_subquery)
        }
        Expr::Slice { array, bounds } => {
            expr_has_subquery(array)
                || bounds.iter().any(|(lo, hi)| {
                    lo.as_ref().is_some_and(|e| expr_has_subquery(e))
                        || hi.as_ref().is_some_and(|e| expr_has_subquery(e))
                })
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_has_subquery(expr)
                || expr_has_subquery(pattern)
                || escape.as_ref().is_some_and(|e| expr_has_subquery(e))
        }
        Expr::Regex { expr, pattern, .. } => expr_has_subquery(expr) || expr_has_subquery(pattern),
        Expr::FieldAccess { expr, .. } | Expr::NamedArg { expr, .. } => expr_has_subquery(expr),
        Expr::WholeRow { .. } => false,
        Expr::Window {
            args,
            partition_by,
            order_by,
            filter,
            ..
        } => {
            args.iter().any(expr_has_subquery)
                || partition_by.iter().any(expr_has_subquery)
                || order_by.iter().any(|t| expr_has_subquery(&t.expr))
                || filter.as_ref().is_some_and(|e| expr_has_subquery(e))
        }
        _ => true,
    }
}

/// v1.47: all range qualifiers visible in a FROM item (flattened joins via
/// `pg_item_quals`, aliases for derived/table-function items).
pub(crate) fn removal_left_quals(item: &FromItem) -> HashSet<String> {
    pg_item_quals(item).into_iter().collect()
}

/// v1.47: map a subquery's output column names to their types where the
/// mapping is unambiguous: single plain-table FROM with `*`/`t.*` (position
/// mapping), or explicit select items whose expression is a plain qualified
/// column. Used for the join-removal type-compatibility check on derived
/// inner sides. Anything ambiguous yields no entry (fail closed downstream).
pub(crate) fn subquery_output_types(
    eng: &Engine,
    sub: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> HashMap<String, ColType> {
    let mut out = HashMap::new();
    // Set-operation: output types come from the leftmost branch (PG's
    // setop output types are determined by the left branch).
    if let Some(root) = sub.set_op.as_ref() {
        return subquery_output_types(eng, &root.left, snap, own, session);
    }
    // Single plain-table FROM?
    let (tbl_name, tbl_cols): (String, Vec<(String, ColType)>) = match sub.from.as_slice() {
        [
            FromItem::Table {
                name, col_aliases, ..
            },
        ] if col_aliases.is_empty() => match eng.db.find_table(name, snap, &[own], session) {
            Some(t) => (name.clone(), t.columns.clone()),
            None => return out,
        },
        _ => return out,
    };
    // Helper: type of a column (qualified or unqualified) against the
    // single FROM table.
    let col_ty = |qt: Option<&str>, qn: &str| -> Option<ColType> {
        match qt {
            Some(q) if !q.eq_ignore_ascii_case(&tbl_name) => None,
            _ => removal_col_type(eng, &sub.from[0], &tbl_name, qn, snap, own, session),
        }
    };
    let mut pos = 0usize;
    for item in &sub.items {
        match item {
            SelectItem::All | SelectItem::AllOf(_) => {
                for (cn, ct) in tbl_cols.iter().skip(pos) {
                    out.insert(cn.to_lowercase(), *ct);
                    pos += 1;
                }
            }
            SelectItem::Expr { expr, alias } => {
                let out_name = alias.clone().unwrap_or_else(|| match expr {
                    Expr::Column { name, .. } => name.clone(),
                    _ => String::new(),
                });
                if out_name.is_empty() {
                    continue;
                }
                match expr {
                    Expr::Column { table, name } => {
                        if let Some(ct) = col_ty(table.as_deref(), name) {
                            out.insert(out_name.to_lowercase(), ct);
                        }
                    }
                    Expr::Literal(lit) => {
                        if let Some(ct) = literal_col_type(lit) {
                            out.insert(out_name.to_lowercase(), ct);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// v1.47: best-effort `ColType` for a literal (for the join-removal
/// type-compatibility check on derived tables like `SELECT 1 AS x`).
pub(crate) fn literal_col_type(lit: &Literal) -> Option<ColType> {
    match lit {
        Literal::Int(_) => Some(ColType::Int),
        Literal::BigInt(_) => Some(ColType::BigInt),
        Literal::SmallInt(_) => Some(ColType::SmallInt),
        Literal::Float(_) | Literal::Decimal(_) => Some(ColType::Float),
        Literal::Text(_) => Some(ColType::Text),
        Literal::Bool(_) => Some(ColType::Bool),
        _ => None,
    }
}

/// v1.47: PG19's `query_is_distinct_for` (analyzejoins.c) for a derived
/// inner side: can the subquery produce at most one row for each distinct
/// combination of the `constrained` output columns? `constrained` holds
/// the subquery's OUTPUT column names constrained by the join's equality
/// conjuncts. Stricter than PG in one corpus-pinned case: any set-returning
/// function in the targetlist fails closed (DISTINCT ON + SRF keeps the join
/// in the expected output).
pub(crate) fn subquery_is_distinct_for(
    eng: &Engine,
    sub: &SelectStmt,
    constrained: &HashSet<String>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    if sub.set_op.is_some() {
        return setop_is_distinct_for(eng, sub, constrained, snap, own, session);
    }
    if sub.from.iter().any(|f| match f {
        FromItem::Derived { lateral, .. }
        | FromItem::Values { lateral, .. }
        | FromItem::Function { lateral, .. } => *lateral,
        _ => false,
    }) {
        return false;
    }
    // Targetlist SRFs fail closed (corpus: DISTINCT ON + generate_series
    // keeps the join, stricter than beta3's analyzejoins).
    for item in &sub.items {
        if let SelectItem::Expr { expr, .. } = item {
            if expr_has_srf(eng, expr) {
                return false;
            }
        }
    }
    // Plain DISTINCT: every DISTINCT-clause column constrained. `SELECT
    // DISTINCT *` constrains all outputs.
    if sub.distinct {
        let cols = match pg_subquery_col_names(eng, sub, snap, own, session, &[]) {
            Some(c) => c,
            None => return false,
        };
        return !cols.is_empty()
            && cols.iter().all(|c| {
                constrained
                    .iter()
                    .any(|x| x.eq_ignore_ascii_case(c.as_str()))
            });
    }
    // DISTINCT ON: every ON-expression column constrained.
    if !sub.distinct_on.is_empty() {
        let mut ok = true;
        for e in &sub.distinct_on {
            let mut refs = Vec::new();
            collect_col_refs(e, &mut refs);
            if refs.is_empty() {
                ok = false;
                break;
            }
            for (_, n) in refs {
                if !constrained
                    .iter()
                    .any(|x| x.eq_ignore_ascii_case(n.as_str()))
                {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        return ok;
    }
    // GROUP BY / grouping sets: our parser cross-products GROUPING SETS /
    // ROLLUP / CUBE into `group_by: Vec<Vec<Expr>>` (and dedups DISTINCT
    // sets), so PG's "groupDistinct OR exactly one expanded set" rule
    // reduces to: exactly one grouping set. An empty set (`GROUP BY ()`)
    // yields a single row.
    if !sub.group_by.is_empty() || sub.group_by_sets {
        if sub.group_by.len() != 1 {
            return false;
        }
        let mut ok = true;
        for e in &sub.group_by[0] {
            let mut refs = Vec::new();
            collect_col_refs(e, &mut refs);
            if refs.is_empty() {
                ok = false;
                break;
            }
            for (_, n) in refs {
                if !constrained
                    .iter()
                    .any(|x| x.eq_ignore_ascii_case(n.as_str()))
                {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        return ok;
    }
    // No GROUP BY but aggregates or HAVING: at most one row (PG's
    // "no groupClause, hasAggs or havingQual" case).
    let has_agg = sub.items.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => expr_has_agg(expr),
        _ => false,
    });
    if has_agg || sub.having.is_some() {
        return true;
    }
    false
}

/// v1.47: PG19's `query_is_distinct_for` set-operation branch: every branch
/// must be non-ALL (`UNION`/`INTERSECT`/`EXCEPT` without ALL dedups), and
/// all non-junk output columns of each branch must be constrained.
pub(crate) fn setop_is_distinct_for(
    eng: &Engine,
    sub: &SelectStmt,
    constrained: &HashSet<String>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    let root = match sub.set_op.as_ref() {
        Some(r) => r,
        None => return false,
    };
    // All links must be non-ALL.
    if root.chain.iter().any(|b| b.all) {
        return false;
    }
    // Every branch's non-junk outputs constrained.
    let mut branches: Vec<&SelectStmt> = vec![&root.left];
    for b in &root.chain {
        branches.push(&b.right);
    }
    for br in branches {
        let cols = match pg_subquery_col_names(eng, br, snap, own, session, &[]) {
            Some(c) => c,
            None => return false,
        };
        if cols.is_empty() {
            return false;
        }
        if !cols.iter().all(|c| {
            constrained
                .iter()
                .any(|x| x.eq_ignore_ascii_case(c.as_str()))
        }) {
            return false;
        }
    }
    let _ = (eng, snap, own, session);
    true
}

/// v1.47: does `e` contain an aggregate call? (For the no-GROUP-BY
/// aggregates/HAVING single-row rule in `subquery_is_distinct_for`.)
pub(crate) fn expr_has_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg { .. } | Expr::WithinGroup { .. } => true,
        Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Cmp {
            left: a, right: b, ..
        }
        | Expr::UserOp {
            left: a, right: b, ..
        }
        | Expr::IsDistinctFrom {
            left: a, right: b, ..
        }
        | Expr::Concat(a, b) => expr_has_agg(a) || expr_has_agg(b),
        Expr::Not(x) | Expr::Neg(x) | Expr::BitNot(x) | Expr::Cast { expr: x, .. } => {
            expr_has_agg(x)
        }
        Expr::Func { args, .. } => args.iter().any(expr_has_agg),
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            operand.as_ref().is_some_and(|x| expr_has_agg(x))
                || whens
                    .iter()
                    .any(|(w, t)| expr_has_agg(w) || expr_has_agg(t))
                || else_.as_ref().is_some_and(|x| expr_has_agg(x))
        }
        _ => false,
    }
}

/// v1.50: rewrite all `alias.col` references in the statement (SELECT items,
/// WHERE, and JOIN ONs) using the pulled-up subquery's tlist. Unqualified
/// refs to the subquery's output column names are also rewritten (they
/// resolved to the subquery before pullup).
pub(crate) fn rewrite_stmt_refs(
    stmt: &mut SelectStmt,
    alias: &str,
    items: &[SelectItem],
    pulled: &mut Vec<Expr>,
) {
    // Build the output column name set for unqualified ref detection.
    let mut out_names = Vec::new();
    for it in items {
        if let SelectItem::Expr { expr, alias: a } = it {
            let name = if let Some(an) = a {
                an.clone()
            } else if let Expr::Column { name, .. } = expr {
                name.clone()
            } else {
                continue;
            };
            out_names.push(name);
        }
    }
    // Rewrite SELECT items.
    for it in stmt.items.iter_mut() {
        if let SelectItem::Expr { expr, .. } = it {
            *expr = replace_subquery_refs(expr, alias, items, &out_names, pulled);
        }
    }
    // Rewrite WHERE.
    if let Some(w) = stmt.where_.take() {
        stmt.where_ = Some(replace_subquery_refs(&w, alias, items, &out_names, pulled));
    }
    // Rewrite JOIN ONs in the FROM tree.
    stmt.from = stmt
        .from
        .iter()
        .map(|it| rewrite_on_in_item(it, alias, items, &out_names, pulled))
        .collect();
}

pub(crate) fn rewrite_on_in_item(
    it: &FromItem,
    alias: &str,
    items: &[SelectItem],
    out_names: &[String],
    pulled: &mut Vec<Expr>,
) -> FromItem {
    match it {
        FromItem::Join {
            left,
            kind,
            right,
            on,
            using,
            natural,
            using_alias,
            alias: jalias,
            col_aliases,
        } => FromItem::Join {
            left: Box::new(rewrite_on_in_item(left, alias, items, out_names, pulled)),
            kind: *kind,
            right: Box::new(rewrite_on_in_item(right, alias, items, out_names, pulled)),
            on: on
                .as_ref()
                .map(|o| replace_subquery_refs(o, alias, items, out_names, pulled)),
            using: using.clone(),
            natural: *natural,
            using_alias: using_alias.clone(),
            alias: jalias.clone(),
            col_aliases: col_aliases.clone(),
        },
        FromItem::Derived {
            alias: da,
            sub,
            col_aliases,
            lateral,
        } => {
            let mut new_sub = sub.clone();
            rewrite_stmt_refs(&mut new_sub, alias, items, pulled);
            FromItem::Derived {
                alias: da.clone(),
                sub: new_sub,
                col_aliases: col_aliases.clone(),
                lateral: *lateral,
            }
        }
        _ => it.clone(),
    }
}

/// v1.50: replace `alias.col` (and unqualified `col` when it's a subquery
/// output name) with the pulled-up subquery's tlist expr. Non-Column
/// replacements are recorded in `pulled` for PG-text paren marking.
pub(crate) fn replace_subquery_refs(
    expr: &Expr,
    alias: &str,
    items: &[SelectItem],
    out_names: &[String],
    pulled: &mut Vec<Expr>,
) -> Expr {
    // Base case: Column ref to the subquery.
    if let Expr::Column { table, name } = expr {
        let is_sub_ref = match table {
            Some(t) => t == alias,
            None => out_names.iter().any(|n| n == name),
        };
        if is_sub_ref {
            if let Some(repl) = subquery_col_expr(items, name) {
                // v1.50: Record ALL replacements (for Result Output tlist
                // and PG-text paren marking). The paren marking filters
                // for non-Column.
                if !pulled.contains(&repl) {
                    pulled.push(repl.clone());
                }
                return repl;
            }
        }
        return expr.clone();
    }
    // Recursive cases.
    match expr {
        Expr::Arith { op, left, right } => Expr::Arith {
            op: *op,
            left: Box::new(replace_subquery_refs(left, alias, items, out_names, pulled)),
            right: Box::new(replace_subquery_refs(
                right, alias, items, out_names, pulled,
            )),
        },
        Expr::Cast {
            expr: e,
            to,
            written,
        } => Expr::Cast {
            expr: Box::new(replace_subquery_refs(e, alias, items, out_names, pulled)),
            to: *to,
            written: written.clone(),
        },
        Expr::CastNamed { expr: e, name } => Expr::CastNamed {
            expr: Box::new(replace_subquery_refs(e, alias, items, out_names, pulled)),
            name: name.clone(),
        },
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: Box::new(replace_subquery_refs(left, alias, items, out_names, pulled)),
            right: Box::new(replace_subquery_refs(
                right, alias, items, out_names, pulled,
            )),
        },
        Expr::And(a, b) => Expr::And(
            Box::new(replace_subquery_refs(a, alias, items, out_names, pulled)),
            Box::new(replace_subquery_refs(b, alias, items, out_names, pulled)),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(replace_subquery_refs(a, alias, items, out_names, pulled)),
            Box::new(replace_subquery_refs(b, alias, items, out_names, pulled)),
        ),
        Expr::Not(e) => Expr::Not(Box::new(replace_subquery_refs(
            e, alias, items, out_names, pulled,
        ))),
        Expr::Neg(e) => Expr::Neg(Box::new(replace_subquery_refs(
            e, alias, items, out_names, pulled,
        ))),
        Expr::BitNot(e) => Expr::BitNot(Box::new(replace_subquery_refs(
            e, alias, items, out_names, pulled,
        ))),
        Expr::IsNull { expr: e, neg } => Expr::IsNull {
            expr: Box::new(replace_subquery_refs(e, alias, items, out_names, pulled)),
            neg: *neg,
        },
        Expr::IsBool { expr: e, neg, val } => Expr::IsBool {
            expr: Box::new(replace_subquery_refs(e, alias, items, out_names, pulled)),
            neg: *neg,
            val: *val,
        },
        Expr::Between {
            expr: e,
            low,
            high,
            neg,
        } => Expr::Between {
            expr: Box::new(replace_subquery_refs(e, alias, items, out_names, pulled)),
            low: Box::new(replace_subquery_refs(low, alias, items, out_names, pulled)),
            high: Box::new(replace_subquery_refs(high, alias, items, out_names, pulled)),
            neg: *neg,
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| replace_subquery_refs(a, alias, items, out_names, pulled))
                .collect(),
        },
        Expr::Concat(a, b) => Expr::Concat(
            Box::new(replace_subquery_refs(a, alias, items, out_names, pulled)),
            Box::new(replace_subquery_refs(b, alias, items, out_names, pulled)),
        ),
        // For other variants (subqueries, windows, etc.), do not recurse
        // (they are not pullup-safe contexts; leave unchanged).
        _ => expr.clone(),
    }
}

/// v1.50: find the tlist expr for output column `col`.
pub(crate) fn subquery_col_expr(items: &[SelectItem], col: &str) -> Option<Expr> {
    for it in items {
        if let SelectItem::Expr { expr, alias } = it {
            let name = if let Some(a) = alias {
                a.clone()
            } else if let Expr::Column { name, .. } = expr {
                name.clone()
            } else {
                continue;
            };
            if name == col {
                return Some(expr.clone());
            }
        }
    }
    None
}

/// v1.50: check if a subquery is "simple" per PG19 `is_simple_subquery`
/// (prepjointree.c:1096-): pullup-safe when it's a single plain SELECT.
pub(crate) fn is_simple_subquery(sub: &SelectStmt) -> bool {
    if !sub.with.is_empty() {
        return false;
    }
    if sub.distinct || !sub.distinct_on.is_empty() {
        return false;
    }
    if !sub.group_by.is_empty() || sub.having.is_some() {
        return false;
    }
    if sub.limit.is_some() || sub.offset.is_some() {
        return false;
    }
    // v1.54 (drive-by #2): PG19's `is_simple_subquery` (prepjointree.c)
    // forbids `sortClause` — a subquery with ORDER BY is never "simple".
    if !sub.order_by.is_empty() {
        return false;
    }
    if sub.for_update || !sub.for_update_of.is_empty() {
        return false;
    }
    if sub.set_op.is_some() {
        return false;
    }
    if is_agg_query(sub) {
        return false;
    }
    if sub.where_.as_ref().is_some_and(contains_agg) {
        return false;
    }
    // No window functions, etc. (conservative: only allow simple exprs)
    true
}

/// v1.50: find the first pullup candidate in the FROM tree. Returns
/// (alias, tlist items, subquery WHERE, subquery FROM) for a Derived that is
/// a direct child of a Join and is a simple subquery.
pub(crate) fn find_pullup_candidate(
    from: &[FromItem],
) -> Option<(String, Vec<SelectItem>, Option<Expr>, Vec<FromItem>)> {
    for it in from {
        // v1.54: PG pulls up top-level subqueries too, not just Join
        // children (needed for inlined CTEs: `select * from (select ..)
        // x`). From-less subqueries are allowed here (PG's
        // `replace_empty_jointree`); they splice to an empty FROM.
        // LATERAL is still never pulled up (v1.51).
        if let FromItem::Derived {
            alias,
            sub,
            lateral,
            ..
        } = it
        {
            if !lateral && sub.from.len() <= 1 && is_simple_subquery(sub.as_ref()) {
                return Some((
                    alias.clone(),
                    sub.items.clone(),
                    sub.where_.clone(),
                    sub.from.clone(),
                ));
            }
        }
        if let Some(cand) = find_pullup_in_item(it) {
            return Some(cand);
        }
    }
    None
}

pub(crate) fn find_pullup_in_item(
    it: &FromItem,
) -> Option<(String, Vec<SelectItem>, Option<Expr>, Vec<FromItem>)> {
    match it {
        FromItem::Join { left, right, .. } => {
            // Check left and right for Derived.
            for child in [left.as_ref(), right.as_ref()] {
                if let FromItem::Derived {
                    alias,
                    sub,
                    lateral,
                    ..
                } = child
                {
                    // v1.51: scope guards — all required for the fixpoint
                    // to terminate and for the documented semantics:
                    // - never pull up LATERAL (the doc claims "not
                    //   LATERAL"; PG19's lateral-outer-ref restrictions
                    //   in `is_simple_subquery` aren't modeled here, so a
                    //   LATERAL pullup could expose an inner FROM-less
                    //   derived as a direct join child and wedge);
                    // - never pull up a FROM-less subquery (there is
                    //   nothing to splice; the splice keeps the Derived
                    //   and the fixpoint finds it again forever);
                    // - only single-FROM-item subqueries (the splice
                    //   replaces the Derived with exactly one item; a
                    //   multi-item FROM would likewise be kept and loop).
                    if !lateral && sub.from.len() == 1 && is_simple_subquery(sub.as_ref()) {
                        return Some((
                            alias.clone(),
                            sub.items.clone(),
                            sub.where_.clone(),
                            sub.from.clone(),
                        ));
                    }
                }
            }
            // Recurse.
            find_pullup_in_item(left).or_else(|| find_pullup_in_item(right))
        }
        FromItem::Derived { sub, .. } => {
            // Recurse into subquery's FROM (for nested pullups).
            find_pullup_candidate(&sub.from)
        }
        _ => None,
    }
}

/// v1.50: replace the Derived with `alias` by splicing in `sub_from`.
/// The subquery always has exactly one FROM item here (enforced by the
/// v1.51 scope guards in `find_pullup_in_item`), so it replaces the
/// Derived directly.
pub(crate) fn splice_pullup_from(
    from: &[FromItem],
    alias: &str,
    sub_from: &[FromItem],
) -> Vec<FromItem> {
    from.iter()
        .flat_map(|it| splice_pullup_in_item(it, alias, sub_from))
        .collect()
}

pub(crate) fn splice_pullup_in_item(
    it: &FromItem,
    alias: &str,
    sub_from: &[FromItem],
) -> Vec<FromItem> {
    match it {
        FromItem::Join {
            left,
            kind,
            right,
            on,
            using,
            natural,
            using_alias,
            alias: jalias,
            col_aliases,
        } => {
            let new_left = splice_pullup_in_item(left, alias, sub_from);
            let new_right = splice_pullup_in_item(right, alias, sub_from);
            // v1.54: from-less pullup only targets top-level Deriveds
            // (never Join children), so each side yields exactly one item.
            vec![FromItem::Join {
                left: Box::new(
                    new_left
                        .into_iter()
                        .next()
                        .expect("v1.54: join-child splice yields one item"),
                ),
                kind: *kind,
                right: Box::new(
                    new_right
                        .into_iter()
                        .next()
                        .expect("v1.54: join-child splice yields one item"),
                ),
                on: on.clone(),
                using: using.clone(),
                natural: *natural,
                using_alias: using_alias.clone(),
                alias: jalias.clone(),
                col_aliases: col_aliases.clone(),
            }]
        }
        FromItem::Derived {
            alias: dalias,
            sub,
            col_aliases,
            lateral,
        } => {
            if dalias == alias {
                // Replace with the subquery's FROM. v1.54: a from-less
                // subquery (PG's `replace_empty_jointree`) splices to
                // nothing — the Derived is removed.
                sub_from.to_vec()
            } else {
                // Recurse into the subquery.
                let mut new_sub = sub.clone();
                new_sub.from = splice_pullup_from(&sub.from, alias, sub_from);
                vec![FromItem::Derived {
                    alias: dalias.clone(),
                    sub: new_sub,
                    col_aliases: col_aliases.clone(),
                    lateral: *lateral,
                }]
            }
        }
        _ => vec![it.clone()],
    }
}

/// v1.50: PG19 `pull_up_simple_subquery` (prepjointree.c) as a preprocess
/// rewrite of the statement, running before `flip_right_joins` (PG's planner
/// order: pull_up_subqueries → reduce_outer_joins → remove_useless_joins).
///
/// A `Derived` subquery that is "simple" (single plain SELECT: no aggregates,
/// no GROUP BY/HAVING, no DISTINCT, no LIMIT/OFFSET, no setops, no CTEs, no
/// locking), is not LATERAL, has exactly one FROM item, and appears as a
/// direct child of a Join is pulled up: its FROM item is spliced into the
/// parent join in place of the subquery, references to `alias.col` in the
/// parent's SELECT/WHERE/JOIN ONs are replaced by the subquery's tlist
/// exprs, and the subquery's WHERE is ANDed into the parent's WHERE.
/// v1.54: also pulls up top-level Deriveds (not just Join children —
/// PG pulls up any simple subquery), including FROM-less subqueries
/// (PG's `replace_empty_jointree`; the Derived is removed, leaving an
/// empty FROM). Each spliced Derived increments `dead_rtes` (PG's dead
/// rtable entries for the EXPLAIN `useprefix` rule).
///
/// Returns `(rewritten_stmt, pulled_exprs)` where `pulled_exprs` are the
/// non-Column tlist exprs that were substituted (for PG-text paren marking
/// per ruleutils.c `get_special_variable`). Returns `None` if no pullup
/// applied.
pub(crate) fn pull_up_simple_subqueries(
    stmt: &SelectStmt,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<(SelectStmt, Vec<Expr>)> {
    let mut new_stmt = stmt.clone();
    let mut pulled = Vec::new();
    // v1.54: PG's dead rtable entries — one per spliced Derived.
    let mut dead_rtes = 0usize;
    // Fixpoint: pull up one subquery at a time (handles nesting).
    // v1.51: bound the iterations (defense-in-depth). Each successful
    // splice removes exactly one Derived from the whole FROM tree —
    // replacing it with its single FROM item (or nothing, if from-less),
    // which was already counted — so the loop can never need more than
    // `derived_count + 1` passes. With the scope guards in
    // `find_pullup_in_item` every candidate is spliceable, so the bound
    // is unreachable in practice.
    let max_iters = count_derived_from_items(&stmt.from) + 1;
    for _ in 0..max_iters {
        // Find the first pullup candidate: (alias, subquery items, subquery
        // where, subquery from), at a Join child or top-level position.
        let candidate = find_pullup_candidate(&new_stmt.from);
        let Some((alias, items, sub_where, sub_from)) = candidate else {
            break;
        };
        // v1.54: was the candidate the ONLY top-level FROM item? (For
        // the from-less `SELECT *` expansion below.)
        let was_single_top =
            new_stmt.from.len() == 1 && matches!(new_stmt.from[0], FromItem::Derived { .. });
        // Splice the subquery's FROM into the parent, replacing the Derived.
        new_stmt.from = splice_pullup_from(&new_stmt.from, &alias, &sub_from);
        dead_rtes += 1;
        // Rewrite SELECT items, WHERE, and all JOIN ONs in the tree.
        // v1.50: qualify unqualified tlist Columns using the schema.
        let qualified_items = qualify_tlist_exprs(&items, &sub_from, eng, snap, own, session);
        // v1.54: top-level pullup of the ONLY from item — `SELECT *`
        // expands over the subquery's tlist (PG's parse-time `*`
        // expansion), not the spliced table's columns. (For from-less
        // subqueries the FROM is empty; otherwise `*` would widen to
        // the whole table.)
        if was_single_top {
            new_stmt.items = new_stmt
                .items
                .iter()
                .flat_map(|it| match it {
                    SelectItem::All => qualified_items.clone(),
                    other => vec![other.clone()],
                })
                .collect();
        }
        rewrite_stmt_refs(&mut new_stmt, &alias, &qualified_items, &mut pulled);
        // AND the subquery's WHERE into the parent's WHERE.
        if let Some(sw) = sub_where {
            new_stmt.where_ = Some(match new_stmt.where_.take() {
                Some(pw) => Expr::And(Box::new(pw), Box::new(sw)),
                None => sw,
            });
        }
    }
    if pulled.is_empty() {
        // No substitution happened; check if FROM actually changed.
        if new_stmt.from == stmt.from {
            return None;
        }
    }
    new_stmt.dead_rtes = stmt.dead_rtes + dead_rtes;
    Some((new_stmt, pulled))
}

/// v1.51: count `Derived` FROM items in a FROM tree, recursing through
/// Joins and into subqueries. Bounds the pullup fixpoint: each splice
/// removes exactly one Derived, so the loop needs at most this many
/// iterations (+1).
pub(crate) fn count_derived_from_items(from: &[FromItem]) -> usize {
    from.iter()
        .map(|it| match it {
            FromItem::Derived { sub, .. } => 1 + count_derived_from_items(&sub.from),
            FromItem::Join { left, right, .. } => {
                count_derived_from_items(std::slice::from_ref(left))
                    + count_derived_from_items(std::slice::from_ref(right))
            }
            _ => 0,
        })
        .sum()
}

/// v1.50: qualify unqualified Column refs in tlist exprs using the subquery's
/// FROM tables and the schema. Only qualifies when unambiguous.
pub(crate) fn qualify_tlist_exprs(
    items: &[SelectItem],
    sub_from: &[FromItem],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Vec<SelectItem> {
    items
        .iter()
        .map(|it| match it {
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: qualify_expr_cols(expr, sub_from, eng, snap, own, session),
                alias: alias.clone(),
            },
            _ => it.clone(),
        })
        .collect()
}

pub(crate) fn qualify_expr_cols(
    expr: &Expr,
    from: &[FromItem],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Expr {
    match expr {
        Expr::Column { table: None, name } => {
            // Find which table has this column.
            if let Some(tname) = find_col_table(from, name, eng, snap, own, session) {
                Expr::Column {
                    table: Some(tname),
                    name: name.clone(),
                }
            } else {
                expr.clone()
            }
        }
        // Recurse for composite exprs (simple cases only).
        Expr::Cast {
            expr: e,
            to,
            written,
        } => Expr::Cast {
            expr: Box::new(qualify_expr_cols(e, from, eng, snap, own, session)),
            to: *to,
            written: written.clone(),
        },
        // v1.56: recurse into Func args, Cmp sides, And/Or so columns
        // inside them get qualified too (previously only Cast was
        // recursed into, leaving e.g. `f(x)` or `x = 1` in a subquery
        // target list with unqualified columns that PG prints qualified,
        // forcing a Debug-fallback EF).
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| qualify_expr_cols(a, from, eng, snap, own, session))
                .collect(),
        },
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: Box::new(qualify_expr_cols(left, from, eng, snap, own, session)),
            right: Box::new(qualify_expr_cols(right, from, eng, snap, own, session)),
        },
        Expr::And(a, b) => Expr::And(
            Box::new(qualify_expr_cols(a, from, eng, snap, own, session)),
            Box::new(qualify_expr_cols(b, from, eng, snap, own, session)),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(qualify_expr_cols(a, from, eng, snap, own, session)),
            Box::new(qualify_expr_cols(b, from, eng, snap, own, session)),
        ),
        _ => expr.clone(),
    }
}

pub(crate) fn find_col_table(
    from: &[FromItem],
    col: &str,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<String> {
    let mut found = None;
    for it in from {
        match it {
            FromItem::Table { name, alias, .. } => {
                let has = eng
                    .db
                    .find_table(name, snap, &[own], session)
                    .map(|t| t.columns.iter().any(|(n, _)| n == col))
                    .unwrap_or(false);
                if has {
                    if found.is_some() {
                        return None; // ambiguous
                    }
                    found = Some(alias.clone().unwrap_or_else(|| name.clone()));
                }
            }
            FromItem::Join { left, right, .. } => {
                // Check left then right (simplified: just check both).
                if let Some(t) =
                    find_col_table(std::slice::from_ref(left), col, eng, snap, own, session)
                {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(t);
                }
                if let Some(t) =
                    find_col_table(std::slice::from_ref(right), col, eng, snap, own, session)
                {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(t);
                }
            }
            _ => {}
        }
    }
    found
}
/// "We get rid of JOIN_RIGHT cases by flipping them around to become
/// JOIN_LEFT." The ON clause is carried over unmodified — PG quals reference
/// base rels by RT index, not side position, so nothing is commuted.
/// Applied to the top-level FROM list before `remove_useless_joins_from`,
/// mirroring PG's pass order (planner() runs reduce_outer_joins before
/// grouping_planner/remove_useless_joins). Subqueries are flipped by their
/// own `plan_select` invocation, so this does not recurse into Derived.
pub(crate) fn flip_right_joins(from: &[FromItem]) -> Vec<FromItem> {
    from.iter().map(flip_right_join_item).collect()
}

pub(crate) fn flip_right_join_item(it: &FromItem) -> FromItem {
    match it {
        FromItem::Join {
            left,
            kind,
            right,
            on,
            using,
            natural,
            using_alias,
            alias,
            col_aliases,
        } => {
            let (new_left, new_kind, new_right) = if *kind == JoinKind::Right {
                (
                    flip_right_join_item(right),
                    JoinKind::Left,
                    flip_right_join_item(left),
                )
            } else {
                (
                    flip_right_join_item(left),
                    *kind,
                    flip_right_join_item(right),
                )
            };
            FromItem::Join {
                left: Box::new(new_left),
                kind: new_kind,
                right: Box::new(new_right),
                on: on.clone(),
                using: using.clone(),
                natural: *natural,
                using_alias: using_alias.clone(),
                alias: alias.clone(),
                col_aliases: col_aliases.clone(),
            }
        }
        other => other.clone(),
    }
}

/// v1.47: PG19's `remove_useless_joins` (analyzejoins.c) — full version.
///
/// A LEFT JOIN whose inner side is provably distinct on the join keys can
/// neither duplicate nor (as a LEFT join) filter the outer side's rows, so
/// when nothing else references the inner range the join is useless.
///
/// Fixpoint loop mirroring PG's `goto restart`: each iteration recomputes
/// ON-qualifier occurrence counts over the CURRENT tree and removes exactly
/// one provably-useless join (innermost-first), then restarts. Removing a
/// join drops its entire ON clause — a LEFT JOIN's ON never filters its
/// left side, so even conjuncts not touching the inner range are safe to
/// drop with it. Chained removals (where an outer ON references an inner
/// range that becomes unreferenced after an inner removal) fall out
/// naturally. Counts are over the whole FROM list so a qualifier referenced
/// by another top-level item's ON blocks removal.
pub(crate) fn remove_useless_joins_from(
    eng: &Engine,
    from: &[FromItem],
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Vec<FromItem> {
    let mut rewritten: Vec<FromItem> = from.to_vec();
    loop {
        let mut on_counts = HashMap::new();
        for it in &rewritten {
            collect_on_qual_counts(it, &mut on_counts);
        }
        let mut progress = false;
        for it in rewritten.iter_mut() {
            if let Some(next) =
                try_remove_one_useless_join(eng, it, stmt, &on_counts, snap, own, session)
            {
                *it = next;
                progress = true;
                break;
            }
        }
        if !progress {
            return rewritten;
        }
    }
}

/// v1.47: try to remove a single useless join from `item`, innermost-first.
/// Returns the rewritten item when one join was removed, else `None`. Only
/// one removal per call — the fixpoint in `remove_useless_joins` restarts
/// after each, mirroring PG's `goto restart`.
pub(crate) fn try_remove_one_useless_join(
    eng: &Engine,
    item: &FromItem,
    stmt: &SelectStmt,
    on_counts: &HashMap<String, usize>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<FromItem> {
    let FromItem::Join {
        left,
        right,
        kind,
        on,
        using,
        natural,
        using_alias,
        alias,
        col_aliases,
    } = item
    else {
        return None;
    };
    let rebuild = |l: FromItem, r: FromItem| {
        Some(FromItem::Join {
            left: Box::new(l),
            right: Box::new(r),
            kind: *kind,
            on: on.clone(),
            using: using.clone(),
            natural: *natural,
            using_alias: using_alias.clone(),
            alias: alias.clone(),
            col_aliases: col_aliases.clone(),
        })
    };
    // Innermost-first: descend before considering this join.
    if let Some(new_left) =
        try_remove_one_useless_join(eng, left, stmt, on_counts, snap, own, session)
    {
        return rebuild(new_left, (**right).clone());
    }
    if let Some(new_right) =
        try_remove_one_useless_join(eng, right, stmt, on_counts, snap, own, session)
    {
        return rebuild((**left).clone(), new_right);
    }
    // Try removing this join; success returns the bare outer side (the
    // entire ON is dropped — a LEFT JOIN's ON never filters its left).
    if join_is_removable(eng, item, stmt, on_counts, snap, own, session) {
        return Some((**left).clone());
    }
    None
}

/// v1.47: PG19's `join_is_removable` (analyzejoins.c) — is this LEFT JOIN
/// provably useless?
///
/// Requirements (all must hold):
/// - plain `LEFT JOIN ... ON` (no USING/NATURAL/alias; v1.45+ never
///   attempts RIGHT JOIN removal or self-join elimination).
/// - every ANDed ON conjunct is `inner_col = outer_expr` with the inner
///   side exactly `rqual.col` (plain column, fail closed on casts), the
///   outer side referencing only left-subtree qualifiers (no `rqual`, no
///   unqualified, no lateral/outer refs), and neither side volatile nor
///   subquery-containing.
/// - inner side is a plain table with a uniqueness guarantee (PK / UNIQUE
///   / planner-usable unique index) covering ALL constrained key columns,
///   with join-equality/type compatibility — or a derived table provably
///   distinct on the constrained outputs (`subquery_is_distinct_for`).
/// - `rqual` appears in no other ON clause (per-iteration counts over the
///   current tree; the candidate's own ON occurrences are excluded) and in
///   no non-FROM expression of the original statement.
pub(crate) fn join_is_removable(
    eng: &Engine,
    item: &FromItem,
    stmt: &SelectStmt,
    on_counts: &HashMap<String, usize>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    let FromItem::Join {
        left,
        right,
        kind,
        on,
        using,
        natural,
        using_alias,
        alias,
        col_aliases,
    } = item
    else {
        return false;
    };
    if !matches!(kind, JoinKind::Left)
        || using_alias.is_some()
        || alias.is_some()
        || !using.is_empty()
        || *natural
        || !col_aliases.is_empty()
    {
        return false;
    }
    let Some(on_expr) = on else {
        return false;
    };

    // Inner range qualifier + kind.
    enum Inner<'a> {
        Table(&'a str),
        Derived(&'a SelectStmt),
    }
    let (rqual, inner) = match right.as_ref() {
        FromItem::Table {
            name,
            alias,
            col_aliases: rca,
            ..
        } => {
            if !rca.is_empty() {
                return false;
            }
            let rq = alias.clone().unwrap_or_else(|| name.clone());
            (rq, Inner::Table(name.as_str()))
        }
        FromItem::Derived {
            sub,
            alias,
            col_aliases: rca,
            ..
        } => {
            if !rca.is_empty() {
                return false;
            }
            (alias.clone(), Inner::Derived(sub.as_ref()))
        }
        _ => return false,
    };

    // v1.50: helpers for unqualified inner-col resolution.
    fn inner_has_col(
        inner: &Inner,
        col: &str,
        eng: &Engine,
        snap: &Snapshot,
        own: u64,
        session: u64,
    ) -> bool {
        match inner {
            Inner::Table(name) => eng
                .db
                .find_table(name, snap, &[own], session)
                .map(|t| t.columns.iter().any(|(n, _)| n == col))
                .unwrap_or(false),
            Inner::Derived(_) => false, // fail closed (scoped: Table only)
        }
    }
    fn left_has_col(
        left: &FromItem,
        col: &str,
        eng: &Engine,
        snap: &Snapshot,
        own: u64,
        session: u64,
    ) -> bool {
        // Check if any table in the left subtree has this column.
        match left {
            FromItem::Table { name, .. } => eng
                .db
                .find_table(name, snap, &[own], session)
                .map(|t| t.columns.iter().any(|(n, _)| n == col))
                .unwrap_or(false),
            FromItem::Join {
                left: l, right: r, ..
            } => {
                left_has_col(l, col, eng, snap, own, session)
                    || left_has_col(r, col, eng, snap, own, session)
            }
            FromItem::Derived { .. } => false, // fail closed
            _ => false,
        }
    }
    /// v1.50: all column references in `e` carry qualifiers drawn from
    /// `allowed`, or are unqualified Columns that resolve to the left
    /// subtree (and not to the inner table).
    fn expr_refs_only_or_unqualified_left(
        e: &Expr,
        allowed: &HashSet<String>,
        left: &FromItem,
        eng: &Engine,
        snap: &Snapshot,
        own: u64,
        session: u64,
    ) -> bool {
        let mut refs = Vec::new();
        collect_col_refs(e, &mut refs);
        refs.iter().all(|(t, n)| match t {
            Some(q) => allowed.contains(q),
            None => {
                // Unqualified: must resolve to left (and be unambiguous).
                left_has_col(left, n, eng, snap, own, session)
            }
        })
    }

    // Qualifiers visible on the left (for the outer-side check).
    let left_quals = removal_left_quals(left);

    // Collect the usable `inner_col = outer_expr` equalities (PG's
    // mergejoinable clause_list). Conjuncts that are not usable equalities
    // (e.g. `t2.id = t3.id`, which does not touch the inner range) are
    // simply dropped with the ON -- a LEFT JOIN's ON never filters its left
    // side, so they cannot affect the removal's safety.
    let mut constrained: HashSet<String> = HashSet::new();
    // (inner col name, outer-side type) per usable conjunct, for the
    // type-compatibility check.
    let mut conjunct_types: Vec<(String, Option<ColType>)> = Vec::new();
    for c in split_conjuncts(on_expr) {
        let Expr::Cmp {
            op: CmpOp::Eq,
            left: a,
            right: b,
        } = c
        else {
            continue;
        };
        if expr_is_volatile(&eng.db, a)
            || expr_is_volatile(&eng.db, b)
            || expr_has_subquery(a)
            || expr_has_subquery(b)
        {
            continue;
        }
        // One side exactly `rqual.col`; the other side outer-only.
        // v1.50: also accept unqualified `col` on the inner side when it
        // unambiguously belongs to the inner table (PG resolves Vars by RT
        // index; unqualified SQL refs are resolved before analyzejoins).
        let (inner_col, outer): (&str, &Expr) = match (&**a, &**b) {
            (
                Expr::Column {
                    table: Some(t1),
                    name: n1,
                },
                other,
            ) if t1 == &rqual => (n1.as_str(), other),
            (
                other,
                Expr::Column {
                    table: Some(t2),
                    name: n2,
                },
            ) if t2 == &rqual => (n2.as_str(), other),
            (
                Expr::Column {
                    table: None,
                    name: n1,
                },
                other,
            ) if inner_has_col(&inner, n1, eng, snap, own, session)
                && !left_has_col(left, n1, eng, snap, own, session) =>
            {
                (n1.as_str(), other)
            }
            (
                other,
                Expr::Column {
                    table: None,
                    name: n2,
                },
            ) if inner_has_col(&inner, n2, eng, snap, own, session)
                && !left_has_col(left, n2, eng, snap, own, session) =>
            {
                (n2.as_str(), other)
            }
            _ => continue,
        };
        // v1.50: allow unqualified outer refs that resolve to the left.
        if !expr_refs_only_or_unqualified_left(outer, &left_quals, left, eng, snap, own, session) {
            continue;
        }
        // Outer-side type for the compatibility check (simple qualified
        // columns only, resolved against the left subtree; anything else
        // fails closed below).
        // v1.50: also resolve unqualified columns against the left.
        let outer_ty = match outer {
            Expr::Column {
                table: Some(qt),
                name: qn,
            } => removal_col_type(eng, left, qt, qn, snap, own, session),
            Expr::Column {
                table: None,
                name: qn,
            } => {
                // Find the qualifier in left_quals that has this column.
                left_quals
                    .iter()
                    .find_map(|q| removal_col_type(eng, left, q, qn, snap, own, session))
            }
            _ => None,
        };
        constrained.insert(inner_col.to_string());
        conjunct_types.push((inner_col.to_string(), outer_ty));
    }
    if constrained.is_empty() {
        return false;
    }

    // Type compatibility: the join's `=` must agree with the equality the
    // uniqueness proof relies on (PG's `equality_ops_are_compatible`).
    match &inner {
        Inner::Table(rname) => {
            for (col, outer_ty) in &conjunct_types {
                let inner_ty =
                    removal_col_type(eng, right, rqual.as_str(), col, snap, own, session);
                match (inner_ty, outer_ty) {
                    (Some(it), Some(ot)) => {
                        if !removal_types_compatible(it, *ot) {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            if !table_has_unique_for(eng, rname, &constrained, snap, own, session) {
                return false;
            }
        }
        Inner::Derived(sub) => {
            let out_types = subquery_output_types(eng, sub, snap, own, session);
            for (col, outer_ty) in &conjunct_types {
                let inner_ty = out_types.get(&col.to_lowercase()).copied();
                match (inner_ty, outer_ty) {
                    (Some(it), Some(ot)) => {
                        if !removal_types_compatible(it, *ot) {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            if !subquery_is_distinct_for(eng, sub, &constrained, snap, own, session) {
                return false;
            }
        }
    }

    // `rqual` must occur in no ON other than this join's own. Count this
    // ON's occurrences of `rqual` and require the tree-wide total to match.
    let mut own_refs = Vec::new();
    collect_col_refs(on_expr, &mut own_refs);
    let own_on_count = own_refs
        .iter()
        .filter(|(t, _)| t.as_deref() == Some(rqual.as_str()))
        .count();
    if on_counts.get(&rqual).copied().unwrap_or(0) != own_on_count {
        return false;
    }

    // Non-FROM expressions of the original statement: `rqual` must appear
    // nowhere, and no unqualified references (fail closed). `SELECT *`
    // expands to the inner range's columns too, so it blocks removal;
    // `t.*` blocks only when `t` is the inner range itself.
    if stmt.items.iter().any(|item2| match item2 {
        SelectItem::Expr { .. } => false,
        SelectItem::All => true,
        SelectItem::AllOf(q) => q == &rqual,
    }) {
        return false;
    }
    let mut quals = HashSet::new();
    // v1.51: resolve unqualified refs against the statement's FROM tree
    // instead of failing closed. PG resolves Vars by RT index, so an
    // unqualified column that unambiguously belongs to a range other than
    // the inner one can never be an inner Var. Only genuinely ambiguous
    // or unresolvable refs (or refs to the inner range itself) block.
    let mut blocked = false;
    let mut push = |e: &Expr| {
        let mut refs = Vec::new();
        collect_col_refs(e, &mut refs);
        for (t, n) in refs {
            match t {
                Some(q) => {
                    quals.insert(q.clone());
                }
                None => match find_col_table(&stmt.from, &n, eng, snap, own, session) {
                    Some(q) => {
                        quals.insert(q);
                    }
                    None => blocked = true,
                },
            }
        }
    };
    for item2 in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item2 {
            push(expr);
        }
    }
    if let Some(w) = &stmt.where_ {
        push(w);
    }
    for g in stmt.group_by.iter().flatten() {
        push(g);
    }
    if let Some(h) = &stmt.having {
        push(h);
    }
    if blocked || quals.contains(&rqual) {
        return false;
    }

    true
}

pub(crate) fn collect_on_qual_counts(item: &FromItem, counts: &mut HashMap<String, usize>) {
    match item {
        FromItem::Join {
            on, left, right, ..
        } => {
            if let Some(e) = on {
                let mut refs = Vec::new();
                collect_col_refs(e, &mut refs);
                for (t, _) in refs {
                    if let Some(q) = t {
                        *counts.entry(q.clone()).or_insert(0) += 1;
                    }
                }
            }
            collect_on_qual_counts(left, counts);
            collect_on_qual_counts(right, counts);
        }
        FromItem::Derived { sub, .. } => {
            for f in &sub.from {
                collect_on_qual_counts(f, counts);
            }
        }
        _ => {}
    }
}
