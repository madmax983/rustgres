/// v2.37: ROW SECURITY REMAINDER — CREATE FUNCTION options
/// (LEAKPROOF / strictness / SQL-standard RETURN body), record-typed
/// function arguments in policy quals, legacy COPY option syntax,
/// COPY (query) TO row production, the RLS recursion guard (42P17),
/// CTAS VALUES / TABLE, SELECT INTO, ALTER VIEW ... OWNER TO,
/// self-join UPDATE alias rules (42712 keyed on the effective
/// alias), the enable_self_join_elimination GUC, row_security_active
/// / set_config, schema-qualified (public.) function resolution,
/// string_to_array, policy-dependency DROP rules (2BP01 / CASCADE),
/// aggregate rejection in policy quals (42803), RETURNING *
/// expansion over FROM/USING/source relations, and the COPY FROM
/// RLS gate (0A000).
/// Ground truth: PG19 src/test/regress/sql/rowsecurity.sql,
/// src/backend/rewrite/rowsecurity.c, src/backend/commands/copy.c,
/// src/backend/parser/gram.y (legacy COPY options).
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

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn v237_create_function_options_and_return_body() {
    let mut eng = engine();
    // LEAKPROOF + SQL-standard RETURN body (test_setup's fipshash).
    run(
        &mut eng,
        "CREATE FUNCTION fipshash(text) RETURNS text LANGUAGE sql IMMUTABLE \
         LEAKPROOF STRICT AS $$ SELECT substr(encode(sha256($1::bytea), 'hex'), 1, 10) $$",
    )
    .unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT fipshash('hello')"),
        vec![vec![s("2cf24dba5f")]]
    );
    // STRICT: NULL in, NULL out, body not evaluated.
    assert_eq!(rows(&mut eng, "SELECT fipshash(NULL)"), vec![vec![None]]);
    run(
        &mut eng,
        "CREATE FUNCTION plus1(int) RETURNS int LANGUAGE sql RETURN $1 + 1",
    )
    .unwrap();
    assert_eq!(rows(&mut eng, "SELECT plus1(41)"), vec![vec![s("42")]]);
    // Schema-qualified call resolves to the bare stored function.
    assert_eq!(
        rows(&mut eng, "SELECT public.fipshash('hello')"),
        vec![vec![s("2cf24dba5f")]]
    );
    // RETURNS NULL ON NULL INPUT is the STRICT spelling.
    run(
        &mut eng,
        "CREATE FUNCTION nn(int) RETURNS int LANGUAGE sql RETURNS NULL ON NULL INPUT \
         RETURN $1 * 2",
    )
    .unwrap();
    assert_eq!(rows(&mut eng, "SELECT nn(NULL)"), vec![vec![None]]);
}

#[test]
fn v237_record_typed_policy_function() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (a int, b text)").unwrap();
    run(&mut eng, "INSERT INTO t VALUES (1, 'x'), (2, 'y')").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON t TO bob").unwrap();
    run(&mut eng, "ALTER TABLE t ENABLE ROW LEVEL SECURITY").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION pf(record) RETURNS bool LANGUAGE sql AS $$ SELECT true $$",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE POLICY p ON t FOR SELECT TO bob USING (pf(t))",
    )
    .unwrap();
    // The whole-row argument must coerce to record (was 42704).
    assert_eq!(
        rows_as(&mut eng, "SELECT a FROM t ORDER BY a", "bob").len(),
        2
    );
}

#[test]
fn v237_rls_recursion_guard() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE rec1 (x int, y int)").unwrap();
    run(&mut eng, "INSERT INTO rec1 VALUES (1, 1)").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON rec1 TO bob").unwrap();
    run(
        &mut eng,
        "CREATE POLICY r1 ON rec1 USING (x = (SELECT r.x FROM rec1 r WHERE y = r.y))",
    )
    .unwrap();
    run(&mut eng, "ALTER TABLE rec1 ENABLE ROW LEVEL SECURITY").unwrap();
    let e = err_as(&mut eng, "SELECT * FROM rec1", "bob");
    assert_eq!(e.code, "42P17");
    assert!(e.message.contains("infinite recursion detected in policy"));
}

