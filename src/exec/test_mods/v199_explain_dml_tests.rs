use super::*;
use crate::sql::parse_statement;

// v1.99: EXPLAIN of DML — planning EXPLAIN / EXPLAIN ANALYZE over
// INSERT/UPDATE/DELETE, CTAS, CREATE MATERIALIZED VIEW, REFRESH and
// DECLARE run the same static analysis as execution (privileges,
// expression resolution, arbiter inference) and render PG19-shaped
// ModifyTable / passthrough plans.

fn engine() -> Engine {
    let mut eng = Engine::new();
    // Make every small xid read as committed (as in the storage
    // tests), so a fresh xid per statement stays visible to the next.
    eng.txns.next_xid = 1_000_000;
    eng
}

// Fresh xid per statement, mirroring production autocommit: index
// created_xmin ordering (the EXPLAIN arbiter list) depends on it.
static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(100);

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_as(eng, sql, "postgres")
}

fn run_as(eng: &mut Engine, sql: &str, role: &str) -> Result<ExecResult, ExecError> {
    let xid = NEXT_XID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stmt = parse_statement(sql).expect("parses");
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        snap: &snap,
        own: xid,
        write_xid: xid,
        all_xids: vec![xid],
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

fn plan(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).expect("explains") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|r| r[0].to_text().unwrap_or_default())
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn rows_of(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql).expect("runs") {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected Select, got {other:?}"),
    }
}

fn one(eng: &mut Engine, sql: &str) -> String {
    rows_of(eng, sql)[0][0].clone()
}

fn seed(eng: &mut Engine) {
    run(eng, "CREATE TABLE t1 (a int PRIMARY KEY, b text UNIQUE)").unwrap();
    run(eng, "INSERT INTO t1 VALUES (1, 'x'), (2, 'y')").unwrap();
    run(eng, "CREATE TABLE t2 (a int, b text)").unwrap();
}

#[test]
fn v199_explain_insert_shapes() {
    let mut eng = engine();
    seed(&mut eng);
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) INSERT INTO t1 VALUES (3, 'z')",
    );
    assert_eq!(p[0], "Insert on t1");
    assert!(p.iter().any(|l| l.contains("Result")), "{p:?}");
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) INSERT INTO t1 VALUES (3, 'z'), (4, 'w')",
    );
    assert!(p.iter().any(|l| l.contains("Values Scan")), "{p:?}");
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) INSERT INTO t1 SELECT * FROM t2",
    );
    assert_eq!(p[0], "Insert on t1");
    assert!(p.iter().any(|l| l.contains("Seq Scan on t2")), "{p:?}");
    // Planning EXPLAIN must not write.
    assert_eq!(one(&mut eng, "SELECT count(*) FROM t1"), "2");
}

#[test]
fn v199_explain_update_delete_shapes() {
    let mut eng = engine();
    seed(&mut eng);
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) UPDATE t1 SET b = 'q' WHERE a = 1",
    );
    assert_eq!(p[0], "Update on t1");
    assert!(p.iter().any(|l| l.contains("Seq Scan on t1")), "{p:?}");
    let p = plan(&mut eng, "EXPLAIN (COSTS OFF) DELETE FROM t1 WHERE a = 1");
    assert_eq!(p[0], "Delete on t1");
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) UPDATE t1 SET b = 'q' RETURNING a",
    );
    assert_eq!(p[0], "Update on t1");
}

#[test]
fn v199_explain_conflict_resolution_lines() {
    let mut eng = engine();
    seed(&mut eng);
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) INSERT INTO t1 VALUES (3, 'z') ON CONFLICT (a) DO NOTHING",
    );
    assert!(
        p.iter().any(|l| l == "  Conflict Resolution: NOTHING"),
        "{p:?}"
    );
    assert!(
        p.iter().any(|l| l == "  Conflict Arbiter Indexes: t1_pkey"),
        "{p:?}"
    );
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) INSERT INTO t1 VALUES (3, 'z') ON CONFLICT (b) DO UPDATE SET a = 5",
    );
    assert!(
        p.iter().any(|l| l == "  Conflict Resolution: UPDATE"),
        "{p:?}"
    );
    assert!(
        p.iter()
            .any(|l| l == "  Conflict Arbiter Indexes: t1_b_key"),
        "{p:?}"
    );
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) INSERT INTO t1 VALUES (3, 'z') ON CONFLICT (a) DO SELECT RETURNING *",
    );
    assert!(
        p.iter().any(|l| l == "  Conflict Resolution: SELECT"),
        "{p:?}"
    );
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) INSERT INTO t1 VALUES (3, 'z') ON CONFLICT (a) DO SELECT FOR KEY SHARE RETURNING *",
    );
    assert!(
        p.iter()
            .any(|l| l == "  Conflict Resolution: SELECT FOR KEY SHARE"),
        "{p:?}"
    );
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) INSERT INTO t1 VALUES (3, 'z') ON CONFLICT (a) DO SELECT FOR UPDATE RETURNING *",
    );
    assert!(
        p.iter()
            .any(|l| l == "  Conflict Resolution: SELECT FOR UPDATE"),
        "{p:?}"
    );
}

