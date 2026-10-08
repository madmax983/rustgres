/// v1.37: planner-time Const substitution for IMMUTABLE SQL-function
/// calls (PG19 `eval_const_expressions`/`evaluate_function`). Once a
/// fold-eligible `Expr::Func` node evaluates, the call-site memo
/// (`Q::plan_fold_memo`) makes it behave as a Const for the rest of
/// the statement: later evaluations skip argument evaluation, the
/// eligibility walk, and the value-keyed cache. The memo is keyed by
/// node address, so it is per call site; eligibility now also rejects
/// bodies naming a relation a caller-visible CTE shadows (FROM
/// resolves CTE-first), closing the v1.36 hole where the
/// value-keyed cache leaked one call site's CTE-visible value into
/// another site's.
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

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn col0(rows: Vec<Vec<String>>) -> Vec<String> {
    rows.into_iter().map(|r| r[0].clone()).collect()
}

/// v1.37: the v1.36 hole — a caller CTE shadowing a catalog table
/// name made the (name, args)-keyed value cache leak one call
/// site's value into another's (`7,7` instead of `7,100`). The
/// call inside the CTE's scope is now ineligible (it reads caller
/// state), so each site evaluates against what IT sees.
#[test]
fn cte_shadow_per_site_correctness_inner_first() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t_sh(v int)").unwrap();
    run(&mut eng, "INSERT INTO t_sh VALUES (100)").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION f_sh(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT v FROM t_sh'",
    )
    .unwrap();
    // Inner call site sees the CTE (7); outer sees the table (100).
    // v1.36 returned 7,7 here.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT x.f1, f_sh(1) FROM \
                 (WITH t_sh AS (SELECT 7 AS v) SELECT f_sh(1) AS f1) x",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["7".to_string(), "100".to_string()]]);
}

/// v1.37: same hole, opposite evaluation order — the outer site
/// folds first (populating the value cache with 100); the inner
/// site must still see its CTE (7), not the cached 100.
#[test]
fn cte_shadow_per_site_correctness_outer_first() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t_sh2(v int)").unwrap();
    run(&mut eng, "INSERT INTO t_sh2 VALUES (100)").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION f_sh2(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT v FROM t_sh2'",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT f_sh2(1), x.f1 FROM \
                 (WITH t_sh2 AS (SELECT 7 AS v) SELECT f_sh2(1) AS f1) x",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["100".to_string(), "7".to_string()]]);
}

/// v1.37: a body-local CTE (not caller-visible) still folds — the
/// shadow check only rejects caller-visible CTE names.
#[test]
fn body_local_cte_still_folds() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION cte137() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'WITH w AS (SELECT 42 AS v) SELECT v FROM w'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT cte137() FROM generate_series(1, 3) g").unwrap());
    assert_eq!(
        col0(rows),
        vec!["42".to_string(), "42".to_string(), "42".to_string()]
    );
}

/// v1.37: a body reading a plain catalog table (no CTE anywhere)
/// still folds across rows.
#[test]
fn catalog_table_body_still_folds() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE base137(v int)").unwrap();
    run(&mut eng, "INSERT INTO base137 VALUES (5)").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION cat137() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT v FROM base137'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT cat137() FROM generate_series(1, 3) g").unwrap());
    assert_eq!(
        col0(rows),
        vec!["5".to_string(), "5".to_string(), "5".to_string()]
    );
}

/// v1.37: a caller CTE shadowing a name the body reads through a
/// derived table still blocks the fold (the walk descends into
/// derived-table subqueries).
#[test]
fn cte_shadow_through_derived_table() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t_shd(v int)").unwrap();
    run(&mut eng, "INSERT INTO t_shd VALUES (100)").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION f_shd(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT v FROM (SELECT v FROM t_shd) d'",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "WITH t_shd AS (SELECT 7 AS v) SELECT f_shd(1), (SELECT v FROM t_shd)",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["7".to_string(), "7".to_string()]]);
}

/// v1.37: a VOLATILE-marked function never touches the memo — a
/// sequence in the body yields a distinct value per row even with
/// two call sites in one statement.
#[test]
fn volatile_never_memoized_two_sites() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s137v").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION sv137() RETURNS int VOLATILE LANGUAGE sql \
             AS 'SELECT nextval(''s137v'')'",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT sv137(), sv137() FROM generate_series(1, 3) g",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 3);
    let flat: Vec<String> = rows.into_iter().flatten().collect();
    let distinct: HashSet<String> = flat.iter().cloned().collect();
    assert_eq!(distinct.len(), 6, "every call must draw fresh: {flat:?}");
}

/// v1.37: STABLE-marked functions never fold (PG folds STABLE only
/// in estimation mode) — the memo stays empty for them.
#[test]
fn stable_never_memoized() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s137s").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION ss137() RETURNS int STABLE LANGUAGE sql \
             AS 'SELECT nextval(''s137s'')'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT ss137() FROM generate_series(1, 3) g").unwrap());
    assert_eq!(
        col0(rows),
        vec!["1".to_string(), "2".to_string(), "3".to_string()]
    );
}

/// v1.37: errors are never memoized — the first row raises exactly
/// as in v1.36 (memo engages only on success).
#[test]
fn error_never_memoized() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION e137() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT 1/0'",
    )
    .unwrap();
    let e = run(&mut eng, "SELECT e137() FROM generate_series(1, 3) g").unwrap_err();
    assert_eq!(e.code, "22012");
}

/// v1.37: zero-argument fold-eligible calls memoize per call site.
#[test]
fn zero_arg_call_memoizes() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION z137() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT 99'",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT z137(), z137() FROM generate_series(1, 3) g",
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|r| r == &vec!["99".to_string(), "99".to_string()])
    );
}

/// v1.37: named-argument calls fold — the memo key is the call-site
/// node, not the (reordered) argument slice.
#[test]
fn named_arg_call_folds() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION add137(x int, y int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT $1 + $2'",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT add137(y => 2, x => 40) FROM generate_series(1, 3) g",
        )
        .unwrap(),
    );
    assert_eq!(
        col0(rows),
        vec!["42".to_string(), "42".to_string(), "42".to_string()]
    );
}

/// v1.37: a fold-eligible call nested inside a subquery (whose own
/// CTE does not shadow anything) folds to the same value.
#[test]
fn fold_inside_subquery_with_unrelated_cte() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION s137() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT 11'",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT (SELECT s137() FROM (SELECT 1) z) FROM generate_series(1, 2) g",
        )
        .unwrap(),
    );
    assert_eq!(col0(rows), vec!["11".to_string(), "11".to_string()]);
}

/// v1.37: body-internal call sites get their own (fresh-per-body-
/// execution) memo: the inner call folds within the body, the
/// outer call folds across statement rows, and every row is
/// correct.
#[test]
fn nested_body_internal_calls_fold() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION inner137(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT $1 * 10'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION outer137() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT sum(inner137(v)) FROM (VALUES (1),(2),(3)) t(v)'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT outer137() FROM generate_series(1, 5) g").unwrap());
    assert_eq!(
        col0(rows),
        vec![
            "60".to_string(),
            "60".to_string(),
            "60".to_string(),
            "60".to_string(),
            "60".to_string()
        ]
    );
}

/// v1.37: recursion still refuses the fold but executes normally.
#[test]
fn recursive_body_runs_unfolded() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION rec137(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT CASE WHEN $1 <= 0 THEN 0 ELSE rec137($1 - 1) + 1 END'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT rec137(3) FROM generate_series(1, 2) g").unwrap());
    assert_eq!(col0(rows), vec!["3".to_string(), "3".to_string()]);
}
