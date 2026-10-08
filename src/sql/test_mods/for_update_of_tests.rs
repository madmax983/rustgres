
use super::*;

fn select_stmt(sql: &str) -> SelectStmt {
    match parse_statement(sql).expect("parses") {
        Stmt::Select(s) => s,
        other => panic!("expected SELECT, got {:?}", other),
    }
}

#[test]
fn bare_for_update_has_empty_of() {
    // v1.14: bare FOR UPDATE leaves for_update_of empty.
    let s = select_stmt("select a from t for update");
    assert!(s.for_update);
    assert!(s.for_update_of.is_empty());
}

#[test]
fn for_update_of_single_table() {
    // v1.14: FOR UPDATE OF tbl parses the target list.
    let s = select_stmt("select a from t1, t2 for update of t1");
    assert!(s.for_update);
    assert_eq!(s.for_update_of, vec!["t1".to_string()]);
}

#[test]
fn for_update_of_multiple_tables() {
    // v1.14: comma-separated OF list.
    let s = select_stmt("select a from t1, t2 for update of t1, t2");
    assert!(s.for_update);
    assert_eq!(s.for_update_of, vec!["t1".to_string(), "t2".to_string()]);
}

#[test]
fn for_update_of_alias() {
    // v1.14: OF names may be aliases.
    let s = select_stmt("select a from t1 as x for update of x");
    assert_eq!(s.for_update_of, vec!["x".to_string()]);
}

#[test]
fn no_for_update_no_of() {
    // v1.14: without FOR UPDATE there is no OF list.
    let s = select_stmt("select a from t");
    assert!(!s.for_update);
    assert!(s.for_update_of.is_empty());
}
