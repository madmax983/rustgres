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

fn plan_lines(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap() {
        ExecResult::Explain { rows, .. } => {
            rows.into_iter().map(|r| r[0].to_text().unwrap()).collect()
        }
        other => panic!("expected EXPLAIN, got {other:?}"),
    }
}

fn setup(eng: &mut Engine) {
    for sql in [
        "CREATE TEMP TABLE a (id int PRIMARY KEY, b_id int)",
        "INSERT INTO a VALUES (0, 0), (1, NULL)",
    ] {
        run(eng, sql).unwrap();
    }
}

/// v1.48: PG19 interpolates the join kind into the Nested Loop label
/// (explain.c `ExplainNode`); `ON true` renders no Join Filter
/// (restrictinfo.c "Constant-TRUE clauses are dropped in any case").
#[test]
fn target3_left_join_label_and_materialize() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) select a1.id from (a a1 left join a a2 on true) left join (a a3 left join a a4 on a3.id = a4.id) on a2.id = a3.id",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop Left Join".to_string(),
            "  ->  Seq Scan on a a1".to_string(),
            "  ->  Materialize".to_string(),
            "        ->  Seq Scan on a a2".to_string(),
        ],
        "{lines:?}"
    );
}

/// v1.48: inner joins keep the bare `Nested Loop` label; both inner
/// sides materialize; the `ON true` Join Filter is dropped.
#[test]
fn target5_inner_label_double_materialize() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) select 1 from a t1 left join a t2 on true inner join a t3 on true left join a t4 on t2.id = t4.id and t2.id = t3.id",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "  ->  Nested Loop Left Join".to_string(),
            "        ->  Seq Scan on a t1".to_string(),
            "        ->  Materialize".to_string(),
            "              ->  Seq Scan on a t2".to_string(),
            "  ->  Materialize".to_string(),
            "        ->  Seq Scan on a t3".to_string(),
        ],
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("Join Filter")),
        "{lines:?}"
    );
}

/// v1.48: a filtered outer scan keeps PG's plain (unmaterialized)
/// shape — rustgres scan estimates ignore filter selectivity, so a
/// filter makes the rescan-count estimate unreliable.
#[test]
fn filtered_self_join_not_materialized() {
    let mut eng = engine();
    for sql in [
        "create temp table sl(a int, b int, c int)",
        "create temp table sj (a int, b int, c int)",
        "insert into sj values (1, null), (null, 2), (2, 1)",
    ] {
        run(&mut eng, sql).unwrap();
    }
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) select * from sl t1, sl t2 where t1.a = t2.a and t1.b = 1 and t2.b = 2",
    );
    assert!(
        !lines.iter().any(|l| l.contains("Materialize")),
        "{lines:?}"
    );
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM sj j1, sj j2 WHERE j1.b = j2.b AND j1.a*j1.a = 1 AND j2.a*j2.a = 2",
    );
    assert!(
        !lines.iter().any(|l| l.contains("Materialize")),
        "{lines:?}"
    );
    assert_eq!(lines[0], "Nested Loop");
}

/// v1.48: single-row outers never materialize (no rescan can happen),
/// and right/full joins get PG's labels.
/// v1.49: PG19 `reduce_outer_joins` flips RIGHT to LEFT in the planner,
/// so the plan label is now `Nested Loop Left Join` (PG never emits a
/// Right-join plan label post-flip).
#[test]
fn single_row_outer_not_materialized() {
    let mut eng = engine();
    setup(&mut eng);
    run(&mut eng, "CREATE TEMP TABLE one (id int PRIMARY KEY)").unwrap();
    run(&mut eng, "INSERT INTO one VALUES (1)").unwrap();
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) select * from one o, a i");
    assert!(
        !lines.iter().any(|l| l.contains("Materialize")),
        "{lines:?}"
    );
    assert_eq!(lines[0], "Nested Loop");
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) select a1.id from a a1 right join a a2 on a1.id = a2.id",
    );
    assert_eq!(lines[0], "Nested Loop Left Join", "{lines:?}");
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) select a1.id from a a1 full join a a2 on a1.id = a2.id",
    );
    assert_eq!(lines[0], "Nested Loop Full Join", "{lines:?}");
}

/// v1.48: constant-TRUE is dropped from scan filters too, matching
/// PG19 (`WHERE true` renders no `Filter:` line).
#[test]
fn constant_true_scan_filter_dropped() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF) select * from a where true");
    assert_eq!(lines, vec!["Seq Scan on a".to_string()], "{lines:?}");
}
