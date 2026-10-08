// ---------------------------------------------------------------------------
// v1.30: PG19 ordered-set aggregates (WITHIN GROUP) — unit tests
// ---------------------------------------------------------------------------
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    // Preserve the parser's SQLSTATE (e.g. 42601), like the main
    // test harness does.
    let stmt = crate::sql::parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        snap: &snap,
        own: 9,
        write_xid: 9,
        all_xids: vec![9],
        level: IsolationLevel::ReadCommitted,
        writes: &mut writes,
        session: 0,
        role: "postgres",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    execute(eng, &mut ctx, &stmt)
}

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } | ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn s(v: &str) -> String {
    v.to_string()
}

fn one(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    rows_of(run(eng, sql).unwrap())
}

fn err_of(eng: &mut Engine, sql: &str) -> ExecError {
    run(eng, sql).unwrap_err()
}

fn cols_of(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap() {
        ExecResult::Select { columns, .. } => columns.into_iter().map(|c| c.0).collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

// -- percentile_cont ----------------------------------------------------

/// Scalar form over ints: PG19 coerces the sort column to float8
/// and linearly interpolates (p*(N-1) = 1.5 → 2 + 0.5*(3-2)).
#[test]
fn cont_scalar_int() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(0.5) within group (order by x) \
                 from (values (1),(2),(3),(4)) as t(x)"
        ),
        vec![vec![s("2.5")]]
    );
}

/// DESC ordering flips the interpolation ends.
#[test]
fn cont_desc() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(0.25) within group (order by x desc) \
                 from (values (1),(2),(3),(4)) as t(x)"
        ),
        vec![vec![s("3.25")]]
    );
}

/// Array-fraction form: one result per element, NULL in → NULL
/// out, float8[] shape.
#[test]
fn cont_array_form() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(array[0,0.25,0.5,0.75,1,null]) \
                 within group (order by x) from (values (1),(2),(3),(4)) as t(x)"
        ),
        vec![vec![s("{1,1.75,2.5,3.25,4,NULL}")]]
    );
}

/// NULL inputs are skipped by the transition (PG19
/// `ordered_set_transition`).
#[test]
fn cont_skips_null_inputs() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(0.5) within group (order by x) \
                 from (values (null),(1),(3)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// NULL fraction → NULL (not 2201W).
#[test]
fn cont_null_fraction() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(null) within group (order by x) \
                 from (values (1)) as t(x)"
        ),
        vec![vec![s("NULL")]]
    );
}

/// No rows → NULL, even for the array form.
#[test]
fn cont_no_rows() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(0.5) within group (order by x) \
                 from (values (1)) as t(x) where false"
        ),
        vec![vec![s("NULL")]]
    );
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(array[0.5]) within group (order by x) \
                 from (values (1)) as t(x) where false"
        ),
        vec![vec![s("NULL")]]
    );
}

/// Only NULL inputs → NULL (PG19 treats "no non-null rows" as
/// empty).
#[test]
fn cont_only_null_inputs() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(0.5) within group (order by x) \
                 from (values (null),(null)) as t(x)"
        ),
        vec![vec![s("NULL")]]
    );
}

/// FILTER gates the input rows before the WITHIN GROUP sort.
#[test]
fn cont_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_cont(0.5) within group (order by x) \
                 filter (where x > 1) from (values (1),(2),(3),(4)) as t(x)"
        ),
        vec![vec![s("3")]]
    );
}

/// Out-of-range and NaN fractions → 2201W, PG-verbatim message.
#[test]
fn cont_bad_fraction() {
    let mut eng = engine();
    for frac in ["2", "-0.5"] {
        let err = err_of(
            &mut eng,
            &format!(
                "select percentile_cont({frac}) within group (order by x) \
                     from (values (1)) as t(x)"
            ),
        );
        assert_eq!(err.code, "2201W");
        assert_eq!(
            err.message,
            format!("percentile value {frac} is not between 0 and 1")
        );
    }
    let err = err_of(
        &mut eng,
        "select percentile_cont('nan'::float8) within group (order by x) \
             from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "2201W");
    assert_eq!(err.message, "percentile value NaN is not between 0 and 1");
}

/// A non-numeric fraction has no such signature (42883).
#[test]
fn cont_text_fraction_rejected() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select percentile_cont('a') within group (order by x) \
             from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42883");
}

