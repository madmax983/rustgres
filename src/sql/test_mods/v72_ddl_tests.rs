// v0.72: parser unit tests for the DDL conformance scope (LIKE,
// reloptions, multi-action ALTER). Exec behavior is pinned by
// tests/protocol_test73.py.
use super::*;

fn create_def(sql: &str) -> TableDef {
    match parse_statement(sql).expect("parses") {
        Stmt::CreateTable { def, .. } => def,
        other => panic!("expected CREATE TABLE, got {:?}", other),
    }
}

#[test]
fn like_bare_clause() {
    let def = create_def("create table t (like src)");
    assert_eq!(def.likes.len(), 1);
    assert_eq!(def.likes[0].source, "src");
    assert!(def.likes[0].options.is_empty());
    assert!(def.columns.is_empty());
}

#[test]
fn like_mixed_with_columns_and_options() {
    let def = create_def(
        "create table t (z int, like src including defaults excluding constraints, like other including all)",
    );
    assert_eq!(def.columns.len(), 1);
    assert_eq!(def.likes.len(), 2);
    assert_eq!(def.likes[0].source, "src");
    assert_eq!(
        def.likes[0].options,
        vec![
            LikeOption {
                including: true,
                kind: LikeKind::Defaults
            },
            LikeOption {
                including: false,
                kind: LikeKind::Constraints
            }
        ]
    );
    assert_eq!(def.likes[1].source, "other");
    assert_eq!(
        def.likes[1].options,
        vec![LikeOption {
            including: true,
            kind: LikeKind::All
        }]
    );
}

#[test]
fn like_schema_qualified_source() {
    let def = create_def("create table t (like public.src)");
    assert_eq!(def.likes[0].source, "public.src");
}

#[test]
fn like_including_excluding_compression() {
    let def = create_def(
        "create table t (like src including compression, like o2 excluding compression)",
    );
    assert_eq!(def.likes.len(), 2);
    assert_eq!(
        def.likes[0].options,
        vec![LikeOption {
            including: true,
            kind: LikeKind::Compression
        }]
    );
    assert_eq!(
        def.likes[1].options,
        vec![LikeOption {
            including: false,
            kind: LikeKind::Compression
        }]
    );
}

#[test]
fn reloptions_fillfactor() {
    let def =
        create_def("create table t (a int) with (fillfactor = 10, autovacuum_enabled = true)");
    assert_eq!(
        def.reloptions,
        vec![
            ("fillfactor".to_string(), "10".to_string()),
            ("autovacuum_enabled".to_string(), "true".to_string())
        ]
    );
}

#[test]
fn alter_multi_action() {
    match parse_statement("alter table t add a int, add b text").expect("parses") {
        Stmt::AlterTable { name, actions, .. } => {
            assert_eq!(name, "t");
            assert_eq!(actions.len(), 2);
            assert!(matches!(actions[0], AlterAction::AddColumn { .. }));
            assert!(matches!(actions[1], AlterAction::AddColumn { .. }));
        }
        other => panic!("expected ALTER TABLE, got {:?}", other),
    }
}

#[test]
fn alter_single_action_still_vec_of_one() {
    match parse_statement("alter table t add a int").expect("parses") {
        Stmt::AlterTable { actions, .. } => assert_eq!(actions.len(), 1),
        other => panic!("expected ALTER TABLE, got {:?}", other),
    }
}
