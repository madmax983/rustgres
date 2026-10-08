// v1.78 mechanical split: moved verbatim from src/exec.rs (41730-43219).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// Casts
// ---------------------------------------------------------------------------

pub(crate) fn cast_err(from: &Value, to: &str) -> ExecError {
    exec_err(
        "42846",
        format!("cannot cast type {} to {}", from.type_name(), to),
    )
}

/// Cast a value to an integer kind. Floats and numerics round half away
/// from zero (like Postgres); text must be plain integer syntax.
pub(crate) fn cast_to_int(v: &Value) -> Result<i128, ExecError> {
    match v {
        Value::SmallInt(i) => Ok(*i as i128),
        Value::Int(i) => Ok(*i as i128),
        Value::BigInt(i) => Ok(*i as i128),
        Value::Numeric(n) => {
            let r = n
                .to_i64()
                .ok_or_else(|| exec_err("22003", "numeric out of range"))?;
            Ok(r as i128)
        }
        Value::Float4(f) => float_to_int(*f as f64),
        Value::Float(f) => float_to_int(*f),
        Value::Text(s) => parse_int_text(s),
        Value::Bool(b) => Ok(*b as i128),
        // v0.28: bytea -> integer is width-sensitive (PG's bytea_int2/int4/
        // int8 reinterpret the bytes at the target width), so it is handled
        // per-target in eval_cast via cast_bytea_to_int, not here.
        other => Err(cast_err(other, "integer")),
    }
}

/// v0.28: bytea -> integer cast, following PG 18+ (present in PG 19).
/// The bytes are the two's complement representation of the integer, most
/// significant byte first. Fewer bytes than the target width are fine
/// (empty -> 0); more bytes than the width is 22003 "<type> out of range".
/// The accumulated bits are reinterpreted as signed *at the target width*,
/// so e.g. '\xFF'::bytea::int2 is 255 while '\x8000'::bytea::int2 is -32768.
pub(crate) fn cast_bytea_to_int(
    b: &[u8],
    width: usize,
    type_name: &str,
) -> Result<i128, ExecError> {
    if b.len() > width {
        return Err(exec_err("22003", format!("{} out of range", type_name)));
    }
    let mut v: u64 = 0;
    for &byte in b {
        v = (v << 8) | u64::from(byte);
    }
    Ok(match width {
        2 => i128::from((v as u16) as i16),
        4 => i128::from((v as u32) as i32),
        _ => i128::from(v as i64),
    })
}

/// v1.39: PG19 `bittoint4` (varbit.c): the bytes assemble big-endian,
/// the padding bits at the end shift out (VARBITPAD), and the result
/// reinterprets as int32. bitlen > 32 is 22003 "integer out of range".
pub(crate) fn bit_to_int4(bs: &BitString) -> Result<i32, ExecError> {
    if bs.bitlen > 32 {
        return Err(exec_err("22003", "integer out of range"));
    }
    let mut v: u32 = 0;
    for &byte in &bs.bytes {
        v = (v << 8) | u32::from(byte);
    }
    let pad = (bs.bytes.len() * 8).saturating_sub(bs.bitlen as usize) as u32;
    v >>= pad;
    Ok(v as i32)
}

/// v1.39: PG19 `bittoint8` (varbit.c): same shape as `bit_to_int4` for
/// 64 bits; bitlen > 64 is 22003 "bigint out of range".
pub(crate) fn bit_to_int8(bs: &BitString) -> Result<i64, ExecError> {
    if bs.bitlen > 64 {
        return Err(exec_err("22003", "bigint out of range"));
    }
    let mut v: u64 = 0;
    for &byte in &bs.bytes {
        v = (v << 8) | u64::from(byte);
    }
    let pad = (bs.bytes.len() * 8).saturating_sub(bs.bitlen as usize) as u32;
    v >>= pad;
    Ok(v as i64)
}

pub(crate) fn float_to_int(f: f64) -> Result<i128, ExecError> {
    if !f.is_finite() {
        return Err(exec_err("22003", "integer out of range"));
    }
    // v0.25: PG rounds half to even (banker's rounding): 2.5 -> 2, 3.5 -> 4.
    let r = f.round_ties_even();
    if r < i128::MIN as f64 || r > i128::MAX as f64 {
        return Err(exec_err("22003", "integer out of range"));
    }
    Ok(r as i128)
}

/// Postgres integer input (v0.25): optional sign, optional `0b`/`0o`/`0x`
/// base prefix (PG 16+), digits with `_` separators between digits
/// (PG 16+), surrounding whitespace. Anything else (decimal points,
/// exponents, bad prefixes) is 22P02; overflow of i128 is 22003 (the
/// caller narrows to int2/int4/int8 with its own range error).
pub(crate) fn parse_int_text(s: &str) -> Result<i128, ExecError> {
    let t = s.trim();
    let after_sign = t.strip_prefix('+').unwrap_or(t);
    let (neg, digits) = match after_sign.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, after_sign),
    };
    // v0.25: base prefix. Underscores may appear between digits; PG
    // also allows one right after the prefix (`0b_101`). A bare prefix
    // or a stray/doubled/trailing underscore is 22P02.
    let (base, mut digits) = if let Some(d) = digits
        .strip_prefix("0b")
        .or_else(|| digits.strip_prefix("0B"))
    {
        (2u32, d)
    } else if let Some(d) = digits
        .strip_prefix("0o")
        .or_else(|| digits.strip_prefix("0O"))
    {
        (8u32, d)
    } else if let Some(d) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (16u32, d)
    } else {
        (10u32, digits)
    };
    let had_prefix = base != 10;
    // PG allows a single underscore immediately after the base prefix.
    if had_prefix && digits.starts_with('_') {
        digits = &digits[1..];
    }
    let digit_ok = |c: u8| match base {
        2 => c == b'0' || c == b'1',
        8 => c.is_ascii_digit() && c < b'8',
        16 => c.is_ascii_hexdigit(),
        _ => c.is_ascii_digit(),
    };
    // Validate: at least one digit; underscores only between digits
    // (no leading, trailing, or doubled underscores).
    let mut saw_digit = false;
    let mut need_digit = true; // leading underscore rejected
    for &c in digits.as_bytes() {
        if c == b'_' {
            if need_digit {
                return Err(int_syntax_err(s));
            }
            need_digit = true;
        } else if digit_ok(c) {
            saw_digit = true;
            need_digit = false;
        } else {
            return Err(int_syntax_err(s));
        }
    }
    if !saw_digit || need_digit {
        return Err(int_syntax_err(s));
    }
    let mut r: i128 = 0;
    for &c in digits.as_bytes() {
        if c == b'_' {
            continue;
        }
        let d = (c as char).to_digit(base).unwrap() as i128;
        r = r
            .checked_mul(base as i128)
            .and_then(|r| r.checked_add(d))
            .ok_or_else(|| exec_err("22003", "value overflows integer"))?;
    }
    if neg {
        r = -r;
    }
    Ok(r)
}

/// 22P02 for a bad integer literal, PG-style message.
pub(crate) fn int_syntax_err(s: &str) -> ExecError {
    exec_err(
        "22P02",
        format!("invalid input syntax for type integer: {:?}", s),
    )
}

