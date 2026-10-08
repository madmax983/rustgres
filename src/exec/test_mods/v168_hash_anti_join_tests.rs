// ============================================================================
// v1.68: NOT IN / NOT EXISTS → Hash Anti Join (PG19 subselect.c
// `convert_ANY_sublink_to_join` / `convert_EXISTS_sublink_to_join`).
// ============================================================================
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
    match run(eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .collect(),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn select_texts(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).expect("runs") {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("expected Select, got {:?}", other),
    }
}

fn setup_notnull(eng: &mut Engine) {
    run(eng, "CREATE TABLE not_null_tab (id int NOT NULL)").unwrap();
    run(eng, "INSERT INTO not_null_tab VALUES (1),(2),(3)").unwrap();
    run(eng, "CREATE TABLE null_tab (id int)").unwrap();
    run(eng, "INSERT INTO null_tab VALUES (1),(NULL),(3)").unwrap();
}

/// v1.68: T11 — `NOT IN` with both sides provably NOT NULL plans as a
/// PG19 `Hash Anti Join`; the pulled-up inner table collides with the
/// outer name, so `set_rtable_names` (ruleutils.c) renames it
/// `not_null_tab_1`. Byte-exact vs the subselect.out oracle.
#[test]
fn v168_not_in_both_notnull_is_hash_anti_join() {
    let mut eng = engine();
    setup_notnull(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM not_null_tab \
             WHERE id NOT IN (SELECT id FROM not_null_tab)",
    );
    assert_eq!(
        lines,
        vec![
            "Hash Anti Join".to_string(),
            "  Hash Cond: (not_null_tab.id = not_null_tab_1.id)".to_string(),
            "  ->  Seq Scan on not_null_tab".to_string(),
            "  ->  Hash".to_string(),
            "        ->  Seq Scan on not_null_tab not_null_tab_1".to_string(),
        ]
    );
}

/// v1.68: T12 — the inner side forced non-nullable by an
/// `IS NOT NULL` qual (`query_outputs_are_not_nullable` via
/// `find_subquery_safe_quals`, clauses.c:2075) still converts; the
/// qual renders as an unqualified inner `Filter:` (PG19
/// `show_scan_qual`: no prefix for plain scans).
#[test]
fn v168_not_in_inner_forced_notnull_keeps_filter() {
    let mut eng = engine();
    setup_notnull(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM not_null_tab \
             WHERE id NOT IN (SELECT id FROM null_tab WHERE id IS NOT NULL)",
    );
    assert_eq!(
        lines,
        vec![
            "Hash Anti Join".to_string(),
            "  Hash Cond: (not_null_tab.id = null_tab.id)".to_string(),
            "  ->  Seq Scan on not_null_tab".to_string(),
            "  ->  Hash".to_string(),
            "        ->  Seq Scan on null_tab".to_string(),
            "              Filter: (id IS NOT NULL)".to_string(),
        ]
    );
}

/// v1.68: T10 — correlated `NOT EXISTS` needs no nullability proof
/// (EXISTS is never NULL); the `LIMIT 1` is dropped by PG19
/// `simplify_EXISTS_query`, and the WHERE equijoin becomes the hash
/// clause with the outer var on the left.
#[test]
fn v168_not_exists_correlated_is_hash_anti_join() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE int4_tbl (f1 int)").unwrap();
    let lines = plan_lines(
        &mut eng,
        "explain (costs off) select * from int4_tbl o where not exists \
             (select 1 from int4_tbl i where i.f1=o.f1 limit 1)",
    );
    assert_eq!(
        lines,
        vec![
            "Hash Anti Join".to_string(),
            "  Hash Cond: (o.f1 = i.f1)".to_string(),
            "  ->  Seq Scan on int4_tbl o".to_string(),
            "  ->  Hash".to_string(),
            "        ->  Seq Scan on int4_tbl i".to_string(),
        ]
    );
}

/// v1.68: fail-closed — a nullable outer side must NOT convert (PG19:
/// "No ANTI JOIN: outer side is nullable"). The plan stays a plain
/// Seq Scan with the sublink as a filter.
#[test]
fn v168_nullable_outer_stays_seqscan() {
    let mut eng = engine();
    setup_notnull(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM null_tab \
             WHERE id NOT IN (SELECT id FROM not_null_tab)",
    );
    assert_eq!(lines[0], "Seq Scan on null_tab");
    assert!(
        !lines.iter().any(|l| l.contains("Anti Join")),
        "no anti join expected, got: {lines:?}"
    );
}

/// v1.68: fail-closed — a nullable inner side with no `IS NOT NULL`
/// qual must NOT convert (PG19: "No ANTI JOIN: inner side is
/// nullable").
#[test]
fn v168_nullable_inner_stays_seqscan() {
    let mut eng = engine();
    setup_notnull(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM not_null_tab \
             WHERE id NOT IN (SELECT id FROM null_tab)",
    );
    assert_eq!(lines[0], "Seq Scan on not_null_tab");
    assert!(
        !lines.iter().any(|l| l.contains("Anti Join")),
        "no anti join expected, got: {lines:?}"
    );
}