/// Works under GROUP BY, one ordered set per group.
#[test]
fn cont_group_by() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select g, percentile_cont(0.5) within group (order by x) \
                 from (values (1,1),(1,2),(2,10),(2,20)) as t(g,x) \
                 group by g order by g"
        ),
        vec![vec![s("1"), s("1.5")], vec![s("2"), s("15")]]
    );
}

// -- percentile_disc ----------------------------------------------------

/// Discrete form: rownum = ceil(p*N), 1-based.
#[test]
fn disc_scalar() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_disc(0.5) within group (order by x) \
                 from (values (1),(2),(3),(4)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// Array form over a non-numeric sort column returns that type's
/// array.
#[test]
fn disc_array_text() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_disc(array[0.25,0.5,0.75]) within group (order by x) \
                 from (values ('a'),('b'),('c'),('d')) as t(x)"
        ),
        vec![vec![s("{a,b,c}")]]
    );
}

/// p=0 takes the first row, p=1 the last.
#[test]
fn disc_endpoints() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_disc(0) within group (order by x), \
                        percentile_disc(1) within group (order by x) \
                 from (values (1),(2),(3)) as t(x)"
        ),
        vec![vec![s("1"), s("3")]]
    );
}

/// NULL fraction → NULL; no rows → NULL.
#[test]
fn disc_nulls() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percentile_disc(null) within group (order by x) \
                 from (values (1)) as t(x)"
        ),
        vec![vec![s("NULL")]]
    );
    assert_eq!(
        one(
            &mut eng,
            "select percentile_disc(0.5) within group (order by x) \
                 from (values (1)) as t(x) where false"
        ),
        vec![vec![s("NULL")]]
    );
}

/// Bad fractions → 2201W, like cont.
#[test]
fn disc_bad_fraction() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select percentile_disc(1.5) within group (order by x) \
             from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "2201W");
    assert_eq!(err.message, "percentile value 1.5 is not between 0 and 1");
}

// -- mode ---------------------------------------------------------------

/// Most frequent value.
#[test]
fn mode_basic() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select mode() within group (order by x) \
                 from (values (1),(2),(2),(3)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// Ties break to the first in sort order.
#[test]
fn mode_tie_first_in_sort_order() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select mode() within group (order by x) \
                 from (values (1),(1),(2),(2)) as t(x)"
        ),
        vec![vec![s("1")]]
    );
    // DESC flips which value sorts first.
    assert_eq!(
        one(
            &mut eng,
            "select mode() within group (order by x desc) \
                 from (values (1),(1),(2),(2)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// NULL inputs are ignored; all-NULL → NULL.
#[test]
fn mode_nulls() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select mode() within group (order by x) \
                 from (values (null),(1),(1)) as t(x)"
        ),
        vec![vec![s("1")]]
    );
    assert_eq!(
        one(
            &mut eng,
            "select mode() within group (order by x) \
                 from (values (1)) as t(x) where false"
        ),
        vec![vec![s("NULL")]]
    );
}

/// Text sort column works too.
#[test]
fn mode_text() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select mode() within group (order by x) \
                 from (values ('b'),('a'),('b')) as t(x)"
        ),
        vec![vec![s("b")]]
    );
}

// -- hypothetical-set aggregates ----------------------------------------

/// PG19's own numbers: rank(3) over (1,1,2,2,3,3,4) = 5.
#[test]
fn hypothetical_rank() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select rank(3) within group (order by x) \
                 from (values (1),(1),(2),(2),(3),(3),(4)) as t(x)"
        ),
        vec![vec![s("5")]]
    );
}

