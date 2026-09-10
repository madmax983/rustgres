//! MVCC storage for rustgres (v0.5).
//!
//! Every row version carries `xmin`/`xmax` transaction ids. A version is
//! visible to a snapshot iff its creator committed before the snapshot was
//! taken and no visible deleter has removed it:
//!
//! ```text
//! visible(v, snap, own) =
//!     (v.xmin == own || (v.xmin < snap.next_xid && v.xmin ∉ snap.active))
//!     && (v.xmax == 0 ||
//!         !(v.xmax == own || (v.xmax < snap.next_xid && v.xmax ∉ snap.active)))
//! ```
//!
//! This works because aborted transactions physically undo their writes
//! (see `WriteOp`): any xid below `next_xid` that is not in any active set
//! is therefore committed. Table existence is versioned the same way
//! (`created_xmin`/`dropped_xmax`), so DDL stays transactional without a
//! separate overlay.
//!
//! Xids come from a global atomic counter (`TxnManager::next_xid`, first
//! xid is 1). Row versions additionally carry a globally unique `id`, used
//! by the WAL to name deleted versions and by undo/vacuum to find them.

use std::collections::{HashMap, HashSet};

use crate::index::{Index, IndexDef, IndexKey};
use crate::sql::{CheckDef, DefaultExpr, FkDef, TableDef, UniqueDef};

/// Column data types supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColType {
    Int,      // INT4, OID 23
    BigInt,   // INT8, OID 20 (v0.7)
    SmallInt, // INT2, OID 21 (v0.7)
    Float,    // FLOAT8, OID 701
    Float4,   // FLOAT4, OID 700 (v0.7)
    Numeric,  // NUMERIC, OID 1700 (v0.7)
    Text,     // OID 25
    Bool,     // OID 16
    Date,     // OID 1082 (v0.7)
    Timestamp,   // OID 1114 (v0.7)
    Timestamptz, // OID 1184 (v0.7)
    Bytea,    // OID 17 (v0.7)
    Uuid,     // OID 2950 (v0.7)
}

impl ColType {
    /// PostgreSQL type OID used in RowDescription.
    pub fn oid(&self) -> i32 {
        match self {
            ColType::Int => 23,         // INT4
            ColType::BigInt => 20,      // INT8
            ColType::SmallInt => 21,    // INT2
            ColType::Text => 25,        // TEXT
            ColType::Bool => 16,        // BOOL
            ColType::Float => 701,      // FLOAT8
            ColType::Float4 => 700,     // FLOAT4
            ColType::Numeric => 1700,   // NUMERIC
            ColType::Date => 1082,      // DATE
            ColType::Timestamp => 1114, // TIMESTAMP
            ColType::Timestamptz => 1184, // TIMESTAMPTZ
            ColType::Bytea => 17,       // BYTEA
            ColType::Uuid => 2950,      // UUID
        }
    }

    pub fn sql_name(&self) -> &'static str {
        match self {
            ColType::Int => "integer",
            ColType::BigInt => "bigint",
            ColType::SmallInt => "smallint",
            ColType::Float => "double precision",
            ColType::Float4 => "real",
            ColType::Numeric => "numeric",
            ColType::Text => "text",
            ColType::Bool => "boolean",
            ColType::Date => "date",
            ColType::Timestamp => "timestamp without time zone",
            ColType::Timestamptz => "timestamp with time zone",
            ColType::Bytea => "bytea",
            ColType::Uuid => "uuid",
        }
    }
}

/// Fixed-precision decimal (v0.7): value = `unscaled * 10^-scale`.
///
/// Always kept normalized (no trailing decimal zeros unless the value is
/// zero, which is `0/10^0`), so derived `PartialEq`/`Eq` compare
/// numerically. The `i128` mantissa holds ~38 significant digits;
/// operations that would exceed it fail with 22003 rather than silently
/// rounding — a deliberate v0.7 deviation from Postgres' arbitrary
/// precision (documented in the README).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Numeric {
    pub unscaled: i128,
    pub scale: u32,
}

impl Numeric {
    /// Build and normalize.
    pub fn new(unscaled: i128, scale: u32) -> Self {
        let mut n = Numeric { unscaled, scale };
        n.normalize();
        n
    }

    fn normalize(&mut self) {
        if self.unscaled == 0 {
            self.scale = 0;
            return;
        }
        while self.scale > 0 && self.unscaled % 10 == 0 {
            self.unscaled /= 10;
            self.scale -= 1;
        }
    }

    pub fn zero() -> Self {
        Numeric {
            unscaled: 0,
            scale: 0,
        }
    }

    pub fn from_i64(i: i64) -> Self {
        Numeric::new(i as i128, 0)
    }

