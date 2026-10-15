/// v1.92: SQL/JSON query-functions remainder — JsonBehavior
/// defaults (VALUE/QUERY default NULL, EXISTS defaults FALSE),
/// parser clauses (wrapper forms, QUOTES ON SCALAR STRING, PASSING
/// quoted identifiers and geometric typed literals, RETURNING
/// bit/varbit typmods), JSON_QUERY populate-item coercion
/// (keep/omit-quotes pairs, bytea, 42804 behavior wraps), JSON_EXISTS
/// root-predicate paths, the index immutability cluster (auto-name
/// uniquification + 42P17 via PG's jspIsMutable rules), int->bit
/// casts, and PG19 record_out quote doubling with unnest of a
/// record array.
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

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn cell(eng: &mut Engine, sql: &str) -> Option<String> {
    rows(eng, sql)
        .first()
        .and_then(|r| r.iter().next())
        .cloned()
        .flatten()
}

fn err_code(eng: &mut Engine, sql: &str) -> &'static str {
    match run(eng, sql) {
        Err(e) => e.code,
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
    }
}

fn b(eng: &mut Engine, sql: &str) -> bool {
    cell(eng, sql).as_deref() == Some("t")
}

#[test]
fn v192_behavior_defaults() {
    let mut eng = engine();
    // JSON_VALUE: unspecified ON EMPTY / ON ERROR default to NULL
    // (a conversion error and a missing item both yield NULL).
    assert_eq!(
        cell(
            &mut eng,
            "select json_value(jsonb '1.23', '$' returning int)"
        ),
        None
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_value(jsonb '{}', '$.a' returning int)"
        ),
        None
    );
    // JSON_QUERY: the same NULL defaults.
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[1,2]', '$[*]' without wrapper)"
        ),
        None
    );
    // JSON_EXISTS: unspecified ON ERROR defaults to FALSE (a strict
    // path error becomes false, not NULL and not an error), while
    // an explicit ERROR behavior still raises.
    assert_eq!(
        err_code(
            &mut eng,
            "select json_exists(jsonb '1', 'strict $.a' error on error)"
        ),
        "2203A"
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_exists(jsonb '1', 'strict $.a' false on error)"
        )
        .as_deref(),
        Some("f")
    );
    assert_eq!(
        cell(&mut eng, "select json_exists(jsonb '1', 'strict $.a')").as_deref(),
        Some("f")
    );
}

#[test]
fn v192_query_scalar_and_wrappers() {
    let mut eng = engine();
    // WITHOUT WRAPPER singleton scalar comes back as a jsonb scalar
    // (PG19 JsonPathQuery; no 2203C).
    assert_eq!(
        cell(&mut eng, "select json_query(jsonb '123', '$')").as_deref(),
        Some("123")
    );
    assert_eq!(
        cell(&mut eng, "select json_query(jsonb '\"aaa\"', '$')").as_deref(),
        Some("\"aaa\"")
    );
    // CONDITIONAL WRAPPER wraps only multi-item sequences; a single
    // scalar item stays bare, a single array item stays as-is.
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[\"1\"]', '$[*]' with conditional wrapper)"
        )
        .as_deref(),
        Some("\"1\"")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[[1,2]]', '$[*]' with conditional wrapper)"
        )
        .as_deref(),
        Some("[1, 2]")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[1,2]', '$[*]' with conditional wrapper)"
        )
        .as_deref(),
        Some("[1, 2]")
    );
    // UNCONDITIONAL (and bare WITH WRAPPER / WITH ARRAY WRAPPER)
    // always wrap.
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[[1,2]]', '$[*]' with unconditional wrapper)"
        )
        .as_deref(),
        Some("[[1, 2]]")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[1,2]', '$[*]' with array wrapper)"
        )
        .as_deref(),
        Some("[1, 2]")
    );
    // OMIT QUOTES with WITH WRAPPER is a parse error (42601); KEEP
    // QUOTES with a wrapper is fine.
    assert_eq!(
        err_code(
            &mut eng,
            "select json_query(jsonb '[1,2]', '$[*]' with wrapper omit quotes)"
        ),
        "42601"
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[1,2]', '$[*]' with wrapper keep quotes)"
        )
        .as_deref(),
        Some("[1, 2]")
    );
}

