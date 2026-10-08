/// v1.12: zero-target-list SELECT (PG19 gram.y `opt_target_list` may be
/// empty in any simple SELECT). The executor produces genuine
/// zero-column rows: `SELECT;` -> one empty row, `SELECT FROM t` -> one
/// empty row per input row, through UNION/INTERSECT/EXCEPT and CTEs.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    // Preserve the parser's SQLSTATE (e.g. 42601), like the main
    // test harness does.
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

/// (ncols, nrows) of a SELECT result.
fn shape(r: ExecResult) -> (usize, usize) {
    match r {
        ExecResult::Select { columns, rows, .. } => (columns.len(), rows.len()),
        other => panic!("expected Select, got {:?}", other),
    }
}

/// v1.13: render a SELECT result as strings (NULL -> "NULL").
fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected Select, got {:?}", other),
    }
}

#[test]
fn v112_select_bare_empty_list() {
    // PG19: `SELECT;` returns a single row with zero columns.
    let mut eng = engine();
    assert_eq!(shape(run(&mut eng, "SELECT;").unwrap()), (0, 1));
    // v0.87's `SELECT WHERE false` keeps working (zero rows).
    assert_eq!(shape(run(&mut eng, "SELECT WHERE false;").unwrap()), (0, 0));
    assert_eq!(shape(run(&mut eng, "SELECT WHERE true;").unwrap()), (0, 1));
}

#[test]
fn v112_select_from_empty_list() {
    // PG19: `SELECT FROM t` returns one zero-column row per input row.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE es_t (a int, b text)").unwrap();
    run(
        &mut eng,
        "INSERT INTO es_t VALUES (1, 'x'), (2, 'y'), (3, 'z')",
    )
    .unwrap();
    assert_eq!(shape(run(&mut eng, "SELECT FROM es_t;").unwrap()), (0, 3));
    assert_eq!(
        shape(run(&mut eng, "SELECT FROM es_t WHERE a > 1;").unwrap()),
        (0, 2)
    );
    assert_eq!(
        shape(run(&mut eng, "SELECT FROM generate_series(1, 4);").unwrap()),
        (0, 4)
    );
}

#[test]
fn v112_empty_select_setops() {
    // PG19 set-operation semantics over zero-column branches:
    // UNION dedups the identical empty rows to one; UNION ALL keeps
    // all; INTERSECT [ALL] / EXCEPT [ALL] use multiset semantics.
    let mut eng = engine();
    assert_eq!(
        shape(run(&mut eng, "SELECT UNION SELECT;").unwrap()),
        (0, 1)
    );
    assert_eq!(
        shape(run(&mut eng, "SELECT INTERSECT SELECT;").unwrap()),
        (0, 1)
    );
    assert_eq!(
        shape(run(&mut eng, "SELECT EXCEPT SELECT;").unwrap()),
        (0, 0)
    );
    assert_eq!(
        shape(
            run(
                &mut eng,
                "SELECT FROM generate_series(1, 5) UNION ALL \
                     SELECT FROM generate_series(1, 3);"
            )
            .unwrap()
        ),
        (0, 8)
    );
    assert_eq!(
        shape(
            run(
                &mut eng,
                "SELECT FROM generate_series(1, 5) UNION \
                     SELECT FROM generate_series(1, 3);"
            )
            .unwrap()
        ),
        (0, 1)
    );
    assert_eq!(
        shape(
            run(
                &mut eng,
                "SELECT FROM generate_series(1, 5) INTERSECT ALL \
                     SELECT FROM generate_series(1, 3);"
            )
            .unwrap()
        ),
        (0, 3)
    );
    assert_eq!(
        shape(
            run(
                &mut eng,
                "SELECT FROM generate_series(1, 5) EXCEPT \
                     SELECT FROM generate_series(1, 3);"
            )
            .unwrap()
        ),
        (0, 0)
    );
    assert_eq!(
        shape(
            run(
                &mut eng,
                "SELECT FROM generate_series(1, 5) EXCEPT ALL \
                     SELECT FROM generate_series(1, 3);"
            )
            .unwrap()
        ),
        (0, 2)
    );
}

