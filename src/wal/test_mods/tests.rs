use super::*;
use crate::storage::WriteOp;

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
fn crc32_known_answers() {
    // Standard check value; guards the table-driven rewrite.
    assert_eq!(crc32(b""), 0x0000_0000);
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b"rustgres"), 0x94E1_4388);
}

#[test]
fn crc32_detects_single_bit_flip() {
    let a = crc32(b"hello world");
    let mut b = b"hello world".to_vec();
    b[5] ^= 1;
    assert_ne!(a, crc32(&b));
}

/// v0.70: partition key expressions must survive the checkpoint
/// round-trip: the encoder writes `format!("{:?}", expr)` and
/// `parse_partition_expr` must read it back exactly.
#[test]
fn partition_expr_debug_roundtrip() {
    use crate::sql::Expr;
    let cases: Vec<Expr> = vec![
        Expr::Column {
            table: None,
            name: "a".to_string(),
        },
        Expr::Column {
            table: Some("t".to_string()),
            name: "b".to_string(),
        },
        Expr::Literal(Literal::Int(42)),
        Expr::Literal(Literal::BigInt(-9_000_000_000)),
        Expr::Literal(Literal::Text("o'brien \"x\"".to_string().into())),
        Expr::Literal(Literal::Bool(true)),
        Expr::Literal(Literal::Null),
        Expr::Literal(Literal::Float(1.5)),
        Expr::Arith {
            op: ArithOp::Add,
            left: Box::new(Expr::Column {
                table: None,
                name: "b".to_string(),
            }),
            right: Box::new(Expr::Literal(Literal::Int(0))),
        },
        Expr::Func {
            name: "lower".to_string(),
            args: vec![Expr::Column {
                table: None,
                name: "a".to_string(),
            }],
        },
        Expr::Func {
            name: "abs".to_string(),
            args: vec![Expr::Arith {
                op: ArithOp::Mul,
                left: Box::new(Expr::Column {
                    table: None,
                    name: "b".to_string(),
                }),
                right: Box::new(Expr::Literal(Literal::Int(-1))),
            }],
        },
    ];
    for e in &cases {
        let s = format!("{:?}", e);
        let back = parse_partition_expr(&s).unwrap_or_else(|| panic!("failed to parse {:?}", s));
        assert_eq!(format!("{:?}", back), s, "round-trip mismatch");
    }
    // Malformed input degrades to None, never panics.
    assert!(parse_partition_expr("").is_none());
    assert!(parse_partition_expr("Garbage { foo").is_none());
    assert!(parse_partition_expr("Column { table: None, name: \"a\" } trailing").is_none());
}

fn roundtrip(r: &WalRecord) -> WalRecord {
    let mut e = Enc::new();
    e.record(r);
    let mut d = Dec::new(&e.buf);
    let out = d.record().unwrap();
    d.end().unwrap();
    out
}

