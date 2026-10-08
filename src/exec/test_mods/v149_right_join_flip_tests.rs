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
        other => panic!("expected EXPLAIN, got {other:?}"),
    }
}

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn setup(eng: &mut Engine) {
    for sql in [
        "CREATE TEMP TABLE j1 (id int PRIMARY KEY, v text)",
        "CREATE TEMP TABLE j2 (id int PRIMARY KEY, w text)",
        "INSERT INTO j1 VALUES (1, 'a'), (2, 'b')",
        "INSERT INTO j2 VALUES (2, 'x'), (3, 'y')",
    ] {
        run(eng, sql).unwrap();
    }
}

/// v1.49: `flip_right_joins` swaps left/right and sets LEFT, carrying the
/// ON clause over unmodified (PG does not commute quals).
#[test]
fn flip_swaps_sides_and_kind() {
    let from = parse_statement("SELECT * FROM j1 RIGHT JOIN j2 ON j1.id = j2.id").unwrap();
    let from = match from {
        crate::sql::Stmt::Select(s) => s.from,
        _ => panic!("expected select"),
    };
    let flipped = flip_right_joins(&from);
    assert_eq!(flipped.len(), 1);
    match &flipped[0] {
        FromItem::Join {
            left,
            kind,
            right,
            on,
            ..
        } => {
            assert_eq!(*kind, JoinKind::Left);
            // left is now j2, right is now j1
            assert!(matches!(left.as_ref(), FromItem::Table { name, .. } if name == "j2"));
            assert!(matches!(right.as_ref(), FromItem::Table { name, .. } if name == "j1"));
            // ON clause preserved verbatim
            assert!(on.is_some());
            let on_text = format!("{on:?}");
            assert!(
                on_text.contains("j1") && on_text.contains("j2"),
                "{on_text}"
            );
        }
        other => panic!("expected join, got {other:?}"),
    }
}

/// v1.49: non-RIGHT joins pass through untouched; nested RIGHT joins flip.
#[test]
fn flip_recurses_nested_joins() {
    let from = parse_statement(
        "SELECT * FROM (j1 RIGHT JOIN j2 ON j1.id = j2.id) LEFT JOIN j1 AS x ON x.id = j2.id",
    )
    .unwrap();
    let from = match from {
        crate::sql::Stmt::Select(s) => s.from,
        _ => panic!("expected select"),
    };
    let flipped = flip_right_joins(&from);
    // outer LEFT JOIN untouched, inner RIGHT flipped to LEFT with swap
    match &flipped[0] {
        FromItem::Join { kind, left, .. } => {
            assert_eq!(*kind, JoinKind::Left);
            match left.as_ref() {
                FromItem::Join {
                    left: il,
                    kind: ik,
                    right: ir,
                    ..
                } => {
                    assert_eq!(*ik, JoinKind::Left);
                    assert!(matches!(il.as_ref(), FromItem::Table { name, .. } if name == "j2"));
                    assert!(matches!(ir.as_ref(), FromItem::Table { name, .. } if name == "j1"));
                }
                other => panic!("expected inner join, got {other:?}"),
            }
        }
        other => panic!("expected join, got {other:?}"),
    }
}

/// v1.49: PG19 rendering — a RIGHT JOIN plans as `Nested Loop Left Join`
/// with swapped children (corpus join.out 4701 oracle).
#[test]
fn explain_right_renders_flipped_left() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM j1 RIGHT JOIN j2 ON j1.id = j2.id",
    );
    assert_eq!(lines[0], "Nested Loop Left Join", "{lines:?}");
    // children swapped: j2 (original right) is now the outer
    assert!(lines[1].contains("j2"), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("j1")), "{lines:?}");
    // and the non-flipped LEFT JOIN renders with the original order
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (COSTS OFF) SELECT * FROM j1 LEFT JOIN j2 ON j1.id = j2.id",
    );
    assert_eq!(lines[0], "Nested Loop Left Join", "{lines:?}");
    assert!(lines[1].contains("j1"), "{lines:?}");
}

/// v1.49: VERBOSE top-level `Output:` for bare `SELECT *` shows PG's
/// written column order even after the flip (query targetlist, not the
/// flipped physical order).
#[test]
fn verbose_star_output_keeps_written_order() {
    let mut eng = engine();
    setup(&mut eng);
    let lines = plan_lines(
        &mut eng,
        "EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM j1 RIGHT JOIN j2 ON j1.id = j2.id",
    );
    let output_line = lines
        .iter()
        .find(|l| l.trim_start().starts_with("Output:"))
        .expect("expected Output line");
    // written order: j1 columns first, then j2
    let j1_pos = output_line.find("j1.id").unwrap();
    let j2_pos = output_line.find("j2.id").unwrap();
    assert!(j1_pos < j2_pos, "{output_line}");
}

/// v1.49: the planner flip is semantics-preserving — a RIGHT JOIN returns
/// the same rows as the manually-flipped LEFT JOIN (non-EXPLAIN path).
/// This is the invariant PG's `reduce_outer_joins` relies on.
#[test]
fn right_join_results_match_flipped_left() {
    let mut eng = engine();
    setup(&mut eng);
    for (right_sql, left_sql) in [
        (
            "SELECT j1.id, j1.v, j2.id, j2.w FROM j1 RIGHT JOIN j2 \
                 ON j1.id = j2.id ORDER BY j2.id",
            "SELECT j1.id, j1.v, j2.id, j2.w FROM j2 LEFT JOIN j1 \
                 ON j1.id = j2.id ORDER BY j2.id",
        ),
        (
            "SELECT count(*) FROM j1 RIGHT JOIN j2 ON j1.id = j2.id",
            "SELECT count(*) FROM j2 LEFT JOIN j1 ON j1.id = j2.id",
        ),
        (
            "SELECT j2.w FROM j1 RIGHT JOIN j2 ON j1.id = j2.id AND j1.v = 'a'",
            "SELECT j2.w FROM j2 LEFT JOIN j1 ON j1.id = j2.id AND j1.v = 'a'",
        ),
    ] {
        let r_rows = rows_of(run(&mut eng, right_sql).unwrap());
        let l_rows = rows_of(run(&mut eng, left_sql).unwrap());
        assert_eq!(r_rows, l_rows, "mismatch:\n{right_sql}\n{left_sql}");
    }
    // spot-check the actual RIGHT JOIN semantics (unmatched right row)
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT j1.id, j2.id FROM j1 RIGHT JOIN j2 ON j1.id = j2.id ORDER BY j2.id",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["2".to_string(), "2".to_string()],
            vec!["NULL".to_string(), "3".to_string()],
        ]
    );
}
