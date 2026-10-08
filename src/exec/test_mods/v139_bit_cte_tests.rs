
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

fn col0(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap() {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|r| r[0].to_text().unwrap_or("NULL".to_string()))
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn err_code(eng: &mut Engine, sql: &str) -> &'static str {
    run(eng, sql).unwrap_err().code
}

fn err_msg(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql).unwrap_err().message
}

// --- bit-string literals (PG19 bit_in) ---

#[test]
fn bit_literal_hex_digits() {
    let mut eng = engine();
    // x'1A' = 00011010 (4 bits per hex digit).
    assert_eq!(col0(&mut eng, "select x'1A'::text;"), vec!["00011010"]);
    assert_eq!(col0(&mut eng, "select X'ff'::text;"), vec!["11111111"]);
}

#[test]
fn bit_literal_binary_digits() {
    let mut eng = engine();
    assert_eq!(col0(&mut eng, "select b'101'::text;"), vec!["101"]);
    assert_eq!(col0(&mut eng, "select B'0010'::text;"), vec!["0010"]);
}

#[test]
fn bit_literal_typeof() {
    let mut eng = engine();
    assert_eq!(col0(&mut eng, "select pg_typeof(x'1A');"), vec!["bit"]);
}

#[test]
fn bit_literal_bad_hex_digit() {
    let mut eng = engine();
    assert_eq!(err_code(&mut eng, "select x'2G';"), "22P02");
    assert!(err_msg(&mut eng, "select x'2G';").contains("\"G\" is not a valid hexadecimal digit"));
}

#[test]
fn bit_literal_bad_binary_digit() {
    let mut eng = engine();
    assert_eq!(err_code(&mut eng, "select b'102';"), "22P02");
    assert!(err_msg(&mut eng, "select b'102';").contains("\"2\" is not a valid binary digit"));
}

// --- bit -> int4 / int8 (PG19 bittoint4/bittoint8) ---

#[test]
fn bit_to_int4() {
    let mut eng = engine();
    assert_eq!(col0(&mut eng, "select x'1A'::integer;"), vec!["26"]);
    assert_eq!(col0(&mut eng, "select b'101'::integer;"), vec!["5"]);
    // Two's complement: 32 one-bits = -1.
    assert_eq!(col0(&mut eng, "select x'FFFFFFFF'::integer;"), vec!["-1"]);
    // Short strings are zero-extended on the left.
    assert_eq!(col0(&mut eng, "select x'2'::integer;"), vec!["2"]);
}

#[test]
fn bit_to_int4_range() {
    let mut eng = engine();
    assert_eq!(err_code(&mut eng, "select x'1FFFFFFFF'::integer;"), "22003");
}

#[test]
fn bit_to_int8() {
    let mut eng = engine();
    assert_eq!(col0(&mut eng, "select b'101'::bigint;"), vec!["5"]);
    assert_eq!(
        col0(&mut eng, "select x'FFFFFFFFFFFFFFFF'::bigint;"),
        vec!["-1"]
    );
}

#[test]
fn bit_to_int8_range() {
    let mut eng = engine();
    assert_eq!(
        err_code(&mut eng, "select x'1FFFFFFFFFFFFFFFF'::bigint;"),
        "22003"
    );
}

// --- data-modifying CTEs (PG19 parse_cte.c) ---

fn mk(eng: &mut Engine) {
    run(eng, "create table t139(a int, b text);").unwrap();
}

#[test]
fn cte_insert_returning() {
    let mut eng = engine();
    mk(&mut eng);
    let rows = match run(
            &mut eng,
            "with ins as (insert into t139 values (1, 'x'), (2, 'y') returning *) select * from ins order by a;",
        )
        .unwrap()
        {
            ExecResult::Select { rows, .. } => rows,
            other => panic!("expected SELECT, got {other:?}"),
        };
    assert_eq!(rows.len(), 2);
    // The rows are visible in the table too (same statement snapshot).
    assert_eq!(col0(&mut eng, "select count(*) from t139;"), vec!["2"]);
}

#[test]
fn cte_delete_returning() {
    let mut eng = engine();
    mk(&mut eng);
    run(&mut eng, "insert into t139 values (1, 'x'), (2, 'y');").unwrap();
    assert_eq!(
        col0(
            &mut eng,
            "with d as (delete from t139 where a = 1 returning a) select * from d;"
        ),
        vec!["1"]
    );
    assert_eq!(col0(&mut eng, "select count(*) from t139;"), vec!["1"]);
}

#[test]
fn cte_update_returning() {
    let mut eng = engine();
    mk(&mut eng);
    run(&mut eng, "insert into t139 values (1, 'x');").unwrap();
    assert_eq!(
        col0(
            &mut eng,
            "with u as (update t139 set b = 'z' where a = 1 returning b) select * from u;"
        ),
        vec!["z"]
    );
}

#[test]
fn cte_dml_chained() {
    let mut eng = engine();
    mk(&mut eng);
    // A second CTE can read the first CTE's RETURNING rows.
    assert_eq!(
        col0(
            &mut eng,
            "with a as (insert into t139 values (7, 'w') returning *), b as (select a from a) select * from b;"
        ),
        vec!["7"]
    );
}

#[test]
fn cte_dml_recursive_rejected() {
    let mut eng = engine();
    mk(&mut eng);
    assert_eq!(
        err_code(
            &mut eng,
            "with recursive r as (insert into t139 values (3, 'q') returning *) select * from r;"
        ),
        "42P19"
    );
}

#[test]
fn cte_dml_empty_returning() {
    let mut eng = engine();
    mk(&mut eng);
    // No RETURNING list: zero columns, but the insert still happens.
    match run(
        &mut eng,
        "with ins as (insert into t139 values (5, 'v')) select * from ins;",
    )
    .unwrap()
    {
        ExecResult::Select { columns, rows, .. } => {
            assert_eq!(columns.len(), 0);
            assert_eq!(rows.len(), 0);
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
    assert_eq!(col0(&mut eng, "select count(*) from t139;"), vec!["1"]);
}
