// v1.78 mechanical split: moved verbatim from src/exec.rs (27570-29283).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

/// v0.10: validate every window function at this query level: arity,
/// no nested windows, no window inside an aggregate argument, and
/// frame discipline.
pub(crate) fn validate_windows(stmt: &SelectStmt) -> Result<(), ExecError> {
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            validate_window_expr(expr, false)?;
        }
    }
    for o in &stmt.order_by {
        validate_window_expr(&o.expr, false)?;
    }
    // v0.52: DISTINCT ON expressions may contain windows.
    for e in &stmt.distinct_on {
        validate_window_expr(e, false)?;
    }
    Ok(())
}

/// v0.10: `in_window` tracks whether we are inside a window's input
/// expressions (nested windows are forbidden); `in_agg` tracks
/// aggregate arguments (windows are forbidden there).
pub(crate) fn validate_window_expr(e: &Expr, in_agg: bool) -> Result<(), ExecError> {
    match e {
        Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            ..
        } => {
            if in_agg {
                return Err(exec_err(
                    "42803",
                    "window functions are not allowed in aggregate arguments",
                ));
            }
            check_window_arity(func, args.len())?;
            if *distinct {
                // v0.92: PG19 parse_func.c message, verbatim.
                return Err(exec_err(
                    "0A000",
                    "DISTINCT is not implemented for window functions",
                ));
            }
            for a in args {
                if contains_window(a) {
                    return Err(exec_err("42803", "window functions cannot be nested"));
                }
                validate_window_expr(a, false)?;
            }
            for p in partition_by {
                if contains_window(p) {
                    return Err(exec_err("42803", "window functions cannot be nested"));
                }
                validate_window_expr(p, false)?;
            }
            for o in order_by {
                if contains_window(&o.expr) {
                    return Err(exec_err("42803", "window functions cannot be nested"));
                }
                validate_window_expr(&o.expr, false)?;
            }
            check_window_frame(frame, order_by.len())?;
            // Frame bounds must be constant.
            Ok(())
        }
        Expr::Agg {
            arg,
            arg2,
            agg_order_by,
            filter,
            ..
        } => {
            if let Some(a) = arg {
                validate_window_expr(a, true)?;
            }
            if let Some(a) = arg2 {
                validate_window_expr(a, true)?;
            }
            // v0.92: the in-aggregate ORDER BY expressions are
            // per-row inputs like the arguments.
            for o in agg_order_by {
                validate_window_expr(&o.expr, true)?;
            }
            // v1.29: PG19 parse_agg.c / parse_func.c reject aggregates,
            // window functions, and set-returning functions directly
            // inside FILTER (the kind name in all three messages is
            // "FILTER"). Subqueries are their own level: `contains_agg`
            // / `contains_window` do not descend into them.
            if let Some(f) = filter {
                // v1.29: PG19 parse_agg.c likewise rejects GROUPING in
                // FILTER (kind name "FILTER"); checked before the
                // aggregate check because `contains_agg` deliberately
                // treats grouping() as an aggregate (v0.80).
                if contains_grouping(f) {
                    return Err(exec_err(
                        "42803",
                        "grouping operations are not allowed in FILTER",
                    ));
                }
                if contains_agg(f) {
                    return Err(exec_err(
                        "42803",
                        "aggregate functions are not allowed in FILTER",
                    ));
                }
                if contains_window(f) {
                    return Err(exec_err(
                        "42803",
                        "window functions are not allowed in FILTER",
                    ));
                }
                let mut srf = false;
                expr_walk(f, &mut |x| {
                    if let Expr::Func { name, .. } = x {
                        if is_builtin_srf(name) {
                            srf = true;
                        }
                    }
                });
                if srf {
                    return Err(exec_err(
                        "0A000",
                        "set-returning functions are not allowed in FILTER",
                    ));
                }
                // No aggregates, windows, or SRFs can remain; validate
                // the filter's contents like any other per-row input.
                validate_window_expr(f, true)?;
            }
            Ok(())
        }
        // v1.30: ordered-set aggregate — direct args, WITHIN GROUP
        // sort keys, and FILTER are aggregate inputs: windows are
        // forbidden there (PG19 parse_agg.c), mirroring the Agg arm.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            for a in direct_args {
                validate_window_expr(a, true)?;
            }
            for o in within_order_by {
                validate_window_expr(&o.expr, true)?;
            }
            if let Some(f) = filter {
                if contains_grouping(f) {
                    return Err(exec_err(
                        "42803",
                        "grouping operations are not allowed in FILTER",
                    ));
                }
                if contains_agg(f) {
                    return Err(exec_err(
                        "42803",
                        "aggregate functions are not allowed in FILTER",
                    ));
                }
                if contains_window(f) {
                    return Err(exec_err(
                        "42803",
                        "window functions are not allowed in FILTER",
                    ));
                }
                let mut srf = false;
                expr_walk(f, &mut |x| {
                    if let Expr::Func { name, .. } = x {
                        if is_builtin_srf(name) {
                            srf = true;
                        }
                    }
                });
                if srf {
                    return Err(exec_err(
                        "0A000",
                        "set-returning functions are not allowed in FILTER",
                    ));
                }
                validate_window_expr(f, true)?;
            }
            Ok(())
        }
        Expr::Arith { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Concat(left, right) => {
            validate_window_expr(left, in_agg)?;
            validate_window_expr(right, in_agg)
        }
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => validate_window_expr(expr, in_agg),
        Expr::Cmp { left, right, .. } => {
            validate_window_expr(left, in_agg)?;
            validate_window_expr(right, in_agg)
        }
        // v0.81: composite expressions — validate sub-expressions.
        Expr::CastNamed { expr, .. } | Expr::FieldAccess { expr, .. } => {
            validate_window_expr(expr, in_agg)
        }
        Expr::Row(elems) => {
            for e in elems {
                validate_window_expr(e, in_agg)?;
            }
            Ok(())
        }
        Expr::Like { expr, pattern, .. } => {
            validate_window_expr(expr, in_agg)?;
            validate_window_expr(pattern, in_agg)
        }
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => {
            validate_window_expr(expr, in_agg)?;
            validate_window_expr(pattern, in_agg)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_window_expr(expr, in_agg)?;
            validate_window_expr(low, in_agg)?;
            validate_window_expr(high, in_agg)
        }
        Expr::Not(x)
        | Expr::BitNot(x)
        | Expr::Neg(x)
        | Expr::IsNull { expr: x, .. }
        | Expr::IsBool { expr: x, .. } => validate_window_expr(x, in_agg),
        // v0.79: array constructors/subscripts/slices propagate the
        // nesting rules into their operands.
        Expr::ArrayCtor { elems, .. } => {
            for e in elems {
                validate_window_expr(e, in_agg)?;
            }
            Ok(())
        }
        Expr::Subscript { array, indices } => {
            validate_window_expr(array, in_agg)?;
            for i in indices {
                validate_window_expr(i, in_agg)?;
            }
            Ok(())
        }
        Expr::Slice { array, bounds } => {
            validate_window_expr(array, in_agg)?;
            for (l, u) in bounds {
                if let Some(l) = l {
                    validate_window_expr(l, in_agg)?;
                }
                if let Some(u) = u {
                    validate_window_expr(u, in_agg)?;
                }
            }
            Ok(())
        }
        // v0.55: windows may appear in any CASE arm (a CASE is not a
        // window boundary); nesting rules propagate unchanged.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                validate_window_expr(o, in_agg)?;
            }
            for (k, r) in whens {
                validate_window_expr(k, in_agg)?;
                validate_window_expr(r, in_agg)?;
            }
            if let Some(e) = else_ {
                validate_window_expr(e, in_agg)?;
            }
            Ok(())
        }
        Expr::IsDistinctFrom { left, right, .. } => {
            validate_window_expr(left, in_agg)?;
            validate_window_expr(right, in_agg)
        }
        Expr::Cast { expr, .. } => validate_window_expr(expr, in_agg),
        Expr::Func { args, .. } => {
            for a in args {
                validate_window_expr(a, in_agg)?;
            }
            Ok(())
        }
        Expr::Extract { from, .. } => validate_window_expr(from, in_agg),
        Expr::InSub { expr, .. } => validate_window_expr(expr, in_agg),
        // v0.87: quantified comparison and user operator.
        Expr::Quantified { left, .. } => validate_window_expr(left, in_agg),
        Expr::UserOp { left, right, .. } => {
            validate_window_expr(left, in_agg)?;
            validate_window_expr(right, in_agg)
        }
        Expr::Column { .. }
        | Expr::ResolvedCol { .. }
        | Expr::WholeRow { .. }
        | Expr::Literal(_)
        | Expr::Param(_)
        | Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::Exists { .. } => Ok(()),
    }
}

