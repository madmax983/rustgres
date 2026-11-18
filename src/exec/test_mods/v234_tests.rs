// v2.34: pg_stat_* monitoring family — generation-aware xact
// merge rules, the DROP-flag model, temp index maintenance,
// snapshot-mode gating, regclass display for sequences, the
// records_for_commit xid-family fix (savepoint-stamped rows),
// and VACUUM BUFFER_USAGE_LIMIT parsing.
use super::*;
use crate::sql::parse_statement;
use crate::storage::PgRelStat;
use crate::wal::WalRecord;
use crate::wal::records_for_commit;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).expect("parses");
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

fn rows_text(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) | Ok(ExecResult::Explain { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
            .collect(),
        other => panic!("expected Select for {sql} -> {other:?}"),
    }
}

fn one(eng: &mut Engine, sql: &str) -> Vec<String> {
    let r = rows_text(eng, sql);
    assert_eq!(r.len(), 1, "{sql} -> {r:?}");
    r.into_iter().next().unwrap()
}

fn committed(eng: &Engine, key: &str) -> PgRelStat {
    eng.db.pgstat_rel.get(key).cloned().unwrap_or_default()
}

// --- merge rules -----------------------------------------------------------

#[test]
fn pgstat_commit_merges_xact_counts() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    record_insert(&mut eng, 0, 9, "t", 3);
    record_delete(&mut eng, 0, 9, "t", 1);
    pgstat_commit(&mut eng, 0, 9);
    let c = committed(&eng, "t");
    assert_eq!(c.n_tup_ins, 3);
    assert_eq!(c.n_tup_del, 1);
    assert_eq!(c.n_live_tup, 2);
    assert_eq!(c.n_dead_tup, 1);
    assert!(eng.pgstat_xact.get(&(0, 9)).is_none());
}

#[test]
fn pgstat_abort_counts_inserts_as_dead() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    record_insert(&mut eng, 0, 9, "t", 3);
    pgstat_abort(&mut eng, 0, 9);
    let c = committed(&eng, "t");
    // Whole-xact abort (PG19): the first generation's inserts
    // count, and its live inserts become dead tuples.
    assert_eq!(c.n_tup_ins, 3);
    assert_eq!(c.n_live_tup, 0);
    assert_eq!(c.n_dead_tup, 3);
}

#[test]
fn pgstat_rollback_to_savepoint_with_truncate_generations() {
    // stats.sql trunc_stats_test3 shape: 2 inserts, SAVEPOINT,
    // 2 inserts, TRUNCATE, 1 insert, ROLLBACK TO SAVEPOINT,
    // COMMIT -> ins 4, live 2, dead 2.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    record_insert(&mut eng, 0, 9, "t", 2);
    let mark = pgstat_mark(&eng, 0, 9);
    record_insert(&mut eng, 0, 9, "t", 2);
    record_truncate(&mut eng, 0, 9, "t");
    record_insert(&mut eng, 0, 9, "t", 1);
    pgstat_rollback_to(&mut eng, 0, 9, &mark);
    pgstat_commit(&mut eng, 0, 9);
    let c = committed(&eng, "t");
    assert_eq!(c.n_tup_ins, 4, "ins: {c:?}");
    assert_eq!(c.n_live_tup, 2, "live: {c:?}");
    assert_eq!(c.n_dead_tup, 2, "dead: {c:?}");
}

#[test]
fn pgstat_drop_flag_hides_xact_accessors_but_abort_merges() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    record_insert(&mut eng, 0, 9, "t", 1);
    note_drop_rel(&mut eng, 0, 9, "t");
    // In-transaction, the xact accessor answers 0 for the
    // dropped relation (PG19).
    let oid = one(&mut eng, "SELECT 't'::regclass::oid")[0].clone();
    let v = one(
        &mut eng,
        &format!("SELECT pg_stat_get_xact_tuples_inserted({oid})"),
    );
    assert_eq!(v[0], "0");
    // Rolling the drop back merges the entry like any abort:
    // the insert counts (and becomes a dead tuple).
    pgstat_abort(&mut eng, 0, 9);
    let c = committed(&eng, "t");
    assert_eq!(c.n_tup_ins, 1);
    assert_eq!(c.n_dead_tup, 1);
    // The commit-time drop mark was rolled back too: a later
    // commit must NOT delete the committed entry.
    assert!(eng.db.pgstat_rel.contains_key("t"));
}

#[test]
fn pgstat_stats_reset_null_until_first_reset() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    record_insert(&mut eng, 0, 0, "t", 1);
    // PG19: stats_reset is the last RESET time — NULL from
    // entry creation until a reset stamps it.
    assert!(committed(&eng, "t").stats_reset.is_none());
    ok(
        &mut eng,
        "SELECT pg_stat_reset_single_table_counters('t'::regclass)",
    );
    assert!(committed(&eng, "t").stats_reset.is_some());
    assert_eq!(committed(&eng, "t").n_tup_ins, 0);
}

// --- snapshot gating --------------------------------------------------------

#[test]
fn snapshot_timestamp_stamps_only_in_snapshot_mode() {
    let mut eng = engine();
    // Default stats_fetch_consistency is cache: accesses fill
    // the per-entry cache but never build a snapshot.
    touch_snapshot(&mut eng, 0, 9);
    assert!(eng.pgstat_snapshot.get(&(0, 9)).is_none());
}

// --- temp indexes ------------------------------------------------------------