/// v0.21: does the (trimmed, non-special) float literal's mantissa
/// contain a nonzero digit? Tells genuine zero ("0e-400") apart from
/// underflow ("10e-400").
pub(crate) fn mantissa_has_nonzero_digit(t: &str) -> bool {
    let mantissa = t.split(|c| c == 'e' || c == 'E').next().unwrap_or(t);
    mantissa.bytes().any(|c| c.is_ascii_digit() && c != b'0')
}

/// v0.21: checked float8 input. Rust's `parse::<f64>()` maps "1e999" to
/// infinity and stays silent on underflow; Postgres raises 22003 for
/// both (unless the text is a genuine infinity spelling), and 22P02
/// for unparseable text.
pub(crate) fn parse_f64_checked(s: &str) -> Result<f64, ExecError> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => return Ok(f64::INFINITY),
        "-inf" | "-infinity" => return Ok(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => return Ok(f64::NAN),
        _ => {}
    }
    match t.parse::<f64>() {
        Ok(f) => {
            if f.is_infinite() {
                // Genuine infinity spellings returned above, so this is
                // overflow like "1e999".
                return Err(exec_err(
                    "22003",
                    format!("value {:?} is out of range for type double precision", s),
                ));
            }
            if f == 0.0 && mantissa_has_nonzero_digit(t) {
                // Underflow: the text denotes a nonzero value.
                return Err(exec_err(
                    "22003",
                    format!("value {:?} is out of range for type double precision", s),
                ));
            }
            Ok(f)
        }
        Err(_) => Err(exec_err(
            "22P02",
            format!("invalid input syntax for type double precision: {:?}", s),
        )),
    }
}

/// v0.21: checked float4 input — same rules as `parse_f64_checked`,
/// with "real" in the messages.
pub(crate) fn parse_f32_checked(s: &str) -> Result<f32, ExecError> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => return Ok(f32::INFINITY),
        "-inf" | "-infinity" => return Ok(f32::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => return Ok(f32::NAN),
        _ => {}
    }
    match t.parse::<f32>() {
        Ok(f) => {
            if f.is_infinite() {
                return Err(exec_err(
                    "22003",
                    format!("value {:?} is out of range for type real", s),
                ));
            }
            if f == 0.0 && mantissa_has_nonzero_digit(t) {
                return Err(exec_err(
                    "22003",
                    format!("value {:?} is out of range for type real", s),
                ));
            }
            Ok(f)
        }
        Err(_) => Err(exec_err(
            "22P02",
            format!("invalid input syntax for type real: {:?}", s),
        )),
    }
}

/// v0.21: narrow an f64 to f32 for casts to real. NaN/±Infinity pass
/// through; overflow is 22003; underflow to 0/subnormal is allowed.
pub(crate) fn narrow_to_f32(f: f64) -> Result<f32, ExecError> {
    if f.is_nan() {
        return Ok(f32::NAN);
    }
    if f.is_infinite() {
        return Ok(if f > 0.0 {
            f32::INFINITY
        } else {
            f32::NEG_INFINITY
        });
    }
    if f.abs() > f32::MAX as f64 {
        return Err(exec_err("22003", "value out of range for type real"));
    }
    Ok(f as f32)
}

pub(crate) fn cast_to_f64(v: &Value) -> Result<f64, ExecError> {
    match v {
        Value::SmallInt(i) => Ok(*i as f64),
        Value::Int(i) => Ok(*i as f64),
        Value::BigInt(i) => Ok(*i as f64),
        Value::Numeric(n) => Ok(n.to_f64()),
        Value::Float4(f) => Ok(*f as f64),
        Value::Float(f) => Ok(*f),
        Value::Bool(b) => Ok(*b as i32 as f64),
        // v0.21: text goes through the checked float8 parser.
        Value::Text(s) => parse_f64_checked(s),
        other => Err(cast_err(other, "double precision")),
    }
}

pub(crate) fn cast_to_numeric(v: &Value) -> Result<Numeric, ExecError> {
    match v {
        Value::SmallInt(i) => Ok(Numeric::new(*i as i128, 0)),
        Value::Int(i) => Ok(Numeric::new(*i as i128, 0)),
        Value::BigInt(i) => Ok(Numeric::new(*i as i128, 0)),
        Value::Numeric(n) => Ok(n.clone()),
        Value::Float4(f) => Numeric::from_f64(*f as f64)
            .map_err(|_| exec_err("22003", "value out of range for type numeric")),
        Value::Float(f) => Numeric::from_f64(*f)
            .map_err(|_| exec_err("22003", "value out of range for type numeric")),
        Value::Bool(b) => Ok(Numeric::new(*b as i128, 0)),
        Value::Text(s) => match Numeric::parse(s) {
            Ok(n) => Ok(n),
            Err(crate::storage::NumericParseError::Syntax) => Err(exec_err(
                "22P02",
                format!("invalid input syntax for type numeric: {:?}", s),
            )),
            Err(crate::storage::NumericParseError::Overflow) => {
                Err(exec_err("22003", "value overflows numeric"))
            }
        },
        other => Err(cast_err(other, "numeric")),
    }
}

pub(crate) fn cast_to_bool(v: &Value) -> Result<bool, ExecError> {
    match v {
        Value::Bool(b) => Ok(*b),
        // v0.14: PostgreSQL boolean input accepts unambiguous prefixes of
        // true/false/yes/no/on/off (e.g. 'of' -> false, 'tru' -> true);
        // a bare 'o' is ambiguous (on/off) and rejected, like PG.
        Value::Text(s) => {
            let l = s.trim().to_ascii_lowercase();
            let parsed = match l.chars().next() {
                Some('t') if "true".starts_with(l.as_str()) => Some(true),
                Some('f') if "false".starts_with(l.as_str()) => Some(false),
                Some('y') if "yes".starts_with(l.as_str()) => Some(true),
                Some('n') if "no".starts_with(l.as_str()) => Some(false),
                Some('o') => {
                    let on = "on".starts_with(l.as_str());
                    let off = "off".starts_with(l.as_str());
                    match (on, off) {
                        (true, false) => Some(true),
                        (false, true) => Some(false),
                        _ => None, // ambiguous ('o') or invalid
                    }
                }
                Some('1') if l.len() == 1 => Some(true),
                Some('0') if l.len() == 1 => Some(false),
                _ => None,
            };
            parsed.ok_or_else(|| {
                exec_err(
                    "22P02",
                    format!("invalid input syntax for type boolean: {:?}", s),
                )
            })
        }
        // v0.14: PostgreSQL casts integers to boolean (0 -> false, else true).
        Value::SmallInt(i) => Ok(*i != 0),
        Value::Int(i) => Ok(*i != 0),
        Value::BigInt(i) => Ok(*i != 0),
        other => Err(cast_err(other, "boolean")),
    }
}

/// Text rendering for casts and `||`: like the wire format, except
/// booleans render as `true`/`false` (Postgres cast output).
/// Text format of one value for a SQL cast, as an owned `String`. Hot
/// paths use `text_value_of` instead, which does not make a `String`.
pub(crate) fn value_to_text_cast(v: &Value) -> String {
    let mut out = String::new();
    write_text_cast(v, &mut out);
    out
}

