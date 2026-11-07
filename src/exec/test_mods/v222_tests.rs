/// v2.22: the collation-completion tail — plpgsql function-body
/// collation propagation (call-site argument collations assigned
/// to parameters; declared variable collations; 42P22 at a body
/// comparison whose sides derive different non-default
/// collations) and the SQL/JSON DEFAULT behavior checks
/// (transformJsonBehavior: 42804 form / column-reference rules,
/// 42P21 collation-vs-RETURNING conflict). Ground truth: PG19
/// collate.icu.utf8.sql mylt2 family + JSON_VALUE domain family,
/// sqljson_queryfuncs.sql DEFAULT-form family, parse_expr.c
/// transformJsonBehavior, parse_collate.c assignment semantics.
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

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql) {
        Ok(_) => panic!("{sql} -> expected error, got success"),
        Err(e) => e.code.to_string(),
    }
}

const MYLT2_V2: &str = "CREATE OR REPLACE FUNCTION mylt2 (x text, y text) \
    RETURNS boolean LANGUAGE plpgsql AS $$ declare xx text COLLATE \"POSIX\" := x; \
    yy text := y; begin return xx < yy; end $$";

#[test]
fn plpgsql_decl_collate_is_captured() {
    let decls = crate::sql::parse_plpgsql_decls("xx text COLLATE \"POSIX\" := x; yy text := y")
        .expect("decls parse");
    assert_eq!(decls.len(), 2);
    assert_eq!(decls[0].collation.as_deref(), Some("POSIX"));
    assert_eq!(decls[0].type_name, "text");
    assert_eq!(decls[1].collation, None);
    // Unquoted names fold like identifiers.
    let decls2 =
        crate::sql::parse_plpgsql_decls("zz text collate case_insensitive").expect("decls parse");
    assert_eq!(decls2[0].collation.as_deref(), Some("case_insensitive"));
}

#[test]
fn plpgsql_body_collation_conflict_errors_42p22() {
    let mut eng = engine();
    ok(&mut eng, MYLT2_V2);
    assert_eq!(
        err_code(&mut eng, "SELECT mylt2('a', 'B' collate \"C\")"),
        "42P22"
    );
}

#[test]
fn plpgsql_body_collation_default_and_matching_args_pass() {
    let mut eng = engine();
    ok(&mut eng, MYLT2_V2);
    // No call-site collation: the default yields to POSIX.
    ok(&mut eng, "SELECT mylt2('a', 'B')");
    // Agreeing collation: no conflict.
    ok(&mut eng, "SELECT mylt2('a', 'B' collate \"POSIX\")");
}

#[test]
fn plpgsql_body_no_declared_collation_no_conflict() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE FUNCTION mylt3 (x text, y text) RETURNS boolean LANGUAGE plpgsql \
         AS $$ declare xx text := x; yy text := y; begin return xx < yy; end $$",
    );
    // Both sides inherit from the same argument pair; xx takes x's
    // (absent) collation, yy takes y's "C" — only one non-default
    // collation at the comparison, so no conflict.
    ok(&mut eng, "SELECT mylt3('a', 'B' collate \"C\")");
}

#[test]
fn json_value_default_domain_collation_conflict_42p21() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE COLLATION case_insensitive (provider = icu, locale = '@colStrength=secondary', deterministic = false)",
    );
    ok(
        &mut eng,
        "CREATE DOMAIN d1 AS text COLLATE case_insensitive",
    );
    ok(&mut eng, "CREATE DOMAIN d2 AS text COLLATE \"C\"");
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON_VALUE('{\"a\": \"A\"}', '$.a' RETURNING d1 DEFAULT 'C'::d2 ON EMPTY)"
        ),
        "42P21"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON_VALUE('{\"a\": \"A\"}', '$.a' RETURNING d1 DEFAULT 'C' COLLATE \"C\" ON EMPTY)"
        ),
        "42P21"
    );
    // Plain default (no collation) and an outer COLLATE matching
    // the RETURNING domain's collation are both fine.
    ok(
        &mut eng,
        "SELECT JSON_VALUE('{\"a\": \"A\"}', '$.a' RETURNING d1 DEFAULT 'C' ON EMPTY)",
    );
    ok(
        &mut eng,
        "SELECT JSON_VALUE('{\"a\": \"A\"}', '$.a' RETURNING d1 DEFAULT ('C' COLLATE \"C\") COLLATE case_insensitive ON EMPTY)",
    );
}

#[test]
fn json_query_default_subquery_form_42804() {
    let mut eng = engine();
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON_QUERY('\"a\"', '$.a' RETURNING int[] DEFAULT (SELECT '{1}')::text COLLATE \"C\" ON ERROR)"
        ),
        "42804"
    );
}

#[test]
fn json_default_column_reference_errors() {
    // PG19 reports this family as 42804 (parse_expr.c
    // transformJsonBehavior); the engine's pre-existing dispatch
    // check (v1.85 analyze_sqljson_defaults) reports it as 42601
    // and fires first — a pre-existing code divergence, recorded
    // in hidden_files/v222/scope.md, not changed by v2.22. The
    // v2.22 collate.rs check carries the PG-exact 42804 for shapes
    // the dispatch walk does not reach.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE jt (a text)");
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT JSON_VALUE('{\"a\": 1}', '$.a' RETURNING int DEFAULT a ON EMPTY) FROM jt"
        ),
        "42601"
    );
}