#[test]
fn v192_query_populate_coercion() {
    let mut eng = engine();
    // Keep/omit quotes pairs for a structured target: OMIT feeds
    // the string content to the array input function; KEEP fails
    // (default ON ERROR -> NULL; ERROR ON ERROR -> 22P02).
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '\"{1,2,3}\"', '$' returning int[] omit quotes)"
        )
        .as_deref(),
        Some("{1,2,3}")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '\"{1,2,3}\"', '$' returning int[] keep quotes)"
        ),
        None
    );
    assert_eq!(
        err_code(
            &mut eng,
            "select json_query(jsonb '\"{1,2,3}\"', '$' returning int[] keep quotes error on error)"
        ),
        "22P02"
    );
    // Scalar string-family target under KEEP QUOTES keeps the JSON
    // representation; OMIT gives the content.
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '\"aaa\"', '$' returning text)"
        )
        .as_deref(),
        Some("\"aaa\"")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '\"aaa\"', '$' returning text omit quotes)"
        )
        .as_deref(),
        Some("aaa")
    );
    // An array item into a text target renders as JSON text (no
    // 2203F; that code is JSON_VALUE-only).
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[1,2]', '$' returning text)"
        )
        .as_deref(),
        Some("[1, 2]")
    );
    // Range target from a string item: OMIT parses the range
    // literal; KEEP fails through input of the quoted text.
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '\"[1,2]\"', '$' returning int4range omit quotes)"
        )
        .as_deref(),
        Some("[1,3)")
    );
    // RETURNING bytea: the item's JSON representation bytes.
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[1,2]', '$' returning bytea)"
        )
        .as_deref(),
        Some("\\x5b312c20325d")
    );
    // EMPTY ARRAY ON ERROR is itself coerced to the RETURNING type
    // (here: the empty int array).
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '\"x\"', '$' returning int[] empty array on error)"
        )
        .as_deref(),
        Some("{}")
    );
}

#[test]
fn v192_behavior_coercion_wrap_42804() {
    let mut eng = engine();
    run(&mut eng, "create domain v192_int_nn as int not null").unwrap();
    // A behavior value that cannot coerce to the RETURNING type is
    // 42804 with PG's message shape.
    let e = run(
        &mut eng,
        "select json_query(jsonb '[3,4]', '$[*]' returning bigint[] empty object on error)",
    )
    .unwrap_err();
    assert_eq!(e.code, "42804");
    assert!(
        e.message
            .contains("could not coerce ON ERROR expression (EMPTY OBJECT)")
    );
    // A JSON null item into a NOT NULL domain fails unconditionally
    // (the coercion sits outside the ON ERROR behavior).
    assert_eq!(
        err_code(
            &mut eng,
            "select json_value(jsonb 'null', '$' returning v192_int_nn)"
        ),
        "23502"
    );
    // Path parse errors and unbound PASSING variables are hard
    // errors, not behavior cases.
    assert_eq!(
        err_code(
            &mut eng,
            "select json_query(jsonb '{\"a\": 123}', 'error' || ' ' || 'error')"
        ),
        "22P02"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "select json_query(jsonb 'null', '$xyz' passing 1 as xy)"
        ),
        "42704"
    );
}

#[test]
fn v192_exists_root_predicates() {
    let mut eng = engine();
    // A root-level predicate in exists mode: PG's appendBoolResult
    // reports existence regardless of the (possibly erroring /
    // empty) predicate outcome.
    assert!(b(&mut eng, "select json_exists(jsonb '1', '$ > 2')"));
    assert!(b(
        &mut eng,
        "select json_exists(jsonb '1', '$.a > 2' error on error)"
    ));
    assert!(b(
        &mut eng,
        "select jsonb_path_exists(jsonb '1', '$.a > 2')"
    ));
}

