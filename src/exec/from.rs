// v1.78 mechanical split: moved verbatim from src/exec.rs (29284-29910).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// FROM
// ---------------------------------------------------------------------------

/// Build the joined working table: schema + one QRow per joined row.
/// `outer` is the enclosing query's scope chain (for correlated
/// subqueries in ON / derived tables never see it — no LATERAL).
pub(crate) fn build_from(
    q: &mut Q,
    outer: &[Scope],
    from: &[FromItem],
    where_: Option<&Expr>,
    need_prov: bool,
    // v0.8: index-order scan hint for the single-table fast path; always
    // None when `from` has more than one item.
    order_hint: Option<&OrderHint>,
    // v0.8: OFFSET+LIMIT row budget for early termination of the
    // index-order scan (only meaningful together with `order_hint`).
    early_limit: Option<usize>,
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    if from.is_empty() {
        // No FROM: exactly one empty row (SELECT 1, SELECT count(*), ...).
        return Ok((Vec::new(), vec![QRow::default()]));
    }
    if from.len() == 1 {
        // Fast path: a single FROM item needs no cross product — filter
        // its rows in place and return them untouched, without the
        // per-row cells/prov rebuild the general loop below performs.
        // (Identical to one loop iteration with an empty accumulator.)
        let (s2, r2) = build_source(
            q,
            outer,
            &from[0],
            where_,
            need_prov,
            order_hint,
            early_limit,
            &[],
        )?;
        let quals: HashSet<String> = s2.iter().map(|c| c.qual.clone()).collect();
        let no_quals = HashSet::new();
        let push = pushdown_for(&q.eng.db, where_, &quals, &no_quals, &s2, &[]);
        let r2 = filter_rows(q, outer, &s2, r2, &push)?;
        return Ok((s2, r2));
    }
    let mut acc_schema: Vec<QCol> = Vec::new();
    let mut acc_rows = vec![QRow::default()];
    for item in from {
        // v0.46: the parser folds comma-separated FROM items into CROSS
        // JOINs (`parse_from` in sql.rs), so `from` always holds exactly
        // one item. Implicit LATERAL for comma joins (`FROM t, f(t.x)`)
        // is handled in the Join arm of `build_source` below, where the
        // left row is in scope when the function is evaluated.
        let (s2, mut r2) = build_source(q, outer, item, where_, need_prov, None, None, &[])?;
        // Comma joins are inner joins: a WHERE conjunct that mentions only
        // this item's columns can filter its rows before the cross product.
        // (Qualified refs must name a qualifier from this item and from no
        // earlier item; unqualified refs must resolve here and nowhere
        // earlier — anything ambiguous stays for the post-join WHERE, which
        // raises the same error it always did.)
        let quals: HashSet<String> = s2.iter().map(|c| c.qual.clone()).collect();
        let acc_quals: HashSet<String> = acc_schema.iter().map(|c| c.qual.clone()).collect();
        let push = pushdown_for(&q.eng.db, where_, &quals, &acc_quals, &s2, &acc_schema);
        r2 = filter_rows(q, outer, &s2, r2, &push)?;
        let mut schema = Vec::with_capacity(acc_schema.len() + s2.len());
        schema.extend(acc_schema.iter().cloned());
        schema.extend(s2.iter().cloned());
        let mut rows = Vec::new();
        for a in &acc_rows {
            for b in &r2 {
                let mut cells = Vec::with_capacity(a.cells.len() + b.cells.len());
                cells.extend(a.cells.iter().cloned());
                cells.extend(b.cells.iter().cloned());
                let mut prov = Vec::with_capacity(a.prov.len() + b.prov.len());
                prov.extend(a.prov.iter().cloned());
                prov.extend(b.prov.iter().cloned());
                rows.push(QRow {
                    cells: Row::new(cells),
                    prov,
                });
            }
        }
        acc_schema = schema;
        acc_rows = rows;
    }
    Ok((acc_schema, acc_rows))
}

/// AND-flatten a WHERE predicate into its top-level conjuncts. Splitting
/// is sound for filtering: a row survives `WHERE (a AND b)` iff it
/// survives both `a` and `b` (three-valued AND is TRUE iff both are TRUE).
pub(crate) fn split_conjuncts(e: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    let mut stack = vec![e];
    while let Some(x) = stack.pop() {
        match x {
            Expr::And(a, b) => {
                stack.push(a);
                stack.push(b);
            }
            _ => out.push(x),
        }
    }
    out
}

