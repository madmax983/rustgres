/// v2.20: partition routing for non-int/text key types + direct
/// leaf DML. Root 1: `compare_partition_values` only ordered
/// integer/text pairs, so no RANGE bound ever contained a
/// numeric/date/float key (23514 at every level; update.sql's
/// part_b_10_b_20 subtree, keyed by `c numeric`, was the first
/// critical path). Root 2: UPDATE/MERGE handed a leaf target to
/// `find_partition_leaf` as if it were a root, so every direct
/// leaf UPDATE with a matched row failed 23514.
/// Ground truth: PG19 src/test/regress/sql/update.sql
/// (range_parted tree, part_def / list_default ok-fail pairs),
/// execPartition.c / nodeModifyTable.c row-movement rules.
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

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql)
        .map(|_| "OK".to_string())
        .unwrap_or_else(|e| e.code.to_string())
}

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn numeric_range_key_routes_flat() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE na (a int, c numeric) PARTITION BY RANGE (c)",
    );
    ok(
        &mut eng,
        "CREATE TABLE na_1 PARTITION OF na FOR VALUES FROM (1) TO (100)",
    );
    ok(
        &mut eng,
        "CREATE TABLE na_2 PARTITION OF na FOR VALUES FROM (100) TO (200)",
    );
    ok(&mut eng, "INSERT INTO na VALUES (1, 50)");
    ok(&mut eng, "INSERT INTO na VALUES (2, 150)");
    // A key below every bound still fails 23514.
    assert_eq!(
        err_code(&mut eng, "INSERT INTO na VALUES (3, 500)"),
        "23514"
    );
    assert_eq!(rows(&mut eng, "SELECT a FROM na_1"), vec![vec![s("1")]]);
    assert_eq!(rows(&mut eng, "SELECT a FROM na_2"), vec![vec![s("2")]]);
}

#[test]
fn date_and_float_range_keys_route() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE da (f1 date, f2 text) PARTITION BY RANGE (f1)",
    );
    ok(
        &mut eng,
        "CREATE TABLE da_1 PARTITION OF da FOR VALUES FROM ('2016-01-01') TO ('2016-08-01')",
    );
    ok(
        &mut eng,
        "CREATE TABLE da_2 PARTITION OF da FOR VALUES FROM ('2016-08-01') TO ('2017-01-01')",
    );
    ok(&mut eng, "INSERT INTO da VALUES ('2016-07-15', 'x')");
    ok(&mut eng, "INSERT INTO da VALUES ('2016-08-15', 'y')");
    assert_eq!(rows(&mut eng, "SELECT f2 FROM da_1"), vec![vec![s("x")]]);
    assert_eq!(rows(&mut eng, "SELECT f2 FROM da_2"), vec![vec![s("y")]]);

    ok(
        &mut eng,
        "CREATE TABLE fa (a float8, b int) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE fa_1 PARTITION OF fa FOR VALUES FROM (0.0) TO (10.0)",
    );
    ok(&mut eng, "INSERT INTO fa VALUES (5.5, 1)");
    assert_eq!(
        err_code(&mut eng, "INSERT INTO fa VALUES (55.5, 2)"),
        "23514"
    );
    assert_eq!(rows(&mut eng, "SELECT b FROM fa_1"), vec![vec![s("1")]]);
}

