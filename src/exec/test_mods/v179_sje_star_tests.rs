// ============================================================================
// v1.79: nested self-join elimination with SELECT * (PG19
// `remove_self_join_rel` rewriting Vars after star expansion), and
// LEFT JOIN Join Filter support (PG19 explain.c shows Join Filter for
// all join types).
// ============================================================================
use super::*;

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

fn plan_text(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn setup_sj(eng: &mut Engine) {
    run(eng, "CREATE TABLE sj (a int unique, b int, c int unique)").unwrap();
    run(eng, "INSERT INTO sj VALUES (1,1,1),(2,2,2)").unwrap();
}

#[test]
fn v179_nested_sje_star_expansion() {
    // PG19's `remove_self_join_rel` rewrites Vars after parse-time star
    // expansion, so `SELECT *` keeps both column sets with the removed
    // qualifier's Vars re-pointed at the kept instance. The nested SJE
    // must not fail closed on a bare star.
    let mut eng = engine();
    setup_sj(&mut eng);
    let plan = plan_text(
        &mut eng,
        "EXPLAIN (COSTS OFF) select * from sj p join sj q on p.a = q.a left join sj r on p.a + q.a = r.a",
    );
    // p is eliminated; only q and r remain.
    assert!(!plan.contains("sj p"), "p must be eliminated:\n{}", plan);
    assert!(plan.contains("Nested Loop Left Join"), "plan:\n{}", plan);
    // The outer ON clause is rewritten p->q.
    assert!(
        plan.contains("Join Filter: ((q.a + q.a) = r.a)"),
        "plan:\n{}",
        plan
    );
}

#[test]
fn v179_nested_sje_star_preserves_rows() {
    // Execution sees the rewritten statement: same rows as the
    // unoptimized join (p's columns valued from q).
    let mut eng = engine();
    setup_sj(&mut eng);
    let n = match run(
        &mut eng,
        "select * from sj p join sj q on p.a = q.a left join sj r on p.a + q.a = r.a",
    )
    .unwrap()
    {
        ExecResult::Select { rows, .. } => rows.len(),
        other => panic!("expected Select, got {:?}", other),
    };
    // 2 sj rows, each joins to 1 q row (unique a), left join to r.
    assert_eq!(n, 2);
}

#[test]
fn v179_left_join_filter_rendering() {
    // PG19 explain.c shows Join Filter for LEFT JOIN (not just inner).
    // The ON clause renders as Join Filter; WHERE on the left side
    // becomes a scan Filter.
    let mut eng = engine();
    setup_sj(&mut eng);
    let plan = plan_text(
        &mut eng,
        "EXPLAIN (COSTS OFF) select * from sj q left join sj r on q.a + q.a = r.a where q.a is not null",
    );
    assert!(
        plan.contains("Join Filter: ((q.a + q.a) = r.a)"),
        "plan:\n{}",
        plan
    );
    assert!(plan.contains("Filter: (a IS NOT NULL)"), "plan:\n{}", plan);
}
