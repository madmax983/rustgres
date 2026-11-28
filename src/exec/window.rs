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
        // v2.47: WHERE CURRENT OF carries nothing to validate.
        Expr::CurrentOf(_) => Ok(()),
        Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            null_treatment,
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
            // v2.39: PG19 nodeWindowAgg.c
            // WinCheckAndInitializeNullTreatment — only lead/lag/
            // first_value/last_value/nth_value accept a null
            // treatment clause; any other window function (including
            // an explicit RESPECT NULLS) is 0A000 at execution.
            if *null_treatment != NullTreatment::Unspecified
                && !matches!(
                    func,
                    WindowFunc::Lag
                        | WindowFunc::Lead
                        | WindowFunc::FirstValue
                        | WindowFunc::LastValue
                        | WindowFunc::NthValue
                )
            {
                return Err(exec_err(
                    "0A000",
                    format!(
                        "function {} does not allow RESPECT/IGNORE NULLS",
                        window_func_name(func)
                    ),
                ));
            }
            // v2.39: PG19 parse_clause.c transformFrameOffset +
            // checkExprIsVarFree — frame offset expressions must not
            // contain variables of this query level (42P10), nor
            // aggregates or window functions.
            for (bound, is_start) in frame_bounds(frame) {
                let mode = frame_mode_name(frame);
                if expr_has_level_var(bound) {
                    return Err(exec_err(
                        "42P10",
                        format!("argument of {} must not contain variables", mode),
                    ));
                }
                if contains_agg(bound) {
                    return Err(exec_err(
                        "42803",
                        format!("aggregate functions are not allowed in {}", mode),
                    ));
                }
                if contains_window(bound) {
                    return Err(exec_err(
                        "42803",
                        format!("window functions are not allowed in {}", mode),
                    ));
                }
                let _ = is_start;
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
        | Expr::JsonOp { left, right, .. }
        | Expr::Concat(left, right) => {
            validate_window_expr(left, in_agg)?;
            validate_window_expr(right, in_agg)
        }
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => validate_window_expr(expr, in_agg),
        // v2.21: collation wrappers are transparent to validation.
        Expr::Collate { expr, .. } | Expr::CollationFor { expr } => {
            validate_window_expr(expr, in_agg)
        }
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
        // v1.86: AT TIME ZONE.
        Expr::AtTimeZone { expr, zone } => {
            validate_window_expr(expr, in_agg)?;
            if let Some(z) = zone {
                validate_window_expr(z, in_agg)?;
            }
            Ok(())
        }
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
        | Expr::JsonArraySubq { .. }
        | Expr::ArraySubquery(_)
        | Expr::Exists { .. } => Ok(()),
    }
}

