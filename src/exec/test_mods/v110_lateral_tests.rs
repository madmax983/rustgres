/// v1.10: explicit LATERAL derived tables and VALUES (PG19 `LATERAL_P
/// select_with_parens`): the right-hand item is evaluated once per left
/// row with the left row as the innermost scope.
use super::*;
use crate::sql::{FromItem, Stmt, parse_statement};

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    // Preserve the parser's SQLSTATE (e.g. 42601), like the main
    // test harness does.
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
        other => panic!("expected Select, got {:?}", other),
    }
}

/// users(id int): (1), (2), (3).
fn users(eng: &mut Engine) {
    run(eng, "CREATE TABLE users (id int)").unwrap();
    run(eng, "INSERT INTO users VALUES (1), (2), (3)").unwrap();
}

/// The parser marks `LATERAL (SELECT ...)` / `LATERAL (VALUES ...)`
/// on the FROM item; a relation literally named `lateral` still
/// parses as a table (PG19: LATERAL is unreserved); LATERAL before
/// a parenthesized join is a PG19 syntax error.
#[test]
fn parse_marks_derived_and_values() {
    // Comma-separated items parse into CROSS JOINs; the LATERAL
    // item is the join's right side.
    let right_of_comma = |sql: &str| match parse_statement(sql).unwrap() {
        Stmt::Select(s) => match s.from.into_iter().next().unwrap() {
            FromItem::Join { right, .. } => *right,
            other => other,
        },
        _ => panic!("expected SELECT"),
    };
    assert!(matches!(
        right_of_comma("SELECT * FROM t, LATERAL (SELECT 1) AS s"),
        FromItem::Derived { lateral: true, .. }
    ));
    assert!(matches!(
        right_of_comma("SELECT * FROM t, LATERAL (VALUES (1)) AS v"),
        FromItem::Values { lateral: true, .. }
    ));
    assert!(matches!(
        right_of_comma("SELECT * FROM t, (SELECT 1) AS s"),
        FromItem::Derived { lateral: false, .. }
    ));
    assert!(matches!(
        parse_statement("SELECT * FROM lateral").unwrap(),
        Stmt::Select(s) if matches!(&s.from[0], FromItem::Table { name, .. } if name == "lateral")
    ));
    let err = parse_statement("SELECT * FROM t, LATERAL ((SELECT 1) CROSS JOIN (SELECT 2)) AS s")
        .unwrap_err();
    assert_eq!(err.code, "42601");
}

/// Comma LATERAL correlated subquery re-evaluates per left row.
#[test]
fn comma_correlated() {
    let mut eng = engine();
    users(&mut eng);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, s.d FROM users u, LATERAL (SELECT u.id * 10 AS d) AS s ORDER BY 1",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "10".to_string()],
            vec!["2".to_string(), "20".to_string()],
            vec!["3".to_string(), "30".to_string()],
        ]
    );
}

/// INNER JOIN LATERAL with ON, and LEFT JOIN LATERAL null-extension
/// when the subquery returns no rows.
#[test]
fn join_on_and_left_null_ext() {
    let mut eng = engine();
    users(&mut eng);
    run(&mut eng, "CREATE TABLE orders (uid int, amt int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO orders VALUES (1, 10), (1, 20), (2, 5)",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, o.amt FROM users u INNER JOIN LATERAL \
                 (SELECT amt FROM orders WHERE uid = u.id) AS o ON o.amt > 10 \
                 ORDER BY 1, 2",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["1".to_string(), "20".to_string()]]);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, o.amt FROM users u LEFT JOIN LATERAL \
                 (SELECT amt FROM orders WHERE uid = u.id AND amt > 100) AS o ON true \
                 ORDER BY 1",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "NULL".to_string()],
            vec!["2".to_string(), "NULL".to_string()],
            vec!["3".to_string(), "NULL".to_string()],
        ]
    );
}

/// LATERAL (VALUES ...) sees the left row; multi-row correlated
/// subqueries fan out; uncorrelated LATERAL evaluates per left row.
#[test]
fn values_and_fanout() {
    let mut eng = engine();
    users(&mut eng);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, v.x FROM users u, LATERAL (VALUES (u.id + 100), (u.id + 200)) AS v(x) \
                 WHERE u.id = 2 ORDER BY 2",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["2".to_string(), "102".to_string()],
            vec!["2".to_string(), "202".to_string()],
        ]
    );
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, s.k FROM users u, LATERAL (SELECT 7 AS k) AS s WHERE u.id = 3",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["3".to_string(), "7".to_string()]]);
}

/// Nested LATERAL sees same-level earlier siblings; an empty left
/// input yields no rows but still type-checks.
#[test]
fn nested_and_empty_left() {
    let mut eng = engine();
    users(&mut eng);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, s1.a, s2.b FROM users u, \
                 LATERAL (SELECT u.id + 1 AS a) AS s1, \
                 LATERAL (SELECT s1.a + 1 AS b) AS s2 \
                 WHERE u.id = 1",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![vec!["1".to_string(), "2".to_string(), "3".to_string()]]
    );
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, s.d FROM users u, LATERAL (SELECT u.id * 10 AS d) AS s WHERE false",
        )
        .unwrap(),
    );
    assert!(rows.is_empty());
}

#[test]
fn cross_nest_lateral() {
    // PG19: a LATERAL item may reference any FROM item that precedes
    // it textually, even outside its own join nest.
    let mut eng = engine();
    users(&mut eng);
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, b.x FROM users u, (SELECT 1 AS one) AS o \
                 JOIN LATERAL (VALUES (u.id + o.one)) AS b(x) ON true \
                 WHERE u.id <= 2 ORDER BY u.id",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "2".to_string()],
            vec!["2".to_string(), "3".to_string()],
        ]
    );
    // Non-LATERAL siblings stay invisible (PG19).
    let err = run(
        &mut eng,
        "SELECT u.id FROM users u, (SELECT u.id AS v) AS s WHERE u.id = 1",
    )
    .unwrap_err();
    assert_eq!(err.code, "42703");
}

