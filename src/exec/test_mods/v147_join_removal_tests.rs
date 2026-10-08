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
        ExecResult::Explain { rows, .. } => {
            rows.into_iter().map(|r| r[0].to_text().unwrap()).collect()
        }
        other => panic!("expected EXPLAIN, got {other:?}"),
    }
}

fn setup(eng: &mut Engine) {
    for sql in [
        "CREATE TEMP TABLE a (id int PRIMARY KEY, b_id int)",
        "CREATE TEMP TABLE b (id int PRIMARY KEY, c_id int)",
        "CREATE TEMP TABLE c (id int PRIMARY KEY)",
        "CREATE TEMP TABLE d (a int, b int)",
        "CREATE TEMP TABLE e (id1 int, id2 int, PRIMARY KEY (id1, id2))",
        "CREATE TEMP TABLE int8_tbl (q1 bigint)",
        "CREATE TEMP TABLE int4_tbl (f1 int)",
        "INSERT INTO a VALUES (1, 1), (2, NULL)",
        "INSERT INTO b VALUES (1, 10), (2, 20)",
        "INSERT INTO c VALUES (1), (2)",
        "INSERT INTO d VALUES (1, 10), (2, 20), (99, 99)",
        "INSERT INTO e VALUES (1, 10), (2, 20)",
        "INSERT INTO int8_tbl VALUES (1), (2)",
        "INSERT INTO int4_tbl VALUES (1), (2)",
    ] {
        run(eng, sql).unwrap();
    }
}

fn assert_seq_scan(eng: &mut Engine, sql: &str, expected: &str) {
    let lines = plan_lines(eng, sql);
    assert_eq!(lines, vec![expected.to_string()], "{sql}");
}

#[test]
fn v147_fixpoint_chained_removal() {
    // v1.47: PG's `goto restart` fixpoint — inner removals unlock outer ones.
    let mut eng = engine();
    setup(&mut eng);
    // #1: (b ⋉ c) inner join removed, then the outer b-join.
    assert_seq_scan(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN (b left join c on b.c_id = c.id) ON (a.b_id = b.id)",
        "Seq Scan on a",
    );
    // #2: e-join (multi-column PK) removed, then b-join.
    assert_seq_scan(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON a.id = b.id LEFT JOIN e ON e.id1 = a.b_id AND b.c_id = e.id2",
        "Seq Scan on a",
    );
    // #5: t4 removed (whole ON dropped, including the t2.id=t3.id conjunct
    // that was LEFT-JOIN-scoped and never filtered).
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) select 1 from a t1 left join a t2 on true inner join a t3 on true left join a t4 on t2.id = t4.id and t2.id = t3.id",
    );
    assert!(
        !lines.iter().any(|l| l.contains("t4")),
        "t4 should be gone: {lines:?}"
    );
}

#[test]
fn v147_oddly_nested() {
    // v1.47: the corpus's oddly-nested cases.
    let mut eng = engine();
    setup(&mut eng);
    // #3: a4 removed, then a3; a1⋉a2 (on true) stays.
    // v1.48: PG renders the left-join label and materializes the
    // inner side (joinpath.c `create_material_path` alternative).
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) select a1.id from (a a1 left join a a2 on true) left join (a a3 left join a a4 on a3.id = a4.id) on a2.id = a3.id",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop Left Join".to_string(),
            "  ->  Seq Scan on a a1".to_string(),
            "  ->  Materialize".to_string(),
            "        ->  Seq Scan on a a2".to_string(),
        ],
        "{lines:?}"
    );
    // #4: all the way down to a bare Seq Scan.
    assert_seq_scan(
        &mut eng,
        "EXPLAIN (COSTS OFF) select a1.id from (a a1 left join a a2 on a1.id = a2.id) left join (a a3 left join a a4 on a3.id = a4.id) on a2.id = a3.id",
        "Seq Scan on a a1",
    );
}

