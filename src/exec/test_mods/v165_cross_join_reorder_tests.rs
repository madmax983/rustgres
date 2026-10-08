// ============================================================================
// v1.65: 3-way cross-join reordering (t5's subquery-flatten +
// qual-pushdown + reorder gap).
// ============================================================================
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

fn plan_lines(mut eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(&mut eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .map(|l| l.trim().to_string())
            .collect(),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn setup_t5(eng: &mut Engine) {
    run(
        &mut *eng,
        "CREATE TABLE sj (a int unique, b int, c int unique)",
    )
    .unwrap();
    run(
        &mut *eng,
        "INSERT INTO sj VALUES (1, null, 2), (null, 2, null), (2, 1, 1), (3, 1, 3)",
    )
    .unwrap();
    run(&mut *eng, "CREATE TABLE sl (a int, b int, c int)").unwrap();
    run(&mut *eng, "CREATE UNIQUE INDEX ON sl (a, b)").unwrap();
}

/// v1.65: t5's plan matches the PG19 oracle byte-exactly — the
/// subquery flattens, `q0.a = 1` pushes to n2's scan (unqualified
/// per PG19 `show_scan_qual`), and the 3-way reorder builds
/// ((sl⋈n2)⋈n1) with the Join Filter at the top.
#[test]
fn v165_t5_plan_matches_oracle() {
    let mut eng = engine();
    setup_t5(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM \
             (SELECT n2.a FROM sj n1, sj n2 WHERE n1.a <> n2.a) q0, sl \
             WHERE q0.a = 1",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "Join Filter: (n1.a <> n2.a)".to_string(),
            "->  Nested Loop".to_string(),
            "->  Seq Scan on sl".to_string(),
            "->  Seq Scan on sj n2".to_string(),
            "Filter: (a = 1)".to_string(),
            "->  Seq Scan on sj n1".to_string(),
        ]
    );
}

/// v1.65: a pulled-up subquery's scan Filter is NOT qualified in
/// non-verbose mode (PG19 `show_scan_qual`: `useprefix =
/// IsA(SubqueryScan) || verbose`). The v1.54 `dead_rtes` rule
/// over-qualified these.
#[test]
fn v165_pulled_up_subquery_filter_unqualified() {
    let mut eng = engine();
    setup_t5(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM \
             (SELECT n2.a, n2.b FROM sj n2 WHERE n2.a = 1) q0, sl",
    );
    assert!(
        lines.iter().any(|l| l == "Filter: (a = 1)"),
        "unqualified filter expected, got: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("n2.a = 1")),
        "no qualified filter expected, got: {lines:?}"
    );
}

/// v1.65: a 3-way cross join where the written order is already
/// optimal keeps it (fail closed to the status-quo text) — the
/// reorder only fires on a strictly better model order.
#[test]
fn v165_written_order_kept_when_optimal() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE w1 (a int)").unwrap();
    run(&mut eng, "CREATE TABLE w2 (a int)").unwrap();
    run(&mut eng, "CREATE TABLE w3 (a int)").unwrap();
    run(&mut eng, "INSERT INTO w1 VALUES (1), (2)").unwrap();
    run(&mut eng, "INSERT INTO w2 VALUES (1), (2)").unwrap();
    run(&mut eng, "INSERT INTO w3 VALUES (1), (2)").unwrap();
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM w1, w2, w3");
    // All three tables are the same size with no quals: every
    // order ties, so the written left-deep order stands (the
    // Materialize nodes are the pre-existing `pg_maybe_materialize`
    // behavior, unaffected by the reorder).
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Seq Scan on w1".to_string(),
            "->  Materialize".to_string(),
            "->  Seq Scan on w2".to_string(),
            "->  Materialize".to_string(),
            "->  Seq Scan on w3".to_string(),
        ]
    );
}

/// v1.65: fail closed — a 3-way join with an outer join (not a
/// pure Cross product) never enters the reorder path.
#[test]
fn v165_outer_join_not_reordered() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE o1 (a int)").unwrap();
    run(&mut eng, "CREATE TABLE o2 (a int)").unwrap();
    run(&mut eng, "CREATE TABLE o3 (a int)").unwrap();
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM o1 LEFT JOIN o2 ON o1.a = o2.a, o3",
    );
    assert!(
        lines.iter().any(|l| l.contains("Left Join")),
        "outer join preserved, got: {lines:?}"
    );
}

/// v1.65: soundness — the reorder changes the PLAN but never the
/// RESULT. The executor never sees `PlanNode`; executing t5's
/// query returns the rows PG would return.
#[test]
fn v165_reorder_result_identity() {
    let mut eng = engine();
    setup_t5(&mut eng);
    run(&mut eng, "INSERT INTO sl VALUES (9, 9, 9)").unwrap();
    // sj.n2 with a=1 is (1, null, 2); n1.a <> 1 holds for the
    // (2,1,1) and (3,1,3) rows (null is not <>). q0 yields two
    // rows, both a=1; cross with sl's single row.
    let out = match run(
        &mut eng,
        "SELECT * FROM (SELECT n2.a FROM sj n1, sj n2 \
             WHERE n1.a <> n2.a) q0, sl WHERE q0.a = 1",
    )
    .expect("runs")
    {
        ExecResult::Select { rows, .. } => rows,
        other => panic!("expected Select, got {:?}", other),
    };
    assert_eq!(out.len(), 2);
    for row in &out {
        let vals: Vec<String> = row.iter().map(|v| v.to_text().unwrap()).collect();
        assert_eq!(vals, vec!["1", "9", "9", "9"]);
    }
}

/// v1.65: the reorder path is EXPLAIN-only — `COSTS ON` (legacy
/// rendering) plans are unaffected.
#[test]
fn v165_costs_on_unaffected() {
    let mut eng = engine();
    setup_t5(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN SELECT * FROM \
             (SELECT n2.a FROM sj n1, sj n2 WHERE n1.a <> n2.a) q0, sl \
             WHERE q0.a = 1",
    );
    assert!(
        lines.iter().any(|l| l.starts_with("Nested Loop")),
        "nested loop plan expected, got: {lines:?}"
    );
}
