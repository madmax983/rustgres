use super::*;
use std::sync::Arc;

/// One-table plan ctx with unqualified column names.
fn pctx(cols: Vec<(&str, ColType)>) -> PgPlanCtx<'static> {
    PgPlanCtx {
        items: vec![(
            vec!["t".to_string()],
            cols.into_iter().map(|(n, t)| (n.to_string(), t)).collect(),
        )],
        pulled_exprs: vec![],
        verbose: false,
        _mark: std::marker::PhantomData,
    }
}

fn col(name: &str) -> Expr {
    Expr::Column {
        table: None,
        name: name.to_string(),
    }
}

fn txt(s: &str) -> Expr {
    Expr::Literal(Literal::Text(Arc::from(s)))
}

fn int(i: i64) -> Expr {
    Expr::Literal(Literal::Int(i))
}

fn eq(l: Expr, r: Expr) -> Expr {
    Expr::Cmp {
        op: CmpOp::Eq,
        left: Box::new(l),
        right: Box::new(r),
    }
}

fn or(l: Expr, r: Expr) -> Expr {
    Expr::Or(Box::new(l), Box::new(r))
}

fn cast_text(e: Expr) -> Expr {
    Expr::Cast {
        expr: Box::new(e),
        to: ColType::Text,
        written: None,
    }
}

/// v1.56: the proven flip — the parser's `stringu1::text IN
/// (VALUES('RFAAAA'),('VJAAAA'))` desugar renders PG19's `= ANY` form.
#[test]
fn v156_fold_cast_text_in_list() {
    let ctx = pctx(vec![("stringu1", ColType::Text)]);
    let lhs = || cast_text(col("stringu1"));
    // Left-deep Or in source order, like the parser builds.
    let e = or(eq(lhs(), txt("RFAAAA")), eq(lhs(), txt("VJAAAA")));
    assert_eq!(
        pg_expr_text(&e, &ctx, false).unwrap(),
        "((stringu1)::text = ANY ('{RFAAAA,VJAAAA}'::text[]))"
    );
}

/// v1.56: plain integer column IN-list.
#[test]
fn v156_fold_int_list() {
    let ctx = pctx(vec![("unique1", ColType::Int)]);
    let e = or(
        or(eq(col("unique1"), int(1)), eq(col("unique1"), int(2))),
        eq(col("unique1"), int(3)),
    );
    assert_eq!(
        pg_expr_text(&e, &ctx, false).unwrap(),
        "(unique1 = ANY ('{1,2,3}'::integer[]))"
    );
}

/// v1.56: PG19 select_common_type widens int4+int8 to int8.
#[test]
fn v156_fold_bigint_widen() {
    let ctx = pctx(vec![("unique1", ColType::Int)]);
    let e = or(
        eq(col("unique1"), int(1)),
        eq(
            col("unique1"),
            Expr::Literal(Literal::BigInt(3_000_000_000)),
        ),
    );
    assert_eq!(
        pg_expr_text(&e, &ctx, false).unwrap(),
        "(unique1 = ANY ('{1,3000000000}'::bigint[]))"
    );
}

/// v1.56: no fold for a single comparison (PG needs >1 items).
#[test]
fn v156_no_fold_single_cmp() {
    let ctx = pctx(vec![("unique1", ColType::Int)]);
    let e = eq(col("unique1"), int(1));
    assert!(pg_fold_in_any(&e, &ctx, false).is_none());
    assert_eq!(pg_expr_text(&e, &ctx, false).unwrap(), "(unique1 = 1)");
}

/// v1.56: no fold when the LHS differs across disjuncts.
#[test]
fn v156_no_fold_mixed_lhs() {
    let ctx = pctx(vec![("a", ColType::Int), ("b", ColType::Int)]);
    let e = or(eq(col("a"), int(1)), eq(col("b"), int(2)));
    assert!(pg_fold_in_any(&e, &ctx, false).is_none());
}

/// v1.56: no fold when an item isn't a literal (PG ORs Vars on).
#[test]
fn v156_no_fold_non_literal_rhs() {
    let ctx = pctx(vec![("a", ColType::Int), ("b", ColType::Int)]);
    let e = or(eq(col("a"), int(1)), eq(col("a"), col("b")));
    assert!(pg_fold_in_any(&e, &ctx, false).is_none());
}

