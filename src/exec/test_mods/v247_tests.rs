/// v2.47: ROWSECURITY REMAINDER — WHERE CURRENT OF (SQL level),
/// MERGE RETURNING * source-first order, BEFORE UPDATE trigger
/// images (RETURNING / WITH CHECK / ON CONFLICT DO UPDATE), the
/// ON CONFLICT DO UPDATE RLS arbiter check, DROP OWNED BY policy
/// handling, ALTER TABLE ADD COLUMN ... GENERATED, and the
/// pg_depend / pg_shdepend policy edges + DROP ROLE policy blocker.
/// Ground truth: PG19 execCurrent.c, rowsecurity.c, policy.c;
/// vendored rowsecurity / portals suites. Scope record:
/// hidden_files/v247/scope.md.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run_as(eng: &mut Engine, role: &str, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        privs_suppressed: false,
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

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_as(eng, "postgres", sql)
}

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn ok_as(eng: &mut Engine, role: &str, sql: &str) {
    run_as(eng, role, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    match run(eng, sql) {
        Ok(_) => panic!("{sql} -> expected error, got success"),
        Err(e) => e,
    }
}

fn err_as(eng: &mut Engine, role: &str, sql: &str) -> ExecError {
    match run_as(eng, role, sql) {
        Ok(_) => panic!("{sql} -> expected error, got success"),
        Err(e) => e,
    }
}

fn one(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => {
            assert_eq!(rows.len(), 1, "{sql}");
            rows[0]
                .iter()
                .map(|v| v.to_text().unwrap_or_default())
                .collect()
        }
        other => panic!("expected Select for {sql} -> {other:?}"),
    }
}

fn tag_of(r: &ExecResult) -> &str {
    match r {
        ExecResult::Dml { tag, .. } => tag,
        other => panic!("expected Dml, got {other:?}"),
    }
}

#[test]
fn v247_current_of_parse() {
    let stmt = parse_statement("UPDATE t SET a = 1 WHERE CURRENT OF cur1").unwrap();
    match stmt {
        Stmt::Update {
            where_: Some(Expr::CurrentOf(n)),
            ..
        } => assert_eq!(n, "cur1"),
        other => panic!("unexpected parse: {other:?}"),
    }
    let stmt = parse_statement("DELETE FROM t WHERE CURRENT OF cur2 RETURNING *").unwrap();
    match stmt {
        Stmt::Delete {
            where_: Some(Expr::CurrentOf(n)),
            ..
        } => assert_eq!(n, "cur2"),
        other => panic!("unexpected parse: {other:?}"),
    }
    // An ordinary WHERE still parses as an expression.
    let stmt = parse_statement("UPDATE t SET a = 1 WHERE a = 2").unwrap();
    match stmt {
        Stmt::Update {
            where_: Some(Expr::Cmp { .. }),
            ..
        } => {}
        other => panic!("unexpected parse: {other:?}"),
    }
}

#[test]
fn v247_current_of_exec_matches_installed_row() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text)");
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, 'z')",
    );
    // No installed target: the executor refuses (0A000).
    let e = err(&mut eng, "UPDATE t SET b = 'q' WHERE CURRENT OF c");
    assert_eq!(e.code, "0A000");
    // Install the second row (ctid offset 1) and update exactly it.
    set_current_of_row(Some(("t".to_string(), 1)));
    let r = run(&mut eng, "UPDATE t SET b = 'Y' WHERE CURRENT OF c").unwrap();
    assert_eq!(tag_of(&r), "UPDATE 1");
    set_current_of_row(Some(("t".to_string(), 0)));
    let r = run(&mut eng, "DELETE FROM t WHERE CURRENT OF c").unwrap();
    assert_eq!(tag_of(&r), "DELETE 1");
    set_current_of_row(None);
    assert_eq!(
        one(&mut eng, "SELECT string_agg(b, ',' ORDER BY a) FROM t"),
        vec!["Y,z".to_string()]
    );
}

