use super::*;

// v2.07: DEFERRED UNIQUE CONSTRAINTS + UNIQUE ATTRIBUTE PARSING
// (+ ADD UNIQUE USING INDEX, ALTER CONSTRAINT on unique/PK errors,
// FK-references-deferrable-unique refusal, TRUNCATE covered-set
// semantics). PG19 grounding: gram.y ConstraintAttributeSpec,
// parse_utilcmd.c transformConstraintAttrs, tablecmds.c
// ATExecAlterConstraint + the deferrable-referenced-table refusals,
// trigger.c AfterTriggerSetState, truncate.c.
//
// Same harness shape as v206: each statement runs as its own xid on
// session 0 and never commits, so deferred flushes are driven
// explicitly through flush_deferred_fk (the COMMIT path).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(71000);

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let xid = NEXT_XID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stmt = crate::sql::parse_statement(sql).expect("parses");
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
    run(eng, sql).expect_err(&format!("{sql} should fail"))
}

/// Fire the session's deferred queue, as COMMIT would.
fn flush(eng: &mut Engine) -> Result<(), ExecError> {
    let xid = NEXT_XID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        role: "postgres",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    flush_deferred_fk(eng, &mut ctx, None)
}

fn count(eng: &mut Engine, sql: &str) -> i64 {
    match run(eng, sql).expect("count query") {
        ExecResult::Select { rows, .. } => match &rows[0][0] {
            Value::Int(i) | Value::BigInt(i) => *i,
            other => panic!("count not int: {other:?}"),
        },
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn v207_deferrable_unique_parse_and_catalog() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE u1 (i int UNIQUE DEFERRABLE, t text)",
    );
    // pg_constraint reports the real flags for contype 'u'.
    match run(
        &mut eng,
        "SELECT condeferrable, condeferred FROM pg_constraint WHERE conname = 'u1_i_key'",
    )
    .expect("pg_constraint select")
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Bool(true));
            assert_eq!(rows[0][1], Value::Bool(false));
        }
        other => panic!("not a select: {other:?}"),
    }
    // Misplaced ENFORCED on a unique constraint: 42601 (parse_utilcmd).
    assert_eq!(
        crate::sql::parse_statement("CREATE TABLE bad (i int UNIQUE NOT ENFORCED)")
            .expect_err("should not parse")
            .code,
        "42601"
    );
    // NOT DEFERRABLE + INITIALLY DEFERRED: 42601.
    assert_eq!(
        crate::sql::parse_statement(
            "CREATE TABLE bad (i int UNIQUE NOT DEFERRABLE INITIALLY DEFERRED)"
        )
        .expect_err("should not parse")
        .code,
        "42601"
    );
    // INITIALLY DEFERRED alone implies DEFERRABLE.
    ok(
        &mut eng,
        "CREATE TABLE u2 (i int UNIQUE INITIALLY DEFERRED, t text)",
    );
    match run(
        &mut eng,
        "SELECT condeferrable, condeferred FROM pg_constraint WHERE conname = 'u2_i_key'",
    )
    .expect("pg_constraint select")
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::Bool(true));
            assert_eq!(rows[0][1], Value::Bool(true));
        }
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn v207_set_constraints_deferred_unique_commit_paths() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE uq (i int UNIQUE DEFERRABLE, t text)",
    );
    ok(&mut eng, "INSERT INTO uq VALUES (1, 'one')");
    ok(&mut eng, "INSERT INTO uq VALUES (2, 'two')");
    // IMMEDIATE by default: a duplicate insert fails at the statement.
    assert_eq!(
        err(&mut eng, "INSERT INTO uq VALUES (1, 'uno')").code,
        "23505"
    );
    // Defer it: duplicate insert is accepted, and deleting the
    // conflicting row before COMMIT makes the flush pass.
    ok(&mut eng, "SET CONSTRAINTS uq_i_key DEFERRED");
    ok(&mut eng, "INSERT INTO uq VALUES (1, 'uno')");
    ok(&mut eng, "DELETE FROM uq WHERE t = 'one'");
    flush(&mut eng).expect("flush after conflict resolved");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM uq"), 2);
    // Unresolved duplicate fails at flush (COMMIT) with 23505.
    ok(&mut eng, "INSERT INTO uq VALUES (2, 'dos')");
    let e = flush(&mut eng).expect_err("flush should fail");
    assert_eq!(e.code, "23505");
    // A non-deferrable unique still 55000s on SET CONSTRAINTS.
    ok(&mut eng, "CREATE TABLE nu (i int UNIQUE)");
    assert_eq!(
        err(&mut eng, "SET CONSTRAINTS nu_i_key DEFERRED").code,
        "55000"
    );
    assert_eq!(
        err(&mut eng, "SET CONSTRAINTS no_such_key DEFERRED").code,
        "42704"
    );
}

