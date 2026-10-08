use super::*;
use crate::sql::parse_statement;
use crate::storage::Table;

/// v0.94: PG19 `select_common_type` — float8 is the preferred type
/// of the numeric category (pg_type.dat typispreferred;
/// numeric->float8 is implicit, float8->numeric is assignment-only),
/// so float8 beats numeric in CASE/UNION type resolution.
#[test]
fn v94_common_supertype_float8_beats_numeric() {
    let f8 = ColType::Float;
    let num = ColType::Numeric(None);
    assert_eq!(common_supertype("CASE", &f8, &num).unwrap(), f8);
    assert_eq!(common_supertype("CASE", &num, &f8).unwrap(), f8);
    assert_eq!(common_supertype("UNION", &ColType::Int, &num).unwrap(), num);
    assert_eq!(common_supertype("UNION", &ColType::Int, &f8).unwrap(), f8);
    assert_eq!(
        common_supertype("UNION", &ColType::Float4, &num).unwrap(),
        num
    );
    assert_eq!(
        common_supertype("UNION", &ColType::Float4, &f8).unwrap(),
        f8
    );
}

/// Engine with two small tables, committed (next_xid past them).
fn engine() -> Engine {
    let mut eng = Engine::new();
    // users(id int, name text): (1,'ann'), (2,'bob'), (3,'cid')
    let mut users = Table::new(
        vec![
            ("id".to_string(), ColType::Int),
            ("name".to_string(), ColType::Text),
        ],
        1,
    );
    for (i, id, name) in [(1, 1, "ann"), (2, 2, "bob"), (3, 3, "cid")] {
        users.push_version(RowVersion::plain(
            i,
            Row::new(vec![Value::Int(id), Value::text(name)]),
            1,
        ));
    }
    // orders(id int, uid int, amt int): (1,1,10), (2,1,20), (3,2,5)
    let mut orders = Table::new(
        vec![
            ("id".to_string(), ColType::Int),
            ("uid".to_string(), ColType::Int),
            ("amt".to_string(), ColType::Int),
        ],
        1,
    );
    for (i, id, uid, amt) in [(4, 1, 1, 10), (5, 2, 1, 20), (6, 3, 2, 5)] {
        orders.push_version(RowVersion::plain(
            i,
            Row::new(vec![Value::Int(id), Value::Int(uid), Value::Int(amt)]),
            1,
        ));
    }
    eng.db.tables.insert("users".to_string(), vec![users]);
    eng.db.tables.insert("orders".to_string(), vec![orders]);
    eng.txns.next_xid = 10;
    eng.txns.next_row_id = 7;
    eng
}

/// Parse + execute a statement as one autocommit-ish step (own xid 9,
/// fresh snapshot). Returns the rows as debug strings.
fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    // v0.79: preserve the parser's SQLSTATE (e.g. 54000 for >6 array
    // dimensions) instead of flattening every parse error to 42601.
    let stmt = parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
    run_stmt(eng, &stmt)
}

/// v0.77: execute an already-parsed (and possibly parameter-bound)
/// statement, sharing `run`'s harness.
fn run_stmt(eng: &mut Engine, stmt: &Stmt) -> Result<ExecResult, ExecError> {
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
    execute(eng, &mut ctx, stmt)
}

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } | ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        ExecResult::Command { tag } => vec![vec![tag]],
        ExecResult::Dml { tag, .. } => vec![vec![tag]],
    }
}

/// v0.60: PG19 numeric(p,s) assignment typmod (round to scale,
/// then 22003 on overflow), grounded in the bundled PG19
/// numeric.out `fract_only` / `num_typemod_test` cases.
#[test]
fn v60_numeric_typmod_assignment() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE fract_only(x numeric(4,4))").unwrap();
    run(&mut eng, "INSERT INTO fract_only VALUES (0.99994)").unwrap();
    run(&mut eng, "INSERT INTO fract_only VALUES (0.00017)").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT x FROM fract_only").unwrap()),
        vec![vec!["0.9999".to_string()], vec!["0.0002".to_string()]]
    );
    let e = run(&mut eng, "INSERT INTO fract_only VALUES (1.0)").unwrap_err();
    assert_eq!(e.code, "22003");
    let e = run(&mut eng, "INSERT INTO fract_only VALUES (0.99995)").unwrap_err();
    assert_eq!(e.code, "22003");

    run(&mut eng, "CREATE TABLE neg_scale(x numeric(3,-6))").unwrap();
    run(&mut eng, "INSERT INTO neg_scale VALUES (123456)").unwrap();
    run(&mut eng, "INSERT INTO neg_scale VALUES (654321)").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT x FROM neg_scale").unwrap()),
        vec![vec!["0".to_string()], vec!["1000000".to_string()]]
    );
    let e = run(&mut eng, "INSERT INTO neg_scale VALUES (999500000)").unwrap_err();
    assert_eq!(e.code, "22003");

    // CAST applies the typmod too.
    let r = run(&mut eng, "SELECT CAST(0.99994 AS numeric(4,4))").unwrap();
    assert_eq!(rows_of(r), vec![vec!["0.9999".to_string()]]);
    let e = run(&mut eng, "SELECT CAST(2.0 AS numeric(4,4))").unwrap_err();
    assert_eq!(e.code, "22003");

    // Invalid typmods are rejected at parse time (the test harness
    // remaps all parse errors to 42601, so check the message).
    let e = run(&mut eng, "CREATE TABLE bad(x numeric(0,1))").unwrap_err();
    assert!(e.message.contains("precision"), "got: {}", e.message);
}

/// A sequential scan must return the stored row. It must not return
/// a copy. See issue #8.
#[test]
fn seq_scan_shares_table_row_storage() {
    let mut eng = engine();
    let stored = eng.db.tables["users"][0].rows[0].values.as_ptr();
    let out = run(&mut eng, "SELECT * FROM users").unwrap();
    let ExecResult::Select { rows, .. } = out else {
        panic!("SELECT returns rows");
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].as_ptr(),
        stored,
        "SELECT * must share the stored row, not copy it"
    );
}

/// v0.66: ALTER SEQUENCE RESTART is setval(r, false) — the next
/// nextval RETURNS r (PG19 ALTER SEQUENCE docs: "equivalent to
/// calling the setval function with is_called = false"), not
/// r + increment.
#[test]
fn v66_alter_sequence_restart_returns_restart_value() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s").unwrap();
    let r = run(&mut eng, "SELECT nextval('s')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
    run(&mut eng, "ALTER SEQUENCE s RESTART WITH 100").unwrap();
    let r = run(&mut eng, "SELECT nextval('s')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["100".to_string()]]);
    let r = run(&mut eng, "SELECT nextval('s')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["101".to_string()]]);
    // Bare RESTART resets to the recorded start value (1).
    run(&mut eng, "ALTER SEQUENCE s RESTART").unwrap();
    let r = run(&mut eng, "SELECT nextval('s')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
}

/// An index scan must share the stored row, like a sequential scan.
#[test]
fn index_scan_shares_table_row_storage() {
    let mut eng = engine();
    run(&mut eng, "CREATE INDEX users_id_ix ON users(id)").unwrap();
    // v1.62: PG19's cost model never index-scans a single-page
    // relation, so seed past one heap page to keep the index scan
    // (the row-sharing assertion is about the executor, not the
    // planner's scan choice).
    for i in 4..=500 {
        run(&mut eng, &format!("INSERT INTO users VALUES ({i}, 'n{i}')")).unwrap();
    }
    let plan = rows_of(run(&mut eng, "EXPLAIN SELECT * FROM users WHERE id = 2").unwrap());
    let plan = plan.concat().join(" ");
    assert!(
        plan.contains("Index"),
        "query must plan an index scan: {}",
        plan
    );
    let stored = eng.db.tables["users"][0].rows[1].values.as_ptr();
    let out = run(&mut eng, "SELECT * FROM users WHERE id = 2").unwrap();
    let ExecResult::Select { rows, .. } = out else {
        panic!("SELECT returns rows");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].as_ptr(),
        stored,
        "an index scan must share the stored row"
    );
}

/// A scanned row must share the text bytes of the table row.
#[test]
fn seq_scan_shares_table_text_bytes() {
    let mut eng = engine();
    let stored = match &eng.db.tables["users"][0].rows[0].values[1] {
        Value::Text(s) => s.as_ptr(),
        v => panic!("column 1 is text, got {:?}", v),
    };
    let out = run(&mut eng, "SELECT * FROM users").unwrap();
    let ExecResult::Select { rows, .. } = out else {
        panic!("SELECT returns rows");
    };
    match &rows[0][1] {
        Value::Text(s) => assert_eq!(s.as_ptr(), stored, "text must be shared"),
        v => panic!("column 1 is text, got {:?}", v),
    }
}

/// A shared row does not change. An UPDATE must not change the rows
/// that an earlier SELECT returned.
#[test]
fn shared_rows_are_immutable_snapshots() {
    let mut eng = engine();
    let out = run(&mut eng, "SELECT * FROM users WHERE id = 1").unwrap();
    let ExecResult::Select { rows, .. } = out else {
        panic!("SELECT returns rows");
    };
    run(&mut eng, "UPDATE users SET name = 'zed' WHERE id = 1").unwrap();
    assert_eq!(rows[0][1].to_text().unwrap(), "ann");
    let after = rows_of(run(&mut eng, "SELECT name FROM users WHERE id = 1").unwrap());
    assert_eq!(after, vec![vec!["zed".to_string()]]);
}

#[test]
fn inner_join_on_equality() {
    let mut eng = engine();
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT users.name, orders.amt FROM users JOIN orders ON users.id = orders.uid ORDER BY orders.amt",
            )
            .unwrap(),
        );
    assert_eq!(
        rows,
        vec![
            vec!["bob".to_string(), "5".to_string()],
            vec!["ann".to_string(), "10".to_string()],
            vec!["ann".to_string(), "20".to_string()],
        ]
    );
}

#[test]
fn left_join_keeps_unmatched() {
    let mut eng = engine();
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT users.name, orders.amt FROM users LEFT JOIN orders ON users.id = orders.uid ORDER BY users.id, orders.amt",
            )
            .unwrap(),
        );
    assert_eq!(
        rows,
        vec![
            vec!["ann".to_string(), "10".to_string()],
            vec!["ann".to_string(), "20".to_string()],
            vec!["bob".to_string(), "5".to_string()],
            vec!["cid".to_string(), "NULL".to_string()],
        ]
    );
}

#[test]
fn ambiguous_column_is_42702() {
    let mut eng = engine();
    let e = run(
        &mut eng,
        "SELECT id FROM users JOIN orders ON users.id = orders.uid",
    )
    .unwrap_err();
    assert_eq!(e.code, "42702");
}

#[test]
fn aggregates_and_group_by() {
    let mut eng = engine();
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT uid, count(*), sum(amt), avg(amt), min(amt), max(amt) FROM orders GROUP BY uid ORDER BY uid",
            )
            .unwrap(),
        );
    assert_eq!(
        rows,
        vec![
            vec!["1", "2", "30", "15", "10", "20"],
            vec!["2", "1", "5", "5", "5", "5"],
        ]
        .into_iter()
        .map(|r: Vec<&str>| r.into_iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .collect::<Vec<_>>()
    );
}

#[test]
fn having_filters_groups() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT uid, count(*) FROM orders GROUP BY uid HAVING count(*) > 1",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["1".to_string(), "2".to_string()]]);
}

#[test]
fn scalar_subquery_in_select() {
    let mut eng = engine();
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT name, (SELECT count(*) FROM orders WHERE orders.uid = users.id) FROM users ORDER BY id",
            )
            .unwrap(),
        );
    assert_eq!(
        rows,
        vec![
            vec!["ann".to_string(), "2".to_string()],
            vec!["bob".to_string(), "1".to_string()],
            vec!["cid".to_string(), "0".to_string()],
        ]
    );
}

/// v0.99: a FROM-subquery inside a scalar subquery resolves
/// correlated references against the enclosing query's scopes
/// (PG19), while same-level FROM siblings stay invisible without
/// LATERAL.
#[test]
fn v99_derived_table_sees_enclosing_scope_not_sibling() {
    let mut eng = engine();
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT name, (SELECT r FROM (SELECT id AS q2) x, (SELECT id AS r) y) FROM users ORDER BY id",
            )
            .unwrap(),
        );
    assert_eq!(
        rows,
        vec![
            vec!["ann".to_string(), "1".to_string()],
            vec!["bob".to_string(), "2".to_string()],
            vec!["cid".to_string(), "3".to_string()],
        ]
    );
    // Same-level sibling reference without LATERAL is 42703.
    let err = run(
        &mut eng,
        "SELECT * FROM (SELECT 1 AS a) x, (SELECT a AS b) y",
    )
    .unwrap_err();
    assert_eq!(err.code, "42703");
}

#[test]
fn in_subquery_and_exists() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT name FROM users WHERE id IN (SELECT uid FROM orders) ORDER BY id",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["ann".to_string()], vec!["bob".to_string()]]);
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.uid = u.id AND o.amt > 15)",
            )
            .unwrap(),
        );
    assert_eq!(rows, vec![vec!["ann".to_string()]]);
}

#[test]
fn derived_table_in_from() {
    let mut eng = engine();
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT t.uid, t.total FROM (SELECT uid, sum(amt) AS total FROM orders GROUP BY uid) t WHERE t.total > 10 ORDER BY t.uid",
            )
            .unwrap(),
        );
    assert_eq!(rows, vec![vec!["1".to_string(), "30".to_string()]]);
}

#[test]
fn distinct_and_offset() {
    let mut eng = engine();
    let rows = rows_of(run(&mut eng, "SELECT DISTINCT uid FROM orders ORDER BY uid").unwrap());
    assert_eq!(rows, vec![vec!["1".to_string()], vec!["2".to_string()]]);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT id FROM orders ORDER BY id LIMIT 2 OFFSET 1",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["2".to_string()], vec!["3".to_string()]]);
}

/// v0.48: `IS [NOT] DISTINCT FROM` — the NULL-safe comparison
/// (PG19 gram.y). The corpus block from PG19's `select_distinct`
/// regression test, adapted to the in-memory harness.
#[test]
fn is_distinct_from_semantics() {
    let mut eng = engine();
    let mut q = |e: &str| rows_of(run(&mut eng, e).unwrap());
    assert_eq!(
        q("SELECT 1 IS DISTINCT FROM 2"),
        vec![vec!["t".to_string()]]
    );
    assert_eq!(
        q("SELECT 2 IS DISTINCT FROM 2"),
        vec![vec!["f".to_string()]]
    );
    assert_eq!(
        q("SELECT 2 IS DISTINCT FROM null"),
        vec![vec!["t".to_string()]]
    );
    assert_eq!(
        q("SELECT null IS DISTINCT FROM null"),
        vec![vec!["f".to_string()]]
    );
    assert_eq!(
        q("SELECT 1 IS NOT DISTINCT FROM 2"),
        vec![vec!["f".to_string()]]
    );
    assert_eq!(
        q("SELECT 2 IS NOT DISTINCT FROM 2"),
        vec![vec!["t".to_string()]]
    );
    assert_eq!(
        q("SELECT 2 IS NOT DISTINCT FROM null"),
        vec![vec!["f".to_string()]]
    );
    assert_eq!(
        q("SELECT null IS NOT DISTINCT FROM null"),
        vec![vec!["t".to_string()]]
    );
    // Cross-type and never-unknown: the result is always t/f,
    // even when an operand is NULL.
    assert_eq!(
        q("SELECT 1 IS DISTINCT FROM 1.0"),
        vec![vec!["f".to_string()]]
    );
    assert_eq!(
        q("SELECT 'a' IS DISTINCT FROM 'b'"),
        vec![vec!["t".to_string()]]
    );
    // Never unknown: the result is always t/f, even with NULL
    // operands.
    assert_eq!(
        q("SELECT NULL IS DISTINCT FROM 1"),
        vec![vec!["t".to_string()]]
    );
}

/// v0.48: duplicate NULLs collapse in `IS DISTINCT FROM`, and NaN
/// compares equal to NaN (unlike `=`).
#[test]
fn is_distinct_from_null_and_nan() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT id FROM users WHERE id IS DISTINCT FROM NULL",
        )
        .unwrap(),
    );
    // NULLs are not distinct from NULL: all three non-null rows.
    assert_eq!(rows.len(), 3);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT 'nan'::numeric IS DISTINCT FROM 'nan'::numeric",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["f".to_string()]]);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT 'nan'::float8 IS DISTINCT FROM 'nan'::float8",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["f".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT 'nan'::numeric = 'nan'::numeric").unwrap());
    // v0.94: PG19 cmp_numerics considers all NaNs equal, so `=`
    // is TRUE (was "f" under the old, incorrect NaN != NaN rule).
    assert_eq!(rows, vec![vec!["t".to_string()]]);
}

/// v0.48: `IS DISTINCT FROM` works in WHERE and in the grouped
/// (HAVING) path, sharing one value-level implementation.
#[test]
fn is_distinct_from_in_where_and_grouped() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT id FROM users WHERE id IS DISTINCT FROM 2 ORDER BY id",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["1".to_string()], vec!["3".to_string()]]);
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT uid, sum(amt) AS total FROM orders GROUP BY uid HAVING sum(amt) IS DISTINCT FROM 30 ORDER BY uid",
            )
            .unwrap(),
        );
    assert_eq!(rows, vec![vec!["2".to_string(), "5".to_string()]]);
}

/// v0.48: single-expression SELECT DISTINCT dedups after projection.
#[test]
fn select_distinct_single_expression() {
    let mut eng = engine();
    let rows = rows_of(run(&mut eng, "SELECT DISTINCT uid FROM orders ORDER BY uid").unwrap());
    assert_eq!(rows, vec![vec!["1".to_string()], vec!["2".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT DISTINCT amt % 10 FROM orders ORDER BY 1").unwrap());
    assert_eq!(rows, vec![vec!["0".to_string()], vec!["5".to_string()]]);
}

/// v0.48: SELECT DISTINCT dedups on the full projected row.
#[test]
fn select_distinct_full_row() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT uid, amt FROM orders ORDER BY uid, amt",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["1".to_string(), "20".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
}

/// v0.48: duplicate NULL rows collapse under DISTINCT.
#[test]
fn select_distinct_null_collapse() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE dnull (a int, b int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO dnull VALUES (1, NULL), (NULL, NULL), (1, NULL), (NULL, NULL), (2, 3)",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT a, b FROM dnull ORDER BY a NULLS LAST, b NULLS LAST",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "NULL".to_string()],
            vec!["2".to_string(), "3".to_string()],
            vec!["NULL".to_string(), "NULL".to_string()],
        ]
    );
}

/// v0.48: DISTINCT applies before ORDER BY and LIMIT.
#[test]
fn select_distinct_before_order_limit() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT uid FROM orders ORDER BY uid DESC LIMIT 1",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["2".to_string()]]);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT uid FROM orders ORDER BY uid LIMIT 1 OFFSET 1",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["2".to_string()]]);
}

/// v0.48: CREATE TABLE AS SELECT (CTAS) — small versions of the
/// v0.48 target queries, with table row counts checked.
#[test]
fn ctas_distinct_group_counts() {
    let mut eng = engine();
    let tag = rows_of(
            run(
                &mut eng,
                "CREATE TABLE distinct_group_1 AS SELECT DISTINCT g % 100 AS grp FROM generate_series(0, 999) g",
            )
            .unwrap(),
        );
    assert_eq!(tag, vec![vec!["SELECT 100".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT count(*) FROM distinct_group_1").unwrap());
    assert_eq!(rows, vec![vec!["100".to_string()]]);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT (g % 100)::text FROM generate_series(0, 999) g",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 100);
    let tag = rows_of(
            run(
                &mut eng,
                "CREATE TABLE distinct_hash_text AS SELECT DISTINCT (g % 100)::text AS h FROM generate_series(0, 999) g",
            )
            .unwrap(),
        );
    assert_eq!(tag, vec![vec!["SELECT 100".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT count(*) FROM distinct_hash_text").unwrap());
    assert_eq!(rows, vec![vec!["100".to_string()]]);
}

#[test]
fn for_update_locks_and_conflicts() {
    let mut eng = engine();
    // Real transaction xids (registered in txns.active).
    let x1 = eng.begin_txn();
    let x2 = eng.begin_txn();
    // x1 locks users/1 via FOR UPDATE.
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        snap: &snap,
        own: x1,
        write_xid: x1,
        all_xids: vec![x1],
        level: IsolationLevel::ReadCommitted,
        writes: &mut writes,
        session: 0,
        role: "postgres",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    let sel = parse_statement("SELECT * FROM users WHERE id = 1 FOR UPDATE").unwrap();
    execute(&mut eng, &mut ctx, &sel).unwrap();
    assert_eq!(eng.row_lock_holder(1), Some(x1));
    // x2 writing that row gets 40001 (fail fast, no waiting).
    let snap2 = eng.take_snapshot();
    let mut writes2 = Vec::new();
    let mut ctx2 = StmtCtx {
        snap: &snap2,
        own: x2,
        write_xid: x2,
        all_xids: vec![x2],
        level: IsolationLevel::ReadCommitted,
        writes: &mut writes2,
        session: 0,
        role: "postgres",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    let upd = parse_statement("UPDATE users SET name = 'x' WHERE id = 1").unwrap();
    let e = execute(&mut eng, &mut ctx2, &upd).unwrap_err();
    assert_eq!(e.code, "40001");
    // x2 can still write a different, unlocked row.
    let upd2 = parse_statement("UPDATE users SET name = 'y' WHERE id = 2").unwrap();
    execute(&mut eng, &mut ctx2, &upd2).unwrap();
    // Locks release on transaction end.
    eng.release_txn_locks(x1);
    eng.end_txn(x1);
    assert_eq!(eng.row_lock_holder(1), None);
    // ...and now x2's write to row 1 succeeds.
    let upd3 = parse_statement("UPDATE users SET name = 'z' WHERE id = 1").unwrap();
    execute(&mut eng, &mut ctx2, &upd3).unwrap();
    eng.end_txn(x2);
}

#[test]
fn null_semantics_in_predicates() {
    let mut eng = engine();
    // cid has no orders: LEFT JOIN gives NULL amt; NULL comparisons
    // are not true.
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT users.name FROM users LEFT JOIN orders ON users.id = orders.uid WHERE orders.amt > 100 OR orders.amt IS NULL ORDER BY users.id",
            )
            .unwrap(),
        );
    assert_eq!(rows, vec![vec!["cid".to_string()]]);
}

#[test]
fn update_still_works() {
    let mut eng = engine();
    match run(&mut eng, "UPDATE users SET name = 'ann2' WHERE id = 1").unwrap() {
        ExecResult::Command { tag } => assert_eq!(tag, "UPDATE 1"),
        ExecResult::Dml { tag, .. } => assert_eq!(tag, "UPDATE 1"),
        _ => panic!("expected command"),
    }
    let rows = rows_of(run(&mut eng, "SELECT name FROM users WHERE id = 1").unwrap());
    assert_eq!(rows, vec![vec!["ann2".to_string()]]);
}

// v0.16: new string/numeric built-ins.
#[test]
fn v16_string_and_numeric_functions() {
    let mut eng = engine();
    let one =
        |eng: &mut Engine, sql: &str| -> String { rows_of(run(eng, sql).unwrap())[0][0].clone() };
    // substr: 1-based, Unicode-aware.
    assert_eq!(one(&mut eng, "SELECT substr('hello', 2, 3)"), "ell");
    assert_eq!(one(&mut eng, "SELECT substr('héllo', 2, 3)"), "éll");
    assert_eq!(one(&mut eng, "SELECT substr('hello', -2, 4)"), "h");
    assert_eq!(one(&mut eng, "SELECT substr('hello', 99)"), "");
    // concat / concat_ws: NULL handling.
    assert_eq!(one(&mut eng, "SELECT concat('a', NULL, 1, true)"), "a1t");
    assert_eq!(
        one(&mut eng, "SELECT concat_ws(',', 'a', NULL, 'b')"),
        "a,b"
    );
    assert_eq!(one(&mut eng, "SELECT concat_ws(',', 'a', 'b')"), "a,b");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT concat_ws(NULL, 'a')").unwrap())[0][0],
        "NULL"
    );
    // to_hex / to_oct / to_bin: int4 width vs bigint width (PG parity).
    assert_eq!(one(&mut eng, "SELECT to_hex(-1234)"), "fffffb2e");
    assert_eq!(one(&mut eng, "SELECT to_hex(255)"), "ff");
    assert_eq!(one(&mut eng, "SELECT to_oct(-1234)"), "37777775456");
    assert_eq!(
        one(&mut eng, "SELECT to_bin(-1234)"),
        "11111111111111111111101100101110"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_hex(-1234::bigint)"),
        "fffffffffffffb2e"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_oct(-1234::bigint)"),
        "1777777777777777775456"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_bin(-1234::bigint)"),
        "1111111111111111111111111111111111111111111111111111101100101110"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_bin(9223372036854775807::bigint)"),
        "111111111111111111111111111111111111111111111111111111111111111"
    );
    // sign across int / numeric / float.
    assert_eq!(one(&mut eng, "SELECT sign(-5)"), "-1");
    assert_eq!(one(&mut eng, "SELECT sign(0)"), "0");
    assert_eq!(one(&mut eng, "SELECT sign(2.5)"), "1");
    assert_eq!(one(&mut eng, "SELECT sign(-2.5::float8)"), "-1");
    assert_eq!(one(&mut eng, "SELECT sign(0.0::numeric)"), "0");
    // left / right with negative and oversized counts.
    assert_eq!(one(&mut eng, "SELECT left('abcdef', 2)"), "ab");
    assert_eq!(one(&mut eng, "SELECT left('abcdef', -2)"), "abcd");
    assert_eq!(one(&mut eng, "SELECT left('abcdef', 99)"), "abcdef");
    assert_eq!(one(&mut eng, "SELECT right('abcdef', 2)"), "ef");
    assert_eq!(one(&mut eng, "SELECT right('abcdef', -2)"), "cdef");
    assert_eq!(one(&mut eng, "SELECT left('héllo', 2)"), "hé");
    // reverse, Unicode-aware.
    assert_eq!(one(&mut eng, "SELECT reverse('abc')"), "cba");
    assert_eq!(one(&mut eng, "SELECT reverse('héllo')"), "olléh");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT reverse(NULL)").unwrap())[0][0],
        "NULL"
    );
}

// v0.17: date/time built-in batch.
#[test]
fn v17_datetime_functions() {
    let mut eng = engine();
    let one =
        |eng: &mut Engine, sql: &str| -> String { rows_of(run(eng, sql).unwrap())[0][0].clone() };
    let err_code =
        |eng: &mut Engine, sql: &str| -> &'static str { run(eng, sql).unwrap_err().code };
    // date_part: function form of extract, numeric result.
    assert_eq!(
        one(&mut eng, "SELECT date_part('year', DATE '2026-09-11')"),
        "2026"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT date_part('month', TIMESTAMP '2026-09-11 13:25:01')"
        ),
        "9"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT date_part('hour', TIMESTAMPTZ '2026-09-11 13:25:01+00')"
        ),
        "13"
    );
    assert_eq!(
        one(&mut eng, "SELECT date_part('dow', DATE '2026-09-11')"),
        "5"
    ); // Friday
    assert_eq!(
        one(&mut eng, "SELECT date_part('quarter', DATE '2026-09-11')"),
        "3"
    );
    assert_eq!(
        one(&mut eng, "SELECT date_part('epoch', DATE '1970-01-02')"),
        "86400"
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT date_part('year', NULL::date)").unwrap())[0][0],
        "NULL"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT date_part('bogus', DATE '2026-09-11')"),
        "22023"
    );
    assert_eq!(err_code(&mut eng, "SELECT date_part('year', 42)"), "42883");
    assert_eq!(err_code(&mut eng, "SELECT date_part('year')"), "42883");
    // to_date with format strings.
    assert_eq!(
        one(&mut eng, "SELECT to_date('2026-01-15', 'YYYY-MM-DD')"),
        "2026-01-15"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_date('15/01/2026', 'DD/MM/YYYY')"),
        "2026-01-15"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_date('09-11-26', 'MM-DD-YY')"),
        "2026-09-11"
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT to_date(NULL, 'YYYY-MM-DD')").unwrap())[0][0],
        "NULL"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT to_date('2026-13-01', 'YYYY-MM-DD')"),
        "22008"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT to_date('2026-02-30', 'YYYY-MM-DD')"),
        "22008"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT to_date('2026/01/15', 'YYYY-MM-DD')"),
        "22008"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT to_date('2026-01-15', 'YYYY-MM-QQ')"),
        "0A000"
    );
    // to_timestamp with format strings.
    assert_eq!(
        one(
            &mut eng,
            "SELECT to_timestamp('2026-09-11 13:25:01', 'YYYY-MM-DD HH24:MI:SS')"
        ),
        "2026-09-11 13:25:01"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT to_timestamp('2026-09-11 01:25 PM', 'YYYY-MM-DD HH12:MI AM')"
        ),
        "2026-09-11 13:25:00"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_timestamp('2026-09-11', 'YYYY-MM-DD')"),
        "2026-09-11 00:00:00"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT to_timestamp('2026-09-11 25:00', 'YYYY-MM-DD HH24:MI')"
        ),
        "22008"
    );
    // to_timestamp(float8): epoch seconds -> timestamptz.
    assert_eq!(
        one(&mut eng, "SELECT to_timestamp(0)"),
        "1970-01-01 00:00:00+00"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_timestamp(86400.5)"),
        "1970-01-02 00:00:00.5+00"
    );
    assert_eq!(
        one(&mut eng, "SELECT to_timestamp(-1)"),
        "1969-12-31 23:59:59+00"
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT to_timestamp(NULL::float8)").unwrap())[0][0],
        "NULL"
    );
    assert_eq!(err_code(&mut eng, "SELECT to_timestamp('x')"), "42883");
    // to_char formatting.
    assert_eq!(
        one(&mut eng, "SELECT to_char(DATE '2026-09-11', 'YYYY/MM/DD')"),
        "2026/09/11"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT to_char(TIMESTAMP '2026-09-11 13:25:01', 'DD-MM-YYYY HH24:MI:SS')"
        ),
        "11-09-2026 13:25:01"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT to_char(TIMESTAMP '2026-09-11 13:25:01', 'HH12:MI AM')"
        ),
        "01:25 PM"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT to_char(TIMESTAMPTZ '2026-09-11 13:25:01+00', 'YYYY-MM-DD')"
        ),
        "2026-09-11"
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT to_char(NULL::date, 'YYYY')").unwrap())[0][0],
        "NULL"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT to_char(DATE '2026-09-11', 'QQ')"),
        "0A000"
    );
    // v0.42: PG19 supports to_char() on int4/int8/numeric (formatting.c
    // lists Timestamp, Numeric, int4, int8, float4, float8), and
    // non-pattern characters are copied literally to the output — so
    // to_char(42, 'YYYY') is 'YYYY', not a 42883.
    assert_eq!(one(&mut eng, "SELECT to_char(42, 'YYYY')"), "YYYY");
    // PG always reserves the sign position: '  42', not ' 42'.
    assert_eq!(one(&mut eng, "SELECT to_char(42, '999')"), "  42");
    // make_date / make_timestamp.
    assert_eq!(one(&mut eng, "SELECT make_date(2026, 9, 11)"), "2026-09-11");
    assert_eq!(one(&mut eng, "SELECT make_date(2024, 2, 29)"), "2024-02-29");
    assert_eq!(err_code(&mut eng, "SELECT make_date(2026, 13, 1)"), "22008");
    assert_eq!(err_code(&mut eng, "SELECT make_date(2026, 2, 30)"), "22008");
    assert_eq!(err_code(&mut eng, "SELECT make_date(2023, 2, 29)"), "22008");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT make_date(2026, NULL, 1)").unwrap())[0][0],
        "NULL"
    );
    assert_eq!(
        one(&mut eng, "SELECT make_timestamp(2026, 9, 11, 13, 25, 1.5)"),
        "2026-09-11 13:25:01.5"
    );
    assert_eq!(
        one(&mut eng, "SELECT make_timestamp(2026, 1, 1, 0, 0, 0)"),
        "2026-01-01 00:00:00"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT make_timestamp(2026, 1, 1, 24, 0, 0)"),
        "22008"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT make_timestamp(2026, 1, 1, 0, 0, 60)"),
        "22008"
    );
    assert_eq!(err_code(&mut eng, "SELECT make_time(1, 2, 3)"), "42883");
    // timezone(): UTC-only.
    assert_eq!(
        one(
            &mut eng,
            "SELECT timezone('UTC', TIMESTAMPTZ '2026-09-11 13:25:01+00')"
        ),
        "2026-09-11 13:25:01"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT timezone('utc', TIMESTAMP '2026-09-11 13:25:01')"
        ),
        "2026-09-11 13:25:01+00"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT timezone('America/Chicago', TIMESTAMPTZ '2026-09-11 13:25:01+00')"
        ),
        "0A000"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT timezone('UTC', DATE '2026-09-11')"),
        "42883"
    );
    // clock/statement/transaction timestamps return timestamptz now.
    let ts = one(&mut eng, "SELECT clock_timestamp()");
    assert!(ts.len() >= 19, "clock_timestamp() returned {ts:?}");
    let ts2 = one(&mut eng, "SELECT statement_timestamp()");
    assert!(ts2.len() >= 19);
    let ts3 = one(&mut eng, "SELECT transaction_timestamp()");
    assert!(ts3.len() >= 19);
    // v0.76: only CURRENT_TIMESTAMP takes the optional precision in
    // PG19 — now()/clock_timestamp()/statement_timestamp()/
    // transaction_timestamp() are 0-argument functions (42883 at the
    // parse layer; the test harness flattens parse codes to 42601).
    let ts_prec = one(&mut eng, "SELECT current_timestamp(3)");
    assert!(ts_prec.len() >= 19);
    for q in [
        "SELECT now(3)",
        "SELECT clock_timestamp(1)",
        "SELECT statement_timestamp(0)",
        "SELECT transaction_timestamp(6)",
    ] {
        assert!(run(&mut eng, q).is_err(), "{}", q);
        let perr = parse_statement(q).unwrap_err();
        assert_eq!(perr.code, "42883", "{}", q);
    }
    // Deliberately unimplemented: 42883.
    assert_eq!(err_code(&mut eng, "SELECT age(now())"), "42883");
    assert_eq!(err_code(&mut eng, "SELECT make_interval(1)"), "42883");
    assert_eq!(
        err_code(&mut eng, "SELECT date_bin('1 day', now(), now())"),
        "42883"
    );
    assert_eq!(err_code(&mut eng, "SELECT justify_days(now())"), "42883");
}

