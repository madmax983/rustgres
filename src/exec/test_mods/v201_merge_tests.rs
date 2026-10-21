use super::*;

// v2.01: MERGE — parser, executor (matched / not-matched-by-target /
// not-matched-by-source), cardinality, privileges, RETURNING with
// merge_action()/old/new, EXPLAIN.

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(5000);

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_as(eng, sql, "postgres")
}

fn run_as(eng: &mut Engine, sql: &str, role: &str) -> Result<ExecResult, ExecError> {
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
        role,
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    execute(eng, &mut ctx, &stmt)
}

fn setup(eng: &mut Engine) {
    run(eng, "CREATE TABLE target (tid integer, balance integer)").unwrap();
    run(eng, "CREATE TABLE source (sid integer, delta integer)").unwrap();
    run(eng, "INSERT INTO target VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    run(eng, "INSERT INTO source VALUES (2, 5), (3, 20), (4, 40)").unwrap();
}

fn rows_of(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql).expect("select works") {
        ExecResult::Select { rows, .. } | ExecResult::Dml { rows, .. } => rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| v.to_text().unwrap_or_else(|| "NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn tag_of(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql).expect("merge works") {
        ExecResult::Dml { tag, .. } => tag,
        other => panic!("expected Dml, got {other:?}"),
    }
}

#[test]
fn v201_merge_upsert() {
    let mut eng = engine();
    setup(&mut eng);
    let tag = tag_of(
        &mut eng,
        "MERGE INTO target t USING source s ON t.tid = s.sid \
         WHEN MATCHED THEN UPDATE SET balance = t.balance + s.delta \
         WHEN NOT MATCHED THEN INSERT VALUES (s.sid, s.delta)",
    );
    assert_eq!(tag, "MERGE 3");
    assert_eq!(
        rows_of(&mut eng, "SELECT * FROM target ORDER BY tid"),
        vec![
            vec!["1", "10"],
            vec!["2", "25"],
            vec!["3", "50"],
            vec!["4", "40"]
        ]
    );
}

#[test]
fn v201_merge_delete_and_by_source() {
    let mut eng = engine();
    setup(&mut eng);
    let tag = tag_of(
        &mut eng,
        "MERGE INTO target t USING source s ON t.tid = s.sid \
         WHEN MATCHED THEN DELETE WHEN NOT MATCHED BY SOURCE THEN DO NOTHING",
    );
    assert_eq!(tag, "MERGE 2");
    assert_eq!(
        rows_of(&mut eng, "SELECT * FROM target ORDER BY tid"),
        vec![vec!["1", "10"]]
    );
    let tag = tag_of(
        &mut eng,
        "MERGE INTO target t USING source s ON t.tid = s.sid \
         WHEN NOT MATCHED BY SOURCE THEN DELETE",
    );
    assert_eq!(tag, "MERGE 1");
    assert_eq!(
        rows_of(&mut eng, "SELECT * FROM target ORDER BY tid"),
        Vec::<Vec<String>>::new()
    );
}

#[test]
fn v201_merge_conditions_and_clause_order() {
    let mut eng = engine();
    setup(&mut eng);
    // First true WHEN wins: balance > 15 deletes, else update to 0.
    let tag = tag_of(
        &mut eng,
        "MERGE INTO target t USING source s ON t.tid = s.sid \
         WHEN MATCHED AND t.balance > 15 THEN DELETE \
         WHEN MATCHED THEN UPDATE SET balance = 0 \
         WHEN NOT MATCHED AND s.delta > 100 THEN INSERT VALUES (s.sid, s.delta) \
         WHEN NOT MATCHED THEN DO NOTHING",
    );
    assert_eq!(tag, "MERGE 2");
    assert_eq!(
        rows_of(&mut eng, "SELECT * FROM target ORDER BY tid"),
        vec![vec!["1", "10"]]
    );
}

#[test]
fn v201_merge_cardinality_error() {
    let mut eng = engine();
    setup(&mut eng);
    run(&mut eng, "INSERT INTO source VALUES (2, 5)").unwrap();
    let e = run(
        &mut eng,
        "MERGE INTO target t USING source s ON t.tid = s.sid \
         WHEN MATCHED THEN UPDATE SET balance = 0",
    )
    .unwrap_err();
    assert_eq!(e.code, "21000");
    // Statement atomicity: nothing was applied.
    assert_eq!(
        rows_of(&mut eng, "SELECT * FROM target ORDER BY tid"),
        vec![vec!["1", "10"], vec!["2", "20"], vec!["3", "30"]]
    );
}

#[test]
fn v201_merge_target_in_source_and_syntax_pairing() {
    let mut eng = engine();
    setup(&mut eng);
    let e = run(
        &mut eng,
        "MERGE INTO target USING target ON tid = tid WHEN MATCHED THEN DO NOTHING",
    )
    .unwrap_err();
    assert_eq!(e.code, "42712");
    // INSERT under WHEN MATCHED is a parse error (42601).
    assert!(crate::sql::parse_statement(
        "MERGE INTO target t USING source s ON t.tid = s.sid WHEN MATCHED THEN INSERT DEFAULT VALUES"
    )
    .is_err());
    // UPDATE under WHEN NOT MATCHED is a parse error.
    assert!(crate::sql::parse_statement(
        "MERGE INTO target t USING source s ON t.tid = s.sid WHEN NOT MATCHED THEN UPDATE SET balance = 0"
    )
    .is_err());
}

#[test]
fn v201_merge_insert_default_values_and_cols() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE t2 (a integer, b integer DEFAULT -1)",
    )
    .unwrap();
    run(&mut eng, "CREATE TABLE s2 (x integer)").unwrap();
    run(&mut eng, "INSERT INTO s2 VALUES (7), (8)").unwrap();
    let tag = tag_of(
        &mut eng,
        "MERGE INTO t2 USING s2 ON t2.a = s2.x WHEN NOT MATCHED THEN INSERT (a) VALUES (s2.x)",
    );
    assert_eq!(tag, "MERGE 2");
    assert_eq!(
        rows_of(&mut eng, "SELECT * FROM t2 ORDER BY a"),
        vec![vec!["7", "-1"], vec!["8", "-1"]]
    );
    // NOT MATCHED condition may only see the source: a target reference
    // is PG's 42P01 invalid-reference error, raised statically.
    let e = run(
        &mut eng,
        "MERGE INTO t2 USING s2 ON t2.a = s2.x WHEN NOT MATCHED AND t2.b = 1 THEN INSERT VALUES (9, 9)",
    )
    .unwrap_err();
    assert_eq!(e.code, "42P01");
}

