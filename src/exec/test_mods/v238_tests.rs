/// v2.38: JSONB TAIL — jsonpath comparison/datetime overhaul
/// (compareItems semantics, the compareDatetime matrix with the
/// 0A000 timezone-usage errors, ISO datetime rendering), silent-mode
/// rules for jsonb_path_exists/match (NULL on error, hard 42704 for
/// unbound variables), .boolean() via int4 text, .decimal()
/// significant-digit precision, unary/binary arithmetic operand
/// unwrapping, like_regex NLSTOP/NLANCH decoupling and PG ARE
/// escapes (\b = backspace, \y = word boundary), jsonb subscript
/// assignment (FILL_GAPS, NULL base builds the document, NULL
/// subscript errors), jsonb || unknown-literal resolution,
/// jsonb_to_tsvector flags, populate_record domain-over-composite
/// and json[] element handling, nested composite casts.
/// Ground truth: PG19 src/backend/utils/adt/jsonpath_exec.c,
/// jsonb.c, regc_lex.c; src/test/regress/sql/jsonb.sql,
/// jsonb_jsonpath.sql.
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
        privs_suppressed: false,
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

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    match run(eng, sql) {
        Err(e) => e,
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
    }
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn v238_compare_items_semantics() {
    let mut eng = engine();
    // null vs non-null: only != is true.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('{\"a\": null}', '$.a == 1')"
        ),
        vec![vec![s("false")]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('{\"a\": null}', '$.a != 1')"
        ),
        vec![vec![s("true")]]
    );
    // No number<->string coercion: the pair is unknown -> JSON null.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('{\"a\": 1}', '$.a == \"1\"')"
        ),
        vec![vec![s("null")]]
    );
    // Arrays are never comparable: an array operand against a
    // scalar is unknown, materializing as a JSON null item.
    assert_eq!(
        rows(&mut eng, "select jsonb_path_query('[1]', '$ == 1')"),
        vec![vec![s("null")]]
    );
    // Strict mode: an unknown pair poisons the comparison.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('[1, \"a\"]', 'strict $[*] > 0')"
        ),
        vec![vec![s("null")]]
    );
}

#[test]
fn v238_exists_match_silent_null_and_hard_var_error() {
    let mut eng = engine();
    // Silent defaults are TRUE for exists/match: an item error
    // (strict-mode structural error here) yields SQL NULL, not
    // false.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_exists('{\"a\": 1}', 'strict $.b')"
        ),
        vec![vec![None]]
    );
    // An unbound variable is a hard error even in silent mode.
    let e = err(&mut eng, "select jsonb_path_exists('1', '$undefined_var')");
    assert_eq!(e.code, "42704");
    // The @@ operator is the silent form too.
    assert_eq!(
        rows(&mut eng, "select '{\"a\": 1}'::jsonb @@ 'strict $.b'"),
        vec![vec![None]]
    );
}

#[test]
fn v238_datetime_compare_matrix() {
    let mut eng = engine();
    // Date x Timestamp compares at midnight.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('\"2023-08-15\"', '$.datetime() < \"2023-08-15 12:00:00\".datetime()')"
        ),
        vec![vec![s("true")]]
    );
    // Naive x zoned without *_tz usage: hard 0A000.
    let e = err(
        &mut eng,
        "select jsonb_path_query('\"2023-08-15\"', '$.datetime() == \"2023-08-15 00:00:00+05\".datetime()')",
    );
    assert_eq!(e.code, "0A000");
    // Time x TimeTz likewise.
    let e = err(
        &mut eng,
        "select jsonb_path_query('\"12:00:00\"', '$.datetime() == \"12:00:00+05\".datetime()')",
    );
    assert_eq!(e.code, "0A000");
    // Overflow-safe date compare (PG clamps by day parts).
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('\"1000000-01-01\"', '$.datetime() > \"2020-01-01 12:00:00\".datetime()'::jsonpath)"
        ),
        vec![vec![s("true")]]
    );
    // .type() names the full PG type names.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('\"12:34:56+05:30\"', '$.time_tz().type()')"
        ),
        vec![vec![s("\"time with time zone\"")]]
    );
    // Named zone designators classify as zoned values.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('\"2017-03-10T12:34:56.789EST\"', '$.datetime()')"
        ),
        vec![vec![s("\"2017-03-10T12:34:56.789-05:00\"")]]
    );
}

#[test]
fn v238_boolean_and_decimal_methods() {
    let mut eng = engine();
    // .boolean() converts the numeric through int4 text.
    assert_eq!(
        rows(&mut eng, "select jsonb_path_query('100', '$.boolean()')"),
        vec![vec![s("true")]]
    );
    assert_eq!(
        err(&mut eng, "select jsonb_path_query('1.23', '$.boolean()')").code,
        "22036"
    );
    assert_eq!(
        err(&mut eng, "select jsonb_path_query('1e1000', '$.boolean()')").code,
        "22036"
    );
    // .decimal() precision counts significant digits of the rounded
    // value: numeric(1,2) accepts 0.01.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('0.0123456', '$.decimal(1, 2)')"
        ),
        vec![vec![s("0.01")]]
    );
    // .size() on an object: 1 in lax mode, error in strict mode.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('{\"a\": 1}', '$.size()')"
        ),
        vec![vec![s("1")]]
    );
    assert_eq!(
        err(
            &mut eng,
            "select jsonb_path_query('{\"a\": 1}', 'strict $.size()')"
        )
        .code,
        "22039"
    );
}

