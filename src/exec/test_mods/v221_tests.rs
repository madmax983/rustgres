/// v2.21: the collation subsystem — derivation and conflict rules
/// (parse_collate.c), CREATE/DROP/ALTER COLLATION, view-column
/// collation tracking (view.c 42P16), nondeterministic restrictions
/// (like.c / regc_pg_locale.c / index.c), `collation for`, and the
/// pg_collation / pg_database catalog surface. Ground truth: PG19
/// src/test/regress/sql/collate.sql + collate.icu.utf8.sql +
/// create_view.sql + aggregates.sql expected outputs.
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
    run(eng, sql)
        .map(|_| "OK".to_string())
        .unwrap_or_else(|e| e.code.to_string())
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

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

fn fixture(eng: &mut Engine) {
    ok(eng, "CREATE TABLE t1 (a int, b text COLLATE \"C\")");
    ok(eng, "CREATE TABLE t2 (a int, b text COLLATE \"POSIX\")");
    ok(
        eng,
        "CREATE TABLE t10 (a int, x text COLLATE \"C\", y text COLLATE \"POSIX\")",
    );
    ok(eng, "INSERT INTO t1 VALUES (1, 'abc'), (2, 'bbc')");
    ok(eng, "INSERT INTO t10 VALUES (1, 'a', 'b')");
}

#[test]
fn explicit_conflict_errors_42p21() {
    let mut eng = engine();
    fixture(&mut eng);
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT * FROM t1 WHERE b COLLATE \"C\" >= 'bbc' COLLATE \"POSIX\""
        ),
        "42P21"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT string_agg(x COLLATE \"C\", y COLLATE \"POSIX\") FROM t10"
        ),
        "42P21"
    );
    // Same explicit collation twice is fine.
    ok(
        &mut eng,
        "SELECT * FROM t1 WHERE b COLLATE \"C\" >= 'bbc' COLLATE \"C\"",
    );
}

#[test]
fn implicit_conflict_defers_to_consumer() {
    let mut eng = engine();
    fixture(&mut eng);
    // No consumer: bare concat is fine in PG.
    ok(&mut eng, "SELECT x || y FROM t10");
    // Comparison consumes it: 42P22.
    assert_eq!(err_code(&mut eng, "SELECT * FROM t10 WHERE x = y"), "42P22");
    // Sort/group consumers: 42P21.
    assert_eq!(
        err_code(&mut eng, "SELECT x FROM t10 ORDER BY x || y"),
        "42P21"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT x || y FROM t10 GROUP BY x || y"),
        "42P21"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT DISTINCT x || y FROM t10"),
        "42P21"
    );
    // Aggregate ORDER BY terms are their own sort-group targets.
    assert_eq!(
        err_code(&mut eng, "SELECT array_agg(x ORDER BY x || y) FROM t10"),
        "42P21"
    );
}

#[test]
fn set_operation_rules() {
    let mut eng = engine();
    fixture(&mut eng);
    // UNION ALL defers; ORDER BY resolves and fails 42P22.
    ok(&mut eng, "SELECT b FROM t1 UNION ALL SELECT b FROM t2");
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT b FROM t1 UNION ALL SELECT b FROM t2 ORDER BY 1"
        ),
        "42P22"
    );
    // DISTINCT set ops resolve eagerly: 42P21.
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT b FROM t1 UNION SELECT b FROM t2 ORDER BY 1"
        ),
        "42P21"
    );
    // CTAS over a deferred conflict is the createas error.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE TABLE ct AS SELECT b FROM t1 UNION ALL SELECT b FROM t2"
        ),
        "42P22"
    );
    // Same collation on both sides is fine everywhere.
    ok(
        &mut eng,
        "SELECT b FROM t1 UNION ALL SELECT b FROM t1 ORDER BY 1",
    );
}

#[test]
fn recursive_cte_conflict_42p21() {
    let mut eng = engine();
    fixture(&mut eng);
    assert_eq!(
        err_code(
            &mut eng,
            "WITH RECURSIVE foo(x) AS (SELECT x FROM (VALUES('a' COLLATE \"C\"),('b')) t(x) \
             UNION ALL SELECT (x || 'c') COLLATE \"POSIX\" FROM foo WHERE length(x) < 10) \
             SELECT * FROM foo"
        ),
        "42P21"
    );
    // Agreeing collations recurse fine.
    ok(
        &mut eng,
        "WITH RECURSIVE foo(x) AS (SELECT x FROM (VALUES('a' COLLATE \"C\"),('b')) t(x) \
         UNION ALL SELECT (x || 'c') COLLATE \"C\" FROM foo WHERE length(x) < 3) \
         SELECT * FROM foo",
    );
}

