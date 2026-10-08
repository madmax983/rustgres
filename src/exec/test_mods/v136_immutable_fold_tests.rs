
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

/// v1.36: a pure IMMUTABLE SQL body with constant arguments folds:
/// one body execution serves every row of the statement.
#[test]
fn immutable_pure_body_folds_across_rows() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION add1(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT $1 + 1'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT add1(41) FROM generate_series(1, 1000) g").unwrap());
    assert_eq!(rows.len(), 1000);
    assert!(rows.iter().all(|r| r == &vec!["42".to_string()]));
}

/// v1.36: the cache is keyed by argument value — repeated arguments
/// hit, distinct ones miss, and every row is still correct.
#[test]
fn cache_key_distinguishes_arg_values() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION add1(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT $1 + 1'",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT add1(v) FROM (VALUES (1),(2),(1),(3),(2)) AS t(v) ORDER BY v",
        )
        .unwrap(),
    );
    assert_eq!(
        col0(rows),
        vec![
            "2".to_string(),
            "2".to_string(),
            "3".to_string(),
            "3".to_string(),
            "4".to_string()
        ]
    );
}

/// v1.36: row-varying arguments never hit (each row evaluates the
/// body, exactly as before the fold existed).
#[test]
fn row_varying_arg_evaluates_per_row() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t136(a int)").unwrap();
    run(&mut eng, "INSERT INTO t136 VALUES (1),(2),(3)").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION add1(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT $1 + 1'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT add1(a) FROM t136 ORDER BY a").unwrap());
    assert_eq!(
        col0(rows),
        vec!["2".to_string(), "3".to_string(), "4".to_string()]
    );
}

/// v1.36: VOLATILE-marked functions never fold (PG19
/// `evaluate_function` folds IMMUTABLE only). A fold would repeat
/// the first sequence value on every row.
#[test]
fn volatile_marked_function_never_folds() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s136v").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION sv136() RETURNS int VOLATILE LANGUAGE sql \
             AS 'SELECT nextval(''s136v'')'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT sv136() FROM generate_series(1, 5) g").unwrap());
    assert_eq!(
        col0(rows),
        vec![
            "1".to_string(),
            "2".to_string(),
            "3".to_string(),
            "4".to_string(),
            "5".to_string()
        ]
    );
}

/// v1.36: STABLE-marked functions never fold either (PG folds
/// STABLE only in estimation mode, never in normal planning).
#[test]
fn stable_marked_function_never_folds() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s136s").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION ss136() RETURNS int STABLE LANGUAGE sql \
             AS 'SELECT nextval(''s136s'')'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT ss136() FROM generate_series(1, 3) g").unwrap());
    assert_eq!(
        col0(rows),
        vec!["1".to_string(), "2".to_string(), "3".to_string()]
    );
}

/// v1.36: a body calling a volatile builtin never folds, even when
/// the function is (falsely) marked IMMUTABLE — PG performs no
/// such check at CREATE, so the fold must fail open.
#[test]
fn volatile_builtin_in_body_never_folds() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION rv136() RETURNS float8 IMMUTABLE LANGUAGE sql \
             AS 'SELECT random()'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT rv136() FROM generate_series(1, 5) g").unwrap());
    let distinct: HashSet<String> = col0(rows).into_iter().collect();
    assert!(
        distinct.len() > 1,
        "random() body must not fold to a single value"
    );
}

/// v1.36: a VOLATILE callee anywhere in the body blocks the fold
/// (transitive check).
#[test]
fn volatile_callee_blocks_fold() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s136c").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION vc136() RETURNS int VOLATILE LANGUAGE sql \
             AS 'SELECT nextval(''s136c'')'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION ic136() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT vc136()'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT ic136() FROM generate_series(1, 3) g").unwrap());
    assert_eq!(
        col0(rows),
        vec!["1".to_string(), "2".to_string(), "3".to_string()]
    );
}

/// v1.36: a STABLE callee is fine (statement-constant by contract)
/// and the fold still applies through it.
#[test]
fn stable_callee_folds_through() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION st136(x int) RETURNS int STABLE LANGUAGE sql \
             AS 'SELECT $1 * 2'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION im136(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT st136($1) + 1'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT im136(10), im136(10)").unwrap());
    assert_eq!(rows, vec![vec!["21".to_string(), "21".to_string()]]);
}

/// v1.36: DML bodies never fold — their write-log effects are
/// row-count-dependent. A fold would run the INSERT once.
#[test]
fn dml_body_never_folds() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE log136(n int)").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION d136() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'INSERT INTO log136 VALUES (1) RETURNING 1'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT d136() FROM generate_series(1, 3) g").unwrap());
    assert_eq!(rows.len(), 3);
    let cnt = rows_of(run(&mut eng, "SELECT count(*) FROM log136").unwrap());
    assert_eq!(cnt, vec![vec!["3".to_string()]]);
}

/// v1.36: a body referencing the caller's query level never folds.
/// `a` binds to the calling query's `t136` (rustgres lets SQL
/// bodies see caller scopes; PG19 rejects such bodies at CREATE).
#[test]
fn outer_ref_body_never_folds() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t136(a int)").unwrap();
    run(&mut eng, "INSERT INTO t136 VALUES (1),(2),(3)").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION o136() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT a + 100'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT o136() FROM t136 ORDER BY a").unwrap());
    assert_eq!(
        col0(rows),
        vec!["101".to_string(), "102".to_string(), "103".to_string()]
    );
}

/// v1.36: the recursion guard refuses the fold but execution still
/// recurses normally (the base case terminates it).
#[test]
fn recursive_body_not_folded_but_runs() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION rec136(x int) RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT CASE WHEN $1 <= 0 THEN 0 ELSE rec136($1 - 1) + 1 END'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT rec136(3)").unwrap());
    assert_eq!(rows, vec![vec!["3".to_string()]]);
}

