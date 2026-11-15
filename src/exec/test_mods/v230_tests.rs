/// v2.30: extended statistics family completion — CREATE STATISTICS
/// kind gates and PG19 error codes, auto-naming, PG-format
/// ndistinct/dependencies payloads, the ANALYZE-side build (incl.
/// SET STATISTICS 0 suppression), DROP COLUMN cleanup, MCV
/// serialization + pg_mcv_list_items, multivariate planner estimates
/// in EXPLAIN ANALYZE (rows= segment), and the regexp_match array
/// fix. Ground truth: PG19 statscmds.c, statistics/{extended_stats,
/// dependencies, mcv, mvdistinct}.c, selfuncs.c, clausesel.c.
/// Scope record: hidden_files/v230/scope.md.
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

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    match run(eng, sql) {
        Ok(_) => panic!("{sql} -> expected error, got success"),
        Err(e) => e,
    }
}

fn rows_text(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) | Ok(ExecResult::Explain { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
            .collect(),
        other => panic!("expected Select for {sql} -> {other:?}"),
    }
}

fn one(eng: &mut Engine, sql: &str) -> Vec<String> {
    let r = rows_text(eng, sql);
    assert_eq!(r.len(), 1, "{sql} -> {r:?}");
    r.into_iter().next().unwrap()
}

/// The EXPLAIN ANALYZE top-line estimated rows for a query.
fn est_rows(eng: &mut Engine, sql: &str) -> i64 {
    let r = rows_text(eng, &format!("EXPLAIN ANALYZE {sql}"));
    let line = &r[0][0];
    let start = line.find("(rows=").expect("rows segment") + 6;
    let rest = &line[start..];
    let end = rest.find(' ').expect("rows terminator");
    rest[..end].parse().expect("numeric rows")
}

#[test]
fn v230_kind_gates_and_errors() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b int, c text, w xid)");
    // Single plain column -> 42P17.
    assert_eq!(
        err(&mut eng, "CREATE STATISTICS s ON a FROM t").code,
        "42P17"
    );
    // Parenthesised single column is still a plain column -> 42P17.
    assert_eq!(
        err(&mut eng, "CREATE STATISTICS s ON (a) FROM t").code,
        "42P17"
    );
    // Single expression with explicit kinds -> 0A000.
    assert_eq!(
        err(
            &mut eng,
            "CREATE STATISTICS s (ndistinct) ON (a + b) FROM t"
        )
        .code,
        "0A000"
    );
    // Single expression without kinds is legal; stxkind is {e}.
    ok(&mut eng, "CREATE STATISTICS s1 ON (a + b) FROM t");
    assert_eq!(
        one(
            &mut eng,
            "SELECT stxkind FROM pg_statistic_ext WHERE stxname = 's1'"
        )[0],
        "{e}"
    );
    // Unparenthesised expression is a grammar error (42601).
    assert_eq!(
        err(&mut eng, "CREATE STATISTICS s ON a + b FROM t").code,
        "42601"
    );
    // Duplicate column / expression -> 42701.
    assert_eq!(
        err(&mut eng, "CREATE STATISTICS s ON a, b, a FROM t").code,
        "42701"
    );
    assert_eq!(
        err(&mut eng, "CREATE STATISTICS s ON (a + b), (a + b) FROM t").code,
        "42701"
    );
    // System columns, plain and inside an expression -> 0A000.
    assert_eq!(
        err(&mut eng, "CREATE STATISTICS s ON tableoid, a FROM t").code,
        "0A000"
    );
    assert_eq!(
        err(
            &mut eng,
            "CREATE STATISTICS s ON (tableoid::int + 1) FROM t"
        )
        .code,
        "0A000"
    );
    // xid has no default btree operator class -> 0A000 multivariate.
    let e = err(&mut eng, "CREATE STATISTICS s (ndistinct) ON a, w FROM t");
    assert_eq!(e.code, "0A000");
    assert!(
        e.message
            .contains("cannot create multivariate statistics on column \"w\"")
    );
    // Default kinds for 2+ items are {d,f,m}.
    ok(&mut eng, "CREATE STATISTICS s2 ON a, b FROM t");
    assert_eq!(
        one(
            &mut eng,
            "SELECT stxkind FROM pg_statistic_ext WHERE stxname = 's2'"
        )[0],
        "{d,f,m}"
    );
}