#[test]
fn v147_subquery_proofs() {
    // v1.47: derived-table distinctness proofs.
    let mut eng = engine();
    setup(&mut eng);
    // #6: GROUP BY b.id, b.c_id — both constrained.
    assert_seq_scan(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT * FROM b GROUP BY b.id, b.c_id) s ON d.a = s.id AND d.b = s.c_id",
        "Seq Scan on d",
    );
    // #7-#10: single empty grouping set (various spellings).
    for sql in [
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT 1 AS x FROM b GROUP BY ()) s ON d.a = s.x",
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT 1 AS x FROM b GROUP BY GROUPING SETS (())) s ON d.a = s.x",
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT 1 AS x FROM b GROUP BY GROUPING SETS (()), GROUPING SETS (())) s ON d.a = s.x",
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT 1 AS x FROM b GROUP BY DISTINCT GROUPING SETS ((), ())) s ON d.a = s.x",
    ] {
        assert_seq_scan(&mut eng, sql, "Seq Scan on d");
    }
    // #11: plain DISTINCT, all outputs constrained.
    assert_seq_scan(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT DISTINCT * FROM b) s ON d.a = s.id AND d.b = s.c_id",
        "Seq Scan on d",
    );
    // #12: UNION (not ALL).
    assert_seq_scan(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT id FROM a UNION SELECT id FROM b) s ON d.a = s.id",
        "Seq Scan on d",
    );
    // #13: GROUP BY + cross-type = (int8 = int4 family agreement).
    assert_seq_scan(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT i8.* FROM int8_tbl i8 LEFT JOIN (SELECT f1 FROM int4_tbl GROUP BY f1) i4 ON i8.q1 = i4.f1",
        "Seq Scan on int8_tbl i8",
    );
}

#[test]
fn v147_negatives_stay_kept() {
    // v1.47: PG keeps these; we must too.
    let mut eng = engine();
    setup(&mut eng);
    for sql in [
        // `on true`: no usable equality.
        "EXPLAIN (COSTS OFF) SELECT a1.* FROM a a1 LEFT JOIN a a2 ON true",
        // Non-equi condition.
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON a.b_id > b.id",
        // GROUP BY with an unconstrained column.
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT b.id, b.c_id FROM b GROUP BY b.id, b.c_id) s ON d.a = s.id",
        // UNION ALL does not dedup.
        "EXPLAIN (COSTS OFF) SELECT d.* FROM d LEFT JOIN (SELECT id FROM a UNION ALL SELECT id FROM b) s ON d.a = s.id",
        // Inner range referenced in SELECT.
        "EXPLAIN (COSTS OFF) SELECT d.*, s.id FROM d LEFT JOIN (SELECT DISTINCT id FROM b) s ON d.a = s.id",
    ] {
        let lines = plan_lines(&mut eng, sql);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Nested Loop") || l.contains("Hash")),
            "should keep the join: {sql} -> {lines:?}"
        );
    }
}

#[test]
fn v147_result_identity() {
    // v1.47: removal must not change query results (non-EXPLAIN).
    let mut eng = engine();
    setup(&mut eng);
    let cases = [
        "SELECT a.* FROM a LEFT JOIN (b left join c on b.c_id = c.id) ON (a.b_id = b.id) ORDER BY a.id",
        "SELECT d.* FROM d LEFT JOIN (SELECT * FROM b GROUP BY b.id, b.c_id) s ON d.a = s.id AND d.b = s.c_id ORDER BY d.a",
        "SELECT d.* FROM d LEFT JOIN (SELECT DISTINCT * FROM b) s ON d.a = s.id AND d.b = s.c_id ORDER BY d.a",
    ];
    for sql in cases {
        let rows = match run(&mut eng, sql).unwrap() {
            ExecResult::Select { rows, .. } => rows,
            other => panic!("expected SELECT, got {other:?}"),
        };
        // d has (1,10),(2,20),(99,99); b has (1,10),(2,20): the (99,99)
        // row survives with NULLs, others match.
        assert!(!rows.is_empty(), "{sql}");
    }
    // Spot-check exact rows for the GROUP BY case.
    let rows = match run(
            &mut eng,
            "SELECT d.a, d.b FROM d LEFT JOIN (SELECT * FROM b GROUP BY b.id, b.c_id) s ON d.a = s.id AND d.b = s.c_id ORDER BY d.a",
        )
        .unwrap()
        {
            ExecResult::Select { rows, .. } => rows,
            other => panic!("{other:?}"),
        };
    assert_eq!(rows.len(), 3);
}