#[test]
fn v199_explain_dml_validation_errors() {
    let mut eng = engine();
    seed(&mut eng);
    for (sql, want) in [
        ("EXPLAIN INSERT INTO missing_tbl VALUES (1)", "42P01"),
        ("EXPLAIN UPDATE missing_tbl SET a = 1", "42P01"),
        ("EXPLAIN DELETE FROM missing_tbl", "42P01"),
        ("EXPLAIN UPDATE t1 SET nosuch = 1", "42703"),
        ("EXPLAIN UPDATE t1 SET b = nosuchfn(a)", "42883"),
        ("EXPLAIN DELETE FROM t1 WHERE nosuchfn(a) > 0", "42883"),
        ("EXPLAIN INSERT INTO t1 (nosuch) VALUES (1)", "42703"),
        ("EXPLAIN INSERT INTO t1 VALUES (1)", "42601"),
        ("EXPLAIN INSERT INTO t1 SELECT a FROM t2", "42601"),
        ("EXPLAIN UPDATE t1 SET b = 'x', b = 'y'", "42701"),
        (
            "EXPLAIN INSERT INTO t1 VALUES (3, 'z') ON CONFLICT (a, b) DO NOTHING",
            "42P10",
        ),
        (
            "EXPLAIN INSERT INTO t1 VALUES (3, 'z') ON CONFLICT (nosuch) DO NOTHING",
            "42703",
        ),
        (
            "EXPLAIN INSERT INTO t1 VALUES (3, 'z') ON CONFLICT DO UPDATE SET a = 1",
            "42601",
        ),
        (
            "EXPLAIN INSERT INTO t1 VALUES (3, 'z') ON CONFLICT (a) DO UPDATE SET nosuch = 1",
            "42703",
        ),
    ] {
        assert_eq!(code(&mut eng, sql), want, "{sql}");
    }
}

#[test]
fn v199_explain_dml_privileges() {
    let mut eng = engine();
    seed(&mut eng);
    run(&mut eng, "CREATE ROLE bob").unwrap();
    // No privileges at all: INSERT/UPDATE/DELETE analysis must 42501
    // exactly like execution would.
    let e = run_as(&mut eng, "EXPLAIN INSERT INTO t1 VALUES (3, 'z')", "bob").unwrap_err();
    assert_eq!(e.code, "42501");
    let e = run_as(&mut eng, "EXPLAIN UPDATE t1 SET b = 'q'", "bob").unwrap_err();
    assert_eq!(e.code, "42501");
    let e = run_as(&mut eng, "EXPLAIN DELETE FROM t1", "bob").unwrap_err();
    assert_eq!(e.code, "42501");
    // INSERT alone does not cover the ON CONFLICT DO UPDATE read side
    // or a WHERE clause (both need SELECT on the target).
    run(&mut eng, "GRANT INSERT ON t1 TO bob").unwrap();
    run_as(&mut eng, "EXPLAIN INSERT INTO t1 VALUES (3, 'z')", "bob").unwrap();
    let e = run_as(&mut eng, "EXPLAIN DELETE FROM t1 WHERE a = 1", "bob").unwrap_err();
    assert_eq!(e.code, "42501");
    run(&mut eng, "GRANT SELECT, UPDATE, DELETE ON t1 TO bob").unwrap();
    run_as(&mut eng, "EXPLAIN UPDATE t1 SET b = 'q' WHERE a = 1", "bob").unwrap();
    run_as(&mut eng, "EXPLAIN DELETE FROM t1 WHERE a = 1", "bob").unwrap();
}

