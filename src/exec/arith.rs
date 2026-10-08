// v1.78 mechanical split: moved verbatim from src/exec.rs (40783-41729).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// v0.2: arithmetic (kept)
// ---------------------------------------------------------------------------

/// A value's type for `+` resolution; NULL contributes no constraint.
// ---------------------------------------------------------------------------
// v0.7 arithmetic, casts, operators, and built-in functions
// ---------------------------------------------------------------------------

/// Numeric category for promotion. Declaration order IS the promotion
/// lattice: smallint < integer < bigint < real < double precision <
/// v0.21: Postgres numeric promotion order — smallint < integer <
/// bigint < numeric < real < double precision — so float beats
/// numeric in `.max()` promotion.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NumCat {
    Small,
    Int,
    Big,
    Numeric,
    Real,
    Double,
}

pub(crate) fn num_cat(v: &Value) -> Option<NumCat> {
    match v {
        Value::SmallInt(_) => Some(NumCat::Small),
        Value::Int(_) => Some(NumCat::Int),
        Value::BigInt(_) => Some(NumCat::Big),
        Value::Float4(_) => Some(NumCat::Real),
        Value::Float(_) => Some(NumCat::Double),
        Value::Numeric(_) => Some(NumCat::Numeric),
        _ => None,
    }
}

/// v0.21: the `ColType` matching a numeric `Value`, for text-operand
/// coercion in operators.
pub(crate) fn num_col_type(v: &Value) -> Option<ColType> {
    match v {
        Value::SmallInt(_) => Some(ColType::SmallInt),
        Value::Int(_) => Some(ColType::Int),
        Value::BigInt(_) => Some(ColType::BigInt),
        Value::Float4(_) => Some(ColType::Float4),
        Value::Float(_) => Some(ColType::Float),
        Value::Numeric(_) => Some(ColType::Numeric(None)),
        _ => None,
    }
}

/// v0.21: when exactly one side of an arithmetic/comparison operator is
/// `Text` and the other side is numeric-kind, parse the text as the
/// other side's type — Postgres resolves unknown-type literals this
/// way (`1.5 = '1.5'` is true). Unparseable text is 22P02; anything
/// else passes through untouched.
/// `None` means neither operand needed coercion; the caller keeps
/// borrowing its original `&Value`s instead of paying for a clone of
/// both sides on every call (the common case: typed columns, no `Text`
/// operand at all).
pub(crate) fn coerce_text_numeric(
    a: &Value,
    b: &Value,
) -> Result<Option<(Value, Value)>, ExecError> {
    let coerced = match (a, b) {
        (Value::Text(s), o) => num_col_type(o).map(|t| (s, t, true)),
        (o, Value::Text(s)) => num_col_type(o).map(|t| (s, t, false)),
        _ => None,
    };
    match coerced {
        Some((s, t, text_first)) => {
            let parsed = eval_cast(&Value::text(s.clone()), t)?;
            Ok(Some(if text_first {
                (parsed, b.clone())
            } else {
                (a.clone(), parsed)
            }))
        }
        None => Ok(None),
    }
}

/// v0.21: Postgres float range checks on an arithmetic result `r`
/// computed from `x`, `y`: an infinite result from finite inputs is
/// 22003 (overflow); an exact-zero result from nonzero inputs is 22003
/// (underflow). Infinities propagate per IEEE when an input was
/// infinite.
pub(crate) fn check_float_arith(r: f64, x: f64, y: f64, ty: &'static str) -> Result<(), ExecError> {
    if r.is_infinite() && x.is_finite() && y.is_finite() {
        return Err(exec_err(
            "22003",
            format!("value out of range for type {}", ty),
        ));
    }
    // v0.21: underflow to zero from finite nonzero inputs (e.g. 1e-30^2 in
    // float4). Infinite inputs legitimately produce zero (42/inf = 0).
    if r == 0.0 && x != 0.0 && y != 0.0 && x.is_finite() && y.is_finite() {
        return Err(exec_err(
            "22003",
            format!("value out of range for type {}", ty),
        ));
    }
    Ok(())
}

pub(crate) fn op_err(op: ArithOp, a: &Value, b: &Value) -> ExecError {
    exec_err(
        "42883",
        format!(
            "operator does not exist: {} {} {}",
            a.type_name(),
            op.sql(),
            b.type_name()
        ),
    )
}

/// Exact numeric -> Numeric. Floats that don't fit (NaN/Inf) -> None.
pub(crate) fn to_numeric_opt(v: &Value) -> Option<Numeric> {
    match v {
        Value::SmallInt(i) => Some(Numeric::new(*i as i128, 0)),
        Value::Int(i) => Some(Numeric::new(*i as i128, 0)),
        Value::BigInt(i) => Some(Numeric::new(*i as i128, 0)),
        Value::Numeric(n) => Some(n.clone()),
        Value::Float4(f) => Numeric::from_f64(*f as f64).ok(),
        Value::Float(f) => Numeric::from_f64(*f).ok(),
        // v0.25: untyped literals in a numeric function position (e.g.
        // `power('inf', '2')`) coerce through numeric's input function.
        Value::Text(s) => Numeric::parse(s).ok(),
        _ => None,
    }
}