#[test]
fn v192_passing_idents_and_literals() {
    let mut eng = engine();
    // Quoted PASSING identifiers keep their case.
    assert_eq!(
        cell(
            &mut eng,
            "select json_value(jsonb '{\"Xyz\": 5}', '$.\"Xyz\"' passing 5 as \"Xyz\")"
        )
        .as_deref(),
        Some("5")
    );
    // Geometric typed literal in PASSING (previously 42601 at AS).
    assert_eq!(
        cell(
            &mut eng,
            "select json_value(jsonb 'null', '$a' passing point ' (1, 2 )' as a)"
        )
        .as_deref(),
        Some("(1,2)")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_value(jsonb 'null', '$a' passing point ' (1, 2 )' as a returning point)"
        )
        .as_deref(),
        Some("(1,2)")
    );
}

#[test]
fn v192_bit_typmod_paths() {
    let mut eng = engine();
    // Text input into bit via bit_in (also a plain cast).
    assert_eq!(
        cell(&mut eng, "select '010'::bit(3)").as_deref(),
        Some("010")
    );
    // int -> bit is an explicit cast of the two's-complement bits.
    assert_eq!(
        cell(&mut eng, "select 255::int::bit").as_deref(),
        Some("00000000000000000000000011111111")
    );
    // Item conversion enforces the fixed length (22026).
    assert_eq!(
        err_code(
            &mut eng,
            "select json_value(jsonb '\"111\"', '$' returning bit(2) error on error)"
        ),
        "22026"
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_value(jsonb '\"101\"', '$' returning bit(3) error on error)"
        )
        .as_deref(),
        Some("101")
    );
    // A behavior DEFAULT is coerced at analysis time: excess
    // nonzero bits are a hard error; an int DEFAULT fails the
    // assignment-context coercion (42846); an explicitly cast one
    // truncates its zero excess.
    assert_eq!(
        err_code(
            &mut eng,
            "select json_value(jsonb '{}', '$.a' returning bit(3) default b'10101' on empty)"
        ),
        "22026"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "select json_value(jsonb '1234', '$' returning bit(3) default 1 on error)"
        ),
        "42846"
    );
    assert_eq!(
        cell(
            &mut eng,
            "select json_value(jsonb '1234', '$' returning bit(3) default 1::bit(3) on error)"
        )
        .as_deref(),
        Some("001")
    );
    // JSON_VALUE bytea: the item's text bytes.
    assert_eq!(
        cell(
            &mut eng,
            "select json_value(jsonb '123', '$' returning bytea error on error)"
        )
        .as_deref(),
        Some("\\x313233")
    );
}

#[test]
fn v192_mutability_walker() {
    use super::jsonpath::{JpDtStatus, jp_path_is_mutable, parse_jsonpath};
    let m = |p: &str| jp_path_is_mutable(&parse_jsonpath(p).unwrap(), &[]);
    // Plain accessors and bare .datetime() are immutable.
    assert!(!m("$"));
    assert!(!m("$.a[0]"));
    assert!(!m("$.datetime()"));
    assert!(!m("$.a ? (@ < $.datetime())"));
    assert!(!m("$.rtrim()"));
    assert!(!m("$.split_part(\",\", 2)"));
    // The datetime-producing methods are mutable anywhere.
    assert!(m("$.time()"));
    assert!(m("$.date()"));
    assert!(m("$.time_tz()"));
    assert!(m("$.timestamp()"));
    assert!(m("$.timestamp_tz()"));
    assert!(m("$.a ? (@.date() < $.time())"));
    // datetime() comparisons: unknown vs zoned is mutable; two
    // unknown or two same-zoned forms are not; zoned vs non-zoned
    // formats mismatch into mutability.
    assert!(m("$.a ? (@.datetime() < $.datetime(\"HH:MI TZH\"))"));
    // Unknown on either side of a comparison is mutable (PG's
    // walker rule) — format-less .datetime() on both sides too.
    assert!(m("$.a ? (@.datetime() < $.datetime())"));
    assert!(!m(
        "$.a ? (@.datetime(\"HH:MI TZH\") < $.datetime(\"HH:MI TZH\"))"
    ));
    assert!(!m(
        "$.a ? (@.datetime(\"HH:MI\") < $.datetime(\"YY-MM-DD HH:MI\"))"
    ));
    assert!(m(
        "$.a ? (@.datetime(\"HH:MI TZH\") < $.datetime(\"YY-MM-DD HH:MI\"))"
    ));
    // PASSING variable statuses: a timetz var is zoned, a date var
    // is non-zoned, an unbound var is plain.
    let vars = [("x".to_string(), JpDtStatus::Zoned)];
    assert!(!jp_path_is_mutable(
        &parse_jsonpath("$.datetime(\"HH:MI TZH\") < $x").unwrap(),
        &vars
    ));
    assert!(jp_path_is_mutable(
        &parse_jsonpath("$.datetime() < $x").unwrap(),
        &vars
    ));
    let vars = [("x".to_string(), JpDtStatus::NonZoned)];
    assert!(!jp_path_is_mutable(
        &parse_jsonpath("$.datetime(\"YY-MM-DD\") ? (@ == $x)").unwrap(),
        &vars
    ));
}

