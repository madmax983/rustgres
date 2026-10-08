/// v1.32: SQL-language CREATE FUNCTION parity (PG19 fmgr_sql_validator +
/// fmgr_sql): multi-statement bodies, language resolution at CREATE
/// (42883), final-statement return-type checks (42P13), duplicate
/// message wording (42723).
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    // Preserve the parser's SQLSTATE (e.g. 42601), like the main
    // test harness does.
    let stmt = crate::sql::parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
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

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } | ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn err_of(eng: &mut Engine, sql: &str) -> ExecError {
    run(eng, sql).unwrap_err()
}

#[test]
fn unknown_language_is_42883_at_create() {
    let mut eng = engine();
    // v1.32: PG19 resolves the language at CREATE (proclang.c
    // get_language_oid): unknown languages are 42883, not 42601.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE c AS 'SELECT 1';",
    );
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "language \"c\" does not exist");
    // Language names resolve case-insensitively (unquoted idents
    // are folded by the tokenizer).
    run(
        &mut eng,
        "CREATE FUNCTION g() RETURNS int LANGUAGE SQL AS 'SELECT 1';",
    )
    .expect("LANGUAGE SQL should resolve");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT g();").unwrap()),
        vec![vec!["1"]]
    );
}

#[test]
fn multi_statement_body_last_statement_wins() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION m() RETURNS int LANGUAGE sql AS 'SELECT 1; SELECT 2; SELECT 3';",
    )
    .expect("multi-statement body should create");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT m();").unwrap()),
        vec![vec!["3"]]
    );
}

#[test]
fn multi_statement_body_binds_args_per_statement() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION add2(a int, b int) RETURNS int LANGUAGE sql \
             AS 'SELECT a + b; SELECT a * 10 + b; SELECT $1 + $2';",
    )
    .expect("named + positional args should create");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT add2(3, 4);").unwrap()),
        vec![vec!["7"]]
    );
}

#[test]
fn multi_statement_body_comments_and_empty_chunks() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION cmt() RETURNS int LANGUAGE sql \
             AS '-- a comment\nSELECT 41;;\n\nSELECT 42;';",
    )
    .expect("comments/empty chunks should create");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT cmt();").unwrap()),
        vec![vec!["42"]]
    );
}

#[test]
fn setof_body_returns_last_statement_rows() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION s() RETURNS SETOF int LANGUAGE sql \
             AS 'SELECT 1; SELECT generate_series(1, 3);';",
    )
    .expect("setof body should create");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT * FROM s();").unwrap()),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
}

#[test]
fn empty_body_is_42p13() {
    let mut eng = engine();
    // PG19 check_sql_fn_retval: an empty body is "the last query
    // rewrote to nothing" -> return type mismatch.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION e() RETURNS int LANGUAGE sql AS '';",
    );
    assert_eq!(e.code, "42P13");
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION e2() RETURNS int LANGUAGE sql AS ';;';",
    );
    assert_eq!(e.code, "42P13");
}

#[test]
fn scalar_two_column_final_is_42p13() {
    let mut eng = engine();
    // PG19 check_sql_fn_retval: scalar functions need exactly one
    // output column from the final statement.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION two() RETURNS int LANGUAGE sql AS 'SELECT 1, 2';",
    );
    assert_eq!(e.code, "42P13");
    // SETOF functions are exempt from the one-column rule.
    run(
        &mut eng,
        "CREATE FUNCTION two_set() RETURNS SETOF int LANGUAGE sql AS 'SELECT 1, 2';",
    )
    .expect("SETOF two-column body should create");
}

#[test]
fn non_select_body_statement_is_0a000() {
    let mut eng = engine();
    // v1.33: PG allows DML bodies and this engine now has a
    // Q-level DML write path (`QWrite`), so INSERT/UPDATE/DELETE
    // body statements are accepted at CREATE...
    run(
            &mut eng,
            "CREATE FUNCTION ins() RETURNS int LANGUAGE sql AS 'SELECT 1; INSERT INTO t VALUES (1) RETURNING 1';",
        )
        .expect("v1.33 accepts DML function bodies");
    // ...while utility statements stay an honest 0A000 (not a
    // mask).
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION dd() RETURNS int LANGUAGE sql AS 'DROP TABLE t;';",
    );
    assert_eq!(e.code, "0A000");
}

#[test]
fn body_syntax_error_transposed_to_create() {
    let mut eng = engine();
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION bad() RETURNS int LANGUAGE sql AS 'SELECT FROM WHERE';",
    );
    assert_eq!(e.code, "42601");
    assert!(e.message.contains("function body"), "got: {}", e.message);
}

#[test]
fn duplicate_message_is_pg_verbatim() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION d() RETURNS int LANGUAGE sql AS 'SELECT 1';",
    )
    .unwrap();
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION d() RETURNS int LANGUAGE sql AS 'SELECT 2';",
    );
    assert_eq!(e.code, "42723");
    assert_eq!(
        e.message,
        "function \"d\" already exists with same argument types"
    );
    // OR REPLACE succeeds and swaps the body.
    run(
        &mut eng,
        "CREATE OR REPLACE FUNCTION d() RETURNS int LANGUAGE sql AS 'SELECT 2;';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT d();").unwrap()),
        vec![vec!["2"]]
    );
}

#[test]
fn drop_wrong_signature_is_42883() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION w(int) RETURNS int LANGUAGE sql AS 'SELECT $1';",
    )
    .unwrap();
    let e = err_of(&mut eng, "DROP FUNCTION w(text);");
    assert_eq!(e.code, "42883");
    run(&mut eng, "DROP FUNCTION w(int);").unwrap();
}

#[test]
fn strict_null_short_circuits_multi_statement() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION sn(int) RETURNS int LANGUAGE sql STRICT AS 'SELECT $1; SELECT $1 + 1';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT sn(NULL);").unwrap()),
        vec![vec!["NULL"]]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT sn(41);").unwrap()),
        vec![vec!["42"]]
    );
}

#[test]
fn volatility_recorded_and_callable() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION imm() RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT 7;';",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION stb() RETURNS int LANGUAGE sql STABLE AS 'SELECT 8; SELECT 9;';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT imm(), stb();").unwrap()),
        vec![vec!["7", "9"]]
    );
}

#[test]
fn single_statement_still_works() {
    let mut eng = engine();
    // Regression: the old single-SELECT path must be unchanged.
    run(
        &mut eng,
        "CREATE FUNCTION one(a int) RETURNS int LANGUAGE sql AS 'SELECT a * 2';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT one(21);").unwrap()),
        vec![vec!["42"]]
    );
}