// v0.58: PG19 binary float I/O — float4send / float4recv / float8recv.
// Byte-exact against PG 19's float4 regression expectations.
#[test]
fn v58_float_send_recv() {
    let mut eng = engine();
    let one =
        |eng: &mut Engine, sql: &str| -> String { rows_of(run(eng, sql).unwrap())[0][0].clone() };
    let err_code =
        |eng: &mut Engine, sql: &str| -> &'static str { run(eng, sql).unwrap_err().code };
    // PG19 float4 regression-exact send bytes (big-endian IEEE-754).
    assert_eq!(
        one(&mut eng, "SELECT float4send('5e-20'::float4)"),
        "\\x1f6c1e4a"
    );
    // Double-rounding trap: naive f64-then-narrow gives 0x15ae43fe.
    assert_eq!(
        one(&mut eng, "SELECT float4send('7038531e-32'::float4)"),
        "\\x15ae43fd"
    );
    assert_eq!(
        one(&mut eng, "SELECT float4send('1.17549435e-38'::float4)"),
        "\\x00800000"
    );
    assert_eq!(
        one(&mut eng, "SELECT float4send('nan'::float4)"),
        "\\x7fc00000"
    );
    assert_eq!(
        one(&mut eng, "SELECT float4send('inf'::float4)"),
        "\\x7f800000"
    );
    assert_eq!(
        one(&mut eng, "SELECT float4send('-inf'::float4)"),
        "\\xff800000"
    );
    assert_eq!(
        one(&mut eng, "SELECT float4send('0'::float4)"),
        "\\x00000000"
    );
    // Strict NULL through the math-function path.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT float4send(NULL::float4)").unwrap())[0][0],
        "NULL"
    );
    // Non-float4 inputs coerce like PG's implicit float8 -> float4.
    assert_eq!(
        one(&mut eng, "SELECT float4send('2.5'::float8)"),
        "\\x40200000"
    );
    // Round trips.
    assert_eq!(
        one(
            &mut eng,
            "SELECT float4recv(float4send('3.14'::float4)) = '3.14'::float4"
        ),
        "t"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT float8recv(float8send('2.5'::float8)) = '2.5'::float8"
        ),
        "t"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT float4recv('\\x3f800000'::bytea) = '1'::float4"
        ),
        "t"
    );
    // Short input: PG's 22P03 "insufficient data left in message".
    assert_eq!(
        err_code(&mut eng, "SELECT float4recv('\\x0102'::bytea)"),
        "22P03"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT float8recv('\\x01020304050607'::bytea)"),
        "22P03"
    );
    // Wrong arity / non-bytea input.
    assert_eq!(
        err_code(&mut eng, "SELECT float4send('1'::float4, '2'::float4)"),
        "42883"
    );
    assert_eq!(err_code(&mut eng, "SELECT float4recv(123)"), "42883");
}

// v0.53: unary minus as first-class Expr::Neg (PG19 doNegate, UMINUS
// precedence) + generic prefix operators (~, @, |/, ||/) at PG19's
// loosest precedence (qual_Op a_expr %prec Op).
#[test]
fn v53_unary_minus_prefix_precedence() {
    let mut eng = engine();
    let one =
        |eng: &mut Engine, sql: &str| -> String { rows_of(run(eng, sql).unwrap())[0][0].clone() };
    let err_code =
        |eng: &mut Engine, sql: &str| -> &'static str { run(eng, sql).unwrap_err().code };
    // Prefix operators bind loosest: operand is a full expression.
    assert_eq!(one(&mut eng, "SELECT ~ 1 + 1"), "-3"); // ~(1+1)
    assert_eq!(one(&mut eng, "SELECT @ 5 - 10"), "5"); // @(5-10)
    assert_eq!(one(&mut eng, "SELECT |/ 2 + 7"), "3.000000000000000"); // |/(2+7); v0.62: PG19 sqrt dscale 15
    assert_eq!(one(&mut eng, "SELECT ||/ 35 - 8"), "3"); // ||/(35-8); v0.62 fix: PG has no numeric cbrt (float8-only), so the formatting scale is stripped
    assert_eq!(one(&mut eng, "SELECT ~ 5::int2"), "-6"); // ~(5::int2)
    assert_eq!(one(&mut eng, "SELECT ~ 7::bigint"), "-8");
    assert_eq!(one(&mut eng, "SELECT ~ NULL"), "NULL");
    // UMINUS: tighter than ^, looser than ::.
    // v0.61: PG's power_var_int gives (-2)^2 rscale 16.
    assert_eq!(one(&mut eng, "SELECT - 2 ^ 2"), "4.0000000000000000"); // (-2)^2
    assert_eq!(one(&mut eng, "SELECT - -5"), "5");
    assert_eq!(one(&mut eng, "SELECT -(3+4)"), "-7");
    assert_eq!(one(&mut eng, "SELECT - 30000::smallint"), "-30000");
    assert_eq!(one(&mut eng, "SELECT - NULL"), "NULL");
    assert_eq!(one(&mut eng, "SELECT -'5'"), "-5");
    // doNegate overflow: 22003 with the per-type message.
    assert_eq!(err_code(&mut eng, "SELECT - (-32768::smallint)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT - (-2147483648::int)"), "22003");
    // Unknown-literal path preserved: fails at evaluation like PG.
    assert_eq!(err_code(&mut eng, "SELECT -'2026-01-01'"), "22P02");
}

// v0.55: CASE (simple and searched), PG19 semantics.
#[test]
fn v55_case_basics() {
    let mut eng = engine();
    let one =
        |eng: &mut Engine, sql: &str| -> String { rows_of(run(eng, sql).unwrap())[0][0].clone() };
    let err_code =
        |eng: &mut Engine, sql: &str| -> &'static str { run(eng, sql).unwrap_err().code };
    // Searched form.
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE WHEN 1=0 THEN 'no' WHEN 1=1 THEN 'yes' ELSE '?' END"
        ),
        "yes"
    );
    assert_eq!(one(&mut eng, "SELECT CASE WHEN false THEN 1 END"), "NULL");
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE WHEN true THEN 1 WHEN true THEN 2 ELSE 3 END"
        ),
        "1"
    );
    // NULL WHEN is not true; non-boolean WHEN is 42804.
    assert_eq!(
        one(&mut eng, "SELECT CASE WHEN NULL THEN 1 ELSE 2 END"),
        "2"
    );
    assert_eq!(err_code(&mut eng, "SELECT CASE WHEN 1 THEN 2 END"), "42804");
    assert_eq!(
        err_code(&mut eng, "SELECT CASE WHEN 'x' THEN 2 END"),
        "42804"
    );
    // Simple form: operand evaluated once, `=` semantics (NULL never matches).
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE 'a' WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE 3 END"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END"
        ),
        "two"
    );
    assert_eq!(
        one(&mut eng, "SELECT CASE NULL WHEN NULL THEN 1 ELSE 2 END"),
        "2"
    );
    assert_eq!(
        one(&mut eng, "SELECT CASE 1 WHEN NULL THEN 'n' ELSE 'o' END"),
        "o"
    );
    // Unknown literal keys coerce to the operand type, like PG.
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE 1 WHEN '1' THEN 'one' ELSE 'other' END"
        ),
        "one"
    );
    // Short-circuit: untaken arms' errors never fire.
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE WHEN 1=0 THEN 1/0 WHEN 1=1 THEN 1 ELSE 2/0 END"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE 1 WHEN 0 THEN 1/0 WHEN 1 THEN 1 ELSE 2/0 END"
        ),
        "1"
    );
    // Result type unification; taken arm coerced to it (type oids
    // asserted in protocol_test56).
    assert_eq!(
        one(&mut eng, "SELECT CASE WHEN false THEN 1 ELSE 2.5 END"),
        "2.5"
    );
    assert_eq!(
        one(&mut eng, "SELECT CASE WHEN true THEN 1 ELSE 2.5 END"),
        "1"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT CASE WHEN true THEN 1 ELSE now() END"),
        "42804"
    );
    // Nested CASE.
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE WHEN true THEN CASE WHEN false THEN 1 ELSE 2 END ELSE 3 END"
        ),
        "2"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT CASE CASE WHEN true THEN 1 ELSE 2 END WHEN 1 THEN 'one' ELSE 'other' END"
        ),
        "one"
    );
}

// v0.55: CASE over table rows, in UPDATE, and with aggregates.
#[test]
fn v55_case_rows() {
    let mut eng = engine();
    // users(id int, name text): (1,'ann'), (2,'bob'), (3,'cid')
    let out = rows_of(run(&mut eng, "SELECT id, CASE WHEN id = 1 THEN 'one' WHEN id = 2 THEN 'two' ELSE 'other' END FROM users ORDER BY id").unwrap());
    assert_eq!(
        out,
        vec![vec!["1", "one"], vec!["2", "two"], vec!["3", "other"]]
    );
    // CASE in UPDATE (pg_regress case.sql pattern).
    run(
        &mut eng,
        "UPDATE users SET id = CASE WHEN id >= 3 THEN (-id) ELSE (2 * id) END",
    )
    .unwrap();
    let out = rows_of(run(&mut eng, "SELECT id FROM users ORDER BY id").unwrap());
    assert_eq!(out, vec![vec!["-3"], vec!["2"], vec!["4"]]);
    // Aggregate inside CASE takes the grouped path.
    let out = rows_of(
        run(
            &mut eng,
            "SELECT CASE WHEN count(*) > 2 THEN 'many' ELSE 'few' END FROM users",
        )
        .unwrap(),
    );
    assert_eq!(out, vec![vec!["many"]]);
    // CASE in WHERE (ids are now 2, 4, -3 after the UPDATE above).
    let out = rows_of(
        run(
            &mut eng,
            "SELECT id FROM users WHERE CASE WHEN id > 1 THEN true ELSE false END ORDER BY id",
        )
        .unwrap(),
    );
    assert_eq!(out, vec![vec!["2"], vec!["4"]]);
}

// v0.16: cursor_window positioning semantics (Postgres rules).
#[test]
fn v16_cursor_window() {
    use crate::server::cursor_window_for_test;
    use crate::sql::FetchDir;
    // 4 rows; cursor starts before the first row (-1).
    assert_eq!(
        cursor_window_for_test(&FetchDir::Forward(Some(1)), -1, 4),
        (0, 1, 0)
    );
    assert_eq!(
        cursor_window_for_test(&FetchDir::Forward(Some(2)), 0, 4),
        (1, 3, 2)
    );
    assert_eq!(
        cursor_window_for_test(&FetchDir::Forward(None), 2, 4),
        (3, 4, 4)
    );
    // Past the end: empty, parked after the last row.
    assert_eq!(
        cursor_window_for_test(&FetchDir::Forward(Some(1)), 3, 4),
        (0, 0, 4)
    );
    // BACKWARD excludes the current row and lands on the first returned.
    assert_eq!(
        cursor_window_for_test(&FetchDir::Backward(Some(3)), 4, 4),
        (1, 4, 1)
    );
    // RELATIVE is relative to the current row (cursor on row 2).
    assert_eq!(
        cursor_window_for_test(&FetchDir::Relative(-1), 1, 4),
        (0, 1, 0)
    );
    assert_eq!(
        cursor_window_for_test(&FetchDir::Absolute(2), -1, 4),
        (1, 2, 1)
    );
    assert_eq!(
        cursor_window_for_test(&FetchDir::Absolute(-1), -1, 4),
        (3, 4, 3)
    );
    // ABSOLUTE 0: before the first row, no rows.
    assert_eq!(
        cursor_window_for_test(&FetchDir::Absolute(0), 2, 4),
        (0, 0, -1)
    );
    assert_eq!(cursor_window_for_test(&FetchDir::First, 2, 4), (0, 1, 0));
    assert_eq!(cursor_window_for_test(&FetchDir::Last, 2, 4), (3, 4, 3));
    // v0.83: BACKWARD ALL from after-the-end parks before the first row.
    assert_eq!(
        cursor_window_for_test(&FetchDir::Backward(None), 4, 4),
        (0, 4, -1)
    );
}

// ====================================================================
// v0.21: float8 math cluster.
// ====================================================================

/// Parse the single row of a single/multi-column float SELECT into f64s.
fn floats_of(eng: &mut Engine, sql: &str) -> Vec<f64> {
    rows_of(run(eng, sql).unwrap())[0]
        .iter()
        .map(|s| {
            s.parse::<f64>()
                .unwrap_or_else(|_| panic!("not a float: {s}"))
        })
        .collect()
}

fn err_code(eng: &mut Engine, sql: &str) -> &'static str {
    run(eng, sql).unwrap_err().code
}

#[test]
fn float8_hyperbolic_values() {
    let mut eng = engine();
    let v = floats_of(&mut eng, "SELECT sinh(1.0), cosh(1.0), tanh(1.0)");
    assert!((v[0] - 1.1752011936438014).abs() < 1e-15, "sinh(1): {v:?}");
    assert!((v[1] - 1.5430806348152437).abs() < 1e-15, "cosh(1): {v:?}");
    assert!((v[2] - 0.7615941559557649).abs() < 1e-15, "tanh(1): {v:?}");
    let v = floats_of(&mut eng, "SELECT asinh(1.0), acosh(2.0), atanh(0.5)");
    assert!((v[0] - 0.881373587019543).abs() < 1e-15, "asinh(1): {v:?}");
    assert!((v[1] - 1.3169578969248167).abs() < 1e-15, "acosh(2): {v:?}");
    assert!(
        (v[2] - 0.5493061443340549).abs() < 1e-15,
        "atanh(0.5): {v:?}"
    );
}

#[test]
fn float8_hyperbolic_specials() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT sinh('Infinity'::float8), cosh('-Infinity'::float8), \
                 tanh('NaN'::float8), asinh('Infinity'::float8), \
                 acosh('Infinity'::float8)",
        )
        .unwrap(),
    )[0]
    .clone();
    assert_eq!(rows[0], "Infinity");
    assert_eq!(rows[1], "Infinity");
    assert_eq!(rows[2], "NaN");
    assert_eq!(rows[3], "Infinity");
    assert_eq!(rows[4], "Infinity");
    // atanh(±1) = ±inf, but |x| > 1 is out of range.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT atanh(1.0::float8)").unwrap())[0][0],
        "Infinity"
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT atanh(-1.0::float8)").unwrap())[0][0],
        "-Infinity"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT atanh('Infinity'::float8)"),
        "22003"
    );
    // sinh/cosh overflow of a finite input is 22003.
    assert_eq!(err_code(&mut eng, "SELECT sinh(1000.0)"), "22003");
}

#[test]
fn float8_trig_domain_errors() {
    let mut eng = engine();
    assert_eq!(
        err_code(&mut eng, "SELECT sin('Infinity'::float8)"),
        "22003"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT cos('-Infinity'::float8)"),
        "22003"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT tan('Infinity'::float8)"),
        "22003"
    );
    assert_eq!(err_code(&mut eng, "SELECT asin(2.0)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT asin(-2.0::float8)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT acos(2.0)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT acosh(0.5)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT atanh(2.0)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT atanh(-1.5::float8)"), "22003");
    // v0.21 repair: sqrt of a negative float is 2201F (was NaN).
    assert_eq!(err_code(&mut eng, "SELECT sqrt(-1.0::float8)"), "2201F");
    assert_eq!(err_code(&mut eng, "SELECT sqrt(-4.0::float4)"), "2201F");
    // NaN still propagates.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT sqrt('NaN'::float8)").unwrap())[0][0],
        "NaN"
    );
}

#[test]
fn float8_degree_trig_exact() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT sind(30), cosd(60), tand(45), asind(0.5), acosd(0.5), \
                 atand(1), atan2d(1, 1)",
        )
        .unwrap(),
    )[0]
    .clone();
    assert_eq!(rows, vec!["0.5", "0.5", "1", "30", "60", "45", "45"]);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT sind(90), cosd(0), tand(90), cotd(0), cotd(45), sind(-30)",
        )
        .unwrap(),
    )[0]
    .clone();
    assert_eq!(rows, vec!["1", "1", "Infinity", "Infinity", "1", "-0.5"]);
    // sind(inf) is out of range; asind/acosd reject |x| > 1.
    assert_eq!(
        err_code(&mut eng, "SELECT sind('Infinity'::float8)"),
        "22003"
    );
    assert_eq!(err_code(&mut eng, "SELECT asind(1.5)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT acosd(-2.0::float8)"), "22003");
}

// v0.94: PG19 float/numeric NaN and signed-zero comparison parity,
// grounded in REL_19_STABLE: src/include/utils/float.h
// (`float8_eq`: `isnan(a) ? isnan(b) : ...` — NaN = NaN is TRUE,
// NaN <> NaN is FALSE) and numeric.c `cmp_numerics` ("We consider
// all NANs to be equal"). -0.0 = 0.0 is TRUE (IEEE `==`).
#[test]
fn v94_nan_signed_zero_cmp() {
    let mut eng = engine();
    let one = |eng: &mut _, sql: &str| rows_of(run(eng, sql).unwrap())[0][0].clone();
    // NaN = NaN is TRUE (float and numeric); <> is FALSE.
    assert_eq!(one(&mut eng, "SELECT 'nan'::float8 = 'nan'::float8"), "t");
    assert_eq!(one(&mut eng, "SELECT 'nan'::float8 <> 'nan'::float8"), "f");
    assert_eq!(one(&mut eng, "SELECT 'nan'::float4 = 'nan'::float4"), "t");
    assert_eq!(one(&mut eng, "SELECT 'nan'::numeric = 'nan'::numeric"), "t");
    assert_eq!(
        one(&mut eng, "SELECT 'nan'::numeric <> 'nan'::numeric"),
        "f"
    );
    assert_eq!(one(&mut eng, "SELECT 'nan'::float8 = 'nan'::numeric"), "t");
    assert_eq!(one(&mut eng, "SELECT 'nan'::float8 = '1'::float8"), "f");
    // -0.0 = 0.0 is TRUE; -0.0 < 0.0 is FALSE.
    assert_eq!(one(&mut eng, "SELECT '-0.0'::float8 = '0.0'::float8"), "t");
    assert_eq!(one(&mut eng, "SELECT '-0.0'::float8 < '0.0'::float8"), "f");
    assert_eq!(one(&mut eng, "SELECT '-0.0'::float4 = '0'::float4"), "t");
    // NaN ordering (float8_lt/gt): NaN sorts after everything.
    assert_eq!(one(&mut eng, "SELECT 'nan'::float8 > '1e308'::float8"), "t");
    assert_eq!(one(&mut eng, "SELECT '1'::float8 < 'nan'::float8"), "t");
    assert_eq!(one(&mut eng, "SELECT 'nan'::float8 < '1'::float8"), "f");
    assert_eq!(one(&mut eng, "SELECT 'nan'::numeric > '1'::numeric"), "t");
    // IS DISTINCT FROM: NaN is still not distinct from NaN.
    assert_eq!(
        one(
            &mut eng,
            "SELECT 'nan'::float8 IS DISTINCT FROM 'nan'::float8"
        ),
        "f"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT 'nan'::float8 IS DISTINCT FROM '1'::float8"
        ),
        "t"
    );
}

#[test]
fn float8_erf_erfc_values() {
    let mut eng = engine();
    let v = floats_of(
        &mut eng,
        "SELECT erf(1.0), erf(-1.0), erfc(6.0), erfc(28.0)",
    );
    assert!((v[0] - 0.8427007929497148).abs() < 1e-12, "erf(1): {v:?}");
    assert!((v[1] + 0.8427007929497148).abs() < 1e-12, "erf(-1): {v:?}");
    assert!(
        (v[2] - 2.1519736712498925e-17).abs() < 1e-29,
        "erfc(6): {v:?}"
    );
    assert_eq!(v[3], 0.0, "erfc(28) clamps to 0: {v:?}");
    // Specials: erf(±inf) = ±1, erfc(+inf) = 0, erfc(-inf) = 2.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT erf('Infinity'::float8), erf('-Infinity'::float8), \
                 erfc('Infinity'::float8), erfc('-Infinity'::float8), \
                 erf('NaN'::float8)",
        )
        .unwrap(),
    )[0]
    .clone();
    assert_eq!(rows, vec!["1", "-1", "0", "2", "NaN"]);
}

