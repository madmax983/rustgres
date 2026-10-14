//! v1.87: the `pg_input_is_valid` / `pg_input_error_info` validators for
//! every type family beyond the scalar core in `cast.rs`.
//!
//! `cast.rs::pg_input_validate` handles the long-standing scalar arms
//! (integers, floats, numeric, bool, bytea, char/varchar, datetime). This
//! module carries the rest, all grounded in the local PG19 sources:
//!   * the statistics types `pg_ndistinct` / `pg_dependencies`
//!     (src/backend/utils/adt/pg_ndistinct.c, pg_dependencies.c) — a
//!     faithful port of their JSON semantic-action state machines over a
//!     small event parser mirroring src/common/jsonapi.c's callback
//!     order (object/array/field/element starts fire before the value);
//!   * `json` / `jsonb` validity with the jsonapi error-detail taxonomy;
//!   * the unsigned Oid family (oid/oid8/xid/xid8/oidvector) per
//!     numutils.c's `pg_strtouint*` (base-0: hex/octal, minus wraps);
//!   * bit/varbit, the geometry types, inet/cidr, macaddr/macaddr8,
//!     money, uuid, tid, pg_lsn, pg_snapshot, xml, tsvector/tsquery,
//!     arrays, ranges/multiranges;
//!   * the catalog-aware layer (`pg_input_validate_catalog`): domains,
//!     composites (incl. table row types), aclitem and the reg* family,
//!     which need the live catalog/roles and so cannot live in the pure
//!     validator.

use super::cast::{PgInputError, PgInputFailure};
use super::*;

// ---------------------------------------------------------------------------
// Small constructors
// ---------------------------------------------------------------------------

pub(crate) fn pgi_soft(code: &'static str, message: String) -> PgInputFailure {
    PgInputFailure::Soft(PgInputError::soft(code, message))
}

pub(crate) fn pgi_soft_detail(
    code: &'static str,
    message: String,
    detail: String,
) -> PgInputFailure {
    PgInputFailure::Soft(PgInputError {
        message,
        detail: Some(detail),
        hint: None,
        code,
    })
}

/// PG's ubiquitous `invalid input syntax for type %s: "%s"` (22P02).
pub(crate) fn pgi_syntax(typname: &str, input: &str) -> PgInputFailure {
    pgi_soft(
        "22P02",
        format!("invalid input syntax for type {typname}: \"{input}\""),
    )
}

// ---------------------------------------------------------------------------
// PG scalar text parsers (numutils.c / float.c semantics)
// ---------------------------------------------------------------------------

/// `pg_strtoint16_safe` / `pg_strtoint32_safe`: whitespace-trimmed,
/// optional sign, digits only, whole string, range-checked.
pub(crate) fn pg_strtoint(token: &str, bits: u32) -> Option<i64> {
    let b = token.as_bytes();
    let mut i = 0usize;
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    let neg = if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        let n = b[i] == b'-';
        i += 1;
        n
    } else {
        false
    };
    let start = i;
    let mut val: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        val = val.checked_mul(10)?.checked_add((b[i] - b'0') as i64)?;
        if val > (1i64 << 32) {
            return None;
        }
        i += 1;
    }
    if i == start {
        return None;
    }
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    if i != b.len() {
        return None;
    }
    let v = if neg { -val } else { val };
    let (lo, hi) = match bits {
        16 => (i16::MIN as i64, i16::MAX as i64),
        32 => (i32::MIN as i64, i32::MAX as i64),
        _ => (i64::MIN, i64::MAX),
    };
    if v < lo || v > hi {
        return None;
    }
    Some(v)
}

/// Whole-string `float8in_internal`: returns the value or `None` on any
/// syntax/range failure. Underscores are allowed between digits (PG16+);
/// overflow to infinity and underflow to zero are errors, like strtod's
/// ERANGE handling in float.c.
pub(crate) fn pg_float8(token: &str) -> Option<f64> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    let (sign, rest) = match t.strip_prefix('-') {
        Some(r) => (-1.0f64, r),
        None => (1.0f64, t.strip_prefix('+').unwrap_or(t)),
    };
    let low = rest.to_ascii_lowercase();
    if low == "infinity" || low == "inf" {
        return Some(sign * f64::INFINITY);
    }
    if low == "nan" {
        return Some(f64::NAN);
    }
    // Underscore placement: each '_' must sit between two digits.
    let rb = rest.as_bytes();
    let mut cleaned = String::with_capacity(rest.len());
    for (idx, &c) in rb.iter().enumerate() {
        if c == b'_' {
            let prev_ok = idx > 0 && rb[idx - 1].is_ascii_digit();
            let next_ok = idx + 1 < rb.len() && rb[idx + 1].is_ascii_digit();
            if !prev_ok || !next_ok {
                return None;
            }
        } else {
            cleaned.push(c as char);
        }
    }
    let v: f64 = cleaned.parse().ok()?;
    if v.is_infinite() {
        return None; // overflow (literal was finite)
    }
    if v == 0.0 {
        // Underflow: a nonzero mantissa that rounded to zero is ERANGE.
        let mantissa_nonzero = cleaned
            .chars()
            .take_while(|c| *c != 'e' && *c != 'E')
            .any(|c| c.is_ascii_digit() && c != '0');
        if mantissa_nonzero {
            return None;
        }
    }
    Some(sign * v)
}

// ---------------------------------------------------------------------------
// JSON event parser (mirrors src/common/jsonapi.c callback order)
// ---------------------------------------------------------------------------

/// Semantic actions over a JSON document, mirroring PG's
/// `JsonSemAction`. Any callback may fail with a soft `PgInputFailure`;
/// the parse aborts immediately, exactly like PG's ErrorSaveContext flow
/// (first semantic error wins over any later lexical error).
pub(crate) trait JsonSem {
    fn object_start(&mut self) -> Result<(), PgInputFailure> {
        Ok(())
    }
    fn object_end(&mut self) -> Result<(), PgInputFailure> {
        Ok(())
    }
    fn array_start(&mut self) -> Result<(), PgInputFailure> {
        Ok(())
    }
    fn array_end(&mut self) -> Result<(), PgInputFailure> {
        Ok(())
    }
    fn field_start(&mut self, _name: &str, _isnull: bool) -> Result<(), PgInputFailure> {
        Ok(())
    }
    fn elem_start(&mut self, _isnull: bool) -> Result<(), PgInputFailure> {
        Ok(())
    }
    fn scalar(&mut self, _token: &str) -> Result<(), PgInputFailure> {
        Ok(())
    }
}

pub(crate) enum JsonRun {
    /// Document parsed (possibly empty: no events fired at all).
    Ok,
    /// A lexical/structural JSON failure with no semantic error set.
    LexError,
    /// A semantic action failed; carries the exact PG error.
    Sem(PgInputFailure),
}

struct JsonParser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

const JSON_MAX_DEPTH: usize = 400;

impl<'a> JsonParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    /// PG's lexer decides null-ness of an upcoming value with a lookahead;
    /// mirror it closely enough for the semantic actions: whitespace then
    /// the literal `null` followed by a value delimiter.
    fn peek_is_null(&self) -> bool {
        let mut j = self.i;
        while j < self.b.len() && matches!(self.b[j], b' ' | b'\t' | b'\n' | b'\r') {
            j += 1;
        }
        if self.b[j..].starts_with(b"null") {
            match self.b.get(j + 4) {
                None => true,
                Some(&c) => matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'),
            }
        } else {
            false
        }
    }

    fn lex_string(&mut self) -> Result<String, JsonRun> {
        // self.b[self.i] == '"'
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or(JsonRun::LexError)?;
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let e = self.peek().ok_or(JsonRun::LexError)?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let h1 = self.lex_hex4()?;
                            if (0xD800..0xDC00).contains(&h1) {
                                // High surrogate: must be followed by \uDC00-DFFF.
                                if self.peek() == Some(b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let h2 = self.lex_hex4()?;
                                    if !(0xDC00..0xE000).contains(&h2) {
                                        return Err(JsonRun::LexError);
                                    }
                                    let cp = 0x10000
                                        + (((h1 - 0xD800) as u32) << 10)
                                        + (h2 - 0xDC00) as u32;
                                    out.push(char::from_u32(cp).ok_or(JsonRun::LexError)?);
                                } else {
                                    return Err(JsonRun::LexError);
                                }
                            } else if (0xDC00..0xE000).contains(&h1) {
                                return Err(JsonRun::LexError);
                            } else {
                                out.push(char::from_u32(h1 as u32).ok_or(JsonRun::LexError)?);
                            }
                        }
                        _ => return Err(JsonRun::LexError),
                    }
                }
                0x00..=0x1F => return Err(JsonRun::LexError),
                _ => {
                    // Copy one UTF-8 char verbatim (input is a &str).
                    let s = &self.b[self.i..];
                    let w = utf8_len(s[0]);
                    if self.i + w > self.b.len() {
                        return Err(JsonRun::LexError);
                    }
                    match std::str::from_utf8(&s[..w]) {
                        Ok(cs) => out.push_str(cs),
                        Err(_) => return Err(JsonRun::LexError),
                    }
                    self.i += w;
                }
            }
        }
    }

    fn lex_hex4(&mut self) -> Result<u16, JsonRun> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let c = self.peek().ok_or(JsonRun::LexError)?;
            let d = (c as char).to_digit(16).ok_or(JsonRun::LexError)?;
            v = v * 16 + d as u16;
            self.i += 1;
        }
        Ok(v)
    }

    fn lex_number(&mut self) -> Result<String, JsonRun> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(c) if c.is_ascii_digit() => {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            _ => return Err(JsonRun::LexError),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(JsonRun::LexError);
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(JsonRun::LexError);
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        Ok(String::from_utf8_lossy(&self.b[start..self.i]).into_owned())
    }

    fn value(&mut self, sem: &mut dyn JsonSem) -> Result<(), JsonRun> {
        self.skip_ws();
        match self.peek().ok_or(JsonRun::LexError)? {
            b'{' => {
                sem.object_start().map_err(JsonRun::Sem)?;
                self.i += 1;
                self.depth += 1;
                if self.depth > JSON_MAX_DEPTH {
                    return Err(JsonRun::LexError);
                }
                self.skip_ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    self.depth -= 1;
                    sem.object_end().map_err(JsonRun::Sem)?;
                    return Ok(());
                }
                loop {
                    self.skip_ws();
                    if self.peek() != Some(b'"') {
                        return Err(JsonRun::LexError);
                    }
                    let name = self.lex_string()?;
                    self.skip_ws();
                    if self.peek() != Some(b':') {
                        return Err(JsonRun::LexError);
                    }
                    self.i += 1;
                    let isnull = self.peek_is_null();
                    sem.field_start(&name, isnull).map_err(JsonRun::Sem)?;
                    self.value(sem)?;
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            self.depth -= 1;
                            sem.object_end().map_err(JsonRun::Sem)?;
                            return Ok(());
                        }
                        _ => return Err(JsonRun::LexError),
                    }
                }
            }
            b'[' => {
                sem.array_start().map_err(JsonRun::Sem)?;
                self.i += 1;
                self.depth += 1;
                if self.depth > JSON_MAX_DEPTH {
                    return Err(JsonRun::LexError);
                }
                self.skip_ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    self.depth -= 1;
                    sem.array_end().map_err(JsonRun::Sem)?;
                    return Ok(());
                }
                loop {
                    let isnull = self.peek_is_null();
                    sem.elem_start(isnull).map_err(JsonRun::Sem)?;
                    self.value(sem)?;
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            self.depth -= 1;
                            sem.array_end().map_err(JsonRun::Sem)?;
                            return Ok(());
                        }
                        _ => return Err(JsonRun::LexError),
                    }
                }
            }
            b'"' => {
                let s = self.lex_string()?;
                sem.scalar(&s).map_err(JsonRun::Sem)
            }
            b't' => {
                if self.b[self.i..].starts_with(b"true") {
                    self.i += 4;
                    sem.scalar("true").map_err(JsonRun::Sem)
                } else {
                    Err(JsonRun::LexError)
                }
            }
            b'f' => {
                if self.b[self.i..].starts_with(b"false") {
                    self.i += 5;
                    sem.scalar("false").map_err(JsonRun::Sem)
                } else {
                    Err(JsonRun::LexError)
                }
            }
            b'n' => {
                if self.b[self.i..].starts_with(b"null") {
                    self.i += 4;
                    sem.scalar("null").map_err(JsonRun::Sem)
                } else {
                    Err(JsonRun::LexError)
                }
            }
            b'-' | b'0'..=b'9' => {
                let n = self.lex_number()?;
                sem.scalar(&n).map_err(JsonRun::Sem)
            }
            _ => Err(JsonRun::LexError),
        }
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Run PG's parse: an empty/whitespace-only document fires no events and
/// succeeds (that is the only way the statistics types' build phase can
/// observe "no input" and report `Value cannot be empty.`).
pub(crate) fn run_json_events(input: &str, sem: &mut dyn JsonSem) -> JsonRun {
    let mut p = JsonParser {
        b: input.as_bytes(),
        i: 0,
        depth: 0,
    };
    p.skip_ws();
    if p.peek().is_none() {
        return JsonRun::Ok;
    }
    match p.value(sem) {
        Ok(()) => {
            p.skip_ws();
            if p.peek().is_some() {
                JsonRun::LexError
            } else {
                JsonRun::Ok
            }
        }
        Err(r) => r,
    }
}

// ---------------------------------------------------------------------------
// pg_ndistinct / pg_dependencies (statistics types)
// Faithful ports of pg_ndistinct.c / pg_dependencies.c semantic actions.
// ---------------------------------------------------------------------------

const STATS_MAX_DIMENSIONS: i64 = 8;

fn stats_fail(typ: &str, input: &str, detail: String) -> PgInputFailure {
    pgi_soft_detail("22P02", format!("malformed {typ}: \"{input}\""), detail)
}

fn attnum_list_str(attnums: &[i16]) -> String {
    attnums
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// PG's `valid_subsequent_attnum`: canonical order is positive numbers
/// ascending, then negative numbers descending.
fn valid_subsequent_attnum(prev: i16, cur: i16) -> bool {
    if prev > 0 {
        cur > prev || cur < 0
    } else {
        cur < prev
    }
}

fn check_attnum_scalar(
    typ: &str,
    input: &str,
    token: &str,
    attnums: &mut Vec<i16>,
) -> Result<i16, PgInputFailure> {
    let attnum = pg_strtoint(token, 16).ok_or_else(|| {
        stats_fail(
            typ,
            input,
            "Key \"attributes\" has an incorrect value.".to_string(),
        )
    })? as i16;
    if attnum == 0 || (attnum as i64) < -STATS_MAX_DIMENSIONS {
        return Err(stats_fail(
            typ,
            input,
            format!("Invalid \"attributes\" element has been found: {attnum}."),
        ));
    }
    if let Some(&prev) = attnums.last() {
        if !valid_subsequent_attnum(prev, attnum) {
            return Err(stats_fail(
                typ,
                input,
                format!(
                    "Invalid \"attributes\" element has been found: {attnum} cannot follow {prev}."
                ),
            ));
        }
    }
    attnums.push(attnum);
    Ok(attnum)
}

// ndistinct states (NDistinctSemanticState)
const ND_START: u8 = 0;
const ND_ITEM: u8 = 1;
const ND_KEY: u8 = 2;
const ND_ATTNUM_LIST: u8 = 3;
const ND_ATTNUM: u8 = 4;
const ND_NDISTINCT: u8 = 5;
const ND_COMPLETE: u8 = 6;

struct NdistinctSem<'a> {
    input: &'a str,
    state: u8,
    found_attributes: bool,
    found_ndistinct: bool,
    attnums: Vec<i16>,
    items: Vec<Vec<i16>>,
}

impl<'a> NdistinctSem<'a> {
    fn fail(&self, detail: String) -> PgInputFailure {
        stats_fail("pg_ndistinct", self.input, detail)
    }
}

