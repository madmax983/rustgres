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
    execute(eng, &mut ctx, &stmt)
}

fn ok(sql: &str) -> ExecResult {
    let mut eng = engine();
    run(&mut eng, sql).unwrap()
}

fn err_code(eng: &mut Engine, sql: &str) -> &'static str {
    match run(eng, sql) {
        Ok(_) => panic!("expected error for {sql}"),
        Err(e) => e.code,
    }
}

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } | ExecResult::Explain { rows, .. } => rows
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

/// v0.97: ALTER DOMAIN ADD/DROP CONSTRAINT (named).
#[test]
fn alter_domain_add_drop_constraint() {
    let mut eng = engine();
    run(&mut eng, "CREATE DOMAIN d97 AS int CHECK (VALUE > 0)").unwrap();
    run(
        &mut eng,
        "ALTER DOMAIN d97 ADD CONSTRAINT c_small CHECK (VALUE < 100)",
    )
    .unwrap();
    assert_eq!(err_code(&mut eng, "SELECT '500'::d97"), "23514");
    assert_eq!(
        rows_of(run(&mut eng, "SELECT '5'::d97").unwrap()),
        vec![vec!["5"]]
    );
    run(&mut eng, "ALTER DOMAIN d97 DROP CONSTRAINT c_small").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT '500'::d97").unwrap()),
        vec![vec!["500"]]
    );
    // The original CREATE DOMAIN check still applies.
    assert_eq!(err_code(&mut eng, "SELECT '-1'::d97"), "23514");
}

/// v0.97: unnamed ADD CONSTRAINT auto-names `<domain>_check[N]`;
/// duplicate names are 42710 (PG19).
#[test]
fn alter_domain_constraint_naming() {
    let mut eng = engine();
    run(&mut eng, "CREATE DOMAIN d97n AS int").unwrap();
    run(&mut eng, "ALTER DOMAIN d97n ADD CHECK (VALUE > 0)").unwrap();
    run(&mut eng, "ALTER DOMAIN d97n ADD CHECK (VALUE < 100)").unwrap();
    assert_eq!(err_code(&mut eng, "SELECT '500'::d97n"), "23514");
    // Auto-names d97n_check and d97n_check1.
    run(&mut eng, "ALTER DOMAIN d97n DROP CONSTRAINT d97n_check1").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT '500'::d97n").unwrap()),
        vec![vec!["500"]]
    );
    // Explicit duplicate name is 42710.
    run(
        &mut eng,
        "ALTER DOMAIN d97n ADD CONSTRAINT dup CHECK (VALUE > 1)",
    )
    .unwrap();
    assert_eq!(
        err_code(
            &mut eng,
            "ALTER DOMAIN d97n ADD CONSTRAINT dup CHECK (VALUE > 2)"
        ),
        "42710"
    );
    // Dropping a missing constraint is 42704; IF EXISTS is silent.
    assert_eq!(
        err_code(&mut eng, "ALTER DOMAIN d97n DROP CONSTRAINT nosuch"),
        "42704"
    );
    run(
        &mut eng,
        "ALTER DOMAIN d97n DROP CONSTRAINT IF EXISTS nosuch",
    )
    .unwrap();
}

/// v0.97: ALTER DOMAIN on a missing name is 42704; on a non-domain
/// type is 42809 (PG19).
#[test]
fn alter_domain_missing_and_not_domain() {
    let mut eng = engine();
    assert_eq!(
        err_code(&mut eng, "ALTER DOMAIN nosuch ADD CHECK (VALUE > 0)"),
        "42704"
    );
    run(&mut eng, "CREATE TYPE t97c AS (a int)").unwrap();
    assert_eq!(
        err_code(&mut eng, "ALTER DOMAIN t97c SET NOT NULL"),
        "42809"
    );
}

/// v0.97: SET/DROP NOT NULL — NULL input is 23502 while set (PG19's
/// domain null-violation code).
#[test]
fn alter_domain_set_drop_not_null() {
    let mut eng = engine();
    run(&mut eng, "CREATE DOMAIN d97nn AS int").unwrap();
    run(&mut eng, "ALTER DOMAIN d97nn SET NOT NULL").unwrap();
    assert_eq!(err_code(&mut eng, "SELECT NULL::d97nn"), "23502");
    run(&mut eng, "ALTER DOMAIN d97nn DROP NOT NULL").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT NULL::d97nn").unwrap()),
        vec![vec!["NULL"]]
    );
}

