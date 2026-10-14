//! v1.88: range / multirange types (PG19 rangetypes.c /
//! multirangetypes.c). One mechanism over `Value` bounds: parsing with
//! PG's exact malformed-literal errors, discrete canonicalization to
//! `[lo, hi)`, range_out quoting (range_bound_escape), containment /
//! overlap / union / intersection / difference, and multirange
//! normalization (sort + merge overlapping or adjacent ranges).
use super::*;
use crate::storage::{MultiVal, RangeElem, RangeVal};

pub(crate) fn elem_coltype(elem: RangeElem) -> ColType {
    match elem {
        RangeElem::Int4 => ColType::Int,
        RangeElem::Int8 => ColType::BigInt,
        RangeElem::Int2 => ColType::SmallInt,
        RangeElem::Num => ColType::Numeric(None),
        RangeElem::Date => ColType::Date,
        RangeElem::Ts => ColType::Timestamp,
        RangeElem::Tstz => ColType::Timestamptz,
        RangeElem::Text => ColType::Text,
        RangeElem::Float8 => ColType::Float,
    }
}

/// PG name of a range type for error messages ("integer range" style is
/// not used by PG here; errors quote the typname, e.g. `int4range`).
pub(crate) fn range_type_name(elem: RangeElem, name: &Option<String>) -> String {
    match name {
        Some(n) => n.clone(),
        None => elem.range_name().unwrap_or("range").to_string(),
    }
}

pub(crate) fn multirange_type_name(elem: RangeElem, name: &Option<String>) -> String {
    match name {
        Some(n) => n.clone(),
        None => elem.multirange_name().unwrap_or("multirange").to_string(),
    }
}

// ---------------------------------------------------------------------------
// Bound values
// ---------------------------------------------------------------------------

/// Compare two bound values of the same elem kind. Mixed integer kinds
/// compare as i64; numeric vs integer goes through Numeric (PG compares
/// in the subtype's own ordering, and constructors coerce bounds to the
/// elem kind before they get here, so pairs are normally homogeneous).
pub(crate) fn bound_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::SmallInt(x), Value::SmallInt(y)) => x.cmp(y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::SmallInt(x), Value::Int(y)) | (Value::Int(y), Value::SmallInt(x)) => {
            let o = (*x as i64).cmp(y);
            if matches!(a, Value::SmallInt(_)) {
                o
            } else {
                o.reverse()
            }
        }
        (Value::Numeric(x), Value::Numeric(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::BpChar(x), Value::BpChar(y)) => x.trim_end().cmp(y.trim_end()),
        (Value::Text(x), Value::BpChar(y)) => x.as_ref().cmp(y.trim_end()),
        (Value::BpChar(x), Value::Text(y)) => x.trim_end().cmp(y.as_ref()),
        (Value::Float(x), Value::Float(y)) => super::eval::pg_float_ord(*x, *y),
        (Value::Float4(x), Value::Float4(y)) => super::eval::pg_float_ord(*x as f64, *y as f64),
        _ => a
            .to_text()
            .unwrap_or_default()
            .cmp(&b.to_text().unwrap_or_default()),
    }
}

fn bound_eq(a: &Value, b: &Value) -> bool {
    bound_cmp(a, b) == Ordering::Equal
}

/// One canonical step up for discrete subtypes (PG canonical functions).
/// Overflow is an error, exactly like int4range_canonical (22003) and
/// daterange_canonical (22008 via the date_out_of_range check).
fn step_up(elem: RangeElem, v: &Value) -> Result<Value, ExecError> {
    Ok(match (elem, v) {
        (RangeElem::Int4, Value::Int(x)) => {
            if *x == i32::MAX as i64 {
                return Err(exec_err("22003", "integer out of range"));
            }
            Value::Int(x + 1)
        }
        (RangeElem::Int2, Value::SmallInt(x)) => {
            if *x == i16::MAX {
                return Err(exec_err("22003", "smallint out of range"));
            }
            Value::SmallInt(x + 1)
        }
        (RangeElem::Int8, Value::BigInt(x)) => {
            if *x == i64::MAX {
                return Err(exec_err("22003", "bigint out of range"));
            }
            Value::BigInt(x + 1)
        }
        (RangeElem::Date, Value::Date(d)) => {
            // DATE_NOT_FINITE bounds are skipped by daterange_canonical.
            if *d == crate::datetime::DATE_INFINITY || *d == crate::datetime::DATE_NEG_INFINITY {
                Value::Date(*d)
            } else if *d as i64 + 1 > crate::datetime::date_max_days() {
                return Err(exec_err("22008", "date out of range"));
            } else {
                Value::Date(d + 1)
            }
        }
        _ => v.clone(),
    })
}

// ---------------------------------------------------------------------------
// Construction / canonicalization (range_serialize + canonical functions)
// ---------------------------------------------------------------------------

/// Build a canonical range from raw bounds, PG `range_in` /
/// range-constructor semantics: lower > upper is 22000; equal bounds are
/// empty unless both inclusive; discrete subtypes canonicalize to
/// `[lo, hi)` (which can itself produce emptiness).
pub(crate) fn make_range(
    elem: RangeElem,
    name: Option<String>,
    lo: Option<Value>,
    lo_inc: bool,
    hi: Option<Value>,
    hi_inc: bool,
) -> Result<RangeVal, ExecError> {
    let empty = RangeVal {
        elem,
        name: name.clone(),
        empty: true,
        lo: None,
        lo_inc: false,
        hi: None,
        hi_inc: false,
    };
    if let (Some(l), Some(u)) = (&lo, &hi) {
        match bound_cmp(l, u) {
            Ordering::Greater => {
                return Err(exec_err(
                    "22000",
                    "range lower bound must be less than or equal to range upper bound",
                ));
            }
            Ordering::Equal if !(lo_inc && hi_inc) => return Ok(empty),
            _ => {}
        }
    }
    let (mut lo, mut lo_inc, mut hi, mut hi_inc) = (lo, lo_inc, hi, hi_inc);
    // PG range_serialize: infinite boundaries are never inclusive.
    if lo.is_none() {
        lo_inc = false;
    }
    if hi.is_none() {
        hi_inc = false;
    }
    if elem.discrete() {
        if let Some(l) = &lo {
            if !lo_inc {
                lo = Some(step_up(elem, l)?);
                lo_inc = true;
            }
        }
        if let Some(u) = &hi {
            if hi_inc {
                hi = Some(step_up(elem, u)?);
                hi_inc = false;
            }
        }
        if let (Some(l), Some(u)) = (&lo, &hi) {
            if bound_cmp(l, u) != Ordering::Less {
                return Ok(empty);
            }
        }
    }
    Ok(RangeVal {
        elem,
        name,
        empty: false,
        lo,
        lo_inc,
        hi,
        hi_inc,
    })
}

