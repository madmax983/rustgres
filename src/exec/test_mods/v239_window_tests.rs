// ---------------------------------------------------------------------------
// v2.39: window functions remainder — named windows, RESPECT/IGNORE
// NULLS, frame bound expressions, percent_rank/cume_dist, and the
// var_samp/stddev_samp window column names.
// ---------------------------------------------------------------------------
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    // Preserve the parser's SQLSTATE (e.g. 42P20), like the main
    // test harness does.
    let stmt = crate::sql::parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
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

fn cols_of(r: ExecResult) -> Vec<String> {
    match r {
        ExecResult::Select { columns, .. } => columns.into_iter().map(|c| c.0).collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn err_of(eng: &mut Engine, sql: &str) -> ExecError {
    run(eng, sql).unwrap_err()
}

/// emp: three departments, salaries with a NULL in the middle.
fn emp_engine() -> Engine {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE emp(dep text, name text, sal int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO emp VALUES \
         ('eng','a',100),('eng','b',NULL),('eng','c',300),('eng','d',200),\
         ('ops','e',50),('ops','f',NULL),('ops','g',150)",
    )
    .unwrap();
    eng
}

fn q(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    rows_of(run(eng, sql).unwrap())
}

fn col(rows: &[Vec<String>], i: usize) -> Vec<String> {
    rows.iter().map(|r| r[i].clone()).collect()
}

// --- 1. named windows -------------------------------------------------

#[test]
fn v239_named_window_bare_and_paren() {
    let mut eng = emp_engine();
    // Bare OVER w.
    let rows = q(
        &mut eng,
        "SELECT name, rank() OVER w FROM emp WINDOW w AS (PARTITION BY dep ORDER BY sal) ORDER BY dep, sal",
    );
    assert_eq!(col(&rows, 1), vec!["1", "2", "3", "4", "1", "2", "3"]);
    // OVER (w) with nothing added — inherits everything (the named
    // window has no frame clause, so the parenthesized form is fine).
    let rows = q(
        &mut eng,
        "SELECT name, rank() OVER (w) FROM emp WINDOW w AS (PARTITION BY dep ORDER BY sal) ORDER BY dep, sal",
    );
    assert_eq!(col(&rows, 1), vec!["1", "2", "3", "4", "1", "2", "3"]);
    // OVER () empty parens spec.
    let rows = q(&mut eng, "SELECT count(*) OVER () FROM emp");
    assert_eq!(col(&rows, 0), vec!["7", "7", "7", "7", "7", "7", "7"]);
}

#[test]
fn v239_named_window_merge_order_by() {
    let mut eng = emp_engine();
    // The referenced window supplies PARTITION BY; the OVER spec
    // adds an ORDER BY the referenced window lacks.
    let rows = q(
        &mut eng,
        "SELECT name, row_number() OVER (w ORDER BY sal) FROM emp \
         WINDOW w AS (PARTITION BY dep) ORDER BY dep, sal",
    );
    assert_eq!(col(&rows, 1), vec!["1", "2", "3", "4", "1", "2", "3"]);
}

#[test]
fn v239_named_window_override_errors() {
    let mut eng = emp_engine();
    // PARTITION BY may never be restated.
    let e = err_of(
        &mut eng,
        "SELECT rank() OVER (w PARTITION BY name) FROM emp WINDOW w AS (PARTITION BY dep)",
    );
    assert_eq!(e.code, "42P20");
    assert!(
        e.message
            .contains("cannot override PARTITION BY clause of window \"w\""),
        "{}",
        e.message
    );
    // ORDER BY may be supplied only when the referenced window has none.
    let e = err_of(
        &mut eng,
        "SELECT rank() OVER (w ORDER BY name) FROM emp WINDOW w AS (ORDER BY sal)",
    );
    assert_eq!(e.code, "42P20");
    assert!(
        e.message
            .contains("cannot override ORDER BY clause of window \"w\""),
        "{}",
        e.message
    );
    // Duplicate definition.
    let e = err_of(
        &mut eng,
        "SELECT rank() OVER w FROM emp WINDOW w AS (ORDER BY sal), w AS (ORDER BY name)",
    );
    assert_eq!(e.code, "42P20");
    assert!(
        e.message.contains("window \"w\" is already defined"),
        "{}",
        e.message
    );
    // Unknown name (PG19 ERRCODE_UNDEFINED_OBJECT).
    let e = err_of(&mut eng, "SELECT rank() OVER nope FROM emp");
    assert_eq!(e.code, "42704");
    assert!(
        e.message.contains("window \"nope\" does not exist"),
        "{}",
        e.message
    );
    // A framed window cannot be copied by a parenthesized OVER.
    let e = err_of(
        &mut eng,
        "SELECT sum(sal) OVER (w) FROM emp \
         WINDOW w AS (ORDER BY sal ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)",
    );
    assert_eq!(e.code, "42P20");
    assert!(
        e.message
            .contains("cannot copy window \"w\" because it has a frame clause"),
        "{}",
        e.message
    );
    // ... but the bare form inherits the frame.
    let rows = q(
        &mut eng,
        "SELECT name, sum(sal) OVER w FROM emp \
         WINDOW w AS (ORDER BY sal ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) ORDER BY sal, name",
    );
    // sal order: 50,100,150,200,300,NULL(b),NULL(f); running pairs,
    // with the all-NULL frame of the last row summing to NULL.
    assert_eq!(
        col(&rows, 1),
        vec!["50", "150", "250", "350", "500", "300", "NULL"]
    );
}

#[test]
fn v239_named_window_in_order_by() {
    // PG19 window.sql's empsalary shape: the named-window reference
    // appears BOTH in the target list and in the statement's ORDER
    // BY. The ORDER BY copy is parsed by the query tail (after the
    // core captured the WINDOW clause) and must resolve against the
    // same definitions — regression: the top-level query finisher
    // parsed the tail into a fresh statement with no WINDOW clause,
    // so this failed with `window "w" does not exist` (42704).
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE empsalary(depname text, empno int, salary int)",
    )
    .unwrap();
    run(
        &mut eng,
        "INSERT INTO empsalary VALUES \
         ('develop',11,5200),('develop',7,4200),('develop',9,4500),\
         ('develop',8,6000),('develop',10,5200),('personnel',5,3500),\
         ('personnel',2,3900),('sales',3,4800),('sales',1,5000),\
         ('sales',4,4800)",
    )
    .unwrap();
    let rows = q(
        &mut eng,
        "SELECT depname, empno, salary, rank() OVER w FROM empsalary \
         WINDOW w AS (PARTITION BY depname ORDER BY salary) \
         ORDER BY rank() OVER w, salary",
    );
    // Sorted by (rank, salary): rank 1 salaries 3500,4200,4800,4800;
    // rank 2: 3900,4500; rank 3: 5000,5200,5200; rank 5: 6000. The
    // tied rows share their salary, so both columns are exact.
    assert_eq!(
        col(&rows, 3),
        vec!["1", "1", "1", "1", "2", "2", "3", "3", "3", "5"]
    );
    assert_eq!(
        col(&rows, 2),
        vec![
            "3500", "4200", "4800", "4800", "3900", "4500", "5000", "5200", "5200", "6000"
        ]
    );
    // A parenthesized ORDER BY reference that restates ORDER BY
    // against a window that already has one is the same 42P20 as in
    // the target list (resolution reached the merge, not 42704).
    let e = err_of(
        &mut eng,
        "SELECT rank() OVER w FROM empsalary \
         WINDOW w AS (PARTITION BY depname ORDER BY salary) \
         ORDER BY rank() OVER (w ORDER BY salary)",
    );
    assert_eq!(e.code, "42P20");
    // An unknown window in ORDER BY is still 42704.
    let e = err_of(
        &mut eng,
        "SELECT salary FROM empsalary ORDER BY rank() OVER nope",
    );
    assert_eq!(e.code, "42704");
    assert!(
        e.message.contains("window \"nope\" does not exist"),
        "{}",
        e.message
    );
}

#[test]
fn v239_named_window_chained_definition() {
    let mut eng = emp_engine();
    // A definition may reference an earlier definition.
    let rows = q(
        &mut eng,
        "SELECT name, rank() OVER w2 FROM emp \
         WINDOW w1 AS (PARTITION BY dep), w2 AS (w1 ORDER BY sal) ORDER BY dep, sal",
    );
    assert_eq!(col(&rows, 1), vec!["1", "2", "3", "4", "1", "2", "3"]);
}

// --- 2. RESPECT / IGNORE NULLS ----------------------------------------

#[test]
fn v239_ignore_nulls_lag_lead() {
    let mut eng = emp_engine();
    // eng partition by sal order: NULL(b), 100(a), 200(d), 300(c).
    let rows = q(
        &mut eng,
        "SELECT name, lag(sal) IGNORE NULLS OVER w, lead(sal) IGNORE NULLS OVER w \
         FROM emp WINDOW w AS (PARTITION BY dep ORDER BY sal) \
         ORDER BY dep, sal NULLS FIRST",
    );
    // Output order is dep, sal NULLS FIRST: b, a, d, c, f, e, g.
    // The window's own ORDER BY sal ASC sorts NULLS LAST, so inside
    // each partition b/f are LAST: their lag is the previous non-NULL
    // salary and their lead is NULL.
    assert_eq!(
        rows,
        vec![
            vec!["b", "300", "NULL"],
            vec!["a", "NULL", "200"],
            vec!["d", "100", "300"],
            vec!["c", "200", "NULL"],
            vec!["f", "150", "NULL"],
            vec!["e", "NULL", "150"],
            vec!["g", "50", "NULL"],
        ]
        .into_iter()
        .map(|r| r.into_iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .collect::<Vec<_>>()
    );
    // RESPECT NULLS is the plain behavior (immediate neighbor,
    // NULL or not).
    let rows = q(
        &mut eng,
        "SELECT name, lag(sal) RESPECT NULLS OVER w FROM emp \
         WINDOW w AS (PARTITION BY dep ORDER BY sal) ORDER BY dep, sal NULLS FIRST",
    );
    assert_eq!(
        col(&rows, 1),
        vec!["300", "NULL", "100", "200", "150", "NULL", "50"]
    );
}

#[test]
fn v239_ignore_nulls_first_last_nth() {
    let mut eng = emp_engine();
    let rows = q(
        &mut eng,
        "SELECT name, \
                first_value(sal) IGNORE NULLS OVER w, \
                last_value(sal) IGNORE NULLS OVER w, \
                nth_value(sal, 2) IGNORE NULLS OVER w \
         FROM emp WINDOW w AS (PARTITION BY dep ORDER BY sal \
                               ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
         ORDER BY dep, sal NULLS FIRST",
    );
    // Window order in each partition is sal ASC (NULLS LAST):
    // eng: a(100), d(200), c(300), b(NULL); ops: e(50), g(150), f(NULL).
    // Output order is dep, sal NULLS FIRST: b, a, d, c, f, e, g.
    assert_eq!(
        col(&rows, 1),
        vec!["300", "100", "100", "200", "150", "50", "50"]
    );
    assert_eq!(
        col(&rows, 2),
        vec!["300", "200", "300", "300", "150", "150", "150"]
    );
    assert_eq!(
        col(&rows, 3),
        vec!["NULL", "200", "200", "300", "NULL", "150", "150"]
    );
}

#[test]
fn v239_null_treatment_rejections() {
    let mut eng = emp_engine();
    // Aggregates reject null treatment, plain and windowed (PG19 42809).
    let e = err_of(&mut eng, "SELECT sum(sal) IGNORE NULLS FROM emp");
    assert_eq!(e.code, "42809");
    let e = err_of(&mut eng, "SELECT sum(sal) IGNORE NULLS OVER () FROM emp");
    assert_eq!(e.code, "42809");
    // Non-window functions reject it (PG19 42809).
    let e = err_of(&mut eng, "SELECT abs(1) IGNORE NULLS");
    assert_eq!(e.code, "42809");
    assert!(
        e.message.contains("is not a window function"),
        "{}",
        e.message
    );
    // Window functions outside the supported five reject it at
    // execution (PG19 0A000), RESPECT included.
    let e = err_of(
        &mut eng,
        "SELECT row_number() IGNORE NULLS OVER () FROM emp",
    );
    assert_eq!(e.code, "0A000");
    assert!(
        e.message
            .contains("function row_number does not allow RESPECT/IGNORE NULLS"),
        "{}",
        e.message
    );
    let e = err_of(&mut eng, "SELECT rank() RESPECT NULLS OVER () FROM emp");
    assert_eq!(e.code, "0A000");
}

// --- 3. frame bound expressions ---------------------------------------

#[test]
fn v239_frame_offset_cast_expr() {
    let mut eng = emp_engine();
    // PG's own case: casts on the offset (`2::int8 PRECEDING`).
    let rows = q(
        &mut eng,
        "SELECT name, sum(sal) OVER (ORDER BY sal ROWS BETWEEN 2::int8 PRECEDING AND CURRENT ROW) \
         FROM emp ORDER BY sal NULLS LAST, name",
    );
    // Ordered by sal: 50,100,150,200,300,NULL(b),NULL(f); sum skips
    // NULL inputs, so b's frame (200,300,NULL) is 500 and f's
    // (300,NULL,NULL) is 300.
    assert_eq!(
        col(&rows, 1),
        vec!["50", "150", "300", "450", "650", "500", "300"]
    );
}

#[test]
fn v239_frame_offset_subquery_and_arith() {
    let mut eng = emp_engine();
    let rows = q(
        &mut eng,
        "SELECT name, sum(sal) OVER (ORDER BY sal ROWS BETWEEN (SELECT 1) + 1 PRECEDING AND CURRENT ROW) \
         FROM emp ORDER BY sal NULLS LAST, name",
    );
    assert_eq!(
        col(&rows, 1),
        vec!["50", "150", "300", "450", "650", "500", "300"]
    );
}

#[test]
fn v239_frame_offset_huge_following_saturates() {
    let mut eng = emp_engine();
    // 9223372036854775807 FOLLOWING must not overflow/panic.
    let rows = q(
        &mut eng,
        "SELECT count(*) OVER (ORDER BY sal ROWS BETWEEN CURRENT ROW AND 9223372036854775807 FOLLOWING) \
         FROM emp ORDER BY sal NULLS LAST, name",
    );
    assert_eq!(col(&rows, 0), vec!["7", "6", "5", "4", "3", "2", "1"]);
    let rows = q(
        &mut eng,
        "SELECT count(*) OVER (ORDER BY sal ROWS BETWEEN 9223372036854775807 PRECEDING AND CURRENT ROW) \
         FROM emp ORDER BY sal NULLS LAST, name",
    );
    assert_eq!(col(&rows, 0), vec!["1", "2", "3", "4", "5", "6", "7"]);
}

#[test]
fn v239_frame_offset_errors() {
    let mut eng = emp_engine();
    // NULL offset: 22004.
    let e = err_of(
        &mut eng,
        "SELECT sum(sal) OVER (ORDER BY sal ROWS BETWEEN NULL PRECEDING AND CURRENT ROW) FROM emp",
    );
    assert_eq!(e.code, "22004");
    assert!(
        e.message.contains("frame starting offset must not be null"),
        "{}",
        e.message
    );
    // Negative offset: 22013.
    let e = err_of(
        &mut eng,
        "SELECT sum(sal) OVER (ORDER BY sal ROWS BETWEEN -1 PRECEDING AND CURRENT ROW) FROM emp",
    );
    assert_eq!(e.code, "22013");
    assert!(
        e.message
            .contains("frame starting offset must not be negative"),
        "{}",
        e.message
    );
    // A column reference in the offset: 42P10 (PG checkExprIsVarFree).
    let e = err_of(
        &mut eng,
        "SELECT sum(sal) OVER (ORDER BY sal ROWS BETWEEN sal PRECEDING AND CURRENT ROW) FROM emp",
    );
    assert_eq!(e.code, "42P10");
    assert!(
        e.message
            .contains("argument of ROWS must not contain variables"),
        "{}",
        e.message
    );
}

#[test]
fn v239_range_interval_offset() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE ev(d date, v int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO ev VALUES ('2024-01-01',1),('2024-01-02',2),('2024-01-05',4),('2024-01-20',8)",
    )
    .unwrap();
    // interval '...' form and the bare string-literal form PG coerces.
    for off in ["interval '3 days'", "'3 days'"] {
        let rows = q(
            &mut eng,
            &format!(
                "SELECT v, sum(v) OVER (ORDER BY d RANGE BETWEEN {off} PRECEDING AND {off} FOLLOWING) \
                 FROM ev ORDER BY d"
            ),
        );
        assert_eq!(col(&rows, 1), vec!["3", "7", "6", "8"], "offset {off}");
    }
    // Timestamp keys with an interval offset.
    run(&mut eng, "CREATE TABLE evt(t timestamp, v int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO evt VALUES ('2024-01-01 00:00',1),('2024-01-01 12:00',2),('2024-01-03 00:00',4)",
    )
    .unwrap();
    let rows = q(
        &mut eng,
        "SELECT v, sum(v) OVER (ORDER BY t RANGE BETWEEN interval '1 day' PRECEDING AND interval '1 day' FOLLOWING) \
         FROM evt ORDER BY t",
    );
    assert_eq!(col(&rows, 1), vec!["3", "3", "4"]);
}

#[test]
fn v239_range_offset_null_keys_peer_group() {
    let mut eng = engine();
    // PG19 window.sql: RANGE offset frames for NULL order keys are
    // the NULL peer group.
    let rows = q(
        &mut eng,
        "SELECT x, first_value(y) OVER w, last_value(y) OVER w \
         FROM (SELECT 1 AS x, 1 AS y UNION ALL SELECT 2, 2 UNION ALL SELECT NULL, 42 UNION ALL SELECT NULL, 43) ss \
         WINDOW w AS (ORDER BY x ASC NULLS FIRST RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING) \
         ORDER BY x NULLS FIRST",
    );
    assert_eq!(col(&rows, 1), vec!["42", "42", "1", "1"]);
    assert_eq!(col(&rows, 2), vec!["43", "43", "2", "2"]);
}

#[test]
fn v239_frame_offset_function_arg() {
    // PG19 window.sql's mutator test: a frame offset that references
    // a SQL function's argument (`group_size FOLLOWING`) — the arg
    // reference inside the offset must survive body rewriting.
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION f(group_size bigint) RETURNS SETOF int[] \
         AS $$ SELECT array_agg(s) OVER w FROM generate_series(1,5) s \
               WINDOW w AS (ORDER BY s ROWS BETWEEN CURRENT ROW AND group_size FOLLOWING) $$ \
         LANGUAGE SQL STABLE",
    )
    .unwrap();
    let rows = q(&mut eng, "SELECT f(2)");
    assert_eq!(
        col(&rows, 0),
        vec!["{1,2,3}", "{2,3,4}", "{3,4,5}", "{4,5}", "{5}"]
    );
}