#[test]
fn v199_arbiter_inference_matrix() {
    // The PG19 insert_conflict arbiter family: opclass / collation /
    // expression inference with the liberal satisfaction rule.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE ict (key int, fruit text)").unwrap();
    run(
        &mut eng,
        "CREATE UNIQUE INDEX op_index_key ON ict (key, fruit text_pattern_ops)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE UNIQUE INDEX collation_index_key ON ict (key, fruit collate \"C\")",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE UNIQUE INDEX both_index_key ON ict (key, fruit collate \"C\" text_pattern_ops)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE UNIQUE INDEX both_index_expr_key ON ict (key, lower(fruit) collate \"C\" text_pattern_ops)",
    )
    .unwrap();
    let arbs = |eng: &mut Engine, spec: &str| -> Vec<String> {
        let sql = format!(
            "EXPLAIN (COSTS OFF) INSERT INTO ict VALUES (0, 'x') ON CONFLICT {spec} DO NOTHING"
        );
        match run(eng, &sql) {
            Ok(ExecResult::Explain { rows, .. }) => rows
                .iter()
                .filter_map(|r| {
                    r[0].to_text().and_then(|l| {
                        l.strip_prefix("  Conflict Arbiter Indexes: ")
                            .map(str::to_string)
                    })
                })
                .collect(),
            Err(e) => vec![format!("ERR {}", e.code)],
            Ok(other) => panic!("expected Explain, got {other:?}"),
        }
    };
    assert_eq!(
        arbs(&mut eng, "(key, fruit)"),
        ["op_index_key, collation_index_key, both_index_key"]
    );
    assert_eq!(
        arbs(&mut eng, "(key, fruit text_pattern_ops)"),
        ["op_index_key, both_index_key"]
    );
    assert_eq!(
        arbs(&mut eng, "(key, fruit collate \"C\")"),
        ["collation_index_key, both_index_key"]
    );
    assert_eq!(
        arbs(&mut eng, "(fruit collate \"C\" text_pattern_ops, key)"),
        ["both_index_key"]
    );
    assert_eq!(
        arbs(&mut eng, "(fruit, key, fruit, key)"),
        ["op_index_key, collation_index_key, both_index_key"]
    );
    assert_eq!(
        arbs(&mut eng, "(lower(fruit) collate \"C\", key, key)"),
        ["both_index_expr_key"]
    );
    assert_eq!(arbs(&mut eng, "(key)"), ["ERR 42P10"]);
    // Cross-matching must not occur.
    run(
        &mut eng,
        "CREATE UNIQUE INDEX cross_match ON ict (lower(fruit) collate \"C\", upper(fruit) text_pattern_ops)",
    )
    .unwrap();
    assert_eq!(
        arbs(
            &mut eng,
            "(lower(fruit) text_pattern_ops, upper(fruit) collate \"C\")"
        ),
        ["ERR 42P10"]
    );
    assert_eq!(
        arbs(
            &mut eng,
            "(lower(fruit) collate \"C\", upper(fruit) text_pattern_ops)"
        ),
        ["cross_match"]
    );
}

#[test]
fn v199_explain_analyze_dml_executes() {
    let mut eng = engine();
    seed(&mut eng);
    let p = plan(
        &mut eng,
        "EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF, TIMING OFF) INSERT INTO t1 VALUES (7, 'q')",
    );
    assert!(p[0].starts_with("Insert on t1 (actual rows="), "{p:?}");
    assert_eq!(one(&mut eng, "SELECT count(*) FROM t1"), "3");
    let p = plan(
        &mut eng,
        "EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF, TIMING OFF) UPDATE t1 SET b = 'z' WHERE a = 7",
    );
    assert!(p[0].starts_with("Update on t1 (actual rows="), "{p:?}");
    assert_eq!(one(&mut eng, "SELECT b FROM t1 WHERE a = 7"), "z");
    let p = plan(
        &mut eng,
        "EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF, TIMING OFF) DELETE FROM t1 WHERE a = 7",
    );
    assert!(p[0].starts_with("Delete on t1 (actual rows="), "{p:?}");
    assert_eq!(one(&mut eng, "SELECT count(*) FROM t1"), "2");
    // A failing statement under ANALYZE errors in analysis, before
    // any execution.
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN (ANALYZE, COSTS OFF) UPDATE t1 SET nosuch = 1"
        ),
        "42703"
    );
}