pub(crate) fn empty_range(elem: RangeElem, name: Option<String>) -> RangeVal {
    RangeVal {
        elem,
        name,
        empty: true,
        lo: None,
        lo_inc: false,
        hi: None,
        hi_inc: false,
    }
}

/// Range from constructor arguments (PG range_constructor_internal):
/// NULL bounds are infinite; `flags` is the two-character bound-flags
/// string ("[)" default), invalid flags are 42601 (range_parse_flags).
pub(crate) fn construct_range(
    elem: RangeElem,
    name: Option<String>,
    lo: &Value,
    hi: &Value,
    flags: Option<&str>,
) -> Result<RangeVal, ExecError> {
    let (lo_inc, hi_inc) = match flags {
        None => (true, false),
        Some(f) => {
            let b = f.as_bytes();
            let bad = || exec_err("42601", "invalid range bound flags");
            if b.len() != 2 {
                return Err(bad());
            }
            let lo_inc = match b[0] {
                b'[' => true,
                b'(' => false,
                _ => return Err(bad()),
            };
            let hi_inc = match b[1] {
                b']' => true,
                b')' => false,
                _ => return Err(bad()),
            };
            (lo_inc, hi_inc)
        }
    };
    let lo = if *lo == Value::Null {
        None
    } else {
        Some(lo.clone())
    };
    let hi = if *hi == Value::Null {
        None
    } else {
        Some(hi.clone())
    };
    make_range(elem, name, lo, lo_inc, hi, hi_inc)
}

// ---------------------------------------------------------------------------
// Text input (range_in / multirange_in grammars, mirrored from pg_input.rs)
// ---------------------------------------------------------------------------

fn malformed(input: &str, detail: &str) -> ExecError {
    exec_err_detail(
        "22P02",
        format!("malformed range literal: \"{input}\""),
        detail,
    )
}

fn multi_malformed(input: &str, detail: &str) -> ExecError {
    exec_err_detail(
        "22P02",
        format!("malformed multirange literal: \"{input}\""),
        detail,
    )
}

struct RawRange {
    lo_inc: bool,
    lo: Option<String>,
    hi: Option<String>,
    hi_inc: bool,
    empty: bool,
}

fn raw_parse_bound(
    input: &str,
    b: &[u8],
    mut i: usize,
) -> Result<(Option<String>, usize), ExecError> {
    match b.get(i).copied() {
        Some(b',') | Some(b')') | Some(b']') => return Ok((None, i)),
        _ => {}
    }
    let mut out = String::new();
    let mut inquote = false;
    loop {
        let ch = match b.get(i).copied() {
            None => return Err(malformed(input, "Unexpected end of input.")),
            Some(c) => c,
        };
        if ch == b'\\' {
            let nxt = match b.get(i + 1).copied() {
                None => return Err(malformed(input, "Unexpected end of input.")),
                Some(c) => c,
            };
            out.push(nxt as char);
            i += 2;
        } else if ch == b'"' {
            if !inquote {
                inquote = true;
                i += 1;
            } else if b.get(i + 1).copied() == Some(b'"') {
                out.push('"');
                i += 2;
            } else {
                inquote = false;
                i += 1;
            }
        } else if !inquote && matches!(ch, b',' | b')' | b']') {
            break;
        } else {
            out.push(ch as char);
            i += 1;
        }
    }
    Ok((Some(out), i))
}

fn raw_parse_range(input: &str) -> Result<RawRange, ExecError> {
    let b = input.as_bytes();
    let mut i = 0usize;
    let skip_ws = |i: &mut usize| {
        while *i < b.len() && (b[*i] as char).is_whitespace() {
            *i += 1;
        }
    };
    skip_ws(&mut i);
    if input[i..].len() >= 5 && input[i..][..5].eq_ignore_ascii_case("empty") {
        i += 5;
        skip_ws(&mut i);
        if i != b.len() {
            return Err(malformed(input, "Junk after \"empty\" key word."));
        }
        return Ok(RawRange {
            lo_inc: false,
            lo: None,
            hi: None,
            hi_inc: false,
            empty: true,
        });
    }
    let mut r = RawRange {
        lo_inc: false,
        lo: None,
        hi: None,
        hi_inc: false,
        empty: false,
    };
    match b.get(i).copied() {
        Some(b'[') => {
            r.lo_inc = true;
            i += 1;
        }
        Some(b'(') => i += 1,
        _ => return Err(malformed(input, "Missing left parenthesis or bracket.")),
    }
    let (lo, ni) = raw_parse_bound(input, b, i)?;
    r.lo = lo;
    i = ni;
    if b.get(i).copied() == Some(b',') {
        i += 1;
    } else {
        return Err(malformed(input, "Missing comma after lower bound."));
    }
    let (hi, ni) = raw_parse_bound(input, b, i)?;
    r.hi = hi;
    i = ni;
    match b.get(i).copied() {
        Some(b']') => {
            r.hi_inc = true;
            i += 1;
        }
        Some(b')') => i += 1,
        _ => return Err(malformed(input, "Too many commas.")),
    }
    skip_ws(&mut i);
    if i != b.len() {
        return Err(malformed(input, "Junk after right parenthesis or bracket."));
    }
    Ok(r)
}

/// Parse one bound's text into a `Value` via the subtype's input rules.
pub(crate) fn parse_bound(elem: RangeElem, text: &str) -> Result<Value, ExecError> {
    Ok(match elem {
        RangeElem::Int4 | RangeElem::Int8 | RangeElem::Int2 => {
            let t = text.trim();
            let (bits, pgname): (u32, &str) = match elem {
                RangeElem::Int4 => (32, "integer"),
                RangeElem::Int8 => (64, "bigint"),
                _ => (16, "smallint"),
            };
            let v: i64 = match parse_pg_int(t, bits) {
                IntParse::Ok(v) => v,
                IntParse::Syntax => {
                    return Err(exec_err(
                        "22P02",
                        format!("invalid input syntax for type {pgname}: \"{text}\""),
                    ));
                }
                IntParse::Range => {
                    return Err(exec_err(
                        "22003",
                        format!("value \"{text}\" is out of range for type {pgname}"),
                    ));
                }
            };
            match elem {
                RangeElem::Int4 => Value::Int(v),
                RangeElem::Int8 => Value::BigInt(v),
                _ => Value::SmallInt(v as i16),
            }
        }
        RangeElem::Num => match Numeric::parse(text) {
            Ok(n) => Value::Numeric(n),
            Err(crate::storage::NumericParseError::Overflow) => {
                return Err(exec_err("22003", "value overflows numeric format"));
            }
            Err(_) => {
                return Err(exec_err(
                    "22P02",
                    format!("invalid input syntax for type numeric: \"{text}\""),
                ));
            }
        },
        RangeElem::Date => Value::Date(
            crate::datetime::parse_date(text)
                .map_err(|e| exec_err(super::eval::dt_sqlstate(&e), e))?,
        ),
        RangeElem::Ts => Value::Timestamp(
            crate::datetime::parse_timestamp(text)
                .map_err(|e| exec_err(super::eval::dt_sqlstate(&e), e))?,
        ),
        RangeElem::Tstz => Value::Timestamptz(
            crate::datetime::parse_timestamptz(text)
                .map_err(|e| exec_err(super::eval::dt_sqlstate(&e), e))?,
        ),
        RangeElem::Text => Value::text(text),
        RangeElem::Float8 => Value::Float(parse_pg_float(text)?),
    })
}

