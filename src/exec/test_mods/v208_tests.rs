use super::*;

// v2.08: NULLS NOT DISTINCT for UNIQUE indexes and constraints.
// PG19 grounding: gram.y opt_unique_null_treatment / IndexStmt
// (clause sits between UNIQUE and the column list for table
// constraints, right after UNIQUE for column constraints, after the
// column list for CREATE INDEX), indexcmds.c (ii_NullsNotDistinct),
// execIndexing.c (NULL keys searched as IS NULL keys under NND),
// execPartition.c IsIndexCompatibleAsArbiter (null treatment must
// match), catalog/index.c (NND index cannot back a PRIMARY KEY,
// 42P16), pg_index.indnullsnotdistinct.
//
// Same harness shape as v207: each statement runs as its own xid on
// session 0 and never commits, so deferred flushes are driven
// explicitly through flush_deferred_fk (the COMMIT path).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(81000);

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
fn v208_parse_forms_carry_the_flag() {
    // Column constraint: UNIQUE NULLS NOT DISTINCT.
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE c1 (i int UNIQUE NULLS NOT DISTINCT, t text)",
    );
    let snap = eng.take_snapshot();
    let t = eng.db.find_table("c1", &snap, &[0], 0).expect("c1 exists");
    assert_eq!(t.uniques.len(), 1);
    assert!(t.uniques[0].nulls_not_distinct);
    // The backing index carries it too.
    let ix = eng
        .db
        .indexes
        .get(&t.uniques[0].name)
        .expect("backing index");
    assert!(ix.def.nulls_not_distinct);

    // Table constraint: clause between UNIQUE and the column list.
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE c2 (i int, t text, UNIQUE NULLS NOT DISTINCT (i))",
    );
    let snap = eng.take_snapshot();
    let t = eng.db.find_table("c2", &snap, &[0], 0).expect("c2 exists");
    assert!(t.uniques[0].nulls_not_distinct);

    // Explicit NULLS DISTINCT parses to false; bare UNIQUE too.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE c3 (i int UNIQUE NULLS DISTINCT)");
    ok(&mut eng, "CREATE TABLE c4 (i int UNIQUE)");
    let snap = eng.take_snapshot();
    assert!(!eng.db.find_table("c3", &snap, &[0], 0).unwrap().uniques[0].nulls_not_distinct);
    assert!(!eng.db.find_table("c4", &snap, &[0], 0).unwrap().uniques[0].nulls_not_distinct);

    // CREATE INDEX: clause after the column list.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE c5 (i int, t text)");
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX c5_nnd ON c5 (i) NULLS NOT DISTINCT",
    );
    assert!(eng.db.indexes.get("c5_nnd").unwrap().def.nulls_not_distinct);
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX c5_d ON c5 (t) NULLS DISTINCT",
    );
    assert!(!eng.db.indexes.get("c5_d").unwrap().def.nulls_not_distinct);
}

#[test]
fn v208_duplicate_null_rejected_distinct_unchanged() {
    // The constraints-suite #49 shape.
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE ut (i int UNIQUE NULLS NOT DISTINCT, t text)",
    );
    ok(&mut eng, "INSERT INTO ut VALUES (NULL, 'seven')");
    let e = err(&mut eng, "INSERT INTO ut VALUES (NULL, 'seven again')");
    assert_eq!(e.code, "23505");
    // ON CONFLICT DO NOTHING (no arbiter) swallows the NULL conflict.
    ok(
        &mut eng,
        "INSERT INTO ut VALUES (NULL, 'eight') ON CONFLICT DO NOTHING",
    );
    assert_eq!(count(&mut eng, "SELECT count(*) FROM ut"), 1);

    // Plain UNIQUE (NULLS DISTINCT default): two NULLs are fine.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE ud (i int UNIQUE, t text)");
    ok(&mut eng, "INSERT INTO ud VALUES (NULL, 'one')");
    ok(&mut eng, "INSERT INTO ud VALUES (NULL, 'two')");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM ud"), 2);
}

#[test]
fn v208_composite_key_partial_null() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE ck (a int, b int, UNIQUE NULLS NOT DISTINCT (a, b))",
    );
    ok(&mut eng, "INSERT INTO ck VALUES (1, NULL)");
    // Under NND a partially-NULL key still conflicts.
    let e = err(&mut eng, "INSERT INTO ck VALUES (1, NULL)");
    assert_eq!(e.code, "23505");
    ok(&mut eng, "INSERT INTO ck VALUES (2, NULL)");
    ok(&mut eng, "INSERT INTO ck VALUES (NULL, NULL)");
    let e = err(&mut eng, "INSERT INTO ck VALUES (NULL, NULL)");
    assert_eq!(e.code, "23505");
}

#[test]
fn v208_update_to_null_conflicts() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE uu (i int UNIQUE NULLS NOT DISTINCT, t text)",
    );
    ok(&mut eng, "INSERT INTO uu VALUES (1, 'a')");
    ok(&mut eng, "INSERT INTO uu VALUES (NULL, 'b')");
    let e = err(&mut eng, "UPDATE uu SET i = NULL WHERE t = 'a'");
    assert_eq!(e.code, "23505");
    // A non-conflicting update still works.
    ok(&mut eng, "UPDATE uu SET i = 2 WHERE t = 'a'");
}