#[test]
fn v247_merge_returning_star_is_source_first() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE s (a int, b text)");
    ok(&mut eng, "CREATE TABLE t (a int, b text, c text)");
    ok(&mut eng, "INSERT INTO s VALUES (1, 'SB')");
    ok(&mut eng, "INSERT INTO t VALUES (1, 'TB', 'TC')");
    let r = run(
        &mut eng,
        "MERGE INTO t USING s ON t.a = s.a WHEN MATCHED THEN UPDATE SET b = s.b RETURNING *",
    )
    .unwrap();
    match r {
        ExecResult::Dml { columns, rows, .. } => {
            let names: Vec<&str> = columns.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(names, vec!["a", "b", "a", "b", "c"]);
            let vals: Vec<String> = rows[0]
                .iter()
                .map(|v| v.to_text().unwrap_or_default())
                .collect();
            assert_eq!(vals, vec!["1", "SB", "1", "SB", "TC"]);
        }
        other => panic!("expected Dml, got {other:?}"),
    }
}

#[test]
fn v247_before_update_trigger_image_in_returning() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text)");
    ok(&mut eng, "INSERT INTO t VALUES (1, 'x')");
    ok(
        &mut eng,
        "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.b := 'TRIG'; RETURN NEW; END $$",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION f()",
    );
    let r = run(&mut eng, "UPDATE t SET b = 'y' RETURNING b").unwrap();
    match r {
        ExecResult::Dml { rows, .. } => {
            assert_eq!(rows[0][0].to_text().as_deref(), Some("TRIG"));
        }
        other => panic!("expected Dml, got {other:?}"),
    }
    assert_eq!(one(&mut eng, "SELECT b FROM t"), vec!["TRIG".to_string()]);
}

#[test]
fn v247_on_conflict_do_update_fires_before_update_trigger() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int PRIMARY KEY, b text)");
    ok(&mut eng, "INSERT INTO t VALUES (1, 'x')");
    ok(
        &mut eng,
        "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.b := 'TRIG'; RETURN NEW; END $$",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION f()",
    );
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1, 'y') ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
    );
    assert_eq!(one(&mut eng, "SELECT b FROM t"), vec!["TRIG".to_string()]);
}

#[test]
fn v247_on_conflict_do_update_rls_arbiter_check() {
    let mut eng = engine();
    ok(&mut eng, "CREATE USER u1");
    ok(&mut eng, "CREATE TABLE t (a int PRIMARY KEY, b text)");
    ok(&mut eng, "INSERT INTO t VALUES (1, 'one')");
    ok(&mut eng, "GRANT SELECT, INSERT, UPDATE ON t TO u1");
    ok(&mut eng, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    ok(
        &mut eng,
        "CREATE POLICY psel ON t FOR SELECT TO u1 USING (b <> 'ZZZ')",
    );
    ok(
        &mut eng,
        "CREATE POLICY pins ON t FOR INSERT TO u1 WITH CHECK (true)",
    );
    ok(
        &mut eng,
        "CREATE POLICY pupd ON t FOR UPDATE TO u1 USING (true) WITH CHECK (true)",
    );
    // PG19 (rowsecurity.c): for DO UPDATE the SELECT policies join
    // as WithCheckOptions ALWAYS (arbiter inference reads the
    // conflicting row), not only with RETURNING — a new row that
    // violates the SELECT policy's qual is rejected 42501 even
    // though the UPDATE policy itself allows it.
    let e = err_as(
        &mut eng,
        "u1",
        "INSERT INTO t VALUES (1, 'x') ON CONFLICT (a) DO UPDATE SET b = 'ZZZ'",
    );
    assert_eq!(e.code, "42501");
    // Control: a conforming update goes through.
    ok_as(
        &mut eng,
        "u1",
        "INSERT INTO t VALUES (1, 'x') ON CONFLICT (a) DO UPDATE SET b = 'ok'",
    );
}

#[test]
fn v247_drop_owned_strips_policy_roles() {
    let mut eng = engine();
    ok(&mut eng, "CREATE USER r1");
    ok(&mut eng, "CREATE USER r2");
    ok(&mut eng, "CREATE TABLE t (a int)");
    ok(&mut eng, "CREATE POLICY p ON t TO r1, r2 USING (true)");
    // Removing r1 leaves the policy (r2 remains); removing r2 then
    // drops it entirely.
    ok(&mut eng, "DROP OWNED BY r1");
    ok(&mut eng, "DROP OWNED BY r2");
    let e = err(&mut eng, "DROP POLICY p ON t");
    assert_eq!(e.code, "42704");
    // A sole-role policy is dropped by DROP OWNED BY.
    ok(&mut eng, "CREATE USER r3");
    ok(&mut eng, "CREATE POLICY q ON t TO r3 USING (true)");
    ok(&mut eng, "DROP OWNED BY r3");
    let e = err(&mut eng, "DROP POLICY q ON t");
    assert_eq!(e.code, "42704");
}

#[test]
fn v247_add_column_generated_backfills() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    ok(&mut eng, "INSERT INTO t VALUES (3)");
    ok(
        &mut eng,
        "ALTER TABLE t ADD COLUMN c int GENERATED ALWAYS AS (a * 100) VIRTUAL",
    );
    assert_eq!(one(&mut eng, "SELECT c FROM t"), vec!["300".to_string()]);
    ok(&mut eng, "INSERT INTO t VALUES (4)");
    assert_eq!(
        one(&mut eng, "SELECT c FROM t WHERE a = 4"),
        vec!["400".to_string()]
    );
    ok(
        &mut eng,
        "ALTER TABLE t ADD COLUMN d int GENERATED ALWAYS AS (a + 1) STORED",
    );
    assert_eq!(
        one(&mut eng, "SELECT d FROM t WHERE a = 3"),
        vec!["4".to_string()]
    );
}

