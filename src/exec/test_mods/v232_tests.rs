use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).expect("parses");
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        privs_suppressed: false,
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
            .collect(),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn seed_lp(eng: &mut Engine) {
    run(eng, "CREATE TABLE lp (a char) PARTITION BY LIST (a)").unwrap();
    run(
        eng,
        "CREATE TABLE lp_ad PARTITION OF lp FOR VALUES IN ('a', 'd')",
    )
    .unwrap();
    run(
        eng,
        "CREATE TABLE lp_bc PARTITION OF lp FOR VALUES IN ('b', 'c')",
    )
    .unwrap();
    run(
        eng,
        "CREATE TABLE lp_ef PARTITION OF lp FOR VALUES IN ('e', 'f')",
    )
    .unwrap();
    run(eng, "CREATE TABLE lp_g PARTITION OF lp FOR VALUES IN ('g')").unwrap();
    run(
        eng,
        "CREATE TABLE lp_null PARTITION OF lp FOR VALUES IN (NULL)",
    )
    .unwrap();
    run(eng, "CREATE TABLE lp_default PARTITION OF lp DEFAULT").unwrap();
}

#[test]
fn v232_list_append_all_leaves() {
    let mut eng = engine();
    seed_lp(&mut eng);
    let plan = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM lp");
    assert_eq!(
        plan,
        vec![
            "Append",
            "  ->  Seq Scan on lp_ad lp_1",
            "  ->  Seq Scan on lp_bc lp_2",
            "  ->  Seq Scan on lp_ef lp_3",
            "  ->  Seq Scan on lp_g lp_4",
            "  ->  Seq Scan on lp_null lp_5",
            "  ->  Seq Scan on lp_default lp_6",
        ]
    );
}

#[test]
fn v232_list_range_clause_prunes_and_keeps_default() {
    let mut eng = engine();
    seed_lp(&mut eng);
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM lp WHERE a > 'a' AND a < 'd'",
    );
    assert_eq!(plan[0], "Append");
    assert_eq!(plan[1], "  ->  Seq Scan on lp_bc lp_1");
    assert_eq!(plan[3], "  ->  Seq Scan on lp_default lp_2");
    assert_eq!(plan.len(), 5);
}

#[test]
fn v232_list_single_survivor_bare_scan_parent_alias() {
    let mut eng = engine();
    seed_lp(&mut eng);
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM lp WHERE a = 'a'",
    );
    assert_eq!(
        plan,
        vec!["Seq Scan on lp_ad lp", "  Filter: (a = 'a'::bpchar)"]
    );
}

#[test]
fn v232_range_pruning() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE rlp (a int, b varchar) PARTITION BY RANGE (a)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE rlp1 PARTITION OF rlp FOR VALUES FROM (0) TO (10)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE rlp2 PARTITION OF rlp FOR VALUES FROM (10) TO (20)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE rlp3 PARTITION OF rlp FOR VALUES FROM (20) TO (30)",
    )
    .unwrap();
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM rlp WHERE a <= 10",
    );
    assert_eq!(plan[0], "Append");
    assert!(
        plan.iter().any(|l| l.contains("Seq Scan on rlp1 rlp_1")),
        "{plan:?}"
    );
    assert!(
        plan.iter().any(|l| l.contains("Seq Scan on rlp2 rlp_2")),
        "{plan:?}"
    );
    assert!(!plan.iter().any(|l| l.contains("rlp3")), "{plan:?}");
    // Equality pins exactly one leaf.
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM rlp WHERE a = 25",
    );
    assert_eq!(plan[0], "Seq Scan on rlp3 rlp");
}

#[test]
fn v232_contradiction_zero_survivors() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE rlp (a int, b varchar) PARTITION BY RANGE (a)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE rlp1 PARTITION OF rlp FOR VALUES FROM (0) TO (10)",
    )
    .unwrap();
    // A range-op contradiction empties the region: dummy Result.
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM rlp WHERE a > 5 AND a < 2",
    );
    assert_eq!(
        plan,
        vec![
            "Result",
            "  Replaces: Scan on rlp",
            "  One-Time Filter: false"
        ]
    );
    // A whole-qual `a = 1 AND a = 3` is refuted by PG19's
    // predicate_refuted_by: dummy Result.
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM rlp WHERE a = 1 AND a = 3",
    );
    assert_eq!(
        plan,
        vec![
            "Result",
            "  Replaces: Scan on rlp",
            "  One-Time Filter: false"
        ]
    );
    // But inside one OR arm, equality steps do NOT intersect (the
    // arm still contributes its leaf); the other arm adds its own.
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM rlp WHERE (a = 1 AND a = 3) OR a = 25",
    );
    assert!(
        plan.iter().any(|l| l.contains("Seq Scan on rlp1")),
        "{plan:?}"
    );
}