#[test]
fn unknown_collation_42704() {
    let mut eng = engine();
    fixture(&mut eng);
    assert_eq!(err_code(&mut eng, "SELECT 'a' COLLATE \"foo\""), "42704");
    assert_eq!(
        err_code(&mut eng, "SELECT 'a' COLLATE \"foo-x-icu\""),
        "42704"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT 'a' COLLATE \"ja_JP.eucjp\""),
        "42704"
    );
    assert_eq!(
        err_code(&mut eng, "CREATE TABLE bad (x text COLLATE \"nonsense\")"),
        "42704"
    );
    assert_eq!(
        err_code(&mut eng, "CREATE DOMAIN bd AS text COLLATE \"nonsense\""),
        "42704"
    );
    // Plausible catalog names resolve.
    ok(&mut eng, "SELECT 'a' COLLATE \"en_US\"");
    ok(&mut eng, "SELECT 'a' COLLATE \"en-x-icu\"");
    ok(&mut eng, "SELECT 'a' COLLATE \"de_DE\"");
}

#[test]
fn collate_on_noncollatable_42804() {
    let mut eng = engine();
    fixture(&mut eng);
    assert_eq!(err_code(&mut eng, "SELECT 1 COLLATE \"C\""), "42804");
    assert_eq!(
        err_code(&mut eng, "CREATE INDEX i1 ON t1 ((a COLLATE \"POSIX\"))"),
        "42804"
    );
    assert_eq!(
        err_code(&mut eng, "CREATE DOMAIN di AS int COLLATE \"POSIX\""),
        "42804"
    );
    assert_eq!(err_code(&mut eng, "SELECT collation_for(1)"), "42804");
}