enum IntParse {
    Ok(i64),
    Syntax,
    Range,
}

/// PG's pg_strtoint family: optional sign, digits with `_` separators
/// (PG16+), whole string, range-checked at `bits`.
fn parse_pg_int(t: &str, bits: u32) -> IntParse {
    let cleaned: String = t.chars().filter(|c| *c != '_').collect();
    let v: i128 = match cleaned.parse::<i128>() {
        Ok(v) => v,
        Err(_) => {
            // Distinguish syntax from range: an all-digits (signed)
            // string that overflows i128 is a range error.
            let body = cleaned.trim_start_matches(['+', '-']);
            if !body.is_empty() && body.chars().all(|c| c.is_ascii_digit()) {
                return IntParse::Range;
            }
            return IntParse::Syntax;
        }
    };
    let (min, max): (i128, i128) = match bits {
        16 => (i16::MIN as i128, i16::MAX as i128),
        32 => (i32::MIN as i128, i32::MAX as i128),
        _ => (i64::MIN as i128, i64::MAX as i128),
    };
    if v < min || v > max {
        return IntParse::Range;
    }
    IntParse::Ok(v as i64)
}

fn parse_pg_float(text: &str) -> Result<f64, ExecError> {
    let t = text.trim();
    let low = t.to_ascii_lowercase();
    let v = match low.as_str() {
        "infinity" | "+infinity" | "inf" | "+inf" => f64::INFINITY,
        "-infinity" | "-inf" => f64::NEG_INFINITY,
        "nan" => f64::NAN,
        _ => match t.parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                return Err(exec_err(
                    "22P02",
                    format!("invalid input syntax for type double precision: \"{text}\""),
                ));
            }
        },
    };
    Ok(v)
}

/// PG `range_in`: parse + validate + canonicalize a range literal.
pub(crate) fn parse_range(
    elem: RangeElem,
    name: Option<String>,
    input: &str,
) -> Result<RangeVal, ExecError> {
    let raw = raw_parse_range(input)?;
    if raw.empty {
        return Ok(empty_range(elem, name));
    }
    let lo = match &raw.lo {
        Some(t) => Some(parse_bound(elem, t)?),
        None => None,
    };
    let hi = match &raw.hi {
        Some(t) => Some(parse_bound(elem, t)?),
        None => None,
    };
    make_range(elem, name, lo, raw.lo_inc, hi, raw.hi_inc)
}

/// Split a multirange literal into its raw range strings (the v1.87
/// multirange_in state machine, producing substrings).
fn split_multirange(input: &str) -> Result<Vec<String>, ExecError> {
    let b = input.as_bytes();
    let mut i = 0usize;
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    if b.get(i).copied() != Some(b'{') {
        return Err(multi_malformed(input, "Missing left brace."));
    }
    i += 1;
    #[derive(PartialEq)]
    enum Ms {
        Before,
        InRange,
        InQuoted,
        InEscaped,
        InQuotedEscaped,
        After,
        Finished,
    }
    let mut state = Ms::Before;
    let mut range_begin = 0usize;
    let mut seen = 0usize;
    let mut ranges: Vec<String> = Vec::new();
    while state != Ms::Finished {
        let ch = match b.get(i).copied() {
            None => return Err(multi_malformed(input, "Unexpected end of input.")),
            Some(c) => c,
        };
        if (ch as char).is_whitespace() && !matches!(state, Ms::InRange) {
            i += 1;
            continue;
        }
        match state {
            Ms::Before => {
                if ch == b'[' || ch == b'(' {
                    range_begin = i;
                    state = Ms::InRange;
                } else if ch == b'}' && seen == 0 {
                    state = Ms::Finished;
                } else if input[i..].len() >= 5 && input[i..][..5].eq_ignore_ascii_case("empty") {
                    ranges.push(input[i..i + 5].to_string());
                    seen += 1;
                    i += 5;
                    state = Ms::After;
                    continue;
                } else {
                    return Err(multi_malformed(input, "Expected range start."));
                }
            }
            Ms::InRange => {
                if ch == b']' || ch == b')' {
                    ranges.push(input[range_begin..=i].to_string());
                    seen += 1;
                    state = Ms::After;
                } else if ch == b'"' {
                    state = Ms::InQuoted;
                } else if ch == b'\\' {
                    state = Ms::InEscaped;
                }
            }
            Ms::InEscaped => state = Ms::InRange,
            Ms::InQuoted => {
                if ch == b'"' {
                    if b.get(i + 1).copied() == Some(b'"') {
                        i += 1;
                    } else {
                        state = Ms::InRange;
                    }
                } else if ch == b'\\' {
                    state = Ms::InQuotedEscaped;
                }
            }
            Ms::InQuotedEscaped => state = Ms::InQuoted,
            Ms::After => {
                if ch == b',' {
                    state = Ms::Before;
                } else if ch == b'}' {
                    state = Ms::Finished;
                } else {
                    return Err(multi_malformed(
                        input,
                        "Expected comma or end of multirange.",
                    ));
                }
            }
            Ms::Finished => {}
        }
        i += 1;
    }
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    if i != b.len() {
        return Err(multi_malformed(input, "Junk after closing right brace."));
    }
    Ok(ranges)
}

/// PG `multirange_in`: parse each range, then normalize.
pub(crate) fn parse_multirange(
    elem: RangeElem,
    name: Option<String>,
    input: &str,
) -> Result<MultiVal, ExecError> {
    let parts = split_multirange(input)?;
    let mut ranges = Vec::with_capacity(parts.len());
    for p in &parts {
        // A malformed component reports as a malformed range literal
        // quoting the component (PG range_in on the substring).
        ranges.push(parse_range(elem, name.clone(), p)?);
    }
    Ok(normalize_multi(elem, name, ranges))
}

// ---------------------------------------------------------------------------
// Text output (range_out / multirange_out)
// ---------------------------------------------------------------------------

