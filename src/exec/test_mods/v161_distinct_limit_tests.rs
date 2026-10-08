// ============================================================================
// v1.61: redundant-DISTINCT LIMIT 1 (`pg_distinct_keys_redundant`).
// ============================================================================
use super::*;

fn select_stmt(sql: &str) -> SelectStmt {
    match parse_statement(sql).expect("parse ok") {
        Stmt::Select(s) => s,
        _ => panic!("expected SELECT"),
    }
}

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

#[test]
fn v161_redundant_single_pinned_column() {
    assert!(pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT four FROM tenk1 WHERE four = 0"
    )));
}

#[test]
fn v161_redundant_pinned_plus_literals() {
    assert!(pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT four, 1, 2, 3 FROM tenk1 WHERE four = 0"
    )));
}

#[test]
fn v161_redundant_flipped_equality() {
    assert!(pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT four FROM tenk1 WHERE 0 = four"
    )));
}

#[test]
fn v161_redundant_with_extra_conjunct() {
    // Extra quals don't un-pin the column (PG's EC still holds).
    assert!(pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT four FROM tenk1 WHERE four = 0 AND two <> 0"
    )));
}

#[test]
fn v161_not_redundant_no_where() {
    assert!(!pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT four FROM tenk1"
    )));
}

#[test]
fn v161_not_redundant_unpinned_column() {
    assert!(!pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT hundred, two FROM tenk1"
    )));
    // `two` is not pinned: one unpinned key defeats the rule.
    assert!(!pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT four, two FROM tenk1 WHERE four = 0"
    )));
}

#[test]
fn v161_not_redundant_null_const() {
    // `= NULL` is never true; PG forms no EC from it.
    assert!(!pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT four FROM tenk1 WHERE four = NULL"
    )));
}

#[test]
fn v161_not_redundant_or_nested() {
    // Only top-level AND conjuncts pin.
    assert!(!pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT four FROM tenk1 WHERE four = 0 OR two = 1"
    )));
}

#[test]
fn v161_not_redundant_star() {
    assert!(!pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT * FROM tenk1 WHERE four = 0"
    )));
}

#[test]
fn v161_not_redundant_expr_target() {
    // Function/expression targets fail closed.
    assert!(!pg_distinct_keys_redundant(&select_stmt(
        "SELECT DISTINCT abs(four) FROM tenk1 WHERE four = 0"
    )));
}

#[test]
fn v161_explain_plans_limit_for_redundant() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1(four int)").unwrap();
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT DISTINCT four FROM t1 WHERE four = 0",
    );
    assert_eq!(lines[0], "Limit");
    assert!(lines.iter().any(|l| l.contains("Seq Scan on t1")));
}

#[test]
fn v161_explain_keeps_unique_otherwise() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1(four int)").unwrap();
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) SELECT DISTINCT four FROM t1");
    assert_eq!(lines[0], "Unique");
}

#[test]
fn v161_explain_limit_does_not_change_results() {
    // The plan shape changes; the executor still runs the real
    // DISTINCT — result identity with the Unique plan.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1(four int)").unwrap();
    run(&mut eng, "INSERT INTO t1 VALUES (0), (0), (1)").unwrap();
    let out = match run(&mut eng, "SELECT DISTINCT four FROM t1 WHERE four = 0").expect("runs") {
        ExecResult::Select { rows, .. } => rows,
        other => panic!("expected Select, got {:?}", other),
    };
    assert_eq!(out.len(), 1);
}