/// dense_rank(3) over (1,1,2,2,3,3,4) = 3.
#[test]
fn hypothetical_dense_rank() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select dense_rank(3) within group (order by x) \
                 from (values (1),(1),(2),(2),(3),(3),(4)) as t(x)"
        ),
        vec![vec![s("3")]]
    );
}

/// percent_rank(3) over 8 rows = (5-1)/8 = 0.5.
#[test]
fn hypothetical_percent_rank() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select percent_rank(3) within group (order by x) \
                 from (values (1),(1),(2),(2),(3),(3),(4),(5)) as t(x)"
        ),
        vec![vec![s("0.5")]]
    );
}

/// cume_dist(3) over (1,1,2,2,3,3,4) = (1+4+2)/8 = 0.875.
#[test]
fn hypothetical_cume_dist() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select cume_dist(3) within group (order by x) \
                 from (values (1),(1),(2),(2),(3),(3),(4)) as t(x)"
        ),
        vec![vec![s("0.875")]]
    );
}

/// Multi-column hypothetical: the direct args line up with the
/// sort columns left to right.
#[test]
fn hypothetical_multi_column() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select rank(2,'b') within group (order by x, y) \
                 from (values (1,'a'),(2,'b'),(2,'c')) as t(x,y)"
        ),
        vec![vec![s("2")]]
    );
}

/// DESC ordering: rank(3) over (1,2,3,4) desc = 2 (only 4 sorts
/// ahead).
#[test]
fn hypothetical_desc() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select rank(3) within group (order by x desc) \
                 from (values (1),(2),(3),(4)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// FILTER gates the hypothetical input rows too.
#[test]
fn hypothetical_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select rank(3) within group (order by x) filter (where x > 1) \
                 from (values (1),(2),(3)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// NULL sort-key inputs are kept (PG19
/// `ordered_set_transition_multi` never skips NULLs): with
/// explicit NULLS FIRST the NULL sorts ahead of the hypothetical
/// row and counts toward its rank.
#[test]
fn hypothetical_keeps_null_keys() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select rank(1) within group (order by x nulls first) \
                 from (values (null),(2)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// Empty input: rank/dense_rank = 1, percent_rank = 0, cume_dist
/// = 1 (PG19 finalfn constants).
#[test]
fn hypothetical_empty_input() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select rank(1) within group (order by x), \
                        dense_rank(1) within group (order by x), \
                        percent_rank(1) within group (order by x), \
                        cume_dist(1) within group (order by x) \
                 from (values (1)) as t(x) where false"
        ),
        vec![vec![s("1"), s("1"), s("0"), s("1")]]
    );
}

/// Each NULL-keyed row is its own dense_rank peer group (PG19
/// uses the `=` operator; NULL never equals). NULLS FIRST puts
/// both NULLs ahead of the hypothetical row; they do not merge.
#[test]
fn hypothetical_dense_rank_nulls_are_peers_of_none() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select dense_rank(1) within group (order by x nulls first) \
                 from (values (null),(null),(1)) as t(x)"
        ),
        vec![vec![s("3")]]
    );
}

/// Mismatched direct-arg / sort-column types → 42804.
#[test]
fn hypothetical_type_mismatch() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select rank('a') within group (order by x) from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42804");
    assert!(
        err.message.contains("WITHIN GROUP types"),
        "{}",
        err.message
    );
}

/// Numerics unify across int/float (PG19 implicit coercion).
#[test]
fn hypothetical_numeric_unification() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select rank(1.5) within group (order by x) \
                 from (values (1),(2)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// Wrong arity → 42883 with PG's hint style.
#[test]
fn hypothetical_arity() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select rank(1,2) within group (order by x) from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42883");
    assert!(err.message.contains("HINT"), "{}", err.message);
}