/// v0.10: argument counts per window function (Postgres arities).
pub(crate) fn check_window_arity(func: &WindowFunc, n: usize) -> Result<(), ExecError> {
    let ok = match func {
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => n == 0,
        WindowFunc::Ntile => n == 1,
        WindowFunc::Lag | WindowFunc::Lead => n >= 1 && n <= 3,
        WindowFunc::FirstValue | WindowFunc::LastValue => n == 1,
        WindowFunc::NthValue => n == 2,
        WindowFunc::Agg(f) => check_agg_window_arity(f, n),
    };
    if !ok {
        return Err(exec_err(
            "42883",
            format!("wrong number of arguments to window function (got {})", n),
        ));
    }
    Ok(())
}

/// v0.10: aggregate arities when used as window functions.
pub(crate) fn check_agg_window_arity(f: &AggFunc, n: usize) -> bool {
    match f {
        AggFunc::Count => n <= 1,
        AggFunc::Sum
        | AggFunc::Avg
        | AggFunc::Min
        | AggFunc::Max
        | AggFunc::BoolAnd
        | AggFunc::VarianceSamp
        | AggFunc::VariancePop
        | AggFunc::StddevSamp
        | AggFunc::StddevPop
        | AggFunc::ArrayAgg => n == 1,
        AggFunc::StringAgg => false,
    }
}

/// v0.10: frame discipline (Postgres restrictions).
pub(crate) fn check_window_frame(frame: &WindowFrame, order_len: usize) -> Result<(), ExecError> {
    match frame {
        WindowFrame::Default => Ok(()),
        WindowFrame::Rows { .. } => Ok(()),
        WindowFrame::Range { start, end } => {
            if has_offset_bound(start) || has_offset_bound(end) {
                if order_len != 1 {
                    return Err(exec_err(
                        "0A000",
                        "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY expression",
                    ));
                }
            }
            Ok(())
        }
        // v1.31: PG19 parse_clause.c, verbatim — "Per spec, GROUPS mode
        // requires an ORDER BY clause" (42P20 windowing error).
        WindowFrame::Groups { .. } => {
            if order_len == 0 {
                return Err(exec_err("42P20", "GROUPS mode requires an ORDER BY clause"));
            }
            Ok(())
        }
    }
}

/// v0.10: true for `N PRECEDING` / `N FOLLOWING` bounds.
pub(crate) fn has_offset_bound(b: &FrameBound) -> bool {
    matches!(b, FrameBound::Preceding(_) | FrameBound::Following(_))
}

/// v0.10: collect the deduplicated window specifications used at this
/// query level (SELECT list and ORDER BY).
pub(crate) fn collect_windows(stmt: &SelectStmt) -> Vec<ExecWindow> {
    let mut out: Vec<ExecWindow> = Vec::new();
    let mut visit = |e: &Expr| {
        if let Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            filter,
            exclusion,
            wid: _,
        } = e
        {
            let w = ExecWindow {
                func: func.clone(),
                args: args.clone(),
                distinct: *distinct,
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: frame.clone(),
                // v1.29: FILTER participates in window identity.
                filter: filter.as_deref().cloned(),
                // v1.31: exclusion participates in window identity.
                exclusion: exclusion.clone(),
            };
            if !out.contains(&w) {
                out.push(w);
            }
        }
    };
    // Walk select items and ORDER BY terms (windows are validated to
    // live only there).
    fn walk(e: &Expr, visit: &mut impl FnMut(&Expr)) {
        match e {
            Expr::Window { .. } => visit(e),
            Expr::Arith { left, right, .. }
            | Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::Concat(left, right) => {
                walk(left, visit);
                walk(right, visit);
            }
            Expr::Cmp { left, right, .. } => {
                walk(left, visit);
                walk(right, visit);
            }
            Expr::Like { expr, pattern, .. } => {
                walk(expr, visit);
                walk(pattern, visit);
            }
            // v0.68: regex match, like LIKE.
            Expr::Regex { expr, pattern, .. } => {
                walk(expr, visit);
                walk(pattern, visit);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                walk(expr, visit);
                walk(low, visit);
                walk(high, visit);
            }
            Expr::Not(x)
            | Expr::BitNot(x)
            | Expr::Neg(x)
            | Expr::IsNull { expr: x, .. }
            | Expr::IsBool { expr: x, .. } => walk(x, visit),
            Expr::IsDistinctFrom { left, right, .. } => {
                walk(left, visit);
                walk(right, visit);
            }
            Expr::Cast { expr, .. } => walk(expr, visit),
            Expr::Func { args, .. } => {
                for a in args {
                    walk(a, visit);
                }
            }
            Expr::Agg { arg, arg2, .. } => {
                if let Some(a) = arg {
                    walk(a, visit);
                }
                if let Some(a) = arg2 {
                    walk(a, visit);
                }
            }
            Expr::Extract { from, .. } => walk(from, visit),
            Expr::InSub { expr, .. } => walk(expr, visit),
            _ => {}
        }
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk(expr, &mut visit);
        }
    }
    for o in &stmt.order_by {
        walk(&o.expr, &mut visit);
    }
    // v0.52: window functions are legal inside DISTINCT ON expressions.
    for e in &stmt.distinct_on {
        walk(e, &mut visit);
    }
    out
}