/// PG range_bound_escape: quote when empty or containing whitespace,
/// `"`, `\`, `(`, `)`, `[`, `]`, `,`; double `"` and `\` inside quotes.
fn bound_escape(s: &str) -> String {
    let need = s.is_empty()
        || s.chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ','));
    if !need {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn bound_text(v: &Value) -> String {
    v.to_text().unwrap_or_default()
}

pub(crate) fn range_to_text(r: &RangeVal) -> String {
    if r.empty {
        return "empty".to_string();
    }
    let mut s = String::new();
    s.push(if r.lo_inc { '[' } else { '(' });
    if let Some(l) = &r.lo {
        s.push_str(&bound_escape(&bound_text(l)));
    }
    s.push(',');
    if let Some(u) = &r.hi {
        s.push_str(&bound_escape(&bound_text(u)));
    }
    s.push(if r.hi_inc { ']' } else { ')' });
    s
}

pub(crate) fn multirange_to_text(m: &MultiVal) -> String {
    let mut s = String::from("{");
    for (i, r) in m.ranges.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&range_to_text(r));
    }
    s.push('}');
    s
}

// ---------------------------------------------------------------------------
// Multirange normalization (multirange_canonicalize)
// ---------------------------------------------------------------------------

/// Lower-bound ordering key: unbounded first; then value; inclusive
/// before exclusive at equal values (a `[5` bound starts before `(5`).
fn lo_cmp(a: &RangeVal, b: &RangeVal) -> Ordering {
    match (&a.lo, &b.lo) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => match bound_cmp(x, y) {
            Ordering::Equal => match (a.lo_inc, b.lo_inc) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => Ordering::Equal,
            },
            o => o,
        },
    }
}

/// Upper-bound ordering key: unbounded last; then value; exclusive
/// before inclusive at equal values (`...,5)` ends before `...,5]`).
fn hi_cmp(a: &RangeVal, b: &RangeVal) -> Ordering {
    match (&a.hi, &b.hi) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => match bound_cmp(x, y) {
            Ordering::Equal => match (a.hi_inc, b.hi_inc) {
                (false, true) => Ordering::Less,
                (true, false) => Ordering::Greater,
                _ => Ordering::Equal,
            },
            o => o,
        },
    }
}

/// Is there a gap between range `a` (ending first) and range `b`
/// (starting later)? Ranges merge iff they overlap or are adjacent —
/// adjacency is `bounds_adjacent`: a's upper meets b's lower, i.e. the
/// values are equal and at least one side is inclusive (for canonical
/// discrete ranges `[x,y)` + `[y,z)`, a's upper is exclusive but the
/// stepped form makes them meet: equality of the *values* with
/// a.hi_inc==false,b.lo_inc==true at the same value IS adjacency for
/// discrete types, and for continuous types `(...,5)` + `[5,...)` is
/// also contiguous). PG's bounds_adjacent for canonical forms reduces
/// to: values equal AND NOT (both exclusive at a point neither
/// contains) — `[1,5)` and `(5,7]` both exclude 5? No: `[1,5)` excludes
/// 5 and `(5,7]` excludes 5, so 5 is a gap point: NOT adjacent. The
/// rule is: adjacent iff values equal and (a.hi_inc || b.lo_inc).
fn gap_between(a: &RangeVal, b: &RangeVal) -> bool {
    match (&a.hi, &b.lo) {
        (None, _) | (_, None) => false, // an infinite bound swallows the rest
        (Some(u), Some(l)) => match bound_cmp(u, l) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => !(a.hi_inc || b.lo_inc),
        },
    }
}

pub(crate) fn normalize_multi(
    elem: RangeElem,
    name: Option<String>,
    mut ranges: Vec<RangeVal>,
) -> MultiVal {
    ranges.retain(|r| !r.empty);
    ranges.sort_by(|a, b| lo_cmp(a, b).then_with(|| hi_cmp(a, b)));
    let mut out: Vec<RangeVal> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if !gap_between(last, &r) => {
                // Merge: keep the later upper bound.
                if hi_cmp(&r, last) == Ordering::Greater {
                    last.hi = r.hi.clone();
                    last.hi_inc = r.hi_inc;
                }
            }
            _ => out.push(r),
        }
    }
    MultiVal {
        elem,
        name,
        ranges: out,
    }
}

pub(crate) fn multi_from_range(r: RangeVal) -> MultiVal {
    let elem = r.elem;
    let name = r.name.clone();
    normalize_multi(elem, name, vec![r])
}

// ---------------------------------------------------------------------------
// Predicates (rangetypes.c range_*_internal)
// ---------------------------------------------------------------------------

/// Does range `r` contain value `v`? (the `@>` element form)
pub(crate) fn range_contains_elem(r: &RangeVal, v: &Value) -> bool {
    if r.empty {
        return false;
    }
    if let Some(l) = &r.lo {
        match bound_cmp(v, l) {
            Ordering::Less => return false,
            Ordering::Equal if !r.lo_inc => return false,
            _ => {}
        }
    }
    if let Some(u) = &r.hi {
        match bound_cmp(v, u) {
            Ordering::Greater => return false,
            Ordering::Equal if !r.hi_inc => return false,
            _ => {}
        }
    }
    true
}

/// range_contains_range: every point of `b` is in `a`.
pub(crate) fn range_contains_range(a: &RangeVal, b: &RangeVal) -> bool {
    if b.empty {
        return true;
    }
    if a.empty {
        return false;
    }
    // a's lower must start at or before b's lower.
    let lo_ok = match (&a.lo, &b.lo) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(x), Some(y)) => match bound_cmp(x, y) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => a.lo_inc || !b.lo_inc,
        },
    };
    if !lo_ok {
        return false;
    }
    match (&a.hi, &b.hi) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(x), Some(y)) => match bound_cmp(x, y) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => a.hi_inc || !b.hi_inc,
        },
    }
}

pub(crate) fn range_overlaps(a: &RangeVal, b: &RangeVal) -> bool {
    if a.empty || b.empty {
        return false;
    }
    // Overlap iff the intersection is non-empty. Computing it directly
    // handles the touching-point case ([1,5] && [5,7] share point 5)
    // that a pure gap test cannot express.
    !range_intersect(a, b).empty
}

pub(crate) fn range_adjacent(a: &RangeVal, b: &RangeVal) -> bool {
    if a.empty || b.empty {
        return false;
    }
    if range_overlaps(a, b) {
        return false;
    }
    // Adjacent iff one's upper meets the other's lower with the meet
    // point covered by exactly the inclusive side (bounds_adjacent):
    // values equal and exactly one of (upper_inc, lower_inc) — for
    // canonical discrete ranges [x,y) meets [y,z): upper exclusive,
    // lower inclusive -> adjacent. Continuous [1,5] meets (5,7]:
    // upper inclusive, lower exclusive -> adjacent. [1,5) and (5,7]
    // leave 5 uncovered -> not adjacent; [1,5] and [5,7] overlap.
    let meets = |u: &RangeVal, l: &RangeVal| -> bool {
        match (&u.hi, &l.lo) {
            (Some(x), Some(y)) => bound_eq(x, y) && (u.hi_inc != l.lo_inc),
            _ => false,
        }
    };
    meets(a, b) || meets(b, a)
}

// ---------------------------------------------------------------------------
// Set operations
// ---------------------------------------------------------------------------