#[test]
fn temp_constraint_index_tree_is_maintained() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TEMPORARY TABLE tt (id int primary key, v text)",
    );
    ok(&mut eng, "INSERT INTO tt VALUES (1, 'a'), (2, 'b')");
    let snap = eng.take_snapshot();
    let ixs = eng.db.visible_indexes_for("tt", &snap, &[9], 0);
    assert_eq!(ixs.len(), 1, "temp PK index must exist");
    let ids = crate::exec::indexes::index_scan_ids(ixs[0], &[Value::Int(2)], None, None);
    assert_eq!(ids.len(), 1, "inserted row must be in the temp index tree");
    // And the pg_stat indexes view lists it exactly once.
    let r = rows_text(
        &mut eng,
        "SELECT indexrelname FROM pg_stat_user_indexes WHERE relname = 'tt'",
    );
    assert_eq!(r.len(), 1, "index listed once: {r:?}");
}

// --- regclass display ---------------------------------------------------------

#[test]
fn regclass_display_names_sequences() {
    let mut eng = engine();
    ok(&mut eng, "CREATE SEQUENCE sq1");
    // The 60000-band synthetic OID renders back as the name
    // (and resolves in pg_stat accessors by that name).
    assert_eq!(one(&mut eng, "SELECT 'sq1'::regclass")[0], "sq1");
    ok(
        &mut eng,
        "SELECT pg_stat_reset_single_table_counters('sq1'::regclass)",
    );
    let r = one(
        &mut eng,
        "SELECT stats_reset IS NOT NULL FROM pg_statio_all_sequences WHERE relname = 'sq1'",
    );
    assert_eq!(r[0], "t");
}

// --- records_for_commit xid family (v2.34 durability fix) ---------------------

fn engine_with_committed_table() -> Engine {
    let mut eng = Engine::new();
    eng.db.tables.insert(
        "t".into(),
        vec![{
            let mut t = Table::new(vec![("a".into(), ColType::Int)], 1);
            t.push_version(RowVersion::plain(1, Row::new(vec![Value::Int(10)]), 1));
            t
        }],
    );
    eng.txns.next_xid = 10;
    eng.txns.next_row_id = 2;
    eng
}

#[test]
fn records_for_commit_includes_savepoint_subxid_inserts() {
    // Rows inserted under a SAVEPOINT carry the sub-xid as xmin.
    // records_for_commit must treat the whole xid family as own:
    // before the fix the row was silently skipped (lost on
    // restart) because own_row compared xmin to the top xid.
    let mut eng = engine_with_committed_table();
    let top = 10u64;
    let sub = 11u64;
    eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
        id: 2,
        values: Row::new(vec![Value::Int(20)]),
        xmin: sub,
        xmax: 0,
        toast: Vec::new(),
    });
    let writes = vec![WriteOp::InsertRow {
        table: "t".into(),
        row_id: 2,
    }];
    let recs = records_for_commit(&eng, top, &writes, 0, &[top, sub]).unwrap();
    let n_rows: usize = recs
        .iter()
        .map(|r| match r {
            WalRecord::InsertRows { rows, .. } => rows.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(
        n_rows, 1,
        "the savepoint insert must reach the WAL: {recs:?}"
    );
}

#[test]
fn records_for_commit_accepts_savepoint_delete_then_insert() {
    // The trunc_stats_test2 shape: a row deleted under a
    // savepoint (xmax = sub-xid) followed by another insert in
    // the same commit used to fail with a spurious 40001
    // "concurrent update" — the delete was misread as another
    // transaction's.
    let mut eng = engine_with_committed_table();
    let top = 10u64;
    let sub = 11u64;
    // Row 1 deleted under the sub-xid.
    eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = sub;
    // Row 2 inserted under the sub-xid.
    eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
        id: 2,
        values: Row::new(vec![Value::Int(20)]),
        xmin: sub,
        xmax: 0,
        toast: Vec::new(),
    });
    let writes = vec![
        WriteOp::DeleteRow {
            table: "t".into(),
            row_id: 1,
            prev_xmax: 0,
        },
        WriteOp::InsertRow {
            table: "t".into(),
            row_id: 2,
        },
    ];
    let recs = records_for_commit(&eng, top, &writes, 0, &[top, sub]);
    assert!(recs.is_ok(), "family commit must not 40001: {recs:?}");
}

// --- VACUUM BUFFER_USAGE_LIMIT parsing ----------------------------------------

#[test]
fn vacuum_buffer_usage_limit_parsing() {
    let stmt = parse_statement("VACUUM (BUFFER_USAGE_LIMIT 120) t").expect("parses");
    match stmt {
        Stmt::Vacuum {
            buffer_usage_limit_kb,
            buffer_usage_overflow,
            buffer_usage_full,
            ..
        } => {
            assert_eq!(buffer_usage_limit_kb, Some(120));
            assert!(!buffer_usage_overflow);
            assert!(!buffer_usage_full);
        }
        s => panic!("expected Vacuum, got {s:?}"),
    }
    let stmt = parse_statement("VACUUM (BUFFER_USAGE_LIMIT 10000000000) t").expect("parses");
    match stmt {
        Stmt::Vacuum {
            buffer_usage_overflow,
            ..
        } => assert!(buffer_usage_overflow),
        s => panic!("expected Vacuum, got {s:?}"),
    }
    let stmt = parse_statement("VACUUM (BUFFER_USAGE_LIMIT '512 kB', FULL) t").expect("parses");
    match stmt {
        Stmt::Vacuum {
            buffer_usage_limit_kb,
            buffer_usage_full,
            ..
        } => {
            assert_eq!(buffer_usage_limit_kb, Some(512));
            assert!(buffer_usage_full);
        }
        s => panic!("expected Vacuum, got {s:?}"),
    }
    // ANALYZE accepts a parenthesized option group too.
    assert!(parse_statement("ANALYZE (BUFFER_USAGE_LIMIT '512 kB') t").is_ok());
}