/// v1.56: no fold for non-`=` operators.
#[test]
fn v156_no_fold_non_eq_op() {
    let ctx = pctx(vec![("a", ColType::Int)]);
    let lt = |l: Expr, r: Expr| Expr::Cmp {
        op: CmpOp::Lt,
        left: Box::new(l),
        right: Box::new(r),
    };
    let e = or(lt(col("a"), int(1)), lt(col("a"), int(2)));
    assert!(pg_fold_in_any(&e, &ctx, false).is_none());
}

/// v1.56: no fold for a `name` column — namein truncates to 63 bytes,
/// which we don't replicate (fail closed, stays EF).
#[test]
fn v156_no_fold_name_col() {
    let ctx = pctx(vec![("stringu1", ColType::Name)]);
    let e = or(eq(col("stringu1"), txt("a")), eq(col("stringu1"), txt("b")));
    assert!(pg_fold_in_any(&e, &ctx, false).is_none());
}

/// v1.56: PG19 array_out element quoting rules.
#[test]
fn v156_quote_elem_cases() {
    let cases = [
        ("RFAAAA", "RFAAAA"),   // plain: bare
        ("", "\"\""),           // empty: quoted
        ("null", "\"null\""),   // case-insensitive NULL: quoted
        ("NULL", "\"NULL\""),   // case-insensitive NULL: quoted
        ("nulLx", "nulLx"),     // merely containing "null": bare
        ("b,c", "\"b,c\""),     // delimiter: quoted
        ("a{b", "\"a{b\""),     // braces: quoted
        ("a}b", "\"a}b\""),     // braces: quoted
        ("a\"b", "\"a\\\"b\""), // quote: quoted + escaped
        ("a\\b", "\"a\\\\b\""), // backslash: quoted + escaped
        ("a b", "\"a b\""),     // space: quoted
        ("a\tb", "\"a\tb\""),   // tab: quoted
    ];
    for (input, want) in cases {
        assert_eq!(pg_array_quote_elem(input), want, "input {input:?}");
    }
}

/// v1.56: array element rendering per type.
#[test]
fn v156_elem_text_cases() {
    use ColType::*;
    let t = |s: &str| Literal::Text(Arc::from(s));
    // NULL prints bare (array_out).
    assert_eq!(pg_array_elem_text(&Literal::Null, Text).unwrap(), "NULL");
    // Unknown literal coerced to int4: int4out.
    assert_eq!(pg_array_elem_text(&t("42"), Int).unwrap(), "42");
    // Unknown literal coerced to bool: boolout is t/f.
    assert_eq!(pg_array_elem_text(&t("true"), Bool).unwrap(), "t");
    assert_eq!(pg_array_elem_text(&t("FALSE"), Bool).unwrap(), "f");
    assert_eq!(pg_array_elem_text(&t("maybe"), Bool).is_none(), true);
    // Int item into numeric: plain digits.
    assert_eq!(
        pg_array_elem_text(&Literal::Int(7), Numeric(None)).unwrap(),
        "7"
    );
    // Int item into float8 below 1e15: plain digits.
    assert_eq!(pg_array_elem_text(&Literal::Int(7), Float).unwrap(), "7");
    // Int item into float8 at/above 1e15: PG uses scientific — fail closed.
    assert_eq!(
        pg_array_elem_text(&Literal::Int(1_000_000_000_000_000), Float).is_none(),
        true
    );
    // Decimal into numeric: plain spelling preserved.
    assert_eq!(
        pg_array_elem_text(&Literal::Decimal("1.50".to_string()), Numeric(None)).unwrap(),
        "1.50"
    );
    // Decimal with exponent: numeric_out normalizes — fail closed.
    assert_eq!(
        pg_array_elem_text(&Literal::Decimal("1.5e3".to_string()), Numeric(None)).is_none(),
        true
    );
    // Text into float8: fail closed (float8out thresholds unreplicated).
    assert_eq!(pg_array_elem_text(&t("1.5"), Float).is_none(), true);
    // Int into text: no implicit coercion — fail closed (PG keeps the OR).
    assert_eq!(pg_array_elem_text(&Literal::Int(1), Text).is_none(), true);
}

/// v1.56 (ride-along fix #2): an untyped text literal compared to a
/// Cast-wrapped column takes the cast's type — previously the whole
/// comparison fell to a Debug-fallback EF.
#[test]
fn v156_cmp_cast_text_literal_coerces() {
    let ctx = pctx(vec![("stringu1", ColType::Text)]);
    let e = eq(cast_text(col("stringu1")), txt("RFAAAA"));
    assert_eq!(
        pg_expr_text(&e, &ctx, false).unwrap(),
        "((stringu1::text) = 'RFAAAA'::text)"
    );
}

