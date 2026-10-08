// ============================================================================
// v1.64: cost-based hash-vs-nestloop choice (PG19 `cost_hashjoin` vs
// `cost_nestloop` parity) + Hash Join EXPLAIN rendering + USING quals.
//
// Soundness: the `HashJoin` plan node is display-only — the executor
// runs its own (v0.93) hash-join detection. These tests prove the plan
// shapes and the result identity (NULL keys never match, duplicate keys
// fan out) that the choice rule depends on.
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

/// Result rows as sorted strings for identity comparison.
fn query_sorted(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).expect("runs") {
        ExecResult::Select { rows, .. } => {
            let mut v: Vec<String> = rows
                .into_iter()
                .map(|r| {
                    r.iter()
                        .map(|c| c.to_text().unwrap_or("NULL".to_string()))
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("expected Select, got {:?}", other),
    }
}

fn setup_hash(eng: &mut Engine) {
    // Two tables with duplicate and NULL keys for identity tests.
    run(eng, "CREATE TABLE h1 (id int, v text)").unwrap();
    run(eng, "CREATE TABLE h2 (id int, w text)").unwrap();
    run(
        eng,
        "INSERT INTO h1 VALUES (1, 'a'), (2, 'b'), (2, 'b2'), (NULL, 'n'), (3, 'c')",
    )
    .unwrap();
    run(
        eng,
        "INSERT INTO h2 VALUES (2, 'x'), (2, 'y'), (NULL, 'z'), (4, 'w'), (1, 'q')",
    )
    .unwrap();
    // Larger tables where hash provably beats nestloop.
    run(eng, "CREATE TABLE big1 (id int, v int)").unwrap();
    run(eng, "CREATE TABLE big2 (id int, w int)").unwrap();
    let mut vals1 = Vec::new();
    let mut vals2 = Vec::new();
    for i in 0..500 {
        vals1.push(format!("({}, {})", i, i * 2));
        vals2.push(format!("({}, {})", i % 250, i));
    }
    run(eng, &format!("INSERT INTO big1 VALUES {}", vals1.join(","))).unwrap();
    run(eng, &format!("INSERT INTO big2 VALUES {}", vals2.join(","))).unwrap();
}

#[test]
fn v164_hash_join_result_identity() {
    // The executor's hash join must return exactly the rows a nested
    // loop would: NULL keys never match, duplicate keys fan out.
    let mut eng = engine();
    setup_hash(&mut eng);
    let hash_rows = query_sorted(
        &mut eng,
        "SELECT h1.v, h2.w FROM h1 JOIN h2 ON h1.id = h2.id ORDER BY 1, 2",
    );
    // Expected by hand: (1,a)x(1,q); (2,b)x(2,x),(2,y); (2,b2)x(2,x),(2,y).
    // NULLs never match; 3 and 4 have no partner.
    assert_eq!(
        hash_rows,
        vec!["a|q", "b2|x", "b2|y", "b|x", "b|y"],
        "hash join rows must match nested-loop semantics"
    );
}

#[test]
fn v164_using_join_result_identity() {
    // USING is an inner equi-join; result identity vs the ON form.
    let mut eng = engine();
    setup_hash(&mut eng);
    let using_rows = query_sorted(
        &mut eng,
        "SELECT v, w FROM h1 JOIN h2 USING (id) ORDER BY 1, 2",
    );
    let on_rows = query_sorted(
        &mut eng,
        "SELECT h1.v, h2.w FROM h1 JOIN h2 ON h1.id = h2.id ORDER BY 1, 2",
    );
    assert_eq!(using_rows, on_rows, "USING must match ON results");
}

#[test]
fn v164_large_join_chooses_hash() {
    // 500x500 equi-join: hash provably cheaper than nestloop.
    let mut eng = engine();
    setup_hash(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM big1 JOIN big2 ON big1.id = big2.id",
    );
    assert!(
        lines.iter().any(|l| l.trim() == "Hash Join"),
        "large join must use Hash Join, got: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.trim() == "Hash Cond: (big1.id = big2.id)"),
        "Hash Cond must be outer-first, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.trim() == "->  Hash"),
        "build side needs a Hash wrapper, got: {lines:?}"
    );
}

#[test]
fn v164_tiny_join_stays_nestloop() {
    // 5x5 equi-join: nestloop wins (fuzz tie-break fails closed).
    let mut eng = engine();
    setup_hash(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM h1 JOIN h2 ON h1.id = h2.id",
    );
    assert!(
        lines.iter().any(|l| l.trim() == "Nested Loop"),
        "tiny join must stay Nested Loop, got: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("Hash Join")),
        "tiny join must not use Hash Join, got: {lines:?}"
    );
}

#[test]
fn v164_using_plans_hash_cond() {
    // USING builds the equi qual; large USING join plans a Hash Join.
    let mut eng = engine();
    setup_hash(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM big1 JOIN big2 USING (id)",
    );
    assert!(
        lines.iter().any(|l| l.trim() == "Hash Join"),
        "large USING join must use Hash Join, got: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.trim() == "Hash Cond: (big1.id = big2.id)"),
        "USING Hash Cond must be outer-first, got: {lines:?}"
    );
}

#[test]
fn v164_hash_cond_outer_first_on_swap() {
    // v1.75: without ANALYZE stats the v1.64 empirical rule is kept
    // (the stats-driven cost model cannot reproduce PG19's
    // stats-driven bucket fractions from a 0.1 punt). When the
    // filtered side is on the right, it probes (outer) and the Hash
    // Cond leads with its column.
    let mut eng = engine();
    setup_hash(&mut eng);
    let lines = plan_raw(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM big1 JOIN big2 ON big1.id = big2.id WHERE big2.w < 10",
    );
    let hc = lines
        .iter()
        .find(|l| l.contains("Hash Cond:"))
        .expect("Hash Cond line");
    assert!(
        hc.trim() == "Hash Cond: (big2.id = big1.id)",
        "filtered right side must probe first without stats, got: {hc}"
    );
}

#[test]
fn v164_cost_helpers_sanity() {
    // pg_cost_hashjoin < pg_cost_nestloop for the large shape (the
    // raw cost functions; the 10-row minimum inner threshold lives
    // in `pg_hashjoin_cost_order` and is covered by
    // v164_tiny_join_stays_nestloop).
    let nl_large = pg_cost_nestloop(500.0, 510.0, 500.0, 510.0, 0.0025);
    let hj_large = pg_cost_hashjoin(500.0, 510.0, 500.0, 510.0, 1.0, 0.1, 1250.0);
    assert!(
        hj_large * PG_STD_FUZZ_FACTOR < nl_large,
        "hash must win large: {hj_large} vs {nl_large}"
    );
    assert_eq!(PG_HASHJOIN_MIN_INNER_ROWS, 10.0);
}
