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

fn plan_lines(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap() {
        ExecResult::Explain { rows, .. } => {
            rows.into_iter().map(|r| r[0].to_text().unwrap()).collect()
        }
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn setup(eng: &mut Engine) {
    for ddl in [
        "CREATE TABLE a (id int PRIMARY KEY, b_id int)",
        "CREATE TABLE b (id int PRIMARY KEY, c_id int)",
        "CREATE TABLE c (id int PRIMARY KEY)",
        "CREATE TABLE np (id int, v int)",
    ] {
        run(eng, ddl).unwrap();
    }
}

#[test]
fn simple_useless_left_join_removed() {
    // v1.45: the corpus cases — PG plans these as a bare Seq Scan.
    let mut eng = engine();
    setup(&mut eng);
    for sql in [
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON a.b_id = b.id",
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b AS x ON a.b_id = x.id",
        // Reversed equality order is fine too.
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON b.id = a.b_id",
    ] {
        let lines = plan_lines(&mut eng, sql);
        assert_eq!(lines, vec!["Seq Scan on a".to_string()], "{sql}");
    }
    // v1.47: chained removal (PG's `goto restart` fixpoint). The
    // c-join is removed first (its ON's b.c_id reference becomes
    // unreferenced), then the b-join goes too.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON a.b_id = b.id LEFT JOIN c ON b.c_id = c.id",
    );
    assert_eq!(lines, vec!["Seq Scan on a".to_string()], "{lines:?}");
}

#[test]
fn join_kept_when_needed() {
    // v1.45: every doubtful case keeps the join (PG does too).
    let mut eng = engine();
    setup(&mut eng);
    // SELECT * needs b's columns.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM a LEFT JOIN b ON a.b_id = b.id",
    );
    assert!(lines.iter().any(|l| l.contains("Nested Loop")), "{lines:?}");
    // WHERE references the inner range.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON a.b_id = b.id WHERE b.c_id > 0",
    );
    assert!(lines.iter().any(|l| l.contains("Nested Loop")), "{lines:?}");
    // Join column not unique.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN np ON a.b_id = np.id",
    );
    assert!(lines.iter().any(|l| l.contains("Nested Loop")), "{lines:?}");
    // INNER join is never removed.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a JOIN b ON a.b_id = b.id",
    );
    assert!(lines.iter().any(|l| l.contains("Nested Loop")), "{lines:?}");
    // Unqualified column reference resolving unambiguously to the outer
    // range: PG resolves `b_id` to `a.b_id` (only `a` has it), so the
    // inner range is unreferenced above the join and PG removes it
    // (v1.51: no longer conservatively kept).
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON a.b_id = b.id WHERE b_id > 0",
    );
    assert!(
        !lines.iter().any(|l| l.contains("Nested Loop")),
        "{lines:?}"
    );
    // Ambiguous unqualified reference: conservative keep.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON a.b_id = b.id WHERE id > 0",
    );
    assert!(lines.iter().any(|l| l.contains("Nested Loop")), "{lines:?}");
    // SELECT list references the inner range.
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.*, b.c_id FROM a LEFT JOIN b ON a.b_id = b.id",
    );
    assert!(lines.iter().any(|l| l.contains("Nested Loop")), "{lines:?}");
}

#[test]
fn removal_preserves_outer_filter() {
    // v1.45: removing the join must not drop the outer WHERE.
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT a.* FROM a LEFT JOIN b ON a.b_id = b.id WHERE a.b_id > 0",
    );
    assert_eq!(
        lines,
        vec![
            "Seq Scan on a".to_string(),
            "  Filter: (b_id > 0)".to_string()
        ],
        "{lines:?}"
    );
}

#[test]
fn non_text_format_honestly_rejected() {
    // v1.45: we render only text; PG19 accepts the syntax, so the
    // failure must be a feature 0A000, not a 42601.
    let mut eng = engine();
    let err = run(&mut eng, "EXPLAIN (FORMAT JSON) SELECT 1").unwrap_err();
    assert_eq!(err.code, "0A000");
    let err = run(&mut eng, "EXPLAIN (FORMAT XML) SELECT 1").unwrap_err();
    assert_eq!(err.code, "0A000");
    // TEXT still works.
    let lines = plan_lines(&mut eng, "EXPLAIN (FORMAT TEXT, COSTS OFF) SELECT 1");
    assert!(!lines.is_empty());
}
