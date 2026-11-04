/// v2.18: AUTO-UPDATABLE VIEW DML — INSERT/UPDATE/DELETE/MERGE
/// rewritten onto the base relation (PG19 rewriteHandler rules),
/// WITH CHECK OPTION (LOCAL/CASCADED), view column defaults,
/// RETURNING old/new, the privilege chain across view hops,
/// information_schema updatability (recursive), and the
/// pg_relation_is_updatable / pg_column_is_updatable functions.
/// Ground truth: PG19 src/test/regress/sql/updatable_views.sql,
/// src/backend/rewrite/rewriteHandler.c,
/// src/backend/catalog/information_schema.sql.
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

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn ok_as(eng: &mut Engine, sql: &str, role: &str) {
    run_as(eng, sql, role)
        .unwrap_or_else(|e| panic!("{sql} as {role} -> {} {}", e.code, e.message));
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql)
        .map(|_| "OK".to_string())
        .unwrap_or_else(|e| e.code.to_string())
}

fn err_code_as(eng: &mut Engine, sql: &str, role: &str) -> String {
    run_as(eng, sql, role)
        .map(|_| "OK".to_string())
        .unwrap_or_else(|e| e.code.to_string())
}

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn dml_rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Dml { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Dml for {sql}, got {other:?}"),
    }
}

fn txt(s: &str) -> Option<String> {
    Some(s.to_string())
}

/// base_tbl(a pk, b default) + simple renamed view fixture.
fn fixture() -> Engine {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE base_tbl (a int PRIMARY KEY, b text DEFAULT 'Unspecified')",
    );
    ok(
        &mut eng,
        "INSERT INTO base_tbl VALUES (1, 'Row 1'), (2, 'Row 2')",
    );
    eng
}

#[test]
fn v218_rewrite_basics_and_base_defaults() {
    let mut eng = fixture();
    ok(
        &mut eng,
        "CREATE VIEW v AS SELECT a AS aa, b AS bb FROM base_tbl",
    );
    ok(&mut eng, "INSERT INTO v VALUES (3, 'Row 3')");
    // Omitted column takes the BASE default through the rewrite.
    ok(&mut eng, "INSERT INTO v (aa) VALUES (4)");
    ok(&mut eng, "UPDATE v SET bb = 'Two' WHERE aa = 2");
    ok(&mut eng, "DELETE FROM v WHERE aa = 1");
    assert_eq!(
        rows(&mut eng, "SELECT a, b FROM base_tbl ORDER BY a"),
        vec![
            vec![txt("2"), txt("Two")],
            vec![txt("3"), txt("Row 3")],
            vec![txt("4"), txt("Unspecified")],
        ]
    );
}

#[test]
fn v218_non_updatable_shapes_error_55000() {
    let mut eng = fixture();
    ok(
        &mut eng,
        "CREATE VIEW dv AS SELECT DISTINCT a, b FROM base_tbl",
    );
    ok(
        &mut eng,
        "CREATE VIEW gv AS SELECT a, b FROM base_tbl GROUP BY a, b",
    );
    ok(
        &mut eng,
        "CREATE VIEW uv AS SELECT a, b FROM base_tbl UNION SELECT a, b FROM base_tbl",
    );
    ok(
        &mut eng,
        "CREATE VIEW lv AS SELECT a, b FROM base_tbl LIMIT 2",
    );
    for v in ["dv", "gv", "uv", "lv"] {
        assert_eq!(
            err_code(&mut eng, &format!("INSERT INTO {v} VALUES (9, 'x')")),
            "55000",
            "insert into {v}"
        );
        assert_eq!(
            err_code(&mut eng, &format!("DELETE FROM {v}")),
            "55000",
            "delete from {v}"
        );
    }
    // MERGE reports the first action's event in the message.
    let e = run(
        &mut eng,
        "MERGE INTO dv t USING (VALUES (1)) AS s(x) ON t.a = s.x \
         WHEN NOT MATCHED THEN INSERT VALUES (1, 'x')",
    )
    .unwrap_err();
    assert_eq!(e.code, "55000");
    assert!(e.message.contains("cannot insert into view \"dv\""));
}

