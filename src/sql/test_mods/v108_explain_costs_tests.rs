
use super::*;

fn explain_costs(sql: &str) -> (bool, bool) {
    match parse_statement(sql).expect("parses") {
        Stmt::Explain { analyze, costs, .. } => (analyze, costs),
        other => panic!("expected Explain, got {:?}", other),
    }
}

#[test]
fn costs_defaults_true() {
    let (_, costs) = explain_costs("EXPLAIN SELECT 1");
    assert!(costs);
    let (_, costs) = explain_costs("EXPLAIN (VERBOSE) SELECT 1");
    assert!(costs);
}

#[test]
fn costs_off_forms() {
    for sql in [
        "EXPLAIN (COSTS OFF) SELECT 1",
        "EXPLAIN (COSTS FALSE) SELECT 1",
        "EXPLAIN (COSTS off) SELECT 1",
        "EXPLAIN (COSTS 0) SELECT 1",
    ] {
        let (_, costs) = explain_costs(sql);
        assert!(!costs, "expected costs=false for {sql}");
    }
}

#[test]
fn costs_on_forms() {
    for sql in [
        "EXPLAIN (COSTS) SELECT 1",
        "EXPLAIN (COSTS ON) SELECT 1",
        "EXPLAIN (COSTS TRUE) SELECT 1",
        "EXPLAIN (COSTS 1) SELECT 1",
    ] {
        let (_, costs) = explain_costs(sql);
        assert!(costs, "expected costs=true for {sql}");
    }
}

#[test]
fn costs_duplicate_last_wins() {
    // v1.08: PG19 processes options sequentially; duplicates are valid,
    // last value wins.
    let (_, costs) = explain_costs("EXPLAIN (COSTS OFF, COSTS ON) SELECT 1");
    assert!(costs);
    let (_, costs) = explain_costs("EXPLAIN (COSTS ON, COSTS OFF) SELECT 1");
    assert!(!costs);
}

#[test]
fn costs_invalid_boolean_errors() {
    // v1.45: PG19 defGetBoolean message ("<name> requires a Boolean
    // value", lowercase name — PG's lexer folds unquoted names).
    let err = parse_statement("EXPLAIN (COSTS foo) SELECT 1").unwrap_err();
    assert_eq!(err.code, "42601");
    assert!(
        err.message.contains("costs requires a Boolean value"),
        "message: {}",
        err.message
    );
}

#[test]
fn unknown_option_errors_42601() {
    let err = parse_statement("EXPLAIN (FOOBAR) SELECT 1").unwrap_err();
    assert_eq!(err.code, "42601");
    assert!(
        err.message.contains("unrecognized EXPLAIN option"),
        "message: {}",
        err.message
    );
}

#[test]
fn known_inert_options_accepted() {
    // v1.08: known non-COSTS options remain accepted and inert.
    for sql in [
        "EXPLAIN (VERBOSE, COSTS OFF) SELECT 1",
        "EXPLAIN (BUFFERS, COSTS OFF) SELECT 1",
        "EXPLAIN (TIMING OFF, COSTS OFF) SELECT 1",
    ] {
        let (_, costs) = explain_costs(sql);
        assert!(!costs, "expected costs=false for {sql}");
    }
}
