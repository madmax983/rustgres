/// v1.90: SQL/JSON constructors — JSON_OBJECT, JSON_ARRAY, JSON(),
/// JSON_SCALAR, JSON_SERIALIZE, JSON_ARRAYAGG, JSON_OBJECTAGG and
/// IS JSON predicates, desugared onto internal builtins over the
/// jsonb value model (grounded in PG19 sqljson.sql/.out + parse_expr.c
/// transformJson* and json.c builder/aggregate workers).
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

/// All rows of a SELECT as output-text cells (None = SQL NULL).
fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("{sql} -> unexpected {other:?}"),
    }
}

fn one(eng: &mut Engine, sql: &str) -> Option<String> {
    let r = rows(eng, sql);
    assert_eq!(r.len(), 1, "{sql}");
    assert_eq!(r[0].len(), 1, "{sql}");
    r[0][0].clone()
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql) {
        Err(e) => e.code.to_string(),
        Ok(_) => panic!("{sql} unexpectedly succeeded"),
    }
}

#[test]
fn json_parse_ctor() {
    let mut eng = engine();
    assert_eq!(
        one(&mut eng, "SELECT JSON('{ \"a\" : 1 } ')").as_deref(),
        Some("{ \"a\" : 1 } ")
    );
    assert_eq!(
        one(&mut eng, "SELECT JSON('   1   '::jsonb)").as_deref(),
        Some("1")
    );
    assert_eq!(err_code(&mut eng, "SELECT JSON(123)"), "42846");
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON('{\"a\": 1, \"a\": 2}' WITH UNIQUE KEYS)"
        ),
        "22030"
    );
    assert_eq!(err_code(&mut eng, "SELECT JSON('[1,2')"), "22P02");
    assert_eq!(one(&mut eng, "SELECT JSON(NULL::text)").as_deref(), None);
}

#[test]
fn json_scalar_and_serialize() {
    let mut eng = engine();
    assert_eq!(
        one(&mut eng, "SELECT JSON_SCALAR(123)").as_deref(),
        Some("123")
    );
    assert_eq!(
        one(&mut eng, "SELECT JSON_SCALAR('abc')").as_deref(),
        Some("\"abc\"")
    );
    assert_eq!(
        one(&mut eng, "SELECT JSON_SCALAR('{}'::jsonb)").as_deref(),
        Some("{}")
    );
    assert_eq!(
        one(&mut eng, "SELECT JSON_SCALAR(NULL::int)").as_deref(),
        None
    );
    assert_eq!(
        one(&mut eng, "SELECT JSON_SERIALIZE('{\"a\":1}')").as_deref(),
        Some("{\"a\":1}")
    );
    assert_eq!(
        err_code(&mut eng, "SELECT JSON_SERIALIZE('{}' RETURNING jsonb)"),
        "42804"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT JSON_SERIALIZE('{}' RETURNING varchar(1))"),
        "22001"
    );
}

#[test]
fn json_object_modes() {
    let mut eng = engine();
    // json text mode: insertion order, `"k" : v` spacing.
    assert_eq!(
        one(&mut eng, "SELECT JSON_OBJECT('a': 2 + 3)").as_deref(),
        Some("{\"a\" : 5}")
    );
    // A jsonb-typed argument flips the default to jsonb mode
    // (transformJsonConstructorOutput): canonical, key-sorted.
    assert_eq!(
        one(&mut eng, "SELECT JSON_OBJECT('b': 1, 'a': '{}'::jsonb)").as_deref(),
        Some("{\"a\": {}, \"b\": 1}")
    );
    // ABSENT ON NULL drops pairs; the UNIQUE check still sees them.
    assert_eq!(
        one(
            &mut eng,
            "SELECT JSON_OBJECT('a': NULL, 'b': 2 ABSENT ON NULL)"
        )
        .as_deref(),
        Some("{\"b\" : 2}")
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON_OBJECT('a': NULL, 'a': 2 ABSENT ON NULL WITH UNIQUE KEYS)"
        ),
        "22030"
    );
    assert_eq!(err_code(&mut eng, "SELECT JSON_OBJECT(NULL: 1)"), "22004");
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON_OBJECT('a': 'b' RETURNING jsonb, 'a': 1)"
        ),
        "42601"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT JSON_OBJECT(json '[1]': 1)"),
        "22023"
    );
    // FORMAT JSON on a non-string type is a parse-analysis error in
    // PG, so it fires even for NULL values.
    assert_eq!(
        err_code(&mut eng, "SELECT JSON_OBJECT('a': NULL::int FORMAT JSON)"),
        "42804"
    );
    assert_eq!(
        one(&mut eng, "SELECT JSON_OBJECT('a': NULL::jsonb FORMAT JSON)").as_deref(),
        Some("{\"a\": null}")
    );
}