#[test]
fn v230_auto_naming() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE mytab (a int, b int)");
    ok(&mut eng, "CREATE STATISTICS ON a, b FROM mytab");
    assert_eq!(
        one(&mut eng, "SELECT stxname FROM pg_statistic_ext")[0],
        "mytab_a_b_stat"
    );
    // Collision: the suffix is appended directly (stat1).
    ok(&mut eng, "CREATE STATISTICS ON a, b FROM mytab");
    let names: Vec<String> = rows_text(&mut eng, "SELECT stxname FROM pg_statistic_ext ORDER BY 1")
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(names, vec!["mytab_a_b_stat", "mytab_a_b_stat1"]);
}

#[test]
fn v230_ndistinct_payload_format() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE nd (a int, b int, c int)");
    // a and b fully correlated (10 combos), c independent (5).
    ok(
        &mut eng,
        "INSERT INTO nd SELECT mod(i, 10), mod(i, 10), mod(i / 10, 5) FROM generate_series(1, 1000) s(i)",
    );
    ok(
        &mut eng,
        "CREATE STATISTICS snd (ndistinct) ON a, b, c FROM nd",
    );
    // No data row before ANALYZE.
    assert_eq!(
        rows_text(&mut eng, "SELECT * FROM pg_statistic_ext_data").len(),
        0
    );
    ok(&mut eng, "ANALYZE nd");
    let payload = one(&mut eng, "SELECT stxdndistinct FROM pg_statistic_ext_data")[0].clone();
    // PG19 mvdistinct text: JSON array, subsets of size >= 2 in
    // lexicographic attribute order, exact census counts.
    assert_eq!(
        payload,
        "[{\"attributes\": [1, 2], \"ndistinct\": 10}, \
         {\"attributes\": [1, 3], \"ndistinct\": 50}, \
         {\"attributes\": [2, 3], \"ndistinct\": 50}, \
         {\"attributes\": [1, 2, 3], \"ndistinct\": 50}]"
    );
}

#[test]
fn v230_dependencies_payload_and_estimate() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE fd (a int, b text)");
    // b is a function of a (degree 1 dependency a -> b).
    ok(
        &mut eng,
        "INSERT INTO fd SELECT mod(i, 100), mod(i, 50) FROM generate_series(1, 5000) s(i)",
    );
    ok(
        &mut eng,
        "CREATE STATISTICS sfd (dependencies) ON a, b FROM fd",
    );
    ok(&mut eng, "ANALYZE fd");
    let payload = one(
        &mut eng,
        "SELECT stxddependencies FROM pg_statistic_ext_data",
    )[0]
    .clone();
    // PG19 mvdependencies text is the JSON form.
    assert_eq!(
        payload,
        "[{\"attributes\": [1], \"dependency\": 2, \"degree\": 1.000000}]"
    );
    // With the dependency, a = 1 fully determines b: estimate is the
    // a-selectivity alone (50), not the independence product (1).
    assert_eq!(
        est_rows(&mut eng, "SELECT * FROM fd WHERE a = 1 AND b = '1'"),
        50
    );
}

#[test]
fn v230_mcv_estimate_and_srf_roundtrip() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE mv (a int, b int)");
    // Perfectly correlated: (0,0) x100, (1,1) x100, (2,2) x800.
    ok(
        &mut eng,
        "INSERT INTO mv SELECT CASE WHEN i <= 100 THEN 0 WHEN i <= 200 THEN 1 ELSE 2 END, \
         CASE WHEN i <= 100 THEN 0 WHEN i <= 200 THEN 1 ELSE 2 END \
         FROM generate_series(1, 1000) s(i)",
    );
    ok(&mut eng, "CREATE STATISTICS smv (mcv) ON a, b FROM mv");
    ok(&mut eng, "ANALYZE mv");
    // MCV estimate: a=0 AND b=0 matches one item at frequency 0.1.
    assert_eq!(
        est_rows(&mut eng, "SELECT * FROM mv WHERE a = 0 AND b = 0"),
        100
    );
    // A combination absent from the MCV list falls back small.
    assert!(est_rows(&mut eng, "SELECT * FROM mv WHERE a = 0 AND b = 1") < 100);
    // stxdmcv is stored (bytea hex rendering) and pg_mcv_list_items
    // decodes it: index is 0-based, frequencies sum to 1 here.
    let items = rows_text(
        &mut eng,
        "SELECT index, frequency FROM pg_mcv_list_items(\
         (SELECT stxdmcv FROM pg_statistic_ext_data)) ORDER BY index",
    );
    assert_eq!(items.len(), 3);
    assert_eq!(items[0][0], "0");
    let total: f64 = items.iter().map(|r| r[1].parse::<f64>().unwrap()).sum();
    assert!((total - 1.0).abs() < 1e-9, "freq total {total}");
}

