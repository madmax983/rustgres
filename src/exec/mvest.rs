//! v2.30: multivariate (extended-statistics) estimation for the
//! EXPLAIN planner — the PG19 `clauselist_selectivity_ext` /
//! `statext_clauselist_selectivity` pipeline (MCV first, functional
//! dependencies second, per-clause independence for the rest) and
//! `estimate_num_groups` with MVNDistinct consumption. Sources:
//! src/backend/statistics/{extended_stats,mcv,dependencies}.c,
//! src/backend/utils/adt/selfuncs.c, src/backend/optimizer/path/
//! clausesel.c of the vendored PG19 tree (see
//! hidden_files/v230/pg19_ground_truth.md §6–§7).
//!
//! Attnum space: table columns are 1-based positions in
//! `Table.columns` (the same space `ExtStatObj` items and the
//! structured build payloads use); expressions are -1, -2, ... per
//! statistics object, in item order among its expression items.

use super::*;
use crate::sql::{CmpOp, Expr, Literal};
use crate::storage::{ExtStatData, ExtStatItem, ExtStatObj, Table, TableStats};
use std::cmp::Ordering;

/// The statistics sources for one scan: column stats plus the built
/// extended-statistics payloads. `family` selects the inherited
/// (stxdinherit) variants.
pub(crate) struct EstSource<'a> {
    pub ts: Option<&'a TableStats>,
    pub objs: Vec<(&'a ExtStatObj, &'a ExtStatData)>,
}

pub(crate) fn est_source<'a>(db: &'a Database, table: &str, family: bool) -> EstSource<'a> {
    let base = stats_base_name(table);
    if family {
        EstSource {
            ts: db.stats_inh.get(table).or_else(|| db.stats.get(table)),
            objs: db
                .ext_stats
                .iter()
                .filter(|o| stats_base_name(&o.table) == base)
                .filter_map(|o| o.data_inh.as_ref().map(|d| (o, d)))
                .filter(|(_, d)| d.built)
                .collect(),
        }
    } else {
        EstSource {
            ts: db.stats.get(table),
            objs: db
                .ext_stats
                .iter()
                .filter(|o| stats_base_name(&o.table) == base)
                .filter(|o| o.data.built)
                .map(|o| (o, &o.data))
                .collect(),
        }
    }
}

/// Total estimated rows of a table's whole family (itself included).
pub(crate) fn family_rel_rows(
    db: &Database,
    table: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> f64 {
    family_tables(db, table, snap, own, session)
        .iter()
        .map(|m| est_rel_rows(db, m, snap, own, session) as f64)
        .sum()
}

/// Whether `table` has any inheritance/partition descendants.
pub(crate) fn has_family(
    db: &Database,
    table: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    family_tables(db, table, snap, own, session).len() > 1
}

// ---------------------------------------------------------------------
// Clause extraction
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
enum MvKind {
    Eq(Value),
    In(Vec<Value>),
    IsNull(bool), // true = IS NOT NULL
    /// v2.30: a range comparison `expr <op> value` (op one of
    /// Lt/Le/Gt/Ge). Comparisons against ANY/ALL of a literal array
    /// are collapsed to the single effective bound at extraction
    /// (PG19 scalararraysel bound semantics).
    Range {
        op: CmpOp,
        value: Value,
    },
    /// v2.30: `expr <> ALL (array)` — the NOT IN form. Estimated by
    /// selectivity complement only; never statext-compatible.
    NotIn(Vec<Value>),
    /// v2.30: a range comparison against ANY/ALL of a literal array,
    /// kept un-collapsed: PG19 scalararraysel prices each element
    /// and merges (or-combine for ANY, product for ALL), and mcv.c
    /// evaluates the quantified comparison per MCV item.
    RangeAny {
        op: CmpOp,
        any: bool,
        values: Vec<Value>,
    },
}

#[derive(Clone, Debug)]
struct MvClause {
    conj: usize,
    attnum: i32,
    /// v2.30: for expression clauses (attnum < 0), the oid of the
    /// statistics object the expression was matched against.
    /// Expression attnums are assigned per object (-1, -2, ...), so
    /// cross-object matching must compare object identity too —
    /// PG remaps dependencies/matches by expression equality within
    /// one object's attnum space. 0 for plain column clauses.
    oid: u32,
    /// v2.30: clause came from an `= ALL (array)` expansion. PG19
    /// never uses a SAOP-ALL clause as a dependency's implied (or
    /// implying) clause — it is priced by plain selectivity.
    from_all: bool,
    kind: MvKind,
}

/// Structural expression equality ignoring column qualification
/// (the planner's WHERE expressions carry the scan alias; stored
/// statistics expressions were parsed standalone).
fn expr_match(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Column { name: na, .. }, Expr::Column { name: nb, .. }) => {
            na.eq_ignore_ascii_case(nb)
        }
        (Expr::Literal(la), Expr::Literal(lb)) => lit_eq(la, lb),
        (Expr::Func { name: na, args: aa }, Expr::Func { name: nb, args: ab }) => {
            na.eq_ignore_ascii_case(nb)
                && aa.len() == ab.len()
                && aa.iter().zip(ab.iter()).all(|(x, y)| expr_match(x, y))
        }
        (
            Expr::Arith {
                op: oa,
                left: la,
                right: ra,
            },
            Expr::Arith {
                op: ob,
                left: lb,
                right: rb,
            },
        ) => oa == ob && expr_match(la, lb) && expr_match(ra, rb),
        (
            Expr::Cast {
                expr: ea, to: ta, ..
            },
            Expr::Cast {
                expr: eb, to: tb, ..
            },
        ) => ta == tb && expr_match(ea, eb),
        (Expr::Neg(ea), Expr::Neg(eb)) => expr_match(ea, eb),
        (Expr::Concat(la, ra), Expr::Concat(lb, rb)) => expr_match(la, lb) && expr_match(ra, rb),
        (
            Expr::Cmp {
                op: oa,
                left: la,
                right: ra,
            },
            Expr::Cmp {
                op: ob,
                left: lb,
                right: rb,
            },
        ) => oa == ob && expr_match(la, lb) && expr_match(ra, rb),
        _ => false,
    }
}