    /// Parse a decimal literal: `[+-]digits[.digits][e[+-]digits]`.
    /// `Err(())` = not a number (caller maps to 22P02).
    /// `Err` with overflow is impossible here only if digits fit; too
    /// many digits yield `None` via the overflow flag (caller: 22003).
    pub fn parse(s: &str) -> Result<Self, NumericParseError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(NumericParseError::Syntax);
        }
        let (neg, rest) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        // Split off an exponent.
        let (mant, exp): (&str, i32) = match rest.find(['e', 'E']) {
            Some(i) => {
                let e: i32 = rest[i + 1..]
                    .parse()
                    .map_err(|_| NumericParseError::Syntax)?;
                (&rest[..i], e)
            }
            None => (rest, 0),
        };
        let (int_part, frac_part) = match mant.find('.') {
            Some(i) => (&mant[..i], &mant[i + 1..]),
            None => (mant, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(NumericParseError::Syntax);
        }
        for c in int_part.chars().chain(frac_part.chars()) {
            if !c.is_ascii_digit() {
                return Err(NumericParseError::Syntax);
            }
        }
        // Assemble the unscaled integer from all digits.
        let mut unscaled: i128 = 0;
        for c in int_part.chars().chain(frac_part.chars()) {
            let d = (c as i128) - ('0' as i128);
            unscaled = unscaled
                .checked_mul(10)
                .and_then(|v| v.checked_add(d))
                .ok_or(NumericParseError::Overflow)?;
        }
        if neg {
            unscaled = -unscaled;
        }
        // scale = frac digits - exponent.
        let scale = frac_part.len() as i32 - exp;
        if scale < 0 {
            let extra = (-scale) as u32;
            unscaled = unscaled
                .checked_mul(10i128.checked_pow(extra).ok_or(NumericParseError::Overflow)?)
                .ok_or(NumericParseError::Overflow)?;
            Ok(Numeric::new(unscaled, 0))
        } else {
            Ok(Numeric::new(unscaled, scale as u32))
        }
    }

    /// Build from an f64's shortest round-trip representation, so
    /// `Numeric::from_f64(0.1)` is exactly 0.1. Non-finite and extreme
    /// magnitudes fail.
    pub fn from_f64(f: f64) -> Result<Self, NumericParseError> {
        if !f.is_finite() {
            return Err(NumericParseError::Syntax);
        }
        // Shortest round-trip text, then decimal parse. Rust's Display
        // for f64 already gives the shortest such string.
        let s = if f.abs() < 1e-6 || f.abs() >= 1e21 {
            format!("{:e}", f)
        } else {
            format!("{}", f)
        };
        Numeric::parse(&s)
    }

    pub fn to_f64(&self) -> f64 {
        self.unscaled as f64 * 10f64.powi(-(self.scale as i32))
    }

    /// Round half away from zero to an integer; overflow -> None.
    pub fn to_i64(&self) -> Option<i64> {
        let half = 10i128.checked_pow(self.scale)? / 2;
        let rounded = if self.unscaled >= 0 {
            self.unscaled.checked_add(half)? / 10i128.checked_pow(self.scale)?
        } else {
            self.unscaled.checked_sub(half)? / 10i128.checked_pow(self.scale)?
        };
        i64::try_from(rounded).ok()
    }

    /// Align `other` to our scale; used by add/sub. Overflow -> None.
    fn aligned(&self, other: &Numeric) -> Option<(i128, i128, u32)> {
        let scale = self.scale.max(other.scale);
        let a = self
            .unscaled
            .checked_mul(10i128.checked_pow(scale - self.scale)?)?;
        let b = other
            .unscaled
            .checked_mul(10i128.checked_pow(scale - other.scale)?)?;
        Some((a, b, scale))
    }

    pub fn checked_add(&self, other: &Numeric) -> Option<Numeric> {
        let (a, b, scale) = self.aligned(other)?;
        Some(Numeric::new(a.checked_add(b)?, scale))
    }

    pub fn checked_sub(&self, other: &Numeric) -> Option<Numeric> {
        let (a, b, scale) = self.aligned(other)?;
        Some(Numeric::new(a.checked_sub(b)?, scale))
    }

    pub fn checked_mul(&self, other: &Numeric) -> Option<Numeric> {
        Some(Numeric::new(
            self.unscaled.checked_mul(other.unscaled)?,
            self.scale.checked_add(other.scale)?,
        ))
    }

    /// Division with 10 guard digits after the decimal point
    /// (documented v0.7 fixed scale). Division by zero -> None with the
    /// `is_zero` flag distinguishable by the caller via `other.is_zero()`.
    pub fn checked_div(&self, other: &Numeric) -> Option<Numeric> {
        if other.unscaled == 0 {
            return None;
        }
        // a/b = (ua * 10^(sb+10)) / (ub * 10^sa), scale 10.
        let num = self
            .unscaled
            .checked_mul(10i128.checked_pow(other.scale + 10)?)?;
        let den = other
            .unscaled
            .checked_mul(10i128.checked_pow(self.scale)?)?;
        Some(Numeric::new(num.checked_div(den)?, 10))
    }

    /// Remainder, sign follows the dividend (like Rust's `%` and PG's
    /// numeric mod). Division by zero -> None.
    pub fn checked_rem(&self, other: &Numeric) -> Option<Numeric> {
        if other.unscaled == 0 {
            return None;
        }
        let (a, b, scale) = self.aligned(other)?;
        Some(Numeric::new(a.checked_rem(b)?, scale))
    }

    pub fn abs(&self) -> Numeric {
        Numeric::new(self.unscaled.abs(), self.scale)
    }

    /// Round to `scale` fractional digits, half away from zero.
    pub fn round_to(&self, scale: u32) -> Option<Numeric> {
        if scale >= self.scale {
            let mul = 10i128.checked_pow(scale - self.scale)?;
            return Some(Numeric::new(self.unscaled.checked_mul(mul)?, scale));
        }
        let drop = self.scale - scale;
        let div = 10i128.checked_pow(drop)?;
        let half = div / 2;
        let adj = if self.unscaled >= 0 { half } else { -half };
        Some(Numeric::new(
            self.unscaled.checked_add(adj)?.checked_div(div)?,
            scale,
        ))
    }

    pub fn floor(&self) -> Option<Numeric> {
        let div = 10i128.checked_pow(self.scale)?;
        let q = self.unscaled.checked_div(div)?;
        let r = self.unscaled.checked_rem(div)?;
        let q = if r != 0 && self.unscaled < 0 { q - 1 } else { q };
        Some(Numeric::new(q, 0))
    }

    pub fn ceil(&self) -> Option<Numeric> {
        let div = 10i128.checked_pow(self.scale)?;
        let q = self.unscaled.checked_div(div)?;
        let r = self.unscaled.checked_rem(div)?;
        let q = if r != 0 && self.unscaled > 0 { q + 1 } else { q };
        Some(Numeric::new(q, 0))
    }

    /// Square root via f64 (documented precision limit: ~15-16
    /// significant digits). Negative -> None.
    pub fn sqrt(&self) -> Option<Numeric> {
        let f = self.to_f64();
        if f < 0.0 {
            return None;
        }
        Numeric::from_f64(f.sqrt()).ok()
    }

    /// Exact power for integer exponents (repeated squaring). `None`
    /// on overflow or absurd exponents (callers fall back to f64 or
    /// raise 22003). Negative exponents divide, with 10 guard digits.
    pub fn pow(&self, exp: i64) -> Option<Numeric> {
        if exp == 0 {
            return Some(Numeric::new(1, 0));
        }
        if exp.unsigned_abs() > 10_000 {
            return None;
        }
        let neg = exp < 0;
        let mut e = exp.unsigned_abs();
        let mut base = self.clone();
        let mut acc = Numeric::new(1, 0);
        while e > 0 {
            if e & 1 == 1 {
                acc = acc.checked_mul(&base)?;
            }
            e >>= 1;
            if e > 0 {
                base = base.checked_mul(&base)?;
            }
        }
        if neg {
            if acc.is_zero() {
                return None;
            }
            Numeric::new(1, 0).checked_div(&acc)
        } else {
            Some(acc)
        }
    }

    pub fn is_zero(&self) -> bool {
        self.unscaled == 0
    }

    pub fn cmp(&self, other: &Numeric) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if self.unscaled == 0 && other.unscaled == 0 {
            return Ordering::Equal;
        }
        let neg_a = self.unscaled < 0;
        let neg_b = other.unscaled < 0;
        if neg_a != neg_b {
            return if neg_a { Ordering::Less } else { Ordering::Greater };
        }
        // Compare by magnitude: digits(unscaled) - scale.
        let mag = |n: &Numeric| -> i64 {
            let mut v = n.unscaled.unsigned_abs();
            let mut digits: i64 = 0;
            while v >= 10 {
                v /= 10;
                digits += 1;
            }
            digits - n.scale as i64
        };
        let (ma, mb) = (mag(self), mag(other));
        if ma != mb {
            return if neg_a { mb.cmp(&ma) } else { ma.cmp(&mb) };
        }
        // Same magnitude: align scales; on overflow fall back to f64.
        let ord = match self.aligned(other) {
            Some((a, b, _)) => a.cmp(&b),
            None => self
                .to_f64()
                .partial_cmp(&other.to_f64())
                .unwrap_or(Ordering::Equal),
        };
        ord
    }

    /// Canonical decimal text: no trailing fractional zeros.
    pub fn to_text(&self) -> String {
        if self.unscaled == 0 {
            return "0".to_string();
        }
        let neg = self.unscaled < 0;
        let digits = self.unscaled.unsigned_abs().to_string();
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        if self.scale == 0 {
            out.push_str(&digits);
        } else if digits.len() > self.scale as usize {
            let at = digits.len() - self.scale as usize;
            out.push_str(&digits[..at]);
            out.push('.');
            out.push_str(&digits[at..]);
        } else {
            out.push_str("0.");
            for _ in 0..(self.scale as usize - digits.len()) {
                out.push('0');
            }
            out.push_str(&digits);
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericParseError {
    Syntax,
    Overflow,
}

impl PartialOrd for Numeric {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(Numeric::cmp(self, other))
    }
}

impl Ord for Numeric {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        Numeric::cmp(self, other)
    }
}

/// A single cell value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    SmallInt(i16),   // v0.7: INT2
    Int(i64),        // INT4 (kept as i64, like v0.1-v0.6)
    BigInt(i64),     // v0.7: INT8
    Float4(f32),     // v0.7: REAL
    Float(f64),      // FLOAT8
    Numeric(Numeric),// v0.7: NUMERIC
    Text(String),
    Bool(bool),
    Date(i32),       // v0.7: days since 1970-01-01
    Timestamp(i64),  // v0.7: micros since 1970-01-01 00:00:00 UTC
    Timestamptz(i64),// v0.7: micros since epoch, UTC
    Bytea(Vec<u8>),  // v0.7
    Uuid([u8; 16]),  // v0.7
    Null,
}