#[test]
fn float8_gamma_lgamma_values() {
    let mut eng = engine();
    let v = floats_of(&mut eng, "SELECT gamma(0.5), gamma(5.0), lgamma(0.5)");
    assert!(
        (v[0] - 1.7724538509055159).abs() < 1e-12,
        "gamma(0.5): {v:?}"
    );
    assert_eq!(v[1], 24.0, "gamma(5): {v:?}");
    assert!(
        (v[2] - 0.5723649429246995).abs() < 1e-12,
        "lgamma(0.5): {v:?}"
    );
    let v = floats_of(&mut eng, "SELECT lgamma(-1000.5), gamma(-0.5)");
    assert!(
        (v[0] - -5914.437701116853).abs() < 1e-6,
        "lgamma(-1000.5): {v:?}"
    );
    assert!(
        (v[1] - -3.544907701811032).abs() < 1e-12,
        "gamma(-0.5): {v:?}"
    );
    // Poles and -inf are 22003; gamma(inf) = inf.
    assert_eq!(err_code(&mut eng, "SELECT gamma(-1.0)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT gamma(0.0::float8)"), "22003");
    assert_eq!(
        err_code(&mut eng, "SELECT gamma('-Infinity'::float8)"),
        "22003"
    );
    assert_eq!(err_code(&mut eng, "SELECT lgamma(0.0)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT gamma(200.0)"), "22003");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT gamma('Infinity'::float8)").unwrap())[0][0],
        "Infinity"
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT lgamma('-Infinity'::float8)").unwrap())[0][0],
        "Infinity"
    );
}

#[test]
fn float8_exp_ln_log_range() {
    let mut eng = engine();
    // v0.21 repair: finite exp overflow/underflow is 22003.
    assert_eq!(err_code(&mut eng, "SELECT exp(1000.0::float8)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT exp(-1000.0::float8)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT exp(1000.0::float4)"), "22003");
    // NaN/Inf pass through.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT exp('NaN'::float8), exp('Infinity'::float8), \
                 exp('-Infinity'::float8)",
        )
        .unwrap(),
    )[0]
    .clone();
    assert_eq!(rows, vec!["NaN", "Infinity", "0"]);
    // ln/log/log10: zero and negative are 2201E.
    assert_eq!(err_code(&mut eng, "SELECT ln(0.0::float8)"), "2201E");
    assert_eq!(err_code(&mut eng, "SELECT ln(-1.0::float8)"), "2201E");
    assert_eq!(err_code(&mut eng, "SELECT log(0.0::float8)"), "2201E");
    assert_eq!(err_code(&mut eng, "SELECT log10(0.0)"), "2201E");
    assert_eq!(err_code(&mut eng, "SELECT log10(-5.0::float8)"), "2201E");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT log10(100.0::float8)").unwrap())[0][0],
        "2"
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT log10(1000.0)").unwrap())[0][0],
        "3"
    );
}

#[test]
fn float8_trunc_and_prefix_ops() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT trunc(3.7), trunc(-3.7::float8), trunc(3.7::float4), \
                 @-5, @5, |/4, ||/8, @-5.5",
        )
        .unwrap(),
    )[0]
    .clone();
    // v0.62 fix: PG has no numeric cbrt (cbrt() is float8-only, and
    // PG's float8 cbrt(8) displays "2"), so the numeric cbrt strips
    // its formatting scale: ||/8 is "2".
    // v0.62: PG19 numeric_sqrt gives >= 16 significant digits, so
    // |/4 is 2.000000000000000 (was "2" under the old f64 sqrt).
    assert_eq!(
        rows,
        vec!["3", "-3", "3", "5", "5", "2.000000000000000", "2", "5.5"]
    );
    // v0.21: text-vs-numeric coercion in arithmetic and comparison.
    // ('1.5' + 1 would coerce the text to integer and fail, like PG.)
    let rows =
        rows_of(run(&mut eng, "SELECT '2' + 1.5, 1.5 = '1.5', '1.5' = 1.5").unwrap())[0].clone();
    assert_eq!(rows, vec!["3.5", "t", "t"]);
    assert_eq!(err_code(&mut eng, "SELECT '1.5' + 1"), "22P02");
    assert_eq!(err_code(&mut eng, "SELECT 'abc' + 1"), "22P02");
    assert_eq!(err_code(&mut eng, "SELECT 'abc' = 1.5"), "22P02");
    // v0.21: float division by zero semantics.
    assert_eq!(err_code(&mut eng, "SELECT 1.0::float8 / 0.0"), "22012");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT 'nan'::float8 / '0'::float8").unwrap())[0][0],
        "NaN"
    );
    // v0.21: float overflow/underflow in arithmetic.
    assert_eq!(err_code(&mut eng, "SELECT 1e308::float8 * 10.0"), "22003");
    assert_eq!(
        err_code(&mut eng, "SELECT '1e-30'::float4 * '1e-30'::float4"),
        "22003"
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT 'Infinity'::float8 + 100.0").unwrap())[0][0],
        "Infinity"
    );
    // v0.21: power domain checks.
    assert_eq!(
        err_code(&mut eng, "SELECT power(0.0, '-Infinity'::float8)"),
        "2201F"
    );
    assert_eq!(err_code(&mut eng, "SELECT power(-1.0, 0.5)"), "2201F");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT power('inf'::float8, -2)").unwrap())[0][0],
        "0"
    );
}

// ====================================================================
// v0.56: width_bucket PG19 hardening + two-argument trunc.
// ====================================================================

#[test]
fn width_bucket_exact_and_errors() {
    let mut eng = engine();
    // Grounded on PG19 numeric.out. v0.55 returned 1 for both.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT width_bucket(0, -1e100::numeric, 1, 10)").unwrap())[0],
        vec!["10"]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT width_bucket(1, 1e100::numeric, 0, 10)").unwrap())[0],
        vec!["10"]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT width_bucket(0, -1e100::float8, 1, 10)").unwrap())[0],
        vec!["10"]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT width_bucket(1, 1e100::float8, 0, 10)").unwrap())[0],
        vec!["10"]
    );
    // Bucket numbering from the vendored 19-row table.
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT width_bucket(-5.2, 0, 10, 5), width_bucket(-5.2, 10, 0, 5)"
            )
            .unwrap()
        )[0],
        vec!["0", "6"]
    );
    assert_eq!(
            rows_of(run(&mut eng, "SELECT width_bucket(10.0000000000001, 0, 10, 5), width_bucket(10.0000000000001, 10, 0, 5)").unwrap())[0],
            vec!["6", "0"]
        );
    assert_eq!(
            rows_of(run(&mut eng, "SELECT width_bucket(4.5, 2, 8, 4), width_bucket(5, 5.0, 5.5, 20), width_bucket(5.5, 5.0, 5.5, 20)").unwrap())[0],
            vec!["2", "1", "21"]
        );
    // PG19 validation.
    assert_eq!(
        err_code(&mut eng, "SELECT width_bucket(5.0, 3.0, 4.0, 0)"),
        "22023"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT width_bucket(5.0, 3.0, 4.0, -5)"),
        "22023"
    );
    // v0.55 returned 889; PG errors.
    assert_eq!(
        err_code(&mut eng, "SELECT width_bucket(3.5, 3.0, 3.0, 888)"),
        "22023"
    );
    // NaN operand -> count+1; NaN bounds -> error.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT width_bucket('NaN', 3.0, 4.0, 888)").unwrap())[0],
        vec!["889"]
    );
    assert_eq!(
        err_code(&mut eng, "SELECT width_bucket(0, 'NaN', 4.0, 888)"),
        "22003"
    );
    // Infinite bounds rejected; infinite operands allowed.
    assert_eq!(
        err_code(&mut eng, "SELECT width_bucket(2.0, 3.0, '-inf', 888)"),
        "22003"
    );
    assert_eq!(
            rows_of(run(&mut eng, "SELECT width_bucket('Infinity'::float8, 1, 10, 10), width_bucket('-Infinity'::float8, 1, 10, 10), width_bucket('-Infinity'::float8, 10, 1, 10)").unwrap())[0],
            vec!["11", "0", "11"]
        );
    // float8 overflow/underflow rows from numeric.out.
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT width_bucket(10.5::float8, -1.797e308::float8, 1.797e308::float8, 2)"
            )
            .unwrap()
        )[0],
        vec!["2"]
    );
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT width_bucket(4.4925e307::float8, -8.985e307::float8, 8.985e307::float8, 10)"
            )
            .unwrap()
        )[0],
        vec!["8"]
    );
    // v0.56 note: `5e-324::float8` underflows to 0 at literal-parse
    // time (pre-existing engine limitation, out of scope); the
    // numeric path exercises the same exact-decimal code exactly.
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT width_bucket(0, 0, 5e-324, 4), width_bucket(5e-324, 0, 5e-324, 4)"
            )
            .unwrap()
        )[0],
        vec!["1", "5"]
    );
    // Result beyond int32 errors (PG: integer out of range).
    assert_eq!(
        err_code(&mut eng, "SELECT width_bucket(1::float8, 0, 1, 2147483647)"),
        "22003"
    );
    // NULL is strict on all four arguments.
    assert_eq!(
            rows_of(run(&mut eng, "SELECT width_bucket(NULL, 0, 10, 5), width_bucket(1, 0, NULL, 5), width_bucket(1, 0, 10, NULL)").unwrap())[0],
            vec!["NULL", "NULL", "NULL"]
        );
}

#[test]
fn trunc_two_arg() {
    let mut eng = engine();
    // v0.55 raised 42883 for the two-argument form.
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT trunc(19.99, 1), trunc(19.99, -1), trunc(1.99999, 3), trunc(-19.99, 1)"
            )
            .unwrap()
        )[0],
        vec!["19.9", "10", "1.999", "-19.9"]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT trunc(19.99::float8, 1)").unwrap())[0],
        vec!["19.9"]
    );
    // Huge negative scale must not overflow (old code panicked in debug).
    assert_eq!(
        rows_of(run(&mut eng, "SELECT trunc(1.5, -2147483648)").unwrap())[0],
        vec!["0"]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT trunc(1.5, NULL)").unwrap())[0],
        vec!["NULL"]
    );
    assert_eq!(err_code(&mut eng, "SELECT trunc(1.5, 1, 2)"), "42883");
    // One-argument behavior unchanged.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT trunc(19.99), trunc(9.9::float8)").unwrap())[0],
        vec!["19", "9"]
    );
}

// ====================================================================
// v0.23: JOIN ... USING / NATURAL JOIN merged-column semantics.
// ====================================================================

fn cols_of(r: &ExecResult) -> Vec<String> {
    match r {
        ExecResult::Select { columns, .. } | ExecResult::Explain { columns, .. } => {
            columns.iter().map(|(n, _)| n.clone()).collect()
        }
        _ => panic!("expected a row-returning result"),
    }
}

fn select_cols(eng: &mut Engine, sql: &str) -> Vec<String> {
    let r = run(eng, sql).unwrap();
    cols_of(&r)
}

/// `USING` merges each key into one visible column; merged keys come
/// first, then the remaining left columns, then the right ones.
#[test]
fn using_merges_single_visible_key_first() {
    let mut eng = engine();
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM users JOIN orders USING (id)"),
        vec!["id", "name", "uid", "amt"]
    );
}

/// The merged column is addressable unqualified — no 42702.
#[test]
fn using_unqualified_key_resolves_without_ambiguity() {
    let mut eng = engine();
    let rows = rows_of(run(&mut eng, "SELECT id FROM users JOIN orders USING (id)").unwrap());
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], "1");
}

/// The original side columns stay reachable through qualifiers, and
/// `qual.*` expands in the side's own order.
#[test]
fn using_keeps_qualified_originals() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT users.id, orders.id FROM users JOIN orders USING (id)",
        )
        .unwrap(),
    );
    assert_eq!(rows[0], vec!["1".to_string(), "1".to_string()]);
    assert_eq!(
        select_cols(&mut eng, "SELECT users.* FROM users JOIN orders USING (id)"),
        vec!["id", "name"]
    );
    assert_eq!(
        select_cols(
            &mut eng,
            "SELECT orders.* FROM users JOIN orders USING (id)"
        ),
        vec!["id", "uid", "amt"]
    );
}

/// INNER/LEFT/RIGHT/FULL disagree only on the merged key of
/// null-extended rows: left value, right value, COALESCE.
#[test]
fn using_outer_join_merged_values() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a(id int, x text)").unwrap();
    run(&mut eng, "CREATE TABLE b(id int, y text)").unwrap();
    run(&mut eng, "INSERT INTO a VALUES (1,'p'),(2,'q')").unwrap();
    run(&mut eng, "INSERT INTO b VALUES (1,'u'),(3,'v')").unwrap();
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT * FROM a LEFT JOIN b USING (id) ORDER BY id"
            )
            .unwrap()
        ),
        vec![vec!["1", "p", "u"], vec!["2", "q", "NULL"]]
    );
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT * FROM a RIGHT JOIN b USING (id) ORDER BY id"
            )
            .unwrap()
        ),
        vec![vec!["1", "p", "u"], vec!["3", "NULL", "v"]]
    );
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT * FROM a FULL JOIN b USING (id) ORDER BY id"
            )
            .unwrap()
        ),
        vec![
            vec!["1", "p", "u"],
            vec!["2", "q", "NULL"],
            vec!["3", "NULL", "v"]
        ]
    );
}

/// Multiple USING keys merge in USING order; a key missing from
/// either side is 42703.
#[test]
fn using_multiple_keys_merge_in_order() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE m1(a int, b int, x text)").unwrap();
    run(&mut eng, "CREATE TABLE m2(a int, b int, y text)").unwrap();
    run(&mut eng, "INSERT INTO m1 VALUES (1,2,'p'),(3,4,'q')").unwrap();
    run(&mut eng, "INSERT INTO m2 VALUES (1,2,'u'),(3,9,'v')").unwrap();
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM m1 JOIN m2 USING (a, b)"),
        vec!["a", "b", "x", "y"]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT * FROM m1 JOIN m2 USING (a, b)").unwrap()),
        vec![vec!["1", "2", "p", "u"]]
    );
    assert_eq!(
        err_code(&mut eng, "SELECT * FROM m1 JOIN m2 USING (a, c)"),
        "42703"
    );
}

/// NATURAL JOIN merges the shared columns like USING does.
#[test]
fn natural_join_merges_shared_columns() {
    let mut eng = engine();
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM users NATURAL JOIN orders"),
        vec!["id", "name", "uid", "amt"]
    );
    let rows = rows_of(run(&mut eng, "SELECT id FROM users NATURAL JOIN orders").unwrap());
    assert_eq!(rows.len(), 3);
}

/// `JOIN ... USING (i) AS x` exposes only the merged columns as `x`;
/// anything else under `x` is 42703.
#[test]
fn using_alias_exposes_only_merged_columns() {
    let mut eng = engine();
    assert_eq!(
        select_cols(
            &mut eng,
            "SELECT x.* FROM users JOIN orders USING (id) AS x"
        ),
        vec!["id"]
    );
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT x.id FROM users JOIN orders USING (id) AS x",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT x.name FROM users JOIN orders USING (id) AS x"
        ),
        "42703"
    );
}

/// `(a JOIN b ...) AS x` requalifies the output to `x` and hides the
/// inner table names.
#[test]
fn whole_join_alias_hides_inner_names() {
    let mut eng = engine();
    assert_eq!(
        select_cols(
            &mut eng,
            "SELECT * FROM (users JOIN orders USING (id)) AS x"
        ),
        vec!["id", "name", "uid", "amt"]
    );
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT x.id, x.name FROM (users JOIN orders USING (id)) AS x ORDER BY x.id",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 3);
    assert!(
        run(
            &mut eng,
            "SELECT users.id FROM (users JOIN orders USING (id)) AS x"
        )
        .is_err(),
        "inner table names must not leak through a whole-join alias"
    );
}

/// A column alias list on a parenthesized join renames positionally.
#[test]
fn whole_join_col_alias_list_renames_positionally() {
    let mut eng = engine();
    assert_eq!(
        select_cols(
            &mut eng,
            "SELECT * FROM (users JOIN orders USING (id)) AS x(p, q, r, s)"
        ),
        vec!["p", "q", "r", "s"]
    );
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT p FROM (users JOIN orders USING (id)) AS x(p, q) ORDER BY p",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], "1");
}

/// More column aliases than available columns is 42601 in PostgreSQL,
/// on every alias-list form (v0.23 arity rule).
#[test]
fn col_alias_list_arity_is_checked() {
    let mut eng = engine();
    for q in [
        "SELECT * FROM users AS u(a, b, c)",
        "SELECT * FROM (users JOIN orders USING (id)) AS x(a, b, c, d, e)",
        "SELECT * FROM (SELECT id FROM users) AS s(a, b)",
        "SELECT * FROM (VALUES (1, 2)) AS v(a, b, c)",
        "WITH c(a, b, c) AS (SELECT 1, 2) SELECT * FROM c",
    ] {
        let err = run(&mut eng, q).unwrap_err();
        assert_eq!(err.code, "42601", "query: {q}");
    }
    // Exact and short lists still work.
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM users AS u(a, b)"),
        vec!["a", "b"]
    );
}

/// `FROM tbl AS t(a, b)` renames positionally; fewer aliases than
/// columns leave the rest under their original names.
#[test]
fn table_col_alias_list_renames_base_table() {
    let mut eng = engine();
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM users AS u(a, b)"),
        vec!["a", "b"]
    );
    let rows = rows_of(run(&mut eng, "SELECT a, u.b FROM users AS u(a, b) ORDER BY a").unwrap());
    assert_eq!(rows[0], vec!["1".to_string(), "ann".to_string()]);
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM users AS u(a)"),
        vec!["a", "name"]
    );
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM users AS u"),
        vec!["id", "name"]
    );
}

/// `(SELECT ...) AS s(x, y)` renames the derived columns positionally.
#[test]
fn derived_col_alias_list_renames() {
    let mut eng = engine();
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM (SELECT 1 AS a, 2 AS b) AS s(x, y)"),
        vec!["x", "y"]
    );
    let rows = rows_of(run(&mut eng, "SELECT x FROM (SELECT 1 AS a, 2 AS b) AS s(x, y)").unwrap());
    assert_eq!(rows, vec![vec!["1".to_string()]]);
}

/// `SELECT *` over a USING join shows exactly one merged key column.
#[test]
fn star_never_shows_hidden_join_originals() {
    let mut eng = engine();
    let cols = select_cols(&mut eng, "SELECT * FROM users JOIN orders USING (id)");
    assert_eq!(cols.iter().filter(|c| *c == "id").count(), 1);
}

/// The merged key is usable in WHERE, GROUP BY and ORDER BY.
#[test]
fn merged_key_usable_in_clauses() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT id, count(*) FROM users JOIN orders USING (id) GROUP BY id ORDER BY id",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 3);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT id FROM users JOIN orders USING (id) WHERE id > 1 ORDER BY id",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["2".to_string()], vec!["3".to_string()]]);
}

// ---------------------------------------------------------------
// v0.23 parser shapes.
// ---------------------------------------------------------------

fn select_from(sql: &str) -> Vec<FromItem> {
    let stmt = crate::sql::parse_statement(sql).expect("parses");
    let crate::sql::Stmt::Select(s) = stmt else {
        panic!("SELECT");
    };
    s.from
}

#[test]
fn parse_using_alias_shape() {
    let from = select_from("SELECT * FROM a JOIN b USING (i) AS x");
    let crate::sql::FromItem::Join { using_alias, .. } = &from[0] else {
        panic!("join");
    };
    assert_eq!(using_alias.as_deref(), Some("x"));
}

#[test]
fn parse_paren_join_alias_shape() {
    let from = select_from("SELECT * FROM (a JOIN b USING (i)) AS x");
    let crate::sql::FromItem::Join { alias, .. } = &from[0] else {
        panic!("join");
    };
    assert_eq!(alias.as_deref(), Some("x"));
}

#[test]
fn parse_table_col_alias_list_shape() {
    let from = select_from("SELECT * FROM tbl AS t(a, b)");
    let crate::sql::FromItem::Table { col_aliases, .. } = &from[0] else {
        panic!("table");
    };
    assert_eq!(col_aliases, &vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn parse_derived_col_alias_list_retained() {
    let from = select_from("SELECT * FROM (SELECT 1) AS s(x, y)");
    let crate::sql::FromItem::Derived { col_aliases, .. } = &from[0] else {
        panic!("derived");
    };
    assert_eq!(col_aliases, &vec!["x".to_string(), "y".to_string()]);
}

#[test]
fn parse_join_col_alias_list_shape() {
    let from = select_from("SELECT * FROM (a JOIN b USING (i)) AS x(p, q)");
    let crate::sql::FromItem::Join { col_aliases, .. } = &from[0] else {
        panic!("join");
    };
    assert_eq!(col_aliases, &vec!["p".to_string(), "q".to_string()]);
}
/// v0.41: ADD COLUMN rewrites rows; the per-cell TOAST value ids must
/// survive the rewrite (PG keeps the toast pointers), so
/// `pg_column_compression` still reports the value's method.
#[test]
fn v41_add_column_preserves_toast_method() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t41ac(v text)").unwrap();
    run(&mut eng, "ALTER TABLE t41ac SET (toast_tuple_target = 128)").unwrap();
    run(
        &mut eng,
        "ALTER TABLE t41ac ALTER COLUMN v SET COMPRESSION pglz",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO t41ac VALUES (repeat('N', 3000))").unwrap();
    let before = rows_of(run(&mut eng, "SELECT pg_column_compression(v) FROM t41ac").unwrap());
    assert_eq!(before, vec![vec!["pglz".to_string()]]);
    run(&mut eng, "ALTER TABLE t41ac ADD COLUMN w text").unwrap();
    let after = rows_of(run(&mut eng, "SELECT pg_column_compression(v) FROM t41ac").unwrap());
    assert_eq!(after, vec![vec!["pglz".to_string()]]);
    // Values themselves survive too.
    let len = rows_of(run(&mut eng, "SELECT length(v) FROM t41ac").unwrap());
    assert_eq!(len, vec![vec!["3000".to_string()]]);
    // col_compression stays parallel to columns (None for the new col).
    let t = eng.db.tables["t41ac"].last().unwrap();
    assert_eq!(t.columns.len(), 2);
    assert_eq!(t.col_compression.len(), 2);
    assert_eq!(
        t.col_compression[0],
        Some(crate::storage::ToastCompression::Pglz)
    );
    assert_eq!(t.col_compression[1], None);
}

/// v0.41: DROP COLUMN drops only the removed column's TOAST flag and
/// its `col_compression` entry; surviving cells keep their methods.
#[test]
fn v41_drop_column_preserves_surviving_toast_method() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t41dc(a text, b text)").unwrap();
    run(&mut eng, "ALTER TABLE t41dc SET (toast_tuple_target = 128)").unwrap();
    run(
        &mut eng,
        "ALTER TABLE t41dc ALTER COLUMN b SET COMPRESSION pglz",
    )
    .unwrap();
    run(
        &mut eng,
        "INSERT INTO t41dc VALUES (repeat('A', 3000), repeat('B', 3000))",
    )
    .unwrap();
    // Column a used the session default in the test harness (pglz).
    run(&mut eng, "ALTER TABLE t41dc DROP COLUMN a").unwrap();
    let after = rows_of(run(&mut eng, "SELECT pg_column_compression(b) FROM t41dc").unwrap());
    assert_eq!(after, vec![vec!["pglz".to_string()]]);
    let len = rows_of(run(&mut eng, "SELECT length(b) FROM t41dc").unwrap());
    assert_eq!(len, vec![vec!["3000".to_string()]]);
    let t = eng.db.tables["t41dc"].last().unwrap();
    assert_eq!(t.columns.len(), 1);
    assert_eq!(t.col_compression.len(), 1);
    assert_eq!(
        t.col_compression[0],
        Some(crate::storage::ToastCompression::Pglz)
    );
}

/// v0.42: regression — a row-rewriting ALTER (ADD/DROP COLUMN) must
/// mint fresh row ids. Reusing ids left two row versions with the
/// same id across table versions, so a later UPDATE failed with a
/// phantom 40001 "concurrent update".
#[test]
fn v42_alter_rewrite_mints_fresh_row_ids() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t42r(v text)").unwrap();
    run(&mut eng, "ALTER TABLE t42r SET (toast_tuple_target = 128)").unwrap();
    run(&mut eng, "INSERT INTO t42r VALUES (repeat('O', 3000))").unwrap();
    run(&mut eng, "ALTER TABLE t42r ADD COLUMN w text").unwrap();
    // This UPDATE returned 40001 before the fix.
    run(
        &mut eng,
        "UPDATE t42r SET w = repeat('W', 3000) WHERE v LIKE 'O%'",
    )
    .unwrap();
    let got = rows_of(run(&mut eng, "SELECT left(v,3), left(w,3) FROM t42r").unwrap());
    assert_eq!(got, vec![vec!["OOO".to_string(), "WWW".to_string()]]);
    // Row ids are globally unique across the rewrite.
    let t = eng.db.tables["t42r"].last().unwrap();
    let mut ids: Vec<u64> = t.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), t.rows.len());
    // DROP COLUMN rewrites too.
    run(&mut eng, "ALTER TABLE t42r DROP COLUMN w").unwrap();
    run(&mut eng, "UPDATE t42r SET v = repeat('Z', 3000)").unwrap();
    let got = rows_of(run(&mut eng, "SELECT left(v,3) FROM t42r").unwrap());
    assert_eq!(got, vec![vec!["ZZZ".to_string()]]);
}

/// v0.42: pg_input_is_valid for the numeric family (PG19 misc.c
/// runs the type's input function and maps soft errors to false).
/// Covers the 23 REAL-FAILs in boolean/int2/int4/int8/float4/
/// float8/numeric conformance suites.
#[test]
fn v42_pg_input_is_valid_numeric_family() {
    let mut eng = engine();
    let one =
        |eng: &mut Engine, sql: &str| -> String { rows_of(run(eng, sql).unwrap())[0][0].clone() };
    let cases = [
        // (input, type, expected)
        ("'true'", "'bool'", "t"),
        ("'asdf'", "'bool'", "f"),
        ("'34'", "'int2'", "t"),
        ("'asdf'", "'int2'", "f"),
        ("'50000'", "'int2'", "f"),
        ("' 1 3  5 '", "'int2vector'", "t"),
        // v0.43: PG's int2vectorin accepts the empty string as an
        // empty vector (verified against REL_19_STABLE int.c).
        ("''", "'int2vector'", "t"),
        ("'34'", "'int4'", "t"),
        ("'asdf'", "'int4'", "f"),
        ("'1000000000000'", "'int4'", "f"),
        ("'34'", "'int8'", "t"),
        ("'asdf'", "'int8'", "f"),
        ("'10000000000000000000'", "'int8'", "f"),
        ("'34.5'", "'float4'", "t"),
        ("'xyz'", "'float4'", "f"),
        ("'1e400'", "'float4'", "f"),
        ("'34.5'", "'float8'", "t"),
        ("'xyz'", "'float8'", "f"),
        ("'1e4000'", "'float8'", "f"),
        ("'34.5'", "'numeric'", "t"),
        ("'34xyz'", "'numeric'", "f"),
        ("'1e400000'", "'numeric'", "f"),
        ("'1234.567'", "'numeric(8,4)'", "t"),
        ("'1234.567'", "'numeric(7,4)'", "f"),
    ];
    for (inp, typ, expected) in cases {
        let q = format!("SELECT pg_input_is_valid({}, {})", inp, typ);
        assert_eq!(
            one(&mut eng, &q),
            expected,
            "pg_input_is_valid({}, {})",
            inp,
            typ
        );
    }
}

