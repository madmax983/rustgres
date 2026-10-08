// ============================================================================
// v1.66: N-way cross-join reordering (PG19 `standard_join_search` DP).
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

fn setup_4way(eng: &mut Engine) {
    for t in ["f1", "f2", "f3", "f4"] {
        run(&mut *eng, &format!("CREATE TABLE {t} (a int)")).unwrap();
        run(
            &mut *eng,
            &format!("INSERT INTO {t} VALUES (1),(2),(3),(4)"),
        )
        .unwrap();
    }
}

fn setup_5way(eng: &mut Engine) {
    for t in ["g1", "g2", "g3", "g4", "g5"] {
        run(&mut *eng, &format!("CREATE TABLE {t} (a int)")).unwrap();
        run(
            &mut *eng,
            &format!("INSERT INTO {t} VALUES (1),(2),(3),(4)"),
        )
        .unwrap();
    }
}

/// v1.66: 4-way comma join — the N-way DP puts the selective
/// (filtered) table first, generalizing v1.65's level-3 logic.
#[test]
fn v166_four_way_selective_first() {
    let mut eng = engine();
    setup_4way(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM f1, f2, f3, f4 WHERE f1.a = 1",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Seq Scan on f1".to_string(),
            "Filter: (a = 1)".to_string(),
            "->  Seq Scan on f2".to_string(),
            "->  Seq Scan on f3".to_string(),
            "->  Seq Scan on f4".to_string(),
        ]
    );
}

/// v1.66: 4-way comma join with no quals — every order ties, so the
/// written left-deep order stands (fail closed, as v1.65 for N=3).
#[test]
fn v166_four_way_ties_written_order() {
    let mut eng = engine();
    setup_4way(&mut eng);
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM f1, f2, f3, f4");
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Seq Scan on f1".to_string(),
            "->  Materialize".to_string(),
            "->  Seq Scan on f2".to_string(),
            "->  Materialize".to_string(),
            "->  Seq Scan on f3".to_string(),
            "->  Materialize".to_string(),
            "->  Seq Scan on f4".to_string(),
        ]
    );
}

/// v1.66: 4-way comma join with a join clause — the clause attaches
/// at the lowest nestloop covering both tables (PG19
/// `distribute_qual_to_rels`), and the clause-linked pair is built
/// first (PG19 `make_rels_by_clause_joins`).
#[test]
fn v166_four_way_clause_at_lowest() {
    let mut eng = engine();
    setup_4way(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM f1, f2, f3, f4 WHERE f1.a = f2.a",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "Join Filter: (f1.a = f2.a)".to_string(),
            "->  Seq Scan on f1".to_string(),
            "->  Materialize".to_string(),
            "->  Seq Scan on f2".to_string(),
            "->  Seq Scan on f3".to_string(),
            "->  Seq Scan on f4".to_string(),
        ]
    );
}

/// v1.66: 5-way comma join — the DP builds the full left-deep tree
/// with the selective table first.
#[test]
fn v166_five_way_selective_first() {
    let mut eng = engine();
    setup_5way(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM g1, g2, g3, g4, g5 WHERE g3.a = 2",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Nested Loop".to_string(),
            "->  Seq Scan on g3".to_string(),
            "Filter: (a = 2)".to_string(),
            "->  Seq Scan on g1".to_string(),
            "->  Seq Scan on g2".to_string(),
            "->  Seq Scan on g4".to_string(),
            "->  Seq Scan on g5".to_string(),
        ]
    );
}

/// v1.72: 3-way comma join — the reorder DP's nestloop nodes render
/// PG19 EC-canonical join-filter representatives (the DP path's share
/// of v1.71's 2-way rule). EC in written WHERE order:
/// [t1.a, t1.b, t2.b, t2.a, t3.b, t3.a]; inner node (t1,t2) ->
/// (t1.a = t2.b), outer node ((t1,t2),t3) -> (t1.a = t3.b).
/// Byte-exact vs the join.out oracle.
#[test]
fn v172_3way_ec_join_filter_canonical() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE sj (a int unique, b int, c int unique)",
    )
    .unwrap();
    run(
        &mut eng,
        "INSERT INTO sj VALUES (1, null, 2), (null, 2, null), (2, 1, 1)",
    )
    .unwrap();
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM sj t1, sj t2, sj t3 \
             WHERE t1.a = t1.b AND t1.b = t2.b AND t2.b = t2.a AND \
             t1.b = t3.b AND t3.b = t3.a",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "Join Filter: (t1.a = t3.b)".to_string(),
            "->  Nested Loop".to_string(),
            "Join Filter: (t1.a = t2.b)".to_string(),
            "->  Seq Scan on sj t1".to_string(),
            "Filter: (a = b)".to_string(),
            "->  Seq Scan on sj t2".to_string(),
            "Filter: (b = a)".to_string(),
            "->  Seq Scan on sj t3".to_string(),
            "Filter: (b = a)".to_string(),
        ]
    );
}

/// v1.72: the EC rewrite is a no-op when the written conjunct is
/// already canonical — zero rendering drift for untouched DP plans.
#[test]
fn v172_dp_ec_noop_when_canonical() {
    let mut eng = engine();
    setup_4way(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM f1, f2, f3, f4 WHERE f1.a = f2.a",
    );
    assert!(lines.iter().any(|l| l == "Join Filter: (f1.a = f2.a)"));
}

/// v1.66: 9-way comma join fails closed to the written order (the
/// N<=8 DP bound; PG19 would use GEQO past 12).
#[test]
fn v166_nine_way_fail_closed() {
    let mut eng = engine();
    for i in 1..=9 {
        run(&mut eng, &format!("CREATE TABLE h{i} (a int)")).unwrap();
        run(&mut eng, &format!("INSERT INTO h{i} VALUES (1),(2)")).unwrap();
    }
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM h1, h2, h3, h4, h5, h6, h7, h8, h9",
    );
    // Written left-deep order, h1..h9 — the DP did not fire.
    let scans: Vec<&String> = lines.iter().filter(|l| l.contains("Seq Scan on")).collect();
    assert_eq!(scans.len(), 9, "nine scans expected, got: {lines:?}");
    for (i, s) in scans.iter().enumerate() {
        assert!(s.ends_with(&format!("Seq Scan on h{}", i + 1)), "got: {s}");
    }
    assert_eq!(
        lines.iter().filter(|l| l.contains("Nested Loop")).count(),
        8,
        "eight nestloops expected, got: {lines:?}"
    );
}

/// v1.66: reordering changes the plan, never the result — the
/// executor never sees `PlanNode`, so 4-way/5-way SELECTs return
/// the full cartesian product regardless of plan order.
#[test]
fn v166_reorder_result_identity() {
    let mut eng = engine();
    setup_4way(&mut eng);
    match run(
        &mut eng,
        "SELECT count(*) FROM f1, f2, f3, f4 WHERE f1.a = 1",
    )
    .expect("runs")
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0].to_text().as_deref(), Some("64"));
        }
        other => panic!("expected Select, got {:?}", other),
    }
    let mut eng = engine();
    setup_5way(&mut eng);
    match run(
        &mut eng,
        "SELECT count(*) FROM g1, g2, g3, g4, g5 WHERE g3.a = 2",
    )
    .expect("runs")
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            // 4^4 * 1 (g3 filtered to one row)
            assert_eq!(rows[0][0].to_text().as_deref(), Some("256"));
        }
        other => panic!("expected Select, got {:?}", other),
    }
}
