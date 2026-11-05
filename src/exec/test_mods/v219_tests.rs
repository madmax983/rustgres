/// v2.19: the CREATE RULE system — DO INSTEAD / DO ALSO rules on
/// views and tables, NEW/OLD substitution, rule-driven INSTEAD
/// semantics replacing the auto-updatable fallback, the
/// RETURNING / ON CONFLICT / MERGE gates, pg_rewrite + pg_rules +
/// pg_get_ruledef, and the adjudicated v2.18 leftovers this
/// milestone closed (extstats MCV tie order is covered in the
/// schedule diff; ISNULL postfix parsing).
/// Ground truth: PG19 src/test/regress/sql/rules.sql +
/// updatable_views.sql, src/backend/rewrite/rewriteDefine.c +
/// rewriteHandler.c.
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

fn run_s(eng: &mut Engine, sql: &str, session: u64) -> Result<ExecResult, ExecError> {
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
        session,
        role: "postgres",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    execute(eng, &mut ctx, &stmt)
}

fn ok_s(eng: &mut Engine, sql: &str, session: u64) {
    run_s(eng, sql, session)
        .unwrap_or_else(|e| panic!("{sql} (s{session}) -> {} {}", e.code, e.message));
}

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql)
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

fn dml(eng: &mut Engine, sql: &str) -> (String, Vec<Vec<Option<String>>>) {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Dml { tag, rows, .. } => (
            tag,
            rows.iter()
                .map(|r| r.iter().map(|v| v.to_text()).collect())
                .collect(),
        ),
        other => panic!("expected Dml for {sql}, got {other:?}"),
    }
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn instead_rule_on_view_with_returning() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE sl_data (name text, avail int, color text)",
    );
    ok(&mut eng, "INSERT INTO sl_data VALUES ('sl1', 10, 'red')");
    ok(&mut eng, "CREATE VIEW sl AS SELECT * FROM sl_data");
    ok(
        &mut eng,
        "CREATE RULE sl_ins AS ON INSERT TO sl DO INSTEAD \
         INSERT INTO sl_data VALUES (NEW.name, NEW.avail, NEW.color) RETURNING *",
    );
    ok(
        &mut eng,
        "CREATE RULE sl_upd AS ON UPDATE TO sl DO INSTEAD \
         UPDATE sl_data SET avail = NEW.avail WHERE name = OLD.name RETURNING *",
    );
    ok(
        &mut eng,
        "CREATE RULE sl_del AS ON DELETE TO sl DO INSTEAD \
         DELETE FROM sl_data WHERE name = OLD.name",
    );
    let (tag, r) = dml(
        &mut eng,
        "INSERT INTO sl VALUES ('sl2', 7, 'blue') RETURNING *",
    );
    assert_eq!(tag, "INSERT 0 1");
    assert_eq!(r, vec![vec![s("sl2"), s("7"), s("blue")]]);
    let (tag, r) = dml(
        &mut eng,
        "UPDATE sl SET avail = 99 WHERE name = 'sl1' RETURNING *",
    );
    assert_eq!(tag, "UPDATE 1");
    assert_eq!(r, vec![vec![s("sl1"), s("99"), s("red")]]);
    let (tag, _) = dml(&mut eng, "DELETE FROM sl WHERE name = 'sl2'");
    assert_eq!(tag, "DELETE 1");
    assert_eq!(
        rows(&mut eng, "SELECT * FROM sl_data"),
        vec![vec![s("sl1"), s("99"), s("red")]]
    );
}

#[test]
fn also_rule_fires_and_original_executes() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text)");
    ok(&mut eng, "CREATE TABLE hist (a int, b text)");
    ok(&mut eng, "CREATE VIEW v AS SELECT * FROM t");
    ok(
        &mut eng,
        "CREATE RULE v_log AS ON INSERT TO v DO ALSO \
         INSERT INTO hist VALUES (NEW.a, NEW.b)",
    );
    let (tag, _) = dml(&mut eng, "INSERT INTO v VALUES (1, 'one'), (2, 'two')");
    assert_eq!(tag, "INSERT 0 2");
    assert_eq!(rows(&mut eng, "SELECT * FROM t ORDER BY a").len(), 2);
    assert_eq!(rows(&mut eng, "SELECT * FROM hist ORDER BY a").len(), 2);
}

