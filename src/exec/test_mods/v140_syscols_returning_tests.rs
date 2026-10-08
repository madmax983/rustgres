// v1.40: system columns in RETURNING (`tableoid`, `xmin`/`xmax` via
// per-row provenance) + the missing SELECT siblings (`ctid`, `cmin`,
// `cmax`). `tableoid` reports the partition leaf holding the row
// (PG19); `ctid` is synthesized from the row's storage position
// `(block, offset)`; `cmin`/`cmax` are always 0 (no per-command
// tracking, like PG's display of a frozen cid).
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

fn dml_rows(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql).unwrap() {
        ExecResult::Dml { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected DML, got {other:?}"),
    }
}

fn err_code(eng: &mut Engine, sql: &str) -> &'static str {
    run(eng, sql).unwrap_err().code
}

#[test]
fn insert_returning_syscols() {
    let mut eng = engine();
    run(&mut eng, "create table t140(a int);").unwrap();
    let rows = dml_rows(
        &mut eng,
        "insert into t140 values (1),(2) returning a, tableoid, xmin, xmax, ctid, cmin, cmax;",
    );
    assert_eq!(rows.len(), 2);
    // a, tableoid>0, xmin=9 (write xid), xmax=0, ctid=(0,0)/(0,1),
    // cmin=0, cmax=0.
    assert_eq!(rows[0][0], "1");
    assert!(rows[0][1].parse::<u32>().unwrap() > 0);
    assert_eq!(rows[0][2], "9");
    assert_eq!(rows[0][3], "0");
    assert_eq!(rows[0][4], "(0,0)");
    assert_eq!(rows[0][5], "0");
    assert_eq!(rows[0][6], "0");
    assert_eq!(rows[1][4], "(0,1)");
}

