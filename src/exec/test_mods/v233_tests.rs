// v2.33: statistics core — pg_statistic as a real relation over the
// ANALYZE census, the 19-column pg_stats view, pg_class relstats,
// and the PG19 pg_restore_*/pg_clear_* statistics import functions.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).expect("parses");
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

fn scalar_bool(eng: &mut Engine, sql: &str) -> bool {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => {
            assert_eq!(rows.len(), 1, "{sql} -> {rows:?}");
            match &rows[0][0] {
                Value::Bool(b) => *b,
                other => panic!("{sql} -> expected bool, got {other:?}"),
            }
        }
        other => panic!("expected Select for {sql} -> {other:?}"),
    }
}

fn seed(eng: &mut Engine) {
    ok(eng, "CREATE TABLE t (a int, b text)");
    ok(
        eng,
        "INSERT INTO t VALUES (1,'x'),(1,'x'),(1,'y'),(2,'x'),(2,'y'),(3,'z')",
    );
    ok(eng, "ANALYZE t");
}

#[test]
fn v233_pg_statistic_slots_after_analyze() {
    let mut eng = engine();
    seed(&mut eng);
    // One pg_statistic row per analyzed column.
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_statistic s JOIN pg_class c \
             ON c.oid = s.starelid WHERE c.relname = 't'"
        )[0],
        "2"
    );
    // Column a (1x3, 2x2, 3x1): MCV in slot 1, correlation in slot 2,
    // no histogram (a single non-MCV distinct value remains).
    assert_eq!(
        one(
            &mut eng,
            "SELECT s.stakind1, s.stakind2 FROM pg_statistic s \
             JOIN pg_class c ON c.oid = s.starelid \
             JOIN pg_attribute at ON at.attrelid = c.oid AND at.attnum = s.staattnum \
             WHERE c.relname = 't' AND at.attname = 'a'"
        ),
        vec!["1".to_string(), "3".to_string()]
    );
    // stadistinct is populated (PG form: negative fraction here).
    assert!(scalar_bool(
        &mut eng,
        "SELECT s.stadistinct <> 0 FROM pg_statistic s \
         JOIN pg_class c ON c.oid = s.starelid WHERE c.relname = 't' AND s.staattnum = 1"
    ));
    // PG19 pg_statistic has exactly 31 columns (attrelid 2619).
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_attribute WHERE attrelid = 2619"
        )[0],
        "31"
    );
}

#[test]
fn v233_pg_stats_wide_shape() {
    let mut eng = engine();
    seed(&mut eng);
    // The 19-column PG19 pg_stats: the tail columns resolve and the
    // MCV of column a renders.
    let r = one(
        &mut eng,
        "SELECT most_common_vals, correlation, range_bounds_histogram \
         FROM pg_stats WHERE tablename = 't' AND attname = 'a'",
    );
    assert_eq!(r[0], "{1,2,3}");
    assert!(!r[1].is_empty());
    // pg_stats_ext_exprs carries the v2.33 columns (22 in PG19).
    let r = rows_text(
        &mut eng,
        "SELECT correlation, most_common_elems, elem_count_histogram \
         FROM pg_stats_ext_exprs LIMIT 0",
    );
    assert!(r.is_empty());
}

#[test]
fn v233_restore_attribute_roundtrip() {
    let mut eng = engine();
    seed(&mut eng);
    ok(&mut eng, "CREATE TABLE b (a int, bb text)");
    // b was never analyzed: no pg_statistic row.
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_statistic s JOIN pg_class c \
             ON c.oid = s.starelid WHERE c.relname = 'b'"
        )[0],
        "0"
    );
    assert!(scalar_bool(
        &mut eng,
        "SELECT pg_restore_attribute_stats('schemaname', 'public', \
         'relname', 'b', 'attname', 'a', 'inherited', false, \
         'null_frac', 0::real, 'avg_width', 4, 'n_distinct', 2::real, \
         'most_common_vals', '{7,8}', 'most_common_freqs', '{0.75,0.25}'::real[])"
    ));
    // The restored statistics render through pg_stats identically to
    // ANALYZE-built ones.
    let r = one(
        &mut eng,
        "SELECT most_common_vals, most_common_freqs, n_distinct \
         FROM pg_stats WHERE tablename = 'b' AND attname = 'a'",
    );
    assert_eq!(r[0], "{7,8}");
    assert_eq!(r[1], "{0.75,0.25}");
    assert_eq!(r[2], "2");
    // Clear removes the row again.
    ok(
        &mut eng,
        "SELECT pg_clear_attribute_stats('public', 'b', 'a', NULL, false)",
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_statistic s JOIN pg_class c \
             ON c.oid = s.starelid WHERE c.relname = 'b'"
        )[0],
        "0"
    );
}