/// Collect every column reference in `e`. Returns false when `e` contains
/// anything we refuse to push below a join — aggregates and subqueries
/// stay above, conservatively (their correlation is someone else's problem).
pub(crate) fn pushable_columns(e: &Expr, cols: &mut Vec<(Option<String>, String)>) -> bool {
    match e {
        Expr::Column { table, name } => {
            cols.push((table.clone(), name.clone()));
            true
        }
        Expr::Literal(_) | Expr::Param(_) => true,
        Expr::Arith { left, right, .. } => {
            pushable_columns(left, cols) && pushable_columns(right, cols)
        }
        Expr::Concat(a, b) => pushable_columns(a, cols) && pushable_columns(b, cols),
        Expr::Cast { expr, .. } => pushable_columns(expr, cols),
        Expr::Like { expr, pattern, .. } => {
            pushable_columns(expr, cols) && pushable_columns(pattern, cols)
        }
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => {
            pushable_columns(expr, cols) && pushable_columns(pattern, cols)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            pushable_columns(expr, cols)
                && pushable_columns(low, cols)
                && pushable_columns(high, cols)
        }
        Expr::Func { args, .. } => args.iter().all(|a| pushable_columns(a, cols)),
        Expr::Extract { from, .. } => pushable_columns(from, cols),
        Expr::Cmp { left, right, .. } => {
            pushable_columns(left, cols) && pushable_columns(right, cols)
        }
        Expr::And(a, b) | Expr::Or(a, b) => pushable_columns(a, cols) && pushable_columns(b, cols),
        Expr::Not(x) => pushable_columns(x, cols),
        Expr::BitNot(x) => pushable_columns(x, cols),
        Expr::Neg(x) => pushable_columns(x, cols),
        Expr::IsNull { expr: x, .. } | Expr::IsBool { expr: x, .. } => pushable_columns(x, cols),
        Expr::IsDistinctFrom { left, right, .. } => {
            pushable_columns(left, cols) && pushable_columns(right, cols)
        }
        _ => false,
    }
}