/// v1.56 (ride-along fix #1): qualify_expr_cols recurses into Func
/// args, Cmp sides, And and Or — not just Cast.
#[test]
fn v156_qualify_recurses_composites() {
    let mut eng = Engine::new();
    let stmt = parse_statement("CREATE TABLE q1 (a integer)").unwrap();
    let snap0 = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut sctx = StmtCtx {
        snap: &snap0,
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
    execute(&mut eng, &mut sctx, &stmt).unwrap();
    let snap = eng.take_snapshot();
    let from = vec![FromItem::Table {
        name: "q1".to_string(),
        alias: None,
        col_aliases: vec![],
        only: false,
    }];
    let qual_col = || Expr::Column {
        table: Some("q1".to_string()),
        name: "a".to_string(),
    };
    // Func args.
    let e = Expr::Func {
        name: "f".to_string(),
        args: vec![col("a")],
    };
    assert_eq!(
        qualify_expr_cols(&e, &from, &eng, &snap, 9, 0),
        Expr::Func {
            name: "f".to_string(),
            args: vec![qual_col()],
        }
    );
    // Cmp sides.
    let e = eq(col("a"), int(1));
    assert_eq!(
        qualify_expr_cols(&e, &from, &eng, &snap, 9, 0),
        eq(qual_col(), int(1))
    );
    // And / Or.
    let e = Expr::And(
        Box::new(eq(col("a"), int(1))),
        Box::new(eq(col("a"), int(2))),
    );
    match qualify_expr_cols(&e, &from, &eng, &snap, 9, 0) {
        Expr::And(a, b) => {
            assert_eq!(*a, eq(qual_col(), int(1)));
            assert_eq!(*b, eq(qual_col(), int(2)));
        }
        other => panic!("expected And, got {other:?}"),
    }
    let e = or(eq(col("a"), int(1)), eq(col("a"), int(2)));
    match qualify_expr_cols(&e, &from, &eng, &snap, 9, 0) {
        Expr::Or(a, b) => {
            assert_eq!(*a, eq(qual_col(), int(1)));
            assert_eq!(*b, eq(qual_col(), int(2)));
        }
        other => panic!("expected Or, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// v1.57: const-folding of IN-list items + coercion-aware LHS deparse.
// ------------------------------------------------------------------

fn dec(s: &str) -> Expr {
    Expr::Literal(Literal::Decimal(s.to_string()))
}

fn func(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Func {
        name: name.to_string(),
        args,
    }
}

/// v1.57: sin(0.5) folds to PG's float8 value, bit-exact
/// (f64::sin is correctly rounded here; verified 2026-10-01).
#[test]
fn v157_fold_sin_const() {
    assert_eq!(
        pg_fold_const_item(&func("sin", vec![dec("0.5")]), &mut PgConstFold::pure()),
        Some(Literal::Float(0.479425538604203))
    );
}

/// v1.57: cos/tan fold; builtin name matching is case-insensitive.
#[test]
fn v157_fold_cos_tan_const() {
    assert_eq!(
        pg_fold_const_item(&func("cos", vec![dec("0.5")]), &mut PgConstFold::pure()),
        Some(Literal::Float(0.8775825618903728))
    );
    assert_eq!(
        pg_fold_const_item(&func("COS", vec![dec("0.5")]), &mut PgConstFold::pure()),
        Some(Literal::Float(0.8775825618903728))
    );
    assert_eq!(
        pg_fold_const_item(&func("tan", vec![dec("0.5")]), &mut PgConstFold::pure()),
        Some(Literal::Float(0.5463024898437905))
    );
}

/// v1.57: strictness — sin(NULL) folds to NULL (PG evaluate_function).
#[test]
fn v157_fold_sin_null_strict() {
    assert_eq!(
        pg_fold_const_item(
            &func("sin", vec![Expr::Literal(Literal::Null)]),
            &mut PgConstFold::pure()
        ),
        Some(Literal::Null)
    );
}

/// v1.57: non-constant arguments don't fold.
#[test]
fn v157_fold_sin_column_no_fold() {
    assert_eq!(
        pg_fold_const_item(&func("sin", vec![col("two")]), &mut PgConstFold::pure()),
        None
    );
}

/// v1.57: unknown builtins, wrong arity, domain-unsafe builtins, and
/// non-finite inputs don't fold (fail closed).
#[test]
fn v157_fold_no_fold_cases() {
    assert_eq!(
        pg_fold_const_item(&func("foobar", vec![dec("0.5")]), &mut PgConstFold::pure()),
        None
    );
    assert_eq!(
        pg_fold_const_item(&func("now", vec![]), &mut PgConstFold::pure()),
        None
    );
    // asin is immutable but domain-checked (|x|>1 raises in PG);
    // conservatively unfolded.
    assert_eq!(
        pg_fold_const_item(&func("asin", vec![dec("0.5")]), &mut PgConstFold::pure()),
        None
    );
    assert_eq!(
        pg_fold_const_item(
            &func("sin", vec![dec("0.5"), dec("0.5")]),
            &mut PgConstFold::pure()
        ),
        None
    );
    // Infinite input: PG's dsin raises.
    assert_eq!(
        pg_fold_const_item(
            &func("sin", vec![Expr::Literal(Literal::Float(f64::INFINITY))]),
            &mut PgConstFold::pure()
        ),
        None
    );
    // NaN input: no faithful array spelling; fail closed.
    assert_eq!(
        pg_fold_const_item(
            &func("sin", vec![Expr::Literal(Literal::Float(f64::NAN))]),
            &mut PgConstFold::pure()
        ),
        None
    );
}

/// v1.57: CAST folding — exact casts only.
#[test]
fn v157_fold_cast_cases() {
    let cast = |e: Expr, to: ColType| Expr::Cast {
        expr: Box::new(e),
        to,
        written: None,
    };
    assert_eq!(
        pg_fold_const_item(&cast(int(2), ColType::Int), &mut PgConstFold::pure()),
        Some(Literal::Int(2))
    );
    assert_eq!(
        pg_fold_const_item(&cast(int(2), ColType::Float), &mut PgConstFold::pure()),
        Some(Literal::Float(2.0))
    );
    // |i| >= 2^53: int8->float8 rounds in PG too — fail closed.
    assert_eq!(
        pg_fold_const_item(
            &cast(Expr::Literal(Literal::Int(1i64 << 53)), ColType::Float),
            &mut PgConstFold::pure()
        ),
        None
    );
    // decimal -> float8 (correctly rounded, like numeric_float8).
    assert_eq!(
        pg_fold_const_item(&cast(dec("0.5"), ColType::Float), &mut PgConstFold::pure()),
        Some(Literal::Float(0.5))
    );
    // int -> text: no faithful fold.
    assert_eq!(
        pg_fold_const_item(&cast(int(2), ColType::Text), &mut PgConstFold::pure()),
        None
    );
    // NULL -> anything: NULL.
    assert_eq!(
        pg_fold_const_item(
            &cast(Expr::Literal(Literal::Null), ColType::Float),
            &mut PgConstFold::pure()
        ),
        Some(Literal::Null)
    );
}

/// v1.57: LHS type inference.
#[test]
fn v157_infer_type_cases() {
    let ctx = pctx(vec![
        ("two", ColType::Int),
        ("four", ColType::Int),
        ("f", ColType::Float),
    ]);
    assert_eq!(pg_infer_type(&col("two"), &ctx), Some(ColType::Int));
    assert_eq!(
        pg_infer_type(&func("sin", vec![col("two")]), &ctx),
        Some(ColType::Float)
    );
    // float8 + int -> float8 (numeric lattice max rank).
    let e = Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(func("sin", vec![col("two")])),
        right: Box::new(col("four")),
    };
    assert_eq!(pg_infer_type(&e, &ctx), Some(ColType::Float));
    // int + int -> int (native cross-type operator, wider wins).
    let e = Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(col("two")),
        right: Box::new(col("four")),
    };
    assert_eq!(pg_infer_type(&e, &ctx), Some(ColType::Int));
    // Unknown builtin -> None.
    assert_eq!(pg_infer_type(&func("foobar", vec![col("two")]), &ctx), None);
    // % is not handled -> None.
    let e = Expr::Arith {
        op: ArithOp::Mod,
        left: Box::new(col("two")),
        right: Box::new(col("four")),
    };
    assert_eq!(pg_infer_type(&e, &ctx), None);
}

/// v1.57: coercion rewrite inserts PG's implicit casts for the
/// `sin(two)+four` LHS; the existing Cast arm renders `(x)::type`.
#[test]
fn v157_rewrite_coercions_sin_plus() {
    let ctx = pctx(vec![("two", ColType::Int), ("four", ColType::Int)]);
    let e = Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(func("sin", vec![col("two")])),
        right: Box::new(col("four")),
    };
    let rw = pg_rewrite_coercions(&e, &ctx).unwrap();
    assert_eq!(
        pg_expr_text(&rw, &ctx, false).unwrap(),
        "(sin((two)::double precision) + (four)::double precision)"
    );
}

/// v1.57: native cross-type int operators get no casts.
#[test]
fn v157_rewrite_coercions_int_pair_no_cast() {
    let ctx = pctx(vec![("a", ColType::SmallInt), ("b", ColType::Int)]);
    let e = Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(col("a")),
        right: Box::new(col("b")),
    };
    let rw = pg_rewrite_coercions(&e, &ctx).unwrap();
    assert_eq!(pg_expr_text(&rw, &ctx, false).unwrap(), "(a + b)");
}