impl Value {
    /// Text-format encoding for the wire protocol (`None` = NULL).
    /// Matches what psql prints: ints/floats via Display, bools as t/f,
    /// bytea as `\x` hex, timestamps as ISO.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::SmallInt(i) => Some(i.to_string()),
            Value::Int(i) => Some(i.to_string()),
            Value::BigInt(i) => Some(i.to_string()),
            Value::Float4(f) => Some(float4_text(*f)),
            Value::Float(f) => Some(float_text(*f)),
            Value::Numeric(n) => Some(n.to_text()),
            Value::Text(s) => Some(s.clone()),
            Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
            Value::Date(d) => Some(crate::datetime::format_date(*d)),
            Value::Timestamp(m) => Some(crate::datetime::format_timestamp(*m)),
            Value::Timestamptz(m) => Some(crate::datetime::format_timestamptz(*m)),
            Value::Bytea(b) => Some(bytea_text(b)),
            Value::Uuid(u) => Some(uuid_text(u)),
            Value::Null => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::SmallInt(_) => "smallint",
            Value::Int(_) => "integer",
            Value::BigInt(_) => "bigint",
            Value::Float4(_) => "real",
            Value::Float(_) => "double precision",
            Value::Numeric(_) => "numeric",
            Value::Text(_) => "text",
            Value::Bool(_) => "boolean",
            Value::Date(_) => "date",
            Value::Timestamp(_) => "timestamp without time zone",
            Value::Timestamptz(_) => "timestamp with time zone",
            Value::Bytea(_) => "bytea",
            Value::Uuid(_) => "uuid",
            Value::Null => "unknown",
        }
    }

    /// The column type this value reports as in RowDescription.
    pub fn col_type(&self) -> ColType {
        match self {
            Value::SmallInt(_) => ColType::SmallInt,
            Value::Int(_) => ColType::Int,
            Value::BigInt(_) => ColType::BigInt,
            Value::Float4(_) => ColType::Float4,
            Value::Float(_) => ColType::Float,
            Value::Numeric(_) => ColType::Numeric,
            Value::Text(_) => ColType::Text,
            Value::Bool(_) => ColType::Bool,
            Value::Date(_) => ColType::Date,
            Value::Timestamp(_) => ColType::Timestamp,
            Value::Timestamptz(_) => ColType::Timestamptz,
            Value::Bytea(_) => ColType::Bytea,
            Value::Uuid(_) => ColType::Uuid,
            Value::Null => ColType::Text,
        }
    }
}

