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
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn setup(eng: &mut Engine) {
    for ddl in [
        "CREATE TABLE t1 (a int, b text)",
        "CREATE TABLE t2 (a int, c int)",
    ] {
        run(eng, ddl).unwrap();
    }
}

#[test]
fn verbose_output_scan_select_then_where_cols() {
    // v1.46: scan tlist = select-list cols, then WHERE-slice cols
    // (PG19 `add_new_columns_to_pathtarget` order).
    // v1.52: VERBOSE schema-qualifies the scan name — PG19
    // `ExplainTargetRel` (explain.c) sets the namespace only when
    // `es->verbose` and text prints ` on %s.%s`. The v1.46 expectation
    // encoded the pre-v1.50 unqualified rendering.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT a FROM t1 WHERE b = 'x'",
    );
    assert_eq!(
        lines,
        vec![
            "Seq Scan on public.t1".to_string(),
            "  Output: a, b".to_string(),
            // v1.54: VERBOSE qualifies scan Filters (PG `useprefix`).
            "  Filter: (t1.b = 'x'::text)".to_string(),
        ]
    );
}

#[test]
fn verbose_output_line_right_after_node_line() {
    // v1.46: PG19 `ExplainNode` prints Output: immediately after the
    // node line, before Filter / Join Filter / Index Cond / Sort Key.
    // v1.52: `public.` per `ExplainTargetRel` verbose-only namespace
    // (explain.c).
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT a, b FROM t1 WHERE a > 1",
    );
    assert_eq!(lines[0], "Seq Scan on public.t1");
    assert_eq!(lines[1], "  Output: a, b");
    // v1.54: VERBOSE qualifies scan Filters (PG `useprefix`,
    // verified against PG16: `Filter: (t1.a > 0)`).
    assert_eq!(lines[2], "  Filter: (t1.a > 1)");
}

#[test]
fn verbose_output_star_expands_in_table_order() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM t1");
    assert_eq!(
        lines,
        // v1.52: `public.` per `ExplainTargetRel` verbose-only namespace
        // (explain.c).
        vec![
            "Seq Scan on public.t1".to_string(),
            "  Output: a, b".to_string(),
        ]
    );
}

#[test]
fn verbose_output_join_uses_rtable_qualification() {
    // v1.46: `useprefix = rtable_size > 1` — multi-table outputs are
    // alias-qualified; the top join's tlist is the query targetlist.
    // v1.52: `public.` per `ExplainTargetRel` verbose-only namespace
    // (explain.c).
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT t1.a, t2.c FROM t1, t2 WHERE t1.a = t2.a",
    );
    assert_eq!(
        lines,
        vec![
            "Nested Loop".to_string(),
            "  Output: t1.a, t2.c".to_string(),
            "  Join Filter: (t1.a = t2.a)".to_string(),
            "  ->  Seq Scan on public.t1".to_string(),
            "        Output: t1.a".to_string(),
            "  ->  Seq Scan on public.t2".to_string(),
            "        Output: t2.c".to_string(),
        ]
    );
}

#[test]
fn verbose_output_top_join_is_targetlist_not_concat() {
    // v1.46: the top join shows the query targetlist even when the
    // inputs need extra columns (t2.c only for the filter).
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT t1.a FROM t1, t2 WHERE t2.c > 0",
    );
    assert_eq!(lines[0], "Nested Loop");
    assert_eq!(lines[1], "  Output: t1.a");
    assert!(
        lines.contains(&"        Output: t2.c".to_string()),
        "{lines:?}"
    );
}

#[test]
fn verbose_output_star_join_expands_all_tables() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM t1, t2",
    );
    assert_eq!(lines[1], "  Output: t1.a, t1.b, t2.a, t2.c");
}

#[test]
fn verbose_output_subquery_scan_alias_qualified() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM (SELECT a AS x FROM t1) ss",
    );
    assert_eq!(
        lines,
        // v1.54: top-level simple subquery pulls up (PG
        // `pull_up_simple_subquery`) — flat Seq Scan, not a
        // Subquery Scan. Verified against PG16.
        vec![
            "Seq Scan on public.t1".to_string(),
            "  Output: t1.a".to_string(),
        ]
    );
}

#[test]
fn verbose_output_values_scan_alias_qualified() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT * FROM (VALUES (1, 2)) AS v(p, q)",
    );
    assert_eq!(
        lines,
        vec!["Values Scan".to_string(), "  Output: v.p, v.q".to_string(),]
    );
}

#[test]
fn verbose_output_sort_limit_show_targetlist() {
    // v1.46: Sort/Limit tlists are the query targetlist (resjunk
    // hidden); the scan below still carries the sort column.
    // v1.52: `public.` per `ExplainTargetRel` verbose-only namespace
    // (explain.c).
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT a FROM t1 ORDER BY b LIMIT 2",
    );
    assert_eq!(
        lines,
        vec![
            "Limit".to_string(),
            "  Output: a".to_string(),
            "  ->  Sort".to_string(),
            "        Output: a".to_string(),
            "        Sort Key: t1.b".to_string(),
            "        ->  Seq Scan on public.t1".to_string(),
            "              Output: a, b".to_string(),
        ]
    );
}

