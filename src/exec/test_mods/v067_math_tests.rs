
use super::*;

#[test]
fn round_float8_returns_float8() {
    // v0.67: PG19's round(float8) -> float8 (dround = rint, half
    // to even); the old code routed float8 through numeric and
    // returned a 200-digit numeric for round(1e200::float8).
    match eval_math_func("round", &[Value::Float(1e200)]).unwrap() {
        Value::Float(f) => assert_eq!(f, 1e200),
        v => panic!("round(1e200::float8) must be Float, got {v:?}"),
    }
    // Half to even, like C rint.
    match eval_math_func("round", &[Value::Float(2.5)]).unwrap() {
        Value::Float(f) => assert_eq!(f, 2.0),
        v => panic!("round(2.5::float8) must be Float(2.0), got {v:?}"),
    }
    match eval_math_func("round", &[Value::Float(3.5)]).unwrap() {
        Value::Float(f) => assert_eq!(f, 4.0),
        v => panic!("round(3.5::float8) must be Float(4.0), got {v:?}"),
    }
    // float4 coerces to float8 like PG's parser.
    match eval_math_func("round", &[Value::Float4(2.5)]).unwrap() {
        Value::Float(f) => assert_eq!(f, 2.0),
        v => panic!("round(2.5::float4) must be Float(2.0), got {v:?}"),
    }
    // Numeric inputs are untouched.
    match eval_math_func("round", &[Value::Numeric(Numeric::parse("2.5").unwrap())]).unwrap() {
        Value::Numeric(n) => assert_eq!(n.to_text(), "3"),
        v => panic!("round(2.5::numeric) must be Numeric(3), got {v:?}"),
    }
}