#[test]
fn v218_expression_columns_and_positional_targets() {
    let mut eng = fixture();
    // Partially updatable: const + expression columns ride along.
    ok(
        &mut eng,
        "CREATE VIEW pv AS SELECT a, b, 'Const' AS c, upper(b) AS ub FROM base_tbl",
    );
    // Positional INSERT addresses updatable columns only.
    ok(&mut eng, "INSERT INTO pv VALUES (3, 'Row 3')");
    // Naming an expression column is the 0A000 column error.
    let e = run(&mut eng, "INSERT INTO pv (a, ub) VALUES (4, 'X')").unwrap_err();
    assert_eq!(e.code, "0A000");
    assert!(
        e.message
            .contains("cannot insert into column \"ub\" of view \"pv\"")
    );
    assert_eq!(
        err_code(&mut eng, "UPDATE pv SET c = 'y' WHERE a = 1"),
        "0A000"
    );
    // ...but expression columns work in WHERE via substitution.
    ok(&mut eng, "UPDATE pv SET b = 'Uno' WHERE ub = 'ROW 1'");
    assert_eq!(
        rows(&mut eng, "SELECT b FROM base_tbl WHERE a = 1"),
        vec![vec![txt("Uno")]]
    );
}

#[test]
fn v218_check_option_local_vs_cascaded() {
    let mut eng = fixture();
    ok(
        &mut eng,
        "CREATE VIEW vpos AS SELECT a, b FROM base_tbl WHERE a > 0 \
         WITH CASCADED CHECK OPTION",
    );
    assert_eq!(
        err_code(&mut eng, "INSERT INTO vpos VALUES (-1, 'neg')"),
        "44000"
    );
    ok(&mut eng, "INSERT INTO vpos VALUES (5, 'pos')");
    // LOCAL outer over CASCADED inner: the inner flavor rules its
    // own qual when the write descends.
    ok(
        &mut eng,
        "CREATE VIEW vloc AS SELECT a, b FROM vpos WHERE a < 100 \
         WITH LOCAL CHECK OPTION",
    );
    // -5 passes vloc's own qual (a<100) but violates vpos (a>0).
    let e = run(&mut eng, "INSERT INTO vloc VALUES (-5, 'neg')").unwrap_err();
    assert_eq!(e.code, "44000");
    assert!(e.message.contains("\"vpos\""));
    // 500 violates vloc's own qual.
    let e = run(&mut eng, "INSERT INTO vloc VALUES (500, 'big')").unwrap_err();
    assert_eq!(e.code, "44000");
    assert!(e.message.contains("\"vloc\""));
    ok(&mut eng, "INSERT INTO vloc VALUES (50, 'ok')");
}

#[test]
fn v218_returning_old_new() {
    let mut eng = fixture();
    ok(&mut eng, "CREATE VIEW v AS SELECT a, b FROM base_tbl");
    // INSERT: new.* is the inserted row.
    assert_eq!(
        dml_rows(
            &mut eng,
            "INSERT INTO v VALUES (3, 'Row 3') RETURNING new.*"
        ),
        vec![vec![txt("3"), txt("Row 3")]]
    );
    // UPDATE: old/new pair via capture + pre-scan.
    assert_eq!(
        dml_rows(
            &mut eng,
            "UPDATE v SET b = 'Three' WHERE a = 3 RETURNING old.b, new.b"
        ),
        vec![vec![txt("Row 3"), txt("Three")]]
    );
    // DELETE: unqualified refs read the old row.
    assert_eq!(
        dml_rows(&mut eng, "DELETE FROM v WHERE a = 3 RETURNING a, b"),
        vec![vec![txt("3"), txt("Three")]]
    );
    // ON CONFLICT: old is the pre-existing conflicting row, and
    // excluded.<expr col> reads the expression over the proposed
    // row. (Space-free values: record text quotes fields with
    // spaces, which this assertion does not exercise.)
    ok(&mut eng, "CREATE TABLE t2 (a int PRIMARY KEY, b text)");
    ok(&mut eng, "INSERT INTO t2 VALUES (2, 'rowtwo')");
    ok(
        &mut eng,
        "CREATE VIEW pv AS SELECT a, b, upper(b) AS ub FROM t2",
    );
    assert_eq!(
        dml_rows(
            &mut eng,
            "INSERT INTO pv (a, b) VALUES (2, 'rowtwo') ON CONFLICT (a) \
             DO UPDATE SET a = excluded.a WHERE excluded.ub = 'ROWTWO' \
             RETURNING old, new"
        ),
        vec![vec![txt("(2,rowtwo,ROWTWO)"), txt("(2,rowtwo,ROWTWO)")]]
    );
}

