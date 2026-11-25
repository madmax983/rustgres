/// v2.44: EXCLUDE constraints — grammar (USING gist/btree, element
/// WITH operator, WHERE partial predicate, INCLUDE), catalog
/// (contype 'x', pg_get_constraintdef), DML enforcement (23P01),
/// deferred firing, ON CONFLICT arbiter rules, partitioned-table
/// rules, and the minimal geometric types (point/box/circle as
/// canonical text) the family rides on. Ground truth: PG19
/// execIndexing.c, indexcmds.c, tablecmds.c, ruleutils.c; vendored
/// constraints / rangetypes / insert_conflict / indexing suites.
/// Scope record: hidden_files/v244/scope.md.
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
        privs_suppressed: false,
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

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    match run(eng, sql) {
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

/// Simulate COMMIT: fire the session's deferred queue.
fn flush(eng: &mut Engine) -> Result<(), ExecError> {
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
        role: "postgres",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    flush_deferred_fk(eng, &mut ctx, None)
}

#[test]
fn v244_gist_range_exclusion_23p01_with_detail() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (room int, during tsrange, EXCLUDE USING gist (room WITH =, during WITH &&))",
    );
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1, '[2020-01-01, 2020-01-02)')",
    );
    // Disjoint period: fine.
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1, '[2020-01-03, 2020-01-04)')",
    );
    // Different room: fine.
    ok(
        &mut eng,
        "INSERT INTO t VALUES (2, '[2020-01-01, 2020-01-02)')",
    );
    let e = err(
        &mut eng,
        "INSERT INTO t VALUES (1, '[2020-01-01 12:00, 2020-01-05)')",
    );
    assert_eq!(e.code, "23P01");
    assert_eq!(
        e.message,
        "conflicting key value violates exclusion constraint \"t_room_during_excl\""
    );
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "Key (room, during)=(1, [\"2020-01-01 12:00:00\",\"2020-01-05 00:00:00\")) conflicts with existing key (room, during)=(1, [\"2020-01-01 00:00:00\",\"2020-01-02 00:00:00\"))."
        )
    );
}

#[test]
fn v244_btree_equality_exclusion_and_nulls() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (a int, EXCLUDE USING btree (a WITH =))",
    );
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    // NULL never conflicts (operator yields NULL).
    ok(&mut eng, "INSERT INTO t VALUES (NULL)");
    ok(&mut eng, "INSERT INTO t VALUES (NULL)");
    let e = err(&mut eng, "INSERT INTO t VALUES (1)");
    assert_eq!(e.code, "23P01");
    // UPDATE path enforces too.
    ok(&mut eng, "INSERT INTO t VALUES (2)");
    let e = err(&mut eng, "UPDATE t SET a = 1 WHERE a = 2");
    assert_eq!(e.code, "23P01");
}

#[test]
fn v244_partial_predicate_both_rows() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (a int, active bool, EXCLUDE (a WITH =) WHERE (active))",
    );
    ok(&mut eng, "INSERT INTO t VALUES (1, false)");
    // Existing row fails the predicate: no conflict.
    ok(&mut eng, "INSERT INTO t VALUES (1, true)");
    // Now an active row exists: conflict.
    let e = err(&mut eng, "INSERT INTO t VALUES (1, true)");
    assert_eq!(e.code, "23P01");
    // Inactive insert against active row: the NEW row fails the
    // predicate, so no conflict either.
    ok(&mut eng, "INSERT INTO t VALUES (1, false)");
}

#[test]
fn v244_expression_element_and_create_validation() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (a int, EXCLUDE USING btree ((a + 0) WITH =))",
    );
    ok(&mut eng, "INSERT INTO t VALUES (5)");
    let e = err(&mut eng, "INSERT INTO t VALUES (5)");
    assert_eq!(e.code, "23P01");
    // ADD CONSTRAINT validates existing rows: 23P01 create form.
    ok(&mut eng, "CREATE TABLE u (r int4range)");
    ok(&mut eng, "INSERT INTO u VALUES ('[1,5)')");
    ok(&mut eng, "INSERT INTO u VALUES ('[3,9)')");
    let e = err(
        &mut eng,
        "ALTER TABLE u ADD CONSTRAINT u_ex EXCLUDE USING gist (r WITH &&)",
    );
    assert_eq!(e.code, "23P01");
    assert_eq!(e.message, "could not create exclusion constraint \"u_ex\"");
}