#[test]
fn v207_initially_deferred_unique_and_update_swap() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE ud (i int, t text, CONSTRAINT ud_i_key UNIQUE (i) DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(&mut eng, "INSERT INTO ud VALUES (1, 'one')");
    ok(&mut eng, "INSERT INTO ud VALUES (2, 'two')");
    // The constraints.sql dance: swap keys via two UPDATEs and delete
    // the duplicates — final image is unique, so flush passes.
    ok(&mut eng, "INSERT INTO ud VALUES (1, 'uno')");
    ok(&mut eng, "UPDATE ud SET i = 3 WHERE t = 'uno'");
    flush(&mut eng).expect("flush after swap");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM ud"), 3);
    // SET CONSTRAINTS ... IMMEDIATE forces the check now.
    ok(&mut eng, "INSERT INTO ud VALUES (2, 'dos')");
    assert_eq!(
        err(&mut eng, "SET CONSTRAINTS ud_i_key IMMEDIATE").code,
        "23505"
    );
}

#[test]
fn v207_deferrable_pkey_and_alter_constraint_errors() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pkd (a int PRIMARY KEY INITIALLY DEFERRED)",
    );
    ok(&mut eng, "INSERT INTO pkd VALUES (1)");
    ok(&mut eng, "INSERT INTO pkd VALUES (1)");
    let e = flush(&mut eng).expect_err("deferred PK flush should fail");
    assert_eq!(e.code, "23505");
    // ALTER CONSTRAINT on a unique/PK: PG errors for both clause
    // families (ATExecAlterConstraint).
    assert_eq!(
        err(
            &mut eng,
            "ALTER TABLE pkd ALTER CONSTRAINT pkd_pkey DEFERRABLE"
        )
        .code,
        "42809"
    );
    assert_eq!(
        err(
            &mut eng,
            "ALTER TABLE pkd ALTER CONSTRAINT pkd_pkey NOT ENFORCED"
        )
        .code,
        "42809"
    );
}

#[test]
fn v207_fk_cannot_reference_deferred_unique() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE refd (a int UNIQUE INITIALLY DEFERRED)",
    );
    // PG: 55000 cannot use a deferrable unique constraint.
    assert_eq!(
        err(&mut eng, "CREATE TABLE child (b int REFERENCES refd (a))").code,
        "55000"
    );
    // DEFERRABLE INITIALLY IMMEDIATE is immediate: referenceable.
    ok(
        &mut eng,
        "CREATE TABLE refi (a int UNIQUE DEFERRABLE INITIALLY IMMEDIATE)",
    );
    ok(&mut eng, "CREATE TABLE child2 (b int REFERENCES refi (a))");
}

#[test]
fn v207_add_unique_using_index() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE cnn (a int)");
    ok(&mut eng, "CREATE UNIQUE INDEX cnn_idx ON cnn (a)");
    ok(&mut eng, "ALTER TABLE cnn ADD UNIQUE USING INDEX cnn_idx");
    ok(&mut eng, "INSERT INTO cnn VALUES (1)");
    assert_eq!(err(&mut eng, "INSERT INTO cnn VALUES (1)").code, "23505");
    // Non-unique index: 42809.
    ok(&mut eng, "CREATE TABLE cnn2 (a int)");
    ok(&mut eng, "CREATE INDEX cnn2_idx ON cnn2 (a)");
    assert_eq!(
        err(&mut eng, "ALTER TABLE cnn2 ADD UNIQUE USING INDEX cnn2_idx").code,
        "42809"
    );
}

#[test]
fn v207_partitioned_deferrable_unique() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pu (i int UNIQUE DEFERRABLE) PARTITION BY RANGE (i)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pu_1 PARTITION OF pu FOR VALUES FROM (0) TO (10)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pu_2 PARTITION OF pu FOR VALUES FROM (20) TO (30)",
    );
    ok(&mut eng, "INSERT INTO pu VALUES (1)");
    // Immediate mode: the second insert violates at the statement.
    assert_eq!(err(&mut eng, "INSERT INTO pu VALUES (1)").code, "23505");
    // Deferred via the root's constraint name: accepted, fails at flush.
    ok(&mut eng, "SET CONSTRAINTS pu_i_key DEFERRED");
    ok(&mut eng, "INSERT INTO pu VALUES (1)");
    let e = flush(&mut eng).expect_err("partitioned flush should fail");
    assert_eq!(e.code, "23505");
}