#[test]
fn v112_empty_select_cte() {
    // PG19: zero-target-list over a CTE (the union.sql variation).
    let mut eng = engine();
    assert_eq!(
        shape(
            run(
                &mut eng,
                "WITH cte AS MATERIALIZED (SELECT s FROM generate_series(1, 5) s) \
                     SELECT FROM cte UNION SELECT FROM cte;"
            )
            .unwrap()
        ),
        (0, 1)
    );
    assert_eq!(
        shape(
            run(
                &mut eng,
                "WITH cte AS NOT MATERIALIZED (SELECT s FROM generate_series(1, 5) s) \
                     SELECT FROM cte UNION SELECT FROM cte;"
            )
            .unwrap()
        ),
        (0, 1)
    );
}

#[test]
fn v112_empty_select_still_rejects_bogus() {
    // A FROM with no table is still a syntax error (PG19: 42601),
    // and a dangling comma is not silently swallowed.
    let mut eng = engine();
    let err = run(&mut eng, "SELECT FROM;").unwrap_err();
    assert_eq!(err.code, "42601");
    let err = run(&mut eng, "SELECT 1 2;").unwrap_err();
    assert_eq!(err.code, "42601");
    // DISTINCT over zero columns dedups to a single empty row (PG19).
    assert_eq!(
        shape(run(&mut eng, "SELECT DISTINCT FROM generate_series(1, 3);").unwrap()),
        (0, 1)
    );
}

/// v1.13: `tableoid` system column — the OID of the partition leaf
/// holding each row, and `::regclass` rendering it as the table name.
#[test]
fn v113_tableoid_partitioned() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE tp (a text, b int) PARTITION BY LIST (a);",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE tp1 PARTITION OF tp FOR VALUES IN ('x');",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE tp2 PARTITION OF tp FOR VALUES IN ('y');",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO tp VALUES ('x', 1), ('y', 2);").unwrap();
    // tableoid::regclass names the leaf partition per row.
    let r = run(&mut eng, "SELECT tableoid::regclass, a FROM tp ORDER BY a;").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 2);
    // Order by a: 'x' first, then 'y'.
    assert_eq!(rows[0][1], "x");
    assert_eq!(rows[1][1], "y");
    // The regclass names must be the leaf partition names.
    assert!(rows[0][0] == "tp1" || rows[0][0] == "tp2");
    assert!(rows[1][0] == "tp1" || rows[1][0] == "tp2");
    assert_ne!(rows[0][0], rows[1][0]);
    // Bare tableoid is the numeric OID.
    let r = run(&mut eng, "SELECT tableoid FROM tp WHERE a = 'x';").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 1);
    let oid: u32 = rows[0][0].parse().unwrap();
    assert!(oid > 0);
    // And it round-trips through ::regclass to the leaf name.
    let r = run(&mut eng, "SELECT tableoid::regclass FROM tp WHERE a = 'x';").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "tp1");
}

/// v1.13: `tableoid` works in GROUP BY / ORDER BY and on plain tables.
#[test]
fn v113_tableoid_group_by_plain() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE pt (a int);").unwrap();
    run(&mut eng, "INSERT INTO pt VALUES (1), (2);").unwrap();
    // Plain (non-partitioned) table: tableoid is the table's own OID.
    let r = run(&mut eng, "SELECT tableoid::regclass FROM pt LIMIT 1;").unwrap();
    assert_eq!(rows_of(r)[0][0], "pt");
    // GROUP BY over tableoid::regclass.
    run(
        &mut eng,
        "CREATE TABLE gp (a text, b int) PARTITION BY LIST (a);",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE gp1 PARTITION OF gp FOR VALUES IN ('x');",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE gp2 PARTITION OF gp FOR VALUES IN ('y');",
    )
    .unwrap();
    run(
        &mut eng,
        "INSERT INTO gp VALUES ('x', 1), ('x', 2), ('y', 3);",
    )
    .unwrap();
    let r = run(
        &mut eng,
        "SELECT tableoid::regclass::text, count(*) FROM gp GROUP BY 1 ORDER BY 1;",
    )
    .unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec!["gp1".to_string(), "2".to_string()]);
    assert_eq!(rows[1], vec!["gp2".to_string(), "1".to_string()]);
    // A user column named tableoid shadows the system column (PG19).
    run(&mut eng, "CREATE TABLE st (tableoid text);").unwrap();
    run(&mut eng, "INSERT INTO st VALUES ('mine');").unwrap();
    let r = run(&mut eng, "SELECT tableoid FROM st;").unwrap();
    assert_eq!(rows_of(r)[0][0], "mine");
}