/// v0.43: pg_input_error_info returns PG's four OUT columns with the
/// input function's PG-exact soft error, or one row of NULLs for
/// valid input (PG19 misc.c, verified against REL_19_STABLE source
/// and the PG regression .out files).
#[test]
fn v43_pg_input_error_info() {
    let mut eng = engine();
    let info = |eng: &mut Engine, inp: &str, typ: &str| -> Vec<Vec<String>> {
        let q = format!("SELECT * FROM pg_input_error_info({inp}, {typ})");
        rows_of(run(eng, &q).unwrap())
    };
    // Column names are PG's OUT parameter names (pg_proc.dat).
    assert_eq!(
        select_cols(&mut eng, "SELECT * FROM pg_input_error_info('x', 'bool')"),
        vec!["message", "detail", "hint", "sql_error_code"]
    );
    // Valid input -> one row of NULLs.
    assert_eq!(
        info(&mut eng, "'true'", "'bool'"),
        vec![vec!["NULL", "NULL", "NULL", "NULL"]]
    );
    let cases: &[(&str, &str, &str, &str, &str)] = &[
        // (input, type, message, detail, sqlstate)
        (
            "'junk'",
            "'bool'",
            "invalid input syntax for type boolean: \"junk\"",
            "NULL",
            "22P02",
        ),
        (
            "'50000'",
            "'int2'",
            "value \"50000\" is out of range for type smallint",
            "NULL",
            "22003",
        ),
        (
            "'1 asdf'",
            "'int2vector'",
            "invalid input syntax for type smallint: \"asdf\"",
            "NULL",
            "22P02",
        ),
        (
            "'1e4000'",
            "'float8'",
            "\"1e4000\" is out of range for type double precision",
            "NULL",
            "22003",
        ),
        (
            "'1e400000'",
            "'numeric'",
            "value overflows numeric format",
            "NULL",
            "22003",
        ),
        (
            "'1234.567'",
            "'numeric(7,4)'",
            "numeric field overflow",
            "A field with precision 7, scale 4 must round to an absolute value less than 10^3.",
            "22003",
        ),
        (
            "'abcde'",
            "'char(4)'",
            "value too long for type character(4)",
            "NULL",
            "22001",
        ),
    ];
    for (inp, typ, message, detail, code) in cases {
        assert_eq!(
            info(&mut eng, inp, typ),
            vec![vec![
                message.to_string(),
                detail.to_string(),
                "NULL".to_string(),
                code.to_string()
            ]],
            "pg_input_error_info({inp}, {typ})",
        );
    }
    // Hard errors propagate: unknown type -> 42883.
    let err = run(&mut eng, "SELECT * FROM pg_input_error_info('x', 'bogus')").unwrap_err();
    assert_eq!(err.code, "42883");
}

/// v0.41: SET COMPRESSION error taxonomy matches PG.
#[test]
fn v41_set_compression_errors() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t41e(v text, i int)").unwrap();
    let e = run(
        &mut eng,
        "ALTER TABLE t41e ALTER COLUMN missing SET COMPRESSION pglz",
    )
    .unwrap_err();
    assert_eq!(e.code, "42703");
    let e = run(
        &mut eng,
        "ALTER TABLE t41e ALTER COLUMN i SET COMPRESSION lz4",
    )
    .unwrap_err();
    assert_eq!(e.code, "0A000");
    let e = run(
        &mut eng,
        "ALTER TABLE t41e ALTER COLUMN v SET COMPRESSION zstd",
    )
    .unwrap_err();
    assert_eq!(e.code, "22023");
    // DEFAULT clears explicit metadata.
    run(
        &mut eng,
        "ALTER TABLE t41e ALTER COLUMN v SET COMPRESSION pglz",
    )
    .unwrap();
    run(
        &mut eng,
        "ALTER TABLE t41e ALTER COLUMN v SET COMPRESSION DEFAULT",
    )
    .unwrap();
    let t = eng.db.tables["t41e"].last().unwrap();
    assert_eq!(t.col_compression, vec![None, None]);
}

/// v0.47: a top-level SRF call in the SELECT list fans each input
/// row out to one row per element (PG19 ProjectSet). The column is
/// named for the function and typed int4 for int literals.
#[test]
fn v47_select_list_srf_basic() {
    let mut eng = engine();
    let r = run(&mut eng, "SELECT generate_series(1, 3)").unwrap();
    match &r {
        ExecResult::Select { columns, .. } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].0, "generate_series");
            assert_eq!(columns[0].1, ColType::Int);
        }
        _ => panic!("expected select"),
    }
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string()],
            vec!["2".to_string()],
            vec!["3".to_string()],
        ]
    );
}

/// v0.47: plain select-list columns repeat on every fanned-out row;
/// an empty SRF set yields zero rows (PG19 `ExecProjectSRF`).
#[test]
fn v47_select_list_srf_repeat_and_empty() {
    let mut eng = engine();
    let r = run(&mut eng, "SELECT 1 AS t, generate_series(1, 3) AS x").unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "1".to_string()],
            vec!["1".to_string(), "2".to_string()],
            vec!["1".to_string(), "3".to_string()],
        ]
    );
    let r = run(&mut eng, "SELECT generate_series(1, 0)").unwrap();
    assert!(rows_of(r).is_empty());
    // Strict: NULL input yields zero rows.
    let r = run(&mut eng, "SELECT generate_series(NULL, 3)").unwrap();
    assert!(rows_of(r).is_empty());
}

/// v0.47: multiple SRFs fan out to the max width; the exhausted one
/// pads NULL (PG19 `ExecProjectSRF` continuing rounds).
#[test]
fn v47_select_list_srf_multi_width() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "SELECT generate_series(1, 2), generate_series(10, 12)",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "11".to_string()],
            vec!["NULL".to_string(), "12".to_string()],
        ]
    );
}

/// v0.47: SELECT-list SRF typing follows the FROM-clause rules
/// (int4/int8/numeric) and the error taxonomy is PG's.
#[test]
fn v47_select_list_srf_types_and_errors() {
    let mut eng = engine();
    let r = run(&mut eng, "SELECT generate_series(1::bigint, 2::bigint)").unwrap();
    match &r {
        ExecResult::Select { columns, .. } => assert_eq!(columns[0].1, ColType::BigInt),
        _ => panic!("expected select"),
    }
    assert_eq!(
        rows_of(r),
        vec![vec!["1".to_string()], vec!["2".to_string()]]
    );
    let r = run(&mut eng, "SELECT generate_series(1.5, 2.5)").unwrap();
    match &r {
        ExecResult::Select { columns, .. } => assert_eq!(columns[0].1, ColType::Numeric(None)),
        _ => panic!("expected select"),
    }
    let err = run(&mut eng, "SELECT generate_series(1, 3, 0)").unwrap_err();
    assert_eq!(err.code, "22023");
    let err = run(&mut eng, "SELECT generate_series(1)").unwrap_err();
    assert_eq!(err.code, "42883");
    let err = run(&mut eng, "SELECT generate_series('a', 'b')").unwrap_err();
    assert_eq!(err.code, "42883");
}

/// v0.47: GROUP BY ordinals resolve to select-list expressions (PG19
/// parse analysis), never to constants.
#[test]
fn v47_group_by_ordinal() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "SELECT id, count(*) FROM users GROUP BY 1 ORDER BY 1",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "1".to_string()],
            vec!["2".to_string(), "1".to_string()],
            vec!["3".to_string(), "1".to_string()],
        ]
    );
    // Out-of-range ordinals are 42803, like PG.
    let err = run(&mut eng, "SELECT id FROM users GROUP BY 5").unwrap_err();
    assert_eq!(err.code, "42803");
    let err = run(&mut eng, "SELECT id FROM users GROUP BY 0").unwrap_err();
    assert_eq!(err.code, "42803");
}