/// v1.36: STRICT NULL short-circuit composes with the cache — the
/// NULL result is cached and replayed, and the body never runs.
#[test]
fn strict_null_result_caches() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION sn136(x int) RETURNS int IMMUTABLE STRICT LANGUAGE sql \
             AS 'SELECT $1 / 0'",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT sn136(NULL), sn136(NULL)").unwrap());
    assert_eq!(rows, vec![vec!["NULL".to_string(), "NULL".to_string()]]);
}

/// v1.36: errors are never cached — every call raises identically.
#[test]
fn error_never_cached() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION e136() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT 1/0'",
    )
    .unwrap();
    let e1 = run(&mut eng, "SELECT e136()").unwrap_err();
    assert_eq!(e1.code, "22012");
    let e2 = run(&mut eng, "SELECT e136() FROM generate_series(1, 3) g").unwrap_err();
    assert_eq!(e2.code, "22012");
}

/// v1.36: the cache key is exact — floats hash by bit pattern, so
/// `-0.0` and `0.0` (which `PartialEq` and text rendering
/// conflate) never alias; neither do `int` vs `bigint` or
/// `text` vs `bpchar`.
#[test]
fn fn_key_val_is_exact() {
    assert_ne!(
        fn_key_val(&Value::Float(0.0)),
        fn_key_val(&Value::Float(-0.0))
    );
    assert_eq!(
        fn_key_val(&Value::Float(0.0)),
        fn_key_val(&Value::Float(0.0))
    );
    assert_ne!(
        fn_key_val(&Value::Float(f64::NAN)),
        fn_key_val(&Value::Float(-f64::NAN))
    );
    assert_ne!(
        fn_key_val(&Value::Float4(0.0)),
        fn_key_val(&Value::Float4(-0.0))
    );
    assert_ne!(fn_key_val(&Value::Int(1)), fn_key_val(&Value::BigInt(1)));
    assert_ne!(
        fn_key_val(&Value::Text("a".into())),
        fn_key_val(&Value::BpChar("a".into()))
    );
    assert_eq!(fn_key_val(&Value::Null), fn_key_val(&Value::Null));
    assert_ne!(fn_key_val(&Value::Null), fn_key_val(&Value::Int(0)));
}

/// v1.36: user-defined operators resolve their procedure at runtime
/// from the operand types, so a VOLATILE procedure is invisible to
/// static inspection — a body using one must never fold. A volatile
/// `?=%` (via a nextval sequence) yields a distinct value per row;
/// folding would repeat one value across all rows.
#[test]
fn userop_volatile_procedure_never_folds() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s136op").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION bump136(x int, y int) RETURNS int VOLATILE LANGUAGE sql \
             AS 'SELECT x + y + nextval(''s136op'')'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE OPERATOR ?=% (procedure = bump136, leftarg = int, rightarg = int)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION fop136() RETURNS int IMMUTABLE LANGUAGE sql \
             AS 'SELECT 1 ?=% 2'",
    )
    .unwrap();
    let vals = col0(rows_of(
        run(&mut eng, "SELECT fop136() FROM generate_series(1, 3) g").unwrap(),
    ));
    assert_eq!(vals.len(), 3);
    assert_ne!(vals[0], vals[1]);
    assert_ne!(vals[1], vals[2]);
}

/// v1.36: `x op ANY (subquery)` with a user-defined operator must
/// never fold for the same reason as plain `UserOp`. The procedure
/// returns boolean (quantified operators require it) with its
/// volatility observable through `nextval` parity.
#[test]
fn quantified_userop_never_folds() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s136q").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION qbump136(x int, y int) RETURNS bool VOLATILE LANGUAGE sql \
             AS 'SELECT (x + y + nextval(''s136q'')) % 2 = 0'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE OPERATOR ?=%% (procedure = qbump136, leftarg = int, rightarg = int)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION fq136() RETURNS bool IMMUTABLE LANGUAGE sql \
             AS 'SELECT 1 ?=%% ANY (SELECT 2)'",
    )
    .unwrap();
    let vals = col0(rows_of(
        run(&mut eng, "SELECT fq136() FROM generate_series(1, 3) g").unwrap(),
    ));
    assert_eq!(vals.len(), 3);
    assert_ne!(vals[0], vals[1]);
    assert_ne!(vals[1], vals[2]);
}

/// v1.36: the array form `x op ANY (array)` desugars to the hidden
/// `__any_all_array` builtin with a `user:`-prefixed operator text;
/// a volatile user procedure must block folding there too. A
/// single-element array keeps exactly one `nextval` per outer row
/// so the parity alternates.
#[test]
fn any_all_array_userop_never_folds() {
    let mut eng = engine();
    run(&mut eng, "CREATE SEQUENCE s136a").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION abump136(x int, y int) RETURNS bool VOLATILE LANGUAGE sql \
             AS 'SELECT (x + y + nextval(''s136a'')) % 2 = 0'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE OPERATOR ?#%% (procedure = abump136, leftarg = int, rightarg = int)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION fa136() RETURNS bool IMMUTABLE LANGUAGE sql \
             AS 'SELECT 1 ?#%% ANY (ARRAY[1])'",
    )
    .unwrap();
    let vals = col0(rows_of(
        run(&mut eng, "SELECT fa136() FROM generate_series(1, 3) g").unwrap(),
    ));
    assert_eq!(vals.len(), 3);
    assert_ne!(vals[0], vals[1]);
    assert_ne!(vals[1], vals[2]);
}
