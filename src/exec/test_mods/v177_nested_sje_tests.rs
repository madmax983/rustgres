// ============================================================================
// v1.77: nested inner-join self-join elimination (PG19
// `remove_useless_self_joins` descending into sub-joinlists).
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

fn plan_text(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn setup_emp(eng: &mut Engine) {
    run(
        eng,
        "CREATE TABLE emp177 (id SERIAL PRIMARY KEY NOT NULL, code int)",
    )
    .unwrap();
    run(eng, "INSERT INTO emp177 VALUES (1, 1), (2, 2)").unwrap();
}

fn parse_expr(sql: &str) -> Expr {
    // Parse via a WHERE clause: `SELECT 1 WHERE <expr>`.
    match parse_statement(&format!("SELECT 1 WHERE {sql}")).unwrap() {
        crate::sql::Stmt::Select(s) => s.where_.unwrap(),
        _ => panic!("expected select"),
    }
}

#[test]
fn v177_nested_pair_removed_from_replaces() {
    // join.out bug-#18187 shape: the (t2,t3) inner pair is proven
    // 1:1 via the PK, t2 removed, Replaces drops it.
    let mut eng = engine();
    setup_emp(&mut eng);
    let plan = plan_text(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT 1 FROM emp177 FULL JOIN \
             (SELECT * FROM emp177 t1 JOIN emp177 t2 JOIN emp177 t3 \
              ON t2.id = t3.id ON TRUE WHERE FALSE) s ON TRUE WHERE FALSE",
    );
    assert!(
        plan.contains("Replaces: Join on emp177, t1, t3"),
        "plan:\n{}",
        plan
    );
    assert!(plan.contains("One-Time Filter: false"), "plan:\n{}", plan);
}

#[test]
fn v177_nested_pair_keeps_null_rejection() {
    // The surviving `t3.id IS NOT NULL` lands as a scan Filter
    // (PG19's baserestrictinfo placement via WHERE distribution).
    let mut eng = engine();
    setup_emp(&mut eng);
    let plan = plan_text(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT t1.id, t3.id FROM emp177 t1 \
             JOIN emp177 t2 JOIN emp177 t3 ON t2.id = t3.id ON TRUE",
    );
    assert!(!plan.contains("t2"), "t2 must be gone:\n{}", plan);
    assert!(plan.contains("IS NOT NULL"), "plan:\n{}", plan);
}

#[test]
fn v177_nested_non_unique_pair_kept() {
    // `code` is not unique: fail closed, all three scans survive.
    let mut eng = engine();
    setup_emp(&mut eng);
    let plan = plan_text(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM emp177 a \
             JOIN emp177 b JOIN emp177 c ON b.code = c.code ON TRUE",
    );
    assert!(plan.contains("Seq Scan on emp177 b"), "plan:\n{}", plan);
    assert!(
        plan.contains("Join Filter: (b.code = c.code)"),
        "plan:\n{}",
        plan
    );
}

#[test]
fn v177_nested_sje_preserves_rows() {
    // Execution sees the rewritten statement: same rows as the
    // unoptimized join.
    let mut eng = engine();
    setup_emp(&mut eng);
    let n = match run(
        &mut eng,
        "SELECT t1.id, t3.id FROM emp177 t1 JOIN emp177 t2 \
             JOIN emp177 t3 ON t2.id = t3.id ON TRUE",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => rows.len(),
        other => panic!("expected Select, got {:?}", other),
    };
    assert_eq!(n, 4); // 2 t1 rows x 2 t3 rows, t2 provably 1:1
}

#[test]
fn v177_fold_false_and_nonconst() {
    // PG19 `eval_const_expressions`: `FALSE AND x` folds to FALSE
    // without needing x to fold.
    let e = parse_expr("false AND (1 = 1)");
    let mut fc = PgConstFold::pure();
    assert_eq!(pg_fold_bool_const(&e, &mut fc), Some(Some(false)));
    assert!(pg_is_const_false(&e, &mut fc));
    let e2 = parse_expr("true OR (1 = 2)");
    assert_eq!(
        pg_fold_bool_const(&e2, &mut PgConstFold::pure()),
        Some(Some(true))
    );
    // `TRUE AND x` still needs x.
    let e3 = parse_expr("true AND (1 = 1)");
    assert_eq!(
        pg_fold_bool_const(&e3, &mut PgConstFold::pure()),
        Some(Some(true))
    );
}
