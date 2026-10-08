// v1.78 mechanical split: moved verbatim from src/exec.rs (16170-18552).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ============================================================================
// v1.77: shared self-join-elimination conjunct classifier (v1.73
// top-level cross shape; v1.77 nested inner joins). Hoisted from
// `remove_useless_self_joins_from` so both shapes share the proof.
pub(crate) enum SjeClass<'x> {
    /// `=` with refs on both sides, one side q1-only and the other q2-only.
    Join { left: &'x Expr, right: &'x Expr },
    /// `q2.col = <column-free expr>`.
    Kept { col: String, conj: &'x Expr },
    /// `q1.col = <column-free expr>`.
    Removed { conj: &'x Expr },
    /// Anything else: rewritten and kept, never used in the proof.
    Other(&'x Expr),
}

/// v1.77: classify conjuncts for the SJE 1:1 proof. Every column
/// reference must carry qualifier q1 (removed instance) or q2 (kept
/// instance); unqualified or foreign references fail closed. Returns
/// the classes in written order.
pub(crate) fn sje_classify_conjuncts<'x>(
    eng: &Engine,
    conjuncts: Vec<&'x Expr>,
    q1: &str,
    q2: &str,
) -> Option<Vec<SjeClass<'x>>> {
    let mut classes: Vec<SjeClass> = Vec::new();
    for c in conjuncts {
        let mut refs: Vec<(Option<String>, String)> = Vec::new();
        collect_col_refs(c, &mut refs);
        if refs
            .iter()
            .any(|(t, _)| t.as_deref() != Some(q1) && t.as_deref() != Some(q2))
        {
            return None;
        }
        let usable = matches!(c, Expr::Cmp { op: CmpOp::Eq, .. })
            && !expr_is_volatile(&eng.db, c)
            && !expr_has_subquery(c);
        if !usable {
            classes.push(SjeClass::Other(c));
            continue;
        }
        let Expr::Cmp { left, right, .. } = c else {
            unreachable!("v1.77: guarded by matches! above")
        };
        let mut lrefs: Vec<(Option<String>, String)> = Vec::new();
        let mut rrefs: Vec<(Option<String>, String)> = Vec::new();
        collect_col_refs(left, &mut lrefs);
        collect_col_refs(right, &mut rrefs);
        let lq1 = lrefs.iter().any(|(t, _)| t.as_deref() == Some(q1));
        let lq2 = lrefs.iter().any(|(t, _)| t.as_deref() == Some(q2));
        let rq1 = rrefs.iter().any(|(t, _)| t.as_deref() == Some(q1));
        let rq2 = rrefs.iter().any(|(t, _)| t.as_deref() == Some(q2));
        // Cross-instance: one side q1-only, the other q2-only.
        if (lq1 && !lq2 && rq2 && !rq1) || (lq2 && !lq1 && rq1 && !rq2) {
            classes.push(SjeClass::Join { left, right });
            continue;
        }
        // Single-side `plain_col = <column-free expr>` (kept or removed).
        let mut done = false;
        for (q, kept) in [(q1, false), (q2, true)] {
            for (e_col, r_col, r_free) in [(&**left, &lrefs, &rrefs), (&**right, &rrefs, &lrefs)] {
                let col_side_ok =
                    !r_col.is_empty() && r_col.iter().all(|(t, _)| t.as_deref() == Some(q));
                if !col_side_ok || !r_free.is_empty() {
                    continue;
                }
                if let Expr::Column {
                    table: Some(t),
                    name,
                } = e_col
                {
                    if t == q {
                        if kept {
                            classes.push(SjeClass::Kept {
                                col: name.clone(),
                                conj: c,
                            });
                        } else {
                            classes.push(SjeClass::Removed { conj: c });
                        }
                        done = true;
                        break;
                    }
                }
            }
            if done {
                break;
            }
        }
        if !done {
            classes.push(SjeClass::Other(c));
        }
    }
    Some(classes)
}

/// v1.77: the 1:1 uniqueness proof shared by v1.73/v1.77 (PG19
/// `innerrel_is_unique_ext` / `relation_has_unique_index_for` /
/// `match_unique_clauses`). Some unique key (PK / UNIQUE /
/// planner-usable unique index) of `tname` must have every column
/// constrained by a mergejoinable `=` whose kept-side operand is the
/// plain column — either a join clause across the two instances, or a
/// kept-side `col = <column-free expr>` restriction (PG19's
/// uclauses/extra_clauses). Every uclause on a winning-key column
/// needs a side-normalized identical counterpart among the removed
/// instance's restrictions.
pub(crate) fn sje_prove_unique(
    eng: &Engine,
    tname: &str,
    classes: &[SjeClass],
    q1: &str,
    q2: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<()> {
    let mut constrained: HashSet<String> = HashSet::new();
    // Kept-side restrictions constraining plain columns = PG19's uclauses.
    let mut uclauses: Vec<(String, &Expr)> = Vec::new();
    for cl in classes {
        match cl {
            SjeClass::Join { left, right } => {
                for side in [*left, *right] {
                    if let Expr::Column {
                        table: Some(t),
                        name,
                    } = side
                    {
                        if t == q2 {
                            constrained.insert(name.clone());
                        }
                    }
                }
            }
            SjeClass::Kept { col, conj } => {
                constrained.insert(col.clone());
                uclauses.push((col.clone(), conj));
            }
            SjeClass::Other(_) | SjeClass::Removed { .. } => {}
        }
    }
    if constrained.is_empty() {
        return None;
    }
    // First covering unique key wins; PG19 commits to it without fallback.
    let key_cols = sje_covering_unique_key(eng, tname, &constrained, snap, own, session)?;
    // PG19 `match_unique_clauses`.
    let removed_conjs: Vec<&Expr> = classes
        .iter()
        .filter_map(|cl| match cl {
            SjeClass::Removed { conj } => Some(*conj),
            _ => None,
        })
        .collect();
    for (ucol, uconj) in &uclauses {
        if !key_cols.iter().any(|k| k.eq_ignore_ascii_case(ucol)) {
            continue;
        }
        let mut matched = false;
        for rconj in &removed_conjs {
            let rw = sje_rewrite_qual(rconj, q1, q2)?;
            if sje_eq_equal(&rw, uconj) {
                matched = true;
                break;
            }
        }
        if !matched {
            return None;
        }
    }
    Some(())
}

/// v1.77: rewritten survivors for an eliminated join's clauses (PG19
/// `remove_self_join_rel` + `replace_relid_callback`). A rewritten
/// clause that becomes `X = X` turns into `X IS NOT NULL`;
/// synthesized NullTests render first (PG19's cost-based qual
/// ordering puts the cheap NullTest first), then the rest in written
/// order with structural and commuted duplicates dropped. Returns
/// None when nothing survives (v1.73's commit gate).
pub(crate) fn sje_survivor_exprs(classes: &[SjeClass], q1: &str, q2: &str) -> Option<Vec<Expr>> {
    let mut notnulls: Vec<Expr> = Vec::new();
    let mut rest: Vec<Expr> = Vec::new();
    for cl in classes {
        match cl {
            SjeClass::Join { left, right } => {
                let l2 = sje_rewrite_qual(left, q1, q2)?;
                let r2 = sje_rewrite_qual(right, q1, q2)?;
                if l2 == r2 {
                    // PG19 `replace_relid_callback`: `X = X` -> `X IS NOT NULL`.
                    let nn = Expr::IsNull {
                        expr: Box::new(l2),
                        neg: true,
                    };
                    if !notnulls.iter().any(|s| s == &nn) {
                        notnulls.push(nn);
                    }
                } else {
                    sje_push_dedup(&mut rest, sje_normalize_eq(l2, r2));
                }
            }
            SjeClass::Kept { conj, .. }
            | SjeClass::Removed { conj, .. }
            | SjeClass::Other(conj) => {
                let rw = sje_rewrite_qual(conj, q1, q2)?;
                sje_push_dedup(&mut rest, sje_normalize_eq_expr(rw));
            }
        }
    }
    notnulls.extend(rest);
    if notnulls.is_empty() {
        return None;
    }
    Some(notnulls)
}

/// v1.77: rewrite the SELECT items for a self-join elimination (PG19
/// `remove_self_join_rel` keeps both column sets, re-pointed at the
/// kept instance). Bare `*` fails closed in the nested case (it spans
/// tables beyond the pair); `q1.*`/`q2.*` expand to the kept
/// instance's columns; anything else is qualifier-rewritten.
pub(crate) fn sje_rewrite_items(
    eng: &Engine,
    stmt: &SelectStmt,
    tname: &str,
    q1: &str,
    q2: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
    nested: bool,
) -> Option<Vec<SelectItem>> {
    let tcols: Vec<String> = eng
        .db
        .find_table(tname, snap, &[own], session)
        .map(|t| t.columns.iter().map(|(n, _)| n.clone()).collect())
        .unwrap_or_default();
    if tcols.is_empty() {
        return None;
    }
    let mut new_items: Vec<SelectItem> = Vec::with_capacity(stmt.items.len());
    for item in &stmt.items {
        match item {
            SelectItem::All if !nested => {
                // PG19 keeps both column sets, re-pointed at the kept
                // instance (oracle: `a | b | c | a | b | c`). Only
                // sound when q1/q2 are the whole FROM (v1.73 shape).
                for _ in 0..2 {
                    for cn in &tcols {
                        new_items.push(SelectItem::Expr {
                            expr: Expr::Column {
                                table: Some(q2.to_string()),
                                name: cn.clone(),
                            },
                            alias: None,
                        });
                    }
                }
            }
            SelectItem::All => return None,
            SelectItem::AllOf(q) if q == q1 || q == q2 => {
                for cn in &tcols {
                    new_items.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(q2.to_string()),
                            name: cn.clone(),
                        },
                        alias: None,
                    });
                }
            }
            // A different table's `t.*` survives untouched (nested only;
            // unreachable in the v1.73 shape, which fails closed anyway).
            SelectItem::AllOf(_) if nested => new_items.push(item.clone()),
            SelectItem::AllOf(_) => return None,
            SelectItem::Expr { expr, alias } => new_items.push(SelectItem::Expr {
                expr: sje_rewrite_qual(expr, q1, q2)?,
                alias: alias.clone(),
            }),
        }
    }
    Some(new_items)
}