/// v0.97: SET/DROP DEFAULT flows into later table defaults.
#[test]
fn alter_domain_set_drop_default() {
    let mut eng = engine();
    run(&mut eng, "CREATE DOMAIN d97d AS int").unwrap();
    run(&mut eng, "ALTER DOMAIN d97d SET DEFAULT 42").unwrap();
    run(&mut eng, "CREATE TABLE t97d (x d97d)").unwrap();
    run(&mut eng, "INSERT INTO t97d DEFAULT VALUES").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT x FROM t97d").unwrap()),
        vec![vec!["42"]]
    );
    run(&mut eng, "ALTER DOMAIN d97d DROP DEFAULT").unwrap();
    // New tables see no default afterwards.
    run(&mut eng, "CREATE TABLE t97d2 (x d97d)").unwrap();
    run(&mut eng, "INSERT INTO t97d2 (x) VALUES (7)").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT x FROM t97d2").unwrap()),
        vec![vec!["7"]]
    );
}

/// v0.97: domain CHECK expressions may only reference VALUE (42601,
/// same rule as CREATE DOMAIN).
#[test]
fn alter_domain_check_value_only() {
    let mut eng = engine();
    run(&mut eng, "CREATE DOMAIN d97v AS int").unwrap();
    assert_eq!(
        err_code(&mut eng, "ALTER DOMAIN d97v ADD CHECK (other > 0)"),
        "42601"
    );
}

/// v0.97: pg_typeof reports the domain for casts and domain-typed
/// columns (PG19 static typing); base types are unchanged.
#[test]
fn pg_typeof_domain_identity() {
    let mut eng = engine();
    run(&mut eng, "CREATE DOMAIN d97t AS text").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT pg_typeof('x'::d97t)").unwrap()),
        vec![vec!["d97t"]]
    );
    run(&mut eng, "CREATE TABLE t97t (a d97t, b int)").unwrap();
    run(&mut eng, "INSERT INTO t97t VALUES ('x', 1)").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT pg_typeof(a), pg_typeof(b) FROM t97t").unwrap()),
        vec![vec!["d97t", "integer"]]
    );
    // Non-domain behavior unchanged.
    assert_eq!(
        rows_of(run(&mut eng, "SELECT pg_typeof(5)").unwrap()),
        vec![vec!["integer"]]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT pg_typeof(NULL)").unwrap()),
        vec![vec!["unknown"]]
    );
    // PG19 still evaluates the argument: a failing domain cast is
    // 23514, not a type name.
    run(&mut eng, "CREATE DOMAIN d97p AS int CHECK (VALUE > 0)").unwrap();
    assert_eq!(err_code(&mut eng, "SELECT pg_typeof('x'::d97p)"), "22P02");
    assert_eq!(err_code(&mut eng, "SELECT pg_typeof('-5'::d97p)"), "23514");
}

/// v0.97: bounded plpgsql — single-RETURN bodies desugar to SQL;
/// richer bodies are an honest 0A000; other languages stay 42601.

/// v1.03: scalar DECLARE + `:=` + RETURN.
#[test]
fn v103_plpgsql_declare_assign() {
    let mut eng = engine();
    run(
            &mut eng,
            "CREATE FUNCTION add103(a int, b int) RETURNS int AS $$ declare s int; begin s := a + b; return s; end; $$ LANGUAGE plpgsql",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT add103(3, 4)").unwrap()),
        vec![vec!["7"]]
    );
    // Variable references in expressions rewrite to the local slot.
    run(
            &mut eng,
            "CREATE FUNCTION dbl103(x int) RETURNS int AS $$ declare y int; begin y := x * 2; return y + x; end; $$ LANGUAGE plpgsql",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT dbl103(5)").unwrap()),
        vec![vec!["15"]]
    );
}