impl JsonSem for NdistinctSem<'_> {
    fn object_start(&mut self) -> Result<(), PgInputFailure> {
        match self.state {
            ND_ITEM => {
                self.state = ND_KEY;
                Ok(())
            }
            ND_START => Err(self.fail("Initial element must be an array.".into())),
            ND_KEY => Err(self.fail("A key was expected.".into())),
            ND_ATTNUM_LIST => {
                Err(self
                    .fail("Value of \"attributes\" must be an array of attribute numbers.".into()))
            }
            ND_ATTNUM => {
                Err(self.fail("Attribute lists can only contain attribute numbers.".into()))
            }
            ND_NDISTINCT => Err(self.fail("Value of \"ndistinct\" must be an integer.".into())),
            _ => Err(self.fail("Input data must be valid JSON.".into())),
        }
    }

    fn object_end(&mut self) -> Result<(), PgInputFailure> {
        if self.state != ND_KEY {
            return Err(self.fail("Input data must be valid JSON.".into()));
        }
        if !self.found_attributes {
            return Err(self.fail("Item must contain \"attributes\" key.".into()));
        }
        if !self.found_ndistinct {
            return Err(self.fail("Item must contain \"ndistinct\" key.".into()));
        }
        let n = self.attnums.len();
        if n < 2 || n as i64 > STATS_MAX_DIMENSIONS {
            return Err(self.fail(format!(
                "The \"attributes\" key must contain an array of at least 2 and no more than {STATS_MAX_DIMENSIONS} attributes."
            )));
        }
        self.items.push(std::mem::take(&mut self.attnums));
        self.found_attributes = false;
        self.found_ndistinct = false;
        self.state = ND_ITEM;
        Ok(())
    }

    fn array_start(&mut self) -> Result<(), PgInputFailure> {
        match self.state {
            ND_ATTNUM_LIST => {
                self.state = ND_ATTNUM;
                Ok(())
            }
            ND_START => {
                self.state = ND_ITEM;
                Ok(())
            }
            _ => Err(self.fail("Array has been found at an unexpected location.".into())),
        }
    }

    fn array_end(&mut self) -> Result<(), PgInputFailure> {
        match self.state {
            ND_ATTNUM => {
                if !self.attnums.is_empty() {
                    self.state = ND_KEY;
                    Ok(())
                } else {
                    Err(self.fail("The \"attributes\" key must be a non-empty array.".into()))
                }
            }
            ND_ITEM => {
                if !self.items.is_empty() {
                    self.state = ND_COMPLETE;
                    Ok(())
                } else {
                    Err(self.fail("Item array cannot be empty.".into()))
                }
            }
            _ => Err(self.fail("Input data must be valid JSON.".into())),
        }
    }

    fn field_start(&mut self, name: &str, _isnull: bool) -> Result<(), PgInputFailure> {
        match name {
            "attributes" => {
                if self.found_attributes {
                    return Err(self.fail("Multiple \"attributes\" keys are not allowed.".into()));
                }
                self.found_attributes = true;
                self.state = ND_ATTNUM_LIST;
                Ok(())
            }
            "ndistinct" => {
                if self.found_ndistinct {
                    return Err(self.fail("Multiple \"ndistinct\" keys are not allowed.".into()));
                }
                self.found_ndistinct = true;
                self.state = ND_NDISTINCT;
                Ok(())
            }
            _ => Err(self.fail("Only allowed keys are \"attributes\" and \"ndistinct\".".into())),
        }
    }

    fn elem_start(&mut self, isnull: bool) -> Result<(), PgInputFailure> {
        match self.state {
            ND_ATTNUM => {
                if isnull {
                    Err(self.fail("Attribute number array cannot be null.".into()))
                } else {
                    Ok(())
                }
            }
            ND_ITEM => {
                if isnull {
                    Err(self.fail("Item list elements cannot be null.".into()))
                } else {
                    Ok(())
                }
            }
            _ => Err(self.fail("Input data must be valid JSON.".into())),
        }
    }

    fn scalar(&mut self, token: &str) -> Result<(), PgInputFailure> {
        match self.state {
            ND_ATTNUM => {
                check_attnum_scalar("pg_ndistinct", self.input, token, &mut self.attnums)?;
                Ok(())
            }
            ND_NDISTINCT => {
                if pg_strtoint(token, 32).is_none() {
                    return Err(self.fail("Key \"ndistinct\" has an incorrect value.".into()));
                }
                self.state = ND_KEY;
                Ok(())
            }
            _ => Err(self.fail("Unexpected scalar has been found.".into())),
        }
    }
}

pub(crate) fn pg_input_validate_ndistinct(input: &str) -> Result<(), PgInputFailure> {
    let mut sem = NdistinctSem {
        input,
        state: ND_START,
        found_attributes: false,
        found_ndistinct: false,
        attnums: Vec::new(),
        items: Vec::new(),
    };
    match run_json_events(input, &mut sem) {
        JsonRun::Sem(f) => return Err(f),
        JsonRun::LexError => {
            return Err(stats_fail(
                "pg_ndistinct",
                input,
                "Input data must be valid JSON.".into(),
            ));
        }
        JsonRun::Ok => {}
    }
    // Build phase (build_mvndistinct).
    if sem.state == ND_START {
        return Err(sem.fail("Value cannot be empty.".into()));
    }
    if sem.state != ND_COMPLETE {
        return Err(sem.fail(format!(
            "Unexpected end state has been found: {}.",
            sem.state
        )));
    }
    let items = &sem.items;
    for i in 0..items.len() {
        for j in 0..i {
            if items[i] == items[j] {
                return Err(sem.fail(format!(
                    "Duplicated \"attributes\" array has been found: [{}].",
                    attnum_list_str(&items[i])
                )));
            }
        }
    }
    // All attribute lists must be subsets of the first longest list.
    let mut most_idx = 0usize;
    let mut most = 0usize;
    for (i, it) in items.iter().enumerate() {
        if it.len() > most {
            most = it.len();
            most_idx = i;
        }
    }
    let reference = &items[most_idx];
    for (i, it) in items.iter().enumerate() {
        if i == most_idx {
            continue;
        }
        if !it.iter().all(|a| reference.contains(a)) {
            return Err(sem.fail(format!(
                "\"attributes\" array [{}] must be a subset of array [{}].",
                attnum_list_str(it),
                attnum_list_str(reference)
            )));
        }
    }
    Ok(())
}

// dependencies states (DependenciesSemanticState; DEPS_PARSE_COMPLETE = 7)
const DP_START: u8 = 0;
const DP_ITEM: u8 = 1;
const DP_KEY: u8 = 2;
const DP_ATTNUM_LIST: u8 = 3;
const DP_ATTNUM: u8 = 4;
const DP_DEPENDENCY: u8 = 5;
const DP_DEGREE: u8 = 6;
const DP_COMPLETE: u8 = 7;

struct DepsSem<'a> {
    input: &'a str,
    state: u8,
    found_attributes: bool,
    found_dependency: bool,
    found_degree: bool,
    attnums: Vec<i16>,
    dependency: i16,
    items: Vec<(Vec<i16>, i16)>, // (attributes, dependency as last element)
}

impl<'a> DepsSem<'a> {
    fn fail(&self, detail: String) -> PgInputFailure {
        stats_fail("pg_dependencies", self.input, detail)
    }
}

impl JsonSem for DepsSem<'_> {
    fn object_start(&mut self) -> Result<(), PgInputFailure> {
        match self.state {
            DP_ITEM => {
                self.state = DP_KEY;
                Ok(())
            }
            DP_START => Err(self.fail("Initial element must be an array.".into())),
            DP_KEY => Err(self.fail("A key was expected.".into())),
            DP_ATTNUM_LIST => {
                Err(self
                    .fail("Value of \"attributes\" must be an array of attribute numbers.".into()))
            }
            DP_ATTNUM => {
                Err(self.fail("Attribute lists can only contain attribute numbers.".into()))
            }
            DP_DEPENDENCY => Err(self.fail("Value of \"dependency\" must be an integer.".into())),
            DP_DEGREE => Err(self.fail("Value of \"degree\" must be an integer.".into())),
            _ => Err(self.fail("Input data must be valid JSON.".into())),
        }
    }

    fn object_end(&mut self) -> Result<(), PgInputFailure> {
        if self.state != DP_KEY {
            return Err(self.fail("Input data must be valid JSON.".into()));
        }
        if !self.found_attributes {
            return Err(self.fail("Item must contain \"attributes\" key.".into()));
        }
        if !self.found_dependency {
            return Err(self.fail("Item must contain \"dependency\" key.".into()));
        }
        if !self.found_degree {
            return Err(self.fail("Item must contain \"degree\" key.".into()));
        }
        let n = self.attnums.len();
        if n < 1 || n as i64 > STATS_MAX_DIMENSIONS - 1 {
            return Err(self.fail(format!(
                "The \"attributes\" key must contain an array of at least 1 and no more than {} elements.",
                STATS_MAX_DIMENSIONS - 1
            )));
        }
        for &a in &self.attnums {
            if a == self.dependency {
                return Err(self.fail(format!(
                    "Item \"dependency\" with value {} has been found in the \"attributes\" list.",
                    self.dependency
                )));
            }
        }
        let mut full = std::mem::take(&mut self.attnums);
        full.push(self.dependency);
        self.items.push((full, self.dependency));
        self.dependency = 0;
        self.found_attributes = false;
        self.found_dependency = false;
        self.found_degree = false;
        self.state = DP_ITEM;
        Ok(())
    }

    fn array_start(&mut self) -> Result<(), PgInputFailure> {
        match self.state {
            DP_ATTNUM_LIST => {
                self.state = DP_ATTNUM;
                Ok(())
            }
            DP_START => {
                self.state = DP_ITEM;
                Ok(())
            }
            _ => Err(self.fail("Array has been found at an unexpected location.".into())),
        }
    }

    fn array_end(&mut self) -> Result<(), PgInputFailure> {
        match self.state {
            DP_ATTNUM => {
                if !self.attnums.is_empty() {
                    self.state = DP_KEY;
                    Ok(())
                } else {
                    Err(self.fail("The \"attributes\" key must be a non-empty array.".into()))
                }
            }
            DP_ITEM => {
                if !self.items.is_empty() {
                    self.state = DP_COMPLETE;
                    Ok(())
                } else {
                    Err(self.fail("Item array cannot be empty.".into()))
                }
            }
            _ => Err(self.fail("Input data must be valid JSON.".into())),
        }
    }

    fn field_start(&mut self, name: &str, _isnull: bool) -> Result<(), PgInputFailure> {
        match name {
            "attributes" => {
                if self.found_attributes {
                    return Err(self.fail("Multiple \"attributes\" keys are not allowed.".into()));
                }
                self.found_attributes = true;
                self.state = DP_ATTNUM_LIST;
                Ok(())
            }
            "dependency" => {
                if self.found_dependency {
                    return Err(self.fail("Multiple \"dependency\" keys are not allowed.".into()));
                }
                self.found_dependency = true;
                self.state = DP_DEPENDENCY;
                Ok(())
            }
            "degree" => {
                if self.found_degree {
                    return Err(self.fail("Multiple \"degree\" keys are not allowed.".into()));
                }
                self.found_degree = true;
                self.state = DP_DEGREE;
                Ok(())
            }
            _ => Err(self.fail(
                "Only allowed keys are \"attributes\", \"dependency\", and \"degree\".".into(),
            )),
        }
    }

    fn elem_start(&mut self, isnull: bool) -> Result<(), PgInputFailure> {
        match self.state {
            DP_ATTNUM => {
                if isnull {
                    Err(self.fail("Attribute number array cannot be null.".into()))
                } else {
                    Ok(())
                }
            }
            DP_ITEM => {
                if isnull {
                    Err(self.fail("Item list elements cannot be null.".into()))
                } else {
                    Ok(())
                }
            }
            _ => Err(self.fail("Input data must be valid JSON.".into())),
        }
    }

    fn scalar(&mut self, token: &str) -> Result<(), PgInputFailure> {
        match self.state {
            DP_ATTNUM => {
                check_attnum_scalar("pg_dependencies", self.input, token, &mut self.attnums)?;
                Ok(())
            }
            DP_DEPENDENCY => {
                let v = pg_strtoint(token, 16)
                    .ok_or_else(|| self.fail("Key \"dependency\" has an incorrect value.".into()))?
                    as i16;
                if v == 0 || (v as i64) < -STATS_MAX_DIMENSIONS {
                    return Err(
                        self.fail(format!("Key \"dependency\" has an incorrect value: {v}."))
                    );
                }
                self.dependency = v;
                self.state = DP_KEY;
                Ok(())
            }
            DP_DEGREE => {
                if pg_float8(token).is_none() {
                    return Err(self.fail("Key \"degree\" has an incorrect value.".into()));
                }
                self.state = DP_KEY;
                Ok(())
            }
            _ => Err(self.fail("Unexpected scalar has been found.".into())),
        }
    }
}