pub(crate) fn to_i128(v: &Value) -> i128 {
    match v {
        Value::SmallInt(i) => *i as i128,
        Value::Int(i) => *i as i128,
        Value::BigInt(i) => *i as i128,
        _ => 0,
    }
}

pub(crate) fn to_f64v(v: &Value) -> f64 {
    match v {
        Value::SmallInt(i) => *i as f64,
        Value::Int(i) => *i as f64,
        Value::BigInt(i) => *i as f64,
        Value::Numeric(n) => n.to_f64(),
        Value::Float4(f) => *f as f64,
        Value::Float(f) => *f,
        _ => f64::NAN,
    }
}

/// v0.18: Simple xorshift64* PRNG for random()/setseed().
/// Seed stored in a static AtomicU64.
pub(crate) static RANDOM_SEED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0x853c49e6748fea9b);

pub(crate) fn random_f64() -> f64 {
    use std::sync::atomic::Ordering;
    let mut s = RANDOM_SEED.load(Ordering::Relaxed);
    // xorshift64*.
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    RANDOM_SEED.store(s, Ordering::Relaxed);
    let r = s.wrapping_mul(0x2545F4914F6CDD1D);
    // Convert to [0,1): use top 53 bits.
    ((r >> 11) as f64) / ((1u64 << 53) as f64)
}

pub(crate) fn set_random_seed(seed: f64) {
    use std::sync::atomic::Ordering;
    // PG: setseed(float) in [-1,1]; hash to u64.
    let bits = (seed * 9.223372036854776e18) as i64 as u64;
    let s = if bits == 0 { 0x853c49e6748fea9b } else { bits };
    RANDOM_SEED.store(s, Ordering::Relaxed);
}

/// v0.18: width_bucket(op, b1, b2, count) and width_bucket(op, thresholds).
/// Returns integer bucket number (0..count+1, or 1-based index into thresholds).
/// v0.56: one width_bucket argument in PG19-exact form.
#[derive(Clone)]
pub(crate) enum WbArg {
    /// Finite value as an exact decimal.
    Dec(crate::storage::BigDec),
    /// NaN (float or numeric).
    Nan,
    /// Infinite; the bool is `is_positive`.
    Inf(bool),
}

/// v0.56: exact form of a float8/float4 argument. NaN/infinity stay
/// special; finite values use the shortest round-trip decimal spelling
/// (what PG's float8out feeds numeric_in), parsed exactly. The
/// regression tests prove the float8 variant is not naive float64
/// arithmetic: width_bucket(0, -1e100::float8, 1, 10) = 10, where
/// float64 would round 1e100/(1e100+1) to 1.0 and yield 11.
pub(crate) fn wb_float_arg(x: f64) -> WbArg {
    use crate::storage::{BigDec, Numeric};
    if x.is_nan() {
        WbArg::Nan
    } else if x.is_infinite() {
        WbArg::Inf(x > 0.0)
    } else {
        // {:e} is shortest-round-trip like {} but always carries an
        // exponent, so 1e100 never overflows Numeric's i128 unscaled.
        // Infallible for finite x; the fallback is defensive.
        match Numeric::parse(&format!("{:e}", x))
            .ok()
            .and_then(|n| BigDec::from_numeric(&n))
        {
            Some(d) => WbArg::Dec(d),
            None => WbArg::Dec(BigDec::zero()),
        }
    }
}

/// v0.56: exact form of a numeric argument.
pub(crate) fn wb_numeric_arg(n: &crate::storage::Numeric) -> WbArg {
    use crate::storage::{BigDec, NumericSpecial};
    match n.special {
        NumericSpecial::NaN => WbArg::Nan,
        NumericSpecial::PosInf => WbArg::Inf(true),
        NumericSpecial::NegInf => WbArg::Inf(false),
        NumericSpecial::Finite => WbArg::Dec(BigDec::from_numeric(n).unwrap_or_else(BigDec::zero)),
    }
}

/// v0.56: convert one width_bucket operand/bound to its exact form.
/// Integers are exact; numerics keep their value; floats go through
/// their shortest decimal spelling; untyped literals coerce through
/// numeric's input function (with an exact fallback for literals too
/// huge for Numeric's i128).
pub(crate) fn wb_arg(name: &str, v: &Value) -> Result<WbArg, ExecError> {
    use crate::storage::{BigDec, DecimalParseError, Numeric};
    match v {
        Value::Float4(f) => Ok(wb_float_arg(*f as f64)),
        Value::Float(f) => Ok(wb_float_arg(*f)),
        Value::SmallInt(i) => Ok(WbArg::Dec(BigDec::from_i64(*i as i64))),
        Value::Int(i) => Ok(WbArg::Dec(BigDec::from_i64(*i))),
        Value::BigInt(i) => Ok(WbArg::Dec(BigDec::from_i64(*i))),
        Value::Numeric(n) => Ok(wb_numeric_arg(n)),
        Value::Text(s) => match Numeric::parse(s) {
            Ok(n) => Ok(wb_numeric_arg(&n)),
            Err(_) => match BigDec::parse_decimal(s) {
                Ok(d) => Ok(WbArg::Dec(d)),
                Err(DecimalParseError::TooBig) => {
                    Err(exec_err("22003", "value overflows numeric format"))
                }
                Err(DecimalParseError::Syntax) => Err(exec_err(
                    "22P02",
                    format!("invalid input syntax for type numeric: {s}"),
                )),
            },
        },
        // NULL is handled by the caller (strict); anything else cannot
        // appear in a numeric argument position.
        other => Err(func_arg_err(name, other)),
    }
}

