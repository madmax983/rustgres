/// v1.38: shell types + `cstring` in internal-function signatures, the
/// `int4in/int4out/int8in/int8out` internal I/O symbols, and
/// `CREATE CAST ... WITHOUT FUNCTION` binary casts (PG19 `CreateCast`).
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

fn err_code(eng: &mut Engine, sql: &str) -> &'static str {
    run(eng, sql).unwrap_err().code
}

/// v1.38 (Root 1): a shell type is legal in a function signature
/// (PG19 allows it — that is the shell's purpose), and `cstring`
/// (PG19's C-string pseudo-type) is accepted too.
#[test]
fn shell_and_cstring_accepted_in_signature() {
    let mut eng = engine();
    run(&mut eng, "CREATE TYPE s138a").unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION s138a_in(cstring) RETURNS s138a IMMUTABLE STRICT \
             LANGUAGE internal AS 'int4in'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION s138a_out(s138a) RETURNS cstring IMMUTABLE STRICT \
             LANGUAGE internal AS 'int4out'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TYPE s138a (INPUT = s138a_in, OUTPUT = s138a_out, LIKE = integer)",
    )
    .unwrap();
}

/// v1.38: `int4in`/`int8in` parse like PG19's `pg_strtoint32_safe` /
/// `pg_strtoint64` — whitespace, sign, 0x/0o/0b prefixes, PG-style
/// 22003/22P02 errors, and STRICT NULL handling.
#[test]
fn int_in_semantics() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION i4in138(cstring) RETURNS integer IMMUTABLE STRICT \
             LANGUAGE internal AS 'int4in'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION i8in138(cstring) RETURNS bigint IMMUTABLE STRICT \
             LANGUAGE internal AS 'int8in'",
    )
    .unwrap();
    let r = rows_of(run(&mut eng, "SELECT i4in138('  +42  ')").unwrap());
    assert_eq!(col0(r), vec!["42".to_string()]);
    let r = rows_of(run(&mut eng, "SELECT i4in138('0x10')").unwrap());
    assert_eq!(col0(r), vec!["16".to_string()]);
    let r = rows_of(run(&mut eng, "SELECT i4in138('-2147483648')").unwrap());
    assert_eq!(col0(r), vec!["-2147483648".to_string()]);
    let r = rows_of(run(&mut eng, "SELECT i8in138('9223372036854775807')").unwrap());
    assert_eq!(col0(r), vec!["9223372036854775807".to_string()]);
    // NULL in (STRICT) -> NULL out, no error.
    let r = rows_of(run(&mut eng, "SELECT i4in138(NULL)").unwrap());
    assert_eq!(col0(r), vec!["NULL".to_string()]);
    // PG19 error codes.
    assert_eq!(err_code(&mut eng, "SELECT i4in138('2147483648')"), "22003");
    assert_eq!(
        err_code(&mut eng, "SELECT i8in138('-9223372036854775809')"),
        "22003"
    );
    assert_eq!(err_code(&mut eng, "SELECT i4in138('abc')"), "22P02");
    assert_eq!(err_code(&mut eng, "SELECT i4in138('12.5')"), "22P02");
}

/// v1.38: `int4out`/`int8out` render decimal text (PG19 `pg_ltoa`).
#[test]
fn int_out_semantics() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION i4out138(integer) RETURNS cstring IMMUTABLE STRICT \
             LANGUAGE internal AS 'int4out'",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE FUNCTION i8out138(bigint) RETURNS cstring IMMUTABLE STRICT \
             LANGUAGE internal AS 'int8out'",
    )
    .unwrap();
    let r = rows_of(run(&mut eng, "SELECT i4out138(-2147483647)").unwrap());
    assert_eq!(col0(r), vec!["-2147483647".to_string()]);
    let r = rows_of(run(&mut eng, "SELECT i8out138(0::bigint)").unwrap());
    assert_eq!(col0(r), vec!["0".to_string()]);
    // Wrong argument type -> 42804 (PG19 would 42883 at lookup; the
    // function resolved by arity here, so the mismatch surfaces).
    assert_eq!(err_code(&mut eng, "SELECT i4out138('42')"), "42804");
    // Wrong arity -> 42883 from overload resolution (the function's
    // 1-arg overload doesn't match 2 args).
    assert_eq!(err_code(&mut eng, "SELECT i4out138(1, 2)"), "42883");
}

/// v1.38: an unknown internal symbol is rejected at CREATE with
/// 0A000 (fail fast, like the v1.37 `int4eq`-only allowlist did).
#[test]
fn unknown_internal_symbol_is_0a000() {
    let mut eng = engine();
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION nosym138(integer) RETURNS integer IMMUTABLE STRICT \
                 LANGUAGE internal AS 'no_such_symbol'"
        ),
        "0A000"
    );
}