#[test]
fn v237_ctas_values_and_select_into() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE coll_t (c) AS VALUES ('bar'::text)").unwrap();
    assert_eq!(rows(&mut eng, "SELECT * FROM coll_t"), vec![vec![s("bar")]]);
    run(&mut eng, "CREATE TABLE doc (did int, dlevel int)").unwrap();
    run(&mut eng, "INSERT INTO doc VALUES (1, 1), (2, 3)").unwrap();
    run(&mut eng, "SELECT * INTO t4 FROM doc").unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT count(*) FROM t4"),
        vec![vec![s("2")]]
    );
    run(&mut eng, "CREATE TABLE t5 AS TABLE doc").unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT count(*) FROM t5"),
        vec![vec![s("2")]]
    );
}

#[test]
fn v237_alter_view_owner_to() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE base (a int)").unwrap();
    run(&mut eng, "CREATE VIEW v1 AS SELECT * FROM base").unwrap();
    run(&mut eng, "CREATE USER carol LOGIN").unwrap();
    let r = run(&mut eng, "ALTER VIEW v1 OWNER TO carol").unwrap();
    match r {
        ExecResult::Command { tag } => assert_eq!(tag, "ALTER VIEW"),
        other => panic!("expected Command, got {other:?}"),
    }
    let e = err(&mut eng, "ALTER VIEW v1 OWNER TO no_such_role");
    assert_eq!(e.code, "42704");
}

#[test]
fn v237_update_self_join_alias_rules() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t2 (a int, b text)").unwrap();
    run(&mut eng, "INSERT INTO t2 VALUES (3, 'cde')").unwrap();
    // Distinct aliases for target and FROM occurrence: legal.
    run(
        &mut eng,
        "UPDATE t2 t2_1 SET b = t2_2.b FROM t2 t2_2 WHERE t2_1.a = 3 AND t2_2.a = t2_1.a",
    )
    .unwrap();
    // Same effective name twice: still 42712.
    let e = err(&mut eng, "UPDATE t2 SET b = 'x' FROM t2 WHERE t2.a = 3");
    assert_eq!(e.code, "42712");
}

#[test]
fn v237_row_security_active_and_set_config() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (a int)").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON t TO bob").unwrap();
    // Not enabled: inactive for everyone.
    assert_eq!(
        rows(&mut eng, "SELECT row_security_active('t')"),
        vec![vec![s("f")]]
    );
    run(&mut eng, "ALTER TABLE t ENABLE ROW LEVEL SECURITY").unwrap();
    // Owner (postgres) bypasses: inactive. Bob is subject: active.
    assert_eq!(
        rows(&mut eng, "SELECT row_security_active('t')"),
        vec![vec![s("f")]]
    );
    assert_eq!(
        rows_as(&mut eng, "SELECT row_security_active('t')", "bob"),
        vec![vec![s("t")]]
    );
    let e = err(&mut eng, "SELECT row_security_active('missing')");
    assert_eq!(e.code, "42P01");
    // set_config returns the value and is visible in-statement.
    assert_eq!(
        rows(&mut eng, "SELECT set_config('x.y', 'z', false)"),
        vec![vec![s("z")]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT set_config('x.y', 'z', true), current_setting('x.y')"
        ),
        vec![vec![s("z"), s("z")]]
    );
}

