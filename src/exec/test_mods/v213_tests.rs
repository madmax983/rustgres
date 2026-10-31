use super::*;

// v2.13: record-returning / SETOF-record SRF column-definition lists
// in FROM, WITH ORDINALITY, ROWS FROM(...), multi-argument unnest(),
// scalar user functions in FROM, the CREATE-time return-shape
// validator for declared result rowtypes (OUT params / RETURNS
// TABLE / named composites), and plpgsql RETURN QUERY for SETOF
// record functions. PG19 grounding: parse_relation.c
// addRangeTableEntryForFunction, parse_clause.c
// transformRangeFunction, funcapi.c unnamed-OUT naming,
// executor/functions.c check_sql_fn_retval.
// Harness shape copied from v212 (each statement its own xid,
// session 0, never commits).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(95000);

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let xid = NEXT_XID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stmt = crate::sql::parse_statement(sql).expect("parses");
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        snap: &snap,
        own: xid,
        write_xid: xid,
        all_xids: vec![xid],
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

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn err_code(eng: &mut Engine, sql: &str) -> &'static str {
    match run(eng, sql) {
        Ok(_) => panic!("expected error for {sql}"),
        Err(e) => e.code,
    }
}

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql).unwrap() {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected select for {sql}: {other:?}"),
    }
}

fn colnames(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap() {
        ExecResult::Select { columns, .. } => columns.iter().map(|(n, _)| n.clone()).collect(),
        other => panic!("expected select for {sql}: {other:?}"),
    }
}

#[test]
fn v213_record_srf_coldef_list() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE FUNCTION rf213(int) RETURNS SETOF record AS $$ SELECT $1, $1::text $$ LANGUAGE sql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM rf213(42) AS p(a int, b text)"),
        vec![vec!["42", "42"]]
    );
    assert_eq!(
        colnames(&mut eng, "SELECT * FROM rf213(42) AS p(a int, b text)"),
        vec!["a", "b"]
    );
    // No column definition list: 42601 (required for record).
    assert_eq!(err_code(&mut eng, "SELECT * FROM rf213(42)"), "42601");
    assert_eq!(
        err_code(&mut eng, "SELECT * FROM rf213(42) AS p(a, b)"),
        "42601"
    );
    // Width mismatch against the produced row: 42804.
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT * FROM rf213(42) AS p(a int, b text, c int)"
        ),
        "42804"
    );
    // Multi-row SETOF record body.
    ok(
        &mut eng,
        "CREATE FUNCTION rf213m(int) RETURNS SETOF record AS $$ SELECT $1, $1::text UNION ALL SELECT $1+1, 'x' $$ LANGUAGE sql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM rf213m(3) AS t(a int, b text)"),
        vec![vec!["3", "3"], vec!["4", "x"]]
    );
}

#[test]
fn v213_coldef_class_errors() {
    let mut eng = engine();
    // Typed coldef on a non-record builtin: 42601 only-allowed.
    assert_eq!(
        err_code(&mut eng, "SELECT * FROM generate_series(1,3) AS t(a int)"),
        "42601"
    );
    // Named composite return: coldef is redundant.
    ok(&mut eng, "CREATE TYPE ct213 AS (x int, y text)");
    ok(
        &mut eng,
        "CREATE FUNCTION cf213(int) RETURNS ct213 AS $$ SELECT $1, 'z' $$ LANGUAGE sql",
    );
    assert_eq!(
        err_code(&mut eng, "SELECT * FROM cf213(1) AS t(a int, b text)"),
        "42601"
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM cf213(1) AS t(a, b)"),
        vec![vec!["1", "z"]]
    );
    assert_eq!(colnames(&mut eng, "SELECT * FROM cf213(1)"), vec!["x", "y"]);
    // OUT-parameter function: coldef is redundant (OUT class).
    ok(
        &mut eng,
        "CREATE FUNCTION of213(int, OUT a int, OUT b text) AS $$ SELECT $1, 'z' $$ LANGUAGE sql",
    );
    assert_eq!(
        err_code(&mut eng, "SELECT * FROM of213(1) AS t(a int, b text)"),
        "42601"
    );
    // RETURNS TABLE is the OUT class too.
    ok(
        &mut eng,
        "CREATE FUNCTION rtv213(int) RETURNS TABLE(a int, b text) AS $$ SELECT $1, 'x' $$ LANGUAGE sql",
    );
    assert_eq!(
        err_code(&mut eng, "SELECT * FROM rtv213(1) AS t(x int, y text)"),
        "42601"
    );
}