/// range_intersect: greatest range contained in both (possibly empty).
pub(crate) fn range_intersect(a: &RangeVal, b: &RangeVal) -> RangeVal {
    let name = a.name.clone();
    let elem = a.elem;
    if a.empty || b.empty {
        return empty_range(elem, name);
    }
    // Lower = the later of the two lowers.
    let (lo, lo_inc) = match lo_cmp(a, b) {
        Ordering::Less | Ordering::Equal => (b.lo.clone(), b.lo_inc),
        Ordering::Greater => (a.lo.clone(), a.lo_inc),
    };
    // Upper = the earlier of the two uppers.
    let (hi, hi_inc) = match hi_cmp(a, b) {
        Ordering::Less | Ordering::Equal => (a.hi.clone(), a.hi_inc),
        Ordering::Greater => (b.hi.clone(), b.hi_inc),
    };
    // The intersection of two canonical ranges is canonical already;
    // make_range re-checks emptiness (equal bounds not both inclusive
    // -> empty; discrete step cases cannot arise from canonical inputs
    // except a point intersection [x,x] from continuous ranges).
    match make_range(elem, name.clone(), lo, lo_inc, hi, hi_inc) {
        Ok(r) => r,
        Err(_) => empty_range(elem, name),
    }
}

/// range_union (the `+` operator on ranges): contiguous-or-error.
pub(crate) fn range_union(a: &RangeVal, b: &RangeVal) -> Result<RangeVal, ExecError> {
    if a.empty {
        return Ok(b.clone());
    }
    if b.empty {
        return Ok(a.clone());
    }
    let (first, second) = if lo_cmp(a, b) == Ordering::Greater {
        (b, a)
    } else {
        (a, b)
    };
    if gap_between(first, second) {
        return Err(exec_err(
            "22000",
            "result of range union would not be contiguous",
        ));
    }
    let (lo, lo_inc) = match lo_cmp(a, b) {
        Ordering::Greater => (b.lo.clone(), b.lo_inc),
        _ => (a.lo.clone(), a.lo_inc),
    };
    let (hi, hi_inc) = match hi_cmp(a, b) {
        Ordering::Less => (b.hi.clone(), b.hi_inc),
        _ => (a.hi.clone(), a.hi_inc),
    };
    make_range(a.elem, a.name.clone(), lo, lo_inc, hi, hi_inc)
}

/// range_minus (the `-` operator on ranges): `a` minus `b` must remain
/// a single range, else 22000 like PG.
pub(crate) fn range_minus(a: &RangeVal, b: &RangeVal) -> Result<RangeVal, ExecError> {
    let parts = range_minus_multi_inner(a, b);
    match parts.len() {
        0 => Ok(empty_range(a.elem, a.name.clone())),
        1 => Ok(parts.into_iter().next().unwrap()),
        _ => Err(exec_err(
            "22000",
            "result of range difference would not be contiguous",
        )),
    }
}

/// The pieces of `a - b` as 0, 1, or 2 ranges (multirange difference
/// building block; PG range_minus_internal logic).
pub(crate) fn range_minus_multi_inner(a: &RangeVal, b: &RangeVal) -> Vec<RangeVal> {
    if a.empty {
        return Vec::new();
    }
    if b.empty || !range_overlaps(a, b) {
        return vec![a.clone()];
    }
    let mut out = Vec::new();
    // Left remnant: a's lower .. b's lower (flipping b's inclusivity).
    if lo_cmp(a, b) == Ordering::Less {
        let rem = make_range(
            a.elem,
            a.name.clone(),
            a.lo.clone(),
            a.lo_inc,
            b.lo.clone(),
            !b.lo_inc,
        );
        if let Ok(r) = rem {
            if !r.empty {
                out.push(r);
            }
        }
    }
    // Right remnant: b's upper .. a's upper.
    if hi_cmp(a, b) == Ordering::Greater {
        let rem = make_range(
            a.elem,
            a.name.clone(),
            b.hi.clone(),
            !b.hi_inc,
            a.hi.clone(),
            a.hi_inc,
        );
        if let Ok(r) = rem {
            if !r.empty {
                out.push(r);
            }
        }
    }
    out
}

/// multirange - multirange / range - multirange etc. over piece lists.
pub(crate) fn multi_minus(a: &MultiVal, b: &MultiVal) -> MultiVal {
    let mut pieces: Vec<RangeVal> = a.ranges.clone();
    for br in &b.ranges {
        let mut next = Vec::new();
        for p in &pieces {
            next.extend(range_minus_multi_inner(p, br));
        }
        pieces = next;
    }
    normalize_multi(a.elem, a.name.clone(), pieces)
}

pub(crate) fn multi_union(a: &MultiVal, b: &MultiVal) -> MultiVal {
    let mut all = a.ranges.clone();
    all.extend(b.ranges.iter().cloned());
    normalize_multi(a.elem, a.name.clone(), all)
}

pub(crate) fn multi_intersect(a: &MultiVal, b: &MultiVal) -> MultiVal {
    let mut out = Vec::new();
    for x in &a.ranges {
        for y in &b.ranges {
            let i = range_intersect(x, y);
            if !i.empty {
                out.push(i);
            }
        }
    }
    normalize_multi(a.elem, a.name.clone(), out)
}

pub(crate) fn multi_contains_range(m: &MultiVal, r: &RangeVal) -> bool {
    if r.empty {
        return true;
    }
    m.ranges.iter().any(|x| range_contains_range(x, r))
}

pub(crate) fn multi_contains_multi(a: &MultiVal, b: &MultiVal) -> bool {
    b.ranges.iter().all(|r| multi_contains_range(a, r))
}

pub(crate) fn multi_contains_elem(m: &MultiVal, v: &Value) -> bool {
    m.ranges.iter().any(|x| range_contains_elem(x, v))
}

pub(crate) fn multi_overlaps(a: &MultiVal, b: &MultiVal) -> bool {
    a.ranges
        .iter()
        .any(|x| b.ranges.iter().any(|y| range_overlaps(x, y)))
}

/// PG range_cmp: empties first, then lower bound, then upper bound.
pub(crate) fn range_cmp(a: &RangeVal, b: &RangeVal) -> Ordering {
    match (a.empty, b.empty) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => lo_cmp(a, b).then_with(|| hi_cmp(a, b)),
    }
}