#[test]
fn v238_arith_unwrap_and_unary() {
    let mut eng = engine();
    // Operand sequences auto-unwrap one array level.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('{\"a\": [2]}', 'lax $.a * 3')"
        ),
        vec![vec![s("6")]]
    );
    // Unary minus applies per item; strings are NOT coerced.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('{\"a\": [2, 3]}', 'lax -$.a')"
        ),
        vec![vec![s("-2")], vec![s("-3")]]
    );
    assert_eq!(
        err(&mut eng, "select jsonb_path_query('\"2\"', '-$')").code,
        "22038"
    );
    // Subscript truncation is toward zero.
    assert_eq!(
        rows(&mut eng, "select jsonb_path_query('[10, 20]', '$[0.5]')"),
        vec![vec![s("10")]]
    );
}

#[test]
fn v238_like_regex_flags_and_escapes() {
    let mut eng = engine();
    // Default: '.' does not cross a newline (NLSTOP), anchors stay
    // string-bound (no NLANCH without the m flag).
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('\"adc\\nabc\"', '$ ? (@ like_regex \"^ab.*c\")')"
        ),
        Vec::<Vec<Option<String>>>::new()
    );
    // PG ARE: \b is a backspace, \y is the word boundary.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('\"a\\b\"', '$ ? (@ like_regex \"a\\\\b\")')"
        ),
        vec![vec![s("\"a\\b\"")]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_path_query('\"ab\"', '$ ? (@ like_regex \"a\\\\yb\")')"
        ),
        Vec::<Vec<Option<String>>>::new()
    );
}

#[test]
fn v238_subscript_assignment() {
    let mut eng = engine();
    run(&mut eng, "create temp table t (id int, j jsonb)").unwrap();
    run(&mut eng, "insert into t values (1, '[0]'), (2, null)").unwrap();
    // FILL_GAPS: arrays pad with JSON nulls.
    run(&mut eng, "update t set j[3] = '9' where id = 1").unwrap();
    assert_eq!(
        rows(&mut eng, "select j from t where id = 1"),
        vec![vec![s("[0, null, null, 9]")]]
    );
    // A NULL base builds the document.
    run(&mut eng, "update t set j['a'] = '1' where id = 2").unwrap();
    assert_eq!(
        rows(&mut eng, "select j from t where id = 2"),
        vec![vec![s("{\"a\": 1}")]]
    );
    // A NULL subscript is an error (22004).
    assert_eq!(err(&mut eng, "update t set j[NULL] = '1'").code, "22004");
    // Descending into an existing scalar errors 22023.
    run(&mut eng, "insert into t values (3, '{\"a\": 1}')").unwrap();
    assert_eq!(
        err(&mut eng, "update t set j['a']['b'] = '1' where id = 3").code,
        "22023"
    );
}

#[test]
fn v238_concat_and_subscript_fetch() {
    let mut eng = engine();
    // An uncast literal beside a jsonb resolves to jsonb concat.
    assert_eq!(
        rows(&mut eng, "select '{\"a\": 1}'::jsonb || '{\"b\": 2}'"),
        vec![vec![s("{\"a\": 1, \"b\": 2}")]]
    );
    // Text subscript on an array parses as an integer index.
    assert_eq!(
        rows(&mut eng, "select ('[1, \"2\", null]'::jsonb)['1']"),
        vec![vec![s("\"2\"")]]
    );
    // jsonb scalar casts: JSON null -> SQL NULL; string -> error.
    assert_eq!(
        rows(&mut eng, "select ('null'::jsonb)::int"),
        vec![vec![None]]
    );
    assert_eq!(err(&mut eng, "select ('\"x\"'::jsonb)::int").code, "42846");
}

#[test]
fn v238_populate_domain_and_json_array() {
    let mut eng = engine();
    run(&mut eng, "create type pair as (x int, y int)").unwrap();
    run(
        &mut eng,
        "create domain ordered_pair as pair check ((value).x <= (value).y)",
    )
    .unwrap();
    // A domain over a composite populates as its base composite.
    assert_eq!(
        rows(
            &mut eng,
            "select jsonb_populate_record(null::ordered_pair, '{\"x\": 0, \"y\": 1}')"
        ),
        vec![vec![s("(0,1)")]]
    );
    // json[] elements keep nested arrays as values.
    run(&mut eng, "create type jrec as (jsa json[])").unwrap();
    assert_eq!(
        rows(
            &mut eng,
            "select jsa from jsonb_populate_record(null::jrec, '{\"jsa\": [\"aaa\", [1, 2]]}') q"
        ),
        vec![vec![s("{\"\\\"aaa\\\"\",\"[1, 2]\"}")]]
    );
}

#[test]
fn v238_tsvector_and_nested_cast() {
    let mut eng = engine();
    // jsonb_to_tsvector flag validation.
    assert_eq!(
        err(
            &mut eng,
            "select jsonb_to_tsvector('{\"a\": 1}', '\"bogus\"'::jsonb)"
        )
        .code,
        "22023"
    );
    // A nested composite field casts through its named type.
    run(&mut eng, "create type inner as (a text, b int)").unwrap();
    run(&mut eng, "create type outer as (i int, r inner)").unwrap();
    assert_eq!(
        rows(&mut eng, "select row(1, row('x', 3)::inner)::outer"),
        vec![vec![s("(1,\"(x,3)\")")]]
    );
}