// --- 4. percent_rank / cume_dist ---------------------------------------

#[test]
fn v239_percent_rank_cume_dist_over() {
    let mut eng = emp_engine();
    let rows = q(
        &mut eng,
        "SELECT name, percent_rank() OVER w, cume_dist() OVER w \
         FROM emp WINDOW w AS (PARTITION BY dep ORDER BY sal) ORDER BY dep, sal",
    );
    // eng (4 rows, distinct sals): pr 0, 1/3, 2/3, 1; cd .25,.5,.75,1
    // ops (3 rows): pr 0, .5, 1; cd 1/3, 2/3, 1
    let pr: Vec<String> = col(&rows, 1);
    let cd: Vec<String> = col(&rows, 2);
    let expect_pr = [
        "0",
        "0.3333333333333333",
        "0.6666666666666666",
        "1",
        "0",
        "0.5",
        "1",
    ];
    let expect_cd = [
        "0.25",
        "0.5",
        "0.75",
        "1",
        "0.3333333333333333",
        "0.6666666666666666",
        "1",
    ];
    for (got, want) in pr.iter().zip(expect_pr.iter()) {
        let g: f64 = got.parse().unwrap();
        let w: f64 = want.parse().unwrap();
        assert!((g - w).abs() < 1e-9, "percent_rank {got} != {want}");
    }
    for (got, want) in cd.iter().zip(expect_cd.iter()) {
        let g: f64 = got.parse().unwrap();
        let w: f64 = want.parse().unwrap();
        assert!((g - w).abs() < 1e-9, "cume_dist {got} != {want}");
    }
    // WITHIN GROUP still routes to the hypothetical-set aggregate.
    let rows = q(
        &mut eng,
        "SELECT percent_rank(200) WITHIN GROUP (ORDER BY sal) FROM emp",
    );
    assert_eq!(rows.len(), 1);
}