#[test]
fn v218_merge_and_do_nothing_on_readonly_view() {
    let mut eng = fixture();
    ok(&mut eng, "CREATE VIEW v AS SELECT a, b FROM base_tbl");
    ok(
        &mut eng,
        "MERGE INTO v t USING (VALUES (2, 'Two'), (3, 'Row 3')) AS s(a, b) \
         ON t.a = s.a WHEN MATCHED THEN UPDATE SET b = s.b \
         WHEN NOT MATCHED THEN INSERT VALUES (s.a, s.b)",
    );
    assert_eq!(
        rows(&mut eng, "SELECT a, b FROM base_tbl ORDER BY a"),
        vec![
            vec![txt("1"), txt("Row 1")],
            vec![txt("2"), txt("Two")],
            vec![txt("3"), txt("Row 3")],
        ]
    );
    // DO NOTHING-only MERGE is legal on any view.
    ok(
        &mut eng,
        "CREATE VIEW dv AS SELECT DISTINCT a, b FROM base_tbl",
    );
    ok(
        &mut eng,
        "MERGE INTO dv t USING (VALUES (1, 'x')) AS s(a, b) ON t.a = s.a \
         WHEN MATCHED THEN DO NOTHING WHEN NOT MATCHED THEN DO NOTHING",
    );
    // ...but a writing MERGE into it is not.
    assert_eq!(
        err_code(
            &mut eng,
            "MERGE INTO dv t USING (VALUES (1, 'x')) AS s(a, b) ON t.a = s.a \
             WHEN MATCHED THEN DELETE"
        ),
        "55000"
    );
}

#[test]
fn v218_view_of_view_reorder_chain() {
    let mut eng = fixture();
    ok(
        &mut eng,
        "CREATE VIEW v1 AS SELECT b AS bb, a AS aa, 'C1' AS c \
         FROM base_tbl WHERE a > 0",
    );
    ok(
        &mut eng,
        "CREATE VIEW v2 AS SELECT aa AS aaa, bb AS bbb, c AS c1 \
         FROM v1 WHERE aa < 10",
    );
    ok(&mut eng, "INSERT INTO v2 VALUES (3, 'Row 3')");
    ok(&mut eng, "UPDATE v2 SET bbb = 'Three' WHERE aaa = 3");
    assert_eq!(
        rows(&mut eng, "SELECT * FROM v2 ORDER BY aaa"),
        vec![
            vec![txt("1"), txt("Row 1"), txt("C1")],
            vec![txt("2"), txt("Row 2"), txt("C1")],
            vec![txt("3"), txt("Three"), txt("C1")],
        ]
    );
    // The inner view's qual (a>0) constrains the outer DELETE:
    // nothing outside it can be touched.
    ok(&mut eng, "DELETE FROM v2 WHERE aaa = -1");
    assert_eq!(
        rows(&mut eng, "SELECT count(*) FROM base_tbl"),
        vec![vec![txt("3")]]
    );
}