pub(crate) fn pg_input_validate_dependencies(input: &str) -> Result<(), PgInputFailure> {
    let mut sem = DepsSem {
        input,
        state: DP_START,
        found_attributes: false,
        found_dependency: false,
        found_degree: false,
        attnums: Vec::new(),
        dependency: 0,
        items: Vec::new(),
    };
    match run_json_events(input, &mut sem) {
        JsonRun::Sem(f) => return Err(f),
        JsonRun::LexError => {
            return Err(stats_fail(
                "pg_dependencies",
                input,
                "Input data must be valid JSON.".into(),
            ));
        }
        JsonRun::Ok => {}
    }
    if sem.state == DP_START {
        return Err(sem.fail("Value cannot be empty.".into()));
    }
    if sem.state != DP_COMPLETE {
        return Err(sem.fail(format!(
            "Unexpected end state has been found: {}.",
            sem.state
        )));
    }
    let items = &sem.items;
    for i in 0..items.len() {
        for j in 0..i {
            if items[i].0 == items[j].0 {
                let (full, dep) = &items[i];
                return Err(sem.fail(format!(
                    "Duplicated \"attributes\" array has been found: [{}] for key \"dependency\" and value {dep}.",
                    attnum_list_str(&full[..full.len() - 1])
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// json / jsonb (jsonapi.c error taxonomy; jsonb adds numeric conversion)
// ---------------------------------------------------------------------------

/// Recursive-descent JSON checker producing PG's `json_errdetail` text.
/// Errors are returned as the detail string; the caller wraps them in
/// `invalid input syntax for type json` / 22P02. For jsonb, a number that
/// overflows PG's numeric format short-circuits with its own failure.
struct JsonCheck<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
    jsonb: bool,
}

const JSON_EOF_DETAIL: &str = "The input string ended unexpectedly.";

impl<'a> JsonCheck<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    /// The token text PG would report at the current position: a string
    /// token reports its raw content; other alnum runs report the run;
    /// punctuation reports the single character.
    fn token_text(&self) -> String {
        match self.peek() {
            None => String::new(),
            Some(b'"') => {
                let mut j = self.i + 1;
                let mut out = String::new();
                while j < self.b.len() {
                    match self.b[j] {
                        b'"' => break,
                        b'\\' if j + 1 < self.b.len() => {
                            out.push(self.b[j] as char);
                            out.push(self.b[j + 1] as char);
                            j += 2;
                        }
                        c => {
                            out.push(c as char);
                            j += 1;
                        }
                    }
                }
                out
            }
            Some(c) if c.is_ascii_alphanumeric() || c == b'-' || c == b'+' || c == b'.' => {
                let mut j = self.i;
                while j < self.b.len()
                    && (self.b[j].is_ascii_alphanumeric()
                        || matches!(self.b[j], b'-' | b'+' | b'.' | b'_'))
                {
                    j += 1;
                }
                String::from_utf8_lossy(&self.b[self.i..j]).into_owned()
            }
            Some(c) => (c as char).to_string(),
        }
    }

    fn err_expected(&self, what: &str) -> PgInputFailure {
        if self.peek().is_none() {
            return pgi_soft_detail(
                "22P02",
                "invalid input syntax for type json".into(),
                JSON_EOF_DETAIL.into(),
            );
        }
        pgi_soft_detail(
            "22P02",
            "invalid input syntax for type json".into(),
            format!("{what}, but found \"{}\".", self.token_text()),
        )
    }

    fn err_detail(&self, detail: String) -> PgInputFailure {
        pgi_soft_detail("22P02", "invalid input syntax for type json".into(), detail)
    }

    fn lex_string_raw(&mut self) -> Result<(), PgInputFailure> {
        // At the opening quote. Validates escapes/surrogates/control chars
        // with jsonapi's specific details.
        self.i += 1;
        loop {
            let c = match self.peek() {
                None => {
                    return Err(self.err_detail(JSON_EOF_DETAIL.into()));
                }
                Some(c) => c,
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.i += 1;
                    let e = match self.peek() {
                        None => return Err(self.err_detail(JSON_EOF_DETAIL.into())),
                        Some(e) => e,
                    };
                    self.i += 1;
                    match e {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            let h = self.lex_hex4_json()?;
                            if h == 0 {
                                return Err(
                                    self.err_detail("\\u0000 cannot be converted to text.".into())
                                );
                            }
                            if (0xD800..0xDC00).contains(&h) {
                                if self.peek() == Some(b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let h2 = self.lex_hex4_json()?;
                                    if (0xD800..0xDC00).contains(&h2) {
                                        return Err(self.err_detail(
                                            "Unicode high surrogate must not follow a high surrogate."
                                                .into(),
                                        ));
                                    }
                                    if !(0xDC00..0xE000).contains(&h2) {
                                        return Err(self.err_detail(
                                            "Unicode low surrogate must follow a high surrogate."
                                                .into(),
                                        ));
                                    }
                                } else {
                                    return Err(self.err_detail(
                                        "Unicode low surrogate must follow a high surrogate."
                                            .into(),
                                    ));
                                }
                            } else if (0xDC00..0xE000).contains(&h) {
                                return Err(self.err_detail(
                                    "Unicode low surrogate must follow a high surrogate.".into(),
                                ));
                            }
                        }
                        _ => {
                            return Err(self.err_detail(format!(
                                "Escape sequence \"\\{}\" is invalid.",
                                e as char
                            )));
                        }
                    }
                }
                0x00..=0x1F => {
                    return Err(
                        self.err_detail(format!("Character with value 0x{c:02x} must be escaped."))
                    );
                }
                _ => {
                    self.i += utf8_len(c);
                }
            }
        }
    }

    fn lex_hex4_json(&mut self) -> Result<u16, PgInputFailure> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            match self.peek() {
                None => return Err(self.err_detail(JSON_EOF_DETAIL.into())),
                Some(c) => match (c as char).to_digit(16) {
                    Some(d) => {
                        v = v * 16 + d as u16;
                        self.i += 1;
                    }
                    None => {
                        return Err(self.err_detail(
                            "\"\\u\" must be followed by four hexadecimal digits.".into(),
                        ));
                    }
                },
            }
        }
        Ok(v)
    }

    /// json_lex_number: scans a number, allowing PG's error-tolerant scan;
    /// a malformed number is `Token "%s" is invalid.` with the scanned
    /// text. On success returns the raw token for jsonb numeric checks.
    fn lex_number_json(&mut self) -> Result<String, PgInputFailure> {
        let start = self.i;
        let mut error = false;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        // int part
        match self.peek() {
            Some(b'0') => {
                self.i += 1;
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    error = true;
                    while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.i += 1;
                    }
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            }
            _ => error = true,
        }
        // frac part
        if self.peek() == Some(b'.') {
            self.i += 1;
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            } else {
                error = true;
            }
        }
        // exp part
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
            } else {
                error = true;
            }
        }
        // trailing alphanumeric garbage joins the token
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            error = true;
            self.i += 1;
        }
        let tok = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
        if error {
            return Err(self.err_detail(format!("Token \"{tok}\" is invalid.")));
        }
        Ok(tok)
    }

    fn value(&mut self) -> Result<(), PgInputFailure> {
        self.skip_ws();
        match self.peek() {
            None => Err(self.err_detail(JSON_EOF_DETAIL.into())),
            Some(b'{') => {
                self.depth += 1;
                if self.depth > 6400 {
                    return Err(self.err_detail(
                        "JSON nested too deep, maximum permitted depth is 6400.".into(),
                    ));
                }
                self.i += 1;
                self.skip_ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(());
                }
                loop {
                    self.skip_ws();
                    match self.peek() {
                        None => return Err(self.err_detail(JSON_EOF_DETAIL.into())),
                        Some(b'"') => self.lex_string_raw()?,
                        Some(_) => {
                            return Err(self.err_expected("Expected string or \"}\""));
                        }
                    }
                    self.skip_ws();
                    match self.peek() {
                        None => return Err(self.err_detail(JSON_EOF_DETAIL.into())),
                        Some(b':') => self.i += 1,
                        Some(_) => return Err(self.err_expected("Expected \":\"")),
                    }
                    self.value()?;
                    self.skip_ws();
                    match self.peek() {
                        None => return Err(self.err_detail(JSON_EOF_DETAIL.into())),
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            self.depth -= 1;
                            return Ok(());
                        }
                        Some(_) => {
                            return Err(self.err_expected("Expected \",\" or \"}\""));
                        }
                    }
                }
            }
            Some(b'[') => {
                self.depth += 1;
                if self.depth > 6400 {
                    return Err(self.err_detail(
                        "JSON nested too deep, maximum permitted depth is 6400.".into(),
                    ));
                }
                self.i += 1;
                self.skip_ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(());
                }
                let mut first = true;
                loop {
                    self.skip_ws();
                    if self.peek().is_none() {
                        return Err(self.err_detail(JSON_EOF_DETAIL.into()));
                    }
                    if matches!(self.peek(), Some(b']') | Some(b',') | Some(b'}')) {
                        if first {
                            return Err(self.err_expected("Expected array element or \"]\""));
                        }
                        return Err(self.err_expected("Expected JSON value"));
                    }
                    first = false;
                    self.value()?;
                    self.skip_ws();
                    match self.peek() {
                        None => return Err(self.err_detail(JSON_EOF_DETAIL.into())),
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            self.depth -= 1;
                            return Ok(());
                        }
                        Some(_) => {
                            return Err(self.err_expected("Expected \",\" or \"]\""));
                        }
                    }
                }
            }
            Some(b'"') => self.lex_string_raw(),
            Some(b't') => {
                if self.b[self.i..].starts_with(b"true") {
                    self.i += 4;
                    Ok(())
                } else {
                    Err(self.err_detail(format!("Token \"{}\" is invalid.", self.token_text())))
                }
            }
            Some(b'f') => {
                if self.b[self.i..].starts_with(b"false") {
                    self.i += 5;
                    Ok(())
                } else {
                    Err(self.err_detail(format!("Token \"{}\" is invalid.", self.token_text())))
                }
            }
            Some(b'n') => {
                if self.b[self.i..].starts_with(b"null") {
                    self.i += 4;
                    Ok(())
                } else {
                    Err(self.err_detail(format!("Token \"{}\" is invalid.", self.token_text())))
                }
            }
            Some(b'-') | Some(b'0'..=b'9') => {
                let tok = self.lex_number_json()?;
                if self.jsonb {
                    match crate::storage::Numeric::parse(&tok) {
                        Ok(_) => Ok(()),
                        Err(crate::storage::NumericParseError::Overflow) => {
                            Err(pgi_soft("22003", "value overflows numeric format".into()))
                        }
                        Err(crate::storage::NumericParseError::Syntax) => Ok(()),
                    }
                } else {
                    Ok(())
                }
            }
            Some(c) if matches!(c, b']' | b'}' | b',' | b':') => {
                Err(self.err_expected("Expected JSON value"))
            }
            Some(_) => Err(self.err_detail(format!("Token \"{}\" is invalid.", self.token_text()))),
        }
    }
}

