// ============================================================================
// v1.76: PG19 EC-deferral ordering for scan Filter quals.
// ============================================================================
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
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

fn plan_raw(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .collect(),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn filter_line(eng: &mut Engine, sql: &str) -> String {
    plan_raw(eng, sql)
        .into_iter()
        .find(|l| l.trim_start().starts_with("Filter:"))
        .expect("Filter line")
        .trim()
        .to_string()
}

#[test]
fn v176_eq_deferred_after_non_eq() {
    // select.sql target shape: `unique2 = 11 AND stringu1 < 'C'` —
    // PG19 defers the `=` conjunct after the non-`=` one.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t176 (a int, s name)").unwrap();
    assert_eq!(
        filter_line(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t176 WHERE a = 11 AND s < 'C'"
        ),
        "Filter: ((s < 'C'::name) AND (a = 11))"
    );
}

#[test]
fn v176_ne_before_eq() {
    // select_distinct.sql target shape: `four = 0 AND two <> 0` —
    // PG19 renders `((two <> 0) AND (four = 0))`.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t176b (a int, b int)").unwrap();
    assert_eq!(
        filter_line(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t176b WHERE a = 0 AND b <> 0"
        ),
        "Filter: ((b <> 0) AND (a = 0))"
    );
}

#[test]
fn v176_all_eq_keeps_written_order() {
    // No mixing: all-`=` Filters are untouched (fail-closed no-op).
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t176c (a int, b int)").unwrap();
    assert_eq!(
        filter_line(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t176c WHERE a = 0 AND b = 1"
        ),
        "Filter: ((a = 0) AND (b = 1))"
    );
}

#[test]
fn v176_no_eq_keeps_written_order() {
    // No mixing: no-`=` Filters are untouched.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t176d (a int, b int)").unwrap();
    assert_eq!(
        filter_line(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t176d WHERE a > 0 AND b < 1"
        ),
        "Filter: ((a > 0) AND (b < 1))"
    );
}

#[test]
fn v176_self_eq_not_deferred() {
    // `X = X` becomes `X IS NOT NULL` in PG19 (not deferred as `=`).
    // rustgres renders the comparison itself; the ordering rule must
    // not move it.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t176e (a int, b int)").unwrap();
    assert_eq!(
        filter_line(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t176e WHERE a = a AND b <> 0"
        ),
        "Filter: ((a = a) AND (b <> 0))"
    );
}

#[test]
fn v176_true_conjunct_preserved() {
    // The re-fold must not drop `true` conjuncts (pg_fold_and would).
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t176f (a int, b boolean)").unwrap();
    assert_eq!(
        filter_line(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t176f WHERE b AND a = 1"
        ),
        "Filter: (b AND (a = 1))"
    );
}
