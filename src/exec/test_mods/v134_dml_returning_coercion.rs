/// v1.34: static return-type coercibility at CREATE for DML...RETURNING
/// bodies (PG19 `check_sql_fn_retval` / `coerce_fn_result_column`,
/// executor/functions.c). The final DML...RETURNING column's type must
/// admit an *assignment* cast to the declared return type, else 42P13
/// with PG's verbatim detail `Actual return type is %s.` Anything not
/// statically decidable defers to the call-time coercion (unchanged).
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
    run(eng, "CREATE TABLE t134(a int, b text, t timestamptz);").unwrap();
}

#[test]
fn dml_returning_wrong_type_is_42p13() {
    let mut eng = engine();
    setup(&mut eng);
    // PG19 check_sql_fn_retval: timestamptz has no assignment cast
    // to integer -> 42P13 at CREATE with PG's verbatim detail.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t134(a) VALUES (1) RETURNING now()';",
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
fn dml_returning_value_level_cast_still_creates() {
    let mut eng = engine();
    setup(&mut eng);
    // text -> integer HAS an assignment cast path (IO coercion), so
    // PG accepts this at CREATE; the cast fails at runtime instead.
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t134(a) VALUES (1) RETURNING ''hello''';",
    )
    .expect("text->int cast path exists: CREATE must succeed");
    let e = err_of(&mut eng, "SELECT f();");
    assert_eq!(e.code, "22P02");
}

#[test]
fn dml_returning_compatible_types_create_and_run() {
    let mut eng = engine();
    setup(&mut eng);
    // int -> text: assignment cast exists; runs fine.
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS text LANGUAGE sql \
             AS 'INSERT INTO t134(a) VALUES (1) RETURNING 42';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f();").unwrap()),
        vec![vec!["42"]]
    );
    // identity: int -> int.
    run(
        &mut eng,
        "CREATE FUNCTION g() RETURNS int LANGUAGE sql \
             AS 'UPDATE t134 SET a = 7 RETURNING a';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT g();").unwrap()),
        vec![vec!["7"]]
    );
}

#[test]
fn dml_returning_whole_row_is_42p13() {
    let mut eng = engine();
    setup(&mut eng);
    // A whole-row RETURNING item has the record type; record has no
    // assignment cast to integer.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
             AS 'DELETE FROM t134 RETURNING t134';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(e.detail.as_deref(), Some("Actual return type is record."));
}

#[test]
fn dml_returning_array_is_42p13() {
    let mut eng = engine();
    setup(&mut eng);
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t134(a) VALUES (1) RETURNING ARRAY[1,2]';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.detail.as_deref(),
        Some("Actual return type is integer[].")
    );
    // (RETURNS int[] itself is a separate parser gap — 42601 in
    // sql.rs — so the array->array acceptance is covered by the
    // can_assignment_coerce_predicate unit test below instead.)
}

#[test]
fn update_returning_column_type_checked() {
    let mut eng = engine();
    setup(&mut eng);
    // RETURNING a timestamptz column for RETURNS int: no cast path.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
             AS 'UPDATE t134 SET a = 1 RETURNING t';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.detail.as_deref(),
        Some("Actual return type is timestamp with time zone.")
    );
    // Aliased target resolves the qualifier.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION h() RETURNS int LANGUAGE sql \
             AS 'UPDATE t134 AS x SET a = 1 RETURNING x.t';",
    );
    assert_eq!(e.code, "42P13");
}

#[test]
fn param_ref_takes_declared_arg_type() {
    let mut eng = engine();
    setup(&mut eng);
    // $1 is declared int: int -> int coerces.
    run(
        &mut eng,
        "CREATE FUNCTION f(x int) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t134(a) VALUES ($1) RETURNING $1';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f(41);").unwrap()),
        vec![vec!["41"]]
    );
    // $1 declared text, RETURNS int: cast path exists -> CREATE ok.
    run(
        &mut eng,
        "CREATE FUNCTION g(x text) RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t134(b) VALUES ($1) RETURNING $1';",
    )
    .expect("text->int cast path exists: CREATE must succeed");
}

#[test]
fn engine_cast_superset_stays_create_ok() {
    let mut eng = engine();
    setup(&mut eng);
    // This engine casts boolean -> integer (cast_to_int); the
    // static check must not reject what the runtime accepts.
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t134(a) VALUES (1) RETURNING true';",
    )
    .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT f();").unwrap()),
        vec![vec!["1"]]
    );
}

#[test]
fn setof_keeps_v133_exemption() {
    let mut eng = engine();
    setup(&mut eng);
    // v1.33 deliberately exempts SETOF from the scalar checks; the
    // v1.34 coercion check follows the same boundary.
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS SETOF int LANGUAGE sql \
             AS 'INSERT INTO t134(a) VALUES (1) RETURNING now()';",
    )
    .expect("SETOF keeps the v1.33 exemption");
}

#[test]
fn select_final_untouched_by_v134() {
    let mut eng = engine();
    setup(&mut eng);
    // v1.35 supersedes this pin: SELECT-body finals are now
    // statically checked too (the v1.34 scope deliberately stopped
    // at DML...RETURNING). Kept as the negative-case anchor for the
    // v1.35 check: timestamptz has no assignment cast to integer.
    let e = err_of(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT now()';",
    );
    assert_eq!(e.code, "42P13");
    assert_eq!(
        e.detail.as_deref(),
        Some("Actual return type is timestamp with time zone.")
    );
}

#[test]
fn ambiguous_shapes_defer_to_call_time() {
    let mut eng = engine();
    setup(&mut eng);
    // RETURNING * is ambiguous: no static check (call-time arity
    // check still applies).
    run(
        &mut eng,
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql \
             AS 'INSERT INTO t134(a) VALUES (1) RETURNING *';",
    )
    .expect("ambiguous RETURNING * defers to call time");
    // Unknown table: PG would 42P01 (separate gap); the coercion
    // check fails open and CREATE still succeeds.
    run(
        &mut eng,
        "CREATE FUNCTION g() RETURNS int LANGUAGE sql \
             AS 'INSERT INTO nosuch134 VALUES (1) RETURNING 1';",
    )
    .expect("unknown table fails open");
}

#[test]
fn can_assignment_coerce_predicate() {
    use crate::storage::ColType;
    // Identity and trivial paths.
    assert!(can_assignment_coerce(&ColType::Int, &ColType::Int));
    assert!(can_assignment_coerce(&ColType::Int, &ColType::Text));
    assert!(can_assignment_coerce(&ColType::Text, &ColType::Int));
    assert!(can_assignment_coerce(&ColType::SmallInt, &ColType::BigInt));
    // Typmods never block.
    assert!(can_assignment_coerce(
        &ColType::Numeric(Some((10, 2))),
        &ColType::Numeric(None)
    ));
    // No cast path: timestamptz/record/array -> integer.
    assert!(!can_assignment_coerce(&ColType::Timestamptz, &ColType::Int));
    assert!(!can_assignment_coerce(&ColType::Record, &ColType::Int));
    assert!(!can_assignment_coerce(
        &ColType::Array(crate::storage::ArrayElem::Int),
        &ColType::Int
    ));
    assert!(!can_assignment_coerce(
        &ColType::Int,
        &ColType::Array(crate::storage::ArrayElem::Int)
    ));
    // Element-wise array casts.
    assert!(can_assignment_coerce(
        &ColType::Array(crate::storage::ArrayElem::Int),
        &ColType::Array(crate::storage::ArrayElem::Text)
    ));
    assert!(!can_assignment_coerce(
        &ColType::Array(crate::storage::ArrayElem::Timestamptz),
        &ColType::Array(crate::storage::ArrayElem::Int)
    ));
}