/// v1.38 (Root 3): `CREATE CAST ... WITHOUT FUNCTION` — the PG19
/// `CreateCast` validation matrix.
#[test]
fn create_cast_validation_matrix() {
    let mut eng = engine();
    run(&mut eng, "CREATE TYPE c138").unwrap();
    // Unknown source type -> 42704.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE CAST (nosuch138 AS integer) WITHOUT FUNCTION"
        ),
        "42704"
    );
    // Unknown target type -> 42704.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE CAST (integer AS nosuch138) WITHOUT FUNCTION"
        ),
        "42704"
    );
    // Same type -> 42P17.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE CAST (integer AS integer) WITHOUT FUNCTION"
        ),
        "42P17"
    );
    // Physically incompatible (different typlen) -> 42P17.
    assert_eq!(
        err_code(&mut eng, "CREATE CAST (integer AS bigint) WITHOUT FUNCTION"),
        "42P17"
    );
    assert_eq!(
        err_code(&mut eng, "CREATE CAST (integer AS text) WITHOUT FUNCTION"),
        "42P17"
    );
    // Shell (pseudo) type -> 42809.
    assert_eq!(
        err_code(&mut eng, "CREATE CAST (c138 AS integer) WITHOUT FUNCTION"),
        "42809"
    );
    // The float4.sql target statements succeed.
    run(&mut eng, "CREATE TYPE c138 (LIKE = float4)").unwrap();
    run(&mut eng, "CREATE CAST (c138 AS float4) WITHOUT FUNCTION").unwrap();
    run(&mut eng, "CREATE CAST (float4 AS c138) WITHOUT FUNCTION").unwrap();
    run(&mut eng, "CREATE CAST (c138 AS integer) WITHOUT FUNCTION").unwrap();
    run(&mut eng, "CREATE CAST (integer AS c138) WITHOUT FUNCTION").unwrap();
    // Duplicate -> 42710.
    assert_eq!(
        err_code(&mut eng, "CREATE CAST (integer AS c138) WITHOUT FUNCTION"),
        "42710"
    );
    // WITH FUNCTION is out of scope -> 0A000, like PG19's
    // "not yet implemented".
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE CAST (integer AS bigint) WITH FUNCTION int4larger"
        ),
        "0A000"
    );
}

/// v1.38: binary casts reinterpret bits (PG19 CoerceViaIO-free
/// binary coercion): int32 1 -> f32 1.401298464324817e-45.
#[test]
fn binary_cast_reinterprets_bits() {
    let mut eng = engine();
    run(&mut eng, "CREATE TYPE b138 (LIKE = float4)").unwrap();
    run(&mut eng, "CREATE CAST (integer AS b138) WITHOUT FUNCTION").unwrap();
    run(&mut eng, "CREATE CAST (b138 AS float4) WITHOUT FUNCTION").unwrap();
    let r = rows_of(run(&mut eng, "SELECT 1::b138::float4").unwrap());
    assert_eq!(col0(r), vec!["1.401298464324817e-45".to_string()]);
    // int8 <-> float8 identity of width.
    run(&mut eng, "CREATE TYPE b8138 (LIKE = float8)").unwrap();
    run(&mut eng, "CREATE CAST (bigint AS b8138) WITHOUT FUNCTION").unwrap();
    run(&mut eng, "CREATE CAST (b8138 AS float8) WITHOUT FUNCTION").unwrap();
    let r = rows_of(run(&mut eng, "SELECT 4613937818241073152::b8138::float8").unwrap());
    assert_eq!(col0(r), vec!["3".to_string()]);
    // Without a registered cast, a LIKE target is not a composite.
    assert_eq!(err_code(&mut eng, "SELECT 1::b8138"), "42846");
}

/// v1.38: rolling back a CREATE CAST removes it (`WriteOp::CreateCast`
/// undo). The unit `run()` helper auto-commits, so the undo log entry
/// is driven directly through the public `undo_write_op`.
#[test]
fn create_cast_rolls_back() {
    let mut eng = engine();
    run(&mut eng, "CREATE TYPE r138 (LIKE = float4)").unwrap();
    run(&mut eng, "CREATE CAST (integer AS r138) WITHOUT FUNCTION").unwrap();
    // The cast is visible: a duplicate is rejected...
    assert_eq!(
        err_code(&mut eng, "CREATE CAST (integer AS r138) WITHOUT FUNCTION"),
        "42710"
    );
    // ...and after undoing the WriteOp it can be created again.
    crate::storage::undo_write_op(
        &mut eng,
        &[9],
        &crate::storage::WriteOp::CreateCast {
            src: "integer".to_string(),
            dst: "r138".to_string(),
            prev: None,
        },
    );
    run(&mut eng, "CREATE CAST (integer AS r138) WITHOUT FUNCTION").unwrap();
}
