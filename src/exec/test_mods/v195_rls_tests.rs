/// v1.95: ROW-LEVEL SECURITY — CREATE/ALTER/DROP POLICY,
/// ALTER TABLE {ENABLE|DISABLE|FORCE|NO FORCE} ROW LEVEL SECURITY,
/// USING / WITH CHECK evaluation for SELECT/INSERT/UPDATE/DELETE,
/// permissive/restrictive combination, pg_policies / pg_policy /
/// pg_class surface, BYPASSRLS, row_security=off error.
/// Ground truth: PG19 src/test/regress/sql/rowsecurity.sql and
/// src/backend/{rewrite/rowsecurity.c,utils/misc/rls.c,commands/policy.c}.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run_as(eng: &mut Engine, sql: &str, role: &str) -> Result<ExecResult, ExecError> {
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
        role,
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    execute(eng, &mut ctx, &stmt)
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_as(eng, sql, "postgres")
}

fn rows_as(eng: &mut Engine, sql: &str, role: &str) -> Vec<Vec<Option<String>>> {
    match run_as(eng, sql, role)
        .unwrap_or_else(|e| panic!("{sql} as {role} -> {} {}", e.code, e.message))
    {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    rows_as(eng, sql, "postgres")
}

fn err_as(eng: &mut Engine, sql: &str, role: &str) -> ExecError {
    match run_as(eng, sql, role) {
        Err(e) => e,
        Ok(r) => panic!("expected error for {sql} as {role}, got {r:?}"),
    }
}

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    err_as(eng, sql, "postgres")
}

/// Fixture: table acct (owner postgres) rows 1..4, user bob with full
/// grants, RLS enabled.
fn fixture() -> Engine {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE acct (a int, b text)").unwrap();
    run(
        &mut eng,
        "INSERT INTO acct VALUES (1,'x'),(2,'y'),(3,'z'),(4,'w')",
    )
    .unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON acct TO bob").unwrap();
    run(&mut eng, "ALTER TABLE acct ENABLE ROW LEVEL SECURITY").unwrap();
    eng
}

#[test]
fn v195_default_deny_without_policies() {
    let mut eng = fixture();
    // No policy: bob sees nothing; the superuser bypasses.
    assert_eq!(
        rows_as(&mut eng, "SELECT a FROM acct ORDER BY a", "bob").len(),
        0
    );
    assert_eq!(rows(&mut eng, "SELECT a FROM acct ORDER BY a").len(), 4);
}

#[test]
fn v195_select_using_filter() {
    let mut eng = fixture();
    run(&mut eng, "CREATE POLICY p1 ON acct USING (a <= 2)").unwrap();
    let got: Vec<_> = rows_as(&mut eng, "SELECT a FROM acct ORDER BY a", "bob")
        .iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(got, vec!["1", "2"]);
}

#[test]
fn v195_force_owner_and_row_security_off() {
    let mut eng = engine();
    run(&mut eng, "CREATE USER alice LOGIN").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "CREATE TABLE t2 (a int)").unwrap();
    run(&mut eng, "INSERT INTO t2 VALUES (1), (5)").unwrap();
    run(&mut eng, "ALTER TABLE t2 OWNER TO alice").unwrap();
    run(&mut eng, "GRANT ALL ON t2 TO alice, bob").unwrap();
    run(&mut eng, "ALTER TABLE t2 ENABLE ROW LEVEL SECURITY").unwrap();
    run(&mut eng, "CREATE POLICY p ON t2 USING (a < 3)").unwrap();
    // Owner bypasses without FORCE.
    assert_eq!(
        rows_as(&mut eng, "SELECT a FROM t2 ORDER BY a", "alice").len(),
        2
    );
    run(&mut eng, "ALTER TABLE t2 FORCE ROW LEVEL SECURITY").unwrap();
    let got: Vec<_> = rows_as(&mut eng, "SELECT a FROM t2 ORDER BY a", "alice")
        .iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(got, vec!["1"]);
    // row_security = off: bob (non-exempt) errors 42501.
    crate::exec::set_row_security(false);
    let e = err_as(&mut eng, "SELECT a FROM t2", "bob");
    assert_eq!(e.code, "42501");
    assert!(
        e.message
            .contains("query would be affected by row-level security policy for table \"t2\""),
        "{}",
        e.message
    );
    // Superuser still bypasses with row_security off.
    assert_eq!(rows(&mut eng, "SELECT a FROM t2 ORDER BY a").len(), 2);
    crate::exec::set_row_security(true);
    // NO FORCE restores owner bypass.
    run(&mut eng, "ALTER TABLE t2 NO FORCE ROW LEVEL SECURITY").unwrap();
    assert_eq!(
        rows_as(&mut eng, "SELECT a FROM t2 ORDER BY a", "alice").len(),
        2
    );
}

