use super::*;
use crate::sql::parse_statement;

// v1.98: EXPLAIN full analysis — planning-only EXPLAIN must raise the
// same analysis errors PG19 raises (42883/42703/42704/42501/42803/
// 42804) instead of rendering a plan for an unresolvable query.

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_as(eng, sql, "postgres")
}

fn run_as(eng: &mut Engine, sql: &str, role: &str) -> Result<ExecResult, ExecError> {
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
        role,
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    execute(eng, &mut ctx, &stmt)
}

fn code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql).unwrap_err().code.to_string()
}

fn seed(eng: &mut Engine) {
    run(eng, "CREATE TABLE t1 (a int, b text)").unwrap();
    run(eng, "INSERT INTO t1 VALUES (1, 'x'), (2, 'y')").unwrap();
    run(eng, "CREATE TABLE empty_t (a int, b text)").unwrap();
}

#[test]
fn v198_explain_unknown_function_everywhere() {
    let mut eng = engine();
    seed(&mut eng);
    for sql in [
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE nosuchfn(a) > 0",
        "EXPLAIN (COSTS OFF) SELECT nosuchfn(a) FROM t1",
        "EXPLAIN (COSTS OFF) SELECT nosuchfn(1)",
        "EXPLAIN (COSTS OFF) SELECT a FROM t1 GROUP BY a HAVING nosuchfn(a) > 0",
        "EXPLAIN (COSTS OFF) SELECT a FROM t1 ORDER BY nosuchfn(a)",
        "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a IN (SELECT nosuchfn(a) FROM t1)",
        "EXPLAIN (COSTS OFF) SELECT CASE WHEN nosuchfn(a) > 0 THEN 1 END FROM t1",
    ] {
        assert_eq!(code(&mut eng, sql), "42883", "{sql}");
    }
}

#[test]
fn v198_explain_user_operator_resolution() {
    let mut eng = engine();
    seed(&mut eng);
    // Never created: 42883 in WHERE and in the target list.
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a >>> 0"
        ),
        "42883"
    );
    assert_eq!(
        code(&mut eng, "EXPLAIN (COSTS OFF) SELECT a >>> 0 FROM t1"),
        "42883"
    );
    // Defined: plans fine. Dropped: 42883 again.
    run(
        &mut eng,
        "CREATE FUNCTION leak2(integer, integer) RETURNS boolean AS $$begin return $1 > $2; end$$ LANGUAGE plpgsql immutable",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE OPERATOR >>> (procedure = leak2, leftarg = integer, rightarg = integer)",
    )
    .unwrap();
    assert!(
        run(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a >>> 0"
        )
        .is_ok()
    );
    run(&mut eng, "DROP OPERATOR >>> (integer, integer)").unwrap();
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a >>> 0"
        ),
        "42883"
    );
}

#[test]
fn v198_explain_operator_type_pairs() {
    let mut eng = engine();
    seed(&mut eng);
    // text / text and integer + text(column): no such operators in PG.
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE b / b > 0"
        ),
        "42883"
    );
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a + b > 0"
        ),
        "42883"
    );
    // Unknown-type literals still coerce (analysis succeeds).
    assert!(
        run(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a + '1' > 0"
        )
        .is_ok()
    );
}

#[test]
fn v198_explain_unknown_column_and_type() {
    let mut eng = engine();
    seed(&mut eng);
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE nosuchcol > 0"
        ),
        "42703"
    );
    assert_eq!(
        code(&mut eng, "EXPLAIN (COSTS OFF) SELECT nosuchcol FROM t1"),
        "42703"
    );
    assert_eq!(
        code(&mut eng, "EXPLAIN (COSTS OFF) SELECT a + nosuchcol FROM t1"),
        "42703"
    );
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a = 1::nosuchtype"
        ),
        "42704"
    );
}

#[test]
fn v198_explain_empty_table_still_analyzes() {
    let mut eng = engine();
    seed(&mut eng);
    // PG analysis is static: zero rows does not rescue a bad query.
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM empty_t WHERE nosuchfn(a) > 0"
        ),
        "42883"
    );
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM empty_t WHERE nosuchcol > 0"
        ),
        "42703"
    );
}

#[test]
fn v198_explain_grouping_and_bool_clauses() {
    let mut eng = engine();
    seed(&mut eng);
    assert_eq!(
        code(&mut eng, "EXPLAIN (COSTS OFF) SELECT a, count(*) FROM t1"),
        "42803"
    );
    assert_eq!(
        code(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a"),
        "42804"
    );
    // Group-bound and aggregate forms plan fine.
    assert!(
        run(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT a, count(*) FROM t1 GROUP BY a"
        )
        .is_ok()
    );
    assert!(run(&mut eng, "EXPLAIN (COSTS OFF) SELECT count(*) FROM t1").is_ok());
    assert!(run(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM t1 WHERE a > 0").is_ok());
}

#[test]
fn v198_explain_privileges() {
    let mut eng = engine();
    seed(&mut eng);
    run(&mut eng, "CREATE ROLE bob").unwrap();
    run(&mut eng, "CREATE VIEW v1 AS SELECT * FROM t1").unwrap();
    // No grants: EXPLAIN as bob is denied for the table and the view,
    // exactly like the plain SELECT.
    let e = run_as(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM t1", "bob").unwrap_err();
    assert_eq!(e.code, "42501");
    let e = run_as(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM v1", "bob").unwrap_err();
    assert_eq!(e.code, "42501");
    // The owner plans fine, and a grant restores bob's EXPLAIN.
    assert!(run(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM t1").is_ok());
    run(&mut eng, "GRANT SELECT ON t1 TO bob").unwrap();
    assert!(run_as(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM t1", "bob").is_ok());
}

#[test]
fn v198_explain_view_internals_analyzed() {
    let mut eng = engine();
    seed(&mut eng);
    // A view whose stored query cannot be created in PG (unknown
    // function) — if creation succeeded here, EXPLAIN of a query over
    // it must still surface the analysis error; if creation itself
    // fails, that is the PG behavior too and there is nothing to check.
    if run(
        &mut eng,
        "CREATE VIEW badv AS SELECT nosuchfn(a) AS x FROM t1",
    )
    .is_ok()
    {
        assert_eq!(
            code(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM badv"),
            "42883"
        );
    }
    run(
        &mut eng,
        "CREATE VIEW goodv AS SELECT a FROM t1 WHERE a > 0",
    )
    .unwrap();
    assert!(
        run(
            &mut eng,
            "EXPLAIN (COSTS OFF) SELECT * FROM goodv WHERE a > 1"
        )
        .is_ok()
    );
}

#[test]
fn v198_explain_analyze_also_analyzes() {
    let mut eng = engine();
    seed(&mut eng);
    let e = run(
        &mut eng,
        "EXPLAIN (ANALYZE, COSTS OFF) SELECT * FROM empty_t WHERE nosuchfn(a) > 0",
    )
    .unwrap_err();
    assert_eq!(e.code, "42883");
}