#[test]
fn conditional_instead_on_table_routes_per_row() {
    // PG19 rules.sql rtest_t4 shape: rows matching a conditional
    // INSTEAD rule go to the rule; unmatched rows execute the
    // original; ALSO rules fire for every row.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t4 (a int, b text)");
    ok(&mut eng, "CREATE TABLE t5 (a int, b text)");
    ok(&mut eng, "CREATE TABLE t6 (a int, b text)");
    ok(
        &mut eng,
        "CREATE RULE t4_ins AS ON INSERT TO t4 WHERE a < 100 DO INSTEAD \
         INSERT INTO t5 VALUES (NEW.a, NEW.b)",
    );
    ok(
        &mut eng,
        "CREATE RULE t4_log AS ON INSERT TO t4 DO ALSO \
         INSERT INTO t6 VALUES (NEW.a, NEW.b)",
    );
    let (tag, _) = dml(
        &mut eng,
        "INSERT INTO t4 VALUES (10, 'ten'), (200, 'two hundred')",
    );
    assert_eq!(tag, "INSERT 0 1");
    assert_eq!(
        rows(&mut eng, "SELECT * FROM t4"),
        vec![vec![s("200"), s("two hundred")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM t5"),
        vec![vec![s("10"), s("ten")]]
    );
    assert_eq!(rows(&mut eng, "SELECT * FROM t6 ORDER BY a").len(), 2);
}

#[test]
fn conditional_only_instead_view_is_not_updatable() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE base (a int, b int)");
    ok(&mut eng, "CREATE VIEW v16 AS SELECT * FROM base");
    // The rw_view16 shape: conditional INSTEAD rules for every
    // event and no unconditional one -> every event is 55000.
    ok(
        &mut eng,
        "CREATE RULE v16_ins AS ON INSERT TO v16 WHERE a = 1 DO INSTEAD \
         INSERT INTO base VALUES (NEW.a, NEW.b)",
    );
    ok(
        &mut eng,
        "CREATE RULE v16_upd AS ON UPDATE TO v16 WHERE a = 1 DO INSTEAD NOTHING",
    );
    ok(
        &mut eng,
        "CREATE RULE v16_del AS ON DELETE TO v16 WHERE a = 1 DO INSTEAD NOTHING",
    );
    assert_eq!(err_code(&mut eng, "INSERT INTO v16 VALUES (1, 2)"), "55000");
    assert_eq!(err_code(&mut eng, "UPDATE v16 SET b = 3"), "55000");
    assert_eq!(err_code(&mut eng, "DELETE FROM v16"), "55000");
    // An unconditional UPDATE rule makes UPDATE work again.
    ok(
        &mut eng,
        "CREATE RULE v16_upd2 AS ON UPDATE TO v16 DO INSTEAD \
         UPDATE base SET b = NEW.b WHERE a = OLD.a",
    );
    ok(&mut eng, "INSERT INTO base VALUES (1, 1)");
    let (tag, _) = dml(&mut eng, "UPDATE v16 SET b = 5 WHERE a = 1");
    assert_eq!(tag, "UPDATE 1");
    assert_eq!(
        rows(&mut eng, "SELECT * FROM base"),
        vec![vec![s("1"), s("5")]]
    );
}

#[test]
fn returning_gate_and_voo_shape() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE foo (f1 int, f2 text, f3 int, f4 int)",
    );
    ok(&mut eng, "CREATE TEMP VIEW voo AS SELECT f1, f2 FROM foo");
    ok(
        &mut eng,
        "CREATE RULE voo_i AS ON INSERT TO voo DO INSTEAD \
         INSERT INTO foo VALUES (NEW.*, 57)",
    );
    // The v2.18 adjudication case: PG errors 0A000.
    assert_eq!(
        err_code(
            &mut eng,
            "INSERT INTO voo VALUES (12, 'zoo') RETURNING *, f1*2"
        ),
        "0A000"
    );
    // Without RETURNING the rule fires and NEW.* expands.
    let (tag, _) = dml(&mut eng, "INSERT INTO voo VALUES (12, 'zoo')");
    assert_eq!(tag, "INSERT 0 1");
    // NEW.* expands to the view's columns; 57 lands in f3.
    assert_eq!(
        rows(&mut eng, "SELECT f1, f2, f3, f4 FROM foo"),
        vec![vec![s("12"), s("zoo"), s("57"), None]]
    );
}

#[test]
fn recursion_is_42p17() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    ok(
        &mut eng,
        "CREATE RULE rec AS ON INSERT TO t DO ALSO INSERT INTO t VALUES (NEW.a + 1)",
    );
    assert_eq!(err_code(&mut eng, "INSERT INTO t VALUES (1)"), "42P17");
}

