use super::*;

// v2.03: PUBLICATION COMPLETENESS — REPLICA IDENTITY syntax + the
// per-table identity record in the DML guard, publication column
// lists + row filters (storage + PG19 validity + 42P10 enforcement),
// generated columns (the prerequisite layer), and the RLS scan
// ORDER BY fix (plan_order_scan must not hand an ordering hint out
// for RLS-enabled tables).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(19000);

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

fn code(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql) {
        Ok(_) => panic!("expected error for {sql}"),
        Err(e) => e.code.to_string(),
    }
}

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql).unwrap() {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected select for {sql}: {other:?}"),
    }
}

#[test]
fn v203_replica_identity_kinds_drive_the_guard() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (a int primary key, b int)").unwrap();
    run(&mut eng, "INSERT INTO t VALUES (1, 1)").unwrap();
    run(&mut eng, "CREATE PUBLICATION p FOR TABLE t").unwrap();
    run(&mut eng, "UPDATE t SET b = 2").unwrap();
    // NOTHING: updates/deletes blocked even though a PK exists.
    run(&mut eng, "ALTER TABLE t REPLICA IDENTITY NOTHING").unwrap();
    assert_eq!(code(&mut eng, "UPDATE t SET b = 3"), "55000");
    assert_eq!(code(&mut eng, "DELETE FROM t"), "55000");
    // FULL: fine again.
    run(&mut eng, "ALTER TABLE t REPLICA IDENTITY FULL").unwrap();
    run(&mut eng, "UPDATE t SET b = 3").unwrap();
    // Back to DEFAULT (the PK).
    run(&mut eng, "ALTER TABLE t REPLICA IDENTITY DEFAULT").unwrap();
    run(&mut eng, "UPDATE t SET b = 4").unwrap();
}

#[test]
fn v203_replica_identity_using_index_validation() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (a int, b int not null)").unwrap();
    assert_eq!(
        code(&mut eng, "ALTER TABLE t REPLICA IDENTITY USING INDEX nope"),
        "42704"
    );
    run(&mut eng, "CREATE UNIQUE INDEX t_a ON t(a)").unwrap();
    // a is nullable: 42809.
    assert_eq!(
        code(&mut eng, "ALTER TABLE t REPLICA IDENTITY USING INDEX t_a"),
        "42809"
    );
    run(&mut eng, "CREATE UNIQUE INDEX t_b ON t(b)").unwrap();
    run(&mut eng, "ALTER TABLE t REPLICA IDENTITY USING INDEX t_b").unwrap();
    run(&mut eng, "CREATE TABLE u (x int)").unwrap();
    run(&mut eng, "CREATE UNIQUE INDEX u_x ON u(x)").unwrap();
    // Index belongs to another table: 42809.
    assert_eq!(
        code(&mut eng, "ALTER TABLE t REPLICA IDENTITY USING INDEX u_x"),
        "42809"
    );
}

#[test]
fn v203_row_filter_validity_and_ri_coverage() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (a int primary key, b int, c int)").unwrap();
    run(&mut eng, "INSERT INTO t VALUES (1, 1, 1)").unwrap();
    // DDL-time validity.
    run(&mut eng, "CREATE PUBLICATION p FOR TABLE t WHERE (a > 0)").unwrap();
    assert_eq!(
        code(&mut eng, "CREATE PUBLICATION q FOR TABLE t WHERE (1234)"),
        "42804"
    );
    assert_eq!(
        code(
            &mut eng,
            "CREATE PUBLICATION q FOR TABLE t WHERE (a < avg(a))"
        ),
        "42803"
    );
    assert_eq!(
        code(
            &mut eng,
            "CREATE PUBLICATION q FOR TABLE t WHERE (a < random())"
        ),
        "0A000"
    );
    assert_eq!(
        code(&mut eng, "CREATE PUBLICATION q FOR TABLE t WHERE (zz > 0)"),
        "42703"
    );
    assert_eq!(
        code(
            &mut eng,
            "CREATE PUBLICATION q FOR TABLE t WHERE (a IN (SELECT 1))"
        ),
        "0A000"
    );
    // Filter over b (not in the PK replica identity): DML blocked 42P10.
    run(&mut eng, "ALTER PUBLICATION p SET TABLE t WHERE (b > 0)").unwrap();
    let e = run(&mut eng, "UPDATE t SET a = 2").unwrap_err();
    assert_eq!(e.code, "42P10");
    assert!(
        e.message.contains("cannot update table \"t\""),
        "{}",
        e.message
    );
    // REPLICA IDENTITY FULL covers b.
    run(&mut eng, "ALTER TABLE t REPLICA IDENTITY FULL").unwrap();
    run(&mut eng, "UPDATE t SET a = 2").unwrap();
    // DROP TABLE with a WHERE clause is a syntax-level rejection.
    assert_eq!(
        code(&mut eng, "ALTER PUBLICATION p DROP TABLE t WHERE (a > 0)"),
        "0A000"
    );
}

