use super::*;

// v2.04: PUBLICATION CATALOG VIEWS + DURABILITY — pg_publication /
// pg_publication_rel / pg_publication_tables as catalog views over
// the stored publication catalog, pg_get_publication_tables with
// PG19 partition semantics, WAL + checkpoint durability for
// publications, and the supporting function-surface work (array
// args, RETURNS TABLE, BEGIN ATOMIC bodies, COMMENT ON
// PUBLICATION). Plus generated columns on the MERGE apply path.

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(29000);

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
    let r = execute(eng, &mut ctx, &stmt);
    if r.is_ok() {
        // Keep the write ops available to the durability test via a
        // thread-local-free path: stash them on the engine is not
        // possible, so tests that need them use run_collect below.
        let _ = writes;
    }
    r
}

/// Like run(), but returns the statement's write ops as well.
fn run_collect(eng: &mut Engine, sql: &str) -> (Result<ExecResult, ExecError>, Vec<WriteOp>) {
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
    let r = execute(eng, &mut ctx, &stmt);
    (r, writes)
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

fn colnames(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap() {
        ExecResult::Select { columns, .. } => columns.into_iter().map(|(n, _)| n).collect(),
        other => panic!("expected select for {sql}: {other:?}"),
    }
}

#[test]
fn v204_pg_publication_view_columns() {
    let mut eng = engine();
    run(&mut eng, "CREATE PUBLICATION p_all FOR ALL TABLES").unwrap();
    run(
        &mut eng,
        "CREATE PUBLICATION p_ins FOR ALL TABLES WITH (publish = 'insert')",
    )
    .unwrap();
    let r = rows(
        &mut eng,
        "SELECT pubname, puballtables, pubinsert, pubupdate, pubdelete, pubtruncate, \
         pubviaroot, pubgencols FROM pg_publication ORDER BY pubname",
    );
    assert_eq!(
        r,
        vec![
            vec!["p_all", "t", "t", "t", "t", "t", "f", "n"],
            vec!["p_ins", "t", "t", "f", "f", "f", "f", "n"],
        ]
    );
}

#[test]
fn v204_publication_tables_cols_and_filter() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int, b int, c int)").unwrap();
    run(
        &mut eng,
        "CREATE PUBLICATION p FOR TABLE t1 (a, c) WHERE (a > 5)",
    )
    .unwrap();
    let r = rows(
        &mut eng,
        "SELECT pubname, schemaname, tablename, attnames, rowfilter FROM pg_publication_tables",
    );
    assert_eq!(r, vec![vec!["p", "public", "t1", "{a,c}", "(a > 5)"]]);
    // pg_publication_rel: prattrs is the attnum list, prqual the
    // deparsed filter text.
    let r = rows(
        &mut eng,
        "SELECT prattrs, prqual, prexcept FROM pg_publication_rel",
    );
    assert_eq!(r, vec![vec!["1 3", "(a > 5)", "f"]]);
}

#[test]
fn v204_publication_tables_except_and_schema() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE keep_t (a int)").unwrap();
    run(&mut eng, "CREATE TABLE drop_t (a int)").unwrap();
    run(
        &mut eng,
        "CREATE PUBLICATION px FOR ALL TABLES EXCEPT (TABLE drop_t)",
    )
    .unwrap();
    let r = rows(
        &mut eng,
        "SELECT tablename FROM pg_publication_tables WHERE pubname = 'px'",
    );
    assert_eq!(r, vec![vec!["keep_t"]]);
    let r = rows(
        &mut eng,
        "SELECT prexcept FROM pg_publication_rel WHERE prpubid = \
         (SELECT oid FROM pg_publication WHERE pubname = 'px')",
    );
    assert_eq!(r, vec![vec!["t"]]);
}

