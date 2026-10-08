/// v1.33: DML bodies in SQL-language functions (PG19 `fmgr_sql`
/// executes every body statement in order; `check_sql_fn_retval`
/// requires the final statement to be SELECT or
/// INSERT/UPDATE/DELETE/MERGE RETURNING). Body DML runs through the
/// statement's write context (`QWrite`): effects are statement-atomic,
/// visible to later body statements (PG19's CommandCounterIncrement
/// between statements), blocked in read-only transactions (25006),
/// and nestable (reborrowed write contexts).
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run_with(eng: &mut Engine, sql: &str, read_only: bool) -> Result<ExecResult, ExecError> {
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
        read_only,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    let r = execute(eng, &mut ctx, &stmt);
    // Mirror production `autocommit_execute`: a failed statement
    // undoes its write log (statement atomicity).
    if r.is_err() {
        for op in writes.iter().rev() {
            crate::storage::undo_write_op(eng, &[9], op);
        }
    }
    r
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_with(eng, sql, false)
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

fn setup(eng: &mut Engine) {
    run(eng, "CREATE TABLE t133(a int);").unwrap();
    run(
        eng,
        "CREATE FUNCTION f133_ins(v int) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t133(a) VALUES (v) RETURNING a';",
    )
    .unwrap();
}

#[test]
fn insert_body_returning_is_the_result() {
    let mut eng = engine();
    setup(&mut eng);
    // The final INSERT...RETURNING's rows are the function result.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f133_ins(42);").unwrap()),
        vec![vec!["42"]]
    );
    // ...and the write really landed in the table.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT a FROM t133;").unwrap()),
        vec![vec!["42"]]
    );
}

#[test]
fn later_body_statements_see_earlier_writes() {
    let mut eng = engine();
    setup(&mut eng);
    // PG19 runs CommandCounterIncrement between body statements;
    // here the shared write_xid/all_xids give the same visibility.
    run(
        &mut eng,
        "CREATE FUNCTION f133_two(v int) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t133(a) VALUES (v); SELECT a FROM t133 WHERE a = v';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f133_two(7);").unwrap()),
        vec![vec!["7"]]
    );
}

#[test]
fn update_and_delete_bodies() {
    let mut eng = engine();
    setup(&mut eng);
    run(&mut eng, "SELECT f133_ins(1);").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION f133_upd(oldv int, newv int) RETURNS int LANGUAGE sql \
             AS 'UPDATE t133 SET a = newv WHERE a = oldv RETURNING a';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f133_upd(1, 10);").unwrap()),
        vec![vec!["10"]]
    );
    run(
        &mut eng,
        "CREATE FUNCTION f133_del(v int) RETURNS int LANGUAGE sql \
             AS 'DELETE FROM t133 WHERE a = v RETURNING a';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f133_del(10);").unwrap()),
        vec![vec!["10"]]
    );
    assert!(rows_of(run(&mut eng, "SELECT a FROM t133;").unwrap()).is_empty());
}

#[test]
fn final_dml_without_returning_is_42p13() {
    let mut eng = engine();
    setup(&mut eng);
    // PG19 check_sql_fn_retval: the final statement must be SELECT
    // or INSERT/UPDATE/DELETE/MERGE RETURNING.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f133_bad(v int) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t133(a) VALUES (v)';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.detail.as_deref(),
        Some("Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING.")
    );
    // Multi-statement: only the FINAL statement matters.
    run(
        &mut eng,
        "CREATE FUNCTION f133_ok(v int) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t133(a) VALUES (v); SELECT v';",
    )
    .expect("non-final DML without RETURNING is fine");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f133_ok(3);").unwrap()),
        vec![vec!["3"]]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT a FROM t133;").unwrap()),
        vec![vec!["3"]]
    );
}