/// Largest scratch buffer to keep. One very long text must not hold
/// memory for the life of the connection thread.
pub(crate) const TEXT_CAST_BUF_MAX: usize = 64 * 1024;

thread_local! {
    /// Scratch buffer for text casts. It keeps its capacity between
    /// calls, so a cast that runs for each row allocates only the
    /// `Arc<str>`.
    pub(crate) static TEXT_CAST_BUF: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };
}

/// Write one value in its text format into `out`. `Bool` writes
/// `true`/`false`, like a SQL cast; the wire format writes `t`/`f`.
pub(crate) fn write_text_cast(v: &Value, out: &mut String) {
    use std::fmt::Write;
    match v {
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::SmallInt(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Int(i) | Value::BigInt(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Text(t) => out.push_str(t),
        // v0.35: PG's text(bpchar) strips trailing spaces (rtrim1).
        Value::BpChar(s) => out.push_str(crate::storage::rtrim_spaces(s)),
        other => {
            if let Some(t) = other.to_text() {
                out.push_str(&t);
            }
        }
    }
}

/// Build a `Value::Text` from the text format of every value in `parts`.
/// The scratch buffer keeps one allocation per call, not one per part.
/// `write_text_cast` never calls this function, so the borrow is safe.
pub(crate) fn text_value_of(parts: &[&Value]) -> Value {
    TEXT_CAST_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        for v in parts {
            write_text_cast(v, &mut buf);
        }
        let out = Value::text(buf.as_str());
        if buf.capacity() > TEXT_CAST_BUF_MAX {
            *buf = String::new();
        }
        out
    })
}

