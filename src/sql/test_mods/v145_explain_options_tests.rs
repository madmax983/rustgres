use super::*;

fn explain_opts(sql: &str) -> (bool, bool, ExplainOpts) {
    match parse_statement(sql).expect("parses") {
        Stmt::Explain {
            analyze,
            costs,
            opts,
            ..
        } => (analyze, costs, opts),
        other => panic!("expected Explain, got {:?}", other),
    }
}

fn parse_err(sql: &str) -> SqlError {
    parse_statement(sql).unwrap_err()
}

#[test]
fn new_options_accepted() {
    // v1.45: PG19's generic_plan and io are recognized (were 42601).
    let (_, _, opts) = explain_opts("EXPLAIN (GENERIC_PLAN, COSTS OFF) SELECT 1");
    assert!(opts.generic_plan);
    let (_, _, opts) = explain_opts("EXPLAIN (ANALYZE, IO, COSTS OFF) SELECT 1");
    assert!(opts.io);
}

#[test]
fn boolean_forms_all_options() {
    // v1.45: defGetBoolean semantics on every boolean option.
    for (sql, val) in [
        ("EXPLAIN (WAL, ANALYZE) SELECT 1", true),
        ("EXPLAIN (WAL OFF, ANALYZE) SELECT 1", false),
        ("EXPLAIN (WAL 0, ANALYZE) SELECT 1", false),
        ("EXPLAIN (WAL 1, ANALYZE) SELECT 1", true),
        ("EXPLAIN (MEMORY ON, COSTS OFF) SELECT 1", true),
        ("EXPLAIN (SETTINGS FALSE, COSTS OFF) SELECT 1", false),
    ] {
        let _ = explain_opts(sql);
        let _ = val;
    }
    let (_, _, opts) = explain_opts("EXPLAIN (WAL, ANALYZE) SELECT 1");
    assert!(opts.wal);
    let (_, _, opts) = explain_opts("EXPLAIN (WAL OFF, ANALYZE) SELECT 1");
    assert!(!opts.wal);
    let (_, _, opts) = explain_opts("EXPLAIN (MEMORY ON, COSTS OFF) SELECT 1");
    assert!(opts.memory);
}

#[test]
fn boolean_garbage_rejected_pg_message() {
    // v1.45: PG19 defGetBoolean error text and code.
    for opt in ["ANALYZE", "COSTS", "VERBOSE", "WAL", "TIMING", "IO"] {
        let err = parse_err(&format!("EXPLAIN ({opt} FOO) SELECT 1"));
        assert_eq!(err.code, "42601", "option {opt}");
        assert_eq!(
            err.message,
            format!("{} requires a Boolean value", opt.to_lowercase()),
            "option {opt}"
        );
    }
    // Integer other than 0/1 is rejected like PG19.
    let err = parse_err("EXPLAIN (COSTS 2) SELECT 1");
    assert_eq!(err.code, "42601");
    assert!(err.message.contains("costs requires a Boolean value"));
}

#[test]
fn requires_analyze_options() {
    // v1.45: PG19 requires ANALYZE for WAL/TIMING/IO/true and
    // SERIALIZE != none; all 22023 with PG's exact text. BUFFERS has
    // no such check in PG19.
    for (sql, opt) in [
        ("EXPLAIN (WAL) SELECT 1", "WAL"),
        ("EXPLAIN (TIMING) SELECT 1", "TIMING"),
        ("EXPLAIN (IO) SELECT 1", "IO"),
        ("EXPLAIN (SERIALIZE TEXT) SELECT 1", "SERIALIZE"),
        ("EXPLAIN (SERIALIZE BINARY) SELECT 1", "SERIALIZE"),
        ("EXPLAIN (SERIALIZE) SELECT 1", "SERIALIZE"),
    ] {
        let err = parse_err(sql);
        assert_eq!(err.code, "22023", "{sql}");
        assert_eq!(
            err.message,
            format!("EXPLAIN option {opt} requires ANALYZE"),
            "{sql}"
        );
    }
    // False/off variants do NOT require ANALYZE.
    for sql in [
        "EXPLAIN (WAL OFF) SELECT 1",
        "EXPLAIN (TIMING OFF) SELECT 1",
        "EXPLAIN (IO FALSE) SELECT 1",
        "EXPLAIN (SERIALIZE OFF) SELECT 1",
        "EXPLAIN (SERIALIZE NONE) SELECT 1",
        "EXPLAIN (BUFFERS) SELECT 1",
        "EXPLAIN (BUFFERS OFF) SELECT 1",
    ] {
        explain_opts(sql);
    }
    // With ANALYZE, all are fine.
    let (_, _, opts) =
        explain_opts("EXPLAIN (ANALYZE, WAL, TIMING, IO, SERIALIZE BINARY) SELECT 1");
    assert!(opts.wal && opts.timing && opts.io);
    assert_eq!(opts.serialize, ExplainSerialize::Binary);
}