#[test]
fn merge_gate_and_disable_rule() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE d (a int)");
    ok(&mut eng, "CREATE TABLE s (a int)");
    ok(&mut eng, "INSERT INTO s VALUES (1)");
    ok(
        &mut eng,
        "CREATE RULE d_r AS ON UPDATE TO d DO ALSO SELECT 1",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "MERGE INTO d USING s ON d.a = s.a WHEN MATCHED THEN DO NOTHING"
        ),
        "0A000"
    );
    ok(&mut eng, "ALTER TABLE d DISABLE RULE d_r");
    ok(
        &mut eng,
        "MERGE INTO d USING s ON d.a = s.a WHEN MATCHED THEN DO NOTHING",
    );
}

#[test]
fn on_conflict_gate() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int PRIMARY KEY, b text)");
    ok(&mut eng, "CREATE VIEW v AS SELECT * FROM t");
    ok(
        &mut eng,
        "CREATE RULE v_ins AS ON INSERT TO v DO INSTEAD \
         INSERT INTO t VALUES (NEW.a, NEW.b)",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "INSERT INTO v VALUES (1, 'x') ON CONFLICT DO NOTHING"
        ),
        "0A000"
    );
}

#[test]
fn drop_and_rename_rule() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    ok(&mut eng, "CREATE VIEW v AS SELECT * FROM t");
    ok(
        &mut eng,
        "CREATE RULE r1 AS ON INSERT TO v DO INSTEAD INSERT INTO t VALUES (NEW.a)",
    );
    assert_eq!(err_code(&mut eng, "DROP RULE _RETURN ON v"), "55000");
    assert_eq!(err_code(&mut eng, "DROP RULE nope ON v"), "42704");
    ok(&mut eng, "ALTER RULE r1 ON v RENAME TO r1_new");
    assert_eq!(
        rows(
            &mut eng,
            "SELECT rulename FROM pg_rules WHERE tablename = 'v' AND rulename <> '_RETURN'"
        ),
        vec![vec![s("r1_new")]]
    );
    ok(&mut eng, "DROP RULE r1_new ON v");
    assert_eq!(
        rows(
            &mut eng,
            "SELECT count(*) FROM pg_rules WHERE tablename = 'v' AND rulename <> '_RETURN'"
        ),
        vec![vec![s("0")]]
    );
}

#[test]
fn catalogs_ruledef_and_return_row() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text)");
    ok(&mut eng, "CREATE VIEW v AS SELECT * FROM t");
    ok(
        &mut eng,
        "CREATE RULE v_ins AS ON INSERT TO v DO INSTEAD \
         INSERT INTO t VALUES (NEW.a, NEW.b) RETURNING *",
    );
    let defs = rows(
        &mut eng,
        "SELECT definition FROM pg_rules WHERE tablename = 'v' AND rulename = 'v_ins'",
    );
    assert_eq!(defs.len(), 1);
    assert!(
        defs[0][0]
            .as_deref()
            .unwrap_or("")
            .contains("ON INSERT TO v")
    );
    let rd = rows(
        &mut eng,
        "SELECT pg_get_ruledef(oid) FROM pg_rewrite WHERE rulename = 'v_ins'",
    );
    assert_eq!(rd.len(), 1);
    assert!(
        rd[0][0]
            .as_deref()
            .unwrap_or("")
            .starts_with("CREATE RULE v_ins AS")
    );
    // The view's synthesized _RETURN row exists.
    let ret = rows(
        &mut eng,
        "SELECT ev_type, is_instead FROM pg_rewrite WHERE rulename = '_RETURN'",
    );
    assert_eq!(ret, vec![vec![s("1"), s("t")]]);
    assert_eq!(rows(&mut eng, "SELECT pg_get_ruledef(0)"), vec![vec![None]]);
}

#[test]
fn old_new_in_update_delete_rules() {
    // rules.sql rtest_emp shape on a plain table.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE emp (ename text, salary int)");
    ok(&mut eng, "INSERT INTO emp VALUES ('a', 100), ('b', 200)");
    ok(
        &mut eng,
        "CREATE RULE emp_upd AS ON UPDATE TO emp WHERE OLD.salary > 150 DO INSTEAD NOTHING",
    );
    let (tag, _) = dml(&mut eng, "UPDATE emp SET salary = salary + 1");
    assert_eq!(tag, "UPDATE 1");
    assert_eq!(
        rows(&mut eng, "SELECT salary FROM emp ORDER BY ename"),
        vec![vec![s("101")], vec![s("200")]]
    );
    ok(
        &mut eng,
        "CREATE RULE emp_del AS ON DELETE TO emp DO INSTEAD NOTHING",
    );
    let (tag, _) = dml(&mut eng, "DELETE FROM emp");
    assert_eq!(tag, "DELETE 0");
    assert_eq!(
        rows(&mut eng, "SELECT count(*) FROM emp"),
        vec![vec![s("2")]]
    );
}