pub(crate) fn pg_input_validate_json(input: &str, jsonb: bool) -> Result<(), PgInputFailure> {
    let mut p = JsonCheck {
        b: input.as_bytes(),
        i: 0,
        depth: 0,
        jsonb,
    };
    p.value()?;
    p.skip_ws();
    if p.peek().is_some() {
        return Err(p.err_expected("Expected end of input"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The unsigned family: oid / oid8 / xid / xid8 / oidvector (numutils.c)
// ---------------------------------------------------------------------------

/// One `strtoul(..., base 0)` scan. Returns (value-as-u64-after-wrap,
/// end_index) or an error kind. Digit scanning stops at the first char
/// that is not a digit of the selected base, like strtoul.
enum UintScan {
    Ok(u64, usize),
    /// No digits at all (strtoul endptr == start).
    NoDigits,
    /// Magnitude overflowed u64 (strtoul ERANGE).
    Range,
}

fn strtoul0(b: &[u8], mut i: usize) -> UintScan {
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    let neg = if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        let n = b[i] == b'-';
        i += 1;
        n
    } else {
        false
    };
    let (base, mut j) = if i + 1 < b.len() && b[i] == b'0' && (b[i + 1] == b'x' || b[i + 1] == b'X')
    {
        (16u32, i + 2)
    } else if i < b.len() && b[i] == b'0' {
        (8u32, i)
    } else {
        (10u32, i)
    };
    let start_digits = j;
    let mut mag: u128 = 0;
    let mut overflow = false;
    while j < b.len() {
        let d = match (b[j] as char).to_digit(base) {
            Some(d) => d as u128,
            None => break,
        };
        if !overflow {
            mag = mag * base as u128 + d;
            if mag > u64::MAX as u128 {
                overflow = true;
            }
        }
        j += 1;
    }
    if j == start_digits {
        // "0x" with no hex digits still consumed the leading 0 in strtoul.
        if base == 16 && j > i {
            // fallthrough: strtoul backs up to the '0'
            j = i + 1; // position after the '0'... see below
            return UintScan::Ok(0, j);
        }
        return UintScan::NoDigits;
    }
    let _ = start_digits;
    if overflow {
        return UintScan::Range;
    }
    let mag64 = mag as u64;
    let val = if neg { 0u64.wrapping_sub(mag64) } else { mag64 };
    UintScan::Ok(val, j)
}

fn uint_range_err(input: &str, typname: &str) -> PgInputFailure {
    pgi_soft(
        "22003",
        format!("value \"{input}\" is out of range for type {typname}"),
    )
}

/// `uint32in_subr` / `uint64in_subr` as a whole-string input function.
pub(crate) fn pg_input_validate_uint(
    input: &str,
    bits: u32,
    typname: &str,
) -> Result<(), PgInputFailure> {
    let b = input.as_bytes();
    match strtoul0(b, 0) {
        UintScan::NoDigits => Err(pgi_syntax(typname, input)),
        UintScan::Range => Err(uint_range_err(input, typname)),
        UintScan::Ok(val, mut j) => {
            while j < b.len() && (b[j] as char).is_whitespace() {
                j += 1;
            }
            if j != b.len() {
                return Err(pgi_syntax(typname, input));
            }
            if bits == 32 {
                // PG's truncation check: accept iff the u64 value equals
                // the zero- or sign-extension of its low 32 bits.
                let lo = val as u32;
                if val != lo as u64 && val != (lo as i32) as i64 as u64 {
                    return Err(uint_range_err(input, typname));
                }
            }
            Ok(())
        }
    }
}

/// `oidvectorin`: whitespace-separated uint32 elements; a failing element
/// reports the error against the string starting at that element.
pub(crate) fn pg_input_validate_oidvector(input: &str) -> Result<(), PgInputFailure> {
    let b = input.as_bytes();
    let mut i = 0usize;
    loop {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return Ok(());
        }
        let tail = &input[i..];
        match strtoul0(b, i) {
            UintScan::NoDigits => return Err(pgi_syntax("oid", tail)),
            UintScan::Range => return Err(uint_range_err(tail, "oid")),
            UintScan::Ok(val, j) => {
                let lo = val as u32;
                if val != lo as u64 && val != (lo as i32) as i64 as u64 {
                    return Err(uint_range_err(tail, "oid"));
                }
                i = j;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// bit / bit varying (varbit.c bit_in)
// ---------------------------------------------------------------------------

pub(crate) fn pg_input_validate_bit(
    input: &str,
    typmod: Option<i32>,
    varying: bool,
) -> Result<(), PgInputFailure> {
    let b = input.as_bytes();
    let (hex, digits) = if !b.is_empty() && (b[0] == b'b' || b[0] == b'B') {
        (false, &input[1..])
    } else if !b.is_empty() && (b[0] == b'x' || b[0] == b'X') {
        (true, &input[1..])
    } else {
        (false, input)
    };
    let bitlen = if hex {
        digits.len() as i64 * 4
    } else {
        digits.len() as i64
    };
    if varying {
        if let Some(tm) = typmod {
            if bitlen > tm as i64 {
                return Err(pgi_soft(
                    "22026",
                    format!("bit string length {bitlen} exceeds type bit varying({tm})"),
                ));
            }
        }
    } else {
        // PG: bare `bit` resolves to typmod 1 at parse time.
        let tm = typmod.unwrap_or(1);
        if bitlen != tm as i64 {
            return Err(pgi_soft(
                "22026",
                format!("bit string length {bitlen} does not match type bit({tm})"),
            ));
        }
    }
    for ch in digits.chars() {
        if hex {
            if !ch.is_ascii_hexdigit() {
                return Err(pgi_soft(
                    "22P02",
                    format!("\"{ch}\" is not a valid hexadecimal digit"),
                ));
            }
        } else if ch != '0' && ch != '1' {
            return Err(pgi_soft(
                "22P02",
                format!("\"{ch}\" is not a valid binary digit"),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// uuid / tid / pg_lsn / pg_snapshot
// ---------------------------------------------------------------------------

pub(crate) fn pg_input_validate_uuid(input: &str) -> Result<(), PgInputFailure> {
    let mut body = input;
    if body.len() >= 9 && body[..9].eq_ignore_ascii_case("urn:uuid:") {
        body = &body[9..];
    }
    if body.len() >= 2 && body.starts_with('{') && body.ends_with('}') {
        body = &body[1..body.len() - 1];
    }
    let bb = body.as_bytes();
    let ok = match bb.len() {
        32 => bb.iter().all(|c| c.is_ascii_hexdigit()),
        36 => bb.iter().enumerate().all(|(i, &c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        }),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(pgi_syntax("uuid", input))
    }
}

pub(crate) fn pg_input_validate_tid(input: &str) -> Result<(), PgInputFailure> {
    let err = || pgi_syntax("tid", input);
    let s = input.trim();
    let inner = s
        .strip_prefix('(')
        .and_then(|x| x.strip_suffix(')'))
        .ok_or_else(err)?;
    let mut parts = inner.split(',');
    let parse_part = |p: Option<&str>| -> Option<u64> {
        let t = p?.trim();
        if t.is_empty() || !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    let block = parse_part(parts.next()).ok_or_else(err)?;
    let offset = parse_part(parts.next()).ok_or_else(err)?;
    if parts.next().is_some() || block > u32::MAX as u64 || offset > 65535 {
        return Err(err());
    }
    Ok(())
}

pub(crate) fn pg_input_validate_pg_lsn(input: &str) -> Result<(), PgInputFailure> {
    let err = || pgi_syntax("pg_lsn", input);
    let (hi, lo) = input.split_once('/').ok_or_else(err)?;
    let parse = |p: &str| -> Option<u64> {
        if p.is_empty() || !p.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(p, 16)
            .ok()
            .filter(|v| *v <= u32::MAX as u64)
    };
    if parse(hi).is_some() && parse(lo).is_some() {
        Ok(())
    } else {
        Err(err())
    }
}

pub(crate) fn pg_input_validate_pg_snapshot(input: &str) -> Result<(), PgInputFailure> {
    let err = || pgi_syntax("pg_snapshot", input);
    let parts: Vec<&str> = input.split(':').collect();
    if parts.len() != 3 {
        return Err(err());
    }
    let parse = |p: &str| -> Option<u64> {
        if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let xmin = parse(parts[0]).ok_or_else(err)?;
    let xmax = parse(parts[1]).ok_or_else(err)?;
    if xmin == 0 || xmax < xmin {
        return Err(err());
    }
    if !parts[2].is_empty() {
        let mut prev: Option<u64> = None;
        for piece in parts[2].split(',') {
            let v = parse(piece).ok_or_else(err)?;
            if v < xmin || v >= xmax {
                return Err(err());
            }
            if let Some(pv) = prev {
                if v < pv {
                    return Err(err());
                }
            }
            prev = Some(v);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// money (cash.c cash_in, C-locale symbols: $ . , + -)
// ---------------------------------------------------------------------------

pub(crate) fn pg_input_validate_money(input: &str) -> Result<(), PgInputFailure> {
    let range_err = || {
        pgi_soft(
            "22003",
            format!("value \"{input}\" is out of range for type money"),
        )
    };
    let b = input.as_bytes();
    let mut s = 0usize;
    let mut sgn: i64 = 1;
    let skip_ws = |s: &mut usize| {
        while *s < b.len() && (b[*s] as char).is_whitespace() {
            *s += 1;
        }
    };
    skip_ws(&mut s);
    if s < b.len() && b[s] == b'$' {
        s += 1;
    }
    skip_ws(&mut s);
    if s < b.len() && b[s] == b'-' {
        sgn = -1;
        s += 1;
    } else if s < b.len() && b[s] == b'(' {
        sgn = -1;
        s += 1;
    } else if s < b.len() && b[s] == b'+' {
        s += 1;
    }
    skip_ws(&mut s);
    if s < b.len() && b[s] == b'$' {
        s += 1;
    }
    skip_ws(&mut s);

    // Value accumulated in the negative, exactly like cash.c.
    let mut value: i64 = 0;
    let mut dec: i32 = 0;
    let mut seen_dot = false;
    while s < b.len() {
        let c = b[s];
        if c.is_ascii_digit() && (!seen_dot || dec < 2) {
            let digit = (c - b'0') as i64;
            value = match value.checked_mul(10).and_then(|v| v.checked_sub(digit)) {
                Some(v) => v,
                None => return Err(range_err()),
            };
            if seen_dot {
                dec += 1;
            }
            s += 1;
        } else if c == b'.' && !seen_dot {
            seen_dot = true;
            s += 1;
        } else if c == b',' {
            s += 1;
        } else {
            break;
        }
    }
    // Round off on one more digit.
    if s < b.len() && b[s].is_ascii_digit() && b[s] >= b'5' {
        value = match value.checked_sub(1) {
            Some(v) => v,
            None => return Err(range_err()),
        };
    }
    while dec < 2 {
        value = match value.checked_mul(10) {
            Some(v) => v,
            None => return Err(range_err()),
        };
        dec += 1;
    }
    while s < b.len() && b[s].is_ascii_digit() {
        s += 1;
    }
    while s < b.len() {
        let c = b[s];
        if (c as char).is_whitespace() || c == b')' {
            s += 1;
        } else if c == b'-' {
            sgn = -1;
            s += 1;
        } else if c == b'+' || c == b'$' {
            s += 1;
        } else {
            return Err(pgi_syntax("money", input));
        }
    }
    if sgn > 0 && value == i64::MIN {
        return Err(range_err());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// inet / cidr (network.c + inet_net_pton.c), macaddr / macaddr8
// ---------------------------------------------------------------------------

pub(crate) fn pg_input_validate_network(input: &str, is_cidr: bool) -> Result<(), PgInputFailure> {
    let typname = if is_cidr { "cidr" } else { "inet" };
    let syntax = || pgi_syntax(typname, input);
    // Split off the mask suffix at the last '/'.
    let (addr, mask) = match input.rfind('/') {
        Some(pos) => (&input[..pos], Some(&input[pos + 1..])),
        None => (input, None),
    };
    if addr.contains(':') {
        // IPv6.
        let ip: std::net::Ipv6Addr = match addr.parse() {
            Ok(ip) => ip,
            Err(_) => return Err(syntax()),
        };
        let bits: u32 = match mask {
            None => 128,
            Some(m) => {
                if m.is_empty() || !m.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(syntax());
                }
                match m.parse::<u32>() {
                    Ok(v) if v <= 128 => v,
                    _ => return Err(syntax()),
                }
            }
        };
        if is_cidr && mask.is_some() {
            let v = u128::from(ip);
            let host = if bits == 128 {
                0
            } else {
                v & ((1u128 << (128 - bits)) - 1)
            };
            if host != 0 {
                return Err(pgi_soft_detail(
                    "22P02",
                    format!("invalid cidr value: \"{input}\""),
                    "Value has bits set to right of mask.".into(),
                ));
            }
        }
        let _ = ip;
        Ok(())
    } else {
        // IPv4 per inet_net_pton_ipv4: dotted decimal octets, each <= 255.
        let mut octets: Vec<u32> = Vec::new();
        let mut cur: Option<u32> = None;
        for c in addr.bytes() {
            match c {
                b'0'..=b'9' => {
                    let v = cur.get_or_insert(0);
                    *v = *v * 10 + (c - b'0') as u32;
                    if *v > 255 {
                        return Err(syntax());
                    }
                }
                b'.' => {
                    octets.push(cur.take().ok_or_else(syntax)?);
                    if octets.len() > 4 {
                        return Err(syntax());
                    }
                }
                _ => return Err(syntax()),
            }
        }
        octets.push(cur.ok_or_else(syntax)?);
        if octets.len() > 4 {
            return Err(syntax());
        }
        let bits: u32 = match mask {
            None => {
                if octets.len() != 4 {
                    return Err(syntax());
                }
                32
            }
            Some(m) => {
                if m.is_empty() || !m.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(syntax());
                }
                match m.parse::<u32>() {
                    Ok(v) if v <= 32 => v,
                    _ => return Err(syntax()),
                }
            }
        };
        if mask.is_some() && (bits / 8) as usize > octets.len() {
            return Err(syntax());
        }
        if is_cidr && mask.is_some() {
            let mut v: u32 = 0;
            for i in 0..4 {
                v = (v << 8) | octets.get(i).copied().unwrap_or(0);
            }
            let host = if bits == 32 {
                0
            } else {
                v & ((1u32 << (32 - bits)) - 1)
            };
            if host != 0 {
                return Err(pgi_soft_detail(
                    "22P02",
                    format!("invalid cidr value: \"{input}\""),
                    "Value has bits set to right of mask.".into(),
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn pg_input_validate_macaddr(input: &str, eui64: bool) -> Result<(), PgInputFailure> {
    let typname = if eui64 { "macaddr8" } else { "macaddr" };
    let want = if eui64 { 8 } else { 6 };
    // PG accepts ':' '-' '.' separated hex byte pairs (and Cisco-style
    // dotted quads for macaddr); validity turns on the hex content.
    let cleaned: String = input
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.' | ' '))
        .collect();
    if cleaned.len() == want * 2 && cleaned.bytes().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(pgi_syntax(typname, input))
    }
}

// ---------------------------------------------------------------------------
// Geometry types (geo_ops.c pair_decode / path_decode ports)
// ---------------------------------------------------------------------------

enum GeoFloat {
    Ok(f64, usize), // value, end position (after trailing ws)
    Syntax,
    Range(String),
}

/// Scan one float like `float8in_internal` with an end pointer: leading
/// whitespace skipped, trailing whitespace consumed into the end point.
fn geo_scan_float(b: &[u8], mut i: usize) -> GeoFloat {
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    let tok_start = i;
    let mut j = i;
    if j < b.len() && (b[j] == b'-' || b[j] == b'+') {
        j += 1;
    }
    // inf / infinity / nan words
    let rest = &b[j..];
    let word_len = [(b"infinity" as &[u8], 8usize), (b"inf", 3), (b"nan", 3)]
        .iter()
        .find(|(w, _)| rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w))
        .map(|(_, n)| *n);
    if let Some(n) = word_len {
        let text = String::from_utf8_lossy(&b[tok_start..j + n]).into_owned();
        let neg = b[tok_start] == b'-';
        let v = if text.to_ascii_lowercase().contains("nan") {
            f64::NAN
        } else if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        let mut end = j + n;
        while end < b.len() && (b[end] as char).is_whitespace() {
            end += 1;
        }
        return GeoFloat::Ok(v, end);
    }
    // Decimal mantissa: digits, one optional '.', more digits (strtod
    // stops at '_' exactly like this scan does).
    let mut k = j;
    let mut saw_digit = false;
    let mut saw_dot = false;
    loop {
        match b.get(k).copied() {
            Some(c) if c.is_ascii_digit() => {
                saw_digit = true;
                k += 1;
            }
            Some(b'.') if !saw_dot => {
                saw_dot = true;
                k += 1;
            }
            _ => break,
        }
    }
    if !saw_digit {
        return GeoFloat::Syntax;
    }
    // Exponent (backtrack if no digits follow).
    if matches!(b.get(k).copied(), Some(b'e') | Some(b'E')) {
        let mut m = k + 1;
        if matches!(b.get(m).copied(), Some(b'+') | Some(b'-')) {
            m += 1;
        }
        let exp_start = m;
        while b.get(m).is_some_and(|c| c.is_ascii_digit()) {
            m += 1;
        }
        if m > exp_start {
            k = m;
        }
    }
    let raw = String::from_utf8_lossy(&b[tok_start..k]).into_owned();
    let v: f64 = match raw.parse() {
        Ok(v) => v,
        Err(_) => return GeoFloat::Syntax,
    };
    if v.is_infinite() || (v == 0.0 && raw.chars().any(|c| ('1'..='9').contains(&c))) {
        return GeoFloat::Range(raw);
    }
    let mut end = k;
    while end < b.len() && (b[end] as char).is_whitespace() {
        end += 1;
    }
    GeoFloat::Ok(v, end)
}

struct Geo<'a> {
    b: &'a [u8],
    orig: &'a str,
    ty: &'a str,
}

impl<'a> Geo<'a> {
    fn syntax(&self) -> PgInputFailure {
        pgi_syntax(self.ty, self.orig)
    }

    fn single(&self, i: usize) -> Result<(f64, usize), PgInputFailure> {
        match geo_scan_float(self.b, i) {
            GeoFloat::Ok(v, end) => Ok((v, end)),
            GeoFloat::Syntax => Err(self.syntax()),
            GeoFloat::Range(tok) => Err(pgi_soft(
                "22003",
                format!("\"{tok}\" is out of range for type double precision"),
            )),
        }
    }

    fn skip_ws(&self, mut i: usize) -> usize {
        while i < self.b.len() && (self.b[i] as char).is_whitespace() {
            i += 1;
        }
        i
    }

    fn at(&self, i: usize) -> Option<u8> {
        self.b.get(i).copied()
    }

    /// pair_decode. When `eof_ok` is false this mirrors endptr_p == NULL
    /// (trailing junk is an error); when true returns the stop position.
    fn pair(&self, mut i: usize, eof_ok: bool) -> Result<(usize, (f64, f64)), PgInputFailure> {
        i = self.skip_ws(i);
        let has_delim = self.at(i) == Some(b'(');
        if has_delim {
            i += 1;
        }
        let (x, ni) = self.single(i)?;
        i = ni;
        if self.at(i) != Some(b',') {
            return Err(self.syntax());
        }
        i += 1;
        let (y, ni) = self.single(i)?;
        i = ni;
        if has_delim {
            if self.at(i) != Some(b')') {
                return Err(self.syntax());
            }
            i += 1;
            i = self.skip_ws(i);
        }
        if !eof_ok && i != self.b.len() {
            return Err(self.syntax());
        }
        Ok((i, (x, y)))
    }

    /// path_decode. Returns (endpos, points) when `eof_ok`, else validates
    /// to end of string.
    fn path(
        &self,
        mut i: usize,
        opentype: bool,
        npts: usize,
        eof_ok: bool,
    ) -> Result<(usize, Vec<(f64, f64)>), PgInputFailure> {
        let mut depth = 0usize;
        let mut isopen = false;
        i = self.skip_ws(i);
        if self.at(i) == Some(b'[') {
            if !opentype {
                return Err(self.syntax());
            }
            isopen = true;
            depth += 1;
            i += 1;
        } else if self.at(i) == Some(b'(') {
            let cp = self.skip_ws(i + 1);
            if self.at(cp) == Some(b'(') {
                depth += 1;
                i = cp;
            } else if !self.b[..i].contains(&b'(') && !self.b[i + 1..].contains(&b'(') {
                // strrchr(str, '(') == str: the only '(' is this one.
                depth += 1;
                i = cp;
            }
        }
        let mut pts = Vec::new();
        for _ in 0..npts {
            let (ni, pt) = self.pair(i, true)?;
            i = ni;
            pts.push(pt);
            if self.at(i) == Some(b',') {
                i += 1;
            }
        }
        while depth > 0 {
            if self.at(i) == Some(b')') || (self.at(i) == Some(b']') && isopen && depth == 1) {
                depth -= 1;
                i += 1;
                i = self.skip_ws(i);
            } else {
                return Err(self.syntax());
            }
        }
        if !eof_ok && i != self.b.len() {
            return Err(self.syntax());
        }
        Ok((i, pts))
    }
}

fn pair_count(input: &str) -> usize {
    input.bytes().filter(|&c| c == b',').count()
}

pub(crate) fn pg_input_validate_geometry(input: &str, ty: &str) -> Result<(), PgInputFailure> {
    let g = Geo {
        b: input.as_bytes(),
        orig: input,
        ty,
    };
    match ty {
        "point" => {
            g.pair(0, false)?;
        }
        "box" => {
            g.path(0, false, 2, false)?;
        }
        "lseg" => {
            g.path(0, true, 2, false)?;
        }
        "path" => {
            if pair_count(input) % 2 == 0 {
                return Err(g.syntax());
            }
            // path_in: count points from commas after optional leading '('.
            let mut i = g.skip_ws(0);
            let mut depth = 0usize;
            if g.at(i) == Some(b'(')
                && !input.as_bytes()[..i].contains(&b'(')
                && !input.as_bytes()[i + 1..].contains(&b'(')
            {
                depth += 1;
                i += 1;
            }
            let (ni, _pts) = g.path_counted(i)?;
            i = ni;
            if depth >= 1 {
                if g.at(i) == Some(b')') {
                    i += 1;
                    i = g.skip_ws(i);
                } else {
                    return Err(g.syntax());
                }
            }
            if i != input.len() {
                return Err(g.syntax());
            }
        }
        "polygon" => {
            if pair_count(input) % 2 == 0 {
                return Err(g.syntax());
            }
            let npts = (pair_count(input) + 1) / 2;
            g.path(0, false, npts, false)?;
        }
        "circle" => {
            let mut i = g.skip_ws(0);
            let mut depth = 0usize;
            if g.at(i) == Some(b'<') {
                depth += 1;
                i += 1;
            } else if g.at(i) == Some(b'(') {
                let cp = g.skip_ws(i + 1);
                if g.at(cp) == Some(b'(') {
                    depth += 1;
                    i = cp;
                }
            }
            let (ni, _center) = g.pair(i, true)?;
            i = ni;
            if g.at(i) == Some(b',') {
                i += 1;
            }
            let (radius, ni) = g.single(i)?;
            i = ni;
            if radius < 0.0 {
                return Err(g.syntax());
            }
            while depth > 0 {
                if g.at(i) == Some(b')') || (g.at(i) == Some(b'>') && depth == 1) {
                    depth -= 1;
                    i += 1;
                    i = g.skip_ws(i);
                } else {
                    return Err(g.syntax());
                }
            }
            if i != input.len() {
                return Err(g.syntax());
            }
        }
        "line" => {
            let mut i = g.skip_ws(0);
            if g.at(i) == Some(b'{') {
                i += 1;
                let (a, ni) = g.single(i)?;
                i = ni;
                if g.at(i) != Some(b',') {
                    return Err(g.syntax());
                }
                i += 1;
                let (bv, ni) = g.single(i)?;
                i = ni;
                if g.at(i) != Some(b',') {
                    return Err(g.syntax());
                }
                i += 1;
                let (_c, ni) = g.single(i)?;
                i = ni;
                if g.at(i) != Some(b'}') {
                    return Err(g.syntax());
                }
                i += 1;
                i = g.skip_ws(i);
                if i != input.len() {
                    return Err(g.syntax());
                }
                if a == 0.0 && bv == 0.0 {
                    return Err(pgi_soft(
                        "22P02",
                        "invalid line specification: A and B cannot both be zero".into(),
                    ));
                }
            } else {
                let (_ni, pts) = g.path(0, true, 2, false)?;
                if pts[0] == pts[1] {
                    return Err(pgi_soft(
                        "22P02",
                        "invalid line specification: must be two distinct points".into(),
                    ));
                }
            }
        }
        _ => return Err(g.syntax()),
    }
    Ok(())
}

impl<'a> Geo<'a> {
    /// path_in's decode: points counted from the comma total; reuses
    /// path_decode with an endptr, with the outer paren already handled
    /// by the caller.
    fn path_counted(&self, i: usize) -> Result<(usize, Vec<(f64, f64)>), PgInputFailure> {
        // pair_count includes commas inside pairs: npts = (commas+1)/2.
        let npts = (pair_count(self.orig) + 1) / 2;
        self.path(i, true, npts, true)
    }
}

// ---------------------------------------------------------------------------
// Ranges and multiranges (rangetypes.c / multirangetypes.c)
// ---------------------------------------------------------------------------

struct RangeParts {
    lb_inc: bool,
    lb: Option<String>,
    ub: Option<String>,
    ub_inc: bool,
    empty: bool,
}

fn range_malformed(input: &str, detail: &str) -> PgInputFailure {
    pgi_soft_detail(
        "22P02",
        format!("malformed range literal: \"{input}\""),
        detail.into(),
    )
}

fn range_parse_bound(
    input: &str,
    b: &[u8],
    mut i: usize,
) -> Result<(Option<String>, usize), PgInputFailure> {
    match b.get(i).copied() {
        Some(b',') | Some(b')') | Some(b']') => return Ok((None, i)),
        _ => {}
    }
    let mut out = String::new();
    let mut inquote = false;
    loop {
        let ch = match b.get(i).copied() {
            None => {
                return Err(range_malformed(input, "Unexpected end of input."));
            }
            Some(c) => c,
        };
        if ch == b'\\' {
            let nxt = match b.get(i + 1).copied() {
                None => {
                    return Err(range_malformed(input, "Unexpected end of input."));
                }
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

fn range_parse(input: &str) -> Result<RangeParts, PgInputFailure> {
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
            return Err(range_malformed(input, "Junk after \"empty\" key word."));
        }
        return Ok(RangeParts {
            lb_inc: false,
            lb: None,
            ub: None,
            ub_inc: false,
            empty: true,
        });
    }
    let mut parts = RangeParts {
        lb_inc: false,
        lb: None,
        ub: None,
        ub_inc: false,
        empty: false,
    };
    match b.get(i).copied() {
        Some(b'[') => {
            parts.lb_inc = true;
            i += 1;
        }
        Some(b'(') => i += 1,
        _ => {
            return Err(range_malformed(
                input,
                "Missing left parenthesis or bracket.",
            ));
        }
    }
    let (lb, ni) = range_parse_bound(input, b, i)?;
    parts.lb = lb;
    i = ni;
    if b.get(i).copied() == Some(b',') {
        i += 1;
    } else {
        return Err(range_malformed(input, "Missing comma after lower bound."));
    }
    let (ub, ni) = range_parse_bound(input, b, i)?;
    parts.ub = ub;
    i = ni;
    match b.get(i).copied() {
        Some(b']') => {
            parts.ub_inc = true;
            i += 1;
        }
        Some(b')') => i += 1,
        _ => return Err(range_malformed(input, "Too many commas.")),
    }
    skip_ws(&mut i);
    if i != b.len() {
        return Err(range_malformed(
            input,
            "Junk after right parenthesis or bracket.",
        ));
    }
    Ok(parts)
}

#[derive(Clone, PartialEq)]
enum RangeKey {
    Int(i64),
    Date(i32),
    Ts(i64),
    Num(crate::storage::Numeric),
}

impl RangeKey {
    fn cmp_key(&self, other: &RangeKey) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (RangeKey::Int(a), RangeKey::Int(b)) => a.cmp(b),
            (RangeKey::Date(a), RangeKey::Date(b)) => a.cmp(b),
            (RangeKey::Ts(a), RangeKey::Ts(b)) => a.cmp(b),
            (RangeKey::Num(a), RangeKey::Num(b)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RangeKind {
    Int4,
    Int8,
    Date,
    Num,
    Ts,
    Tstz,
}

fn range_kind_info(kind: RangeKind) -> (&'static str, bool) {
    // (element type name for the pure validator, discrete?)
    match kind {
        RangeKind::Int4 => ("integer", true),
        RangeKind::Int8 => ("bigint", true),
        RangeKind::Date => ("date", true),
        RangeKind::Num => ("numeric", false),
        RangeKind::Ts => ("timestamp", false),
        RangeKind::Tstz => ("timestamptz", false),
    }
}

fn range_bound_key(fn_name: &str, kind: RangeKind, text: &str) -> Result<RangeKey, PgInputFailure> {
    // The element input function runs first (its error is the result).
    let (elem, _) = range_kind_info(kind);
    super::cast::pg_input_validate(fn_name, text, elem)?;
    Ok(match kind {
        RangeKind::Int4 => RangeKey::Int(pg_strtoint(text, 32).unwrap_or(0)),
        RangeKind::Int8 => RangeKey::Int(pg_strtoint(text, 64).unwrap_or(0)),
        RangeKind::Date => RangeKey::Date(crate::datetime::parse_date(text).unwrap_or(0)),
        RangeKind::Ts => RangeKey::Ts(crate::datetime::parse_timestamp(text).unwrap_or(0)),
        RangeKind::Tstz => RangeKey::Ts(crate::datetime::parse_timestamptz(text).unwrap_or(0)),
        RangeKind::Num => RangeKey::Num(
            crate::storage::Numeric::parse(text).unwrap_or(crate::storage::Numeric::zero()),
        ),
    })
}

/// Compare two bounds the way `range_cmp_bound_values` does, where a
/// missing lower bound is -infinity and a missing upper is +infinity.
fn range_cmp_bounds(
    lb: &Option<RangeKey>,
    lb_inc: bool,
    ub: &Option<RangeKey>,
    ub_inc: bool,
) -> (std::cmp::Ordering, bool) {
    use std::cmp::Ordering;
    let cmp = match (lb, ub) {
        (None, _) | (_, None) => Ordering::Less,
        (Some(a), Some(b)) => a.cmp_key(b),
    };
    (cmp, lb_inc && ub_inc)
}

pub(crate) fn pg_input_validate_range(
    fn_name: &str,
    input: &str,
    kind: RangeKind,
) -> Result<(), PgInputFailure> {
    let parts = range_parse(input)?;
    if parts.empty {
        return Ok(());
    }
    let lb_key = match &parts.lb {
        Some(t) => Some(range_bound_key(fn_name, kind, t)?),
        None => None,
    };
    let ub_key = match &parts.ub {
        Some(t) => Some(range_bound_key(fn_name, kind, t)?),
        None => None,
    };
    let (cmp, both_inc) = range_cmp_bounds(&lb_key, parts.lb_inc, &ub_key, parts.ub_inc);
    match cmp {
        std::cmp::Ordering::Greater => {
            return Err(pgi_soft(
                "22000",
                "range lower bound must be less than or equal to range upper bound".into(),
            ));
        }
        std::cmp::Ordering::Equal if !both_inc => return Ok(()), // empty
        _ => {}
    }
    let (_, discrete) = range_kind_info(kind);
    if discrete {
        // Canonicalize to [lower, upper) via the type's canonical fn.
        let mut lb = lb_key.clone();
        let mut ub = ub_key.clone();
        if let Some(k) = &lb {
            if !parts.lb_inc {
                lb = Some(range_step_up(kind, k)?);
            }
        }
        if let Some(k) = &ub {
            if parts.ub_inc {
                ub = Some(range_step_up(kind, k)?);
            }
        }
        // After canonicalization the range is empty iff lower >= upper.
        if let (Some(a), Some(b)) = (&lb, &ub) {
            if a.cmp_key(b) != std::cmp::Ordering::Less {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn range_step_up(kind: RangeKind, k: &RangeKey) -> Result<RangeKey, PgInputFailure> {
    Ok(match (kind, k) {
        (RangeKind::Int4, RangeKey::Int(v)) => {
            if *v == i32::MAX as i64 {
                return Err(pgi_soft("22003", "integer out of range".into()));
            }
            RangeKey::Int(v + 1)
        }
        (RangeKind::Int8, RangeKey::Int(v)) => {
            if *v == i64::MAX {
                return Err(pgi_soft("22003", "bigint out of range".into()));
            }
            RangeKey::Int(v + 1)
        }
        (RangeKind::Date, RangeKey::Date(v)) => {
            // DATE_NOT_FINITE bounds are skipped by daterange_canonical.
            if *v == crate::datetime::DATE_INFINITY || *v == crate::datetime::DATE_NEG_INFINITY {
                RangeKey::Date(*v)
            } else if *v + 1 > max_valid_date() {
                return Err(pgi_soft("22008", "date out of range".into()));
            } else {
                RangeKey::Date(v + 1)
            }
        }
        _ => k.clone(),
    })
}

fn max_valid_date() -> i32 {
    // PG IS_VALID_DATE upper bound: 5874897-12-31.
    crate::datetime::date_max_days() as i32
}

pub(crate) fn pg_input_validate_multirange(
    fn_name: &str,
    input: &str,
    kind: RangeKind,
) -> Result<(), PgInputFailure> {
    let malformed = |detail: &str| {
        pgi_soft_detail(
            "22P02",
            format!("malformed multirange literal: \"{input}\""),
            detail.into(),
        )
    };
    let b = input.as_bytes();
    let mut i = 0usize;
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    if b.get(i).copied() != Some(b'{') {
        return Err(malformed("Missing left brace."));
    }
    i += 1;
    // States: before-range, in-range, in-range-quoted, in-range-escaped,
    // in-range-quoted-escaped, after-range, finished.
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
            None => return Err(malformed("Unexpected end of input.")),
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
                    seen += 1;
                    i += 5;
                    state = Ms::After;
                    continue;
                } else {
                    return Err(malformed("Expected range start."));
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
                    return Err(malformed("Expected comma or end of multirange."));
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
        return Err(malformed("Junk after closing right brace."));
    }
    for r in &ranges {
        pg_input_validate_range(fn_name, r, kind)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Arrays (array_in's literal grammar; elements validated recursively)
// ---------------------------------------------------------------------------

enum ArrNode {
    Leaf(Option<String>),
    List(Vec<ArrNode>),
}

struct ArrParser<'a> {
    b: &'a [u8],
    i: usize,
    input: &'a str,
}

impl<'a> ArrParser<'a> {
    fn malformed(&self, detail: &str) -> PgInputFailure {
        pgi_soft_detail(
            "22P02",
            format!("malformed array literal: \"{}\"", self.input),
            detail.into(),
        )
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && (self.b[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }

    fn parse_level(&mut self, depth: usize) -> Result<ArrNode, PgInputFailure> {
        // At entry, current char is '{'.
        self.i += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.b.get(self.i).copied() == Some(b'}') {
            self.i += 1;
            return Ok(ArrNode::List(items));
        }
        loop {
            self.skip_ws();
            match self.b.get(self.i).copied() {
                Some(b'{') => {
                    let inner = self.parse_level(depth + 1)?;
                    items.push(inner);
                }
                Some(b'"') => {
                    self.i += 1;
                    let mut val = String::new();
                    loop {
                        match self.b.get(self.i).copied() {
                            None => {
                                return Err(self.malformed("Unexpected end of input."));
                            }
                            Some(b'"') => {
                                self.i += 1;
                                break;
                            }
                            Some(b'\\') => {
                                let n =
                                    self.b.get(self.i + 1).copied().ok_or_else(|| {
                                        self.malformed("Unexpected end of input.")
                                    })?;
                                val.push(n as char);
                                self.i += 2;
                            }
                            Some(c) => {
                                val.push(c as char);
                                self.i += 1;
                            }
                        }
                    }
                    items.push(ArrNode::Leaf(Some(val)));
                }
                Some(_) => {
                    let start = self.i;
                    let mut end = self.i;
                    let mut val = String::new();
                    while let Some(c) = self.b.get(self.i).copied() {
                        if c == b',' || c == b'}' {
                            break;
                        }
                        if c == b'\\' {
                            if let Some(n) = self.b.get(self.i + 1).copied() {
                                val.push(n as char);
                                self.i += 2;
                                end = self.i;
                                continue;
                            }
                            return Err(self.malformed("Unexpected end of input."));
                        }
                        val.push(c as char);
                        self.i += 1;
                        if !(c as char).is_whitespace() {
                            end = self.i;
                        }
                    }
                    let _ = start;
                    let raw = val.trim_end().to_string();
                    let _ = end;
                    if raw.eq_ignore_ascii_case("null") {
                        items.push(ArrNode::Leaf(None));
                    } else {
                        items.push(ArrNode::Leaf(Some(raw)));
                    }
                }
                None => return Err(self.malformed("Unexpected end of input.")),
            }
            self.skip_ws();
            match self.b.get(self.i).copied() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return Err(self.malformed("Missing \",\" or \"}\" in array literal.")),
            }
        }
        Ok(ArrNode::List(items))
    }
}

/// Rectangularity: every sibling subtree must have the identical shape
/// vector (lengths per level), which is PG's "matching dimensions" rule.
fn arr_check_rect(node: &ArrNode) -> Result<Vec<usize>, PgInputFailure> {
    match node {
        ArrNode::Leaf(_) => Ok(Vec::new()),
        ArrNode::List(items) => {
            let mut shape = vec![items.len()];
            let mut child_shape: Option<Vec<usize>> = None;
            for it in items {
                let s = arr_check_rect(it)?;
                match &child_shape {
                    None => child_shape = Some(s),
                    Some(prev) if *prev != s => {
                        return Err(pgi_soft(
                            "22P02",
                            "multidimensional arrays must have array expressions with matching dimensions".into(),
                        ));
                    }
                    _ => {}
                }
            }
            if let Some(cs) = child_shape {
                shape.extend(cs);
            }
            Ok(shape)
        }
    }
}

fn arr_validate_leaves(
    fn_name: &str,
    node: &ArrNode,
    elem_typ: &str,
) -> Result<(), PgInputFailure> {
    match node {
        ArrNode::Leaf(None) => Ok(()),
        ArrNode::Leaf(Some(v)) => super::cast::pg_input_validate(fn_name, v, elem_typ),
        ArrNode::List(items) => {
            for it in items {
                arr_validate_leaves(fn_name, it, elem_typ)?;
            }
            Ok(())
        }
    }
}

/// Parse an array literal into its element tree (structure only; the
/// `[l:u]=` dimension header is accepted and skipped). Shared by the
/// pure and catalog array validators.
fn arr_parse_tree(input: &str) -> Result<ArrNode, PgInputFailure> {
    let mut body = input.trim();
    if body.starts_with('[') {
        // Dimension header(s) run up to the '='.
        match body.find('=') {
            Some(eq) if body[..eq].trim_end().ends_with(']') => {
                body = body[eq + 1..].trim_start();
            }
            _ => {
                let p = ArrParser {
                    b: body.as_bytes(),
                    i: 0,
                    input,
                };
                return Err(
                    p.malformed("Array value must start with \"{\" or dimension information.")
                );
            }
        }
    }
    let mut p = ArrParser {
        b: body.as_bytes(),
        i: 0,
        input,
    };
    if !body.starts_with('{') {
        return Err(p.malformed("Array value must start with \"{\" or dimension information."));
    }
    let node = p.parse_level(1)?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(p.malformed("Junk after closing right brace."));
    }
    Ok(node)
}

/// Validate `input` as an array literal whose leaves are of `elem_typ`.
/// PG makes no distinction between `T[]` and `T[][]` at the type level,
/// so nesting depth is taken from the literal; only rectangularity and
/// the leaf values are checked.
pub(crate) fn pg_input_validate_array(
    fn_name: &str,
    input: &str,
    elem_typ: &str,
) -> Result<(), PgInputFailure> {
    let node = arr_parse_tree(input)?;
    arr_check_rect(&node)?;
    arr_validate_leaves(fn_name, &node, elem_typ)
}

// ---------------------------------------------------------------------------
// xml (xml.c xml_in with DOCUMENT option): a well-formedness checker
// ---------------------------------------------------------------------------

pub(crate) fn pg_input_validate_xml(input: &str) -> Result<(), PgInputFailure> {
    let generic = || pgi_soft("2200N", "invalid XML content".into());
    let decl_err = || {
        pgi_soft(
            "2200N",
            "invalid XML content: invalid XML declaration".into(),
        )
    };
    let b = input.as_bytes();
    let mut i = 0usize;
    let skip_ws = |i: &mut usize| {
        while *i < b.len() && (b[*i] as char).is_whitespace() {
            *i += 1;
        }
    };
    // Optional BOM.
    if input.starts_with('\u{FEFF}') {
        i = 3;
    }
    skip_ws(&mut i);
    // XML declaration: `<?xml ... ?>` — validate strictly like libxml2.
    if input[i..].starts_with("<?xml")
        && matches!(
            b.get(i + 5),
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'?')
        )
    {
        let end = match input[i..].find("?>") {
            Some(e) => i + e,
            None => return Err(decl_err()),
        };
        let decl = &input[i + 5..end];
        // Parse attributes: version (required, first), encoding,
        // standalone ("yes"/"no" only), in document order.
        let mut rest = decl.trim();
        let mut seen: Vec<&str> = Vec::new();
        let mut ok = true;
        while !rest.is_empty() {
            let eq = match rest.find('=') {
                Some(e) => e,
                None => {
                    ok = false;
                    break;
                }
            };
            let name = rest[..eq].trim();
            let after = rest[eq + 1..].trim_start();
            let q = after.chars().next().unwrap_or(' ');
            if q != '"' && q != '\'' {
                ok = false;
                break;
            }
            let close = match after[1..].find(q) {
                Some(c) => c + 1,
                None => {
                    ok = false;
                    break;
                }
            };
            let value = &after[1..close];
            match name {
                "version" => {
                    if !seen.is_empty()
                        || value.is_empty()
                        || !value
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
                    {
                        ok = false;
                        break;
                    }
                }
                "encoding" => {
                    if seen != ["version"] || value.is_empty() {
                        ok = false;
                        break;
                    }
                }
                "standalone" => {
                    if !(seen == ["version"] || seen == ["version", "encoding"])
                        || (value != "yes" && value != "no")
                    {
                        ok = false;
                        break;
                    }
                }
                _ => {
                    ok = false;
                    break;
                }
            }
            seen.push(name);
            rest = after[close + 1..].trim();
        }
        if !ok || seen.first() != Some(&"version") {
            return Err(decl_err());
        }
        i = end + 2;
    }
    // Element structure: exactly one root element, matched tags.
    let mut stack: Vec<String> = Vec::new();
    let mut root_seen = false;
    let is_name = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':');
    while i < b.len() {
        if b[i] == b'<' {
            if input[i..].starts_with("<!--") {
                match input[i + 4..].find("-->") {
                    Some(e) => i += 4 + e + 3,
                    None => return Err(generic()),
                }
            } else if input[i..].starts_with("<![CDATA[") {
                if stack.is_empty() {
                    return Err(generic());
                }
                match input[i + 9..].find("]]>") {
                    Some(e) => i += 9 + e + 3,
                    None => return Err(generic()),
                }
            } else if input[i..].starts_with("<?") {
                match input[i + 2..].find("?>") {
                    Some(e) => i += 2 + e + 2,
                    None => return Err(generic()),
                }
            } else if input[i..].starts_with("<!") {
                // DOCTYPE and friends: skip to '>' (no internal subset
                // support beyond a bracketed block).
                let mut j = i + 2;
                let mut closed = false;
                while j < b.len() {
                    if b[j] == b'[' {
                        match input[j..].find(']') {
                            Some(e) => j += e + 1,
                            None => return Err(generic()),
                        }
                    } else if b[j] == b'>' {
                        closed = true;
                        j += 1;
                        break;
                    } else {
                        j += 1;
                    }
                }
                if !closed {
                    return Err(generic());
                }
                i = j;
            } else if input[i..].starts_with("</") {
                let mut j = i + 2;
                let start = j;
                while j < b.len() && is_name(b[j]) {
                    j += 1;
                }
                let name = &input[start..j];
                while j < b.len() && (b[j] as char).is_whitespace() {
                    j += 1;
                }
                if j >= b.len() || b[j] != b'>' || name.is_empty() {
                    return Err(generic());
                }
                match stack.pop() {
                    Some(open) if open == name => {}
                    _ => return Err(generic()),
                }
                i = j + 1;
            } else {
                // Open tag.
                if stack.is_empty() {
                    if root_seen {
                        return Err(generic());
                    }
                    root_seen = true;
                }
                let mut j = i + 1;
                let start = j;
                while j < b.len() && is_name(b[j]) {
                    j += 1;
                }
                let name = &input[start..j];
                if name.is_empty()
                    || !(name.as_bytes()[0].is_ascii_alphabetic() || name.as_bytes()[0] == b'_')
                {
                    return Err(generic());
                }
                // Attributes up to '>' or '/>'.
                let mut self_close = false;
                let mut closed = false;
                while j < b.len() {
                    let c = b[j];
                    if c == b'>' {
                        closed = true;
                        j += 1;
                        break;
                    }
                    if c == b'/' && b.get(j + 1) == Some(&b'>') {
                        self_close = true;
                        closed = true;
                        j += 2;
                        break;
                    }
                    if c == b'"' || c == b'\'' {
                        let q = c;
                        j += 1;
                        while j < b.len() && b[j] != q {
                            if b[j] == b'<' {
                                return Err(generic());
                            }
                            j += 1;
                        }
                        if j >= b.len() {
                            return Err(generic());
                        }
                        j += 1;
                    } else if c == b'<' {
                        return Err(generic());
                    } else {
                        j += 1;
                    }
                }
                if !closed {
                    return Err(generic());
                }
                if !self_close {
                    stack.push(name.to_string());
                }
                i = j;
            }
        } else if b[i] == b'&' {
            // Character/entity reference in content.
            if stack.is_empty() {
                return Err(generic());
            }
            let semi = match input[i..].find(';') {
                Some(e) if e <= 12 => i + e,
                _ => return Err(generic()),
            };
            let ent = &input[i + 1..semi];
            let valid = matches!(ent, "lt" | "gt" | "amp" | "apos" | "quot")
                || (ent.starts_with("#x")
                    && ent[2..].bytes().all(|c| c.is_ascii_hexdigit())
                    && ent.len() > 2)
                || (ent.starts_with('#')
                    && ent[1..].bytes().all(|c| c.is_ascii_digit())
                    && ent.len() > 1);
            if !valid {
                return Err(generic());
            }
            i = semi + 1;
        } else {
            // Character data: not allowed outside the root element
            // (except whitespace).
            if stack.is_empty() && !(b[i] as char).is_whitespace() {
                return Err(generic());
            }
            i += 1;
        }
    }
    if !stack.is_empty() || !root_seen {
        return Err(generic());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tsvector / tsquery (subset ports of the tsvector/tsquery parsers)
// ---------------------------------------------------------------------------

pub(crate) fn pg_input_validate_tsvector(input: &str) -> Result<(), PgInputFailure> {
    let syntax = || pgi_soft("42601", format!("syntax error in tsvector: \"{input}\""));
    let b = input.as_bytes();
    let mut i = 0usize;
    let mut any = false;
    loop {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        any = true;
        // One lexeme.
        if b[i] == b'\'' {
            i += 1;
            let mut len = 0usize;
            loop {
                match b.get(i).copied() {
                    None => return Err(syntax()),
                    Some(b'\\') => {
                        i += 2;
                        len += 1;
                    }
                    Some(b'\'') => {
                        if b.get(i + 1).copied() == Some(b'\'') {
                            i += 2;
                            len += 1;
                        } else {
                            i += 1;
                            break;
                        }
                    }
                    Some(_) => {
                        i += 1;
                        len += 1;
                    }
                }
            }
            if len == 0 {
                return Err(syntax());
            }
        } else {
            let start = i;
            while i < b.len() && !(b[i] as char).is_whitespace() && b[i] != b':' {
                i += 1;
            }
            if i == start {
                return Err(syntax());
            }
        }
        // Optional :positions.
        if b.get(i).copied() == Some(b':') {
            i += 1;
            loop {
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start {
                    return Err(syntax());
                }
                if matches!(
                    b.get(i).copied(),
                    Some(b'A')
                        | Some(b'B')
                        | Some(b'C')
                        | Some(b'D')
                        | Some(b'a')
                        | Some(b'b')
                        | Some(b'c')
                        | Some(b'd')
                        | Some(b'*')
                ) {
                    i += 1;
                }
                if b.get(i).copied() == Some(b',') {
                    i += 1;
                    continue;
                }
                break;
            }
        }
        if i < b.len() && !(b[i] as char).is_whitespace() {
            return Err(syntax());
        }
    }
    let _ = any;
    Ok(())
}

struct TsqParser<'a> {
    b: &'a [u8],
    i: usize,
    input: &'a str,
}

impl<'a> TsqParser<'a> {
    fn syntax(&self) -> PgInputFailure {
        pgi_soft(
            "42601",
            format!("syntax error in tsquery: \"{}\"", self.input),
        )
    }

    fn skip_ws(&mut self) {
        while self.i < self.b.len() && (self.b[self.i] as char).is_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn parse_or(&mut self) -> Result<(), PgInputFailure> {
        self.parse_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some(b'|') {
                self.i += 1;
                self.parse_and()?;
            } else {
                return Ok(());
            }
        }
    }

    fn parse_and(&mut self) -> Result<(), PgInputFailure> {
        self.parse_unary()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'&') => {
                    self.i += 1;
                    self.parse_unary()?;
                }
                Some(b'<') => {
                    // Phrase operator: <-> or <N>.
                    let save = self.i;
                    self.i += 1;
                    if self.peek() == Some(b'-') && self.b.get(self.i + 1) == Some(&b'>') {
                        self.i += 2;
                    } else {
                        let start = self.i;
                        let mut val: i64 = 0;
                        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                            val = val * 10 + (self.b[self.i] - b'0') as i64;
                            if val > 1_000_000 {
                                val = 1_000_001;
                            }
                            self.i += 1;
                        }
                        if self.i == start || self.peek() != Some(b'>') {
                            return Err(self.syntax());
                        }
                        self.i += 1;
                        if val > 16384 {
                            return Err(pgi_soft(
                                "22023",
                                "distance in phrase operator must be an integer value between zero and 16384 inclusive".into(),
                            ));
                        }
                    }
                    let _ = save;
                    self.parse_unary()?;
                }
                _ => return Ok(()),
            }
        }
    }

    fn parse_unary(&mut self) -> Result<(), PgInputFailure> {
        self.skip_ws();
        if self.peek() == Some(b'!') {
            self.i += 1;
            return self.parse_unary();
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<(), PgInputFailure> {
        self.skip_ws();
        match self.peek() {
            Some(b'(') => {
                self.i += 1;
                self.parse_or()?;
                self.skip_ws();
                if self.peek() != Some(b')') {
                    return Err(self.syntax());
                }
                self.i += 1;
                Ok(())
            }
            Some(b'\'') => {
                self.i += 1;
                let mut len = 0usize;
                loop {
                    match self.peek() {
                        None => return Err(self.syntax()),
                        Some(b'\\') => {
                            self.i += 2;
                            len += 1;
                        }
                        Some(b'\'') => {
                            if self.b.get(self.i + 1).copied() == Some(b'\'') {
                                self.i += 2;
                                len += 1;
                            } else {
                                self.i += 1;
                                break;
                            }
                        }
                        Some(_) => {
                            self.i += 1;
                            len += 1;
                        }
                    }
                }
                if len == 0 {
                    return Err(self.syntax());
                }
                self.operand_suffix()
            }
            Some(c) if !matches!(c, b')' | b'&' | b'|' | b'!' | b'<' | b':') => {
                let start = self.i;
                while let Some(c) = self.peek() {
                    if (c as char).is_whitespace()
                        || matches!(c, b'(' | b')' | b'&' | b'|' | b'!' | b'<' | b':')
                    {
                        break;
                    }
                    self.i += 1;
                }
                if self.i == start {
                    return Err(self.syntax());
                }
                self.operand_suffix()
            }
            _ => Err(self.syntax()),
        }
    }

    fn operand_suffix(&mut self) -> Result<(), PgInputFailure> {
        if self.peek() == Some(b':') {
            self.i += 1;
            // Optional weights: '*' or letters a-d, comma-separated,
            // possibly with a leading '*'.
            loop {
                match self.peek() {
                    Some(b'*') => self.i += 1,
                    Some(c)
                        if matches!(c, b'A' | b'B' | b'C' | b'D' | b'a' | b'b' | b'c' | b'd') =>
                    {
                        self.i += 1;
                    }
                    _ => return Err(self.syntax()),
                }
                if self.peek() == Some(b',') {
                    self.i += 1;
                    continue;
                }
                break;
            }
        }
        Ok(())
    }
}

pub(crate) fn pg_input_validate_tsquery(input: &str) -> Result<(), PgInputFailure> {
    let mut p = TsqParser {
        b: input.as_bytes(),
        i: 0,
        input,
    };
    p.parse_or()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(p.syntax());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// jsonpath: reuse the crate parser (jsonpath.rs), mapping its errors to
// pg_input_error_info's PG forms (jsonpath_gram.y / jsonpath.c)
// ---------------------------------------------------------------------------

pub(crate) fn pg_input_validate_jsonpath(input: &str) -> Result<(), PgInputFailure> {
    match super::jsonpath::parse_jsonpath(input) {
        Ok(_) => Ok(()),
        Err(e) => {
            // LIKE_REGEX flag validation happens in the grammar with a
            // dedicated detail; reproduce it from the input text since
            // the crate checks flags only at execution time.
            if input.to_ascii_lowercase().contains("like_regex") {
                if let Some(flags) = jsonpath_like_regex_flags(input) {
                    for ch in flags.chars() {
                        if !matches!(ch, 'i' | 's' | 'm' | 'x' | 'q') {
                            return Err(pgi_soft_detail(
                                "42601",
                                "invalid input syntax for type jsonpath".into(),
                                format!(
                                    "Unrecognized flag character \"{ch}\" in LIKE_REGEX predicate."
                                ),
                            ));
                        }
                    }
                }
            }
            if e.message.contains("is not allowed in root expressions") {
                return Err(pgi_soft("42601", e.message.clone()));
            }
            if e.message.contains("trailing junk after numeric literal") {
                return Err(pgi_soft("42601", e.message.clone()));
            }
            // The crate parser reports most syntax failures with one
            // generic message; PG's scanner distinguishes two of them.
            if input.trim_start().starts_with('@') {
                // PG: a bare @ can never start a jsonpath.
                return Err(pgi_soft(
                    "42601",
                    "@ is not allowed in root expressions".into(),
                ));
            }
            if let Some(tok) = jsonpath_bad_numeric_token(input) {
                return Err(pgi_soft(
                    "42601",
                    format!(
                        "trailing junk after numeric literal at or near \"{tok}\" of jsonpath input"
                    ),
                ));
            }
            Err(pgi_soft("42601", e.message.clone()))
        }
    }
}

/// Find a malformed numeric token in a jsonpath string (PG's scanner
/// rejects a digit run with a leading zero, or a number immediately
/// followed by a letter, as "trailing junk after numeric literal").
/// Double-quoted strings are skipped.
fn jsonpath_bad_numeric_token(input: &str) -> Option<String> {
    let b = input.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let intpart = &input[start..i];
                // Leading zero followed by another digit (and no '.'
                // fraction) is junk, e.g. "00".
                if intpart.len() > 1 && intpart.starts_with('0') && b.get(i) != Some(&b'.') {
                    let mut j = i;
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    return Some(input[start..j].to_string());
                }
                // Optional fraction and exponent.
                if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if matches!(b.get(i), Some(b'e') | Some(b'E')) {
                    let mut j = i + 1;
                    if matches!(b.get(j), Some(b'+') | Some(b'-')) {
                        j += 1;
                    }
                    if b.get(j).is_some_and(|c| c.is_ascii_digit()) {
                        i = j;
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                // A letter or underscore hard against the number is junk.
                if b.get(i)
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
                {
                    let mut j = i;
                    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                        j += 1;
                    }
                    return Some(input[start..j].to_string());
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Extract the string literal following `flag` in a like_regex predicate.
fn jsonpath_like_regex_flags(input: &str) -> Option<String> {
    let low = input.to_ascii_lowercase();
    let pos = low.find("like_regex")? + "like_regex".len();
    let rest = &input[pos..];
    let fpos = rest.to_ascii_lowercase().find("flag")? + 4;
    let rest = rest[fpos..].trim_start();
    let inner = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => {
                out.push(chars.next()?);
            }
            c => out.push(c),
        }
    }
}

// ---------------------------------------------------------------------------
// Catalog-aware validation: domains, composites, aclitem, reg* types
// ---------------------------------------------------------------------------

/// A parsed type-name argument (PG `parseTypeString` shape): dotted
/// components with quote-aware folding, an optional typmod in parens,
/// and trailing `[]` array suffixes.
pub(crate) struct TypeNameParts {
    /// Last dotted component (identifier-folded unless quoted).
    pub base: String,
    pub base_quoted: bool,
    /// Second-to-last component when present.
    pub schema: Option<String>,
    pub n_components: usize,
    pub typmod: Option<String>,
    pub array_depth: usize,
}

/// Split a dotted name respecting double quotes; returns the components
/// (folded when unquoted) and their quoted flags.
fn split_dotted(s: &str) -> Vec<(String, bool)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    loop {
        if i >= b.len() {
            break;
        }
        if b[i] == b'"' {
            i += 1;
            let mut name = String::new();
            while i < b.len() {
                if b[i] == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        name.push('"');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    name.push(b[i] as char);
                    i += 1;
                }
            }
            out.push((name, true));
        } else {
            let start = i;
            while i < b.len() && b[i] != b'.' {
                i += 1;
            }
            out.push((s[start..i].trim().to_ascii_lowercase(), false));
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
        } else {
            break;
        }
    }
    out
}

pub(crate) fn parse_type_name(typ: &str) -> TypeNameParts {
    let mut s = typ.trim().to_string();
    let mut array_depth = 0usize;
    loop {
        let t = s.trim_end();
        if t.ends_with(']') {
            if let Some(lb) = t.rfind('[') {
                let inner = &t[lb + 1..t.len() - 1];
                if inner.is_empty() || inner.bytes().all(|c| c.is_ascii_digit()) {
                    array_depth += 1;
                    s = t[..lb].to_string();
                    continue;
                }
            }
        }
        break;
    }
    let mut typmod = None;
    let st = s.trim_end();
    if st.ends_with(')') {
        // Match the final ')' back to its '('.
        let b = st.as_bytes();
        let mut depth = 0i32;
        let mut open = None;
        for idx in (0..b.len()).rev() {
            match b[idx] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        if let Some(o) = open {
            typmod = Some(st[o + 1..st.len() - 1].to_string());
            s = st[..o].to_string();
        }
    }
    let comps = split_dotted(s.trim());
    let (base, base_quoted) = comps
        .last()
        .cloned()
        .unwrap_or_else(|| (String::new(), false));
    let schema = if comps.len() >= 2 {
        Some(comps[comps.len() - 2].0.clone())
    } else {
        None
    };
    TypeNameParts {
        base,
        base_quoted,
        schema,
        n_components: comps.len(),
        typmod,
        array_depth,
    }
}

/// The normalized type string handed to the pure validator: schema
/// prefix dropped, typmod and one array level reattached by callers.
fn normalized_pure_typ(parts: &TypeNameParts) -> String {
    let mut out = if parts.base_quoted && parts.base == "char" {
        "\"char\"".to_string()
    } else {
        parts.base.clone()
    };
    if let Some(tm) = &parts.typmod {
        out.push('(');
        out.push_str(tm);
        out.push(')');
    }
    out
}

fn elem_typ_string(parts: &TypeNameParts) -> String {
    let mut out = normalized_pure_typ(parts);
    for _ in 1..parts.array_depth {
        out.push_str("[]");
    }
    out
}

/// PG type name for a `ColType`, with typmod where the ColType has one.
pub(crate) fn coltype_typ_string(ct: &ColType) -> String {
    match ct {
        ColType::SmallInt => "smallint".into(),
        ColType::Int => "integer".into(),
        ColType::BigInt => "bigint".into(),
        ColType::Float4 => "real".into(),
        ColType::Float => "double precision".into(),
        ColType::Numeric(Some((p, s))) => format!("numeric({p},{s})"),
        ColType::Numeric(None) => "numeric".into(),
        ColType::Text => "text".into(),
        ColType::Char(Some(n)) => format!("character({n})"),
        ColType::Char(None) => "character".into(),
        ColType::Varchar(Some(n)) => format!("character varying({n})"),
        ColType::Varchar(None) => "character varying".into(),
        ColType::SingleChar => "\"char\"".into(),
        ColType::Bool => "boolean".into(),
        ColType::Date => "date".into(),
        ColType::Timestamp => "timestamp".into(),
        ColType::Timestamptz => "timestamptz".into(),
        ColType::Time => "time".into(),
        ColType::TimeTz => "timetz".into(),
        ColType::Interval => "interval".into(),
        ColType::Bytea => "bytea".into(),
        ColType::Bit => "bit".into(),
        ColType::Uuid => "uuid".into(),
        ColType::Regclass => "regclass".into(),
        ColType::Name => "name".into(),
        ColType::PgLsn => "pg_lsn".into(),
        ColType::Xid => "xid".into(),
        ColType::Tid => "tid".into(),
        ColType::Json => "json".into(),
        ColType::Jsonb => "jsonb".into(),
        ColType::Jsonpath => "jsonpath".into(),
        // v1.88: range / multirange type names.
        ColType::Range(_) | ColType::Multirange(_) => ct.sql_name(),
        ColType::Array(e) => format!("{}[]", coltype_typ_string(&elem_coltype(e))),
        ColType::Composite | ColType::Record => "record".into(),
    }
}

fn elem_coltype(e: &ArrayElem) -> ColType {
    super::eval::elem_scalar_type(*e)
}

fn role_exists(q: &Q, name: &str) -> bool {
    name == "postgres" || q.eng.db.roles.get(name).is_some_and(|v| !v.is_empty())
}

/// Domain validation (domains.c domain_in): base input function first
/// (soft), then the CHECK constraints — a violated check is soft 23514,
/// an error raised INSIDE a check expression is a hard error, exactly
/// like PG (ExecCheck is not covered by the save context).
fn validate_domain(
    q: &mut Q,
    fn_name: &str,
    input: &str,
    domname: &str,
    dom: &crate::storage::DomainDef,
) -> Result<(), PgInputFailure> {
    let base_typ = if let Some(inner) = &dom.base_domain {
        inner.clone()
    } else if let Some(named) = &dom.base_named {
        named.clone()
    } else {
        coltype_typ_string(&dom.base)
    };
    pg_input_validate_catalog(q, fn_name, input, &base_typ)?;
    // Build the base value; base-cast errors mirror the base input
    // function, so they stay soft.
    let textval = Value::Text(input.into());
    let coerced = match super::cast::eval_cast(&textval, dom.base) {
        Ok(v) => v,
        Err(e) => {
            return Err(PgInputFailure::Soft(PgInputError {
                message: e.message,
                detail: e.detail,
                hint: None,
                code: e.code,
            }));
        }
    };
    match super::constraint::check_domain_value(q, domname, &coerced, false) {
        Ok(_) => Ok(()),
        Err(e) if e.code == "23514" || e.code == "23502" => {
            Err(PgInputFailure::Soft(PgInputError {
                message: e.message,
                detail: e.detail,
                hint: None,
                code: e.code,
            }))
        }
        Err(e) => Err(PgInputFailure::Hard(e)),
    }
}

/// Composite validation (rowtypes.c record_in): fields are extracted
/// and converted one at a time, in order, exactly like PG.
fn validate_composite(
    q: &mut Q,
    fn_name: &str,
    input: &str,
    fields: &[(String, String)],
) -> Result<(), PgInputFailure> {
    let malformed = |detail: &str| {
        pgi_soft_detail(
            "22P02",
            format!("malformed record literal: \"{input}\""),
            detail.into(),
        )
    };
    let b = input.as_bytes();
    let mut i = 0usize;
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    if b.get(i).copied() != Some(b'(') {
        return Err(malformed("Missing left parenthesis."));
    }
    i += 1;
    for (idx, (_fname, ftyp)) in fields.iter().enumerate() {
        if idx > 0 {
            if b.get(i).copied() == Some(b',') {
                i += 1;
            } else {
                return Err(malformed("Too few columns."));
            }
        }
        let field: Option<String> = match b.get(i).copied() {
            Some(b',') | Some(b')') => None,
            _ => {
                let mut out = String::new();
                let mut inquote = false;
                loop {
                    let ch = match b.get(i).copied() {
                        None => {
                            return Err(malformed("Unexpected end of input."));
                        }
                        Some(c) => c,
                    };
                    if ch == b'\\' {
                        let nxt = match b.get(i + 1).copied() {
                            None => {
                                return Err(malformed("Unexpected end of input."));
                            }
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
                    } else if !inquote && (ch == b',' || ch == b')') {
                        break;
                    } else {
                        out.push(ch as char);
                        i += 1;
                    }
                }
                Some(out)
            }
        };
        if let Some(text) = field {
            pg_input_validate_catalog(q, fn_name, &text, ftyp)?;
        }
    }
    if b.get(i).copied() == Some(b')') {
        i += 1;
    } else {
        return Err(malformed("Too many columns."));
    }
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    if i != b.len() {
        return Err(malformed("Junk after right parenthesis."));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// aclitem (acl.c aclparse / getid)
// ---------------------------------------------------------------------------

fn acl_getid(b: &[u8], mut i: usize) -> Result<(String, usize), PgInputFailure> {
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    let mut name: Vec<u8> = Vec::new();
    let mut in_quotes = false;
    while i < b.len() {
        let c = b[i];
        let safe = c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
        if !(in_quotes || c == b'"' || safe) {
            break;
        }
        if c == b'"' {
            if !in_quotes {
                in_quotes = true;
                i += 1;
                continue;
            }
            if b.get(i + 1).copied() == Some(b'"') {
                name.push(b'"');
                i += 2;
                continue;
            }
            in_quotes = false;
            i += 1;
            continue;
        }
        if name.len() >= 63 {
            return Err(pgi_soft_detail(
                "42622",
                "identifier too long".into(),
                "Identifier must be less than 64 characters.".into(),
            ));
        }
        name.push(c);
        i += 1;
    }
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    Ok((String::from_utf8_lossy(&name).into_owned(), i))
}

pub(crate) fn pg_input_validate_aclitem(q: &mut Q, input: &str) -> Result<(), PgInputFailure> {
    let b = input.as_bytes();
    let soft = |code: &'static str, message: String| pgi_soft(code, message);
    let (mut name, mut i) = acl_getid(b, 0)?;
    if b.get(i).copied() != Some(b'=') {
        if name != "group" && name != "user" {
            return Err(PgInputFailure::Soft(PgInputError {
                message: format!("unrecognized key word: \"{name}\""),
                detail: None,
                hint: Some("ACL key word must be \"group\" or \"user\".".into()),
                code: "22P02",
            }));
        }
        let (n2, ni) = acl_getid(b, i)?;
        name = n2;
        i = ni;
        if name.is_empty() {
            return Err(PgInputFailure::Soft(PgInputError {
                message: "missing name".into(),
                detail: None,
                hint: Some("A name must follow the \"group\" or \"user\" key word.".into()),
                code: "22P02",
            }));
        }
    }
    if b.get(i).copied() != Some(b'=') {
        return Err(soft("22P02", "missing \"=\" sign".into()));
    }
    i += 1;
    // Privilege characters (validated before any role lookup, like PG).
    const ACL_ALL: &[u8] = b"arwdDxtXUCTcsAm";
    while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'*') {
        if b[i] != b'*' && !ACL_ALL.contains(&b[i]) {
            return Err(soft(
                "22P02",
                "invalid mode character: must be one of \"arwdDxtXUCTcsAm\"".into(),
            ));
        }
        i += 1;
    }
    if !name.is_empty() && !role_exists(q, &name) {
        return Err(soft("42704", format!("role \"{name}\" does not exist")));
    }
    if b.get(i).copied() == Some(b'/') {
        let (grantor, ni) = acl_getid(b, i + 1)?;
        i = ni;
        if grantor.is_empty() {
            return Err(soft("22P02", "a name must follow the \"/\" sign".into()));
        }
        if !role_exists(q, &grantor) {
            return Err(soft("42704", format!("role \"{grantor}\" does not exist")));
        }
    }
    if i != b.len() {
        return Err(soft(
            "22P02",
            "extra garbage at the end of the ACL specification".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The reg* family (regproc.c)
// ---------------------------------------------------------------------------

/// Builtin type names that exist in every PG catalog (for regtype and
/// regprocedure argument checks).
fn builtin_type_exists(name: &str) -> bool {
    matches!(
        name,
        "bool"
            | "boolean"
            | "bytea"
            | "char"
            | "name"
            | "int8"
            | "bigint"
            | "int2"
            | "smallint"
            | "int2vector"
            | "int"
            | "int4"
            | "integer"
            | "text"
            | "oid"
            | "tid"
            | "xid"
            | "cid"
            | "oidvector"
            | "json"
            | "xml"
            | "point"
            | "lseg"
            | "path"
            | "box"
            | "polygon"
            | "line"
            | "float4"
            | "real"
            | "float8"
            | "double precision"
            | "circle"
            | "macaddr"
            | "macaddr8"
            | "inet"
            | "cidr"
            | "aclitem"
            | "bpchar"
            | "character"
            | "varchar"
            | "character varying"
            | "date"
            | "time"
            | "time without time zone"
            | "timestamp"
            | "timestamp without time zone"
            | "timestamp with time zone"
            | "timestamptz"
            | "interval"
            | "timetz"
            | "time with time zone"
            | "bit"
            | "bit varying"
            | "varbit"
            | "numeric"
            | "decimal"
            | "refcursor"
            | "regprocedure"
            | "regoper"
            | "regoperator"
            | "regproc"
            | "regtype"
            | "trigger"
            | "record"
            | "cstring"
            | "any"
            | "anyarray"
            | "void"
            | "language_handler"
            | "fdw_handler"
            | "pg_ndistinct"
            | "pg_dependencies"
            | "uuid"
            | "pg_lsn"
            | "tsvector"
            | "tsquery"
            | "gtsvector"
            | "gtsqery"
            | "regconfig"
            | "regdictionary"
            | "jsonb"
            | "jsonpath"
            | "txid_snapshot"
            | "pg_snapshot"
            | "xid8"
            | "oid8"
            | "int4range"
            | "int8range"
            | "numrange"
            | "tsrange"
            | "tstzrange"
            | "daterange"
            | "int4multirange"
            | "int8multirange"
            | "nummultirange"
            | "tsmultirange"
            | "tstzmultirange"
            | "datemultirange"
            | "regclass"
            | "regnamespace"
            | "regrole"
            | "regdatabase"
            | "regcollation"
            | "money"
            | "anyelement"
            | "anynonarray"
            | "anyenum"
            | "internal"
            | "event_trigger"
            | "pg_brin_bloom_summary"
            | "pg_brin_minmax_multi_summary"
            | "pg_mcv_list"
    )
}

/// Common builtin function names (regproc checks existence by name only).
fn builtin_function_exists(name: &str) -> bool {
    matches!(
        name,
        "abs"
            | "now"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "sqrt"
            | "exp"
            | "ln"
            | "log"
            | "power"
            | "floor"
            | "ceil"
            | "ceiling"
            | "round"
            | "trunc"
            | "sign"
            | "degrees"
            | "radians"
            | "pi"
            | "random"
            | "length"
            | "lower"
            | "upper"
            | "substr"
            | "substring"
            | "position"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "btrim"
            | "replace"
            | "concat"
            | "concat_ws"
            | "left"
            | "right"
            | "lpad"
            | "rpad"
            | "repeat"
            | "reverse"
            | "split_part"
            | "to_char"
            | "to_date"
            | "to_timestamp"
            | "to_number"
            | "format"
            | "initcap"
            | "translate"
            | "ascii"
            | "chr"
            | "md5"
            | "encode"
            | "decode"
            | "pg_typeof"
            | "pg_input_is_valid"
            | "pg_input_error_info"
            | "current_date"
            | "current_timestamp"
            | "localtimestamp"
            | "clock_timestamp"
            | "statement_timestamp"
            | "timeofday"
            | "age"
            | "date_trunc"
            | "date_part"
            | "extract"
            | "make_date"
            | "make_time"
            | "make_timestamp"
            | "make_interval"
            | "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "array_agg"
            | "string_agg"
            | "bool_and"
            | "bool_or"
            | "every"
            | "coalesce"
            | "nullif"
            | "greatest"
            | "least"
            | "cast"
            | "row"
            | "array"
            | "generate_series"
            | "generate_subscripts"
            | "unnest"
            | "json_build_object"
            | "json_build_array"
            | "to_json"
            | "to_jsonb"
            | "row_to_json"
            | "jsonb_build_object"
            | "jsonb_build_array"
            | "version"
            | "current_database"
            | "current_schema"
            | "current_user"
            | "session_user"
            | "pg_backend_pid"
            | "pg_postmaster_start_time"
            | "has_table_privilege"
            | "obj_description"
            | "col_description"
            | "pg_table_size"
            | "pg_relation_size"
            | "nextval"
            | "currval"
            | "setval"
            | "lastval"
            | "width_bucket"
            | "div"
            | "mod"
            | "gcd"
            | "lcm"
            | "sinh"
            | "cosh"
            | "tanh"
            | "cbrt"
            | "factorial"
    )
}

/// Operators that exist with more than one signature in the PG catalog
/// (regoper reports ambiguity for these).
fn multi_operator(name: &str) -> bool {
    matches!(
        name,
        "+" | "-"
            | "*"
            | "/"
            | "%"
            | "^"
            | "<"
            | ">"
            | "="
            | "<="
            | ">="
            | "<>"
            | "!="
            | "||"
            | "@>"
            | "<@"
            | "&&"
            | "<<"
            | ">>"
            | "&"
            | "|"
            | "#"
            | "~"
            | "!~"
            | "~*"
            | "!~*"
            | "<->"
            | "@"
            | "?"
            | "?|"
            | "?&"
            | "@?"
            | "@@"
    )
}

fn type_exists(q: &Q, name: &str) -> bool {
    builtin_type_exists(name)
        || q.eng.db.types.contains_key(name)
        || q.eng
            .db
            .find_table(name, q.snap, &q.all_xids, q.session)
            .is_some()
}

fn relation_exists(q: &Q, name: &str) -> bool {
    q.eng
        .db
        .find_table(name, q.snap, &q.all_xids, q.session)
        .is_some()
        || q.eng.db.views.contains_key(name)
        || q.eng.db.sequences.contains_key(name)
        || q.eng.db.types.contains_key(name)
}

/// Split a `name(args)` regprocedure/regoperator specification.
fn split_reg_sig(input: &str) -> Result<(String, Option<Vec<String>>), PgInputFailure> {
    let t = input.trim();
    match t.find('(') {
        None => Err(pgi_soft("22P02", "expected a left parenthesis".into())),
        Some(open) => {
            if !t.ends_with(')') {
                return Err(pgi_soft("22P02", "expected a right parenthesis".into()));
            }
            let args_str = &t[open + 1..t.len() - 1];
            let args = if args_str.trim().is_empty() {
                Vec::new()
            } else {
                // Split on top-level commas (typmods contain commas).
                let mut out = Vec::new();
                let mut depth = 0i32;
                let mut cur = String::new();
                for c in args_str.chars() {
                    match c {
                        '(' => {
                            depth += 1;
                            cur.push(c);
                        }
                        ')' => {
                            depth -= 1;
                            cur.push(c);
                        }
                        ',' if depth == 0 => {
                            out.push(cur.trim().to_string());
                            cur = String::new();
                        }
                        _ => cur.push(c),
                    }
                }
                out.push(cur.trim().to_string());
                out
            };
            Ok((t[..open].trim().to_string(), Some(args)))
        }
    }
}

fn reg_validate(q: &mut Q, input: &str, base: &str) -> Result<(), PgInputFailure> {
    let dne = |msg: String, code: &'static str| pgi_soft(code, msg);
    match base {
        "regclass" => {
            let parts = parse_type_name(input);
            let schema_ok = matches!(
                parts.schema.as_deref(),
                None | Some("pg_catalog") | Some("public")
            );
            if schema_ok && relation_exists(q, &parts.base) {
                Ok(())
            } else {
                Err(dne(format!("relation \"{input}\" does not exist"), "42P01"))
            }
        }
        "regnamespace" => {
            let parts = parse_type_name(input);
            if matches!(
                parts.base.as_str(),
                "pg_catalog" | "public" | "information_schema" | "pg_toast"
            ) {
                Ok(())
            } else {
                Err(dne(
                    format!("schema \"{}\" does not exist", parts.base),
                    "3F000",
                ))
            }
        }
        "regrole" => {
            let parts = parse_type_name(input);
            if role_exists(q, &parts.base) {
                Ok(())
            } else {
                Err(dne(
                    format!("role \"{}\" does not exist", parts.base),
                    "42704",
                ))
            }
        }
        "regdatabase" => {
            let parts = parse_type_name(input);
            if parts.base == "postgres" {
                Ok(())
            } else {
                Err(dne(
                    format!("database \"{}\" does not exist", parts.base),
                    "42704",
                ))
            }
        }
        "regconfig" => Err(dne(
            format!(
                "text search configuration \"{}\" does not exist",
                parse_type_name(input).base
            ),
            "42704",
        )),
        "regdictionary" => Err(dne(
            format!(
                "text search dictionary \"{}\" does not exist",
                parse_type_name(input).base
            ),
            "42704",
        )),
        "regcollation" => Err(dne(
            format!(
                "collation \"{}\" does not exist",
                parse_type_name(input).base
            ),
            "42704",
        )),
        "regproc" => {
            let parts = parse_type_name(input);
            let schema_ok = matches!(parts.schema.as_deref(), None | Some("pg_catalog"));
            if schema_ok && builtin_function_exists(&parts.base) {
                Ok(())
            } else {
                Err(dne(format!("function \"{input}\" does not exist"), "42883"))
            }
        }
        "regprocedure" => {
            let (name, args) = split_reg_sig(input)?;
            let parts = parse_type_name(&name);
            let schema_ok = matches!(parts.schema.as_deref(), None | Some("pg_catalog"));
            let args_ok = args
                .unwrap_or_default()
                .iter()
                .all(|a| type_exists(q, &parse_type_name(a).base));
            if schema_ok && args_ok && builtin_function_exists(&parts.base) {
                Ok(())
            } else {
                Err(dne(format!("function \"{input}\" does not exist"), "42883"))
            }
        }
        "regoper" => {
            let parts = parse_type_name(input);
            let schema_ok = matches!(parts.schema.as_deref(), None | Some("pg_catalog"));
            if !schema_ok {
                return Err(dne(format!("operator does not exist: {input}"), "42883"));
            }
            if multi_operator(&parts.base) {
                Err(pgi_soft(
                    "42725",
                    format!("more than one operator named {}", parts.base),
                ))
            } else if is_known_single_operator(&parts.base) {
                Ok(())
            } else {
                Err(dne(format!("operator does not exist: {input}"), "42883"))
            }
        }
        "regoperator" => {
            let (name, args) = split_reg_sig(input)?;
            let parts = parse_type_name(&name);
            let schema_ok = matches!(parts.schema.as_deref(), None | Some("pg_catalog"));
            let args_ok = args
                .unwrap_or_default()
                .iter()
                .all(|a| type_exists(q, &parse_type_name(a).base));
            let known = multi_operator(&parts.base) || is_known_single_operator(&parts.base);
            if schema_ok && args_ok && known {
                Ok(())
            } else {
                Err(dne(format!("operator does not exist: {input}"), "42883"))
            }
        }
        "regtype" => reg_validate_regtype(q, input),
        _ => Err(dne(format!("type \"{input}\" does not exist"), "42704")),
    }
}

fn is_known_single_operator(name: &str) -> bool {
    // Operators with exactly one catalog signature.
    matches!(
        name,
        "&&" | "->"
            | "->>"
            | "#>"
            | "#>>"
            | "@@@"
            | "!!!"
            | "~="
            | "==="
            | ">>="
            | "<<="
            | "~=="
            | "=="
            | "^@"
            | "@-@"
            | "@@-@"
            | "?-"
            | "?-|"
    ) || matches!(name, "@>" | "<@" | "~=" | "->" | "->>")
}

/// regtype goes through `parseTypeString`, whose failures are hard
/// errors (outside the soft context), while a well-formed but unknown
/// name is a soft 42704.
fn reg_validate_regtype(q: &mut Q, input: &str) -> Result<(), PgInputFailure> {
    let parts = parse_type_name(input);
    if parts.n_components >= 4 {
        return Err(PgInputFailure::Hard(exec_err(
            "42601",
            format!("improper qualified name (too many dotted names): {input}"),
        )));
    }
    if parts.n_components == 3 {
        return Err(PgInputFailure::Hard(exec_err(
            "42601",
            format!("cross-database references are not implemented: {input}"),
        )));
    }
    // Identifier syntax of the name components: PG's type-name grammar
    // rejects anything that is not IDENT (. IDENT)* [typmod] [array].
    let name_part = {
        let t = input.trim();
        let t = match t.find('(') {
            Some(p) => &t[..p],
            None => t,
        };
        t.trim_end().trim_end_matches("[]").trim_end()
    };
    for comp in name_part.split('.') {
        let c = comp.trim();
        if c.starts_with('"') {
            continue;
        }
        let valid = !c.is_empty()
            && c.chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_' || (ch as u32) >= 0x80)
            && c.chars().all(|ch| {
                ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || (ch as u32) >= 0x80
            });
        if !valid {
            let word = c.split_whitespace().nth(1).unwrap_or(c).trim_matches('"');
            return Err(PgInputFailure::Hard(exec_err(
                "42601",
                format!("syntax error at or near \"{word}\""),
            )));
        }
    }
    // Typmod sanity for numeric (the corpus bogus case): 3+ arguments
    // or a non-integer argument are a hard parseTypeString failure.
    if matches!(parts.base.as_str(), "numeric" | "decimal") {
        if let Some(tm) = &parts.typmod {
            let args: Vec<&str> = tm.split(',').collect();
            let ok = args.len() <= 2 && args.iter().all(|a| a.trim().parse::<i64>().is_ok());
            if !ok {
                return Err(PgInputFailure::Hard(exec_err(
                    "22023",
                    "invalid NUMERIC type modifier",
                )));
            }
        }
    }
    if type_exists(q, &parts.base) {
        Ok(())
    } else {
        Err(pgi_soft(
            "42704",
            format!("type \"{}\" does not exist", parts.base),
        ))
    }
}

// ---------------------------------------------------------------------------
// The catalog entry point
// ---------------------------------------------------------------------------

fn validate_array_catalog(
    q: &mut Q,
    fn_name: &str,
    input: &str,
    elem_typ: &str,
) -> Result<(), PgInputFailure> {
    let node = arr_parse_tree(input)?;
    arr_check_rect(&node)?;
    validate_array_leaves(q, fn_name, &node, elem_typ)
}

fn validate_array_leaves(
    q: &mut Q,
    fn_name: &str,
    node: &ArrNode,
    elem_typ: &str,
) -> Result<(), PgInputFailure> {
    match node {
        ArrNode::Leaf(None) => Ok(()),
        ArrNode::Leaf(Some(v)) => pg_input_validate_catalog(q, fn_name, v, elem_typ),
        ArrNode::List(items) => {
            for it in items {
                validate_array_leaves(q, fn_name, it, elem_typ)?;
            }
            Ok(())
        }
    }
}

/// Catalog-aware `pg_input_validate`: resolves user-defined types
/// (domains, composites, table row types), aclitem, and the reg* family
/// against the live catalog, delegating everything else to the pure
/// validator in cast.rs.
pub(crate) fn pg_input_validate_catalog(
    q: &mut Q,
    fn_name: &str,
    input: &str,
    typ: &str,
) -> Result<(), PgInputFailure> {
    let parts = parse_type_name(typ);
    if parts.array_depth > 0 {
        let elem = elem_typ_string(&parts);
        return validate_array_catalog(q, fn_name, input, &elem);
    }
    let base = parts.base.clone();
    // User-defined types from the catalog.
    let shell = q.eng.db.types.get(&base).cloned();
    if let Some(st) = &shell {
        if let Some(dom) = &st.domain {
            return validate_domain(q, fn_name, input, &base, dom);
        }
        if let Some(fields) = &st.composite {
            let ft: Vec<(String, String)> = fields
                .iter()
                .map(|(n, ct, nested)| {
                    (
                        n.clone(),
                        nested.clone().unwrap_or_else(|| coltype_typ_string(ct)),
                    )
                })
                .collect();
            return validate_composite(q, fn_name, input, &ft);
        }
    }
    // Table row types.
    if let Some(t) = q
        .eng
        .db
        .find_table(&base, q.snap, &q.all_xids, q.session)
        .cloned()
    {
        let ft: Vec<(String, String)> = t
            .columns
            .iter()
            .enumerate()
            .map(|(idx, (n, ct))| {
                let ts = t
                    .domain_types
                    .get(idx)
                    .cloned()
                    .flatten()
                    .or_else(|| t.composite_types.get(idx).cloned().flatten())
                    .unwrap_or_else(|| coltype_typ_string(ct));
                (n.clone(), ts)
            })
            .collect();
        return validate_composite(q, fn_name, input, &ft);
    }
    if let Some(st) = &shell {
        if let Some(like) = &st.like_base {
            let like = like.clone();
            return pg_input_validate_catalog(q, fn_name, input, &like);
        }
    }
    match base.as_str() {
        "aclitem" => return pg_input_validate_aclitem(q, input),
        "regclass" | "regcollation" | "regconfig" | "regdatabase" | "regdictionary"
        | "regnamespace" | "regoper" | "regoperator" | "regproc" | "regprocedure" | "regrole"
        | "regtype" => return reg_validate(q, input, &base),
        _ => {}
    }
    super::cast::pg_input_validate(fn_name, input, &normalized_pure_typ(&parts))
}

/// Catalog-aware `pg_input_error_info` (the FROM-clause table function):
/// same row contract as the pure arm in funcs.rs, but user-defined
/// types resolve against the live catalog. NULL arguments yield no
/// rows; a valid input yields one row of NULLs; a soft input error
/// yields the (message, detail, hint, sql_error_code) row.
pub(crate) fn pg_input_error_info_catalog(
    q: &mut Q,
    vals: &[Value],
) -> Result<(Vec<String>, Vec<Vec<Value>>), ExecError> {
    let cols = vec![
        "message".to_string(),
        "detail".to_string(),
        "hint".to_string(),
        "sql_error_code".to_string(),
    ];
    let input = match str_arg("pg_input_error_info", &vals[0])? {
        None => return Ok((cols, Vec::new())),
        Some(s) => s.to_string(),
    };
    let typ = match str_arg("pg_input_error_info", &vals[1])? {
        None => return Ok((cols, Vec::new())),
        Some(s) => s.to_string(),
    };
    let row = match pg_input_validate_catalog(q, "pg_input_error_info", &input, &typ) {
        Ok(()) => vec![Value::Null, Value::Null, Value::Null, Value::Null],
        Err(PgInputFailure::Soft(e)) => vec![
            Value::text(e.message),
            e.detail.map(Value::text).unwrap_or(Value::Null),
            e.hint.map(Value::text).unwrap_or(Value::Null),
            Value::text(e.code),
        ],
        Err(PgInputFailure::Hard(e)) => return Err(e),
    };
    Ok((cols, vec![row]))
}