/// PG multirange_cmp: lexicographic over the normalized range lists.
pub(crate) fn multirange_cmp(a: &MultiVal, b: &MultiVal) -> Ordering {
    for (x, y) in a.ranges.iter().zip(b.ranges.iter()) {
        let o = range_cmp(x, y);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.ranges.len().cmp(&b.ranges.len())
}

/// Is this value a range or multirange (the dispatch test used by the
/// shared JsonOp evaluator)?
pub(crate) fn is_rangeish(v: &Value) -> bool {
    matches!(v, Value::Range(_) | Value::Multirange(_))
}

/// Coerce a scalar element value to the elem's storage kind (an `int`
/// literal against an int8range bound, a bpchar against a textrange).
pub(crate) fn coerce_elem_value(v: &Value, elem: RangeElem) -> Value {
    match (elem, v) {
        (RangeElem::Int8, Value::Int(x)) => Value::BigInt(*x),
        (RangeElem::Int8, Value::SmallInt(x)) => Value::BigInt(*x as i64),
        (RangeElem::Int4, Value::BigInt(x)) => Value::Int(*x),
        (RangeElem::Int4, Value::SmallInt(x)) => Value::Int(*x as i64),
        (RangeElem::Int2, Value::Int(x)) => Value::SmallInt(*x as i16),
        (RangeElem::Num, Value::Int(x)) => {
            Value::Numeric(Numeric::parse(&x.to_string()).unwrap_or_else(|_| Numeric::zero()))
        }
        (RangeElem::Num, Value::BigInt(x)) => {
            Value::Numeric(Numeric::parse(&x.to_string()).unwrap_or_else(|_| Numeric::zero()))
        }
        (RangeElem::Num, Value::SmallInt(x)) => {
            Value::Numeric(Numeric::parse(&x.to_string()).unwrap_or_else(|_| Numeric::zero()))
        }
        (RangeElem::Float8, Value::Float4(x)) => Value::Float(*x as f64),
        (RangeElem::Float8, Value::Int(x)) => Value::Float(*x as f64),
        (RangeElem::Text, Value::BpChar(s)) => Value::Text(s.clone()),
        _ => v.clone(),
    }
}

enum Operand {
    R(RangeVal),
    M(MultiVal),
    E(Value),
}

/// Resolve one operator operand against the peer's elem/name: range and
/// multirange values pass through; text parses as the peer's kind
/// (range for a range peer, multirange for a multirange peer — PG
/// resolves the unknown literal against the operator's types); any
/// other scalar is an element value.
fn operand_of(
    v: &Value,
    peer_elem: Option<RangeElem>,
    peer_name: &Option<String>,
    peer_multi: bool,
) -> Result<Operand, ExecError> {
    match v {
        Value::Range(r) => Ok(Operand::R((**r).clone())),
        Value::Multirange(m) => Ok(Operand::M((**m).clone())),
        Value::Text(t) | Value::BpChar(t) => {
            let elem = peer_elem.unwrap_or(RangeElem::Int4);
            if peer_multi && t.trim_start().starts_with('{') {
                Ok(Operand::M(parse_multirange(elem, peer_name.clone(), t)?))
            } else if peer_multi {
                // Multirange peer with a non-brace literal: a single
                // range or an element (PG @> anyelement resolution).
                match parse_range(elem, peer_name.clone(), t) {
                    Ok(r) => Ok(Operand::R(r)),
                    Err(e) => match parse_bound(elem, t.trim()) {
                        Ok(bv) => Ok(Operand::E(bv)),
                        _ => Err(e),
                    },
                }
            } else {
                match parse_range(elem, peer_name.clone(), t) {
                    Ok(r) => Ok(Operand::R(r)),
                    // v1.88: a text operand that is not a range literal
                    // is the ELEMENT form for @> / <@ (PG operator
                    // resolution prefers anyelement there).
                    Err(e) => match parse_bound(elem, t.trim()) {
                        Ok(bv) => Ok(Operand::E(bv)),
                        _ => Err(e),
                    },
                }
            }
        }
        other => Ok(Operand::E(other.clone())),
    }
}

/// The `@>` / `<@` / `&&` family over range/multirange/element operands
/// (PG rangetypes operators). Mismatched combinations are 42883, like
/// PG's "operator does not exist".
pub(crate) fn eval_range_op(op: JsonOp, left: &Value, right: &Value) -> Result<Value, ExecError> {
    let (le, ln, lm) = match left {
        Value::Range(r) => (Some(r.elem), r.name.clone(), false),
        Value::Multirange(m) => (Some(m.elem), m.name.clone(), true),
        _ => (None, None, false),
    };
    let (re, rn, rm) = match right {
        Value::Range(r) => (Some(r.elem), r.name.clone(), false),
        Value::Multirange(m) => (Some(m.elem), m.name.clone(), true),
        _ => (None, None, false),
    };
    let elem = le.or(re).unwrap_or(RangeElem::Int4);
    let name = ln.clone().or(rn.clone());
    let lo = operand_of(left, re.or(le), &rn.clone().or(ln.clone()), rm)?;
    let ro = operand_of(right, le.or(re), &ln.clone().or(rn.clone()), lm)?;
    let _ = (elem, name);
    let no_op = || {
        exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                left.type_name().to_lowercase(),
                op.sql(),
                right.type_name().to_lowercase()
            ),
        )
    };
    let result = match op {
        JsonOp::Contains => match (&lo, &ro) {
            (Operand::R(a), Operand::R(b)) => range_contains_range(a, b),
            (Operand::R(a), Operand::E(v)) => range_contains_elem(a, &coerce_elem_value(v, a.elem)),
            (Operand::M(a), Operand::R(b)) => multi_contains_range(a, b),
            (Operand::M(a), Operand::M(b)) => multi_contains_multi(a, b),
            (Operand::M(a), Operand::E(v)) => multi_contains_elem(a, &coerce_elem_value(v, a.elem)),
            _ => return Err(no_op()),
        },
        JsonOp::ContainedBy => match (&lo, &ro) {
            (Operand::R(a), Operand::R(b)) => range_contains_range(b, a),
            (Operand::E(v), Operand::R(b)) => range_contains_elem(b, &coerce_elem_value(v, b.elem)),
            (Operand::R(a), Operand::M(b)) => multi_contains_range(b, a),
            (Operand::M(a), Operand::M(b)) => multi_contains_multi(b, a),
            (Operand::E(v), Operand::M(b)) => multi_contains_elem(b, &coerce_elem_value(v, b.elem)),
            _ => return Err(no_op()),
        },
        JsonOp::Overlaps => match (&lo, &ro) {
            (Operand::R(a), Operand::R(b)) => range_overlaps(a, b),
            (Operand::R(a), Operand::M(b)) => multi_overlaps(&multi_from_range(a.clone()), b),
            (Operand::M(a), Operand::R(b)) => multi_overlaps(a, &multi_from_range(b.clone())),
            (Operand::M(a), Operand::M(b)) => multi_overlaps(a, b),
            _ => return Err(no_op()),
        },
        _ => return Err(no_op()),
    };
    Ok(Value::Bool(result))
}

/// `+` (union), `*` (intersection), `-` (difference) over range and
/// multirange operands (PG rangetypes/multirangetypes operators). Text
/// operands parse against the peer's type. Returns `None` when neither
/// operand is rangeish (the caller falls through to numeric handling).
pub(crate) fn eval_range_arith(
    op: ArithOp,
    a: &Value,
    b: &Value,
) -> Option<Result<Value, ExecError>> {
    if !is_rangeish(a) && !is_rangeish(b) {
        return None;
    }
    Some(eval_range_arith_inner(op, a, b))
}