#[test]
fn v213_create_validator_declared_rowtype() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TYPE ct213v AS (x int, y text)");
    // Scalar record function with field-shaped body: now legal.
    ok(
        &mut eng,
        "CREATE FUNCTION rf213v(int) RETURNS record AS $$ SELECT $1, $1::text $$ LANGUAGE sql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM rf213v(42) AS p(a int, b text)"),
        vec![vec!["42", "42"]]
    );
    // Named composite with a whole-tuple final statement: legal.
    ok(
        &mut eng,
        "CREATE FUNCTION okc213(int) RETURNS ct213v AS $$ SELECT ROW($1, 'z') $$ LANGUAGE sql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM okc213(5)"),
        vec![vec!["5", "z"]]
    );
    // Too few / too many columns against the declared rowtype: 42P13.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad213a(int) RETURNS ct213v AS $$ SELECT $1 $$ LANGUAGE sql"
        ),
        "42P13"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad213b(int) RETURNS ct213v AS $$ SELECT $1, 'a', 3 $$ LANGUAGE sql"
        ),
        "42P13"
    );
    // OUT-parameter count mismatch: 42P13.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad213c(int, OUT a int, OUT b text) AS $$ SELECT $1 $$ LANGUAGE sql"
        ),
        "42P13"
    );
    // Unnamed OUT parameter is named column{ordinal} (funcapi.c).
    ok(
        &mut eng,
        "CREATE OR REPLACE FUNCTION unu213(IN f1 int, OUT f2 int, OUT text) AS $$ SELECT $1-1, $1::text || 'z' $$ LANGUAGE sql",
    );
    assert_eq!(
        colnames(&mut eng, "SELECT * FROM unu213(42)"),
        vec!["f2", "column2"]
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM unu213(42)"),
        vec![vec!["41", "42z"]]
    );
}

#[test]
fn v213_with_ordinality() {
    let mut eng = engine();
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM generate_series(1,3) WITH ORDINALITY AS t(a, o)"
        ),
        vec![vec!["1", "1"], vec!["2", "2"], vec!["3", "3"]]
    );
    assert_eq!(
        colnames(
            &mut eng,
            "SELECT * FROM generate_series(1,3) WITH ORDINALITY"
        ),
        vec!["generate_series", "ordinality"]
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM unnest(ARRAY['a','b']) WITH ORDINALITY AS z(a, ord)"
        ),
        vec![vec!["a", "1"], vec!["b", "2"]]
    );
    // Ordinality inside a JOIN ... ON (the rangefuncs shape).
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM (VALUES (1),(2)) v(x) JOIN LATERAL generate_series(1, x) \
             WITH ORDINALITY AS g(v, o) ON true ORDER BY x, v"
        ),
        vec![
            vec!["1", "1", "1"],
            vec!["2", "1", "1"],
            vec!["2", "2", "2"]
        ]
    );
    // Ordinality + column definition list: 42601.
    ok(
        &mut eng,
        "CREATE FUNCTION rfo213(int) RETURNS SETOF record AS $$ SELECT $1 $$ LANGUAGE sql",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT * FROM rfo213(1) WITH ORDINALITY AS t(a int)"
        ),
        "42601"
    );
}

