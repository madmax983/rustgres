// ============================================================================
// v1.62: tiny-table SeqScan choice (PG19 cost-model parity).
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

fn plan_lines(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .map(|l| l.trim().to_string())
            .collect(),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn heap_pages(eng: &mut Engine, table: &str) -> u64 {
    let snap = eng.take_snapshot();
    let t = eng
        .db
        .find_table(table, &snap, &[9], 0)
        .expect("table exists");
    est_heap_pages(t)
}

#[test]
fn v162_tiny_indexed_table_plans_seqscan() {
    // PG19's cost model never picks an index scan on a single-page
    // relation (costsize.c: index I/O floor is random_page_cost +
    // descent vs seq_page_cost for the seq scan).
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE sj (a int unique, b int, c int unique)",
    )
    .unwrap();
    run(
        &mut eng,
        "INSERT INTO sj VALUES (1, null, 2), (null, 2, null), (2, 1, 1), (3, 1, 3)",
    )
    .unwrap();
    assert_eq!(heap_pages(&mut eng, "sj"), 1);
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM sj WHERE a = 2");
    assert!(
        lines.iter().any(|l| l.contains("Seq Scan on sj")),
        "tiny table must Seq Scan, got: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("Index Scan")),
        "no Index Scan on a 1-page table, got: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("Filter: (a = 2)")));
}

#[test]
fn v162_empty_indexed_table_plans_seqscan() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE e (a int unique)").unwrap();
    assert_eq!(heap_pages(&mut eng, "e"), 0);
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM e WHERE a = 1");
    assert!(lines.iter().any(|l| l.contains("Seq Scan on e")));
    assert!(!lines.iter().any(|l| l.contains("Index Scan")));
}

#[test]
fn v162_multi_page_table_keeps_indexscan() {
    // The rule is conservative: past one page the index scan stays.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE big (a int unique, b text)").unwrap();
    for i in 0..400 {
        run(&mut eng, &format!("INSERT INTO big VALUES ({i}, 'x')")).unwrap();
    }
    assert!(heap_pages(&mut eng, "big") > 1);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM big WHERE a = 42",
    );
    assert!(
        lines.iter().any(|l| l.contains("Index Scan")),
        "multi-page table must keep its Index Scan, got: {lines:?}"
    );
}

#[test]
fn v162_scan_choice_does_not_change_results() {
    // Scan-type choice changes the plan, never the rows: the full
    // predicate is always re-applied by the executor.
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE sj (a int unique, b int, c int unique)",
    )
    .unwrap();
    run(
        &mut eng,
        "INSERT INTO sj VALUES (1, null, 2), (null, 2, null), (2, 1, 1), (3, 1, 3)",
    )
    .unwrap();
    let out = match run(&mut eng, "SELECT a, b, c FROM sj WHERE a = 2").expect("runs") {
        ExecResult::Select { rows, .. } => rows,
        other => panic!("expected Select, got {:?}", other),
    };
    assert_eq!(out.len(), 1);
}