/// v1.13: `pg_size_pretty(bigint)` formatting (PG19 misc.c thresholds).
#[test]
fn v113_pg_size_pretty() {
    let mut eng = engine();
    let mut one = |sql: &str| rows_of(run(&mut eng, sql).unwrap())[0][0].clone();
    assert_eq!(one("SELECT pg_size_pretty(0);"), "0 bytes");
    assert_eq!(one("SELECT pg_size_pretty(8192);"), "8192 bytes");
    // 10 KiB is the first kB threshold (integer division, like PG).
    assert_eq!(one("SELECT pg_size_pretty(10239);"), "10239 bytes");
    assert_eq!(one("SELECT pg_size_pretty(10240);"), "10 kB");
    assert_eq!(one("SELECT pg_size_pretty(1048576);"), "1024 kB");
    assert_eq!(one("SELECT pg_size_pretty(10485760);"), "10 MB");
    assert_eq!(one("SELECT pg_size_pretty(NULL);"), "NULL");
}

/// v1.16: routing into a childless *partitioned* intermediate names
/// the matched child in the 23514 (PG19 commits to the matched
/// partition; it does not fall through to siblings or DEFAULT).
#[test]
fn v116_childless_intermediate_names_child() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE mp (a int, b int, c text, d int) PARTITION BY RANGE (a, b);",
    )
    .unwrap();
    run(
            &mut eng,
            "CREATE TABLE mp5 PARTITION OF mp FOR VALUES FROM (1, 40) TO (1, 50) PARTITION BY RANGE (c);",
        )
        .unwrap();
    // mp5_cd is a partitioned table with no partitions.
    run(
            &mut eng,
            "CREATE TABLE mp5_cd PARTITION OF mp5 FOR VALUES FROM ('c') TO ('e') PARTITION BY LIST (c);",
        )
        .unwrap();
    // Row matches mp5_cd's bound but mp5_cd has no leaves: PG19
    // reports the matched child, not the parent.
    let err = run(&mut eng, "INSERT INTO mp VALUES (1, 45, 'c', 1);").unwrap_err();
    assert_eq!(err.code, "23514");
    assert!(
        err.message
            .contains("no partition of relation \"mp5_cd\" found for row"),
        "message={}",
        err.message
    );
    // Row matches no child of mp5: the parent is named.
    let err = run(&mut eng, "INSERT INTO mp VALUES (1, 45, 'f', 1);").unwrap_err();
    assert_eq!(err.code, "23514");
    assert!(
        err.message
            .contains("no partition of relation \"mp5\" found for row"),
        "message={}",
        err.message
    );
}

/// v1.17: `xmin`/`xmax` system columns — the row's MVCC version
/// header. `xmin` is the inserting transaction's id, `xmax` is 0
/// for live rows (like PG19).
#[test]
fn v117_xmin_xmax_basic() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE xt (a int);").unwrap();
    run(&mut eng, "INSERT INTO xt VALUES (1), (2);").unwrap();
    // xmin is the inserting xid (positive), xmax is 0 for live rows.
    let r = run(&mut eng, "SELECT xmin, xmax FROM xt ORDER BY a;").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 2);
    for row in &rows {
        let xmin: i64 = row[0].parse().unwrap();
        assert!(xmin > 0, "xmin={}", row[0]);
        assert_eq!(row[1], "0");
    }
    // Both rows were inserted by the same statement: same xmin.
    assert_eq!(rows[0][0], rows[1][0]);
}

