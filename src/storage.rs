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
use std::sync::Arc;

use crate::fxhash::FxBuildHasher;
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
    // v0.35: SQL character types with typmod (PG19 bpchar/varchar).
    // Char(Some(n)) is blank-padded `character(n)`; Char(None) is
    // `character`/`bpchar` with typmod -1 (no padding, no limit).
    // Varchar(Some(n)) is `character varying(n)`; Varchar(None) is
    // unlimited `character varying`.
    Char(Option<i32>),    // OID 1042
    Varchar(Option<i32>), // OID 1043
    // v0.36: PG's one-byte `"char"` type (OID 18), written double-quoted
    // to distinguish it from `character(1)`. A single byte, not a
    // character: multibyte UTF-8 input is rejected by charin.
    SingleChar,  // OID 18 ("char")
    Bool,        // OID 16
    Date,        // OID 1082 (v0.7)
    Timestamp,   // OID 1114 (v0.7)
    Timestamptz, // OID 1184 (v0.7)
    Bytea,       // OID 17 (v0.7)
    Uuid,        // OID 2950 (v0.7)
    // v0.37: PG's regclass type (OID 2205) — an OID that displays as
    // the relation name. Used for pg_class.reltoastrelid::regclass.
    Regclass, // OID 2205
    // v0.57: PG's `name` internal identifier type (OID 19,
    // NAMEDATALEN-1 = 63 bytes). Values are stored as text truncated
    // to 63 bytes on input (PG19 namein); comparison is plain byte
    // order on the truncated values, which is exactly PG's
    // strncmp(..., NAMEDATALEN) semantics since names hold no NULs.
    Name, // OID 19
}

impl ColType {
    /// PostgreSQL type OID used in RowDescription.
    pub fn oid(&self) -> i32 {
        match self {
            ColType::Int => 23,           // INT4
            ColType::BigInt => 20,        // INT8
            ColType::SmallInt => 21,      // INT2
            ColType::Text => 25,          // TEXT
            ColType::Char(_) => 1042,     // BPCHAR (v0.35)
            ColType::Varchar(_) => 1043,  // VARCHAR (v0.35)
            ColType::SingleChar => 18,    // "char" (v0.36)
            ColType::Bool => 16,          // BOOL
            ColType::Float => 701,        // FLOAT8
            ColType::Float4 => 700,       // FLOAT4
            ColType::Numeric => 1700,     // NUMERIC
            ColType::Date => 1082,        // DATE
            ColType::Timestamp => 1114,   // TIMESTAMP
            ColType::Timestamptz => 1184, // TIMESTAMPTZ
            ColType::Bytea => 17,         // BYTEA
            ColType::Uuid => 2950,        // UUID
            ColType::Regclass => 2205,    // REGCLASS (v0.37)
            ColType::Name => 19,          // NAME (v0.57)
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
            // v0.35: PG's format_type() shows the typmod; the harness and
            // wire column names only need the base name.
            ColType::Char(_) => "character",
            ColType::Varchar(_) => "character varying",
            // v0.36: PG's typname for OID 18 is `"char"` (with quotes).
            ColType::SingleChar => "\"char\"",
            ColType::Bool => "boolean",
            ColType::Date => "date",
            ColType::Timestamp => "timestamp without time zone",
            ColType::Timestamptz => "timestamp with time zone",
            ColType::Bytea => "bytea",
            ColType::Uuid => "uuid",
            ColType::Regclass => "regclass",
            ColType::Name => "name",
        }
    }

    /// PostgreSQL `pg_type.typname` (v0.14): used as the default column
    /// name for a bare `SELECT expr::type` cast, like Postgres.
    pub fn pg_typname(&self) -> &'static str {
        match self {
            ColType::Int => "int4",
            ColType::BigInt => "int8",
            ColType::SmallInt => "int2",
            ColType::Float => "float8",
            ColType::Float4 => "float4",
            ColType::Numeric => "numeric",
            ColType::Text => "text",
            // v0.35: PG names `CAST(x AS char(n))` output columns `bpchar`
            // and `CAST(x AS varchar(n))` ones `varchar`.
            ColType::Char(_) => "bpchar",
            ColType::Varchar(_) => "varchar",
            // v0.36: PG labels `SELECT 'a'::"char"` columns bare `char`
            // (char.out), even though the catalog typname is `"char"`.
            ColType::SingleChar => "char",
            ColType::Bool => "bool",
            ColType::Date => "date",
            ColType::Timestamp => "timestamp",
            ColType::Timestamptz => "timestamptz",
            ColType::Bytea => "bytea",
            ColType::Uuid => "uuid",
            ColType::Regclass => "regclass",
            ColType::Name => "name",
        }
    }

    /// v0.35: human-readable type with typmod, e.g. `character(4)` or
    /// `character varying`; used in error messages and display contexts.
    pub fn typmod_display(&self) -> String {
        match self {
            ColType::Char(Some(n)) => format!("character({})", n),
            ColType::Char(None) => "character".to_string(),
            ColType::Varchar(Some(n)) => format!("character varying({})", n),
            ColType::Varchar(None) => "character varying".to_string(),
            other => other.sql_name().to_string(),
        }
    }

    /// v0.37: whether this type is TOAST-able (PG's `typlen == -1`,
    /// varlena types). Only these columns get non-PLAIN storage and a
    /// toast table.
    pub fn is_toastable(&self) -> bool {
        matches!(
            self,
            ColType::Text
                | ColType::Char(_)
                | ColType::Varchar(_)
                | ColType::Bytea
                | ColType::Numeric
        )
    }

    /// v0.37: the type's default TOAST storage strategy (PG's
    /// `pg_type.typstorage`): `x` (extended) for most varlena types,
    /// `m` (main) for numeric, `p` (plain) for the rest.
    pub fn default_toast_storage(&self) -> u8 {
        if !self.is_toastable() {
            return toast_storage::PLAIN;
        }
        match self {
            ColType::Numeric => toast_storage::MAIN,
            _ => toast_storage::EXTENDED,
        }
    }
}

/// v0.37: TOAST storage strategies — PG's `pg_attribute.attstorage`
/// single-character codes (`p`/`e`/`x`/`m`).
pub mod toast_storage {
    /// PLAIN: no compression, no out-of-line storage.
    pub const PLAIN: u8 = b'p';
    /// EXTERNAL: out-of-line storage, no compression.
    pub const EXTERNAL: u8 = b'e';
    /// EXTENDED: compression first, then out-of-line storage (default
    /// for most toastable types).
    pub const EXTENDED: u8 = b'x';
    /// MAIN: compression, out-of-line only as a last resort.
    pub const MAIN: u8 = b'm';

    /// Parse a `SET STORAGE` mode name (case-insensitive), like PG's
    /// `GetAttributeStorage`. Returns the `default` storage for
    /// `"default"`, or `None` for an invalid name (caller reports 22023).
    pub fn parse(name: &str, default: u8) -> Option<u8> {
        match name.to_ascii_lowercase().as_str() {
            "plain" => Some(PLAIN),
            "external" => Some(EXTERNAL),
            "extended" => Some(EXTENDED),
            "main" => Some(MAIN),
            "default" => Some(default),
            _ => None,
        }
    }
}

/// v0.37: per-value TOAST state for one toasted/compressed cell,
/// keyed by value id in `Table::toast_info`.
#[derive(Clone, Debug, Default)]
pub struct ToastInfo {
    /// The stored form is compressed.
    pub compressed: bool,
    /// v0.41: which compressor produced the stored bytes (PG19's
    /// `attcompression` method code). Meaningful only when
    /// `compressed` is true.
    pub method: ToastCompression,
}

/// v0.41: TOAST compression methods (PG19 `ToastCompressionId` /
/// `attcompression` codes in `toast_compression.h`). The `u8` code is
/// the on-wire/varlena method character: `b'p'` for PGLZ, `b'l'` for
/// LZ4. This is also which compressor TOAST uses for a column without
/// an explicit `COMPRESSION` method: PG19's `default_toast_compression`
/// defaults to `pglz` even in LZ4-enabled builds (`lz4` is opt-in per
/// column or via SET); we match that.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToastCompression {
    #[default]
    Pglz,
    Lz4,
}

impl ToastCompression {
    /// Method character, as PG stores it in `attcompression` and the
    /// compressed-varlena header.
    pub fn code(self) -> u8 {
        match self {
            ToastCompression::Pglz => b'p',
            ToastCompression::Lz4 => b'l',
        }
    }

    /// Method name, as `pg_column_compression()` reports it.
    pub fn name(self) -> &'static str {
        match self {
            ToastCompression::Pglz => "pglz",
            ToastCompression::Lz4 => "lz4",
        }
    }

    /// Parse a method name (case-insensitive), like PG's
    /// `CompressionNameToMethod`; `None` for unknown names.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pglz" => Some(ToastCompression::Pglz),
            "lz4" => Some(ToastCompression::Lz4),
            _ => None,
        }
    }

    /// Decode a stored method code; `None` for unknown codes.
    pub fn from_code(b: u8) -> Option<Self> {
        match b {
            b'p' => Some(ToastCompression::Pglz),
            b'l' => Some(ToastCompression::Lz4),
            _ => None,
        }
    }
}

/// v0.37: TOAST constants adapted from PG19 `access/heaptoast.h`.
/// PG's are page-derived (`MaximumBytesPerTuple(4)` with 8 KiB pages =
/// 2037); rustgres has no pages, so the numeric defaults are used
/// directly and documented here.
pub mod toast_consts {
    /// Row width (bytes of toastable data) above which the toaster runs.
    pub const TOAST_TUPLE_THRESHOLD: u32 = 2037;
    /// Default `toast_tuple_target`: row width the toaster aims for.
    pub const TOAST_TUPLE_TARGET: u32 = 2037;
    /// Minimum allowed `toast_tuple_target` (PG's reloption bound).
    pub const TOAST_TUPLE_TARGET_MIN: u32 = 128;
    /// Last-resort target for MAIN columns (PG's `TOAST_TUPLE_TARGET_MAIN`
    /// = `MaximumBytesPerTuple(1)` with 8 KiB pages = 8160).
    pub const TOAST_TUPLE_TARGET_MAIN: u32 = 8160;
    /// Maximum bytes per toast-table chunk (`TOAST_MAX_CHUNK_SIZE`,
    /// ~2 KiB so four chunk rows fit a page).
    pub const TOAST_MAX_CHUNK_SIZE: usize = 2001;
    /// First user-table OID, matching PG's `FirstNormalObjectId`.
    pub const FIRST_USER_OID: u32 = 16384;
}

/// Fixed-precision decimal (v0.7): value = `unscaled * 10^-scale`.
///
/// Always kept normalized (no trailing decimal zeros unless the value is
/// zero, which is `0/10^0`), so derived `PartialEq`/`Eq` compare
/// numerically. The `i128` mantissa holds ~38 significant digits;
/// operations that would exceed it fail with 22003 rather than silently
/// rounding — a deliberate v0.7 deviation from Postgres' arbitrary
/// precision (documented in the README).
///
/// v0.18: adds PostgreSQL's non-finite numerics. `special` distinguishes
/// `NaN`, `+Infinity`, `-Infinity` from finite values; the `unscaled` /
/// `scale` fields are always zero for non-finite values so derived
/// equality stays canonical.
///
/// v0.22: `scale` is signed. A negative scale means the value is an
/// integer multiple of a power of ten (`unscaled * 10^-scale`), which
/// lets huge magnitudes like `1.2345678901234e200` (from float8
/// conversion or `1e200`-style literals) be represented without
/// overflowing the i128 mantissa. Normalization still only strips
/// trailing zeros while `scale > 0`, so `new(1000, 0)` keeps scale 0
/// and prints as `1000`, never `1e+3`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Numeric {
    pub unscaled: i128,
    pub scale: i32,
    pub special: NumericSpecial,
}

/// Non-finite numeric kinds (v0.18), mirroring PostgreSQL's
/// `NUMERIC_NAN` / `NUMERIC_PINF` / `NUMERIC_NINF`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericSpecial {
    Finite,
    NaN,
    PosInf,
    NegInf,
}

impl Numeric {
    /// Build without normalizing (v0.18): for fixed-scale internals
    /// like the transcendental series that need exact scale control.
    fn raw(unscaled: i128, scale: i32) -> Self {
        Numeric {
            unscaled,
            scale,
            special: NumericSpecial::Finite,
        }
    }

    /// Build and normalize.
    pub fn new(unscaled: i128, scale: i32) -> Self {
        let mut n = Numeric {
            unscaled,
            scale,
            special: NumericSpecial::Finite,
        };
        n.normalize();
        n
    }

    /// NaN (v0.18).
    pub fn nan() -> Self {
        Numeric {
            unscaled: 0,
            scale: 0,
            special: NumericSpecial::NaN,
        }
    }

    /// +Infinity (v0.18).
    pub fn infinity() -> Self {
        Numeric {
            unscaled: 0,
            scale: 0,
            special: NumericSpecial::PosInf,
        }
    }

    /// -Infinity (v0.18).
    pub fn neg_infinity() -> Self {
        Numeric {
            unscaled: 0,
            scale: 0,
            special: NumericSpecial::NegInf,
        }
    }

    /// True for NaN / +/-Infinity (v0.18).
    #[allow(dead_code)]
    pub fn is_special(&self) -> bool {
        self.special != NumericSpecial::Finite
    }

    /// True for NaN (v0.18).
    pub fn is_nan(&self) -> bool {
        self.special == NumericSpecial::NaN
    }

    /// Sign of the value: -1, 0, +1; NaN reports 0 (v0.18).
    fn signum(&self) -> i8 {
        match self.special {
            NumericSpecial::NaN => 0,
            NumericSpecial::PosInf => 1,
            NumericSpecial::NegInf => -1,
            NumericSpecial::Finite => {
                if self.unscaled < 0 {
                    -1
                } else if self.unscaled > 0 {
                    1
                } else {
                    0
                }
            }
        }
    }

    /// Arithmetic negation (v0.18); NaN stays NaN, infinities flip.
    #[allow(dead_code)]
    pub fn neg(&self) -> Self {
        match self.special {
            NumericSpecial::NaN => Numeric::nan(),
            NumericSpecial::PosInf => Numeric::neg_infinity(),
            NumericSpecial::NegInf => Numeric::infinity(),
            NumericSpecial::Finite => {
                // `i128::MIN` is unreachable: parse() rejects magnitudes
                // that large and arithmetic is overflow-checked.
                Numeric::new(self.unscaled.saturating_neg(), self.scale)
            }
        }
    }