/// `\x` + lowercase hex, like Postgres' hex-format bytea output.
fn bytea_text(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + b.len() * 2);
    out.push_str("\\x");
    for &byte in b {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Canonical `8-4-4-4-12` lowercase hex.
fn uuid_text(u: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(36);
    for (i, &byte) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Parse `\xdeadbeef` (or a bare even-length hex string) into bytes.
/// `Err(())` = malformed (caller maps to 22P02).
pub fn parse_bytea(s: &str) -> Result<Vec<u8>, ()> {
    // Hex format: `\x` followed by an even number of hex digits.
    if let Some(hex) = s.strip_prefix("\\x").or_else(|| s.strip_prefix("\\X")) {
        if hex.len() % 2 != 0 {
            return Err(());
        }
        let mut out = Vec::with_capacity(hex.len() / 2);
        let bytes = hex.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let hi = (bytes[i] as char).to_digit(16).ok_or(())?;
            let lo = (bytes[i + 1] as char).to_digit(16).ok_or(())?;
            out.push((hi * 16 + lo) as u8);
            i += 2;
        }
        return Ok(out);
    }
    // Escape format: literal bytes, with `\\` for a backslash and
    // `\ooo` (three octal digits) for an arbitrary byte — like Postgres.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 1;
            if i < bytes.len() && bytes[i] == b'\\' {
                out.push(b'\\');
                i += 1;
            } else if i + 2 < bytes.len()
                && (bytes[i] as char).is_digit(8)
                && (bytes[i + 1] as char).is_digit(8)
                && (bytes[i + 2] as char).is_digit(8)
            {
                let v = (bytes[i] - b'0') as u16 * 64
                    + (bytes[i + 1] - b'0') as u16 * 8
                    + (bytes[i + 2] - b'0') as u16;
                if v > 255 {
                    return Err(());
                }
                out.push(v as u8);
                i += 3;
            } else {
                return Err(());
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Parse a UUID in canonical `8-4-4-4-12` form (also accepts 32 bare hex
/// digits). `Err(())` = malformed (caller maps to 22P02).
pub fn parse_uuid(s: &str) -> Result<[u8; 16], ()> {
    let hex: String = s.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(());
    }
    // Dashes, when present, must be in the 8-4-4-4-12 positions.
    if s.contains('-') {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 5
            || parts[0].len() != 8
            || parts[1].len() != 4
            || parts[2].len() != 4
            || parts[3].len() != 4
            || parts[4].len() != 12
        {
            return Err(());
        }
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; 16];
    for i in 0..16 {
        let hi = (bytes[2 * i] as char).to_digit(16).ok_or(())?;
        let lo = (bytes[2 * i + 1] as char).to_digit(16).ok_or(())?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Ok(out)
}

/// Shortest round-trip float rendering, Postgres-style.
fn float_text(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    // `{}` on f64 already prints the shortest string that round-trips.
    format!("{}", f)
}

/// Same for f32 (real): Rust's Display already prints the shortest
/// string that round-trips at f32 precision, so `1.1::real` prints
/// `1.1` rather than `1.100000023841858`.
fn float4_text(f: f32) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    format!("{}", f)
}

/// One version of one row. UPDATE = mark the old version's `xmax` and
/// append a new version; DELETE = mark `xmax`.
#[derive(Clone, Debug)]
pub struct RowVersion {
    /// Globally unique, never reused (survives crashes via the WAL).
    pub id: u64,
    pub values: Vec<Value>,
    /// Xid of the creating transaction.
    pub xmin: u64,
    /// Xid of the deleting/updating transaction; 0 = not deleted.
    pub xmax: u64,
}

/// One table version. Tables themselves are versioned so that
/// CREATE/DROP TABLE are transactional under MVCC.
#[derive(Clone, Debug)]
pub struct Table {
    pub columns: Vec<(String, ColType)>,
    pub rows: Vec<RowVersion>,
    /// Row-version id -> position in `rows`. Keeps id lookups O(1) so
    /// multi-row writes don't degrade to O(rows) per row.
    row_index: HashMap<u64, usize>,
    /// Xid of the CREATE TABLE transaction.
    pub created_xmin: u64,
    /// Xid of the DROP TABLE transaction; 0 = not dropped.
    pub dropped_xmax: u64,
    // --- v0.9: constraints ---
    /// Per-column NOT NULL flags (PRIMARY KEY implies NOT NULL).
    pub not_null: Vec<bool>,
    /// Per-column DEFAULTs; None = no default.
    pub defaults: Vec<Option<DefaultExpr>>,
    pub checks: Vec<CheckDef>,
    pub uniques: Vec<UniqueDef>,
    pub pkey: Option<UniqueDef>,
    pub fks: Vec<FkDef>,
}

impl Table {
    pub fn new(columns: Vec<(String, ColType)>, created_xmin: u64) -> Self {
        let n = columns.len();
        Table {
            columns,
            rows: Vec::new(),
            row_index: HashMap::new(),
            created_xmin,
            dropped_xmax: 0,
            not_null: vec![false; n],
            defaults: vec![None; n],
            checks: Vec::new(),
            uniques: Vec::new(),
            pkey: None,
            fks: Vec::new(),
        }
    }

    /// Build a table from a parsed v0.9 `TableDef` (constraints included).
    pub fn with_def(def: &TableDef, created_xmin: u64) -> Self {
        let mut t = Table::new(
            def.columns.clone(),
            created_xmin,
        );
        t.not_null = def.not_null.clone();
        t.defaults = def.defaults.clone();
        t.checks = def.checks.clone();
        t.uniques = def.uniques.clone();
        t.pkey = def.pkey.clone();
        t.fks = def.fks.clone();
        t
    }

    /// Index of a column by (already lowercased) name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(n, _)| n == name)
    }

    /// Position of the row version with this id, if present (O(1)).
    pub fn row_pos(&self, id: u64) -> Option<usize> {
        let pos = self.row_index.get(&id).copied();
        debug_assert!(
            pos.map_or(true, |p| self.rows.get(p).is_some_and(|r| r.id == id)),
            "row_index out of sync with rows"
        );
        pos
    }

    /// Append a row version (O(1) amortized).
    pub fn push_version(&mut self, rv: RowVersion) {
        self.row_index.insert(rv.id, self.rows.len());
        self.rows.push(rv);
    }

    /// Remove the version at `pos` without preserving order; O(1).
    pub fn swap_remove_version(&mut self, pos: usize) -> RowVersion {
        let rv = self.rows.swap_remove(pos);
        self.row_index.remove(&rv.id);
        if pos < self.rows.len() {
            let moved_id = self.rows[pos].id;
            self.row_index.insert(moved_id, pos);
        }
        rv
    }

    /// Rebuild the id -> position index from scratch (O(rows)).
    pub fn rebuild_row_index(&mut self) {
        self.row_index.clear();
        for (i, r) in self.rows.iter().enumerate() {
            self.row_index.insert(r.id, i);
        }
    }
}

/// The whole database: table name -> versions of that table (usually one).
/// Multiple live versions of one name can only arise from concurrent
/// uncommitted CREATEs; the commit-time check in server.rs rejects the
/// second committer with 40001.
#[derive(Clone, Debug)]
pub struct Database {
    pub tables: HashMap<String, Vec<Table>>,
    /// Secondary indexes by index name (v0.8). DDL is transactional: each
    /// definition carries creator/deleter xids, and entries for
    /// uncommitted row versions are filtered by visibility at scan time.
    pub indexes: HashMap<String, Index>,
    /// ANALYZE statistics by table name (v0.8). Updated non-transactionally
    /// by ANALYZE, like PostgreSQL; never WAL-logged, rebuilt by ANALYZE.
    pub stats: HashMap<String, TableStats>,
    /// Views by name (v0.9). Versioned like tables so CREATE/DROP VIEW are
    /// transactional under MVCC.
    pub views: HashMap<String, Vec<ViewDef>>,
    /// Sequences by name (v0.9). DDL is transactional; the sequence
    /// *value* (`current`) advances non-transactionally, like PostgreSQL.
    pub sequences: HashMap<String, Vec<Sequence>>,
}

/// A view definition (v0.9): the raw SELECT text plus dependency names.
#[derive(Clone, Debug)]
pub struct ViewDef {
    /// The raw SELECT text after AS (re-parsed at query time).
    pub query: String,
    /// Optional CREATE VIEW (col, ...) aliases.
    pub col_aliases: Vec<String>,
    /// Plain table names the query reads (for dependency tracking).
    pub deps: Vec<String>,
    /// Xid of the CREATE VIEW transaction.
    pub created_xmin: u64,
    /// Xid of the DROP VIEW transaction; 0 = not dropped.
    pub dropped_xmax: u64,
}

/// A sequence (v0.9). Bounds and parameters are transactional DDL;
/// `current` (last nextval result) advances outside transactions.
#[derive(Clone, Debug)]
pub struct Sequence {
    pub name: String,
    pub start: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
    /// Last value returned by nextval; None = never called.
    pub current: Option<i64>,
    /// Postgres `is_called`: false after setval(v,false) means the next
    /// nextval returns `current` itself instead of advancing.
    pub is_called: bool,
    /// Xid of the CREATE SEQUENCE transaction.
    pub created_xmin: u64,
    /// Xid of the DROP SEQUENCE transaction; 0 = not dropped.
    pub dropped_xmax: u64,
}

impl Sequence {
    pub fn new(name: String, start: i64, increment: i64, min_value: i64, max_value: i64, cycle: bool, created_xmin: u64) -> Self {
        Sequence {
            name,
            start,
            increment,
            min_value,
            max_value,
            cycle,
            current: None,
            is_called: false,
            created_xmin,
            dropped_xmax: 0,
        }
    }
}

impl Database {
    pub fn new() -> Self {
        Database {
            tables: HashMap::new(),
            indexes: HashMap::new(),
            stats: HashMap::new(),
            views: HashMap::new(),
            sequences: HashMap::new(),
        }
    }

    /// First view version with `name` visible to (`snap`, `own`).
    pub fn find_view(&self, name: &str, snap: &Snapshot, own: u64) -> Option<&ViewDef> {
        self.views
            .get(name)
            .and_then(|vs| vs.iter().find(|v| view_visible(v, snap, own)))
    }

    /// Mutable variant of [`Database::find_view`].
    pub fn find_view_mut(&mut self, name: &str, snap: &Snapshot, own: u64) -> Option<&mut ViewDef> {
        self.views
            .get_mut(name)
            .and_then(|vs| vs.iter_mut().find(|v| view_visible(v, snap, own)))
    }

    /// First sequence version with `name` visible to (`snap`, `own`).
    pub fn find_sequence(&self, name: &str, snap: &Snapshot, own: u64) -> Option<&Sequence> {
        self.sequences
            .get(name)
            .and_then(|vs| vs.iter().find(|s| seq_visible(s, snap, own)))
    }

    /// Mutable variant of [`Database::find_sequence`].
    pub fn find_sequence_mut(
        &mut self,
        name: &str,
        snap: &Snapshot,
        own: u64,
    ) -> Option<&mut Sequence> {
        self.sequences
            .get_mut(name)
            .and_then(|vs| vs.iter_mut().find(|s| seq_visible(s, snap, own)))
    }

    /// First table version with `name` visible to (`snap`, `own`).
    pub fn find_table(&self, name: &str, snap: &Snapshot, own: u64) -> Option<&Table> {
        self.tables
            .get(name)
            .and_then(|vs| vs.iter().find(|t| table_visible(t, snap, own)))
    }

    /// Mutable variant of [`Database::find_table`].
    pub fn find_table_mut(&mut self, name: &str, snap: &Snapshot, own: u64) -> Option<&mut Table> {
        self.tables
            .get_mut(name)
            .and_then(|vs| vs.iter_mut().find(|t| table_visible(t, snap, own)))
    }

    /// The table version created by `own` (for WAL logging of DDL).
    /// Any row version with this id, wherever it lives (ids are global).
    /// Used by undo and WAL replay; both run with the engine lock held.
    pub fn find_row_version_mut(&mut self, id: u64) -> Option<&mut RowVersion> {
        for vs in self.tables.values_mut() {
            for t in vs {
                if let Some(pos) = t.row_pos(id) {
                    return Some(&mut t.rows[pos]);
                }
            }
        }
        None
    }

    /// Immutable twin, for commit-time WAL record validation.
    pub fn find_row_version(&self, id: u64) -> Option<&RowVersion> {
        for vs in self.tables.values() {
            for t in vs {
                if let Some(pos) = t.row_pos(id) {
                    return Some(&t.rows[pos]);
                }
            }
        }
        None
    }

    // -- v0.8: secondary index maintenance -------------------------------

    /// Index definition visible to (`snap`, `own`), if any.
    pub fn find_index(&self, name: &str, snap: &Snapshot, own: u64) -> Option<&Index> {
        self.indexes
            .get(name)
            .filter(|ix| index_visible(&ix.def, snap, own))
    }

    /// Mutable twin of [`Database::find_index`].
    pub fn find_index_mut(
        &mut self,
        name: &str,
        snap: &Snapshot,
        own: u64,
    ) -> Option<&mut Index> {
        self.indexes
            .get_mut(name)
            .filter(|ix| index_visible(&ix.def, snap, own))
    }

    /// All index definitions on `table` visible to (`snap`, `own`),
    /// sorted by name for determinism.
    pub fn visible_indexes_for(&self, table: &str, snap: &Snapshot, own: u64) -> Vec<&Index> {
        let mut out: Vec<&Index> = self
            .indexes
            .values()
            .filter(|ix| ix.def.table == table && index_visible(&ix.def, snap, own))
            .collect();
        out.sort_by(|a, b| a.def.name.cmp(&b.def.name));
        out
    }

    /// Insert entries for one row version into every live (not dropped)
    /// index on `table`. Called for INSERT and for the new version of an
    /// UPDATE — including uncommitted versions, whose entries are filtered
    /// by visibility at scan time (PostgreSQL does the same).
    pub fn index_insert_row(&mut self, table: &str, row_id: u64, values: &[Value]) {
        let targets: Vec<(String, Vec<usize>)> = self
            .indexes
            .values()
            .filter(|ix| ix.def.table == table && ix.def.dropped_xmax == 0)
            .map(|ix| (ix.def.name.clone(), ix.def.cols.clone()))
            .collect();
        for (name, cols) in targets {
            let key = IndexKey(cols.iter().map(|&c| values[c].clone()).collect());
            if let Some(ix) = self.indexes.get_mut(&name) {
                ix.insert(key, row_id);
            }
        }
    }

    /// Remove one row version's entries from every live index on `table`.
    /// Called when a version disappears entirely: ROLLBACK of an INSERT
    /// and VACUUM of dead versions. (DELETE/UPDATE keep the entry — the
    /// version still exists, just invisibly.)
    pub fn index_remove_row(&mut self, table: &str, row_id: u64, values: &[Value]) {
        let targets: Vec<(String, Vec<usize>)> = self
            .indexes
            .values()
            .filter(|ix| ix.def.table == table && ix.def.dropped_xmax == 0)
            .map(|ix| (ix.def.name.clone(), ix.def.cols.clone()))
            .collect();
        for (name, cols) in targets {
            let key = IndexKey(cols.iter().map(|&c| values[c].clone()).collect());
            if let Some(ix) = self.indexes.get_mut(&name) {
                ix.remove(&key, row_id);
            }
        }
    }

    /// Unique-violation check for an INSERT/UPDATE of `values` into
    /// `table`. Returns the name of the violated unique index, if any.
    /// A conflicting entry only counts when its row version is *alive*
    /// for us: our own uncommitted version, or a version visible in our
    /// snapshot. Deleted or invisible versions are ignored — like a
    /// Postgres unique check, which consults the heap. NULL key parts
    /// never conflict (PostgreSQL semantics). `exclude_row_id` skips one
    /// row id (the UPDATE old version, already marked deleted by us).
    pub fn unique_violation(
        &self,
        table: &str,
        values: &[Value],
        exclude_row_id: Option<u64>,
        snap: &Snapshot,
        own: u64,
    ) -> Option<String> {
        let t = self.find_table(table, snap, own)?;
        for ix in self.visible_indexes_for(table, snap, own) {
            if !ix.def.unique {
                continue;
            }
            let key = ix.key_for(values);
            if key.0.iter().any(|v| matches!(v, Value::Null)) {
                continue; // NULLs never conflict
            }
            let Some(bucket) = ix.tree.get(&key) else {
                continue;
            };
            for &id in bucket {
                if Some(id) == exclude_row_id {
                    continue;
                }
                let alive = match t.row_pos(id) {
                    Some(pos) => {
                        let r = &t.rows[pos];
                        if r.xmax == own {
                            false // deleted by us: not a conflict
                        } else {
                            r.xmin == own || row_visible(r, snap, own)
                        }
                    }
                    None => false, // vacuumed away: cannot conflict
                };
                if alive {
                    return Some(ix.def.name.clone());
                }
            }
        }
        None
    }

    /// v0.10: like `unique_violation`, but restricted to the named unique
    /// index and returning the conflicting row version's id instead of the
    /// index name. Used by `INSERT ... ON CONFLICT` to locate the row to
    /// update (DO UPDATE) or skip (DO NOTHING).
    pub fn unique_conflict_row(
        &self,
        table: &str,
        index_name: &str,
        values: &[Value],
        exclude_row_id: Option<u64>,
        snap: &Snapshot,
        own: u64,
    ) -> Option<u64> {
        let t = self.find_table(table, snap, own)?;
        let ix = self
            .visible_indexes_for(table, snap, own)
            .into_iter()
            .find(|ix| ix.def.name == index_name && ix.def.unique)?;
        let key = ix.key_for(values);
        if key.0.iter().any(|v| matches!(v, Value::Null)) {
            return None; // NULLs never conflict
        }
        let bucket = ix.tree.get(&key)?;
        for &id in bucket {
            if Some(id) == exclude_row_id {
                continue;
            }
            let alive = match t.row_pos(id) {
                Some(pos) => {
                    let r = &t.rows[pos];
                    if r.xmax == own {
                        false // deleted by us: not a conflict
                    } else {
                        r.xmin == own || row_visible(r, snap, own)
                    }
                }
                None => false, // vacuumed away: cannot conflict
            };
            if alive {
                return Some(id);
            }
        }
        None
    }
}

/// Index DDL visibility: like a table version, but definitions are stored
/// flat (one live definition per name at a time — enforced at commit).
fn index_visible(def: &IndexDef, snap: &Snapshot, own: u64) -> bool {
    let created_ok = def.created_xmin == own
        || (def.created_xmin < snap.next_xid && !snap.active.contains(&def.created_xmin));
    if !created_ok {
        return false;
    }
    if def.dropped_xmax == 0 {
        return true;
    }
    if def.dropped_xmax == own {
        return false;
    }
    !(def.dropped_xmax < snap.next_xid && !snap.active.contains(&def.dropped_xmax))
}

/// Per-column statistics from ANALYZE (v0.8).
#[derive(Clone, Debug, Default)]
pub struct ColStats {
    /// Fraction of NULL values, 0.0..=1.0.
    pub null_frac: f64,
    /// Estimated number of distinct non-null values.
    pub n_distinct: f64,
    /// Most common values with their frequencies: (value, frac).
    pub mcv: Vec<(Value, f64)>,
    /// Sorted distinct-value bounds for range selectivity (a coarse
    /// histogram: consecutive pairs bracket ~equal row counts).
    pub hist_bounds: Vec<Value>,
}

/// Per-table statistics from ANALYZE (v0.8).
#[derive(Clone, Debug, Default)]
pub struct TableStats {
    /// Estimated live row count.
    pub reltuples: f64,
    pub cols: HashMap<String, ColStats>,
}

/// A transaction's consistent view: the xids active when it was taken plus
/// the next xid to be assigned. A version created by an xid below
/// `next_xid` that is absent from `active` is committed.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub active: Vec<u64>,
    pub next_xid: u64,
}

/// Global transaction state, kept beside the database inside [`Engine`].
#[derive(Clone, Debug)]
pub struct TxnManager {
    /// Next xid to hand out. Starts at 1; 0 is never a real xid.
    pub next_xid: u64,
    /// Next row-version id to hand out. Starts at 1.
    pub next_row_id: u64,
    /// Xids of currently running transactions.
    pub active: HashSet<u64>,
    /// Current snapshot per active xid (for VACUUM's dead-to-all check).
    pub snapshots: HashMap<u64, Snapshot>,
    /// Row-version locks from `SELECT ... FOR UPDATE` (v0.6): row-version
    /// id -> holding xid. Locks are held to transaction end (commit,
    /// rollback, or disconnect) and are never waited on: a conflicting
    /// locker/writer fails fast with 40001 instead of blocking.
    pub row_locks: HashMap<u64, u64>,
    /// Acquisition order of the row locks above, per xid: xid -> row ids
    /// in the order they were taken. Lets `ROLLBACK TO SAVEPOINT` release
    /// exactly the locks taken after the savepoint.
    pub lock_order: HashMap<u64, Vec<u64>>,
}

/// Everything shared between connections: the versioned database plus
/// the transaction manager, behind a single mutex in main.rs. One lock
/// for the whole engine keeps the locking discipline trivial (engine ->
/// wal) at the cost of serializing statements — acceptable for v0.5.
#[derive(Debug)]
pub struct Engine {
    pub db: Database,
    pub txns: TxnManager,
    /// v0.9: session-local `currval` state: (session id, sequence name) ->
    /// last `nextval` result in that session. Cleared when... never
    /// automatically (Postgres keeps it for the session); keyed by the
    /// server-assigned session id.
    pub seq_currval: HashMap<(u64, String), i64>,
    /// v0.9: names of sequences advanced by the in-flight statement
    /// (nextval/setval). Drained into `WriteOp::SeqAdvance` markers by the
    /// server after a successful statement, for commit-time WAL logging.
    pub seq_advanced: Vec<String>,
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            db: Database::new(),
            txns: TxnManager {
                next_xid: 1,
                next_row_id: 1,
                active: HashSet::new(),
                snapshots: HashMap::new(),
                row_locks: HashMap::new(),
                lock_order: HashMap::new(),
            },
            seq_currval: HashMap::new(),
            seq_advanced: Vec::new(),
        }
    }

    pub fn alloc_xid(&mut self) -> u64 {
        let xid = self.txns.next_xid;
        self.txns.next_xid += 1;
        xid
    }

    /// Start a transaction: allocate an xid and register it active.
    pub fn begin_txn(&mut self) -> u64 {
        let xid = self.alloc_xid();
        self.txns.active.insert(xid);
        xid
    }

    /// End a transaction (commit or abort): unregister the xid and drop
    /// its registered snapshot, if any.
    pub fn end_txn(&mut self, xid: u64) {
        self.txns.active.remove(&xid);
        self.txns.snapshots.remove(&xid);
    }

    /// Register a long-lived snapshot (REPEATABLE READ / SERIALIZABLE).
    /// The snapshot pins the versions it can see against VACUUM until
    /// the transaction ends.
    pub fn register_snapshot(&mut self, xid: u64, snap: Snapshot) {
        self.txns.snapshots.insert(xid, snap);
    }

    pub fn alloc_row_id(&mut self) -> u64 {
        let id = self.txns.next_row_id;
        self.txns.next_row_id += 1;
        id
    }

    /// Try to take a `SELECT ... FOR UPDATE` lock on a row version for
    /// `xid`. Re-locking your own rows is a no-op. Returns the conflicting
    /// holder's xid when another *active* transaction holds the lock; a
    /// lock whose holder is gone (should not happen — locks release at
    /// txn end) is treated as stale and taken over.
    pub fn try_row_lock(&mut self, row_id: u64, xid: u64) -> Result<(), u64> {
        match self.txns.row_locks.get(&row_id) {
            Some(&h) if h == xid => Ok(()),
            Some(&h) if self.txns.active.contains(&h) => Err(h),
            _ => {
                self.txns.row_locks.insert(row_id, xid);
                self.txns.lock_order.entry(xid).or_default().push(row_id);
                Ok(())
            }
        }
    }

    /// Who holds the lock on this row version, if anyone.
    pub fn row_lock_holder(&self, row_id: u64) -> Option<u64> {
        self.txns.row_locks.get(&row_id).copied()
    }

    /// Release every row lock held by `xid`: commit, rollback, and the
    /// disconnect-abort path all funnel through here, so locks never leak
    /// past transaction end.
    pub fn release_txn_locks(&mut self, xid: u64) {
        self.txns.row_locks.retain(|_, h| *h != xid);
        self.txns.lock_order.remove(&xid);
    }

    /// Release the locks `xid` took most recently, keeping the first
    /// `keep` of them. Used by `ROLLBACK TO SAVEPOINT`, which records the
    /// lock count alongside the write-log position when the savepoint is
    /// established.
    pub fn release_locks_after(&mut self, xid: u64, keep: usize) {
        if let Some(order) = self.txns.lock_order.get_mut(&xid) {
            for row_id in order.drain(keep..) {
                // Only drop the lock if we still hold it (a stale entry
                // must never release someone else's lock).
                if self.txns.row_locks.get(&row_id) == Some(&xid) {
                    self.txns.row_locks.remove(&row_id);
                }
            }
        }
    }

    /// How many row locks `xid` currently holds (savepoint bookkeeping).
    pub fn txn_lock_count(&self, xid: u64) -> usize {
        self.txns.lock_order.get(&xid).map(|v| v.len()).unwrap_or(0)
    }

    /// A fresh snapshot of the currently active transactions.
    pub fn take_snapshot(&self) -> Snapshot {
        let mut active: Vec<u64> = self.txns.active.iter().copied().collect();
        active.sort_unstable();
        Snapshot {
            active,
            next_xid: self.txns.next_xid,
        }
    }

    /// Is `xid` a committed transaction from the engine's point of view?
    /// (Aborted transactions undo their writes, so anything below
    /// next_xid that is not active has committed.)
    pub fn xid_committed(&self, xid: u64) -> bool {
        xid < self.txns.next_xid && !self.txns.active.contains(&xid)
    }

    /// Physically remove dead versions from one table's versions.
    /// Returns the number of versions removed.
    pub fn vacuum_table(&mut self, name: &str) -> usize {
        let mut removed = 0;
        // Borrow the manager immutably for the dead-to-all test while the
        // table versions are borrowed mutably: the two are disjoint.
        let txns = &self.txns;
        // Collect (id, values) of the dead versions first: index cleanup
        // needs each version's key, hence its values.
        let mut dead: Vec<(u64, Vec<Value>)> = Vec::new();
        if let Some(versions) = self.db.tables.get_mut(name) {
            for t in versions {
                for v in t.rows.iter().filter(|v| version_dead_to_all(txns, v)) {
                    dead.push((v.id, v.values.clone()));
                }
                let before = t.rows.len();
                t.rows.retain(|v| !version_dead_to_all(txns, v));
                let gone = before - t.rows.len();
                if gone > 0 {
                    t.rebuild_row_index();
                }
                removed += gone;
            }
        }
        for (id, values) in &dead {
            self.db.index_remove_row(name, *id, values);
        }
        removed
    }

    /// Vacuum every table. Returns (table, removed) per table touched.
    pub fn vacuum_all(&mut self) -> Vec<(String, usize)> {
        let names: Vec<String> = self.db.tables.keys().cloned().collect();
        let mut out = Vec::new();
        for name in names {
            let n = self.vacuum_table(&name);
            if n > 0 {
                out.push((name, n));
            }
        }
        out.sort();
        out
    }
}