#[test]
fn v218_view_column_defaults() {
    let mut eng = fixture();
    ok(
        &mut eng,
        "CREATE VIEW pv AS SELECT a, upper(b) AS ub, b FROM base_tbl",
    );
    ok(
        &mut eng,
        "ALTER VIEW pv ALTER COLUMN ub SET DEFAULT 'NOT SET'",
    );
    // A default on a non-updatable column pulls it into the
    // INSERT target set: PG errors 0A000.
    assert_eq!(
        err_code(&mut eng, "INSERT INTO pv (a, b) VALUES (3, 'Row 3')"),
        "0A000"
    );
    ok(&mut eng, "ALTER VIEW pv ALTER COLUMN ub DROP DEFAULT");
    // A default on an updatable column is used when omitted.
    ok(
        &mut eng,
        "ALTER VIEW pv ALTER COLUMN b SET DEFAULT 'View default'",
    );
    ok(&mut eng, "INSERT INTO pv (a) VALUES (3)");
    assert_eq!(
        rows(&mut eng, "SELECT b FROM base_tbl WHERE a = 3"),
        vec![vec![txt("View default")]]
    );
}

#[test]
fn v218_privilege_chain_and_invoker() {
    let mut eng = fixture();
    ok(&mut eng, "CREATE USER u1");
    ok(&mut eng, "CREATE USER u2");
    ok_as(
        &mut eng,
        "CREATE VIEW v1 AS SELECT b AS bb, a AS aa FROM base_tbl",
        "u1",
    );
    ok_as(
        &mut eng,
        "CREATE VIEW v2 AS SELECT aa AS aaa, bb AS bbb FROM v1",
        "u2",
    );
    // No grants yet: the caller has no rights on v2 at all.
    assert_eq!(
        err_code_as(&mut eng, "UPDATE v2 SET bbb = 'x'", "u2"),
        "42501"
    );
    // The owner of v2 (u2) still needs rights down the chain:
    // u1 grants u2 column UPDATE on v1, and postgres (base owner)
    // grants u1 the base rights.
    ok_as(&mut eng, "GRANT UPDATE (bb) ON v1 TO u2", "u1");
    ok_as(&mut eng, "GRANT SELECT ON v1 TO u2", "u1");
    ok(&mut eng, "GRANT SELECT ON base_tbl TO u1");
    ok(&mut eng, "GRANT UPDATE (b) ON base_tbl TO u1");
    ok_as(&mut eng, "UPDATE v2 SET bbb = 'Two' WHERE aaa = 2", "u2");
    // aa is not covered by the column grants anywhere.
    assert_eq!(
        err_code_as(&mut eng, "UPDATE v2 SET aaa = 9 WHERE aaa = 2", "u2"),
        "42501"
    );
    // security_invoker: the caller's own base rights are used for
    // the hop below the invoker view instead of the view owner's.
    let mut eng = fixture();
    ok(&mut eng, "CREATE USER u1");
    ok(&mut eng, "CREATE USER u2");
    ok_as(
        &mut eng,
        "CREATE VIEW iv AS SELECT a, b FROM base_tbl",
        "u1",
    );
    ok_as(
        &mut eng,
        "ALTER VIEW iv SET (security_invoker = true)",
        "u1",
    );
    ok_as(&mut eng, "GRANT SELECT, UPDATE ON iv TO u2", "u1");
    // u2 has no base rights: the invoker hop must deny.
    assert_eq!(
        err_code_as(&mut eng, "UPDATE iv SET b = 'x' WHERE a = 1", "u2"),
        "42501"
    );
    // Give the CALLER the base rights: now it goes through.
    ok(&mut eng, "GRANT SELECT, UPDATE ON base_tbl TO u2");
    ok_as(&mut eng, "UPDATE iv SET b = 'x' WHERE a = 1", "u2");
}