#[test]
fn collation_for_values() {
    let mut eng = engine();
    fixture(&mut eng);
    assert_eq!(
        rows(&mut eng, "SELECT collation_for('foo'::text)"),
        vec![vec![s("default")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT collation_for('foo')"),
        vec![vec![None]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT collation_for((SELECT b FROM t1 LIMIT 1))"),
        vec![vec![s("C")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT collation_for(b) FROM t1 LIMIT 1"),
        vec![vec![s("C")]]
    );
}

#[test]
fn create_collation_lifecycle() {
    let mut eng = engine();
    fixture(&mut eng);
    ok(&mut eng, "CREATE COLLATION myc FROM \"C\"");
    assert_eq!(
        err_code(&mut eng, "CREATE COLLATION myc FROM \"C\""),
        "42P06"
    );
    ok(&mut eng, "CREATE COLLATION IF NOT EXISTS myc FROM \"C\"");
    assert_eq!(
        err_code(&mut eng, "CREATE COLLATION bad FROM \"default\""),
        "22023"
    );
    assert_eq!(
        err_code(&mut eng, "CREATE COLLATION bad FROM nonsense"),
        "42704"
    );
    assert_eq!(
        err_code(&mut eng, "CREATE COLLATION bad (provider = builtin)"),
        "22023"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE COLLATION bad (provider = builtin, locale = 'en_US')"
        ),
        "22023"
    );
    ok(
        &mut eng,
        "CREATE COLLATION bc (provider = builtin, locale = 'C.UTF-8')",
    );
    ok(
        &mut eng,
        "CREATE COLLATION ic (provider = icu, locale = 'en')",
    );
    ok(&mut eng, "ALTER COLLATION ic RENAME TO ic2");
    ok(&mut eng, "DROP COLLATION ic2");
    assert_eq!(err_code(&mut eng, "DROP COLLATION ic2"), "42704");
    ok(&mut eng, "DROP COLLATION IF EXISTS ic2");
    // A collation in use by a column cannot be dropped (restrict).
    ok(
        &mut eng,
        "CREATE COLLATION used_c (provider = icu, locale = 'en')",
    );
    ok(&mut eng, "CREATE TABLE uses_c (x text COLLATE used_c)");
    assert_eq!(err_code(&mut eng, "DROP COLLATION used_c"), "2BP01");
}

#[test]
fn icu_validation_level_honored() {
    let mut eng = engine();
    // Default level (warning): both malformed locales CREATE
    // successfully. The error level is covered end-to-end by the
    // v221 wire probe (SET does not run through this harness's
    // execute() path; the GUC snapshot is session-installed).
    ok(
        &mut eng,
        "CREATE COLLATION w1 (provider = icu, locale = 'nonsense-nowhere')",
    );
    ok(
        &mut eng,
        "CREATE COLLATION w2 (provider = icu, locale = '@colStrength=primary;nonsense=yes')",
    );
}

#[test]
fn nondeterministic_restrictions() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE COLLATION ci (provider = icu, locale = 'und@colCaseLevel=true', deterministic = false)",
    );
    ok(&mut eng, "CREATE TABLE tci (x text COLLATE ci)");
    ok(&mut eng, "INSERT INTO tci VALUES ('abc')");
    assert_eq!(
        err_code(&mut eng, "SELECT x FROM tci WHERE x ILIKE 'a%'"),
        "0A000"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT x FROM tci WHERE x SIMILAR TO 'a%'"),
        "0A000"
    );
    assert_eq!(
        err_code(&mut eng, "SELECT x FROM tci WHERE x ~ 'a'"),
        "0A000"
    );
    // Plain LIKE is char-wise and allowed.
    ok(&mut eng, "SELECT x FROM tci WHERE x LIKE 'a%'");
    // Comparisons are allowed under nondeterministic collations.
    ok(&mut eng, "SELECT x FROM tci WHERE x = 'abc'");
}

#[test]
fn like_trailing_escape_22025() {
    let mut eng = engine();
    assert_eq!(err_code(&mut eng, "SELECT 'foox' LIKE 'foo\\'"), "22025");
    // An escaped backslash at the end is a literal, not dangling.
    ok(&mut eng, "SELECT 'foo\\' LIKE 'foo\\\\'");
}

#[test]
fn view_column_collation_tracking() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE vt (a int, d text COLLATE \"C\")");
    ok(&mut eng, "CREATE VIEW v AS SELECT a, d FROM vt");
    // Same collation: replace is fine.
    ok(&mut eng, "CREATE OR REPLACE VIEW v AS SELECT a, d FROM vt");
    // Changed collation: 42P16.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE OR REPLACE VIEW v AS SELECT a, d COLLATE \"POSIX\" FROM vt"
        ),
        "42P16"
    );
    // A view over a deferred set-op conflict cannot be created.
    ok(&mut eng, "CREATE TABLE v2 (b text COLLATE \"POSIX\")");
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE VIEW bv AS SELECT d FROM vt UNION ALL SELECT b FROM v2"
        ),
        "42P22"
    );
    // The view's stored collation propagates to scans of the view.
    assert_eq!(
        rows(&mut eng, "SELECT collation_for(d) FROM v LIMIT 0"),
        Vec::<Vec<Option<String>>>::new()
    );
    ok(&mut eng, "INSERT INTO vt VALUES (1, 'x')");
    assert_eq!(
        rows(&mut eng, "SELECT collation_for(d) FROM v"),
        vec![vec![s("C")]]
    );
}

#[test]
fn hypothetical_set_pair_conflict() {
    let mut eng = engine();
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT rank('adam'::text collate \"C\") within group (order by x collate \"POSIX\") \
             FROM (VALUES ('fred'),('jim')) v(x)"
        ),
        "42P21"
    );
}

#[test]
fn catalog_surface() {
    let mut eng = engine();
    assert_eq!(
        rows(&mut eng, "SELECT getdatabaseencoding()"),
        vec![vec![s("UTF8")]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT datcollate, datlocale FROM pg_database"),
        vec![vec![s("en_US.utf8"), None]]
    );
    // The collate-suite skip guard: at least one ICU row besides
    // 'unicode' must exist.
    let n = rows(
        &mut eng,
        "SELECT count(*) FROM pg_collation WHERE collprovider = 'i' AND collname <> 'unicode'",
    );
    assert!(n[0][0].as_deref().unwrap() != "0");
    ok(
        &mut eng,
        "CREATE COLLATION regc (provider = icu, locale = 'sv', deterministic = false)",
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT collprovider, collisdeterministic FROM pg_collation WHERE collname = 'regc'"
        ),
        vec![vec![s("i"), s("f")]]
    );
}