/// v0.98: true when evaluating `e` can produce different values on
/// repeated calls with the same row (Postgres VOLATILE). The sequence
/// functions (nextval/currval/setval/lastval) advance or read session
/// sequence state; random/setseed drive the PRNG; now()/clock_timestamp()
/// and friends call `now_micros()` per evaluation in this engine (PG
/// marks now() stable, but the implementation is per-call, so it is
/// behaviorally volatile here). A user-defined function is volatile when
/// any arity-matching overload declares VOLATILE (PG's default when the
/// volatility is not specified); an unresolvable name is conservatively
/// volatile. Subqueries are conservatively volatile (they were never
/// pushable anyway: `pushable_columns` returns false for them).
pub(crate) fn expr_is_volatile(db: &Database, e: &Expr) -> bool {
    // v1.36: the behaviorally-volatile builtin list lives at top level
    // (`volatile_builtin`) so the IMMUTABLE-fold body scan shares it.
    fn any_volatile(db: &Database, es: &[Expr]) -> bool {
        es.iter().any(|x| expr_is_volatile(db, x))
    }
    match e {
        Expr::Column { .. }
        | Expr::Literal(_)
        | Expr::Param(_)
        | Expr::WholeRow { .. }
        | Expr::ResolvedCol { .. } => false,
        Expr::Arith { left, right, .. }
        | Expr::Cmp { left, right, .. }
        | Expr::IsDistinctFrom { left, right, .. }
        | Expr::UserOp { left, right, .. } => {
            expr_is_volatile(db, left) || expr_is_volatile(db, right)
        }
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            expr_is_volatile(db, a) || expr_is_volatile(db, b)
        }
        Expr::Cast { expr, .. }
        | Expr::CastNamed { expr, .. }
        | Expr::FieldAccess { expr, .. }
        | Expr::Not(expr)
        | Expr::BitNot(expr)
        | Expr::Neg(expr)
        | Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Extract { from: expr, .. }
        | Expr::NamedArg { expr, .. } => expr_is_volatile(db, expr),
        Expr::Like { expr, pattern, .. } | Expr::Regex { expr, pattern, .. } => {
            expr_is_volatile(db, expr) || expr_is_volatile(db, pattern)
        }
        Expr::Between {
            expr, low, high, ..
        } => expr_is_volatile(db, expr) || expr_is_volatile(db, low) || expr_is_volatile(db, high),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_ref().is_some_and(|o| expr_is_volatile(db, o))
                || whens
                    .iter()
                    .any(|(w, t)| expr_is_volatile(db, w) || expr_is_volatile(db, t))
                || else_.as_ref().is_some_and(|x| expr_is_volatile(db, x))
        }
        Expr::Row(elems) => any_volatile(db, elems),
        Expr::ArrayCtor { elems, .. } => any_volatile(db, elems),
        Expr::Subscript { array, indices } => {
            expr_is_volatile(db, array) || any_volatile(db, indices)
        }
        Expr::Slice { array, bounds } => {
            expr_is_volatile(db, array)
                || bounds.iter().any(|(l, u)| {
                    l.as_ref().is_some_and(|x| expr_is_volatile(db, x))
                        || u.as_ref().is_some_and(|x| expr_is_volatile(db, x))
                })
        }
        Expr::Agg {
            arg,
            arg2,
            agg_order_by,
            filter,
            ..
        } => {
            arg.as_ref().is_some_and(|x| expr_is_volatile(db, x))
                || arg2.as_ref().is_some_and(|x| expr_is_volatile(db, x))
                || agg_order_by.iter().any(|o| expr_is_volatile(db, &o.expr))
                // v1.29: a volatile FILTER makes the aggregate volatile.
                || filter.as_ref().is_some_and(|x| expr_is_volatile(db, x))
        }
        // v1.30: ordered-set aggregate — volatile iff a direct arg,
        // a WITHIN GROUP sort key, or the FILTER is.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            direct_args.iter().any(|x| expr_is_volatile(db, x))
                || within_order_by
                    .iter()
                    .any(|o| expr_is_volatile(db, &o.expr))
                || filter.as_ref().is_some_and(|x| expr_is_volatile(db, x))
        }
        Expr::Func { name, args } => {
            if any_volatile(db, args) {
                return true;
            }
            if volatile_builtin(name) {
                return true;
            }
            // Mirror eval_func's dispatch: the special-cased builtins
            // above are handled; a user definition shadows any other
            // builtin when name+arity resolve.
            if let Some(overloads) = db.functions.get(name) {
                let mut arity_match = false;
                for fdef in overloads {
                    if fdef.arg_types.len() == args.len() {
                        arity_match = true;
                        if fdef.volatility == crate::sql::FuncVolatility::Volatile {
                            return true;
                        }
                    }
                }
                // No arity-matching overload: the call cannot resolve to
                // a calmer function; PG's default volatility is VOLATILE.
                return !arity_match;
            }
            false
        }
        // Subqueries and window calls are conservatively volatile.
        // (Subquery conjuncts were never pushable: `pushable_columns`
        // returns false for them, so this changes nothing there.)
        Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::InSub { .. }
        | Expr::Quantified { .. }
        | Expr::Exists { .. }
        | Expr::Window { .. } => true,
    }
}

/// The WHERE conjuncts that mention only `own`'s columns and may therefore
/// filter `own`'s rows before the join. A qualified ref must name a
/// qualifier of `own` and of no table in `other_quals`; an unqualified ref
/// must resolve to a column of `own` and of no column of `other`. Anything
/// ambiguous is left for the post-join WHERE, which reports it exactly as
/// before — pushdown never changes which queries error.
/// v0.98: volatile conjuncts are never pushed. The pre-join filter plus
/// the post-join full-WHERE pass would evaluate them twice per row;
/// Postgres evaluates a volatile qual exactly once per row (the
/// `nextval('ts1')` conformance case: the final nextval must be 11, not
/// 21). Non-volatile pushdown is unchanged (pure optimization).
pub(crate) fn pushdown_for<'a>(
    db: &Database,
    where_: Option<&'a Expr>,
    own_quals: &HashSet<String>,
    other_quals: &HashSet<String>,
    own: &[QCol],
    other: &[QCol],
) -> Vec<&'a Expr> {
    let Some(w) = where_ else {
        return Vec::new();
    };
    split_conjuncts(w)
        .into_iter()
        .filter(|c| {
            if expr_is_volatile(db, c) {
                return false;
            }
            let mut cols = Vec::new();
            if !pushable_columns(c, &mut cols) {
                return false;
            }
            cols.iter().all(|(q, name)| match q {
                Some(qq) => own_quals.contains(qq) && !other_quals.contains(qq),
                None => {
                    own.iter().any(|c| c.name == *name) && !other.iter().any(|c| c.name == *name)
                }
            })
        })
        .collect()
}

