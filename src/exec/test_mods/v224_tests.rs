/// v2.24: RLS whole-row comparison on moved rows (update.sql).
/// (1) Record equality follows PG19 record_eq/record_cmp
/// (utils/adt/rowtypes.c): two NULL fields are equal, one NULL is
/// definitively unequal, and the comparison never yields SQL NULL.
/// (2) A partition move is heap_delete + ExecInsert
/// (nodeModifyTable.c): the destination leaf's BEFORE INSERT triggers
/// fire on the routed row and the RLS WITH CHECK judges the final
/// image. Ground truth: src/test/regress/sql/update.sql, the "RLS
/// policies with update-row-movement" section.
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

fn err_code_as(eng: &mut Engine, sql: &str, role: &str) -> String {
    match run_as(eng, sql, role) {
        Ok(_) => panic!("{sql} as {role} -> expected error, got success"),
        Err(e) => e.code.to_string(),
    }
}

fn one_bool(eng: &mut Engine, sql: &str) -> Option<bool> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => match &rows[0][0] {
            Value::Bool(b) => Some(*b),
            Value::Null => None,
            other => panic!("{sql} -> {other:?}"),
        },
        other => panic!("{sql} -> {other:?}"),
    }
}

fn rows_text(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

#[test]
fn record_eq_null_fields_are_equal() {
    let mut eng = engine();
    // record_eq: NULL == NULL per field; never SQL NULL.
    assert_eq!(
        one_bool(&mut eng, "SELECT row(1, NULL) = row(1, NULL)"),
        Some(true)
    );
    assert_eq!(
        one_bool(&mut eng, "SELECT row(1, NULL) <> row(1, NULL)"),
        Some(false)
    );
    assert_eq!(
        one_bool(&mut eng, "SELECT row(NULL, NULL) = row(NULL, NULL)"),
        Some(true)
    );
}

#[test]
fn record_eq_one_null_field_is_unequal() {
    let mut eng = engine();
    assert_eq!(
        one_bool(&mut eng, "SELECT row(1, NULL) = row(1, 2)"),
        Some(false)
    );
    assert_eq!(
        one_bool(&mut eng, "SELECT row(1, NULL) <> row(1, 2)"),
        Some(true)
    );
    assert_eq!(
        one_bool(&mut eng, "SELECT row(1, 2) = row(1, NULL)"),
        Some(false)
    );
}

#[test]
fn record_ordering_null_sorts_larger() {
    let mut eng = engine();
    assert_eq!(
        one_bool(&mut eng, "SELECT row(1, NULL) > row(1, 2)"),
        Some(true)
    );
    assert_eq!(
        one_bool(&mut eng, "SELECT row(1, 2) < row(1, NULL)"),
        Some(true)
    );
    assert_eq!(
        one_bool(&mut eng, "SELECT row(1, NULL) <= row(1, NULL)"),
        Some(true)
    );
}

/// Fixture mirroring update.sql's RLS row-movement section on a
/// two-leaf range tree: row ('a', 10, 200, 1) lives in the 'a' leaf;
/// SET a='b' moves it into the 'b' leaf.
fn rls_move_fixture() -> Engine {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE rp (a text, b bigint, c numeric, d int, e varchar) PARTITION BY RANGE (a, b)",
    );
    ok(
        &mut eng,
        "CREATE TABLE rp_a PARTITION OF rp FOR VALUES FROM ('a', 1) TO ('a', 20)",
    );
    ok(
        &mut eng,
        "CREATE TABLE rp_b PARTITION OF rp FOR VALUES FROM ('b', 1) TO ('b', 20)",
    );
    ok(&mut eng, "CREATE USER rpuser");
    ok(&mut eng, "GRANT ALL ON rp TO rpuser");
    ok(&mut eng, "ALTER TABLE rp ENABLE ROW LEVEL SECURITY");
    ok(
        &mut eng,
        "CREATE POLICY seeall ON rp AS PERMISSIVE FOR SELECT USING (true)",
    );
    ok(
        &mut eng,
        "CREATE POLICY pol ON rp FOR UPDATE USING (true) WITH CHECK (c % 2 = 0)",
    );
    ok(&mut eng, "INSERT INTO rp VALUES ('a', 10, 200, 1)");
    eng
}

#[test]
fn moved_row_rls_checks_pre_trigger_image_without_trigger() {
    let mut eng = rls_move_fixture();
    // Odd c violates WITH CHECK — with or without movement.
    assert_eq!(
        err_code_as(
            &mut eng,
            "UPDATE rp SET a='b', c=151 WHERE a='a' AND c=200",
            "rpuser"
        ),
        "42501"
    );
    ok_as(
        &mut eng,
        "UPDATE rp SET a='b', c=150 WHERE a='a' AND c=200",
        "rpuser",
    );
}

#[test]
fn moved_row_fires_destination_insert_trigger_before_rls_check() {
    let mut eng = rls_move_fixture();
    ok(
        &mut eng,
        "CREATE FUNCTION bump() RETURNS trigger AS $$ BEGIN NEW.c = NEW.c + 1; RETURN NEW; END $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER trg BEFORE INSERT ON rp_b FOR EACH ROW EXECUTE PROCEDURE bump()",
    );
    // SET c=151 (odd) becomes 152 (even) in the destination leaf: the
    // RLS check must pass and the stored row must show the trigger's +1.
    ok_as(
        &mut eng,
        "UPDATE rp SET a='b', c=151 WHERE a='a' AND c=200",
        "rpuser",
    );
    assert_eq!(
        rows_text(&mut eng, "SELECT c FROM rp WHERE a='b'"),
        vec![vec!["152".to_string()]]
    );
}

#[test]
fn moved_row_trigger_can_turn_pass_into_rls_failure() {
    let mut eng = rls_move_fixture();
    ok(
        &mut eng,
        "CREATE FUNCTION bump() RETURNS trigger AS $$ BEGIN NEW.c = NEW.c + 1; RETURN NEW; END $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER trg BEFORE INSERT ON rp_b FOR EACH ROW EXECUTE PROCEDURE bump()",
    );
    // SET c=150 (even) becomes 151 (odd): 42501.
    assert_eq!(
        err_code_as(
            &mut eng,
            "UPDATE rp SET a='b', c=150 WHERE a='a' AND c=200",
            "rpuser"
        ),
        "42501"
    );
}

#[test]
fn wholerow_policy_on_moved_row_with_null_column() {
    let mut eng = rls_move_fixture();
    ok(
        &mut eng,
        "CREATE POLICY wr ON rp AS RESTRICTIVE FOR UPDATE USING (true) WITH CHECK (rp = row('b', 10, 112, 1, NULL)::rp)",
    );
    // The new row ('b', 10, 112, 1, NULL) equals the policy's whole-row
    // target, e column NULL on both sides: must pass.
    ok_as(
        &mut eng,
        "UPDATE rp SET a='b', c=112 WHERE a='a' AND c=200",
        "rpuser",
    );
    // A different c is definitively unequal: must fail.
    ok(&mut eng, "INSERT INTO rp VALUES ('a', 10, 200, 1)");
    assert_eq!(
        err_code_as(
            &mut eng,
            "UPDATE rp SET a='b', c=116 WHERE a='a' AND c=200",
            "rpuser"
        ),
        "42501"
    );
}