#[test]
fn v204_gpt_partition_semantics() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE parent (a int, b int) PARTITION BY RANGE (a)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE part1 PARTITION OF parent FOR VALUES FROM (1) TO (10)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE PUBLICATION pv FOR TABLE parent WITH (publish_via_partition_root = true)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE PUBLICATION pnv FOR TABLE parent WITH (publish_via_partition_root = false)",
    )
    .unwrap();
    // via root: the root answers for the partition; the partition
    // itself yields no row.
    let r = rows(
        &mut eng,
        "SELECT pubid, relid, attrs, qual FROM pg_get_publication_tables(ARRAY['pv'], \
         (SELECT oid FROM pg_class WHERE relname = 'part1'))",
    );
    assert!(r.is_empty(), "viaroot partition query must be empty: {r:?}");
    let r = rows(
        &mut eng,
        "SELECT relid FROM pg_get_publication_tables(ARRAY['pv'], \
         (SELECT oid FROM pg_class WHERE relname = 'parent'))",
    );
    assert_eq!(r.len(), 1);
    // no viaroot: the partition answers with the parent's spec; the
    // parent itself yields no row.
    let r = rows(
        &mut eng,
        "SELECT relid FROM pg_get_publication_tables(ARRAY['pnv'], \
         (SELECT oid FROM pg_class WHERE relname = 'parent'))",
    );
    assert!(r.is_empty(), "no-viaroot parent query must be empty: {r:?}");
    let r = rows(
        &mut eng,
        "SELECT relid FROM pg_get_publication_tables(ARRAY['pnv'], \
         (SELECT oid FROM pg_class WHERE relname = 'part1'))",
    );
    assert_eq!(r.len(), 1);
    // Unknown publication contributes no rows and no error.
    let r = rows(
        &mut eng,
        "SELECT relid FROM pg_get_publication_tables(ARRAY['nope'], \
         (SELECT oid FROM pg_class WHERE relname = 'part1'))",
    );
    assert!(r.is_empty());
}

#[test]
fn v204_comment_and_describe_object() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    run(&mut eng, "CREATE PUBLICATION pd FOR TABLE t1").unwrap();
    run(&mut eng, "COMMENT ON PUBLICATION pd IS 'note one'").unwrap();
    let r = rows(
        &mut eng,
        "SELECT obj_description(oid, 'pg_publication') FROM pg_publication WHERE pubname = 'pd'",
    );
    assert_eq!(r, vec![vec!["note one"]]);
    run(&mut eng, "COMMENT ON PUBLICATION pd IS NULL").unwrap();
    let r = rows(
        &mut eng,
        "SELECT obj_description(oid, 'pg_publication') FROM pg_publication WHERE pubname = 'pd'",
    );
    assert_eq!(r, vec![vec!["NULL"]]);
    let r = rows(
        &mut eng,
        "SELECT pg_describe_object('pg_publication_rel'::regclass, pr.oid, 0) \
         FROM pg_publication_rel pr",
    );
    assert_eq!(r, vec![vec!["publication of table t1 in publication pd"]]);
    // COMMENT ON a missing publication is 42P01.
    let e = run(&mut eng, "COMMENT ON PUBLICATION nope IS 'x'").unwrap_err();
    assert_eq!(e.code, "42P01");
}

#[test]
fn v204_test_gpt_function_shape() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE tbl_normal (id int)").unwrap();
    run(
        &mut eng,
        "CREATE PUBLICATION pub_normal FOR TABLE tbl_normal WHERE (id < 10)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION test_gpt(pubnames text[], relname text)\n\
         RETURNS TABLE (pubname text, relname name, attrs text, qual text)\n\
         BEGIN ATOMIC\n\
         SELECT p.pubname, c.relname, gpt.attrs::text, pg_get_expr(gpt.qual, gpt.relid)\n\
         FROM pg_get_publication_tables(pubnames, relname::regclass::oid) gpt\n\
         JOIN pg_publication p ON p.oid = gpt.pubid\n\
         JOIN pg_class c ON c.oid = gpt.relid\n\
         ORDER BY p.pubname, c.relname;\n\
         END",
    )
    .unwrap();
    // Output columns come from the RETURNS TABLE declaration.
    let names = colnames(
        &mut eng,
        "SELECT * FROM test_gpt(ARRAY['pub_normal'], 'tbl_normal')",
    );
    assert_eq!(names, vec!["pubname", "relname", "attrs", "qual"]);
    let r = rows(
        &mut eng,
        "SELECT * FROM test_gpt(ARRAY['pub_normal'], 'tbl_normal')",
    );
    assert_eq!(r, vec![vec!["pub_normal", "tbl_normal", "1", "(id < 10)"]]);
    run(&mut eng, "DROP FUNCTION test_gpt(text[], text)").unwrap();
}

#[test]
fn v204_split_statements_begin_atomic() {
    let sql = "CREATE FUNCTION f() RETURNS int BEGIN ATOMIC SELECT 1; SELECT 2; END; SELECT 3";
    let parts = crate::sql::split_statements(sql);
    assert_eq!(parts.len(), 2, "split kept BEGIN ATOMIC whole: {parts:?}");
    assert!(parts[0].contains("END"));
    assert_eq!(parts[1].trim(), "SELECT 3");
}

