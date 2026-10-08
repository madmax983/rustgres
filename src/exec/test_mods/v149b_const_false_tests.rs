// v1.49b: PG19 const-false dummy rels (`restriction_is_constant_false`,
// joinrels.c:1547-1588) on the EXPLAIN (COSTS OFF) plan path.
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
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|r| r[0].to_text().unwrap().trim().to_string())
            .collect(),
        other => panic!("expected EXPLAIN, got {other:?}"),
    }
}

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn setup(eng: &mut Engine) {
    for sql in [
        "CREATE TEMP TABLE cf1 (a int)",
        "CREATE TEMP TABLE cf2 (b int)",
        "INSERT INTO cf1 VALUES (1), (2)",
        "INSERT INTO cf2 VALUES (1), (3)",
    ] {
        run(eng, sql).unwrap();
    }
}

fn parse_expr(sql: &str) -> Expr {
    // Parse via a WHERE clause: `SELECT 1 WHERE <expr>`.
    match parse_statement(&format!("SELECT 1 WHERE {sql}")).unwrap() {
        crate::sql::Stmt::Select(s) => s.where_.unwrap(),
        _ => panic!("expected select"),
    }
}

/// v1.49b: the boolean-const folder — literals, NULL, NOT/AND/OR.
#[test]
fn fold_bool_const_skeleton() {
    assert_eq!(
        pg_fold_bool_const(&parse_expr("false"), &mut PgConstFold::pure()),
        Some(Some(false))
    );
    assert_eq!(
        pg_fold_bool_const(&parse_expr("true"), &mut PgConstFold::pure()),
        Some(Some(true))
    );
    assert_eq!(
        pg_fold_bool_const(&parse_expr("NULL"), &mut PgConstFold::pure()),
        Some(None)
    );
    assert_eq!(
        pg_fold_bool_const(&parse_expr("NOT true"), &mut PgConstFold::pure()),
        Some(Some(false))
    );
    assert_eq!(
        pg_fold_bool_const(&parse_expr("NOT NULL"), &mut PgConstFold::pure()),
        Some(None)
    );
    assert_eq!(
        pg_fold_bool_const(&parse_expr("false AND true"), &mut PgConstFold::pure()),
        Some(Some(false))
    );
    assert_eq!(
        pg_fold_bool_const(&parse_expr("true AND NULL"), &mut PgConstFold::pure()),
        Some(None)
    );
    assert_eq!(
        pg_fold_bool_const(&parse_expr("true OR NULL"), &mut PgConstFold::pure()),
        Some(Some(true))
    );
    assert_eq!(
        pg_fold_bool_const(&parse_expr("false OR false"), &mut PgConstFold::pure()),
        Some(Some(false))
    );
    // Non-foldable: vars, function calls.
    assert_eq!(
        pg_fold_bool_const(&parse_expr("a = 1"), &mut PgConstFold::pure()),
        None
    );
    // v1.59: constant comparisons now fold (PG19 eval_const_expressions
    // folds an OpExpr whose args are all Consts) — `1 = 0` is false.
    assert_eq!(
        pg_fold_bool_const(&parse_expr("1 = 0"), &mut PgConstFold::pure()),
        Some(Some(false))
    );
    // v1.77: PG19 `eval_const_expressions` simplifies `FALSE AND x`
    // to FALSE without needing x to fold (likewise `TRUE OR x`).
    assert_eq!(
        pg_fold_bool_const(&parse_expr("false AND a = 1"), &mut PgConstFold::pure()),
        Some(Some(false))
    );
}

/// v1.49b: `pg_is_const_false` — FALSE-or-NULL counts as false (PG19
/// joinrels.c:1580-1582), TRUE does not.
#[test]
fn is_const_false_matches_pg() {
    assert!(pg_is_const_false(
        &parse_expr("false"),
        &mut PgConstFold::pure()
    ));
    assert!(pg_is_const_false(
        &parse_expr("NULL"),
        &mut PgConstFold::pure()
    ));
    assert!(pg_is_const_false(
        &parse_expr("NOT true"),
        &mut PgConstFold::pure()
    ));
    assert!(pg_is_const_false(
        &parse_expr("false AND NULL"),
        &mut PgConstFold::pure()
    ));
    assert!(!pg_is_const_false(
        &parse_expr("true"),
        &mut PgConstFold::pure()
    ));
    assert!(!pg_is_const_false(
        &parse_expr("NOT false"),
        &mut PgConstFold::pure()
    ));
    assert!(!pg_is_const_false(
        &parse_expr("a = 1"),
        &mut PgConstFold::pure()
    ));
}