/// v0.43: parse a `numeric(p)` / `numeric(p,s)` typmod for the pg_input_*
/// family, returning `(precision, scale)`. `None` = malformed (PG raises
/// 22023 here; the engine's pg_input_is_valid has historically reported
/// malformed typmods as false, and pg_input_error_info mirrors that as a
/// soft `invalid type modifier` row).
pub(crate) fn parse_numeric_typmod(t: &str) -> Option<(i32, i32)> {
    let open = t.find('(')?;
    let close = t.rfind(')')?;
    if close != t.len() - 1 {
        return None;
    }
    let args = &t[open + 1..close];
    let mut parts = args.split(',');
    let precision: i32 = parts.next()?.trim().parse().ok()?;
    let scale: i32 = match parts.next() {
        Some(s) => s.trim().parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    // PG19 numeric typmod bounds: 1..=1000 for precision; scale may be
    // negative (down to -1000), per make_numeric_typmod's
    // "scale is constrained to the range [-1000, 1000]".
    if !(1..=1000).contains(&precision) || !(-1000..=1000).contains(&scale) {
        return None;
    }
    Some((precision, scale))
}

/// v0.43: integer digits of a scale-rounded `Numeric`: value =
/// unscaled * 10^-scale. `None` only if 10^scale overflows i128, which
/// cannot happen for a parsed value at a legal scale.
pub(crate) fn numeric_integer_digits(rounded: &crate::storage::Numeric) -> Option<i32> {
    // v0.63: big-mantissa aware via the exact digit count.
    if rounded.is_big() {
        let mag = rounded.mag();
        let d = mag.decimal_digits() as i64 - rounded.scale as i64;
        return Some(d.max(1) as i32);
    }
    let divisor = 10i128.checked_pow(rounded.scale as u32)?;
    let int_part = rounded.unscaled.abs() / divisor;
    if int_part == 0 {
        return Some(1);
    }
    let mut d = 0;
    let mut v = int_part;
    while v > 0 {
        d += 1;
        v /= 10;
    }
    Some(d)
}

/// v0.43: a "soft" input error, PG19 misc.c `ErrorSaveContext` style: the
/// PG-exact primary message, optional detail/hint, and the 5-char SQLSTATE
/// from the type's input function.
pub(crate) struct PgInputError {
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
    pub(crate) hint: Option<String>,
    pub(crate) code: &'static str,
}

impl PgInputError {
    pub(crate) fn soft(code: &'static str, message: impl Into<String>) -> Self {
        PgInputError {
            message: message.into(),
            detail: None,
            hint: None,
            code,
        }
    }
}

/// v0.43: how `pg_input_validate` failed. `Soft` is the input function's
/// soft error (pg_input_is_valid reports false; pg_input_error_info
/// returns it as a row). `Hard` is raised outside the soft-error context
/// and propagates: unknown type name -> 42883 (the engine's v0.29
/// choice), malformed character typmod -> 22023 (v0.35).
pub(crate) enum PgInputFailure {
    Soft(PgInputError),
    Hard(ExecError),
}

impl From<ExecError> for PgInputFailure {
    fn from(e: ExecError) -> Self {
        PgInputFailure::Hard(e)
    }
}

/// v0.43: integer-family input validation with PG-exact soft errors (PG19
/// int.c: `invalid input syntax for type %s: "%s"` / 22P02 vs `value "%s"
/// is out of range for type %s` / 22003). `pgname` is the PG type name
/// (smallint/integer/bigint); `bits` selects the range check.
pub(crate) fn pg_input_validate_int(
    input: &str,
    pgname: &str,
    bits: u32,
) -> Result<(), PgInputFailure> {
    let range_err = || {
        PgInputFailure::Soft(PgInputError::soft(
            "22003",
            format!("value {input:?} is out of range for type {pgname}"),
        ))
    };
    let v = match parse_int_text(input) {
        Ok(v) => v,
        Err(e) if e.code == "22P02" => {
            return Err(PgInputFailure::Soft(PgInputError::soft(
                "22P02",
                format!("invalid input syntax for type {pgname}: {input:?}"),
            )));
        }
        // v0.25's i128 overflow ("value overflows integer") is out of
        // range for every int width, like PG's ERANGE path.
        Err(_) => return Err(range_err()),
    };
    let in_range = match bits {
        16 => i16::try_from(v).is_ok(),
        32 => i32::try_from(v).is_ok(),
        _ => i64::try_from(v).is_ok(),
    };
    if in_range { Ok(()) } else { Err(range_err()) }
}

/// v0.43: int2vector validation (PG19 int.c int2vectorin):
/// whitespace-separated int2s; element errors name the element type
/// ("smallint") and quote the failing element. The empty string is a
/// valid empty vector, like PG (this also corrects pg_input_is_valid,
/// which previously rejected it).
pub(crate) fn pg_input_validate_int2vector(input: &str) -> Result<(), PgInputFailure> {
    for elem in input.split_whitespace() {
        let range_err = || {
            PgInputFailure::Soft(PgInputError::soft(
                "22003",
                format!("value {elem:?} is out of range for type smallint"),
            ))
        };
        match parse_int_text(elem) {
            Ok(v) => {
                if i16::try_from(v).is_err() {
                    return Err(range_err());
                }
            }
            Err(e) if e.code == "22P02" => {
                return Err(PgInputFailure::Soft(PgInputError::soft(
                    "22P02",
                    format!("invalid input syntax for type smallint: {elem:?}"),
                )));
            }
            Err(_) => return Err(range_err()),
        }
    }
    Ok(())
}

/// v0.43: `numeric(p[,s])` / `decimal(p[,s])` validation with PG-exact soft
/// errors (PG19 numeric.c apply_typmod / apply_typmod_special):
/// `numeric field overflow` / 22003, with the `must round to an absolute
/// value less than 10^N` detail (N = precision - scale; "1" when N = 0),
/// or `cannot hold an infinite value` for infinities. NaN is valid for any
/// typmod, like PG. A malformed typmod is a soft `invalid type modifier`
/// (22023), mirroring pg_input_is_valid's historical false.
pub(crate) fn pg_input_validate_numeric_typmod(input: &str, t: &str) -> Result<(), PgInputFailure> {
    let (precision, scale) = match parse_numeric_typmod(t) {
        Some(ps) => ps,
        None => {
            return Err(PgInputFailure::Soft(PgInputError::soft(
                "22023",
                "invalid type modifier",
            )));
        }
    };
    let maxdigits = precision - scale;
    let bound = if maxdigits == 0 {
        "1".to_string()
    } else {
        format!("10^{maxdigits}")
    };
    let field_overflow = |detail: String| {
        PgInputFailure::Soft(PgInputError {
            message: "numeric field overflow".to_string(),
            detail: Some(detail),
            hint: None,
            code: "22003",
        })
    };
    let n = match crate::storage::Numeric::parse(input) {
        Ok(n) => n,
        Err(crate::storage::NumericParseError::Syntax) => {
            return Err(PgInputFailure::Soft(PgInputError::soft(
                "22P02",
                format!("invalid input syntax for type numeric: {input:?}"),
            )));
        }
        Err(crate::storage::NumericParseError::Overflow) => {
            return Err(PgInputFailure::Soft(PgInputError::soft(
                "22003",
                "value overflows numeric format",
            )));
        }
    };
    if n.is_nan() {
        return Ok(());
    }
    if n.is_special() {
        return Err(field_overflow(format!(
            "A field with precision {precision}, scale {scale} cannot hold an infinite value."
        )));
    }
    let int_digits = n
        .round_to(scale)
        .and_then(|r| numeric_integer_digits(&r))
        // Unreachable for parsed values at a legal scale; an
        // astronomically large value cannot fit the typmod anyway.
        .ok_or_else(|| {
            field_overflow(format!(
                "A field with precision {precision}, scale {scale} must round to an absolute value less than {bound}."
            ))
        })?;
    if int_digits + scale > precision {
        return Err(field_overflow(format!(
            "A field with precision {precision}, scale {scale} must round to an absolute value less than {bound}."
        )));
    }
    Ok(())
}

/// v0.43: shared input-validation core for `pg_input_is_valid` and
/// `pg_input_error_info` (PG19 misc.c `pg_input_is_valid_common`). Runs the
/// type's input function against `input`; `Ok(())` = valid input. The type
/// dispatch mirrors the v0.29/v0.35/v0.42 `pg_input_is_valid` arms exactly
/// so the two functions cannot drift. `fn_name` names the caller for the
/// 42883 unknown-type error.
pub(crate) fn pg_input_validate(
    fn_name: &str,
    input: &str,
    typ: &str,
) -> Result<(), PgInputFailure> {
    // v0.36: PG's one-byte `"char"` type (the quotes are part of the name;
    // like PG it never folds case, so this check runs on the un-lowercased
    // spelling). charin is total: every input is valid.
    if typ.trim() == "\"char\"" {
        return Ok(());
    }
    // Map an input-function ExecError straight into a soft error; the
    // messages below are already PG-exact (bool, bytea, char/varchar).
    let soft = |e: ExecError| {
        PgInputFailure::Soft(PgInputError {
            message: e.message,
            detail: None,
            hint: None,
            code: e.code,
        })
    };
    // v0.43: float overflow drops PG's missing "value " prefix (PG19
    // float.c: `"%s" is out of range for type double precision`); the
    // 22P02 syntax message is already exact.
    let float_err = |e: ExecError, pgname: &str| {
        PgInputFailure::Soft(if e.code == "22003" {
            PgInputError::soft(
                "22003",
                format!("{input:?} is out of range for type {pgname}"),
            )
        } else {
            PgInputError {
                message: e.message,
                detail: None,
                hint: None,
                code: e.code,
            }
        })
    };
    match typ.to_ascii_lowercase().as_str() {
        "bytea" => crate::storage::parse_bytea(input).map(|_| ()).map_err(|e| {
            use crate::storage::ByteaParseError;
            PgInputFailure::Soft(match e {
                ByteaParseError::OddHexDigits => {
                    PgInputError::soft("22023", "invalid hexadecimal data: odd number of digits")
                }
                ByteaParseError::BadHexDigit(c) => {
                    PgInputError::soft("22023", format!("invalid hexadecimal digit: \"{c}\""))
                }
                ByteaParseError::Invalid => {
                    PgInputError::soft("22P02", "invalid input syntax for type bytea")
                }
            })
        }),
        "bool" | "boolean" => cast_to_bool(&Value::text(input)).map(|_| ()).map_err(soft),
        "smallint" | "int2" => pg_input_validate_int(input, "smallint", 16),
        "int" | "integer" | "int4" => pg_input_validate_int(input, "integer", 32),
        "bigint" | "int8" => pg_input_validate_int(input, "bigint", 64),
        "int2vector" => pg_input_validate_int2vector(input),
        "real" | "float4" => parse_f32_checked(input)
            .map(|_| ())
            .map_err(|e| float_err(e, "real")),
        "double precision" | "float8" | "float" => parse_f64_checked(input)
            .map(|_| ())
            .map_err(|e| float_err(e, "double precision")),
        t if t == "numeric" || t == "decimal" => {
            match crate::storage::Numeric::parse(input) {
                Ok(_) => Ok(()),
                Err(crate::storage::NumericParseError::Syntax) => {
                    Err(PgInputFailure::Soft(PgInputError::soft(
                        "22P02",
                        format!("invalid input syntax for type numeric: {input:?}"),
                    )))
                }
                // v0.43: PG19 numeric.c says "value overflows numeric
                // format" (the bare-numeric cast path keeps its
                // historical text; only the soft error is exact).
                Err(crate::storage::NumericParseError::Overflow) => Err(PgInputFailure::Soft(
                    PgInputError::soft("22003", "value overflows numeric format"),
                )),
            }
        }
        t if t.starts_with("numeric(") || t.starts_with("decimal(") => {
            pg_input_validate_numeric_typmod(input, t)
        }
        // v0.35: character types with typmod, via the same blank-tolerant
        // input coercion as INSERT. A malformed typmod is 22023, like PG's
        // type parser (hard error, via `?`).
        t => match parse_char_type_arg(t)? {
            Some((is_char, n)) => {
                let label = if is_char {
                    "character"
                } else {
                    "character varying"
                };
                coerce_char_len(input, n, false, is_char, label)
                    .map(|_| ())
                    .map_err(soft)
            }
            None => Err(PgInputFailure::Hard(exec_err(
                "42883",
                format!("function {fn_name}(unknown, {typ}) does not exist"),
            ))),
        },
    }
}

/// v0.35: parse a `pg_input_is_valid` type argument naming a character
/// type, returning `(is_char, typmod)`. `None` = not a character type.
/// A malformed typmod is 22023, like PG's type parser. Bare `char`
/// means `char(1)`; bare `bpchar`/`varchar` carry no typmod.
pub(crate) fn parse_char_type_arg(t: &str) -> Result<Option<(bool, Option<i32>)>, ExecError> {
    let t = t.trim();
    let (base, n) = match t.find('(') {
        None => (t, None),
        Some(i) => {
            let base = t[..i].trim();
            let rest = &t[i + 1..];
            let bad = || exec_err("22023", "invalid type modifier");
            let end = rest.find(')').ok_or_else(bad)?;
            if !rest[end + 1..].trim().is_empty() {
                return Err(bad());
            }
            let num: i64 = rest[..end].trim().parse().map_err(|_| bad())?;
            if !(1..=10_485_760).contains(&num) {
                return Err(bad());
            }
            (base, Some(num as i32))
        }
    };
    let base_lc = base.to_ascii_lowercase();
    let is_char = match base_lc.as_str() {
        "char" | "character" | "bpchar" => true,
        "varchar" | "character varying" => false,
        _ => return Ok(None),
    };
    let n = match (base_lc.as_str(), n) {
        ("char" | "character", None) => Some(1),
        _ => n,
    };
    Ok(Some((is_char, n)))
}

/// v0.35: PG19 varchar.c length coercion (`bpchar()` / `varchar()`).
///
/// `s` is the source string (already rtrimmed when the source is bpchar
/// and the target is varchar/text, per PG's `text(bpchar)` = rtrim1).
/// `n` is the typmod (None = unlimited); `is_explicit` selects CAST
/// semantics (silent truncation of any excess) vs assignment/input
/// semantics (excess must be all spaces, else 22001); `pad` blank-pads to
/// `n` (bpchar only). Lengths count Unicode characters, like PG.
///
/// Returns the coerced string (unpadded for varchar).
pub(crate) fn coerce_char_len(
    s: &str,
    n: Option<i32>,
    is_explicit: bool,
    pad: bool,
    type_label: &str,
) -> Result<String, ExecError> {
    let Some(n) = n else {
        return Ok(s.to_string());
    };
    let n = n as usize;
    let char_count = s.chars().count();
    if char_count <= n {
        if pad && char_count < n {
            let mut out = String::with_capacity(s.len() + (n - char_count));
            out.push_str(s);
            for _ in char_count..n {
                out.push(' ');
            }
            return Ok(out);
        }
        return Ok(s.to_string());
    }
    // Overlength: find the byte offset of the n-th character boundary.
    let cut = s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len());
    let (keep, rest) = s.split_at(cut);
    if !is_explicit && !rest.bytes().all(|b| b == b' ') {
        return Err(exec_err(
            "22001",
            format!("value too long for type {}({})", type_label, n),
        ));
    }
    Ok(keep.to_string())
}

