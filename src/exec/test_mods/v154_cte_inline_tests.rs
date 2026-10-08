/// v1.54: CTE inlining for simple non-recursive CTEs (PG19 `inline_cte`
/// + `pull_up_simple_subquery`). Single-ref CTEs with a simple SELECT
/// body, no DML, and no volatile functions inline; the resulting
/// top-level subquery pulls up to a flat scan.
use super::*;
use crate::sql::parse_statement;

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

fn plan_lines(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap() {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|r| r[0].to_text().unwrap().to_string())
            .collect(),
        other => panic!("expected EXPLAIN, got {other:?}"),
    }
}

fn setup(eng: &mut Engine) {
    run(
        eng,
        "CREATE TABLE subselect_tbl (f1 integer, f2 integer, f3 integer)",
    )
    .unwrap();
    run(eng, "INSERT INTO subselect_tbl VALUES (1, 2, 3)").unwrap();
    run(eng, "CREATE TABLE int4_tbl (f1 integer)").unwrap();
}

#[test]
fn cte_inline_basic() {
    // v1.54: basic single-ref CTE inlines to a flat Seq Scan with
    // qualified Output/Filter (dead RTE forces useprefix).
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) WITH x AS (SELECT f1 FROM subselect_tbl) SELECT * FROM x WHERE f1 = 1",
    );
    assert_eq!(lines[0], "Seq Scan on public.subselect_tbl");
    assert_eq!(lines[1], "  Output: subselect_tbl.f1");
    assert_eq!(lines[2], "  Filter: (subselect_tbl.f1 = 1)");
}

#[test]
fn cte_inline_stable_fn() {
    // v1.54: STABLE functions (now()) do not block inlining — PG
    // treats now()/current_timestamp as STABLE, not VOLATILE.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) WITH x AS (SELECT f1, now() FROM subselect_tbl) SELECT * FROM x WHERE f1 = 1",
    );
    assert_eq!(lines[0], "Seq Scan on public.subselect_tbl");
    assert_eq!(lines[1], "  Output: subselect_tbl.f1, now()");
}

#[test]
fn cte_no_inline_volatile() {
    // v1.54: VOLATILE functions (random()) block inlining — the CTE
    // stays a Subquery Scan.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) WITH x AS (SELECT f1, random() FROM subselect_tbl) SELECT * FROM x WHERE f1 = 1",
    );
    assert_eq!(lines[0], "Subquery Scan on x");
}

#[test]
fn cte_no_inline_materialized() {
    // v1.54: AS MATERIALIZED blocks inlining per PG semantics.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) WITH x AS MATERIALIZED (SELECT f1 FROM subselect_tbl) SELECT * FROM x WHERE f1 = 1",
    );
    assert_eq!(lines[0], "Subquery Scan on x");
}

#[test]
fn cte_inline_nested() {
    // v1.54: nested CTEs inline recursively — inner refs resolve to
    // the inlined outer body.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) WITH x AS (SELECT * FROM int4_tbl) SELECT * FROM (WITH y AS (SELECT * FROM x) SELECT * FROM y) ss",
    );
    assert_eq!(lines[0], "Seq Scan on public.int4_tbl");
    assert_eq!(lines[1], "  Output: int4_tbl.f1");
}

#[test]
fn cte_inline_shadowed() {
    // v1.54: inner WITH rebinds the name (PG `ctelevelsup`
    // shadowing) — the inner x (SELECT 2) wins.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) WITH x AS (SELECT 1) SELECT * FROM (WITH x AS (SELECT 2) SELECT * FROM x) ss",
    );
    assert_eq!(lines[0], "Result");
    assert_eq!(lines[1], "  Output: 2");
}

#[test]
fn cte_multi_ref_no_inline() {
    // v1.54: multi-referenced CTEs never inline (stricter than PG,
    // which allows it for non-volatile; untestable here so we stay
    // conservative).
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) WITH x AS (SELECT f1 FROM subselect_tbl) SELECT * FROM x, x AS x2",
    );
    assert!(
        lines.iter().any(|l| l.contains("Subquery Scan on x")),
        "{lines:?}"
    );
}