/// Free-function twin of [`Engine::version_dead_to_all`], so callers
/// that already hold a mutable borrow of the tables (VACUUM) can test
/// deadness against an immutably borrowed manager.
pub fn version_dead_to_all(txns: &TxnManager, v: &RowVersion) -> bool {
    if v.xmax == 0 {
        return false;
    }
    if txns.active.contains(&v.xmin) || txns.active.contains(&v.xmax) {
        return false;
    }
    // xmax committed (not active, below next_xid by construction of a
    // version that exists). Dead unless some active snapshot predates
    // the deleter's commit and could still see the old version.
    for snap in txns.snapshots.values() {
        if !(v.xmax < snap.next_xid && !snap.active.contains(&v.xmax)) {
            return false;
        }
    }
    true
}

/// Row-version visibility for (`snap`, `own`).
pub fn row_visible(v: &RowVersion, snap: &Snapshot, own: u64) -> bool {
    let xmin_ok = v.xmin == own || (v.xmin < snap.next_xid && !snap.active.contains(&v.xmin));
    if !xmin_ok {
        return false;
    }
    if v.xmax == 0 {
        return true;
    }
    if v.xmax == own {
        return false;
    }
    !(v.xmax < snap.next_xid && !snap.active.contains(&v.xmax))
}

/// Table-version visibility for (`snap`, `own`).
pub fn table_visible(t: &Table, snap: &Snapshot, own: u64) -> bool {
    let created_ok = t.created_xmin == own
        || (t.created_xmin < snap.next_xid && !snap.active.contains(&t.created_xmin));
    if !created_ok {
        return false;
    }
    if t.dropped_xmax == 0 {
        return true;
    }
    if t.dropped_xmax == own {
        return false;
    }
    !(t.dropped_xmax < snap.next_xid && !snap.active.contains(&t.dropped_xmax))
}