#[test]
fn json_array_and_subquery() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "SELECT JSON_ARRAY('aaa', 111, true, NULL, json '{\"a\": [1]}')"
        )
        .as_deref(),
        Some("[\"aaa\", 111, true, {\"a\": [1]}]")
    );
    assert_eq!(
        one(&mut eng, "SELECT JSON_ARRAY('a', NULL, 'b' NULL ON NULL)").as_deref(),
        Some("[\"a\", null, \"b\"]")
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT JSON_ARRAY(SELECT i FROM (VALUES (1), (2), (NULL), (4)) foo(i))"
        )
        .as_deref(),
        Some("[1, 2, 4]")
    );
    assert_eq!(
        one(&mut eng, "SELECT JSON_ARRAY(SELECT 1 WHERE FALSE)").as_deref(),
        Some("[]")
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON_ARRAY(SELECT i, i FROM (VALUES (1)) foo(i))"
        ),
        "42601"
    );
    // RETURNING coercion of the '[]' default is eager.
    assert_eq!(
        err_code(&mut eng, "SELECT JSON_ARRAY(SELECT 1 RETURNING varchar(1))"),
        "22001"
    );
}

#[test]
fn constructor_aggregates() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "SELECT JSON_ARRAYAGG(i) FROM generate_series(1, 3) i"
        )
        .as_deref(),
        Some("[1, 2, 3]")
    );
    // Zero input rows -> SQL NULL (not '[]').
    assert_eq!(
        one(
            &mut eng,
            "SELECT JSON_ARRAYAGG(i) FROM generate_series(1, 0) i"
        )
        .as_deref(),
        None
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT JSON_OBJECTAGG(k: v) FROM (VALUES (1, 1), (1, NULL), (2, 2)) foo(k, v)"
        )
        .as_deref(),
        Some("{ \"1\" : 1, \"1\" : null, \"2\" : 2 }")
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON_OBJECTAGG(k: v WITH UNIQUE KEYS) FROM (VALUES (1, 1), (1, 2)) foo(k, v)"
        ),
        "22030"
    );
    // Windowed form over a default frame.
    let r = rows(
        &mut eng,
        "SELECT JSON_ARRAYAGG(i) OVER (ORDER BY i) FROM generate_series(1, 3) i",
    );
    assert_eq!(
        r,
        vec![
            vec![Some("[1]".to_string())],
            vec![Some("[1, 2]".to_string())],
            vec![Some("[1, 2, 3]".to_string())],
        ]
    );
}

#[test]
fn is_json_predicates() {
    let mut eng = engine();
    assert_eq!(one(&mut eng, "SELECT '123' IS JSON").as_deref(), Some("t"));
    assert_eq!(one(&mut eng, "SELECT '' IS JSON").as_deref(), Some("f"));
    assert_eq!(one(&mut eng, "SELECT NULL IS JSON").as_deref(), None);
    assert_eq!(err_code(&mut eng, "SELECT 123 IS JSON"), "42804");
    assert_eq!(
        one(
            &mut eng,
            "SELECT '{\"a\": 1, \"a\": 2}' IS JSON WITH UNIQUE KEYS"
        )
        .as_deref(),
        Some("f")
    );
    assert_eq!(
        one(&mut eng, "SELECT '[1]' IS JSON ARRAY").as_deref(),
        Some("t")
    );
    assert_eq!(
        one(&mut eng, "SELECT 'null' IS JSON SCALAR").as_deref(),
        Some("t")
    );
    assert_eq!(
        one(&mut eng, "SELECT 'aaa' IS NOT JSON").as_deref(),
        Some("t")
    );
    // bytea with a NUL byte: the encoding verifier errors (22021)
    // instead of the predicate returning false.
    assert_eq!(err_code(&mut eng, "SELECT bytea '\\x00' IS JSON"), "22021");
}