#[test]
fn record_roundtrip_all_kinds() {
    let empty_constraints =
        "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))".to_string();
    let cases = vec![
        WalRecord::CreateTable {
            name: "t".into(),
            columns: vec![("a".into(), ColType::Int)],
            constraints: empty_constraints.clone(),
            owner: "postgres".into(),
            acl: vec![],
            col_acl: vec![],
            oid: 16384,
            toast_relid: 16385,
            col_compression: vec![0],
            composite_types: vec![None],
            domain_types: vec![None],
            domain_elem: vec![false],
            inherits: vec![],
            // v1.41
            attnums: vec![1],
            next_attnum: 2,
            fillfactor: 100,
            partition: Some(v180_sample_partition()),
            xmin: 3,
        },
        WalRecord::InsertRows {
            table: "t".into(),
            rows: vec![
                WalRow {
                    id: 7,
                    xmin: 4,
                    values: Row::new(vec![Value::Int(1), Value::Null]),
                    toast: vec![0; 2],
                    toast_meta: vec![],
                },
                WalRow {
                    id: 8,
                    xmin: 4,
                    values: Row::new(vec![Value::Int(2), Value::text("x")]),
                    toast: vec![0; 2],
                    toast_meta: vec![],
                },
            ],
        },
        WalRecord::DropTable {
            name: "t".into(),
            xmax: 5,
        },
        WalRecord::DeleteRows {
            table: "t".into(),
            ids: vec![7, 8, 9],
            old_rows: vec![
                WalRow {
                    id: 7,
                    xmin: 3,
                    values: Row::new(vec![Value::Int(1)]),
                    toast: vec![0; 1],
                    toast_meta: vec![],
                },
                WalRow {
                    id: 8,
                    xmin: 4,
                    values: Row::new(vec![Value::Int(2)]),
                    toast: vec![0; 1],
                    toast_meta: vec![],
                },
                WalRow {
                    id: 9,
                    xmin: 4,
                    values: Row::new(vec![Value::Int(3)]),
                    toast: vec![0; 1],
                    toast_meta: vec![],
                },
            ],
            xmax: 6,
        },
        // v0.13: first-class UPDATE with old+new tuples.
        WalRecord::UpdateRows {
            table: "t".into(),
            old: vec![WalRow {
                id: 7,
                xmin: 4,
                values: Row::new(vec![Value::Int(1), Value::text("a")]),
                toast: vec![0; 2],
                toast_meta: vec![],
            }],
            new: vec![WalRow {
                id: 10,
                xmin: 6,
                values: Row::new(vec![Value::Int(1), Value::text("b")]),
                toast: vec![0; 2],
                toast_meta: vec![],
            }],
            xmax: 6,
        },
        // v0.13: replication slot metadata.
        WalRecord::ReplSlotCreate {
            name: "s1".into(),
            plugin: "rustgres_decoding".into(),
            slot_type: "logical".into(),
            restart_lsn: 0x1_0000_0020,
        },
        WalRecord::ReplSlotFlush {
            name: "s1".into(),
            restart_lsn: 0x1_0000_0020,
            confirmed_flush_lsn: 0x1_0000_0100,
        },
        WalRecord::ReplSlotDrop { name: "s1".into() },
        // v1.05: lazy reltoastrelid link.
        WalRecord::SetToastRelid {
            name: "t".into(),
            toast_relid: 16385,
            xmin: 42,
        },
    ];
    for c in &cases {
        assert_eq!(&roundtrip(c), c);
    }
}

#[test]
fn v96_create_table_inherits_roundtrip() {
    // v0.96: the `inherits` parent links survive WAL encode/decode
    // on both CreateTable and AlterTable records.
    let empty_constraints =
        "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))".to_string();
    let create = WalRecord::CreateTable {
        name: "c".into(),
        columns: vec![("a".into(), ColType::Int)],
        constraints: empty_constraints.clone(),
        owner: "postgres".into(),
        acl: vec![],
        col_acl: vec![],
        oid: 16384,
        toast_relid: 0,
        col_compression: vec![0],
        composite_types: vec![None],
        domain_types: vec![None],
        domain_elem: vec![false],
        inherits: vec!["p1".to_string(), "p2".to_string()],
        // v1.41
        attnums: vec![1],
        next_attnum: 2,
        fillfactor: 100,
        partition: None,
        xmin: 3,
    };
    assert_eq!(&roundtrip(&create), &create);
    let alter = WalRecord::AlterTable {
        name: "c".into(),
        columns: vec![("a".into(), ColType::Int)],
        constraints: empty_constraints,
        copy_rows: true,
        owner: "postgres".into(),
        acl: vec![],
        col_acl: vec![],
        oid: 16384,
        toast_relid: 0,
        toast_target: 0,
        col_storage: vec![0],
        col_compression: vec![0],
        composite_types: vec![None],
        domain_types: vec![None],
        domain_elem: vec![false],
        inherits: vec!["p1".to_string()],
        // v1.41
        attnums: vec![1],
        next_attnum: 2,
        fillfactor: 100,
        partition: None,
        next_value_id: 1,
        toast_info: vec![],
        xmin: 4,
    };
    assert_eq!(&roundtrip(&alter), &alter);
}