#[test]
fn v213_rows_from() {
    let mut eng = engine();
    // Zip semantics: max length, NULL padding.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM ROWS FROM (generate_series(1,2), generate_series(10,30,10)) AS z(a, b)"
        ),
        vec![vec!["1", "10"], vec!["2", "20"], vec!["NULL", "30"]]
    );
    // Per-part column aliases and per-part coldefs.
    ok(
        &mut eng,
        "CREATE FUNCTION rfr213(int) RETURNS SETOF record AS $$ SELECT $1, $1::text $$ LANGUAGE sql",
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM ROWS FROM (rfr213(1) AS (a int, b text), generate_series(1,2) AS (g))"
        ),
        vec![vec!["1", "1", "1"], vec!["NULL", "NULL", "2"]]
    );
    // WITH ORDINALITY over the zip.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM ROWS FROM (generate_series(1,2), generate_series(10,20,10)) \
             WITH ORDINALITY AS z(a, b, ord)"
        ),
        vec![vec!["1", "10", "1"], vec!["2", "20", "2"]]
    );
    // Top-level coldef on multiple functions: 42601.
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT * FROM ROWS FROM (generate_series(1,2), generate_series(1,2)) AS z(a int, b int)"
        ),
        "42601"
    );
}

#[test]
fn v213_multi_arg_unnest_and_scalar_in_from() {
    let mut eng = engine();
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM unnest(ARRAY[10,20], ARRAY['foo','bar']) AS t(a, b)"
        ),
        vec![vec!["10", "foo"], vec!["20", "bar"]]
    );
    // Unequal lengths pad with NULL.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM unnest(ARRAY[1,2,3], ARRAY['a']) AS t(a, b)"
        ),
        vec![vec!["1", "a"], vec!["2", "NULL"], vec!["3", "NULL"]]
    );
    // Scalar user function in FROM: one column named for the function.
    ok(
        &mut eng,
        "CREATE FUNCTION si213(int) RETURNS int AS $$ SELECT $1 $$ LANGUAGE sql",
    );
    assert_eq!(rows(&mut eng, "SELECT * FROM si213(3)"), vec![vec!["3"]]);
    assert_eq!(colnames(&mut eng, "SELECT * FROM si213(3)"), vec!["si213"]);
    assert_eq!(
        rows(&mut eng, "SELECT * FROM si213(3) AS t(a)"),
        vec![vec!["3"]]
    );
}

#[test]
fn v213_plpgsql_setof_record_return_query() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE FUNCTION rfp213(int) RETURNS SETOF record AS $$ BEGIN RETURN QUERY SELECT $1, $1::text; END $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM rfp213(7) AS p(a int, b text)"),
        vec![vec!["7", "7"]]
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT * FROM rfp213(7) AS p(a int, b text, c int)"
        ),
        "42804"
    );
    // Multiple RETURN QUERY statements accumulate.
    ok(
        &mut eng,
        "CREATE FUNCTION rfp213m(int) RETURNS SETOF record AS $$ BEGIN RETURN QUERY SELECT $1, $1::text; RETURN QUERY SELECT $1+1, 'y'; END $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM rfp213m(7) AS p(a int, b text)"),
        vec![vec!["7", "7"], vec!["8", "y"]]
    );
}

#[test]
fn v213_json_record_coldefs() {
    let mut eng = engine();
    // Single-field coldef: the column takes the DECLARED name, not
    // the table alias (the finish is authoritative for json SRFs).
    assert_eq!(
        colnames(
            &mut eng,
            "SELECT * FROM json_to_record('{\"ia\":[1,2]}') AS x(ia int[])"
        ),
        vec!["ia"]
    );
    // Underscore array spelling in a coldef.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM json_to_record('{\"ia\":[1,2]}') AS x(ia _int4)"
        ),
        vec![vec!["{1,2}"]]
    );
    // Nested-array field value with matching key.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT * FROM json_to_record('{\"ia2\":[[1,2],[3,4]]}') AS x(ia2 int[][])"
        ),
        vec![vec!["{{1,2},{3,4}}"]]
    );
    // json record function without a coldef: 42601.
    assert_eq!(
        err_code(&mut eng, "SELECT * FROM json_to_record('{\"a\":1}')"),
        "42601"
    );
}