/// v1.03: `RETURN NEXT` accumulates SETOF rows; loop var binds per row.
#[test]
fn v103_plpgsql_return_next() {
    let mut eng = engine();
    run(
            &mut eng,
            "CREATE FUNCTION gen103(n int) RETURNS SETOF int AS $$ declare i int; begin for i in select * from generate_series(1, n) loop return next i * 10; end loop; end; $$ LANGUAGE plpgsql",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT * FROM gen103(3)").unwrap()),
        vec![vec!["10"], vec!["20"], vec!["30"]]
    );
}

/// v1.03: the conformance target — FOR over EXPLAIN (ANALYZE) with
/// per-row regexp_replace, exact plan shape.
#[test]
fn v103_explain_analyze_setof() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE sq_limit (pk int primary key, c1 int, c2 int)",
    )
    .unwrap();
    run(
            &mut eng,
            "INSERT INTO sq_limit VALUES (1,1,1),(2,2,2),(3,3,3),(4,4,4),(5,1,1),(6,2,2),(7,3,3),(8,4,4)",
        )
        .unwrap();
    run(
            &mut eng,
            "CREATE FUNCTION explain_sq_limit() RETURNS SETOF text LANGUAGE plpgsql AS $$ declare ln text; begin for ln in explain (analyze, summary off, timing off, costs off, buffers off) select * from (select pk,c2 from sq_limit order by c1,pk) as x limit 3 loop ln := regexp_replace(ln, 'Memory: \\S*', 'Memory: xxx'); return next ln; end loop; end; $$",
        )
        .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT * FROM explain_sq_limit()").unwrap());
    assert_eq!(
        rows,
        vec![
            vec!["Limit (actual rows=3.00 loops=1)".to_string()],
            vec!["   ->  Subquery Scan on x (actual rows=3.00 loops=1)".to_string()],
            vec!["         ->  Sort (actual rows=3.00 loops=1)".to_string()],
            vec!["               Sort Key: sq_limit.c1, sq_limit.pk".to_string()],
            vec!["               Sort Method: top-N heapsort  Memory: xxx".to_string()],
            vec!["               ->  Seq Scan on sq_limit (actual rows=8.00 loops=1)".to_string()],
        ]
    );
}

/// v1.03: top-level EXPLAIN (ANALYZE) renders actual rows.
#[test]
fn v103_explain_analyze_toplevel() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE ea103 (a int)").unwrap();
    run(&mut eng, "INSERT INTO ea103 VALUES (1), (2), (3)").unwrap();
    let rows = rows_of(run(&mut eng, "EXPLAIN (ANALYZE) SELECT * FROM ea103").unwrap());
    assert!(
        rows[0][0].contains("actual rows=3.00 loops=1"),
        "{}",
        rows[0][0]
    );
    // Planning-only EXPLAIN has no actual-rows tag.
    let rows = rows_of(run(&mut eng, "EXPLAIN SELECT * FROM ea103").unwrap());
    assert!(!rows[0][0].contains("actual"), "{}", rows[0][0]);
}

/// v1.03: validation errors — RETURN NEXT in scalar, RETURN in SETOF,
/// undeclared assignment target, duplicate declaration.

#[test]
fn v103_plpgsql_validation() {
    let mut eng = engine();
    // RETURN NEXT in a scalar function.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION rn103() RETURNS int AS $$ begin return next 1; end; $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
    // Value RETURN in a SETOF function.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION r103() RETURNS SETOF int AS $$ begin return 1; end; $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
    // Assignment to undeclared variable.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION u103() RETURNS int AS $$ declare x int; begin y := 1; return x; end; $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
    // Duplicate declaration.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION d103() RETURNS int AS $$ declare x int; x text; begin return 1; end; $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
    // Variable colliding with an argument name.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION c103(a int) RETURNS int AS $$ declare a text; begin return 1; end; $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
}
#[test]
fn plpgsql_bounded_subset() {
    let mut eng = engine();
    run(
            &mut eng,
            "CREATE FUNCTION vol97(text) returns text as 'begin return $1; end' language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT vol97('hello')").unwrap()),
        vec![vec!["hello"]]
    );
    // Case-insensitive, whitespace-tolerant.
    run(
            &mut eng,
            "CREATE FUNCTION vol97b(int) returns int as '  BEGIN  RETURN $1 + 1; END  ' language plpgsql",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT vol97b(41)").unwrap()),
        vec![vec!["42"]]
    );
    // v1.03: `:=` is in the subset but requires DECLARE —
    // assignment to an undeclared variable is 42601.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad97() returns int as 'begin x := 1; return 1; end' language plpgsql"
        ),
        "42601"
    );
    // WHILE is still outside the subset: 0A000.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad97w() returns int as 'begin while true loop return 1; end loop; end' language plpgsql"
        ),
        "0A000"
    );
    // v1.32: unknown languages now parse (PG19 gram.y accepts any
    // language name) and fail at CREATE with 42883
    // (proclang.c get_language_oid), not 42601 at parse time.
    let stmt =
        crate::sql::parse_statement("CREATE FUNCTION c97() returns int as 'int f(){}' language c")
            .expect("unknown language should parse");
    assert!(matches!(stmt, crate::sql::Stmt::CreateFunction { .. }));
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION c97() returns int as 'int f(){}' language c"
        ),
        "42883"
    );
}