#[test]
fn v218_information_schema_updatability() {
    let mut eng = fixture();
    ok(
        &mut eng,
        "CREATE VIEW v AS SELECT a, upper(b) AS ub FROM base_tbl",
    );
    ok(
        &mut eng,
        "CREATE VIEW dv AS SELECT DISTINCT a, b FROM base_tbl",
    );
    ok(&mut eng, "CREATE VIEW dv2 AS SELECT * FROM dv");
    ok(&mut eng, "CREATE SEQUENCE seq1");
    ok(&mut eng, "CREATE VIEW sv AS SELECT * FROM seq1");
    assert_eq!(
        rows(
            &mut eng,
            "SELECT table_name, is_updatable, is_insertable_into \
             FROM information_schema.views ORDER BY table_name"
        ),
        vec![
            vec![txt("dv"), txt("NO"), txt("NO")],
            vec![txt("dv2"), txt("NO"), txt("NO")],
            vec![txt("sv"), txt("NO"), txt("NO")],
            vec![txt("v"), txt("YES"), txt("YES")],
        ]
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT column_name, is_updatable FROM information_schema.columns \
             WHERE table_name = 'v' ORDER BY ordinal_position"
        ),
        vec![vec![txt("a"), txt("YES")], vec![txt("ub"), txt("NO")]]
    );
    // Sequence views still list their pseudo-columns.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'sv' ORDER BY ordinal_position"
        ),
        vec![
            vec![txt("last_value")],
            vec![txt("log_cnt")],
            vec![txt("is_called")]
        ]
    );
    // The catalog functions agree.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT pg_relation_is_updatable('v'::regclass, false), \
             pg_relation_is_updatable('dv'::regclass, false), \
             pg_column_is_updatable('v'::regclass, 2::smallint, false)"
        ),
        vec![vec![txt("28"), txt("0"), txt("f")]]
    );
}

#[test]
fn v218_create_view_with_cte_and_sequence_dep() {
    let mut eng = fixture();
    // ROOT C regression: a CTE name is not a dependency.
    ok(
        &mut eng,
        "CREATE VIEW cv AS WITH t AS (SELECT a, b FROM base_tbl) \
         SELECT * FROM t",
    );
    // A view over a sequence is creatable (SELECT from it is a
    // separate, pre-existing gap).
    ok(&mut eng, "CREATE SEQUENCE seq9");
    ok(&mut eng, "CREATE VIEW sv9 AS SELECT * FROM seq9");
    // ALTER VIEW SET (check_option=...) reclassifies enforcement.
    ok(
        &mut eng,
        "CREATE VIEW cov AS SELECT a, b FROM base_tbl WHERE a > 0",
    );
    ok(&mut eng, "INSERT INTO cov VALUES (-1, 'neg')");
    ok(&mut eng, "ALTER VIEW cov SET (check_option = cascaded)");
    assert_eq!(
        err_code(&mut eng, "INSERT INTO cov VALUES (-2, 'neg')"),
        "44000"
    );
    ok(&mut eng, "ALTER VIEW cov RESET (check_option)");
    ok(&mut eng, "INSERT INTO cov VALUES (-3, 'neg')");
}

#[test]
fn v218_merge_check_option_and_qual_filter() {
    let mut eng = fixture();
    ok(&mut eng, "CREATE TABLE mt (a int, b int DEFAULT 10)");
    ok(&mut eng, "INSERT INTO mt VALUES (1, 2), (2, 3), (1, -1)");
    ok(
        &mut eng,
        "CREATE VIEW mv AS SELECT * FROM mt WHERE a < b WITH LOCAL CHECK OPTION",
    );
    // NOT MATCHED insert inside the qual: ok.
    ok(
        &mut eng,
        "MERGE INTO mv t USING (VALUES (10)) AS v(a) ON t.a = v.a \
         WHEN NOT MATCHED THEN INSERT VALUES (v.a, v.a + 1)",
    );
    // NOT MATCHED insert violating the check option: 44000.
    assert_eq!(
        err_code(
            &mut eng,
            "MERGE INTO mv t USING (VALUES (11)) AS v(a) ON t.a = v.a \
             WHEN NOT MATCHED THEN INSERT VALUES (v.a, v.a - 1)"
        ),
        "44000"
    );
    // MATCHED update: the (1,-1) row is NOT in the view, so only
    // (1,2) can match; a := 0 keeps 0 < 2 and must succeed.
    ok(
        &mut eng,
        "MERGE INTO mv t USING (VALUES (1)) AS v(a) ON t.a = v.a \
         WHEN MATCHED THEN UPDATE SET a = t.a - 1",
    );
    // MATCHED update violating the check option: 44000.
    assert_eq!(
        err_code(
            &mut eng,
            "MERGE INTO mv t USING (VALUES (2)) AS v(a) ON t.a = v.a \
             WHEN MATCHED THEN UPDATE SET a = t.a + 1"
        ),
        "44000"
    );
    // The out-of-view row was never touched.
    assert_eq!(
        rows(&mut eng, "SELECT a, b FROM mt WHERE b < 0"),
        vec![vec![txt("1"), txt("-1")]]
    );
}