#[test]
fn v233_restore_relation_stats_and_clear() {
    let mut eng = engine();
    seed(&mut eng);
    assert!(scalar_bool(
        &mut eng,
        "SELECT pg_restore_relation_stats('schemaname', 'public', \
         'relname', 't', 'relpages', 7, 'reltuples', 42::real, \
         'relallvisible', 3, 'relallfrozen', 1)"
    ));
    assert!(scalar_bool(
        &mut eng,
        "SELECT relpages = 7 AND reltuples = 42 AND relallvisible = 3 \
         AND relallfrozen = 1 FROM pg_class WHERE relname = 't'"
    ));
    ok(&mut eng, "SELECT pg_clear_relation_stats('public', 't')");
    // PG clear state for a table: 0 / -1 / 0 / 0.
    assert!(scalar_bool(
        &mut eng,
        "SELECT relpages = 0 AND reltuples = -1 AND relallvisible = 0 \
         AND relallfrozen = 0 FROM pg_class WHERE relname = 't'"
    ));
}

#[test]
fn v233_restore_error_paths() {
    let mut eng = engine();
    seed(&mut eng);
    // Missing required argument -> 22023.
    assert_eq!(
        err(
            &mut eng,
            "SELECT pg_restore_attribute_stats('relname', 't', 'attname', 'a', 'inherited', false)"
        )
        .code,
        "22023"
    );
    // attname and attnum together -> 22023.
    assert_eq!(
        err(
            &mut eng,
            "SELECT pg_restore_attribute_stats('schemaname', 'public', 'relname', 't', \
             'attname', 'a', 'attnum', 1::smallint, 'inherited', false)"
        )
        .code,
        "22023"
    );
    // Missing relation -> 42P01.
    assert_eq!(
        err(
            &mut eng,
            "SELECT pg_restore_relation_stats('schemaname', 'public', 'relname', 'nope')"
        )
        .code,
        "42P01"
    );
    // Unknown argument name -> warning path, returns false.
    assert!(!scalar_bool(
        &mut eng,
        "SELECT pg_restore_relation_stats('schemaname', 'public', 'relname', 't', 'bogus', 1)"
    ));
    // Exact-type mismatch (text for a real) -> warning path, false.
    assert!(!scalar_bool(
        &mut eng,
        "SELECT pg_restore_attribute_stats('schemaname', 'public', 'relname', 't', \
         'attname', 'a', 'inherited', false, 'null_frac', 'x')"
    ));
    // Odd pair count -> error with the PG hint text.
    let e = err(
        &mut eng,
        "SELECT pg_restore_relation_stats('schemaname', 'public', 'relname')",
    );
    assert_eq!(
        e.hint.as_deref(),
        Some("Provide an even number of variadic arguments that can be divided into pairs.")
    );
}

#[test]
fn v233_extended_restore_and_clear() {
    let mut eng = engine();
    seed(&mut eng);
    ok(&mut eng, "CREATE STATISTICS s1 ON a, b FROM t");
    // No ANALYZE-built data was kept for s1 in this engine seed
    // (ANALYZE ran before CREATE STATISTICS), so pg_stats_ext is
    // empty until the restore lands.
    assert_eq!(one(&mut eng, "SELECT count(*) FROM pg_stats_ext")[0], "0");
    assert!(scalar_bool(
        &mut eng,
        "SELECT pg_restore_extended_stats('schemaname', 'public', \
         'relname', 't', 'statistics_schemaname', 'public', \
         'statistics_name', 's1', 'inherited', false, \
         'n_distinct', '[{\"attributes\": [1, 2], \"ndistinct\": 3}]')"
    ));
    assert_eq!(one(&mut eng, "SELECT count(*) FROM pg_stats_ext")[0], "1");
    ok(
        &mut eng,
        "SELECT pg_clear_extended_stats('public', 't', 'public', 's1', false)",
    );
    assert_eq!(one(&mut eng, "SELECT count(*) FROM pg_stats_ext")[0], "0");
}