#[test]
fn v232_is_null_keeps_only_null_partition() {
    let mut eng = engine();
    seed_lp(&mut eng);
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM lp WHERE a IS NULL",
    );
    assert_eq!(plan[0], "Seq Scan on lp_null lp");
}

#[test]
fn v232_negative_list_bound_literals() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE part (a int) PARTITION BY LIST (a)").unwrap();
    run(
        &mut eng,
        "CREATE TABLE part_p1 PARTITION OF part FOR VALUES IN (-2, -1, 0, 1, 2)",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO part VALUES (-1)").unwrap();
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM part WHERE a = -1",
    );
    assert_eq!(plan[0], "Seq Scan on part_p1 part");
}

#[test]
fn v232_or_union_pruning() {
    let mut eng = engine();
    seed_lp(&mut eng);
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM lp WHERE a = 'a' OR a = 'g'",
    );
    assert_eq!(plan[0], "Append");
    assert!(plan.iter().any(|l| l.contains("lp_ad lp_1")), "{plan:?}");
    assert!(plan.iter().any(|l| l.contains("lp_g lp_2")), "{plan:?}");
    assert_eq!(
        plan.iter().filter(|l| l.contains("Seq Scan")).count(),
        2,
        "{plan:?}"
    );
}

#[test]
fn v232_iboolpart_not_key() {
    let mut e = engine();
    run(
        &mut e,
        "create table iboolpart (a bool) partition by list ((not a))",
    )
    .unwrap();
    run(
        &mut e,
        "create table iboolpart_default partition of iboolpart default",
    )
    .unwrap();
    run(
        &mut e,
        "create table iboolpart_f partition of iboolpart for values in ('true')",
    )
    .unwrap();
    run(
        &mut e,
        "create table iboolpart_t partition of iboolpart for values in ('false')",
    )
    .unwrap();
    // `a = false` pins the (not a) key to TRUE: iboolpart_f alone,
    // deparsed in PG19's simplified boolean form.
    let p = plan_lines(
        &mut e,
        "explain (costs off) select * from iboolpart where a = false",
    );
    assert_eq!(
        p,
        vec![
            "Seq Scan on iboolpart_f iboolpart".to_string(),
            "  Filter: (NOT a)".to_string(),
        ]
    );
    // BooleanTest matching is structural in PG19: `a IS NOT TRUE`
    // does not match the `(not a)` key, so nothing is pruned.
    let p = plan_lines(
        &mut e,
        "explain (costs off) select * from iboolpart where a is not true",
    );
    assert!(p.iter().any(|l| l.contains("iboolpart_f")), "{p:?}");
    assert!(p.iter().any(|l| l.contains("iboolpart_default")), "{p:?}");
    assert!(p.iter().any(|l| l.contains("iboolpart_t")), "{p:?}");
}

#[test]
fn v232_hash_opclass_routing() {
    let mut e = engine();
    run(&mut e, "create function part_hashint4_noop(value int, seed bigint) returns bigint as $$ select value + seed; $$ language sql strict immutable parallel safe").unwrap();
    run(&mut e, "create operator class part_test_int4_ops for type int using hash as operator 1 =, function 2 part_hashint4_noop(int, bigint)").unwrap();
    run(&mut e, "create function part_hashtext_length(value text, seed bigint) returns bigint as $$ select length(coalesce(value, ''))::bigint $$ language sql strict immutable parallel safe").unwrap();
    run(&mut e, "create operator class part_test_text_ops for type text using hash as operator 1 =, function 2 part_hashtext_length(text, bigint)").unwrap();
    run(&mut e, "create table hp (a int, b text, c int) partition by hash (a part_test_int4_ops, b part_test_text_ops)").unwrap();
    for i in 0..4 {
        run(
            &mut e,
            &format!(
                "create table hp{i} partition of hp for values with (modulus 4, remainder {i})"
            ),
        )
        .unwrap();
    }
    // PG19: (1 + seed) combined alone (NULL parts are skipped)
    // routes to hp1.
    let p = plan_lines(
        &mut e,
        "explain (costs off) select * from hp where a = 1 and b is null",
    );
    assert_eq!(p[0], "Seq Scan on hp1 hp");
}
