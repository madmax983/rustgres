use super::*;

// v2.06: DEFERRED FK CHECKING + NOT ENFORCED FK SEMANTICS +
// DETACH PARTITION (+ SET DATA TYPE, ADD PRIMARY KEY USING INDEX,
// pg_constraint). PG19 grounding: trigger.c (RI queue,
// AfterTriggerSetState), tablecmds.c ATExecAlterConstraint /
// ATExecDetachPartition, gram.y ConstraintAttributeSpec.
//
// The harness runs each statement as its own xid on session 0 and
// never commits, so deferred flushes are driven explicitly through
// flush_deferred_fk — the same function the server calls at COMMIT.

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(41000);

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
fn v206_not_enforced_fk_lifecycle() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TABLE f (a int REFERENCES pk NOT ENFORCED)",
    );
    ok(&mut eng, "INSERT INTO f VALUES (999)");
    // Becoming ENFORCED validates existing rows: 23503.
    assert_eq!(
        err(&mut eng, "ALTER TABLE f ALTER CONSTRAINT f_a_fkey ENFORCED").code,
        "23503"
    );
    ok(&mut eng, "DELETE FROM f");
    ok(&mut eng, "ALTER TABLE f ALTER CONSTRAINT f_a_fkey ENFORCED");
    assert_eq!(err(&mut eng, "INSERT INTO f VALUES (999)").code, "23503");
    ok(
        &mut eng,
        "ALTER TABLE f ALTER CONSTRAINT f_a_fkey NOT ENFORCED",
    );
    ok(&mut eng, "INSERT INTO f VALUES (999)");
    // Parent-side actions are skipped too.
    ok(&mut eng, "INSERT INTO pk VALUES (1)");
    ok(&mut eng, "INSERT INTO f VALUES (1)");
    ok(&mut eng, "DELETE FROM pk WHERE a = 1");
}

#[test]
fn v206_not_enforced_check_and_add_constraint_form() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (b int)");
    ok(
        &mut eng,
        "ALTER TABLE t ADD CONSTRAINT t_b_chk CHECK (b > 10) NOT ENFORCED",
    );
    ok(&mut eng, "INSERT INTO t VALUES (5)");
    assert_eq!(
        err(&mut eng, "ALTER TABLE t ALTER CONSTRAINT t_b_chk ENFORCED").code,
        "23514"
    );
    ok(&mut eng, "DELETE FROM t");
    ok(&mut eng, "ALTER TABLE t ALTER CONSTRAINT t_b_chk ENFORCED");
    assert_eq!(err(&mut eng, "INSERT INTO t VALUES (5)").code, "23514");
    // Column-level CHECK NOT ENFORCED at CREATE.
    ok(
        &mut eng,
        "CREATE TABLE t2 (x int CHECK (x > 0) NOT ENFORCED)",
    );
    ok(&mut eng, "INSERT INTO t2 VALUES (-1)");
}

#[test]
fn v206_deferred_child_check_at_flush() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TABLE f (a int REFERENCES pk DEFERRABLE INITIALLY DEFERRED)",
    );
    // Bad insert is accepted at statement time (queued)...
    ok(&mut eng, "INSERT INTO f VALUES (7)");
    // ...and fails when the queue fires.
    assert_eq!(flush(&mut eng).unwrap_err().code, "23503");
    // Fix the parent side, requeue, fire clean.
    ok(&mut eng, "INSERT INTO f VALUES (8)");
    ok(&mut eng, "INSERT INTO pk VALUES (8)");
    flush(&mut eng).expect("flush ok");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM f"), 2);
}

#[test]
fn v206_set_constraints_modes_and_errors() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(&mut eng, "CREATE TABLE f (a int REFERENCES pk DEFERRABLE)");
    // Initially immediate: bad insert fails at once.
    assert_eq!(err(&mut eng, "INSERT INTO f VALUES (1)").code, "23503");
    ok(&mut eng, "SET CONSTRAINTS f_a_fkey DEFERRED");
    ok(&mut eng, "INSERT INTO f VALUES (1)");
    // IMMEDIATE fires the queued check now.
    assert_eq!(
        err(&mut eng, "SET CONSTRAINTS f_a_fkey IMMEDIATE").code,
        "23503"
    );
    // Errors: unknown name 42704; non-deferrable (the PK) 55000.
    assert_eq!(err(&mut eng, "SET CONSTRAINTS nope DEFERRED").code, "42704");
    assert_eq!(
        err(&mut eng, "SET CONSTRAINTS pk_pkey DEFERRED").code,
        "55000"
    );
    deferred_clear(&eng, 0);
}