#[test]
fn v195_insert_with_check_and_using_fallback() {
    let mut eng = fixture();
    run(
        &mut eng,
        "CREATE POLICY pi ON acct FOR INSERT WITH CHECK (a < 10)",
    )
    .unwrap();
    run(&mut eng, "CREATE POLICY pa ON acct FOR ALL USING (a <= 3)").unwrap();
    // Permissive checks OR together: pi(a<10) OR pa(a<=3 as fallback).
    run_as(&mut eng, "INSERT INTO acct VALUES (5, 'q')", "bob").unwrap();
    let e = err_as(&mut eng, "INSERT INTO acct VALUES (50, 'q')", "bob");
    assert_eq!(e.code, "42501");
    assert!(
        e.message
            .contains("new row violates row-level security policy"),
        "{}",
        e.message
    );
}

#[test]
fn v195_insert_default_deny_names_no_policy() {
    let mut eng = fixture();
    run(&mut eng, "CREATE POLICY ps ON acct FOR SELECT USING (true)").unwrap();
    let e = err_as(&mut eng, "INSERT INTO acct VALUES (9, 'q')", "bob");
    assert_eq!(e.code, "42501");
    assert_eq!(
        e.message,
        "new row violates row-level security policy for table \"acct\""
    );
}

#[test]
fn v195_update_using_and_check() {
    let mut eng = fixture();
    run(
        &mut eng,
        "CREATE POLICY pu ON acct FOR UPDATE USING (a <= 2) WITH CHECK (a <= 3)",
    )
    .unwrap();
    run(&mut eng, "CREATE POLICY ps ON acct FOR SELECT USING (true)").unwrap();
    // Row 3 invisible to the UPDATE policy: silently skipped.
    run_as(&mut eng, "UPDATE acct SET b = 'n' WHERE a = 3", "bob").unwrap();
    let b3 = rows(&mut eng, "SELECT b FROM acct WHERE a = 3");
    assert_eq!(b3[0][0].as_deref(), Some("z"));
    // Row 2 updatable; new value 3 passes WITH CHECK.
    run_as(&mut eng, "UPDATE acct SET a = 3 WHERE a = 2", "bob").unwrap();
    // New value 9 violates WITH CHECK -> 42501 naming pu.
    let e = err_as(&mut eng, "UPDATE acct SET a = 9 WHERE a = 1", "bob");
    assert_eq!(e.code, "42501");
    assert!(e.message.contains("\"pu\""), "{}", e.message);
}

#[test]
fn v195_delete_using() {
    let mut eng = fixture();
    run(
        &mut eng,
        "CREATE POLICY pd ON acct FOR DELETE USING (a <= 2)",
    )
    .unwrap();
    run(&mut eng, "CREATE POLICY ps ON acct FOR SELECT USING (true)").unwrap();
    run_as(&mut eng, "DELETE FROM acct", "bob").unwrap();
    let got: Vec<_> = rows(&mut eng, "SELECT a FROM acct ORDER BY a")
        .iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(got, vec!["3", "4"]);
}

#[test]
fn v195_restrictive_and_permissive_combination() {
    let mut eng = fixture();
    run(&mut eng, "CREATE POLICY pp ON acct USING (a <= 3)").unwrap();
    run(
        &mut eng,
        "CREATE POLICY pr ON acct AS RESTRICTIVE USING (a <> 2)",
    )
    .unwrap();
    let got: Vec<_> = rows_as(&mut eng, "SELECT a FROM acct ORDER BY a", "bob")
        .iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(got, vec!["1", "3"]);
}

#[test]
fn v195_to_roles_and_groups() {
    let mut eng = fixture();
    run(&mut eng, "CREATE USER carol LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON acct TO carol").unwrap();
    run(&mut eng, "CREATE POLICY pc ON acct TO carol USING (a <= 1)").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "bob").len(), 0);
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "carol").len(), 1);
    // Group membership: bob in group g1, policy TO g1.
    run(&mut eng, "CREATE ROLE g1 NOLOGIN").unwrap();
    run(&mut eng, "GRANT g1 TO bob").unwrap();
    run(&mut eng, "CREATE POLICY pg ON acct TO g1 USING (a <= 4)").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "bob").len(), 4);
}