fn as_multi(v: &Value, peer: &Value) -> Result<MultiVal, ExecError> {
    match v {
        Value::Multirange(m) => Ok((**m).clone()),
        Value::Range(r) => Ok(multi_from_range((**r).clone())),
        Value::Text(t) | Value::BpChar(t) => {
            let (elem, name) = match peer {
                Value::Range(r) => (r.elem, r.name.clone()),
                Value::Multirange(m) => (m.elem, m.name.clone()),
                _ => (RangeElem::Int4, None),
            };
            parse_multirange(elem, name, t)
        }
        other => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} + {}",
                other.type_name().to_lowercase(),
                peer.type_name().to_lowercase()
            ),
        )),
    }
}

fn as_range(v: &Value, peer: &Value) -> Result<RangeVal, ExecError> {
    match v {
        Value::Range(r) => Ok((**r).clone()),
        Value::Text(t) | Value::BpChar(t) => {
            let (elem, name) = match peer {
                Value::Range(r) => (r.elem, r.name.clone()),
                Value::Multirange(m) => (m.elem, m.name.clone()),
                _ => (RangeElem::Int4, None),
            };
            parse_range(elem, name, t)
        }
        other => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} + {}",
                other.type_name().to_lowercase(),
                peer.type_name().to_lowercase()
            ),
        )),
    }
}

fn eval_range_arith_inner(op: ArithOp, a: &Value, b: &Value) -> Result<Value, ExecError> {
    let any_multi = matches!(a, Value::Multirange(_)) || matches!(b, Value::Multirange(_));
    match op {
        ArithOp::Add => {
            if any_multi {
                let (x, y) = (as_multi(a, b)?, as_multi(b, a)?);
                Ok(Value::multirange(multi_union(&x, &y)))
            } else {
                let (x, y) = (as_range(a, b)?, as_range(b, a)?);
                range_union(&x, &y).map(Value::range)
            }
        }
        ArithOp::Mul => {
            if any_multi {
                let (x, y) = (as_multi(a, b)?, as_multi(b, a)?);
                Ok(Value::multirange(multi_intersect(&x, &y)))
            } else {
                let (x, y) = (as_range(a, b)?, as_range(b, a)?);
                Ok(Value::range(range_intersect(&x, &y)))
            }
        }
        ArithOp::Sub => {
            // PG: range - range -> range (contiguity error possible);
            // anything involving a multirange -> multirange.
            if any_multi {
                let (x, y) = (as_multi(a, b)?, as_multi(b, a)?);
                Ok(Value::multirange(multi_minus(&x, &y)))
            } else {
                let (x, y) = (as_range(a, b)?, as_range(b, a)?);
                range_minus(&x, &y).map(Value::range)
            }
        }
        _ => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                a.type_name().to_lowercase(),
                op.sql(),
                b.type_name().to_lowercase()
            ),
        )),
    }
}

/// Builtin range constructor name -> elem.
pub(crate) fn builtin_range_elem(name: &str) -> Option<RangeElem> {
    Some(match name {
        "int4range" => RangeElem::Int4,
        "int8range" => RangeElem::Int8,
        "numrange" => RangeElem::Num,
        "daterange" => RangeElem::Date,
        "tsrange" => RangeElem::Ts,
        "tstzrange" => RangeElem::Tstz,
        _ => return None,
    })
}

/// Builtin multirange constructor name -> elem.
pub(crate) fn builtin_multirange_elem(name: &str) -> Option<RangeElem> {
    Some(match name {
        "int4multirange" => RangeElem::Int4,
        "int8multirange" => RangeElem::Int8,
        "nummultirange" => RangeElem::Num,
        "datemultirange" => RangeElem::Date,
        "tsmultirange" => RangeElem::Ts,
        "tstzmultirange" => RangeElem::Tstz,
        _ => return None,
    })
}

/// Coerce one constructor bound argument to the elem kind: NULL stays
/// NULL (an infinite bound), text parses via the subtype input, other
/// values go through the cast machinery.
fn bound_arg(v: &Value, elem: RangeElem) -> Result<Value, ExecError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Text(t) | Value::BpChar(t) => parse_bound(elem, t),
        other => super::cast::eval_cast(other, elem_coltype(elem)),
    }
}