#[test]
fn verbose_output_aggregate_plain_group_key() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT a FROM t1 GROUP BY a",
    );
    assert_eq!(lines[0], "Aggregate");
    assert_eq!(lines[1], "  Output: a");
}

#[test]
fn verbose_output_omitted_when_no_faithful_spelling() {
    // v1.46: strict deparse — `count(*)` has no pg_expr_text
    // spelling, so the Aggregate prints no Output line at all
    // (never a wrong line).
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) SELECT a, count(*) FROM t1 GROUP BY a",
    );
    // The Aggregate itself prints no Output line (strict omission);
    // the scan below still shows its faithfully-known tlist.
    // v1.52: `public.` per `ExplainTargetRel` verbose-only namespace
    // (explain.c).
    assert_eq!(lines[0], "Aggregate");
    assert_eq!(lines[1], "  ->  Seq Scan on public.t1");
    assert_eq!(lines[2], "        Output: a");
}

#[test]
fn verbose_output_result_no_from() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF, VERBOSE) SELECT 1");
    assert_eq!(
        lines,
        vec!["Result".to_string(), "  Output: 1".to_string(),]
    );
}

#[test]
fn nonverbose_output_byte_identical() {
    // v1.46: without VERBOSE the plan text is unchanged (no Output
    // lines anywhere).
    let mut eng = engine();
    setup(&mut eng);
    for sql in [
        "EXPLAIN (COSTS OFF) SELECT a, b FROM t1 WHERE a > 1",
        "EXPLAIN (COSTS OFF) SELECT t1.a, t2.c FROM t1, t2 WHERE t1.a = t2.a",
        "EXPLAIN (COSTS OFF) SELECT * FROM (SELECT a AS x FROM t1) ss",
    ] {
        let lines = plan_lines(&mut eng, sql);
        assert!(
            !lines.iter().any(|l| l.contains("Output:")),
            "{sql}: {lines:?}"
        );
    }
}

#[test]
fn verbose_output_cte_scan() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF, VERBOSE) WITH x AS (SELECT a AS q FROM t1) SELECT * FROM x",
    );
    // v1.54: single-ref simple CTE inlines (PG `inline_cte`), then the
    // top-level subquery pulls up — PG renders a flat Seq Scan.
    assert_eq!(lines[0], "Seq Scan on public.t1");
    assert_eq!(lines[1], "  Output: t1.a");
}

#[test]
fn verbose_output_empty_tlist_prints_no_line() {
    // v1.46: `SELECT 1 FROM t1` needs no columns from the scan —
    // PG's show_plan_tlist NIL check prints no Output line.
    // v1.52: `public.` per `ExplainTargetRel` verbose-only namespace
    // (explain.c). And the Output line IS printed: PG19
    // `apply_scanjoin_target_to_paths` applies the FULL final
    // targetlist (including the Const `1`) to the top scan path —
    // verified on live PG16: `Output: 1` (PG16 prints the same as
    // PG19 here per createplan.c). Neither the v1.46 expectation
    // (no Output line) nor the v1.50 behavior (`Output: a, b` via
    // the all-columns fallback) was PG-correct.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(&mut eng, "EXPLAIN (COSTS OFF, VERBOSE) SELECT 1 FROM t1");
    assert_eq!(
        lines,
        vec![
            "Seq Scan on public.t1".to_string(),
            "  Output: 1".to_string()
        ]
    );
}

/// v1.50: `is_simple_subquery` rejects aggregates.
#[test]
fn v150_simple_subquery_rejects_aggregate() {
    let stmt = parse_statement("SELECT COUNT(*) FROM t1").unwrap();
    if let crate::sql::Stmt::Select(s) = stmt {
        assert!(!is_simple_subquery(&s));
    } else {
        panic!("expected SELECT");
    }
}

/// v1.50: `is_simple_subquery` accepts a plain SELECT.
#[test]
fn v150_simple_subquery_accepts_plain() {
    let stmt = parse_statement("SELECT a, b FROM t1 WHERE c > 1").unwrap();
    if let crate::sql::Stmt::Select(s) = stmt {
        assert!(is_simple_subquery(&s));
    } else {
        panic!("expected SELECT");
    }
}

/// v1.50: `is_simple_subquery` rejects LIMIT.
#[test]
fn v150_simple_subquery_rejects_limit() {
    let stmt = parse_statement("SELECT a FROM t1 LIMIT 10").unwrap();
    if let crate::sql::Stmt::Select(s) = stmt {
        assert!(!is_simple_subquery(&s));
    } else {
        panic!("expected SELECT");
    }
}