/// The update.sql tree shape: permuted ATTACHed subtree root keyed
/// by numeric, DROP/ADD-history grandchild keyed by abs(d).
#[test]
fn permuted_numeric_subtree_routes_like_update_sql() {
    let mut eng = engine();
    for sql in [
        "CREATE TABLE range_parted (a text, b bigint, c numeric, d int, e varchar) PARTITION BY RANGE (a, b)",
        "CREATE TABLE part_b_10_b_20 (e varchar, c numeric, a text, b bigint, d int) PARTITION BY RANGE (c)",
        "CREATE TABLE part_b_1_b_10 PARTITION OF range_parted FOR VALUES FROM ('b', 1) TO ('b', 10)",
        "ALTER TABLE range_parted ATTACH PARTITION part_b_10_b_20 FOR VALUES FROM ('b', 10) TO ('b', 20)",
        "CREATE TABLE part_c_100_200 (e varchar, c numeric, a text, b bigint, d int) PARTITION BY range (abs(d))",
        "ALTER TABLE part_c_100_200 DROP COLUMN e, DROP COLUMN c, DROP COLUMN a",
        "ALTER TABLE part_c_100_200 ADD COLUMN c numeric, ADD COLUMN e varchar, ADD COLUMN a text",
        "ALTER TABLE part_c_100_200 DROP COLUMN b",
        "ALTER TABLE part_c_100_200 ADD COLUMN b bigint",
        "CREATE TABLE part_d_1_15 PARTITION OF part_c_100_200 FOR VALUES FROM (1) TO (15)",
        "CREATE TABLE part_d_15_20 PARTITION OF part_c_100_200 FOR VALUES FROM (15) TO (20)",
        "ALTER TABLE part_b_10_b_20 ATTACH PARTITION part_c_100_200 FOR VALUES FROM (100) TO (200)",
        "CREATE TABLE part_c_1_100 (e varchar, d int, c numeric, b bigint, a text)",
        "ALTER TABLE part_b_10_b_20 ATTACH PARTITION part_c_1_100 FOR VALUES FROM (1) TO (100)",
        "INSERT INTO range_parted VALUES ('b', 12, 96, 1), ('b', 15, 105, 16), ('b', 17, 105, 19)",
    ] {
        ok(&mut eng, sql);
    }
    // c=96 lands in the [1,100) leaf; c=105 rows fall through to the
    // abs(d)-keyed grandchildren.
    assert_eq!(
        rows(&mut eng, "SELECT c FROM part_c_1_100"),
        vec![vec![s("96")]]
    );
    assert_eq!(rows(&mut eng, "SELECT c FROM part_d_1_15").len(), 0);
    assert_eq!(rows(&mut eng, "SELECT c FROM part_d_15_20").len(), 2);
    // Direct insert into the subtree root and into the leaf.
    ok(
        &mut eng,
        "INSERT INTO part_b_10_b_20 VALUES (NULL, 97, 'b', 13, 2)",
    );
    ok(
        &mut eng,
        "INSERT INTO part_c_1_100 VALUES (NULL, 3, 98, 14, 'b')",
    );
    assert_eq!(
        rows(&mut eng, "SELECT c FROM part_c_1_100 ORDER BY 1").len(),
        3
    );
}

#[test]
fn direct_leaf_update_in_place_and_violation() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE g (a int, b int) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE g1 PARTITION OF g FOR VALUES FROM (0) TO (10)",
    );
    ok(
        &mut eng,
        "CREATE TABLE g2 PARTITION OF g FOR VALUES FROM (10) TO (20)",
    );
    ok(&mut eng, "INSERT INTO g VALUES (5, 1), (15, 2)");
    // Non-key update on the leaf: in place.
    ok(&mut eng, "UPDATE g1 SET b = 99 WHERE a = 5");
    assert_eq!(
        rows(&mut eng, "SELECT b FROM g1 WHERE a = 5"),
        vec![vec![s("99")]]
    );
    // Key-changing update on the leaf: PG forbids movement from a
    // direct leaf UPDATE — 23514, row untouched.
    assert_eq!(
        err_code(&mut eng, "UPDATE g1 SET a = 15 WHERE a = 5"),
        "23514"
    );
    assert_eq!(rows(&mut eng, "SELECT a FROM g1"), vec![vec![s("5")]]);
    // The same change through the root moves the row.
    ok(&mut eng, "UPDATE g SET a = 15 WHERE a = 5");
    assert_eq!(rows(&mut eng, "SELECT a FROM g1").len(), 0);
    assert_eq!(rows(&mut eng, "SELECT a FROM g2 ORDER BY 1").len(), 2);
}

#[test]
fn default_leaf_update_ok_fail_pair() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE g (a int, b int) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE g1 PARTITION OF g FOR VALUES FROM (0) TO (10)",
    );
    ok(&mut eng, "CREATE TABLE gd PARTITION OF g DEFAULT");
    ok(&mut eng, "INSERT INTO g VALUES (25, 7)");
    // Still matches no explicit bound: stays in the DEFAULT leaf.
    ok(&mut eng, "UPDATE gd SET b = 8 WHERE a = 25");
    ok(&mut eng, "UPDATE gd SET a = 26 WHERE a = 25");
    // Moving into an explicit partition's range from the leaf fails.
    assert_eq!(
        err_code(&mut eng, "UPDATE gd SET a = 5 WHERE a = 26"),
        "23514"
    );
    assert_eq!(rows(&mut eng, "SELECT b FROM gd"), vec![vec![s("8")]]);
}