#[test]
fn v244_deferred_fires_at_flush() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (a int, CONSTRAINT t_ex EXCLUDE (a WITH =) DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    // Deferred: the conflicting insert is accepted for now...
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    // ...and COMMIT (flush) raises 23P01.
    let e = flush(&mut eng).expect_err("deferred conflict must fire at commit");
    assert_eq!(e.code, "23P01");
}

#[test]
fn v244_on_conflict_arbiter_rules() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, EXCLUDE (a WITH =))");
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    // Named exclusion arbiter + DO NOTHING absorbs.
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1) ON CONFLICT ON CONSTRAINT t_a_excl DO NOTHING",
    );
    // DO UPDATE with an exclusion arbiter: 42809.
    let e = err(
        &mut eng,
        "INSERT INTO t VALUES (1) ON CONFLICT ON CONSTRAINT t_a_excl DO UPDATE SET a = 2",
    );
    assert_eq!(e.code, "42809");
    assert_eq!(
        e.message,
        "ON CONFLICT DO UPDATE not supported with exclusion constraints"
    );
    // Inference naming the exclusion index: DO NOTHING absorbs.
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1) ON CONFLICT (a) DO NOTHING",
    );
    // A conflict on a DIFFERENT (non-arbiter) constraint still errors
    // when the arbiter names another constraint.
    ok(
        &mut eng,
        "CREATE TABLE u (a int UNIQUE, b int, EXCLUDE (b WITH =))",
    );
    ok(&mut eng, "INSERT INTO u VALUES (1, 10)");
    let e = err(
        &mut eng,
        "INSERT INTO u VALUES (2, 10) ON CONFLICT ON CONSTRAINT u_a_key DO NOTHING",
    );
    assert_eq!(e.code, "23P01");
}

#[test]
fn v244_deferrable_arbiter_55000() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (a int, CONSTRAINT dx EXCLUDE (a WITH =) DEFERRABLE)",
    );
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    let e = err(
        &mut eng,
        "INSERT INTO t VALUES (1) ON CONFLICT ON CONSTRAINT dx DO NOTHING",
    );
    assert_eq!(e.code, "55000");
    assert_eq!(
        e.message,
        "ON CONFLICT does not support deferrable unique constraints/exclusion constraints as arbiters"
    );
}

#[test]
fn v244_catalog_rows_and_constraintdef() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (a int, r int4range, EXCLUDE USING gist (a WITH =, r WITH &&) WHERE (a > 0))",
    );
    let row = one(
        &mut eng,
        "SELECT contype, condeferrable, condeferred FROM pg_constraint WHERE conname = 't_a_r_excl'",
    );
    assert_eq!(row, vec!["x", "f", "f"]);
    let def = one(
        &mut eng,
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 't_a_r_excl'",
    );
    assert_eq!(
        def[0],
        "EXCLUDE USING gist (a WITH =, r WITH &&) WHERE (a > 0)"
    );
    // Auto-naming: second identical-shape constraint gets suffix 1.
    ok(
        &mut eng,
        "CREATE TABLE c (c1 circle, c2 circle, EXCLUDE USING gist (c1 WITH &&, (c2::circle) WITH &&))",
    );
    let n = one(
        &mut eng,
        "SELECT conname FROM pg_constraint WHERE conrelid = 'c'::regclass AND contype = 'x'",
    );
    assert_eq!(n[0], "c_c1_c2_excl");
}