/// v0.10: stamp each `Expr::Window` with its deduplicated index.
pub(crate) fn assign_window_ids(stmt: &mut SelectStmt, windows: &[ExecWindow]) {
    fn stamp(e: &mut Expr, windows: &[ExecWindow]) {
        match e {
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
            } => {
                let w = ExecWindow {
                    func: func.clone(),
                    args: args.clone(),
                    distinct: *distinct,
                    partition_by: partition_by.clone(),
                    order_by: order_by.clone(),
                    frame: frame.clone(),
                    // v1.29: FILTER participates in window identity.
                    filter: filter.as_deref().cloned(),
                    // v1.31: exclusion participates in window identity.
                    exclusion: exclusion.clone(),
                };
                *wid = windows.iter().position(|x| x == &w).unwrap_or(0);
            }
            Expr::Arith { left, right, .. }
            | Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::Concat(left, right) => {
                stamp(left, windows);
                stamp(right, windows);
            }
            Expr::Cmp { left, right, .. } => {
                stamp(left, windows);
                stamp(right, windows);
            }
            Expr::Like { expr, pattern, .. } => {
                stamp(expr, windows);
                stamp(pattern, windows);
            }
            // v0.68: regex match, like LIKE.
            Expr::Regex { expr, pattern, .. } => {
                stamp(expr, windows);
                stamp(pattern, windows);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                stamp(expr, windows);
                stamp(low, windows);
                stamp(high, windows);
            }
            Expr::Not(x)
            | Expr::BitNot(x)
            | Expr::Neg(x)
            | Expr::IsNull { expr: x, .. }
            | Expr::IsBool { expr: x, .. } => stamp(x, windows),
            Expr::IsDistinctFrom { left, right, .. } => {
                stamp(left, windows);
                stamp(right, windows);
            }
            Expr::Cast { expr, .. } => stamp(expr, windows),
            Expr::Func { args, .. } => {
                for a in args {
                    stamp(a, windows);
                }
            }
            Expr::Agg { arg, arg2, .. } => {
                if let Some(a) = arg {
                    stamp(a, windows);
                }
                if let Some(a) = arg2 {
                    stamp(a, windows);
                }
            }
            Expr::Extract { from, .. } => stamp(from, windows),
            Expr::InSub { expr, .. } => stamp(expr, windows),
            _ => {}
        }
    }
    for item in &mut stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            stamp(expr, windows);
        }
    }
    for o in &mut stmt.order_by {
        stamp(&mut o.expr, windows);
    }
    // v0.52: stamp DISTINCT ON window expressions too.
    for e in &mut stmt.distinct_on {
        stamp(e, windows);
    }
}

/// v0.10: precomputed input values for one window: partition keys,
/// order keys, and argument values, one entry per input row.
pub(crate) struct WindowInput {
    pub(crate) part_keys: Vec<Vec<Value>>,
    pub(crate) order_keys: Vec<Vec<Value>>,
    pub(crate) arg_vals: Vec<Vec<Value>>,
    /// v1.29: per-row FILTER results for windowed aggregates (PG19
    /// nodeWindowAgg.c "Skip anything FILTERed out"); `true` for every
    /// row when the spec has no FILTER.
    pub(crate) filter_vals: Vec<bool>,
}

/// v0.10: compare two order-key vectors using the window's ORDER BY
/// terms (direction + null placement).
pub(crate) fn compare_window_keys(
    a: &[Value],
    b: &[Value],
    order_by: &[OrderTerm],
) -> Result<Ordering, ExecError> {
    for (i, o) in order_by.iter().enumerate() {
        let av = a.get(i).unwrap_or(&Value::Null);
        let bv = b.get(i).unwrap_or(&Value::Null);
        let ord = compare_values(av, bv, o.desc, o.nulls_first)?;
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// v0.10: peer test for ranking and RANGE frames: all order keys equal
/// (NULL = NULL for peer grouping, like Postgres).
pub(crate) fn window_keys_equal(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        let eq = match (x, y) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            _ => {
                let mut ka = Vec::new();
                let mut kb = Vec::new();
                value_key(x, &mut ka);
                value_key(y, &mut kb);
                ka == kb
            }
        };
        if !eq {
            return false;
        }
    }
    true
}

/// v0.10: resolve a ROWS frame bound to an inclusive row position.
pub(crate) fn rows_bound(b: &FrameBound, pos: usize, n: usize) -> usize {
    match b {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::Preceding(k) => pos.saturating_sub((*k).min(usize::MAX as u64) as usize),
        FrameBound::CurrentRow => pos,
        FrameBound::Following(k) => pos
            .saturating_add((*k).min(usize::MAX as u64) as usize)
            .min(n.saturating_sub(1)),
        FrameBound::UnboundedFollowing => n.saturating_sub(1),
    }
}

/// v1.31: resolve a GROUPS frame to an inclusive (start, end) position
/// range (PG19 nodeWindowAgg.c `update_frameheadpos` /
/// `update_frametailpos` GROUPS branches). `peer_id` numbers the
/// partition's peer groups 0, 1, 2, … from the partition start
/// (contiguous runs — the partition is ordered by the window ORDER
/// BY); bounds count groups, not rows. Empty frame is `(1, 0)`, like
/// the other modes.
///
/// Bound mapping (PG, verbatim semantics):
/// - start: UNBOUNDED PRECEDING → 0; CURRENT ROW → first row of the
///   current group; `k` PRECEDING → first row of group `curg − k`
///   (clamped at 0); `k` FOLLOWING → first row of group `curg + k`
///   (past the last group → empty frame).
/// - end: UNBOUNDED FOLLOWING → `n − 1`; CURRENT ROW → last row of the
///   current group; `k` PRECEDING → last row of group `curg − k`
///   (before group 0 → empty frame); `k` FOLLOWING → last row of group
///   `curg + k` (past the last group → `n − 1`).
pub(crate) fn groups_frame(
    peer_id: &[usize],
    pos: usize,
    start: &FrameBound,
    end: &FrameBound,
) -> (usize, usize) {
    let n = peer_id.len();
    debug_assert!(n > 0 && pos < n);
    let maxg = peer_id.iter().copied().max().unwrap_or(0);
    let curg = peer_id[pos];
    // First/last position of each group (groups are contiguous runs).
    let mut first = vec![usize::MAX; maxg + 1];
    let mut last = vec![0usize; maxg + 1];
    for (p, &g) in peer_id.iter().enumerate() {
        if first[g] == usize::MAX {
            first[g] = p;
        }
        last[g] = p;
    }
    let s = match start {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::CurrentRow => first[curg],
        FrameBound::Preceding(k) => {
            let target = curg as i128 - *k as i128;
            first[target.max(0) as usize]
        }
        FrameBound::Following(k) => {
            let target = curg as i128 + *k as i128;
            if target > maxg as i128 {
                return (1, 0);
            }
            first[target as usize]
        }
        FrameBound::UnboundedFollowing => return (1, 0),
    };
    let e = match end {
        FrameBound::UnboundedFollowing => n - 1,
        FrameBound::CurrentRow => last[curg],
        FrameBound::Preceding(k) => {
            let target = curg as i128 - *k as i128;
            if target < 0 {
                return (1, 0);
            }
            last[target as usize]
        }
        FrameBound::Following(k) => {
            let target = curg as i128 + *k as i128;
            last[target.min(maxg as i128) as usize]
        }
        FrameBound::UnboundedPreceding => return (1, 0),
    };
    if s > e {
        return (1, 0);
    }
    (s, e)
}