#[test]
fn v247_pg_depend_and_shdepend_policy_edges() {
    let mut eng = engine();
    ok(&mut eng, "CREATE USER bob");
    ok(&mut eng, "CREATE USER carol");
    ok(&mut eng, "CREATE TABLE dep1 (c1 int)");
    ok(&mut eng, "CREATE TABLE dep2 (c1 int)");
    ok(
        &mut eng,
        "CREATE POLICY dep_p1 ON dep1 TO bob USING (c1 > (SELECT max(dep2.c1) FROM dep2))",
    );
    // The policy's own-table edge: exactly one 'a' row to pg_class.
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_depend WHERE classid = 'pg_policy'::regclass AND refclassid = 'pg_class'::regclass AND deptype = 'a'"
        ),
        vec!["1".to_string()]
    );
    // The expression's dep2 edge exists, and the role edge is 'r'.
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) = 1 FROM pg_depend WHERE objid = (SELECT oid FROM pg_policy WHERE polname = 'dep_p1') AND refobjid = (SELECT oid FROM pg_class WHERE relname = 'dep2')"
        ),
        vec!["t".to_string()]
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) = 1 FROM pg_shdepend WHERE objid = (SELECT oid FROM pg_policy WHERE polname = 'dep_p1') AND refobjid = (SELECT oid FROM pg_authid WHERE rolname = 'bob') AND deptype = 'r'"
        ),
        vec!["t".to_string()]
    );
    // Rewriting the qual drops the dep2 edge.
    ok(&mut eng, "ALTER POLICY dep_p1 ON dep1 USING (true)");
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) = 0 FROM pg_depend WHERE objid = (SELECT oid FROM pg_policy WHERE polname = 'dep_p1') AND refobjid = (SELECT oid FROM pg_class WHERE relname = 'dep2')"
        ),
        vec!["t".to_string()]
    );
}

#[test]
fn v247_drop_role_blocked_by_policy_target() {
    let mut eng = engine();
    ok(&mut eng, "CREATE USER victim");
    ok(&mut eng, "CREATE TABLE t (a int)");
    ok(&mut eng, "CREATE POLICY p ON t TO victim USING (true)");
    let e = err(&mut eng, "DROP ROLE victim");
    assert_eq!(e.code, "2BP01");
    let detail = e.detail.clone().unwrap_or_default();
    assert!(
        detail.contains("target of policy p on table t"),
        "detail: {detail}"
    );
    // Once the policy is gone the drop succeeds.
    ok(&mut eng, "DROP POLICY p ON t");
    ok(&mut eng, "DROP ROLE victim");
}