#[test]
fn v192_index_cluster() {
    let mut eng = engine();
    run(&mut eng, "create table v192_mut (js jsonb, b int)").unwrap();
    // Unnamed expression indexes take uniquified auto-names (the
    // second identical expression no longer collides 42P07).
    run(&mut eng, "create index on v192_mut (json_query(js, '$'))").unwrap();
    run(
        &mut eng,
        "create index on v192_mut (json_query(js, '$.a[0]'))",
    )
    .unwrap();
    // A mutable jsonpath is 42P17; a volatile DEFAULT likewise.
    assert_eq!(
        err_code(
            &mut eng,
            "create index on v192_mut (json_query(js, '$.time()'))"
        ),
        "42P17"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "create index on v192_mut (json_value(js, '$' default random()::int on error))"
        ),
        "42P17"
    );
    run(&mut eng, "drop table v192_mut").unwrap();
}

#[test]
fn v192_record_output_and_unnest() {
    let mut eng = engine();
    run(
        &mut eng,
        "create type v192_rec as (a int, t text, js json, jb jsonb, jsa json[])",
    )
    .unwrap();
    // PG19 record_out doubles quotes inside quoted fields.
    assert_eq!(
        cell(
            &mut eng,
            "select json_query(jsonb '[{\"a\": 1, \"t\": \"aaa\", \"js\": [1, \"2\", {}], \"jb\": {\"x\": [1, \"2\", {}]}}]', '$[0]' returning v192_rec)"
        )
        .as_deref(),
        Some("(1,aaa,\"[1, \"\"2\"\", {}]\",\"{\"\"x\"\": [1, \"\"2\"\", {}]}\",)")
    );
    // record_in accepts the doubled-quote form back.
    assert_eq!(
        cell(
            &mut eng,
            "select ('(1,aaa,\"[1, \"\"2\"\", {}]\",\"{\"\"x\"\": [1, \"\"2\"\", {}]}\",)'::v192_rec).a"
        )
        .as_deref(),
        Some("1")
    );
    // unnest of a record array expands to the composite's columns.
    let rs = rows(
        &mut eng,
        "select * from unnest(json_query(jsonb '[{\"a\": 1, \"t\": \"x\"}, {\"a\": 2}]', '$' returning v192_rec[]))",
    );
    assert_eq!(rs.len(), 2);
    assert_eq!(rs[0][0].as_deref(), Some("1"));
    assert_eq!(rs[0][1].as_deref(), Some("x"));
    assert_eq!(rs[1][0].as_deref(), Some("2"));
    // A composite field that is an array of composites populates.
    run(&mut eng, "create type v192_reca as (reca v192_rec[])").unwrap();
    let rs = rows(
        &mut eng,
        "select * from unnest((json_query(jsonb '{\"reca\": [{\"a\": 3, \"t\": \"z\"}]}', '$' returning v192_reca)).reca)",
    );
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0][0].as_deref(), Some("3"));
    assert_eq!(rs[0][1].as_deref(), Some("z"));
}