#[test]
fn intermediate_update_must_keep_ancestor_bound() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE r (a text, b bigint, c int) PARTITION BY RANGE (a, b)",
    );
    ok(
        &mut eng,
        "CREATE TABLE sub (c int, a text, b bigint) PARTITION BY RANGE (c)",
    );
    ok(
        &mut eng,
        "ALTER TABLE r ATTACH PARTITION sub FOR VALUES FROM ('b', 10) TO ('b', 20)",
    );
    ok(&mut eng, "CREATE TABLE leaf (c int, a text, b bigint)");
    ok(
        &mut eng,
        "ALTER TABLE sub ATTACH PARTITION leaf FOR VALUES FROM (1) TO (100)",
    );
    ok(&mut eng, "INSERT INTO r VALUES ('b', 12, 50)");
    // b is the PARENT's key: pushing it out of sub's bound under r
    // must fail even though c still routes inside the subtree.
    assert_eq!(
        err_code(&mut eng, "UPDATE sub SET b = 25 WHERE c = 50"),
        "23514"
    );
    ok(&mut eng, "UPDATE sub SET b = 19 WHERE c = 50");
}

#[test]
fn numeric_list_partition_scale_equality() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE ln (c numeric) PARTITION BY LIST (c)",
    );
    ok(
        &mut eng,
        "CREATE TABLE ln_1 PARTITION OF ln FOR VALUES IN (1.00, 2.5)",
    );
    // Stored bound has scale 2; the row literal has scale 0. PG's
    // btree equality matches them; raw Value equality did not.
    ok(&mut eng, "INSERT INTO ln VALUES (1)");
    ok(&mut eng, "INSERT INTO ln VALUES (2.50)");
    assert_eq!(err_code(&mut eng, "INSERT INTO ln VALUES (3)"), "23514");
    assert_eq!(rows(&mut eng, "SELECT count(*) FROM ln_1").len(), 1);
}

/// The span-capture guard for the COLLATE slice: CREATE INDEX and
/// ON CONFLICT capture element key texts from raw token spans, and
/// gram.y's opt_collate sits AFTER the element expression — an
/// expression parser that swallows the postfix clause would leak it
/// into the index's key text (`lower(fruit) collate "C"`) and break
/// arbiter inference against the plain element (`lower(fruit)`).
/// insert_conflict.sql's both_index_expr_key shape, end to end.
#[test]
fn collate_in_index_element_keeps_arbiter_match() {
    let mut eng = engine();
    ok(&mut eng, "create table ict (key int4, fruit text)");
    ok(
        &mut eng,
        "create unique index both_index_expr_key on ict(key, lower(fruit) collate \"C\" text_pattern_ops)",
    );
    ok(&mut eng, "insert into ict values (0, 'Crowberry')");
    // Duplicate-element inference spec naming the bare expression:
    // must find both_index_expr_key, not 42P10.
    ok(
        &mut eng,
        "insert into ict values(0, 'Crowberry') on conflict (lower(fruit), key, lower(fruit), key) do nothing",
    );
    // And the explicit-collation element form still matches it.
    ok(
        &mut eng,
        "insert into ict values(0, 'Crowberry') on conflict (lower(fruit) collate \"C\", key, key) do nothing",
    );
    // The index is catalog-only (expression indexes are never built),
    // so per the v1.99 degradation both DO NOTHINGs validate and then
    // insert plain: 3 rows. A span leak would 42P10 both statements
    // above instead — that is the regression this test guards.
    assert_eq!(
        rows(&mut eng, "select count(*) from ict"),
        vec![vec![s("3")]]
    );
}

#[test]
fn collate_postfix_accepted_in_expressions() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a text)");
    ok(&mut eng, "INSERT INTO t VALUES ('x')");
    assert_eq!(
        rows(&mut eng, "SELECT a::text COLLATE \"C\" FROM t"),
        vec![vec![s("x")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT a FROM t WHERE a COLLATE \"C\" = 'x'"),
        vec![vec![s("x")]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT a FROM t ORDER BY a COLLATE pg_catalog.\"C\""
        ),
        vec![vec![s("x")]]
    );
}