#[test]
fn v244_partitioned_table_rules() {
    let mut eng = engine();
    // Missing partition-key column: 0A000 + DETAIL.
    ok(
        &mut eng,
        "CREATE TABLE p (a int, b int4range) PARTITION BY RANGE (a)",
    );
    let e = err(
        &mut eng,
        "ALTER TABLE p ADD CONSTRAINT px EXCLUDE USING gist (b WITH &&)",
    );
    assert_eq!(e.code, "0A000");
    assert_eq!(
        e.message,
        "EXCLUDE constraint on partitioned table must include all partitioning columns"
    );
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "EXCLUDE constraint on table \"p\" lacks column \"a\" which is part of the partition key."
        )
    );
    // Key column present but only with a non-equal operator: 0A000.
    let e = err(
        &mut eng,
        "ALTER TABLE p ADD CONSTRAINT px EXCLUDE USING gist (a WITH &&, b WITH &&)",
    );
    assert_eq!(e.code, "0A000");
    assert_eq!(
        e.message,
        "cannot match partition key to index on column \"a\" using non-equal operator \"&&\""
    );
    // Valid: key column with =.
    ok(
        &mut eng,
        "ALTER TABLE p ADD CONSTRAINT px EXCLUDE USING gist (a WITH =, b WITH &&)",
    );
    ok(
        &mut eng,
        "CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (100)",
    );
    ok(&mut eng, "INSERT INTO p VALUES (1, '[1,5)')");
    let e = err(&mut eng, "INSERT INTO p VALUES (1, '[3,9)')");
    assert_eq!(e.code, "23P01");
}

#[test]
fn v244_geometry_types() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (c circle, b box, p point, EXCLUDE USING gist (c WITH &&), EXCLUDE USING gist (b WITH &&))",
    );
    ok(
        &mut eng,
        "INSERT INTO t VALUES ('<(0,0),1>', '((0,0),(1,1))', '(0,0)')",
    );
    // Circle overlap.
    let e = err(
        &mut eng,
        "INSERT INTO t VALUES ('<(1,0),1>', '((5,5),(6,6))', '(9,9)')",
    );
    assert_eq!(e.code, "23P01");
    // Box overlap (disjoint circle).
    let e = err(
        &mut eng,
        "INSERT INTO t VALUES ('<(10,10),1>', '((0,0),(2,2))', '(9,9)')",
    );
    assert_eq!(e.code, "23P01");
    // Canonical output forms.
    let row = one(&mut eng, "SELECT b FROM t");
    assert_eq!(row[0], "(1,1),(0,0)");
    let row = one(&mut eng, "SELECT c FROM t");
    assert_eq!(row[0], "<(0,0),1>");
    // Bad input: 22P02.
    let e = err(&mut eng, "INSERT INTO t VALUES ('bogus', NULL, NULL)");
    assert_eq!(e.code, "22P02");
    // Geometric functions (plan-time typing in func_result_type
    // plus the eval_func_vals match arm — a pre-dispatch hook in
    // eval_func_vals was a measurable codegen cliff, and without
    // the result types the calls die 42883 before evaluation).
    let row = one(&mut eng, "SELECT point(1,2)");
    assert_eq!(row[0], "(1,2)");
    let row = one(&mut eng, "SELECT circle_center('<(3,4),5>'::circle)");
    assert_eq!(row[0], "(3,4)");
    let row = one(&mut eng, "SELECT center(box(point(0,0),point(2,4)))");
    assert_eq!(row[0], "(1,2)");
    let row = one(&mut eng, "SELECT circle(point(1,1),2)");
    assert_eq!(row[0], "<(1,1),2>");
    let e = err(&mut eng, "SELECT circle(point(1,1),-1)");
    assert_eq!(e.code, "22023");
}

#[test]
fn v244_drop_constraint_removes_enforcement() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, EXCLUDE (a WITH =))");
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    ok(&mut eng, "ALTER TABLE t DROP CONSTRAINT t_a_excl");
    ok(&mut eng, "INSERT INTO t VALUES (1)");
    // And the catalog row is gone.
    let row = one(
        &mut eng,
        "SELECT count(*) FROM pg_constraint WHERE conname = 't_a_excl'",
    );
    assert_eq!(row[0], "0");
}