/// v0.47: an SRF in the GROUP BY keys expands below the aggregate
/// (PG19 ProjectSet under Agg): each input row fans out per SRF
/// element and aggregates fold the expanded rows.
#[test]
fn v47_group_by_srf_expands_below_agg() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE gs_ten(ten int)").unwrap();
    run(&mut eng, "INSERT INTO gs_ten VALUES (0), (1), (2), (1)").unwrap();
    // Expansions: 0 -> {}, 1 -> {1}, 2 -> {1,2}, 1 -> {1}:
    // g=1 has 3 rows, g=2 has 1 row.
    let r = run(
        &mut eng,
        "SELECT generate_series(1, ten) AS g, count(*) FROM gs_ten GROUP BY 1 ORDER BY 1",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "3".to_string()],
            vec!["2".to_string(), "1".to_string()],
        ]
    );
    // The explicit-expression form groups identically.
    let r = run(
            &mut eng,
            "SELECT generate_series(1, ten) AS g, count(*) FROM gs_ten GROUP BY generate_series(1, ten) ORDER BY 1",
        )
        .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "3".to_string()],
            vec!["2".to_string(), "1".to_string()],
        ]
    );
}
/// v0.49: a parenthesized expression as the first select-list item
/// parses as an expression, not a parenthesized query (PG19 gram.y:
/// only a target_list follows SELECT). Regression: `SELECT (-1)`
/// used to fail with "expected SELECT, found Minus".
#[test]
fn v49_paren_expr_first_select_item() {
    let mut eng = engine();
    let rows = rows_of(run(&mut eng, "SELECT (-1)").unwrap());
    assert_eq!(rows, vec![vec!["-1".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT (1+2)").unwrap());
    assert_eq!(rows, vec![vec!["3".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT (-1)::int").unwrap());
    assert_eq!(rows, vec![vec!["-1".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT (1+2)::int").unwrap());
    assert_eq!(rows, vec![vec!["3".to_string()]]);
    // Non-first position already worked; keep it covered.
    let rows = rows_of(run(&mut eng, "SELECT 1, (-1)::int").unwrap());
    assert_eq!(rows, vec![vec!["1".to_string(), "-1".to_string()]]);
}

/// v0.49: PG's one-byte `"char"` type in `::` casts after a
/// parenthesized operand — the protocol-36 B9 case.
#[test]
fn v49_quoted_char_cast_after_paren() {
    let mut eng = engine();
    // (-1)::"char" is i4tochar(-1): byte 255, out as `\377`.
    let rows = rows_of(run(&mut eng, r#"SELECT (-1)::"char""#).unwrap());
    assert_eq!(rows, vec![vec!["\\377".to_string()]]);
    let rows = rows_of(run(&mut eng, r#"SELECT 'a'::"char""#).unwrap());
    assert_eq!(rows, vec![vec!["a".to_string()]]);
    let rows = rows_of(run(&mut eng, r#"SELECT 65::"char""#).unwrap());
    assert_eq!(rows, vec![vec!["A".to_string()]]);
    // CAST(x AS "char") with the quoted name also works.
    let rows = rows_of(run(&mut eng, r#"SELECT CAST((-1) AS "char")"#).unwrap());
    assert_eq!(rows, vec![vec!["\\377".to_string()]]);
}

/// v0.49: `SELECT (SELECT ...)` is a scalar subquery expression, not
/// an unwrapped set branch — the 21000 multi-row check applies.
#[test]
fn v49_scalar_subquery_not_unwrapped() {
    let mut eng = engine();
    let err = run(&mut eng, "SELECT (SELECT id FROM users)").unwrap_err();
    assert_eq!(err.code, "21000");
    // Empty scalar subquery is NULL.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT (SELECT name FROM users WHERE id = 99) IS NULL AS e",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["t".to_string()]]);
    // Single-row scalar subquery evaluates in place.
    let rows = rows_of(run(&mut eng, "SELECT (SELECT 10 AS v) + 5").unwrap());
    assert_eq!(rows, vec![vec!["15".to_string()]]);
}

/// v0.50: integer arithmetic overflow detection, matching Postgres
/// int.c / int8.c (REL_19_STABLE): +,-,*,/ and INT_MIN/-1 raise 22003
/// with a per-type message; int2 pairs do NOT promote to int4.
#[test]
fn v50_int_overflow_detection() {
    use ArithOp::{Add, Div, Mod, Mul};
    let ovf = |op, a: Value, b: Value| {
        let e = eval_arith(op, &a, &b).unwrap_err();
        (e.code, e.message)
    };
    // int2 overflow: no promotion to int4.
    assert_eq!(
        ovf(Add, Value::SmallInt(30000), Value::SmallInt(30000)),
        ("22003", "smallint out of range".to_string())
    );
    assert_eq!(
        ovf(Mul, Value::SmallInt(-32768), Value::SmallInt(-1)),
        ("22003", "smallint out of range".to_string())
    );
    assert_eq!(
        ovf(Div, Value::SmallInt(-32768), Value::SmallInt(-1)),
        ("22003", "smallint out of range".to_string())
    );
    // int2 boundary results keep the smallint type.
    assert_eq!(
        eval_arith(Add, &Value::SmallInt(32767), &Value::SmallInt(0)).unwrap(),
        Value::SmallInt(32767)
    );
    assert_eq!(
        eval_arith(Mul, &Value::SmallInt(-32768), &Value::SmallInt(1)).unwrap(),
        Value::SmallInt(-32768)
    );
    // int4 overflow.
    assert_eq!(
        ovf(Add, Value::Int(i32::MAX as i64), Value::Int(1)),
        ("22003", "integer out of range".to_string())
    );
    assert_eq!(
        ovf(Mul, Value::Int(i32::MIN as i64), Value::Int(-1)),
        ("22003", "integer out of range".to_string())
    );
    // int8 overflow; INT_MIN % -1 is 0, not an error.
    assert_eq!(
        ovf(Add, Value::BigInt(i64::MAX), Value::BigInt(1)),
        ("22003", "bigint out of range".to_string())
    );
    assert_eq!(
        ovf(Div, Value::BigInt(i64::MIN), Value::BigInt(-1)),
        ("22003", "bigint out of range".to_string())
    );
    assert_eq!(
        eval_arith(Mod, &Value::BigInt(i64::MIN), &Value::BigInt(-1)).unwrap(),
        Value::BigInt(0)
    );
    // Mixed widths resolve to the wider type.
    assert_eq!(
        eval_arith(Add, &Value::SmallInt(30000), &Value::Int(30000)).unwrap(),
        Value::Int(60000)
    );
    // Division by zero is still 22012, not 22003.
    let e = eval_arith(Div, &Value::Int(1), &Value::Int(0)).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("22012", "division by zero"));
}

/// v0.51: int2 bitwise operators (`& | # << >>`) return int2, like
/// PG19 int.c (int2and/int2or/int2xor/int2shl/int2shr all
/// PG_RETURN_INT16) — no promotion to int4. Shifts use C
/// wrap-then-truncate semantics (integer promotion to int, shift in
/// 32 bits, truncate to int16), never 22003: 1::int2 << 15 is
/// -32768. int4/int8 bitwise already return their own width (only
/// asserted here).
#[test]
fn v51_int2_bitwise_returns_int2() {
    use ArithOp::{BitAnd, BitOr, BitXor, Shl, Shr};
    // & | # stay int2; never overflow.
    assert_eq!(
        eval_arith(BitAnd, &Value::SmallInt(0b1100), &Value::SmallInt(0b1010)).unwrap(),
        Value::SmallInt(0b1000)
    );
    assert_eq!(
        eval_arith(BitOr, &Value::SmallInt(0b1100), &Value::SmallInt(0b1010)).unwrap(),
        Value::SmallInt(0b1110)
    );
    assert_eq!(
        eval_arith(BitXor, &Value::SmallInt(0b1100), &Value::SmallInt(0b1010)).unwrap(),
        Value::SmallInt(0b0110)
    );
    assert_eq!(
        eval_arith(BitAnd, &Value::SmallInt(-1), &Value::SmallInt(0x7fff)).unwrap(),
        Value::SmallInt(0x7fff)
    );
    // << wraps into the sign bit: 1 << 15 = 32768 -> -32768, not 22003.
    assert_eq!(
        eval_arith(Shl, &Value::SmallInt(1), &Value::SmallInt(15)).unwrap(),
        Value::SmallInt(-32768)
    );
    // 20000 << 2 = 80000 = 0x13880 -> truncate to 0x3880 = 14464.
    assert_eq!(
        eval_arith(Shl, &Value::SmallInt(20000), &Value::SmallInt(2)).unwrap(),
        Value::SmallInt(14464)
    );
    // >> is an arithmetic right shift after promotion.
    assert_eq!(
        eval_arith(Shr, &Value::SmallInt(-1), &Value::SmallInt(1)).unwrap(),
        Value::SmallInt(-1)
    );
    assert_eq!(
        eval_arith(Shr, &Value::SmallInt(-32768), &Value::SmallInt(15)).unwrap(),
        Value::SmallInt(-1)
    );
    // int4 / int8 pairs return their own width (unchanged semantics).
    assert_eq!(
        eval_arith(Shl, &Value::Int(1), &Value::Int(31)).unwrap(),
        Value::Int(-2147483648)
    );
    assert_eq!(
        eval_arith(BitOr, &Value::Int(5), &Value::Int(2)).unwrap(),
        Value::Int(7)
    );
    assert_eq!(
        eval_arith(BitAnd, &Value::BigInt(-1), &Value::BigInt(42)).unwrap(),
        Value::BigInt(42)
    );
    // Mixed widths still resolve to the wider type.
    assert_eq!(
        eval_arith(BitOr, &Value::SmallInt(5), &Value::Int(2)).unwrap(),
        Value::Int(7)
    );
    // Unary ~ on int2 still promotes to int4 (PG has no int2not).
    assert_eq!(
        eval_bitnot_val(&Value::SmallInt(5)).unwrap(),
        Value::Int(!5i64)
    );
}

/// v0.52: `SELECT DISTINCT ON` basics — one row per group of equal key
/// expressions (NULLs group), the ORDER BY winner kept. Verified
/// against PostgreSQL 16.2 (2026-09-17).
#[test]
fn v52_distinct_on_basics() {
    let mut eng = engine();
    let mut q = |e: &str| rows_of(run(&mut eng, e).unwrap());
    // One row per uid: the smallest amt.
    assert_eq!(
        q("SELECT DISTINCT ON (uid) uid, amt FROM orders ORDER BY uid, amt"),
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
    // DESC picks the largest amt per group.
    assert_eq!(
        q("SELECT DISTINCT ON (uid) uid, amt FROM orders ORDER BY uid, amt DESC"),
        vec![
            vec!["1".to_string(), "20".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
    // The DISTINCT ON expression need not be projected, and ORDER BY
    // may name non-projected columns (no 42703, unlike plain DISTINCT).
    assert_eq!(
        q("SELECT DISTINCT ON (uid) amt FROM orders ORDER BY uid, amt"),
        vec![vec!["10".to_string()], vec!["5".to_string()]]
    );
    // Multiple DISTINCT ON expressions.
    assert_eq!(
        q("SELECT DISTINCT ON (uid, amt) uid, amt FROM orders ORDER BY uid, amt"),
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["1".to_string(), "20".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
    // ORDER BY ordinals resolve for the prefix check.
    assert_eq!(
        q("SELECT DISTINCT ON (uid) uid, amt FROM orders ORDER BY 1, 2"),
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
    // Output aliases resolve for the prefix check.
    assert_eq!(
        q("SELECT DISTINCT ON (uid) uid AS u, amt FROM orders ORDER BY u, amt"),
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
    // Expression keys: buckets by amt % 2, smallest amt per bucket.
    assert_eq!(
        q("SELECT DISTINCT ON (amt % 2) amt FROM orders ORDER BY amt % 2, amt"),
        vec![vec!["10".to_string()], vec!["5".to_string()]]
    );
    // Works through a derived table (the corpus shape).
    assert_eq!(
        q("SELECT * FROM (SELECT DISTINCT ON (uid) uid, amt FROM orders ORDER BY uid, amt) s"),
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
    // LIMIT applies after the first-row-per-group filter.
    assert_eq!(
        q("SELECT DISTINCT ON (uid) uid, amt FROM orders ORDER BY uid, amt LIMIT 1"),
        vec![vec!["1".to_string(), "10".to_string()]]
    );
}

/// v0.52: NULL DISTINCT ON keys group together (NULLs equal, like PG).
#[test]
fn v52_distinct_on_nulls_group() {
    let mut eng = engine();
    run(&mut eng, "INSERT INTO orders VALUES (7, NULL, 30)").unwrap();
    run(&mut eng, "INSERT INTO orders VALUES (8, NULL, 40)").unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT ON (uid) uid, amt FROM orders ORDER BY uid, amt",
        )
        .unwrap(),
    );
    // ASC puts NULLs last; the two NULL-uid rows form one group and
    // the smallest amt (30) wins.
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "5".to_string()],
            vec!["NULL".to_string(), "30".to_string()],
        ]
    );
}

/// v0.52: without ORDER BY, PG still sorts by the distinct keys (the
/// "arbitrary" first row is the first after that sort).
#[test]
fn v52_distinct_on_no_order_by() {
    let mut eng = engine();
    // Output comes out ordered by the distinct key, like PG.
    let rows = rows_of(run(&mut eng, "SELECT DISTINCT ON (uid) uid FROM orders").unwrap());
    assert_eq!(rows, vec![vec!["1".to_string()], vec!["2".to_string()]]);
    // The winner is the first row after sorting by the key: uid=1's
    // smallest id (1, amt 10) beats (2, amt 20).
    let rows = rows_of(run(&mut eng, "SELECT DISTINCT ON (uid) uid, amt FROM orders").unwrap());
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
}

/// v0.52: the DISTINCT ON prefix rule is PG19's set-prefix rule, not
/// positional: ORDER BY may permute the keys, may name fewer keys than
/// DISTINCT ON (the rest are appended implicitly), but a key after a
/// non-key term — or an unreached key with a non-key tail — is 42803.
#[test]
fn v52_distinct_on_order_prefix_42803() {
    let mut eng = engine();
    let mut e = |s: &str| run(&mut eng, s).unwrap_err();
    // Key after a non-key term: 42803.
    let err = e("SELECT DISTINCT ON (uid) uid, amt FROM orders ORDER BY amt, uid");
    assert_eq!(err.code, "42803");
    assert!(
        err.message
            .contains("must match initial ORDER BY expressions")
    );
    // Structurally different expression: 42803.
    let err = e("SELECT DISTINCT ON (uid) uid, amt FROM orders ORDER BY uid + 0, amt");
    assert_eq!(err.code, "42803");
    // Unreached key with a non-key tail: 42803.
    let err = e("SELECT DISTINCT ON (uid, amt) uid, amt FROM orders ORDER BY uid, amt + 100");
    assert_eq!(err.code, "42803");
    // Permuted keys: fine.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT ON (uid, amt) uid, amt FROM orders ORDER BY amt, uid",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["2".to_string(), "5".to_string()],
            vec!["1".to_string(), "10".to_string()],
            vec!["1".to_string(), "20".to_string()],
        ]
    );
    // Short prefix: the missing key is appended implicitly (ASC).
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT ON (uid, amt) uid, amt FROM orders ORDER BY uid",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["1".to_string(), "20".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
    // Duplicate keys and duplicate ORDER BY terms: fine.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT DISTINCT ON (uid, uid) uid, amt FROM orders ORDER BY uid, uid, amt",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
}

/// v0.52: DISTINCT ON supports GROUP BY and aggregates (PG19) — the
/// filter runs above the grouping, keyed by group-level expressions.
#[test]
fn v52_distinct_on_group_by() {
    let mut eng = engine();
    let mut q = |e: &str| rows_of(run(&mut eng, e).unwrap());
    // One row per group, like a plain DISTINCT ON over the groups.
    assert_eq!(
        q("SELECT DISTINCT ON (uid) uid, count(*) FROM orders GROUP BY uid"),
        vec![
            vec!["1".to_string(), "2".to_string()],
            vec!["2".to_string(), "1".to_string()],
        ]
    );
    // ORDER BY picks the winner among groups.
    assert_eq!(
        q("SELECT DISTINCT ON (uid) uid, max(amt) FROM orders GROUP BY uid ORDER BY uid"),
        vec![
            vec!["1".to_string(), "20".to_string()],
            vec!["2".to_string(), "5".to_string()],
        ]
    );
    // An expression key over group columns.
    assert_eq!(
        q("SELECT DISTINCT ON (uid + 0) uid, count(*) FROM orders GROUP BY uid"),
        vec![
            vec!["1".to_string(), "2".to_string()],
            vec!["2".to_string(), "1".to_string()],
        ]
    );
    // An aggregate key: the counts (2 and 1) are distinct, so both
    // groups survive; ORDER BY count(*) orders them.
    assert_eq!(
        q("SELECT DISTINCT ON (count(*)) uid, count(*) FROM orders GROUP BY uid ORDER BY count(*)"),
        vec![
            vec!["2".to_string(), "1".to_string()],
            vec!["1".to_string(), "2".to_string()],
        ]
    );
    // A non-grouped column in the DISTINCT ON key is the ordinary
    // grouping error (PG19 42803).
    let err = run(
        &mut eng,
        "SELECT DISTINCT ON (amt) uid, count(*) FROM orders GROUP BY uid",
    )
    .unwrap_err();
    assert_eq!(err.code, "42803");
    // An aggregate key without GROUP BY makes the whole query
    // aggregated, so the bare column is the grouping error.
    let err = run(&mut eng, "SELECT DISTINCT ON (count(*)) uid FROM orders").unwrap_err();
    assert_eq!(err.code, "42803");
}

/// v0.52: window functions are legal inside DISTINCT ON expressions
/// (PG19) — the key reads the precomputed window value.
#[test]
fn v52_distinct_on_window_key() {
    let mut eng = engine();
    // ranks by amt: (2,5)->1, (1,10)->2, (1,20)->3 — all distinct.
    let rows = rows_of(
            run(
                &mut eng,
                "SELECT DISTINCT ON (rank() OVER (ORDER BY amt)) uid, amt FROM orders ORDER BY rank() OVER (ORDER BY amt), uid",
            )
            .unwrap(),
        );
    assert_eq!(
        rows,
        vec![
            vec!["2".to_string(), "5".to_string()],
            vec!["1".to_string(), "10".to_string()],
            vec!["1".to_string(), "20".to_string()],
        ]
    );
}

/// v0.52: DISTINCT ON shape rejections — FOR UPDATE (0A000), the
/// mandatory parens (42601), and the comma-after-ON syntax PG rejects
/// (42601). Note: the v0.52 assignment brief claimed
/// `SELECT DISTINCT ON (a), b` is valid, but real PostgreSQL rejects
/// the comma — this implementation follows PostgreSQL.
#[test]
fn v52_distinct_on_rejects_bad_shapes() {
    let mut eng = engine();
    let mut e = |s: &str| run(&mut eng, s).unwrap_err();
    assert_eq!(
        e("SELECT DISTINCT ON (uid) uid FROM orders FOR UPDATE").code,
        "0A000"
    );
    assert_eq!(e("SELECT DISTINCT ON uid, amt FROM orders").code, "42601");
    assert_eq!(e("SELECT DISTINCT ON (uid), amt FROM orders").code, "42601");
}

#[test]
fn v68_regex_operators_match_pg() {
    // PG19 textregexeq family: `~` / `!~` / `~*` / `!~*`.
    let mut eng = engine();
    let mut one = |sql: &str| rows_of(run(&mut eng, sql).unwrap())[0][0].clone();
    assert_eq!(one("select 'abc' ~ '.*'"), "t");
    assert_eq!(one("select 'abc' !~ '.*'"), "f");
    assert_eq!(one("select 'ABC' ~ 'abc'"), "f");
    assert_eq!(one("select 'ABC' ~* 'abc'"), "t");
    assert_eq!(one("select 'ABC' !~* 'abc'"), "f");
    assert_eq!(one("select 'a1' ~ '[0-9]'"), "t");
    assert_eq!(one("select 'asdfghjkl;' ~ '.*asdf.*'"), "t");
    // Anchors and alternation.
    assert_eq!(one("select 'abc' ~ '^a.c$'"), "t");
    assert_eq!(one("select 'abc' ~ '^(a|b)c$'"), "f");
    // NULL propagates (PG three-valued logic).
    assert_eq!(one("select NULL ~ 'x'"), "NULL");
    assert_eq!(one("select 'x' ~ NULL"), "NULL");
    // Precedence: `~` binds tighter than `=` and LIKE, looser than `||`.
    assert_eq!(one("select 'ab' ~ '^a' = true"), "t");
    assert_eq!(one("select 'ab' ~ 'a' || 'b'"), "t");
    // Works in WHERE over a table scan.
    let rows = rows_of(run(&mut eng, "select name from users where name ~ '^[ab]'").unwrap());
    let names: Vec<_> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(names, vec!["ann", "bob"]);
}

#[test]
fn v68_regex_operators_errors() {
    // Invalid pattern is 2201B, like the regexp_* functions.
    let mut eng = engine();
    let err = run(&mut eng, "select 'abc' ~ '('").unwrap_err();
    assert_eq!(err.code, "2201B");
    // Non-text operands are 42883 with the operator spelled out.
    let err = run(&mut eng, "select 1 ~ 'x'").unwrap_err();
    assert_eq!(err.code, "42883");
    assert!(
        err.message.contains("integer ~ text"),
        "got: {}",
        err.message
    );
    let err = run(&mut eng, "select 'x' !~* 1").unwrap_err();
    assert_eq!(err.code, "42883");
    assert!(
        err.message.contains("text !~* integer"),
        "got: {}",
        err.message
    );
}

#[test]
fn v68_regex_expr_codec_roundtrip() {
    // The new Expr variant survives the CHECK-constraint s-expr codec
    // (used by WAL checkpoints).
    let mut eng = engine();
    run(&mut eng, "create table rgx (a text check (a ~* 'x'))").unwrap();
    let tbl = &eng.db.tables["rgx"][0];
    let enc = crate::sql::encode_constraints(tbl);
    assert!(
        enc.contains("(regex 0 1 "),
        "codec must tag the regex op: {}",
        enc
    );
    let dec = crate::sql::decode_constraints(&enc).unwrap();
    let before = format!("{:?}", tbl.checks);
    let after = format!("{:?}", dec.checks);
    assert_eq!(before, after);
}

#[test]
fn v68_regex_in_check_constraint() {
    // CHECK (a ~* 'x') enforces end to end.
    let mut eng = engine();
    run(&mut eng, "create table rgxc (a text check (a ~* 'x'))").unwrap();
    run(&mut eng, "insert into rgxc values ('xyz')").unwrap();
    let err = run(&mut eng, "insert into rgxc values ('abc')").unwrap_err();
    assert_eq!(err.code, "23514");
}

/// v0.76: UPDATE ... FROM — SET/WHERE/RETURNING resolve the FROM
/// tables (PG19). Regression test for the protocol-78 A3 failure
/// (`column "delta" does not exist` on
/// `UPDATE u78a SET code = code + delta FROM u78b WHERE ...`).
#[test]
fn v76_update_from_returning_sees_from() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a(id int, code int)").unwrap();
    run(&mut eng, "CREATE TABLE b(id int, delta int)").unwrap();
    run(&mut eng, "INSERT INTO a VALUES (1, 10), (2, 20)").unwrap();
    run(&mut eng, "INSERT INTO b VALUES (1, 5), (2, 7)").unwrap();
    let r = run(
        &mut eng,
        "UPDATE a SET code = code + delta FROM b WHERE a.id = b.id RETURNING a.id, b.delta",
    )
    .unwrap();
    let (tag, cols, rows) = match r {
        ExecResult::Dml { tag, columns, rows } => (tag, columns, rows),
        _ => panic!("expected Dml"),
    };
    assert_eq!(tag, "UPDATE 2");
    let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["id", "delta"]);
    let mut got: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                .collect()
        })
        .collect();
    got.sort();
    assert_eq!(got, vec![vec!["1", "5"], vec!["2", "7"]]);
    // The SET expressions themselves applied the FROM values.
    let got = rows_of(run(&mut eng, "SELECT id, code FROM a ORDER BY id").unwrap());
    assert_eq!(got, vec![vec!["1", "15"], vec!["2", "27"]]);
}

/// v0.76: UPDATE with a target alias — the alias is the visible
/// qualifier in SET/WHERE/RETURNING (PG19).
#[test]
fn v76_update_alias_from_returning() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a(id int, code int)").unwrap();
    run(&mut eng, "CREATE TABLE b(id int, delta int)").unwrap();
    run(&mut eng, "INSERT INTO a VALUES (1, 10), (2, 20)").unwrap();
    run(&mut eng, "INSERT INTO b VALUES (1, 5), (2, 7)").unwrap();
    let r = run(
        &mut eng,
        "UPDATE a AS x SET code = code + b.delta FROM b WHERE x.id = b.id RETURNING x.id, x.code",
    )
    .unwrap();
    let rows = match r {
        ExecResult::Dml { rows, .. } => rows,
        _ => panic!("expected Dml"),
    };
    let mut got: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                .collect()
        })
        .collect();
    got.sort();
    assert_eq!(got, vec![vec!["1", "15"], vec!["2", "27"]]);
}

/// v0.76: the Describe path for UPDATE ... FROM ... RETURNING
/// resolves FROM columns (the protocol-78 A3 failure was in
/// statement description, SQLSTATE 42703).
#[test]
fn v76_describe_update_from_returning() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a(id int, code int)").unwrap();
    run(&mut eng, "CREATE TABLE b(id int, delta int)").unwrap();
    let stmt = parse_statement(
        "UPDATE a SET code = code + delta FROM b WHERE a.id = b.id RETURNING a.id, b.delta",
    )
    .unwrap();
    let snap = eng.take_snapshot();
    let cols = describe_columns(&stmt, &[], &eng, &snap, 9, 0)
        .unwrap()
        .expect("RETURNING describes columns");
    let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["id", "delta"]);
    assert!(cols.iter().all(|(_, ty)| *ty == ColType::Int));
}

/// v0.76: DELETE ... USING — RETURNING sees the USING tables (PG19).
#[test]
fn v76_delete_using_returning_sees_using() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a(id int, code int)").unwrap();
    run(&mut eng, "CREATE TABLE b(id int, delta int)").unwrap();
    run(&mut eng, "INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    run(&mut eng, "INSERT INTO b VALUES (1, 5), (3, 9)").unwrap();
    let r = run(
        &mut eng,
        "DELETE FROM a USING b WHERE a.id = b.id RETURNING a.id, b.delta",
    )
    .unwrap();
    let (tag, rows) = match r {
        ExecResult::Dml { tag, rows, .. } => (tag, rows),
        _ => panic!("expected Dml"),
    };
    assert_eq!(tag, "DELETE 2");
    let mut got: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                .collect()
        })
        .collect();
    got.sort();
    assert_eq!(got, vec![vec!["1", "5"], vec!["3", "9"]]);
    let left = rows_of(run(&mut eng, "SELECT id FROM a").unwrap());
    assert_eq!(left, vec![vec!["2"]]);
}

/// v0.76: DELETE ... USING without WHERE deletes every target row
/// (PG19 cross-product semantics); RETURNING sees the first USING
/// row deterministically.
#[test]
fn v76_delete_using_no_where_returns_first_using_row() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a(id int)").unwrap();
    run(&mut eng, "CREATE TABLE b(id int, delta int)").unwrap();
    run(&mut eng, "INSERT INTO a VALUES (1), (2)").unwrap();
    run(&mut eng, "INSERT INTO b VALUES (9, 99), (8, 88)").unwrap();
    let r = run(&mut eng, "DELETE FROM a USING b RETURNING a.id, b.delta").unwrap();
    let (tag, rows) = match r {
        ExecResult::Dml { tag, rows, .. } => (tag, rows),
        _ => panic!("expected Dml"),
    };
    assert_eq!(tag, "DELETE 2");
    let mut got: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                .collect()
        })
        .collect();
    got.sort();
    assert_eq!(got, vec![vec!["1", "99"], vec!["2", "99"]]);
}

/// v0.76: DELETE ... USING with an empty USING table matches nothing
/// (PG19: no USING rows means no target row can satisfy the implicit
/// cross product).
#[test]
fn v76_delete_using_empty_matches_nothing() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a(id int)").unwrap();
    run(&mut eng, "CREATE TABLE b(id int)").unwrap();
    run(&mut eng, "INSERT INTO a VALUES (1), (2)").unwrap();
    let r = run(&mut eng, "DELETE FROM a USING b").unwrap();
    let tag = match r {
        ExecResult::Dml { tag, .. } => tag,
        _ => panic!("expected Dml"),
    };
    assert_eq!(tag, "DELETE 0");
    let left = rows_of(run(&mut eng, "SELECT id FROM a ORDER BY id").unwrap());
    assert_eq!(left, vec![vec!["1"], vec!["2"]]);
}

/// v0.76: `ADD CONSTRAINT ... NOT NULL ... NOT VALID` skips the
/// existing-row scan but still enforces the constraint on future
/// writes (PG19), via check_row_constraints.
#[test]
fn v76_not_null_not_valid_enforced_on_writes() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t(id int, v int)").unwrap();
    run(&mut eng, "INSERT INTO t VALUES (1, NULL)").unwrap();
    run(
        &mut eng,
        "ALTER TABLE t ADD CONSTRAINT nn_v NOT NULL v NOT VALID",
    )
    .unwrap();
    // Existing NULL row survives the NOT VALID add.
    let got = rows_of(run(&mut eng, "SELECT id, v FROM t").unwrap());
    assert_eq!(got, vec![vec!["1", "NULL"]]);
    // Future NULL writes are rejected.
    let err = run(&mut eng, "INSERT INTO t VALUES (2, NULL)").unwrap_err();
    assert_eq!(err.code, "23502");
    let err = run(&mut eng, "UPDATE t SET v = NULL WHERE id = 1").unwrap_err();
    assert_eq!(err.code, "23502");
    // Valid writes still work.
    run(&mut eng, "INSERT INTO t VALUES (2, 5)").unwrap();
    run(&mut eng, "UPDATE t SET v = 7 WHERE id = 2").unwrap();
    let got = rows_of(run(&mut eng, "SELECT id, v FROM t ORDER BY id").unwrap());
    assert_eq!(got, vec![vec!["1", "NULL"], vec!["2", "7"]]);
}

/// v0.76: timestamp precision rounds half away from zero, like PG19
/// AdjustTimestampForTypmod (it does not truncate).
#[test]
fn v76_round_micros_to_precision() {
    assert_eq!(round_micros_to_precision(1_234_567, None), 1_234_567);
    // Ties round up (away from zero).
    assert_eq!(round_micros_to_precision(1_500_000, Some(0)), 2_000_000);
    assert_eq!(round_micros_to_precision(1_499_999, Some(0)), 1_000_000);
    // Symmetric for negative timestamps.
    assert_eq!(round_micros_to_precision(-1_500_000, Some(0)), -2_000_000);
    assert_eq!(round_micros_to_precision(-1_499_999, Some(0)), -1_000_000);
    assert_eq!(round_micros_to_precision(-1_400_000, Some(0)), -1_000_000);
    // Sub-second scales.
    assert_eq!(round_micros_to_precision(1_234_567, Some(3)), 1_235_000);
    assert_eq!(round_micros_to_precision(1_234_499, Some(3)), 1_234_000);
    assert_eq!(round_micros_to_precision(1_234_567, Some(6)), 1_234_567);
}

/// v0.76: only CURRENT_TIMESTAMP takes the optional precision in
/// PG19 — now()/clock_timestamp()/statement_timestamp()/
/// transaction_timestamp() are 0-argument functions (42883).
#[test]
fn v76_timestamp_precision_arity_pg19() {
    let mut eng = engine();
    // current_timestamp(3) works and returns a value.
    assert!(run(&mut eng, "SELECT current_timestamp(3)").is_ok());
    assert!(run(&mut eng, "SELECT current_timestamp").is_ok());
    // The rest are 0-argument in PG19: the parse layer raises 42883
    // (the run() harness flattens parse codes to 42601).
    for q in [
        "SELECT now(3)",
        "SELECT clock_timestamp(1)",
        "SELECT statement_timestamp(0)",
        "SELECT transaction_timestamp(6)",
    ] {
        assert!(run(&mut eng, q).is_err(), "{}", q);
        assert_eq!(parse_statement(q).unwrap_err().code, "42883", "{}", q);
    }
    assert!(run(&mut eng, "SELECT now()").is_ok());
    assert!(run(&mut eng, "SELECT clock_timestamp()").is_ok());
}

/// v0.76: the VALUES form of quantified comparisons desugars ANY/SOME
/// to OR and ALL to AND, preserving NULL three-valued logic.
#[test]
fn v76_quantified_values_forms() {
    let mut eng = engine();
    let one = |eng: &mut Engine, sql: &str| rows_of(run(eng, sql).unwrap());
    assert_eq!(
        one(&mut eng, "SELECT 1 = ANY (VALUES (2), (3))"),
        vec![vec!["f"]]
    );
    assert_eq!(
        one(&mut eng, "SELECT 1 = ANY (VALUES (1), (3))"),
        vec![vec!["t"]]
    );
    assert_eq!(
        one(&mut eng, "SELECT 1 = SOME (VALUES (1), (3))"),
        vec![vec!["t"]]
    );
    assert_eq!(
        one(&mut eng, "SELECT 1 = ALL (VALUES (1), (1))"),
        vec![vec!["t"]]
    );
    assert_eq!(
        one(&mut eng, "SELECT 1 = ALL (VALUES (1), (2))"),
        vec![vec!["f"]]
    );
    // NULL three-valued logic survives the desugar.
    assert_eq!(
        one(&mut eng, "SELECT NULL = ANY (VALUES (NULL))"),
        vec![vec!["NULL"]]
    );
    assert_eq!(
        one(&mut eng, "SELECT 1 = ANY (VALUES (NULL))"),
        vec![vec!["NULL"]]
    );
    assert_eq!(
        one(&mut eng, "SELECT 1 = ALL (VALUES (1), (NULL))"),
        vec![vec!["NULL"]]
    );
}

/// v0.76: `Stmt::max_param` sees parameters inside UPDATE ... FROM
/// and DELETE ... USING items (derived tables, functions, VALUES),
/// so Bind sizes the parameter vector correctly.
#[test]
fn v76_max_param_sees_from_and_using() {
    let u = parse_statement("UPDATE a SET x = 1 FROM (SELECT $1 AS y) s WHERE a.id = s.y").unwrap();
    assert_eq!(u.max_param(), 1);
    let d = parse_statement("DELETE FROM a USING (SELECT $2 AS y) s WHERE a.id = s.y").unwrap();
    assert_eq!(d.max_param(), 2);
}

/// v0.76: the NOT VALID flag on check constraints survives the
/// checkpoint encode/decode round trip, and pre-v0.76 checkpoints
/// that omit the flag decode as not_valid = false.
#[test]
fn v76_check_not_valid_encode_decode_roundtrip() {
    use crate::sql::{CheckDef, Expr, Literal, decode_constraints, encode_constraints};
    let mut t = Table::new(vec![("v".to_string(), ColType::Int)], 1);
    t.checks.push(CheckDef {
        name: "c_valid".to_string(),
        expr: Expr::Literal(Literal::Int(1)),
        not_valid: false,
        kind: crate::sql::CheckKind::Check,
    });
    t.checks.push(CheckDef {
        name: "c_nv".to_string(),
        expr: Expr::Literal(Literal::Int(1)),
        not_valid: true,
        kind: crate::sql::CheckKind::Check,
    });
    // v0.77: a NOT NULL-kind check round-trips its kind.
    t.checks.push(CheckDef {
        name: "c_nn".to_string(),
        expr: Expr::IsNull {
            expr: Box::new(Expr::Column {
                table: None,
                name: "v".to_string(),
            }),
            neg: true,
        },
        not_valid: true,
        kind: crate::sql::CheckKind::NotNull,
    });
    let encoded = encode_constraints(&t);
    let dec = decode_constraints(&encoded).unwrap();
    let flags: Vec<(String, bool)> = dec
        .checks
        .iter()
        .map(|c| (c.name.clone(), c.not_valid))
        .collect();
    assert_eq!(
        flags,
        vec![
            ("c_valid".to_string(), false),
            ("c_nv".to_string(), true),
            ("c_nn".to_string(), true)
        ]
    );
    let kinds: Vec<crate::sql::CheckKind> = dec.checks.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        vec![
            crate::sql::CheckKind::Check,
            crate::sql::CheckKind::Check,
            crate::sql::CheckKind::NotNull
        ]
    );
    // Backward compatibility: strip the new flag and kind (as a
    // pre-v0.76 checkpoint would) and confirm they decode as false
    // / CHECK.
    let legacy = encoded.replace("(\"c_nv\" (lit int 1) 1 c)", "(\"c_nv\" (lit int 1))");
    assert_ne!(legacy, encoded);
    let dec = decode_constraints(&legacy).unwrap();
    let nv = dec.checks.iter().find(|c| c.name == "c_nv").unwrap();
    assert!(!nv.not_valid);
    assert_eq!(nv.kind, crate::sql::CheckKind::Check);
}

/// v0.77: ALTER TABLE ... ADD CONSTRAINT ... NOT NULL [NOT VALID]
/// on a TEMP table (was 0A000 before v0.77). Mirrors the
/// subselect.sql NOT VALID suite that drove this work.
#[test]
fn v77_temp_alter_add_constraint_not_null_not_valid() {
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE t77a (id int)").unwrap();
    run(&mut eng, "INSERT INTO t77a VALUES (NULL)").unwrap();
    run(
        &mut eng,
        "ALTER TABLE t77a ADD CONSTRAINT nn NOT NULL id NOT VALID",
    )
    .unwrap();
    // The existing NULL row survives NOT VALID.
    let got = rows_of(run(&mut eng, "SELECT id FROM t77a").unwrap());
    assert_eq!(got, vec![vec!["NULL".to_string()]]);
    // Future NULL writes are rejected as 23502 (CheckKind::NotNull).
    let err = run(&mut eng, "INSERT INTO t77a VALUES (NULL)").unwrap_err();
    assert_eq!(err.code, "23502");
    run(&mut eng, "INSERT INTO t77a VALUES (1)").unwrap();
    let err = run(&mut eng, "UPDATE t77a SET id = NULL").unwrap_err();
    assert_eq!(err.code, "23502");
    // Without NOT VALID, existing NULLs are rejected at ALTER time.
    run(&mut eng, "CREATE TEMP TABLE t77b (id int)").unwrap();
    run(&mut eng, "INSERT INTO t77b VALUES (NULL)").unwrap();
    let err = run(&mut eng, "ALTER TABLE t77b ADD CONSTRAINT nn NOT NULL id").unwrap_err();
    assert_eq!(err.code, "23502");
    // DROP CONSTRAINT removes the enforcement.
    run(&mut eng, "ALTER TABLE t77a DROP CONSTRAINT nn").unwrap();
    run(&mut eng, "INSERT INTO t77a VALUES (NULL)").unwrap();
    let got = rows_of(run(&mut eng, "SELECT count(*) FROM t77a").unwrap());
    assert_eq!(got, vec![vec!["3".to_string()]]);
}

/// v0.77: row-rewriting ALTERs (ADD/DROP COLUMN) on temp tables keep
/// the data; RENAME COLUMN / RENAME TO work on temp tables.
#[test]
fn v77_temp_alter_add_drop_rename_column() {
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE t77c (id int)").unwrap();
    run(&mut eng, "INSERT INTO t77c VALUES (1), (2)").unwrap();
    run(&mut eng, "ALTER TABLE t77c ADD COLUMN v int DEFAULT 7").unwrap();
    let got = rows_of(run(&mut eng, "SELECT id, v FROM t77c ORDER BY id").unwrap());
    assert_eq!(got, vec![vec!["1", "7"], vec!["2", "7"]]);
    // ADD COLUMN NOT NULL without a default fails on existing rows.
    let err = run(&mut eng, "ALTER TABLE t77c ADD COLUMN w int NOT NULL").unwrap_err();
    assert_eq!(err.code, "23502");
    run(&mut eng, "ALTER TABLE t77c DROP COLUMN v").unwrap();
    let got = rows_of(run(&mut eng, "SELECT id FROM t77c ORDER BY id").unwrap());
    assert_eq!(got, vec![vec!["1"], vec!["2"]]);
    run(&mut eng, "ALTER TABLE t77c RENAME COLUMN id TO num").unwrap();
    let got = rows_of(run(&mut eng, "SELECT num FROM t77c ORDER BY num").unwrap());
    assert_eq!(got, vec![vec!["1"], vec!["2"]]);
    run(&mut eng, "ALTER TABLE t77c RENAME TO t77c2").unwrap();
    let err = run(&mut eng, "SELECT num FROM t77c").unwrap_err();
    assert_eq!(err.code, "42P01");
    let got = rows_of(run(&mut eng, "SELECT num FROM t77c2 ORDER BY num").unwrap());
    assert_eq!(got, vec![vec!["1"], vec!["2"]]);
}

/// v0.77: ADD CONSTRAINT UNIQUE / PRIMARY KEY on a temp table —
/// duplicates are rejected by a row scan (temp tables have no
/// backing global indexes), and the constraint is scan-enforced on
/// later writes.
#[test]
fn v77_temp_alter_add_unique() {
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE t77d (id int, v int)").unwrap();
    run(&mut eng, "INSERT INTO t77d VALUES (1, 1), (2, 2)").unwrap();
    run(&mut eng, "ALTER TABLE t77d ADD CONSTRAINT uq UNIQUE (id)").unwrap();
    // Scan-enforced going forward (no backing index on temp tables).
    let err = run(&mut eng, "INSERT INTO t77d VALUES (1, 9)").unwrap_err();
    assert_eq!(err.code, "23505");
    // NULLs stay distinct, like PG.
    run(&mut eng, "INSERT INTO t77d VALUES (NULL, 3)").unwrap();
    run(&mut eng, "INSERT INTO t77d VALUES (NULL, 4)").unwrap();
    // Duplicates present at ADD time are rejected.
    run(&mut eng, "CREATE TEMP TABLE t77e (id int)").unwrap();
    run(&mut eng, "INSERT INTO t77e VALUES (1), (1)").unwrap();
    let err = run(&mut eng, "ALTER TABLE t77e ADD CONSTRAINT uq2 UNIQUE (id)").unwrap_err();
    assert_eq!(err.code, "23505");
    // PRIMARY KEY on a temp table too.
    run(&mut eng, "CREATE TEMP TABLE t77f (id int)").unwrap();
    run(&mut eng, "INSERT INTO t77f VALUES (1), (2)").unwrap();
    run(
        &mut eng,
        "ALTER TABLE t77f ADD CONSTRAINT pk PRIMARY KEY (id)",
    )
    .unwrap();
    let err = run(&mut eng, "INSERT INTO t77f VALUES (2)").unwrap_err();
    assert_eq!(err.code, "23505");
}

/// v0.77: the `CheckKind` marker — not the expression shape — decides
/// 23502 vs 23514. An ordinary user CHECK with an `IS NOT NULL`
/// shape stays 23514; an ALTER-added NOT NULL reports 23502.
#[test]
fn v77_checkkind_notnull_vs_check_shape() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t77g (id int)").unwrap();
    run(
        &mut eng,
        "ALTER TABLE t77g ADD CONSTRAINT ck CHECK (id IS NOT NULL)",
    )
    .unwrap();
    let err = run(&mut eng, "INSERT INTO t77g VALUES (NULL)").unwrap_err();
    assert_eq!(err.code, "23514");
    run(&mut eng, "CREATE TABLE t77h (id int)").unwrap();
    run(&mut eng, "ALTER TABLE t77h ADD CONSTRAINT nn NOT NULL id").unwrap();
    let err = run(&mut eng, "INSERT INTO t77h VALUES (NULL)").unwrap_err();
    assert_eq!(err.code, "23502");
}

/// v0.77: an unqualified column present in both the UPDATE target and
/// a FROM item is ambiguous (42702); qualified references resolve.
#[test]
fn v77_update_from_ambiguous_column_is_42702() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a77 (id int, v int)").unwrap();
    run(&mut eng, "CREATE TABLE b77 (id int, v int)").unwrap();
    run(&mut eng, "INSERT INTO a77 VALUES (1, 10)").unwrap();
    run(&mut eng, "INSERT INTO b77 VALUES (1, 20)").unwrap();
    // `v` is in both a77 and b77: ambiguous in the WHERE clause.
    let err = run(&mut eng, "UPDATE a77 SET v = 0 FROM b77 WHERE v = 1").unwrap_err();
    assert_eq!(err.code, "42702");
    // Ambiguous in a SET expression too (combined FROM+target schema).
    let err = run(
        &mut eng,
        "UPDATE a77 SET v = v + 1 FROM b77 WHERE a77.id = b77.id",
    )
    .unwrap_err();
    assert_eq!(err.code, "42702");
    // Qualified references resolve fine.
    run(
        &mut eng,
        "UPDATE a77 SET v = b77.v FROM b77 WHERE a77.id = b77.id",
    )
    .unwrap();
    let got = rows_of(run(&mut eng, "SELECT v FROM a77").unwrap());
    assert_eq!(got, vec![vec!["20".to_string()]]);
}

/// v0.77: ambiguity inside DELETE ... USING items is 42702 (when the
/// target table doesn't have the column to shadow it). An
/// unqualified column present in both the target and a USING item
/// resolves to the target by the documented eval_dml_expr
/// convention — target frame last.
#[test]
fn v77_delete_using_ambiguous_column_is_42702() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a77 (id int, v int)").unwrap();
    run(&mut eng, "CREATE TABLE b77 (id int, w int)").unwrap();
    run(&mut eng, "CREATE TABLE c77 (id int, w int)").unwrap();
    run(&mut eng, "INSERT INTO a77 VALUES (1, 10), (2, 20)").unwrap();
    run(&mut eng, "INSERT INTO b77 VALUES (1, 10)").unwrap();
    run(&mut eng, "INSERT INTO c77 VALUES (1, 10)").unwrap();
    // `w` is in both USING items b77 and c77 (not in the target):
    // ambiguous.
    let err = run(&mut eng, "DELETE FROM a77 USING b77, c77 WHERE w = 10").unwrap_err();
    assert_eq!(err.code, "42702");
    // Qualified references resolve fine.
    run(
        &mut eng,
        "DELETE FROM a77 USING b77 WHERE a77.id = b77.id AND b77.w = 10",
    )
    .unwrap();
    let got = rows_of(run(&mut eng, "SELECT id FROM a77 ORDER BY id").unwrap());
    assert_eq!(got, vec![vec!["2".to_string()]]);
    // Target-vs-USING: the unqualified `v` resolves to the target
    // table (eval_dml_expr convention), deleting the row with
    // a77.v = 20 only.
    run(&mut eng, "DELETE FROM a77 USING b77 WHERE v = 20").unwrap();
    let got = rows_of(run(&mut eng, "SELECT id FROM a77").unwrap());
    assert!(got.is_empty(), "got: {:?}", got);
}

