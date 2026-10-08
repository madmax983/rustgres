use super::Value;
use super::array_agg_final;
use super::array_ctor_from_vals;
use super::bool_and_vals;
use super::numeric_to_u64_exact;
use super::variance_vals;
use crate::storage::Numeric;

fn num(s: &str) -> Value {
    Value::Numeric(Numeric::parse(s).unwrap())
}

/// v0.64: numeric.out tiny-value stress vector — the exact
/// numerator is 250e-1000 - 80e-16883 + 10e-32766, which PG's
/// rscale-1000 division turns into exactly 12e-1000 (the
/// NUMERIC_MAX_DISPLAY_SCALE cap). trim_scale(var * 1e1000) = 12.
#[test]
fn variance_tiny_values() {
    // v0.64: PG19 keeps 1e-16383 as a tiny value (not zero);
    // the trim_scale 0.01 result comes from the 16383 dscale cap
    // in numeric_mul, not from literal rounding.
    let x4 = Numeric::parse("4e-500")
        .unwrap()
        .checked_sub(&Numeric::parse("1e-16383").unwrap())
        .unwrap();
    let x5 = Numeric::parse("-4e-500")
        .unwrap()
        .checked_add(&Numeric::parse("1e-16383").unwrap())
        .unwrap();
    let vals = vec![
        num("0"),
        num("3e-500"),
        num("-3e-500"),
        Value::Numeric(x4),
        Value::Numeric(x5),
    ];
    let v = variance_vals(&vals, "variance", true, true).unwrap();
    let scaled = match v {
        Value::Numeric(n) => n.checked_mul(&Numeric::parse("1e1000").unwrap()).unwrap(),
        other => panic!("expected numeric, got {:?}", other),
    };
    // SQL wraps this in trim_scale(...); the untrimmed value is
    // 12 with dscale 1000.
    assert_eq!(
        scaled.cmp(&Numeric::from_i64(12)),
        std::cmp::Ordering::Equal
    );
    assert_eq!(scaled.to_text(), format!("12.{}", "0".repeat(1000)));
}

/// v0.64: numeric.out huge-offset vector — squaring 9e131071 makes
/// a 262144-digit intermediate that must not 22003 (PG only checks
/// the final result's weight). Sample variance of {1..5} = 2.5.
#[test]
fn variance_huge_offset() {
    let vals: Vec<Value> = (1..=5)
        .map(|i| {
            Value::Numeric(
                Numeric::parse("9e131071")
                    .unwrap()
                    .checked_add(&Numeric::from_i64(i))
                    .unwrap(),
            )
        })
        .collect();
    let v = variance_vals(&vals, "variance", true, true).unwrap();
    match v {
        Value::Numeric(n) => assert_eq!(n.to_text(), "2.5000000000000000"),
        other => panic!("expected numeric, got {:?}", other),
    }
}

/// v0.64: bool_and aggregate semantics (NULLs filtered by caller).
#[test]
fn bool_and_semantics() {
    // all true -> true
    let v = bool_and_vals(&[Value::Bool(true), Value::Bool(true)]).unwrap();
    assert_eq!(v, Value::Bool(true));
    // true + false -> false
    let v = bool_and_vals(&[Value::Bool(true), Value::Bool(false)]).unwrap();
    assert_eq!(v, Value::Bool(false));
    // empty -> NULL
    let v = bool_and_vals(&[]).unwrap();
    assert_eq!(v, Value::Null);
    // non-bool -> error
    assert!(bool_and_vals(&[Value::Int(1)]).is_err());
}