#[test]
fn v230_set_statistics_zero_suppresses_build() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE zt (a int, b int)");
    ok(
        &mut eng,
        "INSERT INTO zt SELECT mod(i, 10), mod(i, 10) FROM generate_series(1, 100) s(i)",
    );
    ok(&mut eng, "CREATE STATISTICS sz (ndistinct) ON a, b FROM zt");
    ok(&mut eng, "ALTER STATISTICS sz SET STATISTICS 0");
    ok(&mut eng, "ANALYZE zt");
    // Object-level target 0: no data row is built.
    assert_eq!(
        rows_text(&mut eng, "SELECT * FROM pg_statistic_ext_data").len(),
        0
    );
    // Column-level target 0 makes the whole object unbuildable too.
    ok(
        &mut eng,
        "CREATE STATISTICS sz2 (ndistinct) ON a, b FROM zt",
    );
    ok(&mut eng, "ALTER TABLE zt ALTER COLUMN a SET STATISTICS 0");
    ok(&mut eng, "ANALYZE zt");
    assert_eq!(
        rows_text(&mut eng, "SELECT * FROM pg_statistic_ext_data").len(),
        0
    );
}

#[test]
fn v230_drop_column_drops_statistics() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE dc (a int, b int, c int)");
    ok(
        &mut eng,
        "INSERT INTO dc SELECT i, i, i FROM generate_series(1, 50) s(i)",
    );
    ok(
        &mut eng,
        "CREATE STATISTICS sdc (ndistinct) ON a, b FROM dc",
    );
    ok(
        &mut eng,
        "CREATE STATISTICS sdc2 (ndistinct) ON b, c FROM dc",
    );
    ok(&mut eng, "ANALYZE dc");
    assert_eq!(
        rows_text(&mut eng, "SELECT stxname FROM pg_statistic_ext ORDER BY 1").len(),
        2
    );
    // Dropping b removes BOTH objects entirely (PG semantics).
    ok(&mut eng, "ALTER TABLE dc DROP COLUMN b");
    assert_eq!(
        rows_text(&mut eng, "SELECT stxname FROM pg_statistic_ext").len(),
        0
    );
    assert_eq!(
        rows_text(&mut eng, "SELECT * FROM pg_statistic_ext_data").len(),
        0
    );
}

#[test]
fn v230_explain_analyze_rows_segment() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE ea (a int)");
    ok(
        &mut eng,
        "INSERT INTO ea SELECT mod(i, 10) FROM generate_series(1, 1000) s(i)",
    );
    ok(&mut eng, "ANALYZE ea");
    // The estimated-rows segment precedes the actual segment, with a
    // trailing space (check_estimated_rows' regex needs it).
    let r = rows_text(&mut eng, "EXPLAIN ANALYZE SELECT * FROM ea WHERE a = 1");
    assert!(
        r[0][0].contains("(rows=100 ) (actual rows=100.00"),
        "line: {}",
        r[0][0]
    );
    // Multi-table VACUUM ANALYZE is legal PG19 grammar (covered
    // over the wire by the stats_ext suite; VACUUM itself goes
    // through the session layer, not this harness).
    assert!(crate::sql::parse_statement("VACUUM ANALYZE ea, eb").is_ok());
}

#[test]
fn v230_regexp_match_returns_array() {
    let mut eng = engine();
    // regexp_match must yield a real text[] so subscripting works
    // (check_estimated_rows depends on this inside plpgsql).
    assert_eq!(
        one(
            &mut eng,
            "SELECT (regexp_match('abc123def', '([0-9]+)'))[1]"
        )[0],
        "123"
    );
    assert_eq!(
        one(&mut eng, "SELECT (regexp_split_to_array('a,b,c', ','))[2]")[0],
        "b"
    );
}