#[test]
fn v203_column_list_validity_and_coverage() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE t (a int primary key, b text, c int GENERATED ALWAYS AS (a + 1) STORED NOT NULL, d int GENERATED ALWAYS AS (a + 2) VIRTUAL)",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO t (a, b) VALUES (1, 'x')").unwrap();
    assert_eq!(rows(&mut eng, "SELECT c, d FROM t"), vec![vec!["2", "3"]]);
    assert_eq!(
        code(&mut eng, "CREATE PUBLICATION p FOR TABLE t (a, zz)"),
        "42703"
    );
    assert_eq!(
        code(&mut eng, "CREATE PUBLICATION p FOR TABLE t (a, a)"),
        "42701"
    );
    assert_eq!(
        code(&mut eng, "CREATE PUBLICATION p FOR TABLE t (a, d)"),
        "0A000"
    );
    // List without the PK column: DML blocked (list must cover RI).
    run(&mut eng, "CREATE PUBLICATION p FOR TABLE t (b)").unwrap();
    assert_eq!(code(&mut eng, "UPDATE t SET a = 5"), "42P10");
    // Stored generated column in the RI but unpublished: blocked until
    // publish_generated_columns = stored.
    run(&mut eng, "CREATE UNIQUE INDEX t_c ON t(c)").unwrap();
    run(&mut eng, "ALTER PUBLICATION p SET TABLE t (a)").unwrap();
    run(&mut eng, "ALTER TABLE t REPLICA IDENTITY USING INDEX t_c").unwrap();
    assert_eq!(code(&mut eng, "UPDATE t SET a = 5"), "42P10");
    run(
        &mut eng,
        "ALTER PUBLICATION p SET (publish_generated_columns = stored)",
    )
    .unwrap();
    // RI is now (c); the column list (a) does not cover it.
    assert_eq!(code(&mut eng, "UPDATE t SET a = 5"), "42P10");
    run(&mut eng, "ALTER PUBLICATION p SET TABLE t (a, c)").unwrap();
    run(&mut eng, "UPDATE t SET a = 5").unwrap();
    // Generated values follow the update.
    assert_eq!(rows(&mut eng, "SELECT c FROM t"), vec![vec!["6"]]);
}

#[test]
fn v203_generated_columns_write_rules() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE g (a int, b int GENERATED ALWAYS AS (a * 10) STORED)",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO g (a) VALUES (3)").unwrap();
    assert_eq!(rows(&mut eng, "SELECT b FROM g"), vec![vec!["30"]]);
    // Explicit insert into a generated column: 428C9.
    assert_eq!(
        code(&mut eng, "INSERT INTO g (a, b) VALUES (1, 1)"),
        "428C9"
    );
    // UPDATE of a base column recomputes.
    run(&mut eng, "UPDATE g SET a = 4").unwrap();
    assert_eq!(rows(&mut eng, "SELECT b FROM g"), vec![vec!["40"]]);
}

#[test]
fn v203_rls_order_by_after_update_is_sorted() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (id int primary key, v int)").unwrap();
    run(&mut eng, "INSERT INTO t VALUES (1, 30), (2, 10), (3, 20)").unwrap();
    run(&mut eng, "ALTER TABLE t ENABLE ROW LEVEL SECURITY").unwrap();
    run(&mut eng, "CREATE POLICY p ON t USING (true)").unwrap();
    run(&mut eng, "UPDATE t SET v = v + 0").unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT v FROM t ORDER BY v"),
        vec![vec!["10"], vec!["20"], vec!["30"]]
    );
}