#[test]
fn right_full_correlated_lateral_rejected() {
    // PG19: "The combining JOIN type must be INNER or LEFT for a
    // LATERAL reference." Correlated LATERAL on RIGHT/FULL is an
    // error; uncorrelated LATERAL is legal.
    let mut eng = engine();
    users(&mut eng);
    for kind in ["RIGHT", "FULL"] {
        let err = run(
            &mut eng,
            &format!(
                "SELECT u.id, s.v FROM users u {} JOIN LATERAL \
                     (SELECT u.id + 1 AS v) AS s ON true WHERE u.id = 1",
                kind
            ),
        )
        .unwrap_err();
        assert_eq!(err.code, "42P10", "kind={}", kind);
    }
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT u.id, s.v FROM users u RIGHT JOIN LATERAL \
                 (SELECT 42 AS v) AS s ON true WHERE u.id = 1",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["1".to_string(), "42".to_string()]]);
}

// v1.11: composite value expressions — ARRAY[...] and ROW(...).

#[test]
fn v111_array_union_describes_int_array() {
    // PG19: UNION over VALUES with array[...] columns describes the
    // column as integer[] (not text), and dedups correctly.
    let mut eng = engine();
    let r = run(
        &mut eng,
        "select x from (values (array[1, 2]), (array[1, 3])) _(x) \
             union select x from (values (array[1, 2]), (array[1, 4])) _(x)",
    )
    .unwrap();
    match &r {
        ExecResult::Select { columns, .. } => {
            assert_eq!(columns.len(), 1);
            assert_eq!(columns[0].1, ColType::Array(ArrayElem::Int));
            assert_eq!(columns[0].1.oid(), 1007); // _int4, not 25
        }
        _ => panic!("expected Select"),
    }
    let mut rows = rows_of(r);
    rows.sort();
    assert_eq!(
        rows,
        vec![
            vec!["{1,2}".to_string()],
            vec!["{1,3}".to_string()],
            vec!["{1,4}".to_string()],
        ]
    );
}

#[test]
fn v111_array_intersect_except() {
    let mut eng = engine();
    let rows = rows_of(
        run(
            &mut eng,
            "select x from (values (array[1, 2]), (array[1, 3])) _(x) \
                 intersect select x from (values (array[1, 2]), (array[1, 4])) _(x)",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["{1,2}".to_string()]]);
    let rows = rows_of(
        run(
            &mut eng,
            "select x from (values (array[1, 2]), (array[1, 3])) _(x) \
                 except select x from (values (array[1, 2]), (array[1, 4])) _(x)",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["{1,3}".to_string()]]);
}

#[test]
fn v111_record_setops() {
    // PG19: UNION/INTERSECT/EXCEPT over VALUES with row(...) columns
    // describes the column as record and applies set semantics.
    let mut eng = engine();
    let r = run(
        &mut eng,
        "select x from (values (row(1, 2)), (row(1, 3))) _(x) \
             union select x from (values (row(1, 2)), (row(1, 4))) _(x)",
    )
    .unwrap();
    match &r {
        ExecResult::Select { columns, .. } => {
            assert_eq!(columns[0].1, ColType::Record);
            assert_eq!(columns[0].1.oid(), 2249);
        }
        _ => panic!("expected Select"),
    }
    let mut rows = rows_of(r);
    rows.sort();
    assert_eq!(
        rows,
        vec![
            vec!["(1,2)".to_string()],
            vec!["(1,3)".to_string()],
            vec!["(1,4)".to_string()],
        ]
    );
    let rows = rows_of(
        run(
            &mut eng,
            "select x from (values (row(1, 2)), (row(1, 3))) _(x) \
                 intersect select x from (values (row(1, 2)), (row(1, 4))) _(x)",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["(1,2)".to_string()]]);
}

#[test]
fn v111_row_subquery_comparison() {
    // PG19: a multi-column subquery is allowed as an operand of a
    // row comparison; it evaluates to a record (f1, f2, ...).
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE rq_t (f1 int, f2 int)").unwrap();
    run(&mut eng, "INSERT INTO rq_t VALUES (1, 2), (3, 4)").unwrap();
    // Correlated: ROW(1,2) matches the first outer row only.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT ROW(1, 2) = (SELECT f1, f2) AS eq FROM rq_t",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["t".to_string()], vec!["f".to_string()]]);
    // Uncorrelated.
    let rows = rows_of(run(&mut eng, "SELECT ROW(1, 2) = (SELECT 3, 4)").unwrap());
    assert_eq!(rows, vec![vec!["f".to_string()]]);
    // Row subquery on the left side.
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT (SELECT f1, f2 FROM rq_t LIMIT 1) = ROW(1, 2)",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["t".to_string()]]);
}

#[test]
fn v111_row_subquery_errors() {
    // PG19: more than one row is 21000.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE rq2_t (f1 int, f2 int)").unwrap();
    run(&mut eng, "INSERT INTO rq2_t VALUES (1, 2), (3, 4)").unwrap();
    let err = run(&mut eng, "SELECT ROW(1, 2) = (SELECT f1, f2 FROM rq2_t)").unwrap_err();
    assert_eq!(err.code, "21000");
    // Scalar context still rejects multi-column subqueries: 42601.
    let err = run(&mut eng, "SELECT (SELECT 1, 2)").unwrap_err();
    assert_eq!(err.code, "42601");
}