/// v0.35: assignment/input coercion to `character(n)` / `character
/// varying(n)`: blank-tolerant truncation (22001 on non-blank excess),
/// blank-padding for char. `n` = None means no typmod (unlimited).
/// Returns `Value::BpChar` for char (padded) and `Value::Text` for
/// varchar.
pub(crate) fn eval_char_assign(s: &str, n: Option<i32>, is_char: bool) -> Result<Value, ExecError> {
    let label = if is_char {
        "character"
    } else {
        "character varying"
    };
    let out = coerce_char_len(s, n, false, is_char, label)?;
    Ok(if is_char {
        Value::bpchar(out)
    } else {
        Value::text(out)
    })
}

/// v0.35: explicit-cast coercion to `character(n)` / `character
/// varying(n)`: silent truncation of any excess, blank-padding for char.
/// The caller rtrims bpchar sources targeting varchar (PG's
/// `varchar(bpchar)` goes through `text(bpchar)` = rtrim1); bpchar
/// sources targeting char keep their padding (PG's `bpchar()` works on
/// the padded datum).
pub(crate) fn eval_char_cast(s: &str, n: Option<i32>, is_char: bool) -> Result<Value, ExecError> {
    let label = if is_char {
        "character"
    } else {
        "character varying"
    };
    let out = coerce_char_len(s, n, true, is_char, label)?;
    Ok(if is_char {
        Value::bpchar(out)
    } else {
        Value::text(out)
    })
}

/// v0.35: assignment/input coercion to a column type. For
/// `character(n)` / `character varying(n)` targets this is PG's
/// assignment cast (blank-tolerant truncation, 22001 on non-blank
/// excess, blank-padding for char); every other target goes through
/// the explicit `eval_cast` (unchanged behavior).
pub(crate) fn eval_assign_cast(v: &Value, to: &ColType) -> Result<Value, ExecError> {
    match to {
        ColType::Char(n) => match v {
            // A bpchar source keeps its padding: PG's bpchar() works on
            // the padded datum.
            Value::Text(s) | Value::BpChar(s) => eval_char_assign(s, *n, true),
            _ => {
                let t = text_value_of(&[v]);
                match &t {
                    Value::Text(s) => eval_char_assign(s, *n, true),
                    _ => Ok(t),
                }
            }
        },
        ColType::Varchar(n) => match v {
            // PG's bpchar->varchar cast goes through text(bpchar).
            Value::BpChar(s) => eval_char_assign(crate::storage::rtrim_spaces(s), *n, false),
            Value::Text(s) => eval_char_assign(s, *n, false),
            _ => {
                let t = text_value_of(&[v]);
                match &t {
                    Value::Text(s) => eval_char_assign(s, *n, false),
                    _ => Ok(t),
                }
            }
        },
        _ => eval_cast(v, *to),
    }
}

/// v0.37: regclass output — an OID rendered as text, like PG19's
/// `regclassout`: OID 0 renders as `"-"`, a known visible relation
/// renders as its name, and any other OID renders as its decimal
/// string (never an error).
pub(crate) fn regclass_display(q: &Q, oid: u32) -> String {
    if oid == 0 {
        return "-".to_string();
    }
    for (name, versions) in &q.eng.db.tables {
        if versions
            .iter()
            .any(|t| t.oid == oid && crate::storage::table_visible(t, q.snap, &q.all_xids))
        {
            return name.clone();
        }
    }
    oid.to_string()
}

/// v0.37: `CAST(x AS regclass)`, like PG19's `regclassin`: `"-"`
/// signifies OID 0 and an all-digit string is taken as the OID
/// directly (no existence check); anything else must name a visible
/// relation (42P01 otherwise). A regclass value is represented by its
/// display text (see [`regclass_display`]).
pub(crate) fn eval_regclass_cast(q: &mut Q, v: &Value) -> Result<Value, ExecError> {
    let oid: u32 = match v {
        Value::Null => return Ok(Value::Null),
        Value::Int(i) => *i as u32,
        Value::BigInt(i) => *i as u32,
        Value::SmallInt(i) => *i as u32,
        Value::Text(s) => {
            if s.as_ref() == "-" {
                0
            } else if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                // PG's parseNumericOid: overflow fails soft to InvalidOid.
                s.parse::<u32>().unwrap_or(0)
            } else {
                // A relation name: must resolve, like PG (42P01).
                let t = q
                    .eng
                    .db
                    .find_table(s, q.snap, &q.all_xids, q.session)
                    .ok_or_else(|| {
                        exec_err("42P01", format!("relation \"{}\" does not exist", s))
                    })?;
                t.oid
            }
        }
        other => {
            return Err(exec_err(
                "42846",
                format!("cannot cast type {} to regclass", other.type_name()),
            ));
        }
    };
    Ok(Value::text(regclass_display(q, oid)))
}