#[test]
fn final_dml_wrong_column_count_is_42p13() {
    let mut eng = engine();
    setup(&mut eng);
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f133_wide(v int) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t133(a) VALUES (v) RETURNING a, a';",
    );
    assert_eq!(e.code, "42P13");
}

#[test]
fn utility_body_still_0a000() {
    let mut eng = engine();
    setup(&mut eng);
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f133_util() RETURNS int LANGUAGE sql \
             AS 'DROP TABLE t133;';",
    );
    assert_eq!(e.code, "0A000");
}

#[test]
fn named_args_rewritten_in_dml_positions() {
    let mut eng = engine();
    setup(&mut eng);
    // Named argument in VALUES and RETURNING.
    run(
        &mut eng,
        "CREATE FUNCTION f133_named(val int) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t133(a) VALUES (val) RETURNING a + val';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f133_named(5);").unwrap()),
        vec![vec!["10"]]
    );
    // Named arguments in UPDATE SET and WHERE.
    run(
        &mut eng,
        "CREATE FUNCTION f133_nupd(oldv int, newv int) RETURNS int LANGUAGE sql \
             AS 'UPDATE t133 SET a = newv WHERE a = oldv RETURNING a';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f133_nupd(5, 50);").unwrap()),
        vec![vec!["50"]]
    );
}

#[test]
fn body_dml_is_statement_atomic() {
    let mut eng = engine();
    setup(&mut eng);
    // The second body statement fails: the first statement's
    // insert must roll back with the statement.
    run(
        &mut eng,
        "CREATE FUNCTION f133_boom(v int) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t133(a) VALUES (v); SELECT 1/0';",
    )
    .unwrap();
    let e = err_of(&mut eng, "SELECT f133_boom(9);");
    assert_eq!(e.code, "22012");
    assert!(rows_of(run(&mut eng, "SELECT a FROM t133;").unwrap()).is_empty());
}

#[test]
fn nested_function_dml_reborrows_write_context() {
    let mut eng = engine();
    setup(&mut eng);
    // A DML-bodied function called from another function body:
    // the write context reborrows (no RefCell, no lost writes).
    run(
        &mut eng,
        "CREATE FUNCTION f133_outer(v int) RETURNS int LANGUAGE sql \
             AS 'SELECT f133_ins(v) + 100';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f133_outer(11);").unwrap()),
        vec![vec!["111"]]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT a FROM t133;").unwrap()),
        vec![vec!["11"]]
    );
}

#[test]
fn read_only_blocks_body_dml() {
    let mut eng = engine();
    setup(&mut eng);
    // PG19: a read-only transaction rejects body DML with 25006;
    // the server's statement-level check cannot see inside
    // function bodies, so run_func_dml enforces it.
    let e = run_with(&mut eng, "SELECT f133_ins(1);", true).unwrap_err();
    assert_eq!(e.code, "25006");
    assert_eq!(
        e.message,
        "cannot execute INSERT in a read-only transaction"
    );
    assert!(rows_of(run(&mut eng, "SELECT a FROM t133;").unwrap()).is_empty());
}

#[test]
fn dml_bodied_function_called_from_dml() {
    let mut eng = engine();
    setup(&mut eng);
    // INSERT INTO ... SELECT f(): the inner function's INSERT and
    // the outer INSERT share the statement's write log.
    run(&mut eng, "INSERT INTO t133(a) SELECT f133_ins(99);").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT a FROM t133 ORDER BY a;").unwrap()),
        vec![vec!["99"], vec!["99"]]
    );
}

#[test]
fn setof_dml_body() {
    let mut eng = engine();
    setup(&mut eng);
    run(
        &mut eng,
        "CREATE FUNCTION f133_set() RETURNS SETOF int LANGUAGE sql \
             AS 'INSERT INTO t133(a) VALUES (1), (2), (3) RETURNING a';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT * FROM f133_set();").unwrap()),
        vec![vec!["1"], vec!["2"], vec!["3"]]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT a FROM t133;").unwrap()).len(),
        3
    );
}