#[test]
fn do_select_action_requires_returning() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE hd (name text PRIMARY KEY, color text)",
    );
    ok(&mut eng, "CREATE TABLE h (name text, color text)");
    // CREATE-time: DO SELECT action without its own RETURNING.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE RULE bad AS ON INSERT TO h DO INSTEAD \
             INSERT INTO hd VALUES (NEW.name, NEW.color) \
             ON CONFLICT (name) DO SELECT"
        ),
        "42601"
    );
    ok(
        &mut eng,
        "CREATE RULE cs AS ON INSERT TO h DO INSTEAD \
         INSERT INTO hd VALUES (NEW.name, NEW.color) \
         ON CONFLICT (name) DO SELECT RETURNING *",
    );
    // Fire-time: the triggering statement needs RETURNING too.
    assert_eq!(
        err_code(&mut eng, "INSERT INTO h VALUES ('h7', 'blue')"),
        "42601"
    );
    let (tag, _) = dml(&mut eng, "INSERT INTO h VALUES ('h7', 'blue') RETURNING *");
    assert_eq!(tag, "INSERT 0 1");
}

#[test]
fn create_rule_validation() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    // ON SELECT on a non-view.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE RULE r AS ON SELECT TO t DO INSTEAD SELECT 1"
        ),
        "42809"
    );
    // OLD in an INSERT rule.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE RULE r AS ON INSERT TO t DO INSTEAD INSERT INTO t VALUES (OLD.a)"
        ),
        "42P01"
    );
    // Missing relation.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE RULE r AS ON INSERT TO nope DO INSTEAD NOTHING"
        ),
        "42P01"
    );
    // Duplicate name.
    ok(
        &mut eng,
        "CREATE RULE r AS ON INSERT TO t DO INSTEAD NOTHING",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE RULE r AS ON DELETE TO t DO INSTEAD NOTHING"
        ),
        "42710"
    );
    // NOTIFY action dispatches (own table: t's INSERT rule above
    // is DO INSTEAD NOTHING, so t stays empty by design).
    ok(&mut eng, "CREATE TABLE t2 (a int)");
    ok(
        &mut eng,
        "CREATE RULE rn AS ON DELETE TO t2 DO ALSO NOTIFY chan",
    );
    ok(&mut eng, "INSERT INTO t2 VALUES (1)");
    let (tag, _) = dml(&mut eng, "DELETE FROM t2");
    assert_eq!(tag, "DELETE 1");
}

#[test]
fn isnull_notnull_postfix() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text)");
    ok(&mut eng, "INSERT INTO t VALUES (1, 'x'), (2, NULL)");
    assert_eq!(
        rows(&mut eng, "SELECT a FROM t WHERE b ISNULL"),
        vec![vec![s("2")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT a FROM t WHERE b NOTNULL ORDER BY a"),
        vec![vec![s("1")]]
    );
}

#[test]
fn or_replace_returning_count_must_match_event_relation() {
    // returning.sql: the action's RETURNING * expands over the
    // action target (foo, 4 cols) but the event relation (voo)
    // has 2 — PG errors 42601 at CREATE/REPLACE time.
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE foo (f1 int, f2 text, f3 int, f4 int)",
    );
    ok(&mut eng, "CREATE VIEW voo AS SELECT f1, f2 FROM foo");
    ok(
        &mut eng,
        "CREATE RULE voo_i AS ON INSERT TO voo DO INSTEAD INSERT INTO foo VALUES (NEW.*, 57)",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE OR REPLACE RULE voo_i AS ON INSERT TO voo DO INSTEAD \
             INSERT INTO foo VALUES (NEW.*, 57) RETURNING *"
        ),
        "42601"
    );
    ok(
        &mut eng,
        "CREATE OR REPLACE RULE voo_i AS ON INSERT TO voo DO INSTEAD \
         INSERT INTO foo VALUES (NEW.*, 57) RETURNING f1, f2",
    );
    let (_, r) = dml(&mut eng, "INSERT INTO voo VALUES (14, 'zoo2') RETURNING *");
    assert_eq!(r, vec![vec![s("14"), s("zoo2")]]);
}

