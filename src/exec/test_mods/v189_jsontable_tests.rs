/// v1.89: JSON_TABLE — the SQL/JSON table function over the v1.85
/// jsonpath engine: root-path row source, FOR ORDINALITY, regular
/// columns (PATH / FORMAT JSON / WRAPPER / QUOTES / ON EMPTY /
/// ON ERROR), EXISTS columns, NESTED PATH union semantics, PASSING,
/// domains/composites/arrays, and the PG error cases (grounded in
/// PG19 sqljson_jsontable.sql + parse_expr.c behavior validity).
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
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql)
        .expect_err(&format!("{sql} should fail"))
        .code
        .to_string()
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn jt_basic_and_ordinality() {
    let mut eng = engine();
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '[10, 20]', '$[*]' columns (n for ordinality, v int path '$')) jt"
        ),
        vec![vec![s("1"), s("10")], vec![s("2"), s("20")]]
    );
    // NULL document => empty table.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(null::jsonb, '$' columns (a int)) jt"
        ),
        Vec::<Vec<Option<String>>>::new()
    );
    // Implicit path is $."<name>".
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '{\"a\": 7}', '$' columns (a int)) jt"
        ),
        vec![vec![s("7")]]
    );
}

#[test]
fn jt_root_on_error_default_empty() {
    let mut eng = engine();
    // Default table-level ON ERROR is EMPTY: a strict root-path error
    // yields zero rows; the explicit ERROR form raises.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '\"1.23\"', 'strict $.a' columns (js2 int path '$')) jt"
        ),
        Vec::<Vec<Option<String>>>::new()
    );
    assert_eq!(
        err_code(
            &mut eng,
            "select * from json_table(jsonb '\"1.23\"', 'strict $.a' columns (js2 int path '$') error on error) jt"
        ),
        "2203A"
    );
    // Table-level behavior validation: only EMPTY/ERROR family.
    err_code(
        &mut eng,
        "select * from json_table(jsonb '1', '$' columns (a int) null on error) jt",
    );
}

#[test]
fn jt_column_behaviors() {
    let mut eng = engine();
    // Conversion failure defaults to NULL ON ERROR (no error raised).
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '\"err\"', '$' columns (a int path '$')) jt"
        ),
        vec![vec![None]]
    );
    // ERROR ON EMPTY names the column (22035).
    assert_eq!(
        err_code(
            &mut eng,
            "select * from json_table(jsonb '1', '$' columns (a int path '$.a' error on empty)) jt"
        ),
        "22035"
    );
    // DEFAULT on empty / on error.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '\"a\"', '$' columns (a int path '$' default 1 on empty default 2 on error)) jt"
        ),
        vec![vec![s("2")]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '\"a\"', '$' columns (a int path 'lax $.a' default 1 on empty default 2 on error)) jt"
        ),
        vec![vec![s("1")]]
    );
}

#[test]
fn jt_exists_columns() {
    let mut eng = engine();
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '{\"a\": 1}', '$' columns (e int exists path '$.a', f bool exists path '$.b')) jt"
        ),
        vec![vec![s("1"), s("f")]]
    );
    // int2 has no bool cast in PG: coercing the default FALSE
    // ON ERROR behavior is a hard 42804.
    assert_eq!(
        err_code(
            &mut eng,
            "select * from json_table(jsonb '\"a\"', '$' columns (a int2 exists path '$.a')) jt"
        ),
        "42804"
    );
    // UNKNOWN ON ERROR on a strict-path failure => NULL.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '1', '$' columns (e text exists path 'strict $.a' unknown on error)) jt"
        ),
        vec![vec![None]]
    );
    // EXISTS with an ON EMPTY clause is an error.
    err_code(
        &mut eng,
        "select * from json_table(jsonb '1', '$' columns (a int exists path '$' null on empty)) jt",
    );
}

#[test]
fn jt_wrapper_and_quotes() {
    let mut eng = engine();
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '\"world\"', '$' columns (item text path '$' keep quotes on scalar string)) jt"
        ),
        vec![vec![s("\"world\"")]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '\"world\"', '$' columns (item text format json path '$' with wrapper)) jt"
        ),
        vec![vec![s("[\"world\"]")]]
    );
    // WITH WRAPPER + OMIT QUOTES is an error.
    err_code(
        &mut eng,
        "select * from json_table(jsonb '\"world\"', '$' columns (item text path '$' with wrapper omit quotes)) jt",
    );
    // Multiple items without a wrapper: error subject to ON ERROR
    // (default NULL); WITH WRAPPER collects them.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '{\"a\": [1, 2]}', '$' columns (b jsonb path '$.a[*]')) jt"
        ),
        vec![vec![None]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '{\"a\": [1, 2]}', '$' columns (b jsonb path '$.a[*]' with wrapper)) jt"
        ),
        vec![vec![s("[1, 2]")]]
    );
}

#[test]
fn jt_nested_union_semantics() {
    let mut eng = engine();
    // Sibling NESTED paths union (3 + 2 rows), they do not cross-join.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '[{\"b\": [1, 2, 3], \"c\": [10, 20]}]', '$[*]' columns (nested path '$.b[*]' columns (b int path '$'), nested path '$.c[*]' columns (c int path '$'))) jt"
        ),
        vec![
            vec![s("1"), None],
            vec![s("2"), None],
            vec![s("3"), None],
            vec![None, s("10")],
            vec![None, s("20")],
        ]
    );
    // All nested sequences empty => one all-NULL row for the parent.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '[{\"b\": [], \"c\": []}]', '$[*]' columns (a int path '$.a' default -1 on empty, nested path '$.b[*]' columns (b int path '$'), nested path '$.c[*]' columns (c int path '$'))) jt"
        ),
        vec![vec![s("-1"), None, None]]
    );
    // Per-parent ordinality inside a nested list restarts at 1.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '[[1, 2], [3]]', '$[*]' columns (nested path '$[*]' columns (id for ordinality, v int path '$'))) jt"
        ),
        vec![
            vec![s("1"), s("1")],
            vec![s("2"), s("2")],
            vec![s("1"), s("3")],
        ]
    );
}

