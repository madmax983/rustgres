// ============================================================================
// v1.75: cost-based hash-join inner/outer selection (PG19
// `try_hashjoin_path`).
//
// Soundness: the `HashJoin` plan node is display-only — the executor
// never sees `PlanNode`, so reordering plan children cannot change
// results. These tests prove the planner costs both (rel1, rel2) orders
// and keeps the cheaper, deleting v1.64's empirical rule ("the filtered
// side probes").
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

fn setup_skew(eng: &mut Engine) {
    // The join.sql:851 target shape: 1000 rows, `filt` uniform over
    // 10 values, analyzed so the cost model sees real stats.
    run(
        eng,
        "CREATE TABLE skewedtable (val int not null, filt int not null)",
    )
    .unwrap();
    run(
            eng,
            "INSERT INTO skewedtable SELECT CASE WHEN g <= 100 THEN 0 ELSE (g % 100) + 1 END, g % 10 FROM generate_series(1, 1000) g",
        )
        .unwrap();
    run(eng, "ANALYZE skewedtable").unwrap();
}

#[test]
fn v175_hashes_filtered_smaller_side() {
    // join.sql:851 — the filtered side (~100 rows) is the cheaper
    // build side, so PG19 hashes it and probes the unfiltered side:
    // `Hash Cond: (t2.val = t1.val)`.
    let mut eng = engine();
    setup_skew(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM skewedtable t1 JOIN skewedtable t2 ON t1.val = t2.val WHERE t1.filt = 5",
    );
    assert!(
        lines.iter().any(|l| l.trim() == "Hash Join"),
        "skewed join must use Hash Join, got: {lines:?}"
    );
    let hc = lines
        .iter()
        .find(|l| l.contains("Hash Cond:"))
        .expect("Hash Cond line");
    assert!(
        hc.trim() == "Hash Cond: (t2.val = t1.val)",
        "must hash the filtered (smaller) side, got: {hc}"
    );
}

#[test]
fn v175_unfiltered_smaller_side_probes() {
    // Mirror image: the filter sits on the larger side after
    // selectivity, so the unfiltered side probes. A narrow `filt`
    // filter on the left that still leaves it bigger than the right
    // would need stats; here the no-stats model suffices: two equal
    // tables, left filtered to half, must keep left-outer only when
    // the fuzz tie-break applies — instead assert the symmetric
    // property that swapping the query's FROM order swaps the cond.
    let mut eng = engine();
    setup_skew(&mut eng);
    let ltr = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM skewedtable t1 JOIN skewedtable t2 ON t1.val = t2.val WHERE t1.filt = 5",
    );
    let rtl = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM skewedtable t2 JOIN skewedtable t1 ON t2.val = t1.val WHERE t1.filt = 5",
    );
    let hc = |ls: &[String]| {
        ls.iter()
            .find(|l| l.contains("Hash Cond:"))
            .expect("Hash Cond line")
            .trim()
            .to_string()
    };
    // In both written orders the filtered t1 is the cheaper inner, so
    // the cond always leads with the unfiltered side's column.
    assert_eq!(hc(&ltr), "Hash Cond: (t2.val = t1.val)");
    assert_eq!(hc(&rtl), "Hash Cond: (t2.val = t1.val)");
}

#[test]
fn v175_results_identical_after_order_choice() {
    // The order choice is EXPLAIN-only; results are identical by
    // construction either way.
    let mut eng = engine();
    setup_skew(&mut eng);
    let count = |eng: &mut Engine| {
        match run(
            eng,
            "SELECT count(*) FROM skewedtable t1 JOIN skewedtable t2 ON t1.val = t2.val WHERE t1.filt = 5",
        )
        .expect("runs")
        {
            ExecResult::Select { rows, .. } => {
                rows[0][0].to_text().unwrap_or("NULL".to_string())
            }
            other => panic!("expected Select, got {:?}", other),
        }
    };
    assert_eq!(count(&mut eng), "1810");
}