/// v1.31: PG19 `row_is_in_frame` exclusion half — true when the frame
/// row at partition position `p` is excluded from the current row `j`'s
/// frame by the window's `EXCLUDE` clause:
/// - `EXCLUDE CURRENT ROW`: `p == j`.
/// - `EXCLUDE GROUP`: every row of `j`'s peer group (with no ORDER BY
///   all rows are peers — `peer_id` is all zeros — so everything is
///   excluded).
/// - `EXCLUDE TIES`: `j`'s peers except `j` itself (with no ORDER BY,
///   everything but `j`).
/// Aggregates skip excluded rows' transitions; first/last/nth_value
/// navigate to the first/last/nth non-excluded row (PG19
/// `WindowGetFuncArgInFrame` HEAD/TAIL adjustments). lead/lag are
/// physical navigation (`WINDOW_SEEK_CURRENT`) — no exclusion, untouched.
pub(crate) fn frame_excluded(
    exclusion: &FrameExclusion,
    peer_id: &[usize],
    j: usize,
    p: usize,
) -> bool {
    match exclusion {
        FrameExclusion::NoOthers => false,
        FrameExclusion::CurrentRow => p == j,
        FrameExclusion::Group => peer_id[p] == peer_id[j],
        FrameExclusion::Ties => p != j && peer_id[p] == peer_id[j],
    }
}

/// v0.10: resolve a window frame to an inclusive (start, end) position
/// range within the ordered partition.
/// v1.31: `peer_id` is the partition's peer-group numbering (PG19
/// nodeWindowAgg.c `currentgroup`), needed for GROUPS mode and for
/// frame exclusion.
pub(crate) fn resolve_frame(
    spec: &ExecWindow,
    input: &WindowInput,
    idxs: &[usize],
    pos: usize,
    peer_id: &[usize],
) -> Result<(usize, usize), ExecError> {
    let n = idxs.len();
    if n == 0 {
        return Ok((0, 0));
    }
    let order_len = spec.order_by.len();
    // v1.31: GROUPS mode — bounds count peer groups, not rows (PG19
    // nodeWindowAgg.c `update_frameheadpos` / `update_frametailpos`
    // GROUPS branches). Frame bounds ignore exclusion (PG comment,
    // verbatim: "computed without regard for any window exclusion
    // clause"); exclusion is applied per-row by the callers.
    if let WindowFrame::Groups { start, end } = &spec.frame {
        return Ok(groups_frame(peer_id, pos, start, end));
    }
    // Effective frame (Postgres default rule).
    let (is_range, start, end): (bool, FrameBound, FrameBound) = match &spec.frame {
        WindowFrame::Default => {
            if order_len > 0 {
                (true, FrameBound::UnboundedPreceding, FrameBound::CurrentRow)
            } else {
                (
                    false,
                    FrameBound::UnboundedPreceding,
                    FrameBound::UnboundedFollowing,
                )
            }
        }
        WindowFrame::Rows { start, end } => (false, start.clone(), end.clone()),
        WindowFrame::Range { start, end } => (true, start.clone(), end.clone()),
        // v1.31: GROUPS returns early above; this arm is unreachable but
        // required for exhaustiveness.
        WindowFrame::Groups { .. } => unreachable!("GROUPS handled before frame dispatch"),
    };
    if !is_range {
        let s = rows_bound(&start, pos, n);
        let e = rows_bound(&end, pos, n);
        // v0.10: an inverted span is empty (not silently reversed).
        if s > e {
            return Ok((1, 0));
        }
        return Ok((s, e));
    }
    // RANGE mode without ORDER BY: no ordering values exist, so every
    // row is a peer of every other; resolve positionally like ROWS.
    let key_at = |p: usize| -> &[Value] { &input.order_keys[idxs[p]] };
    if order_len == 0 {
        let s = rows_bound(&start, pos, n);
        let e = rows_bound(&end, pos, n);
        if s > e {
            return Ok((1, 0));
        }
        return Ok((s, e));
    }
    // RANGE mode: the frame is all rows whose single order key lies in
    // the [lo, hi] value interval derived from the bounds (Postgres
    // value-based RANGE semantics; CURRENT ROW expands to peers via the
    // interval). NULL order keys never match.
    if order_len > 1
        && matches!(
            (&start, &end),
            (FrameBound::Preceding(_), _)
                | (FrameBound::Following(_), _)
                | (_, FrameBound::Preceding(_))
                | (_, FrameBound::Following(_))
        )
    {
        return Err(exec_err(
            "0A000",
            "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY expression",
        ));
    }
    let cur = key_at(pos).first().unwrap_or(&Value::Null).clone();
    // v0.10: RANGE without offsets uses peer semantics (works for any
    // orderable type, like Postgres); offsets require numeric.
    let has_offset = has_offset_bound(&start) || has_offset_bound(&end);
    if !has_offset {
        // Peer-based: expand CURRENT ROW to the peer group.
        // Use the ORDER BY direction for the comparison.
        let (desc, nulls_first) = spec
            .order_by
            .first()
            .map(|o| (o.desc, o.nulls_first))
            .unwrap_or((false, None));
        let is_peer = |a: &Value, b: &Value| -> bool {
            // NULLs are peers of each other (they sort together).
            match (a, b) {
                (Value::Null, Value::Null) => true,
                (Value::Null, _) | (_, Value::Null) => false,
                _ => matches!(
                    compare_values(a, b, desc, nulls_first),
                    Ok(std::cmp::Ordering::Equal)
                ),
            }
        };
        let bound_pos = |b: &FrameBound, is_start: bool| -> usize {
            match b {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::UnboundedFollowing => n - 1,
                FrameBound::CurrentRow => {
                    if is_start {
                        // First peer at or before pos.
                        let mut s = pos;
                        while s > 0 && is_peer(key_at(s - 1).first().unwrap_or(&Value::Null), &cur)
                        {
                            s -= 1;
                        }
                        s
                    } else {
                        // Last peer at or after pos.
                        let mut e = pos;
                        while e + 1 < n
                            && is_peer(key_at(e + 1).first().unwrap_or(&Value::Null), &cur)
                        {
                            e += 1;
                        }
                        e
                    }
                }
                // Unreachable: has_offset is false.
                FrameBound::Preceding(_) | FrameBound::Following(_) => pos,
            }
        };
        let s = bound_pos(&start, true);
        let e = bound_pos(&end, false);
        // An inverted span (e.g. start after end) is empty.
        if s > e {
            return Ok((1, 0));
        }
        return Ok((s, e));
    }
    // RANGE with offsets: numeric interval semantics.
    if matches!(cur, Value::Null) {
        return Err(exec_err(
            "0A000",
            "RANGE offset requires a numeric ORDER BY expression",
        ));
    }
    let to_f = |v: &Value| -> Result<f64, ExecError> {
        match v {
            Value::Int(i) => Ok(*i as f64),
            Value::BigInt(i) => Ok(*i as f64),
            Value::Float(f) => Ok(*f),
            Value::Numeric(n) => Ok(n.to_f64()),
            _ => Err(exec_err(
                "0A000",
                "RANGE offset requires a numeric ORDER BY expression",
            )),
        }
    };
    let cur_f = if matches!(cur, Value::Null) {
        f64::NAN
    } else {
        to_f(&cur)?
    };
    let bound_val = |b: &FrameBound| -> Result<f64, ExecError> {
        match b {
            FrameBound::UnboundedPreceding => Ok(f64::NEG_INFINITY),
            FrameBound::UnboundedFollowing => Ok(f64::INFINITY),
            FrameBound::CurrentRow => Ok(cur_f),
            FrameBound::Preceding(k) => Ok(cur_f - (*k as f64)),
            FrameBound::Following(k) => Ok(cur_f + (*k as f64)),
        }
    };
    // Note: for the start bound, PRECEDING gives lo; for the end bound,
    // FOLLOWING gives hi. (A start FOLLOWING / end PRECEDING was
    // rejected by the parser's frame sanity check.)
    let lo = bound_val(&start)?;
    let hi = bound_val(&end)?;
    let mut s = n;
    let mut e = n;
    for p in 0..n {
        let v = key_at(p).first().unwrap_or(&Value::Null);
        if matches!(v, Value::Null) {
            continue;
        }
        let f = to_f(v)?;
        if f >= lo && f <= hi {
            if s == n {
                s = p;
            }
            e = p;
        }
    }
    if s == n {
        return Ok((1, 0)); // empty frame
    }
    Ok((s, e))
}