#[test]
fn v195_policy_ddl_errors() {
    let mut eng = fixture();
    run(&mut eng, "CREATE POLICY p1 ON acct USING (a > 0)").unwrap();
    let e = err(&mut eng, "CREATE POLICY p1 ON acct USING (a > 0)");
    assert_eq!(e.code, "42710");
    let e = err(
        &mut eng,
        "CREATE POLICY px ON acct FOR INSERT USING (a > 0)",
    );
    assert_eq!(e.code, "42601");
    let e = err(
        &mut eng,
        "CREATE POLICY py ON acct FOR SELECT WITH CHECK (a > 0)",
    );
    assert_eq!(e.code, "42601");
    assert!(
        e.message
            .contains("WITH CHECK cannot be applied to SELECT or DELETE")
    );
    let e = err(
        &mut eng,
        "CREATE POLICY pz ON acct TO nosuchrole USING (a > 0)",
    );
    assert_eq!(e.code, "42704");
    let e = err(&mut eng, "DROP POLICY nosuch ON acct");
    assert_eq!(e.code, "42704");
    run(&mut eng, "DROP POLICY IF EXISTS nosuch ON acct").unwrap();
    let e = err(&mut eng, "ALTER POLICY nosuch ON acct USING (a > 0)");
    assert_eq!(e.code, "42704");
    let e = err(&mut eng, "CREATE POLICY pq ON missing_table USING (a > 0)");
    assert_eq!(e.code, "42P01");
}

#[test]
fn v195_alter_policy_rename_and_replace() {
    let mut eng = fixture();
    run(&mut eng, "CREATE POLICY p1 ON acct USING (a <= 2)").unwrap();
    run(&mut eng, "ALTER POLICY p1 ON acct USING (a <= 1)").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "bob").len(), 1);
    run(&mut eng, "ALTER POLICY p1 ON acct RENAME TO p2").unwrap();
    let names = rows(
        &mut eng,
        "SELECT policyname FROM pg_policies WHERE tablename = 'acct'",
    );
    assert_eq!(names[0][0].as_deref(), Some("p2"));
    run(&mut eng, "DROP POLICY p2 ON acct").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "bob").len(), 0);
}

#[test]
fn v195_bypassrls_role() {
    let mut eng = fixture();
    run(&mut eng, "CREATE USER exempt BYPASSRLS LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON acct TO exempt").unwrap();
    run(&mut eng, "CREATE POLICY p1 ON acct USING (a <= 1)").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "exempt").len(), 4);
    let v = rows(
        &mut eng,
        "SELECT rolbypassrls FROM pg_roles WHERE rolname = 'exempt'",
    );
    assert_eq!(v[0][0].as_deref(), Some("t"));
    let v = rows(
        &mut eng,
        "SELECT rolbypassrls FROM pg_roles WHERE rolname = 'bob'",
    );
    assert_eq!(v[0][0].as_deref(), Some("f"));
    run(&mut eng, "ALTER USER bob BYPASSRLS").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "bob").len(), 4);
}

#[test]
fn v195_catalog_surface() {
    let mut eng = fixture();
    run(
        &mut eng,
        "CREATE POLICY p1 ON acct AS RESTRICTIVE FOR UPDATE TO bob USING (a <= 2) WITH CHECK (a <= 3)",
    )
    .unwrap();
    let r = rows(
        &mut eng,
        "SELECT schemaname, tablename, policyname, permissive, cmd, qual, with_check \
         FROM pg_policies WHERE tablename = 'acct'",
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0].as_deref(), Some("public"));
    assert_eq!(r[0][2].as_deref(), Some("p1"));
    assert_eq!(r[0][3].as_deref(), Some("RESTRICTIVE"));
    assert_eq!(r[0][4].as_deref(), Some("UPDATE"));
    assert_eq!(r[0][5].as_deref(), Some("(a <= 2)"));
    assert_eq!(r[0][6].as_deref(), Some("(a <= 3)"));
    let c = rows(
        &mut eng,
        "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname = 'acct'",
    );
    assert_eq!(c[0][0].as_deref(), Some("t"));
    assert_eq!(c[0][1].as_deref(), Some("f"));
    let pp = rows(
        &mut eng,
        "SELECT polname, polcmd, polpermissive FROM pg_policy",
    );
    assert_eq!(pp[0][0].as_deref(), Some("p1"));
    assert_eq!(pp[0][1].as_deref(), Some("w"));
    // polpermissive is false for the RESTRICTIVE policy p1.
    assert_eq!(pp[0][2].as_deref(), Some("f"));
}