/// v0.77: extended-protocol parameters inside FROM/USING items bind
/// and execute (v0.76 only checked `max_param` sizing).
#[test]
fn v77_params_in_from_using_items_execute() {
    use crate::storage::Value;
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a77 (id int, v int)").unwrap();
    run(&mut eng, "INSERT INTO a77 VALUES (1, 10), (2, 20)").unwrap();
    // $1 inside a FROM derived table.
    let mut stmt =
        parse_statement("UPDATE a77 SET v = v + s.d FROM (SELECT $1 AS d) s WHERE a77.id = 1")
            .unwrap();
    subst_params(&mut stmt, &[Some(Value::Int(5))]).unwrap();
    run_stmt(&mut eng, &stmt).unwrap();
    let got = rows_of(run(&mut eng, "SELECT v FROM a77 WHERE id = 1").unwrap());
    assert_eq!(got, vec![vec!["15".to_string()]]);
    // $1 inside a USING derived table.
    let mut stmt =
        parse_statement("DELETE FROM a77 USING (SELECT $1 AS did) s WHERE a77.id = s.did").unwrap();
    subst_params(&mut stmt, &[Some(Value::Int(2))]).unwrap();
    run_stmt(&mut eng, &stmt).unwrap();
    let got = rows_of(run(&mut eng, "SELECT id FROM a77 ORDER BY id").unwrap());
    assert_eq!(got, vec![vec!["1".to_string()]]);
}

/// v0.77: ALTER on a temp table never touches a same-named
/// permanent table's indexes — temp tables shadow by name, and the
/// v0.77 guards keep the global index catalog out of reach.
#[test]
fn v77_temp_alter_isolated_from_shadowed_permanent() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE sh77 (id int, v int)").unwrap();
    run(&mut eng, "CREATE INDEX sh77_idx ON sh77(v)").unwrap();
    run(&mut eng, "INSERT INTO sh77 VALUES (1, 10)").unwrap();
    run(&mut eng, "CREATE TEMP TABLE sh77 (id int, v int)").unwrap();
    run(&mut eng, "INSERT INTO sh77 VALUES (2, 20)").unwrap();
    // RENAME COLUMN on the temp table: the permanent index keeps `v`.
    run(&mut eng, "ALTER TABLE sh77 RENAME COLUMN v TO w").unwrap();
    let ix = eng.db.indexes.get("sh77_idx").expect("perm index survives");
    assert_eq!(ix.def.table, "sh77");
    assert_eq!(ix.def.col_names, vec!["v".to_string()]);
    // DROP COLUMN ... CASCADE on the temp table must not drop the
    // permanent table's index.
    run(&mut eng, "ALTER TABLE sh77 DROP COLUMN w CASCADE").unwrap();
    assert!(eng.db.indexes.get("sh77_idx").is_some());
    // The temp table itself did change.
    let got = rows_of(run(&mut eng, "SELECT id FROM sh77").unwrap());
    assert_eq!(got, vec![vec!["2".to_string()]]);
    // RENAME TO on the temp table: the permanent index still points
    // at `sh77`.
    run(&mut eng, "ALTER TABLE sh77 RENAME TO sh77t").unwrap();
    let ix = eng.db.indexes.get("sh77_idx").expect("perm index survives");
    assert_eq!(ix.def.table, "sh77");
    // Permanent table's schema and data intact (checked in the
    // catalog; it is shadowed for this session's SQL).
    let pt = eng.db.tables.get("sh77").unwrap();
    let live = pt.last().unwrap();
    assert!(live.column_index("v").is_some());
    assert_eq!(live.rows.len(), 1);
}

/// v1.18: index-ordered scan over a TEMP table's index. The ORDER BY fast
/// path (`OrderHint`) looked the planned index up only in the global map,
/// but temp indexes live in the session-local map — the lookup returned
/// `None` and the server thread panicked, killing the connection
/// (select.sql `SELECT * FROM foo ORDER BY f1` wedges).
#[test]
fn v118_temp_index_order_scan_asc() {
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE t118a (f1 int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO t118a VALUES (42),(3),(10),(7),(null),(null),(1)",
    )
    .unwrap();
    run(&mut eng, "CREATE INDEX t118a_i ON t118a (f1)").unwrap();
    // ASC with PG-default NULLS LAST, served from the temp index.
    let got = rows_of(run(&mut eng, "SELECT f1 FROM t118a ORDER BY f1").unwrap());
    let want: Vec<Vec<String>> = ["1", "3", "7", "10", "42", "NULL", "NULL"]
        .iter()
        .map(|s| s.to_string())
        .map(|s| vec![s])
        .collect();
    assert_eq!(got, want);
}

/// v1.18: temp index-ordered scan, DESC (PG-default NULLS FIRST).
/// Second wedging shape in select.sql (`ORDER BY f1 DESC`).
#[test]
fn v118_temp_index_order_scan_desc() {
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE t118d (f1 int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO t118d VALUES (42),(3),(10),(7),(null),(null),(1)",
    )
    .unwrap();
    run(&mut eng, "CREATE INDEX t118d_i ON t118d (f1)").unwrap();
    let got = rows_of(run(&mut eng, "SELECT f1 FROM t118d ORDER BY f1 DESC").unwrap());
    let want: Vec<Vec<String>> = ["NULL", "NULL", "42", "10", "7", "3", "1"]
        .iter()
        .map(|s| vec![s.to_string()])
        .collect();
    assert_eq!(got, want);
}

/// v1.18: temp ORDER BY with LIMIT goes through the early-limit
/// index-order path (same `OrderHint` arm).
#[test]
fn v118_temp_index_order_scan_limit() {
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE t118l (f1 int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO t118l VALUES (42),(3),(10),(7),(null),(null),(1)",
    )
    .unwrap();
    run(&mut eng, "CREATE INDEX t118l_i ON t118l (f1)").unwrap();
    let got = rows_of(run(&mut eng, "SELECT f1 FROM t118l ORDER BY f1 LIMIT 3").unwrap());
    let want: Vec<Vec<String>> = ["1", "3", "7"]
        .iter()
        .map(|s| vec![s.to_string()])
        .collect();
    assert_eq!(got, want);
}

/// v1.18: explicit NULLS FIRST on an ASC temp index — the hint declines
/// (null placement mismatch) and the sort fallback stays correct.
#[test]
fn v118_temp_index_order_scan_nulls_first_fallback() {
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE t118n (f1 int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO t118n VALUES (42),(3),(10),(7),(null),(null),(1)",
    )
    .unwrap();
    run(&mut eng, "CREATE INDEX t118n_i ON t118n (f1)").unwrap();
    let got = rows_of(run(&mut eng, "SELECT f1 FROM t118n ORDER BY f1 NULLS FIRST").unwrap());
    let want: Vec<Vec<String>> = ["NULL", "NULL", "1", "3", "7", "10", "42"]
        .iter()
        .map(|s| vec![s.to_string()])
        .collect();
    assert_eq!(got, want);
}

/// v0.77: quantified comparisons over an empty set — `= ANY` is
/// false, `<> ALL` is true (PG semantics).
#[test]
fn v77_quantified_empty_set() {
    let mut eng = engine();
    let one = |eng: &mut Engine, sql: &str| rows_of(run(eng, sql).unwrap());
    assert_eq!(
        one(
            &mut eng,
            "SELECT 1 = ANY (SELECT id FROM users WHERE id > 100)"
        ),
        vec![vec!["f".to_string()]]
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT 1 = SOME (SELECT id FROM users WHERE id > 100)"
        ),
        vec![vec!["f".to_string()]]
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT 1 <> ALL (SELECT id FROM users WHERE id > 100)"
        ),
        vec![vec!["t".to_string()]]
    );
    // Non-empty control: the desugar still works.
    assert_eq!(
        one(&mut eng, "SELECT 2 = ANY (SELECT id FROM users)"),
        vec![vec!["t".to_string()]]
    );
    assert_eq!(
        one(&mut eng, "SELECT 9 <> ALL (SELECT id FROM users)"),
        vec![vec!["t".to_string()]]
    );
}

#[test]
fn v77_recursive_cte_parenthesized_terms_with_inner_with() {
    // v0.77: a recursive CTE whose non-recursive term is a
    // parenthesized VALUES and whose recursive term is a
    // parenthesized query with its own WITH (NOT MATERIALIZED).
    // From the subselect.sql conformance suite.
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "WITH RECURSIVE x(a) AS (
                   (VALUES ('a'), ('b'))
                    UNION ALL
                    (WITH z AS NOT MATERIALIZED (SELECT * FROM x)
                     SELECT z.a || z1.a AS a FROM z CROSS JOIN z AS z1
                     WHERE length(z.a || z1.a) < 5)
                 )
                 SELECT * FROM x",
        )
        .unwrap(),
    );
    // 2 seeds + 4 (len 2) + 16 (len 4) = 22 rows.
    assert_eq!(rows.len(), 22);
    let mut vals: Vec<String> = rows.into_iter().map(|r| r[0].clone()).collect();
    vals.sort();
    assert!(vals.contains(&"a".to_string()));
    assert!(vals.contains(&"bbbb".to_string()));
}

#[test]
fn v77_create_table_inherits_copies_columns() {
    // v0.77: `CREATE TABLE ... INHERITS (parent)` copies the
    // parent's columns so the child is usable.
    // v0.96: parent scans now include children's rows (PG19); ONLY
    // restricts the scan to the parent.
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE inh_p (a int, b int)").unwrap();
    run(&mut eng, "INSERT INTO inh_p VALUES (1, 10)").unwrap();
    run(&mut eng, "CREATE TEMP TABLE inh_c () INHERITS (inh_p)").unwrap();
    run(&mut eng, "INSERT INTO inh_c VALUES (2, 20)").unwrap();
    let child = rows_of(run(&mut eng, "SELECT * FROM inh_c").unwrap());
    assert_eq!(child, vec![vec!["2".to_string(), "20".to_string()]]);
    // Parent scan includes the child's row (v0.96).
    let parent = rows_of(run(&mut eng, "SELECT * FROM inh_p").unwrap());
    assert_eq!(
        parent,
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "20".to_string()]
        ]
    );
    // ONLY restricts the scan to the parent's own rows.
    let only = rows_of(run(&mut eng, "SELECT * FROM ONLY inh_p").unwrap());
    assert_eq!(only, vec![vec!["1".to_string(), "10".to_string()]]);
    // Unknown parent is 42P01.
    let err = run(&mut eng, "CREATE TEMP TABLE inh_x () INHERITS (nope)").unwrap_err();
    assert_eq!(err.code, "42P01");
}

#[test]
fn v96_alter_table_inherit_no_inherit() {
    // v0.96: `ALTER TABLE ... INHERIT / NO INHERIT` (PG19). INHERIT
    // never adds or reorders columns; NO INHERIT removes only the
    // link, keeping columns and data.
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE ai_p (a text, b text)").unwrap();
    run(&mut eng, "CREATE TEMP TABLE ai_c (b text, a text)").unwrap();
    run(&mut eng, "ALTER TABLE ai_c INHERIT ai_p").unwrap();
    run(&mut eng, "INSERT INTO ai_c VALUES ('v', 'w')").unwrap();
    // Parent scan sees the child row, remapped to parent order.
    let rows = rows_of(run(&mut eng, "SELECT a, b FROM ai_p").unwrap());
    assert_eq!(rows, vec![vec!["w".to_string(), "v".to_string()]]);
    // NO INHERIT drops the link; data and columns stay.
    run(&mut eng, "ALTER TABLE ai_c NO INHERIT ai_p").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT a, b FROM ai_p").unwrap());
    assert!(rows.is_empty());
    let rows = rows_of(run(&mut eng, "SELECT b, a FROM ai_c").unwrap());
    assert_eq!(rows, vec![vec!["v".to_string(), "w".to_string()]]);
    // Removing a non-link is 42P01.
    let err = run(&mut eng, "ALTER TABLE ai_c NO INHERIT ai_p").unwrap_err();
    assert_eq!(err.code, "42P01");
    // Re-adding the link works.
    run(&mut eng, "ALTER TABLE ai_c INHERIT ai_p").unwrap();
    // Duplicate link is 42P16.
    let err = run(&mut eng, "ALTER TABLE ai_c INHERIT ai_p").unwrap_err();
    assert_eq!(err.code, "42P16");
}

#[test]
fn v96_inherit_compatibility_errors() {
    // v0.96: ALTER INHERIT compatibility checks (PG19, 42804).
    let mut eng = engine();
    // Missing column.
    run(&mut eng, "CREATE TEMP TABLE ic_p (a int, b int)").unwrap();
    run(&mut eng, "CREATE TEMP TABLE ic_miss (a int)").unwrap();
    let err = run(&mut eng, "ALTER TABLE ic_miss INHERIT ic_p").unwrap_err();
    assert_eq!(err.code, "42804");
    // Type conflict.
    run(&mut eng, "CREATE TEMP TABLE ic_type (a text, b int)").unwrap();
    let err = run(&mut eng, "ALTER TABLE ic_type INHERIT ic_p").unwrap_err();
    assert_eq!(err.code, "42804");
    // Parent NOT NULL requires child NOT NULL.
    run(&mut eng, "CREATE TEMP TABLE ic_nnp (a int NOT NULL, b int)").unwrap();
    run(&mut eng, "CREATE TEMP TABLE ic_nnc (a int, b int)").unwrap();
    let err = run(&mut eng, "ALTER TABLE ic_nnc INHERIT ic_nnp").unwrap_err();
    assert_eq!(err.code, "42804");
    // Child NOT NULL is fine.
    run(
        &mut eng,
        "CREATE TEMP TABLE ic_nnc2 (a int NOT NULL, b int)",
    )
    .unwrap();
    run(&mut eng, "ALTER TABLE ic_nnc2 INHERIT ic_nnp").unwrap();
    // Self-inheritance is 42P16.
    let err = run(&mut eng, "ALTER TABLE ic_p INHERIT ic_p").unwrap_err();
    assert_eq!(err.code, "42P16");
    // Circular inheritance is 42P16.
    run(&mut eng, "CREATE TEMP TABLE ic_a (a int, b int)").unwrap();
    run(&mut eng, "CREATE TEMP TABLE ic_b (a int, b int)").unwrap();
    run(&mut eng, "ALTER TABLE ic_b INHERIT ic_a").unwrap();
    let err = run(&mut eng, "ALTER TABLE ic_a INHERIT ic_b").unwrap_err();
    assert_eq!(err.code, "42P16");
    // Missing parent is 42P01.
    let err = run(&mut eng, "ALTER TABLE ic_a INHERIT nope").unwrap_err();
    assert_eq!(err.code, "42P01");
}

#[test]
fn v96_inherit_multiple_parents_and_defaults() {
    // v0.96: multiple parents merge by column name; conflicting
    // inherited defaults are 42804 unless the child overrides.
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE mp1 (a int, b text)").unwrap();
    run(&mut eng, "CREATE TEMP TABLE mp2 (b text, c int)").unwrap();
    run(
        &mut eng,
        "CREATE TEMP TABLE mpc (d text) INHERITS (mp1, mp2)",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO mpc VALUES (1, 'x', 2, 'y')").unwrap();
    // The child has all four columns in merge order.
    let rows = rows_of(run(&mut eng, "SELECT a, b, c, d FROM mpc").unwrap());
    assert_eq!(
        rows,
        vec![vec![
            "1".to_string(),
            "x".to_string(),
            "2".to_string(),
            "y".to_string()
        ]]
    );
    // Each parent scan sees the child row remapped to its own columns.
    let rows = rows_of(run(&mut eng, "SELECT a, b FROM mp1").unwrap());
    assert_eq!(rows, vec![vec!["1".to_string(), "x".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT b, c FROM mp2").unwrap());
    assert_eq!(rows, vec![vec!["x".to_string(), "2".to_string()]]);
    // Duplicate parent in one list is 42P16.
    let err = run(
        &mut eng,
        "CREATE TEMP TABLE mpd (z int) INHERITS (mp1, mp1)",
    )
    .unwrap_err();
    assert_eq!(err.code, "42P16");
    // Conflicting defaults.
    run(&mut eng, "CREATE TEMP TABLE md1 (a int DEFAULT 1)").unwrap();
    run(&mut eng, "CREATE TEMP TABLE md2 (a int DEFAULT 2)").unwrap();
    let err = run(
        &mut eng,
        "CREATE TEMP TABLE mdc (z int) INHERITS (md1, md2)",
    )
    .unwrap_err();
    assert_eq!(err.code, "42804");
    // Explicit child default resolves the conflict.
    run(
        &mut eng,
        "CREATE TEMP TABLE mdok (z int, a int DEFAULT 5) INHERITS (md1, md2)",
    )
    .unwrap();
    // Identical defaults merge silently.
    run(&mut eng, "CREATE TEMP TABLE md3 (a int DEFAULT 1)").unwrap();
    run(
        &mut eng,
        "CREATE TEMP TABLE mdsame (z int) INHERITS (md1, md3)",
    )
    .unwrap();
}

#[test]
fn v96_inherit_recursive_scan_and_temp_rules() {
    // v0.96: scans recurse through grandchildren; temp/permanent
    // inheritance follows PG's rules.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE rp (a int)").unwrap();
    run(&mut eng, "CREATE TABLE rc (b int) INHERITS (rp)").unwrap();
    run(&mut eng, "CREATE TABLE rg (c int) INHERITS (rc)").unwrap();
    run(&mut eng, "INSERT INTO rp VALUES (1)").unwrap();
    run(&mut eng, "INSERT INTO rc VALUES (2, 20)").unwrap();
    run(&mut eng, "INSERT INTO rg VALUES (3, 30, 300)").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT a FROM rp ORDER BY a").unwrap());
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string()],
            vec!["2".to_string()],
            vec!["3".to_string()]
        ]
    );
    let rows = rows_of(run(&mut eng, "SELECT a FROM ONLY rp ORDER BY a").unwrap());
    assert_eq!(rows, vec![vec!["1".to_string()]]);
    // pg_inherits exposes both links.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT inhseqno FROM pg_inherits ORDER BY inhrelid, inhseqno",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 2);
    // Permanent child of a temp parent is 42809.
    run(&mut eng, "CREATE TEMP TABLE rt_p (a int)").unwrap();
    let err = run(&mut eng, "CREATE TABLE rt_c (z int) INHERITS (rt_p)").unwrap_err();
    assert_eq!(err.code, "42809");
    // Temp child of a permanent parent is fine.
    run(&mut eng, "CREATE TEMP TABLE rt_c2 (z int) INHERITS (rp)").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT a FROM rp ORDER BY a").unwrap());
    assert_eq!(rows.len(), 3);
}

#[test]
fn v96_inherit_constraint_only_create() {
    // v0.96: a constraint-only definition with INHERITS (the
    // conformance union.sql shape) inherits every parent column.
    let mut eng = engine();
    run(&mut eng, "CREATE TEMP TABLE co_p (ab text primary key)").unwrap();
    run(
        &mut eng,
        "CREATE TEMP TABLE co_c (primary key (ab)) INHERITS (co_p)",
    )
    .unwrap();
    run(&mut eng, "INSERT INTO co_c VALUES ('xy')").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT ab FROM co_p").unwrap());
    assert_eq!(rows, vec![vec!["xy".to_string()]]);
    let rows = rows_of(run(&mut eng, "SELECT ab FROM ONLY co_p").unwrap());
    assert!(rows.is_empty());
}

#[test]
fn v77_explain_with_options_parses() {
    // v0.77: `EXPLAIN (COSTS OFF, ...)` — the parenthesized option
    // list is accepted (options parsed and ignored) so a valid PG
    // statement doesn't 42601 and poison an explicit transaction.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE exp_t (a int)").unwrap();
    let r = run(&mut eng, "EXPLAIN (COSTS OFF) SELECT * FROM exp_t").unwrap();
    match r {
        ExecResult::Explain { columns, rows } => {
            assert_eq!(columns[0].0, "QUERY PLAN");
            assert!(!rows.is_empty());
        }
        _ => panic!("expected Explain"),
    }
    // Multiple options with values.
    let r2 = run(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE TRUE) SELECT * FROM exp_t",
    )
    .unwrap();
    assert!(matches!(r2, ExecResult::Explain { .. }));
}

#[test]
fn v77_array_subquery_formats_literal() {
    // v0.77: `array(SELECT ...)` — ARRAY constructor with a subquery.
    // Returns the first column as a PG array literal (Text for now).
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE arr_t (x int)").unwrap();
    run(&mut eng, "INSERT INTO arr_t VALUES (1), (2), (3)").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT array(SELECT x FROM arr_t ORDER BY x)").unwrap());
    assert_eq!(rows, vec![vec!["{1,2,3}".to_string()]]);
    // Empty subquery -> '{}'.
    let rows2 = rows_of(run(&mut eng, "SELECT array(SELECT x FROM arr_t WHERE x > 100)").unwrap());
    assert_eq!(rows2, vec![vec!["{}".to_string()]]);
}

#[test]
fn v78_array_subquery_has_array_type() {
    // v0.78: `array(SELECT ...)` carries a proper array type with
    // PG's array OID (not text's 25). Values stay the `{...}`
    // literal text, which is what array_out puts on the wire.
    use crate::storage::ArrayElem;
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE arr_t (x int, t text)").unwrap();
    run(&mut eng, "INSERT INTO arr_t VALUES (1, 'a'), (2, 'b')").unwrap();

    let r = run(&mut eng, "SELECT array(SELECT x FROM arr_t ORDER BY x)").unwrap();
    match &r {
        ExecResult::Select { columns, .. } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].1, ColType::Array(ArrayElem::Int));
            assert_eq!(columns[0].1.oid(), 1007); // _int4, not 25
            assert_eq!(columns[0].1.sql_name(), "integer[]");
            assert_eq!(columns[0].1.pg_typname(), "_int4");
        }
        _ => panic!("expected Select"),
    }
    assert_eq!(rows_of(r).concat(), vec!["{1,2}".to_string()]);

    // Text element -> _text (1009).
    let r = run(&mut eng, "SELECT array(SELECT t FROM arr_t ORDER BY t)").unwrap();
    match &r {
        ExecResult::Select { columns, .. } => {
            assert_eq!(columns[0].1, ColType::Array(ArrayElem::Text));
            assert_eq!(columns[0].1.oid(), 1009);
            assert_eq!(columns[0].1.sql_name(), "text[]");
            assert_eq!(columns[0].1.pg_typname(), "_text");
        }
        _ => panic!("expected Select"),
    }

    // PG flattens nested arrays: still _int4.
    let r = run(&mut eng, "SELECT array(SELECT array(SELECT x FROM arr_t))").unwrap();
    match &r {
        ExecResult::Select { columns, .. } => {
            assert_eq!(columns[0].1, ColType::Array(ArrayElem::Int));
            assert_eq!(columns[0].1.oid(), 1007);
        }
        _ => panic!("expected Select"),
    }

    // Element-type mapping spot checks (pg_type.dat array OIDs).
    assert_eq!(ArrayElem::of(&ColType::Bool).array_oid(), 1000);
    assert_eq!(ArrayElem::of(&ColType::BigInt).array_oid(), 1016);
    assert_eq!(ArrayElem::of(&ColType::Float).array_oid(), 1022);
    assert_eq!(ArrayElem::of(&ColType::Numeric(None)).array_oid(), 1231);
    assert_eq!(ArrayElem::of(&ColType::Uuid).array_oid(), 2951);
    assert_eq!(ArrayElem::of(&ColType::Timestamp).array_oid(), 1115);
    assert_eq!(ArrayElem::of(&ColType::Date).array_oid(), 1182);
    // Nested input flattens instead of nesting.
    assert_eq!(
        ArrayElem::of(&ColType::Array(ArrayElem::Int)),
        ArrayElem::Int
    );
}

#[test]
fn v78_explain_resolves_ctes() {
    // v0.78: the planner resolves CTE names in FROM (including joins
    // and derived tables), like the executor. Previously EXPLAIN of a
    // query referencing a CTE failed with 42P01, which — now that
    // errors abort the txn (v0.77) — poisoned explicit transactions
    // in the conformance suite.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE cte_t (a int)").unwrap();
    // CTE in a join (the join.sql #19560 shape).
    let r = run(
        &mut eng,
        // v1.08: plain EXPLAIN (COSTS ON) — the v1.08 PG-text renderer
        // honestly masks LEFT JOIN + CTE under COSTS OFF (0A000).
        "EXPLAIN WITH viewer AS (SELECT 'bob' AS id) \
             SELECT count(*) FROM cte_t LEFT JOIN viewer ON true",
    )
    .unwrap();
    let plan = rows_of(r)
        .into_iter()
        .map(|r| r[0].clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        plan.contains("Subquery Scan on viewer"),
        "plan was:\n{plan}"
    );
    // Chained CTEs: inner CTE visible to later ones.
    // v1.54: single-ref simple CTEs inline (PG `inline_cte`) — both
    // x and y flatten to a bare Result (verified against PG16).
    let r2 = run(
        &mut eng,
        "EXPLAIN WITH x AS (SELECT 1 AS a), y AS (SELECT a + 1 AS b FROM x) SELECT * FROM y",
    )
    .unwrap();
    let plan2 = rows_of(r2)
        .into_iter()
        .map(|r| r[0].clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plan2.contains("Result"), "plan was:\n{plan2}");
    assert!(!plan2.contains("Subquery Scan"), "plan was:\n{plan2}");
    // CTE shadowing a real table plans the CTE, like the executor.
    // v1.54: single-ref simple CTE inlines (PG `inline_cte`) — flat
    // Result (verified against PG16).
    let r3 = run(
        &mut eng,
        "EXPLAIN WITH cte_t AS (SELECT 2 AS a) SELECT * FROM cte_t",
    )
    .unwrap();
    let plan3 = rows_of(r3)
        .into_iter()
        .map(|r| r[0].clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plan3.contains("Result"), "plan was:\n{plan3}");
    // Recursive CTE: nominal estimate, no infinite recursion.
    let r4 = run(
            &mut eng,
            "EXPLAIN WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r",
        )
        .unwrap();
    assert!(!rows_of(r4).is_empty());
    // Unknown relation is still 42P01.
    let err = run(&mut eng, "EXPLAIN SELECT * FROM no_such_rel").unwrap_err();
    assert_eq!(err.code, "42P01");
}