/// v0.64: for each spec, the index of the earliest spec in the slice
/// that shares its exact `(partition_by, order_by)` signature (its own
/// index if none does). `func`/`args`/`frame` play no part in either a
/// window's row-partitioning or its within-partition ordering, so every
/// window function built on the same `PARTITION BY .. ORDER BY ..` —
/// e.g. `row_number()`/`sum()`/`count(*)` all `OVER (PARTITION BY dept
/// ORDER BY id)` in one SELECT list, the common "running report" shape —
/// shares one signature and needs that grouping computed exactly once.
pub(crate) fn window_group_reps(specs: &[ExecWindow]) -> Vec<usize> {
    let mut reps = Vec::with_capacity(specs.len());
    for (i, s) in specs.iter().enumerate() {
        let rep = specs[..i]
            .iter()
            .position(|p| p.partition_by == s.partition_by && p.order_by == s.order_by)
            .unwrap_or(i);
        reps.push(rep);
    }
    reps
}

/// v0.10: gather window inputs (partition keys, order keys, argument
/// values) for the non-aggregated case: one entry per filtered row.
pub(crate) fn gather_window_inputs_plain(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: &[QRow],
    specs: &[ExecWindow],
) -> Result<Vec<WindowInput>, ExecError> {
    let reps = window_group_reps(specs);
    let mut out: Vec<WindowInput> = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        if reps[i] != i {
            // v0.64: this spec's (partition_by, order_by) is exactly
            // `specs[reps[i]]`'s — that earlier `WindowInput` already
            // holds the identical per-row values (same exprs, same
            // rows, same row order), so only this spec's own argument
            // values need evaluating here.
            let part_keys = out[reps[i]].part_keys.clone();
            let order_keys = out[reps[i]].order_keys.clone();
            let mut arg_vals = Vec::with_capacity(rows.len());
            // v1.29: per-row FILTER values for this spec's own window
            // function (the rep's filter values do not apply).
            let mut filter_vals = Vec::with_capacity(rows.len());
            for r in rows {
                let frame = Scope {
                    schema,
                    row: &r.cells,
                    prov: None,
                };
                let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                scopes.extend_from_slice(outer);
                scopes.push(frame);
                let mut av = Vec::with_capacity(spec.args.len());
                for a in &spec.args {
                    av.push(eval_expr(q, &scopes, a)?);
                }
                arg_vals.push(av);
                filter_vals.push(match &spec.filter {
                    Some(f) => check_bool(eval_expr(q, &scopes, f)?, "FILTER")?,
                    None => true,
                });
            }
            out.push(WindowInput {
                part_keys,
                order_keys,
                arg_vals,
                filter_vals,
            });
            continue;
        }
        // Original per-row evaluation order, unchanged: partition keys,
        // then order keys, then argument values, for every row in turn.
        let mut part_keys = Vec::with_capacity(rows.len());
        let mut order_keys = Vec::with_capacity(rows.len());
        let mut arg_vals = Vec::with_capacity(rows.len());
        // v1.29: per-row FILTER values (true for all rows when absent).
        let mut filter_vals = Vec::with_capacity(rows.len());
        for r in rows {
            let frame = Scope {
                schema,
                row: &r.cells,
                prov: None,
            };
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(frame);
            // Window inputs cannot contain window functions (validated)
            // or aggregates (that would make this an aggregate query).
            let mut pk = Vec::with_capacity(spec.partition_by.len());
            for p in &spec.partition_by {
                pk.push(eval_expr(q, &scopes, p)?);
            }
            let mut ok = Vec::with_capacity(spec.order_by.len());
            for o in &spec.order_by {
                ok.push(eval_expr(q, &scopes, &o.expr)?);
            }
            let mut av = Vec::with_capacity(spec.args.len());
            for a in &spec.args {
                av.push(eval_expr(q, &scopes, a)?);
            }
            part_keys.push(pk);
            order_keys.push(ok);
            arg_vals.push(av);
            filter_vals.push(match &spec.filter {
                Some(f) => check_bool(eval_expr(q, &scopes, f)?, "FILTER")?,
                None => true,
            });
        }
        out.push(WindowInput {
            part_keys,
            order_keys,
            arg_vals,
            filter_vals,
        });
    }
    Ok(out)
}