/// v1.57: end-to-end — subselect L3393 shape folds to PG's exact text.
#[test]
fn v157_fold_in_any_sin_items() {
    let ctx = pctx(vec![("two", ColType::Int), ("four", ColType::Int)]);
    let lhs = || Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(func("sin", vec![col("two")])),
        right: Box::new(col("four")),
    };
    let e = or(eq(lhs(), func("sin", vec![dec("0.5")])), eq(lhs(), int(2)));
    assert_eq!(
        pg_fold_in_any(&e, &ctx, false).unwrap(),
        "((sin((two)::double precision) + (four)::double precision) = ANY ('{0.479425538604203,2}'::double precision[]))"
    );
}

/// v1.57: subselect L3402 shape — NULL item renders bare.
#[test]
fn v157_fold_in_any_sin_null_item() {
    let ctx = pctx(vec![("two", ColType::Int), ("four", ColType::Int)]);
    let lhs = || Expr::Arith {
        op: ArithOp::Add,
        left: Box::new(func("sin", vec![col("two")])),
        right: Box::new(col("four")),
    };
    let e = or(
        or(
            eq(lhs(), func("sin", vec![dec("0.5")])),
            eq(lhs(), Expr::Literal(Literal::Null)),
        ),
        eq(lhs(), int(2)),
    );
    assert_eq!(
        pg_fold_in_any(&e, &ctx, false).unwrap(),
        "((sin((two)::double precision) + (four)::double precision) = ANY ('{0.479425538604203,NULL,2}'::double precision[]))"
    );
}