// --- 5. column naming ---------------------------------------------------

#[test]
fn v239_window_agg_written_names() {
    let mut eng = emp_engine();
    let r = run(
        &mut eng,
        "SELECT var_samp(sal) OVER (), stddev_samp(sal) OVER (), variance(sal) OVER (), stddev(sal) OVER () FROM emp",
    )
    .unwrap();
    assert_eq!(
        cols_of(r),
        vec!["var_samp", "stddev_samp", "variance", "stddev"]
    );
}

#[test]
fn v239_named_window_in_having_and_distinct_on() {
    let mut eng = emp_engine();
    // HAVING audit: a known reference RESOLVES in the core walk —
    // the failure is the executor's PG-matching 42803 rejection of
    // window functions in HAVING, not a spurious 42704.
    let e = err_of(
        &mut eng,
        "SELECT dep, count(*) FROM emp GROUP BY dep \
         HAVING rank() OVER w >= 1 WINDOW w AS (PARTITION BY dep)",
    );
    assert_eq!(e.code, "42803");
    assert!(
        e.message
            .contains("window functions are not allowed in HAVING"),
        "{}",
        e.message
    );
    // An unknown reference in HAVING is still caught by the walk.
    let e = err_of(
        &mut eng,
        "SELECT dep FROM emp GROUP BY dep HAVING rank() OVER nope > 1",
    );
    assert_eq!(e.code, "42704");
    assert!(
        e.message.contains("window \"nope\" does not exist"),
        "{}",
        e.message
    );
    // DISTINCT ON audit: the reference in the DISTINCT ON expression
    // itself resolves and executes. eng ranks by sal (NULLS LAST):
    // a=1, d=2, c=3, b=4; ORDER BY rank, dep keeps eng's row first
    // for each rank, so DISTINCT ON (rank) yields exactly those.
    let rows = q(
        &mut eng,
        "SELECT DISTINCT ON (rank() OVER w) rank() OVER w AS r, dep, name FROM emp \
         WINDOW w AS (PARTITION BY dep ORDER BY sal) ORDER BY rank() OVER w, dep",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "eng", "a"],
            vec!["2", "eng", "d"],
            vec!["3", "eng", "c"],
            vec!["4", "eng", "b"],
        ]
    );
    // An unknown reference in DISTINCT ON is still 42704.
    let e = err_of(
        &mut eng,
        "SELECT DISTINCT ON (rank() OVER nope) dep FROM emp ORDER BY rank() OVER nope",
    );
    assert_eq!(e.code, "42704");
    assert!(
        e.message.contains("window \"nope\" does not exist"),
        "{}",
        e.message
    );
}
