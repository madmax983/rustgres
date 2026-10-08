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

/// v1.62: seed the table past one heap page so the planner
/// legitimately keeps the index scan — PG19's cost model never
/// index-scans a single-page relation, so these rendering tests
/// need a multi-page table to exercise the IndexScan path.
fn seed_multi_page(eng: &mut Engine, table: &str, vals: impl Fn(u32) -> String) {
    for i in 1..=400 {
        run(eng, &format!("INSERT INTO {table} VALUES {}", vals(i))).unwrap();
    }
}

#[test]
fn index_cond_single_parens() {
    // v1.08: single index condition renders one paren pair: `(a = 42)`.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int, b text)").unwrap();
    run(&mut eng, "CREATE INDEX i1 ON t1 (a)").unwrap();
    seed_multi_page(&mut eng, "t1", |i| format!("({i}, 'x')"));
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a = 42",
    );
    assert_eq!(plan[0], "Index Scan using i1 on t1");
    assert_eq!(plan[1], "  Index Cond: (a = 42)");
}

#[test]
fn index_cond_multi_source_order() {
    // v1.08: multiple index conds render in source order with AND.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    run(&mut eng, "CREATE INDEX i1 ON t1 (a)").unwrap();
    seed_multi_page(&mut eng, "t1", |i| format!("({i})"));
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a > 10 AND a < 100",
    );
    assert_eq!(plan[1], "  Index Cond: ((a > 10) AND (a < 100))");
}

#[test]
fn residual_filter_text_coercion() {
    // v1.08: residual filter gets planner-coerced type label.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int, b text)").unwrap();
    run(&mut eng, "CREATE INDEX i1 ON t1 (a)").unwrap();
    seed_multi_page(&mut eng, "t1", |i| format!("({i}, 'hello')"));
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a = 42 AND b = 'hello'",
    );
    assert_eq!(plan[1], "  Index Cond: (a = 42)");
    assert_eq!(plan[2], "  Filter: (b = 'hello'::text)");
}

#[test]
fn alias_rendered_on_scan() {
    // v1.08: table alias renders on scan lines.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    run(&mut eng, "CREATE INDEX i1 ON t1 (a)").unwrap();
    seed_multi_page(&mut eng, "t1", |i| format!("({i})"));
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 t WHERE t.a = 1",
    );
    assert_eq!(plan[0], "Index Scan using i1 on t1 t");
    assert_eq!(plan[1], "  Index Cond: (a = 1)");
}

#[test]
fn where_distribution_join_filter() {
    // v1.08: source-local predicates stay on scans; multi-source
    // predicates become the join's Join Filter.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    run(&mut eng, "CREATE TABLE t2 (x int)").unwrap();
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 a, t2 b WHERE a.a = b.x AND a.a > 0",
    );
    assert_eq!(plan[0], "Nested Loop");
    assert_eq!(plan[1], "  Join Filter: (a.a = b.x)");
    assert_eq!(plan[2], "  ->  Seq Scan on t1 a");
    assert_eq!(plan[3], "        Filter: (a > 0)");
    assert_eq!(plan[4], "  ->  Seq Scan on t2 b");
}

#[test]
fn nested_indentation_exact() {
    // v1.08: PG19 indentation — depth 0 none, depth 1 `  ->  `,
    // properties at 2/8 spaces.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    run(&mut eng, "CREATE TABLE t2 (x int)").unwrap();
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 a JOIN t2 b ON a.a = b.x WHERE a.a > 0",
    );
    // Verify exact spacing with visible markers.
    assert!(plan[0].starts_with("Nested Loop"));
    assert!(plan[1].starts_with("  Join Filter:"));
    assert!(plan[2].starts_with("  ->  Seq Scan"));
    assert!(plan[3].starts_with("        Filter:"));
}

#[test]
fn sort_key_expression() {
    // v1.08: sort keys render the PG-text expression (qualified, per
    // PG corpus `Sort Key: ((t2.q1 + 1))`).
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 ORDER BY a + 1",
    );
    assert_eq!(plan[0], "Sort");
    assert_eq!(plan[1], "  Sort Key: (t1.a + 1)");
    assert_eq!(plan[2], "  ->  Seq Scan on t1");
}

#[test]
fn limit_no_count() {
    // v1.08: COSTS OFF renders bare `Limit`, not `Limit N`.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    let plan = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM t1 LIMIT 5");
    assert_eq!(plan[0], "Limit");
    assert_eq!(plan[1], "  ->  Seq Scan on t1");
}

#[test]
fn comparison_coercion_both_directions() {
    // v1.08: literal gets the column's type label regardless of side.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (b text)").unwrap();
    let plan = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE 'x' = b",
    );
    assert_eq!(plan[1], "  Filter: ('x'::text = b)");
}