#[test]
fn v201_merge_returning_action_old_new() {
    let mut eng = engine();
    setup(&mut eng);
    let res = run(
        &mut eng,
        "MERGE INTO target t USING source s ON t.tid = s.sid \
         WHEN MATCHED THEN UPDATE SET balance = t.balance + s.delta \
         WHEN NOT MATCHED THEN INSERT VALUES (s.sid, s.delta) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE \
         RETURNING merge_action(), t.tid, old.balance, new.balance",
    )
    .expect("merge returning works");
    match res {
        ExecResult::Dml { tag, rows, .. } => {
            assert_eq!(tag, "MERGE 4");
            let got: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| v.to_text().unwrap_or_else(|| "NULL".to_string()))
                        .collect()
                })
                .collect();
            assert_eq!(
                got,
                vec![
                    vec!["UPDATE", "2", "20", "25"],
                    vec!["UPDATE", "3", "30", "50"],
                    vec!["INSERT", "4", "NULL", "40"],
                    vec!["DELETE", "1", "10", "NULL"],
                ]
            );
        }
        other => panic!("expected Dml, got {other:?}"),
    }
}

#[test]
fn v201_merge_explain() {
    let mut eng = engine();
    setup(&mut eng);
    match run(
        &mut eng,
        "EXPLAIN (COSTS OFF) MERGE INTO target t USING source s ON t.tid = s.sid WHEN MATCHED THEN DELETE",
    )
    .expect("explain merge works")
    {
        ExecResult::Explain { rows, .. } => {
            assert_eq!(rows[0][0].to_text().unwrap_or_default(), "Merge on target t");
        }
        other => panic!("expected Explain, got {other:?}"),
    }
}

#[test]
fn v201_merge_privileges() {
    let mut eng = engine();
    setup(&mut eng);
    run(&mut eng, "CREATE ROLE m_none").unwrap();
    // No privileges at all: even DO NOTHING fails on the target.
    let e = run_as(
        &mut eng,
        "MERGE INTO target USING (SELECT 1) s ON true WHEN MATCHED THEN DO NOTHING",
        "m_none",
    )
    .unwrap_err();
    assert_eq!(e.code, "42501");
    // UPDATE action without UPDATE privilege fails 42501.
    run(&mut eng, "GRANT SELECT ON target TO m_none").unwrap();
    run(&mut eng, "GRANT SELECT ON source TO m_none").unwrap();
    let e = run_as(
        &mut eng,
        "MERGE INTO target t USING source s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = 0",
        "m_none",
    )
    .unwrap_err();
    assert_eq!(e.code, "42501");
}

#[test]
fn v201_merge_subquery_and_values_source() {
    let mut eng = engine();
    setup(&mut eng);
    let tag = tag_of(
        &mut eng,
        "MERGE INTO target t USING (VALUES (2, 100), (9, 90)) AS v(a, b) ON t.tid = v.a \
         WHEN MATCHED THEN UPDATE SET balance = v.b \
         WHEN NOT MATCHED THEN INSERT VALUES (v.a, v.b)",
    );
    assert_eq!(tag, "MERGE 2");
    assert_eq!(
        rows_of(&mut eng, "SELECT * FROM target ORDER BY tid"),
        vec![
            vec!["1", "10"],
            vec!["2", "100"],
            vec!["3", "30"],
            vec!["9", "90"]
        ]
    );
}