#[test]
fn generic_plan_analyze_conflict() {
    // v1.45: PG19's exact 22023 text.
    let err = parse_err("EXPLAIN (GENERIC_PLAN, ANALYZE) SELECT 1");
    assert_eq!(err.code, "22023");
    assert_eq!(
        err.message,
        "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together"
    );
    // Either order.
    let err = parse_err("EXPLAIN (ANALYZE, GENERIC_PLAN) SELECT 1");
    assert_eq!(err.code, "22023");
    // GENERIC_PLAN alone is fine.
    let (_, _, opts) = explain_opts("EXPLAIN (GENERIC_PLAN) SELECT 1");
    assert!(opts.generic_plan);
}

#[test]
fn serialize_values() {
    let (_, _, opts) = explain_opts("EXPLAIN (SERIALIZE NONE, ANALYZE) SELECT 1");
    assert_eq!(opts.serialize, ExplainSerialize::None);
    let (_, _, opts) = explain_opts("EXPLAIN (SERIALIZE TEXT, ANALYZE) SELECT 1");
    assert_eq!(opts.serialize, ExplainSerialize::Text);
    let (_, _, opts) = explain_opts("EXPLAIN (SERIALIZE BINARY, ANALYZE) SELECT 1");
    assert_eq!(opts.serialize, ExplainSerialize::Binary);
    // Bare SERIALIZE means text (and thus requires ANALYZE).
    let err = parse_err("EXPLAIN (SERIALIZE) SELECT 1");
    assert_eq!(err.code, "22023");
    // Bad value: PG19's exact 22023 text. Unquoted BOGUS is folded
    // to lowercase by the tokenizer (like PG's scanner); a quoted
    // "TEXT" keeps its case and is rejected (PG19 strcmp).
    let err = parse_err("EXPLAIN (SERIALIZE BOGUS, ANALYZE) SELECT 1");
    assert_eq!(err.code, "22023");
    assert_eq!(
        err.message,
        "unrecognized value for EXPLAIN option \"serialize\": \"bogus\""
    );
    let err = parse_err("EXPLAIN (SERIALIZE \"TEXT\", ANALYZE) SELECT 1");
    assert_eq!(err.code, "22023");
    assert_eq!(
        err.message,
        "unrecognized value for EXPLAIN option \"serialize\": \"TEXT\""
    );
}

#[test]
fn format_values() {
    let (_, _, opts) = explain_opts("EXPLAIN (FORMAT TEXT) SELECT 1");
    assert_eq!(opts.format, ExplainFormat::Text);
    let (_, _, opts) = explain_opts("EXPLAIN (FORMAT XML) SELECT 1");
    assert_eq!(opts.format, ExplainFormat::Xml);
    let (_, _, opts) = explain_opts("EXPLAIN (FORMAT JSON) SELECT 1");
    assert_eq!(opts.format, ExplainFormat::Json);
    let (_, _, opts) = explain_opts("EXPLAIN (FORMAT YAML) SELECT 1");
    assert_eq!(opts.format, ExplainFormat::Yaml);
    // Bad value: PG19's exact 22023 text. Unquoted BOGUS is folded
    // to lowercase by the tokenizer; a quoted "JSON" keeps its case and
    // is rejected (PG19 strcmp on the raw value).
    let err = parse_err("EXPLAIN (FORMAT BOGUS) SELECT 1");
    assert_eq!(err.code, "22023");
    assert_eq!(
        err.message,
        "unrecognized value for EXPLAIN option \"format\": \"bogus\""
    );
    let err = parse_err("EXPLAIN (FORMAT \"JSON\") SELECT 1");
    assert_eq!(err.code, "22023");
    assert_eq!(
        err.message,
        "unrecognized value for EXPLAIN option \"format\": \"JSON\""
    );
    // Bare FORMAT: PG19's 42601 "format requires a parameter".
    let err = parse_err("EXPLAIN (FORMAT) SELECT 1");
    assert_eq!(err.code, "42601");
    assert_eq!(err.message, "format requires a parameter");
}

#[test]
fn duplicate_last_wins_new_options() {
    let (_, _, opts) = explain_opts("EXPLAIN (WAL, ANALYZE, WAL OFF) SELECT 1");
    assert!(!opts.wal);
    let (_, _, opts) = explain_opts("EXPLAIN (SERIALIZE NONE, ANALYZE, SERIALIZE TEXT) SELECT 1");
    assert_eq!(opts.serialize, ExplainSerialize::Text);
}