#[test]
fn v195_current_user_in_policy() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE docs (owner text, v int)").unwrap();
    run(&mut eng, "INSERT INTO docs VALUES ('bob', 1), ('carol', 2)").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "CREATE USER carol LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON docs TO bob, carol").unwrap();
    run(&mut eng, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY").unwrap();
    run(
        &mut eng,
        "CREATE POLICY own ON docs USING (owner = current_user)",
    )
    .unwrap();
    let got = rows_as(&mut eng, "SELECT v FROM docs", "bob");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0].as_deref(), Some("1"));
    let got = rows_as(&mut eng, "SELECT v FROM docs", "carol");
    assert_eq!(got[0][0].as_deref(), Some("2"));
}

#[test]
fn v195_subquery_in_policy() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE uacct (u text, lv int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO uacct VALUES ('bob', 5), ('carol', 1)",
    )
    .unwrap();
    run(&mut eng, "CREATE TABLE doc (d int, lv int)").unwrap();
    run(&mut eng, "INSERT INTO doc VALUES (1, 3), (2, 9)").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON uacct TO bob").unwrap();
    run(&mut eng, "GRANT ALL ON doc TO bob").unwrap();
    run(&mut eng, "ALTER TABLE doc ENABLE ROW LEVEL SECURITY").unwrap();
    run(
        &mut eng,
        "CREATE POLICY p ON doc USING (lv <= (SELECT lv FROM uacct WHERE u = current_user))",
    )
    .unwrap();
    let got = rows_as(&mut eng, "SELECT d FROM doc", "bob");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0].as_deref(), Some("1"));
}

#[test]
fn v195_upsert_conflict_check() {
    let mut eng = fixture();
    run(&mut eng, "ALTER TABLE acct ADD PRIMARY KEY (a)").unwrap();
    // Proposed row passes the INSERT check (a <= 10) but the
    // conflicting existing row 4 fails the UPDATE USING group
    // (a <= 3): ON CONFLICT DO UPDATE errors with the conflict
    // flavour (rowsecurity.c).
    run(
        &mut eng,
        "CREATE POLICY pins ON acct FOR INSERT WITH CHECK (a <= 10)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE POLICY pupd ON acct FOR UPDATE USING (a <= 3) WITH CHECK (a <= 10)",
    )
    .unwrap();
    let e = err_as(
        &mut eng,
        "INSERT INTO acct VALUES (4, 'q') ON CONFLICT (a) DO UPDATE SET b = 'q'",
        "bob",
    );
    assert_eq!(e.code, "42501");
    assert!(e.message.contains("(USING expression)"), "{}", e.message);
    // Visible conflict row 2 updates fine.
    run_as(
        &mut eng,
        "INSERT INTO acct VALUES (2, 'q') ON CONFLICT (a) DO UPDATE SET b = 'q'",
        "bob",
    )
    .unwrap();
}

#[test]
fn v195_codec_round_trip() {
    let mut eng = fixture();
    run(
        &mut eng,
        "CREATE POLICY p1 ON acct AS RESTRICTIVE FOR UPDATE TO bob USING (a <= 2) WITH CHECK (a <= 3)",
    )
    .unwrap();
    run(&mut eng, "ALTER TABLE acct FORCE ROW LEVEL SECURITY").unwrap();
    let t = eng
        .db
        .find_table("acct", &eng.take_snapshot(), &[9], 0)
        .unwrap()
        .clone();
    let enc = crate::sql::encode_constraints(&t);
    let dec = crate::sql::decode_constraints(&enc).unwrap();
    assert!(dec.rls_enabled);
    assert!(dec.rls_forced);
    assert_eq!(dec.policies.len(), 1);
    assert_eq!(dec.policies[0].name, "p1");
    assert!(!dec.policies[0].permissive);
    assert_eq!(dec.policies[0].cmd, PolicyCmd::Update);
    assert_eq!(dec.policies[0].roles, vec!["bob".to_string()]);
    assert!(dec.policies[0].using_expr.is_some());
    assert!(dec.policies[0].check_expr.is_some());
    // An RLS-free table's encoding decodes with defaults.
    let mut bare = t.clone();
    bare.rls_enabled = false;
    bare.rls_forced = false;
    bare.policies.clear();
    let enc2 = crate::sql::encode_constraints(&bare);
    let dec2 = crate::sql::decode_constraints(&enc2).unwrap();
    assert!(!dec2.rls_enabled && dec2.policies.is_empty());
}