#[test]
fn v213_tail_fixes_from_schedule_diff() {
    let mut eng = engine();
    // Whole-tuple parameter: a composite-typed $1 IS the result
    // (the indexing suite's row_image shape).
    ok(&mut eng, "CREATE TABLE t213w (a int, b text)");
    ok(
        &mut eng,
        "CREATE OR REPLACE FUNCTION row_image213(t213w) RETURNS t213w AS $$ SELECT $1 $$ LANGUAGE sql IMMUTABLE",
    );
    // But a SQL body returning too few fields for a named
    // composite still fails CREATE (42P13).
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad213w(int) RETURNS t213w AS $$ SELECT $1 $$ LANGUAGE sql"
        ),
        "42P13"
    );
    // plpgsql bodies are NOT subject to check_sql_fn_retval: a
    // single-RETURN desugared body creating against a composite
    // return type succeeds (the plpgsql suite's compos shape).
    ok(&mut eng, "CREATE TYPE ct213w AS (x int, y int)");
    ok(
        &mut eng,
        "CREATE OR REPLACE FUNCTION compos213() RETURNS ct213w AS $$ BEGIN RETURN 1 + 1; END; $$ LANGUAGE plpgsql",
    );
    // OR REPLACE cannot rename an input parameter (pg_proc.c).
    ok(
        &mut eng,
        "CREATE FUNCTION rn213(a varchar = 'def a', OUT _a varchar, c numeric = NULL, OUT _c numeric) RETURNS record AS $$ SELECT $1, $2 $$ LANGUAGE sql",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE OR REPLACE FUNCTION rn213(a varchar = 'def a', OUT _a varchar, x numeric = NULL, OUT _c numeric) RETURNS record AS $$ SELECT $1, $2 $$ LANGUAGE sql"
        ),
        "42P13"
    );
    // Same-name replace (default change only) is fine.
    ok(
        &mut eng,
        "CREATE OR REPLACE FUNCTION rn213(a varchar = 'def a', OUT _a varchar, c numeric = 5, OUT _c numeric) RETURNS record AS $$ SELECT $1, $2 $$ LANGUAGE sql",
    );
    // Assignment coercion to text is universal (IO conversion):
    // a SETOF rowtype function whose body yields text[] lands the
    // rendering in the text column (the rowsecurity rls_f shape).
    ok(&mut eng, "CREATE TABLE rls213 (c text)");
    ok(&mut eng, "INSERT INTO rls213 VALUES ('invisible to bob')");
    ok(
        &mut eng,
        "CREATE OR REPLACE FUNCTION rls_f213() RETURNS SETOF rls213 STABLE LANGUAGE sql AS $$ SELECT * FROM (SELECT array_agg(c) AS cs FROM rls213) _ GROUP BY cs $$",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM rls_f213()"),
        vec![vec!["{\"invisible to bob\"}"]]
    );
    // NOT NULL domain fields are checked by json_populate_record
    // even when the key is absent (23502), not just on explicit
    // JSON null.
    ok(&mut eng, "CREATE DOMAIN d213nn AS int NOT NULL");
    ok(&mut eng, "CREATE TYPE t213nn AS (i d213nn)");
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT i FROM json_populate_record(NULL::t213nn, '{\"x\": 43.2}') q"
        ),
        "23502"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT i FROM json_populate_record(NULL::t213nn, '{\"i\": null}') q"
        ),
        "23502"
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT i FROM json_populate_record(NULL::t213nn, '{\"i\": 12345}') q"
        ),
        vec![vec!["12345"]]
    );
}