#[test]
fn records_for_commit_groups_inserts() {
    let mut eng = engine_with_committed_table();
    let xid = eng.begin_txn();
    // Stage two inserts into t.
    for (id, v) in [(2u64, 20i64), (3, 30)] {
        eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
            id,
            values: Row::new(vec![Value::Int(v)]),
            xmin: xid,
            xmax: 0,
            toast: Vec::new(),
        });
    }
    let writes = vec![
        WriteOp::InsertRow {
            table: "t".into(),
            row_id: 2,
        },
        WriteOp::InsertRow {
            table: "t".into(),
            row_id: 3,
        },
    ];
    let recs = records_for_commit(&eng, xid, &writes, 0).unwrap();
    assert_eq!(recs.len(), 1);
    match &recs[0] {
        WalRecord::InsertRows { table, rows } => {
            assert_eq!(table, "t");
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].values, Row::new(vec![Value::Int(20)]));
        }
        r => panic!("unexpected record: {:?}", r),
    }
}

#[test]
fn records_for_commit_rejects_lost_create_race() {
    let mut eng = engine_with_committed_table();
    // Rival transaction created "t" and committed first.
    eng.db.tables.get_mut("t").unwrap()[0].created_xmin = 5;
    let xid = eng.begin_txn(); // 11
    // Our own uncommitted create of the same name.
    eng.db
        .tables
        .get_mut("t")
        .unwrap()
        .push(Table::new(vec![("a".into(), ColType::Int)], xid));
    let writes = vec![WriteOp::CreateTable { name: "t".into() }];
    let err = records_for_commit(&eng, xid, &writes, 0).unwrap_err();
    assert!(err.contains("already exists"), "got: {}", err);
}

#[test]
fn records_for_commit_skips_overwritten_delete() {
    let mut eng = engine_with_committed_table();
    let xid = eng.begin_txn();
    // We delete row 1...
    eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = xid;
    // ...but a concurrent txn overwrites our delete (last-writer-wins).
    let other = eng.begin_txn();
    eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = other;
    let writes = vec![WriteOp::DeleteRow {
        table: "t".into(),
        row_id: 1,
        prev_xmax: 0,
    }];
    let recs = records_for_commit(&eng, xid, &writes, 0).unwrap();
    assert!(recs.is_empty());
}

#[test]
fn records_for_commit_fails_lost_update() {
    // UPDATE/UPDATE race: our delete-half was overwritten, but our
    // insert-half survives -> committing would duplicate the row.
    let mut eng = engine_with_committed_table();
    let xid = eng.begin_txn();
    // Our UPDATE of row 1: delete-half + insert-half (adjacent pair).
    eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = xid;
    eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
        id: 2,
        values: Row::new(vec![Value::Int(99)]),
        xmin: xid,
        xmax: 0,
        toast: Vec::new(),
    });
    // Concurrent txn overwrites our delete-half.
    let other = eng.begin_txn();
    eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = other;
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
    let err = records_for_commit(&eng, xid, &writes, 0).unwrap_err();
    assert!(err.contains("concurrent update"), "got: {}", err);
}

#[test]
fn apply_record_rebuilds_versions_and_counters() {
    let mut eng = Engine::new();
    let empty_constraints =
        "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))".to_string();
    apply_record(
        &mut eng,
        &WalRecord::CreateTable {
            name: "t".into(),
            columns: vec![("a".into(), ColType::Int)],
            constraints: empty_constraints,
            owner: "postgres".into(),
            acl: vec![],
            col_acl: vec![],
            oid: 16384,
            toast_relid: 0,
            col_compression: vec![0],
            composite_types: vec![None],
            domain_types: vec![None],
            domain_elem: vec![false],
            inherits: vec![],
            // v1.41
            attnums: vec![1],
            next_attnum: 2,
            fillfactor: 100,
            partition: None,
            xmin: 4,
        },
    )
    .unwrap();
    apply_record(
        &mut eng,
        &WalRecord::InsertRows {
            table: "t".into(),
            rows: vec![WalRow {
                id: 12,
                xmin: 5,
                values: Row::new(vec![Value::Int(1)]),
                toast: vec![0; 1],
                toast_meta: vec![],
            }],
        },
    )
    .unwrap();
    apply_record(
        &mut eng,
        &WalRecord::DeleteRows {
            table: "t".into(),
            ids: vec![12],
            old_rows: vec![WalRow {
                id: 12,
                xmin: 5,
                values: Row::new(vec![Value::Int(1)]),
                toast: vec![0; 1],
                toast_meta: vec![],
            }],
            xmax: 6,
        },
    )
    .unwrap();
    let t = &eng.db.tables["t"][0];
    assert_eq!(t.rows.len(), 1);
    assert_eq!(t.rows[0].xmin, 5);
    assert_eq!(t.rows[0].xmax, 6);
    // Counters resume above every id the log used.
    assert_eq!(eng.txns.next_xid, 7);
    assert_eq!(eng.txns.next_row_id, 13);
}