#[test]
fn jt_passing_and_lateral() {
    let mut eng = engine();
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '[1, 2, 3]', '$[*] ? (@ < $x)' passing 3 as x columns (y text format json path '$')) jt"
        ),
        vec![vec![s("1")], vec![s("2")]]
    );
    // Implicit LATERAL against a preceding FROM item (no keyword).
    run(&mut eng, "create temp table jt_docs (js jsonb)").unwrap();
    run(&mut eng, "insert into jt_docs values ('[1, 2]'), ('[]')").unwrap();
    assert_eq!(
        rows(
            &mut eng,
            "select v from jt_docs d left outer join json_table(d.js, '$[*]' columns (v int path '$')) jt on true order by v"
        ),
        vec![vec![s("1")], vec![s("2")], vec![None]]
    );
}

#[test]
fn jt_name_validation() {
    let mut eng = engine();
    // Duplicate column name.
    assert_eq!(
        err_code(
            &mut eng,
            "select * from json_table(jsonb '1', '$' columns (a int, a int)) jt"
        ),
        "42P09"
    );
    // Root path name collides with a column name.
    assert_eq!(
        err_code(
            &mut eng,
            "select * from json_table(jsonb '1', '$' as a columns (a int)) jt"
        ),
        "42P09"
    );
    // Two FOR ORDINALITY columns in one list.
    err_code(
        &mut eng,
        "select * from json_table(jsonb '1', '$' columns (a for ordinality, b for ordinality)) jt",
    );
    // Non-constant path.
    err_code(
        &mut eng,
        "select * from json_table(jsonb '{\"a\": 1}', '$' || '.a' columns (a int)) jt",
    );
    // Scalar column behavior validity: TRUE ON EMPTY is invalid.
    err_code(
        &mut eng,
        "select * from json_table(jsonb '1', '$' columns (a int true on empty)) jt",
    );
}

#[test]
fn jt_types_domain_composite_array() {
    let mut eng = engine();
    run(
        &mut eng,
        "create domain jt_dom as text check (value <> 'foo')",
    )
    .unwrap();
    run(&mut eng, "create type jt_comp as (a int, b int)").unwrap();
    // Domain: a CHECK-violating item lands NULL (default ON ERROR).
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '[\"foo\", \"bar\"]', '$[*]' columns (d jt_dom path '$')) jt"
        ),
        vec![vec![None], vec![s("bar")]]
    );
    // Composite from a string item via its input function.
    assert_eq!(
        rows(
            &mut eng,
            "select (comp).a from json_table(jsonb '{\"rec\": \"(1,2)\"}', '$' columns (comp jt_comp path '$.rec' omit quotes)) jt"
        ),
        vec![vec![s("1")]]
    );
    // Array column from a JSON array item, elementwise.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '{\"a\": [1, 2]}', '$' columns (ia int[] path '$.a')) jt"
        ),
        vec![vec![s("{1,2}")]]
    );
    // Bool item into a plain text column renders 't'/'f' (boolout).
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '[true]', '$[*]' columns (t text path '$')) jt"
        ),
        vec![vec![s("t")]]
    );
    // Object item into a scalar column is an error class (2203F),
    // surfaced as NULL under the default ON ERROR.
    assert_eq!(
        rows(
            &mut eng,
            "select * from json_table(jsonb '[{\"a\": 1}]', '$[*]' columns (t text path '$')) jt"
        ),
        vec![vec![None]]
    );
}

#[test]
fn sqljson_value_default_string_assignment_coercion() {
    // PG19 transformJsonBehavior: a JSON_VALUE DEFAULT expression is
    // coerced to a string-category RETURNING type with an ASSIGNMENT
    // cast, so an over-length default errors 22001 instead of being
    // silently truncated by explicit-cast semantics (PG19
    // sqljson_queryfuncs.sql, the typmod-coercion trio).
    let mut eng = engine();
    assert_eq!(
        err_code(
            &mut eng,
            "select json_value(jsonb '{}', '$.a' returning varchar(3) default 'toolong'::varchar(10) on empty)"
        ),
        "22001"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "select json_value(jsonb '{}', '$.a' returning varchar(3) default 'toolong' on empty)"
        ),
        "22001"
    );
    // A default that fits is returned as-is; a NULL default needs no
    // coercion and yields NULL.
    assert_eq!(
        rows(
            &mut eng,
            "select json_value(jsonb '{}', '$.a' returning varchar(3) default 'ab' on empty)"
        ),
        vec![vec![s("ab")]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "select json_value(jsonb '{}', '$.a' returning numeric(4,1) default null::numeric on empty)"
        ),
        vec![vec![None]]
    );
    // The numeric sibling: typmod overflow under the (explicit) cast
    // coercion PG uses for non-string categories.
    assert_eq!(
        err_code(
            &mut eng,
            "select json_value(jsonb '{}', '$.a' returning numeric(4,1) default 99999.999 on empty)"
        ),
        "22003"
    );
}