/// Keep the rows for which every conjunct is TRUE (same check_bool the
/// post-join WHERE uses).
pub(crate) fn filter_rows(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: Vec<QRow>,
    conjuncts: &[&Expr],
) -> Result<Vec<QRow>, ExecError> {
    if conjuncts.is_empty() {
        return Ok(rows);
    }
    let mut out = Vec::new();
    for row in rows {
        let frame = Scope {
            schema,
            row: &row.cells,
            prov: None,
        };
        let scopes_storage: Vec<Scope>;
        let scopes: &[Scope] = if outer.is_empty() {
            // Common case: no correlated outer query, so the scope chain
            // is just this row — a 1-element slice needs no heap allocation.
            std::slice::from_ref(&frame)
        } else {
            let mut buf: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            buf.extend_from_slice(outer);
            buf.push(frame);
            scopes_storage = buf;
            &scopes_storage
        };
        let mut keep = true;
        for c in conjuncts {
            if !check_bool(eval_expr(q, scopes, c)?, "WHERE")? {
                keep = false;
                break;
            }
        }
        if keep {
            out.push(row);
        }
    }
    Ok(out)
}

/// Every `(qualifier, name)` column reference in an expression, at any
/// depth — including inside subqueries. Conservative: a ref that merely
/// looks ambiguous at the join level disables the fast path even if an
/// inner scope would shadow it; the slow path keeps exact old semantics.
pub(crate) fn collect_column_refs(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => collect_column_refs(expr, out),
        Expr::Column { table, name } => out.push((table.clone(), name.clone())),
        // v0.73: a whole-row ref depends on every column of the range.
        Expr::WholeRow { qual } => out.push((Some(qual.clone()), "*".to_string())),
        // Already resolved (unambiguous by construction): nothing to collect.
        Expr::ResolvedCol { .. } => {}
        Expr::Literal(_) | Expr::Param(_) => {}
        Expr::Arith { left, right, .. } => {
            collect_column_refs(left, out);
            collect_column_refs(right, out);
        }
        Expr::Concat(a, b) => {
            collect_column_refs(a, out);
            collect_column_refs(b, out);
        }
        // v0.79: array constructors/subscripts/slices depend on their
        // operands' columns.
        Expr::ArrayCtor { elems, .. } => {
            for e in elems {
                collect_column_refs(e, out);
            }
        }
        // v0.81: composite expressions depend on their operands' columns.
        Expr::Row(elems) => {
            for e in elems {
                collect_column_refs(e, out);
            }
        }
        Expr::CastNamed { expr, .. } | Expr::FieldAccess { expr, .. } => {
            collect_column_refs(expr, out);
        }
        Expr::Subscript { array, indices } => {
            collect_column_refs(array, out);
            for i in indices {
                collect_column_refs(i, out);
            }
        }
        Expr::Slice { array, bounds } => {
            collect_column_refs(array, out);
            for (l, u) in bounds {
                if let Some(l) = l {
                    collect_column_refs(l, out);
                }
                if let Some(u) = u {
                    collect_column_refs(u, out);
                }
            }
        }
        Expr::Cast { expr, .. } => collect_column_refs(expr, out),
        Expr::Like { expr, pattern, .. } => {
            collect_column_refs(expr, out);
            collect_column_refs(pattern, out);
        }
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => {
            collect_column_refs(expr, out);
            collect_column_refs(pattern, out);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_column_refs(expr, out);
            collect_column_refs(low, out);
            collect_column_refs(high, out);
        }
        Expr::IsBool { expr: x, .. } => collect_column_refs(x, out),
        Expr::Func { args, .. } => {
            for a in args {
                collect_column_refs(a, out);
            }
        }
        Expr::Extract { from, .. } => collect_column_refs(from, out),
        Expr::Cmp { left, right, .. } => {
            collect_column_refs(left, out);
            collect_column_refs(right, out);
        }
        Expr::And(a, b) | Expr::Or(a, b) => {
            collect_column_refs(a, out);
            collect_column_refs(b, out);
        }
        Expr::Not(x) => collect_column_refs(x, out),
        Expr::BitNot(x) => collect_column_refs(x, out),
        Expr::Neg(x) => collect_column_refs(x, out),
        // v0.55: column references in every CASE arm.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                collect_column_refs(o, out);
            }
            for (k, r) in whens {
                collect_column_refs(k, out);
                collect_column_refs(r, out);
            }
            if let Some(e) = else_ {
                collect_column_refs(e, out);
            }
        }
        Expr::IsNull { expr: x, .. } => collect_column_refs(x, out),
        Expr::IsDistinctFrom { left, right, .. } => {
            collect_column_refs(left, out);
            collect_column_refs(right, out);
        }
        Expr::Agg {
            arg, arg2, filter, ..
        } => {
            if let Some(x) = arg {
                collect_column_refs(x, out);
            }
            if let Some(x) = arg2 {
                collect_column_refs(x, out);
            }
            // v1.29: FILTER reads input columns too.
            if let Some(x) = filter {
                collect_column_refs(x, out);
            }
        }
        // v1.30: ordered-set aggregate — refs in direct args, the
        // WITHIN GROUP sort keys, and the FILTER.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            for x in direct_args {
                collect_column_refs(x, out);
            }
            for o in within_order_by {
                collect_column_refs(&o.expr, out);
            }
            if let Some(x) = filter {
                collect_column_refs(x, out);
            }
        }
        Expr::ScalarSub(s) => collect_stmt_refs(s, out),
        Expr::ArraySubquery(s) => collect_stmt_refs(s, out),
        Expr::InSub { expr, sub, .. } => {
            collect_column_refs(expr, out);
            collect_stmt_refs(sub, out);
        }
        // v0.87: quantified comparison and user operator.
        Expr::Quantified { left, sub, .. } => {
            collect_column_refs(left, out);
            collect_stmt_refs(sub, out);
        }
        Expr::UserOp { left, right, .. } => {
            collect_column_refs(left, out);
            collect_column_refs(right, out);
        }
        Expr::Exists { sub, .. } => collect_stmt_refs(sub, out),
        // v0.10: collect column refs from window inputs.
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                collect_column_refs(a, out);
            }
            for p in partition_by {
                collect_column_refs(p, out);
            }
            for o in order_by {
                collect_column_refs(&o.expr, out);
            }
        }
    }
}