#[test]
fn apply_record_tolerates_missing_targets() {
    let mut eng = Engine::new();
    // No such table / row: warnings, not errors.
    apply_record(
        &mut eng,
        &WalRecord::DeleteRows {
            table: "nope".into(),
            ids: vec![1],
            old_rows: vec![WalRow {
                id: 1,
                xmin: 2,
                values: Row::new(vec![Value::Int(1)]),
                toast: vec![0; 1],
                toast_meta: vec![],
            }],
            xmax: 9,
        },
    )
    .unwrap();
    apply_record(
        &mut eng,
        &WalRecord::DropTable {
            name: "nope".into(),
            xmax: 9,
        },
    )
    .unwrap();
    assert_eq!(eng.txns.next_xid, 10);
}

#[test]
fn v105_set_toast_relid_apply_and_emit() {
    // v1.05: the lazy reltoastrelid link replays onto the live table
    // version (and folds its xid), and records_for_commit emits it
    // from the write op — but skips a stale op whose link moved on.
    let mut eng = Engine::new();
    eng.db
        .tables
        .entry("t".into())
        .or_default()
        .push(Table::new(vec![("a".into(), ColType::Int)], 4));
    apply_record(
        &mut eng,
        &WalRecord::SetToastRelid {
            name: "t".into(),
            toast_relid: 16385,
            xmin: 7,
        },
    )
    .unwrap();
    let t = eng.db.tables["t"]
        .iter()
        .find(|t| t.dropped_xmax == 0)
        .unwrap();
    assert_eq!(t.toast_relid, 16385);
    assert!(eng.txns.next_xid >= 8);
    // Missing table: warning, not an error.
    apply_record(
        &mut eng,
        &WalRecord::SetToastRelid {
            name: "nope".into(),
            toast_relid: 1,
            xmin: 7,
        },
    )
    .unwrap();

    let writes = vec![WriteOp::SetToastRelid {
        table: "t".into(),
        toast_relid: 16385,
        prev: 0,
    }];
    let recs = records_for_commit(&eng, 9, &writes, 0).unwrap();
    assert_eq!(
        recs,
        vec![WalRecord::SetToastRelid {
            name: "t".into(),
            toast_relid: 16385,
            xmin: 9,
        }]
    );
    // Stale op: the link no longer matches, so nothing is logged.
    eng.db
        .tables
        .get_mut("t")
        .unwrap()
        .last_mut()
        .unwrap()
        .toast_relid = 0;
    let recs = records_for_commit(&eng, 9, &writes, 0).unwrap();
    assert!(recs.is_empty());
}

#[test]
fn apply_record_slot_lifecycle() {
    let mut eng = Engine::new();
    // Create.
    apply_record(
        &mut eng,
        &WalRecord::ReplSlotCreate {
            name: "s1".into(),
            plugin: "rustgres_decoding".into(),
            slot_type: "logical".into(),
            restart_lsn: 100,
        },
    )
    .unwrap();
    let s = eng.repl_slots.get("s1").unwrap();
    assert_eq!(s.plugin, "rustgres_decoding");
    assert_eq!(s.slot_type, "logical");
    assert_eq!(s.restart_lsn, 100);
    assert_eq!(s.confirmed_flush_lsn, 100);
    assert!(!s.active); // replay never resurrects active state
    // Flush forward, then backward (flush never moves backwards).
    apply_record(
        &mut eng,
        &WalRecord::ReplSlotFlush {
            name: "s1".into(),
            restart_lsn: 100,
            confirmed_flush_lsn: 200,
        },
    )
    .unwrap();
    assert_eq!(eng.repl_slots["s1"].confirmed_flush_lsn, 200);
    assert_eq!(eng.repl_slots["s1"].restart_lsn, 100);
    apply_record(
        &mut eng,
        &WalRecord::ReplSlotFlush {
            name: "s1".into(),
            restart_lsn: 150,
            confirmed_flush_lsn: 150,
        },
    )
    .unwrap();
    assert_eq!(eng.repl_slots["s1"].confirmed_flush_lsn, 200);
    // restart_lsn follows the flushed record forward (never back).
    assert_eq!(eng.repl_slots["s1"].restart_lsn, 150);
    // Flush of a missing slot is a no-op, not an error.
    apply_record(
        &mut eng,
        &WalRecord::ReplSlotFlush {
            name: "ghost".into(),
            restart_lsn: 0,
            confirmed_flush_lsn: 9,
        },
    )
    .unwrap();
    // Drop.
    apply_record(&mut eng, &WalRecord::ReplSlotDrop { name: "s1".into() }).unwrap();
    assert!(!eng.repl_slots.contains_key("s1"));
}