#[test]
fn v199_explain_ctas_and_matview() {
    let mut eng = engine();
    seed(&mut eng);
    // CTAS has no wrapper node: the inner SELECT plan verbatim.
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) CREATE TABLE c1 AS SELECT * FROM t1",
    );
    assert!(p.iter().any(|l| l.contains("Seq Scan on t1")), "{p:?}");
    // IF NOT EXISTS over an existing name: empty plan, no error.
    run(&mut eng, "CREATE TABLE dup AS SELECT 1").unwrap();
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) CREATE TABLE IF NOT EXISTS dup AS SELECT 1/0",
    );
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(
        code(&mut eng, "EXPLAIN CREATE TABLE dup AS SELECT 1"),
        "42P07"
    );
    // ANALYZE executes; WITH NO DATA leaves every node unexecuted.
    plan(
        &mut eng,
        "EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF, TIMING OFF) CREATE TABLE c2 AS SELECT * FROM t1 WITH DATA",
    );
    assert_eq!(one(&mut eng, "SELECT count(*) FROM c2"), "2");
    let p = plan(
        &mut eng,
        "EXPLAIN (ANALYZE, COSTS OFF, SUMMARY OFF, TIMING OFF) CREATE TABLE c3 AS SELECT * FROM t1 WITH NO DATA",
    );
    assert!(p.iter().all(|l| l.contains("never executed")), "{p:?}");
    assert_eq!(one(&mut eng, "SELECT count(*) FROM c3"), "0");
    // Matview create + refresh.
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) CREATE MATERIALIZED VIEW mv AS SELECT * FROM t1",
    );
    assert!(p.iter().any(|l| l.contains("Seq Scan on t1")), "{p:?}");
    run(&mut eng, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM t1").unwrap();
    let p = plan(&mut eng, "EXPLAIN (COSTS OFF) REFRESH MATERIALIZED VIEW mv");
    assert!(p.iter().any(|l| l.contains("Seq Scan on t1")), "{p:?}");
    assert_eq!(
        code(&mut eng, "EXPLAIN REFRESH MATERIALIZED VIEW missing_mv"),
        "42P01"
    );
    // Plain CREATE VIEW is not explainable in PG19.
    assert!(parse_statement("EXPLAIN CREATE VIEW v AS SELECT 1").is_err());
}

#[test]
fn v199_ctas_with_data_spelling() {
    // Regression: the bare-alias parser used to eat the trailing
    // WITH of CTAS (is_reserved lacked "with"), so the explicit
    // WITH DATA / WITH NO DATA spellings were 42601.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE wd (a) AS SELECT 1 WITH DATA").unwrap();
    assert_eq!(one(&mut eng, "SELECT count(*) FROM wd"), "1");
    run(&mut eng, "CREATE TABLE wnd (a) AS SELECT 1 WITH NO DATA").unwrap();
    assert_eq!(one(&mut eng, "SELECT count(*) FROM wnd"), "0");
    let stmt = parse_statement("CREATE TABLE x (a) AS EXECUTE p1 (1, 'x') WITH NO DATA")
        .expect("ctas-as-execute parses");
    match stmt {
        crate::sql::Stmt::CreateTableAs {
            name,
            execute,
            with_data,
            ..
        } => {
            assert_eq!(name, "x");
            assert_eq!(execute.expect("execute spec").0, "p1");
            assert!(!with_data);
        }
        other => panic!("expected CreateTableAs, got {other:?}"),
    }
}

#[test]
fn v199_explain_declare_and_parser_gates() {
    let mut eng = engine();
    seed(&mut eng);
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) DECLARE cur CURSOR FOR SELECT * FROM t1 WHERE a = 1",
    );
    assert!(p.iter().any(|l| l.contains("Seq Scan on t1")), "{p:?}");
    // MERGE has no engine statement at all yet: parse-level 42601.
    assert!(
        parse_statement("EXPLAIN MERGE INTO t1 USING t2 ON t1.a = t2.a WHEN MATCHED THEN DELETE")
            .is_err()
    );
    // The EXPLAIN arm accepts exactly the PG19 ExplainableStmt set.
    for sql in [
        "EXPLAIN INSERT INTO t1 VALUES (1, 'x')",
        "EXPLAIN UPDATE t1 SET b = 'x'",
        "EXPLAIN DELETE FROM t1",
        "EXPLAIN CREATE TABLE q AS SELECT 1",
        "EXPLAIN DECLARE c CURSOR FOR SELECT 1",
    ] {
        assert!(parse_statement(sql).is_ok(), "{sql}");
    }
}

#[test]
fn v199_explain_dml_ctes() {
    let mut eng = engine();
    seed(&mut eng);
    // DML as the source of an INSERT via CTE, and a data-modifying
    // CTE inside an explained SELECT: analysis reaches the bodies.
    let p = plan(
        &mut eng,
        "EXPLAIN (COSTS OFF) WITH s AS (SELECT * FROM t2) INSERT INTO t1 SELECT * FROM s",
    );
    assert_eq!(p[0], "Insert on t1");
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN WITH d AS (DELETE FROM t1 WHERE nosuchfn(a) > 0 RETURNING *) SELECT * FROM d"
        ),
        "42883"
    );
    assert_eq!(
        code(
            &mut eng,
            "EXPLAIN WITH d AS (DELETE FROM missing_tbl RETURNING *) SELECT * FROM d"
        ),
        "42P01"
    );
}