#[test]
fn v78_grouping_sets() {
    // v0.78: PG grouping-set syntax (`()`, ROLLUP, CUBE, GROUPING
    // SETS, DISTINCT) parses and executes: one aggregation per set,
    // bare columns not in the current set project as NULL (PG19).
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE gs_u (a int, b int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO gs_u VALUES (1, 10), (1, 20), (2, 30)",
    )
    .unwrap();
    // GROUPING SETS ((a), (b), ()): per-set groups plus NULLs for
    // the columns each set does not group by.
    let r = run(
        &mut eng,
        "SELECT a, b, count(*) FROM gs_u \
             GROUP BY GROUPING SETS ((a), (b), ()) ORDER BY 1, 2, 3",
    )
    .unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 6);
    assert_eq!(
        rows[0],
        vec!["1".to_string(), "NULL".to_string(), "2".to_string()]
    );
    assert_eq!(
        rows[1],
        vec!["2".to_string(), "NULL".to_string(), "1".to_string()]
    );
    assert_eq!(
        rows[2],
        vec!["NULL".to_string(), "10".to_string(), "1".to_string()]
    );
    assert_eq!(
        rows[5],
        vec!["NULL".to_string(), "NULL".to_string(), "3".to_string()]
    );
    // ROLLUP: (a) then the grand total.
    let r = run(
        &mut eng,
        "SELECT a, count(*) FROM gs_u GROUP BY ROLLUP (a) ORDER BY 1, 2",
    )
    .unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2], vec!["NULL".to_string(), "3".to_string()]);
    // CUBE (a) on one column: same shape as ROLLUP here.
    let r = run(
        &mut eng,
        "SELECT a, count(*) FROM gs_u GROUP BY CUBE (a) ORDER BY 1, 2",
    )
    .unwrap();
    assert_eq!(rows_of(r).len(), 3);
    // GROUP BY (): grand total only.
    let r = run(&mut eng, "SELECT count(*) FROM gs_u GROUP BY ()").unwrap();
    assert_eq!(rows_of(r), vec![vec!["3".to_string()]]);
    // DISTINCT dedups identical sets.
    let r = run(
        &mut eng,
        "SELECT a, count(*) FROM gs_u \
             GROUP BY DISTINCT GROUPING SETS ((a), (a)) ORDER BY 1, 2",
    )
    .unwrap();
    assert_eq!(rows_of(r).len(), 2);
    // Plain GROUP BY still raises 42803 for unbound bare columns.
    let err = run(&mut eng, "SELECT a, b FROM gs_u GROUP BY a").unwrap_err();
    assert_eq!(err.code, "42803");
    // Non-trivial expressions over unbound columns stay 42803 even
    // with grouping-set syntax (only bare columns go NULL).
    let err = run(
        &mut eng,
        "SELECT a + 1 FROM gs_u GROUP BY GROUPING SETS ((b))",
    )
    .unwrap_err();
    assert_eq!(err.code, "42803");
    // EXPLAIN accepts grouping-set syntax (this poisoned explicit
    // transactions in the JSS join suite before v0.78).
    let r = run(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a, count(*) FROM gs_u \
             GROUP BY GROUPING SETS ((), (a))",
    )
    .unwrap();
    assert!(!rows_of(r).is_empty());
}

#[test]
fn v80_grouping_function() {
    // v0.80: PG19's GROUPING() mask function (parse_agg.c
    // transformGroupingFunc / finalize_grouping_exprs): bit (n-1-i)
    // is 1 when argument i is absent from the current grouping set.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE grp (a int, b int)").unwrap();
    run(&mut eng, "INSERT INTO grp VALUES (1, 10), (1, 20), (2, 30)").unwrap();
    // ROLLUP(a): per-group rows mask 0, grand total masks 1.
    let r = run(
        &mut eng,
        "SELECT a, grouping(a) FROM grp GROUP BY ROLLUP (a) ORDER BY 1, 2",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string(), "0".to_string()],
            vec!["2".to_string(), "0".to_string()],
            vec!["NULL".to_string(), "1".to_string()],
        ]
    );
    // Multi-argument bit order: rightmost argument is the LSB.
    // ROLLUP(a, b) yields sets (a,b), (a), () -> masks 0, 1, 3.
    let r = run(
        &mut eng,
        "SELECT grouping(a, b) FROM grp GROUP BY ROLLUP (a, b) ORDER BY 1",
    )
    .unwrap();
    let masks: Vec<String> = rows_of(r).into_iter().map(|row| row[0].clone()).collect();
    assert_eq!(masks, vec!["0", "0", "0", "1", "1", "3"]);
    // CUBE(a,b): all four masks appear.
    let r = run(
        &mut eng,
        "SELECT DISTINCT grouping(a, b) FROM grp \
             GROUP BY CUBE (a, b) ORDER BY 1",
    )
    .unwrap();
    let masks: Vec<String> = rows_of(r).into_iter().map(|row| row[0].clone()).collect();
    assert_eq!(masks, vec!["0", "1", "2", "3"]);
    // Plain GROUP BY: every argument present, mask 0. GROUPING()
    // reports int4, like PG19.
    let r = run(&mut eng, "SELECT grouping(a) FROM grp GROUP BY a").unwrap();
    let ExecResult::Select { columns, rows } = r else {
        panic!("SELECT returns rows");
    };
    assert_eq!(columns[0].1, ColType::Int);
    assert_eq!(rows.len(), 2);
    // Qualified and complex expression arguments match grouping keys.
    let r = run(
        &mut eng,
        "SELECT count(*), grouping(grp.a), grouping(a * 2) FROM grp GROUP BY grp.a, a * 2",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["2".to_string(), "0".to_string(), "0".to_string()],
            vec!["1".to_string(), "0".to_string(), "0".to_string()],
        ]
    );
    // GROUPING() is legal in HAVING and ORDER BY.
    let r = run(
        &mut eng,
        "SELECT a FROM grp GROUP BY ROLLUP (a) HAVING grouping(a) = 1",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["NULL".to_string()]]);
    // An argument that is not a grouping expression: 42803 (PG19's
    // exact message).
    let err = run(&mut eng, "SELECT grouping(b) FROM grp GROUP BY a").unwrap_err();
    assert_eq!(err.code, "42803");
    assert!(
        err.message
            .contains("grouping expressions of the associated query level"),
        "got: {}",
        err.message
    );
    // No GROUP BY at all: 42803.
    let err = run(&mut eng, "SELECT grouping(a) FROM grp").unwrap_err();
    assert_eq!(err.code, "42803");
    // GROUPING() inside an aggregate: 42803.
    let err = run(&mut eng, "SELECT sum(grouping(a)) FROM grp GROUP BY a").unwrap_err();
    assert_eq!(err.code, "42803");
    // GROUPING() in WHERE / GROUP BY: PG19's 42803 placement texts.
    let err = run(
        &mut eng,
        "SELECT a FROM grp WHERE grouping(a) = 0 GROUP BY a",
    )
    .unwrap_err();
    assert_eq!(err.code, "42803");
    assert!(
        err.message.contains("not allowed in WHERE"),
        "got: {}",
        err.message
    );
    let err = run(&mut eng, "SELECT a FROM grp GROUP BY grouping(a)").unwrap_err();
    assert_eq!(err.code, "42803");
    // Zero arguments: syntax error, like PG19's expr_list.
    let err = run(&mut eng, "SELECT grouping() FROM grp GROUP BY a").unwrap_err();
    assert_eq!(err.code, "42601");
    // More than 31 arguments: 54023, PG19's exact message.
    let many = vec!["a"; 32].join(", ");
    let err = run(
        &mut eng,
        &format!("SELECT grouping({many}) FROM grp GROUP BY a"),
    )
    .unwrap_err();
    assert_eq!(err.code, "54023");
    assert_eq!(err.message, "GROUPING must have fewer than 32 arguments");
    // A column named `grouping` still parses as a column reference
    // (GROUPING is unreserved in PG19).
    let r = run(&mut eng, "SELECT grouping FROM (SELECT 1 AS grouping) s").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
}

#[test]
fn v80_hashed_subplans() {
    // v0.80: hashed correlated-EXISTS and cached uncorrelated-IN
    // subplans — one inner build per statement instead of one
    // subquery execution per outer row.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE h1(a int, b int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO h1 SELECT g, g % 2 FROM generate_series(1, 200) g",
    )
    .unwrap();
    run(&mut eng, "CREATE TABLE h2(x int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO h2 SELECT g FROM generate_series(1, 100) g",
    )
    .unwrap();
    // Correlated EXISTS over the equality shape -> hashed.
    let r = run(
        &mut eng,
        "SELECT count(*) FROM h1 WHERE EXISTS (SELECT 1 FROM h2 k WHERE k.x = h1.a)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["100".to_string()]]);
    // NOT EXISTS.
    let r = run(
        &mut eng,
        "SELECT count(*) FROM h1 WHERE NOT EXISTS (SELECT 1 FROM h2 k WHERE k.x = h1.a)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["100".to_string()]]);
    // Probe on the left, and inside OR (the PG19 subselect shape).
    let r = run(
        &mut eng,
        "SELECT count(*) FROM h1 t WHERE (EXISTS (SELECT 1 FROM h2 k WHERE t.a = k.x) OR t.b < 0)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["100".to_string()]]);
    // NULL probe never matches.
    let r = run(
            &mut eng,
            "SELECT count(*) FROM (SELECT NULL::int AS z) s WHERE EXISTS (SELECT 1 FROM h2 k WHERE k.x = s.z)",
        )
        .unwrap();
    assert_eq!(rows_of(r), vec![vec!["0".to_string()]]);
    // Uncorrelated IN -> single subquery execution.
    let r = run(
        &mut eng,
        "SELECT count(*) FROM h1 WHERE a IN (SELECT x FROM h2)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["100".to_string()]]);
    let r = run(
        &mut eng,
        "SELECT count(*) FROM h1 WHERE a NOT IN (SELECT x FROM h2)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["100".to_string()]]);
    // Three-valued logic through the cache: NULL in the output and
    // a missed probe -> NULL, not false.
    let r = run(
        &mut eng,
        "SELECT 9999 IN (SELECT x FROM h2 UNION ALL SELECT NULL::int)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["NULL".to_string()]]);
    // Non-hashable shapes still work via the slow path.
    let r = run(
        &mut eng,
        "SELECT count(*) FROM h1 WHERE EXISTS (SELECT 1 FROM h2 k WHERE k.x = h1.a AND k.x > 10)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["90".to_string()]]);
    // Correlated IN is not cached but stays correct: h1.b is 0/1,
    // so the subquery yields {1} and only a = 1 matches.
    let r = run(
        &mut eng,
        "SELECT count(*) FROM h1 WHERE a IN (SELECT x FROM h2 WHERE x = h1.b)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
    // v0.80 regression: NULL probe against an empty / NULL-free
    // set. `NULL NOT IN (empty)` is true, `NULL IN (empty)` false.
    let r = run(
        &mut eng,
        "SELECT count(*) FROM (SELECT NULL::int AS a UNION ALL SELECT 1) s \
             WHERE a NOT IN (SELECT x FROM h2 WHERE x > 100000)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["2".to_string()]]);
    let r = run(
        &mut eng,
        "SELECT count(*) FROM (SELECT NULL::int AS a UNION ALL SELECT 1) s \
             WHERE a IN (SELECT x FROM h2 WHERE x > 100000)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["0".to_string()]]);
}

#[test]
fn v079_subscript_pg19_semantics() {
    // v0.79: PG19-grounded subscript/slice semantics (REL_19_STABLE
    // gram.y, parse_node.c, arraysubs.c, arrayfuncs.c):
    // - adjacent brackets are ONE multidimensional operation;
    // - partial subscript (fewer indices than dims) is NULL;
    // - >6 brackets is 54000;
    // - non-integer index is 42804;
    // - slices reset lower bounds to 1 (array_get_slice);
    // - prepend readjusts the lower bound to the input's.
    let mut eng = engine();
    let r = run(&mut eng, "SELECT ('{{1,2},{3,4}}'::int[])[2][1]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["3".to_string()]]);
    let r = run(&mut eng, "SELECT ('{{1,2},{3,4}}'::int[])[2]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["NULL".to_string()]]);
    let r = run(
        &mut eng,
        "SELECT (ARRAY[10,20,30])[5], (ARRAY[10,20,30])[0]",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec!["NULL".to_string(), "NULL".to_string()]]
    );
    let r = run(&mut eng, "SELECT (NULL::int[])[1], (ARRAY[1])[NULL]").unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec!["NULL".to_string(), "NULL".to_string()]]
    );
    let err = run(&mut eng, "SELECT 5[1]").unwrap_err();
    assert_eq!(err.code, "42804");
    let err = run(&mut eng, "SELECT (ARRAY[1])['x']").unwrap_err();
    assert_eq!(err.code, "42804");
    let err = run(&mut eng, "SELECT ('{1}'::int[])[1][1][1][1][1][1][1]").unwrap_err();
    assert_eq!(err.code, "54000");
    let r = run(&mut eng, "SELECT ('{1,2,3,4}'::int[])[2:3]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["{2,3}".to_string()]]);
    let r = run(&mut eng, "SELECT array_dims(('{1,2,3,4}'::int[])[2:3])").unwrap();
    assert_eq!(rows_of(r), vec![vec!["[1:2]".to_string()]]);
    // Mixed chain: any slice makes the whole chain a slice; a
    // plain [i] becomes [1:i].
    let r = run(&mut eng, "SELECT ('{{1,2,3},{4,5,6}}'::int[])[1:2][2:3]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["{{2,3},{5,6}}".to_string()]]);
    // Empty slice range -> PG's empty (0-dim) array.
    let r = run(&mut eng, "SELECT ('{1,2}'::int[])[3:1]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["{}".to_string()]]);
    // Prepend keeps the input's lower bound (PG19 array_prepend).
    let r = run(&mut eng, "SELECT 0 || '{1,2}'::int[]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["{0,1,2}".to_string()]]);
    // Array function result types (inference, not just runtime).
    let r = run(
        &mut eng,
        "SELECT pg_typeof(array_length(ARRAY[1],1)), pg_typeof(cardinality(ARRAY[1])), \
             pg_typeof(array_ndims(ARRAY[1])), pg_typeof(array_dims(ARRAY[1])), \
             pg_typeof(unnest(ARRAY['a']))",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec![
            "integer".to_string(),
            "integer".to_string(),
            "integer".to_string(),
            "text".to_string(),
            "text".to_string()
        ]]
    );
}

#[test]
fn v079_array_index_type_check() {
    // v0.79: PG19 `array_subscript_transform` coerces every subscript
    // (and slice bound) to int4: int2/int4/int8 indices are accepted, a
    // NULL literal index is coercible (NULL at execution), and anything
    // else raises 42804 "array subscript must have type integer".
    let mut eng = engine();
    // int2 / int8 indices are accepted.
    let r = run(&mut eng, "SELECT (ARRAY[10,20,30])[1::smallint]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["10".to_string()]]);
    let r = run(&mut eng, "SELECT (ARRAY[10,20,30])[2::bigint]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["20".to_string()]]);
    // NULL literal index passes the type check; NULL at execution.
    let r = run(&mut eng, "SELECT (ARRAY[1])[NULL]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["NULL".to_string()]]);
    // Non-integer index: 42804.
    let err = run(&mut eng, "SELECT (ARRAY[1])['x']").unwrap_err();
    assert_eq!(err.code, "42804");
    assert!(
        err.message
            .contains("array subscript must have type integer")
    );
    let err = run(&mut eng, "SELECT (ARRAY[1])[1.5]").unwrap_err();
    assert_eq!(err.code, "42804");
    assert!(
        err.message
            .contains("array subscript must have type integer")
    );
    // Slice bounds are validated the same way.
    let err = run(&mut eng, "SELECT (ARRAY[1,2])['a':2]").unwrap_err();
    assert_eq!(err.code, "42804");
    let r = run(&mut eng, "SELECT ('{1,2,3}'::int[])[1::smallint:2::bigint]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["{1,2}".to_string()]]);
}

#[test]
fn v079_empty_array_ctor_cast() {
    // v0.79: `ARRAY[]::int[]` takes its element type from the cast
    // (PG's parse analysis coerces the empty ArrayExpr); a bare
    // `ARRAY[]` is still 42P08.
    let mut eng = engine();
    let r = run(&mut eng, "SELECT array[]::int[]").unwrap();
    assert_eq!(rows_of(r), vec![vec!["{}".to_string()]]);
    let r = run(&mut eng, "SELECT pg_typeof(array[]::int[])").unwrap();
    assert_eq!(rows_of(r), vec![vec!["integer[]".to_string()]]);
    let r = run(&mut eng, "SELECT cardinality(array[]::text[])").unwrap();
    assert_eq!(rows_of(r), vec![vec!["0".to_string()]]);
    let err = run(&mut eng, "SELECT array[]").unwrap_err();
    assert_eq!(err.code, "42P08");
}

/// v0.88: CREATE INDEX accepts DESC / NULLS FIRST|LAST key options and
/// stores them as catalog metadata. The tree is still built in
/// canonical ascending order, so ORDER BY results are unaffected.
#[test]
fn v088_desc_index_metadata_and_order() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t88 (a int, b int)").unwrap();
    run(&mut eng, "INSERT INTO t88 VALUES (3, 1), (1, 2), (2, 3)").unwrap();
    run(&mut eng, "CREATE INDEX t88_desc ON t88 (a DESC)").unwrap();
    let ix = &eng.db.indexes["t88_desc"];
    assert_eq!(ix.def.desc, vec![true]);
    // PG default for a DESC key is NULLS FIRST.
    assert_eq!(ix.def.nulls_first, vec![true]);
    assert!(ix.def.planner_usable);
    assert!(!ix.tree.is_empty());
    // Explicit NULLS FIRST on a DESC key is stored too.
    run(&mut eng, "CREATE INDEX t88_nf ON t88 (b DESC NULLS FIRST)").unwrap();
    let ix = &eng.db.indexes["t88_nf"];
    assert_eq!(ix.def.desc, vec![true]);
    assert_eq!(ix.def.nulls_first, vec![true]);
    // Multi-key: direction is per-column.
    run(
        &mut eng,
        "CREATE INDEX t88_m ON t88 (a ASC, b DESC NULLS LAST)",
    )
    .unwrap();
    let ix = &eng.db.indexes["t88_m"];
    assert_eq!(ix.def.desc, vec![false, true]);
    assert_eq!(ix.def.nulls_first, vec![false, false]);
    // ORDER BY correctness is unaffected by the stored direction.
    let r = run(&mut eng, "SELECT a FROM t88 ORDER BY a").unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["1".to_string()],
            vec!["2".to_string()],
            vec!["3".to_string()]
        ]
    );
    let r = run(&mut eng, "SELECT a FROM t88 ORDER BY a DESC").unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec!["3".to_string()],
            vec!["2".to_string()],
            vec!["1".to_string()]
        ]
    );
}

/// v0.88: expression indexes are accepted as catalog-only definitions
/// (planner_usable = false, nothing built) and DML never touches them
/// — the `usize::MAX` key positions must not reach `key_for`.
#[test]
fn v088_expression_index_is_catalog_only() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t88e (a int)").unwrap();
    run(&mut eng, "CREATE UNIQUE INDEX t88e_fn ON t88e ((a * a))").unwrap();
    let ix = &eng.db.indexes["t88e_fn"];
    assert!(!ix.def.planner_usable);
    assert_eq!(ix.def.exprs, vec![Some("a * a".to_string())]);
    assert!(ix.tree.is_empty());
    // DML with an expression index present must not panic (this hit
    // `committed_unique_violation` -> `key_for` before the v0.88
    // planner_usable gate).
    run(&mut eng, "INSERT INTO t88e VALUES (1), (2)").unwrap();
    run(&mut eng, "UPDATE t88e SET a = 5 WHERE a = 1").unwrap();
    run(&mut eng, "DELETE FROM t88e WHERE a = 2").unwrap();
    assert!(eng.db.indexes["t88e_fn"].tree.is_empty());
    let r = run(&mut eng, "SELECT a FROM t88e").unwrap();
    assert_eq!(rows_of(r), vec![vec!["5".to_string()]]);
}

/// v0.88: partial indexes (`WHERE` predicate) are likewise
/// catalog-only: accepted, stored, never built or consulted.
#[test]
fn v088_partial_index_is_catalog_only() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t88p (a int)").unwrap();
    run(&mut eng, "CREATE INDEX t88p_idx ON t88p (a) WHERE a > 0").unwrap();
    let ix = &eng.db.indexes["t88p_idx"];
    assert!(!ix.def.planner_usable);
    assert_eq!(ix.def.predicate, Some("a > 0".to_string()));
    assert!(ix.tree.is_empty());
    run(&mut eng, "INSERT INTO t88p VALUES (1), (-1)").unwrap();
    assert!(eng.db.indexes["t88p_idx"].tree.is_empty());
    let r = run(&mut eng, "SELECT a FROM t88p WHERE a > 0").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
}

/// v0.88: `DROP INDEX a, b, ...` drops every named index; a missing
/// name aborts with 42P01.
#[test]
fn v088_drop_index_multi_name() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t88d (a int)").unwrap();
    // v0.89: `USING btree` is accepted (and ignored — rustgres only
    // implements btree); any other access method is 0A000.
    run(&mut eng, "CREATE UNIQUE INDEX u88 ON t88d USING btree (a)").unwrap();
    assert!(eng.db.indexes["u88"].def.planner_usable);
    let err = run(&mut eng, "CREATE INDEX u88h ON t88d USING hash (a)").unwrap_err();
    assert_eq!(err.code, "0A000");
    run(&mut eng, "CREATE INDEX d1 ON t88d (a)").unwrap();
    run(&mut eng, "CREATE INDEX d2 ON t88d (a)").unwrap();
    run(&mut eng, "DROP INDEX d1, d2").unwrap();
    // Drops are MVCC (dropped_xmax), not map removals.
    assert_ne!(eng.db.indexes["d1"].def.dropped_xmax, 0);
    assert_ne!(eng.db.indexes["d2"].def.dropped_xmax, 0);
    let err = run(&mut eng, "DROP INDEX nope_missing").unwrap_err();
    assert_eq!(err.code, "42P01");
    // A missing name anywhere in the list aborts the whole statement
    // with 42P01, and (like PostgreSQL) no index is dropped — the
    // existing-first/missing-later order is the case that caught the
    // old sequential implementation.
    run(&mut eng, "CREATE INDEX d3 ON t88d (a)").unwrap();
    let err = run(&mut eng, "DROP INDEX d3, nope_missing").unwrap_err();
    assert_eq!(err.code, "42P01");
    assert_eq!(eng.db.indexes["d3"].def.dropped_xmax, 0);
    let err = run(&mut eng, "DROP INDEX nope_missing, d3").unwrap_err();
    assert_eq!(err.code, "42P01");
    assert_eq!(eng.db.indexes["d3"].def.dropped_xmax, 0);
}

/// v0.88: the virtual pg_attribute exposes attrelid/attname/attnum;
/// the regression suite's RIGHT JOIN introspection query returns the
/// expected row.
#[test]
fn v088_pg_attribute_virtual() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "SELECT attname, attnum FROM pg_attribute WHERE attname = 'uid'",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["uid".to_string(), "2".to_string()]]);
    // The fixture tables share oid 0, so the join test uses a
    // SQL-created table (which gets a distinct OID, like the server).
    run(&mut eng, "CREATE TABLE pa88 (x int, y int)").unwrap();
    let r = run(
        &mut eng,
        "SELECT tname, attname FROM (SELECT relname AS tname, * \
             FROM (SELECT * FROM pg_class c) ss1) ss2 \
             RIGHT JOIN pg_attribute a ON a.attrelid = ss2.oid \
             WHERE tname = 'pa88' AND attnum = 1",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["pa88".to_string(), "x".to_string()]]);
}

/// v0.95: parse_ident identifier parsing (PG19 parse_ident).
#[test]
fn v95_parse_ident_parts() {
    // Basic qualified name.
    assert_eq!(
        parse_ident_parts("foo.boo", true).unwrap(),
        vec!["foo".to_string(), "boo".to_string()]
    );
    // Unquoted folds to lowercase.
    assert_eq!(
        parse_ident_parts("Foo.Boo", true).unwrap(),
        vec!["foo".to_string(), "boo".to_string()]
    );
    // Quoted preserves case.
    assert_eq!(
        parse_ident_parts("\"Foo\".\"Boo\"", true).unwrap(),
        vec!["Foo".to_string(), "Boo".to_string()]
    );
    // Doubled quotes collapse.
    assert_eq!(
        parse_ident_parts("\"a\"\"b\"", true).unwrap(),
        vec!["a\"b".to_string()]
    );
    // Surrounding whitespace skipped.
    assert_eq!(
        parse_ident_parts(" foo. boo ", true).unwrap(),
        vec!["foo".to_string(), "boo".to_string()]
    );
    // Strict rejects trailing garbage.
    assert!(parse_ident_parts("foo.boo[]", true).is_err());
    // Non-strict returns the valid prefix.
    assert_eq!(
        parse_ident_parts("foo.boo[]", false).unwrap(),
        vec!["foo".to_string(), "boo".to_string()]
    );
    // Empty input is an error in strict mode.
    assert!(parse_ident_parts("", true).is_err());
    assert!(parse_ident_parts("", false).unwrap().is_empty());
}

/// v1.15: quote_ident (PG19 quote_identifier, ruleutils.c).
#[test]
fn v115_quote_ident() {
    // Safe shapes pass through unquoted.
    assert_eq!(pg_quote_identifier_fn("abc"), "abc");
    assert_eq!(pg_quote_identifier_fn("_foo"), "_foo");
    assert_eq!(pg_quote_identifier_fn("a1"), "a1");
    assert_eq!(pg_quote_identifier_fn("a_b_c9"), "a_b_c9");
    // Unreserved keywords do not need quoting (PG quotes keywords
    // except unreserved ones).
    assert_eq!(pg_quote_identifier_fn("abort"), "abort");
    // Reserved / col_name / type_func_name keywords are quoted.
    assert_eq!(pg_quote_identifier_fn("select"), "\"select\"");
    assert_eq!(pg_quote_identifier_fn("all"), "\"all\"");
    assert_eq!(pg_quote_identifier_fn("user"), "\"user\"");
    assert_eq!(pg_quote_identifier_fn("between"), "\"between\"");
    assert_eq!(pg_quote_identifier_fn("natural"), "\"natural\"");
    // Unsafe shapes are quoted.
    assert_eq!(pg_quote_identifier_fn("a b"), "\"a b\"");
    assert_eq!(pg_quote_identifier_fn("Abc"), "\"Abc\"");
    assert_eq!(pg_quote_identifier_fn("1abc"), "\"1abc\"");
    assert_eq!(pg_quote_identifier_fn("a$b"), "\"a$b\"");
    assert_eq!(pg_quote_identifier_fn(""), "\"\"");
    // Embedded double quotes are doubled.
    assert_eq!(pg_quote_identifier_fn("a\"b"), "\"a\"\"b\"");
    // Non-ASCII is never safe (PG's byte-wise ASCII check).
    assert_eq!(pg_quote_identifier_fn("caf\u{e9}"), "\"caf\u{e9}\"");
}

/// v1.15: quote_literal (PG19 quote_literal_cstr, quote.c).
#[test]
fn v115_quote_literal() {
    assert_eq!(pg_quote_literal_cstr(""), "''");
    assert_eq!(pg_quote_literal_cstr("abc"), "'abc'");
    // Embedded quotes are doubled.
    assert_eq!(pg_quote_literal_cstr("abc'"), "'abc'''");
    assert_eq!(pg_quote_literal_cstr("a'b'c"), "'a''b''c'");
    // A backslash triggers E'' syntax with doubled backslashes
    // (works on servers with standard_conforming_strings=off).
    assert_eq!(pg_quote_literal_cstr("\\"), "E'\\\\'");
    assert_eq!(pg_quote_literal_cstr("a\\b'c"), "E'a\\\\b''c'");
    // No backslash: plain quotes even with other escapes.
    assert_eq!(pg_quote_literal_cstr("a\nb"), "'a\nb'");
}

/// v1.15: quote_nullable NULL handling (strictness per pg_proc.dat).
#[test]
fn v115_quote_strictness() {
    // quote_ident / quote_literal are STRICT: NULL -> NULL.
    let null = vec![Value::Null];
    assert_eq!(eval_str_func("quote_ident", &null).unwrap(), Value::Null);
    assert_eq!(eval_str_func("quote_literal", &null).unwrap(), Value::Null);
    // quote_nullable maps NULL to the text 'NULL'.
    assert_eq!(
        eval_str_func("quote_nullable", &null).unwrap(),
        Value::text("NULL")
    );
    // Non-null values quote normally.
    let v = vec![Value::text("a'b")];
    assert_eq!(
        eval_str_func("quote_nullable", &v).unwrap(),
        Value::text("'a''b'")
    );
    let v = vec![Value::text("sel ect")];
    assert_eq!(
        eval_str_func("quote_ident", &v).unwrap(),
        Value::text("\"sel ect\"")
    );
}

/// v0.95: degenerate grouping detection (HAVING without GROUP BY or
/// aggregates).
#[test]
fn v95_is_degenerate_grouping() {
    let stmt = parse_statement("SELECT 1 FROM t WHERE 1/a = 1 HAVING 1 < 2").unwrap();
    if let crate::sql::Stmt::Select(s) = stmt {
        assert!(is_degenerate_grouping(&s));
    } else {
        panic!("expected SELECT");
    }
    // With an aggregate, not degenerate.
    let stmt2 = parse_statement("SELECT count(*) FROM t HAVING count(*) > 1").unwrap();
    if let crate::sql::Stmt::Select(s) = stmt2 {
        assert!(!is_degenerate_grouping(&s));
    } else {
        panic!("expected SELECT");
    }
    // With GROUP BY, not degenerate.
    let stmt3 = parse_statement("SELECT a FROM t GROUP BY a HAVING 1 < 2").unwrap();
    if let crate::sql::Stmt::Select(s) = stmt3 {
        assert!(!is_degenerate_grouping(&s));
    } else {
        panic!("expected SELECT");
    }
}
// v0.98: PostgreSQL 19 sequence parity
// ========================================================================

/// The subselect.sql regression: a volatile predicate (nextval) must
/// not be pushed down and evaluated twice. Ten rows survive, each
/// advancing the sequence once; the final nextval is 11 (PG19), not 21.
#[test]
fn v98_volatile_predicate_not_pushed_down() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t10(x int)").unwrap();
    for i in 0..10 {
        run(&mut eng, &format!("INSERT INTO t10 VALUES ({})", i)).unwrap();
    }
    run(&mut eng, "CREATE SEQUENCE ts1").unwrap();
    let r = run(
        &mut eng,
        "SELECT * FROM (SELECT DISTINCT x FROM t10) ss WHERE x < 10 + nextval('ts1') ORDER BY 1",
    )
    .unwrap();
    let rows = rows_of(r);
    assert_eq!(rows.len(), 10);
    let r = run(&mut eng, "SELECT nextval('ts1')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["11".to_string()]]);
    let r = run(&mut eng, "SELECT currval('ts1')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["11".to_string()]]);
}

/// A volatile SQL-language UDF wrapping nextval is also evaluated
/// once per surviving row, never pushed down and doubled.
#[test]
fn v98_volatile_udf_not_pushed_down() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t10(x int)").unwrap();
    for i in 0..10 {
        run(&mut eng, &format!("INSERT INTO t10 VALUES ({})", i)).unwrap();
    }
    run(&mut eng, "CREATE SEQUENCE uvs").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION unv() RETURNS bigint VOLATILE LANGUAGE sql AS $$ SELECT nextval('uvs') $$",
    )
    .unwrap();
    let r = run(
        &mut eng,
        "SELECT * FROM (SELECT DISTINCT x FROM t10) ss WHERE x < 10 + unv() ORDER BY 1",
    )
    .unwrap();
    assert_eq!(rows_of(r).len(), 10);
    let r = run(&mut eng, "SELECT nextval('uvs')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["11".to_string()]]);
}

/// currval: 55000 before the first nextval in the session.
#[test]
fn v98_currval_undefined_before_nextval() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE cuv").unwrap();
    assert_eq!(err_code(&mut eng, "SELECT currval('cuv')"), "55000");
    run(&mut eng, "SELECT nextval('cuv')").unwrap();
    let r = run(&mut eng, "SELECT currval('cuv')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
}

/// lastval: 55000 before any nextval; tracks the most recent nextval
/// across sequences; setval never touches it.
#[test]
fn v98_lastval_tracks_most_recent_nextval() {
    let mut eng = engine();
    assert_eq!(err_code(&mut eng, "SELECT lastval()"), "55000");
    run(&mut eng, "CREATE SEQUENCE la START WITH 100").unwrap();
    run(&mut eng, "CREATE SEQUENCE lb START WITH 200").unwrap();
    run(&mut eng, "SELECT nextval('la')").unwrap();
    let r = run(&mut eng, "SELECT lastval()").unwrap();
    assert_eq!(rows_of(r), vec![vec!["100".to_string()]]);
    run(&mut eng, "SELECT nextval('lb')").unwrap();
    let r = run(&mut eng, "SELECT lastval()").unwrap();
    assert_eq!(rows_of(r), vec![vec!["200".to_string()]]);
    run(&mut eng, "SELECT setval('la', 500)").unwrap();
    let r = run(&mut eng, "SELECT lastval()").unwrap();
    assert_eq!(rows_of(r), vec![vec!["200".to_string()]]);
}

/// setval(n, true): next nextval returns n + increment.
/// setval(n, false): next nextval returns n itself.
#[test]
fn v98_setval_is_called_semantics() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE sv").unwrap();
    let r = run(&mut eng, "SELECT setval('sv', 41)").unwrap();
    assert_eq!(rows_of(r), vec![vec!["41".to_string()]]);
    let r = run(&mut eng, "SELECT nextval('sv')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["42".to_string()]]);
    let r = run(&mut eng, "SELECT setval('sv', 77, false)").unwrap();
    assert_eq!(rows_of(r), vec![vec!["77".to_string()]]);
    let r = run(&mut eng, "SELECT nextval('sv')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["77".to_string()]]);
}

/// PG19: setval(n, false) does NOT change the value reported by currval
/// — a defined currval keeps its old value, an undefined one stays
/// undefined (55000). (Docs: "the value reported by currval is not
/// changed in this case".)
#[test]
fn v98_setval_false_leaves_currval() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE cf").unwrap();
    run(&mut eng, "SELECT nextval('cf')").unwrap(); // currval = 1
    let r = run(&mut eng, "SELECT setval('cf', 77, false)").unwrap();
    assert_eq!(rows_of(r), vec![vec!["77".to_string()]]);
    let r = run(&mut eng, "SELECT currval('cf')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
    // Never-called sequence: currval stays undefined after setval false.
    run(&mut eng, "CREATE SEQUENCE cu").unwrap();
    run(&mut eng, "SELECT setval('cu', 77, false)").unwrap();
    assert_eq!(err_code(&mut eng, "SELECT currval('cu')"), "55000");
}

/// setval outside [min_value, max_value] is 22003 (PG19 sequence.c
/// do_setval), not a silent clamp.
#[test]
fn v98_setval_out_of_bounds_is_22003() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE SEQUENCE bd MINVALUE 1 MAXVALUE 10 START WITH 1",
    )
    .unwrap();
    assert_eq!(err_code(&mut eng, "SELECT setval('bd', 99)"), "22003");
    assert_eq!(err_code(&mut eng, "SELECT setval('bd', 0)"), "22003");
    // Boundary values are fine.
    run(&mut eng, "SELECT setval('bd', 10)").unwrap();
    run(&mut eng, "SELECT setval('bd', 1)").unwrap();
}

/// ALTER SEQUENCE ... RESTART WITH outside [min,max] is 22023
/// (PG19: "RESTART value (n) cannot be greater than MAXVALUE (max)").
#[test]
fn v98_restart_out_of_bounds_is_22023() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE SEQUENCE rs MINVALUE 1 MAXVALUE 10 START WITH 1",
    )
    .unwrap();
    assert_eq!(
        err_code(&mut eng, "ALTER SEQUENCE rs RESTART WITH 99"),
        "22023"
    );
    assert_eq!(
        err_code(&mut eng, "ALTER SEQUENCE rs RESTART WITH 0"),
        "22023"
    );
    run(&mut eng, "ALTER SEQUENCE rs RESTART WITH 5").unwrap();
    let r = run(&mut eng, "SELECT nextval('rs')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["5".to_string()]]);
}

/// CACHE is accepted on CREATE/ALTER and surfaced in pg_sequences;
/// CACHE < 1 is 22023.
#[test]
fn v98_cache_option() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE cs CACHE 20").unwrap();
    let r = run(
        &mut eng,
        "SELECT cache_size FROM pg_sequences WHERE sequencename = 'cs'",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["20".to_string()]]);
    run(&mut eng, "ALTER SEQUENCE cs CACHE 50").unwrap();
    let r = run(
        &mut eng,
        "SELECT cache_size FROM pg_sequences WHERE sequencename = 'cs'",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["50".to_string()]]);
    assert_eq!(err_code(&mut eng, "CREATE SEQUENCE cs0 CACHE 0"), "22023");
    // Bare START n is legal PG19 (START [ WITH ] n).
    run(&mut eng, "CREATE SEQUENCE cs1 START 7").unwrap();
    let r = run(&mut eng, "SELECT nextval('cs1')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["7".to_string()]]);
}

