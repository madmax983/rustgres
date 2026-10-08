/// v1.60: EXPLAIN-path folding of IMMUTABLE function calls with constant
/// arguments (PG19 `simplify_function`, clauses.c:5205-5297) and the
/// pulled-up function-scan constants (PG19 `pull_up_constant_function`,
/// prepjointree.c:2235).
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

fn int(i: i64) -> Expr {
    Expr::Literal(Literal::Int(i))
}

fn null() -> Expr {
    Expr::Literal(Literal::Null)
}

fn ufunc(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Func {
        name: name.to_string(),
        args,
    }
}

fn plan_lines(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).expect("runs") {
        ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| row[0].to_text().unwrap_or("NULL".to_string()))
            .map(|l| l.trim().to_string())
            .collect(),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn setup(eng: &mut Engine) {
    run(eng, "CREATE TABLE t1(a int)").unwrap();
    run(
            eng,
            "CREATE FUNCTION f_immutable_int4(i int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$ begin return i; end $$",
        )
        .unwrap();
    run(
            eng,
            "CREATE FUNCTION f_stable_int4(i int) RETURNS int LANGUAGE plpgsql STABLE AS $$ begin return i; end $$",
        )
        .unwrap();
    run(
            eng,
            "CREATE FUNCTION f_volatile_int4(i int) RETURNS int LANGUAGE plpgsql VOLATILE AS $$ begin return i; end $$",
        )
        .unwrap();
    run(
            eng,
            "CREATE FUNCTION f_strict_imm(i int) RETURNS int LANGUAGE plpgsql IMMUTABLE STRICT AS $$ begin return i; end $$",
        )
        .unwrap();
    run(
            eng,
            "CREATE FUNCTION f_recur(i int) RETURNS int LANGUAGE plpgsql IMMUTABLE AS $$ begin return f_recur(i); end $$",
        )
        .unwrap();
}

/// The immutable plpgsql function folds with constant args.
#[test]
fn v160_fold_immutable_call() {
    let mut eng = engine();
    setup(&mut eng);
    let mut fc = PgConstFold::with_db(&eng.db);
    assert_eq!(
        pg_fold_immutable_call(&mut fc, "f_immutable_int4", &[int(1)]),
        Some(Literal::Int(1))
    );
    // Nested immutable calls fold inside-out.
    assert_eq!(
        pg_fold_immutable_call(
            &mut fc,
            "f_immutable_int4",
            &[ufunc("f_immutable_int4", vec![int(2)])]
        ),
        Some(Literal::Int(2))
    );
    // Non-constant argument fails closed.
    let col = Expr::Column {
        table: None,
        name: "a".to_string(),
    };
    assert_eq!(
        pg_fold_immutable_call(&mut fc, "f_immutable_int4", &[col]),
        None
    );
    // Unknown function fails closed.
    assert_eq!(
        pg_fold_immutable_call(&mut fc, "no_such_fn", &[int(1)]),
        None
    );
}

/// STABLE and VOLATILE functions never fold (PG only folds IMMUTABLE).
#[test]
fn v160_fold_volatile_stable_never() {
    let mut eng = engine();
    setup(&mut eng);
    let mut fc = PgConstFold::with_db(&eng.db);
    assert_eq!(
        pg_fold_immutable_call(&mut fc, "f_stable_int4", &[int(1)]),
        None
    );
    assert_eq!(
        pg_fold_immutable_call(&mut fc, "f_volatile_int4", &[int(1)]),
        None
    );
}

/// STRICT + NULL input folds to NULL without calling the body; a
/// non-strict immutable function evaluates the body with NULL bound.
#[test]
fn v160_fold_null_args() {
    let mut eng = engine();
    setup(&mut eng);
    let mut fc = PgConstFold::with_db(&eng.db);
    assert_eq!(
        pg_fold_immutable_call(&mut fc, "f_strict_imm", &[null()]),
        Some(Literal::Null)
    );
    // Non-strict `return i` with NULL input evaluates to NULL.
    assert_eq!(
        pg_fold_immutable_call(&mut fc, "f_immutable_int4", &[null()]),
        Some(Literal::Null)
    );
}

/// A self-recursive call hits the recursion guard (PG's `active_fns`).
#[test]
fn v160_fold_recursion_guard() {
    let mut eng = engine();
    setup(&mut eng);
    let mut fc = PgConstFold::with_db(&eng.db);
    assert_eq!(pg_fold_immutable_call(&mut fc, "f_recur", &[int(1)]), None);
    // The guard is scoped: an unrelated fold still works after.
    assert_eq!(
        pg_fold_immutable_call(&mut fc, "f_immutable_int4", &[int(1)]),
        Some(Literal::Int(1))
    );
}

/// The corpus target: the pulled-up constant makes the qual const-false
/// → `Result` + `Replaces: Scan on t1` + `One-Time Filter: false`.
#[test]
fn v160_explain_one_time_filter_fnscan() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a FROM t1, f_immutable_int4(1) x WHERE x = 42",
    );
    assert_eq!(
        lines,
        vec!["Result", "Replaces: Scan on t1", "One-Time Filter: false"]
    );
}

/// A const-true qual does not collapse; a stable function never folds.
#[test]
fn v160_explain_no_collapse_cases() {
    let mut eng = engine();
    setup(&mut eng);
    // x = 1 is const-true after pullup: no dummy rel.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a FROM t1, f_immutable_int4(1) x WHERE x = 1",
    );
    assert!(!lines.iter().any(|l| l.contains("One-Time Filter")));
    // STABLE is never pulled up: the qual stays non-constant.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a FROM t1, f_stable_int4(1) x WHERE x = 42",
    );
    assert!(!lines.iter().any(|l| l.contains("One-Time Filter")));
}

/// The pulled-up function is dropped from `Replaces:` even when the
/// const-false qual does not involve it (PG pulls up before checking).
#[test]
fn v160_explain_replaces_drops_folded_fnscan() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a FROM t1, f_immutable_int4(1) x WHERE 1 = 0",
    );
    assert_eq!(
        lines,
        vec!["Result", "Replaces: Scan on t1", "One-Time Filter: false"]
    );
}