#[test]
fn domain_collation_flows_to_columns() {
    let mut eng = engine();
    fixture(&mut eng);
    ok(&mut eng, "CREATE DOMAIN dp AS text COLLATE \"POSIX\"");
    ok(&mut eng, "CREATE TABLE dt (v dp)");
    ok(&mut eng, "INSERT INTO dt VALUES ('x')");
    assert_eq!(
        rows(&mut eng, "SELECT collation_for(v) FROM dt"),
        vec![vec![s("POSIX")]]
    );
    // The domain's implicit POSIX conflicts with t1's implicit C at
    // a comparison: 42P22.
    assert_eq!(
        err_code(&mut eng, "SELECT * FROM dt, t1 WHERE dt.v = t1.b"),
        "42P22"
    );
}

#[test]
fn collate_expr_codec_roundtrip() {
    // Regression: the WAL/checkpoint expression codec must round-trip
    // the v2.21 wrappers (a CHECK constraint carrying COLLATE killed
    // recovery with "bad expr head collate" before the decoder arms).
    use crate::sql::{Expr, decode_expr, encode_expr};
    let col = |n: &str| Expr::Column {
        table: None,
        name: n.to_string(),
    };
    let e = Expr::Collate {
        expr: Box::new(col("x")),
        name: "C".to_string(),
    };
    assert_eq!(decode_expr(&encode_expr(&e)).unwrap(), e);
    let e2 = Expr::CollationFor {
        expr: Box::new(col("y")),
    };
    assert_eq!(decode_expr(&encode_expr(&e2)).unwrap(), e2);
}

#[test]
fn partition_bound_with_collate() {
    // Regression: bounds over collated keys (create_table
    // test_part_coll family + LIST key `c collate "C"`).
    let mut eng = engine();
    ok(
        &mut eng,
        "create table pp (a text) partition by range (a collate \"POSIX\")",
    );
    ok(
        &mut eng,
        "create table pc partition of pp for values from ('a' collate \"C\") to ('g')",
    );
    ok(
        &mut eng,
        "create table pc2 partition of pp for values from (name 'm' collate \"C\") to ('s')",
    );
    ok(&mut eng, "INSERT INTO pp VALUES ('c'), ('n')");
    assert_eq!(
        rows(&mut eng, "SELECT a FROM pp ORDER BY 1"),
        vec![vec![s("c")], vec![s("n")]]
    );
    let mut eng2 = engine();
    ok(
        &mut eng2,
        "CREATE TABLE lp (a text, c text) PARTITION BY LIST (c collate \"C\")",
    );
    ok(
        &mut eng2,
        "CREATE TABLE lp1 PARTITION OF lp FOR VALUES IN ('a', 'b')",
    );
    ok(&mut eng2, "INSERT INTO lp VALUES ('1', 'a')");
    assert_eq!(rows(&mut eng2, "SELECT c FROM lp"), vec![vec![s("a")]]);
}

#[test]
fn create_collation_validation_family() {
    let mut eng = engine();
    // Duplicate option -> error (PG: conflicting or redundant options).
    assert!(
        err_code(
            &mut eng,
            "CREATE COLLATION d1 (LC_COLLATE = \"POSIX\", LC_COLLATE = \"NONSENSE\", LC_CTYPE = \"POSIX\")"
        ) != "OK"
    );
    // ICU boolean keyword with a non-boolean value -> error.
    assert!(
        err_code(
            &mut eng,
            "CREATE COLLATION d2 (provider = icu, locale = '@colNumeric=lower')"
        ) != "OK"
    );
    // Malformed ICU rules -> error; well-formed rules pass.
    assert!(
        err_code(
            &mut eng,
            "CREATE COLLATION d3 (provider = icu, locale = '', rules = '!!wrong!!')"
        ) != "OK"
    );
    ok(
        &mut eng,
        "CREATE COLLATION d4 (provider = icu, locale = '', rules = '&a < g')",
    );
    // libc: lc_collate without lc_ctype -> error naming lc_ctype.
    assert!(err_code(&mut eng, "CREATE COLLATION d5 (lc_collate = 'en_US.utf8')") != "OK");
    // libc: bogus locale name -> error.
    assert!(err_code(&mut eng, "CREATE COLLATION d6 (locale = 'nonsense')") != "OK");
    ok(
        &mut eng,
        "CREATE COLLATION d7 (lc_collate = 'en_US.utf8', lc_ctype = 'en_US.utf8')",
    );
}