/// A range constructor call (2 or 3 args) over already-evaluated values.
pub(crate) fn range_constructor(
    elem: RangeElem,
    name: Option<String>,
    vals: &[Value],
) -> Result<Value, ExecError> {
    if vals.len() < 2 || vals.len() > 3 {
        return Err(exec_err(
            "42883",
            format!(
                "function {}({}) does not exist",
                range_type_name(elem, &name),
                vals.iter()
                    .map(|v| v.type_name().to_lowercase())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let lo = bound_arg(&vals[0], elem)?;
    let hi = bound_arg(&vals[1], elem)?;
    let flags = match vals.get(2) {
        None => None,
        Some(Value::Null) => None,
        Some(Value::Text(t)) | Some(Value::BpChar(t)) => Some(t.to_string()),
        Some(other) => {
            return Err(exec_err(
                "42883",
                format!(
                    "function {}(text) does not exist",
                    other.type_name().to_lowercase()
                ),
            ));
        }
    };
    construct_range(elem, name, &lo, &hi, flags.as_deref()).map(Value::range)
}

/// A multirange constructor call: variadic range arguments (text args
/// parse as ranges; a single array-of-ranges argument also works, as
/// in PG's variadic-any construction from constructor calls in the
/// regression suites).
pub(crate) fn multirange_constructor(
    elem: RangeElem,
    name: Option<String>,
    vals: &[Value],
) -> Result<Value, ExecError> {
    let mut ranges = Vec::new();
    for v in vals {
        match v {
            Value::Null => {
                return Err(exec_err(
                    "22004",
                    "multirange values cannot contain null members",
                ));
            }
            Value::Range(r) => {
                let mut r = (**r).clone();
                r.name = name.clone();
                ranges.push(r);
            }
            Value::Text(t) | Value::BpChar(t) => {
                ranges.push(parse_range(elem, name.clone(), t)?);
            }
            Value::Array(a) => {
                for e in &a.elems {
                    match e {
                        Value::Range(r) => {
                            let mut r = (**r).clone();
                            r.name = name.clone();
                            ranges.push(r);
                        }
                        Value::Text(t) | Value::BpChar(t) => {
                            ranges.push(parse_range(elem, name.clone(), t)?);
                        }
                        Value::Null => {
                            return Err(exec_err(
                                "22004",
                                "multirange values cannot contain null members",
                            ));
                        }
                        other => {
                            return Err(exec_err(
                                "42804",
                                format!(
                                    "argument of multirange constructor must be a range, not {}",
                                    other.type_name().to_lowercase()
                                ),
                            ));
                        }
                    }
                }
            }
            other => {
                return Err(exec_err(
                    "42804",
                    format!(
                        "argument of multirange constructor must be a range, not {}",
                        other.type_name().to_lowercase()
                    ),
                ));
            }
        }
    }
    Ok(Value::multirange(normalize_multi(elem, name, ranges)))
}

fn first_range(m: &MultiVal) -> Option<&RangeVal> {
    m.ranges.first()
}

fn last_range(m: &MultiVal) -> Option<&RangeVal> {
    m.ranges.last()
}

/// The range/multirange scalar functions over evaluated values:
/// lower/upper/isempty/lower_inc/upper_inc/lower_inf/upper_inf,
/// range_merge (range and multirange forms), and the SQL-callable
/// operator functions (range_union, range_intersect, range_minus,
/// range_minus_multi, multirange_minus_multi, range_adjacent,
/// range_overlaps, range_contains_range, ...). Returns None when the
/// call is not a range function (the caller falls through).
pub(crate) fn try_eval_range_func(name: &str, vals: &[Value]) -> Option<Result<Value, ExecError>> {
    // Constructors by builtin name.
    if let Some(elem) = builtin_range_elem(name) {
        return Some(range_constructor(elem, None, vals));
    }
    if let Some(elem) = builtin_multirange_elem(name) {
        return Some(multirange_constructor(elem, None, vals));
    }
    match name {
        "lower" | "upper" | "isempty" | "lower_inc" | "upper_inc" | "lower_inf" | "upper_inf" => {
            let v = vals.first()?;
            if !is_rangeish(v) {
                return None;
            }
            Some(eval_bound_func(name, v))
        }
        "range_merge" => match vals {
            [v] if matches!(v, Value::Multirange(_)) => {
                let m = match v {
                    Value::Multirange(m) => m,
                    _ => unreachable!(),
                };
                if m.ranges.is_empty() {
                    return Some(Ok(Value::range(empty_range(m.elem, m.name.clone()))));
                }
                let mut acc = m.ranges[0].clone();
                for r in &m.ranges[1..] {
                    acc = range_merge(&acc, r);
                }
                Some(Ok(Value::range(acc)))
            }
            [a, b] => {
                let ra = as_range(a, b).map(|_| ());
                match ra {
                    Err(e) => Some(Err(e)),
                    Ok(()) => Some((|| {
                        let x = as_range(a, b)?;
                        let y = as_range(b, a)?;
                        Ok(Value::range(range_merge(&x, &y)))
                    })()),
                }
            }
            _ => None,
        },
        "range_union"
        | "range_intersect"
        | "range_minus"
        | "range_adjacent"
        | "range_overlaps"
        | "range_contains_range"
        | "range_contained_by_range" => {
            if vals.len() != 2 {
                return None;
            }
            let (a, b) = (&vals[0], &vals[1]);
            if !is_rangeish(a) && !is_rangeish(b) {
                return None;
            }
            Some((|| {
                let x = as_range(a, b)?;
                let y = as_range(b, a)?;
                Ok(match name {
                    "range_union" => Value::range(range_union(&x, &y)?),
                    "range_intersect" => Value::range(range_intersect(&x, &y)),
                    "range_minus" => Value::range(range_minus(&x, &y)?),
                    "range_adjacent" => Value::Bool(range_adjacent(&x, &y)),
                    "range_overlaps" => Value::Bool(range_overlaps(&x, &y)),
                    "range_contains_range" => Value::Bool(range_contains_range(&x, &y)),
                    _ => Value::Bool(range_contains_range(&y, &x)),
                })
            })())
        }
        "range_minus_multi"
        | "multirange_minus_multi"
        | "range_plus_multi"
        | "multirange_plus_multirange"
        | "range_intersect_multi"
        | "multirange_intersect_multirange" => {
            if vals.len() != 2 {
                return None;
            }
            let (a, b) = (&vals[0], &vals[1]);
            if !is_rangeish(a) && !is_rangeish(b) {
                return None;
            }
            Some((|| {
                let x = as_multi(a, b)?;
                let y = as_multi(b, a)?;
                Ok(Value::multirange(match name {
                    "range_minus_multi" | "multirange_minus_multi" => multi_minus(&x, &y),
                    "range_plus_multi" | "multirange_plus_multirange" => multi_union(&x, &y),
                    _ => multi_intersect(&x, &y),
                }))
            })())
        }
        _ => None,
    }
}

fn eval_bound_func(name: &str, v: &Value) -> Result<Value, ExecError> {
    // For a multirange, lower* reads the first range and upper* the
    // last (PG multirange_lower/upper and friends); an empty multirange
    // is NULL for lower/upper, and isempty is true.
    let (lo_r, hi_r): (Option<&RangeVal>, Option<&RangeVal>) = match v {
        Value::Range(r) => (Some(r), Some(r)),
        Value::Multirange(m) => (first_range(m), last_range(m)),
        _ => (None, None),
    };
    Ok(match name {
        "lower" => lo_r
            .and_then(|r| if r.empty { None } else { r.lo.clone() })
            .unwrap_or(Value::Null),
        "upper" => hi_r
            .and_then(|r| if r.empty { None } else { r.hi.clone() })
            .unwrap_or(Value::Null),
        "isempty" => Value::Bool(match v {
            Value::Range(r) => r.empty,
            Value::Multirange(m) => m.ranges.is_empty(),
            _ => false,
        }),
        "lower_inc" => Value::Bool(lo_r.map(|r| !r.empty && r.lo_inc).unwrap_or(false)),
        "upper_inc" => Value::Bool(hi_r.map(|r| !r.empty && r.hi_inc).unwrap_or(false)),
        "lower_inf" => Value::Bool(lo_r.map(|r| !r.empty && r.lo.is_none()).unwrap_or(false)),
        "upper_inf" => Value::Bool(hi_r.map(|r| !r.empty && r.hi.is_none()).unwrap_or(false)),
        _ => Value::Null,
    })
}

/// range_merge: smallest range containing both.
pub(crate) fn range_merge(a: &RangeVal, b: &RangeVal) -> RangeVal {
    if a.empty {
        return b.clone();
    }
    if b.empty {
        return a.clone();
    }
    let (lo, lo_inc) = match lo_cmp(a, b) {
        Ordering::Greater => (b.lo.clone(), b.lo_inc),
        _ => (a.lo.clone(), a.lo_inc),
    };
    let (hi, hi_inc) = match hi_cmp(a, b) {
        Ordering::Less => (b.hi.clone(), b.hi_inc),
        _ => (a.hi.clone(), a.hi_inc),
    };
    make_range(a.elem, a.name.clone(), lo, lo_inc, hi, hi_inc)
        .unwrap_or_else(|_| empty_range(a.elem, a.name.clone()))
}