/// v0.56: exact in-range bucket, floor(lower/upper * count) + 1, where
/// lower and upper are non-negative finite decimals and upper > 0.
/// Errors 22003 when the bucket exceeds int4 range.
pub(crate) fn wb_bucket_in_range(
    lower: &crate::storage::BigDec,
    upper: &crate::storage::BigDec,
    count: u64,
) -> Result<u64, ExecError> {
    use crate::storage::BigUint;
    use std::cmp::Ordering;
    debug_assert!(lower.cmp(&crate::storage::BigDec::zero()) != Ordering::Less);
    debug_assert!(upper.cmp(&crate::storage::BigDec::zero()) == Ordering::Greater);
    // Align the decimal scales; then everything is integer arithmetic:
    // floor((lower_mag * count) / upper_mag).
    let (mut a, mut b) = (lower.mag().clone(), upper.mag().clone());
    let pad = |from: i32, to: i32| {
        u32::try_from(to as i64 - from as i64)
            .map_err(|_| exec_err("22003", "value overflows numeric format"))
    };
    match lower.scale().cmp(&upper.scale()) {
        // lower = a*10^-sl, upper = b*10^-su, so lower/upper =
        // (a*10^(su-sl))/b: scale up the numerator's magnitude when
        // su > sl, the denominator's when sl > su.
        Ordering::Less => a.mul_pow10_assign(pad(lower.scale(), upper.scale())?),
        Ordering::Greater => b.mul_pow10_assign(pad(upper.scale(), lower.scale())?),
        Ordering::Equal => {}
    }
    a.mul_small_assign(count);
    match BigUint::floor_div_bounded(&a, &b) {
        // floor_div_bounded caps the quotient below 2^31, so this fits.
        Some(q) => Ok(q.to_u64().expect("bounded quotient fits u64") + 1),
        None => Err(exec_err("22003", "integer out of range")),
    }
}

/// v1.25: PG19 `width_bucket_float8` (float.c) ported verbatim to f64.
/// PG resolves all-float8 (operand, bound1, bound2) calls to this
/// overload, which computes in float64 — including the divide-by-2
/// overflow path when the bound difference overflows DBL_MAX, and the
/// "quotient could round to 1.0, which would be a lie" guard. Error
/// order and codes mirror PG: 22023 for count/NaN-bound/inf-bound/
/// equal-bound, 22003 for the count+1 int32 overflow
/// (pg_add_s32_overflow). `count` arrives as i64 because rustgres also
/// accepts int2/int8; buckets above i32::MAX are 22003, matching the
/// exact path's `int_result`.
pub(crate) fn eval_width_bucket_float8(
    operand: f64,
    bound1: f64,
    bound2: f64,
    count: i64,
) -> Result<Value, ExecError> {
    if count <= 0 {
        return Err(exec_err("22023", "count must be greater than zero"));
    }
    if bound1.is_nan() || bound2.is_nan() {
        return Err(exec_err("22023", "lower and upper bounds cannot be NaN"));
    }
    if bound1.is_infinite() || bound2.is_infinite() {
        return Err(exec_err("22023", "lower and upper bounds must be finite"));
    }
    // pg_add_s32_overflow(count, 1): count+1 must fit int32.
    let count_plus_one = || -> Result<Value, ExecError> {
        if count >= i32::MAX as i64 {
            Err(exec_err("22003", "integer out of range"))
        } else {
            Ok(Value::Int(count + 1))
        }
    };
    // PG returns int4: buckets above i32::MAX are 22003.
    let int_result = |bucket: i64| -> Result<Value, ExecError> {
        if bucket > i32::MAX as i64 {
            Err(exec_err("22003", "integer out of range"))
        } else {
            Ok(Value::Int(bucket))
        }
    };
    // C `result = count * quotient` truncates the double product toward
    // zero into int32; the quotient is in [0,1] so no overflow for
    // PG-sized counts. `as i64` truncates toward zero identically.
    let scaled = |quotient: f64| -> i64 {
        let mut result = (count as f64 * quotient) as i64;
        // The quotient could round to 1.0, which would be a lie.
        if result >= count {
            result = count - 1;
        }
        result + 1
    };
    if bound1 < bound2 {
        if operand.is_nan() || operand >= bound2 {
            return count_plus_one();
        } else if operand < bound1 {
            return Ok(Value::Int(0));
        }
        let quotient = if (bound2 - bound1).is_infinite() {
            // bound2 - bound1 overflows DBL_MAX; both bounds are finite
            // so halving all inputs is exact (except negligible
            // tiny-operand underflow — PG's own comment).
            (operand / 2.0 - bound1 / 2.0) / (bound2 / 2.0 - bound1 / 2.0)
        } else {
            (operand - bound1) / (bound2 - bound1)
        };
        int_result(scaled(quotient))
    } else if bound1 > bound2 {
        if operand.is_nan() || operand > bound1 {
            return Ok(Value::Int(0));
        } else if operand <= bound2 {
            return count_plus_one();
        }
        let quotient = if (bound1 - bound2).is_infinite() {
            (bound1 / 2.0 - operand / 2.0) / (bound1 / 2.0 - bound2 / 2.0)
        } else {
            (bound1 - operand) / (bound1 - bound2)
        };
        int_result(scaled(quotient))
    } else {
        Err(exec_err("22023", "lower bound cannot equal upper bound"))
    }
}