fn lit_eq(a: &Literal, b: &Literal) -> bool {
    use crate::sql::Literal as L;
    match (a, b) {
        (L::Int(x), L::Int(y)) => x == y,
        (L::BigInt(x), L::BigInt(y)) => x == y,
        (L::SmallInt(x), L::SmallInt(y)) => x == y,
        (L::Text(x), L::Text(y)) => x == y,
        (L::Bool(x), L::Bool(y)) => x == y,
        (L::Float(x), L::Float(y)) => x == y,
        (L::Decimal(x), L::Decimal(y)) => x == y,
        _ => false,
    }
}

/// Find the (object, attnum) for a statistics expression matching `e`.
fn match_expr_attnum<'a>(src: &EstSource<'a>, e: &Expr) -> Option<(&'a ExtStatObj, i32)> {
    for (obj, _) in &src.objs {
        let attnums = item_attnums(obj);
        for (i, item) in obj.items.iter().enumerate() {
            if let ExtStatItem::Expr(_, stored) = item {
                if expr_match(e, stored) {
                    return Some((obj, attnums[i]));
                }
            }
        }
    }
    None
}

/// Extract the multivariate-compatible clauses (equality / IN /
/// NULL tests on one column or one statistics expression) from the
/// WHERE conjuncts of a single-table scan.
fn extract_clauses(
    src: &EstSource,
    table: &str,
    qual: &str,
    t: &Table,
    conjuncts: &[&Expr],
) -> Vec<MvClause> {
    let mut out: Vec<MvClause> = Vec::new();
    // One equality side: returns (attnum, object oid, coerced
    // value). The oid is 0 for plain columns and synthetic
    // expressions; for statistics expressions it identifies the
    // owning object (expression attnums are per-object).
    let eq_side = |c: &Expr, ci: usize| -> Option<(i32, u32, Value)> {
        if let Some(bs) = conjunct_bounds(c, qual, table, &t.columns) {
            if let [(pos, IndexBoundKind::Eq, v)] = bs.as_slice() {
                return Some((*pos as i32 + 1, 0, v.clone()));
            }
            return None;
        }
        // Expression equality: <expr> = <literal> (either order),
        // where <expr> is not a plain column.
        if let Expr::Cmp {
            op: CmpOp::Eq,
            left,
            right,
        } = c
        {
            for (eside, lside) in [(left, right), (right, left)] {
                if matches!(&**eside, Expr::Column { .. }) {
                    continue;
                }
                if let Expr::Literal(l) = &**lside {
                    if let Some((obj, att)) = match_expr_attnum(src, eside) {
                        return Some((att, obj.oid, l.clone().into_value()));
                    }
                    // v2.30: an expression with no statistics of its
                    // own still gets PG's eqsel treatment (default
                    // ndistinct 200 via expr_eq_sel's fallthrough).
                    // Give it a synthetic per-conjunct attnum that no
                    // statistics object can match, so the MCV /
                    // dependency phases skip it but the independence
                    // phase prices it.
                    let mut cols = Vec::new();
                    pull_columns(eside, t, &mut cols);
                    if !cols.is_empty() {
                        return Some((-(10_000 + ci as i32), 0, l.clone().into_value()));
                    }
                }
            }
        }
        None
    };
    for (ci, c) in conjuncts.iter().enumerate() {
        // v2.30: quantified comparisons over literal arrays, which
        // the parser desugars to the hidden __any_all_array builtin:
        // (left, op-text, quant-text, array).
        if let Expr::Func { name, args } = c {
            if name == "__any_all_array" && args.len() == 4 {
                if let (
                    Expr::Literal(crate::sql::Literal::Text(op)),
                    Expr::Literal(crate::sql::Literal::Text(quant)),
                ) = (&args[1], &args[2])
                {
                    let op = op.as_ref();
                    let quant = quant.as_ref();
                    let left = &args[0];
                    let elems: Vec<crate::sql::Literal> =
                        if let Expr::ArrayCtor { elems, .. } = &args[3] {
                            let lits: Vec<crate::sql::Literal> = elems
                                .iter()
                                .filter_map(|e| match e {
                                    Expr::Literal(l) => Some(l.clone()),
                                    _ => None,
                                })
                                .collect();
                            // Only a fully-literal array is priced;
                            // a partial collection would misstate
                            // the clause.
                            if lits.len() == elems.len() {
                                lits
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        };
                    if !elems.is_empty() {
                        // Resolve each element through the equality
                        // machinery (coercion + attnum resolution).
                        let mut resolved: Vec<(i32, u32, Value)> = Vec::new();
                        let mut ok = true;
                        for l in &elems {
                            let cmp = Expr::Cmp {
                                op: CmpOp::Eq,
                                left: Box::new(left.clone()),
                                right: Box::new(Expr::Literal(l.clone())),
                            };
                            match eq_side(&cmp, ci) {
                                Some(r) => resolved.push(r),
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok
                            && resolved
                                .iter()
                                .all(|(a, o, _)| *a == resolved[0].0 && *o == resolved[0].1)
                        {
                            let attnum = resolved[0].0;
                            let oid = resolved[0].1;
                            let vals: Vec<Value> =
                                resolved.into_iter().map(|(_, _, v)| v).collect();
                            match (op, quant) {
                                ("=", "any") => {
                                    out.push(MvClause {
                                        conj: ci,
                                        attnum,
                                        oid,
                                        from_all: false,
                                        kind: MvKind::In(vals),
                                    });
                                    continue;
                                }
                                ("=", "all") => {
                                    for v in vals {
                                        out.push(MvClause {
                                            conj: ci,
                                            attnum,
                                            oid,
                                            from_all: true,
                                            kind: MvKind::Eq(v),
                                        });
                                    }
                                    continue;
                                }
                                ("<>", "all") => {
                                    out.push(MvClause {
                                        conj: ci,
                                        attnum,
                                        oid,
                                        from_all: false,
                                        kind: MvKind::NotIn(vals),
                                    });
                                    continue;
                                }
                                ("<", q) | ("<=", q) | (">", q) | (">=", q) => {
                                    // NULL elements never contribute
                                    // (strict operator semantics).
                                    let nonnull: Vec<Value> = vals
                                        .into_iter()
                                        .filter(|v| !matches!(v, Value::Null))
                                        .collect();
                                    if nonnull.is_empty() {
                                        continue;
                                    }
                                    let eff = match op {
                                        "<" => CmpOp::Lt,
                                        "<=" => CmpOp::Le,
                                        ">" => CmpOp::Gt,
                                        _ => CmpOp::Ge,
                                    };
                                    out.push(MvClause {
                                        conj: ci,
                                        attnum,
                                        oid,
                                        from_all: false,
                                        kind: MvKind::RangeAny {
                                            op: eff,
                                            any: q == "any",
                                            values: nonnull,
                                        },
                                    });
                                    continue;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        // Plain equality (column or expression).
        if let Some((attnum, oid, v)) = eq_side(c, ci) {
            out.push(MvClause {
                conj: ci,
                attnum,
                oid,
                from_all: false,
                kind: MvKind::Eq(v),
            });
            continue;
        }
        // v2.30: range comparisons (column or expression against a
        // literal), the MCV-compatible operator clauses of mcv.c.
        if let Expr::Cmp { op, left, right } = c {
            if matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge) {
                // Normalize to `side <op> literal` with the compared
                // expression on the left (flipping the operator when
                // the literal is on the left).
                let norm = if matches!(&**right, Expr::Literal(_)) {
                    Some((left, *op))
                } else if matches!(&**left, Expr::Literal(_)) {
                    let flipped = match op {
                        CmpOp::Lt => CmpOp::Gt,
                        CmpOp::Le => CmpOp::Ge,
                        CmpOp::Gt => CmpOp::Lt,
                        _ => CmpOp::Le,
                    };
                    Some((right, flipped))
                } else {
                    None
                };
                if let Some((side, eff_op)) = norm {
                    let lit = if matches!(&**right, Expr::Literal(_)) {
                        right
                    } else {
                        left
                    };
                    let lv = match &**lit {
                        Expr::Literal(l) => Some(l.clone().into_value()),
                        _ => None,
                    };
                    if let Some(lv) = lv {
                        // Resolve the compared side: plain column
                        // (coerced through conjunct_bounds via a
                        // synthetic equality probe is wrong for range
                        // coercion; use the bounds machinery
                        // directly), statistics expression, or a
                        // generic expression.
                        let mut att: Option<(i32, u32)> = None;
                        if let Expr::Column { table: cq, name } = &**side {
                            if cq.as_ref().is_none_or(|q| q == qual || q == table) {
                                if let Some(pos) = t.columns.iter().position(|(n, _)| n == name) {
                                    att = Some((pos as i32 + 1, 0));
                                }
                            }
                        }
                        if att.is_none() && !matches!(&**side, Expr::Column { .. }) {
                            if let Some((obj, a)) = match_expr_attnum(src, side) {
                                att = Some((a, obj.oid));
                            } else {
                                let mut cols = Vec::new();
                                pull_columns(side, t, &mut cols);
                                if !cols.is_empty() {
                                    att = Some((-(10_000 + ci as i32), 0));
                                }
                            }
                        }
                        if let Some((a, oid)) = att {
                            out.push(MvClause {
                                conj: ci,
                                attnum: a,
                                oid,
                                from_all: false,
                                kind: MvKind::Range {
                                    op: eff_op,
                                    value: lv,
                                },
                            });
                            continue;
                        }
                    }
                }
            }
        }
        // IS [NOT] NULL on a column.
        if let Expr::IsNull { expr, neg } = c {
            if let Expr::Column { table: cq, name } = &**expr {
                if cq.as_ref().is_none_or(|q| q == qual || q == table) {
                    if let Some(pos) = t.columns.iter().position(|(n, _)| n == name) {
                        out.push(MvClause {
                            conj: ci,
                            attnum: pos as i32 + 1,
                            oid: 0,
                            from_all: false,
                            kind: MvKind::IsNull(*neg),
                        });
                        continue;
                    }
                }
            }
        }
        // v2.30: a bare boolean column (or its negation) is a
        // restriction clause in PG (boolvarsel): price it as an
        // equality on the boolean value so MCV lists apply.
        {
            let bool_side: Option<(&Expr, bool)> = match c {
                Expr::Column { .. } => Some((c, true)),
                Expr::Not(inner) => Some((&**inner, false)),
                _ => None,
            };
            if let Some((side, want)) = bool_side {
                if let Expr::Column { table: cq, name } = side {
                    if cq.as_ref().is_none_or(|q| q == qual || q == table) {
                        if let Some(pos) = t
                            .columns
                            .iter()
                            .position(|(n, ty)| n == name && matches!(ty, ColType::Bool))
                        {
                            out.push(MvClause {
                                conj: ci,
                                attnum: pos as i32 + 1,
                                oid: 0,
                                from_all: false,
                                kind: MvKind::Eq(Value::Bool(want)),
                            });
                            continue;
                        }
                    }
                }
            }
        }
        // IN list, desugared by the parser into an OR of equalities
        // on the same side.
        if matches!(c, Expr::Or(..)) {
            let mut leaves: Vec<&Expr> = Vec::new();
            let mut stack: Vec<&Expr> = vec![c];
            let mut ok = true;
            while let Some(e) = stack.pop() {
                match e {
                    Expr::Or(a, b) => {
                        stack.push(a);
                        stack.push(b);
                    }
                    _ => leaves.push(e),
                }
            }
            let mut att: Option<(i32, u32)> = None;
            let mut vals: Vec<Value> = Vec::new();
            for leaf in leaves {
                match eq_side(leaf, ci) {
                    Some((a, o, v)) => {
                        if att.is_some_and(|x| x != (a, o)) {
                            ok = false;
                            break;
                        }
                        att = Some((a, o));
                        vals.push(v);
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if let Some((a, o)) = att {
                    if !vals.is_empty() {
                        out.push(MvClause {
                            conj: ci,
                            attnum: a,
                            oid: o,
                            from_all: false,
                            kind: MvKind::In(vals),
                        });
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Per-clause (non-multivariate) selectivity of an extracted clause
// ---------------------------------------------------------------------

/// Is this clause usable by the given statistics object in an MCV
/// evaluation? Attnum must be one of the object's keys, and an
/// expression clause (attnum < 0) must belong to THIS object —
/// expression attnums are only unique within an object.
fn clause_fits(c: &MvClause, obj: &ExtStatObj, keys: &[i32]) -> bool {
    keys.contains(&c.attnum)
        && (c.attnum > 0 || c.oid == obj.oid)
        && matches!(
            c.kind,
            MvKind::Eq(_)
                | MvKind::In(_)
                | MvKind::IsNull(_)
                | MvKind::Range { .. }
                | MvKind::RangeAny { .. }
        )
}

/// Does the MCV dimension value `dv` satisfy `dv <op> v`?
fn range_match(op: &CmpOp, dv: &Value, v: &Value) -> bool {
    let ord = index_key_cmp(dv, v);
    match op {
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Ge => ord != Ordering::Less,
        _ => false,
    }
}

fn expr_eq_sel(src: &EstSource, oid: u32, attnum: i32, v: &Value) -> f64 {
    // Expression statistics live on the object that owns the attnum;
    // expression attnums are per-object, so the oid must match too.
    for (obj, data) in &src.objs {
        if obj.oid != oid {
            continue;
        }
        let attnums = item_attnums(obj);
        if let Some(pos) = attnums.iter().position(|a| *a == attnum) {
            if matches!(obj.items[pos], ExtStatItem::Expr(..)) {
                // expr_stats are parallel to the Expr items.
                let expr_ord = obj.items[..pos]
                    .iter()
                    .filter(|i| matches!(i, ExtStatItem::Expr(..)))
                    .count();
                if let Some(es) = data.expr_stats.get(expr_ord) {
                    if let Some((_, f)) = es
                        .mcv
                        .iter()
                        .find(|(mv, _)| index_key_cmp(mv, v) == Ordering::Equal)
                    {
                        return f.clamp(0.0001, 1.0);
                    }
                    let mcv_total: f64 = es.mcv.iter().map(|(_, f)| f).sum();
                    let rest = (1.0 - es.null_frac - mcv_total).max(0.0);
                    let nd = (es.n_distinct - es.mcv.len() as f64).max(1.0);
                    return (rest / nd).clamp(0.0001, 1.0);
                }
            }
        }
    }
    // No expression statistics: PG's DEFAULT_NUM_DISTINCT.
    1.0 / 200.0
}

fn simple_sel(src: &EstSource, t: &Table, cl: &MvClause) -> f64 {
    match &cl.kind {
        MvKind::Eq(v) => {
            if cl.attnum > 0 {
                let name = &t.columns[(cl.attnum - 1) as usize].0;
                est_eq_sel(src.ts, name, v)
            } else {
                expr_eq_sel(src, cl.oid, cl.attnum, v)
            }
        }
        MvKind::In(vals) => {
            // PG19 scalararraysel: for `= ANY` / IN, the element
            // probabilities are DISJOINT (distinct values of one
            // attribute), so PG sums them (the s1disjoint rule) and
            // only falls back to s1+s2-s1*s2 merging when the sum
            // leaves [0, 1]. A written single-attribute OR group is
            // indistinguishable from an IN list in our AST (the
            // parser desugars IN to OR); PG prices the written OR
            // with the merge rule instead — a known small delta.
            let s: f64 = vals
                .iter()
                .filter(|v| !matches!(v, Value::Null))
                .map(|v| {
                    simple_sel(
                        src,
                        t,
                        &MvClause {
                            conj: cl.conj,
                            attnum: cl.attnum,
                            oid: cl.oid,
                            from_all: false,
                            kind: MvKind::Eq(v.clone()),
                        },
                    )
                })
                .sum();
            if s > 1.0 {
                let mut m = 0.0f64;
                for v in vals {
                    let s2 = simple_sel(
                        src,
                        t,
                        &MvClause {
                            conj: cl.conj,
                            attnum: cl.attnum,
                            oid: cl.oid,
                            from_all: false,
                            kind: MvKind::Eq(v.clone()),
                        },
                    );
                    m = m + s2 - m * s2;
                }
                m.clamp(0.0, 1.0)
            } else {
                s
            }
        }
        MvKind::NotIn(vals) => {
            // Complement of the IN selectivity (disjoint sum, the
            // scalararraysel s1disjoint rule for <> ALL).
            let s: f64 = vals
                .iter()
                .filter(|v| !matches!(v, Value::Null))
                .map(|v| {
                    simple_sel(
                        src,
                        t,
                        &MvClause {
                            conj: cl.conj,
                            attnum: cl.attnum,
                            oid: cl.oid,
                            from_all: false,
                            kind: MvKind::Eq(v.clone()),
                        },
                    )
                })
                .sum();
            (1.0 - s).clamp(0.0, 1.0)
        }
        MvKind::Range { op, value } => {
            if cl.attnum > 0 {
                let name = &t.columns[(cl.attnum - 1) as usize].0;
                est_ineq_sel(src.ts, name, *op, value)
            } else if cl.attnum < -10_000 {
                // Generic expression without statistics: PG's
                // default inequality selectivity.
                1.0 / 3.0
            } else {
                // Statistics expression: no histogram is kept for
                // expression stats, so estimate from its MCV list —
                // matched frequency plus a proportional share of
                // the non-MCV mass.
                let mut sel = 1.0 / 3.0;
                for (obj, data) in &src.objs {
                    if obj.oid != cl.oid {
                        continue;
                    }
                    let attnums = item_attnums(obj);
                    if let Some(pos) = attnums.iter().position(|a| *a == cl.attnum) {
                        let expr_ord = obj.items[..pos]
                            .iter()
                            .filter(|i| matches!(i, ExtStatItem::Expr(..)))
                            .count();
                        if let Some(es) = data.expr_stats.get(expr_ord) {
                            let (mut mf, mut mc) = (0.0f64, 0usize);
                            for (mv, f) in &es.mcv {
                                if range_match(op, mv, value) {
                                    mf += f;
                                    mc += 1;
                                }
                            }
                            let total: f64 = es.mcv.iter().map(|(_, f)| f).sum();
                            let rest = (1.0 - es.null_frac - total).max(0.0);
                            let frac = if es.mcv.is_empty() {
                                0.0
                            } else {
                                mc as f64 / es.mcv.len() as f64
                            };
                            sel = (mf + rest * frac).clamp(0.0, 1.0);
                        }
                    }
                }
                sel
            }
        }
        MvKind::RangeAny { op, any, values } => {
            let mut acc = if *any { 0.0 } else { 1.0 };
            for v in values {
                let one = MvClause {
                    conj: cl.conj,
                    attnum: cl.attnum,
                    oid: cl.oid,
                    from_all: false,
                    kind: MvKind::Range {
                        op: *op,
                        value: v.clone(),
                    },
                };
                let s2 = simple_sel(src, t, &one);
                acc = if *any { acc + s2 - acc * s2 } else { acc * s2 };
            }
            acc.clamp(0.0, 1.0)
        }
        MvKind::IsNull(neg) => {
            let nf = if cl.attnum > 0 {
                let name = &t.columns[(cl.attnum - 1) as usize].0;
                src.ts
                    .and_then(|ts| ts.cols.get(name))
                    .map(|cs| cs.null_frac)
                    .unwrap_or(0.005)
            } else {
                // Expression null fraction from its stats entry.
                let mut nf = 0.005;
                for (obj, data) in &src.objs {
                    let attnums = item_attnums(obj);
                    if let Some(pos) = attnums.iter().position(|a| *a == cl.attnum) {
                        let expr_ord = obj.items[..pos]
                            .iter()
                            .filter(|i| matches!(i, ExtStatItem::Expr(..)))
                            .count();
                        if let Some(es) = data.expr_stats.get(expr_ord) {
                            nf = es.null_frac;
                        }
                    }
                }
                nf
            };
            if *neg { 1.0 - nf } else { nf }
        }
    }
}

// ---------------------------------------------------------------------
// Scan-row estimation (clauselist_selectivity_ext port)
// ---------------------------------------------------------------------

/// v2.30: selectivity of one OR group (PG19
/// `clauselist_selectivity_or`): the MCV phase runs jointly over
/// all disjuncts (an item counts when it fully matches ANY
/// disjunct's clause set), functional dependencies do not apply
/// inside OR, and the remaining disjuncts are folded in with
/// s1+s2-s1*s2. Each disjunct is itself an AND list.
fn or_clause_sel(
    db: &Database,
    src: &EstSource,
    table: &str,
    qual: &str,
    t: &Table,
    or_expr: &Expr,
    depth: usize,
) -> f64 {
    // Flatten the OR into disjuncts.
    let mut disjuncts: Vec<&Expr> = Vec::new();
    let mut stack: Vec<&Expr> = vec![or_expr];
    while let Some(e) = stack.pop() {
        match e {
            Expr::Or(a, b) => {
                stack.push(a);
                stack.push(b);
            }
            _ => disjuncts.push(e),
        }
    }
    // Per-disjunct conjuncts + extracted clauses.
    let groups: Vec<(Vec<&Expr>, Vec<MvClause>)> = disjuncts
        .iter()
        .map(|d| {
            let conjs = split_conjuncts(d);
            let cls = extract_clauses(src, table, qual, t, &conjs);
            (conjs, cls)
        })
        .collect();
    let mut covered = vec![false; groups.len()];
    let mut sel = 0.0f64;
    // ---- Joint MCV phase (PG19 statext_mcv_clauselist_selectivity
    // with is_or=true): per chosen object, walk its covered
    // disjuncts in order, maintaining the bitmap of MCV items
    // already matched; each disjunct contributes its own combined
    // selectivity minus the overlap with what came before. A
    // disjunct mentioning a single attribute/expression is "simple":
    // its contribution is the plain selectivity, and the MCV list
    // only shapes the overlap correction.
    let combine = |simple: f64, mcv_sel: f64, mcv_base: f64, total: f64| -> f64 {
        let mut other = (simple - mcv_base).clamp(0.0, 1.0);
        other = other.min(1.0 - total);
        (mcv_sel + other).clamp(0.0, 1.0)
    };
    loop {
        let mut best: Option<usize> = None;
        let mut best_score = 0usize;
        for (oi, (obj, data)) in src.objs.iter().enumerate() {
            if !obj.kinds.contains(&'m') || data.mcv.is_empty() {
                continue;
            }
            let keys = item_attnums(obj);
            let score = groups
                .iter()
                .enumerate()
                .filter(|(gi, (_, cls))| {
                    !covered[*gi]
                        && !cls.is_empty()
                        && cls.iter().all(|c| clause_fits(c, obj, &keys))
                })
                .count();
            if score > best_score {
                best_score = score;
                best = Some(oi);
            }
        }
        let Some(oi) = best else { break };
        let (obj, data) = src.objs[oi];
        let keys = item_attnums(obj);
        let stat_groups: Vec<usize> = groups
            .iter()
            .enumerate()
            .filter(|(gi, (_, cls))| {
                !covered[*gi] && !cls.is_empty() && cls.iter().all(|c| clause_fits(c, obj, &keys))
            })
            .map(|(gi, _)| gi)
            .collect();
        if stat_groups.is_empty() {
            break;
        }
        let total_sel: f64 = data.mcv.iter().map(|(_, f, _)| *f).sum();
        let mut or_matched = vec![false; data.mcv.len()];
        let mut simple_or_sel = 0.0f64;
        let mut stat_sel = 0.0f64;
        for gi in &stat_groups {
            let (conjs, cls) = &groups[*gi];
            // Plain selectivity of this disjunct (no statext).
            let mut simple_sel_g = 1.0f64;
            for (i, c) in conjs.iter().enumerate() {
                if let Some(mc) = cls.iter().find(|mc| mc.conj == i) {
                    simple_sel_g *= simple_sel(src, t, mc);
                } else if matches!(c, Expr::Or(..)) && depth < 4 {
                    simple_sel_g *= or_clause_sel(db, src, table, qual, t, c, depth + 1);
                } else {
                    simple_sel_g *= est_conjunct_sel_ts(db, table, qual, &t.columns, c, src.ts);
                }
            }
            let overlap_simple = simple_or_sel * simple_sel_g;
            simple_or_sel = simple_or_sel + simple_sel_g - overlap_simple;
            // MCV item matches for this disjunct + overlap with the
            // items matched by earlier disjuncts.
            let (mut mcv_sel, mut mcv_base) = (0.0f64, 0.0f64);
            let (mut overlap_mcv, mut overlap_base) = (0.0f64, 0.0f64);
            for (ii, (vals, freq, base)) in data.mcv.iter().enumerate() {
                let m = cls.iter().all(|c| {
                    let pos = keys.iter().position(|k| *k == c.attnum).unwrap();
                    let dim = &vals[pos];
                    match &c.kind {
                        MvKind::Eq(v) => dim
                            .as_ref()
                            .is_some_and(|dv| index_key_cmp(dv, v) == Ordering::Equal),
                        MvKind::In(vs) => dim.as_ref().is_some_and(|dv| {
                            vs.iter().any(|v| index_key_cmp(dv, v) == Ordering::Equal)
                        }),
                        MvKind::IsNull(neg) => {
                            if *neg {
                                dim.is_some()
                            } else {
                                dim.is_none()
                            }
                        }
                        MvKind::Range { op, value } => {
                            !matches!(dim, None)
                                && dim.as_ref().is_some_and(|dv| range_match(op, dv, value))
                        }
                        MvKind::RangeAny { op, any, values } => dim.as_ref().is_some_and(|dv| {
                            if *any {
                                values.iter().any(|v| range_match(op, dv, v))
                            } else {
                                values.iter().all(|v| range_match(op, dv, v))
                            }
                        }),
                        _ => false,
                    }
                });
                if m {
                    mcv_sel += freq;
                    mcv_base += base;
                    if or_matched[ii] {
                        overlap_mcv += freq;
                        overlap_base += base;
                    }
                    or_matched[ii] = true;
                }
            }
            let distinct_atts: std::collections::HashSet<i32> =
                cls.iter().map(|c| c.attnum).collect();
            let clause_sel = if distinct_atts.len() <= 1 {
                simple_sel_g
            } else {
                combine(simple_sel_g, mcv_sel, mcv_base, total_sel)
            };
            let overlap_sel = combine(overlap_simple, overlap_mcv, overlap_base, total_sel);
            stat_sel = (stat_sel + clause_sel - overlap_sel).clamp(0.0, 1.0);
        }
        sel = sel + stat_sel - sel * stat_sel;
        for gi in stat_groups {
            covered[gi] = true;
        }
    }
    // ---- Remaining disjuncts: independence within, OR across -------
    for (gi, (conjs, cls)) in groups.iter().enumerate() {
        if covered[gi] {
            continue;
        }
        let mut g = 1.0f64;
        for (i, c) in conjs.iter().enumerate() {
            if let Some(mc) = cls.iter().find(|mc| mc.conj == i) {
                g *= simple_sel(src, t, mc);
            } else if matches!(c, Expr::Or(..)) && depth < 4 {
                g *= or_clause_sel(db, src, table, qual, t, c, depth + 1);
            } else {
                g *= est_conjunct_sel_ts(db, table, qual, &t.columns, c, src.ts);
            }
            if g <= 0.0 {
                break;
            }
        }
        sel = sel + g - sel * g;
    }
    sel.clamp(0.0, 1.0)
}

/// Estimated output rows of a single-table scan after its WHERE
/// slice, with multivariate statistics applied (MCV, then
/// dependencies, then independence for the remaining conjuncts).
/// Mirrors `est_filtered_rows`' clamping (never 0 on a non-empty
/// relation).
pub(crate) fn est_scan_rows(
    db: &Database,
    table: &str,
    qual: &str,
    t: &Table,
    rel_rows: f64,
    where_: Option<&Expr>,
    family: bool,
) -> f64 {
    let Some(w) = where_ else {
        return rel_rows.max(0.0);
    };
    let src = est_source(db, table, family);
    let conjuncts = split_conjuncts(w);
    let clauses = extract_clauses(&src, table, qual, t, &conjuncts);
    let mut estimated = vec![false; conjuncts.len()];
    let mut sel = 1.0f64;

    // ---- MCV phase -------------------------------------------------
    loop {
        // Choose the MCV object covering the most unestimated
        // compatible clauses (choose_best_statistics).
        let mut best: Option<usize> = None;
        let mut best_score = 0usize;
        for (oi, (obj, data)) in src.objs.iter().enumerate() {
            if !obj.kinds.contains(&'m') || data.mcv.is_empty() {
                continue;
            }
            let keys = item_attnums(obj);
            let score = clauses
                .iter()
                .filter(|c| !estimated[c.conj] && clause_fits(c, obj, &keys))
                .count();
            if score > best_score {
                best_score = score;
                best = Some(oi);
            }
        }
        let Some(oi) = best else { break };
        let (obj, data) = src.objs[oi];
        let keys = item_attnums(obj);
        let stat_clauses: Vec<&MvClause> = clauses
            .iter()
            .filter(|c| !estimated[c.conj] && clause_fits(c, obj, &keys))
            .collect();
        if stat_clauses.is_empty() {
            break;
        }
        let simple: f64 = stat_clauses
            .iter()
            .map(|c| simple_sel(&src, t, c))
            .product();
        let total_sel: f64 = data.mcv.iter().map(|(_, f, _)| *f).sum();
        let (mut mcv_sel, mut mcv_base) = (0.0f64, 0.0f64);
        for (vals, freq, base) in &data.mcv {
            let matches = stat_clauses.iter().all(|c| {
                let pos = keys.iter().position(|k| *k == c.attnum).unwrap();
                let dim = &vals[pos];
                match &c.kind {
                    MvKind::Eq(v) => dim
                        .as_ref()
                        .is_some_and(|dv| index_key_cmp(dv, v) == Ordering::Equal),
                    MvKind::In(vs) => dim.as_ref().is_some_and(|dv| {
                        vs.iter().any(|v| index_key_cmp(dv, v) == Ordering::Equal)
                    }),
                    MvKind::RangeAny { op, any, values } => dim.as_ref().is_some_and(|dv| {
                        if *any {
                            values.iter().any(|v| range_match(op, dv, v))
                        } else {
                            values.iter().all(|v| range_match(op, dv, v))
                        }
                    }),
                    MvKind::IsNull(neg) => {
                        if *neg {
                            dim.is_some()
                        } else {
                            dim.is_none()
                        }
                    }
                    MvKind::Range { op, value } => {
                        dim.as_ref().is_some_and(|dv| range_match(op, dv, value))
                    }
                    MvKind::NotIn(_) => false,
                }
            });
            if matches {
                mcv_sel += freq;
                mcv_base += base;
            }
        }
        // mcv_combine_selectivities.
        let mut other = (simple - mcv_base).clamp(0.0, 1.0);
        other = other.min(1.0 - total_sel);
        let stat_sel = (mcv_sel + other).clamp(0.0, 1.0);
        sel *= stat_sel;
        for c in &stat_clauses {
            estimated[c.conj] = true;
        }
    }

    // ---- Dependencies phase ----------------------------------------
    {
        let eligible: Vec<&MvClause> = clauses
            .iter()
            .filter(|c| {
                !estimated[c.conj] && !c.from_all && matches!(c.kind, MvKind::Eq(_) | MvKind::In(_))
            })
            .collect();
        let mut involved: Vec<i32> = eligible.iter().map(|c| c.attnum).collect();
        involved.sort_unstable();
        involved.dedup();
        if involved.len() >= 2 {
            // Candidate dependencies fully covered by the clause
            // attributes.
            let mut cand: Vec<(Vec<i32>, i32, f64)> = Vec::new();
            for (obj, data) in &src.objs {
                if !obj.kinds.contains(&'f') {
                    continue;
                }
                for (ante, implied, degree) in &data.deps_items {
                    // Expression attnums are per-object: every
                    // attribute the dependency mentions must be
                    // priced by a clause of THIS object.
                    let oid_ok = std::iter::once(implied).chain(ante.iter()).all(|a| {
                        *a > 0 || eligible.iter().any(|c| c.attnum == *a && c.oid == obj.oid)
                    });
                    if oid_ok
                        && involved.contains(implied)
                        && ante.iter().all(|a| involved.contains(a))
                    {
                        cand.push((ante.clone(), *implied, *degree));
                    }
                }
            }
            // find_strongest_dependency loop: widest first, then
            // highest degree; each attribute implied at most once.
            let mut applied: Vec<(Vec<i32>, i32, f64)> = Vec::new();
            let mut implied_set: Vec<i32> = Vec::new();
            loop {
                let mut pick: Option<usize> = None;
                for (i, (ante, implied, degree)) in cand.iter().enumerate() {
                    if implied_set.contains(implied) {
                        continue;
                    }
                    let better = match pick {
                        None => true,
                        Some(p) => {
                            let (pa, _, pd) = &cand[p];
                            (ante.len() + 1, *degree) > (pa.len() + 1, *pd)
                        }
                    };
                    if better {
                        pick = Some(i);
                    }
                }
                let Some(p) = pick else { break };
                let dep = cand[p].clone();
                implied_set.push(dep.1);
                applied.push(dep);
            }
            if !applied.is_empty() {
                // Per-attribute plain selectivities.
                let mut attr_sel: Vec<(i32, f64)> = involved
                    .iter()
                    .map(|a| {
                        let s: f64 = eligible
                            .iter()
                            .filter(|c| c.attnum == *a)
                            .map(|c| simple_sel(&src, t, c))
                            .product();
                        (*a, s)
                    })
                    .collect();
                let get = |m: &Vec<(i32, f64)>, a: i32| {
                    m.iter()
                        .find(|(x, _)| *x == a)
                        .map(|(_, s)| *s)
                        .unwrap_or(1.0)
                };
                // Apply in reverse order (narrowest last-picked
                // first), replacing the implied attribute's
                // selectivity with the conditional.
                for (ante, implied, f) in applied.iter().rev() {
                    let s1: f64 = ante.iter().map(|a| get(&attr_sel, *a)).product();
                    let s2 = get(&attr_sel, *implied);
                    let cond = if s1 <= 0.0 {
                        s2
                    } else if s1 <= s2 {
                        f + (1.0 - f) * s2
                    } else {
                        f * s2 / s1 + (1.0 - f) * s2
                    };
                    for e in attr_sel.iter_mut() {
                        if e.0 == *implied {
                            e.1 = cond;
                        }
                    }
                }
                let dep_sel: f64 = attr_sel.iter().map(|(_, s)| *s).product();
                sel *= dep_sel.clamp(0.0, 1.0);
                for c in &eligible {
                    estimated[c.conj] = true;
                }
            }
        }
    }

    // ---- Remaining conjuncts: independence --------------------------
    for (i, c) in conjuncts.iter().enumerate() {
        if estimated[i] {
            continue;
        }
        let mine: Vec<&MvClause> = clauses.iter().filter(|mc| mc.conj == i).collect();
        if !mine.is_empty() {
            for mc in mine {
                sel *= simple_sel(&src, t, mc);
            }
        } else if matches!(c, Expr::Or(..)) {
            // v2.30: a general OR group (not a single-attribute IN
            // desugar) is one clause with OR selectivity.
            sel *= or_clause_sel(db, &src, table, qual, t, c, 0);
        } else {
            sel *= est_conjunct_sel_ts(db, table, qual, &t.columns, c, src.ts);
        }
        if sel <= 0.0 {
            break;
        }
    }
    (rel_rows * sel).max(if rel_rows > 0.0 { 1.0 } else { 0.0 })
}

// ---------------------------------------------------------------------
// estimate_num_groups (selfuncs.c port)
// ---------------------------------------------------------------------

fn var_ndistinct(src: &EstSource, t: &Table, attnum: i32, oid: u32, tuples: f64) -> f64 {
    if attnum == 0 {
        // v2.30: the ctid sentinel (a system column pulled into the
        // grouping vars): unique per row, like PG's SelfItemPointer
        // case in get_variable_numdistinct.
        return tuples.max(1.0);
    }
    if attnum > 0 {
        let name = &t.columns[(attnum - 1) as usize].0;
        if let Some(cs) = src.ts.and_then(|ts| ts.cols.get(name)) {
            if cs.n_distinct == 0.0 {
                // PG19: stadistinct 0 means "unknown" (e.g. an
                // all-NULL column): fall back to the relation size
                // for small relations, else DEFAULT_NUM_DISTINCT.
                return if tuples < 200.0 {
                    tuples.max(1.0)
                } else {
                    200.0
                };
            }
            return cs.n_distinct.max(1.0);
        }
        200.0
    } else {
        for (obj, data) in &src.objs {
            if obj.oid != oid {
                continue;
            }
            let attnums = item_attnums(obj);
            if let Some(pos) = attnums.iter().position(|a| *a == attnum) {
                let expr_ord = obj.items[..pos]
                    .iter()
                    .filter(|i| matches!(i, ExtStatItem::Expr(..)))
                    .count();
                if let Some(es) = data.expr_stats.get(expr_ord) {
                    return es.n_distinct.max(1.0);
                }
            }
        }
        200.0
    }
}

/// Collect the plain columns referenced by an expression
/// (pull_var_clause for the no-stats decomposition path).
fn pull_columns(e: &Expr, t: &Table, out: &mut Vec<i32>) {
    match e {
        Expr::Column { name, .. } => {
            if let Some(pos) = t.columns.iter().position(|(n, _)| n == name) {
                let a = pos as i32 + 1;
                if !out.contains(&a) {
                    out.push(a);
                }
            } else if name.eq_ignore_ascii_case("ctid") && !out.contains(&0) {
                out.push(0);
            }
        }
        Expr::Arith { left, right, .. } => {
            pull_columns(left, t, out);
            pull_columns(right, t, out);
        }
        Expr::Func { args, .. } => {
            for a in args {
                pull_columns(a, t, out);
            }
        }
        Expr::Cast { expr, .. } => pull_columns(expr, t, out),
        Expr::Neg(x) => pull_columns(x, t, out),
        Expr::Concat(a, b) => {
            pull_columns(a, t, out);
            pull_columns(b, t, out);
        }
        Expr::Cmp { left, right, .. } => {
            pull_columns(left, t, out);
            pull_columns(right, t, out);
        }
        _ => {}
    }
}

/// PG19 `estimate_num_groups` for a single-relation GROUP BY /
/// DISTINCT: multivariate ndistinct items are consumed exactly,
/// remaining variables multiply their univariate ndistincts, then
/// the per-relation clamp and restriction correction apply.
/// `tuples` is the relation's total estimated rows; `input_rows`
/// the (filtered) scan estimate feeding the aggregate.
pub(crate) fn est_num_groups(
    src: &EstSource,
    t: &Table,
    tuples: f64,
    input_rows: f64,
    group_exprs: &[Expr],
) -> f64 {
    if group_exprs.is_empty() {
        return 1.0;
    }
    // A group variable: (attnum, owning-object oid). Expression
    // attnums are per-object, so the oid disambiguates.
    let mut vars: Vec<(i32, u32)> = Vec::new();
    let mut bool_factors = 0usize;
    for e in group_exprs {
        match e {
            Expr::Column { name, .. } => {
                if let Some(pos) = t.columns.iter().position(|(n, _)| n == name) {
                    let a = (pos as i32 + 1, 0);
                    if !vars.contains(&a) {
                        vars.push(a);
                    }
                } else if name.eq_ignore_ascii_case("ctid") && !vars.contains(&(0, 0)) {
                    // v2.30: ctid groups like a unique Var (PG's
                    // SelfItemPointer case); attnum 0 is the sentinel
                    // var_ndistinct maps to the tuple count.
                    vars.push((0, 0));
                }
            }
            Expr::Literal(Literal::Bool(_)) | Expr::IsBool { .. } => {
                bool_factors += 1;
            }
            _ => {
                // Expression with statistics = a single Var;
                // otherwise decompose into component columns. A
                // boolean-typed expression contributes a factor of 2.
                if let Some((obj, att)) = match_expr_attnum(src, e) {
                    let v = (att, obj.oid);
                    if !vars.contains(&v) {
                        vars.push(v);
                    }
                } else if matches!(e, Expr::Cmp { .. }) {
                    bool_factors += 1;
                } else {
                    let mut cols: Vec<i32> = Vec::new();
                    pull_columns(e, t, &mut cols);
                    for a in cols {
                        let v = (a, 0);
                        if !vars.contains(&v) {
                            vars.push(v);
                        }
                    }
                }
            }
        }
    }
    let mut reldistinct = 1.0f64;
    let mut relvarcount = 0usize;
    let mut relmax = 0.0f64;
    let mut remaining: Vec<(i32, u32)> = vars.clone();
    // Multivariate ndistinct consumption (exact attribute-set items).
    loop {
        let mut consumed: Option<(Vec<(i32, u32)>, f64)> = None;
        for (obj, data) in &src.objs {
            if !obj.kinds.contains(&'d') || data.ndistinct_items.is_empty() {
                continue;
            }
            let keys = item_attnums(obj);
            let matched: Vec<(i32, u32)> = remaining
                .iter()
                .copied()
                .filter(|(a, o)| keys.contains(a) && (*a > 0 || *o == obj.oid))
                .collect();
            if matched.len() < 2 {
                continue;
            }
            let matched_atts: Vec<i32> = matched.iter().map(|(a, _)| *a).collect();
            if let Some((_, nd)) = data.ndistinct_items.iter().find(|(atts, _)| {
                atts.len() == matched_atts.len() && matched_atts.iter().all(|a| atts.contains(a))
            }) {
                consumed = Some((matched, *nd));
                break;
            }
        }
        let Some((matched, nd)) = consumed else { break };
        reldistinct *= nd;
        relvarcount += 1;
        relmax = relmax.max(nd);
        remaining.retain(|v| !matched.contains(v));
    }
    for (a, o) in &remaining {
        let nd = var_ndistinct(src, t, *a, *o, tuples);
        reldistinct *= nd;
        relvarcount += 1;
        relmax = relmax.max(nd);
    }
    // Per-relation clamp + restriction correction.
    if tuples > 0.0 {
        let mut clamp = tuples;
        if relvarcount > 1 {
            clamp = tuples * 0.1;
            if clamp < relmax {
                clamp = relmax.min(tuples);
            }
        }
        if reldistinct > clamp {
            reldistinct = clamp;
        }
        if input_rows < tuples && reldistinct > 0.0 {
            reldistinct *= 1.0 - ((tuples - input_rows) / tuples).powf(tuples / reldistinct);
        }
    }
    let mut total = reldistinct * 2f64.powi(bool_factors as i32);
    total = total.ceil();
    if input_rows > 0.0 {
        total = total.clamp(1.0, input_rows.max(1.0));
    } else {
        total = total.max(0.0);
    }
    total
}