/// v0.10: gather window inputs for the aggregated case: one entry per
/// group, evaluated with the group's context (aggregates and GROUP BY
/// keys via `eval_grouped`).
pub(crate) fn gather_window_inputs_grouped(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: &[QRow],
    groups: &[(Vec<Value>, Vec<usize>)],
    // v0.10: indices of groups surviving HAVING (windows see only these,
    // like Postgres).
    surviving: &[usize],
    group_by: &[Expr],
    specs: &[ExecWindow],
) -> Result<Vec<WindowInput>, ExecError> {
    let reps = window_group_reps(specs);
    let mut out: Vec<WindowInput> = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        if reps[i] != i {
            // v0.64: see the identical dedup in gather_window_inputs_plain.
            let part_keys = out[reps[i]].part_keys.clone();
            let order_keys = out[reps[i]].order_keys.clone();
            let mut arg_vals = Vec::with_capacity(surviving.len());
            // v1.29: per-group FILTER values for this spec's own window
            // function.
            let mut filter_vals = Vec::with_capacity(surviving.len());
            for &gi in surviving {
                let (key_vals, idxs) = &groups[gi];
                let first: &[Value] = match idxs.first() {
                    Some(&i) => &rows[i].cells,
                    None => &[],
                };
                let gscope = Scope {
                    schema,
                    row: first,
                    prov: None,
                };
                let mut av = Vec::with_capacity(spec.args.len());
                for a in &spec.args {
                    av.push(eval_grouped(
                        q, outer, gscope, schema, rows, idxs, key_vals, group_by, a,
                    )?);
                }
                arg_vals.push(av);
                filter_vals.push(match &spec.filter {
                    Some(f) => check_bool(
                        eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, f)?,
                        "FILTER",
                    )?,
                    None => true,
                });
            }
            out.push(WindowInput {
                part_keys,
                order_keys,
                arg_vals,
                filter_vals,
            });
            continue;
        }
        let mut part_keys = Vec::with_capacity(surviving.len());
        let mut order_keys = Vec::with_capacity(surviving.len());
        let mut arg_vals = Vec::with_capacity(surviving.len());
        // v1.29: per-group FILTER values (true for all groups when
        // absent).
        let mut filter_vals = Vec::with_capacity(surviving.len());
        for &gi in surviving {
            let (key_vals, idxs) = &groups[gi];
            let first: &[Value] = match idxs.first() {
                Some(&i) => &rows[i].cells,
                None => &[],
            };
            let gscope = Scope {
                schema,
                row: first,
                prov: None,
            };
            let mut pk = Vec::with_capacity(spec.partition_by.len());
            for p in &spec.partition_by {
                pk.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, p,
                )?);
            }
            let mut ok = Vec::with_capacity(spec.order_by.len());
            for o in &spec.order_by {
                ok.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, &o.expr,
                )?);
            }
            let mut av = Vec::with_capacity(spec.args.len());
            for a in &spec.args {
                av.push(eval_grouped(
                    q, outer, gscope, schema, rows, idxs, key_vals, group_by, a,
                )?);
            }
            part_keys.push(pk);
            order_keys.push(ok);
            arg_vals.push(av);
            filter_vals.push(match &spec.filter {
                Some(f) => check_bool(
                    eval_grouped(q, outer, gscope, schema, rows, idxs, key_vals, group_by, f)?,
                    "FILTER",
                )?,
                None => true,
            });
        }
        out.push(WindowInput {
            part_keys,
            order_keys,
            arg_vals,
            filter_vals,
        });
    }
    Ok(out)
}

/// v0.10: compute every window's value vector and install the query's
/// window context.
pub(crate) fn install_windows(
    q: &mut Q,
    specs: &[ExecWindow],
    inputs: &[WindowInput],
) -> Result<(), ExecError> {
    let reps = window_group_reps(specs);
    // v0.64: the partition/order grouping (`window_partitions`) is a
    // pure function of `input.part_keys`/`input.order_keys` and
    // `spec.order_by` — identical for every spec sharing a rep, so it's
    // computed once per rep and reused, cached by rep index.
    let mut partitions_cache: Vec<Option<Vec<Vec<usize>>>> = vec![None; specs.len()];
    let mut values = Vec::with_capacity(specs.len());
    for (i, (spec, input)) in specs.iter().zip(inputs.iter()).enumerate() {
        let rep = reps[i];
        if partitions_cache[rep].is_none() {
            partitions_cache[rep] = Some(window_partitions(&specs[rep], &inputs[rep])?);
        }
        let partitions = partitions_cache[rep].as_ref().unwrap();
        values.push(compute_window_values(spec, input, partitions)?);
    }
    q.wctx = Some(WindowCtx { values, row: 0 });
    Ok(())
}

