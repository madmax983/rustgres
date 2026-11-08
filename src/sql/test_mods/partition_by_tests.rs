// --- v0.70: focused parser tests for PARTITION BY keys -----------------------
use super::*;

fn part_def(sql: &str) -> PartitionDef {
    match parse_statement(sql).expect("parses") {
        Stmt::CreateTable { def, .. } => def.partition.expect("has partition def"),
        other => panic!("expected CREATE TABLE, got {:?}", other),
    }
}

#[test]
fn unparenthesized_expression_key() {
    // v0.70: PG accepts any expression as a partition key —
    // `PARTITION BY LIST (lower(a))` (v0.69 required `((lower(a)))`).
    let p = part_def("create table t (a text) partition by list (lower(a))");
    assert_eq!(p.keys.len(), 1);
    match &p.keys[0] {
        PartitionKeyDef::Expr(Expr::Func { name, .. }, _) => assert_eq!(name, "lower"),
        other => panic!("expected Expr key, got {:?}", other),
    }
}

#[test]
fn bare_column_key() {
    let p = part_def("create table t (a int) partition by list (a)");
    assert_eq!(p.keys.len(), 1);
    assert_eq!(p.keys[0], PartitionKeyDef::Column("a".to_string(), None));
}

#[test]
fn parenthesized_arithmetic_key() {
    let p = part_def("create table t (a int, b int) partition by range ((a + b))");
    assert_eq!(p.keys.len(), 1);
    assert!(matches!(p.keys[0], PartitionKeyDef::Expr(_, _)));
}

#[test]
fn opclass_key() {
    // v2.23: a bare column with an operator class stays a column
    // key, and the opclass name is now captured (it was discarded
    // before v2.23).
    let p = part_def("create table t (a int) partition by hash (a int4_ops)");
    assert_eq!(p.keys.len(), 1);
    assert_eq!(
        p.keys[0],
        PartitionKeyDef::Column("a".to_string(), Some("int4_ops".to_string()))
    );
}

#[test]
fn multiple_keys() {
    let p = part_def("create table t (a int, b text) partition by list (a, lower(b))");
    assert_eq!(p.keys.len(), 2);
    assert_eq!(p.keys[0], PartitionKeyDef::Column("a".to_string(), None));
    assert!(matches!(p.keys[1], PartitionKeyDef::Expr(_, _)));
}