#[test]
fn v218_on_conflict_privilege_deep_rule() {
    let mut eng = fixture();
    ok(&mut eng, "CREATE USER ioc_u1");
    ok(&mut eng, "CREATE USER ioc_u2");
    ok_as(
        &mut eng,
        "CREATE TABLE itab (a int UNIQUE, b text, c float)",
        "ioc_u1",
    );
    ok_as(
        &mut eng,
        "INSERT INTO itab VALUES (1, 'xxx', 1.0)",
        "ioc_u1",
    );
    ok_as(&mut eng, "GRANT SELECT (a, b) ON itab TO ioc_u2", "ioc_u1");
    ok_as(&mut eng, "GRANT INSERT (a, b) ON itab TO ioc_u2", "ioc_u1");
    ok_as(&mut eng, "GRANT UPDATE (a, b) ON itab TO ioc_u2", "ioc_u1");
    // A view exposing only granted columns: DO UPDATE is fine.
    ok_as(
        &mut eng,
        "CREATE VIEW iv3 AS SELECT b AS bb, a AS aa FROM itab",
        "ioc_u2",
    );
    ok_as(
        &mut eng,
        "INSERT INTO iv3 (aa, bb) VALUES (1, 'xxx') \
         ON CONFLICT (aa) DO UPDATE SET bb = excluded.bb",
        "ioc_u2",
    );
    // A view also exposing the ungranted column c: the deep
    // whole-row read rule denies both DO UPDATE and DO SELECT.
    ok_as(
        &mut eng,
        "CREATE VIEW iv2 AS SELECT b AS bb, c AS cc, a AS aa FROM itab",
        "ioc_u2",
    );
    assert_eq!(
        err_code_as(
            &mut eng,
            "INSERT INTO iv2 (aa, bb) VALUES (1, 'xxx') \
             ON CONFLICT (aa) DO UPDATE SET bb = excluded.bb",
            "ioc_u2"
        ),
        "42501"
    );
    assert_eq!(
        err_code_as(
            &mut eng,
            "INSERT INTO iv2 (aa, bb) VALUES (1, 'xxx') \
             ON CONFLICT (aa) DO SELECT RETURNING 1",
            "ioc_u2"
        ),
        "42501"
    );
    // DO UPDATE SET on a column without UPDATE coverage: denied.
    ok_as(
        &mut eng,
        "CREATE VIEW iv1 AS SELECT b AS bb, c AS cc, a AS aa FROM itab",
        "ioc_u1",
    );
    ok_as(&mut eng, "GRANT SELECT (aa, bb) ON iv1 TO ioc_u2", "ioc_u1");
    ok_as(&mut eng, "GRANT INSERT ON iv1 TO ioc_u2", "ioc_u1");
    ok_as(&mut eng, "GRANT UPDATE (bb) ON iv1 TO ioc_u2", "ioc_u1");
    assert_eq!(
        err_code_as(
            &mut eng,
            "INSERT INTO iv1 VALUES ('zzz', 2.0, 1) \
             ON CONFLICT (aa) DO UPDATE SET cc = 3.0",
            "ioc_u2"
        ),
        "42501"
    );
    ok_as(
        &mut eng,
        "INSERT INTO iv1 VALUES ('zzz', 2.0, 1) \
         ON CONFLICT (aa) DO UPDATE SET bb = excluded.bb",
        "ioc_u2",
    );
}