#[test]
fn old_new_inside_action_cte_is_0a000() {
    // with.sql: OLD/NEW references inside a rule action's WITH
    // query are refused at CREATE time.
    let mut eng = engine();
    ok(&mut eng, "CREATE TEMPORARY TABLE x (n integer)");
    ok(&mut eng, "CREATE TABLE y (a int)");
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE RULE r2 AS ON UPDATE TO x DO INSTEAD \
             WITH t AS (SELECT OLD.*) UPDATE y SET a = t.n FROM t"
        ),
        "0A000"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE RULE r3 AS ON UPDATE TO x DO INSTEAD \
             WITH t AS (SELECT OLD.n AS n) UPDATE y SET a = t.n FROM t"
        ),
        "0A000"
    );
}

#[test]
fn parenthesized_action_list_survives_statement_split() {
    // split_statements must not chop a CREATE RULE at the
    // semicolons inside its parenthesized action list.
    let sql = "CREATE RULE rn AS ON INSERT TO t DO INSTEAD (NOTIFY a; NOTIFY b)";
    let parts = crate::sql::split_statements(sql);
    assert_eq!(parts.len(), 1);
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    ok(&mut eng, &parts[0]);
    assert_eq!(
        rows(
            &mut eng,
            "SELECT count(*) FROM pg_rules WHERE tablename = 't' AND rulename = 'rn'"
        ),
        vec![vec![s("1")]]
    );
    let (tag, _) = dml(&mut eng, "INSERT INTO t VALUES (1)");
    assert_eq!(tag, "INSERT 0 0");
}

#[test]
fn temp_table_rules_are_session_scoped() {
    // The rules->window schedule pollution: rules.sql leaves
    // INSTEAD rules on a TEMP table t1; a later session's own
    // temp t1 must not see them.
    let mut eng = engine();
    ok_s(&mut eng, "CREATE TEMP TABLE t1 (a integer PRIMARY KEY)", 1);
    ok_s(
        &mut eng,
        "CREATE RULE t1_ins AS ON INSERT TO t1 WHERE NEW.a >= 100 DO INSTEAD NOTHING",
        1,
    );
    // Session 2 has its own temp t1 with different columns.
    ok_s(&mut eng, "CREATE TEMP TABLE t1 (f1 int, f2 int)", 2);
    let r = run_s(&mut eng, "INSERT INTO t1 VALUES (1, 1), (2, 2)", 2)
        .unwrap_or_else(|e| panic!("insert into s2 t1 -> {} {}", e.code, e.message));
    match r {
        ExecResult::Dml { tag, .. } => assert_eq!(tag, "INSERT 0 2"),
        other => panic!("expected Dml, got {other:?}"),
    }
    // Session end purges the temp rules.
    eng.db.drop_session_temps(1);
    assert!(eng.db.rules.keys().all(|k| !k.starts_with("pg_temp:1:")));
}

#[test]
fn view_on_view_rules_fire_through_auto_chain() {
    // updatable_views.sql: DML on a view over a ruled view lands
    // on the inner view's INSTEAD rules; check options do not
    // cascade through them.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE base (a int, b int)");
    ok(&mut eng, "INSERT INTO base VALUES (1, 10)");
    ok(&mut eng, "CREATE VIEW v1 AS SELECT a FROM base WHERE a < b");
    ok(
        &mut eng,
        "CREATE RULE v1_ins AS ON INSERT TO v1 DO INSTEAD INSERT INTO base VALUES (NEW.a, 10)",
    );
    ok(
        &mut eng,
        "CREATE RULE v1_upd AS ON UPDATE TO v1 DO INSTEAD UPDATE base SET a = NEW.a WHERE a = OLD.a",
    );
    ok(
        &mut eng,
        "CREATE VIEW v2 AS SELECT * FROM v1 WHERE a > 0 WITH LOCAL CHECK OPTION",
    );
    let (tag, _) = dml(&mut eng, "INSERT INTO v2 VALUES (5)");
    assert_eq!(tag, "INSERT 0 1");
    let (tag, _) = dml(&mut eng, "INSERT INTO v2 VALUES (20)");
    assert_eq!(tag, "INSERT 0 1");
    let (tag, _) = dml(&mut eng, "UPDATE v2 SET a = 30 WHERE a = 5");
    assert_eq!(tag, "UPDATE 1");
    assert_eq!(
        rows(&mut eng, "SELECT a FROM base ORDER BY a"),
        vec![vec![s("1")], vec![s("20")], vec![s("30")]]
    );
}
