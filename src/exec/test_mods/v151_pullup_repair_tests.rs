use super::*;
use crate::sql::parse_statement;
use std::time::Duration;

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

fn plan_lines(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap() {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|r| r[0].to_text().unwrap().trim().to_string())
            .collect(),
        other => panic!("expected EXPLAIN, got {other:?}"),
    }
}

fn parse_select(sql: &str) -> SelectStmt {
    match parse_statement(sql).unwrap() {
        crate::sql::Stmt::Select(s) => s,
        _ => panic!("expected SELECT"),
    }
}

/// Run EXPLAIN `sql` on a fresh engine in a worker thread. Returns
/// `None` if it does not finish within `secs` — a hang becomes a test
/// failure instead of wedging the whole suite.
fn explain_guarded(sql: &str, secs: u64) -> Option<Vec<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let sql = sql.to_string();
    std::thread::spawn(move || {
        let mut eng = engine();
        let lines = plan_lines(&mut eng, &sql);
        let _ = tx.send(lines);
    });
    rx.recv_timeout(Duration::from_secs(secs)).ok()
}

/// v1.51: a FROM-less Derived that is a direct Join child is not a
/// pullup candidate (there is nothing to splice; v1.50 looped on it
/// forever). Minimal repro of the v1.50 hang.
#[test]
fn v151_empty_from_derived_not_a_candidate() {
    let s = parse_select("select * from (select 0 as z) as t1 join (select 1 as y) as t2 on true");
    assert!(find_pullup_candidate(&s.from).is_none());
}

/// v1.51: a multi-FROM-item Derived is not a pullup candidate either
/// (the splice only handles exactly one FROM item; keeping the
/// Derived would make the fixpoint find it again forever). The SQL
/// parser folds comma lists into Cross joins, so this shape is built
/// by hand to pin the guard.
#[test]
fn v151_multi_from_derived_not_a_candidate() {
    let mut s = parse_select("select * from (select 1 as a from t1) as s join t3 on true");
    let FromItem::Join { left, .. } = &mut s.from[0] else {
        panic!("expected top-level join");
    };
    let FromItem::Derived { sub, .. } = left.as_mut() else {
        panic!("expected derived left child");
    };
    sub.from.push(FromItem::Table {
        name: "t2".to_string(),
        alias: None,
        col_aliases: Vec::new(),
        only: false,
    });
    assert_eq!(sub.from.len(), 2);
    assert!(find_pullup_candidate(&s.from).is_none());
}

/// v1.51: a LATERAL Derived is never pulled up (the documented claim;
/// pulling one up could expose an inner FROM-less derived as a direct
/// join child — the v1.50 two-step hang).
#[test]
fn v151_lateral_derived_not_pulled_up() {
    let mut eng = engine();
    for sql in [
        "CREATE TEMP TABLE lt1 (a int)",
        "INSERT INTO lt1 VALUES (1)",
    ] {
        run(&mut eng, sql).unwrap();
    }
    let s = parse_select("select * from lt1 join lateral (select lt1.a as b) as s on true");
    let snap = eng.take_snapshot();
    assert!(pull_up_simple_subqueries(&s, &eng, &snap, 9, 0).is_none());
}

/// v1.51: the v1.50 minimal hang repro completes (guarded: a regression
/// fails this test instead of hanging the suite).
#[test]
fn v151_minimal_repro_completes() {
    let lines = explain_guarded(
            "explain (costs off) select * from (select 0 as z) as t1 join (select 1 as y) as t2 on true",
            15,
        )
        .expect("v1.50 hang regression: EXPLAIN did not finish in 15s");
    assert!(!lines.is_empty());
    assert!(lines.iter().any(|l| l.contains("Nested Loop")));
}

/// v1.51: more hang shapes from the v1.50 wedge battery (self-contained;
/// the full 17-statement corpus battery runs in the conformance tally).
#[test]
fn v151_wedge_shapes_complete() {
    for sql in [
        // wedge #3 shape (lateral has LIMIT: not simple anyway).
        "explain (costs off) select * from (select 1 as x) ss1 left join (select 2 as y) ss2 on (true)",
        // nested FROM-less deriveds under a multi-FROM subquery.
        "explain (costs off) select * from (select 1 as a from (select 3 as c) q1, (select 4 as d) q2) s join (select 5 as e) t on true",
        // FROM-less derived under a left join (wedge #8 shape, no lateral).
        "explain (costs off) select * from (select 0 as z) as t1 left join (select true as a) as t2 on true",
    ] {
        explain_guarded(sql, 15).unwrap_or_else(|| panic!("hang regression on: {sql}"));
    }
}

/// v1.51: T2 #17786 — the pulled-up inner join's useless LEFT JOIN is
/// removed (PG19 `remove_useless_joins`: only the constant `42` is
/// needed above; `innertab.id` is PK-unique), so the const-false dummy
/// inner Result reports `Replaces: Scan on int8_tbl` exactly as the
/// `join.out` oracle requires.
#[test]
fn v151_t2_replaces_scan_on_int8_tbl() {
    let mut eng = engine();
    for sql in [
        "CREATE TEMP TABLE int4_tbl(f1 int4)",
        "INSERT INTO int4_tbl(f1) VALUES (0), (123456)",
        "CREATE TEMP TABLE int8_tbl(q1 int8, q2 int8)",
        "INSERT INTO int8_tbl VALUES (123, 456)",
        "CREATE TEMP TABLE innertab (id int8 primary key, dat1 int8)",
        "INSERT INTO innertab VALUES(123, 42)",
        "CREATE TEMP TABLE tenk1(unique1 int4, unique2 int4)",
        "INSERT INTO tenk1 VALUES (1, 1)",
    ] {
        run(&mut eng, sql).unwrap();
    }
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT f1, x FROM int4_tbl JOIN ((SELECT 42 AS x FROM int8_tbl LEFT JOIN innertab ON q1 = id) AS ss1 RIGHT JOIN tenk1 ON NULL) ON tenk1.unique1 = ss1.x OR tenk1.unique2 = ss1.x",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop",
            "->  Seq Scan on int4_tbl",
            "->  Materialize",
            "->  Nested Loop Left Join",
            "Join Filter: NULL::boolean",
            "Filter: ((tenk1.unique1 = (42)) OR (tenk1.unique2 = (42)))",
            "->  Seq Scan on tenk1",
            "->  Result",
            "Replaces: Scan on int8_tbl",
            "One-Time Filter: false",
        ]
    );
}

/// v1.51: T1 #17773 still emits its `Replaces: Scan on int8_tbl`
/// (no-regression anchor for the `join_is_removable` change).
#[test]
fn v151_t1_replaces_still_emitted() {
    let mut eng = engine();
    for sql in [
        "CREATE TEMP TABLE int4_tbl(f1 int4)",
        "INSERT INTO int4_tbl(f1) VALUES (0)",
        "CREATE TEMP TABLE int8_tbl(q1 int8, q2 int8)",
        "INSERT INTO int8_tbl VALUES (123, 456)",
        "CREATE TEMP TABLE innertab (id int8 primary key, dat1 int8)",
        "INSERT INTO innertab VALUES(123, 42)",
    ] {
        run(&mut eng, sql).unwrap();
    }
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) SELECT q2 FROM (SELECT q2, 'constant'::text AS x FROM int8_tbl LEFT JOIN innertab ON q2 = id) ss RIGHT JOIN int4_tbl ON NULL WHERE x >= x",
    );
    assert!(
        lines.iter().any(|l| l == "Replaces: Scan on int8_tbl"),
        "T1 lost its Replaces line: {lines:?}"
    );
}