#[test]
fn update_returning_syscols_new_version() {
    let mut eng = engine();
    run(&mut eng, "create table t140(a int);").unwrap();
    run(&mut eng, "insert into t140 values (1),(2);").unwrap();
    let rows = dml_rows(
        &mut eng,
        "update t140 set a = 10 where a = 1 returning a, tableoid, xmin, xmax, ctid;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "10");
    assert!(rows[0][1].parse::<u32>().unwrap() > 0);
    // PG evaluates UPDATE RETURNING against the NEW version.
    assert_eq!(rows[0][2], "9");
    assert_eq!(rows[0][3], "0");
    // New version appended after the two inserts: position (0,2).
    assert_eq!(rows[0][4], "(0,2)");
}

#[test]
fn delete_returning_syscols_old_version() {
    let mut eng = engine();
    run(&mut eng, "create table t140(a int);").unwrap();
    run(&mut eng, "insert into t140 values (1),(2);").unwrap();
    let rows = dml_rows(
        &mut eng,
        "delete from t140 where a = 2 returning a, tableoid, xmin, xmax, ctid;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "2");
    // PG evaluates DELETE RETURNING against the OLD version: its
    // xmax is the deleting xid.
    assert_eq!(rows[0][2], "9");
    assert_eq!(rows[0][3], "9");
    assert_eq!(rows[0][4], "(0,1)");
}

#[test]
fn partitioned_insert_returning_tableoid_leaf() {
    let mut eng = engine();
    run(&mut eng, "create table p140(a int) partition by range (a);").unwrap();
    run(
        &mut eng,
        "create table p140_1 partition of p140 for values from (1) to (10);",
    )
    .unwrap();
    run(
        &mut eng,
        "create table p140_2 partition of p140 for values from (10) to (20);",
    )
    .unwrap();
    // insert.sql:442 equivalent — tableoid::regclass names the leaf
    // holding each row, not the partitioned parent.
    let rows = dml_rows(
        &mut eng,
        "insert into p140 values (5),(15) returning a, tableoid::regclass;",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec!["5".to_string(), "p140_1".to_string()]);
    assert_eq!(rows[1], vec!["15".to_string(), "p140_2".to_string()]);
}

#[test]
fn update_from_unqualified_ctid_ambiguous() {
    let mut eng = engine();
    run(&mut eng, "create table t140(a int);").unwrap();
    run(&mut eng, "create table u140(a int);").unwrap();
    run(&mut eng, "insert into t140 values (1);").unwrap();
    run(&mut eng, "insert into u140 values (1);").unwrap();
    // Two ranges in scope: unqualified ctid is 42702, like PG.
    // (Pins join.sql:3448's PASS — this must stay an error.)
    assert_eq!(
        err_code(&mut eng, "update t140 set a = 1 from u140 returning ctid;"),
        "42702"
    );
    // Qualified resolves to the target's new version (fresh table:
    // the 42702 statement above applied its UPDATE before RETURNING
    // failed, so t140 already has a second version).
    run(&mut eng, "create table t141(a int);").unwrap();
    run(&mut eng, "insert into t141 values (1);").unwrap();
    let rows = dml_rows(
        &mut eng,
        "update t141 set a = 2 from u140 returning t141.ctid;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "(0,1)");
}

#[test]
fn select_ctid_cmin_cmax_siblings() {
    let mut eng = engine();
    run(&mut eng, "create table t140(a int);").unwrap();
    run(&mut eng, "insert into t140 values (1);").unwrap();
    match run(&mut eng, "select ctid, cmin, cmax from t140;").unwrap() {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let cells: Vec<String> = rows
                .into_iter()
                .next()
                .unwrap()
                .into_iter()
                .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                .collect();
            assert_eq!(cells, vec!["(0,0)", "0", "0"]);
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.41: `pg_relation_size` does PG19 heap page accounting — the
/// insert.sql:51 case: fillfactor=10, one 32-byte and one
/// 1032-byte tuple. PG's "nearly empty page" rule puts the large
/// tuple on page 0 anyway: exactly 1 page = 8192 bytes.
#[test]
fn v141_pg_relation_size_heap_page_accounting() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE large_tuple_test (a int, b text) WITH (fillfactor = 10);",
    )
    .unwrap();
    run(
        &mut eng,
        "ALTER TABLE large_tuple_test ALTER COLUMN b SET STORAGE plain;",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO large_tuple_test (select 1, NULL);").unwrap();
    run(
        &mut eng,
        "INSERT INTO large_tuple_test (select 2, repeat('a', 1000));",
    )
    .unwrap();
    match run(
        &mut eng,
        "SELECT pg_relation_size('large_tuple_test'::regclass, 'main');",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let row = rows.into_iter().next().unwrap().into_cells();
            assert_eq!(row.into_iter().next().unwrap(), Value::BigInt(8192));
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.41: `pg_relation_size` of an empty table is 0 (no pages).
#[test]
fn v141_pg_relation_size_empty_is_zero() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t141empty (a int);").unwrap();
    match run(&mut eng, "SELECT pg_relation_size('t141empty');").unwrap() {
        ExecResult::Select { rows, .. } => {
            let row = rows.into_iter().next().unwrap().into_cells();
            assert_eq!(row.into_iter().next().unwrap(), Value::BigInt(0));
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.41: `pg_attribute.attnum` never reuses dropped numbers — the
/// insert.sql:382 sequence (add/drop/add/drop/add) leaves attnum 4
/// on the surviving column.
#[test]
fn v141_attnum_never_reused() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t141a (a int, b int);").unwrap();
    run(&mut eng, "ALTER TABLE t141a DROP COLUMN a;").unwrap();
    run(&mut eng, "ALTER TABLE t141a ADD COLUMN a int;").unwrap();
    run(&mut eng, "ALTER TABLE t141a DROP COLUMN a;").unwrap();
    run(&mut eng, "ALTER TABLE t141a ADD COLUMN a int NOT NULL;").unwrap();
    match run(
            &mut eng,
            "SELECT attname, attnum FROM pg_attribute WHERE attrelid = 't141a'::regclass ORDER BY attname;",
        )
        .unwrap()
        {
            ExecResult::Select { rows, .. } => {
                let mut cells: Vec<String> = Vec::new();
                for r in rows {
                    for v in r.into_cells() {
                        cells.push(v.to_text().unwrap_or("NULL".to_string()));
                    }
                }
                // b keeps 2, a was re-added twice -> 4.
                assert_eq!(cells, vec!["a", "4", "b", "2"]);
            }
            other => panic!("expected SELECT, got {other:?}"),
        }
}

/// v1.41: fresh tables get sequential attnums 1..=n.
#[test]
fn v141_attnum_fresh_table_sequential() {
    let t = Table::new(
        vec![
            ("x".to_string(), ColType::Int),
            ("y".to_string(), ColType::Text),
        ],
        1,
    );
    assert_eq!(t.attnums, vec![1, 2]);
    assert_eq!(t.next_attnum, 3);
    assert_eq!(t.fillfactor, 100);
}

/// v1.41: `pg_heap_tuple_len` matches PG19 `heap_form_tuple` for
/// the two large_tuple_test rows: (1,NULL) -> 32, (2,'a'x1000) ->
/// 1032 (MAXALIGN(23+1 bitmap) = 32 header; int4 at offset 32;
/// text short-varlena header 1 + 1000).
#[test]
fn v141_pg_heap_tuple_len() {
    let t = Table::new(
        vec![
            ("a".to_string(), ColType::Int),
            ("b".to_string(), ColType::Text),
        ],
        1,
    );
    let r1 = RowVersion::plain(1, Row::new(vec![Value::Int(1), Value::Null]), 1);
    let r2 = RowVersion::plain(
        2,
        Row::new(vec![Value::Int(2), Value::text("a".repeat(1000))]),
        1,
    );
    assert_eq!(pg_heap_tuple_len(&t, &r1), 32);
    assert_eq!(pg_heap_tuple_len(&t, &r2), 1032);
}

/// v1.41: `pg_heap_page_count` — fillfactor reserve is dropped for
/// tuples larger than the nearly-empty threshold (PG19 hio.c), so
/// the 1032-byte tuple joins the 32-byte tuple on page 0.
#[test]
fn v141_pg_heap_page_count_fillfactor() {
    let mut t = Table::new(
        vec![
            ("a".to_string(), ColType::Int),
            ("b".to_string(), ColType::Text),
        ],
        1,
    );
    t.fillfactor = 10;
    assert_eq!(pg_heap_page_count(&t, &[32, 1032]), 8192);
    // Empty table: no pages.
    assert_eq!(pg_heap_page_count(&t, &[]), 0);
    // Default fillfactor: ~227 small tuples per page.
    t.fillfactor = 100;
    let lens = vec![32; 300];
    assert_eq!(pg_heap_page_count(&t, &lens), 2 * 8192);
}

/// v1.42: `pg_class.relkind` is 't' for TOAST tables (PG19
/// RELKIND_TOASTVALUE, pg_class.h: "for out-of-line values").
#[test]
fn v142_pg_class_relkind_toast_is_t() {
    let mut eng = engine();
    // text is toastable, so the toast table is created eagerly at
    // CREATE TABLE (ensure_toast_table_eager).
    run(&mut eng, "CREATE TABLE t142toast (f1 text);").unwrap();
    match run(
        &mut eng,
        "SELECT relkind FROM pg_class WHERE relname LIKE 'pg_toast.%';",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let cells = rows.into_iter().next().unwrap().into_cells();
            assert_eq!(cells, vec![Value::SingleChar(b't')]);
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.42: the ordinary table itself stays 'r' (regression guard for
/// the relkind if/else chain).
#[test]
fn v142_pg_class_relkind_ordinary_is_r() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t142ord (a int);").unwrap();
    match run(
        &mut eng,
        "SELECT relkind FROM pg_class WHERE relname = 't142ord';",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let cells = rows.into_iter().next().unwrap().into_cells();
            assert_eq!(cells, vec![Value::SingleChar(b'r')]);
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.42: partitioned tables stay 'p' (regression guard — the 't'
/// arm must not swallow the partitioned arm).
#[test]
fn v142_pg_class_relkind_partitioned_stays_p() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE t142part (a int, b int) PARTITION BY RANGE (a, b);",
    )
    .unwrap();
    match run(
        &mut eng,
        "SELECT relkind FROM pg_class WHERE relname = 't142part';",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let cells = rows.into_iter().next().unwrap().into_cells();
            assert_eq!(cells, vec![Value::SingleChar(b'p')]);
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.43: `ROW(t.*)` expands the star into the row's field list
/// (flat), per PG19 — it is not a nested whole-row value. PG16:
/// `SELECT ROW(pj1.*) FROM pj1` → `(1,4,one)`.
#[test]
fn v143_row_star_expands_flat() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t143r (a int, b text);").unwrap();
    run(&mut eng, "INSERT INTO t143r VALUES (1, 'x');").unwrap();
    match run(&mut eng, "SELECT ROW(t143r.*) FROM t143r;").unwrap() {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let cells = rows.into_iter().next().unwrap().into_cells();
            assert_eq!(cells.len(), 1);
            match &cells[0] {
                Value::Record(fields) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].0, "f1");
                    assert_eq!(fields[0].1, Value::Int(1));
                    assert_eq!(fields[1].0, "f2");
                    assert_eq!(fields[1].1, Value::Text("x".into()));
                }
                other => panic!("expected Record, got {other:?}"),
            }
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.43: the corpus statement shape (join.sql:138) — an
/// unparenthesized `JOIN ... USING ... AS x` alias exposes only the
/// merged key columns, so `ROW(x.*)` is a one-field row. PG19
/// `expected/join.out` → `(1)`.
#[test]
fn v143_row_star_join_using_alias() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t143j1 (i int, j int);").unwrap();
    run(&mut eng, "CREATE TABLE t143j2 (i int, k int);").unwrap();
    run(&mut eng, "INSERT INTO t143j1 VALUES (1, 4);").unwrap();
    run(&mut eng, "INSERT INTO t143j2 VALUES (1, -1);").unwrap();
    match run(
        &mut eng,
        "SELECT ROW(x.*) FROM t143j1 JOIN t143j2 USING (i) AS x;",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let cells = rows.into_iter().next().unwrap().into_cells();
            assert_eq!(cells.len(), 1);
            match &cells[0] {
                Value::Record(fields) => {
                    assert_eq!(fields.len(), 1);
                    assert_eq!(fields[0].0, "f1");
                    assert_eq!(fields[0].1, Value::Int(1));
                }
                other => panic!("expected Record, got {other:?}"),
            }
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.43: star fields and scalar fields share one sequential
/// `f1, f2, ...` namespace. PG16:
/// `row_to_json(ROW(pj1.*, 99))` → `{"f1":..,"f2":..,"f3":99}`.
#[test]
fn v143_row_star_mixed_with_scalar() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t143m (a int, b int);").unwrap();
    run(&mut eng, "INSERT INTO t143m VALUES (1, 2);").unwrap();
    match run(&mut eng, "SELECT ROW(t143m.*, 99) FROM t143m;").unwrap() {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let cells = rows.into_iter().next().unwrap().into_cells();
            match &cells[0] {
                Value::Record(fields) => {
                    assert_eq!(fields.len(), 3);
                    assert_eq!(fields[0].0, "f1");
                    assert_eq!(fields[1].0, "f2");
                    assert_eq!(fields[2].0, "f3");
                    assert_eq!(fields[2].1, Value::Int(99));
                }
                other => panic!("expected Record, got {other:?}"),
            }
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.43: plain `ROW(a, b)` without a star takes the unchanged path
/// (regression guard).
#[test]
fn v143_row_no_star_unchanged() {
    let mut eng = engine();
    match run(&mut eng, "SELECT ROW(1, 'a');").unwrap() {
        ExecResult::Select { rows, .. } => {
            let cells = rows.into_iter().next().unwrap().into_cells();
            match &cells[0] {
                Value::Record(fields) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0], ("f1".to_string(), Value::Int(1)));
                    assert_eq!(fields[1], ("f2".to_string(), Value::Text("a".into())));
                }
                other => panic!("expected Record, got {other:?}"),
            }
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.43: PG19's `FigureColnameInternal` names an unaliased
/// `ROW(...)` output column "row" (parse_target.c, `T_RowExpr`:
/// "make ROW() act like a function"). join.sql:138 needs this as
/// well as the value fix — the runner compares column names.
#[test]
fn v143_row_colname_is_row() {
    let mut eng = engine();
    match run(&mut eng, "SELECT ROW(1, 2);").unwrap() {
        ExecResult::Select { columns, .. } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].0, "row");
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
    // A cast over ROW() keeps "row" (strength 2), like PG16.
    match run(&mut eng, "SELECT ROW(1, 2)::text;").unwrap() {
        ExecResult::Select { columns, .. } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].0, "row");
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.43: a whole-row Var OUTSIDE `ROW(...)` still evaluates to a
/// nested composite (regression guard — `eval_wholerow` behavior
/// is unchanged; only `ROW(qual.*)` flattens).
#[test]
fn v143_wholerow_outside_row_still_nests() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t143w (a int, b int);").unwrap();
    run(&mut eng, "INSERT INTO t143w VALUES (1, 2);").unwrap();
    match run(&mut eng, "SELECT t143w FROM t143w;").unwrap() {
        ExecResult::Select { rows, .. } => {
            let cells = rows.into_iter().next().unwrap().into_cells();
            match &cells[0] {
                Value::Record(fields) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].0, "a");
                    assert_eq!(fields[1].0, "b");
                }
                other => panic!("expected Record, got {other:?}"),
            }
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.44: `information_schema.columns.ordinal_position` is PG19's
/// `CAST(a.attnum AS cardinal_number)` (information_schema.sql:672),
/// not the dense 1-based column position. With no DROP COLUMN the
/// two coincide.
#[test]
fn v144_ordinal_position_dense_without_drop() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t144a (a int, b text, c int);").unwrap();
    match run(
        &mut eng,
        "SELECT column_name, ordinal_position FROM information_schema.columns \
             WHERE table_name = 't144a' ORDER BY ordinal_position",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => {
            let got: Vec<(String, i64)> = rows
                .into_iter()
                .map(|r| {
                    let c = r.into_cells();
                    let Value::Text(n) = &c[0] else {
                        panic!("name")
                    };
                    let Value::Int(p) = c[1] else { panic!("pos") };
                    (n.to_string(), p)
                })
                .collect();
            assert_eq!(
                got,
                vec![
                    ("a".to_string(), 1),
                    ("b".to_string(), 2),
                    ("c".to_string(), 3)
                ]
            );
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.44: after `ALTER TABLE ... DROP COLUMN`, the attnum gap
/// survives in ordinal_position — PG16 probe: positions 1 and 3
/// (the dropped column's attnum is never reused).
#[test]
fn v144_ordinal_position_gap_after_drop() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t144d (a int, b text, c int);").unwrap();
    run(&mut eng, "ALTER TABLE t144d DROP COLUMN b;").unwrap();
    match run(
        &mut eng,
        "SELECT column_name, ordinal_position FROM information_schema.columns \
             WHERE table_name = 't144d' ORDER BY ordinal_position",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => {
            let got: Vec<(String, i64)> = rows
                .into_iter()
                .map(|r| {
                    let c = r.into_cells();
                    let Value::Text(n) = &c[0] else {
                        panic!("name")
                    };
                    let Value::Int(p) = c[1] else { panic!("pos") };
                    (n.to_string(), p)
                })
                .collect();
            assert_eq!(got, vec![("a".to_string(), 1), ("c".to_string(), 3)]);
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

/// v1.44: `ALTER TABLE ... ADD COLUMN` after a DROP takes
/// max(attnums ever)+1 — never the dropped attnum — and
/// ordinal_position follows it (PG19 ATExecAddColumn).
#[test]
fn v144_ordinal_position_add_after_drop_never_reuses() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t144n (a int, b text, c int);").unwrap();
    run(&mut eng, "ALTER TABLE t144n DROP COLUMN b;").unwrap();
    run(&mut eng, "ALTER TABLE t144n ADD COLUMN d int;").unwrap();
    match run(
        &mut eng,
        "SELECT column_name, ordinal_position FROM information_schema.columns \
             WHERE table_name = 't144n' ORDER BY ordinal_position",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => {
            let got: Vec<(String, i64)> = rows
                .into_iter()
                .map(|r| {
                    let c = r.into_cells();
                    let Value::Text(n) = &c[0] else {
                        panic!("name")
                    };
                    let Value::Int(p) = c[1] else { panic!("pos") };
                    (n.to_string(), p)
                })
                .collect();
            assert_eq!(
                got,
                vec![
                    ("a".to_string(), 1),
                    ("c".to_string(), 3),
                    ("d".to_string(), 4)
                ]
            );
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}