/// v0.64: partition `input`'s rows by `spec.partition_by` and, within
/// each partition, order them by `spec.order_by` (stable: ties keep
/// input order). Returns each partition's row indices, in that order.
///
/// A pure function of `input.part_keys`/`input.order_keys` and
/// `spec.order_by` alone — `func`/`args`/`frame` never enter into it —
/// so `install_windows` computes it once per distinct `(partition_by,
/// order_by)` signature and shares the result across every window spec
/// built on that signature, instead of rebuilding the same `HashMap`
/// grouping and re-running the same partition sort once per spec.
pub(crate) fn window_partitions(
    spec: &ExecWindow,
    input: &WindowInput,
) -> Result<Vec<Vec<usize>>, ExecError> {
    let nrows = input.part_keys.len();
    if nrows == 0 {
        return Ok(Vec::new());
    }
    // Partition rows by partition-key.
    let mut parts: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    let mut part_order: Vec<Vec<u8>> = Vec::new();
    for i in 0..nrows {
        let mut k = Vec::new();
        for v in &input.part_keys[i] {
            value_key(v, &mut k);
        }
        if !parts.contains_key(&k) {
            part_order.push(k.clone());
        }
        parts.entry(k).or_default().push(i);
    }
    let mut partitions = Vec::with_capacity(part_order.len());
    for pk in &part_order {
        let mut idxs = parts[pk].clone();
        // Order within the partition (stable: ties keep input order).
        if !spec.order_by.is_empty() {
            let mut err: Option<ExecError> = None;
            idxs.sort_by(|&a, &b| {
                if err.is_some() {
                    return Ordering::Equal;
                }
                match compare_window_keys(
                    &input.order_keys[a],
                    &input.order_keys[b],
                    &spec.order_by,
                ) {
                    Ok(o) => o,
                    Err(e) => {
                        err = Some(e);
                        Ordering::Equal
                    }
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
        partitions.push(idxs);
    }
    Ok(partitions)
}

/// v0.10: compute one window's value for every input row, given its
/// (possibly shared — see `window_partitions`) partitioning.
pub(crate) fn compute_window_values(
    spec: &ExecWindow,
    input: &WindowInput,
    partitions: &[Vec<usize>],
) -> Result<Vec<Value>, ExecError> {
    let nrows = input.arg_vals.len();
    let mut result = vec![Value::Null; nrows];
    for idxs in partitions {
        let vals = compute_partition(spec, input, idxs)?;
        for (j, &row_idx) in idxs.iter().enumerate() {
            result[row_idx] = vals[j].clone();
        }
    }
    Ok(result)
}

/// v0.63: true when `resolve_frame` is guaranteed to return `s == 0`
/// for *every* row of this partition (so the frame only ever grows as
/// the row position increases, never shrinks — what makes an
/// incremental, append-only accumulator safe in `compute_partition`).
///
/// `WindowFrame::Default` always qualifies: its effective start is
/// `UNBOUNDED PRECEDING` whether or not there's an ORDER BY, and its
/// bounds never carry a numeric offset. For an explicit `ROWS` frame,
/// `rows_bound(UnboundedPreceding, ..)` is `0` regardless of the end
/// bound (it's a pure position computation). For an explicit `RANGE`
/// frame, the same holds *unless* there's an ORDER BY and the end bound
/// has a numeric offset (`resolve_frame`'s "RANGE with offsets" branch
/// finds `s` by scanning for the first row whose order key clears the
/// offset interval, which can be > 0, or even skip past a leading NULL
/// order key — not a guaranteed `0`).
pub(crate) fn frame_start_is_unbounded_preceding(spec: &ExecWindow) -> bool {
    match &spec.frame {
        WindowFrame::Default => true,
        WindowFrame::Rows { start, .. } => matches!(start, FrameBound::UnboundedPreceding),
        // v1.31: GROUPS + UNBOUNDED PRECEDING also starts every frame at
        // row 0, so the cumulative fast paths stay valid.
        WindowFrame::Groups { start, .. } => matches!(start, FrameBound::UnboundedPreceding),
        WindowFrame::Range { start, end } => {
            matches!(start, FrameBound::UnboundedPreceding)
                && (spec.order_by.is_empty() || !has_offset_bound(end))
        }
    }
}

/// v0.63: the single `NumCat` every non-NULL value of the partition's
/// `sum()` argument belongs to, if and only if they're all exact
/// integers (`SmallInt`/`Int`/`BigInt` — never `Numeric`/`Float4`/
/// `Float`, whose incremental-sum arithmetic isn't attempted here).
/// A `sum_vals` fold over any sub-range of a partition this returns
/// `Some` for always picks the same result category (`sum_vals`'s `cat`
/// is a `max()` over exactly the exact-integer variants), so an `i128`
/// running accumulator reproduces it exactly, row by row.
pub(crate) fn partition_sum_int_cat(input: &WindowInput, idxs: &[usize]) -> Option<NumCat> {
    let mut cat: Option<NumCat> = None;
    for &i in idxs {
        let c = match input.arg_vals[i].first()? {
            Value::Null => continue,
            Value::SmallInt(_) => NumCat::Small,
            Value::Int(_) => NumCat::Int,
            Value::BigInt(_) => NumCat::Big,
            _ => return None,
        };
        cat = Some(cat.map_or(c, |prev| prev.max(c)));
    }
    cat
}

/// v0.10: compute a window function over one ordered partition.
/// `idxs` are input-row indices in partition order; returns one value
/// per position.
pub(crate) fn compute_partition(
    spec: &ExecWindow,
    input: &WindowInput,
    idxs: &[usize],
) -> Result<Vec<Value>, ExecError> {
    let n = idxs.len();
    let mut out = vec![Value::Null; n];
    // Peer groups (for rank/dense_rank and RANGE): consecutive rows
    // with equal order keys.
    let mut peer_id = vec![0usize; n];
    if !spec.order_by.is_empty() && n > 0 {
        let mut p = 0;
        for i in 1..n {
            if !window_keys_equal(&input.order_keys[idxs[i]], &input.order_keys[idxs[i - 1]]) {
                p += 1;
            }
            peer_id[i] = p;
        }
    }
    let arg = |pos: usize, k: usize| -> Value {
        input
            .arg_vals
            .get(idxs[pos])
            .and_then(|v| v.get(k))
            .cloned()
            .unwrap_or(Value::Null)
    };
    match &spec.func {
        WindowFunc::RowNumber => {
            for (j, v) in out.iter_mut().enumerate() {
                *v = Value::BigInt(j as i64 + 1);
            }
        }
        WindowFunc::Rank => {
            for j in 0..n {
                // 1 + rows before the first peer.
                let mut first = j;
                while first > 0 && peer_id[first - 1] == peer_id[j] {
                    first -= 1;
                }
                out[j] = Value::BigInt(first as i64 + 1);
            }
        }
        WindowFunc::DenseRank => {
            for j in 0..n {
                out[j] = Value::BigInt(peer_id[j] as i64 + 1);
            }
        }
        WindowFunc::Ntile => {
            let k_val = arg(0, 0);
            let k = match k_val {
                Value::Int(i) => i as usize,
                Value::BigInt(i) => i.max(0) as usize,
                Value::Null => return Err(exec_err("22004", "ntile argument must not be null")),
                _ => return Err(exec_err("22003", "ntile argument must be an integer")),
            };
            if k == 0 {
                return Err(exec_err("22003", "ntile argument must be positive"));
            }
            // As evenly as possible, larger buckets first (Postgres).
            let base = n / k;
            let rem = n % k;
            let mut pos = 0;
            for b in 0..k {
                let size = base + if b < rem { 1 } else { 0 };
                for _ in 0..size {
                    if pos < n {
                        out[pos] = Value::Int((b + 1) as i64);
                    }
                    pos += 1;
                }
            }
        }
        WindowFunc::Lag | WindowFunc::Lead => {
            let is_lag = matches!(spec.func, WindowFunc::Lag);
            for j in 0..n {
                let off_val = if input.arg_vals.get(idxs[j]).map(|v| v.len()).unwrap_or(0) > 1 {
                    arg(j, 1)
                } else {
                    Value::Int(1)
                };
                let off: i64 = match off_val {
                    Value::Int(i) => i as i64,
                    Value::BigInt(i) => i,
                    Value::Null => {
                        return Err(exec_err("22004", "lag/lead offset must not be null"));
                    }
                    _ => return Err(exec_err("22003", "lag/lead offset must be an integer")),
                };
                if off < 0 {
                    return Err(exec_err("22003", "lag/lead offset must not be negative"));
                }
                let target = if is_lag {
                    (j as i64) - off
                } else {
                    (j as i64) + off
                };
                out[j] = if target >= 0 && (target as usize) < n {
                    arg(target as usize, 0)
                } else if input.arg_vals.get(idxs[j]).map(|v| v.len()).unwrap_or(0) > 2 {
                    arg(j, 2)
                } else {
                    Value::Null
                };
            }
        }
        WindowFunc::FirstValue => {
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                // v1.31: PG19 `WindowGetFuncArgInFrame` HEAD adjustment —
                // the first *non-excluded* row of the frame.
                let mut v = Value::Null;
                if s <= e {
                    for p in s..=e {
                        if !frame_excluded(&spec.exclusion, &peer_id, j, p) {
                            v = arg(p, 0);
                            break;
                        }
                    }
                }
                out[j] = v;
            }
        }
        WindowFunc::LastValue => {
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                // v1.31: PG19 TAIL adjustment — the last non-excluded
                // row of the frame.
                let mut v = Value::Null;
                if s <= e {
                    for p in (s..=e).rev() {
                        if !frame_excluded(&spec.exclusion, &peer_id, j, p) {
                            v = arg(p, 0);
                            break;
                        }
                    }
                }
                out[j] = v;
            }
        }
        WindowFunc::NthValue => {
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                let nth = match arg(j, 1) {
                    Value::Int(i) => i as i64,
                    Value::BigInt(i) => i,
                    Value::Null => {
                        return Err(exec_err("22004", "nth_value argument must not be null"));
                    }
                    _ => return Err(exec_err("22003", "nth_value argument must be an integer")),
                };
                // v1.31: PG19 HEAD adjustment — the nth non-excluded row
                // of the frame.
                let mut v = Value::Null;
                if nth >= 1 && s <= e {
                    let mut seen = 0i64;
                    for p in s..=e {
                        if frame_excluded(&spec.exclusion, &peer_id, j, p) {
                            continue;
                        }
                        seen += 1;
                        if seen == nth {
                            v = arg(p, 0);
                            break;
                        }
                    }
                }
                out[j] = v;
            }
        }
        WindowFunc::Agg(f) => {
            // count(*) counts rows (NULLs included); other aggregates
            // skip NULL inputs, like their grouped counterparts.
            let count_star = *f == AggFunc::Count && spec.args.is_empty();
            // v1.29: FILTER skips input rows per-frame (PG19
            // nodeWindowAgg.c "Skip anything FILTERed out"), which
            // disables the cumulative fast paths below — they cannot
            // observe the per-row skip.
            let has_filter = spec.filter.is_some();
            let filt = |p: usize| -> bool {
                has_filter && !input.filter_vals.get(idxs[p]).copied().unwrap_or(true)
            };
            // v1.31: frame exclusion skips rows per-frame (PG19
            // nodeWindowAgg.c `row_is_in_frame`); composes with FILTER.
            // The excluded set varies with the current row j (EXCLUDE
            // CURRENT ROW drops a different row per j; EXCLUDE
            // GROUP/TIES drop j's own group), so exclusion disables the
            // cumulative fast paths below like FILTER does — row j+1
            // cannot reuse row j's accumulator.
            let has_exclusion = !matches!(spec.exclusion, FrameExclusion::NoOthers);
            let excl = |j: usize, p: usize| -> bool {
                has_exclusion && frame_excluded(&spec.exclusion, &peer_id, j, p)
            };
            // v0.63: `resolve_frame` proves `s` is always 0 whenever the
            // frame's start bound is UNBOUNDED PRECEDING (the implicit
            // default frame an ORDER BY window gets, and the most common
            // real one) — the frame then only ever grows as `j`
            // increases, so count(*)/count(x)/sum(exact int) can be
            // accumulated once instead of re-scanned from position 0 on
            // every row (was O(n) per row, O(n^2) per partition).
            let cumulative =
                frame_start_is_unbounded_preceding(spec) && !has_filter && !has_exclusion;
            let sum_int_cat = if cumulative && *f == AggFunc::Sum && spec.args.len() == 1 {
                partition_sum_int_cat(input, idxs)
            } else {
                None
            };
            if count_star {
                // Closed form: the frame's size needs no row data.
                // v1.29: with FILTER, count only the passing rows.
                // v1.31: with EXCLUDE, count only the non-excluded rows.
                for j in 0..n {
                    let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                    let c = if s <= e {
                        (s..=e).filter(|&p| !filt(p) && !excl(j, p)).count() as i64
                    } else {
                        0
                    };
                    out[j] = Value::BigInt(c);
                }
            } else if cumulative && *f == AggFunc::Count {
                let mut count: i64 = 0;
                let mut next = 0usize;
                for j in 0..n {
                    let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                    debug_assert_eq!(s, 0);
                    while next <= e {
                        if !matches!(arg(next, 0), Value::Null) && !excl(j, next) {
                            count += 1;
                        }
                        next += 1;
                    }
                    out[j] = Value::BigInt(count);
                }
            } else if let Some(cat) = sum_int_cat {
                let icat = if cat == NumCat::Small {
                    NumCat::Int
                } else {
                    cat
                };
                let mut acc: i128 = 0;
                let mut any = false;
                let mut next = 0usize;
                for j in 0..n {
                    let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                    debug_assert_eq!(s, 0);
                    while next <= e {
                        let v = arg(next, 0);
                        // v1.31: excluded rows never accumulate (PG19
                        // skips their transition function calls).
                        if !matches!(v, Value::Null) && !excl(j, next) {
                            any = true;
                            acc = acc
                                .checked_add(to_i128(&v))
                                .ok_or_else(|| exec_err("22003", "integer out of range"))?;
                        }
                        next += 1;
                    }
                    out[j] = if any {
                        fit_int_result(icat, acc)?
                    } else {
                        Value::Null
                    };
                }
            } else {
                // v0.92: static array_agg overload probe for the windowed
                // path (schema-free: array ctors and casts to array).
                let array_input = *f == AggFunc::ArrayAgg
                    && spec
                        .args
                        .first()
                        .is_some_and(|a| expr_is_statically_array(a, None));
                for j in 0..n {
                    let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                    let mut vals: Vec<Value> = Vec::new();
                    if s <= e {
                        for p in s..=e {
                            // v1.29: skip FILTERed-out rows (PG19
                            // nodeWindowAgg.c "Skip anything FILTERed
                            // out").
                            if filt(p) {
                                continue;
                            }
                            // v1.31: skip excluded rows (PG19
                            // `row_is_in_frame`).
                            if excl(j, p) {
                                continue;
                            }
                            let v = arg(p, 0);
                            // v0.92: array_agg keeps NULL inputs (PG19:
                            // "Collects all the input values, including
                            // nulls, into an array"); other aggregates
                            // skip nulls, like their grouped
                            // counterparts. (PG19 rejects ORDER BY
                            // inside windowed aggregates with 0A000, so
                            // no sort key applies here.)
                            if *f == AggFunc::ArrayAgg || !matches!(v, Value::Null) {
                                vals.push(v);
                            }
                        }
                    }
                    out[j] = eval_window_agg(*f, &vals, array_input)?;
                }
            }
        }
    }
    Ok(out)
}

/// v0.10: evaluate a windowed aggregate over frame values.
pub(crate) fn eval_window_agg(
    f: AggFunc,
    vals: &[Value],
    array_input: bool,
) -> Result<Value, ExecError> {
    match f {
        AggFunc::Count => {
            // count(x): non-null inputs; count(*): all rows. (NULLs are
            // filtered by the caller for count(x).)
            Ok(Value::BigInt(vals.len() as i64))
        }
        AggFunc::Sum => sum_vals(vals),
        AggFunc::Avg => avg_vals(vals),
        AggFunc::BoolAnd => bool_and_vals(vals),
        AggFunc::VarianceSamp => variance_vals(vals, "variance", true, true),
        AggFunc::VariancePop => variance_vals(vals, "var_pop", false, true),
        AggFunc::StddevSamp => variance_vals(vals, "stddev", true, false),
        AggFunc::StddevPop => variance_vals(vals, "stddev_pop", false, false),
        AggFunc::Min | AggFunc::Max => {
            let mut best: Option<&Value> = None;
            for v in vals {
                match best {
                    None => best = Some(v),
                    Some(b) => {
                        let ord = cmp_ordering(v, b, CmpOp::Lt)?.expect("non-null values compare");
                        let better = if f == AggFunc::Min {
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
        AggFunc::StringAgg => Err(exec_err(
            "0A000",
            "string_agg is not supported as a window function",
        )),
        // v0.92: PG19 supports array_agg as a window function; NULLs
        // are kept for array_agg (see array_agg_final) — the caller
        // only filters them for the other aggregates. The static arg
        // type picks the overload (schema-free probe: array ctors and
        // casts; the windowed path has no query schema handy).
        AggFunc::ArrayAgg => array_agg_final(vals.to_vec(), array_input),
    }
}

/// A query is aggregated when it has GROUP BY / HAVING, or an aggregate
/// anywhere at its own level (select list, HAVING, or ORDER BY — the
/// Postgres rule).
pub(crate) fn is_agg_query(stmt: &SelectStmt) -> bool {
    !stmt.group_by.is_empty()
        || stmt.having.is_some()
        || stmt.items.iter().any(|i| match i {
            SelectItem::Expr { expr, .. } => contains_agg(expr),
            _ => false,
        })
        || stmt.order_by.iter().any(|o| contains_agg(&o.expr))
        // v0.52: an aggregate inside DISTINCT ON (e.g. `DISTINCT ON
        // (count(*))`) makes the whole query aggregated — PG19 treats the
        // key as a group-level expression.
        || stmt.distinct_on.iter().any(|e| contains_agg(e))
}