#[test]
fn v204_publications_wal_round_trip() {
    let mut eng = engine();
    let mut records = Vec::new();
    for sql in [
        "CREATE TABLE t1 (a int, b int)",
        "CREATE PUBLICATION pw FOR TABLE t1 (a) WHERE (a > 0)",
        "CREATE PUBLICATION pd FOR ALL TABLES",
        "ALTER PUBLICATION pw RENAME TO pw2",
        "DROP PUBLICATION pd",
        "COMMENT ON PUBLICATION pw2 IS 'durable note'",
    ] {
        let (r, writes) = run_collect(&mut eng, sql);
        r.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let recs = crate::wal::records_for_commit(&eng, 42, &writes, 0).expect("records");
        records.extend(recs);
    }
    // Encode/decode every record at the current WAL version, then
    // replay into a fresh engine.
    let mut eng2 = engine();
    for rec in &records {
        let mut enc = crate::wal::Enc::new();
        enc.record(rec);
        let bytes = enc.buf;
        let mut dec = crate::wal::Dec::with_wal_version(&bytes, crate::wal::WAL_VERSION);
        let back = dec.record().expect("decode");
        crate::wal::apply_record(&mut eng2, &back).expect("apply");
    }
    assert_eq!(eng2.db.publications.len(), 1);
    let p = eng2
        .db
        .publications
        .get("pw2")
        .expect("renamed pub replayed");
    assert_eq!(p.comment.as_deref(), Some("durable note"));
    assert_eq!(p.tables.len(), 1);
    // The replayed catalog answers the views, and post-restart
    // ALTER works (the v2.03 failure mode was 42P01 here).
    let r = rows(
        &mut eng2,
        "SELECT pubname FROM pg_publication ORDER BY pubname",
    );
    assert_eq!(r, vec![vec!["pw2"]]);
    run(&mut eng2, "ALTER PUBLICATION pw2 SET (publish = 'insert')").unwrap();
}

#[test]
fn v204_publications_checkpoint_round_trip() {
    let dir = std::env::temp_dir().join("rg204-pub-checkpoint");
    let _ = std::fs::remove_dir_all(&dir);
    let (mut eng, mut wal) = crate::wal::Wal::open(&dir).expect("open data dir");
    eng.txns.next_xid = 1_000_000;
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    run(&mut eng, "CREATE PUBLICATION pc FOR TABLE t1 WHERE (a > 3)").unwrap();
    wal.checkpoint(&eng).expect("checkpoint");
    let (mut eng2, _wal2) = crate::wal::Wal::open(&dir).expect("reopen data dir");
    let r = rows(&mut eng2, "SELECT pubname FROM pg_publication");
    assert_eq!(r, vec![vec!["pc"]]);
    let r = rows(
        &mut eng2,
        "SELECT tablename, rowfilter FROM pg_publication_tables",
    );
    assert_eq!(r, vec![vec!["t1", "(a > 3)"]]);
    run(&mut eng2, "ALTER PUBLICATION pc ADD TABLE t1").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v204_merge_generated_columns() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE tgt (id int PRIMARY KEY, a int, b int GENERATED ALWAYS AS (a * 2) STORED)",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO tgt (id, a) VALUES (1, 10)").unwrap();
    run(&mut eng, "CREATE TABLE src (id int, a int)").unwrap();
    run(&mut eng, "INSERT INTO src VALUES (1, 20), (2, 30)").unwrap();
    run(
        &mut eng,
        "MERGE INTO tgt t USING src s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET a = s.a \
         WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
    )
    .unwrap();
    let r = rows(&mut eng, "SELECT id, a, b FROM tgt ORDER BY id");
    assert_eq!(
        r,
        vec![vec!["1", "20", "40"], vec!["2", "30", "60"]],
        "MERGE update recomputes and MERGE insert computes generated columns"
    );
}

#[test]
fn v204_aggregate_polymorphic_transition_type() {
    // PG19 pg_aggregate.c: a polymorphic STYPE needs a polymorphic
    // aggregate argument to deduce the transition type from.
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION stfnp(int[]) RETURNS int[] AS 'select $1' LANGUAGE SQL",
    )
    .unwrap();
    let e = run(
        &mut eng,
        "CREATE AGGREGATE agg_bad(*) (SFUNC = stfnp, STYPE = anyarray, INITCOND = '{}')",
    )
    .unwrap_err();
    assert_eq!(e.code, "42P13");
    assert!(e.message.contains("cannot determine transition data type"));
    // With a polymorphic argument the type is deducible: no 42P13.
    run(
        &mut eng,
        "CREATE AGGREGATE agg_ok(anyarray) (SFUNC = stfnp, STYPE = anyarray, INITCOND = '{}')",
    )
    .unwrap();
    // Concrete STYPE is unaffected.
    run(
        &mut eng,
        "CREATE FUNCTION stfn2(int, int) RETURNS int AS 'select $1' LANGUAGE SQL",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE AGGREGATE agg_conc(int) (SFUNC = stfn2, STYPE = int, INITCOND = '0')",
    )
    .unwrap();
}
