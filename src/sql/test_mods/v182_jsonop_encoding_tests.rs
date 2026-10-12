//! v1.82: JSON operators inside CHECK / domain expressions must
//! round-trip through the constraint encoding that WAL records and
//! checkpoints use. The first v1.82 build encoded `(jsonop -> ...)`
//! with no matching `SexprParser` arm; WAL recovery then refused to
//! open a data directory containing such a check ("corrupt domain
//! checks in WAL: bad expr head jsonop") — the v1.80 bug class.

use super::*;

fn lit(s: &str) -> Box<Expr> {
    Box::new(Expr::Literal(Literal::Text(s.into())))
}

fn roundtrip(e: &Expr) {
    let mut enc = String::new();
    encode_expr_inner(e, &mut enc);
    let decoded = SexprParser::new(&enc)
        .expr()
        .unwrap_or_else(|err| panic!("{e:?}: {err} in {enc}"));
    assert_eq!(decoded, *e, "via {enc}");
}

#[test]
fn v182_jsonop_roundtrips_through_constraint_encoding() {
    for op in [
        JsonOp::Arrow,
        JsonOp::ArrowText,
        JsonOp::HashArrow,
        JsonOp::HashArrowText,
    ] {
        roundtrip(&Expr::JsonOp {
            op,
            left: lit("{\"a\":1}"),
            right: lit("a"),
        });
    }
    // Nested: (doc -> 'a') ->> 'b'.
    roundtrip(&Expr::JsonOp {
        op: JsonOp::ArrowText,
        left: Box::new(Expr::JsonOp {
            op: JsonOp::Arrow,
            left: Box::new(Expr::Column {
                table: None,
                name: "j".to_string(),
            }),
            right: lit("a"),
        }),
        right: lit("b"),
    });
}