#[test]
fn v203_generated_expression_validity_at_create() {
    let mut eng = engine();
    // Aggregate / window / subquery / SRF / mutable / nested-generated /
    // unknown / system column references are all rejected at CREATE
    // (PG19 gtest_err family) — critically, none of them may reach
    // the WAL, where an unrepresentable expression would break
    // replay on every restart.
    for bad in [
        "CREATE TABLE b1 (a int, g int GENERATED ALWAYS AS (avg(a)) STORED)",
        "CREATE TABLE b2 (a int, g int GENERATED ALWAYS AS (row_number() OVER (ORDER BY a)) STORED)",
        "CREATE TABLE b3 (a int, g int GENERATED ALWAYS AS ((SELECT a)) STORED)",
        "CREATE TABLE b4 (a int, g int GENERATED ALWAYS AS (generate_series(1, a)) STORED)",
        "CREATE TABLE b5 (a int, g double precision GENERATED ALWAYS AS (random()) STORED)",
        "CREATE TABLE b6 (a int, g int GENERATED ALWAYS AS (zz * 2) STORED)",
        "CREATE TABLE b7 (a int, g bool GENERATED ALWAYS AS (xmin <> 37) STORED)",
        "CREATE TABLE b8 (a int, g int GENERATED ALWAYS AS (g * 2) STORED)",
        "CREATE TABLE b9 (a int, h int GENERATED ALWAYS AS (a * 2) STORED, g int GENERATED ALWAYS AS (h * 3) STORED)",
    ] {
        let r = run(&mut eng, bad);
        assert!(r.is_err(), "expected CREATE to fail: {bad}");
    }
    assert_eq!(
        code(
            &mut eng,
            "CREATE TABLE b11 (a int, g int GENERATED ALWAYS AS (avg(a)) STORED)"
        ),
        "42803"
    );
    // DEFAULT + generation on one column is rejected (at parse time).
    assert!(
        crate::sql::parse_statement(
            "CREATE TABLE b10 (a int, x int DEFAULT 1 GENERATED ALWAYS AS (a) STORED)"
        )
        .is_err()
    );
    assert!(
        crate::sql::parse_statement(
            "CREATE TABLE b12 (a int, x int GENERATED ALWAYS AS (a) STORED GENERATED ALWAYS AS (a + 1) STORED)"
        )
        .is_err()
    );
    // A valid expression still works.
    run(
        &mut eng,
        "CREATE TABLE ok1 (a int, g int GENERATED ALWAYS AS (abs(a) + 1) STORED)",
    )
    .unwrap();
    // DEFAULT placeholder into a generated column is accepted; a real
    // value is 428C9.
    run(&mut eng, "INSERT INTO ok1 VALUES (5, DEFAULT)").unwrap();
    assert_eq!(rows(&mut eng, "SELECT g FROM ok1"), vec![vec!["6"]]);
    assert_eq!(code(&mut eng, "INSERT INTO ok1 VALUES (5, 9)"), "428C9");
}

#[test]
fn v203_drop_generated_column_keeps_metadata_parallel() {
    // Regression: DROP COLUMN must remove the gen_cols slot too —
    // leaving it long made the RI guard index columns[] out of
    // bounds and killed the connection (v2.03 schedule run,
    // testpub_tbl5).
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE t (a int primary key, b text, c text GENERATED ALWAYS AS (a || b) STORED, d int)",
    )
    .unwrap();
    run(&mut eng, "CREATE PUBLICATION p FOR TABLE t").unwrap();
    run(&mut eng, "INSERT INTO t (a, b, d) VALUES (1, 'aa', 1)").unwrap();
    run(&mut eng, "ALTER TABLE t DROP COLUMN c").unwrap();
    run(&mut eng, "UPDATE t SET a = 2").unwrap();
    assert_eq!(
        rows(&mut eng, "SELECT a, b, d FROM t"),
        vec![vec!["2", "aa", "1"]]
    );
    // SET on a generated column is 428C9.
    run(
        &mut eng,
        "CREATE TABLE g (a int, b int GENERATED ALWAYS AS (a * 2) STORED)",
    )
    .unwrap();
    assert_eq!(code(&mut eng, "UPDATE g SET b = 3"), "428C9");
}

#[test]
fn v203_codec_roundtrip_replica_identity_and_gen_cols() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE t (a int primary key, b int, g int GENERATED ALWAYS AS (a + b) STORED)",
    )
    .unwrap();
    run(&mut eng, "ALTER TABLE t REPLICA IDENTITY FULL").unwrap();
    let snap = eng.take_snapshot();
    let t = eng
        .db
        .find_table("t", &snap, &[u64::MAX], 0)
        .expect("table")
        .clone();
    let enc = crate::sql::encode_constraints(&t);
    let dc = crate::sql::decode_constraints(&enc).expect("decodes");
    assert_eq!(dc.replica_identity, crate::storage::ReplicaIdentity::Full);
    assert_eq!(dc.gen_cols.len(), 3);
    assert!(dc.gen_cols[2].is_some());
    // Pre-v2.03 encodings (no trailing sections) decode as defaults:
    // truncate this encoding before the replident section.
    let cut = enc.find(" (replident").unwrap();
    let old = format!("{})", &enc[..cut]);
    let dc2 = crate::sql::decode_constraints(&old).expect("old decodes");
    assert_eq!(
        dc2.replica_identity,
        crate::storage::ReplicaIdentity::Default
    );
    assert!(dc2.gen_cols.is_empty());
}