/// v1.49b: top-level WHERE false → bare `Result` with `One-Time
/// Filter: false` + `Replaces: Scan on <table>`.
#[test]
fn explain_where_false_collapses_to_result() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM cf1 WHERE false",
    );
    assert_eq!(
        lines,
        vec!["Result", "Replaces: Scan on cf1", "One-Time Filter: false",]
    );
}

/// v1.49b: no FROM → no `Replaces:` line (PG19 explain.c:5048-5059).
#[test]
fn explain_where_false_no_from_no_replaces() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) SELECT 1 WHERE false");
    assert_eq!(lines, vec!["Result", "One-Time Filter: false"]);
}

/// v1.49b: VERBOSE `Output:` renders before `Replaces:` (PG19
/// explain.c:1947 then 2249-2256).
#[test]
fn explain_where_false_verbose_output_order() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) SELECT 1 FROM cf1 WHERE false",
    );
    assert_eq!(
        lines,
        vec![
            "Result",
            "Output: 1",
            "Replaces: Scan on cf1",
            "One-Time Filter: false",
        ]
    );
}

/// v1.49b: `LEFT JOIN ... ON false` → inner side becomes a dummy
/// `Result`; the join keeps `Join Filter: false`.
#[test]
fn explain_left_join_on_false_inner_dummy() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM cf1 LEFT JOIN cf2 ON false",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop Left Join",
            "Join Filter: false",
            "->  Seq Scan on cf1",
            "->  Result",
            "Replaces: Scan on cf2",
            "One-Time Filter: false",
        ]
    );
}

/// v1.49b: `LEFT JOIN ... ON NULL` → `Join Filter: NULL::boolean`
/// (PG19 renders the NULL Const with its type).
#[test]
fn explain_left_join_on_null_filter_spelling() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM cf1 LEFT JOIN cf2 ON NULL",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop Left Join",
            "Join Filter: NULL::boolean",
            "->  Seq Scan on cf1",
            "->  Result",
            "Replaces: Scan on cf2",
            "One-Time Filter: false",
        ]
    );
}

/// v1.49b: `INNER JOIN ... ON false` → the whole join is a bare
/// `Result` with `Replaces: Join on ...`.
#[test]
fn explain_inner_join_on_false_whole_dummy() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM cf1 JOIN cf2 ON false",
    );
    assert_eq!(
        lines,
        vec![
            "Result",
            "Replaces: Join on cf1, cf2",
            "One-Time Filter: false",
        ]
    );
}

/// v1.49b: FULL JOIN ON false is NOT dummy (PG19 must emit both
/// sides; joinrels.c only dummies INNER/LEFT/ANTI).
#[test]
fn explain_full_join_on_false_not_dummy() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM cf1 FULL JOIN cf2 ON false",
    );
    assert_eq!(lines[0], "Nested Loop Full Join");
    assert!(!lines.iter().any(|l| l.contains("One-Time Filter")));
}

/// v1.49b: the const-false plan shape is EXPLAIN-only — execution
/// still evaluates WHERE/ON normally (zero rows for false WHERE,
/// preserved left rows for LEFT JOIN ON false).
#[test]
fn const_false_never_changes_execution() {
    let mut eng = engine();
    setup(&mut eng);
    // WHERE false → zero rows (before and after: the plan shape
    // change must not alter this).
    let rows = rows_of(run(&mut eng, "SELECT * FROM cf1 WHERE false").unwrap());
    assert!(rows.is_empty());
    // LEFT JOIN ON false → left rows preserved with NULL extension.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT a, b FROM cf1 LEFT JOIN cf2 ON false ORDER BY a",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "NULL".to_string()],
            vec!["2".to_string(), "NULL".to_string()],
        ]
    );
    // INNER JOIN ON false → zero rows.
    let rows = rows_of(run(&mut eng, "SELECT * FROM cf1 JOIN cf2 ON false").unwrap());
    assert!(rows.is_empty());
    // WHERE true → all rows (not dummy).
    let rows = rows_of(run(&mut eng, "SELECT a FROM cf1 WHERE true").unwrap());
    assert_eq!(rows.len(), 2);
}