/// Visibility for view versions (v0.9): same rules as tables.
pub fn view_visible(v: &ViewDef, snap: &Snapshot, own: u64) -> bool {
    let created_ok = v.created_xmin == own
        || (v.created_xmin < snap.next_xid && !snap.active.contains(&v.created_xmin));
    if !created_ok {
        return false;
    }
    if v.dropped_xmax == 0 {
        return true;
    }
    if v.dropped_xmax == own {
        return false;
    }
    !(v.dropped_xmax < snap.next_xid && !snap.active.contains(&v.dropped_xmax))
}

/// Visibility for sequence versions (v0.9): same rules as tables.
pub fn seq_visible(s: &Sequence, snap: &Snapshot, own: u64) -> bool {
    let created_ok = s.created_xmin == own
        || (s.created_xmin < snap.next_xid && !snap.active.contains(&s.created_xmin));
    if !created_ok {
        return false;
    }
    if s.dropped_xmax == 0 {
        return true;
    }
    if s.dropped_xmax == own {
        return false;
    }
    !(s.dropped_xmax < snap.next_xid && !snap.active.contains(&s.dropped_xmax))
}

// ---------------------------------------------------------------------------
// Write log: per-transaction undo + commit-time WAL records
// ---------------------------------------------------------------------------

/// One uncommitted change, enough to undo it (abort / ROLLBACK TO /
/// failed statement) and, at commit, to derive the WAL records.
#[derive(Clone, Debug)]
pub enum WriteOp {
    InsertRow {
        table: String,
        row_id: u64,
    },
    DeleteRow {
        table: String,
        row_id: u64,
        prev_xmax: u64,
    },
    CreateTable {
        name: String,
    },
    DropTable {
        name: String,
        prev_xmax: u64,
    },
    // --- v0.8: index DDL. DropIndex carries the whole index so undo can
    // restore the definition and its entries exactly.
    CreateIndex {
        name: String,
    },
    DropIndex {
        name: String,
        index: Index,
    },
    // --- v0.9: ALTER TABLE carries the whole previous table (schema and
    // rows) so undo restores it exactly. `renamed_to` is Some when the
    // alter renamed the table: the altered version lives under the new
    // name and the previous one is restored under `name`.
    AlterTable {
        name: String,
        prev: Table,
        renamed_to: Option<String>,
        /// True when ADD/DROP COLUMN rewrote the rows (new ids, logged as
        /// InsertRow ops); false for pure-metadata alters.
        rewrite_rows: bool,
    },
    CreateView {
        name: String,
    },
    DropView {
        name: String,
        view: ViewDef,
    },
    CreateSequence {
        name: String,
    },
    DropSequence {
        name: String,
        seq: Sequence,
    },
    AlterSequence {
        name: String,
        prev: Sequence,
    },
    /// A non-transactional sequence advance (Postgres semantics): abort
    /// never rolls it back; the op is only a commit-time WAL marker.
    SeqAdvance {
        name: String,
    },
}

