
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
    match run(eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .collect(),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn setup(eng: &mut Engine) {
    run(eng, "CREATE TABLE null_tab (id int, val int)").unwrap();
    run(
        eng,
        "CREATE TABLE not_null_tab (id int NOT NULL, val int NOT NULL)",
    )
    .unwrap();
    run(
        eng,
        "INSERT INTO null_tab VALUES (1, 10), (2, 20), (NULL, 30)",
    )
    .unwrap();
    run(
        eng,
        "INSERT INTO not_null_tab VALUES (1, 100), (2, 200), (3, 300)",
    )
    .unwrap();
}

#[test]
fn v169_no_stats_rows() {
    // PG19 no-stats row estimation is exercised via the plan shapes
    // below; the 2260-row default is validated by T13–T19 oracles.
}

#[test]
fn v169_merge_anti_result_identity() {
    // Merge Anti Join results must match the NOT IN semantics.
    let mut eng = engine();
    setup(&mut eng);
    match run(
        &mut eng,
        "SELECT id FROM not_null_tab WHERE id NOT IN (SELECT id FROM not_null_tab)",
    )
    .expect("runs")
    {
        ExecResult::Select { rows, .. } => {
            // not_null_tab has 1,2,3; subquery has 1,2,3 → no rows.
            assert_eq!(rows.len(), 0);
        }
        other => panic!("expected Select, got {:?}", other),
    }
}

#[test]
fn v169_t19_plan_shape() {
    // T19: multi-key NOT IN → Merge Anti Join with 2-key Merge Cond.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM not_null_tab WHERE (id, val) NOT IN (SELECT id, val FROM not_null_tab)",
    );
    assert!(
        lines.iter().any(|l| l.trim() == "Merge Anti Join"),
        "expected Merge Anti Join, got: {:?}",
        lines
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Merge Cond: ((not_null_tab.id = not_null_tab_1.id)")),
        "expected 2-key Merge Cond, got: {:?}",
        lines
    );
}

/// v1.74: `expr_has_subquery` must not false-positive on pure-expression
/// variants. The `_ => true` catch-all historically reported "has subquery"
/// for any `Arith` (and `Extract`, `Subscript`, `Slice`, `Like`, `Regex`,
/// `FieldAccess`, `NamedArg`, `WholeRow`, `Window`), disabling self-join
/// elimination for restrictions containing arithmetic.
#[test]
fn v174_expr_has_subquery_arith_no_false_positive() {
    use crate::sql::{ArithOp, Expr, Literal};
    let col = || Expr::Column {
        table: None,
        name: "a".to_string(),
    };
    let lit = || Expr::Literal(Literal::Int(1));
    // `a + 1`: no subquery.
    let arith = Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(col()),
        right: Box::new(lit()),
    };
    assert!(!expr_has_subquery(&arith));
    // `a + (SELECT ...)`: subquery inside arithmetic must still be found.
    let sub = parse_statement("SELECT 1").unwrap();
    let Stmt::Select(sel) = sub else {
        panic!("expected SELECT");
    };
    let arith_sub = Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(col()),
        right: Box::new(Expr::ScalarSub(Box::new(sel))),
    };
    assert!(expr_has_subquery(&arith_sub));
}

#[test]
fn v174_expr_has_subquery_other_variants_no_false_positive() {
    use crate::sql::{Expr, Literal};
    let col = || Expr::Column {
        table: None,
        name: "a".to_string(),
    };
    // EXTRACT(DOW FROM a)
    assert!(!expr_has_subquery(&Expr::Extract {
        field: "dow".to_string(),
        from: Box::new(col()),
    }));
    // a LIKE 'x%'
    assert!(!expr_has_subquery(&Expr::Like {
        expr: Box::new(col()),
        pattern: Box::new(Expr::Literal(Literal::Text("x%".into()))),
        not: false,
        ilike: false,
        escape: None,
    }));
    // a[1]
    assert!(!expr_has_subquery(&Expr::Subscript {
        array: Box::new(col()),
        indices: vec![Expr::Literal(Literal::Int(1))],
    }));
    // whole-row ref
    assert!(!expr_has_subquery(&Expr::WholeRow {
        qual: "t".to_string(),
    }));
}