/// v0.64: Convert a finite Numeric to u64 exactly; None when the value
/// is negative, has a fractional part, or exceeds u64::MAX (pg_lsn input).
pub(crate) fn numeric_to_u64_exact(n: &crate::storage::Numeric) -> Option<u64> {
    // v0.64: big-mantissa values can still be in-range (e.g. 1e100/1e90
    // = 1e10 after extreme-scale arithmetic). When big.is_some(),
    // unscaled is the sign (-1/1) and big is the magnitude.
    if let Some(mag) = &n.big {
        if n.unscaled < 0 {
            return None;
        }
        if n.scale > 0 {
            let (q, r) = mag.div_rem_pow10(n.scale as u32);
            if !r.is_zero() {
                return None;
            }
            return q.to_u64();
        } else {
            let mut v = (**mag).clone();
            v.mul_pow10_assign((-n.scale) as u32);
            return v.to_u64();
        }
    }
    if n.unscaled < 0 {
        return None;
    }
    if n.scale > 0 {
        let divisor = 10i128.checked_pow(n.scale as u32)?;
        if n.unscaled % divisor != 0 {
            return None;
        }
        u64::try_from(n.unscaled / divisor).ok()
    } else {
        let mult = 10i128.checked_pow((-n.scale) as u32)?;
        let v = n.unscaled.checked_mul(mult)?;
        u64::try_from(v).ok()
    }
}

pub(crate) fn eval_width_bucket(
    op: &Value,
    vals: &[Value],
    name: &str,
) -> Result<Value, ExecError> {
    use std::cmp::Ordering;
    // The 3-argument array-bounds form needs arrays, which rustgres lacks.
    if vals.len() == 3 {
        return Err(exec_err(
            "42883",
            "width_bucket with array thresholds is not supported",
        ));
    }
    if vals.len() != 4 {
        return Err(func_arg_err(name, op));
    }
    // PG's width_bucket is strict: any NULL argument yields NULL (this
    // also covers vals[2]/vals[3], which eval_math_func's NULL check
    // does not see).
    if matches!(op, Value::Null) || vals[1..4].iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    // count is int4 in PG; rustgres also accepts wider integer kinds.
    let count: i64 = match &vals[3] {
        Value::SmallInt(i) => *i as i64,
        Value::Int(i) => *i,
        Value::BigInt(i) => *i,
        other => return Err(func_arg_err(name, other)),
    };
    // v0.56: PG19 reports 22023 here (was 2201F).
    if count <= 0 {
        return Err(exec_err("22023", "count must be greater than zero"));
    }

    // v1.25: PG resolves an all-float8 (operand, bound1, bound2) call to
    // width_bucket_float8 (float.c), which computes in float64 — not the
    // exact decimal arithmetic below. At extreme magnitudes the two
    // disagree (numeric.sql's LATERAL overflow test, row 6: exact yields
    // 1, PG yields 2), so float8 triples take the verbatim PG19 path.
    // Every other shape stays on the exact path, so no currently-passing
    // statement changes behavior (verified against the corpus).
    if let (Value::Float(o), Value::Float(b1), Value::Float(b2)) = (op, &vals[1], &vals[2]) {
        return eval_width_bucket_float8(*o, *b1, *b2, count);
    }

    let operand = wb_arg(name, op)?;
    let b1 = wb_arg(name, &vals[1])?;
    let b2 = wb_arg(name, &vals[2])?;

    // Bound validation in PG's order: NaN, then infinite, then equal.
    // v0.56: NaN/infinite bounds are errors (22003); only a NaN
    // *operand* yields count+1. Equal bounds are 22023 (the old code
    // silently returned count+1).
    for b in [&b1, &b2] {
        if matches!(b, WbArg::Nan) {
            return Err(exec_err("22003", "lower and upper bounds cannot be NaN"));
        }
        if matches!(b, WbArg::Inf(_)) {
            return Err(exec_err("22003", "lower and upper bounds must be finite"));
        }
    }
    let (b1d, b2d) = match (&b1, &b2) {
        (WbArg::Dec(a), WbArg::Dec(b)) => (a, b),
        _ => unreachable!("bounds validated finite above"),
    };
    if b1d.cmp(b2d) == Ordering::Equal {
        return Err(exec_err("22023", "lower bound cannot equal upper bound"));
    }
    let ascending = b1d.cmp(b2d) == Ordering::Less;

    // PG returns int4, so any bucket above i32::MAX is 22003
    // "integer out of range".
    let int_result = |bucket: u64| -> Result<Value, ExecError> {
        if bucket > i32::MAX as u64 {
            Err(exec_err("22003", "integer out of range"))
        } else {
            Ok(Value::Int(bucket as i64))
        }
    };
    let count_u = count as u64;

    match operand {
        // A NaN operand is above every bound: count+1 either way.
        WbArg::Nan => int_result(count_u + 1),
        WbArg::Inf(positive) => {
            // An infinite operand is beyond the bounds on its side:
            // above the range -> count+1 ascending, 0 descending;
            // below the range -> 0 ascending, count+1 descending.
            int_result(match (positive, ascending) {
                (true, true) | (false, false) => count_u + 1,
                _ => 0,
            })
        }
        WbArg::Dec(ref d) => {
            let bucket = if ascending {
                if d.cmp(b1d) == Ordering::Less {
                    0
                } else if d.cmp(b2d) == Ordering::Greater {
                    count_u + 1
                } else {
                    wb_bucket_in_range(&d.sub(b1d), &b2d.sub(b1d), count_u)?
                }
            } else if d.cmp(b1d) == Ordering::Greater {
                0
            } else if d.cmp(b2d) == Ordering::Less {
                count_u + 1
            } else {
                wb_bucket_in_range(&b1d.sub(d), &b1d.sub(b2d), count_u)?
            };
            int_result(bucket)
        }
    }
}