/// Works under GROUP BY with correlated direct args.
#[test]
fn hypothetical_group_by() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select g, rank(g) within group (order by x) \
                 from (values (1,10),(1,20),(2,30)) as t(g,x) \
                 group by g order by g"
        ),
        vec![vec![s("1"), s("1")], vec![s("2"), s("1")]]
    );
}

// -- parse / validation errors ------------------------------------------

/// WITHIN GROUP is required for ordered-set aggregates (42809).
#[test]
fn missing_within_group() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select percentile_cont(0.5) from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42809");
    assert!(
        err.message.contains("WITHIN GROUP is required"),
        "{}",
        err.message
    );
    let err = err_of(&mut eng, "select mode() from (values (1)) as t(x)");
    assert_eq!(err.code, "42809");
}

/// `rank()`/`dense_rank()` without WITHIN GROUP still parse as
/// window functions (PG19's rule).
#[test]
fn bare_rank_is_window() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select rank() over (order by x) from (values (2),(1)) as t(x) order by x"
        ),
        vec![vec![s("1")], vec![s("2")]]
    );
}

/// A plain aggregate cannot take WITHIN GROUP (42809).
#[test]
fn plain_agg_within_group_rejected() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select sum(x) within group (order by x) from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42809");
    assert!(
        err.message.contains("not an ordered-set aggregate"),
        "{}",
        err.message
    );
}

/// A non-aggregate cannot take WITHIN GROUP (42809, PG19
/// parse_func.c ERRCODE_WRONG_OBJECT_TYPE).
#[test]
fn nonagg_within_group_rejected() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select abs(x) within group (order by x) from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42809");
    assert!(
        err.message.contains("not an aggregate function"),
        "{}",
        err.message
    );
    let err = err_of(
        &mut eng,
        "select row_number() within group (order by x) from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42809");
    assert!(
        err.message
            .contains("window function row_number cannot have WITHIN GROUP"),
        "{}",
        err.message
    );
}

/// DISTINCT on the direct args is rejected (42601).
#[test]
fn distinct_direct_rejected() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select percentile_cont(distinct 0.5) within group (order by x) \
             from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42601");
}

/// Two ORDER BYs (aggregate's own + WITHIN GROUP) → 42601.
#[test]
fn double_order_by_rejected() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select sum(x order by x) within group (order by x) \
             from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42601");
}

/// Ordered-set aggregates cannot be windowed (0A000).
#[test]
fn over_rejected() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select percentile_cont(0.5) within group (order by x) over () \
             from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "0A000");
}

/// An unknown function with WITHIN GROUP → 42809. (PG raises
/// 42883 because its catalog lookup fails first; rustgres
/// resolves unknown names at execution, so the WITHIN GROUP
/// kind check applies uniformly — deliberate approximation.)
#[test]
fn unknown_func_within_group() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select nosuchfn(1) within group (order by x) from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42809");
}

/// Unaliased select items are named after the aggregate (PG19).
#[test]
fn output_column_names() {
    let mut eng = engine();
    assert_eq!(
        cols_of(
            &mut eng,
            "select percentile_cont(0.5) within group (order by x), \
                        mode() within group (order by x), \
                        rank(1) within group (order by x) \
                 from (values (1)) as t(x)"
        ),
        vec![s("percentile_cont"), s("mode"), s("rank")]
    );
}

/// FILTER on a within-group aggregate rejects grouping ops, like
/// plain aggregates.
#[test]
fn filter_rejects_grouping() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select percentile_cont(0.5) within group (order by x) \
             filter (where x > percentile_cont(0.5) within group (order by x)) \
             from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42803");
}

/// percentile_cont with a bad sort type → 42883.
#[test]
fn cont_text_sort_rejected() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select percentile_cont(0.5) within group (order by x) \
             from (values ('a')) as t(x)",
    );
    assert_eq!(err.code, "42883");
}