/// v0.81: cast to a named composite type (`expr::t_rec`). Resolves
/// `name` against the type catalog (42704 if undefined or not a
/// composite). A `Value::Record` is coerced field-by-field: the source
/// must have at least as many fields as the target; extra source fields
/// are dropped (PG matches by position for ROW() casts); each field is
/// coerced to the target field type. A non-record source is 42846.
/// v1.38: apply a user-defined binary cast (`CREATE CAST ...
/// WITHOUT FUNCTION`) to a value. Returns `Ok(None)` when no cast is
/// registered for (value's physical type, target). The source key is
/// the value's physical type name (`integer`, `real`, `bigint`,
/// `double precision`); the target key folds builtins but keeps
/// LIKE-defined names. Validation guaranteed physical compatibility,
/// so this only reinterprets bits between the representable pairs
/// (int4<->float4, int8<->float8) or returns the value unchanged when
/// the physical types already match.
pub(crate) fn apply_binary_cast(q: &Q, v: &Value, name: &str) -> Result<Option<Value>, ExecError> {
    let src_key = v.col_type().sql_name().to_string();
    let dst_key = match cast_type_key(q.eng, name) {
        Some(k) => k,
        None => return Ok(None),
    };
    if q.eng.db.casts.get(&(src_key, dst_key)).is_none() {
        return Ok(None);
    }
    // Target's physical column type (LIKE types resolve to base).
    let dst_ct = {
        let mut cur = name;
        let ct = loop {
            if let Ok(ct) = crate::sql::coltype_by_name(cur) {
                break Some(ct);
            }
            match q
                .eng
                .db
                .types
                .get(cur)
                .and_then(|st| st.like_base.as_deref())
            {
                Some(base) => cur = base,
                None => break None,
            }
        };
        match ct {
            Some(ct) => ct,
            None => return Ok(None),
        }
    };
    match (v, dst_ct) {
        (Value::Int(i), crate::storage::ColType::Float4) => {
            Ok(Some(Value::Float4(f32::from_bits(*i as u32))))
        }
        (Value::Float4(f), crate::storage::ColType::Int) => {
            Ok(Some(Value::Int(f.to_bits() as i32 as i64)))
        }
        (Value::BigInt(i), crate::storage::ColType::Float) => {
            Ok(Some(Value::Float(f64::from_bits(*i as u64))))
        }
        (Value::Float(f), crate::storage::ColType::BigInt) => {
            Ok(Some(Value::BigInt(f.to_bits() as i64)))
        }
        _ if v.col_type() == dst_ct => Ok(Some(v.clone())),
        // No value-level rule (unreachable for validated casts) —
        // fall through to the normal cast path.
        _ => Ok(None),
    }
}

pub(crate) fn eval_cast_named(q: &mut Q, v: &Value, name: &str) -> Result<Value, ExecError> {
    // v0.85: cast to a domain — coerce to the base type, then enforce
    // the domain's constraints (PG19 coerce_to_domain). NULL still goes
    // through so NOT NULL domains can reject it (23502); plain casts
    // return NULL unchanged.
    let dom = q.eng.db.types.get(name).and_then(|st| st.domain.clone());
    if let Some(dom) = dom {
        if v == &Value::Null {
            return check_domain_value(q, name, v, false);
        }
        let coerced = match &dom.base {
            ColType::Composite => {
                let bn = dom.base_named.clone().ok_or_else(|| {
                    exec_err(
                        "0A000",
                        format!("domain \"{}\" has no composite base type", name),
                    )
                })?;
                eval_cast_composite_inner(q, v, &bn)?
            }
            _ => eval_cast(v, dom.base.clone())?,
        };
        return check_domain_value(q, name, &coerced, false);
    }
    if v == &Value::Null {
        return Ok(Value::Null);
    }
    // v1.38: user-defined binary casts take precedence over the
    // composite path (a LIKE-defined target is not a composite).
    if let Some(casted) = apply_binary_cast(q, v, name)? {
        return Ok(casted);
    }
    eval_cast_composite_inner(q, v, name)
}