#[test]
fn system_id_is_stable_per_data_directory() {
    let dir = std::env::temp_dir().join("rg13-system-id-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // First open mints a non-zero id and persists exactly 8 bytes.
    let id1 = load_or_create_system_id(&dir).unwrap();
    assert_ne!(id1, 0);
    let raw = std::fs::read(dir.join("system.id")).unwrap();
    assert_eq!(raw.len(), 8);
    assert_eq!(u64::from_be_bytes(raw.try_into().unwrap()), id1);
    // Reopening the same directory returns the identical id.
    let id2 = load_or_create_system_id(&dir).unwrap();
    assert_eq!(id1, id2);
    // A corrupt (short) file is replaced, not propagated.
    std::fs::write(dir.join("system.id"), b"junk").unwrap();
    let id3 = load_or_create_system_id(&dir).unwrap();
    assert_ne!(id3, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn v060_numeric_typmod_coltype_roundtrip() {
    // v0.60: constrained numeric(p,s) survives the WAL coltype
    // codec (tag 18); unconstrained keeps the legacy tag 7.
    for tm in [
        ColType::Numeric(None),
        ColType::Numeric(Some((10, 2))),
        ColType::Numeric(Some((3, 6))),
        ColType::Numeric(Some((5, -2))),
    ] {
        let mut e = Enc::new();
        e.col_type(&tm);
        let mut d = Dec::new(&e.buf);
        assert_eq!(d.col_type().unwrap(), tm);
        d.end().unwrap();
    }
}

#[test]
fn v078_array_coltype_roundtrip() {
    // v0.78: array column types survive the WAL codec (tag 22 +
    // the PG array OID identifying the element type).
    use crate::storage::ArrayElem;
    for elem in [
        ArrayElem::Bool,
        ArrayElem::Int,
        ArrayElem::Text,
        ArrayElem::Numeric,
        ArrayElem::Timestamp,
        ArrayElem::Uuid,
        ArrayElem::Json,
    ] {
        let t = ColType::Array(elem);
        let mut e = Enc::new();
        e.col_type(&t);
        let mut d = Dec::new(&e.buf);
        assert_eq!(d.col_type().unwrap(), t);
        d.end().unwrap();
    }
}

#[test]
fn v088_create_index_metadata_roundtrip() {
    // v0.88: CreateIndex carries per-column direction / null-placement
    // / expression sources plus the partial predicate (RGSWAL15).
    let rec = WalRecord::CreateIndex {
        name: "ix".into(),
        table: "t".into(),
        columns: vec!["a".into(), "(a + b)".into()],
        unique: true,
        internal: false,
        desc: vec![true, false],
        nulls_first: vec![false, true],
        exprs: vec![None, Some("a + b".into())],
        predicate: Some("b > 0".into()),
        xmin: 42,
    };
    let mut e = Enc::new();
    e.record(&rec);
    let mut d = Dec::new(&e.buf);
    match d.record().unwrap() {
        WalRecord::CreateIndex {
            name,
            table,
            columns,
            unique,
            internal,
            desc,
            nulls_first,
            exprs,
            predicate,
            xmin,
        } => {
            assert_eq!(name, "ix");
            assert_eq!(table, "t");
            assert_eq!(columns, vec!["a".to_string(), "(a + b)".to_string()]);
            assert!(unique);
            assert!(!internal);
            assert_eq!(desc, vec![true, false]);
            assert_eq!(nulls_first, vec![false, true]);
            assert_eq!(exprs, vec![None, Some("a + b".to_string())]);
            assert_eq!(predicate, Some("b > 0".to_string()));
            assert_eq!(xmin, 42);
        }
        other => panic!("expected CreateIndex, got {:?}", other),
    }
    d.end().unwrap();
}

/// Copy the v1.79 DROP COLUMN fixture into a fresh scratch directory.
fn v180_fixture_dir(tag: &str) -> std::path::PathBuf {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wal_v179_drop_column");
    let dir = std::env::temp_dir().join(format!("rg180-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    for f in ["wal.log", "system.id"] {
        std::fs::copy(src.join(f), dir.join(f)).expect("copy fixture");
    }
    dir
}

#[test]
fn v180_replays_v179_drop_column_wal() {
    // Regression: v1.79 and earlier logged DROP COLUMN as AlterTable -> InsertRows ->
    // DropIndex -> CreateIndex. Replaying InsertRows indexed the rewritten
    // one-column rows through the stale index (position 1) and panicked,
    // so the data directory could never be opened again.
    let dir = v180_fixture_dir("replay");
    let (eng, _wal) = Wal::open(&dir).expect("v1.79 WAL must replay");
    let t = eng
        .db
        .tables
        .get("t")
        .and_then(|vs| vs.iter().find(|t| t.dropped_xmax == 0))
        .expect("table t survives recovery");
    let cols: Vec<&str> = t.columns.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(cols, vec!["a"]);
    let mut live: Vec<i64> = t
        .rows
        .iter()
        .filter(|r| r.xmax == 0)
        .map(|r| match r.values[0] {
            Value::Int(v) => v,
            ref other => panic!("unexpected value {:?}", other),
        })
        .collect();
    live.sort_unstable();
    assert_eq!(live, vec![3, 4]);
    // The unique index is rebuilt on the shifted position and covers
    // every surviving row exactly once.
    let ix = eng
        .db
        .indexes
        .get("t_a_key")
        .expect("unique index survives recovery");
    assert_eq!(ix.def.cols, vec![0]);
    assert_eq!(ix.def.dropped_xmax, 0);
    // Opening an older-format log upgrades the data directory before any
    // new frame is appended: the live WAL generation carries the current
    // magic, so new-format records never land in an old-format file.
    let mut hdr = [0u8; 8];
    use std::io::Read;
    std::fs::File::open(dir.join("wal.log"))
        .and_then(|mut f| f.read_exact(&mut hdr))
        .expect("read header");
    assert_eq!(&hdr, WAL_MAGIC);
    drop(eng);
    // And the upgraded directory reopens with the same contents.
    let (eng, _wal) = Wal::open(&dir).expect("upgraded dir reopens");
    assert!(eng.db.indexes.contains_key("t_a_key"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A partition descriptor exercising every encoded field: a range bound
/// with an unbounded side, an expression key, a parent link and children.
fn v180_sample_partition() -> crate::storage::PartitionInfo {
    use crate::storage::{PartBound, PartKey, PartMethod, PartitionInfo, RangeBound};
    PartitionInfo {
        method: PartMethod::Range,
        key: vec![PartKey { col: 1, expr: None }],
        bound: Some(PartBound::Range {
            lower: vec![RangeBound::Val(Value::Int(0))],
            upper: vec![RangeBound::Max],
        }),
        is_default: false,
        parent: Some("root".into()),
        children: vec!["leaf_a".into(), "leaf_b".into()],
        is_partitioned: true,
    }
}

#[test]
fn v180_rgswal20_records_decode_without_partition() {
    // An RGSWAL20 frame ends at xmin; decoding it as version 20 must not
    // read past the record (no trailing partition field).
    let rec = WalRecord::CreateTable {
        name: "t".into(),
        columns: vec![("a".into(), ColType::Int)],
        constraints: "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))".into(),
        owner: "postgres".into(),
        acl: vec![],
        col_acl: vec![],
        oid: 16384,
        toast_relid: 0,
        col_compression: vec![0],
        composite_types: vec![None],
        domain_types: vec![None],
        domain_elem: vec![false],
        inherits: vec![],
        attnums: vec![1],
        next_attnum: 2,
        fillfactor: 100,
        partition: Some(v180_sample_partition()),
        xmin: 3,
    };
    let mut e = Enc::new();
    e.record(&rec);
    // Strip the trailing v21 partition bytes to get the v20 layout.
    let mut tail = Enc::new();
    tail.partition(&Some(v180_sample_partition()));
    let v20 = &e.buf[..e.buf.len() - tail.buf.len()];
    let mut d = Dec::with_wal_version(v20, 20);
    match d.record().unwrap() {
        WalRecord::CreateTable {
            partition, xmin, ..
        } => {
            assert_eq!(partition, None);
            assert_eq!(xmin, 3);
        }
        other => panic!("expected CreateTable, got {:?}", other),
    }
    d.end().unwrap();
    // And the full v21 bytes round-trip the partition.
    let mut d = Dec::new(&e.buf);
    assert_eq!(d.record().unwrap(), rec);
    d.end().unwrap();
}

#[test]
fn v180_legacy_fill_partition_keeps_live_metadata() {
    // An RGSWAL20 AlterTable on a partitioned table decodes with no
    // partition metadata. open() must carry the live version's metadata
    // forward instead of de-partitioning the table.
    let empty_constraints =
        "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))".to_string();
    let mut eng = Engine::new();
    apply_record(
        &mut eng,
        &WalRecord::CreateTable {
            name: "c".into(),
            columns: vec![("a".into(), ColType::Int)],
            constraints: empty_constraints.clone(),
            owner: "postgres".into(),
            acl: vec![],
            col_acl: vec![],
            oid: 16384,
            toast_relid: 0,
            col_compression: vec![0],
            composite_types: vec![None],
            domain_types: vec![None],
            domain_elem: vec![false],
            inherits: vec![],
            attnums: vec![1],
            next_attnum: 2,
            fillfactor: 100,
            partition: Some(v180_sample_partition()),
            xmin: 3,
        },
    )
    .unwrap();
    let legacy_alter = WalRecord::AlterTable {
        name: "c".into(),
        columns: vec![("a".into(), ColType::Int)],
        constraints: empty_constraints,
        copy_rows: true,
        owner: "postgres".into(),
        acl: vec![],
        col_acl: vec![],
        oid: 16384,
        toast_relid: 0,
        toast_target: 0,
        col_storage: vec![0],
        col_compression: vec![0],
        composite_types: vec![None],
        domain_types: vec![None],
        domain_elem: vec![false],
        inherits: vec![],
        attnums: vec![1],
        next_attnum: 2,
        fillfactor: 100,
        partition: None,
        next_value_id: 1,
        toast_info: vec![],
        xmin: 4,
    };
    let filled = legacy_fill_partition(&eng, &legacy_alter).expect("filled");
    apply_record(&mut eng, &filled).unwrap();
    let live = eng.db.tables["c"]
        .iter()
        .find(|t| t.dropped_xmax == 0)
        .expect("live version");
    assert_eq!(live.partition, Some(v180_sample_partition()));
    // A current-format record is applied exactly as written: None means
    // "not partitioned", and an unpartitioned table needs no fill.
    let mut plain = Engine::new();
    assert!(legacy_fill_partition(&plain, &legacy_alter).is_none());
    apply_record(&mut plain, &legacy_alter).unwrap();
}

fn v180_alter(cols: &[&str], xmin: u64) -> WalRecord {
    let n = cols.len();
    WalRecord::AlterTable {
        name: "t".into(),
        columns: cols
            .iter()
            .map(|c| ((*c).to_string(), ColType::Int))
            .collect(),
        constraints: "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))"
            .to_string(),
        copy_rows: false,
        owner: "postgres".into(),
        acl: vec![],
        col_acl: vec![],
        oid: 16384,
        toast_relid: 0,
        toast_target: 0,
        col_storage: vec![0; n],
        col_compression: vec![0; n],
        composite_types: vec![None; n],
        domain_types: vec![None; n],
        domain_elem: vec![false; n],
        inherits: vec![],
        attnums: (1..=n as i16).collect(),
        next_attnum: n as i16 + 1,
        fillfactor: 100,
        partition: None,
        next_value_id: 1,
        toast_info: vec![],
        xmin,
    }
}

fn v180_insert(id: u64, width: usize, xmin: u64) -> WalRecord {
    WalRecord::InsertRows {
        table: "t".into(),
        rows: vec![WalRow {
            id,
            xmin,
            values: Row::new((0..width as i64).map(Value::Int).collect()),
            toast: vec![0; width],
            toast_meta: vec![],
        }],
    }
}

#[test]
fn v180_replay_skips_rows_wider_than_their_version() {
    // v1.79 logged every AlterTable of a multi-action ALTER with the final
    // shape, so the first rewrite's (wider) rows land in a version with
    // fewer columns. Replay must not build that inconsistent version, or
    // the next checkpoint (including the RGSWAL20 upgrade checkpoint) is
    // unloadable.
    let mut eng = engine_with_committed_table();
    for r in [
        v180_alter(&["k", "keep1", "keep2"], 20),
        v180_insert(100, 4, 20), // the mis-logged intermediate rewrite
        v180_alter(&["k", "keep1", "keep2"], 20),
        v180_insert(101, 3, 20),
    ] {
        apply_record(&mut eng, &r).unwrap();
    }
    for v in &eng.db.tables["t"] {
        for row in &v.rows {
            assert_eq!(row.values.len(), v.columns.len(), "version {:?}", v.columns);
        }
    }
    let live = eng.db.tables["t"]
        .iter()
        .find(|t| t.dropped_xmax == 0)
        .unwrap();
    assert_eq!(
        live.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![101]
    );
}

#[test]
fn v180_checkpoint_load_tolerates_bad_rows_in_superseded_versions() {
    // Data directories checkpointed by v1.79 after replaying a mis-logged
    // multi-action ALTER hold a superseded table version with rows wider
    // than its columns. That version is invisible to every snapshot, so
    // loading drops those rows instead of refusing to start. A LIVE
    // version with such rows is still rejected as corruption.
    let dir = std::env::temp_dir().join("rg180-ckpt-superseded");
    let _ = std::fs::remove_dir_all(&dir);
    let cols = |names: &[&str]| -> Vec<(String, ColType)> {
        names
            .iter()
            .map(|c| ((*c).to_string(), ColType::Int))
            .collect()
    };
    let row = |id: u64, width: usize| {
        RowVersion::plain(id, Row::new((0..width as i64).map(Value::Int).collect()), 2)
    };
    let mut eng = Engine::new();
    let mut superseded = Table::new(cols(&["k", "a", "b"]), 2);
    superseded.dropped_xmax = 2;
    superseded.push_version(row(10, 4));
    let mut live = Table::new(cols(&["k", "a", "b"]), 2);
    live.push_version(row(11, 3));
    eng.db.tables.insert("t".into(), vec![superseded, live]);
    eng.txns.next_xid = 3;
    eng.txns.next_row_id = 12;
    {
        let (_e, mut wal) = Wal::open(&dir).expect("open");
        wal.checkpoint(&eng).expect("checkpoint");
    }
    let (loaded, _wal) = Wal::open(&dir).expect("superseded junk must not block startup");
    let vs = &loaded.db.tables["t"];
    let live = vs
        .iter()
        .find(|t| t.dropped_xmax == 0)
        .expect("live version");
    assert_eq!(live.rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![11]);
    for v in vs {
        assert!(v.rows.iter().all(|r| r.values.len() == v.columns.len()));
    }

    // The same bad row in the LIVE version is real corruption: refuse.
    let _ = std::fs::remove_dir_all(&dir);
    let mut eng = Engine::new();
    let mut live = Table::new(cols(&["k", "a", "b"]), 2);
    live.push_version(row(11, 4));
    eng.db.tables.insert("t".into(), vec![live]);
    eng.txns.next_xid = 3;
    eng.txns.next_row_id = 12;
    {
        let (_e, mut wal) = Wal::open(&dir).expect("open");
        wal.checkpoint(&eng).expect("checkpoint");
    }
    assert!(
        Wal::open(&dir).is_err(),
        "a live mismatch must still be rejected"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