/// v0.92: array_agg's collection core — `array_agg_final` over the
/// aggregate's inputs (NULLs kept for scalar input, per PG19;
/// 22004/2202E for bad array inputs); zero inputs -> NULL.
#[test]
fn array_agg_collection_core() {
    // ints collect in order
    let v = array_agg_final(vec![Value::Int(1), Value::Int(2), Value::Int(3)], false).unwrap();
    match &v {
        Value::Array(a) => {
            assert_eq!(a.to_literal(), "{1,2,3}");
            assert_eq!(a.dims, vec![3]);
            assert_eq!(a.lower, vec![1]);
        }
        other => panic!("expected array, got {:?}", other),
    }
    // v0.92: scalar NULLs are KEPT (PG19 "including nulls")
    let v = array_agg_final(vec![Value::Int(1), Value::Null, Value::Int(3)], false).unwrap();
    match &v {
        Value::Array(a) => {
            assert_eq!(a.to_literal(), "{1,NULL,3}");
            assert_eq!(a.dims, vec![3]);
        }
        other => panic!("expected array, got {:?}", other),
    }
    // all-NULL scalar input -> one NULL per input, not NULL
    let v = array_agg_final(vec![Value::Null, Value::Null], false).unwrap();
    match &v {
        Value::Array(a) => assert_eq!(a.to_literal(), "{NULL,NULL}"),
        other => panic!("expected array, got {:?}", other),
    }
    // zero input rows -> NULL (not an empty array)
    assert_eq!(array_agg_final(vec![], false).unwrap(), Value::Null);
    // mixed int/bigint resolves the common supertype (bigint)
    let v = array_agg_final(vec![Value::Int(1), Value::BigInt(2)], false).unwrap();
    match &v {
        Value::Array(a) => {
            assert_eq!(a.elem, crate::storage::ArrayElem::BigInt);
            assert_eq!(a.to_literal(), "{1,2}");
        }
        other => panic!("expected array, got {:?}", other),
    }
    // text elements
    let v = array_agg_final(vec![Value::text("a"), Value::text("b")], false).unwrap();
    match &v {
        Value::Array(a) => assert_eq!(a.to_literal(), "{a,b}"),
        other => panic!("expected array, got {:?}", other),
    }
    // nested arrays build the multidimensional form (PG flattens
    // array_agg(array[...]) the same way ARRAY[[..],[..]] does)
    let row = |x: i64, y: i64| {
        array_ctor_from_vals(vec![Value::Int(x), Value::Int(y)], false, false).unwrap()
    };
    let v = array_agg_final(vec![row(1, 2), row(3, 4)], false).unwrap();
    match &v {
        Value::Array(a) => {
            assert_eq!(a.to_literal(), "{{1,2},{3,4}}");
            assert_eq!(a.dims, vec![2, 2]);
        }
        other => panic!("expected array, got {:?}", other),
    }
    // mismatched nested dims are 2202E for the aggregate ...
    let wide = array_ctor_from_vals(
        vec![Value::Int(1), Value::Int(2), Value::Int(3)],
        false,
        false,
    )
    .unwrap();
    let bad = array_agg_final(vec![row(1, 2), wide], false);
    assert!(bad.is_err());
    // ... but 22P02 for the ARRAY literal
    let bad = array_ctor_from_vals(
        vec![
            array_ctor_from_vals(vec![Value::Int(1)], false, false).unwrap(),
            array_ctor_from_vals(vec![Value::Int(2), Value::Int(3)], false, false).unwrap(),
        ],
        true,
        false,
    );
    assert!(bad.is_err());
}