/// v0.81: cast to a named composite type (`expr::t_rec`). Resolves
/// `name` against the type catalog (42704 if unknown, 42846 if not a
/// composite).
pub(crate) fn eval_cast_composite_inner(
    q: &mut Q,
    v: &Value,
    name: &str,
) -> Result<Value, ExecError> {
    // v0.86: every table has a rowtype (PG19) — resolve table names to
    // their columns when the name isn't a catalog composite type.
    let table_fields: Vec<(String, ColType, Option<String>)>;
    let fields: &[(String, ColType, Option<String>)] = match q.eng.db.types.get(name) {
        Some(st) => st
            .composite
            .as_ref()
            .ok_or_else(|| exec_err("42846", format!("cannot cast type record to {}", name)))?,
        None => match q.eng.db.find_table(name, q.snap, &q.all_xids, q.session) {
            Some(t) => {
                table_fields = t
                    .columns
                    .iter()
                    .map(|(n, c)| (n.clone(), c.clone(), None))
                    .collect();
                &table_fields
            }
            None => {
                return Err(exec_err(
                    "42704",
                    format!("type \"{}\" does not exist", name),
                ));
            }
        },
    };
    let src = match v {
        Value::Record(fields) => fields,
        // v0.82: composite text input `'(f1,f2,...)'::mytype` — PG19
        // `record_in` format, parsed field-wise against the composite
        // definition and coerced through each field's input function.
        Value::Text(s) | Value::BpChar(s) => {
            let parsed = parse_record_literal(s, name, fields, &q.eng.db.types)?;
            let mut out = Vec::with_capacity(fields.len());
            for ((fname, _, _), val) in fields.iter().zip(parsed) {
                out.push((fname.clone(), val));
            }
            return Ok(Value::Record(out));
        }
        _ => {
            return Err(exec_err(
                "42846",
                format!("cannot cast type {:?} to {}", v.col_type(), name),
            ));
        }
    };
    if src.len() < fields.len() {
        return Err(exec_err(
            "42846",
            format!(
                "cannot cast record with {} fields to {} with {} fields",
                src.len(),
                name,
                fields.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(fields.len());
    for (i, (fname, fty, _)) in fields.iter().enumerate() {
        let (_, sv) = &src[i];
        let cv = eval_cast(sv, *fty)?;
        out.push((fname.clone(), cv));
    }
    Ok(Value::Record(out))
}

pub(crate) fn eval_cast(v: &Value, to: ColType) -> Result<Value, ExecError> {
    if v == &Value::Null {
        return Ok(Value::Null);
    }
    if v.col_type() == to {
        return Ok(v.clone());
    }
    match to {
        ColType::Text => Ok(text_value_of(&[v])),
        // v1.40: cast to tid (from text `(b,o)`, like PG's tidin).
        ColType::Tid => {
            // Identity: already a tid.
            if let Value::Tid(b, o) = v {
                return Ok(Value::Tid(*b, *o));
            }
            // Text: parse `(b,o)`.
            if let Value::Text(t) = v {
                let s = t.trim().to_string();
                let inner = s
                    .strip_prefix('(')
                    .and_then(|t| t.strip_suffix(')'))
                    .ok_or_else(|| {
                        exec_err(
                            "22P02",
                            format!("invalid input syntax for type tid: \"{s}\""),
                        )
                    })?;
                let (bs, os) = inner.split_once(',').ok_or_else(|| {
                    exec_err(
                        "22P02",
                        format!("invalid input syntax for type tid: \"{s}\""),
                    )
                })?;
                let b: u32 = bs.trim().parse().map_err(|_| {
                    exec_err(
                        "22P02",
                        format!("invalid input syntax for type tid: \"{s}\""),
                    )
                })?;
                let o: u32 = os.trim().parse().map_err(|_| {
                    exec_err(
                        "22P02",
                        format!("invalid input syntax for type tid: \"{s}\""),
                    )
                })?;
                return Ok(Value::Tid(b, o));
            }
            return Err(exec_err("42846", "cannot cast to tid"));
        }
        // v0.64: cast to pg_lsn (from text like '0/016AE7F8' or numeric).
        // Note: `pg_lsn(23783416)` parses as a cast (function-call syntax
        // for the type), so numeric inputs must be handled here.
        ColType::PgLsn => {
            // Identity: already pg_lsn.
            if let Value::PgLsn(lsn) = v {
                return Ok(Value::PgLsn(*lsn));
            }
            // Text: parse HIGH/LOW hex format.
            if let Value::Text(t) = v {
                let s = t.to_string();
                let parts: Vec<&str> = s.split('/').collect();
                if parts.len() != 2 {
                    return Err(exec_err("22P02", "invalid pg_lsn format"));
                }
                let high = u64::from_str_radix(parts[0], 16)
                    .map_err(|_| exec_err("22P02", "invalid pg_lsn"))?;
                let low = u64::from_str_radix(parts[1], 16)
                    .map_err(|_| exec_err("22P02", "invalid pg_lsn"))?;
                return Ok(Value::PgLsn((high << 32) | low));
            }
            // Numeric: convert via numeric_to_u64_exact (PG's pg_lsn(numeric)).
            if let Some(num) = to_numeric_opt(v) {
                if num.is_nan() {
                    return Err(exec_err("0A000", "cannot convert NaN to pg_lsn"));
                }
                if num.is_special() {
                    return Err(exec_err("0A000", "cannot convert infinity to pg_lsn"));
                }
                let lsn = numeric_to_u64_exact(&num)
                    .ok_or_else(|| exec_err("22023", "pg_lsn out of range"))?;
                return Ok(Value::PgLsn(lsn));
            }
            return Err(exec_err("42846", "cannot cast to pg_lsn"));
        }
        // v1.17: cast to xid (from integer or unsigned-decimal text,
        // like PG's xidin; `SELECT 123::xid` works in PG).
        ColType::Xid => {
            // Identity: already an xid (carried as Value::Int).
            if let Value::Int(i) = v {
                return Ok(Value::Int(*i));
            }
            // Text: parse unsigned decimal (PG's xidin).
            if let Value::Text(t) = v {
                let s = t.to_string();
                let n = s.parse::<u64>().map_err(|_| {
                    exec_err(
                        "22P02",
                        format!("invalid input syntax for type xid: \"{s}\""),
                    )
                })?;
                return Ok(Value::Int(n as i64));
            }
            // SmallInt/BigInt: widen (PG casts int2/int8 to xid).
            match v {
                Value::SmallInt(s) => return Ok(Value::Int(*s as i64)),
                Value::BigInt(b) => return Ok(Value::Int(*b)),
                _ => {}
            }
            return Err(exec_err("42846", "cannot cast to xid"));
        }
        // v0.35: explicit casts to character(n) / varchar(n): silent
        // truncation of any excess, blank-padding for char. A bpchar
        // source keeps its padding for char targets (PG's bpchar()
        // works on the padded datum); for varchar targets it is first
        // rtrimmed (PG's varchar(bpchar) goes through text(bpchar)).
        // Non-text sources go through their text form.
        ColType::Char(n) => match v {
            Value::Text(s) | Value::BpChar(s) => eval_char_cast(s, n, true),
            _ => {
                let t = text_value_of(&[v]);
                match &t {
                    Value::Text(s) => eval_char_cast(s, n, true),
                    _ => Ok(t),
                }
            }
        },
        ColType::Varchar(n) => match v {
            Value::BpChar(s) => eval_char_cast(crate::storage::rtrim_spaces(s), n, false),
            Value::Text(s) => eval_char_cast(s, n, false),
            _ => {
                let t = text_value_of(&[v]);
                match &t {
                    Value::Text(s) => eval_char_cast(s, n, false),
                    _ => Ok(t),
                }
            }
        },
        // v0.36: casts to PG's one-byte `"char"` type go through charin
        // (char.c): `\ooo` is a bytea-style octal escape, otherwise the
        // FIRST byte wins (empty is NUL); charin is total, never errors.
        // int4 -> "char" is i4tochar (explicit): -128..=127 else 22003
        // `"char" out of range`. Like PG, text/bpchar/int4 are the only
        // sources with a cast to "char".
        ColType::SingleChar => match v {
            Value::Text(s) => Ok(Value::SingleChar(crate::storage::char_in(s))),
            // A bpchar source goes through text(bpchar) first (rtrim),
            // like PG's cast chain.
            Value::BpChar(s) => Ok(Value::SingleChar(crate::storage::char_in(
                crate::storage::rtrim_spaces(s),
            ))),
            Value::Int(i) => {
                if !(-128..=127).contains(i) {
                    Err(exec_err("22003", "\"char\" out of range"))
                } else {
                    Ok(Value::SingleChar(*i as u8))
                }
            }
            other => Err(cast_err(other, "\"char\"")),
        },
        ColType::Bool => cast_to_bool(v).map(Value::Bool),
        ColType::SmallInt => {
            let i = match v {
                Value::Bytea(b) => cast_bytea_to_int(b, 2, "smallint")?,
                _ => cast_to_int(v)?,
            };
            i16::try_from(i)
                .map(Value::SmallInt)
                .map_err(|_| exec_err("22003", "smallint out of range"))
        }
        ColType::Int => {
            let i = match v {
                Value::Bytea(b) => cast_bytea_to_int(b, 4, "integer")?,
                // v0.36: "char" -> int4 is chartoi4: the byte as SIGNED int8.
                Value::SingleChar(b) => i128::from(*b as i8),
                // v1.39: bit -> int4 is PG19 `bittoint4` (varbit.c),
                // an explicit-context cast (pg_cast.dat 'e').
                Value::BitString(bs) => return bit_to_int4(bs).map(|w| Value::Int(w as i64)),
                _ => cast_to_int(v)?,
            };
            i32::try_from(i)
                .map(|w| Value::Int(w as i64))
                .map_err(|_| exec_err("22003", "integer out of range"))
        }
        ColType::BigInt => {
            let i = match v {
                Value::Bytea(b) => cast_bytea_to_int(b, 8, "bigint")?,
                // v1.39: bit -> int8 is PG19 `bittoint8` (varbit.c),
                // an explicit-context cast (pg_cast.dat 'e').
                Value::BitString(bs) => return bit_to_int8(bs).map(Value::BigInt),
                _ => cast_to_int(v)?,
            };
            i64::try_from(i)
                .map(Value::BigInt)
                .map_err(|_| exec_err("22003", "bigint out of range"))
        }
        ColType::Float4 => match v {
            // v0.21: text goes through the checked float4 parser
            // (overflow/underflow are 22003, like float8); other inputs
            // widen through f64 and narrow with the same range check.
            Value::Text(s) => parse_f32_checked(s).map(Value::Float4),
            _ => narrow_to_f32(cast_to_f64(v)?).map(Value::Float4),
        },
        ColType::Float => cast_to_f64(v).map(Value::Float),
        // v0.60: numeric(p,s) casts apply PG19's typmod.
        ColType::Numeric(tm) => cast_to_numeric(v)
            .and_then(|n| apply_numeric_typmod(n, tm))
            .map(Value::Numeric),
        ColType::Date => match v {
            Value::Text(s) => crate::datetime::parse_date(s)
                .map(Value::Date)
                .map_err(|e| exec_err("22P02", e)),
            // Timestamps truncate to their UTC date (v0.7 is always UTC).
            Value::Timestamp(m) | Value::Timestamptz(m) => {
                Ok(Value::Date((m.div_euclid(86_400_000_000)) as i32))
            }
            other => Err(cast_err(other, "date")),
        },
        ColType::Timestamp => match v {
            Value::Text(s) => crate::datetime::parse_timestamp(s)
                .map(Value::Timestamp)
                .map_err(|e| exec_err("22P02", e)),
            Value::Date(d) => (*d as i64)
                .checked_mul(86_400_000_000)
                .map(Value::Timestamp)
                .ok_or_else(|| exec_err("22008", "datetime field overflow")),
            // Timestamptz -> timestamp keeps the instant; v0.7 has no
            // session timezone so this is the UTC wall-clock time.
            Value::Timestamptz(m) => Ok(Value::Timestamp(*m)),
            other => Err(cast_err(other, "timestamp without time zone")),
        },
        ColType::Timestamptz => match v {
            Value::Text(s) => crate::datetime::parse_timestamptz(s)
                .map(Value::Timestamptz)
                .map_err(|e| exec_err("22P02", e)),
            Value::Date(d) => (*d as i64)
                .checked_mul(86_400_000_000)
                .map(Value::Timestamptz)
                .ok_or_else(|| exec_err("22008", "datetime field overflow")),
            // Timestamp -> timestamptz assumes UTC (documented: v0.7
            // has no session timezone setting).
            Value::Timestamp(m) => Ok(Value::Timestamptz(*m)),
            other => Err(cast_err(other, "timestamp with time zone")),
        },
        ColType::Bytea => match v {
            Value::Text(s) => crate::storage::parse_bytea(s)
                .map(Value::Bytea)
                .map_err(|e| {
                    // v0.29: PG's exact hex-format errors (22023); escape
                    // format stays 22P02 like PG.
                    use crate::storage::ByteaParseError;
                    match e {
                        ByteaParseError::OddHexDigits => {
                            exec_err("22023", "invalid hexadecimal data: odd number of digits")
                        }
                        ByteaParseError::BadHexDigit(c) => {
                            exec_err("22023", format!("invalid hexadecimal digit: \"{}\"", c))
                        }
                        ByteaParseError::Invalid => {
                            exec_err("22P02", "invalid input syntax for type bytea")
                        }
                    }
                }),
            // v0.28: integer -> bytea is big-endian binary.
            Value::SmallInt(i) => Ok(Value::Bytea(i.to_be_bytes().to_vec())),
            Value::Int(i) => Ok(Value::Bytea((*i as i32).to_be_bytes().to_vec())),
            Value::BigInt(i) => Ok(Value::Bytea(i.to_be_bytes().to_vec())),
            other => Err(cast_err(other, "bytea")),
        },
        // v1.39: `bit` target — identity for bit values (PG's bit(bit)
        // typmod cast is a no-op here since the value carries its own
        // bit length). No other source casts to bit (PG19 pg_cast.dat
        // has only int4/int8 -> bit as explicit casts, which need the
        // int-to-bits conversion — not yet implemented).
        ColType::Bit => match v {
            Value::BitString(bs) => Ok(Value::BitString(bs.clone())),
            other => Err(cast_err(other, "bit")),
        },
        ColType::Uuid => match v {
            Value::Text(s) => crate::storage::parse_uuid(s).map(Value::Uuid).map_err(|_| {
                exec_err(
                    "22P02",
                    format!("invalid input syntax for type uuid: {:?}", s),
                )
            }),
            other => Err(cast_err(other, "uuid")),
        },
        // v0.37: regclass is handled in eval_expr (needs catalog access).
        // This arm is unreachable but required for exhaustiveness.
        ColType::Regclass => Err(cast_err(v, "regclass")),
        // v0.57: casts to `name` go through namein (name.c, PG19):
        // text/bpchar input is silently truncated at 63 bytes; any
        // other source goes through its text form first. Total: never
        // errors.
        ColType::Name => match v {
            Value::Text(s) | Value::BpChar(s) => Ok(Value::text(crate::storage::truncate_name(s))),
            _ => {
                let t = text_value_of(&[v]);
                match &t {
                    Value::Text(s) => Ok(Value::text(crate::storage::truncate_name(s))),
                    _ => Ok(t),
                }
            }
        },
        // v0.73: no composite input function — nothing casts TO record.
        ColType::Record => Err(cast_err(v, "record")),
        // v0.73: PG19 has record->json and text->json casts.
        ColType::Json => match v {
            Value::Record(fields) => Ok(Value::text(row_to_json_text(fields))),
            Value::Text(_) | Value::BpChar(_) => Ok(text_value_of(&[v])),
            other => Err(cast_err(other, "json")),
        },
        // v0.79: real array casts (PG19 array_in/array_out semantics).
        // Text (or bpchar) input parses as a `{...}` literal through
        // the element type's input function (22P02 on bad elements, like
        // PG); array-to-array casts retype element-wise, preserving
        // dims, lower bounds, and NULL elements.
        ColType::Array(elem) => match v {
            Value::Text(s) | Value::BpChar(s) => {
                parse_array_literal(s, elem).map(|a| Value::Array(Box::new(a)))
            }
            Value::Array(a) => {
                let ty = elem_scalar_type(elem);
                let mut out = Vec::with_capacity(a.elems.len());
                for e in &a.elems {
                    out.push(eval_cast(e, ty)?);
                }
                Ok(Value::Array(Box::new(ArrayVal {
                    elem,
                    dims: a.dims.clone(),
                    lower: a.lower.clone(),
                    elems: out,
                })))
            }
            other => Err(cast_err(other, &to.sql_name())),
        },
        // v0.81: the builtin eval_cast never receives a named composite
        // target (named casts resolve against the type catalog before
        // reaching it); nothing casts here.
        ColType::Composite => Err(exec_err("42846", "cannot cast to composite type here")),
    }
}