#[test]
fn v206_deferred_parent_restrict_at_flush() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TABLE f (a int REFERENCES pk DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(&mut eng, "INSERT INTO pk VALUES (1)");
    ok(&mut eng, "INSERT INTO f VALUES (1)");
    flush(&mut eng).expect("setup flush");
    // Deleting the parent queues instead of erroring...
    ok(&mut eng, "DELETE FROM pk WHERE a = 1");
    // ...until the queue fires.
    assert_eq!(flush(&mut eng).unwrap_err().code, "23503");
    // Delete + reinsert of the same key inside the txn is fine.
    let mut eng2 = engine();
    ok(&mut eng2, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(
        &mut eng2,
        "CREATE TABLE f (a int REFERENCES pk DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(&mut eng2, "INSERT INTO pk VALUES (1)");
    ok(&mut eng2, "INSERT INTO f VALUES (1)");
    flush(&mut eng2).expect("setup flush");
    ok(&mut eng2, "DELETE FROM pk WHERE a = 1");
    ok(&mut eng2, "INSERT INTO pk VALUES (1)");
    flush(&mut eng2).expect("reinsert flush");
}

#[test]
fn v206_match_full() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (a int, b int, UNIQUE (a, b))");
    ok(
        &mut eng,
        "CREATE TABLE f (x int, y int, FOREIGN KEY (x, y) REFERENCES pk (a, b) MATCH FULL)",
    );
    assert_eq!(
        err(&mut eng, "INSERT INTO f VALUES (1, NULL)").code,
        "23503"
    );
    ok(&mut eng, "INSERT INTO f VALUES (NULL, NULL)");
    ok(&mut eng, "INSERT INTO pk VALUES (1, 2)");
    ok(&mut eng, "INSERT INTO f VALUES (1, 2)");
    // MATCH SIMPLE (default) still skips on any NULL.
    ok(
        &mut eng,
        "CREATE TABLE f2 (x int, y int, FOREIGN KEY (x, y) REFERENCES pk (a, b))",
    );
    ok(&mut eng, "INSERT INTO f2 VALUES (1, NULL)");
}

#[test]
fn v206_alter_constraint_errors_and_flags() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(&mut eng, "CREATE TABLE f (a int REFERENCES pk)");
    assert_eq!(
        err(&mut eng, "ALTER TABLE f ALTER CONSTRAINT nope ENFORCED").code,
        "42704"
    );
    ok(
        &mut eng,
        "ALTER TABLE f ALTER CONSTRAINT f_a_fkey DEFERRABLE INITIALLY DEFERRED",
    );
    // pg_constraint reports the new flags.
    match run(&mut eng, "SELECT condeferrable, condeferred, conenforced FROM pg_constraint WHERE conname = 'f_a_fkey'").expect("catalog") {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Bool(true));
            assert_eq!(rows[0][1], Value::Bool(true));
            assert_eq!(rows[0][2], Value::Bool(true));
        }
        other => panic!("not a select: {other:?}"),
    }
    ok(
        &mut eng,
        "ALTER TABLE f ALTER CONSTRAINT f_a_fkey NOT DEFERRABLE",
    );
    // NOT DEFERRABLE also clears INITIALLY DEFERRED.
    match run(
        &mut eng,
        "SELECT condeferrable, condeferred FROM pg_constraint WHERE conname = 'f_a_fkey'",
    )
    .expect("catalog")
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::Bool(false));
            assert_eq!(rows[0][1], Value::Bool(false));
        }
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn v206_detach_partition() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE p (a int, b int) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (100)",
    );
    ok(
        &mut eng,
        "CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (100) TO (200)",
    );
    ok(&mut eng, "INSERT INTO p VALUES (50, 1), (150, 2)");
    ok(&mut eng, "ALTER TABLE p DETACH PARTITION p1");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM p"), 1);
    assert_eq!(count(&mut eng, "SELECT count(*) FROM p1"), 1);
    // The detached table is standalone: any value inserts now.
    ok(&mut eng, "INSERT INTO p1 VALUES (500, 3)");
    assert_eq!(
        err(&mut eng, "ALTER TABLE p DETACH PARTITION p1").code,
        "42P01"
    );
    ok(&mut eng, "ALTER TABLE p DETACH PARTITION p2 CONCURRENTLY");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM p2"), 1);
}