/// v0.10: argument counts per window function (Postgres arities).
pub(crate) fn check_window_arity(func: &WindowFunc, n: usize) -> Result<(), ExecError> {
    let ok = match func {
        WindowFunc::RowNumber
        | WindowFunc::Rank
        | WindowFunc::DenseRank
        | WindowFunc::PercentRank
        | WindowFunc::CumeDist => n == 0,
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
        | AggFunc::BitAnd
        | AggFunc::BitOr
        | AggFunc::BitXor
        | AggFunc::VarianceSamp
        | AggFunc::VariancePop
        | AggFunc::StddevSamp
        | AggFunc::StddevPop
        | AggFunc::BoolOr
        | AggFunc::AnyValue
        | AggFunc::ArrayAgg => n == 1,
        // v2.41: regression aggregates take (Y, X) as window funcs too.
        AggFunc::RegrCount
        | AggFunc::RegrAvgX
        | AggFunc::RegrAvgY
        | AggFunc::RegrSxx
        | AggFunc::RegrSyy
        | AggFunc::RegrSxy
        | AggFunc::RegrSlope
        | AggFunc::RegrIntercept
        | AggFunc::RegrR2
        | AggFunc::Corr
        | AggFunc::CovarPop
        | AggFunc::CovarSamp => n == 2,
        AggFunc::StringAgg => false,
        // v1.84: json_agg/jsonb_agg are window-capable in PG19;
        // the object aggregates need a second value stream the
        // window machinery does not carry (StringAgg precedent).
        AggFunc::JsonAgg | AggFunc::JsonbAgg => n == 1,
        // v2.42: xmlagg is window-capable in PG19.
        AggFunc::XmlAgg => n == 1,
        AggFunc::JsonObjectAgg | AggFunc::JsonbObjectAgg => false,
        // v1.90: PG19 supports both SQL/JSON constructor aggregates
        // as window functions.
        AggFunc::JsonArrayAgg => n == 1,
        AggFunc::JsonObjectAggSql => n == 2,
        // v1.88: PG19 allows range_agg / range_intersect_agg as window
        // aggregates.
        AggFunc::RangeAgg | AggFunc::RangeIntersectAgg => n == 1,
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

/// v2.39: PG19's display name for a window function (error messages).
pub(crate) fn window_func_name(func: &WindowFunc) -> String {
    match func {
        WindowFunc::RowNumber => "row_number".to_string(),
        WindowFunc::Rank => "rank".to_string(),
        WindowFunc::DenseRank => "dense_rank".to_string(),
        WindowFunc::PercentRank => "percent_rank".to_string(),
        WindowFunc::CumeDist => "cume_dist".to_string(),
        WindowFunc::Ntile => "ntile".to_string(),
        WindowFunc::Lag => "lag".to_string(),
        WindowFunc::Lead => "lead".to_string(),
        WindowFunc::FirstValue => "first_value".to_string(),
        WindowFunc::LastValue => "last_value".to_string(),
        WindowFunc::NthValue => "nth_value".to_string(),
        WindowFunc::Agg(f) => f.name().to_string(),
    }
}

/// v2.39: the (bound, is_start) offset expressions of a frame.
pub(crate) fn frame_bounds(frame: &WindowFrame) -> Vec<(&Expr, bool)> {
    let mut out = Vec::new();
    let (start, end) = match frame {
        WindowFrame::Default => return out,
        WindowFrame::Rows { start, end }
        | WindowFrame::Range { start, end }
        | WindowFrame::Groups { start, end } => (start, end),
    };
    if let FrameBound::Preceding(e) | FrameBound::Following(e) = start {
        out.push((&**e, true));
    }
    if let FrameBound::Preceding(e) | FrameBound::Following(e) = end {
        out.push((&**e, false));
    }
    out
}

/// v2.39: PG19's construct name for a frame mode ("ROWS" / "RANGE" /
/// "GROUPS"), used by the frame-offset error messages.
pub(crate) fn frame_mode_name(frame: &WindowFrame) -> &'static str {
    match frame {
        WindowFrame::Range { .. } => "RANGE",
        WindowFrame::Groups { .. } => "GROUPS",
        _ => "ROWS",
    }
}

/// v2.39: does `e` reference a column of the current query level?
/// Subqueries are their own level and are not descended into (their
/// columns are legal inside a frame offset, PG19 checkExprIsVarFree
/// counts only vars of level 0).
pub(crate) fn expr_has_level_var(e: &Expr) -> bool {
    match e {
        Expr::Column { .. } | Expr::ResolvedCol { .. } | Expr::WholeRow { .. } => true,
        Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::Exists { .. }
        | Expr::JsonArraySubq { .. } => false,
        Expr::InSub { expr, .. } => expr_has_level_var(expr),
        Expr::Quantified { left, .. } => expr_has_level_var(left),
        _ => {
            let mut found = false;
            expr_walk(e, &mut |x| {
                if matches!(
                    x,
                    Expr::Column { .. } | Expr::ResolvedCol { .. } | Expr::WholeRow { .. }
                ) {
                    found = true;
                }
            });
            found
        }
    }
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
            null_treatment,
            wid: _,
            ..
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
                // v2.39: null treatment participates in window identity.
                null_treatment: *null_treatment,
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
                null_treatment,
                ..
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
                    // v2.39: null treatment participates in window identity.
                    null_treatment: *null_treatment,
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
    /// v2.39: the frame's start/end offset values, evaluated once per
    /// spec (PG19 nodeWindowAgg.c `EvalWindowFrameOffset` runs once,
    /// before the first row) with the first input row's scope.
    /// `None` when the bound carries no offset. For ROWS/GROUPS the
    /// value is the coerced count (`Value::BigInt`, non-negative);
    /// for RANGE it is the raw offset value (numeric or interval).
    pub(crate) off_start: Option<Value>,
    pub(crate) off_end: Option<Value>,
}

/// v2.39: coerce a ROWS/GROUPS frame offset value to a row count
/// (PG19 parse_clause.c coerces the offset expression to int8;
/// nodeWindowAgg.c rejects NULL with 22004 and negative counts with
/// 22013 ERRCODE_INVALID_PRECEDING_OR_FOLLOWING_SIZE).
pub(crate) fn frame_offset_count(v: &Value, is_start: bool) -> Result<i64, ExecError> {
    let c: i64 = match v {
        Value::SmallInt(i) => *i as i64,
        Value::Int(i) => *i,
        Value::BigInt(i) => *i,
        Value::Float(f) => f.round() as i64,
        Value::Float4(f) => (*f as f64).round() as i64,
        Value::Numeric(n) => n.to_f64().round() as i64,
        Value::Text(s) => s
            .trim()
            .parse::<i64>()
            .or_else(|_| s.trim().parse::<f64>().map(|f| f.round() as i64))
            .map_err(|_| {
                exec_err(
                    "42804",
                    "argument of ROWS must be integer, not a non-numeric value".to_string(),
                )
            })?,
        _ => {
            return Err(exec_err(
                "42804",
                format!(
                    "argument of ROWS must be integer, not type {}",
                    v.type_name().to_lowercase()
                ),
            ));
        }
    };
    if c < 0 {
        return Err(exec_err(
            "22013",
            if is_start {
                "frame starting offset must not be negative"
            } else {
                "frame ending offset must not be negative"
            },
        ));
    }
    Ok(c)
}

/// v2.39: evaluate a window spec's frame offset expressions once
/// (PG19 `EvalWindowFrameOffset`), with the given (first-row) scope.
/// Returns (start offset, end offset) as stored on `WindowInput`.
pub(crate) fn eval_frame_offsets(
    q: &mut Q,
    scopes: &[Scope],
    spec: &ExecWindow,
) -> Result<(Option<Value>, Option<Value>), ExecError> {
    let rows_mode = matches!(
        spec.frame,
        WindowFrame::Rows { .. } | WindowFrame::Groups { .. }
    );
    // v2.39 finisher: static type of a single ORDER BY key, for the
    // analysis-time RANGE operator check below (PG19 resolves the
    // bound through the key's own +/- operator at parse analysis,
    // so the error fires even on an empty input — the per-row path
    // in range_frame never runs there).
    let key_ty: Option<ColType> = if spec.order_by.len() == 1 {
        match &spec.order_by[0].expr {
            Expr::ResolvedCol { frame, idx } => scopes
                .get(*frame)
                .and_then(|s| s.schema.get(*idx))
                .map(|c| c.ty.clone()),
            Expr::Column { name, .. } => scopes.iter().rev().find_map(|s| {
                s.schema
                    .iter()
                    .find(|c| &c.name == name)
                    .map(|c| c.ty.clone())
            }),
            _ => None,
        }
    } else {
        None
    };
    let mut eval_one = |b: &FrameBound, is_start: bool| -> Result<Option<Value>, ExecError> {
        let e = match b {
            FrameBound::Preceding(e) | FrameBound::Following(e) => e,
            _ => return Ok(None),
        };
        // v2.39 finisher: PG19 42883 — a float8/float4-cast RANGE
        // offset on a numeric ORDER BY key has no numeric -/+ float8
        // operator. (A bare decimal literal is NOT this: PG coerces
        // the unknown-typed literal to numeric, and this engine's
        // Literal::Decimal follows that path.)
        if !rows_mode && matches!(key_ty, Some(ColType::Numeric(_))) {
            if let Expr::Cast {
                to: ColType::Float | ColType::Float4,
                ..
            } = &**e
            {
                let op = if matches!(b, FrameBound::Preceding(_)) {
                    "-"
                } else {
                    "+"
                };
                return Err(exec_err(
                    "42883",
                    format!("operator does not exist: numeric {op} double precision"),
                ));
            }
        }
        let v = eval_expr(q, scopes, e)?;
        if matches!(v, Value::Null) {
            return Err(exec_err(
                "22004",
                if is_start {
                    "frame starting offset must not be null"
                } else {
                    "frame ending offset must not be null"
                },
            ));
        }
        if rows_mode {
            Ok(Some(Value::BigInt(frame_offset_count(&v, is_start)?)))
        } else {
            // v2.39 finisher: PG19 nodeWindowAgg RANGE-offset
            // validation, applied once per partition with the offset
            // value: a negative offset is 42P20 "invalid preceding
            // or following size in window function" (interval
            // offsets included — '-70 min', '-1 year',
            // '-infinity'), and a NaN offset is "argument of RANGE
            // must not be NaN". The inherited code skipped both
            // checks, so PG-error statements silently succeeded.
            let neg_err = || {
                exec_err(
                    "42P20",
                    "invalid preceding or following size in window function",
                )
            };
            let nan_err = || exec_err("22023", "argument of RANGE must not be NaN");
            let check_num = |f: f64| -> Result<(), ExecError> {
                if f.is_nan() {
                    Err(nan_err())
                } else if f < 0.0 {
                    Err(neg_err())
                } else {
                    Ok(())
                }
            };
            match &v {
                Value::Interval(iv) => {
                    if crate::datetime::interval_cmp(iv, &crate::datetime::Interval::zero())
                        == std::cmp::Ordering::Less
                    {
                        return Err(neg_err());
                    }
                }
                Value::Float(f) => check_num(*f)?,
                Value::Float4(f) => check_num(*f as f64)?,
                Value::Numeric(n) => check_num(n.to_f64())?,
                Value::SmallInt(i) => {
                    if *i < 0 {
                        return Err(neg_err());
                    }
                }
                Value::Int(i) => {
                    if *i < 0 {
                        return Err(neg_err());
                    }
                }
                Value::BigInt(i) => {
                    if *i < 0 {
                        return Err(neg_err());
                    }
                }
                Value::Text(s) => {
                    if let Ok(f) = s.trim().parse::<f64>() {
                        check_num(f)?;
                    } else if let Ok(iv) = crate::datetime::parse_interval(s) {
                        if crate::datetime::interval_cmp(&iv, &crate::datetime::Interval::zero())
                            == std::cmp::Ordering::Less
                        {
                            return Err(neg_err());
                        }
                    }
                }
                _ => {}
            }
            Ok(Some(v))
        }
    };
    let (start, end) = match &spec.frame {
        WindowFrame::Default => (None, None),
        WindowFrame::Rows { start, end }
        | WindowFrame::Range { start, end }
        | WindowFrame::Groups { start, end } => (eval_one(start, true)?, eval_one(end, false)?),
    };
    Ok((start, end))
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
/// v2.39: `off` is the bound's evaluated offset count (from
/// `WindowInput::off_start` / `off_end`); i128 math keeps a huge
/// offset (e.g. 9223372036854775807 FOLLOWING) from overflowing.
pub(crate) fn rows_bound(b: &FrameBound, pos: usize, n: usize, off: i64) -> usize {
    match b {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::Preceding(_) => (pos as i128 - off as i128).max(0) as usize,
        FrameBound::CurrentRow => pos,
        FrameBound::Following(_) => (pos as i128 + off as i128)
            .min(n.saturating_sub(1) as i128)
            .max(0) as usize,
        FrameBound::UnboundedFollowing => n.saturating_sub(1),
    }
}

/// v2.39: the evaluated offset count stored for a bound (ROWS/GROUPS
/// offsets are coerced to `Value::BigInt` at gather time).
pub(crate) fn stored_count(v: &Option<Value>) -> i64 {
    match v {
        Some(Value::BigInt(i)) => *i,
        _ => 0,
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
    off_start: i64,
    off_end: i64,
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
        FrameBound::Preceding(_) => {
            let target = curg as i128 - off_start as i128;
            first[target.max(0) as usize]
        }
        FrameBound::Following(_) => {
            let target = curg as i128 + off_start as i128;
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
        FrameBound::Preceding(_) => {
            let target = curg as i128 - off_end as i128;
            if target < 0 {
                return (1, 0);
            }
            last[target as usize]
        }
        FrameBound::Following(_) => {
            let target = curg as i128 + off_end as i128;
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
        return Ok(groups_frame(
            peer_id,
            pos,
            start,
            end,
            stored_count(&input.off_start),
            stored_count(&input.off_end),
        ));
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
        let s = rows_bound(&start, pos, n, stored_count(&input.off_start));
        let e = rows_bound(&end, pos, n, stored_count(&input.off_end));
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
        let s = rows_bound(&start, pos, n, stored_count(&input.off_start));
        let e = rows_bound(&end, pos, n, stored_count(&input.off_end));
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
    // RANGE with offsets. v2.39: the offset is an evaluated value —
    // a numeric difference for numeric order keys, or an interval
    // for date/time/timestamp order keys (PG19 nodeWindowAgg.c uses
    // the order-by type's own +/- operators for the bound values).
    // An unknown-typed string literal offset ('1 day') is read as an
    // interval for temporal keys, as PG's parse-time coercion does.
    let off_start = input.off_start.clone();
    let off_end = input.off_end.clone();
    let as_interval = |v: &Option<Value>| -> Option<crate::datetime::Interval> {
        match v {
            Some(Value::Interval(iv)) => Some(iv.clone()),
            Some(Value::Text(s)) => crate::datetime::parse_interval(s).ok(),
            _ => None,
        }
    };
    let temporal = matches!(
        cur,
        Value::Date(_) | Value::Timestamp(_) | Value::Timestamptz(_) | Value::Time(_)
    );
    if temporal && (as_interval(&off_start).is_some() || as_interval(&off_end).is_some()) {
        return range_interval_frame(&cur, &start, &end, &off_start, &off_end, key_at, n);
    }
    if matches!(cur, Value::Null) {
        // v2.39: PG19 nodeWindowAgg.c — NULL order keys are peers of
        // each other, and an offset RANGE frame for a NULL-keyed row
        // is exactly its NULL peer group (the +/- bound values are
        // NULL, and only NULL keys satisfy the frame test).
        let mut s = pos;
        while s > 0 && matches!(key_at(s - 1).first(), Some(Value::Null)) {
            s -= 1;
        }
        let mut e = pos;
        while e + 1 < n && matches!(key_at(e + 1).first(), Some(Value::Null)) {
            e += 1;
        }
        return Ok((s, e));
    }
    let to_f = |v: &Value| -> Result<f64, ExecError> {
        match v {
            Value::SmallInt(i) => Ok(*i as f64),
            Value::Int(i) => Ok(*i as f64),
            Value::BigInt(i) => Ok(*i as f64),
            Value::Float(f) => Ok(*f),
            Value::Float4(f) => Ok(*f as f64),
            Value::Numeric(n) => Ok(n.to_f64()),
            Value::Text(s) => s.trim().parse::<f64>().map_err(|_| {
                exec_err(
                    "0A000",
                    "RANGE offset requires a numeric ORDER BY expression",
                )
            }),
            _ => Err(exec_err(
                "0A000",
                "RANGE offset requires a numeric ORDER BY expression",
            )),
        }
    };
    let cur_f = to_f(&cur)?;
    // v2.39 finisher: PG19 parse analysis resolves the RANGE bound
    // through the order key's own +/- operator; a float8 offset on a
    // numeric key has none (42883 "operator does not exist:
    // numeric - double precision"). The inherited f64 flattening
    // silently accepted it.
    for (b, off) in [(&start, &off_start), (&end, &off_end)] {
        // A bare decimal-literal offset coerces to numeric in PG —
        // its evaluated Value::Float is not the 42883 case (the
        // cast-typed form is rejected earlier, in eval_frame_offsets).
        let decimal_literal = matches!(
            b,
            FrameBound::Preceding(e) | FrameBound::Following(e)
                if matches!(e.as_ref(), Expr::Literal(Literal::Decimal(_)))
        );
        if !decimal_literal && matches!(cur, Value::Numeric(_)) {
            if let Some(Value::Float(_)) | Some(Value::Float4(_)) = off {
                let op = if matches!(b, FrameBound::Preceding(_)) {
                    "-"
                } else {
                    "+"
                };
                return Err(exec_err(
                    "42883",
                    format!("operator does not exist: numeric {op} double precision"),
                ));
            }
        }
    }
    let bound_val = |b: &FrameBound, off: &Option<Value>| -> Result<f64, ExecError> {
        match b {
            FrameBound::UnboundedPreceding => Ok(f64::NEG_INFINITY),
            FrameBound::UnboundedFollowing => Ok(f64::INFINITY),
            FrameBound::CurrentRow => Ok(cur_f),
            FrameBound::Preceding(_) => Ok(cur_f - to_f(off.as_ref().unwrap_or(&Value::Null))?),
            FrameBound::Following(_) => Ok(cur_f + to_f(off.as_ref().unwrap_or(&Value::Null))?),
        }
    };
    // Note: for the start bound, PRECEDING gives lo; for the end bound,
    // FOLLOWING gives hi. (A start FOLLOWING / end PRECEDING was
    // rejected by the parser's frame sanity check.)
    let lo = bound_val(&start, &off_start)?;
    let hi = bound_val(&end, &off_end)?;
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

/// v2.39: RANGE frame with interval offsets over a temporal order
/// key. All keys and bounds are projected to epoch microseconds
/// (dates at midnight, times within the day) and the frame is the
/// contiguous run of rows whose key lies in [lo, hi].
pub(crate) fn range_interval_frame<'a>(
    cur: &Value,
    start: &FrameBound,
    end: &FrameBound,
    off_start: &Option<Value>,
    off_end: &Option<Value>,
    key_at: impl Fn(usize) -> &'a [Value],
    n: usize,
) -> Result<(usize, usize), ExecError> {
    use crate::datetime as dt;
    fn key_micros(v: &Value) -> Option<i64> {
        match v {
            Value::Date(d) => (*d as i64).checked_mul(86_400_000_000),
            Value::Timestamp(m) | Value::Timestamptz(m) | Value::Time(m) => Some(*m),
            _ => None,
        }
    }
    fn off_interval(v: &Option<Value>) -> Option<dt::Interval> {
        match v {
            Some(Value::Interval(iv)) => Some(iv.clone()),
            Some(Value::Text(s)) => dt::parse_interval(s).ok(),
            _ => None,
        }
    }
    fn shift(cur: &Value, cur_m: i64, iv: &dt::Interval, sign: i32) -> Result<i64, ExecError> {
        let err = |e: String| exec_err("22008", e);
        match cur {
            Value::Timestamp(_) => dt::timestamp_add_interval(cur_m, iv, sign).map_err(err),
            Value::Timestamptz(_) => dt::timestamptz_add_interval(cur_m, iv, sign).map_err(err),
            // PG date +/- interval is midnight-based timestamp math.
            Value::Date(_) => dt::timestamp_add_interval(cur_m, iv, sign).map_err(err),
            Value::Time(_) => {
                // v2.39 finisher: saturating math — an 'infinity'
                // interval offset must saturate the bound (PG: the
                // frame then covers every row), never overflow-panic
                // (the inherited code multiplied days * 86.4e9 in
                // checked-arithmetic style and panicked the server
                // thread on interval 'infinity').
                let delta = iv
                    .micros
                    .saturating_add((iv.days as i64).saturating_mul(86_400_000_000))
                    .saturating_mul(sign as i64);
                Ok(cur_m.saturating_add(delta))
            }
            _ => Err(exec_err(
                "0A000",
                "RANGE offset requires a numeric ORDER BY expression",
            )),
        }
    }
    // A NULL current key matches nothing (PG: comparisons with the
    // NULL bound value are never true).
    let Some(cur_m) = key_micros(cur) else {
        return Ok((1, 0));
    };
    let bound_micros = |b: &FrameBound,
                        off: &Option<Value>,
                        is_start: bool|
     -> Result<Option<i64>, ExecError> {
        match b {
            FrameBound::UnboundedPreceding if is_start => Ok(None),
            FrameBound::UnboundedFollowing if !is_start => Ok(None),
            FrameBound::UnboundedPreceding | FrameBound::UnboundedFollowing => {
                Ok(Some(if is_start { i64::MAX } else { i64::MIN }))
            }
            FrameBound::CurrentRow => Ok(Some(cur_m)),
            FrameBound::Preceding(_) => {
                let iv = off_interval(off)
                    .ok_or_else(|| exec_err("0A000", "RANGE offset requires an interval value"))?;
                Ok(Some(shift(cur, cur_m, &iv, -1)?))
            }
            FrameBound::Following(_) => {
                let iv = off_interval(off)
                    .ok_or_else(|| exec_err("0A000", "RANGE offset requires an interval value"))?;
                Ok(Some(shift(cur, cur_m, &iv, 1)?))
            }
        }
    };
    let lo = bound_micros(start, off_start, true)?;
    let hi = bound_micros(end, off_end, false)?;
    let mut s = n;
    let mut e = n;
    for p in 0..n {
        let v = key_at(p).first().unwrap_or(&Value::Null);
        let Some(f) = key_micros(v) else {
            continue;
        };
        if lo.is_none_or(|l| f >= l) && hi.is_none_or(|h| f <= h) {
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
        // v2.39: frame offsets are evaluated once per spec (PG19
        // EvalWindowFrameOffset), with the first row's scope.
        let (off_start, off_end) = {
            let empty_cells = Row::new(Vec::new());
            let first = rows.first().map(|r| &r.cells).unwrap_or(&empty_cells);
            let frame0 = Scope {
                schema,
                row: first,
                prov: None,
            };
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(frame0);
            eval_frame_offsets(q, &scopes, spec)?
        };
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
                off_start,
                off_end,
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
            off_start,
            off_end,
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
        // v2.39: frame offsets evaluated once per spec, with the
        // first surviving group's first-row scope.
        let (off_start, off_end) = {
            let empty_cells = Row::new(Vec::new());
            let first: &[Value] = match surviving.first() {
                Some(&gi) => match groups[gi].1.first() {
                    Some(&ri) => &rows[ri].cells,
                    None => &empty_cells,
                },
                None => &empty_cells,
            };
            let frame0 = Scope {
                schema,
                row: first,
                prov: None,
            };
            let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            scopes.extend_from_slice(outer);
            scopes.push(frame0);
            eval_frame_offsets(q, &scopes, spec)?
        };
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
                off_start,
                off_end,
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
            off_start,
            off_end,
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
        // v2.39: PG19 window_percent_rank / window_cume_dist —
        // (rank - 1) / (partition rows - 1), and (rows preceding or
        // peer) / partition rows, both float8.
        WindowFunc::PercentRank => {
            for j in 0..n {
                out[j] = if n <= 1 {
                    Value::Float(0.0)
                } else {
                    // First position of j's peer group = rank - 1.
                    let mut first = j;
                    while first > 0 && peer_id[first - 1] == peer_id[j] {
                        first -= 1;
                    }
                    Value::Float(first as f64 / (n - 1) as f64)
                };
            }
        }
        WindowFunc::CumeDist => {
            for j in 0..n {
                // Last position of j's peer group, +1 = NP.
                let mut last = j;
                while last + 1 < n && peer_id[last + 1] == peer_id[j] {
                    last += 1;
                }
                out[j] = Value::Float((last + 1) as f64 / n as f64);
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
            // v2.39: IGNORE NULLS — the offset counts non-NULL values
            // (PG19 nodeWindowAgg.c WinGetFuncArgInPartition with
            // relpos counting only non-null rows).
            let ignore_nulls = spec.null_treatment == NullTreatment::Ignore;
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
                let default = if input.arg_vals.get(idxs[j]).map(|v| v.len()).unwrap_or(0) > 2 {
                    arg(j, 2)
                } else {
                    Value::Null
                };
                if ignore_nulls && off == 0 {
                    // PG19 nodeWindowAgg.c: null treatment is disabled
                    // for relpos == 0 — the current row's own value is
                    // returned, NULL or not.
                    out[j] = arg(j, 0);
                    continue;
                }
                if ignore_nulls {
                    // Walk strictly away from j in the navigation
                    // direction, counting only rows whose value is
                    // non-NULL; the off-th such row supplies the
                    // value (the current row's own nullness is
                    // irrelevant — PG counts preceding/following
                    // non-null rows).
                    let step: i64 = if is_lag { -1 } else { 1 };
                    let mut seen: i64 = 0;
                    let mut found: Option<Value> = None;
                    let mut p = j as i64 + step;
                    while p >= 0 && (p as usize) < n {
                        let v = arg(p as usize, 0);
                        if !matches!(v, Value::Null) {
                            seen += 1;
                            if seen == off {
                                found = Some(v);
                                break;
                            }
                        }
                        p += step;
                    }
                    out[j] = found.unwrap_or(default);
                    continue;
                }
                let target = if is_lag {
                    (j as i64) - off
                } else {
                    (j as i64) + off
                };
                out[j] = if target >= 0 && (target as usize) < n {
                    arg(target as usize, 0)
                } else {
                    default
                };
            }
        }
        WindowFunc::FirstValue => {
            // v2.39: IGNORE NULLS additionally skips NULL-valued rows.
            let ignore_nulls = spec.null_treatment == NullTreatment::Ignore;
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                // v1.31: PG19 `WindowGetFuncArgInFrame` HEAD adjustment —
                // the first *non-excluded* row of the frame.
                let mut v = Value::Null;
                if s <= e {
                    for p in s..=e {
                        if !frame_excluded(&spec.exclusion, &peer_id, j, p) {
                            let cand = arg(p, 0);
                            if ignore_nulls && matches!(cand, Value::Null) {
                                continue;
                            }
                            v = cand;
                            break;
                        }
                    }
                }
                out[j] = v;
            }
        }
        WindowFunc::LastValue => {
            let ignore_nulls = spec.null_treatment == NullTreatment::Ignore;
            for j in 0..n {
                let (s, e) = resolve_frame(spec, input, idxs, j, &peer_id)?;
                // v1.31: PG19 TAIL adjustment — the last non-excluded
                // row of the frame.
                let mut v = Value::Null;
                if s <= e {
                    for p in (s..=e).rev() {
                        if !frame_excluded(&spec.exclusion, &peer_id, j, p) {
                            let cand = arg(p, 0);
                            if ignore_nulls && matches!(cand, Value::Null) {
                                continue;
                            }
                            v = cand;
                            break;
                        }
                    }
                }
                out[j] = v;
            }
        }
        WindowFunc::NthValue => {
            let ignore_nulls = spec.null_treatment == NullTreatment::Ignore;
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
                // of the frame. v2.39: with IGNORE NULLS, only
                // non-NULL rows count toward n.
                let mut v = Value::Null;
                if nth >= 1 && s <= e {
                    let mut seen = 0i64;
                    for p in s..=e {
                        if frame_excluded(&spec.exclusion, &peer_id, j, p) {
                            continue;
                        }
                        let cand = arg(p, 0);
                        if ignore_nulls && matches!(cand, Value::Null) {
                            continue;
                        }
                        seen += 1;
                        if seen == nth {
                            v = cand;
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
                    // v1.90: windowed SQL/JSON constructor aggregates
                    // need their marker options and (for
                    // JSON_OBJECTAGG) both argument streams, which
                    // the single-stream path below cannot carry.
                    if matches!(f, AggFunc::JsonArrayAgg | AggFunc::JsonObjectAggSql) {
                        let is_obj = *f == AggFunc::JsonObjectAggSql;
                        let marker_expr = if is_obj { &spec.args[1] } else { &spec.args[0] };
                        let (_, o) = super::json::jsonagg_opt_unwrap(marker_expr);
                        let o = o.unwrap_or_default();
                        let kcat = super::json::ctor_cat_shallow_nodb(&spec.args[0]);
                        let vcat = if is_obj {
                            super::json::ctor_cat_shallow_nodb(&spec.args[1])
                        } else {
                            kcat
                        };
                        let mut nrows = 0usize;
                        let mut cargs: Vec<super::json::CtorArg> = Vec::new();
                        let mut cpairs: Vec<(super::json::CtorArg, super::json::CtorArg)> =
                            Vec::new();
                        if s <= e {
                            for p in s..=e {
                                if filt(p) || excl(j, p) {
                                    continue;
                                }
                                nrows += 1;
                                if is_obj {
                                    cpairs.push((
                                        super::json::CtorArg {
                                            v: arg(p, 0),
                                            cat: kcat,
                                            fmt: false,
                                            enc: 0,
                                        },
                                        super::json::CtorArg {
                                            v: arg(p, 1),
                                            cat: vcat,
                                            fmt: o.fmt,
                                            enc: o.enc,
                                        },
                                    ));
                                } else {
                                    cargs.push(super::json::CtorArg {
                                        v: arg(p, 0),
                                        cat: vcat,
                                        fmt: o.fmt,
                                        enc: o.enc,
                                    });
                                }
                            }
                        }
                        out[j] = if nrows == 0 {
                            Value::Null
                        } else if is_obj {
                            let (text, jb) = super::json::ctor_build_object_parts(
                                cpairs,
                                !o.null_on_null,
                                o.unique,
                                &o.spec,
                                true,
                            )?;
                            super::json::ctor_apply_returning_pure(text, &jb, &o.spec)?
                        } else {
                            let (text, jb) = super::json::ctor_build_array_parts(
                                cargs,
                                !o.null_on_null,
                                &o.spec,
                                true,
                            )?;
                            super::json::ctor_apply_returning_pure(text, &jb, &o.spec)?
                        };
                        continue;
                    }
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
                            if *f == AggFunc::ArrayAgg
                                || matches!(f, AggFunc::JsonAgg | AggFunc::JsonbAgg)
                                || !matches!(v, Value::Null)
                            {
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
        AggFunc::BoolOr => bool_or_vals(vals),
        AggFunc::AnyValue => Ok(vals.first().cloned().unwrap_or(Value::Null)),
        // v2.41: windowed regression aggregates need the paired (Y, X)
        // stream, which the window machinery does not carry; PG19
        // supports them, so this is a documented approximation gap
        // (the grouped path is exact).
        AggFunc::RegrCount
        | AggFunc::RegrAvgX
        | AggFunc::RegrAvgY
        | AggFunc::RegrSxx
        | AggFunc::RegrSyy
        | AggFunc::RegrSxy
        | AggFunc::RegrSlope
        | AggFunc::RegrIntercept
        | AggFunc::RegrR2
        | AggFunc::Corr
        | AggFunc::CovarPop
        | AggFunc::CovarSamp => Err(exec_err(
            "0A000",
            "regression aggregates are not supported as window functions",
        )),
        AggFunc::BitAnd | AggFunc::BitOr | AggFunc::BitXor => bit_agg_vals(f, vals),
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
        // v1.84: windowed json_agg/jsonb_agg render the frame values
        // exactly like the grouped finalize (NULLs kept by the
        // caller for these two, as for array_agg).
        AggFunc::JsonAgg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut out = String::from("[");
            for (i, v) in vals.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                    if matches!(v, Value::Array(_) | Value::Record(_)) {
                        out.push_str("\n ");
                    }
                }
                datum_to_json_text(v, &mut out);
            }
            out.push(']');
            Ok(Value::text(out))
        }
        AggFunc::JsonbAgg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let mut elems = Vec::with_capacity(vals.len());
            for v in vals {
                elems.push(datum_to_jsonb(v)?);
            }
            Ok(Value::jsonb(JsonbVal::Array(elems).render()))
        }
        // v2.42: xmlagg as a window aggregate (same final as agg.rs).
        AggFunc::XmlAgg => {
            let mut out = String::new();
            let mut any = false;
            for v in vals {
                let Some(t) = super::xml::arg_xml_text(v)? else {
                    continue;
                };
                any = true;
                let doc = super::xml::parse_xml(&t, false).map_err(|e| e.to_exec())?;
                for n in &doc.nodes {
                    super::xml::serialize_node(n, &mut out);
                }
            }
            if !any {
                return Ok(Value::Null);
            }
            Ok(Value::xml(out))
        }
        // v1.90: windowed SQL/JSON constructor aggregates are
        // intercepted in compute_partition (they need their marker
        // options and both argument streams); reaching here means a
        // caller without that interception — fail like the other
        // two-stream aggregates.
        AggFunc::JsonArrayAgg | AggFunc::JsonObjectAggSql => Err(exec_err(
            "0A000",
            "SQL/JSON constructor aggregate is not supported as a window function here",
        )),
        AggFunc::JsonObjectAgg | AggFunc::JsonbObjectAgg => Err(exec_err(
            "0A000",
            "json_object_agg is not supported as a window function",
        )),
        // v1.88: windowed range aggregates share the grouped logic.
        AggFunc::RangeAgg => {
            let mut ranges: Vec<RangeVal> = Vec::new();
            let mut elem_name: Option<(RangeElem, Option<String>)> = None;
            for v in vals {
                match v {
                    Value::Range(r) => {
                        elem_name = Some((r.elem, r.name.clone()));
                        ranges.push((**r).clone());
                    }
                    Value::Multirange(m) => {
                        elem_name = Some((m.elem, m.name.clone()));
                        ranges.extend(m.ranges.iter().cloned());
                    }
                    Value::Null => {}
                    other => {
                        return Err(exec_err(
                            "42883",
                            format!(
                                "function range_agg({}) does not exist",
                                other.type_name().to_lowercase()
                            ),
                        ));
                    }
                }
            }
            // v1.91: PG range_agg_finalfn returns NULL on zero
            // non-NULL inputs (see agg.rs).
            if elem_name.is_none() {
                return Ok(Value::Null);
            }
            let (elem, mname) = elem_name.unwrap_or((RangeElem::Int4, None));
            Ok(Value::multirange(super::ranges::normalize_multi(
                elem, mname, ranges,
            )))
        }
        AggFunc::RangeIntersectAgg => {
            let mut acc: Option<MultiVal> = None;
            let mut single = true;
            for v in vals {
                let m = match v {
                    Value::Range(r) => super::ranges::multi_from_range((**r).clone()),
                    Value::Multirange(m) => {
                        single = false;
                        (**m).clone()
                    }
                    Value::Null => continue,
                    other => {
                        return Err(exec_err(
                            "42883",
                            format!(
                                "function range_intersect_agg({}) does not exist",
                                other.type_name().to_lowercase()
                            ),
                        ));
                    }
                };
                acc = Some(match acc {
                    None => m,
                    Some(a) => super::ranges::multi_intersect(&a, &m),
                });
            }
            match acc {
                None => Ok(Value::Null),
                Some(acc) if single && acc.ranges.len() == 1 => {
                    Ok(Value::range(acc.ranges[0].clone()))
                }
                Some(acc) if single && acc.ranges.is_empty() => Ok(Value::range(
                    super::ranges::empty_range(acc.elem, acc.name.clone()),
                )),
                Some(acc) => Ok(Value::multirange(acc)),
            }
        }
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