pub(crate) fn collect_stmt_refs(s: &SelectStmt, out: &mut Vec<(Option<String>, String)>) {
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_column_refs(expr, out);
        }
    }
    for f in &s.from {
        collect_from_refs(f, out);
    }
    if let Some(w) = &s.where_ {
        collect_column_refs(w, out);
    }
    for g in s.group_by.iter().flatten() {
        collect_column_refs(g, out);
    }
    if let Some(h) = &s.having {
        collect_column_refs(h, out);
    }
    for o in &s.order_by {
        collect_column_refs(&o.expr, out);
    }
}

pub(crate) fn collect_from_refs(f: &FromItem, out: &mut Vec<(Option<String>, String)>) {
    match f {
        FromItem::Join {
            left, right, on, ..
        } => {
            collect_from_refs(left, out);
            collect_from_refs(right, out);
            if let Some(p) = on {
                collect_column_refs(p, out);
            }
        }
        FromItem::Derived { sub, .. } => collect_stmt_refs(sub, out),
        FromItem::Values { rows, .. } => {
            for row in rows {
                for e in row {
                    collect_column_refs(e, out);
                }
            }
        }
        FromItem::Table { .. } => {}
        // v0.32: table-function args are uncorrelated expressions.
        FromItem::Function { args, .. } => {
            for a in args {
                collect_column_refs(a, out);
            }
        }
    }
}

/// True when a JOIN's ON predicate can be evaluated with two separate
/// frames (left row / right row) instead of a combined row buffer: no
/// column reference is ambiguous across the two sides. When this holds,
/// the old single-frame code could never have raised 42702 on this
/// predicate, so the two-frame evaluation is exactly equivalent — and it
/// allocates nothing per row pair in the common case. Anything doubtful
/// takes the slow path (the old combined-frame code, unchanged).
pub(crate) fn join_fast_path(on: &Expr, lschema: &[QCol], rschema: &[QCol]) -> bool {
    let mut refs = Vec::new();
    collect_column_refs(on, &mut refs);
    refs.iter().all(|(q, name)| {
        let count = |s: &[QCol]| {
            s.iter()
                .filter(|c| c.name == *name && q.as_deref().map_or(true, |qq| c.qual == qq))
                .count()
        };
        count(lschema) + count(rschema) < 2
    })
}