#[test]
fn v206_set_data_type_and_pk_using_index() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text)");
    ok(&mut eng, "INSERT INTO t VALUES (1, 'x')");
    ok(
        &mut eng,
        "ALTER TABLE t ALTER COLUMN a SET DATA TYPE bigint",
    );
    ok(&mut eng, "ALTER TABLE t ALTER a SET DATA TYPE text");
    ok(&mut eng, "CREATE TABLE u (a int NOT NULL, b int)");
    ok(&mut eng, "CREATE UNIQUE INDEX u_idx ON u (a)");
    ok(&mut eng, "ALTER TABLE u ADD PRIMARY KEY USING INDEX u_idx");
    ok(&mut eng, "INSERT INTO u VALUES (1, 1)");
    assert_eq!(err(&mut eng, "INSERT INTO u VALUES (1, 2)").code, "23505");
    ok(&mut eng, "CREATE TABLE v (a int NOT NULL)");
    ok(&mut eng, "CREATE INDEX v_idx ON v (a)");
    assert_eq!(
        err(&mut eng, "ALTER TABLE v ADD PRIMARY KEY USING INDEX v_idx").code,
        "42809"
    );
}

#[test]
fn v206_on_update_cascade_moves_child_key() {
    // Regression (found in the v2.06 schedule diff, proven broken on
    // the v2.05 base binary too): the cascade planner re-checked the
    // cascaded child row against the parent before the parent's new
    // row version existed, so plain ON UPDATE CASCADE failed 23503.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE p (a int PRIMARY KEY, b text)");
    ok(
        &mut eng,
        "CREATE TABLE f (x int REFERENCES p ON UPDATE CASCADE ON DELETE CASCADE, y int)",
    );
    ok(&mut eng, "INSERT INTO p VALUES (2, 'two')");
    ok(&mut eng, "INSERT INTO f VALUES (2, 3)");
    ok(&mut eng, "UPDATE p SET a = 1 WHERE a = 2");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM f WHERE x = 1"), 1);
    // Restrict still fires for a non-cascade FK on the same shape.
    ok(&mut eng, "CREATE TABLE f2 (x int REFERENCES p, y int)");
    ok(&mut eng, "INSERT INTO f2 VALUES (1, 9)");
    assert_eq!(
        err(&mut eng, "UPDATE p SET a = 5 WHERE a = 1").code,
        "23503"
    );
}

#[test]
fn v206_multiple_fks_same_row_compose() {
    // foreign_key.sql users/tasks shape: three FKs from one child
    // to the same parent, all acting on the same row. PG applies
    // every RI action to the row's current image; the pre-fix
    // planner's blanket already-handled skip applied only the
    // first action per row.
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TEMP TABLE users (id int PRIMARY KEY, name text)",
    );
    ok(&mut eng, "INSERT INTO users VALUES (3, 'Samko')");
    ok(
        &mut eng,
        "CREATE TEMP TABLE tasks (id int PRIMARY KEY, \
         owner int REFERENCES users ON UPDATE CASCADE ON DELETE SET NULL, \
         worker int REFERENCES users ON UPDATE CASCADE ON DELETE SET NULL, \
         checked_by int REFERENCES users ON UPDATE CASCADE ON DELETE SET NULL)",
    );
    ok(&mut eng, "INSERT INTO tasks VALUES (3, 3, 3, 3)");
    ok(&mut eng, "UPDATE users SET id = 4 WHERE id = 3");
    assert_eq!(
        count(
            &mut eng,
            "SELECT count(*) FROM tasks WHERE owner = 4 AND worker = 4 AND checked_by = 4"
        ),
        1
    );
    ok(&mut eng, "DELETE FROM users WHERE id = 4");
    assert_eq!(
        count(
            &mut eng,
            "SELECT count(*) FROM tasks WHERE owner IS NULL AND worker IS NULL AND checked_by IS NULL"
        ),
        1
    );
}