#[test]
fn v207_truncate_cascade_via_partition_root_fk() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE ta (a int PRIMARY KEY) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE ta1 PARTITION OF ta FOR VALUES FROM (0) TO (10)",
    );
    ok(
        &mut eng,
        "CREATE TABLE ta2 PARTITION OF ta FOR VALUES FROM (10) TO (20)",
    );
    ok(
        &mut eng,
        "CREATE TABLE ref_b (b int PRIMARY KEY, a int REFERENCES ta (a) ON DELETE CASCADE)",
    );
    ok(&mut eng, "INSERT INTO ta VALUES (0), (5), (15)");
    ok(
        &mut eng,
        "INSERT INTO ref_b VALUES (10, 0), (50, 5), (100, 15)",
    );
    // Without CASCADE, truncating a leaf whose root is referenced blocks.
    assert_eq!(err(&mut eng, "TRUNCATE TABLE ta1").code, "2BP01");
    // With CASCADE, the referencing table is truncated IN FULL.
    ok(&mut eng, "TRUNCATE TABLE ta1 CASCADE");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM ref_b"), 0);
    assert_eq!(count(&mut eng, "SELECT count(*) FROM ta"), 1);
}

#[test]
fn v207_truncate_statement_set() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE sa (a int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TABLE sb (b int PRIMARY KEY, a int REFERENCES sa (a))",
    );
    ok(&mut eng, "INSERT INTO sa VALUES (1)");
    ok(&mut eng, "INSERT INTO sb VALUES (1, 1)");
    // Referencing table in the same statement set: no block.
    ok(&mut eng, "TRUNCATE TABLE sa, sb");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM sa"), 0);
    assert_eq!(count(&mut eng, "SELECT count(*) FROM sb"), 0);
    // Alone it blocks again.
    ok(&mut eng, "INSERT INTO sa VALUES (1)");
    assert_eq!(err(&mut eng, "TRUNCATE TABLE sa").code, "2BP01");
}

#[test]
fn v207_using_index_requires_default_sorting() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE cwi (a int, b varchar(10), c char)");
    ok(&mut eng, "INSERT INTO cwi VALUES (1, 'x', 'y')");
    // DESC index column: no default sorting behavior (42809).
    ok(&mut eng, "CREATE UNIQUE INDEX cwi_desc ON cwi (a DESC)");
    let e = err(&mut eng, "ALTER TABLE cwi ADD UNIQUE USING INDEX cwi_desc");
    assert_eq!(e.code, "42809");
    assert!(
        e.message.contains("default sorting behavior"),
        "{}",
        e.message
    );
    // Non-default collation on a key column: same 42809, and the
    // ADD PRIMARY KEY USING INDEX path shares the check.
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX cwi_coll ON cwi (b COLLATE \"POSIX\")",
    );
    let e = err(&mut eng, "ALTER TABLE cwi ADD UNIQUE USING INDEX cwi_coll");
    assert_eq!(e.code, "42809");
    let e = err(
        &mut eng,
        "ALTER TABLE cwi ADD PRIMARY KEY USING INDEX cwi_desc",
    );
    assert_eq!(e.code, "42809");
    // A plain default index still adopts cleanly.
    ok(&mut eng, "CREATE UNIQUE INDEX cwi_plain ON cwi (a, b)");
    ok(&mut eng, "ALTER TABLE cwi ADD UNIQUE USING INDEX cwi_plain");
}

#[test]
fn v207_replica_identity_rejects_deferrable_index() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE ri (id int PRIMARY KEY, keya text NOT NULL, keyb text NOT NULL, \
         CONSTRAINT ri_defer UNIQUE (keya, keyb) DEFERRABLE, \
         CONSTRAINT ri_plain UNIQUE (keya))",
    );
    // PG19: a deferrable unique index is not indimmediate (0A000).
    let e = err(
        &mut eng,
        "ALTER TABLE ri REPLICA IDENTITY USING INDEX ri_defer",
    );
    assert_eq!(e.code, "0A000");
    assert!(e.message.contains("non-immediate"), "{}", e.message);
    // The non-deferrable unique constraint's index is fine.
    ok(
        &mut eng,
        "ALTER TABLE ri REPLICA IDENTITY USING INDEX ri_plain",
    );
}

#[test]
fn v207_deferrable_pkey_is_not_replica_identity() {
    let mut eng = engine();
    // PG19 publication.sql testpub_tbl5d: a DEFERRABLE primary key's
    // index is non-immediate, so the published table has no replica
    // identity and UPDATE errors (55000 family via the guard's code).
    ok(
        &mut eng,
        "CREATE TABLE pub5d (a int PRIMARY KEY DEFERRABLE)",
    );
    ok(&mut eng, "INSERT INTO pub5d VALUES (1)");
    ok(&mut eng, "CREATE PUBLICATION pub5dp FOR TABLE pub5d");
    let e = err(&mut eng, "UPDATE pub5d SET a = 1");
    assert!(
        e.message.contains("replica identity"),
        "{} {}",
        e.code,
        e.message
    );
    // A plain (immediate) PK does serve: same shape updates cleanly.
    ok(&mut eng, "CREATE TABLE pub5e (a int PRIMARY KEY)");
    ok(&mut eng, "INSERT INTO pub5e VALUES (1)");
    ok(&mut eng, "CREATE PUBLICATION pub5ep FOR TABLE pub5e");
    ok(&mut eng, "UPDATE pub5e SET a = 1");
}