#[test]
fn v208_arbiter_spec_does_not_absorb_nnd() {
    // insert_conflict #71 shape: a plain UNIQUE constraint on b plus
    // an NND index on b. ON CONFLICT ON CONSTRAINT names the DISTINCT
    // constraint; a NULL key passes it (NULLs distinct) and then the
    // NND index raises its own 23505.
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE nc (a int, b int, CONSTRAINT nc_b_key UNIQUE (b))",
    );
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX nc_b_nnd ON nc (b) NULLS NOT DISTINCT",
    );
    ok(&mut eng, "INSERT INTO nc VALUES (1, NULL)");
    let e = err(
        &mut eng,
        "INSERT INTO nc VALUES (2, NULL) ON CONFLICT ON CONSTRAINT nc_b_key DO NOTHING",
    );
    assert_eq!(e.code, "23505");
    assert!(
        e.message.contains("nc_b_nnd"),
        "error should name the NND index: {}",
        e.message
    );
    // DO NOTHING without an arbiter DOES absorb the NND conflict.
    ok(
        &mut eng,
        "INSERT INTO nc VALUES (3, NULL) ON CONFLICT DO NOTHING",
    );
    assert_eq!(count(&mut eng, "SELECT count(*) FROM nc"), 1);
    // A column-list spec on the DISTINCT constraint arbitrates a
    // non-NULL conflict normally.
    ok(&mut eng, "INSERT INTO nc VALUES (4, 5)");
    ok(
        &mut eng,
        "INSERT INTO nc VALUES (5, 5) ON CONFLICT (b) DO NOTHING",
    );
    assert_eq!(count(&mut eng, "SELECT count(*) FROM nc"), 2);
}

#[test]
fn v208_deferred_nnd_revalidation() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE dn (i int UNIQUE NULLS NOT DISTINCT DEFERRABLE INITIALLY DEFERRED)",
    );
    ok(&mut eng, "INSERT INTO dn VALUES (NULL)");
    // Deferred: the duplicate is tolerated until COMMIT.
    ok(&mut eng, "INSERT INTO dn VALUES (NULL)");
    let e = flush(&mut eng).expect_err("commit-time revalidation must fail");
    assert_eq!(e.code, "23505");
}

#[test]
fn v208_partitioned_nnd() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pp (g text, i int, UNIQUE NULLS NOT DISTINCT (g, i)) PARTITION BY LIST (g)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pp_a PARTITION OF pp FOR VALUES IN ('a')",
    );
    ok(&mut eng, "CREATE TABLE pp_d PARTITION OF pp DEFAULT");
    ok(&mut eng, "INSERT INTO pp VALUES ('a', NULL)");
    let e = err(&mut eng, "INSERT INTO pp VALUES ('a', NULL)");
    assert_eq!(e.code, "23505");
    // A different leaf may hold its own NULL (per-leaf uniqueness).
    ok(&mut eng, "INSERT INTO pp VALUES ('z', NULL)");
    assert_eq!(count(&mut eng, "SELECT count(*) FROM pp"), 2);
}

#[test]
fn v208_using_index_adoption_and_pk_refusal() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE ui (i int, t text)");
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX ui_nnd ON ui (i) NULLS NOT DISTINCT",
    );
    ok(
        &mut eng,
        "ALTER TABLE ui ADD CONSTRAINT ui_uq UNIQUE USING INDEX ui_nnd",
    );
    let snap = eng.take_snapshot();
    let t = eng.db.find_table("ui", &snap, &[0], 0).expect("ui exists");
    assert_eq!(t.uniques.len(), 1);
    assert!(t.uniques[0].nulls_not_distinct);
    ok(&mut eng, "INSERT INTO ui VALUES (NULL, 'x')");
    let e = err(&mut eng, "INSERT INTO ui VALUES (NULL, 'y')");
    assert_eq!(e.code, "23505");

    // An NND index cannot back a PRIMARY KEY (42P16, catalog/index.c).
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk (i int NOT NULL, t text)");
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX pk_nnd ON pk (i) NULLS NOT DISTINCT",
    );
    let e = err(
        &mut eng,
        "ALTER TABLE pk ADD PRIMARY KEY USING INDEX pk_nnd",
    );
    assert_eq!(e.code, "42P16");
}

#[test]
fn v208_constraint_codec_round_trip() {
    // encode -> decode preserves the flag; a plain unique stays
    // byte-identical to the pre-v2.08 form (no attrs section).
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE cc (i int UNIQUE NULLS NOT DISTINCT, j int UNIQUE)",
    );
    let snap = eng.take_snapshot();
    let t = eng
        .db
        .find_table("cc", &snap, &[0], 0)
        .expect("cc exists")
        .clone();
    let enc = crate::sql::encode_constraints(&t);
    let dec = crate::sql::decode_constraints(&enc).expect("decodes");
    assert_eq!(dec.uniques.len(), 2);
    assert!(dec.uniques[0].nulls_not_distinct);
    assert!(!dec.uniques[1].nulls_not_distinct);
    // The plain entry must not gain an attrs section.
    assert!(
        !enc.contains("(attrs 0 0)"),
        "plain unique encoding changed: {enc}"
    );
}