#[test]
fn v195_public_acl_applies_to_everyone() {
    // GRANT ... TO PUBLIC stores an ACL entry for the pseudo-role;
    // the privilege computation must honor it for any role (PG19
    // ACL_ID_PUBLIC) — before this, public grants were dead letters.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE pt (a int)").unwrap();
    run(&mut eng, "INSERT INTO pt VALUES (1)").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    let e = err_as(&mut eng, "SELECT a FROM pt", "bob");
    assert_eq!(e.code, "42501");
    run(&mut eng, "GRANT SELECT ON pt TO public").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM pt", "bob").len(), 1);
    run(&mut eng, "REVOKE SELECT ON pt FROM public").unwrap();
    let e = err_as(&mut eng, "SELECT a FROM pt", "bob");
    assert_eq!(e.code, "42501");
}

#[test]
fn v195_multi_object_grant() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE g1 (a int)").unwrap();
    run(&mut eng, "CREATE TABLE g2 (a int)").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "GRANT SELECT ON g1, g2 TO bob").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM g1", "bob").len(), 0);
    assert_eq!(rows_as(&mut eng, "SELECT a FROM g2", "bob").len(), 0);
    run(&mut eng, "INSERT INTO g1 VALUES (1)").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM g1", "bob").len(), 1);
}

#[test]
fn v195_view_options_parse_and_store() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE base (a int)").unwrap();
    run(
        &mut eng,
        "CREATE VIEW v1 WITH (security_invoker, security_barrier) AS SELECT * FROM base",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE VIEW v2 AS SELECT * FROM base WHERE a > 0 WITH CASCADED CHECK OPTION",
    )
    .unwrap();
    let v1 = eng.db.find_view("v1", &eng.take_snapshot(), 9).unwrap();
    assert!(v1.security_invoker);
    assert!(!v1.check_option);
    let v2 = eng.db.find_view("v2", &eng.take_snapshot(), 9).unwrap();
    assert!(!v2.security_invoker);
    assert!(v2.check_option);
    assert!(
        v2.query.to_ascii_lowercase().ends_with("a > 0"),
        "{}",
        v2.query
    );
    // Views are selectable.
    run(&mut eng, "INSERT INTO base VALUES (1), (-1)").unwrap();
    assert_eq!(rows(&mut eng, "SELECT a FROM v2").len(), 1);
    // Unknown option is an error.
    let e = err(
        &mut eng,
        "CREATE VIEW v3 WITH (nonsense) AS SELECT * FROM base",
    );
    assert_eq!(e.code, "42601");
}

#[test]
fn v195_codec_round_trip_subquery_policy() {
    // Regression: a policy whose WITH CHECK holds a scalar subquery
    // (update.sql's policy_range_parted_subplan) encoded as `(invalid)`
    // through the s-expr codec's catch-all, which made WAL replay
    // hard-fail ("bad expr head invalid"). Policy exprs now persist
    // via their captured source SQL.
    let mut eng = fixture();
    run(&mut eng, "CREATE TABLE mintab (c1 int)").unwrap();
    run(&mut eng, "INSERT INTO mintab VALUES (120)").unwrap();
    run(
        &mut eng,
        "CREATE POLICY subp ON acct AS RESTRICTIVE FOR UPDATE USING (true) \
         WITH CHECK ((SELECT a <= c1 FROM mintab))",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE POLICY inp ON acct USING (a IN (SELECT c1 FROM mintab))",
    )
    .unwrap();
    let t = eng
        .db
        .find_table("acct", &eng.take_snapshot(), &[9], 0)
        .unwrap()
        .clone();
    let enc = crate::sql::encode_constraints(&t);
    assert!(enc.contains("sql"), "policy exprs persist as sql: {enc}");
    assert!(!enc.contains("invalid"), "no (invalid) catch-all: {enc}");
    let dec = crate::sql::decode_constraints(&enc).unwrap();
    assert_eq!(dec.policies.len(), 2);
    assert!(dec.policies[0].check_expr.is_some());
    assert!(dec.policies[0].check_sql.is_some());
    assert!(dec.policies[1].using_expr.is_some());
    // And the decoded form re-encodes identically (stable round-trip).
    let mut t2 = t.clone();
    t2.policies = dec.policies.clone();
    assert_eq!(crate::sql::encode_constraints(&t2), enc);
}

#[test]
fn v195_disable_rls() {
    let mut eng = fixture();
    run(&mut eng, "CREATE POLICY p1 ON acct USING (a <= 1)").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "bob").len(), 1);
    run(&mut eng, "ALTER TABLE acct DISABLE ROW LEVEL SECURITY").unwrap();
    assert_eq!(rows_as(&mut eng, "SELECT a FROM acct", "bob").len(), 4);
}