/// v0.18: log(b, x) = ln(x)/ln(b) for numerics.
pub(crate) fn eval_log_base(
    b: &crate::storage::Numeric,
    x: &crate::storage::Numeric,
    _name: &str,
) -> Result<Value, ExecError> {
    use crate::storage::{Numeric, NumericSpecial};
    // Handle specials per PG19 numeric_log: NaN anywhere -> NaN; negative
    // (including -Inf) -> 2201E "negative"; zero -> 2201E "zero";
    // log(+Inf, +Inf) = NaN, log(+Inf, finite) = 0, log(finite, +Inf) = +Inf.
    if b.is_nan() || x.is_nan() {
        return Ok(Value::Numeric(Numeric::nan()));
    }
    let b_neg = b.special == NumericSpecial::NegInf
        || (b.special == NumericSpecial::Finite && b.unscaled < 0);
    let x_neg = x.special == NumericSpecial::NegInf
        || (x.special == NumericSpecial::Finite && x.unscaled < 0);
    if b_neg || x_neg {
        return Err(exec_err(
            "2201E",
            "cannot take logarithm of a negative number",
        ));
    }
    if b.is_zero() || x.is_zero() {
        return Err(exec_err("2201E", "cannot take logarithm of zero"));
    }
    if b.special == NumericSpecial::PosInf {
        // log(+Inf, +Inf) reduces to Inf/Inf -> NaN.
        if x.special == NumericSpecial::PosInf {
            return Ok(Value::Numeric(Numeric::nan()));
        }
        // log(+Inf, finite-positive) is zero (no underflow throw).
        return Ok(Value::Numeric(Numeric::new(0, 0)));
    }
    if x.special == NumericSpecial::PosInf {
        // log(finite-positive, +Inf) is +Inf.
        return Ok(Value::Numeric(Numeric::infinity()));
    }
    // v0.62: PG19 log_var — two separately-scaled natural logarithms,
    // divided (handles result-scale selection itself).
    match crate::storage::Numeric::log_pg(b, x) {
        Ok(r) => Ok(Value::Numeric(r)),
        Err(crate::storage::LogError::DivisionByZero) => Err(exec_err("22012", "division by zero")),
        Err(crate::storage::LogError::Overflow) => {
            Err(exec_err("22003", "value overflows numeric format"))
        }
    }
}