    fn normalize(&mut self) {
        if self.special != NumericSpecial::Finite {
            self.unscaled = 0;
            self.scale = 0;
            return;
        }
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
            special: NumericSpecial::Finite,
        }
    }

    pub fn from_i64(i: i64) -> Self {
        Numeric::new(i as i128, 0)
    }

    /// Parse a decimal literal: `[+-]digits[.digits][e[+-]digits]`.
    /// `Err(())` = not a number (caller maps to 22P02).
    /// `Err` with overflow is impossible here only if digits fit; too
    /// many digits yield `None` via the overflow flag (caller: 22003).
    /// v0.25: parse a non-decimal integer literal (`0b`/`0o`/`0x`, PG 16+)
    /// into a Numeric. `digits` is the part after the prefix; PG allows a
    /// single `_` right after the prefix, then underscores only between
    /// digits.
    fn parse_based(digits: &str, base: u32, neg: bool) -> Result<Self, NumericParseError> {
        let digits = digits.strip_prefix('_').unwrap_or(digits);
        let digit_ok = |c: u8| match base {
            2 => c == b'0' || c == b'1',
            8 => c.is_ascii_digit() && c < b'8',
            16 => c.is_ascii_hexdigit(),
            _ => false,
        };
        let mut saw_digit = false;
        let mut need_digit = true;
        let mut unscaled: i128 = 0;
        for &c in digits.as_bytes() {
            if c == b'_' {
                if need_digit {
                    return Err(NumericParseError::Syntax);
                }
                need_digit = true;
            } else if digit_ok(c) {
                saw_digit = true;
                need_digit = false;
                let d = (c as char).to_digit(base).unwrap() as i128;
                unscaled = unscaled
                    .checked_mul(base as i128)
                    .and_then(|v| v.checked_add(d))
                    .ok_or(NumericParseError::Overflow)?;
            } else {
                return Err(NumericParseError::Syntax);
            }
        }
        if !saw_digit || need_digit {
            return Err(NumericParseError::Syntax);
        }
        if neg {
            unscaled = -unscaled;
        }
        Ok(Numeric::new(unscaled, 0))
    }

    pub fn parse(s: &str) -> Result<Self, NumericParseError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(NumericParseError::Syntax);
        }
        let (neg, rest) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        // v0.18: PostgreSQL's non-finite numerics, case-insensitive.
        // v0.25: a sign before NaN is rejected (PG 16: `'+NaN'` is
        // 22P02); signed infinities are fine.
        let lower: String = rest.to_lowercase();
        if lower == "nan" {
            if neg || s.starts_with('+') {
                return Err(NumericParseError::Syntax);
            }
            return Ok(Numeric::nan());
        }
        if lower == "inf" || lower == "infinity" {
            return Ok(if neg {
                Numeric::neg_infinity()
            } else {
                Numeric::infinity()
            });
        }
        // v0.25: PG 16 non-decimal integer syntax and `_` digit
        // separators. A base prefix means the whole literal is an
        // integer (no fraction/exponent); underscores are stripped
        // after validating placement.
        let (base, rest) =
            if let Some(d) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
                (2u32, d)
            } else if let Some(d) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
                (8u32, d)
            } else if let Some(d) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
                (16u32, d)
            } else {
                (10u32, rest)
            };
        if base != 10 {
            return Self::parse_based(rest, base, neg);
        }
        // Split off an exponent before underscore stripping (the
        // exponent carries its own optional sign, e.g. `1e-1_0`).
        let (mant_raw, exp_raw) = match rest.find(['e', 'E']) {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let exp: i32 = if exp_raw.is_empty() {
            0
        } else {
            strip_underscores(exp_raw)
                .ok_or(NumericParseError::Syntax)?
                .parse()
                .map_err(|_| NumericParseError::Syntax)?
        };
        // Split the mantissa on '.', then strip underscores from each
        // part (empty parts are fine: `.5`, `5.`).
        let (int_raw, frac_raw) = match mant_raw.find('.') {
            Some(i) => (&mant_raw[..i], &mant_raw[i + 1..]),
            None => (mant_raw, ""),
        };
        let int_part = strip_underscores_opt(int_raw).ok_or(NumericParseError::Syntax)?;
        let frac_part = strip_underscores_opt(frac_raw).ok_or(NumericParseError::Syntax)?;
        let (int_part, frac_part) = (int_part.as_str(), frac_part.as_str());
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
        // scale = frac digits - exponent. v0.22: a negative scale is
        // kept as-is (value = unscaled * 10^-scale) instead of erroring,
        // so huge magnitudes like 1e200 survive in the i128 mantissa.
        // PostgreSQL allows up to 131072 digits before the decimal point
        // ("value overflows numeric format" beyond that); the fractional
        // side is capped symmetrically to keep every scale arithmetic
        // below in i32 range.
        let scale = frac_part.len() as i32 - exp;
        let int_digits = (int_part.len() + frac_part.len()) as i64 - scale as i64;
        if int_digits > 131072 || scale > 200_000 || scale < -200_000 {
            return Err(NumericParseError::Overflow);
        }
        Ok(Numeric::new(unscaled, scale))
    }

    /// Build from an f64's shortest round-trip representation, so
    /// `Numeric::from_f64(0.1)` is exactly 0.1. Non-finite and extreme
    /// magnitudes fail.
    pub fn from_f64(f: f64) -> Result<Self, NumericParseError> {
        // v0.25: PG's float->numeric cast maps non-finite floats to the
        // corresponding numeric special values (not an error).
        if f.is_nan() {
            return Ok(Numeric::nan());
        }
        if f.is_infinite() {
            return Ok(if f.is_sign_positive() {
                Numeric::infinity()
            } else {
                Numeric::neg_infinity()
            });
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
        match self.special {
            NumericSpecial::NaN => f64::NAN,
            NumericSpecial::PosInf => f64::INFINITY,
            NumericSpecial::NegInf => f64::NEG_INFINITY,
            NumericSpecial::Finite => self.unscaled as f64 * 10f64.powi(-self.scale),
        }
    }

    /// Round half away from zero to an integer; overflow or a special
    /// value -> None.
    pub fn to_i64(&self) -> Option<i64> {
        if self.special != NumericSpecial::Finite {
            return None;
        }
        // v0.22: a non-positive scale means the value is already an
        // integer multiple of 10^-scale.
        if self.scale <= 0 {
            let mul = 10i128.checked_pow((-self.scale) as u32)?;
            return i64::try_from(self.unscaled.checked_mul(mul)?).ok();
        }
        let half = 10i128.checked_pow(self.scale as u32)? / 2;
        let div = 10i128.checked_pow(self.scale as u32)?;
        let rounded = if self.unscaled >= 0 {
            self.unscaled.checked_add(half)? / div
        } else {
            self.unscaled.checked_sub(half)? / div
        };
        i64::try_from(rounded).ok()
    }

    /// v0.18: true for finite zero (specials are never zero).
    #[allow(dead_code)]
    pub fn is_zero(&self) -> bool {
        self.special == NumericSpecial::Finite && self.unscaled == 0
    }

    /// Align `other` to our scale; used by add/sub. Overflow -> None.
    fn aligned(&self, other: &Numeric) -> Option<(i128, i128, i32)> {
        let scale = self.scale.max(other.scale);
        let up = |n: &Numeric| {
            n.unscaled
                .checked_mul(10i128.checked_pow((scale - n.scale) as u32)?)
        };
        if let (Some(a), Some(b)) = (up(self), up(other)) {
            return Some((a, b, scale));
        }
        // v0.22: the scales differ by more than i128 can bridge (one
        // operand is a huge multiple of a power of ten). Align at the
        // coarser scale instead, rounding the finer operand half away
        // from zero. The dropped fraction is unrepresentable in i128
        // anyway; this keeps e.g. `0 - 1e308` = `-1e308` instead of
        // erroring with 22003.
        let coarse = self.scale.min(other.scale);
        let a = self.round_to(coarse)?.unscaled;
        let b = other.round_to(coarse)?.unscaled;
        Some((a, b, coarse))
    }

    /// Addition with PostgreSQL's non-finite semantics (v0.18): NaN
    /// propagates; `Inf + -Inf` is NaN; infinities otherwise dominate.
    pub fn checked_add(&self, other: &Numeric) -> Option<Numeric> {
        match (self.special, other.special) {
            (NumericSpecial::NaN, _) | (_, NumericSpecial::NaN) => Some(Numeric::nan()),
            (NumericSpecial::Finite, NumericSpecial::Finite) => {
                let (a, b, scale) = self.aligned(other)?;
                Some(Numeric::new(a.checked_add(b)?, scale))
            }
            (a, b) => {
                if (a == NumericSpecial::PosInf && b == NumericSpecial::NegInf)
                    || (a == NumericSpecial::NegInf && b == NumericSpecial::PosInf)
                {
                    return Some(Numeric::nan());
                }
                Some(
                    if a == NumericSpecial::PosInf || b == NumericSpecial::PosInf {
                        Numeric::infinity()
                    } else {
                        Numeric::neg_infinity()
                    },
                )
            }
        }
    }

    /// Subtraction via negation (v0.18 keeps PG's special-value rules).
    pub fn checked_sub(&self, other: &Numeric) -> Option<Numeric> {
        self.checked_add(&other.neg())
    }

    /// Multiplication with PostgreSQL's non-finite semantics (v0.18):
    /// NaN propagates; `0 * Inf` is NaN; infinities otherwise dominate
    /// with the product's sign.
    pub fn checked_mul(&self, other: &Numeric) -> Option<Numeric> {
        match (self.special, other.special) {
            (NumericSpecial::NaN, _) | (_, NumericSpecial::NaN) => Some(Numeric::nan()),
            (NumericSpecial::Finite, NumericSpecial::Finite) => Some(Numeric::new(
                self.unscaled.checked_mul(other.unscaled)?,
                self.scale.checked_add(other.scale)?,
            )),
            _ => {
                if self.is_zero() || other.is_zero() {
                    return Some(Numeric::nan());
                }
                let neg = (self.signum() < 0) != (other.signum() < 0);
                Some(if neg {
                    Numeric::neg_infinity()
                } else {
                    Numeric::infinity()
                })
            }
        }
    }

    /// Division with 10 guard digits after the decimal point
    /// (documented v0.7 fixed scale). Division by zero -> None with the
    /// `is_zero` flag distinguishable by the caller via `other.is_zero()`.
    /// v0.18: NaN propagates; `finite / Inf` is 0; `Inf / Inf` is NaN;
    /// `Inf / finite` is a signed infinity; division by zero stays None.
    pub fn checked_div(&self, other: &Numeric) -> Option<Numeric> {
        use NumericSpecial::*;
        match (self.special, other.special) {
            (NaN, _) | (_, NaN) => Some(Numeric::nan()),
            (_, Finite) if other.unscaled == 0 => None,
            (Finite, PosInf) | (Finite, NegInf) => Some(Numeric::zero()),
            (PosInf, Finite) | (NegInf, Finite) => {
                let neg = (self.special == NegInf) != (other.unscaled < 0);
                Some(if neg {
                    Numeric::neg_infinity()
                } else {
                    Numeric::infinity()
                })
            }
            (PosInf, PosInf) | (PosInf, NegInf) | (NegInf, PosInf) | (NegInf, NegInf) => {
                Some(Numeric::nan())
            }
            (Finite, Finite) => {
                // a/b = (ua * 10^(sb+10)) / (ub * 10^sa), scale 10.
                // v0.22: scales are signed; a negative power is out of
                // range for this fixed-scale path (None -> 22003).
                let num = self
                    .unscaled
                    .checked_mul(10i128.checked_pow(u32::try_from(other.scale + 10).ok()?)?)?;
                let den = other
                    .unscaled
                    .checked_mul(10i128.checked_pow(u32::try_from(self.scale).ok()?)?)?;
                Some(Numeric::new(num.checked_div(den)?, 10))
            }
        }
    }

    /// v0.18: Divide at a specific output scale, rescaling inputs to avoid
    /// overflow. Returns None on overflow or division by zero.
    /// v0.22: `out_scale` is signed (callers pass non-negative scales).
    pub fn div_at_scale(&self, other: &Numeric, out_scale: i32) -> Option<Numeric> {
        use NumericSpecial::*;
        match (self.special, other.special) {
            (NaN, _) | (_, NaN) => Some(Numeric::nan()),
            (_, Finite) if other.unscaled == 0 => None,
            (Finite, PosInf) | (Finite, NegInf) => Some(Numeric::zero()),
            (PosInf, Finite) | (NegInf, Finite) => {
                let neg = (self.special == NegInf) != (other.unscaled < 0);
                Some(if neg {
                    Numeric::neg_infinity()
                } else {
                    Numeric::infinity()
                })
            }
            (PosInf, PosInf) | (PosInf, NegInf) | (NegInf, PosInf) | (NegInf, NegInf) => {
                Some(Numeric::nan())
            }
            (Finite, Finite) => {
                // Use f64 for the division to avoid overflow complexities.
                // This gives ~15-16 significant digits, sufficient for out_scale <= 16.
                let ratio = self.to_f64() / other.to_f64();
                if !ratio.is_finite() {
                    return None;
                }
                let scaled = (ratio * 10f64.powi(out_scale)).round() as i128;
                Some(Numeric::new(scaled, out_scale))
            }
        }
    }

    /// Remainder, sign follows the dividend (like Rust's `%` and PG's
    /// numeric mod). Division by zero -> None. v0.18: NaN propagates and
    /// any infinite operand yields NaN, like PostgreSQL.
    pub fn checked_rem(&self, other: &Numeric) -> Option<Numeric> {
        use NumericSpecial::*;
        match (self.special, other.special) {
            (NaN, _) | (_, NaN) => Some(Numeric::nan()),
            (_, Finite) if other.unscaled == 0 => None,
            (Finite, Finite) => {
                let (a, b, scale) = self.aligned(other)?;
                Some(Numeric::new(a.checked_rem(b)?, scale))
            }
            _ => Some(Numeric::nan()),
        }
    }

    /// Absolute value (v0.18): NaN stays NaN, -Infinity becomes +Infinity.
    pub fn abs(&self) -> Numeric {
        match self.special {
            NumericSpecial::NaN => Numeric::nan(),
            NumericSpecial::NegInf => Numeric::infinity(),
            _ => {
                if self.special == NumericSpecial::Finite {
                    Numeric::new(self.unscaled.abs(), self.scale)
                } else {
                    Numeric::infinity()
                }
            }
        }
    }

    /// Round to `scale` fractional digits, half away from zero.
    /// v0.18: specials round to themselves (PostgreSQL).
    /// v0.22: `scale` is signed. Upscaling that would overflow the i128
    /// mantissa keeps the numerically-equal narrower representation
    /// (PostgreSQL would widen the scale; the value is identical, and
    /// this is what lets `round(1e200)` succeed).
    /// Digits before the decimal point (<= 0 when |value| < 1).
    /// Used to enforce the 131072-digit numeric format limit after
    /// operations that can carry into a new leading digit.
    fn int_digits(&self) -> i64 {
        if self.unscaled == 0 {
            return 0;
        }
        let mut v = self.unscaled.unsigned_abs();
        let mut digits: i64 = 0;
        while v > 0 {
            v /= 10;
            digits += 1;
        }
        digits - self.scale as i64
    }

    pub fn round_to(&self, scale: i32) -> Option<Numeric> {
        if self.special != NumericSpecial::Finite {
            return Some(self.clone());
        }
        if scale >= self.scale {
            if scale == self.scale {
                return Some(self.clone());
            }
            let mul = 10i128.checked_pow((scale - self.scale) as u32);
            match mul {
                Some(m) => {
                    return Some(Numeric::new(self.unscaled.checked_mul(m)?, scale));
                }
                None => return Some(self.clone()),
            }
        }
        let drop = (self.scale - scale) as u32;
        // v0.22: if 10^drop overflows i128, then |unscaled| < 10^39/2 <=
        // div/2, so the value rounds to zero at any target scale.
        let div = match 10i128.checked_pow(drop) {
            Some(d) => d,
            None => return Some(Numeric::zero()),
        };
        let half = div / 2;
        let adj = if self.unscaled >= 0 { half } else { -half };
        let r = Numeric::new(self.unscaled.checked_add(adj)?.checked_div(div)?, scale);
        // v0.22: rounding up can carry past the numeric format limit
        // (131072 digits before the point), e.g.
        // round(5.5e131071, -131072) = 1e131072. PostgreSQL raises
        // "value overflows numeric format"; we return None (-> 22003).
        if r.int_digits() > 131072 {
            return None;
        }
        Some(r)
    }

    /// v0.18: specials are fixed points of floor/ceil.
    /// v0.22: a non-positive scale is already integral.
    pub fn floor(&self) -> Option<Numeric> {
        if self.special != NumericSpecial::Finite {
            return Some(self.clone());
        }
        if self.scale <= 0 {
            return Some(self.clone());
        }
        // v0.22: if 10^scale overflows i128, |value| < 1.
        let div = match 10i128.checked_pow(self.scale as u32) {
            Some(d) => d,
            None => return Some(Numeric::new(if self.unscaled < 0 { -1 } else { 0 }, 0)),
        };
        let q = self.unscaled.checked_div(div)?;
        let r = self.unscaled.checked_rem(div)?;
        let q = if r != 0 && self.unscaled < 0 {
            q - 1
        } else {
            q
        };
        Some(Numeric::new(q, 0))
    }

    /// v0.18: specials are fixed points of floor/ceil.
    /// v0.22: a non-positive scale is already integral.
    pub fn ceil(&self) -> Option<Numeric> {
        if self.special != NumericSpecial::Finite {
            return Some(self.clone());
        }
        if self.scale <= 0 {
            return Some(self.clone());
        }
        // v0.22: if 10^scale overflows i128, |value| < 1.
        let div = match 10i128.checked_pow(self.scale as u32) {
            Some(d) => d,
            None => return Some(Numeric::new(if self.unscaled > 0 { 1 } else { 0 }, 0)),
        };
        let q = self.unscaled.checked_div(div)?;
        let r = self.unscaled.checked_rem(div)?;
        let q = if r != 0 && self.unscaled > 0 {
            q + 1
        } else {
            q
        };
        Some(Numeric::new(q, 0))
    }

    /// Square root via f64 (documented precision limit: ~15-16
    /// significant digits). Negative finite -> None (caller: 2201F).
    /// v0.18: NaN -> NaN, +Infinity -> +Infinity, -Infinity -> None.
    pub fn sqrt(&self) -> Option<Numeric> {
        match self.special {
            NumericSpecial::NaN => return Some(Numeric::nan()),
            NumericSpecial::PosInf => return Some(Numeric::infinity()),
            NumericSpecial::NegInf => return None,
            NumericSpecial::Finite => {}
        }
        let f = self.to_f64();
        if f < 0.0 {
            return None;
        }
        Numeric::from_f64(f.sqrt()).ok()
    }

    /// Exact power for integer exponents (repeated squaring). `None`
    /// on overflow or absurd exponents (callers fall back to f64 or
    /// raise 22003). Negative exponents divide, with 10 guard digits.
    /// v0.18: NaN base -> NaN; infinite base follows PG's sign rules;
    /// `0^0` is 1 (PostgreSQL).
    pub fn pow(&self, exp: i64) -> Option<Numeric> {
        match self.special {
            NumericSpecial::NaN => return Some(Numeric::nan()),
            NumericSpecial::PosInf => {
                return Some(if exp == 0 {
                    Numeric::new(1, 0)
                } else if exp > 0 {
                    Numeric::infinity()
                } else {
                    Numeric::zero()
                });
            }
            NumericSpecial::NegInf => {
                return Some(if exp == 0 {
                    Numeric::new(1, 0)
                } else if exp > 0 {
                    if exp % 2 == 0 {
                        Numeric::infinity()
                    } else {
                        Numeric::neg_infinity()
                    }
                } else {
                    Numeric::zero()
                });
            }
            NumericSpecial::Finite => {}
        }
        if exp == 0 {
            return Some(Numeric::new(1, 0));
        }
        if self.is_zero() && exp < 0 {
            // 0 raised to a negative power: division by zero.
            return None;
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

    /// Total order with PostgreSQL's non-finite rules (v0.18):
    /// `-Infinity < finite < +Infinity < NaN`; NaN equals NaN.
    pub fn cmp(&self, other: &Numeric) -> std::cmp::Ordering {
        use NumericSpecial::*;
        use std::cmp::Ordering;
        match (self.special, other.special) {
            (NaN, NaN) => return Ordering::Equal,
            (NaN, _) => return Ordering::Greater,
            (_, NaN) => return Ordering::Less,
            (NegInf, NegInf) | (PosInf, PosInf) => return Ordering::Equal,
            (NegInf, _) => return Ordering::Less,
            (_, NegInf) => return Ordering::Greater,
            (PosInf, _) => return Ordering::Greater,
            (_, PosInf) => return Ordering::Less,
            (Finite, Finite) => {}
        }
        if self.unscaled == 0 && other.unscaled == 0 {
            return Ordering::Equal;
        }
        let neg_a = self.unscaled < 0;
        let neg_b = other.unscaled < 0;
        if neg_a != neg_b {
            return if neg_a {
                Ordering::Less
            } else {
                Ordering::Greater
            };
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
    /// v0.18: specials render as PostgreSQL does (`NaN`, `Infinity`,
    /// `-Infinity`).
    /// v0.22: a negative scale (integer multiple of a power of ten)
    /// prints in plain notation while the integer part stays short,
    /// and switches to scientific notation for very large magnitudes
    /// (PostgreSQL prints `round(1.2345678901234e200)` as
    /// `1.2345678901234e+200`). The plain/scientific cutoff is an
    /// internal approximation; PostgreSQL's exact threshold is not
    /// publicly documented.
    pub fn to_text(&self) -> String {
        match self.special {
            NumericSpecial::NaN => return "NaN".to_string(),
            NumericSpecial::PosInf => return "Infinity".to_string(),
            NumericSpecial::NegInf => return "-Infinity".to_string(),
            NumericSpecial::Finite => {}
        }
        if self.unscaled == 0 {
            return "0".to_string();
        }
        let neg = self.unscaled < 0;
        let digits = self.unscaled.unsigned_abs().to_string();
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        if self.scale < 0 {
            // Integer multiple of 10^-scale.
            let int_digits = digits.len() as i64 - self.scale as i64;
            if int_digits <= 39 {
                out.push_str(&digits);
                for _ in 0..-self.scale {
                    out.push('0');
                }
            } else {
                out.push_str(&digits[..1]);
                if digits.len() > 1 {
                    out.push('.');
                    out.push_str(&digits[1..]);
                }
                let exp = digits.len() as i64 - 1 - self.scale as i64;
                out.push('e');
                if exp >= 0 {
                    out.push('+');
                }
                out.push_str(&exp.to_string());
            }
            return out;
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

/// v0.25: like `strip_underscores`, but an empty part (as in `.5` or
/// `5.`) is allowed.
fn strip_underscores_opt(s: &str) -> Option<String> {
    if s.is_empty() {
        return Some(String::new());
    }
    strip_underscores(s)
}

/// v0.25: validate and remove `_` digit separators (PG 16+). Underscores
/// must sit between digits; a single leading underscore is allowed (the
/// caller strips a base prefix first, so it means the underscore followed
/// the prefix). Returns the cleaned string, preserving a leading sign.
fn strip_underscores(s: &str) -> Option<String> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => match s.strip_prefix('+') {
            Some(d) => (false, d),
            None => (false, s),
        },
    };
    // PG allows one underscore right after a stripped base prefix.
    let digits = digits.strip_prefix('_').unwrap_or(digits);
    let mut out = String::with_capacity(digits.len());
    let mut saw_digit = false;
    let mut need_digit = true; // leading underscore rejected
    for &c in digits.as_bytes() {
        if c == b'_' {
            if need_digit {
                return None;
            }
            need_digit = true;
        } else if c.is_ascii_digit() {
            saw_digit = true;
            need_digit = false;
            out.push(c as char);
        } else {
            return None;
        }
    }
    if !saw_digit || need_digit {
        return None;
    }
    if neg {
        out.insert(0, '-');
    }
    Some(out)
}

/// v0.18: fixed scale-18 decimal for transcendental series evaluation.
/// f64 carries only ~15.95 decimal digits, but PostgreSQL's numeric
/// `exp`/`ln`/`log`/`power` round to 16 fractional digits, so the 16th
/// digit must be computed correctly. A scale-18 fixed-point decimal in
/// i128 gives ~18 digits with headroom; multiplications stay within
/// i128 as long as operands are < ~4e18 (values < 4), which the series
/// below guarantee by argument reduction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HpDec(i128);

/// 10^18, the HpDec unit.
const HP_ONE: i128 = 1_000_000_000_000_000_000;

/// v0.18: Scale-36 ultra-high-precision decimal for transcendental series.
/// Used where scale-18 HpDec loses too many digits (e.g. exp(f)×2^k
/// amplifies truncation). Multiplication uses 18-digit half splitting
/// to stay in i128 range.
#[derive(Clone, Copy, Debug)]
struct Hp36(i128);

const HP36_ONE: i128 = 1_000_000_000_000_000_000_000_000_000_000_000_000; // 10^36
const HP36_HALF: i128 = 1_000_000_000_000_000_000; // 10^18

impl Hp36 {
    fn one() -> Self {
        Hp36(HP36_ONE)
    }
    fn add(self, o: Hp36) -> Option<Hp36> {
        Some(Hp36(self.0.checked_add(o.0)?))
    }
    fn sub(self, o: Hp36) -> Option<Hp36> {
        Some(Hp36(self.0.checked_sub(o.0)?))
    }
    /// Multiply with 18-digit splitting: a×b/10^36 stays in i128
    /// for halves < ~4e18 (values < ~4 at scale 36).
    fn mul(self, o: Hp36) -> Option<Hp36> {
        let neg = (self.0 < 0) != (o.0 < 0);
        let a = self.0.unsigned_abs();
        let b = o.0.unsigned_abs();
        let half = HP36_HALF as u128;
        let a_hi = a / half;
        let a_lo = a % half;
        let b_hi = b / half;
        let b_lo = b % half;
        // a×b/10^36 = a_hi×b_hi + (a_hi×b_lo + a_lo×b_hi)/10^18 + a_lo×b_lo/10^36
        let t0 = a_hi.checked_mul(b_hi)?;
        let t1 = a_hi
            .checked_mul(b_lo)?
            .checked_add(a_lo.checked_mul(b_hi)?)?
            / half;
        let t2 = a_lo.checked_mul(b_lo)? / HP36_ONE as u128;
        let p = t0.checked_add(t1)?.checked_add(t2)?;
        if p > i128::MAX as u128 {
            return None;
        }
        let v = p as i128;
        Some(Hp36(if neg { -v } else { v }))
    }
    fn div_int(self, d: i128) -> Option<Hp36> {
        if d == 0 {
            return None;
        }
        Some(Hp36(self.0.checked_div(d)?))
    }
    fn cmp_abs(self, o: Hp36) -> std::cmp::Ordering {
        self.0.unsigned_abs().cmp(&o.0.unsigned_abs())
    }
    /// Convert from scale-18 HpDec.
    #[allow(dead_code)]
    fn from_hp(h: HpDec) -> Option<Hp36> {
        Some(Hp36(h.0.checked_mul(HP36_HALF)?))
    }
    /// Convert from a finite Numeric, rounding to 36 fractional digits.
    #[allow(dead_code)]
    fn from_numeric(n: &Numeric) -> Option<Hp36> {
        if n.special != NumericSpecial::Finite {
            return None;
        }
        if n.scale <= 36 {
            let mul = 10i128.checked_pow((36 - n.scale) as u32)?;
            Some(Hp36(n.unscaled.checked_mul(mul)?))
        } else {
            let div = 10i128.checked_pow((n.scale - 36) as u32)?;
            let half = div / 2;
            let q = if n.unscaled >= 0 {
                (n.unscaled + half) / div
            } else {
                (n.unscaled - half) / div
            };
            Some(Hp36(q))
        }
    }
    /// Convert to scale-18 HpDec (rounding to nearest).
    #[allow(dead_code)]
    fn to_hp(self) -> HpDec {
        let q = self.0 / HP36_HALF;
        let r = self.0 % HP36_HALF;
        let adj = if r.abs() >= HP36_HALF / 2 {
            if self.0 < 0 { -1 } else { 1 }
        } else {
            0
        };
        HpDec(q + adj)
    }
}

/// v0.18: parses a "d.dddd" literal to scale-36, rounding to 36 digits.
fn hp36_const(s: &str) -> Hp36 {
    let mut parts = s.split('.');
    let int: i128 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let frac = parts.next().unwrap_or("");
    let frac36: String = if frac.len() > 36 {
        let (keep, rest) = frac.split_at(36);
        let mut v: i128 = keep.parse().unwrap_or(0);
        if rest.chars().next().unwrap_or('0') >= '5' {
            v += 1;
        }
        v.to_string()
    } else {
        frac.to_string()
    };
    let mut f: i128 = frac36.parse().unwrap_or(0);
    for _ in frac36.len()..36 {
        f = f.saturating_mul(10);
    }
    Hp36(int.saturating_mul(HP36_ONE).saturating_add(f))
}

impl HpDec {
    #[allow(dead_code)]
    fn zero() -> Self {
        HpDec(0)
    }

    fn one() -> Self {
        HpDec(HP_ONE)
    }

    /// Round a finite Numeric to scale 18. None on overflow.
    #[allow(dead_code)]
    fn from_numeric(n: &Numeric) -> Option<Self> {
        if n.special != NumericSpecial::Finite {
            return None;
        }
        if n.scale <= 18 {
            let mul = 10i128.checked_pow((18 - n.scale) as u32)?;
            Some(HpDec(n.unscaled.checked_mul(mul)?))
        } else {
            let div = 10i128.checked_pow((n.scale - 18) as u32)?;
            let half = div / 2;
            let adj = if n.unscaled >= 0 { half } else { -half };
            Some(HpDec(n.unscaled.checked_add(adj)?.checked_div(div)?))
        }
    }

    fn to_numeric(self) -> Numeric {
        Numeric::new(self.0, 18)
    }

    #[allow(dead_code)]
    fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[allow(dead_code)]
    fn neg(self) -> Self {
        HpDec(-self.0)
    }

    fn add(self, o: HpDec) -> Option<HpDec> {
        Some(HpDec(self.0.checked_add(o.0)?))
    }

    fn sub(self, o: HpDec) -> Option<HpDec> {
        Some(HpDec(self.0.checked_sub(o.0)?))
    }

    /// (a*b)/10^18. Caller keeps |operands| < 4e18.
    fn mul(self, o: HpDec) -> Option<HpDec> {
        Some(HpDec(self.0.checked_mul(o.0)?.checked_div(HP_ONE)?))
    }

    /// (a/b) at scale 18. Caller keeps |a| < 1.7e20 and b != 0.
    fn div(self, o: HpDec) -> Option<HpDec> {
        if o.0 == 0 {
            return None;
        }
        Some(HpDec(self.0.checked_mul(HP_ONE)?.checked_div(o.0)?))
    }

    /// Divide by a small integer.
    fn div_int(self, d: i128) -> Option<HpDec> {
        if d == 0 {
            return None;
        }
        Some(HpDec(self.0.checked_div(d)?))
    }

    /// Multiply by a small integer.
    fn mul_int(self, m: i128) -> Option<HpDec> {
        Some(HpDec(self.0.checked_mul(m)?))
    }

    fn cmp_abs(self, o: HpDec) -> std::cmp::Ordering {
        self.0.unsigned_abs().cmp(&o.0.unsigned_abs())
    }
}

/// ln(2) to 24 digits (v0.18).
const HP_LN2: &str = "0.6931471805599453094172321";
/// ln(10) to 24 digits (v0.18).
const HP_LN10: &str = "2.3025850929940456840179915";

fn hp_const(s: &str) -> HpDec {
    // Parses a "d.dddd" literal, rounding the fraction to 18 digits.
    let mut parts = s.split('.');
    let int: i128 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let frac = parts.next().unwrap_or("");
    let frac18: String = if frac.len() > 18 {
        // Round to 18 digits.
        let (keep, rest) = frac.split_at(18);
        let mut v: i128 = keep.parse().unwrap_or(0);
        if rest.chars().next().unwrap_or('0') >= '5' {
            v += 1;
        }
        v.to_string()
    } else {
        frac.to_string()
    };
    let mut f: i128 = frac18.parse().unwrap_or(0);
    for _ in frac18.len()..18 {
        f *= 10;
    }
    HpDec(int * HP_ONE + f)
}

impl Numeric {
    /// v0.18: e^self via Taylor series with `2^k` range reduction.
    /// Returns `(mantissa, exp10)` with value = mantissa × 10^exp10,
    /// mantissa in [1,10) at scale 18 (or zero for deep underflow).
    /// `None` when e^self overflows i128 range (self > ~87.3).
    /// Only finite inputs are accepted.
    ///
    /// Range reduction avoids the error-doubling of repeated squaring:
    /// k = round(x/ln2), f = x - k·ln2 (|f| ≤ 0.35), e^x = 2^k · e^f,
    /// and e^f comes from a short Taylor series with no squaring.
    pub fn exp_sci(&self) -> Option<(Numeric, i32)> {
        if self.special != NumericSpecial::Finite {
            return None;
        }
        let x = self.to_f64();
        if x > 87.3 {
            return None;
        }
        if x < -60.0 {
            return Some((Numeric::zero(), 0));
        }
        // k = round(x / ln2); f = x - k*ln2, computed at scale 36.
        let k = (x / std::f64::consts::LN_2).round() as i64;
        let x_h = Hp36::from_numeric(self)?;
        // k*ln2 at scale 36: k up to ±126, fine.
        let k_ln2 = Hp36(
            hp36_const("0.693147180559945309417232121458176568")
                .0
                .checked_mul(k as i128)?,
        );
        let f = x_h.sub(k_ln2)?;
        // Taylor at scale 36: sum f^n / n!, stopping when terms vanish.
        let mut sum = Hp36::one();
        let mut term = Hp36::one();
        let mut n: i128 = 1;
        loop {
            term = term.mul(f)?.div_int(n)?;
            sum = sum.add(term)?;
            n += 1;
            if term.cmp_abs(Hp36(10)) == std::cmp::Ordering::Less || n > 300 {
                break;
            }
        }
        // 2^k as M × 10^E with M an Hp36 in [1,10) (M.0 has 37 digits),
        // computed at scale 36 so the final multiplication keeps full precision.
        let (m2, e2) = if k >= 0 {
            let v = 2i128.checked_pow(k as u32)?;
            let digits = v.to_string().len() as i32;
            let shift = digits - 37;
            let m2_0 = if shift >= 0 {
                let div = 10i128.checked_pow(shift as u32)?;
                (v + div / 2) / div
            } else {
                v.checked_mul(10i128.checked_pow((-shift) as u32)?)?
            };
            (Hp36(m2_0), digits - 1)
        } else {
            // 2^k = 1/2^|k|: start from 10^36/2^|k| (scale-36 value < 1)
            // and scale up to 37 digits.
            let v = 2i128.checked_pow((-k) as u32)?;
            let mut m = 10i128.checked_pow(36)?.checked_div(v)?;
            let mut t: i32 = 0;
            while m < HP36_ONE && m > 0 {
                m = m.checked_mul(10)?;
                t += 1;
            }
            // value = m × 10^(-t) as an Hp36 (m has 37 digits).
            (Hp36(m), -t)
        };
        // result = sum × m2 at scale 36; value = result.0 × 10^(e2-36).
        let r36 = sum.mul(m2)?;
        if r36.0 == 0 {
            return Some((Numeric::zero(), 0));
        }
        // Extract 19-digit mantissa and e10.
        let d = r36.0.to_string().len() as i32; // digits (r36.0 > 0)
        let div = 10i128.checked_pow((d - 19) as u32)?;
        let rm = (r36.0 + div / 2) / div; // 19 digits, [10^18, 10^19)
        let re10 = (e2 - 36) + (d - 19) + 18;
        // Normalize rm into [10^18, 10^19) (rounding may have pushed it out).
        let (rm, re10) = if rm >= 10 * HP_ONE {
            (rm / 10, re10 + 1)
        } else if rm < HP_ONE {
            (rm * 10, re10 - 1)
        } else {
            (rm, re10)
        };
        Some((Numeric::raw(rm, 18), re10))
    }

    /// v0.18: Rescale to `new_scale`, rounding half away from zero.
    /// v0.22: `new_scale` is signed; delegates to `round_to` (upscaling
    /// past i128 keeps the numerically-equal narrower form).
    pub fn rescale(&self, new_scale: i32) -> Option<Numeric> {
        self.round_to(new_scale)
    }

    /// v0.18: Truncate toward zero to `new_scale` (no rounding).
    /// v0.22: `new_scale` is signed. If the divisor would overflow i128
    /// the truncated value is necessarily zero (|unscaled| < divisor).
    /// v0.56: use i64 arithmetic for the scale difference — with a very
    /// negative `new_scale` (e.g. `trunc(x, -2147483648)`) the i32
    /// subtraction `self.scale - new_scale` could overflow and panic in
    /// debug builds. |unscaled| < 10^39, so a divisor of 10^39 or more
    /// always truncates to zero.
    pub fn trunc_to_scale(&self, new_scale: i32) -> Numeric {
        if self.special != NumericSpecial::Finite {
            return self.clone();
        }
        if new_scale >= self.scale {
            return self.clone();
        }
        let diff = self.scale as i64 - new_scale as i64;
        if diff >= 39 {
            return Numeric::new(0, new_scale);
        }
        let div = 10i128.pow(diff as u32);
        Numeric::new(self.unscaled / div, new_scale)
    }

    /// v0.18: Convert a (mantissa, e10) scientific value to a Numeric
    /// at `out_scale`, rounding to nearest. Returns None on overflow.
    /// The mantissa is expected at scale 18 (as from exp_sci).
    /// v0.22: `out_scale` is signed.
    pub fn from_sci(mantissa: &Numeric, e10: i32, out_scale: i32) -> Option<Numeric> {
        if mantissa.special != NumericSpecial::Finite {
            return Some(mantissa.clone());
        }
        // value = mantissa.unscaled × 10^(e10-18).
        // Want: unscaled_out × 10^-out_scale.
        // unscaled_out = mantissa.unscaled × 10^(e10-18+out_scale).
        let shift = e10 - 18 + out_scale;
        let unscaled = if shift >= 0 {
            mantissa
                .unscaled
                .checked_mul(10i128.checked_pow(shift as u32)?)?
        } else {
            let div = 10i128.checked_pow((-shift) as u32)?;
            let half = div / 2;
            if mantissa.unscaled >= 0 {
                (mantissa.unscaled + half) / div
            } else {
                (mantissa.unscaled - half) / div
            }
        };
        Some(Numeric::new(unscaled, out_scale))
    }

    /// v0.18: ln(self) at scale 18 via range reduction + atanh series.

    /// v0.18: ln(self) at scale 18 for finite self > 0. None otherwise.
    /// Range reduction: self = d × 10^e10, ln = ln(d) + e10×ln(10),
    /// ln(d) via k×ln2 + 2×atanh((m-1)/(m+1)) with m in [1,2).
    pub fn ln_hp(&self) -> Option<Numeric> {
        if self.special != NumericSpecial::Finite || self.unscaled <= 0 {
            return None;
        }
        // Scientific split: self = d × 10^e10, d in [1, 10).
        let digits = self.unscaled.unsigned_abs().to_string().len() as i32;
        let e10 = (digits - 1) - self.scale;
        // d at scale 18: round(unscaled × 10^(18-(digits-1))).
        let shift = 18 - (digits - 1);
        let d_raw: i128 = if shift >= 0 {
            let mul = 10i128.checked_pow(shift as u32)?;
            self.unscaled.checked_mul(mul)?.unsigned_abs() as i128
        } else {
            let div = 10i128.checked_pow((-shift) as u32)?;
            let half = div / 2;
            let q = if self.unscaled >= 0 {
                (self.unscaled + half) / div
            } else {
                (self.unscaled - half) / div
            };
            q.unsigned_abs() as i128
        };
        let d = HpDec(d_raw);
        // k = floor(log2(d)), m = d / 2^k in [1, 2).
        let d_f = d_raw as f64 / 1e18;
        let k = d_f.log2().floor() as i32;
        let m = d.div_int(1i128 << k)?;
        // y = (m-1)/(m+1); ln(m) = 2*(y + y^3/3 + y^5/5 + ...).
        let y = m.sub(HpDec::one())?.div(m.add(HpDec::one())?)?;
        let y2 = y.mul(y)?;
        let mut sum = y;
        let mut num = y; // y^(2k+1)
        let mut n: i128 = 1;
        loop {
            num = num.mul(y2)?; // y^(2k+3)
            n += 2;
            let term = num.div_int(n)?;
            if term.cmp_abs(HpDec(10)) == std::cmp::Ordering::Less || n > 300 {
                sum = sum.add(term)?;
                break;
            }
            sum = sum.add(term)?;
        }
        let ln_m = sum.mul_int(2)?;
        let ln_d = ln_m.add(hp_const(HP_LN2).mul_int(k as i128)?)?;
        let result = ln_d.add(hp_const(HP_LN10).mul_int(e10 as i128)?)?;
        Some(result.to_numeric())
    }

    /// v0.37: byte size of this numeric for TOAST accounting. Uses the
    /// binary encoding size (unscaled i128 + scale i32 + discriminant).
    pub fn toast_len(&self) -> usize {
        16 + 4 + 1
    }

    /// v0.37: raw bytes of this numeric for TOAST compression/chunking:
    /// big-endian unscaled + scale + special discriminant.
    pub fn toast_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(21);
        b.extend_from_slice(&self.unscaled.to_be_bytes());
        b.extend_from_slice(&self.scale.to_be_bytes());
        b.push(match self.special {
            NumericSpecial::Finite => 0,
            NumericSpecial::NaN => 1,
            NumericSpecial::PosInf => 2,
            NumericSpecial::NegInf => 3,
        });
        b
    }
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

// ---------------------------------------------------------------------------
// v0.56: exact decimal arithmetic for `width_bucket` (PG19 semantics).
//
// PG's width_bucket computes the in-range bucket as
// floor((op - b1) / (b2 - b1) * count) + 1 exactly: the regression tests
// prove the float8 variant is not naive float64 arithmetic (e.g.
// width_bucket(0, -1e100::float8, 1, 10) = 10, where float64 would round
// 1e100/(1e100+1) to 1.0 and yield 11). Doing it in f64 also collapses
// huge numerics to infinity and i128 is too small for 1e100-scale
// values, so this section provides the minimal exact machinery: a
// base-1e9 unsigned bignum (`BigUint`) and a signed decimal (`BigDec`:
// sign + magnitude + decimal scale), with comparison, add/sub, small
// multiplication, multiply-by-10^k, schoolbook big multiplication, and
// exact floor division with a 2^31 quotient cap (width_bucket returns
// int4, so a larger quotient is PG's "integer out of range" anyway).
// The width_bucket orchestration itself lives in exec.rs.
// ---------------------------------------------------------------------------

/// v0.56: why `BigDec::parse_decimal` refused a literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecimalParseError {
    /// Not a decimal literal at all.
    Syntax,
    /// Syntactically valid but beyond the supported digit caps (PG also
    /// rejects these: "value overflows numeric format").
    TooBig,
}

/// v0.56: unsigned arbitrary-precision integer, base-1e9 limbs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BigUint {
    /// Little-endian base-1_000_000_000 limbs; canonical form has no
    /// leading zero limbs (zero is the empty vector).
    limbs: Vec<u32>,
}

impl BigUint {
    /// The value zero.
    pub(crate) fn zero() -> Self {
        BigUint { limbs: Vec::new() }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    pub(crate) fn from_u64(mut v: u64) -> Self {
        let mut out = BigUint::zero();
        while v > 0 {
            out.limbs.push((v % 1_000_000_000) as u32);
            v /= 1_000_000_000;
        }
        out
    }

    pub(crate) fn from_u128(mut v: u128) -> Self {
        let mut out = BigUint::zero();
        while v > 0 {
            out.limbs.push((v % 1_000_000_000) as u32);
            v /= 1_000_000_000;
        }
        out
    }

    /// Parse an all-digit string (no sign, point, or exponent); leading
    /// zeros are fine. Linear time: each 9-digit chunk taken from the
    /// right end is exactly one limb.
    pub(crate) fn from_decimal_str(s: &str) -> Self {
        let bytes = s.as_bytes();
        let mut limbs = Vec::new();
        let mut end = bytes.len();
        while end > 0 {
            let start = end.saturating_sub(9);
            let mut v: u32 = 0;
            for &b in &bytes[start..end] {
                v = v * 10 + (b - b'0') as u32;
            }
            limbs.push(v);
            end = start;
        }
        let mut out = BigUint { limbs };
        out.normalize();
        out
    }

    pub(crate) fn cmp(&self, other: &BigUint) -> std::cmp::Ordering {
        if self.limbs.len() != other.limbs.len() {
            return self.limbs.len().cmp(&other.limbs.len());
        }
        for (a, b) in self.limbs.iter().rev().zip(other.limbs.iter().rev()) {
            if a != b {
                return a.cmp(b);
            }
        }
        std::cmp::Ordering::Equal
    }

    /// self += other.
    pub(crate) fn add_assign(&mut self, other: &BigUint) {
        if other.is_zero() {
            return;
        }
        if self.limbs.len() < other.limbs.len() {
            self.limbs.resize(other.limbs.len(), 0);
        }
        let mut carry: u64 = 0;
        for (i, &o) in other.limbs.iter().enumerate() {
            let cur = self.limbs[i] as u64 + o as u64 + carry;
            self.limbs[i] = (cur % 1_000_000_000) as u32;
            carry = cur / 1_000_000_000;
        }
        let mut i = other.limbs.len();
        while carry > 0 {
            if i == self.limbs.len() {
                self.limbs.push(0);
            }
            let cur = self.limbs[i] as u64 + carry;
            self.limbs[i] = (cur % 1_000_000_000) as u32;
            carry = cur / 1_000_000_000;
            i += 1;
        }
    }

    /// self -= other; requires self >= other.
    pub(crate) fn sub_assign(&mut self, other: &BigUint) {
        debug_assert!(self.cmp(other) != std::cmp::Ordering::Less);
        let mut borrow: i64 = 0;
        for i in 0..self.limbs.len() {
            let o = if i < other.limbs.len() {
                other.limbs[i] as i64
            } else {
                0
            };
            let cur = self.limbs[i] as i64 - o - borrow;
            if cur < 0 {
                self.limbs[i] = (cur + 1_000_000_000) as u32;
                borrow = 1;
            } else {
                self.limbs[i] = cur as u32;
                borrow = 0;
            }
        }
        debug_assert!(borrow == 0);
        self.normalize();
    }

    /// self *= m.
    pub(crate) fn mul_small_assign(&mut self, m: u64) {
        if m == 0 || self.is_zero() {
            self.limbs.clear();
            return;
        }
        if m == 1 {
            return;
        }
        let mut carry: u128 = 0;
        for limb in self.limbs.iter_mut() {
            let cur = *limb as u128 * m as u128 + carry;
            *limb = (cur % 1_000_000_000) as u32;
            carry = cur / 1_000_000_000;
        }
        while carry > 0 {
            self.limbs.push((carry % 1_000_000_000) as u32);
            carry /= 1_000_000_000;
        }
    }

    /// self *= 10^k.
    pub(crate) fn mul_pow10_assign(&mut self, k: u32) {
        if self.is_zero() || k == 0 {
            return;
        }
        let r = k % 9;
        if r > 0 {
            self.mul_small_assign(10u64.pow(r));
        }
        // A base-1e9 limb shift multiplies by 10^(9q).
        let q = (k / 9) as usize;
        if q > 0 {
            let mut limbs = vec![0u32; q];
            limbs.append(&mut self.limbs);
            self.limbs = limbs;
        }
    }

    /// self /= d, rounding down (d <= 1_000_000_000).
    pub(crate) fn div_small_assign(&mut self, d: u32) {
        debug_assert!(d > 0 && d <= 1_000_000_000);
        let mut rem: u64 = 0;
        for i in (0..self.limbs.len()).rev() {
            let cur = rem * 1_000_000_000 + self.limbs[i] as u64;
            self.limbs[i] = (cur / d as u64) as u32;
            rem = cur % d as u64;
        }
        self.normalize();
    }

    /// Full product (schoolbook O(n*m)). The u128 accumulator is far
    /// wider than any entry can reach: each entry sums at most
    /// min(len) products below 1e18 plus carries.
    pub(crate) fn mul(&self, other: &BigUint) -> BigUint {
        if self.is_zero() || other.is_zero() {
            return BigUint::zero();
        }
        let mut acc = vec![0u128; self.limbs.len() + other.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            for (j, &b) in other.limbs.iter().enumerate() {
                acc[i + j] += a as u128 * b as u128;
            }
        }
        let mut limbs = Vec::with_capacity(acc.len() + 1);
        let mut carry: u128 = 0;
        for v in acc {
            let cur = v + carry;
            limbs.push((cur % 1_000_000_000) as u32);
            carry = cur / 1_000_000_000;
        }
        while carry > 0 {
            limbs.push((carry % 1_000_000_000) as u32);
            carry /= 1_000_000_000;
        }
        let mut out = BigUint { limbs };
        out.normalize();
        out
    }

    /// Exact floor(self / other); requires other > 0. Returns None when
    /// the quotient reaches 2^31 — width_bucket returns int4, so the
    /// caller reports PG's "integer out of range" instead. At most 31
    /// halving steps, each one schoolbook multiply plus a comparison.
    pub(crate) fn floor_div_bounded(&self, other: &BigUint) -> Option<BigUint> {
        debug_assert!(!other.is_zero());
        // quotient >= 2^31  <=>  self >= other * 2^31.
        let mut limit = other.clone();
        limit.mul_small_assign(1u64 << 31);
        if self.cmp(&limit) != std::cmp::Ordering::Less {
            return None;
        }
        let mut lo = BigUint::zero();
        let mut hi = BigUint::from_u64(1u64 << 31);
        // Invariant: lo*other <= self < hi*other.
        for _ in 0..31 {
            let mut mid = lo.clone();
            mid.add_assign(&hi);
            mid.div_small_assign(2);
            if mid.mul(other).cmp(self) != std::cmp::Ordering::Greater {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Some(lo)
    }

    /// Value as u64; None when it does not fit (only used on bounded
    /// quotients, which always fit).
    pub(crate) fn to_u64(&self) -> Option<u64> {
        let mut v: u64 = 0;
        for &limb in self.limbs.iter().rev() {
            v = v.checked_mul(1_000_000_000)?.checked_add(limb as u64)?;
        }
        Some(v)
    }
}

/// v0.56: exact signed decimal: value = (-1)^neg * mag * 10^(-scale).
/// Zero is always non-negative with an empty magnitude.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BigDec {
    neg: bool,
    mag: BigUint,
    scale: i32,
}

impl BigDec {
    /// Digit caps for `parse_decimal`, mirroring `Numeric::parse`'s
    /// 131072-digit / 200000-scale limits so adversarial literals stay
    /// cheap (PG rejects them with "value overflows numeric format").
    const MAX_DIGITS: usize = 200_000;
    const MAX_SCALE: i32 = 200_000;

    pub(crate) fn zero() -> Self {
        BigDec {
            neg: false,
            mag: BigUint::zero(),
            scale: 0,
        }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.mag.is_zero()
    }

    /// Canonicalize (-0 becomes +0).
    fn normalize(mut self) -> Self {
        if self.mag.is_zero() {
            self.neg = false;
        }
        self
    }

    /// |self.scale - other.scale| as u32 for limb padding. All
    /// constructors cap |scale| (parse: 200000, f64: 1074), so this
    /// cannot overflow in practice; the expect documents the invariant.
    fn scale_pad(a: i32, b: i32) -> u32 {
        let d = (a as i64 - b as i64).unsigned_abs();
        u32::try_from(d).expect("width_bucket scale difference is bounded")
    }

    /// Exact conversion from i64.
    pub(crate) fn from_i64(v: i64) -> Self {
        BigDec {
            neg: v < 0,
            mag: BigUint::from_u64(v.unsigned_abs()),
            scale: 0,
        }
        .normalize()
    }

    /// Exact conversion from a finite Numeric; None for NaN/infinity.
    pub(crate) fn from_numeric(n: &Numeric) -> Option<Self> {
        if n.special != NumericSpecial::Finite {
            return None;
        }
        Some(
            BigDec {
                neg: n.unscaled < 0,
                mag: BigUint::from_u128(n.unscaled.unsigned_abs()),
                scale: n.scale,
            }
            .normalize(),
        )
    }

    /// Parse `[+-]?digits[.digits][e[+-]digits]`. Fallback for literals
    /// `Numeric::parse` rejects with Overflow (huge digit counts); PG
    /// caps such inputs too, so anything beyond the caps is TooBig.
    pub(crate) fn parse_decimal(s: &str) -> Result<Self, DecimalParseError> {
        let s = s.trim();
        let (neg, s) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (mant_raw, exp_raw) = match s.find(['e', 'E']) {
            Some(i) => (&s[..i], Some(&s[i + 1..])),
            None => (s, None),
        };
        // v0.25-style `_` digit separators (PG 16+), validated per part
        // exactly like Numeric::parse.
        let exp_clean;
        let exp_str = match exp_raw {
            Some(e) => {
                exp_clean = strip_underscores(e).ok_or(DecimalParseError::Syntax)?;
                Some(exp_clean.as_str())
            }
            None => None,
        };
        let (int_raw, frac_raw) = match mant_raw.find('.') {
            Some(i) => (&mant_raw[..i], &mant_raw[i + 1..]),
            None => (mant_raw, ""),
        };
        let int_clean = strip_underscores_opt(int_raw).ok_or(DecimalParseError::Syntax)?;
        let frac_clean = strip_underscores_opt(frac_raw).ok_or(DecimalParseError::Syntax)?;
        let (int_part, frac_part) = (int_clean.as_str(), frac_clean.as_str());
        let exp: i32 = match exp_str {
            Some(e) => e.parse().map_err(|_| DecimalParseError::Syntax)?,
            None => 0,
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(DecimalParseError::Syntax);
        }
        if int_part.len() + frac_part.len() > Self::MAX_DIGITS {
            return Err(DecimalParseError::TooBig);
        }
        // value = digits * 10^(exp - frac_len)
        let scale = frac_part.len() as i64 - exp as i64;
        if scale.abs() > Self::MAX_SCALE as i64 {
            return Err(DecimalParseError::TooBig);
        }
        let mut digits = String::with_capacity(int_part.len() + frac_part.len());
        digits.push_str(int_part);
        digits.push_str(frac_part);
        let mut mag = BigUint::from_decimal_str(&digits);
        let scale = if scale < 0 {
            mag.mul_pow10_assign((-scale) as u32);
            0
        } else {
            scale as i32
        };
        Ok(BigDec { neg, mag, scale }.normalize())
    }

    /// Signed comparison.
    pub(crate) fn cmp(&self, other: &BigDec) -> std::cmp::Ordering {
        if self.is_zero() && other.is_zero() {
            return std::cmp::Ordering::Equal;
        }
        if self.neg != other.neg {
            return if self.neg {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        let ord = if self.scale == other.scale {
            self.mag.cmp(&other.mag)
        } else if self.scale > other.scale {
            let mut padded = other.mag.clone();
            padded.mul_pow10_assign(Self::scale_pad(self.scale, other.scale));
            self.mag.cmp(&padded)
        } else {
            let mut padded = self.mag.clone();
            padded.mul_pow10_assign(Self::scale_pad(other.scale, self.scale));
            padded.cmp(&other.mag)
        };
        if self.neg { ord.reverse() } else { ord }
    }

    /// Signed subtraction.
    pub(crate) fn sub(&self, other: &BigDec) -> BigDec {
        let (mut ma, mut mb) = (self.mag.clone(), other.mag.clone());
        let scale = if self.scale >= other.scale {
            mb.mul_pow10_assign(Self::scale_pad(self.scale, other.scale));
            self.scale
        } else {
            ma.mul_pow10_assign(Self::scale_pad(other.scale, self.scale));
            other.scale
        };
        if self.neg == other.neg {
            match ma.cmp(&mb) {
                std::cmp::Ordering::Equal => BigDec::zero(),
                std::cmp::Ordering::Greater => {
                    ma.sub_assign(&mb);
                    BigDec {
                        neg: self.neg,
                        mag: ma,
                        scale,
                    }
                    .normalize()
                }
                std::cmp::Ordering::Less => {
                    mb.sub_assign(&ma);
                    BigDec {
                        neg: !self.neg,
                        mag: mb,
                        scale,
                    }
                    .normalize()
                }
            }
        } else {
            ma.add_assign(&mb);
            BigDec {
                neg: self.neg,
                mag: ma,
                scale,
            }
            .normalize()
        }
    }

    /// Magnitude accessor for the bucket computation in exec.rs.
    pub(crate) fn mag(&self) -> &BigUint {
        &self.mag
    }

    /// Scale accessor for the bucket computation in exec.rs.
    pub(crate) fn scale(&self) -> i32 {
        self.scale
    }
}

/// A single cell value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    SmallInt(i16),    // v0.7: INT2
    Int(i64),         // INT4 (kept as i64, like v0.1-v0.6)
    BigInt(i64),      // v0.7: INT8
    Float4(f32),      // v0.7: REAL
    Float(f64),       // FLOAT8
    Numeric(Numeric), // v0.7: NUMERIC
    /// A clone of this value shares the text bytes. Text cells do not
    /// change, so a shared buffer is safe. This removes one copy for
    /// each row. See issue #8.
    Text(Arc<str>),
    // v0.35: a blank-padded `character(n)` value, stored exactly as PG
    // stores bpchar (padded to n characters with ASCII spaces). Kept
    // distinct from Text so comparisons, octet_length, and output typing
    // can apply PG's trailing-space rules.
    BpChar(Arc<str>),
    // v0.36: PG's one-byte `"char"` value (OID 18): a raw byte 0-255.
    // A u8 (not char) so high-bit bytes and NUL round-trip exactly.
    SingleChar(u8),
    Bool(bool),
    Date(i32),        // v0.7: days since 1970-01-01
    Timestamp(i64),   // v0.7: micros since 1970-01-01 00:00:00 UTC
    Timestamptz(i64), // v0.7: micros since epoch, UTC
    Bytea(Vec<u8>),   // v0.7
    Uuid([u8; 16]),   // v0.7
    Null,
}

impl Value {
    /// Make a text value from `&str`, `String` or `Arc<str>`.
    /// Only the `Arc<str>` form does not copy the bytes: `String` copies
    /// too, because `Arc<str>` needs its own buffer. Give an `Arc<str>`
    /// on a path that runs for each row.
    /// (`&String` needs `.as_str()`. An `AsRef<str>` bound would accept
    /// it but would remove the free `Arc<str>` form.)
    pub fn text(s: impl Into<Arc<str>>) -> Value {
        Value::Text(s.into())
    }

    /// Make a blank-padded `character(n)` value (v0.35). `s` must already
    /// be padded to exactly `n` characters.
    pub fn bpchar(s: impl Into<Arc<str>>) -> Value {
        Value::BpChar(s.into())
    }

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
            Value::Text(s) => Some(s.to_string()),
            // v0.35: bpchar is stored blank-padded; the wire shows the
            // padded bytes, like Postgres.
            Value::BpChar(s) => Some(s.to_string()),
            // v0.36: PG's charout: NUL renders empty, high-bit bytes as
            // `\ooo` octal escapes, other bytes as themselves.
            Value::SingleChar(b) => Some(char_out(*b)),
            Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
            Value::Date(d) => Some(crate::datetime::format_date(*d)),
            Value::Timestamp(m) => Some(crate::datetime::format_timestamp(*m)),
            Value::Timestamptz(m) => Some(crate::datetime::format_timestamptz(*m)),
            Value::Bytea(b) => Some(bytea_text(b)),
            Value::Uuid(u) => Some(uuid_text(u)),
            Value::Null => None,
        }
    }

    /// Same encoding as `to_text`, but written straight into `out` instead
    /// of returned as an owned `String`. Returns `false` for NULL (nothing
    /// written) or `true` otherwise. Used by `send_data_row`'s per-row wire
    /// encoding, the hottest caller of the text format: for `Text` this
    /// avoids `to_text`'s `String::clone()` entirely (the bytes already
    /// live in `s`), and for the integer/bool arms it skips materializing
    /// an intermediate `String` just to copy its bytes and drop it. The
    /// less common variants fall back to `to_text()` — they are not on
    /// this hot path and formatting them (NUMERIC, dates, bytea, UUID)
    /// already goes through non-trivial `String`-returning helpers.
    pub fn write_text_into(&self, out: &mut Vec<u8>, bytea_output: ByteaOutput) -> bool {
        use std::io::Write;
        match self {
            Value::SmallInt(i) => {
                let _ = write!(out, "{i}");
                true
            }
            Value::Int(i) => {
                let _ = write!(out, "{i}");
                true
            }
            Value::BigInt(i) => {
                let _ = write!(out, "{i}");
                true
            }
            Value::Text(s) => {
                out.extend_from_slice(s.as_bytes());
                true
            }
            // v0.35: BpChar shares Text's zero-copy fast path (already
            // padded; the wire shows the padded bytes).
            Value::BpChar(s) => {
                out.extend_from_slice(s.as_bytes());
                true
            }
            // v0.36: PG's charout, written directly (usually 1 byte, or
            // 4 for a `\ooo` octal escape, or 0 for NUL).
            Value::SingleChar(b) => {
                char_out_into(*b, out);
                true
            }
            Value::Bool(b) => {
                out.push(if *b { b't' } else { b'f' });
                true
            }
            Value::Null => false,
            // v0.29: bytea rendering honors the bytea_output GUC.
            Value::Bytea(b) => {
                let s = match bytea_output {
                    ByteaOutput::Hex => bytea_text(b),
                    ByteaOutput::Escape => bytea_escape(b),
                };
                out.extend_from_slice(s.as_bytes());
                true
            }
            other => match other.to_text() {
                Some(s) => {
                    out.extend_from_slice(s.as_bytes());
                    true
                }
                None => false,
            },
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
            Value::BpChar(_) => "character", // v0.35
            // v0.36: PG's format_type(18) renders `"char"` (quoted).
            Value::SingleChar(_) => "\"char\"",
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
            // v0.35: the value alone doesn't record the typmod; Char(None)
            // is the honest fallback (bare `character`).
            Value::BpChar(_) => ColType::Char(None),
            Value::SingleChar(_) => ColType::SingleChar, // v0.36
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

/// v0.29: `bytea_output` GUC values. `Hex` (`\xdeadbeef`) is the default;
/// `Escape` renders per PG docs Table 8.8 (printables literal, backslash
/// doubled, others as `\ooo` octal).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ByteaOutput {
    #[default]
    Hex,
    Escape,
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

/// v0.36: PG's `charout` for the one-byte `"char"` type: byte 0 renders
/// as the empty string (a C string can't hold NUL), bytes with the high
/// bit set render as `\ooo` octal escapes, every other byte renders as
/// itself. (char.out: `'\377'::"char"` -> `\377`, `'\000'` -> empty.)
pub(crate) fn char_out(b: u8) -> String {
    if b == 0 {
        String::new()
    } else if b >= 0x80 {
        format!("\\{:03o}", b)
    } else {
        (b as char).to_string()
    }
}

/// v0.36: `charout` writing UTF-8 bytes straight into `out` (the `\ooo`
/// escape is pure ASCII, so this is byte-exact).
pub(crate) fn char_out_into(b: u8, out: &mut Vec<u8>) {
    if b == 0 {
        // NUL renders as the empty string.
    } else if b >= 0x80 {
        const OCT: &[u8; 8] = b"01234567";
        out.push(b'\\');
        out.push(OCT[(b >> 6) as usize]);
        out.push(OCT[((b >> 3) & 7) as usize]);
        out.push(OCT[(b & 7) as usize]);
    } else {
        out.push(b);
    }
}

/// v0.36: PG's `charin` for the one-byte `"char"` type (char.c, PG19).
/// Exact rules:
/// - exactly 4 bytes `\ooo` (backslash + 3 octal digits) -> the byte value
/// - otherwise the FIRST byte, silently discarding the rest (a
///   backwards-compatibility provision, per the char.c comment); the empty
///   string is NUL (ch[0] of "" is '\0')
/// `charin` is total: it never errors.
pub(crate) fn char_in(s: &str) -> u8 {
    let b = s.as_bytes();
    if b.len() == 4
        && b[0] == b'\\'
        && (b'0'..=b'7').contains(&b[1])
        && (b'0'..=b'7').contains(&b[2])
        && (b'0'..=b'7').contains(&b[3])
    {
        ((b[1] - b'0') << 6) | ((b[2] - b'0') << 3) | (b[3] - b'0')
    } else {
        // First byte; empty input -> NUL.
        b.first().copied().unwrap_or(0)
    }
}

/// v0.57: PG19's `namein` truncation (name.c): inputs to the `name`
/// type are silently truncated at NAMEDATALEN-1 = 63 *bytes*. PG cuts
/// raw bytes (it can split a multibyte character); we cut at the last
/// UTF-8 char boundary at or below 63 bytes so the result stays valid
/// UTF-8 — identical to PG for all ASCII input, and never panics.
pub(crate) fn truncate_name(s: &str) -> &str {
    const MAX: usize = 63; // NAMEDATALEN - 1
    if s.len() <= MAX {
        return s;
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// v0.29: PG bytea `escape` output format (docs Table 8.8): printable ASCII
/// as-is, backslash doubled, others as `\ooo` octal.
pub(crate) fn bytea_escape(data: &[u8]) -> String {
    let mut out = String::new();
    for &b in data {
        if b == b'\\' {
            out.push_str("\\\\");
        } else if b >= 32 && b < 127 {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\{:03o}", b));
        }
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
/// v0.29: rich bytea parse errors so callers can emit PG's exact messages.
/// Hex-format failures are 22023 with PG's specific texts; anything else
/// is the generic 22P02 `invalid input syntax for type bytea`.
#[derive(Debug, PartialEq, Eq)]
pub enum ByteaParseError {
    /// `\x` hex with an odd number of digits.
    OddHexDigits,
    /// `\x` hex with a non-hex digit (the offending char).
    BadHexDigit(char),
    /// Escape format or otherwise malformed.
    Invalid,
}

pub fn parse_bytea(s: &str) -> Result<Vec<u8>, ByteaParseError> {
    // Hex format: `\x` followed by hex digits. PG ignores whitespace
    // between the hex digits (e.g. '\x De Ad Be Ef ').
    if let Some(hex) = s.strip_prefix("\\x").or_else(|| s.strip_prefix("\\X")) {
        let digits: Vec<u8> = hex.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        if digits.len() % 2 != 0 {
            return Err(ByteaParseError::OddHexDigits);
        }
        let mut out = Vec::with_capacity(digits.len() / 2);
        let mut i = 0;
        while i < digits.len() {
            let hi = (digits[i] as char)
                .to_digit(16)
                .ok_or(ByteaParseError::BadHexDigit(digits[i] as char))?;
            let lo = (digits[i + 1] as char)
                .to_digit(16)
                .ok_or(ByteaParseError::BadHexDigit(digits[i + 1] as char))?;
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
                    return Err(ByteaParseError::Invalid);
                }
                out.push(v as u8);
                i += 3;
            } else {
                return Err(ByteaParseError::Invalid);
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
    // v0.22: `{}` on f64 never uses scientific notation, so huge
    // magnitudes explode into hundreds of digits. PostgreSQL's %g
    // (17 significant digits for float8) switches to scientific when
    // the decimal exponent is >= 17 or < -4; mirror that so e.g.
    // trunc('1e200'::numeric) prints "1e+200" like PG.
    if f != 0.0 {
        let a = f.abs();
        if a >= 1e17 || a < 1e-4 {
            return float_scientific(f);
        }
    }
    // `{}` on f64 already prints the shortest string that round-trips.
    format!("{}", f)
}

/// Scientific notation à la PostgreSQL float8out: shortest
/// significant digits, `e+NN`/`e-NN` with a sign and at least two
/// exponent digits (`1e+200`, `1.5e-07`).
fn float_scientific(f: f64) -> String {
    let s = format!("{:e}", f);
    let epos = s.find('e').unwrap_or(s.len());
    let (mant, exp) = s.split_at(epos);
    let exp: i32 = exp[1..].parse().unwrap_or(0);
    format!("{}e{:+03}", mant, exp)
}

/// Same for f32 (real): Rust's Display already prints the shortest
/// string that round-trips at f32 precision, so `1.1::real` prints
/// `1.1` rather than `1.100000023841858`. v0.22: huge magnitudes get
/// the same scientific treatment as float8 (threshold at 1e9 for
/// float4's 9 significant digits).
fn float4_text(f: f32) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if f != 0.0 {
        let a = f.abs();
        if a >= 1e9 || a < 1e-4 {
            return float_scientific(f as f64);
        }
    }
    format!("{}", f)
}

/// v0.35: strip trailing ASCII spaces, like PG19's `rtrim1` /
/// `bcTruelen` in varchar.c. bpchar blank-padding is always ASCII space
/// (0x20); only that byte is significant, never other whitespace.
pub fn rtrim_spaces(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// The cells of one row. A reference count controls the memory, thus a
/// clone increases the count and does not copy the cells.
///
/// An MVCC write adds a new [`RowVersion`]. It does not change the cells
/// of a row that is in the table. Thus a shared row is always safe. The
/// cells are behind a private `Arc`, so no code can get write access to
/// a row that it shares. This keeps the rule.
///
/// Note: a `Row` holds the cells only. For a row record, see
/// [`RowVersion`].
///
/// `Arc<Vec<Value>>` and not `Arc<[Value]>`: the second is one heap
/// block and not two, but its handle is 16 bytes and not 8, which makes
/// the large per-scan row buffers larger. See `benches/BASELINE.md`.
#[derive(Clone, Debug, PartialEq)]
pub struct Row(Arc<Vec<Value>>);

impl Row {
    /// Make a row from its cells. Clone the `Row` after this to share
    /// the cells.
    pub fn new(cells: Vec<Value>) -> Row {
        Row(Arc::new(cells))
    }

    /// Take the cells out. This copies them only if another handle
    /// shares them.
    pub fn into_cells(self) -> Vec<Value> {
        Arc::try_unwrap(self.0).unwrap_or_else(|c| (*c).clone())
    }
}

impl Default for Row {
    /// An empty row. All empty rows share one buffer, thus this function
    /// does not allocate. `Arc::default()` allocates for each call.
    fn default() -> Row {
        static EMPTY: std::sync::OnceLock<Row> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| Row(Arc::new(Vec::new()))).clone()
    }
}

impl std::ops::Deref for Row {
    type Target = [Value];

    fn deref(&self) -> &[Value] {
        &self.0
    }
}

impl From<Vec<Value>> for Row {
    fn from(cells: Vec<Value>) -> Row {
        Row::new(cells)
    }
}

/// One version of one row. UPDATE = mark the old version's `xmax` and
/// append a new version; DELETE = mark `xmax`.
#[derive(Clone, Debug)]
pub struct RowVersion {
    /// Globally unique, never reused (survives crashes via the WAL).
    pub id: u64,
    pub values: Row,
    /// Xid of the creating transaction.
    pub xmin: u64,
    /// Xid of the deleting/updating transaction; 0 = not deleted.
    pub xmax: u64,
    /// v0.37: per-cell toast value id, parallel to `values.values`;
    /// 0 = stored plain inline, otherwise a key into the owning
    /// table's `toast_info` (and, for out-of-line values, the
    /// `chunk_id` in the toast table).
    pub toast: Vec<u32>,
}

impl RowVersion {
    /// v0.37: build a row version with no toasted cells (`toast` all
    /// zero, parallel to `values`).
    pub fn plain(id: u64, values: Row, xmin: u64) -> Self {
        let n = values.len();
        RowVersion {
            id,
            values,
            xmin,
            xmax: 0,
            toast: vec![0; n],
        }
    }
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
    /// v0.11: role that owns the table (the creating role).
    pub owner: String,
    /// v0.11: explicit GRANT entries (owner and superusers bypass).
    pub acl: Vec<AclEntry>,
    /// v0.11: column-level GRANT entries (empty = none granted).
    pub col_acl: Vec<ColAclEntry>,
    // --- v0.37: TOAST ---
    /// Table OID (`pg_class.oid`), assigned from `Database::next_oid`.
    pub oid: u32,
    /// OID of this table's toast table (`pg_class.reltoastrelid`);
    /// 0 = no toast table (no toastable columns).
    pub toast_relid: u32,
    /// `toast_tuple_target` reloption (default `TOAST_TUPLE_TARGET`).
    pub toast_target: u32,
    /// Per-column TOAST storage strategy (`toast_storage::*`), parallel
    /// to `columns`.
    pub col_storage: Vec<u8>,
    /// v0.41: per-column TOAST compression method, parallel to
    /// `columns`; `None` = no explicit method (PG's invalid/default
    /// `attcompression`), resolved to the session's
    /// `default_toast_compression` at compression time.
    pub col_compression: Vec<Option<ToastCompression>>,
    /// Toast value id -> storage state, for cells recorded in
    /// `RowVersion::toast`.
    pub toast_info: HashMap<u32, ToastInfo>,
    /// Next toast value id to assign in this table (starts at 1; 0
    /// means "not toasted" in `RowVersion::toast`).
    pub next_value_id: u32,
}

impl Table {
    pub fn new(columns: Vec<(String, ColType)>, created_xmin: u64) -> Self {
        let n = columns.len();
        // v0.37: compute storage strategies before `columns` moves.
        let col_storage: Vec<u8> = columns
            .iter()
            .map(|(_, t)| t.default_toast_storage())
            .collect();
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
            owner: "postgres".to_string(),
            acl: Vec::new(),
            col_acl: Vec::new(),
            // --- v0.37: TOAST (oid assigned by the caller via
            // `Database::alloc_oid`; toast table created on demand). ---
            oid: 0,
            toast_relid: 0,
            toast_target: toast_consts::TOAST_TUPLE_TARGET,
            col_storage,
            // v0.41: no explicit per-column compression methods.
            col_compression: vec![None; n],
            toast_info: HashMap::new(),
            next_value_id: 1,
        }
    }

    /// Build a table from a parsed v0.9 `TableDef` (constraints included).
    pub fn with_def(def: &TableDef, created_xmin: u64) -> Self {
        let mut t = Table::new(def.columns.clone(), created_xmin);
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
/// v0.22: session id meaning "no session". Real session ids come from an
/// incrementing counter starting at 1 (`NEXT_SID` in server.rs), so this
/// never collides. Pass it to session-aware helpers from paths that only
/// ever touch permanent tables (WAL replay, vacuum).
pub const NO_SESSION: u64 = u64::MAX;

/// v0.22: bounded shell-type registry entry. PostgreSQL's
/// `CREATE TYPE name;` creates an undefined "shell" type that a later
/// `CREATE TYPE name (... LIKE = base)` completes. rustgres supports
/// only the shell + LIKE-completion forms (no I/O functions, no
/// composite/enum/range types): enough for the pg_regress float8
/// cluster, which builds a float8 alias this way.
#[derive(Clone, Debug)]
pub struct ShellType {
    /// Base type name from LIKE = <base>; None while still a shell.
    pub like_base: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Database {
    /// Keyed by table name — a short, trusted identifier (whoever is
    /// connected to this server chose it), looked up several times per
    /// query (`find_table`). Hashed with `FxHasher` instead of the
    /// default `SipHash`: SipHash's flooding resistance is wasted on a
    /// key that isn't attacker-controlled input from an untrusted
    /// boundary, and its fixed per-call mixing cost showed up at 6.75%
    /// of Callgrind `Ir` on an ordinary query (see `benches/BASELINE.md`,
    /// "catalog HashMap hasher"). Same rationale for the five maps below.
    pub tables: HashMap<String, Vec<Table>, FxBuildHasher>,
    /// v0.22: session-local temporary tables, keyed by session id then
    /// table name. A temp table shadows any permanent table of the same
    /// name *only within its owning session* (PostgreSQL semantics); the
    /// permanent table is never deleted by temp DDL. Temp tables are
    /// single-version (no MVCC history), never checkpointed, never
    /// WAL-logged, and are dropped automatically when the session ends.
    /// DDL on them is still statement-atomic via the txn undo log
    /// (`WriteOp::CreateTempTable` / `WriteOp::DropTempTable`).
    pub temp_tables: HashMap<u64, HashMap<String, Table, FxBuildHasher>, FxBuildHasher>,
    /// Secondary indexes by index name (v0.8). DDL is transactional: each
    /// definition carries creator/deleter xids, and entries for
    /// uncommitted row versions are filtered by visibility at scan time.
    pub indexes: HashMap<String, Index, FxBuildHasher>,
    /// ANALYZE statistics by table name (v0.8). Updated non-transactionally
    /// by ANALYZE, like PostgreSQL; never WAL-logged, rebuilt by ANALYZE.
    pub stats: HashMap<String, TableStats, FxBuildHasher>,
    /// Shell types by name (v0.22, bounded — see `ShellType`). Types are
    /// database-global and transactional via the statement undo log
    /// (`WriteOp::CreateType` / `WriteOp::DropType`), but are not
    /// WAL-logged or checkpointed in v0.22: an inter-checkpoint crash or
    /// a restart loses type definitions. Nothing can reference a shell
    /// type yet (no column-type or cast support), so recovery stays
    /// consistent; documented in the README.
    pub types: HashMap<String, ShellType, FxBuildHasher>,
    /// Views by name (v0.9). Versioned like tables so CREATE/DROP VIEW are
    /// transactional under MVCC.
    pub views: HashMap<String, Vec<ViewDef>, FxBuildHasher>,
    /// Sequences by name (v0.9). DDL is transactional; the sequence
    /// *value* (`current`) advances non-transactionally, like PostgreSQL.
    pub sequences: HashMap<String, Vec<Sequence>, FxBuildHasher>,
    /// Roles by name (v0.11). Versioned like tables so CREATE / DROP /
    /// ALTER ROLE are transactional under MVCC.
    pub roles: HashMap<String, Vec<Role>, FxBuildHasher>,
    /// v0.11: database-level GRANT entries (CONNECT). Empty = default
    /// allow, matching a fresh PostgreSQL install's PUBLIC grant.
    pub db_acl: Vec<AclEntry>,
    /// v0.37: next table OID to assign (`pg_class.oid`). Starts at
    /// `FIRST_USER_OID` (16384), like PG's `FirstNormalObjectId`.
    pub next_oid: u32,
}

/// v0.13: a replication slot. Cluster-global (not per-database), and
/// non-transactional: CREATE/DROP take effect immediately and are
/// WAL-logged as ReplSlot* records, surviving checkpoint + recovery.
/// `active` is runtime-only (a connected walsender holds it) and is
/// never persisted.
#[derive(Clone, Debug)]
pub struct ReplSlot {
    pub name: String,
    /// Output plugin name, e.g. "rustgres_decoding" (v0.13's single
    /// built-in logical plugin). Empty for physical slots.
    pub plugin: String,
    /// "logical" or "physical".
    pub slot_type: String,
    /// Oldest LSN the slot still needs (WAL retention floor).
    pub restart_lsn: u64,
    /// Last LSN the downstream confirmed flushed.
    pub confirmed_flush_lsn: u64,
    pub active: bool,
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
    /// v0.11: role that owns the view.
    pub owner: String,
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
    /// v0.11: role that owns the sequence.
    pub owner: String,
    /// v0.11: explicit GRANT entries for USAGE (owner/superuser bypass).
    pub acl: Vec<AclEntry>,
}

impl Sequence {
    pub fn new(
        name: String,
        start: i64,
        increment: i64,
        min_value: i64,
        max_value: i64,
        cycle: bool,
        created_xmin: u64,
    ) -> Self {
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
            owner: "postgres".to_string(),
            acl: Vec::new(),
        }
    }
}

impl Database {
    /// v0.39: find a table's name by its OID (any live version), for
    /// resolving a main table's `toast_relid` to its toast table during
    /// vacuum. Returns the first match; OIDs are unique across live
    /// versions.
    pub fn table_name_by_oid(&self, oid: u32) -> Option<String> {
        self.tables
            .iter()
            .find_map(|(n, vs)| vs.iter().any(|v| v.oid == oid).then(|| n.clone()))
    }

    pub fn new() -> Self {
        let mut db = Database {
            tables: HashMap::default(),
            indexes: HashMap::default(),
            stats: HashMap::default(),
            views: HashMap::default(),
            sequences: HashMap::default(),
            roles: HashMap::default(),
            db_acl: Vec::new(),
            // v0.22: session-local TEMP tables live here, keyed by session id.
            temp_tables: HashMap::default(),
            // v0.22: bounded shell-type registry.
            types: HashMap::default(),
            // v0.37: table OIDs for pg_class.
            next_oid: toast_consts::FIRST_USER_OID,
        };
        // v0.11: the bootstrap superuser always exists.
        db.roles
            .insert("postgres".to_string(), vec![Role::bootstrap_postgres()]);
        db
    }

    /// v0.37: hand out the next table OID (`pg_class.oid`).
    pub fn alloc_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid = oid.wrapping_add(1).max(toast_consts::FIRST_USER_OID);
        oid
    }

    /// First role version with `name` visible to (`snap`, `own`).
    pub fn find_role(&self, name: &str, snap: &Snapshot, own: u64) -> Option<&Role> {
        self.roles
            .get(name)
            .and_then(|vs| vs.iter().find(|r| role_visible(r, snap, own)))
    }

    /// Mutable variant of [`Database::find_role`].
    pub fn find_role_mut(&mut self, name: &str, snap: &Snapshot, own: u64) -> Option<&mut Role> {
        self.roles
            .get_mut(name)
            .and_then(|vs| vs.iter_mut().find(|r| role_visible(r, snap, own)))
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
    pub fn find_table(
        &self,
        name: &str,
        snap: &Snapshot,
        own: u64,
        session: u64,
    ) -> Option<&Table> {
        // v0.22: a session-local temp table shadows the permanent one.
        if let Some(t) = self.temp_tables.get(&session).and_then(|m| m.get(name)) {
            return Some(t);
        }
        self.tables
            .get(name)
            .and_then(|vs| vs.iter().find(|t| table_visible(t, snap, own)))
    }

    /// Mutable variant of [`Database::find_table`].
    pub fn find_table_mut(
        &mut self,
        name: &str,
        snap: &Snapshot,
        own: u64,
        session: u64,
    ) -> Option<&mut Table> {
        // v0.22: a session-local temp table shadows the permanent one.
        if let Some(t) = self
            .temp_tables
            .get_mut(&session)
            .and_then(|m| m.get_mut(name))
        {
            return Some(t);
        }
        self.tables
            .get_mut(name)
            .and_then(|vs| vs.iter_mut().find(|t| table_visible(t, snap, own)))
    }

    /// v0.22: drop all of a session's temp tables (disconnect cleanup).
    /// Temp tables never own entries in the global index map — their
    /// PRIMARY KEY / UNIQUE constraints are enforced by a session-local
    /// scan, and CREATE INDEX on a temp table is rejected — so there is
    /// no index cleanup to do here. There MUST NOT be: an index whose
    /// `def.table` equals a temp table's name belongs to a same-named
    /// permanent table, and deleting it would corrupt that table.
    pub fn drop_session_temps(&mut self, session: u64) {
        self.temp_tables.remove(&session);
    }

    /// The table version created by `own` (for WAL logging of DDL).
    /// Any row version with this id, wherever it lives (ids are global).
    /// Used by undo and WAL replay; both run with the engine lock held.
    /// v0.22: also searches session-local temp tables — row ids are
    /// globally unique, so a match is unambiguous.
    pub fn find_row_version_mut(&mut self, id: u64) -> Option<&mut RowVersion> {
        for vs in self.tables.values_mut() {
            for t in vs {
                if let Some(pos) = t.row_pos(id) {
                    return Some(&mut t.rows[pos]);
                }
            }
        }
        for tmps in self.temp_tables.values_mut() {
            for t in tmps.values_mut() {
                if let Some(pos) = t.row_pos(id) {
                    return Some(&mut t.rows[pos]);
                }
            }
        }
        None
    }

    /// Immutable twin, for commit-time WAL record validation.
    /// v0.22: also searches session-local temp tables (see above).
    pub fn find_row_version(&self, id: u64) -> Option<&RowVersion> {
        for vs in self.tables.values() {
            for t in vs {
                if let Some(pos) = t.row_pos(id) {
                    return Some(&t.rows[pos]);
                }
            }
        }
        for tmps in self.temp_tables.values() {
            for t in tmps.values() {
                if let Some(pos) = t.row_pos(id) {
                    return Some(&t.rows[pos]);
                }
            }
        }
        None
    }

    /// v0.22: does `session` hold a temp table called `name`?
    ///
    /// Pass [`NO_SESSION`] for paths that only ever touch permanent
    /// tables (WAL replay, vacuum): it is never a real session id.
    pub fn is_temp_table(&self, session: u64, name: &str) -> bool {
        self.temp_tables
            .get(&session)
            .is_some_and(|m| m.contains_key(name))
    }

    /// v0.22: is `row_id` held by any session's temp table? Row ids are
    /// globally unique, so this is unambiguous. Used to keep temp-table
    /// DML out of the WAL (PostgreSQL never WAL-logs temp-table data).
    pub fn row_id_in_temp(&self, row_id: u64) -> bool {
        self.temp_tables
            .values()
            .any(|tmps| tmps.values().any(|t| t.row_pos(row_id).is_some()))
    }

    /// v0.22: remove the row version `row_id` created by `own`, wherever
    /// it lives (permanent or temp table). Returns the removed values and
    /// whether the table was a session-local temp table — temp rows have
    /// no global index entries, so callers skip index cleanup for them.
    fn remove_own_version(&mut self, row_id: u64, own: u64) -> Option<(Row, bool)> {
        for vs in self.tables.values_mut() {
            for t in vs {
                if let Some(pos) = t.row_pos(row_id) {
                    if t.rows[pos].xmin == own {
                        let values = t.rows[pos].values.clone();
                        t.swap_remove_version(pos);
                        return Some((values, false));
                    }
                    return None;
                }
            }
        }
        for tmps in self.temp_tables.values_mut() {
            for t in tmps.values_mut() {
                if let Some(pos) = t.row_pos(row_id) {
                    if t.rows[pos].xmin == own {
                        let values = t.rows[pos].values.clone();
                        t.swap_remove_version(pos);
                        return Some((values, true));
                    }
                    return None;
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
    pub fn find_index_mut(&mut self, name: &str, snap: &Snapshot, own: u64) -> Option<&mut Index> {
        self.indexes
            .get_mut(name)
            .filter(|ix| index_visible(&ix.def, snap, own))
    }

    /// All index definitions on `table` visible to (`snap`, `own`),
    /// sorted by name for determinism.
    pub fn visible_indexes_for(
        &self,
        table: &str,
        snap: &Snapshot,
        own: u64,
        session: u64,
    ) -> Vec<&Index> {
        // v0.22: temp tables never have entries in the global index map
        // (their PRIMARY KEY / UNIQUE constraints are enforced by a
        // session-local scan instead), so no index is visible for them.
        // Without this, a temp table would "see" a same-named permanent
        // table's indexes and read the wrong rows.
        if self.is_temp_table(session, table) {
            return Vec::new();
        }
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
    /// v0.22: no-op for temp tables (see `visible_indexes_for`); without
    /// this a temp row would pollute a same-named permanent table's
    /// indexes with an entry no permanent-table scan could resolve.
    pub fn index_insert_row(&mut self, table: &str, row_id: u64, values: &[Value], session: u64) {
        if self.is_temp_table(session, table) {
            return;
        }
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
        session: u64,
    ) -> Option<String> {
        let t = self.find_table(table, snap, own, session)?;
        // v0.22: temp tables have no backing indexes; enforce their
        // PRIMARY KEY / UNIQUE constraints with a session-local scan.
        if self.is_temp_table(session, table) {
            return self
                .temp_scan_constraint(t, values, exclude_row_id, snap, own, None)
                .map(|(name, _)| name);
        }
        for ix in self.visible_indexes_for(table, snap, own, session) {
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

    /// v0.22: scan a temp table's rows for a live row matching `values`
    /// on a PRIMARY KEY / UNIQUE constraint's columns. `only` restricts
    /// the scan to the named constraint (for ON CONFLICT arbiters);
    /// None scans every unique constraint. Returns the conflicting
    /// constraint's name and row id. NULL key parts never conflict
    /// (PostgreSQL semantics).
    fn temp_scan_constraint(
        &self,
        t: &Table,
        values: &[Value],
        exclude_row_id: Option<u64>,
        snap: &Snapshot,
        own: u64,
        only: Option<&str>,
    ) -> Option<(String, u64)> {
        let mut constraints: Vec<&UniqueDef> = t.uniques.iter().collect();
        if let Some(pk) = &t.pkey {
            constraints.push(pk);
        }
        for c in constraints {
            if only.is_some_and(|name| name != c.name) {
                continue;
            }
            let cols: Vec<usize> = c.cols.iter().filter_map(|n| t.column_index(n)).collect();
            if cols.len() != c.cols.len() {
                continue; // schema changed under us; shouldn't happen
            }
            if cols.iter().any(|&p| matches!(values[p], Value::Null)) {
                continue; // NULLs never conflict
            }
            for r in &t.rows {
                if Some(r.id) == exclude_row_id {
                    continue;
                }
                let alive = if r.xmax == own {
                    false // deleted by us: not a conflict
                } else {
                    r.xmin == own || row_visible(r, snap, own)
                };
                if !alive {
                    continue;
                }
                let same = cols
                    .iter()
                    .all(|&p| !matches!(r.values[p], Value::Null) && r.values[p] == values[p]);
                if same {
                    return Some((c.name.clone(), r.id));
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
        session: u64,
    ) -> Option<u64> {
        let t = self.find_table(table, snap, own, session)?;
        // v0.22: temp tables have no backing indexes; match the arbiter
        // against the table's constraint definitions and scan.
        if self.is_temp_table(session, table) {
            return self
                .temp_scan_constraint(t, values, exclude_row_id, snap, own, Some(index_name))
                .map(|(_, id)| id);
        }
        let ix = self
            .visible_indexes_for(table, snap, own, session)
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

    /// v0.14: commit-time unique recheck. Like [`Self::unique_violation`],
    /// but against rows committed by OTHER transactions — i.e. visible to
    /// a fresh snapshot, not to our own (possibly stale) one. Catches the
    /// race where a concurrent transaction committed the same unique key
    /// after our statement-time check ran (see
    /// `tests/conformance/isolation_specs.py::insert-conflict-do-nothing`).
    /// Returns the conflicting index name. `own_row_id` (our inserted row)
    /// is excluded.
    pub fn committed_unique_violation(
        &self,
        txns: &TxnManager,
        table: &str,
        values: &[Value],
        own_row_id: u64,
        own: u64,
        session: u64,
    ) -> Option<String> {
        // Fresh snapshot: everything committed as of now is visible.
        // `own` is still in `active`; row_visible would accept our own
        // rows, so they are excluded explicitly by id below.
        let fresh = Snapshot {
            active: txns.active.iter().copied().collect(),
            next_xid: txns.next_xid,
        };
        // v0.22: temp tables are session-local — no concurrent transaction
        // can write to them, so the commit-time recheck is vacuous.
        if self.is_temp_table(session, table) {
            return None;
        }
        let t = self.find_table(table, &fresh, own, session)?;
        for ix in self.visible_indexes_for(table, &fresh, own, session) {
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
                if id == own_row_id {
                    continue;
                }
                let Some(pos) = t.row_pos(id) else {
                    continue; // vacuumed away: cannot conflict
                };
                let r = &t.rows[pos];
                if r.xmin != own && row_visible(r, &fresh, own) {
                    return Some(ix.def.name.clone());
                }
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
    /// v0.13: replication slots by name. Cluster-global, non-transactional;
    /// WAL-logged (ReplSlot* records) and checkpointed for durability.
    pub repl_slots: HashMap<String, ReplSlot>,
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
            // v0.13: replication slots.
            repl_slots: HashMap::new(),
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
        // Collect (id, values, toast flags) of the dead versions first:
        // index cleanup needs each version's key (hence its values), and
        // the toast flags drive out-of-line chunk cleanup below.
        let mut dead: Vec<(u64, Row, Vec<u32>)> = Vec::new();
        if let Some(versions) = self.db.tables.get_mut(name) {
            for t in versions {
                for v in t.rows.iter().filter(|v| version_dead_to_all(txns, v)) {
                    dead.push((v.id, v.values.clone(), v.toast.clone()));
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
        for (id, values, _) in &dead {
            self.db.index_remove_row(name, *id, values);
        }
        // v0.39: remove the dead versions' out-of-line toast chunks. A
        // dead main-table version can no longer be seen by any snapshot,
        // so its chunks are unreachable; leaving them would leak toast
        // rows (and their index entries) forever. Like the rest of
        // vacuum this is non-transactional surgery — but chunk rows are
        // only ever read through the flags of a live main version, so
        // removing a dead version's chunks cannot orphan live data.
        self.vacuum_toast_chunks(name, &dead);
        removed
    }

    /// v0.39: remove toast-table chunk rows for the value ids referenced
    /// by the given dead main-table versions' toast flags. No-op when the
    /// table has no toast table or no dead version was toasted.
    fn vacuum_toast_chunks(&mut self, name: &str, dead: &[(u64, Row, Vec<u32>)]) {
        let vids: Vec<u32> = dead
            .iter()
            .flat_map(|(_, _, toast)| toast.iter().copied())
            .filter(|v| *v != 0)
            .collect();
        if vids.is_empty() {
            return;
        }
        // Resolve the toast table name from the main table's relid. The
        // borrow ends before the mutable work below.
        let toast_name: Option<String> = self
            .db
            .tables
            .get(name)
            .and_then(|versions| versions.last())
            .and_then(|t| {
                if t.toast_relid == 0 {
                    None
                } else {
                    self.db.table_name_by_oid(t.toast_relid)
                }
            });
        let Some(toast_name) = toast_name else {
            return;
        };
        let Some(versions) = self.db.tables.get_mut(&toast_name) else {
            return;
        };
        let mut gone: Vec<(u64, Row)> = Vec::new();
        for t in versions {
            t.rows.retain(|r| {
                let chunked = match r.values.first() {
                    Some(Value::Int(vid)) => vids.contains(&(*vid as u32)),
                    _ => false,
                };
                if chunked {
                    gone.push((r.id, r.values.clone()));
                    false
                } else {
                    true
                }
            });
            if !gone.is_empty() {
                t.rebuild_row_index();
            }
        }
        // The tables borrow ends here; index cleanup needs `&mut self`.
        for (id, values) in &gone {
            self.db.index_remove_row(&toast_name, *id, values);
        }
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

/// Visibility for role versions (v0.11): same rules as tables.
pub fn role_visible(r: &Role, snap: &Snapshot, own: u64) -> bool {
    let created_ok = r.created_xmin == own
        || (r.created_xmin < snap.next_xid && !snap.active.contains(&r.created_xmin));
    if !created_ok {
        return false;
    }
    if r.dropped_xmax == 0 {
        return true;
    }
    if r.dropped_xmax == own {
        return false;
    }
    !(r.dropped_xmax < snap.next_xid && !snap.active.contains(&r.dropped_xmax))
}

// ---------------------------------------------------------------------------
// Roles, privileges, and access control (v0.11)
// ---------------------------------------------------------------------------

/// Table/sequence/database privilege bits (v0.11).
pub const PRIV_SELECT: u32 = 1;
pub const PRIV_INSERT: u32 = 2;
pub const PRIV_UPDATE: u32 = 4;
pub const PRIV_DELETE: u32 = 8;
pub const PRIV_TRUNCATE: u32 = 16;
pub const PRIV_REFERENCES: u32 = 32;
pub const PRIV_TRIGGER: u32 = 64;
pub const PRIV_USAGE: u32 = 128; // sequences
pub const PRIV_CONNECT: u32 = 256; // database
pub const PRIV_ALL_TABLE: u32 = PRIV_SELECT
    | PRIV_INSERT
    | PRIV_UPDATE
    | PRIV_DELETE
    | PRIV_TRUNCATE
    | PRIV_REFERENCES
    | PRIV_TRIGGER;

/// One GRANT entry: `role` holds `privs` on the object the entry is
/// attached to (a table's `acl`, a sequence's `acl`, or the database's
/// `db_acl`).
#[derive(Clone, Debug)]
pub struct AclEntry {
    pub role: String,
    pub privs: u32,
}

/// v0.11: a column-level GRANT entry (`GRANT SELECT (a, b) ON t TO r`).
/// `privs` applies only to the named `columns`. Owner and superusers
/// bypass ACLs entirely, so they never need entries here.
#[derive(Clone, Debug)]
pub struct ColAclEntry {
    pub role: String,
    pub privs: u32,
    pub columns: Vec<String>,
}

/// A database role (v0.11). Versioned like tables so CREATE / DROP /
/// ALTER ROLE are transactional under MVCC.
///
/// Passwords are never stored: `password` holds the SCRAM-SHA-256
/// verifier (salt, iteration count, StoredKey, ServerKey). `None` means
/// the role has no password and can only connect via trust auth.
#[derive(Clone, Debug)]
pub struct Role {
    pub name: String,
    pub password: Option<crate::crypto::ScramVerifier>,
    pub can_login: bool,
    pub superuser: bool,
    /// -1 = unlimited, like PostgreSQL.
    pub connlimit: i32,
    /// Group roles this role is a member of (`GRANT group TO member`).
    /// Membership confers the group's GRANTed privileges (transitively).
    pub memberships: Vec<RoleMembership>,
    /// Password expiry from VALID UNTIL, as the raw timestamp literal.
    /// None = never expires. Enforced at SCRAM authentication.
    pub valid_until: Option<String>,
    /// Xid of the CREATE ROLE transaction (0 = bootstrap role, always
    /// visible: `xid_committed(0)` is true).
    pub created_xmin: u64,
    /// Xid of the DROP ROLE transaction; 0 = not dropped.
    pub dropped_xmax: u64,
}

/// One `GRANT group_role TO member` edge: `member` inherits the
/// privileges granted to `group_role`.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleMembership {
    /// The group role.
    pub role: String,
    /// The role that ran the GRANT (for pg_auth_members.grantor).
    pub grantor: String,
}

impl Role {
    /// The bootstrap superuser. Always present (created by
    /// `Database::new`, re-serialized by checkpoints).
    pub fn bootstrap_postgres() -> Self {
        Role {
            name: "postgres".to_string(),
            password: None,
            can_login: true,
            superuser: true,
            connlimit: -1,
            memberships: Vec::new(),
            valid_until: None,
            created_xmin: 0,
            dropped_xmax: 0,
        }
    }
}

/// Validate a VALID UNTIL literal at DDL time. Accepts any timestamp the
/// datetime parser handles, plus 'infinity' (never expires, like
/// PostgreSQL).
pub fn check_valid_until(vu: &str) -> Result<(), String> {
    if vu.eq_ignore_ascii_case("infinity") {
        return Ok(());
    }
    crate::datetime::parse_timestamp(vu).map(|_| ())
}

pub fn normalize_valid_until(vu: &str) -> Option<String> {
    if vu.eq_ignore_ascii_case("infinity") {
        None
    } else {
        Some(vu.to_string())
    }
}

/// Whether the role's password has expired (VALID UNTIL passed).
/// Unparseable literals (impossible after DDL validation) are treated
/// as not expired.
pub fn password_expired(valid_until: &Option<String>) -> bool {
    match valid_until {
        None => false,
        Some(vu) => match crate::datetime::parse_timestamp(vu) {
            Ok(t) => crate::datetime::now_micros() >= t,
            Err(_) => false,
        },
    }
}

/// Transitive closure of `role` plus every group role it is (transitively)
/// a member of. Used for privilege inheritance: a GRANT to any role in
/// the closure counts. Owner and superuser status are NOT inherited
/// (PostgreSQL requires SET ROLE for those, which rustgres does not
/// implement yet).
pub fn role_closure(db: &Database, role: &str, snap: &Snapshot, own: u64) -> Vec<String> {
    let mut out = vec![role.to_string()];
    let mut i = 0;
    while i < out.len() {
        let name = out[i].clone();
        i += 1;
        if let Some(r) = db.find_role(&name, snap, own) {
            for m in &r.memberships {
                if !out.iter().any(|n| n == &m.role) {
                    out.push(m.role.clone());
                }
            }
        }
    }
    out
}

/// Effective privilege bits `role` holds on table `t`: superusers and
/// the table owner implicitly hold everything; everyone else gets the
/// union of their GRANT entries, including entries granted to group
/// roles they are a member of.
pub fn table_privs(db: &Database, role: &str, t: &Table, snap: &Snapshot, own: u64) -> u32 {
    table_privs_in(db, role, t, &role_closure(db, role, snap, own), snap, own)
}

/// [`table_privs`] with a precomputed [`role_closure`]: the hot privilege
/// loops call this so the closure is built once per statement, not once
/// per column (callgrind showed the closure dominating SELECT checks).
pub fn table_privs_in(
    db: &Database,
    role: &str,
    t: &Table,
    closure: &[String],
    snap: &Snapshot,
    own: u64,
) -> u32 {
    if is_superuser_snap(db, role, snap, own) || t.owner == role {
        return PRIV_ALL_TABLE;
    }
    let mut privs = 0u32;
    for e in &t.acl {
        if closure.iter().any(|n| n == &e.role) {
            privs |= e.privs;
        }
    }
    privs
}

/// Effective privilege bits `role` holds on `column` of table `t`:
/// the table-level bits plus any column-level grant covering that
/// column (inherited through role memberships). Owners and superusers
/// get everything via [`table_privs`].
/// Effective privilege bits `role` holds on `column` of table `t`:
/// the table-level bits plus any column-level grant covering that
/// column (inherited through role memberships). Owners and superusers
/// get everything via [`table_privs_in`]. Takes a precomputed
/// [`role_closure`] so hot loops build it once per statement.
pub fn column_privs_in(
    db: &Database,
    role: &str,
    t: &Table,
    column: &str,
    closure: &[String],
    snap: &Snapshot,
    own: u64,
) -> u32 {
    let mut privs = table_privs_in(db, role, t, closure, snap, own);
    if privs == PRIV_ALL_TABLE {
        return privs;
    }
    for e in &t.col_acl {
        if closure.iter().any(|n| n == &e.role) && e.columns.iter().any(|c| c == column) {
            privs |= e.privs;
        }
    }
    privs
}

/// Whether `role` holds any column-level grant containing `bit` on
/// table `t` (used as a coarse gate before the precise per-column
/// check).
pub fn has_col_priv(
    db: &Database,
    role: &str,
    t: &Table,
    bit: u32,
    snap: &Snapshot,
    own: u64,
) -> bool {
    if table_privs(db, role, t, snap, own) & bit == bit {
        return true;
    }
    let closure = role_closure(db, role, snap, own);
    t.col_acl
        .iter()
        .any(|e| closure.iter().any(|n| n == &e.role) && e.privs & bit == bit)
}

/// Effective USAGE bits `role` holds on sequence `s`.
pub fn sequence_privs(db: &Database, role: &str, s: &Sequence, snap: &Snapshot, own: u64) -> u32 {
    if is_superuser_snap(db, role, snap, own) || s.owner == role {
        return PRIV_USAGE;
    }
    let closure = role_closure(db, role, snap, own);
    let mut privs = 0u32;
    for e in &s.acl {
        if closure.iter().any(|n| n == &e.role) {
            privs |= e.privs;
        }
    }
    privs
}

/// Whether `role` may open a session at all (database CONNECT).
/// Default: everyone may connect (matches a fresh PostgreSQL
/// install where PUBLIC has CONNECT); REVOKE CONNECT takes it away.
pub fn db_connect_allowed(db: &Database, role: &str, snap: &Snapshot, own: u64) -> bool {
    if is_superuser_snap(db, role, snap, own) {
        return true;
    }
    // No explicit ACL = default allow (PostgreSQL's PUBLIC grant).
    let closure = role_closure(db, role, snap, own);
    let mut denied = false;
    let mut allowed = false;
    for e in &db.db_acl {
        if closure.iter().any(|n| n == &e.role) {
            if e.privs & PRIV_CONNECT != 0 {
                allowed = true;
            } else {
                // An entry without CONNECT for this role counts as an
                // explicit revoke.
                denied = true;
            }
        }
    }
    if denied && !allowed {
        return false;
    }
    true
}

/// Whether `role` is a superuser under (`snap`, `own`). Unknown roles
/// are not superusers.
pub fn is_superuser_snap(db: &Database, role: &str, snap: &Snapshot, own: u64) -> bool {
    db.find_role(role, snap, own)
        .map(|r| r.superuser)
        .unwrap_or(false)
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
    // --- v0.13: an UPDATE is a first-class op (delete old version +
    // insert new version), not an adjacent DeleteRow/InsertRow pair.
    // Exec-time pairing is exact — unlike pairing at commit/decode time,
    // it cannot mislabel `DELETE; INSERT` in one transaction as UPDATE —
    // and it carries the old values the logical decoder needs.
    UpdateRow {
        table: String,
        old_id: u64,
        new_id: u64,
        prev_xmax: u64,
        old_values: Row,
    },
    CreateTable {
        name: String,
    },
    DropTable {
        name: String,
        prev_xmax: u64,
    },
    // --- v0.22: temp-table DDL. Temp tables live in
    // `Database::temp_tables[session]` (not the versioned catalog), so
    // their undo ops carry the whole previous `Table` (or None) and the
    // owning session id. They are never WAL-logged.
    CreateTempTable {
        session: u64,
        name: String,
    },
    DropTempTable {
        session: u64,
        name: String,
        prev: Option<Table>,
    },
    // --- v0.22: CREATE/DROP TYPE. Types live in `Database::types` (not
    // the versioned catalog); the op carries the previous entry (or
    // None) so undo restores it exactly. Types are never WAL-logged.
    CreateType {
        name: String,
        prev: Option<ShellType>,
    },
    DropType {
        name: String,
        prev: Option<ShellType>,
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
    // --- v0.11: role DDL. DropRole / AlterRole carry the previous role
    // version so undo restores it exactly.
    CreateRole {
        name: String,
    },
    DropRole {
        name: String,
        prev: Role,
    },
    AlterRole {
        name: String,
        prev: Role,
    },
    /// v0.11: database-level GRANT/REVOKE CONNECT. Carries the previous
    /// ACL for undo; the record logs the new full ACL.
    DbAcl {
        prev: Vec<AclEntry>,
    },
}

/// Undo a single write op. Each undo is conditional on the version still
/// being ours: a concurrent transaction may have overwritten xmax after
/// us (last-writer-wins, no row locking in v0.5), in which case their
/// op owns the version now and ours must not clobber it.
pub fn undo_write_op(eng: &mut Engine, own: u64, op: &WriteOp) {
    match op {
        WriteOp::InsertRow { table, row_id } => {
            // v0.22: the row may live in a session-local temp table
            // (`remove_own_version` searches both). Only permanent-table
            // rows have global index entries to clean up.
            if let Some((values, is_temp)) = eng.db.remove_own_version(*row_id, own) {
                if !is_temp {
                    eng.db.index_remove_row(table, *row_id, &values);
                }
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
        // v0.13: undo an UPDATE = remove the new version (exactly the
        // InsertRow undo) and restore the old version's xmax (exactly
        // the DeleteRow undo). v0.22: both halves are temp-aware —
        // `remove_own_version` and `find_row_version_mut` search session
        // temp tables too, and temp rows have no index entries to clean.
        WriteOp::UpdateRow {
            table,
            old_id,
            new_id,
            prev_xmax,
            old_values: _,
        } => {
            if let Some((values, is_temp)) = eng.db.remove_own_version(*new_id, own) {
                if !is_temp {
                    eng.db.index_remove_row(table, *new_id, &values);
                }
            }
            if let Some(v) = eng.db.find_row_version_mut(*old_id) {
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
        // --- v0.22: temp-table undos. Temp tables are single-version and
        // session-local; undo simply removes a created temp table or
        // restores a dropped one.
        WriteOp::CreateTempTable { session, name } => {
            if let Some(tmps) = eng.db.temp_tables.get_mut(session) {
                tmps.remove(name);
                if tmps.is_empty() {
                    eng.db.temp_tables.remove(session);
                }
            }
        }
        WriteOp::DropTempTable {
            session,
            name,
            prev,
        } => {
            let tmps = eng.db.temp_tables.entry(*session).or_default();
            match prev {
                Some(t) => {
                    tmps.insert(name.clone(), t.clone());
                }
                None => {
                    tmps.remove(name);
                }
            }
            if tmps.is_empty() {
                eng.db.temp_tables.remove(session);
            }
        }
        // --- v0.22: type DDL undos. Restore the previous entry, or
        // remove the type when there was none.
        WriteOp::CreateType { name, prev } | WriteOp::DropType { name, prev } => match prev {
            Some(t) => {
                eng.db.types.insert(name.clone(), t.clone());
            }
            None => {
                eng.db.types.remove(name);
            }
        },
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
            rewrite_rows,
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
            // v0.42: a row-rewriting ALTER (ADD/DROP COLUMN) mints fresh
            // row ids and migrates index entries at exec time, so rolling
            // back must restore the old id->key mapping or index scans
            // would miss the restored rows. Rebuild every live index on
            // the table from the restored version's rows. Undo runs in
            // reverse, so indexes created after the ALTER are already
            // gone and dropped ones already restored — the defs present
            // match the restored column positions.
            if *rewrite_rows {
                let rows: Vec<(Row, u64)> =
                    prev.rows.iter().map(|r| (r.values.clone(), r.id)).collect();
                for ix in eng.db.indexes.values_mut() {
                    let ours = ix.def.table == *name
                        || renamed_to.as_deref().is_some_and(|rt| ix.def.table == rt);
                    if ours && ix.def.dropped_xmax == 0 {
                        ix.tree.clear();
                        for (values, id) in &rows {
                            let key = ix.key_for(values);
                            ix.insert(key, *id);
                        }
                    }
                }
            }
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
        // --- v0.11: role DDL undo. Conditional on the version still
        // being ours, like the table/version cases above.
        WriteOp::CreateRole { name } => {
            if let Some(versions) = eng.db.roles.get_mut(name) {
                versions.retain(|r| r.created_xmin != own);
                if versions.is_empty() {
                    eng.db.roles.remove(name);
                }
            }
        }
        WriteOp::DropRole { name, prev } => {
            let ours = eng
                .db
                .roles
                .get(name)
                .map(|vs| {
                    vs.iter()
                        .any(|r| r.created_xmin == prev.created_xmin && r.dropped_xmax == own)
                })
                .unwrap_or(false);
            if ours {
                let mut restored = prev.clone();
                restored.dropped_xmax = 0;
                if let Some(versions) = eng.db.roles.get_mut(name) {
                    for r in versions.iter_mut() {
                        if r.created_xmin == prev.created_xmin {
                            *r = restored;
                            break;
                        }
                    }
                }
            }
        }
        WriteOp::AlterRole { name, prev } => {
            if let Some(versions) = eng.db.roles.get_mut(name) {
                for r in versions.iter_mut() {
                    if r.created_xmin == prev.created_xmin {
                        *r = prev.clone();
                        break;
                    }
                }
            }
        }
        WriteOp::DbAcl { prev } => {
            eng.db.db_acl = prev.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.57: `namein` truncates at 63 bytes (NAMEDATALEN-1) without
    /// error, staying on a UTF-8 char boundary.
    #[test]
    fn truncate_name_at_63_bytes() {
        assert_eq!(truncate_name("abc"), "abc");
        assert_eq!(truncate_name(""), "");
        let s63: String = "x".repeat(63);
        assert_eq!(truncate_name(&s63), s63);
        let s64: String = "y".repeat(64);
        assert_eq!(truncate_name(&s64).len(), 63);
        // Multibyte: 32 two-byte chars = 64 bytes -> cut to 31 chars (62
        // bytes), not a split UTF-8 sequence.
        let mb: String = "é".repeat(32);
        let t = truncate_name(&mb);
        assert_eq!(t.chars().count(), 31);
        assert_eq!(t.len(), 62);
        // 63 ASCII + one multibyte char = 65 bytes -> keep the 63 ASCII.
        let mixed = format!("{}é", "z".repeat(63));
        assert_eq!(truncate_name(&mixed), "z".repeat(63));
        // 62 ASCII + multibyte char: the char starts at byte 62, adding
        // it would exceed 63, so it is dropped.
        let mixed2 = format!("{}é", "z".repeat(62));
        assert_eq!(truncate_name(&mixed2), "z".repeat(62));
    }

    /// A clone of a `Value::Text` must share the text bytes. It must not
    /// copy them. See issue #8.
    #[test]
    fn text_clone_shares_one_buffer() {
        let a = Value::text("a text long enough to need the heap");
        let b = a.clone();
        let (Value::Text(x), Value::Text(y)) = (&a, &b) else {
            panic!("both values are Text");
        };
        assert_eq!(
            x.as_ptr(),
            y.as_ptr(),
            "Value::Text clone must share the buffer, not copy it"
        );
        assert_eq!(&**x, "a text long enough to need the heap");
    }

    /// A shared buffer must not change equality or order. Two equal
    /// texts must compare equal. Two different texts must not.
    #[test]
    fn text_equality_and_order_ignore_sharing() {
        use crate::index::IndexKey;
        let a = Value::text("abc");
        let b = Value::text("abc");
        let c = a.clone();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_ne!(a, Value::text("abd"));
        // Order comes from IndexKey; `Value` has no `Ord`.
        let key = |v: &Value| IndexKey(vec![v.clone()]);
        assert!(key(&a) < key(&Value::text("abd")));
        assert!(key(&Value::text("ab")) < key(&a));
        assert_eq!(key(&a).cmp(&key(&c)), std::cmp::Ordering::Equal);
    }

    fn engine_with_table() -> Engine {
        let mut eng = Engine::new();
        eng.db.tables.insert(
            "t".to_string(),
            vec![{
                let mut t = Table::new(vec![("a".to_string(), ColType::Int)], 1);
                t.push_version(RowVersion {
                    id: 1,
                    values: Row::new(vec![Value::Int(1)]),
                    xmin: 1,
                    xmax: 0,
                    toast: Vec::new(),
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
            values: Row::new(vec![Value::Int(2)]),
            xmin: 1,
            xmax: 8,
            toast: Vec::new(),
        });
        t.push_version(RowVersion {
            id: 3,
            values: Row::new(vec![Value::Int(3)]),
            xmin: 1,
            xmax: 0,
            toast: Vec::new(),
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
            values: Row::new(vec![Value::Int(9)]),
            xmin: 7,
            xmax: 0,
            toast: Vec::new(),
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

    // v0.18: numeric special values (NaN, Infinity, -Infinity).
    // v0.25: PG rejects a sign before NaN (`'+NaN'`/`'-NaN'` are 22P02).
    #[test]
    fn numeric_special_parse_case_insensitive() {
        for (s, expect) in [
            ("NaN", NumericSpecial::NaN),
            ("nan", NumericSpecial::NaN),
            ("NAN", NumericSpecial::NaN),
            ("Inf", NumericSpecial::PosInf),
            ("inf", NumericSpecial::PosInf),
            ("INF", NumericSpecial::PosInf),
            ("Infinity", NumericSpecial::PosInf),
            ("infinity", NumericSpecial::PosInf),
            ("-Infinity", NumericSpecial::NegInf),
            ("-inf", NumericSpecial::NegInf),
            ("+Inf", NumericSpecial::PosInf),
        ] {
            let n = Numeric::parse(s).expect(s);
            assert_eq!(n.special, expect, "parse {}", s);
        }
    }

    // v0.25: signed NaN is a syntax error (PG 16).
    #[test]
    fn numeric_signed_nan_rejected() {
        for s in ["+NaN", "-NaN", "+nan", "-nan", " +NaN ", " -NAN "] {
            assert_eq!(
                Numeric::parse(s),
                Err(NumericParseError::Syntax),
                "parse {}",
                s
            );
        }
        // Unsigned NaN still parses.
        assert_eq!(Numeric::parse("NaN").unwrap().special, NumericSpecial::NaN);
        assert_eq!(
            Numeric::parse(" nan ").unwrap().special,
            NumericSpecial::NaN
        );
    }

    #[test]
    fn numeric_special_canonical_output() {
        assert_eq!(Numeric::nan().to_text(), "NaN");
        assert_eq!(Numeric::infinity().to_text(), "Infinity");
        assert_eq!(Numeric::neg_infinity().to_text(), "-Infinity");
    }

    #[test]
    fn numeric_special_arithmetic() {
        let nan = Numeric::nan();
        let inf = Numeric::infinity();
        let neg_inf = Numeric::neg_infinity();
        let one = Numeric::from_i64(1);
        let zero = Numeric::from_i64(0);

        // NaN propagates
        assert_eq!(nan.checked_add(&one).unwrap().special, NumericSpecial::NaN);
        assert_eq!(
            inf.checked_add(&neg_inf).unwrap().special,
            NumericSpecial::NaN
        );
        assert_eq!(inf.checked_mul(&zero).unwrap().special, NumericSpecial::NaN);

        // Infinity arithmetic
        assert_eq!(
            inf.checked_add(&one).unwrap().special,
            NumericSpecial::PosInf
        );
        assert_eq!(
            neg_inf.checked_mul(&one).unwrap().special,
            NumericSpecial::NegInf
        );
    }

    #[test]
    fn numeric_special_ordering() {
        // -Inf < finite < Inf < NaN
        let neg_inf = Numeric::neg_infinity();
        let one = Numeric::from_i64(1);
        let inf = Numeric::infinity();
        let nan = Numeric::nan();

        assert!(neg_inf < one);
        assert!(one < inf);
        assert!(inf < nan);
        assert!(neg_inf < nan);
    }

    /// v0.56: exact-decimal support behind width_bucket. The classic
    /// hazard is 1e100/(1e100+1) rounding to 1.0 in float64, so the
    /// bucket math must be exact on 300+ digit integers.
    #[test]
    fn biguint_exact_arithmetic() {
        use std::cmp::Ordering;
        // 10^100 + 1, then back down: add/sub round-trip.
        let a = BigUint::from_decimal_str(&format!("1{}", "0".repeat(100)));
        let mut b = a.clone();
        b.add_assign(&BigUint::from_u64(1));
        assert_eq!(b.cmp(&a), Ordering::Greater);
        b.sub_assign(&BigUint::from_u64(1));
        assert_eq!(b.cmp(&a), Ordering::Equal);
        // Bounded floor division: floor(10 * 1e100 / (1e100+1)) = 9,
        // the bucket-quotient heart of width_bucket.
        let mut num = a.clone();
        num.mul_small_assign(10);
        let mut den = a.clone();
        den.add_assign(&BigUint::from_u64(1));
        let q = BigUint::floor_div_bounded(&num, &den).expect("quotient < 2^31");
        assert_eq!(q.to_u64(), Some(9));
        // A quotient at or above 2^31 is reported as None, not wrapped.
        let big = BigUint::from_decimal_str("4294967296");
        assert!(BigUint::floor_div_bounded(&big, &BigUint::from_u64(1)).is_none());
        // Schoolbook mul: 10^18 * 10^18 = 10^36 exactly.
        let e18 = BigUint::from_decimal_str(&format!("1{}", "0".repeat(18)));
        let e36 = e18.mul(&e18);
        let want36 = BigUint::from_decimal_str(&format!("1{}", "0".repeat(36)));
        assert_eq!(e36.cmp(&want36), Ordering::Equal);
    }

    #[test]
    fn bigdec_parse_compare_sub() {
        use std::cmp::Ordering;
        let n = Numeric::parse("123.450").unwrap();
        let d = BigDec::from_numeric(&n).unwrap();
        assert_eq!(
            d.cmp(&BigDec::parse_decimal("123.450").unwrap()),
            Ordering::Equal
        );
        assert_eq!(
            d.cmp(&BigDec::parse_decimal("123.45").unwrap()),
            Ordering::Equal
        );
        // Underscore separators validate like the numeric parser.
        assert!(BigDec::parse_decimal("1_0.5").is_ok());
        assert!(BigDec::parse_decimal("1__0").is_err());
        assert!(BigDec::parse_decimal("1.5_").is_err());
        assert!(BigDec::parse_decimal("abc").is_err());
        // Comparison across scales: -1e100 < 1.
        let lo = BigDec::parse_decimal(&format!("-1{}", "0".repeat(100))).unwrap();
        let one = BigDec::parse_decimal("1").unwrap();
        assert_eq!(lo.cmp(&one), Ordering::Less);
        // 1 - (-1e100) = 1e100 + 1 exactly: magnitude has 101 digits.
        let hi = one.sub(&lo);
        assert!(!hi.is_zero());
        let mut want = BigUint::from_decimal_str(&format!("1{}", "0".repeat(100)));
        want.add_assign(&BigUint::from_u64(1));
        assert_eq!(hi.mag().cmp(&want), Ordering::Equal);
        assert_eq!(hi.scale(), 0);
        // from_i64 extremes order sanely.
        assert_eq!(
            BigDec::from_i64(i64::MIN).cmp(&BigDec::from_i64(-1)),
            Ordering::Less
        );
        // Scale alignment in sub: 5.0000000000001 - 5 = 1e-13.
        let a = BigDec::parse_decimal("5.0000000000001").unwrap();
        let b = BigDec::parse_decimal("5").unwrap();
        let diff = a.sub(&b);
        let tiny = BigDec::parse_decimal("0.0000000000001").unwrap();
        assert_eq!(diff.cmp(&tiny), Ordering::Equal);
    }
}
