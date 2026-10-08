
use super::*;

#[test]
fn set_transaction_adjacent_modes() {
    // v1.07: PG19 accepts adjacent (comma-less) transaction modes.
    match parse_statement("SET TRANSACTION READ WRITE, ISOLATION LEVEL SERIALIZABLE")
        .expect("parses")
    {
        Stmt::SetTransaction {
            level,
            read_only,
            snapshot,
            ..
        } => {
            assert_eq!(level, Some(IsolationLevel::Serializable));
            assert_eq!(read_only, Some(false));
            assert_eq!(snapshot, None);
        }
        other => panic!("expected SetTransaction, got {:?}", other),
    }
    match parse_statement("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .expect("parses")
    {
        Stmt::SetTransaction {
            level, read_only, ..
        } => {
            assert_eq!(level, Some(IsolationLevel::RepeatableRead));
            assert_eq!(read_only, Some(true));
        }
        other => panic!("expected SetTransaction, got {:?}", other),
    }
}

#[test]
fn set_transaction_snapshot() {
    // v1.07: SET TRANSACTION SNAPSHOT 'id'.
    match parse_statement("SET TRANSACTION SNAPSHOT '0003-A1'").expect("parses") {
        Stmt::SetTransaction {
            snapshot, level, ..
        } => {
            assert_eq!(snapshot, Some("0003-A1".to_string()));
            assert_eq!(level, None);
        }
        other => panic!("expected SetTransaction, got {:?}", other),
    }
}

#[test]
fn set_role_bare() {
    // v1.07: bare SET ROLE name (PG19).
    match parse_statement("SET ROLE regress_insert_other_user").expect("parses") {
        Stmt::Set { name, value, .. } => {
            assert_eq!(name, "role");
            match value {
                SetValue::Str(s) => assert_eq!(s, "regress_insert_other_user"),
                other => panic!("expected Str, got {:?}", other),
            }
        }
        other => panic!("expected Set, got {:?}", other),
    }
}

#[test]
fn set_signed_numeric_value() {
    // v1.07: signed numeric SET values (e.g. extra_float_digits = -1).
    match parse_statement("SET extra_float_digits = -1").expect("parses") {
        Stmt::Set { name, value, .. } => {
            assert_eq!(name, "extra_float_digits");
            match value {
                SetValue::Str(s) => assert_eq!(s, "-1"),
                other => panic!("expected Str, got {:?}", other),
            }
        }
        other => panic!("expected Set, got {:?}", other),
    }
}