/// v1.17: qualified `a.xmin = b.xmin` across a self-join — the
/// qualifier picks its own range's row (the three conformance
/// statements that motivated v1.17).
#[test]
fn v117_xmin_self_join_qualified() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE xsj (a int);").unwrap();
    run(&mut eng, "INSERT INTO xsj VALUES (6), (8), (10), (12);").unwrap();
    // All rows inserted by one statement: every pair shares xmin.
    let r = run(
        &mut eng,
        "SELECT a.xmin = b.xmin FROM xsj a, xsj b WHERE a.a=6 AND b.a=8;",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["t".to_string()]]);
    let r = run(
        &mut eng,
        "SELECT a.xmin = b.xmin FROM xsj a, xsj b WHERE a.a=10 AND b.a=10;",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["t".to_string()]]);
    // xmax is 0 for all live rows, so xmax equality holds too.
    let r = run(
        &mut eng,
        "SELECT a.xmax = b.xmax FROM xsj a, xsj b WHERE a.a=6 AND b.a=8;",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["t".to_string()]]);
}

/// v1.17: qualifiers distinguish ranges over different tables —
/// each side reports its own inserting xid. (The `run` harness uses
/// a fixed xid, so rows are built directly with distinct xmins.)
#[test]
fn v117_xmin_qualifier_picks_range() {
    use crate::storage::{Row, RowVersion, Table};
    let mut eng = engine();
    // The v112 module's engine() leaves next_xid at 1; bump it so
    // manually-created tables (created_xmin=1) are visible.
    eng.txns.next_xid = 30;
    let mut t1 = Table::new(vec![("a".to_string(), ColType::Int)], 1);
    t1.push_version(RowVersion::plain(100, Row::new(vec![Value::Int(1)]), 11));
    let mut t2 = Table::new(vec![("b".to_string(), ColType::Int)], 1);
    t2.push_version(RowVersion::plain(200, Row::new(vec![Value::Int(2)]), 22));
    eng.db.tables.insert("xq1".to_string(), vec![t1]);
    eng.db.tables.insert("xq2".to_string(), vec![t2]);
    // Different inserting xids: xmin equality is false.
    let r = run(&mut eng, "SELECT a.xmin = b.xmin FROM xq1 a, xq2 b;").unwrap();
    assert_eq!(rows_of(r), vec![vec!["f".to_string()]]);
    // Each qualifier reports its own table's xmin.
    let r = run(&mut eng, "SELECT a.xmin, b.xmin FROM xq1 a, xq2 b;").unwrap();
    assert_eq!(rows_of(r), vec![vec!["11".to_string(), "22".to_string()]]);
}

/// v1.17: unqualified `xmin` over a join is ambiguous (42702, like
/// PG19); a user column named `xmin` wins over the system column.
#[test]
fn v117_xmin_ambiguous_and_shadowed() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE xa (a int);").unwrap();
    run(&mut eng, "INSERT INTO xa VALUES (1);").unwrap();
    let err = run(&mut eng, "SELECT xmin FROM xa p, xa q;").unwrap_err();
    assert_eq!(err.code, "42702");
    // A user column named xmin shadows the system column.
    run(&mut eng, "CREATE TABLE xsh (xmin int);").unwrap();
    run(&mut eng, "INSERT INTO xsh VALUES (42);").unwrap();
    let r = run(&mut eng, "SELECT xmin FROM xsh;").unwrap();
    assert_eq!(rows_of(r), vec![vec!["42".to_string()]]);
}

/// v1.17: `xmax` reflects the deleting transaction — after DELETE,
/// the surviving rows still show xmax 0 (like PG19's live tuples).
#[test]
fn v117_xmax_after_delete() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE xd (a int);").unwrap();
    run(&mut eng, "INSERT INTO xd VALUES (1), (2);").unwrap();
    run(&mut eng, "DELETE FROM xd WHERE a = 1;").unwrap();
    let r = run(&mut eng, "SELECT a, xmax FROM xd;").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "2");
    assert_eq!(rows[0][1], "0");
}

/// v1.22: plan-time CASE constant folding — a reachable constant
/// `1/0` in a WHEN-true arm raises 22012 at plan time (PG19
/// `eval_const_expressions`), even though the row's WHEN is false.
#[test]
fn v122_case_reachable_const_div0_errors() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE ct (i int);").unwrap();
    run(&mut eng, "INSERT INTO ct VALUES (50);").unwrap();
    let e = run(
        &mut eng,
        "SELECT CASE WHEN i > 100 THEN 1/0 ELSE 0 END FROM ct;",
    )
    .unwrap_err();
    assert_eq!(e.code, "22012");
}