/// OWNED BY: the sequence is dropped with the owning table; a bad
/// target errors and leaves no leaked sequence behind.
#[test]
fn v98_owned_by_drop_dependency() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE own_t(a int)").unwrap();
    run(&mut eng, "CREATE SEQUENCE own_s OWNED BY own_t.a").unwrap();
    run(&mut eng, "DROP TABLE own_t").unwrap();
    assert_eq!(err_code(&mut eng, "SELECT nextval('own_s')"), "42P01");
    // Invalid target: error, and nothing leaks.
    assert!(err_code(&mut eng, "CREATE SEQUENCE own_bad OWNED BY nosuch_t.a") != "00000");
    let r = run(
        &mut eng,
        "SELECT sequencename FROM pg_sequences WHERE sequencename = 'own_bad'",
    )
    .unwrap();
    assert!(rows_of(r).is_empty());
    // OWNED BY NONE clears the link; the sequence survives the drop.
    run(&mut eng, "CREATE TABLE own_t2(a int, b int)").unwrap();
    run(&mut eng, "CREATE SEQUENCE own_s2").unwrap();
    run(&mut eng, "ALTER SEQUENCE own_s2 OWNED BY own_t2.b").unwrap();
    run(&mut eng, "ALTER SEQUENCE own_s2 OWNED BY NONE").unwrap();
    run(&mut eng, "DROP TABLE own_t2").unwrap();
    let r = run(&mut eng, "SELECT nextval('own_s2')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
}

/// DROP COLUMN drops the sequences owned by that column only
/// (PG19 AUTO dependency); other columns' sequences survive.
#[test]
fn v98_owned_by_drop_column() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE dco_t(a int, b int)").unwrap();
    run(&mut eng, "CREATE SEQUENCE dco_a OWNED BY dco_t.a").unwrap();
    run(&mut eng, "CREATE SEQUENCE dco_b OWNED BY dco_t.b").unwrap();
    run(&mut eng, "ALTER TABLE dco_t DROP COLUMN a").unwrap();
    assert_eq!(err_code(&mut eng, "SELECT nextval('dco_a')"), "42P01");
    let r = run(&mut eng, "SELECT nextval('dco_b')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["1".to_string()]]);
    run(&mut eng, "ALTER TABLE dco_t DROP COLUMN b CASCADE").unwrap();
    assert_eq!(err_code(&mut eng, "SELECT nextval('dco_b')"), "42P01");
}

/// OWNED BY requires the table and sequence to share an owner
/// (PG19 42832 "sequence must have same owner and schema as table");
/// same-owner links are accepted, and a refused CREATE leaks nothing.
#[test]
fn v98_owned_by_owner_restriction() {
    let mut eng = engine();
    run(&mut eng, "CREATE ROLE owr_r").unwrap();
    run(&mut eng, "CREATE TABLE owr_t(a int)").unwrap();
    run(&mut eng, "CREATE SEQUENCE owr_s").unwrap();
    // Same owner: fine.
    run(&mut eng, "ALTER SEQUENCE owr_s OWNED BY owr_t.a").unwrap();
    // Transfer the table away: re-linking is refused with 42832.
    run(&mut eng, "ALTER TABLE owr_t OWNER TO owr_r").unwrap();
    run(&mut eng, "ALTER SEQUENCE owr_s OWNED BY NONE").unwrap();
    assert_eq!(
        err_code(&mut eng, "ALTER SEQUENCE owr_s OWNED BY owr_t.a"),
        "42832"
    );
    // CREATE with a mismatched owner is refused and leaks nothing.
    assert_eq!(
        err_code(&mut eng, "CREATE SEQUENCE owr_s2 OWNED BY owr_t.a"),
        "42832"
    );
    let r = run(
        &mut eng,
        "SELECT sequencename FROM pg_sequences WHERE sequencename = 'owr_s2'",
    )
    .unwrap();
    assert!(rows_of(r).is_empty());
}

/// ALTER SEQUENCE IF EXISTS on a missing sequence is a no-op;
/// without IF EXISTS it is 42P01.
#[test]
fn v98_alter_sequence_if_exists() {
    let mut eng = engine();
    run(&mut eng, "ALTER SEQUENCE IF EXISTS nosuch RESTART").unwrap();
    assert_eq!(err_code(&mut eng, "ALTER SEQUENCE nosuch RESTART"), "42P01");
}

/// Exhaustion is PG19 22000 (not 55000); CYCLE wraps to the bound;
/// descending defaults are start -1 / min PG_INT64_MIN / max -1
/// (PG19 init_params; v0.99 corrected the old -(2^63-1) default).
#[test]
fn v98_sequence_overflow_and_cycle() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE ov_s START WITH 9 MAXVALUE 10").unwrap();
    let r = run(&mut eng, "SELECT nextval('ov_s'), nextval('ov_s')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["9".to_string(), "10".to_string()]]);
    assert_eq!(err_code(&mut eng, "SELECT nextval('ov_s')"), "22000");
    run(
        &mut eng,
        "CREATE SEQUENCE cy_s START WITH 9 MINVALUE 1 MAXVALUE 10 CYCLE",
    )
    .unwrap();
    let r = run(
        &mut eng,
        "SELECT nextval('cy_s'), nextval('cy_s'), nextval('cy_s')",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec!["9".to_string(), "10".to_string(), "1".to_string()]]
    );
    run(&mut eng, "CREATE SEQUENCE dn_s INCREMENT BY -1").unwrap();
    let r = run(
        &mut eng,
        "SELECT start_value, min_value, max_value, increment_by \
             FROM pg_sequences WHERE sequencename = 'dn_s'",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec![
            "-1".to_string(),
            "-9223372036854775808".to_string(),
            "-1".to_string(),
            "-1".to_string(),
        ]]
    );
    let r = run(&mut eng, "SELECT nextval('dn_s'), nextval('dn_s')").unwrap();
    assert_eq!(rows_of(r), vec![vec!["-1".to_string(), "-2".to_string()]]);
}

/// pg_sequences exposes the PG19 column shape, including schemaname
/// and cache_size; information_schema.sequences likewise.
#[test]
fn v98_sequence_catalogs() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE cat_s CACHE 9").unwrap();
    let r = run(
        &mut eng,
        "SELECT schemaname, sequencename, sequenceowner, data_type, \
             start_value, min_value, max_value, increment_by, cache_size \
             FROM pg_sequences WHERE sequencename = 'cat_s'",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec![
            "public".to_string(),
            "cat_s".to_string(),
            "postgres".to_string(),
            "bigint".to_string(),
            "1".to_string(),
            "1".to_string(),
            "9223372036854775807".to_string(),
            "1".to_string(),
            "9".to_string(),
        ]]
    );
    let r = run(
        &mut eng,
        "SELECT sequence_catalog, sequence_schema, sequence_name, data_type, \
             numeric_precision, numeric_precision_radix, numeric_scale, cycle_option \
             FROM information_schema.sequences WHERE sequence_name = 'cat_s'",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec![
            "rustgres".to_string(),
            "public".to_string(),
            "cat_s".to_string(),
            "bigint".to_string(),
            "64".to_string(),
            "2".to_string(),
            "0".to_string(),
            "NO".to_string(),
        ]]
    );
}

/// v0.99: explicit `AS` sequence types (PG19 init_params).
#[test]
fn v99_sequence_explicit_types() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE t_small AS smallint").unwrap();
    run(&mut eng, "CREATE SEQUENCE t_int AS int").unwrap();
    run(&mut eng, "CREATE SEQUENCE t_big AS bigint").unwrap();
    // descending smallint: type-driven defaults (max -1, min
    // type-min, start max).
    run(
        &mut eng,
        "CREATE SEQUENCE t_desc AS smallint INCREMENT BY -1",
    )
    .unwrap();
    let r = run(
        &mut eng,
        "SELECT sequencename, data_type, min_value, max_value, start_value \
             FROM pg_sequences WHERE sequencename LIKE 't_%' ORDER BY 1",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec![
                "t_big".to_string(),
                "bigint".to_string(),
                "1".to_string(),
                "9223372036854775807".to_string(),
                "1".to_string(),
            ],
            vec![
                "t_desc".to_string(),
                "smallint".to_string(),
                "-32768".to_string(),
                "-1".to_string(),
                "-1".to_string(),
            ],
            vec![
                "t_int".to_string(),
                "integer".to_string(),
                "1".to_string(),
                "2147483647".to_string(),
                "1".to_string(),
            ],
            vec![
                "t_small".to_string(),
                "smallint".to_string(),
                "1".to_string(),
                "32767".to_string(),
                "1".to_string(),
            ],
        ]
    );
    // Explicit bounds outside the type range are 22023.
    let e = run(
        &mut eng,
        "CREATE SEQUENCE t_bad AS smallint MAXVALUE 100000",
    )
    .unwrap_err();
    assert_eq!(e.code, "22023");
    // pg_sequences exposes `cycle`, not `cycle_option`.
    let r = run(
        &mut eng,
        "SELECT cycle FROM pg_sequences WHERE sequencename = 't_small'",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec!["f".to_string()]]);
    let e = run(&mut eng, "SELECT cycle_option FROM pg_sequences").unwrap_err();
    assert_eq!(e.code, "42703");
}

/// v0.99: ALTER ... AS resets default bounds, keeps explicit ones.
#[test]
fn v99_alter_sequence_as_resets_default_bounds() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE a_dflt").unwrap();
    run(&mut eng, "ALTER SEQUENCE a_dflt AS smallint").unwrap();
    let r = run(
        &mut eng,
        "SELECT data_type, min_value, max_value FROM pg_sequences \
             WHERE sequencename = 'a_dflt'",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec![
            "smallint".to_string(),
            "1".to_string(),
            "32767".to_string(),
        ]]
    );
    run(&mut eng, "CREATE SEQUENCE a_exp MAXVALUE 1000").unwrap();
    run(&mut eng, "ALTER SEQUENCE a_exp AS smallint").unwrap();
    let r = run(
        &mut eng,
        "SELECT data_type, min_value, max_value FROM pg_sequences \
             WHERE sequencename = 'a_exp'",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec![
            "smallint".to_string(),
            "1".to_string(),
            "1000".to_string(),
        ]]
    );
}

/// v1.23: PG19 `FigureColnameInternal` (parse_target.c) for array
/// expressions. A subscript/slice with no field name in the
/// indirection is named after its operand (`T_A_Indirection`
/// recurses into `ind->arg`), and `ARRAY[...]` / `ARRAY(subquery)`
/// act like functions named `array` (`T_A_ArrayExpr`,
/// `T_SubLink`/`ARRAY_SUBLINK`). Previously all of these came out
/// as `?column?`.
fn col_name_of(sql: &str) -> String {
    let crate::sql::Stmt::Select(sel) = parse_statement(sql).unwrap() else {
        panic!("not a SELECT: {sql}");
    };
    let [crate::sql::SelectItem::Expr { expr, alias: None }] = sel.items.as_slice() else {
        panic!("unexpected select list: {sql}");
    };
    expr_col_name(expr)
}

#[test]
fn v123_array_expr_col_names() {
    // The five conformance statements: subscripts over a scalar
    // subquery, over a bare ARRAY constructor, and a slice with
    // subquery bounds.
    assert_eq!(col_name_of("SELECT (SELECT ARRAY[1,2,3])[1]"), "array");
    assert_eq!(col_name_of("SELECT ((SELECT ARRAY[1,2,3]))[2]"), "array");
    assert_eq!(col_name_of("SELECT (((SELECT ARRAY[1,2,3])))[3]"), "array");
    assert_eq!(
        col_name_of("SELECT (array[1,2])[(SELECT 1)] FROM generate_series(1, 1) g(i)"),
        "array"
    );
    assert_eq!(
        col_name_of("SELECT (array[1,2])[(SELECT 1):(SELECT 2)] FROM generate_series(1, 1) g(i)"),
        "array"
    );
    // Bare constructors and ARRAY(subquery) are named `array`.
    assert_eq!(col_name_of("SELECT ARRAY[1,2,3]"), "array");
    assert_eq!(col_name_of("SELECT ARRAY(SELECT 1)"), "array");
    // A subscript over a plain column keeps the column's name.
    assert_eq!(col_name_of("SELECT arr[1] FROM (SELECT 1 AS arr) t"), "arr");
    // A cast of an array constructor keeps `array` (PG19: the
    // inner name has strength 2, so the type-name fallback does
    // not apply).
    assert_eq!(col_name_of("SELECT ARRAY[1,2]::text[]"), "array");
}

/// v1.24: PG19 text_format() VARIADIC handling (varlena.c).
/// %s/%I/%L stringify through the type output function (bool ->
/// t/f, not true/false), and a NULL VARIADIC array expands to zero
/// arguments for format() only — concat/concat_ws still return NULL.
#[test]
fn v124_format_variadic_pg19() {
    let mut eng = engine();
    let mut one = |sql: &str| -> String {
        let r = run(&mut eng, sql).expect(sql);
        let rows = rows_of(r);
        assert_eq!(rows.len(), 1, "{sql}");
        rows[0][0].clone()
    };
    // The two conformance statements (text.sql).
    assert_eq!(
        one("select format('%s, %s', variadic array[true, false])"),
        "t, f"
    );
    assert_eq!(one("select format('Hello', variadic NULL::int[])"), "Hello");
    // Bool %s uses the output function in all positions.
    assert_eq!(one("select format('%s', true)"), "t");
    assert_eq!(one("select format('%s-%s', false, true)"), "f-t");
    // The ::text[] twin is unaffected (text elements already).
    assert_eq!(
        one("select format('%s, %s', variadic array[true, false]::text[])"),
        "true, false"
    );
    // VARIADIC NULL stays whole-call NULL for concat/concat_ws.
    assert_eq!(one("select concat(variadic NULL::int[]) is NULL"), "t");
    assert_eq!(
        one("select concat_ws(',', variadic NULL::int[]) is NULL"),
        "t"
    );
    // NULL argument rendering per conversion: %s -> '', %L -> NULL.
    assert_eq!(one("select format('[%s]', NULL)"), "[]");
    assert_eq!(one("select format('[%L]', NULL)"), "[NULL]");
    // Non-variadic format() is unchanged.
    assert_eq!(one("select format('%s, %s', 'a', 'b')"), "a, b");
    assert_eq!(one("select format('%I', 'mytab')"), "mytab");
}

/// v1.25: PG19 width_bucket_float8 (float.c) computes in float64, not
/// exact decimal. Verbatim-port cases from numeric.sql's LATERAL
/// overflow test (item 686): row 6 is the diverger (exact decimal
/// yields 1, PG yields 2 via the divide-by-2 overflow path).
#[test]
fn v125_width_bucket_float8_pg19() {
    let mut eng = engine();
    // The corpus statement's 12 bucket values (numeric.out).
    let buckets = run(
            &mut eng,
            "SELECT width_bucket(oper, low, high, cnt) FROM (SELECT 1.797e+308::float8 AS big, 5e-324::float8 AS tiny) AS v, LATERAL (VALUES (10.5::float8, -big, big, 1), (10.5::float8, -big, big, 2), (10.5::float8, -big, big, 3), (big / 4, -big / 2, big / 2, 10), (10.5::float8, big, -big, 1), (10.5::float8, big, -big, 2), (10.5::float8, big, -big, 3), (big / 4, big / 2, -big / 2, 10), (0, 0, tiny, 4), (tiny, 0, tiny, 4), (0, 0, 1, 2147483647), (1, 1, 0, 2147483647)) AS sample(oper, low, high, cnt)",
        )
        .expect("width_bucket corpus statement");
    let got: Vec<String> = rows_of(buckets).into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        got,
        vec!["1", "2", "2", "8", "1", "2", "2", "3", "1", "5", "1", "1"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    );
    // All-float8 error paths keep PG's codes/messages.
    let e = run(
        &mut eng,
        "SELECT width_bucket(5.0::float8, 3.0::float8, 4.0::float8, 0)",
    )
    .unwrap_err();
    assert_eq!(e.code, "22023");
    assert_eq!(e.message, "count must be greater than zero");
    let e = run(
        &mut eng,
        "SELECT width_bucket(3.5::float8, 3.0::float8, 3.0::float8, 888)",
    )
    .unwrap_err();
    assert_eq!(e.code, "22023");
    assert_eq!(e.message, "lower bound cannot equal upper bound");
    // count+1 int32 overflow -> 22003, like PG's pg_add_s32_overflow.
    let e = run(
        &mut eng,
        "SELECT width_bucket(2.0::float8, 0.0::float8, 1.0::float8, 2147483647)",
    )
    .unwrap_err();
    assert_eq!(e.code, "22003");
    // Single-value checks go through the closure, defined after the
    // last direct `run` so the mutable borrow of `eng` doesn't
    // overlap (E0499).
    let mut one = |sql: &str| -> String {
        let r = run(&mut eng, sql).expect(sql);
        let rows = rows_of(r);
        assert_eq!(rows.len(), 1, "{sql}");
        rows[0][0].clone()
    };
    // NaN operand on float8 bounds -> count + 1.
    assert_eq!(
        one("SELECT width_bucket('NaN'::float8, 3.0::float8, 4.0::float8, 888)"),
        "889"
    );
    // Mixed-type calls stay on the exact-decimal path (no behavior
    // change): the v0.56 roundoff-hazard cases.
    assert_eq!(one("SELECT width_bucket(0, -1e100::float8, 1, 10)"), "10");
    assert_eq!(one("SELECT width_bucket(1, 1e100::float8, 0, 10)"), "10");
    // The numeric overload is untouched.
    assert_eq!(one("SELECT width_bucket(5.0, 3.0, 4.0, 10)"), "11");
    assert_eq!(one("SELECT width_bucket(3.5, 4.0, 3.0, 10)"), "6");
}
