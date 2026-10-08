/// v1.29: PG19 `FILTER (WHERE ...)` on aggregate calls — the filter
/// gates each input row before accumulation (PG19 nodeAgg.c folds
/// `aggfilter` into the transition expression); groups are still formed
/// from all rows. Also covers the PG19 error shapes: FILTER on a
/// non-aggregate (42803), on a true window function (0A000), and
/// aggregates / windows / SRFs directly inside FILTER (42803 / 42803 /
/// 0A000), plus the 42804 boolean-coercion rule.
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

// -- plain aggregates -------------------------------------------------

/// The basic shape: only passing rows feed the transition.
#[test]
fn plain_sum_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select sum(x) filter (where x > 2) from (values (1),(2),(3),(4)) as t(x)"
        ),
        vec![vec![s("7")]]
    );
}

/// `count(*)` with FILTER counts only passing rows (not `idxs.len()`).
#[test]
fn count_star_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select count(*) filter (where x > 2), count(*) from (values (1),(2),(3)) as t(x)"
        ),
        vec![vec![s("1"), s("3")]]
    );
}

/// FALSE everywhere: sum is NULL, count is 0 (PG19 semantics).
#[test]
fn filter_all_false() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select sum(x) filter (where false), count(*) filter (where false) from (values (1),(2)) as t(x)"
        ),
        vec![vec![s("NULL"), s("0")]]
    );
}

/// Groups are formed from ALL rows; the filter only gates the
/// transition input.
#[test]
fn grouped_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select a, sum(x) filter (where y > 0) from (values (1,10,1),(1,20,0),(2,30,1)) as t(a,x,y) group by a order by a"
        ),
        vec![vec![s("1"), s("10")], vec![s("2"), s("30")]]
    );
}

/// A group whose rows all fail the filter still appears (with NULL).
#[test]
fn grouped_filter_empty_group() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select a, sum(x) filter (where y > 0) from (values (1,10,1),(2,30,0)) as t(a,x,y) group by a order by a"
        ),
        vec![vec![s("1"), s("10")], vec![s("2"), s("NULL")]]
    );
}

/// DISTINCT applies after the filter (PG19 sorts the
/// filter-passing input).
#[test]
fn distinct_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select count(distinct x) filter (where x > 1) from (values (1),(2),(2),(3)) as t(x)"
        ),
        vec![vec![s("2")]]
    );
}

/// In-aggregate ORDER BY sorts the filter-passing rows.
#[test]
fn agg_order_by_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select string_agg(x::text, ',' order by x) filter (where x > 1) from (values (3),(1),(2)) as t(x)"
        ),
        vec![vec![s("2,3")]]
    );
}

/// NULL filter results skip the row (coerce_to_boolean semantics).
#[test]
fn filter_null_skips() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select count(*) filter (where x > 1) from (values (1),(null),(3)) as t(x)"
        ),
        vec![vec![s("1")]]
    );
}

/// FILTER in HAVING.
#[test]
fn having_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select a from (values (1),(1),(2)) as t(a) group by a having count(*) filter (where a = 1) > 1"
        ),
        vec![vec![s("1")]]
    );
}

// -- windowed aggregates ----------------------------------------------

/// FILTER on a windowed aggregate skips rows per frame (PG19
/// nodeWindowAgg.c "Skip anything FILTERed out").
#[test]
fn windowed_sum_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select x, sum(x) filter (where x > 1) over (order by x) from (values (1),(2),(3)) as t(x)"
        ),
        vec![
            vec![s("1"), s("NULL")],
            vec![s("2"), s("2")],
            vec![s("3"), s("5")]
        ]
    );
}

/// Windowed `count(*)` with FILTER counts passing rows per frame.
#[test]
fn windowed_count_star_filter() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select count(*) filter (where x > 1) over () from (values (1),(2),(3)) as t(x)"
        ),
        vec![vec![s("2")], vec![s("2")], vec![s("2")]]
    );
}

/// FILTER respects PARTITION BY and explicit frames.
#[test]
fn windowed_filter_partition_frame() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select x, sum(y) filter (where y > 0) over (order by x rows between 1 preceding and 1 following) from (values (1,10),(2,-5),(3,20)) as t(x,y)"
        ),
        vec![
            vec![s("1"), s("10")],
            vec![s("2"), s("30")],
            vec![s("3"), s("20")]
        ]
    );
}

// -- PG19 error shapes -------------------------------------------------

/// FILTER on a non-aggregate: 42803, PG-verbatim message.
#[test]
fn filter_on_plain_func() {
    let mut eng = engine();
    let err = err_of(&mut eng, "select upper('a') filter (where true)");
    assert_eq!(err.code, "42803");
    assert_eq!(
        err.message,
        "FILTER specified, but upper is not an aggregate function"
    );
}

/// FILTER on a true window function with OVER: 0A000.
#[test]
fn filter_on_window_func() {
    let mut eng = engine();
    let err = err_of(&mut eng, "select row_number() filter (where true) over ()");
    assert_eq!(err.code, "0A000");
    assert_eq!(
        err.message,
        "FILTER is not implemented for non-aggregate window functions"
    );
}

/// Aggregate directly inside FILTER: 42803.
#[test]
fn agg_in_filter() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select sum(x) filter (where sum(y) > 0) from (values (1,1)) as t(x,y)",
    );
    assert_eq!(err.code, "42803");
    assert_eq!(err.message, "aggregate functions are not allowed in FILTER");
}

/// Window function directly inside FILTER: 42803.
#[test]
fn window_in_filter() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select sum(x) filter (where y > row_number() over ()) from (values (1,1)) as t(x,y)",
    );
    assert_eq!(err.code, "42803");
    assert_eq!(err.message, "window functions are not allowed in FILTER");
}

/// SRF directly inside FILTER: 0A000.
#[test]
fn srf_in_filter() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select sum(x) filter (where generate_series(1,2) > 0) from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "0A000");
    assert_eq!(
        err.message,
        "set-returning functions are not allowed in FILTER"
    );
}

/// GROUPING directly inside FILTER: 42803.
#[test]
fn grouping_in_filter() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select sum(x) filter (where grouping(a) = 0) from (values (1,1)) as t(a,x) group by a",
    );
    assert_eq!(err.code, "42803");
    assert_eq!(err.message, "grouping operations are not allowed in FILTER");
}

/// Non-boolean FILTER: 42804, PG-verbatim construct name.
#[test]
fn filter_non_boolean() {
    let mut eng = engine();
    let err = err_of(
        &mut eng,
        "select sum(x) filter (where 'a') from (values (1)) as t(x)",
    );
    assert_eq!(err.code, "42804");
    assert_eq!(
        err.message,
        "argument of FILTER must be type boolean, not type text"
    );
}