// v1.73: PG19's `remove_useless_self_joins`
// (`src/backend/optimizer/plan/analyzejoins.c`) — self-join elimination
// for plain-column unique keys.
//
// For a top-level cross self-join `FROM t j1, t j2` (exactly two plain
// table items, same table, distinct qualifiers), when the join is
// provably a 1:1 row match, the first-listed instance is removed and
// every reference to its qualifier is rewritten to the kept instance —
// PG19's `remove_self_join_rel`, which rewrites Vars in the parse tree
// and targetlist (so `SELECT *` keeps both column sets, valued from the
// kept instance). A rewritten join clause that becomes `X = X` is
// replaced by `X IS NOT NULL` (PG19 `replace_relid_callback`); duplicate
// and commuted-duplicate conjuncts are elided.
//
// The 1:1 proof (PG19 `innerrel_is_unique_ext` /
// `relation_has_unique_index_for` / `match_unique_clauses`):
// - some unique key (PK / UNIQUE / planner-usable unique index) of `t`
//   has every column constrained by a mergejoinable `=` clause whose
//   kept-side operand is the plain column — either a join clause across
//   the two instances, or a kept-side `col = <column-free expr>`
//   restriction (the latter are PG19's `uclauses`/`extra_clauses`);
// - every such kept-side restriction has a side-normalized identical
//   counterpart among the removed instance's restrictions. This check
//   is load-bearing for soundness: without it, `j1.b = j2.b AND
//   2 = j2.a` under unique index (a,b) would wrongly eliminate, since
//   several j1 rows can share one j2 row.
//
// Mergejoinable here means `CmpOp::Eq`, non-volatile
// (`expr_is_volatile`), and subquery-free — the properties PG19's
// `mergeopfamilies` test guarantees for this purpose. Expression unique
// indexes are not considered (deferred to v1.74).
//
// Observable output rules (verified against a live PG19beta3; the exact
// internal pass that commutes `1 = j2.a` to `(a = 1)` was not isolated,
// so the implementation reproduces the observed text):
// - synthesized `IS NOT NULL`s render first (PG19's cost-based qual
//   ordering puts the cheap NullTest first);
// - surviving `col = <column-free>` equalities render column-left;
// - structural and commuted duplicates are dropped.
//
// Returns `Some(new_stmt)` when the rewrite applied, else `None`.
// Fail-closed: anything but the exact 2-table cross shape; CTEs, set
// operations, FOR UPDATE, subqueries anywhere in the statement,
// unqualified or foreign column references, volatile conjuncts, no
// covering unique key, and unmatched unique-clause counterparts.
pub(crate) fn remove_useless_self_joins_from(
    eng: &Engine,
    from: &[FromItem],
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<SelectStmt> {
    // --- Shape gate: a cross self-join of one table. The parser
    // represents `FROM sj j1, sj j2` as a single `FromItem::Join` with
    // `JoinKind::Cross`; accept that (or, defensively, two top-level
    // plain tables with the same name). The first-listed instance (q1)
    // is removed; the second (q2) is kept, matching PG19's relid order.
    let Some((tname, q1, q2, kept_item)) = (|| {
        if from.len() == 1 {
            if let FromItem::Join {
                left,
                kind: JoinKind::Cross,
                right,
                on: None,
                using,
                natural,
                ..
            } = &from[0]
            {
                if !using.is_empty() || *natural {
                    return None;
                }
                if let (
                    FromItem::Table {
                        name: n1,
                        alias: a1,
                        ..
                    },
                    FromItem::Table {
                        name: n2,
                        alias: a2,
                        ..
                    },
                ) = (&**left, &**right)
                {
                    if n1 == n2 {
                        let q1 = a1.clone().unwrap_or_else(|| n1.clone());
                        let q2 = a2.clone().unwrap_or_else(|| n2.clone());
                        if q1 != q2 {
                            return Some((n1.clone(), q1, q2, (**right).clone()));
                        }
                    }
                }
            }
            return None;
        }
        if from.len() == 2 {
            if let (
                FromItem::Table {
                    name: n1,
                    alias: a1,
                    ..
                },
                FromItem::Table {
                    name: n2,
                    alias: a2,
                    ..
                },
            ) = (&from[0], &from[1])
            {
                if n1 == n2 {
                    let q1 = a1.clone().unwrap_or_else(|| n1.clone());
                    let q2 = a2.clone().unwrap_or_else(|| n2.clone());
                    if q1 != q2 {
                        return Some((n1.clone(), q1, q2, from[1].clone()));
                    }
                }
            }
        }
        None
    })() else {
        return None;
    };
    // --- Fail closed on statement features that complicate rewriting. ---
    if !stmt.with.is_empty()
        || stmt.set_op.is_some()
        || stmt.for_update
        || !stmt.for_update_of.is_empty()
    {
        return None;
    }
    if sje_stmt_has_subquery(stmt) {
        return None;
    }
    let where_ = stmt.where_.as_ref()?;

    // --- Classify WHERE conjuncts (shared v1.77 helper). ---
    // `split_conjuncts` uses a stack and returns conjuncts in reverse
    // written order; reverse to restore it (PG's output follows written
    // order for equal-cost clauses).
    let conjuncts: Vec<&Expr> = split_conjuncts(where_).into_iter().rev().collect();
    let classes = sje_classify_conjuncts(eng, conjuncts, &q1, &q2)?;

    // --- Uniqueness proof (shared v1.77 helper). ---
    sje_prove_unique(eng, &tname, &classes, &q1, &q2, snap, own, session)?;

    // --- Rewrite (PG19 `remove_self_join_rel`). ---
    let new_items = sje_rewrite_items(eng, stmt, &tname, &q1, &q2, snap, own, session, false)?;
    // New WHERE: synthesized IS NOT NULLs first (PG19's cost-based qual
    // ordering puts the cheap NullTest first), then the rest in original
    // relative order. `col = <column-free>` equalities normalize to
    // column-left (observed PG19 output); structural and commuted
    // duplicates are dropped (shared v1.77 helper).
    let survivors = sje_survivor_exprs(&classes, &q1, &q2)?;
    let mut it = survivors.into_iter();
    let first = it.next().unwrap();
    let new_where = Some(it.fold(first, |a, b| Expr::And(Box::new(a), Box::new(b))));

    // Rewrite remaining Expr positions.
    let rw_vec = |v: &[Expr]| -> Option<Vec<Expr>> {
        v.iter().map(|e| sje_rewrite_qual(e, &q1, &q2)).collect()
    };
    let mut new_stmt = stmt.clone();
    new_stmt.items = new_items;
    new_stmt.from = vec![kept_item];
    new_stmt.where_ = new_where;
    new_stmt.distinct_on = rw_vec(&stmt.distinct_on)?;
    new_stmt.group_by = stmt
        .group_by
        .iter()
        .map(|set| rw_vec(set))
        .collect::<Option<Vec<_>>>()?;
    new_stmt.having = match &stmt.having {
        Some(h) => Some(sje_rewrite_qual(h, &q1, &q2)?),
        None => None,
    };
    new_stmt.order_by = stmt
        .order_by
        .iter()
        .map(|t| {
            Some(OrderTerm {
                expr: sje_rewrite_qual(&t.expr, &q1, &q2)?,
                desc: t.desc,
                nulls_first: t.nulls_first,
            })
        })
        .collect::<Option<Vec<_>>>()?;

    // --- Post-rewrite verification: no reference to the removed
    // qualifier may survive anywhere. ---
    let refs_q1 = sje_stmt_refs_qual(&new_stmt, &q1);
    if refs_q1 {
        return None;
    }
    Some(new_stmt)
}

// v1.77: PG19 `remove_useless_self_joins` for nested inner joins
// (analyzejoins.c `remove_self_joins_recurse` descending into
// sub-joinlists). v1.73 handled only the top-level 2-table cross
// shape; this applies the same 1:1 proof to an inner `Join` node
// nested anywhere in the FROM tree whose left/right are plain scans
// of one table with distinct qualifiers, with the proof clauses in
// the node's ON clause (PG19 `generate_join_implied_equalities`
// over the pair's joinrelids).
//
// Grounded in PG19 source (postgresql-19beta3):
// - `pull_up_simple_subquery` (prep/prepjointree.c) pulls the
//   non-lateral simple subquery up despite the FULL JOIN above (only
//   lateral refs are blocked by an enclosing outer join), so the
//   target's FROM is flat: emp1, t1, t2, t3.
// - `remove_self_joins_recurse` (plan/analyzejoins.c) descends into
//   sub-joinlists; for the group {t1,t2,t3} (same relid) the pair
//   (t2,t3) yields `t2.id=t3.id` from
//   `generate_join_implied_equalities`, `split_selfjoin_quals`
//   classifies it as a self-join qual, and `innerrel_is_unique_ext`
//   proves t2 unique via the PK on id. The jinfo_check passes: both
//   are on the same side of the FULL JOIN. t2 (lower relid) is
//   removed, t3 kept; `replace_relid_callback` turns the rewritten
//   `t3.id=t3.id` into `t3.id IS NOT NULL`, re-attached to the kept
//   rel's baserestrictinfo.
// - `make_one_row_result` (plan/createplan.c) sets the dummy Result's
//   relids to the final join rel {emp1,t1,t3}, RESULT_TYPE_JOIN.
// - `show_result_replacement_info` (commands/explain.c) iterates
//   relids ascending, skips RTE_JOIN entries, names from
//   rtable_names — the oracle's `Replaces: Join on emp1, t1, t3`.
//
// Fail-closed: only INNER join nodes (a pair is always on one side of
// any enclosing outer join, matching PG19's jinfo_check); the node's
// ON must be Some (PG19's no-qual degenerate case needs
// baserestrictinfo matching, not implemented); USING/NATURAL and
// join aliases fail closed; the v1.73 statement gates apply
// unchanged. One pair per milestone (first in tree order).
pub(crate) fn remove_useless_self_joins_nested_from(
    eng: &Engine,
    from: &[FromItem],
    stmt: &SelectStmt,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<SelectStmt> {
    // --- Statement gates (same as v1.73). ---
    if !stmt.with.is_empty()
        || stmt.set_op.is_some()
        || stmt.for_update
        || !stmt.for_update_of.is_empty()
    {
        return None;
    }
    if sje_stmt_has_subquery(stmt) {
        return None;
    }
    // Find the first removable pair in tree order.
    let mut target: Option<(String, String, String, Expr)> = None;
    for item in from {
        if let Some(t) = sje_nested_find(item) {
            target = Some(t);
            break;
        }
    }
    let (tname, q1, q2, on) = target?;
    // Proof over the node's ON conjuncts (PG19's joinrelid quals for
    // the pair).
    // `split_conjuncts` returns conjuncts in reverse written order;
    // restore it.
    let conjuncts: Vec<&Expr> = split_conjuncts(&on).into_iter().rev().collect();
    let classes = sje_classify_conjuncts(eng, conjuncts, &q1, &q2)?;
    sje_prove_unique(eng, &tname, &classes, &q1, &q2, snap, own, session)?;
    let survivors = sje_survivor_exprs(&classes, &q1, &q2)?;
    // v1.77: PG19's `match_unique_clauses` gate — if the WHERE clause
    // references the removed qualifier, fail closed. Rewriting WHERE
    // quals (q1→q2) is only sound when the kept instance carries the
    // same baserestrictinfo; the conservative fail-closed is to not
    // eliminate at all. (The target's WHERE is const-false, unaffected.)
    if let Some(w) = &stmt.where_ {
        if sje_expr_refs_qual(w, &q1) {
            return None;
        }
    }
    // Surgery: replace the pair's Join node with the kept Table, and
    // rewrite q1→q2 qualifiers in every surviving ON clause.
    let mut done = false;
    let mut new_from: Vec<FromItem> = Vec::with_capacity(from.len());
    for item in from {
        new_from.push(sje_nested_surgery(item, &tname, &q1, &q2, &on, &mut done));
    }
    if !done {
        return None;
    }
    // Verify the removed table is truly gone from the FROM tree.
    if sje_from_refs_qual(&new_from, &q1) {
        return None;
    }
    let mut new_stmt = stmt.clone();
    new_stmt.items = sje_rewrite_items(eng, stmt, &tname, &q1, &q2, snap, own, session, true)?;
    new_stmt.from = new_from;
    // New WHERE: the existing WHERE (qualifier-rewritten) AND the
    // surviving ON-clause quals. PG19 attaches these to the kept
    // rel's baserestrictinfo, which rustgres's WHERE distribution
    // models as scan Filters.
    let mut surv_it = survivors.into_iter();
    let first = surv_it.next().unwrap();
    let surv_expr = surv_it.fold(first, |a, b| Expr::And(Box::new(a), Box::new(b)));
    new_stmt.where_ = Some(match &stmt.where_ {
        Some(w) => Expr::And(
            Box::new(sje_rewrite_qual(w, &q1, &q2)?),
            Box::new(surv_expr),
        ),
        None => surv_expr,
    });
    // Rewrite remaining Expr positions.
    let rw_vec = |v: &[Expr]| -> Option<Vec<Expr>> {
        v.iter().map(|e| sje_rewrite_qual(e, &q1, &q2)).collect()
    };
    new_stmt.distinct_on = rw_vec(&stmt.distinct_on)?;
    new_stmt.group_by = stmt
        .group_by
        .iter()
        .map(|set| rw_vec(set))
        .collect::<Option<Vec<_>>>()?;
    new_stmt.having = match &stmt.having {
        Some(h) => Some(sje_rewrite_qual(h, &q1, &q2)?),
        None => None,
    };
    new_stmt.order_by = stmt
        .order_by
        .iter()
        .map(|t| {
            Some(OrderTerm {
                expr: sje_rewrite_qual(&t.expr, &q1, &q2)?,
                desc: t.desc,
                nulls_first: t.nulls_first,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    // --- Post-rewrite verification: no reference to the removed
    // qualifier may survive anywhere. ---
    if sje_stmt_refs_qual(&new_stmt, &q1) {
        return None;
    }
    Some(new_stmt)
}

/// v1.77: find the first inner Join node in tree order whose left and
/// right are plain scans of one table with distinct qualifiers.
/// Returns (table, q_remove, q_keep, on_expr). The search descends
/// through joins of any kind (a pair inside one subtree is always on
/// one side of every enclosing outer join, matching PG19's
/// jinfo_check); only INNER nodes are candidates.
pub(crate) fn sje_nested_find(item: &FromItem) -> Option<(String, String, String, Expr)> {
    if let FromItem::Join {
        left,
        kind: JoinKind::Inner,
        right,
        on: Some(o),
        using,
        natural,
        using_alias,
        alias,
        col_aliases,
    } = item
    {
        if using.is_empty()
            && !*natural
            && using_alias.is_none()
            && alias.is_none()
            && col_aliases.is_empty()
        {
            if let (
                FromItem::Table {
                    name: n1,
                    alias: a1,
                    ..
                },
                FromItem::Table {
                    name: n2,
                    alias: a2,
                    ..
                },
            ) = (&**left, &**right)
            {
                if n1 == n2 {
                    let q1 = a1.clone().unwrap_or_else(|| n1.clone());
                    let q2 = a2.clone().unwrap_or_else(|| n2.clone());
                    if q1 != q2 {
                        return Some((n1.clone(), q1, q2, o.clone()));
                    }
                }
            }
        }
    }
    if let FromItem::Join { left, right, .. } = item {
        if let Some(t) = sje_nested_find(left) {
            return Some(t);
        }
        if let Some(t) = sje_nested_find(right) {
            return Some(t);
        }
    }
    None
}

/// v1.77: tree surgery for a proven nested pair. The first inner Join
/// node structurally matching (tname, q1, q2, on) is replaced by its
/// kept (right) Table child; every other Join node's ON clause gets
/// q1→q2 qualifier rewriting (PG19 `remove_self_join_rel`
/// re-pointing the surviving rel's quals).
pub(crate) fn sje_nested_surgery(
    item: &FromItem,
    tname: &str,
    q1: &str,
    q2: &str,
    on: &Expr,
    done: &mut bool,
) -> FromItem {
    if !*done {
        if let FromItem::Join {
            left,
            kind: JoinKind::Inner,
            right,
            on: jon,
            using,
            natural,
            using_alias,
            alias,
            col_aliases,
        } = item
        {
            if using.is_empty()
                && !*natural
                && using_alias.is_none()
                && alias.is_none()
                && col_aliases.is_empty()
            {
                if let (
                    FromItem::Table {
                        name: n1,
                        alias: a1,
                        ..
                    },
                    FromItem::Table {
                        name: n2,
                        alias: a2,
                        ..
                    },
                ) = (&**left, &**right)
                {
                    let qq1 = a1.clone().unwrap_or_else(|| n1.clone());
                    let qq2 = a2.clone().unwrap_or_else(|| n2.clone());
                    if n1 == tname && n2 == tname && qq1 == q1 && qq2 == q2 {
                        if let Some(o) = jon {
                            if o == on {
                                *done = true;
                                return (**right).clone();
                            }
                        }
                    }
                }
            }
        }
    }
    match item {
        FromItem::Join {
            left,
            kind,
            right,
            on: jon,
            using,
            natural,
            using_alias,
            alias,
            col_aliases,
        } => {
            let nl = Box::new(sje_nested_surgery(left, tname, q1, q2, on, done));
            let nr = Box::new(sje_nested_surgery(right, tname, q1, q2, on, done));
            let non = match jon {
                Some(e) => Some(sje_rewrite_qual(e, q1, q2).unwrap_or_else(|| e.clone())),
                None => None,
            };
            FromItem::Join {
                left: nl,
                kind: *kind,
                right: nr,
                on: non,
                using: using.clone(),
                natural: *natural,
                using_alias: using_alias.clone(),
                alias: alias.clone(),
                col_aliases: col_aliases.clone(),
            }
        }
        _ => item.clone(),
    }
}

/// v1.77: does any Table item in the FROM tree still carry qualifier
/// `q`, or does any Join ON clause still reference it?
pub(crate) fn sje_from_refs_qual(from: &[FromItem], q: &str) -> bool {
    for item in from {
        match item {
            FromItem::Table { name, alias, .. } => {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                if qual == q {
                    return true;
                }
            }
            FromItem::Join {
                left, right, on, ..
            } => {
                if let Some(e) = on {
                    if sje_expr_refs_qual(e, q) {
                        return true;
                    }
                }
                if sje_from_refs_qual(std::slice::from_ref(left), q)
                    || sje_from_refs_qual(std::slice::from_ref(right), q)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// v1.73: does any expression in the statement contain a subquery?
/// The self-join rewrite bails on these (different query level).
pub(crate) fn sje_stmt_has_subquery(stmt: &SelectStmt) -> bool {
    let mut es: Vec<&Expr> = Vec::new();
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            es.push(expr);
        }
    }
    if let Some(w) = &stmt.where_ {
        es.push(w);
    }
    for set in &stmt.group_by {
        es.extend(set.iter());
    }
    if let Some(h) = &stmt.having {
        es.push(h);
    }
    for t in &stmt.order_by {
        es.push(&t.expr);
    }
    es.extend(stmt.distinct_on.iter());
    es.iter().any(|e| expr_has_subquery(e))
}

/// v1.73: first unique key (PK, then UNIQUE constraints, then
/// planner-usable unique indexes in catalog order) whose columns are all
/// in `constrained`. Mirrors `table_has_unique_for`'s order; PG19 tries
/// indexes in order and commits to the first covering one.
pub(crate) fn sje_covering_unique_key(
    eng: &Engine,
    table: &str,
    constrained: &HashSet<String>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<Vec<String>> {
    let db = &eng.db;
    let t = db.find_table(table, snap, &[own], session)?;
    let covers = |cols: &[String]| {
        !cols.is_empty()
            && cols
                .iter()
                .all(|c| constrained.iter().any(|x| x.eq_ignore_ascii_case(c)))
    };
    if let Some(pk) = t.pkey.as_ref() {
        if covers(&pk.cols) {
            return Some(pk.cols.clone());
        }
    }
    for u in &t.uniques {
        if covers(&u.cols) {
            return Some(u.cols.clone());
        }
    }
    if !db.is_temp_table(session, table) {
        for ix in db.visible_indexes_for(table, snap, &[own], session) {
            if ix.def.unique && ix.def.planner_usable && covers(&ix.def.col_names) {
                return Some(ix.def.col_names.clone());
            }
        }
    }
    None
}

/// v1.73: structural equality of two `=` conjuncts up to commutation.
pub(crate) fn sje_eq_equal(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (
            Expr::Cmp {
                op: CmpOp::Eq,
                left: l1,
                right: r1,
            },
            Expr::Cmp {
                op: CmpOp::Eq,
                left: l2,
                right: r2,
            },
        ) => (l1 == l2 && r1 == r2) || (l1 == r2 && r1 == l2),
        _ => false,
    }
}

/// v1.73: push `e` unless a structural or commuted duplicate is present.
pub(crate) fn sje_push_dedup(v: &mut Vec<Expr>, e: Expr) {
    if !v.iter().any(|s| s == &e || sje_eq_equal(s, &e)) {
        v.push(e);
    }
}

/// v1.73: normalize `l = r` to column-left when exactly one side is a
/// plain column and the other side is column-free (observed PG19 output
/// renders the surviving restriction as `(a = 1)`, never `(1 = a)`).
pub(crate) fn sje_normalize_eq(l: Expr, r: Expr) -> Expr {
    fn col_free(e: &Expr) -> bool {
        let mut refs = Vec::new();
        collect_col_refs(e, &mut refs);
        refs.is_empty()
    }
    let l_is_col = matches!(l, Expr::Column { .. });
    let r_is_col = matches!(r, Expr::Column { .. });
    if l_is_col && !r_is_col && col_free(&r) {
        Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(l),
            right: Box::new(r),
        }
    } else if r_is_col && !l_is_col && col_free(&l) {
        Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(r),
            right: Box::new(l),
        }
    } else {
        Expr::Cmp {
            op: CmpOp::Eq,
            left: Box::new(l),
            right: Box::new(r),
        }
    }
}

/// v1.73: apply `sje_normalize_eq` when the expression is an `=` conjunct.
pub(crate) fn sje_normalize_eq_expr(e: Expr) -> Expr {
    match e {
        Expr::Cmp {
            op: CmpOp::Eq,
            left,
            right,
        } => sje_normalize_eq(*left, *right),
        other => other,
    }
}

/// v1.73: rewrite qualifier `from_q` to `to_q` throughout an expression.
/// Returns `None` for subquery-bearing variants (the statement gate
/// already rejected those; this is a backstop).
pub(crate) fn sje_rewrite_qual(e: &Expr, from_q: &str, to_q: &str) -> Option<Expr> {
    let rq = |t: &Option<String>| -> Option<String> {
        match t {
            Some(q) if q == from_q => Some(to_q.to_string()),
            other => other.clone(),
        }
    };
    let rb =
        |e: &Box<Expr>| -> Option<Box<Expr>> { sje_rewrite_qual(e, from_q, to_q).map(Box::new) };
    let rv = |v: &[Expr]| -> Option<Vec<Expr>> {
        v.iter()
            .map(|e| sje_rewrite_qual(e, from_q, to_q))
            .collect()
    };
    let ro = |o: &Option<Box<Expr>>| -> Option<Option<Box<Expr>>> {
        match o {
            Some(x) => Some(Some(rb(x)?)),
            None => Some(None),
        }
    };
    let rot = |v: &[OrderTerm]| -> Option<Vec<OrderTerm>> {
        v.iter()
            .map(|t| {
                Some(OrderTerm {
                    expr: sje_rewrite_qual(&t.expr, from_q, to_q)?,
                    desc: t.desc,
                    nulls_first: t.nulls_first,
                })
            })
            .collect::<Option<Vec<_>>>()
    };
    Some(match e {
        Expr::Column { table, name } => Expr::Column {
            table: rq(table),
            name: name.clone(),
        },
        Expr::WholeRow { qual } => Expr::WholeRow {
            qual: if qual == from_q {
                to_q.to_string()
            } else {
                qual.clone()
            },
        },
        Expr::ResolvedCol { .. } | Expr::Literal(_) | Expr::Param(_) => e.clone(),
        Expr::Arith { op, left, right } => Expr::Arith {
            op: *op,
            left: rb(left)?,
            right: rb(right)?,
        },
        Expr::Cast { expr, to, written } => Expr::Cast {
            expr: rb(expr)?,
            to: to.clone(),
            written: written.clone(),
        },
        Expr::CastNamed { expr, name } => Expr::CastNamed {
            expr: rb(expr)?,
            name: name.clone(),
        },
        Expr::Row(elems) => Expr::Row(rv(elems)?),
        Expr::FieldAccess { expr, field } => Expr::FieldAccess {
            expr: rb(expr)?,
            field: field.clone(),
        },
        Expr::Concat(a, b) => Expr::Concat(rb(a)?, rb(b)?),
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => Expr::Like {
            expr: rb(expr)?,
            pattern: rb(pattern)?,
            not: *not,
            ilike: *ilike,
            escape: ro(escape)?,
        },
        Expr::Regex {
            expr,
            pattern,
            not,
            case_insensitive,
        } => Expr::Regex {
            expr: rb(expr)?,
            pattern: rb(pattern)?,
            not: *not,
            case_insensitive: *case_insensitive,
        },
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => Expr::Between {
            expr: rb(expr)?,
            low: rb(low)?,
            high: rb(high)?,
            neg: *neg,
        },
        Expr::IsBool { expr, neg, val } => Expr::IsBool {
            expr: rb(expr)?,
            neg: *neg,
            val: *val,
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: rv(args)?,
        },
        Expr::NamedArg { name, expr } => Expr::NamedArg {
            name: name.clone(),
            expr: rb(expr)?,
        },
        Expr::Extract { field, from } => Expr::Extract {
            field: field.clone(),
            from: rb(from)?,
        },
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: rb(left)?,
            right: rb(right)?,
        },
        Expr::And(a, b) => Expr::And(rb(a)?, rb(b)?),
        Expr::Or(a, b) => Expr::Or(rb(a)?, rb(b)?),
        Expr::Not(a) => Expr::Not(rb(a)?),
        Expr::BitNot(a) => Expr::BitNot(rb(a)?),
        Expr::Neg(a) => Expr::Neg(rb(a)?),
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: ro(operand)?,
            whens: whens
                .iter()
                .map(|(k, r)| Some((rb(k)?, rb(r)?)))
                .collect::<Option<Vec<_>>>()?,
            else_: ro(else_)?,
        },
        Expr::IsNull { expr, neg } => Expr::IsNull {
            expr: rb(expr)?,
            neg: *neg,
        },
        Expr::IsDistinctFrom { left, right, neg } => Expr::IsDistinctFrom {
            left: rb(left)?,
            right: rb(right)?,
            neg: *neg,
        },
        Expr::Agg {
            func,
            arg,
            distinct,
            arg2,
            agg_order_by,
            filter,
        } => Expr::Agg {
            func: func.clone(),
            arg: ro(arg)?,
            distinct: *distinct,
            arg2: ro(arg2)?,
            agg_order_by: rot(agg_order_by)?,
            filter: ro(filter)?,
        },
        Expr::WithinGroup {
            func,
            direct_args,
            within_order_by,
            filter,
        } => Expr::WithinGroup {
            func: func.clone(),
            direct_args: rv(direct_args)?,
            within_order_by: rot(within_order_by)?,
            filter: ro(filter)?,
        },
        Expr::ArrayCtor { elems, nested } => Expr::ArrayCtor {
            elems: rv(elems)?,
            nested: *nested,
        },
        Expr::Subscript { array, indices } => Expr::Subscript {
            array: rb(array)?,
            indices: rv(indices)?,
        },
        Expr::Slice { array, bounds } => Expr::Slice {
            array: rb(array)?,
            bounds: bounds
                .iter()
                .map(|(lo, hi)| Some((ro(lo)?, ro(hi)?)))
                .collect::<Option<Vec<_>>>()?,
        },
        Expr::InSub { .. }
        | Expr::Quantified { .. }
        | Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::Exists { .. } => return None,
        Expr::UserOp { op, left, right } => Expr::UserOp {
            op: op.clone(),
            left: rb(left)?,
            right: rb(right)?,
        },
        Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            wid,
            filter,
            exclusion,
        } => Expr::Window {
            func: func.clone(),
            args: rv(args)?,
            distinct: *distinct,
            partition_by: rv(partition_by)?,
            order_by: rot(order_by)?,
            frame: frame.clone(),
            wid: *wid,
            filter: ro(filter)?,
            exclusion: exclusion.clone(),
        },
    })
}

/// v1.73: post-rewrite verification helper — does `e` reference `q`?
pub(crate) fn sje_expr_refs_qual(e: &Expr, q: &str) -> bool {
    let mut refs = Vec::new();
    collect_col_refs(e, &mut refs);
    refs.iter().any(|(t, _)| t.as_deref() == Some(q))
}

/// v1.73: post-rewrite verification — no reference to the removed
/// qualifier may survive anywhere in the statement.
pub(crate) fn sje_stmt_refs_qual(stmt: &SelectStmt, q: &str) -> bool {
    for item in &stmt.items {
        match item {
            SelectItem::AllOf(aq) if aq == q => return true,
            SelectItem::Expr { expr, .. } => {
                if sje_expr_refs_qual(expr, q) {
                    return true;
                }
            }
            _ => {}
        }
    }
    if let Some(w) = &stmt.where_ {
        if sje_expr_refs_qual(w, q) {
            return true;
        }
    }
    for set in &stmt.group_by {
        for e in set {
            if sje_expr_refs_qual(e, q) {
                return true;
            }
        }
    }
    if let Some(h) = &stmt.having {
        if sje_expr_refs_qual(h, q) {
            return true;
        }
    }
    for t in &stmt.order_by {
        if sje_expr_refs_qual(&t.expr, q) {
            return true;
        }
    }
    for e in &stmt.distinct_on {
        if sje_expr_refs_qual(e, q) {
            return true;
        }
    }
    false
}
pub(crate) fn plan_from_item(
    eng: &Engine,
    item: &FromItem,
    where_: Option<&Expr>,
    snap: &Snapshot,
    own: u64,
    session: u64,
    // v0.78: effective CTE list (outer ++ this level's, in definition
    // order) so EXPLAIN can resolve CTE references like the executor.
    ctes: &[CteDef],
    // v1.08: PG-text context for EXPLAIN (COSTS OFF). `None` selects the
    // legacy Debug filter text. When `Some`, `where_` is the item's own
    // conjunct slice (already distributed by `pg_split_where`).
    pctx: Option<&PgPlanCtx>,
    // v1.46: the query level being planned — for VERBOSE `Output:`
    // targetlist computation (select list, GROUP BY/HAVING/ORDER BY,
    // FROM for the `useprefix` rule).
    stmt: &SelectStmt,
) -> Result<PlanNode, ExecError> {
    // v1.46: VERBOSE `Output:` entries for a base-table scan of `name`
    // / `alias`. `index_cols` are the index key columns (index scans
    // only). Empty (not `None`) when `pctx` is `None` (legacy mode).
    let scan_output = |name: &str,
                       alias: Option<&str>,
                       table_cols: &[String],
                       where_: Option<&Expr>,
                       index_cols: &[String]|
     -> Vec<String> {
        match pctx {
            Some(px) => pg_scan_output(stmt, name, alias, table_cols, where_, index_cols, px)
                .unwrap_or_default(),
            None => Vec::new(),
        }
    };
    match item {
        FromItem::Table { name, alias, .. } => {
            // v0.78: CTEs shadow everything (like Postgres and the
            // executor's build_source). Plan the CTE body as a subquery
            // scan; the body only sees CTEs defined before it (plus
            // outer ones), matching materialization order.
            if let Some(pos) = ctes.iter().rposition(|c| c.name == *name) {
                let cte = &ctes[pos];
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                // Recursive CTEs reference themselves: planning the body
                // would recurse forever, so use a nominal estimate (the
                // executor iterates to a fixpoint instead).
                let child = if cte.recursive {
                    PlanNode::Values {
                        rows: 100,
                        output: Vec::new(),
                    }
                } else {
                    plan_cte_body(
                        eng,
                        cte,
                        &ctes[..pos],
                        snap,
                        own,
                        session,
                        pctx.is_some(),
                        pctx.map(|p| p.verbose).unwrap_or(false),
                    )?
                };
                let rows = child.rows();
                // v1.46: VERBOSE `Output:` — the CTE's output columns
                // qualified by the scan alias.
                let output = match pctx {
                    Some(_) => pg_cte_col_names(eng, cte, &ctes[..pos], snap, own, session)
                        .map(|names| {
                            names
                                .into_iter()
                                .map(|n| {
                                    format!("{}.{}", pg_quote_ident(&qual), pg_quote_ident(&n))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    None => Vec::new(),
                };
                return Ok(PlanNode::SubqueryScan {
                    alias: qual,
                    rows,
                    child: Box::new(child),
                    output,
                });
            }
            // v0.9: information_schema virtual tables plan as scans.
            if name == "information_schema.tables"
                || name == "information_schema.columns"
                || name == "information_schema.sequences"
            {
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    alias: None,
                    filter: None,
                    rows: 100,
                    // v1.46: VERBOSE `Output:` (virtual-table columns are
                    // unknown: refs render, `*` omits the line).
                    output: scan_output(name, None, &[], where_, &[]),
                });
            }
            // v0.9: views plan as a plain scan; build_source expands the
            // stored SELECT (no index access into views).
            if eng.db.find_view(name, snap, own).is_some() {
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    alias: None,
                    filter: None,
                    rows: 1000,
                    // v1.46: VERBOSE `Output:` (view columns are not
                    // expanded: refs render, `*` omits the line).
                    output: scan_output(name, None, &[], where_, &[]),
                });
            }
            if name == "pg_stats" && eng.db.find_table(name, snap, &[own], session).is_none() {
                let rows: u64 = eng.db.stats.values().map(|ts| ts.cols.len() as u64).sum();
                return Ok(PlanNode::SeqScan {
                    table: "pg_stats".to_string(),
                    alias: None,
                    filter: None,
                    rows,
                    // v1.46: VERBOSE `Output:`.
                    output: scan_output("pg_stats", None, &[], where_, &[]),
                });
            }
            // v0.37: pg_class is virtual (TOAST introspection).
            if name == "pg_class" && eng.db.find_table(name, snap, &[own], session).is_none() {
                return Ok(PlanNode::SeqScan {
                    table: "pg_class".to_string(),
                    alias: None,
                    filter: None,
                    rows: eng.db.tables.len() as u64,
                    // v1.46: VERBOSE `Output:`.
                    output: scan_output("pg_class", None, &[], where_, &[]),
                });
            }
            // v0.88: pg_attribute is virtual (bounded catalog subset).
            if name == "pg_attribute" && eng.db.find_table(name, snap, &[own], session).is_none() {
                return Ok(PlanNode::SeqScan {
                    table: "pg_attribute".to_string(),
                    alias: None,
                    filter: None,
                    rows: eng
                        .db
                        .tables
                        .values()
                        .map(|vs| {
                            vs.iter()
                                .find(|t| t.dropped_xmax == 0)
                                .map(|t| t.columns.len() as u64)
                                .unwrap_or(0)
                        })
                        .sum(),
                    // v1.46: VERBOSE `Output:`.
                    output: scan_output("pg_attribute", None, &[], where_, &[]),
                });
            }
            // v0.11: role catalogs are virtual.
            if matches!(
                name.as_str(),
                "pg_authid" | "pg_roles" | "pg_user" | "pg_auth_members"
            ) && eng.db.find_table(name, snap, &[own], session).is_none()
            {
                let rows = virtual_role_catalog_rows(&eng.db, name, snap, own);
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    alias: None,
                    filter: None,
                    rows,
                    // v1.46: VERBOSE `Output:`.
                    output: scan_output(name, None, &[], where_, &[]),
                });
            }
            // v0.13: pg_replication_slots is virtual too; the estimate is
            // the live slot count.
            if name.as_str() == "pg_replication_slots"
                && eng.db.find_table(name, snap, &[own], session).is_none()
            {
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    alias: None,
                    filter: None,
                    rows: eng.repl_slots.len() as u64,
                    // v1.46: VERBOSE `Output:`.
                    output: scan_output(name, None, &[], where_, &[]),
                });
            }
            // v0.98: pg_sequences is virtual too; the estimate is the
            // live sequence count.
            if name.as_str() == "pg_sequences"
                && eng.db.find_table(name, snap, &[own], session).is_none()
            {
                return Ok(PlanNode::SeqScan {
                    table: name.clone(),
                    alias: None,
                    filter: None,
                    rows: eng.db.sequences.len() as u64,
                    // v1.46: VERBOSE `Output:`.
                    output: scan_output(name, None, &[], where_, &[]),
                });
            }
            let t = eng
                .db
                .find_table(name, snap, &[own], session)
                .ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", name))
                })?;
            let qual = alias.clone().unwrap_or_else(|| name.clone());
            let rel_rows = est_rel_rows(&eng.db, name, snap, own, session);
            // v1.70: PG19 `restriction_is_constant_false` (joinrels.c): a
            // scan WHERE with `col = c1 AND col = c2` on the same qualified
            // column, with provably-different literals, plans as a dummy
            // `Result` (`One-Time Filter: false`, `Replaces: Scan on
            // <alias>`). EXPLAIN-path only (`pctx.is_some()`); the
            // executor never sees `PlanNode`.
            if pctx.is_some() {
                if let Some(w) = where_ {
                    if pg_where_is_contradiction(w) {
                        return Ok(PlanNode::Result {
                            rows: 1,
                            filter: None,
                            output: Vec::new(),
                            one_time_filter: true,
                            replaces: Some(format!("Scan on {}", qual)),
                        });
                    }
                }
            }
            match plan_access_path(&eng.db, t, name, &qual, where_, snap, own, session) {
                AccessPath::SeqScan => {
                    // v1.08: PG-text Filter (the whole item slice; index
                    // selection found nothing usable). Falls back to Debug
                    // format (EXPECTED-FAIL via mask, no txn abort).
                    // v1.65: PG19's Filter prefixing (explain.c
                    // `show_scan_qual`): `useprefix =
                    // (IsA(planstate->plan, SubqueryScan) || es->verbose)`.
                    // A pulled-up subquery plans as a plain SeqScan, so
                    // its Filter is NOT qualified in non-verbose mode
                    // (the v1.54 `dead_rtes` rule over-qualified; the t5
                    // oracle shows `Filter: (a = 1)`). rustgres never
                    // sets a Filter on a SubqueryScan node (the Derived
                    // branch ignores the slice), so the rule is just
                    // `verbose` here. A plain multi-table query does NOT
                    // qualify scan Filters in non-verbose mode.
                    let filter = match (pctx, where_) {
                        (Some(px), Some(w)) => {
                            // v1.76: PG19 scan-Filter qual ordering (EC
                            // deferral): `=` conjuncts render after non-`=`
                            // conjuncts. EXPLAIN-deparse only.
                            let ordered = pg_order_scan_filter(w);
                            Some(pg_expr_text_or_debug(&ordered, px, px.verbose))
                        }
                        _ => None,
                    };
                    // v1.46: VERBOSE `Output:` — the table's columns in
                    // table order for `*` expansion.
                    let table_cols: Vec<String> =
                        t.columns.iter().map(|(n, _)| n.clone()).collect();
                    Ok(PlanNode::SeqScan {
                        table: name.clone(),
                        alias: alias.clone(),
                        filter,
                        rows: rel_rows,
                        output: scan_output(name, alias.as_deref(), &table_cols, where_, &[]),
                    })
                }
                AccessPath::IndexScan {
                    index,
                    prefix,
                    lo,
                    hi,
                    cond,
                    used,
                } => {
                    // v0.87: temp indexes live in the session-local map.
                    let ix = eng
                        .db
                        .temp_indexes
                        .get(&session)
                        .and_then(|m| m.get(&index))
                        .or_else(|| eng.db.indexes.get(&index))
                        .expect("planned index still present; engine lock held throughout");
                    let rows = est_index_rows(
                        &eng.db,
                        name,
                        ix,
                        &prefix,
                        lo.as_ref().map(|(v, _)| v),
                        hi.as_ref().map(|(v, _)| v),
                        snap,
                        own,
                        session,
                    );
                    // v1.08: residual Filter = conjuncts the index
                    // cond did not consume. `used` indexes the reversed
                    // `split_conjuncts` order; map back to source order
                    // for the text. Index Cond values with no faithful PG
                    // spelling render legacy bare text (wrong,
                    // EXPECTED-FAIL via mask).
                    let filter = match (pctx, where_) {
                        (Some(px), Some(w)) => {
                            let rev = split_conjuncts(w);
                            let n = rev.len();
                            // `used` indexes the reversed `split_conjuncts`
                            // order; collect the unused conjuncts and reverse
                            // back to source order for the text.
                            let mut residual: Vec<Expr> = (0..n)
                                .filter(|i| !used.contains(i))
                                .map(|i| rev[i].clone())
                                .collect();
                            residual.reverse();
                            match pg_fold_and(residual) {
                                // v1.65: PG19's Filter prefixing
                                // (explain.c `show_scan_qual`):
                                // `useprefix = IsA(SubqueryScan) ||
                                // verbose` — see the SeqScan site.
                                Some(e) => Some(pg_expr_text_or_debug(&e, px, px.verbose)),
                                None => None,
                            }
                        }
                        _ => None,
                    };
                    Ok(PlanNode::IndexScan {
                        table: name.clone(),
                        alias: alias.clone(),
                        index,
                        cond,
                        filter,
                        rows,
                        // v1.46: VERBOSE `Output:` (index key columns are
                        // added to the baserel tlist in PG).
                        output: scan_output(
                            name,
                            alias.as_deref(),
                            &t.columns.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                            where_,
                            &ix.def.col_names,
                        ),
                    })
                }
            }
        }
        FromItem::Derived {
            sub,
            alias,
            col_aliases,
            ..
        } => {
            // v1.08: a filter slice on a subquery scan: PG would push it
            // inside, but we render without it (wrong, EXPECTED-FAIL via
            // mask; does not abort the transaction).
            let _ = (pctx, where_);
            // v0.78: derived tables see the enclosing CTEs (like the
            // executor's shared CTE bindings).
            let child = plan_select(
                eng,
                sub,
                snap,
                own,
                session,
                ctes,
                pctx.is_some(),
                pctx.map(|p| p.verbose).unwrap_or(false),
            )?;
            let rows = child.rows();
            // v1.46: VERBOSE `Output:` — the subquery's output columns
            // qualified by the scan alias (`ss.x, ss.u`).
            let output = match pctx {
                Some(_) => {
                    pg_subquery_scan_output(eng, sub, alias, col_aliases, snap, own, session, ctes)
                        .unwrap_or_default()
                }
                None => Vec::new(),
            };
            Ok(PlanNode::SubqueryScan {
                alias: alias.clone(),
                rows,
                child: Box::new(child),
                output,
            })
        }
        // v0.32: table function — cardinality is unknown at plan time
        // (args need evaluation), so plan it as a single-row VALUES.
        FromItem::Function { .. } => {
            // v1.08: filter on function scan ignored (wrong, EXPECTED-FAIL).
            let _ = (pctx, where_);
            // v1.46: no faithful per-column Output rendering for
            // function scans (the line is omitted).
            Ok(PlanNode::Values {
                rows: 1,
                output: Vec::new(),
            })
        }
        // v0.14: VALUES rows are uncorrelated constants.
        FromItem::Values {
            rows,
            alias,
            col_aliases,
            ..
        } => {
            // v1.08: filter on values scan ignored (wrong, EXPECTED-FAIL).
            let _ = (pctx, where_);
            // v1.46: VERBOSE `Output:` — the column aliases qualified by
            // the VALUES alias (PG's `column1, ...` when unaliased).
            let output = match pctx {
                Some(_) => {
                    let n = rows.first().map(|r| r.len()).unwrap_or(0);
                    let names: Vec<String> = if col_aliases.is_empty() {
                        (1..=n).map(|i| format!("column{i}")).collect()
                    } else {
                        col_aliases.clone()
                    };
                    names
                        .into_iter()
                        .map(|c| format!("{}.{}", pg_quote_ident(alias), pg_quote_ident(&c)))
                        .collect()
                }
                None => Vec::new(),
            };
            Ok(PlanNode::Values {
                rows: rows.len() as u64,
                output,
            })
        }
        // The executor runs joins as nested loops; the filter attaches to
        // the outermost Nested Loop node at the top level.
        FromItem::Join {
            left,
            right,
            kind,
            on,
            using,
            natural,
            ..
        } => {
            // v1.49: PG19 const-false JOIN qual (joinrels.c:1090-1121,
            // `restriction_is_constant_false`). An ON clause folding to
            // FALSE-or-NULL makes the joinrel dummy (INNER/CROSS: the
            // whole join plans as a bare `Result`) or the inner rel
            // dummy (LEFT: the inner side plans as a `Result` with
            // `One-Time Filter: false`, the join keeps a
            // `Join Filter: false` / `Join Filter: NULL::boolean`).
            // JOIN_FULL is never dummy (both sides must be emitted);
            // JOIN_RIGHT was already flipped to LEFT above.
            // EXPLAIN-path only (`pctx.is_some()`); execution still
            // evaluates the ON clause normally.
            if pctx.is_some() {
                if let Some(on_expr) = on {
                    // v1.60: catalog-backed fold (immutable calls with
                    // constant args fold, PG19 `simplify_function`); no
                    // pulled-up function-scan bindings at the ON site.
                    let mut fc = PgConstFold::with_db(&eng.db);
                    if pg_is_const_false(on_expr, &mut fc) {
                        let null_qual = matches!(pg_fold_bool_const(on_expr, &mut fc), Some(None));
                        match kind {
                            JoinKind::Inner | JoinKind::Cross => {
                                let mut names = Vec::new();
                                pg_replaces_names(std::slice::from_ref(left), &mut names);
                                pg_replaces_names(std::slice::from_ref(right), &mut names);
                                let replaces = if names.len() == 1 {
                                    format!("Scan on {}", names[0])
                                } else {
                                    format!("Join on {}", names.join(", "))
                                };
                                return Ok(PlanNode::Result {
                                    rows: 1,
                                    filter: None,
                                    output: Vec::new(),
                                    one_time_filter: true,
                                    replaces: Some(replaces),
                                });
                            }
                            JoinKind::Left => {
                                let outer = plan_from_item(
                                    eng, left, None, snap, own, session, ctes, pctx, stmt,
                                )?;
                                let mut inames = Vec::new();
                                pg_replaces_names(std::slice::from_ref(right), &mut inames);
                                // v1.51: `Replaces:` for the dummy inner names the
                                // RTEs that survived `remove_useless_joins`
                                // (PG19 explain.c `show_result_replacement_info`).
                                // T2 #17786: the pulled-up `int8_tbl LEFT JOIN
                                // innertab` lost its useless inner join, so
                                // the dummy inner is just `int8_tbl` →
                                // `Replaces: Scan on int8_tbl`. Multi-RTE
                                // inners print no `Replaces:` line here (the
                                // whole-join dummy path above renders those
                                // as `Join on ...`).
                                let ireplaces = if inames.len() == 1 {
                                    Some(format!("Scan on {}", inames[0]))
                                } else {
                                    None
                                };
                                // v1.50: VERBOSE `Output:` for the dummy inner.
                                // The pulled-up subquery's tlist exprs are in
                                // `pctx.pulled_exprs`; deparse them. For a
                                // Cast of a literal, PG's Output omits the
                                // outer parens (`'constant'::text`, not
                                // `('constant'::text)`).
                                let inner_output: Vec<String> = match pctx {
                                    Some(px) if !px.pulled_exprs.is_empty() => px
                                        .pulled_exprs
                                        .iter()
                                        .filter_map(|e| match e {
                                            Expr::Cast { expr, to, .. }
                                                if matches!(&**expr, Expr::Literal(_)) =>
                                            {
                                                let label = pg_type_label(*to)?;
                                                let inner_s = match &**expr {
                                                    Expr::Literal(Literal::Text(s)) => {
                                                        pg_quote_literal(s)
                                                    }
                                                    _ => return None,
                                                };
                                                Some(format!("{}::{label}", inner_s))
                                            }
                                            _ => pg_expr_text(e, px, true),
                                        })
                                        .collect(),
                                    _ => Vec::new(),
                                };
                                let inner = PlanNode::Result {
                                    rows: 1,
                                    filter: None,
                                    output: inner_output,
                                    one_time_filter: true,
                                    replaces: ireplaces,
                                };
                                // v1.46: VERBOSE `Output:` — the join's
                                // tlist starts as the concatenation of
                                // its inputs' (the dummy inner
                                // contributes none here).
                                let output = outer.output().to_vec();
                                let rows = outer.rows();
                                return Ok(PlanNode::NestedLoop {
                                    filter: None,
                                    join_filter: Some(if null_qual {
                                        "NULL::boolean".to_string()
                                    } else {
                                        "false".to_string()
                                    }),
                                    rows,
                                    outer: Box::new(outer),
                                    inner: Box::new(inner),
                                    kind: *kind,
                                    output,
                                });
                            }
                            JoinKind::Right | JoinKind::Full => {}
                            // v1.68: `JoinKind::Anti` is EXPLAIN-planner
                            // only (never parsed from a FROM join).
                            JoinKind::Anti => {}
                        }
                    }
                }
            }
            // v1.65: 3-way cross-join reordering (t5's subquery-flatten +
            // qual-pushdown + reorder gap; PG19 `standard_join_search`
            // miniature, nestloop paths only). A pure comma product over
            // exactly three base tables is reordered by the absolute
            // `pg_cost_nestloop` model; anything else — or any
            // estimation failure — falls through to the written-order
            // path below. EXPLAIN-plan only (`PlanNode` never reaches
            // the executor), so a reorder changes the plan text, never
            // the result.
            if let Some(px) = pctx {
                if matches!(kind, JoinKind::Cross) && on.is_none() && using.is_empty() && !natural {
                    if let Some(node) = pg_reorder_cross_join(
                        eng, left, right, where_, snap, own, session, ctes, px, stmt,
                    ) {
                        return Ok(node);
                    }
                }
            }
            // v1.08: in PG-text mode the join's WHERE slice is split
            // between the inputs; the ON clause (inner joins only)
            // becomes the Join Filter. USING/NATURAL and outer joins
            // have no faithful spelling here → render best-effort
            // (wrong, EXPECTED-FAIL via mask; does not abort txn).
            let mut left_where: Option<Expr> = None;
            let mut right_where: Option<Expr> = None;
            let mut join_filter: Option<String> = None;
            // v1.71: EC union-find + raw join-filter conjuncts. The text is
            // rendered post-swap (see the NestedLoop branch below) using the
            // PG19 first-EC-call orientation.
            let mut jf_exprs: Vec<Expr> = Vec::new();
            let mut join_ec = PgEc::new();
            // v1.71: item qualifier/column split for EC member resolution
            // (shared by the ON and WHERE EC feeds below).
            let ec_split: [(Vec<String>, Vec<String>); 2] = [
                pg_split_item(eng, left, snap, own, session, ctes),
                pg_split_item(eng, right, snap, own, session, ctes),
            ];
            // v1.71: PG19 `check_index_predicates` side effect — a partial
            // index on the left table fixes the EC-derived orientation to
            // (right-first, left-second).
            let ec_partial_left = pg_item_has_partial_index(eng, left, snap, own, session);
            // v1.50: One-sided ON pushdown target (set inside the supported
            // block, used after inner is planned).
            let mut push_on_to_right: Option<Expr> = None;
            // v1.64: equi clauses (left_expr, right_expr) for the
            // hash-vs-nestloop choice (set inside the supported block).
            let mut hash_clauses: Vec<(Expr, Expr)> = Vec::new();
            if let Some(px) = pctx {
                // Only inner/cross joins get the full PG-text treatment;
                // others render without a Join Filter (wrong, masked).
                let supported = matches!(kind, JoinKind::Inner | JoinKind::Cross)
                    && !*natural
                    && !(matches!(kind, JoinKind::Cross) && on.is_some())
                    // v1.64: USING is supported for inner joins — PG19's
                    // `transformJoinUsingClause` builds `lvar = rvar` per
                    // column, which is exactly an inner equi-join.
                    && (using.is_empty()
                        || (matches!(kind, JoinKind::Inner) && on.is_none()));
                if supported {
                    // The ON clause is always a join-level predicate; the
                    // WHERE slice (when present) splits between the inputs.
                    let mut jf: Vec<Expr> = Vec::new();
                    // v1.50: One-sided ON pushdown (T2). If the ON references
                    // only the right side's tables (none from the left),
                    // push it down as a Filter on the right side, not a Join
                    // Filter here.
                    if let Some(o) = on {
                        let mut left_names = Vec::new();
                        pg_replaces_names(std::slice::from_ref(left), &mut left_names);
                        let mut right_names = Vec::new();
                        pg_replaces_names(std::slice::from_ref(right), &mut right_names);
                        let mut on_refs = Vec::new();
                        pg_expr_tables(o, &mut on_refs);
                        let on_tables: Vec<String> =
                            on_refs.iter().filter_map(|(t, _)| t.clone()).collect();
                        let refs_left = on_tables.iter().any(|t| left_names.contains(t));
                        let refs_right = on_tables.iter().any(|t| right_names.contains(t));
                        // v1.50: push down only if ON refs right (and not left).
                        // Unqualified refs (None table) are treated as not
                        // pushable (conservative).
                        let has_unqualified = on_refs.iter().any(|(t, _)| t.is_none());
                        if !refs_left && refs_right && !has_unqualified {
                            push_on_to_right = Some(o.clone());
                        } else {
                            jf.push(o.clone());
                        }
                        // v1.71: EC from ON conjuncts (PG19 distributes the
                        // join's own quals before the top WHERE).
                        for c in pg_conjuncts(o) {
                            pg_ec_union_conjunct(&mut join_ec, c, &ec_split);
                        }
                    }
                    // v1.64: USING quals (PG19 `transformJoinUsingClause`).
                    // Each USING column becomes `left.col = right.col` with
                    // the side's display qualifier (table name or alias).
                    // If either side lacks a display qualifier (nested join
                    // source), fail closed — the join stays unsupported.
                    let mut using_ok = true;
                    if !using.is_empty() {
                        // The display qualifier for a USING qual is the
                        // side's alias-or-table name (PG19's RTE alias).
                        // Nested join sources have no single qualifier →
                        // fail closed.
                        let qual_of = |it: &FromItem| match it {
                            FromItem::Table { name, alias, .. } => {
                                Some(alias.clone().unwrap_or_else(|| name.clone()))
                            }
                            _ => None,
                        };
                        let lq = qual_of(left);
                        let rq = qual_of(right);
                        match (lq, rq) {
                            (Some(lq), Some(rq)) => {
                                for col in using.iter() {
                                    jf.push(Expr::Cmp {
                                        op: CmpOp::Eq,
                                        left: Box::new(Expr::Column {
                                            table: Some(lq.clone()),
                                            name: col.clone(),
                                        }),
                                        right: Box::new(Expr::Column {
                                            table: Some(rq.clone()),
                                            name: col.clone(),
                                        }),
                                    });
                                }
                            }
                            _ => {
                                using_ok = false;
                            }
                        }
                    }
                    if using_ok {
                        if let Some(w) = where_ {
                            // v1.71: reuse the EC split computed above.
                            let (per_item, mid) = pg_split_where(w, &ec_split)?;
                            let mut it = per_item.into_iter();
                            left_where = pg_fold_and(it.next().unwrap_or_default());
                            right_where = pg_fold_and(it.next().unwrap_or_default());
                            jf.extend(mid);
                            // v1.71: EC from WHERE conjuncts, written order
                            // (after the ON conjuncts, per PG19's post-order
                            // jointree distribution).
                            for c in pg_conjuncts(w) {
                                pg_ec_union_conjunct(&mut join_ec, c, &ec_split);
                            }
                        }
                        // v1.64: hashable equi clauses for the hash-vs-nestloop
                        // choice (PG19 `hash_inner_and_outer`). Extracted from
                        // the raw conjuncts (ON + USING + WHERE-mid), before
                        // they are folded into the Join Filter text.
                        if matches!(kind, JoinKind::Inner) {
                            let mut ln = Vec::new();
                            pg_replaces_names(std::slice::from_ref(left), &mut ln);
                            let mut rn = Vec::new();
                            pg_replaces_names(std::slice::from_ref(right), &mut rn);
                            hash_clauses = pg_hash_clauses(&jf, &ln, &rn);
                        }
                        // v1.71: keep the raw conjuncts for the post-swap
                        // EC-canonical render in the NestedLoop branch.
                        jf_exprs = jf.clone();
                        join_filter = match pg_fold_and(jf) {
                            Some(e) => Some(pg_expr_text_or_debug(&e, px, true)),
                            None => None,
                        };
                    } // end if using_ok
                } // end if supported
            }
            let outer = plan_from_item(
                eng,
                left,
                left_where.as_ref(),
                snap,
                own,
                session,
                ctes,
                pctx,
                stmt,
            )?;
            let mut inner = plan_from_item(
                eng,
                right,
                right_where.as_ref(),
                snap,
                own,
                session,
                ctes,
                pctx,
                stmt,
            )?;
            // v1.50: One-sided ON pushdown (T2). If the ON was pushed to the
            // right side, set it as a Filter on the inner node.
            // v1.63: read the pushdown flag before the `if let` moves it.
            let on_pushed_right = push_on_to_right.is_some();
            // v1.70: dummy-input propagation (PG19's dummy-rel join: an
            // inner/cross join with a dummy input is itself dummy). A
            // `PlanNode::Result { one_time_filter: true, .. }` is only
            // produced on the EXPLAIN path (v1.60 const-false ON-clause
            // whole-join early-return above, and the v1.70
            // scan-contradiction above); the executor never sees it.
            let dummy_input = matches!(
                &outer,
                PlanNode::Result {
                    one_time_filter: true,
                    ..
                }
            ) || matches!(
                &inner,
                PlanNode::Result {
                    one_time_filter: true,
                    ..
                }
            );
            if pctx.is_some() && matches!(kind, JoinKind::Inner | JoinKind::Cross) && dummy_input {
                if let Some(replaces) = pg_result_replaces(std::slice::from_ref(item)) {
                    return Ok(PlanNode::Result {
                        rows: 1,
                        filter: None,
                        output: Vec::new(),
                        one_time_filter: true,
                        replaces: Some(replaces),
                    });
                }
            }
            if let (Some(px), Some(on_expr)) = (pctx, push_on_to_right) {
                let filter_text = pg_expr_text_or_debug(&on_expr, px, true);
                inner.set_filter(Some(filter_text));
            }
            // v1.63: cost-based nestloop side selection (selective side
            // outer). PG19 tries both orders for JOIN_INNER only
            // (joinrels.c); outer joins keep their written order. Skipped
            // when the ON was pushed to the right side — that filter's
            // selectivity is not in `right_where`, so fail closed. The
            // swap only reorders plan children; the executor never sees
            // PlanNode, so results are identical by construction.
            let swapped = matches!(kind, JoinKind::Inner | JoinKind::Cross)
                && !on_pushed_right
                && pg_nestloop_swap(
                    &eng.db,
                    &outer,
                    left_where.as_ref(),
                    &inner,
                    right_where.as_ref(),
                    snap,
                    own,
                    session,
                );
            // v1.64: hash join planning (PG19 `cost_hashjoin` vs
            // `cost_nestloop`). The hash join gets its own order: v1.75
            // ports PG19's `make_join_rel` (joinrels.c) + `try_hashjoin_path`
            // (joinpath.c) — both (rel1, rel2) and (rel2, rel1) orders are
            // costed and `add_path` keeps the cheaper — deleting the v1.64
            // empirical rule ("the filtered side probes"), which only
            // matched PG19 when the filtered side happened to be the
            // cheaper inner (PG19 hashes the smaller side regardless of
            // which side the filter sits on). Only for inner equi-joins
            // with hashable clauses and estimable SeqScan sides; fail
            // closed to nestloop otherwise. The clauses are oriented
            // outer-var-left for `Hash Cond:` (PG19 `create_hashjoin_plan`
            // / `get_switched_clauses`).
            //
            // Note: (outer, inner) here are still the pre-swap (left,
            // right) plans; the v1.63 `swapped` order applies to the
            // nestloop fallback only.
            let hash_win: Option<(bool, Vec<(Expr, Expr)>)> = if matches!(kind, JoinKind::Inner)
                && !on_pushed_right
                && !hash_clauses.is_empty()
            {
                // Candidate orders as (outer_is_left, oriented_clauses).
                let clauses_lr = hash_clauses.clone();
                let clauses_rl: Vec<(Expr, Expr)> =
                    hash_clauses.iter().cloned().map(|(l, r)| (r, l)).collect();
                // v1.75: PG19 `make_join_rel`/`try_hashjoin_path` try both
                // orders and keep the cheaper — but only when ANALYZE
                // stats back the bucket fractions
                // (`estimate_hash_bucket_stats`); without stats the model
                // cannot reproduce PG19's stats-driven choice (the
                // subselect VtA oracles were generated with stats), so the
                // v1.64 validated behavior is kept: the empirical
                // filtered-probes rule for one-sided filters, the punted
                // cost-model order for both/neither. `hash_clauses` are
                // (left, right): the inner keys are the right sides for
                // LR, the left sides for RL.
                let keys_r: Vec<Expr> = hash_clauses.iter().map(|(_, r)| r.clone()).collect();
                let keys_l: Vec<Expr> = hash_clauses.iter().map(|(l, _)| l.clone()).collect();
                // The stats-driven choice is the v1.75 scope: one side
                // filtered and the other not, with stats backing both
                // orders' inner keys. Everything else keeps v1.64
                // behavior exactly (fail closed per the load-bearing
                // requirement).
                let stats_driven = left_where.is_some() != right_where.is_some()
                    && pg_inner_keys_have_stats(&eng.db, &inner, &keys_r)
                    && pg_inner_keys_have_stats(&eng.db, &outer, &keys_l);
                let outer_is_left = if stats_driven {
                    // PG19 tries both orders and keeps the cheaper
                    // (costsize.c). The fuzz tie-break keeps the written
                    // (left-outer) order, mirroring PG19's
                    // `compare_path_costs_fuzzily` (the first-added path
                    // wins near-ties).
                    let cost_lr = pg_hashjoin_cost_order(
                        &eng.db,
                        &outer,
                        left_where.as_ref(),
                        &inner,
                        right_where.as_ref(),
                        hash_clauses.len(),
                        Some(&keys_r[..]),
                        snap,
                        own,
                        session,
                    );
                    let cost_rl = pg_hashjoin_cost_order(
                        &eng.db,
                        &inner,
                        right_where.as_ref(),
                        &outer,
                        left_where.as_ref(),
                        hash_clauses.len(),
                        Some(&keys_l[..]),
                        snap,
                        own,
                        session,
                    );
                    match (cost_lr, cost_rl) {
                        (Some(c1), Some(c2)) if c2 * PG_STD_FUZZ_FACTOR < c1 => false,
                        _ => true,
                    }
                } else {
                    // v1.64 behavior: the filtered side probes;
                    // both/neither filtered picks the cheaper punted
                    // cost-model order (fuzz tie-break keeps left-outer).
                    match (left_where.is_some(), right_where.is_some()) {
                        (true, false) => true,
                        (false, true) => false,
                        _ => {
                            let cost_lr = pg_hashjoin_cost_order(
                                &eng.db,
                                &outer,
                                left_where.as_ref(),
                                &inner,
                                right_where.as_ref(),
                                hash_clauses.len(),
                                None,
                                snap,
                                own,
                                session,
                            );
                            let cost_rl = pg_hashjoin_cost_order(
                                &eng.db,
                                &inner,
                                right_where.as_ref(),
                                &outer,
                                left_where.as_ref(),
                                hash_clauses.len(),
                                None,
                                snap,
                                own,
                                session,
                            );
                            match (cost_lr, cost_rl) {
                                (Some(c1), Some(c2)) if c2 * PG_STD_FUZZ_FACTOR < c1 => false,
                                _ => true,
                            }
                        }
                    }
                };
                let (h_ow, h_iw, h_clauses) = if outer_is_left {
                    (left_where.as_ref(), right_where.as_ref(), clauses_lr)
                } else {
                    (right_where.as_ref(), left_where.as_ref(), clauses_rl)
                };
                // Hash wins iff provably cheaper than the nestloop plan
                // (in v1.63's order) by more than the fuzz factor.
                let (nl_o, nl_i, nl_ow, nl_iw) = if swapped {
                    (&inner, &outer, right_where.as_ref(), left_where.as_ref())
                } else {
                    (&outer, &inner, left_where.as_ref(), right_where.as_ref())
                };
                let (h_o, h_i) = if outer_is_left {
                    (&outer, &inner)
                } else {
                    (&inner, &outer)
                };
                // The hash-vs-nestloop comparison uses the chosen order with
                // the same bucket fractions as the order choice (stats
                // driven iff the choice was).
                let hash_keys: Option<&[Expr]> = if stats_driven {
                    Some(if outer_is_left {
                        &keys_r[..]
                    } else {
                        &keys_l[..]
                    })
                } else {
                    None
                };
                let hash_cost = pg_hashjoin_cost_order(
                    &eng.db,
                    h_o,
                    h_ow,
                    h_i,
                    h_iw,
                    h_clauses.len(),
                    hash_keys,
                    snap,
                    own,
                    session,
                );
                let nl_cost = pg_nestloop_cost_order(
                    &eng.db,
                    nl_o,
                    nl_ow,
                    nl_i,
                    nl_iw,
                    h_clauses.len(),
                    snap,
                    own,
                    session,
                );
                match (hash_cost, nl_cost) {
                    (Some(h), Some(n)) if h * PG_STD_FUZZ_FACTOR < n => {
                        Some((outer_is_left, h_clauses))
                    }
                    _ => None,
                }
            } else {
                None
            };
            // v1.69: merge join planning (PG19 `cost_mergejoin` vs
            // `cost_hashjoin` vs `cost_nestloop` on PG19 no-stats row
            // estimates — v1.67's principled path). Only for inner
            // equi-joins with mergeable clauses and SeqScan sides; fail
            // closed to the v1.64 choice otherwise. Merge takes
            // precedence when it fuzz-wins the no-stats three-way: PG19
            // compares all three on the same (no-stats) estimates.
            let merge_win: Option<Vec<(Expr, Expr)>> = if matches!(kind, JoinKind::Inner)
                && !on_pushed_right
                && !hash_clauses.is_empty()
            {
                if pg_merge_wins_ns(
                    &eng.db,
                    &outer,
                    left_where.as_ref(),
                    &inner,
                    right_where.as_ref(),
                    hash_clauses.len(),
                    snap,
                    own,
                    session,
                ) {
                    Some(hash_clauses.clone())
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(oriented) = merge_win {
                // v1.69: merge join wins. `Merge Cond:` renders the
                // oriented clauses PG19-style (`(outer.col = inner.col)`,
                // `AND`-joined for multi-key); each input renders under a
                // `Sort` (SeqScans are unordered — PG19's `T_Sort` above
                // `T_MergeJoin`). Written order is kept (left = outer).
                let px = pctx.unwrap();
                // PG19 `Merge Cond:`: `(a = b)` for one clause,
                // `((a = b) AND (c = d))` for multi-key.
                let clauses: Vec<String> = oriented
                    .iter()
                    .map(|(o, i)| {
                        format!(
                            "({} = {})",
                            pg_expr_text_or_debug(o, px, true),
                            pg_expr_text_or_debug(i, px, true)
                        )
                    })
                    .collect();
                let merge_cond = if clauses.len() == 1 {
                    clauses.into_iter().next().unwrap()
                } else {
                    format!("({})", clauses.join(" AND "))
                };
                // Sort keys are the clause sides' deparsed columns.
                let sort_key = |e: &Expr| pg_expr_text_or_debug(e, px, true);
                let outer_keys = oriented
                    .iter()
                    .map(|(o, _)| sort_key(o))
                    .collect::<Vec<_>>()
                    .join(", ");
                let inner_keys = oriented
                    .iter()
                    .map(|(_, i)| sort_key(i))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mk_sort = |child: PlanNode, keys: String| {
                    let rows = child.rows();
                    let output = child.output().to_vec();
                    PlanNode::Sort {
                        keys,
                        rows,
                        child: Box::new(child),
                        output,
                    }
                };
                let m_outer = mk_sort(outer, outer_keys);
                let m_inner = mk_sort(inner, inner_keys);
                let rows = m_outer.rows().saturating_mul(m_inner.rows());
                let mut m_output = m_outer.output().to_vec();
                m_output.extend(m_inner.output().iter().cloned());
                Ok(PlanNode::MergeJoin {
                    filter: None,
                    merge_cond,
                    rows,
                    outer: Box::new(m_outer),
                    inner: Box::new(m_inner),
                    kind: JoinKind::Inner,
                    output: m_output,
                })
            } else if let Some((outer_is_left, oriented)) = hash_win {
                // v1.64: hash join wins. `Hash Cond:` renders the oriented
                // clauses PG19-style (`(outer.col = inner.col)`); the
                // build (inner) side renders under a `Hash` wrapper node.
                // (outer, inner) are still the pre-swap (left, right)
                // plans; `outer_is_left` selects the hash order.
                let (h_outer, h_inner) = if outer_is_left {
                    (outer, inner)
                } else {
                    (inner, outer)
                };
                let rows = h_outer.rows().saturating_mul(h_inner.rows());
                // v1.46: VERBOSE `Output:` follows the hash order.
                let mut h_output = h_outer.output().to_vec();
                h_output.extend(h_inner.output().iter().cloned());
                let px = pctx.unwrap();
                let hash_cond = oriented
                    .iter()
                    .map(|(o, i)| {
                        format!(
                            "({} = {})",
                            pg_expr_text_or_debug(o, px, true),
                            pg_expr_text_or_debug(i, px, true)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(PlanNode::HashJoin {
                    filter: None,
                    hash_cond,
                    rows,
                    outer: Box::new(h_outer),
                    inner: Box::new(h_inner),
                    kind: JoinKind::Inner,
                    output: h_output,
                })
            } else {
                let (outer, inner) = if swapped {
                    (inner, outer)
                } else {
                    (outer, inner)
                };
                // v1.71: EC-canonical join-filter representatives (PG19
                // equivclass.c `generate_join_implied_equalities`): the clause
                // orientation is fixed by PG's first EC call — a partial index
                // on the left table flips it to (right-first, left-second),
                // otherwise (left-first, right-first) at build time. The
                // orientation persists regardless of the plan's outer/inner,
                // so this uses the build-time sides, not the swapped ones.
                // EXPLAIN-text only; skipped in legacy (non-PG-text) mode.
                if let Some(px) = pctx {
                    let mut onm = Vec::new();
                    pg_replaces_names(std::slice::from_ref(left), &mut onm);
                    let mut inm = Vec::new();
                    pg_replaces_names(std::slice::from_ref(right), &mut inm);
                    let (outer_names, inner_names): (&[String], &[String]) = if ec_partial_left {
                        (&inm, &onm)
                    } else {
                        (&onm, &inm)
                    };
                    if let Some(t) = pg_ec_join_filter_text(
                        &jf_exprs,
                        &mut join_ec,
                        outer_names,
                        inner_names,
                        &ec_split,
                        px,
                    ) {
                        join_filter = Some(t);
                    }
                }
                let rows = outer.rows().saturating_mul(inner.rows());
                // v1.46: VERBOSE `Output:` — a join's tlist starts as the
                // verbatim concatenation of its inputs' tlists (PG19). The
                // top-level join's entries are replaced with the query
                // targetlist at the end of `plan_select`.
                let mut output = outer.output().to_vec();
                output.extend(inner.output().iter().cloned());
                // v1.48: PG19 renders the join kind in the node label
                // (`Nested Loop Left Join`, ...) and may materialize the
                // inner side (see `pg_should_materialize`).
                let inner = pg_maybe_materialize(&outer, inner);
                Ok(PlanNode::NestedLoop {
                    filter: None,
                    join_filter,
                    rows,
                    outer: Box::new(outer),
                    inner: Box::new(inner),
                    kind: *kind,
                    // v1.46: VERBOSE `Output:` filled by the caller (join of
                    // children's entries); the top-level join's entries are
                    // replaced with the query targetlist at the end of
                    // `plan_select`.
                    output,
                })
            }
        }
    }
}
