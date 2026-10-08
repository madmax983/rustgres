/// v1.59: const-false qual folding for EXPLAIN (`pg_fold_bool_const` +
/// `pg_literal_cmp` + NULLIF in `pg_fold_const_item`). Flips the three
/// case.out statements to `Result` / `One-Time Filter: false`.
use super::*;
use std::sync::Arc;

fn int(i: i64) -> Expr {
    Expr::Literal(Literal::Int(i))
}

fn flt(f: f64) -> Expr {
    Expr::Literal(Literal::Float(f))
}

fn txt(s: &str) -> Expr {
    Expr::Literal(Literal::Text(Arc::from(s)))
}

fn null() -> Expr {
    Expr::Literal(Literal::Null)
}

fn cmp(op: CmpOp, l: Expr, r: Expr) -> Expr {
    Expr::Cmp {
        op,
        left: Box::new(l),
        right: Box::new(r),
    }
}

fn nullif(a: Expr, b: Expr) -> Expr {
    Expr::Func {
        name: "nullif".to_string(),
        args: vec![a, b],
    }
}

fn is_null(e: Expr, neg: bool) -> Expr {
    Expr::IsNull {
        expr: Box::new(e),
        neg,
    }
}

/// v1.59: integer comparisons fold exactly (i64).
#[test]
fn v159_literal_cmp_int() {
    let a = Literal::Int(1);
    let b = Literal::Int(2);
    assert_eq!(pg_literal_cmp(&a, CmpOp::Eq, &b), Some(false));
    assert_eq!(pg_literal_cmp(&a, CmpOp::Ne, &b), Some(true));
    assert_eq!(pg_literal_cmp(&a, CmpOp::Lt, &b), Some(true));
    assert_eq!(pg_literal_cmp(&b, CmpOp::Gt, &a), Some(true));
    assert_eq!(pg_literal_cmp(&a, CmpOp::Le, &a), Some(true));
    assert_eq!(pg_literal_cmp(&a, CmpOp::Ge, &a), Some(true));
    // SmallInt/BigInt participate in the integer domain.
    assert_eq!(
        pg_literal_cmp(&Literal::SmallInt(1), CmpOp::Eq, &Literal::BigInt(1)),
        Some(true)
    );
}

/// v1.59: float comparisons; NaN fails closed (PG's float8eq treats
/// NaN as equal — no faithful fold here).
#[test]
fn v159_literal_cmp_float() {
    let a = Literal::Float(1.5);
    let b = Literal::Float(2.5);
    assert_eq!(pg_literal_cmp(&a, CmpOp::Lt, &b), Some(true));
    assert_eq!(pg_literal_cmp(&a, CmpOp::Eq, &a), Some(true));
    // Mixed int/float: PG coerces int to float8; exact for |i| < 2^53.
    assert_eq!(
        pg_literal_cmp(&Literal::Int(2), CmpOp::Eq, &Literal::Float(2.0)),
        Some(true)
    );
    assert_eq!(
        pg_literal_cmp(&Literal::Float(0.1), CmpOp::Gt, &Literal::Int(0)),
        Some(true)
    );
    assert_eq!(
        pg_literal_cmp(&Literal::Float(f64::NAN), CmpOp::Eq, &a),
        None
    );
    // Huge ints are not exactly representable as f64: fail closed.
    assert_eq!(
        pg_literal_cmp(&Literal::Int(1 << 60), CmpOp::Eq, &Literal::Float(1e18)),
        None
    );
}

/// v1.59: text/bool compare naturally; mismatched domains fail closed.
#[test]
fn v159_literal_cmp_text_bool_other() {
    assert_eq!(
        pg_literal_cmp(
            &Literal::Text(Arc::from("a")),
            CmpOp::Lt,
            &Literal::Text(Arc::from("b"))
        ),
        Some(true)
    );
    assert_eq!(
        pg_literal_cmp(&Literal::Bool(true), CmpOp::Eq, &Literal::Bool(false)),
        Some(false)
    );
    assert_eq!(
        pg_literal_cmp(&Literal::Int(1), CmpOp::Eq, &Literal::Text(Arc::from("1"))),
        None
    );
    assert_eq!(
        pg_literal_cmp(&Literal::Int(1), CmpOp::ImageEq, &Literal::Int(1)),
        None
    );
}