/// `+ - * / %` with Postgres-ish numeric promotion. NULL propagates.
/// Date arithmetic is handled first: date +/- integer-kind -> date,
/// date - date -> integer days. No intervals in v0.7, so timestamp
/// arithmetic (other than comparisons) is 42883.
pub(crate) fn eval_arith(op: ArithOp, a: &Value, b: &Value) -> Result<Value, ExecError> {
    if a == &Value::Null || b == &Value::Null {
        return Ok(Value::Null);
    }
    // v0.21: text-vs-numeric coercion (unknown-literal resolution)
    // before anything else, so '1.5' + 1 works like Postgres.
    let coerced = coerce_text_numeric(a, b)?;
    let (a, b): (&Value, &Value) = match &coerced {
        Some((ac, bc)) => (ac, bc),
        None => (a, b),
    };
    // v0.25: bitwise operators only accept the integer kinds.
    if matches!(
        op,
        ArithOp::BitAnd | ArithOp::BitOr | ArithOp::BitXor | ArithOp::Shl | ArithOp::Shr
    ) {
        return eval_bitwise(op, a, b);
    }
    if let Some(v) = eval_datetime_arith(op, a, b)? {
        return Ok(v);
    }
    if op == ArithOp::Pow {
        // Postgres `^`: exact numeric power for integer exponents,
        // float8 when either side is floating.
        return eval_power_op(a, b, |w| op_err(op, a, w));
    }
    let (ca, cb) = match (num_cat(a), num_cat(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(op_err(op, a, b)),
    };
    let cat = ca.max(cb);
    if op == ArithOp::Mod && matches!(cat, NumCat::Real | NumCat::Double) {
        // Postgres defines % only for the exact numeric types
        // (smallint/int/bigint/numeric), not for real/double.
        return Err(op_err(op, a, b));
    }
    match cat {
        NumCat::Numeric => {
            let x = to_numeric_opt(a)
                .ok_or_else(|| exec_err("22003", "value out of range for numeric"))?;
            let y = to_numeric_opt(b)
                .ok_or_else(|| exec_err("22003", "value out of range for numeric"))?;
            // v0.25: NaN propagates through div/mod even with a zero
            // divisor ('nan' % '0' = NaN, not division by zero).
            use crate::storage::NumericSpecial;
            if matches!(op, ArithOp::Div | ArithOp::Mod)
                && (x.special == NumericSpecial::NaN || y.special == NumericSpecial::NaN)
            {
                return Ok(Value::Numeric(crate::storage::Numeric::nan()));
            }
            if matches!(op, ArithOp::Div | ArithOp::Mod) && y.is_zero() {
                return Err(exec_err("22012", "division by zero"));
            }
            let r = match op {
                ArithOp::Add => x.checked_add(&y),
                ArithOp::Sub => x.checked_sub(&y),
                ArithOp::Mul => x.checked_mul(&y),
                ArithOp::Div => x.checked_div(&y),
                ArithOp::Mod => x.checked_rem(&y),
                ArithOp::Pow => unreachable!("^ is handled before the category dispatch"),
                ArithOp::BitAnd
                | ArithOp::BitOr
                | ArithOp::BitXor
                | ArithOp::Shl
                | ArithOp::Shr => {
                    unreachable!("bitwise ops return early via eval_bitwise")
                }
            };
            r.map(Value::Numeric)
                .ok_or_else(|| exec_err("22003", "numeric field overflow"))
        }
        NumCat::Double | NumCat::Real => {
            // v0.21: float arithmetic with Postgres range checks. Real
            // (float4) computes in f32 so underflow is judged at f32
            // precision; NaN propagates per IEEE.
            let x = to_f64v(a);
            let y = to_f64v(b);
            if op == ArithOp::Div && y == 0.0 {
                // Postgres: 'nan'::float8 / 0 -> NaN; anything else
                // divided by zero -> 22012.
                if x.is_nan() {
                    return Ok(if cat == NumCat::Real {
                        Value::Float4(f32::NAN)
                    } else {
                        Value::Float(f64::NAN)
                    });
                }
                return Err(exec_err("22012", "division by zero"));
            }
            if cat == NumCat::Real {
                let xf = x as f32;
                let yf = y as f32;
                let r = match op {
                    ArithOp::Add => xf + yf,
                    ArithOp::Sub => xf - yf,
                    ArithOp::Mul => xf * yf,
                    ArithOp::Div => xf / yf,
                    ArithOp::Mod => unreachable!("rejected above"),
                    ArithOp::Pow => unreachable!("^ is handled before the category dispatch"),
                    ArithOp::BitAnd
                    | ArithOp::BitOr
                    | ArithOp::BitXor
                    | ArithOp::Shl
                    | ArithOp::Shr => {
                        unreachable!("bitwise ops return early via eval_bitwise")
                    }
                };
                check_float_arith(f64::from(r), f64::from(xf), f64::from(yf), "real")?;
                Ok(Value::Float4(r))
            } else {
                let r = match op {
                    ArithOp::Add => x + y,
                    ArithOp::Sub => x - y,
                    ArithOp::Mul => x * y,
                    ArithOp::Div => x / y,
                    ArithOp::Mod => unreachable!("rejected above"),
                    ArithOp::Pow => unreachable!("^ is handled before the category dispatch"),
                    ArithOp::BitAnd
                    | ArithOp::BitOr
                    | ArithOp::BitXor
                    | ArithOp::Shl
                    | ArithOp::Shr => {
                        unreachable!("bitwise ops return early via eval_bitwise")
                    }
                };
                check_float_arith(r, x, y, "double precision")?;
                Ok(Value::Float(r))
            }
        }
        _ => {
            let x = to_i128(a);
            let y = to_i128(b);
            if matches!(op, ArithOp::Div | ArithOp::Mod) && y == 0 {
                return Err(exec_err("22012", "division by zero"));
            }
            // v0.50: Postgres (int.c, int8.c, REL_19_STABLE) checks integer
            // arithmetic for overflow against the *result* type and raises
            // 22003 — smallint pairs do NOT promote to integer, so e.g.
            // 30000::int2 + 30000::int2 and (-32768)::int2 * (-1)::int2 are
            // "smallint out of range", not int4 results. Operands are at most
            // i64 in magnitude (to_i128), so these i128 intermediates cannot
            // overflow and cannot divide by zero (y == 0 is rejected above);
            // the range check in fit_int_result is the only possible failure.
            // Modulo matches PG exactly: INT_MIN % -1 is 0, never an error
            // (division by zero is still 22012).
            let r: i128 = match op {
                ArithOp::Add => x + y,
                ArithOp::Sub => x - y,
                ArithOp::Mul => x * y,
                ArithOp::Div => x / y,
                ArithOp::Mod => x % y,
                ArithOp::Pow => unreachable!("^ is handled before the category dispatch"),
                ArithOp::BitAnd
                | ArithOp::BitOr
                | ArithOp::BitXor
                | ArithOp::Shl
                | ArithOp::Shr => {
                    unreachable!("bitwise ops return early via eval_bitwise")
                }
            };
            fit_int_result(cat, r)
        }
    }
}

/// v0.25: `& | # << >>` on smallint/integer/bigint.
/// v0.51: PG19 (int.c, REL_19_STABLE) returns the max operand type — no
/// smallint->integer promotion. int2and/int2or/int2xor/int2shl/int2shr
/// all PG_RETURN_INT16. Shifts use C integer promotion: the shift is
/// computed in 32 bits (as C does after promoting int16 to int) and
/// then truncated to int16 — it *wraps*, never raises 22003, so
/// 1::int2 << 15 is -32768. Anything non-integer is 42883.
pub(crate) fn eval_bitwise(op: ArithOp, a: &Value, b: &Value) -> Result<Value, ExecError> {
    let (ca, cb) = match (num_cat(a), num_cat(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(op_err(op, a, b)),
    };
    let int_kind = |c: NumCat| matches!(c, NumCat::Small | NumCat::Int | NumCat::Big);
    if !int_kind(ca) || !int_kind(cb) {
        return Err(op_err(op, a, b));
    }
    // v0.51: PG19 returns the operand max type (int2 pairs stay int2);
    // the v0.25 smallint->int promotion was wrong (it survived v0.50,
    // which fixed the arithmetic promotion only).
    let icat = ca.max(cb);
    let x = to_i128(a);
    let y = to_i128(b);
    let r: i128 = match op {
        ArithOp::BitAnd => x & y,
        ArithOp::BitOr => x | y,
        ArithOp::BitXor => x ^ y,
        ArithOp::Shl => {
            if matches!(icat, NumCat::Big) {
                (x as i64).wrapping_shl(y as u32) as i128
            } else if matches!(icat, NumCat::Small) {
                // int.c int2shl: C promotes int16 to int, shifts in 32
                // bits, then PG_RETURN_INT16 truncates. 1::int2 << 15
                // wraps to -32768 (never 22003).
                ((x as i32).wrapping_shl(y as u32) as i16) as i128
            } else {
                // int4: C promotes to 32 bits before shifting.
                (x as i32).wrapping_shl(y as u32) as i128
            }
        }
        ArithOp::Shr => {
            if matches!(icat, NumCat::Big) {
                (x as i64).wrapping_shr(y as u32) as i128
            } else if matches!(icat, NumCat::Small) {
                // int2shr: arithmetic right shift after promotion to
                // int, then truncate to int16.
                ((x as i32).wrapping_shr(y as u32) as i16) as i128
            } else {
                (x as i32).wrapping_shr(y as u32) as i128
            }
        }
        _ => unreachable!("non-bitwise op in eval_bitwise"),
    };
    fit_int_result(icat, r)
}

/// v0.25: `~x` bitwise NOT. NULL propagates; smallint promotes to
/// integer (like PG, which has no int2not); non-integers are 42883.
pub(crate) fn eval_bitnot_val(v: &Value) -> Result<Value, ExecError> {
    if v == &Value::Null {
        return Ok(Value::Null);
    }
    match num_cat(v) {
        Some(NumCat::Small) | Some(NumCat::Int) => {
            let x = to_i128(v) as i32;
            Ok(Value::Int((!x) as i64))
        }
        Some(NumCat::Big) => Ok(Value::BigInt(!(to_i128(v) as i64))),
        _ => Err(exec_err(
            "42883",
            format!("operator does not exist: ~ {}", v.type_name()),
        )),
    }
}

/// v0.53: unary minus as a first-class operator (PG19's doNegate).
/// Type-preserving: `-smallint` stays smallint (the old `0 - x`
/// desugar widened it to integer). Overflow raises 22003 with the
/// per-type message, like the binary operators. NULL stays NULL; an
/// unknown-type text literal keeps the old `0 - x` resolution path
/// (so `-'5'` is still -5 and `-'2026-01-01'` still fails at
/// evaluation, like Postgres).
pub(crate) fn eval_neg_val(v: &Value) -> Result<Value, ExecError> {
    if v == &Value::Null {
        return Ok(Value::Null);
    }
    if let Value::Text(_) = v {
        // v0.7 unknown-literal path: text resolves against the other
        // side's numeric type, exactly as `0 - x` did before v0.53.
        return eval_arith(ArithOp::Sub, &Value::Int(0), v);
    }
    match v {
        Value::SmallInt(i) => i
            .checked_neg()
            .map(Value::SmallInt)
            .ok_or_else(|| exec_err("22003", "smallint out of range")),
        Value::Int(i) => {
            let x = *i as i32;
            x.checked_neg()
                .map(|r| Value::Int(r as i64))
                .ok_or_else(|| exec_err("22003", "integer out of range"))
        }
        Value::BigInt(i) => i
            .checked_neg()
            .map(Value::BigInt)
            .ok_or_else(|| exec_err("22003", "bigint out of range")),
        Value::Float4(f) => Ok(Value::Float4(-f)),
        Value::Float(f) => Ok(Value::Float(-f)),
        Value::Numeric(n) => Ok(Value::Numeric(n.neg())),
        _ => Err(exec_err(
            "42883",
            format!("operator does not exist: - {}", v.type_name()),
        )),
    }
}

/// Fit an i128 arithmetic result into the resolved integer kind.
/// v0.50: Postgres raises 22003 with a per-type message on overflow
/// ("smallint out of range" / "integer out of range" / "bigint out of
/// range"), matching int.c / int8.c on REL_19_STABLE.
pub(crate) fn fit_int_result(icat: NumCat, r: i128) -> Result<Value, ExecError> {
    match icat {
        NumCat::Small => Ok(Value::SmallInt(
            i16::try_from(r).map_err(|_| exec_err("22003", "smallint out of range"))?,
        )),
        NumCat::Int => Ok(Value::Int(
            i32::try_from(r).map_err(|_| exec_err("22003", "integer out of range"))? as i64,
        )),
        _ => Ok(Value::BigInt(
            i64::try_from(r).map_err(|_| exec_err("22003", "bigint out of range"))?,
        )),
    }
}

/// Date arithmetic. Ok(None) = not date/time operands (caller falls
/// through to numeric handling).
pub(crate) fn eval_datetime_arith(
    op: ArithOp,
    a: &Value,
    b: &Value,
) -> Result<Option<Value>, ExecError> {
    fn int_days(v: &Value) -> Option<i64> {
        match v {
            Value::SmallInt(i) => Some(*i as i64),
            Value::Int(i) => Some(*i),
            Value::BigInt(i) => Some(*i),
            _ => None,
        }
    }
    let is_dt = |v: &Value| {
        matches!(
            v,
            Value::Date(_) | Value::Timestamp(_) | Value::Timestamptz(_)
        )
    };
    if !is_dt(a) && !is_dt(b) {
        return Ok(None);
    }
    // date +/- integer-kind -> date
    if let Value::Date(d) = a {
        if let Some(days) = int_days(b) {
            match op {
                ArithOp::Add | ArithOp::Sub => {
                    let delta = if op == ArithOp::Add { days } else { -days };
                    let r = (*d as i64)
                        .checked_add(delta)
                        .and_then(|r| i32::try_from(r).ok())
                        .ok_or_else(|| exec_err("22008", "datetime field overflow"))?;
                    return Ok(Some(Value::Date(r)));
                }
                _ => return Err(op_err(op, a, b)),
            }
        }
    }
    // integer + date -> date (commutative with date + integer; only
    // addition commutes — `int - date` is undefined, like Postgres).
    if let Value::Date(d) = b {
        if let Some(days) = int_days(a) {
            if op == ArithOp::Add {
                let r = (*d as i64)
                    .checked_add(days)
                    .and_then(|r| i32::try_from(r).ok())
                    .ok_or_else(|| exec_err("22008", "datetime field overflow"))?;
                return Ok(Some(Value::Date(r)));
            }
            return Err(op_err(op, a, b));
        }
    }
    // date - date -> integer days
    if let (Value::Date(d1), Value::Date(d2)) = (a, b) {
        return match op {
            ArithOp::Sub => Ok(Some(Value::Int(*d1 as i64 - *d2 as i64))),
            _ => Err(op_err(op, a, b)),
        };
    }
    // timestamp - timestamp would be an interval; v0.7 has none.
    if matches!(
        (a, b),
        (Value::Timestamp(_), Value::Timestamp(_)) | (Value::Timestamptz(_), Value::Timestamptz(_))
    ) {
        return Err(exec_err(
            "42883",
            "operator does not exist: timestamp - timestamp (intervals are not supported in v0.7)",
        ));
    }
    Err(op_err(op, a, b))
}