/// v1.57: int widening still shows no LHS cast — PG uses its native
/// cross-type `=` (int48eq etc.; verified against subselect.out:3355
/// `(unique1 = ANY ('{1200,1}'::bigint[]))`).
#[test]
fn v157_fold_in_any_int_widening_no_lhs_cast() {
    let ctx = pctx(vec![("unique1", ColType::Int)]);
    let e = or(
        eq(col("unique1"), int(1)),
        eq(
            col("unique1"),
            Expr::Literal(Literal::BigInt(3_000_000_000)),
        ),
    );
    assert_eq!(
        pg_fold_in_any(&e, &ctx, false).unwrap(),
        "(unique1 = ANY ('{1,3000000000}'::bigint[]))"
    );
}

/// v1.57: a volatile item kills the fold (fail closed, OR form kept).
#[test]
fn v157_no_fold_volatile_item() {
    let ctx = pctx(vec![("a", ColType::Float)]);
    let e = or(
        eq(col("a"), func("sin", vec![dec("0.5")])),
        eq(col("a"), func("random", vec![])),
    );
    assert!(pg_fold_in_any(&e, &ctx, false).is_none());
}

/// v1.57: a non-constant (Var) item kills the fold.
#[test]
fn v157_no_fold_var_item() {
    let ctx = pctx(vec![("a", ColType::Float), ("b", ColType::Float)]);
    let e = or(
        eq(col("a"), func("sin", vec![dec("0.5")])),
        eq(col("a"), col("b")),
    );
    assert!(pg_fold_in_any(&e, &ctx, false).is_none());
}

/// v1.57: a computed LHS whose type can't be inferred kills the fold.
#[test]
fn v157_no_fold_uninferable_lhs() {
    let ctx = pctx(vec![("a", ColType::Float)]);
    let lhs = || Expr::Arith {
        op: ArithOp::Mod,
        left: Box::new(col("a")),
        right: Box::new(int(2)),
    };
    let e = or(eq(lhs(), func("sin", vec![dec("0.5")])), eq(lhs(), int(2)));
    assert!(pg_fold_in_any(&e, &ctx, false).is_none());
}