/// Undo a single write op. Each undo is conditional on the version still
/// being ours: a concurrent transaction may have overwritten xmax after
/// us (last-writer-wins, no row locking in v0.5), in which case their
/// op owns the version now and ours must not clobber it.
pub fn undo_write_op(eng: &mut Engine, own: u64, op: &WriteOp) {
    match op {
        WriteOp::InsertRow { table, row_id } => {
            // Two phases: removing the version borrows the table mutably,
            // and the index cleanup needs `eng.db` mutably too — so the
            // values are cloned and the table borrow is dropped first.
            // (DELETE/UPDATE never remove entries, so undoing an insert is
            // the only DML case that touches the index.)
            let removed: Option<Vec<Value>> =
                if let Some(versions) = eng.db.tables.get_mut(table) {
                    let mut out = None;
                    for t in versions.iter_mut() {
                        if let Some(pos) = t.row_pos(*row_id) {
                            if t.rows[pos].xmin == own {
                                out = Some(t.rows[pos].values.clone());
                                t.swap_remove_version(pos);
                            }
                            break;
                        }
                    }
                    out
                } else {
                    None
                };
            if let Some(values) = removed {
                eng.db.index_remove_row(table, *row_id, &values);
            }
        }
        WriteOp::DeleteRow {
            table: _,
            row_id,
            prev_xmax,
        } => {
            if let Some(v) = eng.db.find_row_version_mut(*row_id) {
                if v.xmax == own {
                    v.xmax = *prev_xmax;
                }
            }
        }
        WriteOp::CreateTable { name } => {
            if let Some(versions) = eng.db.tables.get_mut(name) {
                if let Some(pos) = versions.iter().position(|t| t.created_xmin == own) {
                    versions.swap_remove(pos);
                }
                if versions.is_empty() {
                    eng.db.tables.remove(name);
                }
            }
        }
        WriteOp::DropTable { name, prev_xmax } => {
            if let Some(versions) = eng.db.tables.get_mut(name) {
                if let Some(t) = versions.iter_mut().find(|t| t.dropped_xmax == own) {
                    t.dropped_xmax = *prev_xmax;
                }
            }
        }
        WriteOp::CreateIndex { name } => {
            // Undo a CREATE INDEX: drop the definition (and its entries)
            // iff it is still ours — a concurrent DROP INDEX of the same
            // name owns it now.
            let ours = eng
                .db
                .indexes
                .get(name)
                .map(|ix| ix.def.created_xmin == own)
                .unwrap_or(false);
            if ours {
                eng.db.indexes.remove(name);
            }
        }
        WriteOp::DropIndex { name, index } => {
            // Undo a DROP INDEX: restore the definition and entries iff
            // the drop is still ours.
            let ours = eng
                .db
                .indexes
                .get(name)
                .map(|ix| ix.def.dropped_xmax == own)
                .unwrap_or(false);
            if ours {
                eng.db.indexes.insert(name.clone(), index.clone());
            }
        }
        // --- v0.9 undos ---
        WriteOp::AlterTable {
            name,
            prev,
            renamed_to,
            ..
        } => {
            // Remove our altered version (under the new name if renamed),
            // then restore the previous table under its original name.
            let target = renamed_to.as_deref().unwrap_or(name);
            if let Some(versions) = eng.db.tables.get_mut(target) {
                versions.retain(|t| t.created_xmin != own);
                if versions.is_empty() {
                    eng.db.tables.remove(target);
                }
            }
            eng.db
                .tables
                .entry(name.clone())
                .or_default()
                .push(prev.clone());
        }
        WriteOp::CreateView { name } => {
            if let Some(versions) = eng.db.views.get_mut(name) {
                versions.retain(|v| v.created_xmin != own);
                if versions.is_empty() {
                    eng.db.views.remove(name);
                }
            }
        }
        WriteOp::DropView { name, view } => {
            let ours = eng
                .db
                .views
                .get(name)
                .map(|vs| {
                    vs.iter()
                        .any(|v| v.created_xmin == view.created_xmin && v.dropped_xmax == own)
                })
                .unwrap_or(false);
            if ours {
                let mut restored = view.clone();
                restored.dropped_xmax = 0;
                if let Some(versions) = eng.db.views.get_mut(name) {
                    for v in versions.iter_mut() {
                        if v.created_xmin == view.created_xmin {
                            *v = restored;
                            break;
                        }
                    }
                }
            }
        }
        WriteOp::CreateSequence { name } => {
            if let Some(versions) = eng.db.sequences.get_mut(name) {
                versions.retain(|s| s.created_xmin != own);
                if versions.is_empty() {
                    eng.db.sequences.remove(name);
                }
            }
        }
        WriteOp::DropSequence { name, seq } => {
            let ours = eng
                .db
                .sequences
                .get(name)
                .map(|vs| {
                    vs.iter()
                        .any(|s| s.created_xmin == seq.created_xmin && s.dropped_xmax == own)
                })
                .unwrap_or(false);
            if ours {
                let mut restored = seq.clone();
                restored.dropped_xmax = 0;
                if let Some(versions) = eng.db.sequences.get_mut(name) {
                    for s in versions.iter_mut() {
                        if s.created_xmin == seq.created_xmin {
                            *s = restored;
                            break;
                        }
                    }
                }
            }
        }
        WriteOp::AlterSequence { name, prev } => {
            if let Some(versions) = eng.db.sequences.get_mut(name) {
                for s in versions.iter_mut() {
                    if s.created_xmin == prev.created_xmin {
                        *s = prev.clone();
                        break;
                    }
                }
            }
        }
        WriteOp::SeqAdvance { name, .. } => {
            // Postgres semantics: nextval/setval advances are NOT rolled
            // back on abort. The op is only a commit-time WAL marker, so
            // undo is a deliberate no-op. (If the sequence itself was
            // created by this transaction, the CreateSequence undo drops
            // it, taking the advance with it.)
            let _ = name;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_table() -> Engine {
        let mut eng = Engine::new();
        eng.db.tables.insert(
            "t".to_string(),
            vec![{
                let mut t = Table::new(vec![("a".to_string(), ColType::Int)], 1);
                t.push_version(RowVersion {
                    id: 1,
                    values: vec![Value::Int(1)],
                    xmin: 1,
                    xmax: 0,
                });
                t
            }],
        );
        eng.txns.next_xid = 10;
        eng.txns.next_row_id = 2;
        eng
    }

    fn snap(active: &[u64], next_xid: u64) -> Snapshot {
        Snapshot {
            active: active.to_vec(),
            next_xid,
        }
    }

    #[test]
    fn committed_version_visible_to_later_snapshot() {
        let eng = engine_with_table();
        let v = &eng.db.tables["t"][0].rows[0];
        // xid 1 committed (below next_xid=10, not active).
        assert!(row_visible(v, &snap(&[], 10), 99));
    }

    #[test]
    fn uncommitted_version_invisible_to_others() {
        let eng = engine_with_table();
        let mut v = eng.db.tables["t"][0].rows[0].clone();
        v.xmin = 7;
        // xid 7 still active: invisible to everyone else...
        assert!(!row_visible(&v, &snap(&[7], 10), 99));
        // ...but visible to its owner.
        assert!(row_visible(&v, &snap(&[7], 10), 7));
    }

    #[test]
    fn deleted_version_hidden_once_deleter_commits() {
        let eng = engine_with_table();
        let mut v = eng.db.tables["t"][0].rows[0].clone();
        v.xmax = 8; // deleter still active: delete not yet visible
        assert!(row_visible(&v, &snap(&[8], 10), 99));
        // Deleter committed (gone from active): version invisible.
        assert!(!row_visible(&v, &snap(&[], 10), 99));
        // But a snapshot taken while the deleter was active still sees it.
        assert!(row_visible(&v, &snap(&[8], 10), 99));
    }

    #[test]
    fn own_delete_hides_from_self() {
        let eng = engine_with_table();
        let mut v = eng.db.tables["t"][0].rows[0].clone();
        v.xmax = 7;
        assert!(!row_visible(&v, &snap(&[7], 10), 7));
    }

    #[test]
    fn version_dead_to_all_respects_snapshots() {
        let mut eng = engine_with_table();
        eng.txns.active.insert(20);
        // Snapshot taken while deleter 8 was still active.
        eng.txns.snapshots.insert(20, snap(&[8, 20], 10));
        let mut v = eng.db.tables["t"][0].rows[0].clone();
        v.xmax = 8;
        // Deleter 8 has committed since, but txn 20's old snapshot could
        // still see the version: not dead.
        assert!(!version_dead_to_all(&eng.txns, &v));
        // Once txn 20 goes away, the version is dead.
        eng.txns.snapshots.remove(&20);
        eng.txns.active.remove(&20);
        assert!(version_dead_to_all(&eng.txns, &v));
    }

    #[test]
    fn vacuum_table_removes_only_dead_versions() {
        let mut eng = engine_with_table();
        // Add a dead version (deleted by committed xid 8) and a live one.
        let t = &mut eng.db.tables.get_mut("t").unwrap()[0];
        t.push_version(RowVersion {
            id: 2,
            values: vec![Value::Int(2)],
            xmin: 1,
            xmax: 8,
        });
        t.push_version(RowVersion {
            id: 3,
            values: vec![Value::Int(3)],
            xmin: 1,
            xmax: 0,
        });
        assert_eq!(eng.vacuum_table("t"), 1);
        let ids: Vec<u64> = eng.db.tables["t"][0].rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn undo_insert_removes_version() {
        let mut eng = engine_with_table();
        eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
            id: 9,
            values: vec![Value::Int(9)],
            xmin: 7,
            xmax: 0,
        });
        undo_write_op(
            &mut eng,
            7,
            &WriteOp::InsertRow {
                table: "t".to_string(),
                row_id: 9,
            },
        );
        assert_eq!(eng.db.tables["t"][0].rows.len(), 1);
    }

    #[test]
    fn undo_delete_restores_xmax_only_if_still_ours() {
        let mut eng = engine_with_table();
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = 7;
        let op = WriteOp::DeleteRow {
            table: "t".to_string(),
            row_id: 1,
            prev_xmax: 0,
        };
        // Someone else overwrote xmax after us: our undo must not clobber.
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = 8;
        undo_write_op(&mut eng, 7, &op);
        assert_eq!(eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax, 8);
        // Still ours: restore.
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = 7;
        undo_write_op(&mut eng, 7, &op);
        assert_eq!(eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax, 0);
    }
}
