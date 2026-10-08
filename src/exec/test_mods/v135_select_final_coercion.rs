/// v1.35: static return-type coercibility at CREATE for SELECT-body
/// finals (PG19 `check_sql_fn_retval` / `coerce_fn_result_column`,
/// executor/functions.c) — the remaining quadrant after v1.33
/// (SELECT-body arity) and v1.34 (DML...RETURNING coercibility). The
/// final SELECT's single output column must admit an *assignment* cast
/// to the declared return type, else 42P13 with PG's verbatim detail
/// `Actual return type is %s.` Anything not statically decidable fails
/// open to the existing call-time coercion (unchanged).
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = crate::sql::parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
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
    let r = execute(eng, &mut ctx, &stmt);
    if r.is_err() {
        for op in writes.iter().rev() {
            crate::storage::undo_write_op(eng, &[9], op);
        }
    }
    r
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

fn err_of(eng: &mut Engine, sql: &str) -> ExecError {
    run(eng, sql).unwrap_err()
}

fn setup(eng: &mut Engine) {
    run(eng, "CREATE TABLE t135(a int, b text, t timestamptz);").unwrap();
}

#[test]
fn select_final_wrong_type_is_42p13() {
    let mut eng = engine();
    setup(&mut eng);
    // PG19 check_sql_fn_retval: timestamptz has no assignment cast
    // to integer -> 42P13 at CREATE with PG's verbatim detail.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT now()';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.message,
        "return type mismatch in function declared to return int"
    );
    assert_eq!(
        e.detail.as_deref(),
        Some("Actual return type is timestamp with time zone.")
    );
}

#[test]
fn select_final_array_mismatch_names_element_type() {
    let mut eng = engine();
    setup(&mut eng);
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT ARRAY[1,2]';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.detail.as_deref(),
        Some("Actual return type is integer[].")
    );
}

#[test]
fn select_final_value_level_cast_still_creates() {
    let mut eng = engine();
    setup(&mut eng);
    // text -> integer HAS an assignment cast path, so PG accepts
    // this at CREATE; the cast fails at runtime instead.
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT ''hello''';",
    )
    .expect("text->int cast path exists: CREATE must succeed");
    let e = err_of(&mut eng, "SELECT f();");
    assert_eq!(e.code, "22P02");
}

#[test]
fn select_final_compatible_types_create_and_run() {
    let mut eng = engine();
    setup(&mut eng);
    // int -> text: assignment cast exists; runs fine.
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS text LANGUAGE sql AS 'SELECT 42';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f();").unwrap()),
        vec![vec!["42"]]
    );
}

#[test]
fn select_final_checks_only_the_last_statement() {
    let mut eng = engine();
    setup(&mut eng);
    // The first statement's type is irrelevant; the final SELECT
    // decides (PG19 checks only the last query's tlist).
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
             AS 'SELECT 1; SELECT now();';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.detail.as_deref(),
        Some("Actual return type is timestamp with time zone.")
    );
    run(
        &mut eng,
        "CREATE FUNCTION g() RETURNS int LANGUAGE sql \
             AS 'SELECT now(); SELECT 7;';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT g();").unwrap()),
        vec![vec!["7"]]
    );
}

#[test]
fn select_final_param_typed_by_declared_arg() {
    let mut eng = engine();
    setup(&mut eng);
    // Named refs are rewritten to $n; the declared argument type
    // drives the check (int -> text coerces, so this creates).
    run(
        &mut eng,
        "CREATE FUNCTION f(a int) RETURNS text LANGUAGE sql AS 'SELECT a';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f(42);").unwrap()),
        vec![vec!["42"]]
    );
    // timestamptz -> int has no assignment cast -> 42P13 naming the
    // argument's type.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION h(a timestamptz) RETURNS int LANGUAGE sql AS 'SELECT a';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.detail.as_deref(),
        Some("Actual return type is timestamp with time zone.")
    );
}

#[test]
fn select_final_from_table_column() {
    let mut eng = engine();
    setup(&mut eng);
    run(&mut eng, "INSERT INTO t135(a, b) VALUES (1, 'x');").unwrap();
    // Column types resolve through the FROM range: timestamptz
    // cannot coerce to int.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT t FROM t135';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.detail.as_deref(),
        Some("Actual return type is timestamp with time zone.")
    );
    run(
        &mut eng,
        "CREATE FUNCTION g() RETURNS timestamptz LANGUAGE sql \
             AS 'SELECT t FROM t135 WHERE a = 1';",
    )
    .unwrap();
}

#[test]
fn select_final_fail_open_shapes_still_create() {
    let mut eng = engine();
    setup(&mut eng);
    // Composite declared return type: the existing call-time
    // `eval_cast_named` path applies (v1.34's fail-open rule).
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS t135 LANGUAGE sql AS 'SELECT a FROM t135';",
    )
    .unwrap();
    // `*` shapes defer to the call-time arity/coercion check.
    run(
        &mut eng,
        "CREATE FUNCTION g() RETURNS int LANGUAGE sql AS 'SELECT * FROM t135';",
    )
    .unwrap();
    // Set-operation finals defer (v1.33 arity conservatism).
    run(
        &mut eng,
        "CREATE FUNCTION h() RETURNS int LANGUAGE sql \
             AS 'SELECT 1 UNION SELECT now()';",
    )
    .unwrap();
    // WITH queries defer (no CTE schema bindings built).
    run(
        &mut eng,
        "CREATE FUNCTION k() RETURNS int LANGUAGE sql \
             AS 'WITH x AS (SELECT now() AS t) SELECT t FROM x';",
    )
    .unwrap();
    // Field access on a composite-typed argument cannot be typed
    // statically (mirrors the corpus f_field_select case).
    run(
        &mut eng,
        "CREATE FUNCTION m(t t135) RETURNS int LANGUAGE sql AS 'SELECT t.a';",
    )
    .unwrap();
    // Out-of-range $n in the output: defer (signature validation
    // already guarantees resolvable declared types, so only the
    // index can fail here).
    run(
        &mut eng,
        "CREATE FUNCTION n(a int) RETURNS int LANGUAGE sql AS 'SELECT $5';",
    )
    .unwrap();
}

#[test]
fn select_final_setof_keeps_exemption() {
    let mut eng = engine();
    setup(&mut eng);
    // v1.33 pinned: SETOF finals skip the scalar checks entirely.
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS SETOF int LANGUAGE sql AS 'SELECT now()';",
    )
    .unwrap();
}