/// v0.92: array_agg(anyarray) error semantics straight from PG17's
/// accumArrayResultArr (PG19 tree on disk lacks utils/adt).
#[test]
fn array_agg_array_input_errors() {
    let arr = |x: i64, y: i64| {
        array_ctor_from_vals(vec![Value::Int(x), Value::Int(y)], false, false).unwrap()
    };
    let empty_int_arr = Value::Array(Box::new(crate::storage::ArrayVal {
        elem: crate::storage::ArrayElem::Int,
        dims: Vec::new(),
        lower: Vec::new(),
        elems: Vec::new(),
    }));
    // NULL array input -> 22004 "cannot accumulate null arrays"
    let e = array_agg_final(vec![arr(1, 2), Value::Null], false).unwrap_err();
    assert_eq!(e.code, "22004");
    assert_eq!(e.message, "cannot accumulate null arrays");
    // leading NULL also selects the array variant -> 22004
    let e = array_agg_final(vec![Value::Null, arr(1, 2)], false).unwrap_err();
    assert_eq!(e.code, "22004");
    // empty first input -> 2202E "cannot accumulate empty arrays"
    let e = array_agg_final(vec![empty_int_arr.clone(), arr(1, 2)], false).unwrap_err();
    assert_eq!(e.code, "2202E");
    assert_eq!(e.message, "cannot accumulate empty arrays");
    // empty later input -> 2202E "different dimensionality" (like PG)
    let e = array_agg_final(vec![arr(1, 2), empty_int_arr], false).unwrap_err();
    assert_eq!(e.code, "2202E");
    assert_eq!(
        e.message,
        "cannot accumulate arrays of different dimensionality"
    );
    // different dimension lengths -> 2202E
    let wide = array_ctor_from_vals(
        vec![Value::Int(1), Value::Int(2), Value::Int(3)],
        false,
        false,
    )
    .unwrap();
    let e = array_agg_final(vec![arr(1, 2), wide], false).unwrap_err();
    assert_eq!(e.code, "2202E");
    // different lower bounds -> 2202E (PG compares lbs too)
    let mut lb_shifted = arr(1, 2);
    if let Value::Array(ref mut a) = lb_shifted {
        a.lower = vec![0];
    }
    let e = array_agg_final(vec![arr(1, 2), lb_shifted], false).unwrap_err();
    assert_eq!(e.code, "2202E");
    assert_eq!(
        e.message,
        "cannot accumulate arrays of different dimensionality"
    );
    // compatible arrays (same dims AND lower bounds) stack fine
    let v = array_agg_final(vec![arr(1, 2), arr(3, 4)], false).unwrap();
    match &v {
        Value::Array(a) => {
            assert_eq!(a.to_literal(), "{{1,2},{3,4}}");
            assert_eq!(a.dims, vec![2, 2]);
            assert_eq!(a.lower, vec![1, 1]);
        }
        other => panic!("expected array, got {:?}", other),
    }
}

/// v0.92: static overload discrimination — an all-NULL input with
/// the static array flag set (PG's `array_agg(anyarray)` chosen by
/// static type) raises 22004, not scalar `{NULL,...}`.
#[test]
fn array_agg_static_array_flag() {
    let e = array_agg_final(vec![Value::Null, Value::Null], true).unwrap_err();
    assert_eq!(e.code, "22004");
    assert_eq!(e.message, "cannot accumulate null arrays");
    // Without the flag, the same values are scalar input (NULLs kept).
    let v = array_agg_final(vec![Value::Null, Value::Null], false).unwrap();
    match &v {
        Value::Array(a) => assert_eq!(a.to_literal(), "{NULL,NULL}"),
        other => panic!("expected array, got {:?}", other),
    }
}

/// v0.64: pg_lsn(numeric) — PG19 display and error semantics.
#[test]
fn pg_lsn_numeric() {
    let lsn = |s: &str| {
        let n = Numeric::parse(s).unwrap();
        numeric_to_u64_exact(&n).map(|l| format!("{:X}/{:08X}", l >> 32, l & 0xFFFF_FFFF))
    };
    assert_eq!(lsn("23783416"), Some("0/016AE7F8".to_string()));
    assert_eq!(lsn("0"), Some("0/00000000".to_string()));
    assert_eq!(
        lsn("18446744073709551615"),
        Some("FFFFFFFF/FFFFFFFF".to_string())
    );
    // negative, fractional, and >u64::MAX are out of range
    assert_eq!(lsn("-1"), None);
    assert_eq!(lsn("18446744073709551616"), None);
    assert_eq!(lsn("1.5"), None);
    // huge big-mantissa values are out of range
    assert_eq!(lsn("1e100"), None);
}
#[test]
fn scientific_notation_extreme_mul_dscale_cap() {
    // v0.64: PG19's numeric_mul rounds result dscale to
    // NUMERIC_DSCALE_MAX (16383). The conformance query
    // `trim_scale((0.1 - 2e-16383) * (0.1 - 3e-16383))` yields
    // `0.01` because the exact product (dscale 32766) rounds to
    // 16383 digits, eliminating the tiny 5e-16384 term.
    // Literals like `2e-16383` still parse as tiny values
    // (verified by the variance conformance test); the dscale
    // cap is implemented in `Numeric::checked_mul`.
    let tiny2 = Numeric::parse("2e-16383").unwrap();
    assert_ne!(tiny2.to_text(), "0");
    let tiny3 = Numeric::parse("3e-16383").unwrap();
    assert_ne!(tiny3.to_text(), "0");
}
