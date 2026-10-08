use super::*;

fn create_fn(sql: &str) -> Stmt {
    parse_statement(sql).expect("parses")
}

#[test]
fn parallel_safe_parses() {
    // v1.09: PARALLEL SAFE is accepted (planner hint, like COST).
    match create_fn(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql IMMUTABLE PARALLEL SAFE AS 'SELECT $1';",
    ) {
        Stmt::CreateFunction { .. } => {}
        other => panic!("expected CreateFunction, got {:?}", other),
    }
}

#[test]
fn parallel_restricted_parses() {
    match create_fn(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql PARALLEL RESTRICTED AS 'SELECT $1';",
    ) {
        Stmt::CreateFunction { .. } => {}
        other => panic!("expected CreateFunction, got {:?}", other),
    }
}

#[test]
fn parallel_unsafe_parses() {
    match create_fn(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql PARALLEL UNSAFE AS 'SELECT $1';",
    ) {
        Stmt::CreateFunction { .. } => {}
        other => panic!("expected CreateFunction, got {:?}", other),
    }
}

#[test]
fn parallel_bogus_rejected() {
    // v1.09: invalid PARALLEL value is a syntax error.
    let err = parse_statement(
        "CREATE FUNCTION f(int) RETURNS int LANGUAGE sql PARALLEL BOGUS AS 'SELECT $1';",
    )
    .expect_err("should fail");
    assert!(err.message.contains("PARALLEL"), "message: {}", err.message);
}

#[test]
fn alter_function_immutable_parses() {
    // v1.09: ALTER FUNCTION ... IMMUTABLE.
    match create_fn("ALTER FUNCTION f(int) IMMUTABLE;") {
        Stmt::AlterFunction {
            name,
            arg_types,
            volatility,
        } => {
            assert_eq!(name, "f");
            assert_eq!(arg_types, vec!["int".to_string()]);
            assert_eq!(volatility, FuncVolatility::Immutable);
        }
        other => panic!("expected AlterFunction, got {:?}", other),
    }
}

#[test]
fn alter_function_stable_parses() {
    match create_fn("ALTER FUNCTION f(text, int) STABLE;") {
        Stmt::AlterFunction {
            name,
            arg_types,
            volatility,
        } => {
            assert_eq!(name, "f");
            assert_eq!(arg_types.len(), 2);
            assert_eq!(volatility, FuncVolatility::Stable);
        }
        other => panic!("expected AlterFunction, got {:?}", other),
    }
}

#[test]
fn alter_function_volatile_parses() {
    match create_fn("ALTER FUNCTION f() VOLATILE;") {
        Stmt::AlterFunction { volatility, .. } => {
            assert_eq!(volatility, FuncVolatility::Volatile);
        }
        other => panic!("expected AlterFunction, got {:?}", other),
    }
}

#[test]
fn alter_function_other_action_rejected() {
    // v1.09: only volatility actions are supported; STRICT etc. are 0A000.
    let err = parse_statement("ALTER FUNCTION f(int) STRICT;").expect_err("should fail");
    assert_eq!(err.code, "0A000", "message: {}", err.message);
}
