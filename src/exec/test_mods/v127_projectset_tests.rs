/// v1.27: PG19 `select_with_parens` in FROM/LATERAL (S1) and PG19
/// ProjectSet-over-Agg SRF fan-out (S2).
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

fn setup(eng: &mut Engine) {
    run(eng, "CREATE TABLE int4_tbl (f1 int)").unwrap();
    run(
        eng,
        "INSERT INTO int4_tbl VALUES (0),(123456),(-123456),(2147483647),(-2147483647)",
    )
    .unwrap();
}

// -- S1: PG19 select_with_parens in FROM/LATERAL --------------------

/// Corpus join-suite B: `((select 1) union all (select 2))` shapes
/// in FROM used to be 42601; now they parse per PG19
/// `select_with_parens`.
#[test]
fn parenthesized_setop_in_from() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select * from (select null::int as c0 from ((select 1) union all (select 2))) t1 \
             cross join (select null::int as c1 from ((select 1) union all (select 2))) t2",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["NULL".to_string(), "NULL".to_string()],
            vec!["NULL".to_string(), "NULL".to_string()],
            vec!["NULL".to_string(), "NULL".to_string()],
            vec!["NULL".to_string(), "NULL".to_string()],
        ]
    );
}

/// Corpus join-suite C: LATERAL over a parenthesized set operation.
#[test]
fn lateral_parenthesized_setop() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select q1.v, q2.v from (select 1 as v) as q1 cross join lateral \
             ((select * from ((select 4 as v) union all (select 5 as v)) as q3) \
             union all (select q1.v)) as q2 order by 1, 2",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "1".to_string()],
            vec!["1".to_string(), "4".to_string()],
            vec!["1".to_string(), "5".to_string()],
        ]
    );
}

/// Control: single-paren setop in FROM (pre-v1.27 behavior).
#[test]
fn single_paren_setop_unchanged() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select * from (select 1 union all select 2) t order by 1",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec!["1".to_string()], vec!["2".to_string()]]
    );
}

/// Control: redundant parens around a plain subselect (v0.14).
#[test]
fn redundant_parens_unchanged() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(&mut eng, "select * from ((select 1 as x)) ss order by 1").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
}

/// Control: parenthesized VALUES in FROM (pre-v1.27 behavior).
#[test]
fn parenthesized_values_unchanged() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(&mut eng, "select * from ((values (1),(2))) v(x) order by 1").unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec!["1".to_string()], vec!["2".to_string()]]
    );
}

// -- S2: PG19 ProjectSet over Agg ----------------------------------

/// Bare top-level SRF in a grouped select fans out per group.
#[test]
fn grouped_bare_srf_fans_out() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select x, generate_series(1,3) from (values (1),(2)) t(x) group by x order by 1, 2",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "1".to_string()],
            vec!["1".to_string(), "2".to_string()],
            vec!["1".to_string(), "3".to_string()],
            vec!["2".to_string(), "1".to_string()],
            vec!["2".to_string(), "2".to_string()],
            vec!["2".to_string(), "3".to_string()],
        ]
    );
}

/// Corpus subselect-suite A: the SRF is nested
/// (`generate_series(1,50)/10`); PG19's target-list SRF scan finds
/// it and fans out over the aggregate.
#[test]
fn grouped_nested_srf_in_subselect() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select * from int4_tbl o where (f1, f1) in \
             (select f1, generate_series(1,50) / 10 g from int4_tbl i group by f1)",
    )
    .unwrap();
    // int4_tbl = {0, 123456, -123456, 2147483647, -2147483647};
    // only f1 = 0 matches (f1 in {0..5} after the /10 fan-out).
    assert_eq!(rows_of(r), vec![vec!["0".to_string()]]);
}

/// Nested SRF under arithmetic fans out with the expression
/// evaluated per fanned row.
#[test]
fn grouped_nested_srf_arithmetic() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select x, generate_series(1,6)/2 g from (values (1)) t(x) group by x order by 1, 2",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "0".to_string()],
            vec!["1".to_string(), "1".to_string()],
            vec!["1".to_string(), "1".to_string()],
            vec!["1".to_string(), "2".to_string()],
            vec!["1".to_string(), "2".to_string()],
            vec!["1".to_string(), "3".to_string()],
        ]
    );
}

/// Several SRFs zip; the exhausted one pads with NULL.
#[test]
fn grouped_multi_srf_null_pads() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select generate_series(1,2), generate_series(10,12) from (values (1)) t(x) group by x",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "11".to_string()],
            vec!["NULL".to_string(), "12".to_string()],
        ]
    );
}

/// An all-empty SRF set drops the grouped row (PG `hasresult`).
#[test]
fn grouped_empty_srf_drops_row() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select generate_series(2,1) from (values (1)) t(x) group by x",
    )
    .unwrap();
    assert!(rows_of(r).is_empty());
}

/// An SRF that is itself a GROUP BY key reads the grouped value
/// (v0.47 path), not a fresh fan-out.
#[test]
fn grouped_srf_key_unchanged() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
            &mut eng,
            "select generate_series(1,3) g from (values (1)) t(x) group by generate_series(1,3) order by 1",
        )
        .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string()],
            vec!["2".to_string()],
            vec!["3".to_string()],
        ]
    );
}

/// SRF arguments evaluate once per group in grouped context:
/// aggregates fold the group.
#[test]
fn grouped_srf_agg_argument() {
    let mut eng = engine();
    setup(&mut eng);
    let r = run(
        &mut eng,
        "select x, generate_series(1, max(x)) from (values (1),(2)) t(x) group by x order by 1, 2",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "1".to_string()],
            vec!["2".to_string(), "1".to_string()],
            vec!["2".to_string(), "2".to_string()],
        ]
    );
}

/// An SRF under an aggregate keeps the 42883 it always had (PG19
/// evaluates such SRFs below the Agg, which this engine rejects).
#[test]
fn srf_under_agg_still_42883() {
    let mut eng = engine();
    setup(&mut eng);
    let err = run(
        &mut eng,
        "select sum(generate_series(1,3)) from (values (1)) t(x) group by x",
    )
    .unwrap_err();
    assert_eq!(err.code, "42883");
}