/// v0.97: desugar_plpgsql_body unit coverage (sql.rs).
#[test]
fn desugar_plpgsql_body_cases() {
    use crate::sql::desugar_plpgsql_body;
    assert_eq!(
        desugar_plpgsql_body("begin return $1; end").unwrap(),
        "SELECT $1"
    );
    assert_eq!(
        desugar_plpgsql_body("BEGIN RETURN 1 + 2; END;").unwrap(),
        "SELECT 1 + 2"
    );
    assert_eq!(
        desugar_plpgsql_body("  begin\n return 'Weekend';\n end ").unwrap(),
        "SELECT 'Weekend'"
    );
    // The word "end" inside the expression must not confuse it.
    assert_eq!(
        desugar_plpgsql_body("begin return weekend; end").unwrap(),
        "SELECT weekend"
    );
    assert!(desugar_plpgsql_body("begin x := 1; return 1; end").is_err());
    assert!(desugar_plpgsql_body("begin return 1; return 2; end").is_err());
    assert!(desugar_plpgsql_body("begin return; end").is_err());
    assert!(desugar_plpgsql_body("select 1").is_err());
    assert!(desugar_plpgsql_body("beginner return 1; end").is_err());
}

/// v1.01: bounded plpgsql statement sequences + EXCEPTION/WHEN
/// (PG19 `pl_exec.c` `exec_stmt_block` semantics).
#[test]
fn plpgsql_exceptions() {
    use crate::sql::{parse_plpgsql_body, plpgsql_condition_matches};
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t101a (a int, b text)").unwrap();
    run(&mut eng, "INSERT INTO t101a VALUES (1, 'x')").unwrap();
    // Multi-statement body with a division_by_zero trap, inverse()
    // style: the target conformance case in miniature.
    run(
            &mut eng,
            "CREATE FUNCTION inv101(int) returns float8 as 'begin analyze t101a; return 1::float8/$1; exception when division_by_zero then return 0; end' language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT inv101(2)").unwrap()),
        vec![vec!["0.5"]]
    );
    assert_eq!(
        rows_of(run(&mut eng, "SELECT inv101(0)").unwrap()),
        vec![vec!["0"]]
    );
    // The ANALYZE utility actually ran: pg_stats has one row per
    // column of the analyzed table (t101a: a, b).
    assert_eq!(
        rows_of(
            run(
                &mut eng,
                "SELECT count(*) FROM pg_stats WHERE tablename = 't101a'"
            )
            .unwrap()
        ),
        vec![vec!["2"]]
    );
    // Untrapped error propagates with its own code.
    run(
            &mut eng,
            "CREATE FUNCTION untrapped101() returns int as 'begin return 1/0; exception when unique_violation then return 9; end' language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(err_code(&mut eng, "SELECT untrapped101()"), "22012");
    // Category condition (22xxx) and OTHERS match 22012.
    run(
            &mut eng,
            "CREATE FUNCTION cat101() returns int as 'begin return 1/0; exception when data_exception then return 7; end' language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT cat101()").unwrap()),
        vec![vec!["7"]]
    );
    run(
            &mut eng,
            "CREATE FUNCTION oth101() returns int as 'begin return 1/0; exception when others then return 9; end' language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT oth101()").unwrap()),
        vec![vec!["9"]]
    );
    // SQLSTATE literal condition.
    run(
            &mut eng,
            "CREATE FUNCTION sq101() returns int as $$begin return 1/0; exception when sqlstate '22012' then return 5; end$$ language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT sq101()").unwrap()),
        vec![vec!["5"]]
    );
    // First matching handler wins.
    run(
            &mut eng,
            "CREATE FUNCTION first101() returns int as 'begin return 1/0; exception when division_by_zero then return 1; when others then return 2; end' language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT first101()").unwrap()),
        vec![vec!["1"]]
    );
    // Errors raised inside a handler propagate untrapped.
    run(
            &mut eng,
            "CREATE FUNCTION herr101() returns int as 'begin return 1/0; exception when division_by_zero then return 1/0; end' language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(err_code(&mut eng, "SELECT herr101()"), "22012");
    // Implicit RETURN NULL when the sequence has no RETURN.
    run(
            &mut eng,
            "CREATE FUNCTION nullret101() returns int as 'begin analyze t101a; end' language plpgsql volatile",
        )
        .unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT nullret101()").unwrap()),
        vec![vec!["NULL"]]
    );
    // v1.03: `:=` is supported but requires DECLARE — undeclared
    // target is 42601 (was 0A000 in v1.01 when `:=` was unsupported).
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad101a() returns int as 'begin x := 1; return 1; end' language plpgsql"
        ),
        "42601"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad101b() returns int as 'begin return 1; return 2; end' language plpgsql"
        ),
        "0A000"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad101c() returns int as 'begin insert into t101a values (2, ''y''); return 1; end' language plpgsql"
        ),
        "0A000"
    );
    // Unknown condition name is 42704; malformed RETURN is 42601.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad101d() returns int as 'begin return 1; exception when nosuchcond then return 0; end' language plpgsql"
        ),
        "42704"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION bad101e() returns int as 'begin return; end' language plpgsql"
        ),
        "42601"
    );
    // Missing EXCEPTION keyword / missing THEN are syntax errors.
    assert!(parse_plpgsql_body("begin return 1; when others then return 2; end", false).is_err());
    assert!(
        parse_plpgsql_body("begin return 1; exception when others return 2; end", false).is_err()
    );
    // Condition matching unit coverage (PG19 pl_exec.c rules).
    // (The parser normalizes OTHERS to the uppercase sentinel.)
    assert!(plpgsql_condition_matches("22012", "22012"));
    assert!(plpgsql_condition_matches("22000", "22012"));
    assert!(plpgsql_condition_matches("OTHERS", "23505"));
    assert!(plpgsql_condition_matches("OTHERS", "22012"));
    // OTHERS does not catch query_canceled (57014) or
    // assert_failure (P0004).
    assert!(!plpgsql_condition_matches("OTHERS", "57014"));
    assert!(!plpgsql_condition_matches("OTHERS", "P0004"));
    assert!(!plpgsql_condition_matches("22000", "23505"));
    // Named conditions resolve through the errcodes table.
    let pb = parse_plpgsql_body(
        "begin return 1; exception when division_by_zero or unique_violation then return 0; end",
        false,
    )
    .unwrap();
    assert_eq!(pb.handlers[0].sqlstates, vec!["22012", "23505"]);
}

/// v0.97: ALTER DOMAIN persists across statements in the same
/// engine (transactional rollback/commit is covered over the wire
/// in protocol_test86).
#[test]
fn alter_domain_persists() {
    let mut eng = engine();
    run(&mut eng, "CREATE DOMAIN d97r AS int CHECK (VALUE > 0)").unwrap();
    run(
        &mut eng,
        "ALTER DOMAIN d97r ADD CONSTRAINT c1 CHECK (VALUE < 10)",
    )
    .unwrap();
    assert_eq!(err_code(&mut eng, "SELECT '50'::d97r"), "23514");
    run(&mut eng, "ALTER DOMAIN d97r DROP CONSTRAINT c1").unwrap();
    assert_eq!(
        rows_of(run(&mut eng, "SELECT '50'::d97r").unwrap()),
        vec![vec!["50"]]
    );
}
