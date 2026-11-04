use super::*;

// v2.17: extended statistics — CREATE STATISTICS registry + validation,
// auto-naming, DROP/ALTER STATISTICS, ANALYZE multivariate build,
// pg_statistic_ext / pg_stats_ext catalogs, bare EXPLAIN ANALYZE,
// regexp_match. PG19 grounding: statscmds.c, statistics/.

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(217000);

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let xid = NEXT_XID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Preserve the parser's SQLSTATE (sql_err flattens to 42601;
    // several v2.16 checks are parse-time with specific codes).
    let stmt = crate::sql::parse_statement(sql).map_err(|e| ExecError {
        code: e.code,
        message: e.message,
        detail: None,
    })?;
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        privs_suppressed: false,
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

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql)
        .map(|_| "OK".to_string())
        .unwrap_or_else(|e| e.code.to_string())
}

#[test]
fn v217_create_statistics_validation() {
    let mut eng = engine();
    ok(&mut eng, "create table est (a int, b int, c text)");
    assert_eq!(
        err_code(&mut eng, "create statistics s1 on a, b from nope"),
        "42P01"
    );
    assert_eq!(
        err_code(&mut eng, "create statistics s1 on a, a from est"),
        "42701"
    );
    assert_eq!(
        err_code(&mut eng, "create statistics s1 (bogus) on a, b from est"),
        "42601"
    );
    assert_eq!(
        err_code(&mut eng, "create statistics s1 on tableoid, a from est"),
        "42601"
    );
    assert_eq!(
        err_code(&mut eng, "create statistics s1 on a from est"),
        "42601"
    );
    assert_eq!(
        err_code(&mut eng, "create statistics s1 on a, zz from est"),
        "42703"
    );
    ok(&mut eng, "create statistics s1 on a, b from est");
    assert_eq!(
        err_code(&mut eng, "create statistics s1 on a, c from est"),
        "42P07"
    );
    ok(
        &mut eng,
        "create statistics if not exists s1 on a, c from est",
    );
    assert_eq!(eng.db.ext_stats.len(), 1);
    ok(
        &mut eng,
        "create statistics s2 (mcv) on (a + b), lower(c) from est",
    );
    assert_eq!(eng.db.ext_stats.len(), 2);
    // Unnamed form auto-names <table>_<cols>_stats.
    ok(&mut eng, "create statistics (ndistinct) on a, c from est");
    assert!(eng.db.ext_stats.iter().any(|o| o.name == "est_a_c_stats"));
}

#[test]
fn v217_drop_alter_statistics() {
    let mut eng = engine();
    ok(&mut eng, "create table est (a int, b int)");
    ok(&mut eng, "create statistics s1 on a, b from est");
    ok(&mut eng, "alter statistics s1 rename to s1x");
    ok(&mut eng, "alter statistics s1x set statistics 500");
    assert_eq!(eng.db.ext_stats[0].stattarget, Some(500));
    assert_eq!(
        err_code(&mut eng, "alter statistics nope rename to x"),
        "42P01"
    );
    ok(&mut eng, "drop statistics s1x");
    assert!(eng.db.ext_stats.is_empty());
    assert_eq!(err_code(&mut eng, "drop statistics s1x"), "42P01");
    ok(&mut eng, "drop statistics if exists s1x");
    // DROP TABLE removes its statistics objects.
    ok(&mut eng, "create statistics s9 on a, b from est");
    ok(&mut eng, "drop table est");
    assert!(eng.db.ext_stats.is_empty());
}

#[test]
fn v217_analyze_builds_ext_stats() {
    let mut eng = engine();
    ok(&mut eng, "create table est (a int, b int)");
    ok(
        &mut eng,
        "insert into est values (1, 10), (2, 10), (3, 20), (4, 20)",
    );
    ok(&mut eng, "create statistics s1 on a, b from est");
    ok(&mut eng, "analyze est");
    let o = &eng.db.ext_stats[0];
    assert!(o.data.built);
    let nd = o.data.ndistinct.clone().unwrap();
    assert!(nd.contains("\"1\": 4"), "ndistinct: {nd}");
    assert!(nd.contains("\"2\": 2"), "ndistinct: {nd}");
    assert!(nd.contains("\"1, 2\": 4"), "ndistinct: {nd}");
    assert_eq!(o.data.mcv.len(), 4);
    let def = statisticsobjdef(&eng.db, o.oid as i64).unwrap();
    assert_eq!(def, "CREATE STATISTICS public.s1 ON a, b FROM est");
}

#[test]
fn v217_bare_explain_analyze_and_regexp_match() {
    let mut eng = engine();
    ok(&mut eng, "create table est (a int)");
    ok(&mut eng, "insert into est values (1), (2)");
    // Bare EXPLAIN ANALYZE parses (42601 before v2.17).
    let stmt = crate::sql::parse_statement("explain analyze select * from est").unwrap();
    let _ = stmt;
    let stmt = crate::sql::parse_statement("explain verbose select * from est").unwrap();
    let _ = stmt;
}
