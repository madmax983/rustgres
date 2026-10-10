//! v1.80: casts inside CHECK / DEFAULT expressions must round-trip
//! through the constraint encoding that WAL records and checkpoints use.
//! v1.79 wrote the cast type as a bare `sql_name()`, so multi-word names
//! (`timestamp without time zone`, `double precision`, `character
//! varying`) and names `coltype_by_name` did not know (`character`,
//! `"char"`, arrays) decoded as garbage or failed. The table's whole
//! constraint string was then rejected, and WAL recovery refused to open
//! the data directory ("bad constraints for table ...").

use super::*;
use crate::storage::ColType;

fn cast_roundtrip(sql_type: &str) -> ColType {
    let stmt = parse_statement(&format!("SELECT NULL::{sql_type}")).expect("parses");
    let Stmt::Select(sel) = stmt else {
        panic!("expected SELECT");
    };
    let Some(SelectItem::Expr { expr, .. }) = sel.items.first().cloned() else {
        panic!("expected one expression item");
    };
    let Expr::Cast { to, .. } = &expr else {
        panic!("expected a cast, got {:?}", expr);
    };
    let mut enc = String::new();
    encode_expr_inner(&expr, &mut enc);
    let decoded = SexprParser::new(&enc)
        .expr()
        .unwrap_or_else(|e| panic!("{sql_type}: {e} in {enc}"));
    let Expr::Cast { to: back, .. } = decoded else {
        panic!("{sql_type}: decoded to a non-cast");
    };
    assert_eq!(&back, to, "{sql_type} via {enc}");
    let back: ColType = back;
    back
}

#[test]
fn v180_cast_types_roundtrip_through_constraint_encoding() {
    assert_eq!(cast_roundtrip("timestamp"), ColType::Timestamp);
    assert_eq!(cast_roundtrip("timestamptz"), ColType::Timestamptz);
    assert_eq!(cast_roundtrip("double precision"), ColType::Float);
    assert_eq!(cast_roundtrip("varchar(5)"), ColType::Varchar(Some(5)));
    assert_eq!(cast_roundtrip("char(3)"), ColType::Char(Some(3)));
    assert_eq!(
        cast_roundtrip("numeric(10,2)"),
        ColType::Numeric(Some((10, 2)))
    );
    assert_eq!(cast_roundtrip("\"char\""), ColType::SingleChar);
    cast_roundtrip("int[]");
    cast_roundtrip("text");
    cast_roundtrip("date");
}

#[test]
fn v180_decodes_v179_bare_cast_types() {
    // Records written by v1.79 carry the bare form; they must still decode.
    for (enc, want) in [
        (
            "(cast timestamp without time zone (lit null))",
            ColType::Timestamp,
        ),
        (
            "(cast timestamp with time zone (lit null))",
            ColType::Timestamptz,
        ),
        ("(cast double precision (lit null))", ColType::Float),
        (
            "(cast character varying (lit null))",
            ColType::Varchar(None),
        ),
        ("(cast integer (lit null))", ColType::Int),
        ("(cast \"char\" (lit null))", ColType::SingleChar),
    ] {
        match SexprParser::new(enc).expr() {
            Ok(Expr::Cast { to, .. }) => assert_eq!(to, want, "{enc}"),
            other => panic!("{enc}: {:?}", other),
        }
    }
}