/// v1.22: an unreachable CASE arm (WHEN folds to FALSE) is never
/// folded — no error, ELSE result returned.
#[test]
fn v122_case_unreachable_arm_not_folded() {
    let mut eng = engine();
    let r = run(&mut eng, "SELECT CASE WHEN 1=0 THEN 1/0 ELSE 1 END;").unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "1");
}

/// v1.22: a WHEN that folds to TRUE drops later arms — their
/// constant errors are never folded.
#[test]
fn v122_case_true_when_drops_later_arms() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "SELECT CASE WHEN 1=1 THEN 7 WHEN 2=2 THEN 1/0 ELSE 1/0 END;",
    )
    .unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "7");
}

/// v1.22: grouped aggregate as a quantified comparison's left
/// operand — the aggregate folds per group, then the subquery
/// comparison runs against the value (PG19 SubPlan semantics).
/// `1 = ANY([1,2])` is true; `true = ANY(SELECT false)` is false.
#[test]
fn v122_quantified_grouped_agg_left() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE qa (f1 int);").unwrap();
    run(&mut eng, "INSERT INTO qa VALUES (1), (2);").unwrap();
    let r = run(
        &mut eng,
        "SELECT (1 = ANY(array_agg(f1))) = ANY (SELECT false) FROM qa;",
    )
    .unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "f");
}

/// v1.22: `INSERT ... RETURNING *` expands to the target table's
/// columns in order (PG19).
#[test]
fn v122_returning_star_insert() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE rs (k int, v text);").unwrap();
    let r = run(&mut eng, "INSERT INTO rs VALUES (1, 'a') RETURNING *;").unwrap();
    match r {
        ExecResult::Dml { columns, rows, .. } => {
            assert_eq!(
                columns,
                vec![
                    ("k".to_string(), ColType::Int),
                    ("v".to_string(), ColType::Text)
                ]
            );
            assert_eq!(rows.len(), 1);
            let vals: Vec<String> = rows[0].iter().map(|v| v.to_text().unwrap()).collect();
            assert_eq!(vals, vec!["1".to_string(), "a".to_string()]);
        }
        _ => panic!("expected Dml, got {:?}", r),
    }
}

/// v1.22: `UPDATE ... RETURNING *` and `DELETE ... RETURNING *`.
#[test]
fn v122_returning_star_update_delete() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE rs2 (k int, v text);").unwrap();
    run(&mut eng, "INSERT INTO rs2 VALUES (1, 'a'), (2, 'b');").unwrap();
    let r = run(&mut eng, "UPDATE rs2 SET v = 'z' WHERE k = 1 RETURNING *;").unwrap();
    match r {
        ExecResult::Dml { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let vals: Vec<String> = rows[0].iter().map(|v| v.to_text().unwrap()).collect();
            assert_eq!(vals, vec!["1".to_string(), "z".to_string()]);
        }
        _ => panic!("expected Dml"),
    }
    let r = run(&mut eng, "DELETE FROM rs2 WHERE k = 2 RETURNING *;").unwrap();
    match r {
        ExecResult::Dml { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let vals: Vec<String> = rows[0].iter().map(|v| v.to_text().unwrap()).collect();
            assert_eq!(vals, vec!["2".to_string(), "b".to_string()]);
        }
        _ => panic!("expected Dml"),
    }
}

/// v1.22: qualified `RETURNING tbl.*` expands like `*`.
#[test]
fn v122_returning_qualified_star() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE rs3 (k int, v text);").unwrap();
    let r = run(&mut eng, "INSERT INTO rs3 VALUES (3, 'c') RETURNING rs3.*;").unwrap();
    match r {
        ExecResult::Dml { rows, .. } => {
            assert_eq!(rows.len(), 1);
            let vals: Vec<String> = rows[0].iter().map(|v| v.to_text().unwrap()).collect();
            assert_eq!(vals, vec!["3".to_string(), "c".to_string()]);
        }
        _ => panic!("expected Dml"),
    }
}