#[test]
fn v206_deferred_fk_against_partitioned_parent() {
    // fkpart9 shape: the referenced key lives in a partition leaf;
    // both the immediate check and the deferred flush must find it
    // (fk_parent_key_exists walks the partition tree).
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pk (a int PRIMARY KEY) PARTITION BY LIST (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pk1 PARTITION OF pk FOR VALUES IN (1, 2) PARTITION BY LIST (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pk11 PARTITION OF pk1 FOR VALUES IN (1)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pk3 PARTITION OF pk FOR VALUES IN (3)",
    );
    ok(
        &mut eng,
        "CREATE TABLE fk (a int REFERENCES pk DEFERRABLE INITIALLY IMMEDIATE)",
    );
    ok(&mut eng, "INSERT INTO pk VALUES (1)");
    ok(&mut eng, "INSERT INTO fk VALUES (1)");
    assert_eq!(err(&mut eng, "INSERT INTO fk VALUES (3)").code, "23503");
    // Deferred: queue the child check, satisfy it, flush clean.
    ok(&mut eng, "SET CONSTRAINTS fk_a_fkey DEFERRED");
    ok(&mut eng, "INSERT INTO fk VALUES (3)");
    ok(&mut eng, "INSERT INTO pk VALUES (3)");
    flush(&mut eng).expect("deferred flush finds leaf key");
    // Deferred violation still fires at flush.
    ok(&mut eng, "INSERT INTO fk VALUES (99)");
    assert_eq!(flush(&mut eng).unwrap_err().code, "23503");
}

#[test]
fn v206_attach_default_with_rows() {
    // PG rule for ATTACH ... DEFAULT: existing rows must fall
    // outside every sibling bound (the pre-fix code tested them
    // against the dummy default bound and always failed 23514).
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pa (a int, b int) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pa_p1 PARTITION OF pa FOR VALUES FROM (100) TO (200)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pa_p2 PARTITION OF pa FOR VALUES FROM (200) TO (300)",
    );
    ok(&mut eng, "INSERT INTO pa VALUES (150, 1), (250, 2)");
    ok(&mut eng, "ALTER TABLE pa DETACH PARTITION pa_p1");
    ok(&mut eng, "ALTER TABLE pa ATTACH PARTITION pa_p1 DEFAULT");
    // A candidate whose rows collide with a sibling bound fails
    // (free the DEFAULT slot first so row validation is reached).
    ok(&mut eng, "ALTER TABLE pa DETACH PARTITION pa_p1");
    ok(&mut eng, "CREATE TABLE stray (a int, b int)");
    ok(&mut eng, "INSERT INTO stray VALUES (250, 9)");
    assert_eq!(
        err(&mut eng, "ALTER TABLE pa ATTACH PARTITION stray DEFAULT").code,
        "23514"
    );
}

#[test]
fn v206_initially_deferred_implies_deferrable() {
    // PG19 parse_utilcmd.c: "If only INITIALLY DEFERRED appears,
    // assume DEFERRABLE"; an explicit NOT DEFERRABLE conflicts 42601.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TABLE f (a int REFERENCES pk INITIALLY DEFERRED)",
    );
    ok(&mut eng, "INSERT INTO f VALUES (7)");
    ok(&mut eng, "INSERT INTO pk VALUES (7)");
    flush(&mut eng).expect("implied-deferrable FK defers and fires clean");
    // The conflicting form is a PARSE error (err() only models exec
    // errors), so assert on parse_statement directly.
    let perr = crate::sql::parse_statement(
        "CREATE TABLE g (a int REFERENCES pk NOT DEFERRABLE INITIALLY DEFERRED)",
    )
    .expect_err("conflicting attrs must not parse");
    assert_eq!(perr.code, "42601");
}

#[test]
fn v206_fk_not_valid_add() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(&mut eng, "CREATE TABLE f (a int)");
    ok(&mut eng, "INSERT INTO f VALUES (999)");
    // NOT VALID skips the existing-row validation...
    ok(
        &mut eng,
        "ALTER TABLE f ADD CONSTRAINT f_a_fkey FOREIGN KEY (a) REFERENCES pk NOT VALID NOT ENFORCED",
    );
    // ...ENFORCED alone does not validate while NOT VALID stands? PG:
    // ENFORCED validates. Our alter validates on the ENFORCED
    // transition and clears NOT VALID, so this must fail 23503.
    assert_eq!(
        err(&mut eng, "ALTER TABLE f ALTER CONSTRAINT f_a_fkey ENFORCED").code,
        "23503"
    );
    ok(&mut eng, "DELETE FROM f");
    ok(&mut eng, "ALTER TABLE f ALTER CONSTRAINT f_a_fkey ENFORCED");
    assert_eq!(err(&mut eng, "INSERT INTO f VALUES (999)").code, "23503");
}