#[test]
fn v237_string_to_array() {
    let mut eng = engine();
    assert_eq!(
        rows(&mut eng, "SELECT string_to_array('a:b:c', ':')"),
        vec![vec![s("{a,b,c}")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT string_to_array('a::b', ':', '')"),
        vec![vec![Some("{a,NULL,b}".to_string())]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT (string_to_array('x:y', ':'))[2]"),
        vec![vec![s("y")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT string_to_array('', ',')"),
        vec![vec![s("{}")]]
    );
    // PG19 split_text: an empty delimiter yields the whole string
    // as one element; a NULL delimiter splits into characters; the
    // null-string substitution applies to the single element too.
    assert_eq!(
        rows(&mut eng, "SELECT string_to_array('1|2|3', '')"),
        vec![vec![s("{1|2|3}")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT string_to_array('1|2|3', NULL)"),
        vec![vec![s("{1,|,2,|,3}")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT string_to_array('abc', '', 'abc')"),
        vec![vec![Some("{NULL}".to_string())]]
    );
}

#[test]
fn v237_policy_dependency_drop_rules() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE dependee (x int, y int)").unwrap();
    run(&mut eng, "CREATE TABLE dependent (x int, y int)").unwrap();
    run(
        &mut eng,
        "CREATE POLICY d1 ON dependent FOR ALL TO PUBLIC \
         USING (x = (SELECT d.x FROM dependee d WHERE d.y = y))",
    )
    .unwrap();
    let e = err(&mut eng, "DROP TABLE dependee");
    assert_eq!(e.code, "2BP01");
    run(&mut eng, "DROP TABLE dependee CASCADE").unwrap();
    // The dependent policy was dropped with it.
    assert_eq!(
        rows(&mut eng, "SELECT count(*) FROM pg_policy"),
        vec![vec![s("0")]]
    );
}

#[test]
fn v237_aggregate_in_policy_rejected() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (a int, c int)").unwrap();
    let e = err(&mut eng, "CREATE POLICY p ON t USING (max(c) > 1)");
    assert_eq!(e.code, "42803");
}

#[test]
fn v237_returning_star_covers_from_and_source() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE r1 (a int)").unwrap();
    run(&mut eng, "CREATE TABLE r2 (a int)").unwrap();
    run(&mut eng, "INSERT INTO r1 VALUES (1)").unwrap();
    run(&mut eng, "INSERT INTO r2 VALUES (1)").unwrap();
    // UPDATE ... FROM: RETURNING * = target cols + FROM cols.
    match run(
        &mut eng,
        "UPDATE r1 SET a = r2.a + 2 FROM r2 WHERE r1.a = r2.a RETURNING *",
    )
    .unwrap()
    {
        ExecResult::Dml { columns, rows, .. } => {
            assert_eq!(columns.len(), 2);
            assert_eq!(rows.len(), 1);
        }
        other => panic!("expected Dml, got {other:?}"),
    }
    // DELETE ... USING: same rule.
    match run(
        &mut eng,
        "DELETE FROM r1 USING r2 WHERE r1.a = r2.a + 2 RETURNING *",
    )
    .unwrap()
    {
        ExecResult::Dml { columns, .. } => assert_eq!(columns.len(), 2),
        other => panic!("expected Dml, got {other:?}"),
    }
    // MERGE: RETURNING * = target cols + source cols.
    run(&mut eng, "INSERT INTO r1 VALUES (5)").unwrap();
    match run(
        &mut eng,
        "MERGE INTO r1 t USING r2 s ON t.a = s.a \
         WHEN NOT MATCHED THEN INSERT VALUES (9) RETURNING *",
    )
    .unwrap()
    {
        ExecResult::Dml { columns, .. } => assert_eq!(columns.len(), 2),
        other => panic!("expected Dml, got {other:?}"),
    }
}

#[test]
fn v237_copy_from_rls_gate() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE ct (a int, b text)").unwrap();
    run(&mut eng, "CREATE POLICY p1 ON ct USING (a % 2 = 0)").unwrap();
    run(&mut eng, "ALTER TABLE ct ENABLE ROW LEVEL SECURITY").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run(&mut eng, "GRANT ALL ON ct TO bob").unwrap();
    // As bob (subject to RLS), COPY FROM is rejected outright (0A000),
    // mirroring copy.c — even for zero rows.
    let stmt = parse_statement("COPY ct FROM STDIN").unwrap();
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
        role: "bob",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    let r = crate::exec::copy_from_rows(&mut eng, &mut ctx, "ct", &None, Vec::new());
    match r {
        Err(e) => assert_eq!(e.code, "0A000"),
        Ok(n) => panic!("expected 0A000 for COPY FROM under RLS, got {n} rows"),
    }
    let _ = stmt;
}

#[test]
fn v237_copy_legacy_option_syntax() {
    // Parser-level: legacy unparenthesized COPY options and the bare
    // WITH keyword parse to the same CopyOptions as the modern form.
    for sql in [
        "COPY t FROM stdin WITH DELIMITER ','",
        "COPY t FROM stdin WITH;",
        "COPY t TO stdout WITH DELIMITER ',' NULL ''",
        "COPY (SELECT * FROM t) TO STDOUT WITH DELIMITER ','",
        "COPY t TO STDOUT WITH (FREEZE, OIDS)",
    ] {
        let stmt =
            parse_statement(sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
        match stmt {
            Stmt::Copy { .. } => {}
            other => panic!("expected Copy for {sql}, got {other:?}"),
        }
    }
}

#[test]
fn v237_tableoid_union_branches() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t2 (a int, b text)").unwrap();
    run(&mut eng, "CREATE TABLE t3 (a int, b text)").unwrap();
    run(&mut eng, "INSERT INTO t2 VALUES (1, 'x')").unwrap();
    run(&mut eng, "INSERT INTO t3 VALUES (2, 'y')").unwrap();
    // Was 42804: regclass branch values ride as text.
    let out = rows(
        &mut eng,
        "SELECT a, b, tableoid::regclass FROM t2 \
         UNION ALL SELECT a, b, tableoid::regclass FROM t3",
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0][2], s("t2"));
    assert_eq!(out[1][2], s("t3"));
}

#[test]
fn v237_merge_policy_whole_row_under_alias() {
    // Policy quals reference the policy's own table by name even when
    // the MERGE source/target carry aliases (was 42703).
    let mut eng = engine();
    run(&mut eng, "CREATE USER alice LOGIN").unwrap();
    run(&mut eng, "CREATE USER bob LOGIN").unwrap();
    run_as(
        &mut eng,
        "CREATE TABLE src (a int PRIMARY KEY, b text)",
        "alice",
    )
    .unwrap();
    run_as(
        &mut eng,
        "ALTER TABLE src ENABLE ROW LEVEL SECURITY",
        "alice",
    )
    .unwrap();
    run_as(&mut eng, "GRANT SELECT, UPDATE ON src TO PUBLIC", "alice").unwrap();
    run_as(&mut eng, "INSERT INTO src VALUES (1, 'src a')", "alice").unwrap();
    run_as(
        &mut eng,
        "CREATE TABLE tgt (a int PRIMARY KEY, b text)",
        "alice",
    )
    .unwrap();
    run_as(
        &mut eng,
        "ALTER TABLE tgt ENABLE ROW LEVEL SECURITY",
        "alice",
    )
    .unwrap();
    run_as(
        &mut eng,
        "GRANT SELECT, INSERT, UPDATE, DELETE ON tgt TO PUBLIC",
        "alice",
    )
    .unwrap();
    run_as(
        &mut eng,
        "CREATE FUNCTION pfn(text, record) RETURNS bool LANGUAGE sql AS $$ SELECT true $$",
        "alice",
    )
    .unwrap();
    run_as(
        &mut eng,
        "CREATE POLICY sp ON src FOR SELECT USING (pfn('s', src))",
        "alice",
    )
    .unwrap();
    run_as(
        &mut eng,
        "CREATE POLICY ip ON tgt FOR INSERT WITH CHECK (pfn('i', tgt))",
        "alice",
    )
    .unwrap();
    run_as(
        &mut eng,
        "MERGE INTO tgt t USING src s ON t.a = s.a \
         WHEN NOT MATCHED THEN INSERT VALUES (1, 'tgt a')",
        "bob",
    )
    .unwrap();
    assert_eq!(
        rows_as(&mut eng, "SELECT count(*) FROM tgt", "alice"),
        vec![vec![s("1")]]
    );
}

#[test]
fn v237_select_into_placement_rules() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE sit (a int)").unwrap();
    run(&mut eng, "INSERT INTO sit VALUES (1), (2)").unwrap();
    // Statement-level SELECT INTO still works.
    run(&mut eng, "SELECT * INTO sit_copy FROM sit").unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT count(*) FROM sit_copy"),
        vec![vec![s("2")]]
    );
    // PG19 gram.y: INTO inside a subquery or an INSERT source is a
    // syntax error (42601 "INTO is not allowed here").
    let e = err(&mut eng, "SELECT * FROM (SELECT 1 INTO f) bar");
    assert_eq!(e.code, "42601");
    let e = err(&mut eng, "INSERT INTO sit SELECT 1 INTO f");
    assert_eq!(e.code, "42601");
    let e = err(&mut eng, "SELECT (SELECT 1 INTO f)");
    assert_eq!(e.code, "42601");
}

#[test]
fn v237_create_function_body_rules() {
    let mut eng = engine();
    // AS body + RETURN body is a duplicate (42601).
    let e = err(
        &mut eng,
        "CREATE FUNCTION dup_body() RETURNS int LANGUAGE sql \
         AS $$ SELECT 1 $$ RETURN 2",
    );
    assert_eq!(e.code, "42601");
    assert!(e.message.contains("duplicate function body"));
    // LEAKPROOF requires a superuser (42501); NOT LEAKPROOF is fine.
    run(&mut eng, "CREATE ROLE fn_alice LOGIN").unwrap();
    let e = err_as(
        &mut eng,
        "CREATE FUNCTION lp1(int) RETURNS int LANGUAGE sql LEAKPROOF \
         AS $$ SELECT $1 $$",
        "fn_alice",
    );
    assert_eq!(e.code, "42501");
    run_as(
        &mut eng,
        "CREATE FUNCTION lp2(int) RETURNS int LANGUAGE sql NOT LEAKPROOF \
         AS $$ SELECT $1 $$",
        "fn_alice",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION lp3(int) RETURNS int LANGUAGE sql LEAKPROOF \
         AS $$ SELECT $1 $$",
    )
    .unwrap();
}

#[test]
fn v237_copy_option_validation() {
    let perr = |sql: &str| parse_statement(sql).unwrap_err();
    // Redundant option (42601).
    let e = perr("COPY t FROM stdin (format csv, force_not_null (a), force_not_null (b))");
    assert_eq!(e.code, "42601");
    // FORCE_NOT_NULL is FROM-only (22023 with COPY TO).
    let e = perr("COPY t TO stdout (format csv, force_not_null (a))");
    assert_eq!(e.code, "22023");
    // FORCE_QUOTE is CSV-only and TO-only.
    let e = perr("COPY t FROM stdin (format csv, force_quote (a))");
    assert_eq!(e.code, "0A000");
    let e = perr("COPY t TO stdout (force_quote (a))");
    assert_eq!(e.code, "0A000");
    // JSON mode prohibitions and direction.
    let e = perr("COPY t TO stdout (format json, delimiter '|')");
    assert_eq!(e.code, "0A000");
    let e = perr("COPY t FROM stdin (format json)");
    assert_eq!(e.code, "22023");
    let e = perr("COPY t TO stdout (format csv, force_array true)");
    assert_eq!(e.code, "22023");
    // The valid shapes parse.
    assert!(parse_statement("COPY (SELECT 1) TO stdout (format json)").is_ok());
    assert!(parse_statement("COPY (SELECT 1) TO stdout (format json, force_array true)").is_ok());
    assert!(
        parse_statement("COPY t FROM stdin (format csv, force_not_null (a), force_null (b))")
            .is_ok()
    );
    assert!(parse_statement("COPY t TO stdout (format csv, force_quote (a))").is_ok());
}

#[test]
fn v237_plpgsql_dynamic_into_never_creates_table() {
    // PG19: inside plpgsql, SELECT ... INTO binds variables — a
    // dynamic EXECUTE string must not create a table.
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION dyn_into() RETURNS void LANGUAGE plpgsql AS $$ \
         BEGIN EXECUTE 'SELECT 10 INTO no_such_table'; END; $$",
    )
    .unwrap();
    let _ = run(&mut eng, "SELECT dyn_into()");
    let e = err(&mut eng, "SELECT * FROM no_such_table");
    assert_eq!(e.code, "42P01");
    // The SQL-level form still creates the table.
    run(&mut eng, "SELECT 10 INTO made_table").unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT * FROM made_table"),
        vec![vec![s("10")]]
    );
}

#[test]
fn v237_plpgsql_dynamic_explain_into_still_creates() {
    // PG19 plpgsql intercepts INTO only for bare SELECT/DML: an
    // EXECUTE'd EXPLAIN ANALYZE SELECT INTO creates the table.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE easi_src (a int)").unwrap();
    run(&mut eng, "INSERT INTO easi_src VALUES (1)").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION dyn_explain() RETURNS void LANGUAGE plpgsql AS $$ \
         BEGIN EXECUTE 'EXPLAIN ANALYZE SELECT * INTO TABLE easi FROM easi_src'; END; $$",
    )
    .unwrap();
    run(&mut eng, "SELECT dyn_explain()").unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT count(*) FROM easi"),
        vec![vec![s("1")]]
    );
}