/// v1.59: NULLIF folds per PG's CASE semantics.
#[test]
fn v159_fold_nullif() {
    // Equal args → NULL.
    assert_eq!(
        pg_fold_const_item(&nullif(int(1), int(1)), &mut PgConstFold::pure()),
        Some(Literal::Null)
    );
    // Unequal args → first arg.
    assert_eq!(
        pg_fold_const_item(&nullif(int(1), int(2)), &mut PgConstFold::pure()),
        Some(Literal::Int(1))
    );
    // A NULL argument never makes the equality true → first arg
    // (NULLIF is not strict).
    assert_eq!(
        pg_fold_const_item(&nullif(int(1), null()), &mut PgConstFold::pure()),
        Some(Literal::Int(1))
    );
    assert_eq!(
        pg_fold_const_item(&nullif(null(), int(1)), &mut PgConstFold::pure()),
        Some(Literal::Null)
    );
    // Name matching is case-insensitive; wrong arity fails closed.
    assert_eq!(
        pg_fold_const_item(
            &Expr::Func {
                name: "NULLIF".to_string(),
                args: vec![int(1), int(1)],
            },
            &mut PgConstFold::pure()
        ),
        Some(Literal::Null)
    );
    assert_eq!(
        pg_fold_const_item(
            &Expr::Func {
                name: "nullif".to_string(),
                args: vec![int(1)],
            },
            &mut PgConstFold::pure()
        ),
        None
    );
}

/// v1.59: the three case.out target quals fold to false.
#[test]
fn v159_fold_bool_const_targets() {
    // NULLIF(1, 2) = 2 → 1 = 2 → false.
    let e1 = cmp(CmpOp::Eq, nullif(int(1), int(2)), int(2));
    assert_eq!(
        pg_fold_bool_const(&e1, &mut PgConstFold::pure()),
        Some(Some(false))
    );
    assert!(pg_is_const_false(&e1, &mut PgConstFold::pure()));
    // NULLIF(1, 1) IS NOT NULL → NULL IS NOT NULL → false.
    let e2 = is_null(nullif(int(1), int(1)), true);
    assert_eq!(
        pg_fold_bool_const(&e2, &mut PgConstFold::pure()),
        Some(Some(false))
    );
    assert!(pg_is_const_false(&e2, &mut PgConstFold::pure()));
    // NULLIF(1, null) = 2 → 1 = 2 → false.
    let e3 = cmp(CmpOp::Eq, nullif(int(1), null()), int(2));
    assert_eq!(
        pg_fold_bool_const(&e3, &mut PgConstFold::pure()),
        Some(Some(false))
    );
    assert!(pg_is_const_false(&e3, &mut PgConstFold::pure()));
}

/// v1.59: strictness and fail-closed behavior.
#[test]
fn v159_fold_bool_const_strict() {
    // NULL = 2 → NULL; PG treats a NULL qual as dummy ("constant NULL
    // is as good as constant FALSE", joinrels.c) so pg_is_const_false
    // is true — but the fold itself reports NULL, not false.
    let e = cmp(CmpOp::Eq, null(), int(2));
    assert_eq!(pg_fold_bool_const(&e, &mut PgConstFold::pure()), Some(None));
    assert!(pg_is_const_false(&e, &mut PgConstFold::pure()));
    // A true constant is not const-false.
    let t = cmp(CmpOp::Eq, int(2), int(2));
    assert_eq!(
        pg_fold_bool_const(&t, &mut PgConstFold::pure()),
        Some(Some(true))
    );
    assert!(!pg_is_const_false(&t, &mut PgConstFold::pure()));
    // Non-constant operands fail closed.
    let col = Expr::Column {
        table: None,
        name: "two".to_string(),
    };
    let nc = cmp(CmpOp::Eq, col, int(2));
    assert_eq!(pg_fold_bool_const(&nc, &mut PgConstFold::pure()), None);
    assert!(!pg_is_const_false(&nc, &mut PgConstFold::pure()));
    // IS NULL on a non-null constant.
    assert_eq!(
        pg_fold_bool_const(&is_null(int(1), false), &mut PgConstFold::pure()),
        Some(Some(false))
    );
    assert_eq!(
        pg_fold_bool_const(&is_null(null(), false), &mut PgConstFold::pure()),
        Some(Some(true))
    );
}