/// v1.68: the COSTS ON (legacy) rendering path is untouched by the
/// anti-join planner (the hook only fires for `pg == true`).
#[test]
fn v168_costs_on_unaffected() {
    let mut eng = engine();
    setup_notnull(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN SELECT * FROM not_null_tab \
             WHERE id NOT IN (SELECT id FROM not_null_tab)",
    );
    assert!(
        !lines.iter().any(|l| l.contains("Anti Join")),
        "COSTS ON must not use the anti-join path, got: {lines:?}"
    );
}

/// v1.68: PG19 `set_rtable_names` uniquification — first use keeps the
/// bare name, collisions get `_1`, `_2`, ... (ruleutils.c:4335-4370).
#[test]
fn v168_unique_names_collide_with_suffix() {
    assert_eq!(
        pg_anti_unique_names(&["a".to_string(), "a".to_string(), "b".to_string()]),
        vec!["a".to_string(), "a_1".to_string(), "b".to_string()]
    );
    assert_eq!(
        pg_anti_unique_names(&["a".to_string(), "a".to_string(), "a".to_string()]),
        vec!["a".to_string(), "a_1".to_string(), "a_2".to_string()]
    );
    // A suffixed name that is itself taken pushes the collision on.
    assert_eq!(
        pg_anti_unique_names(&["a_1".to_string(), "a".to_string(), "a".to_string()]),
        vec!["a_1".to_string(), "a".to_string(), "a_2".to_string()]
    );
}

/// v1.68: result identity — the classic `NOT IN` NULL trap. A NULL in
/// the inner set makes the whole predicate unknown, so NO rows come
/// back (the executor's own subplan evaluation; the EXPLAIN node is
/// display-only and never changes this).
#[test]
fn v168_not_in_null_trap_result_identity() {
    let mut eng = engine();
    setup_notnull(&mut eng);
    // null_tab = (1), (NULL), (3): the NULL poisons the NOT IN.
    assert_eq!(
        select_texts(
            &mut eng,
            "SELECT * FROM not_null_tab WHERE id NOT IN (SELECT id FROM null_tab)"
        ),
        Vec::<String>::new(),
    );
    // With the NULL filtered out, the anti join's rows appear.
    assert_eq!(
        select_texts(
            &mut eng,
            "SELECT * FROM not_null_tab \
                 WHERE id NOT IN (SELECT id FROM null_tab WHERE id IS NOT NULL)"
        ),
        vec!["2".to_string()],
    );
    // Both sides provably non-null: plain anti semantics.
    assert_eq!(
        select_texts(
            &mut eng,
            "SELECT * FROM not_null_tab WHERE id NOT IN (SELECT id FROM not_null_tab)"
        ),
        Vec::<String>::new(),
    );
}

/// v1.68: result identity — `NOT EXISTS` with NULL keys on both sides.
/// EXISTS/NOT EXISTS is never NULL itself, so NULL keys simply never
/// match (no poisoning, unlike `NOT IN`).
#[test]
fn v168_not_exists_null_keys_result_identity() {
    let mut eng = engine();
    setup_notnull(&mut eng);
    // not_null_tab(1,2,3) NOT EXISTS null_tab(1,NULL,3) on id=id:
    // 1 and 3 match, NULL never matches → only 2 survives.
    assert_eq!(
        select_texts(
            &mut eng,
            "SELECT * FROM not_null_tab o WHERE NOT EXISTS \
                 (SELECT 1 FROM null_tab i WHERE i.id = o.id)"
        ),
        vec!["2".to_string()],
    );
    // NULL outer keys never match either: null_tab's NULL row survives
    // (no NOT NULL constraint here, so the plan stays a filter — the
    // rows are what matter).
    let mut got = select_texts(
        &mut eng,
        "SELECT id FROM null_tab o WHERE NOT EXISTS \
             (SELECT 1 FROM not_null_tab i WHERE i.id = o.id)",
    );
    got.sort();
    assert_eq!(got, vec!["NULL".to_string()]);
}

/// v1.68: the EXPLAIN shape and the executed rows agree — the plan
/// claims an anti join and the executor returns exactly the anti
/// join's rows.
#[test]
fn v168_plan_shape_matches_executed_rows() {
    let mut eng = engine();
    setup_notnull(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM not_null_tab \
             WHERE id NOT IN (SELECT id FROM null_tab WHERE id IS NOT NULL)",
    );
    assert_eq!(lines[0], "Hash Anti Join");
    assert_eq!(
        select_texts(
            &mut eng,
            "SELECT * FROM not_null_tab \
                 WHERE id NOT IN (SELECT id FROM null_tab WHERE id IS NOT NULL)"
        ),
        vec!["2".to_string()],
    );
}
