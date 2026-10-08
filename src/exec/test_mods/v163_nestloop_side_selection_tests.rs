// ============================================================================
// v1.63: cost-based nestloop side selection (PG19 `cost_nestloop` parity).
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

/// Raw (indented) EXPLAIN lines, so child order is observable.
fn plan_raw(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .collect(),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn setup_sj(eng: &mut Engine) {
    run(eng, "CREATE TABLE sj (a int unique, b int, c int unique)").unwrap();
    run(
        eng,
        "INSERT INTO sj VALUES (1, null, 2), (null, 2, null), (2, 1, 1), (3, 1, 3)",
    )
    .unwrap();
}

/// Index of the first line containing `pat` (trimmed compare).
fn find_line(lines: &[String], pat: &str) -> usize {
    lines
        .iter()
        .position(|l| l.trim() == pat)
        .unwrap_or_else(|| panic!("missing line {pat:?} in {lines:?}"))
}

#[test]
fn v163_t3_selective_side_outer() {
    // PG19 puts the filtered (smaller) side outer: `2 = j2.a`
    // selects ~1 of 4 rows, so j2 wins the outer slot (cost_nestloop:
    // the inner scan is re-run per outer row).
    let mut eng = engine();
    setup_sj(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b AND 2 = j2.a",
    );
    let outer = find_line(&lines, "->  Seq Scan on sj j2");
    let inner = find_line(&lines, "->  Seq Scan on sj j1");
    assert!(outer < inner, "j2 must be outer, got: {lines:?}");
    assert!(lines.iter().any(|l| l.trim() == "Filter: (2 = a)"));
    assert!(
        lines
            .iter()
            .any(|l| l.trim() == "Join Filter: (j1.b = j2.b)")
    );
}

#[test]
fn v163_t4_selective_side_outer() {
    let mut eng = engine();
    setup_sj(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT n2.a FROM sj n1, sj n2 WHERE n1.a <> n2.a AND n2.a = 1",
    );
    let outer = find_line(&lines, "->  Seq Scan on sj n2");
    let inner = find_line(&lines, "->  Seq Scan on sj n1");
    assert!(outer < inner, "n2 must be outer, got: {lines:?}");
    assert!(lines.iter().any(|l| l.trim() == "Filter: (a = 1)"));
}

#[test]
fn v163_explicit_inner_join_swaps() {
    // Site 2: explicit JOIN ... ON syntax gets the same treatment.
    let mut eng = engine();
    setup_sj(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM sj j1 INNER JOIN sj j2 ON j1.b = j2.b WHERE 2 = j2.a",
    );
    let outer = find_line(&lines, "->  Seq Scan on sj j2");
    let inner = find_line(&lines, "->  Seq Scan on sj j1");
    assert!(outer < inner, "j2 must be outer, got: {lines:?}");
}

#[test]
fn v163_symmetric_keeps_from_order() {
    // Tie: both sides unfiltered, same cost — strict `<` keeps FROM
    // order (this is also PG's tie behavior: the first-added path wins).
    let mut eng = engine();
    setup_sj(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b",
    );
    let j1 = find_line(&lines, "->  Seq Scan on sj j1");
    let j2 = find_line(&lines, "->  Seq Scan on sj j2");
    assert!(j1 < j2, "FROM order must be kept on a tie, got: {lines:?}");
}

#[test]
fn v163_already_selective_outer_no_swap() {
    // The selective side is already outer: swapping would lose.
    let mut eng = engine();
    setup_sj(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b AND j1.a = 2",
    );
    let j1 = find_line(&lines, "->  Seq Scan on sj j1");
    let j2 = find_line(&lines, "->  Seq Scan on sj j2");
    assert!(j1 < j2, "selective j1 must stay outer, got: {lines:?}");
    assert!(lines.iter().any(|l| l.trim() == "Filter: (a = 2)"));
}

#[test]
fn v163_left_join_keeps_written_order() {
    // PG19 only tries both orders for JOIN_INNER (joinrels.c); outer
    // joins keep their written order even when the inner side is
    // more selective.
    let mut eng = engine();
    setup_sj(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM sj j1 LEFT JOIN sj j2 ON j1.b = j2.b WHERE 2 = j2.a",
    );
    assert!(
        lines.iter().any(|l| l.contains("Nested Loop Left Join")),
        "must stay a left join, got: {lines:?}"
    );
    let j1 = find_line(&lines, "->  Seq Scan on sj j1");
    let j2 = find_line(&lines, "->  Seq Scan on sj j2");
    assert!(j1 < j2, "left join must keep written order, got: {lines:?}");
}

#[test]
fn v163_subquery_side_no_swap() {
    // Fail closed: a non-scan side is unestimable, so FROM order is
    // kept even when the subquery side is tiny.
    let mut eng = engine();
    setup_sj(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM sj j1, (SELECT 1 AS x) q WHERE j1.a = 1",
    );
    let j1 = find_line(&lines, "->  Seq Scan on sj j1");
    let q = lines
        .iter()
        .position(|l| l.contains("Subquery Scan"))
        .expect("subquery scan");
    assert!(j1 < q, "sj must stay outer, got: {lines:?}");
}

#[test]
fn v163_results_identical_after_swap() {
    // Soundness: the swap only reorders EXPLAIN plan children (the
    // executor never sees PlanNode), so results are unchanged. Run
    // the t3/t4 queries for real and check the rows.
    let mut eng = engine();
    setup_sj(&mut eng);
    let rows_of = |eng: &mut Engine, sql: &str| -> Vec<Vec<String>> {
        match run(eng, sql).expect("runs") {
            ExecResult::Select { rows, .. } => rows
                .into_iter()
                .map(|r| {
                    r.into_iter()
                        .map(|c| c.to_text().unwrap_or("NULL".to_string()))
                        .collect()
                })
                .collect(),
            other => panic!("expected Select, got {:?}", other),
        }
    };
    // t3: j1.b = j2.b AND 2 = j2.a. sj rows: (1,null,2),
    // (null,2,null), (2,1,1), (3,1,3). j2.a = 2 -> j2 = (2,1,1);
    // j1.b = 1 -> j1 = (2,1,1) and (3,1,3).
    let mut t3 = rows_of(
        &mut eng,
        "SELECT j1.a, j2.a FROM sj j1, sj j2 WHERE j1.b = j2.b AND 2 = j2.a ORDER BY 1, 2",
    );
    t3.sort();
    assert_eq!(
        t3,
        vec![
            vec!["2".to_string(), "2".to_string()],
            vec!["3".to_string(), "2".to_string()]
        ]
    );
    // t4: n1.a <> n2.a AND n2.a = 1 -> n2 = (1,null,2); n1.a <>
    // 1 -> n1 = (2,1,1), (3,1,3) (the null-a row fails the <>).
    let mut t4 = rows_of(
        &mut eng,
        "SELECT n2.a FROM sj n1, sj n2 WHERE n1.a <> n2.a AND n2.a = 1 ORDER BY 1",
    );
    t4.sort();
    assert_eq!(t4.len(), 2);
    assert!(t4.iter().all(|r| r == &vec!["1".to_string()]));
}

#[test]
fn v163_conjunct_selectivity_shapes() {
    // Unit-cover the selectivity shapes: Eq/Ne/range get opinions,
    // unestimable shapes fail closed to 1.0.
    let mut eng = engine();
    setup_sj(&mut eng);
    let snap = eng.take_snapshot();
    let db = &eng.db;
    // No ANALYZE has run: est_eq_sel's no-stats default (0.1).
    let col_a = Expr::Column {
        table: Some("j2".to_string()),
        name: "a".to_string(),
    };
    let lit2 = Expr::Literal(Literal::Int(2));
    let eq = Expr::Cmp {
        op: CmpOp::Eq,
        left: Box::new(lit2.clone()),
        right: Box::new(col_a.clone()),
    };
    let s = est_conjunct_sel(
        db,
        "sj",
        "j2",
        &db.find_table("sj", &snap, &[9], 0).unwrap().columns,
        &eq,
    );
    assert!((s - 0.1).abs() < 1e-9, "eq selectivity, got {s}");
    let ne = Expr::Cmp {
        op: CmpOp::Ne,
        left: Box::new(lit2.clone()),
        right: Box::new(col_a.clone()),
    };
    let s = est_conjunct_sel(
        db,
        "sj",
        "j2",
        &db.find_table("sj", &snap, &[9], 0).unwrap().columns,
        &ne,
    );
    assert!((s - 0.9).abs() < 1e-9, "ne selectivity, got {s}");
    let lt = Expr::Cmp {
        op: CmpOp::Lt,
        left: Box::new(col_a.clone()),
        right: Box::new(lit2.clone()),
    };
    let s = est_conjunct_sel(
        db,
        "sj",
        "j2",
        &db.find_table("sj", &snap, &[9], 0).unwrap().columns,
        &lt,
    );
    assert!((s - 0.1).abs() < 1e-9, "range selectivity, got {s}");
    // Column-vs-column: unestimable -> 1.0.
    let col_b = Expr::Column {
        table: Some("j2".to_string()),
        name: "b".to_string(),
    };
    let cmp = Expr::Cmp {
        op: CmpOp::Eq,
        left: Box::new(col_a.clone()),
        right: Box::new(col_b),
    };
    let s = est_conjunct_sel(
        db,
        "sj",
        "j2",
        &db.find_table("sj", &snap, &[9], 0).unwrap().columns,
        &cmp,
    );
    assert!((s - 1.0).abs() < 1e-9, "unestimable must be 1.0, got {s}");
    // IS NULL without stats: no opinion -> 1.0.
    let isn = Expr::IsNull {
        expr: Box::new(col_a),
        neg: false,
    };
    let s = est_conjunct_sel(
        db,
        "sj",
        "j2",
        &db.find_table("sj", &snap, &[9], 0).unwrap().columns,
        &isn,
    );
    assert!(
        (s - 1.0).abs() < 1e-9,
        "IS NULL without stats must be 1.0, got {s}"
    );
}

#[test]
fn v163_t5_inner_pair_selective_outer() {
    // t5's inner pair gets the pairwise PG-correct order (n2 outer).
    // v1.65: the full 3-way reorder now builds ((sl⋈n2)⋈n1); n2's
    // scan still precedes n1's, so this assertion holds.
    let mut eng = engine();
    setup_sj(&mut eng);
    run(&mut eng, "CREATE TABLE sl (a int, b int, c int)").unwrap();
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM (SELECT n2.a FROM sj n1, sj n2 WHERE n1.a <> n2.a) q0, sl WHERE q0.a = 1",
    );
    // Inner nested loop: n2 (filtered) outer, n1 inner.
    let n2 = lines
        .iter()
        .position(|l| l.contains("Seq Scan on sj n2"))
        .expect("n2 scan");
    let n1 = lines
        .iter()
        .position(|l| l.contains("Seq Scan on sj n1"))
        .expect("n1 scan");
    assert!(n2 < n1, "n2 must be the inner loop's outer, got: {lines:?}");
}
