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
    Int,                         // INT4, OID 23
    BigInt,                      // INT8, OID 20 (v0.7)
    SmallInt,                    // INT2, OID 21 (v0.7)
    Float,                       // FLOAT8, OID 701
    Float4,                      // FLOAT4, OID 700 (v0.7)
    Numeric(Option<(u32, i32)>), // NUMERIC, OID 1700 (v0.7); v0.60: optional
    // (precision, scale) typmod, like PG19 `numeric(p,s)`.
    Text, // OID 25
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
            ColType::Numeric(..) => 1700, // NUMERIC
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
            ColType::Numeric(..) => "numeric",
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
            ColType::Numeric(..) => "numeric",
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
            // v0.60: PG19's format_type shows the numeric typmod too.
            ColType::Numeric(Some((p, s))) => format!("numeric({},{})", p, s),
            ColType::Numeric(None) => "numeric".to_string(),
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
                | ColType::Numeric(..)
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
            ColType::Numeric(..) => toast_storage::MAIN,
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
///
/// v0.61: equality is by *value* only — `dscale` is deliberately
/// ignored so `1.50 = 1.5` still holds. (`unscaled`/`scale` are kept
/// canonical by normalization, so field comparison is exact.)
///
/// v0.63: arbitrary-precision mantissa for values whose unscaled
/// magnitude exceeds 38 digits (i128 range), completing v0.62's
/// transcendental cluster (huge `exp`/`power`/`log` results, giant
/// literals, big `div`). When `big` is `Some(mag)`, `unscaled` holds
/// only the sign (-1, 0, 1 — never 0 with a nonzero magnitude) and
/// `mag` is the exact positive unscaled magnitude with no leading
/// zero limbs. All sign/zero tests on `unscaled` keep working
/// unchanged; magnitude reads must go through [`Numeric::mag`].
/// Invariants: `big = Some` implies `unscaled ∈ {-1, 1}`,
/// `mag > 0`, and no trailing zero limbs while `scale > 0`
/// (same normalization as `new`). Zero is always the plain
/// `unscaled = 0, big = None` form.
#[derive(Clone, Debug)]
pub struct Numeric {
    pub unscaled: i128,
    pub scale: i32,
    /// v0.61: PG19 `NUMERIC_DSCALE` — the display/declared scale, kept
    /// separate from `scale` (the stored minimal scale). PG retains the
    /// literal's declared fractional digits (`select 21.00` -> dscale 2)
    /// and pads output (`to_text`) with trailing zeros to `dscale`.
    /// Arithmetic propagates it per PG19's rules (add/sub: max;
    /// mul: sum; div/power: rscale; round: target scale). Always >= 0.
    pub dscale: i32,
    pub special: NumericSpecial,
    pub big: Option<Box<BigUint>>,
}

impl PartialEq for Numeric {
    fn eq(&self, other: &Self) -> bool {
        if self.special != other.special || self.scale != other.scale {
            return false;
        }
        match (&self.big, &other.big) {
            (None, None) => self.unscaled == other.unscaled,
            // Big values are normalized, so `unscaled` holds the sign
            // and `mag` the exact magnitude: field comparison is exact.
            _ => self.unscaled == other.unscaled && self.mag() == other.mag(),
        }
    }
}
impl Eq for Numeric {}

/// v0.61: failure mode of [`Numeric::power_int`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerError {
    /// PG19 SQLSTATE 22003 "value overflows numeric format".
    Overflow,
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

/// Outcome of [`Numeric::apply_typmod`]: PG19 raises
/// `ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE` (22003, "numeric field
/// overflow") in both cases, with a DETAIL naming the typmod.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypmodError {
    /// +/-Infinity cannot be stored in a typmod-constrained column
    /// (PG19: "cannot hold an infinite value").
    Infinite,
    /// The value rounded to `scale` needs more than
    /// `precision - scale` digits left of the decimal point
    /// (PG19: "must round to an absolute value less than ...").
    Overflow,
}

impl Numeric {
    /// Build and normalize.
    pub fn new(unscaled: i128, scale: i32) -> Self {
        let mut n = Numeric {
            unscaled,
            scale,
            // v0.61: the display scale is the declared (pre-normalization)
            // scale — PG's dscale survives trailing-zero stripping, so
            // `new(150, 2)` ("1.50") keeps dscale 2 while storing (15, 1).
            dscale: scale.max(0),
            special: NumericSpecial::Finite,
            big: None,
        };
        n.normalize();
        n
    }

    /// v0.61: like [`Numeric::new`], but the display scale is set
    /// explicitly (PG19 operators that fix the result's display scale,
    /// e.g. `round(x, s)` -> dscale `s`, `x / y` -> rscale).
    pub fn with_dscale(mut self, dscale: i32) -> Self {
        self.dscale = dscale.max(0);
        self
    }

    /// NaN (v0.18).
    pub fn nan() -> Self {
        Numeric {
            unscaled: 0,
            scale: 0,
            dscale: 0,
            special: NumericSpecial::NaN,
            big: None,
        }
    }

    /// +Infinity (v0.18).
    pub fn infinity() -> Self {
        Numeric {
            unscaled: 0,
            scale: 0,
            dscale: 0,
            special: NumericSpecial::PosInf,
            big: None,
        }
    }

    /// -Infinity (v0.18).
    pub fn neg_infinity() -> Self {
        Numeric {
            unscaled: 0,
            scale: 0,
            dscale: 0,
            special: NumericSpecial::NegInf,
            big: None,
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
    /// v0.63: big-mantissa aware — under the sign convention, negating
    /// `unscaled` (±1) is exact; the magnitude is preserved.
    #[allow(dead_code)]
    pub fn neg(&self) -> Self {
        match self.special {
            NumericSpecial::NaN => Numeric::nan(),
            NumericSpecial::PosInf => Numeric::neg_infinity(),
            NumericSpecial::NegInf => Numeric::infinity(),
            NumericSpecial::Finite => {
                // `i128::MIN` is unreachable: parse() rejects magnitudes
                // that large and arithmetic is overflow-checked.
                // v0.61: negation preserves the display scale.
                let mut n = Numeric::new(self.unscaled.saturating_neg(), self.scale)
                    .with_dscale(self.dscale);
                if self.big.is_some() {
                    n.big = self.big.clone();
                }
                n
            }
        }
    }

    fn normalize(&mut self) {
        if self.special != NumericSpecial::Finite {
            self.unscaled = 0;
            self.scale = 0;
            self.big = None;
            return;
        }
        // v0.63: big values normalize the BigUint magnitude instead.
        // (All big constructors normalize at build time; this keeps
        // the invariant if a scale is ever adjusted in place.)
        if let Some(mag) = self.big.as_mut() {
            let ten = BigUint::from_u64(10);
            while self.scale > 0 {
                let (q, r) = mag.div_rem(&ten);
                if !r.is_zero() {
                    break;
                }
                **mag = q;
                self.scale -= 1;
            }
            debug_assert!(!mag.is_zero());
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
            dscale: 0,
            special: NumericSpecial::Finite,
            big: None,
        }
    }

    pub fn from_i64(i: i64) -> Self {
        Numeric::new(i as i128, 0)
    }

    /// v0.63: exact positive unscaled magnitude as a [`BigUint`].
    /// The only sanctioned way to read the magnitude of a value that
    /// may carry the big-mantissa extension.
    pub(crate) fn mag(&self) -> BigUint {
        match &self.big {
            Some(m) => (**m).clone(),
            None => BigUint::from_u128(self.unscaled.unsigned_abs()),
        }
    }

    /// v0.63: true when the big-mantissa extension is in use.
    pub(crate) fn is_big(&self) -> bool {
        self.big.is_some()
    }

    /// v0.63: build from an exact decimal `(sign, magnitude, scale)`,
    /// normalizing trailing zeros while `scale > 0` (the same
    /// normalization [`Numeric::new`] applies to i128 values).
    /// Downgrades to the i128 fast path whenever the magnitude fits.
    /// `None` only past PostgreSQL's 131072-integer-digit numeric
    /// format limit (caller maps to 22003).
    pub(crate) fn from_big(
        neg: bool,
        mut mag: BigUint,
        mut scale: i32,
        dscale: i32,
    ) -> Option<Numeric> {
        if mag.is_zero() {
            return Some(Numeric::zero().with_dscale(dscale));
        }
        // Strip trailing zeros while scale > 0, exactly like
        // `normalize` does for the i128 representation.
        let ten = BigUint::from_u64(10);
        while scale > 0 {
            let (q, r) = mag.div_rem(&ten);
            if !r.is_zero() {
                break;
            }
            mag = q;
            scale -= 1;
        }
        let int_digits = mag.decimal_digits() as i64 - scale as i64;
        if int_digits > 131_072 {
            return None;
        }
        if let Some(u) = mag.to_i128() {
            let unscaled = if neg { -u } else { u };
            return Some(Numeric::new(unscaled, scale).with_dscale(dscale));
        }
        Some(Numeric {
            unscaled: if neg { -1 } else { 1 },
            scale,
            dscale: dscale.max(0),
            special: NumericSpecial::Finite,
            big: Some(Box::new(mag)),
        })
    }

    /// v0.63: build from an exact [`BigDec`], keeping big magnitudes
    /// instead of failing like `to_numeric_narrowed`. `dscale` is the
    /// declared display scale (mirrors [`Numeric::new`]'s
    /// pre-normalization dscale).
    pub(crate) fn from_bigdec(d: &BigDec, dscale: i32) -> Option<Numeric> {
        Self::from_big(d.neg && !d.mag.is_zero(), d.mag.clone(), d.scale, dscale)
    }

    /// v0.63: like [`Numeric::from_bigdec`], but the display scale is
    /// the BigDec's own scale — mirrors [`BigDec::to_numeric`], for
    /// the transcendental producers (`sqrt`/`exp`/`ln`/`log`/`power`)
    /// whose results PG19 displays at the computed rscale
    /// (e.g. `sqrt(4)` -> `2.000000000000000`, 15 fractional digits).
    pub(crate) fn from_bigdec_exact(d: &BigDec) -> Option<Numeric> {
        Self::from_big(
            d.neg && !d.mag.is_zero(),
            d.mag.clone(),
            d.scale,
            d.scale.max(0),
        )
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
            // v0.59: PG19 requires the exponent's first char (after an
            // optional sign) to be a digit; `1.2e_34` is invalid.
            strip_underscores_strict(exp_raw)
                .ok_or(NumericParseError::Syntax)?
                .parse()
                .map_err(|_| NumericParseError::Syntax)?
        };
        // Split the mantissa on '.', then strip underscores from each
        // part (empty parts are fine: `.5`, `5.`). v0.59: PG19 rules —
        // `_123` and `123._456` are invalid syntax.
        let (int_raw, frac_raw) = match mant_raw.find('.') {
            Some(i) => (&mant_raw[..i], &mant_raw[i + 1..]),
            None => (mant_raw, ""),
        };
        let int_part = strip_underscores_opt_strict(int_raw).ok_or(NumericParseError::Syntax)?;
        let frac_part = strip_underscores_opt_strict(frac_raw).ok_or(NumericParseError::Syntax)?;
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
        let mut overflowed = false;
        for c in int_part.chars().chain(frac_part.chars()) {
            let d = (c as i128) - ('0' as i128);
            match unscaled.checked_mul(10).and_then(|v| v.checked_add(d)) {
                Some(v) => unscaled = v,
                None => {
                    overflowed = true;
                    break;
                }
            }
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
        if overflowed {
            // v0.63: magnitude past i128's 38 digits — parse exactly via
            // BigDec and keep the big mantissa (PG19 accepts up to
            // 131072 integer digits). `parse_decimal` re-validates the
            // already-checked literal; TooBig maps to Overflow (22003).
            let d = BigDec::parse_decimal(s).map_err(|_| NumericParseError::Overflow)?;
            return Self::from_bigdec(&d, scale.max(0)).ok_or(NumericParseError::Overflow);
        }
        if neg {
            unscaled = -unscaled;
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
            // v0.63: big magnitudes go through BigDec's f64 conversion
            // (leading-digit based); the i128 path is unchanged.
            NumericSpecial::Finite => match &self.big {
                Some(_) => BigDec::from_numeric(self)
                    .map(|d| d.to_f64())
                    .unwrap_or(f64::NAN),
                None => self.unscaled as f64 * 10f64.powi(-self.scale),
            },
        }
    }

    /// Round half away from zero to an integer; overflow or a special
    /// value -> None.
    /// v0.63: big-mantissa aware — exact via BigDec long division.
    pub fn to_i64(&self) -> Option<i64> {
        if self.special != NumericSpecial::Finite {
            return None;
        }
        if self.big.is_some() {
            // Exact: round half away from zero at scale 0.
            let d = BigDec::from_numeric(self)?;
            let r = d.round_to_scale(0);
            if r.int_digits() > 18 {
                return None;
            }
            let mag = r.mag.to_i128()?;
            let v = if r.neg { mag.checked_neg()? } else { mag };
            return i64::try_from(v).ok();
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
    /// v0.63: big-mantissa operands — or an i128 overflow on the fast
    /// path — fall back to exact BigDec addition instead of 22003.
    pub fn checked_add(&self, other: &Numeric) -> Option<Numeric> {
        match (self.special, other.special) {
            (NumericSpecial::NaN, _) | (_, NumericSpecial::NaN) => Some(Numeric::nan()),
            (NumericSpecial::Finite, NumericSpecial::Finite) => {
                // v0.61: PG's add_var keeps res_dscale = max(d1, d2).
                let dscale = self.dscale.max(other.dscale);
                if self.big.is_some() || other.big.is_some() {
                    let a = BigDec::from_numeric(self)?;
                    let b = BigDec::from_numeric(other)?;
                    return Self::from_bigdec(&a.add(&b), dscale);
                }
                let (a, b, scale) = self.aligned(other)?;
                match a.checked_add(b) {
                    Some(s) => Some(Numeric::new(s, scale).with_dscale(dscale)),
                    None => {
                        // i128 overflow on the fast path: exact add.
                        let x = BigDec::from_numeric(self)?;
                        let y = BigDec::from_numeric(other)?;
                        Self::from_bigdec(&x.add(&y), dscale)
                    }
                }
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
    /// v0.63: big-mantissa operands — or an i128 overflow on the fast
    /// path — fall back to exact BigDec multiplication instead of 22003.
    pub fn checked_mul(&self, other: &Numeric) -> Option<Numeric> {
        match (self.special, other.special) {
            (NumericSpecial::NaN, _) | (_, NumericSpecial::NaN) => Some(Numeric::nan()),
            (NumericSpecial::Finite, NumericSpecial::Finite) => {
                // v0.61: PG's mul_var keeps res_dscale = d1 + d2.
                let dscale = self.dscale.saturating_add(other.dscale);
                if self.big.is_some() || other.big.is_some() {
                    let a = BigDec::from_numeric(self)?;
                    let b = BigDec::from_numeric(other)?;
                    return a.mul_exact(&b).and_then(|p| Self::from_bigdec(&p, dscale));
                }
                let scale = self.scale.checked_add(other.scale)?;
                match self.unscaled.checked_mul(other.unscaled) {
                    Some(p) => Some(Numeric::new(p, scale).with_dscale(dscale)),
                    None => {
                        // i128 overflow on the fast path: exact multiply.
                        let a = BigDec::from_numeric(self)?;
                        let b = BigDec::from_numeric(other)?;
                        a.mul_exact(&b).and_then(|p| Self::from_bigdec(&p, dscale))
                    }
                }
            }
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
    /// v0.59: exact decimal division with PostgreSQL 19 display-scale and
    /// rounding semantics.
    ///
    /// Result scale (`rscale` fractional digits) follows PG19
    /// `select_div_scale()`: `rscale = max(max(16 - qweight * 4, max(dscale
    /// of either input)), 0)`, clamped to `NUMERIC_MAX_RESULT_SCALE`
    /// (1000). The quotient weight `qweight` is the normalized
    /// base-10000 weight of the quotient: `weight1 - weight2`, minus one
    /// when the dividend's leading digit group does not exceed the
    /// divisor's (exactly PG19's rule, in decimal-digit form).
    ///
    /// The quotient is computed exactly with `BigUint` long division at
    /// one guard digit past `rscale`; when `round` is true the guard digit
    /// rounds half-away-from-zero (PG's `div_var`), when false it is
    /// truncated (PG's `div()` SQL function calls the divider directly at
    /// rscale 0 with no rounding).
    fn div_impl(&self, other: &Numeric, rscale: i32, round: bool) -> Option<Numeric> {
        debug_assert!(self.special == NumericSpecial::Finite);
        debug_assert!(other.special == NumericSpecial::Finite);
        debug_assert!(!other.is_zero());
        // Signed unscaled magnitudes as BigUints.
        // v0.63: big-mantissa aware via the exact magnitude accessor.
        let a_mag = self.mag();
        let a_neg = self.unscaled < 0;
        let b_mag = other.mag();
        let b_neg = other.unscaled < 0;
        let neg = a_neg != b_neg;
        // rscale follows select_div_scale for the `/` operator; the SQL
        // div() function passes rscale 0 explicitly.
        let rscale = if round {
            Self::select_div_scale(self, other)
        } else {
            rscale
        };
        // Quotient digits at rscale + 1 (guard) fractional digits, exact.
        // a/b = (a_mag/b_mag) * 10^(b.scale - a.scale), so to land the
        // quotient at scale rscale+1 we shift the numerator by
        // (b.scale - a.scale) + rscale + 1. When that shift is negative
        // we must scale the *denominator* up, never pre-truncate the
        // numerator: pre-truncation would discard the guard digit that
        // rounding needs.
        let shift = (other.scale - self.scale + rscale + 1) as i64;
        let (num, den) = if shift >= 0 {
            let mut n = a_mag;
            n.mul_pow10_assign(shift as u32);
            (n, b_mag)
        } else {
            let mut d = b_mag;
            d.mul_pow10_assign((-shift) as u32);
            (a_mag, d)
        };
        let (q, _rem) = num.div_rem(&den);
        // Drop the guard digit: exact division by 10. The guard digit
        // (q mod 10) decides rounding half away from zero; when
        // truncating it is simply discarded.
        let ten = BigUint::from_u64(10);
        let (mut q, guard) = q.div_rem(&ten);
        if round {
            let g = guard.to_u64().unwrap_or(0);
            if g >= 5 {
                q.add_small_assign(1);
            }
        }
        // v0.63: build through the big-aware constructor — quotients
        // past 38 digits keep their exact magnitude instead of 22003.
        let dscale = rscale;
        let dec = BigDec {
            neg,
            mag: q,
            scale: rscale,
        };
        // v0.61: PG's div_var sets the display scale to rscale
        // (the runner compares numerics, so trailing-zero padding is
        // harmless there but required for e.g. 10.0 ^ -2147483648).
        Self::from_bigdec(&dec, dscale)
    }

    /// v0.59: PG19 `select_div_scale()` faithfully, in decimal-digit
    /// form. PG works in base-10000 digits: for each operand it takes
    /// the normalized weight (base-10000 exponent of the leading digit
    /// group) and the leading group itself (`firstdigit`); then
    /// `qweight = weight1 - weight2 - (firstdigit1 <= firstdigit2)`,
    /// `rscale = max(16 - qweight*4, dscale1, dscale2, 0)`, clamped to
    /// 1000. Here weight = floor((int_digits-1)/4) and the leading group
    /// is the first 1-4 significant decimal digits (zero-padded on the
    /// right when the value is fractional).
    fn select_div_scale(a: &Numeric, b: &Numeric) -> i32 {
        const NUMERIC_MIN_SIG_DIGITS: i32 = 16;
        const NUMERIC_MAX_DISPLAY_SCALE: i32 = 1000;
        /// (normalized base-10000 weight, leading digit group) of a
        /// nonzero finite Numeric.
        /// v0.63: big-mantissa aware via the exact digit string.
        fn weight_firstdigit(n: &Numeric) -> (i32, u32) {
            if n.is_zero() {
                return (0, 0);
            }
            let digits = match &n.big {
                Some(mag) => mag.to_decimal_string(),
                None => n.unscaled.unsigned_abs().to_string(),
            };
            let d = digits.len() as i32;
            // Integer digits of the value (may be <= 0 for fractions).
            let int_d = d - n.scale;
            let weight = (int_d - 1).div_euclid(4);
            let k = ((int_d - 1).rem_euclid(4) + 1) as usize; // 1..=4
            let mut g = digits[..digits.len().min(k)].to_string();
            while g.len() < k {
                g.push('0');
            }
            (weight, g.parse::<u32>().unwrap_or(0))
        }
        let (w1, fd1) = weight_firstdigit(a);
        let (w2, fd2) = weight_firstdigit(b);
        let mut qweight = w1 - w2;
        if fd1 <= fd2 {
            qweight -= 1;
        }
        let rscale = NUMERIC_MIN_SIG_DIGITS - qweight * 4;
        let rscale = rscale.max(a.scale.max(b.scale)).max(0);
        rscale.min(NUMERIC_MAX_DISPLAY_SCALE)
    }

    /// SQL `div(y, x)` integer division toward zero: exact quotient at
    /// rscale 0 with truncation, like PG19 (which calls `div_var` with
    /// rscale 0 and no rounding).
    pub(crate) fn div_trunc(&self, other: &Numeric) -> Option<Numeric> {
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
                if self.is_zero() {
                    return Some(Numeric::zero());
                }
                self.div_impl(other, 0, false)
            }
        }
    }

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
                if self.is_zero() {
                    return Some(Numeric::zero());
                }
                // v0.59: exact PG19 division (select_div_scale + exact
                // BigUint long division, rounded half away from zero).
                self.div_impl(other, 0, true)
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
                // v0.63: exact division at out_scale for every magnitude.
                // (The old f64 fast path silently saturated `as i128`
                // when the quotient exceeded i128, and read only the
                // sign of big-mantissa operands.)
                let a = BigDec::from_numeric(self)?;
                let b = BigDec::from_numeric(other)?;
                let q = a.div_round(&b, out_scale)?;
                Self::from_bigdec(&q, out_scale)
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
                // v0.61: PG's mod_var keeps res_dscale = max(d1, d2).
                let dscale = self.dscale.max(other.dscale);
                // v0.63: big-mantissa aware — aligned() reads the
                // sign-only unscaled, so route big operands through the
                // exact BigDec remainder (sign follows the dividend).
                if self.big.is_some() || other.big.is_some() {
                    let a = BigDec::from_numeric(self)?;
                    let b = BigDec::from_numeric(other)?;
                    return Self::from_bigdec(&a.rem(&b), dscale);
                }
                let (a, b, scale) = self.aligned(other)?;
                Some(Numeric::new(a.checked_rem(b)?, scale).with_dscale(dscale))
            }
            _ => Some(Numeric::nan()),
        }
    }

    /// Absolute value (v0.18): NaN stays NaN, -Infinity becomes +Infinity.
    /// v0.63: big-mantissa aware — flipping the sign bit on `unscaled`
    /// is exact under the sign convention.
    pub fn abs(&self) -> Numeric {
        match self.special {
            NumericSpecial::NaN => Numeric::nan(),
            NumericSpecial::NegInf => Numeric::infinity(),
            _ => {
                if self.special == NumericSpecial::Finite {
                    // v0.61: abs preserves the display scale.
                    let mut n =
                        Numeric::new(self.unscaled.abs(), self.scale).with_dscale(self.dscale);
                    // v0.63: keep a big magnitude big (its unscaled is
                    // already just the sign).
                    if self.big.is_some() {
                        n.unscaled = 1;
                        n.big = self.big.clone();
                    }
                    n
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
    /// v0.63: big-mantissa aware via the exact digit count.
    fn int_digits(&self) -> i64 {
        if self.unscaled == 0 {
            return 0;
        }
        let digits: i64 = match &self.big {
            Some(mag) => mag.decimal_digits() as i64,
            None => {
                let mut v = self.unscaled.unsigned_abs();
                let mut digits: i64 = 0;
                while v > 0 {
                    v /= 10;
                    digits += 1;
                }
                digits
            }
        };
        digits - self.scale as i64
    }

    pub fn round_to(&self, scale: i32) -> Option<Numeric> {
        if self.special != NumericSpecial::Finite {
            return Some(self.clone());
        }
        // v0.63: big-mantissa values round exactly via BigDec.
        if self.big.is_some() {
            let d = BigDec::from_numeric(self)?;
            let r = d.round_to_scale(scale);
            // v0.61: the rounded value carries the target display scale.
            let n = Self::from_bigdec(&r, scale.max(0))?.with_dscale(scale.max(0));
            if n.int_digits() > 131072 {
                return None;
            }
            return Some(n);
        }
        if scale >= self.scale {
            // v0.59: widening the scale (or a no-op) is value-identical;
            // PostgreSQL pads trailing zeros in the display scale, but
            // the value is unchanged. Never 22003 on zero-padding
            // (e.g. round(3.14, 40)): the old zero-pad multiply could
            // overflow i128 and wrongly error.
            // v0.61: PG's round_var sets the display scale to the target
            // (round(3.14, 40) prints 40 fractional digits).
            let mut n = self.clone();
            n.dscale = scale.max(0);
            return Some(n);
        }
        let drop = (self.scale - scale) as u32;
        // v0.22: if 10^drop overflows i128, then |unscaled| < 10^39/2 <=
        // div/2, so the value rounds to zero at any target scale.
        let div = match 10i128.checked_pow(drop) {
            Some(d) => d,
            None => return Some(Numeric::zero().with_dscale(scale)),
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
        // v0.61: the rounded value carries the target display scale.
        Some(r.with_dscale(scale))
    }

    /// v0.60: PG19 `apply_typmod` for `numeric(p, s)` assignment
    /// (src/backend/utils/adt/numeric.c): NaN passes through
    /// unchanged; infinities error; otherwise the value is rounded
    /// to `s` fractional digits (ties away from zero, like
    /// [`Numeric::round_to`]) and errors when the integer digits
    /// exceed `p - s`. Negative scales are allowed since PG15: the
    /// value rounds left of the decimal point.
    ///
    /// The stored result keeps a non-negative display scale: PG's
    /// own regression suite shows `scale()` returning 0 for values
    /// in a `numeric(3,-6)` column, and values print as plain
    /// integers.
    pub fn apply_typmod(&self, precision: u32, scale: i32) -> Result<Numeric, TypmodError> {
        if self.is_nan() {
            return Ok(self.clone());
        }
        if self.special != NumericSpecial::Finite {
            return Err(TypmodError::Infinite);
        }
        let rounded = self.round_to(scale).ok_or(TypmodError::Overflow)?;
        // PG's rule (numeric.out: "must round to an absolute value less
        // than 10^(precision-scale)"): overflow iff
        // |rounded| >= 10^(precision-scale). In unscaled terms with the
        // rounded value at scale rs, that is
        // |unscaled| >= 10^(precision - scale + rs).
        // v0.63: big-mantissa aware — compare exact digit counts.
        let limit_pow = precision as i64 - scale as i64 + rounded.scale as i64;
        let overflows = if rounded.unscaled == 0 {
            false
        } else {
            let mag_digits = match &rounded.big {
                Some(m) => m.decimal_digits() as i64,
                None => {
                    let mut v = rounded.unscaled.unsigned_abs();
                    let mut d: i64 = 0;
                    while v > 0 {
                        v /= 10;
                        d += 1;
                    }
                    d
                }
            };
            // |unscaled| >= 10^limit_pow  <=>  digits >= limit_pow + 1
            // (for limit_pow >= 0); limit_pow <= 0 overflows any nonzero.
            if limit_pow <= 0 {
                true
            } else {
                mag_digits > limit_pow
            }
        };
        if overflows {
            return Err(TypmodError::Overflow);
        }
        // Clamp a negative display scale up to 0: the value is an
        // exact integer multiple of 10^-scale by construction, so
        // shifting the point is value-identical. Checked multiply;
        // on overflow (absurd magnitudes only) the negative scale
        // is kept and the value stays numerically correct.
        // v0.63: big values shift the BigUint magnitude instead.
        let mut r = rounded;
        if r.big.is_some() {
            while r.scale < 0 {
                // Invariant: big is Some(mag) with unscaled = ±1 here.
                if let Some(mag) = r.big.as_mut() {
                    mag.mul_pow10_assign(1);
                }
                r.scale += 1;
            }
            return Ok(r);
        }
        while r.scale < 0 {
            match r.unscaled.checked_mul(10) {
                Some(m) => {
                    r.unscaled = m;
                    r.scale += 1;
                }
                None => break,
            }
        }
        Ok(r)
    }

    /// v0.18: specials are fixed points of floor/ceil.
    /// v0.22: a non-positive scale is already integral.
    /// v0.63: big-mantissa aware via exact BigUint division.
    pub fn floor(&self) -> Option<Numeric> {
        if self.special != NumericSpecial::Finite {
            return Some(self.clone());
        }
        if self.scale <= 0 {
            return Some(self.clone());
        }
        if self.big.is_some() {
            // |value| < 1 iff mag < 10^scale.
            let mut pow10 = BigUint::from_u64(1);
            pow10.mul_pow10_assign(self.scale as u32);
            let mag = self.mag();
            if mag.cmp(&pow10) == std::cmp::Ordering::Less {
                // -1 < value < 0 -> -1; 0 <= value < 1 -> 0.
                return Some(if self.unscaled < 0 {
                    Numeric::from_i64(-1)
                } else {
                    Numeric::zero()
                });
            }
            let (mut q, r) = mag.div_rem(&pow10);
            if !r.is_zero() && self.unscaled < 0 {
                q.add_small_assign(1);
            }
            let neg = self.unscaled < 0;
            return Self::from_big(neg, q, 0, 0);
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
    /// v0.63: big-mantissa aware via exact BigUint division.
    pub fn ceil(&self) -> Option<Numeric> {
        if self.special != NumericSpecial::Finite {
            return Some(self.clone());
        }
        if self.scale <= 0 {
            return Some(self.clone());
        }
        if self.big.is_some() {
            let mut pow10 = BigUint::from_u64(1);
            pow10.mul_pow10_assign(self.scale as u32);
            let mag = self.mag();
            if mag.cmp(&pow10) == std::cmp::Ordering::Less {
                // 0 < value < 1 -> 1; -1 < value <= 0 -> 0.
                return Some(if self.unscaled > 0 {
                    Numeric::from_i64(1)
                } else {
                    Numeric::zero()
                });
            }
            let (mut q, r) = mag.div_rem(&pow10);
            if !r.is_zero() && self.unscaled > 0 {
                q.add_small_assign(1);
            }
            let neg = self.unscaled < 0;
            return Self::from_big(neg, q, 0, 0);
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

    /// Exact power for integer exponents (repeated squaring). `None`
    /// on overflow or absurd exponents (callers fall back to f64 or
    /// raise 22003). Negative exponents divide, with 10 guard digits.
    /// v0.18: NaN base -> NaN; infinite base follows PG's sign rules;
    /// `0^0` is 1 (PostgreSQL).
    /// v0.61: PG19 `power_var_int` — exact integer-exponent power with
    /// PG's adaptive working precision (`src/backend/utils/adt/numeric.c`,
    /// REL_19_STABLE). `exp` is the integer exponent value and
    /// `exp_dscale` the exponent literal's declared display scale (PG
    /// feeds `exp->dscale` into the result-scale computation).
    ///
    /// The caller handles specials, the `0 ^ negative` 2201F rule, and
    /// non-integral exponents; `self` is finite here.
    ///
    /// Result display scale follows PG19 exactly:
    /// `rscale = min(max(max(16 - int(f), base.dscale, exp.dscale), 0),
    /// 1000)` where `f = exp * log10(|base|)`.
    ///
    /// Overflow (`Err(PowerError::Overflow)`, SQLSTATE 22003) when `f`
    /// exceeds what our i128 mantissa can represent (PG's own bound is
    /// `f > 524288` for its wider storage; past ~10^45 no i128-based
    /// value can hold the result, so both agree on 22003).
    pub fn power_int(&self, exp: i64, exp_dscale: i32) -> Result<Numeric, PowerError> {
        debug_assert!(self.special == NumericSpecial::Finite);
        // PG: f = exp * (log10(leading digits) + weight*4); the
        // decimal-digit form is exp * log10(|unscaled| * 10^-scale).
        // v0.63: big-mantissa aware — log10 from the exact digit count
        // and leading digits instead of the i128 unscaled value.
        let f = if self.unscaled == 0 {
            0.0
        } else {
            let log10_abs = match &self.big {
                Some(mag) => {
                    let s = mag.to_decimal_string();
                    // v0.63: lead holds the first k digits, so its own
                    // log10 already counts (k - 1) of the decades; add
                    // only the remaining (len - k).
                    let k = s.len().min(19);
                    let lead: f64 = s[..k].parse().unwrap_or(0.0);
                    (s.len() as f64 - k as f64) + lead.log10() - self.scale as f64
                }
                None => (self.unscaled.unsigned_abs() as f64).log10() - self.scale as f64,
            };
            exp as f64 * log10_abs
        };
        // v0.63: the old `f > 45.0` early bail assumed an i128 mantissa;
        // with the big-mantissa extension the real bound is PG's own
        // (NUMERIC_WEIGHT_MAX+1)*4 = 524288, enforced per-iteration
        // below and by the 131072-digit format limit on the result.
        if f > 524_288.0 {
            return Err(PowerError::Overflow);
        }
        if f + 1.0 < -1000.0 {
            // PG: the true result is below 10^-1000, so it is zero at
            // the maximum display scale.
            return Ok(Numeric::zero().with_dscale(1000));
        }
        // PG: rscale = Max(16 - (int) f, Max(base->dscale, exp->dscale));
        // then clamped to [0, 1000]. Rust's `as` truncates toward zero
        // exactly like C's `(int)` cast for this range.
        let mut rscale = 16 - f as i32;
        rscale = rscale.max(self.dscale).max(exp_dscale).max(0).min(1000);
        if exp == 0 {
            // PG: result is exactly 1 with dscale rscale (no rounding).
            return Ok(Numeric::new(1, 0).with_dscale(rscale));
        }
        if exp == 1 {
            // PG: round_var(base, rscale).
            return self.round_to(rscale).ok_or(PowerError::Overflow);
        }
        if self.unscaled == 0 {
            // exp < 0 is rejected by the caller with 2201F
            // ("zero raised to a negative power is undefined").
            debug_assert!(exp > 0);
            return Ok(Numeric::zero().with_dscale(rscale));
        }
        let neg = exp < 0;
        let mut mask: u64 = exp.unsigned_abs();
        // PG: sig_digits = 1 + rscale + (int) f
        //     + (int) log10(abs(exp)) + 8 — the working precision that
        // keeps every intermediate at enough significant digits.
        let sig_digits = 1i64 + rscale as i64 + f as i64 + (mask as f64).log10() as i64 + 8;
        let mut base_prod = BigDec::from_numeric(self).expect("power_int: finite base");
        let mut result = if mask & 1 == 1 {
            base_prod.clone()
        } else {
            BigDec::one()
        };
        mask >>= 1;
        while mask > 0 {
            // PG: local_rscale = Min(2*base_prod->dscale,
            //     sig_digits - 2*(int_digits(base_prod))), at least 0.
            let local = (sig_digits - 2 * base_prod.int_digits())
                .min(2 * base_prod.scale() as i64)
                .max(0)
                .min(i32::MAX as i64) as i32;
            base_prod = base_prod.mul_round(&base_prod, local);
            if mask & 1 == 1 {
                let local2 = (sig_digits - (base_prod.int_digits() + result.int_digits()))
                    .min((base_prod.scale() + result.scale()) as i64)
                    .max(0)
                    .min(i32::MAX as i64) as i32;
                result = base_prod.mul_round(&result, local2);
            }
            // PG: weight past NUMERIC_WEIGHT_MAX (131071 base-10000
            // digits = 524284 decimal digits) overflows, unless the
            // exponent is negative, in which case the result is zero.
            if base_prod.int_digits() > 524_284 || result.int_digits() > 524_284 {
                if !neg {
                    return Err(PowerError::Overflow);
                }
                return Ok(Numeric::zero().with_dscale(rscale));
            }
            mask >>= 1;
        }
        let dec = if neg {
            // PG: div_var(&const_one, result, result, rscale, true).
            result.recip_round(rscale).ok_or(PowerError::Overflow)?
        } else {
            // PG: round_var(result, rscale).
            result.round_to_scale(rscale)
        };
        // v0.63: big-mantissa results keep their exact magnitude
        // (PG19 allows 131072 integer digits); only past that is 22003.
        let mut n = Self::from_bigdec(&dec, rscale).ok_or(PowerError::Overflow)?;
        n.dscale = rscale;
        Ok(n)
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
        // v0.63: big-mantissa aware via the exact digit count.
        let mag = |n: &Numeric| -> i64 {
            let digits: i64 = match &n.big {
                Some(m) => m.decimal_digits() as i64,
                None => {
                    let mut v = n.unscaled.unsigned_abs();
                    let mut digits: i64 = 0;
                    while v >= 10 {
                        v /= 10;
                        digits += 1;
                    }
                    digits
                }
            };
            digits - n.scale as i64
        };
        let (ma, mb) = (mag(self), mag(other));
        if ma != mb {
            return if neg_a { mb.cmp(&ma) } else { ma.cmp(&mb) };
        }
        // Same magnitude: align scales; on overflow fall back to f64.
        // v0.63: big values compare exactly through BigDec.
        let ord = if self.big.is_some() || other.big.is_some() {
            let (a, b) = (
                BigDec::from_numeric(self).expect("cmp: finite"),
                BigDec::from_numeric(other).expect("cmp: finite"),
            );
            a.cmp(&b)
        } else {
            match self.aligned(other) {
                Some((a, b, _)) => a.cmp(&b),
                None => self
                    .to_f64()
                    .partial_cmp(&other.to_f64())
                    .unwrap_or(Ordering::Equal),
            }
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
            // v0.61: PG pads zero to the display scale
            // (e.g. 10.0 ^ -2147483648 -> 0.000...0 with 1000 digits).
            if self.dscale > 0 {
                let mut out = String::from("0.");
                for _ in 0..self.dscale {
                    out.push('0');
                }
                return out;
            }
            return "0".to_string();
        }
        let neg = self.unscaled < 0;
        // v0.63: big-mantissa values render their exact magnitude.
        let digits = match &self.big {
            Some(mag) => mag.to_decimal_string(),
            None => self.unscaled.unsigned_abs().to_string(),
        };
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
        // v0.61: pad trailing zeros to the display scale (PG's dscale),
        // e.g. numeric '1.50' prints "1.50", not "1.5".
        if self.dscale > self.scale {
            if self.scale <= 0 {
                out.push('.');
            }
            for _ in self.scale.max(0)..self.dscale {
                out.push('0');
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericParseError {
    Syntax,
    Overflow,
}

/// v0.59: validate and remove `_` digit separators with PostgreSQL 19
/// rules. The first character after an optional sign must be a digit, so
/// `_123`, `123._456`, and `1.2e_34` are invalid syntax, while `1_2`,
/// `1.2_5`, and `1e1_2` are fine. (Based literals like `0x_1F` are
/// handled separately in `parse_based`, which keeps PG's rule allowing
/// one underscore right after the base prefix.)
fn strip_underscores_strict(s: &str) -> Option<String> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => match s.strip_prefix('+') {
            Some(d) => (false, d),
            None => (false, s),
        },
    };
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

/// v0.59: `strip_underscores_strict`, but an empty part (as in `.5` or
/// `5.`) is allowed instead of rejected.
fn strip_underscores_opt_strict(s: &str) -> Option<String> {
    if s.is_empty() {
        return Some(String::new());
    }
    strip_underscores_strict(s)
}

impl Numeric {
    /// v0.18: Truncate toward zero to `new_scale` (no rounding).
    /// v0.22: `new_scale` is signed. If the divisor would overflow i128
    /// the truncated value is necessarily zero (|unscaled| < divisor).
    /// v0.56: use i64 arithmetic for the scale difference — with a very
    /// negative `new_scale` (e.g. `trunc(x, -2147483648)`) the i32
    /// subtraction `self.scale - new_scale` could overflow and panic in
    /// debug builds. |unscaled| < 10^39, so a divisor of 10^39 or more
    /// always truncates to zero.
    /// v0.63: big-mantissa aware via exact BigUint division.
    pub fn trunc_to_scale(&self, new_scale: i32) -> Numeric {
        if self.special != NumericSpecial::Finite {
            return self.clone();
        }
        if new_scale >= self.scale {
            return self.clone();
        }
        // v0.63: exact truncation of a big magnitude.
        if self.big.is_some() {
            let diff = self.scale as i64 - new_scale as i64;
            // v0.61: PG's trunc_var keeps dscale = min(dscale, rscale).
            let dscale = self.dscale.min(new_scale);
            if diff >= 1_000_000 {
                return Numeric::zero().with_dscale(dscale);
            }
            let mut pow10 = BigUint::from_u64(1);
            pow10.mul_pow10_assign(diff as u32);
            let (q, _) = self.mag().div_rem(&pow10);
            let neg = self.unscaled < 0;
            return Self::from_big(neg, q, new_scale, dscale)
                .unwrap_or_else(|| Numeric::zero().with_dscale(dscale));
        }
        let diff = self.scale as i64 - new_scale as i64;
        if diff >= 39 {
            // v0.61: PG's trunc_var keeps dscale = min(dscale, rscale).
            return Numeric::new(0, new_scale).with_dscale(self.dscale.min(new_scale));
        }
        let div = 10i128.pow(diff as u32);
        Numeric::new(self.unscaled / div, new_scale).with_dscale(self.dscale.min(new_scale))
    }

    /// v0.37: byte size of this numeric for TOAST accounting. Uses the
    /// binary encoding size (unscaled i128 + scale i32 + discriminant).
    /// v0.63: big-mantissa aware — a big value's magnitude rides along
    /// as decimal bytes (see [`Numeric::toast_bytes`]).
    pub fn toast_len(&self) -> usize {
        16 + 4
            + 1
            + self
                .big
                .as_ref()
                .map(|m| m.decimal_digits() as usize)
                .unwrap_or(0)
    }

    /// v0.37: raw bytes of this numeric for TOAST compression/chunking:
    /// big-endian unscaled + scale + special discriminant.
    /// v0.63: big-mantissa aware — for big values the exact magnitude
    /// follows as decimal bytes (`unscaled` alone holds only the sign).
    pub fn toast_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.toast_len());
        b.extend_from_slice(&self.unscaled.to_be_bytes());
        b.extend_from_slice(&self.scale.to_be_bytes());
        b.push(match self.special {
            NumericSpecial::Finite => 0,
            NumericSpecial::NaN => 1,
            NumericSpecial::PosInf => 2,
            NumericSpecial::NegInf => 3,
        });
        if let Some(mag) = &self.big {
            b.extend_from_slice(mag.to_decimal_string().as_bytes());
        }
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

    /// v0.63: exact decimal rendering of the magnitude (no leading
    /// zeros; `"0"` for zero). Repeated short division by 10^9,
    /// collecting 9-digit groups — quadratic in the digit count, but
    /// only used for display/serialization of big numerics.
    pub(crate) fn to_decimal_string(&self) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        let mut tmp = self.clone();
        let mut groups: Vec<u32> = Vec::new();
        while !tmp.is_zero() {
            // Short division by 1e9, capturing the remainder.
            let mut rem: u64 = 0;
            for i in (0..tmp.limbs.len()).rev() {
                let cur = rem * 1_000_000_000 + tmp.limbs[i] as u64;
                tmp.limbs[i] = (cur / 1_000_000_000) as u32;
                rem = cur % 1_000_000_000;
            }
            tmp.normalize();
            groups.push(rem as u32);
        }
        let mut out = String::new();
        let mut it = groups.iter().rev();
        out.push_str(&it.next().unwrap_or(&0).to_string());
        for g in it {
            out.push_str(&format!("{:09}", g));
        }
        out
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

    /// Value as i128; None when it does not fit.
    pub(crate) fn to_i128(&self) -> Option<i128> {
        let mut v: i128 = 0;
        for &limb in self.limbs.iter().rev() {
            v = v.checked_mul(1_000_000_000)?.checked_add(limb as i128)?;
        }
        Some(v)
    }

    /// v0.61: decimal digit count of the value (0 for zero).
    pub(crate) fn decimal_digits(&self) -> u32 {
        let mut top_idx: Option<usize> = None;
        for (i, &l) in self.limbs.iter().enumerate() {
            if l != 0 {
                top_idx = Some(i);
            }
        }
        let i = match top_idx {
            Some(i) => i,
            None => return 0,
        };
        let mut d = 0u32;
        let mut t = self.limbs[i];
        while t > 0 {
            d += 1;
            t /= 10;
        }
        d + 9 * i as u32
    }

    /// self += v for a small v < 1_000_000_000.
    fn add_small_assign(&mut self, v: u32) {
        debug_assert!(v < 1_000_000_000);
        if v == 0 {
            return;
        }
        let mut carry = v as u64;
        let mut i = 0;
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

    /// Quotient and remainder; requires other > 0. Binary long division:
    /// the dividend's bits are extracted MSB-first by repeated halving
    /// (base-1e9 limbs are not bit-addressable, so no per-bit indexing),
    /// then the classic shift-subtract loop runs on whole-BigUint
    /// doubling. O(bits^2) limb ops; dividends here are at most a few
    /// thousand bits.
    pub(crate) fn div_rem(&self, other: &BigUint) -> (BigUint, BigUint) {
        debug_assert!(!other.is_zero());
        if self.cmp(other) == std::cmp::Ordering::Less {
            return (BigUint::zero(), self.clone());
        }
        // Dividend bits, LSB-first: bit = limbs[0] & 1, then halve.
        let mut tmp = self.clone();
        let mut bits_lsb = Vec::new();
        while !tmp.is_zero() {
            bits_lsb.push(tmp.limbs[0] & 1 == 1);
            tmp.div_small_assign(2);
        }
        let mut q = BigUint::zero();
        let mut r = BigUint::zero();
        for &b in bits_lsb.iter().rev() {
            r.mul_small_assign(2);
            if b {
                r.add_small_assign(1);
            }
            q.mul_small_assign(2);
            if r.cmp(other) != std::cmp::Ordering::Less {
                r.sub_assign(other);
                q.add_small_assign(1);
            }
        }
        (q, r)
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
    /// v0.63: big-mantissa aware — with the sign convention
    /// (`unscaled` holds the sign when `big` is `Some`), `neg` and
    /// [`Numeric::mag`] read the value exactly.
    pub(crate) fn from_numeric(n: &Numeric) -> Option<Self> {
        if n.special != NumericSpecial::Finite {
            return None;
        }
        Some(
            BigDec {
                neg: n.unscaled < 0,
                mag: n.mag(),
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
                // v0.59: PG19 underscore rules, like Numeric::parse.
                exp_clean = strip_underscores_strict(e).ok_or(DecimalParseError::Syntax)?;
                Some(exp_clean.as_str())
            }
            None => None,
        };
        let (int_raw, frac_raw) = match mant_raw.find('.') {
            Some(i) => (&mant_raw[..i], &mant_raw[i + 1..]),
            None => (mant_raw, ""),
        };
        let int_clean = strip_underscores_opt_strict(int_raw).ok_or(DecimalParseError::Syntax)?;
        let frac_clean = strip_underscores_opt_strict(frac_raw).ok_or(DecimalParseError::Syntax)?;
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

// ---------------------------------------------------------------------------
// v0.61: BigDec helpers for PG19 `power_var_int` (exact integer-exponent
// power with adaptive working precision) and the numeric gcd/lcm
// Euclidean algorithm.
// ---------------------------------------------------------------------------

impl BigDec {
    /// The value one.
    pub(crate) fn one() -> Self {
        BigDec::from_i64(1)
    }

    /// Decimal-digit count of the integer part (`digits(mag) - scale`),
    /// mirroring PG's `weight * 4 + 4` bound in `power_var_int`.
    pub(crate) fn int_digits(&self) -> i64 {
        self.mag.decimal_digits() as i64 - self.scale as i64
    }

    /// Round to `rscale` fractional digits, half away from zero
    /// (PG's `round_var`). Widening (`rscale >= scale`) zero-pads and
    /// never fails.
    pub(crate) fn round_to_scale(mut self, rscale: i32) -> BigDec {
        if rscale >= self.scale {
            let pad = (rscale - self.scale) as u32;
            self.mag.mul_pow10_assign(pad);
            self.scale = rscale;
            return self.normalize();
        }
        let drop = self.scale as i64 - rscale as i64;
        // Defensive: a drop this large always rounds to zero at any
        // non-negative rscale; avoids building a gigantic 10^drop.
        if drop > 1_000_000 {
            return BigDec::zero();
        }
        let mut pow10 = BigUint::from_u64(1);
        pow10.mul_pow10_assign(drop as u32);
        let (mut q, r) = self.mag.div_rem(&pow10);
        // Half away from zero: 2*r >= pow10 rounds up.
        let mut twice = r.clone();
        twice.add_assign(&r);
        if twice.cmp(&pow10) != std::cmp::Ordering::Less {
            q.add_small_assign(1);
        }
        BigDec {
            neg: self.neg,
            mag: q,
            scale: rscale,
        }
        .normalize()
    }

    /// Exact product, then [`BigDec::round_to_scale`] to `rscale`
    /// (PG's `mul_var(..., rscale)`).
    pub(crate) fn mul_round(&self, other: &BigDec, rscale: i32) -> BigDec {
        let mag = self.mag.mul(&other.mag);
        let scale =
            (self.scale as i64 + other.scale as i64).clamp(i32::MIN as i64, i32::MAX as i64);
        BigDec {
            neg: self.neg != other.neg,
            mag,
            scale: scale as i32,
        }
        .normalize()
        .round_to_scale(rscale)
    }

    /// Exact product (no rounding).
    pub(crate) fn mul_exact(&self, other: &BigDec) -> Option<BigDec> {
        let mag = self.mag.mul(&other.mag);
        let scale =
            (self.scale as i64 + other.scale as i64).clamp(i32::MIN as i64, i32::MAX as i64);
        Some(
            BigDec {
                neg: self.neg != other.neg,
                mag,
                scale: scale as i32,
            }
            .normalize(),
        )
    }

    /// `1/self` rounded to `rscale` fractional digits, half away from
    /// zero (PG's `div_var(&const_one, x, rscale)`). None for zero.
    pub(crate) fn recip_round(&self, rscale: i32) -> Option<BigDec> {
        if self.mag.is_zero() {
            return None;
        }
        // Normalize a negative scale away: 1/(mag*10^-scale).
        let mut mag = self.mag.clone();
        let mut scale = self.scale;
        if scale < 0 {
            mag.mul_pow10_assign((-scale) as u32);
            scale = 0;
        }
        // 1/x = 10^scale / mag; want round(10^(scale+rscale) / mag).
        // Compute one extra digit, then round half away from zero.
        let t = scale as i64 + rscale as i64 + 1;
        debug_assert!(t >= 1);
        if t > 1_000_000 {
            return None;
        }
        let mut num = BigUint::from_u64(1);
        num.mul_pow10_assign(t as u32);
        let (q, _r) = num.div_rem(&mag);
        let ten = BigUint::from_u64(10);
        let (mut qi, digit) = q.div_rem(&ten);
        if digit.to_u64().unwrap_or(0) >= 5 {
            qi.add_small_assign(1);
        }
        Some(
            BigDec {
                neg: self.neg,
                mag: qi,
                scale: rscale,
            }
            .normalize(),
        )
    }

    /// Exact remainder (`self mod other`), result scale
    /// `max(self.scale, other.scale)` like PG's `mod_var`. The caller
    /// guarantees `other` is non-zero.
    pub(crate) fn rem(&self, other: &BigDec) -> BigDec {
        debug_assert!(!other.mag.is_zero());
        let s = self.scale.max(other.scale);
        let mut a = self.mag.clone();
        a.mul_pow10_assign((s - self.scale) as u32);
        let mut b = other.mag.clone();
        b.mul_pow10_assign((s - other.scale) as u32);
        let (_, r) = a.div_rem(&b);
        BigDec {
            neg: self.neg,
            mag: r,
            scale: s,
        }
        .normalize()
    }

    /// Exact division; the caller guarantees `other` divides `self`
    /// (PG's `div_var(x, gcd, 0, exact=true)` in `numeric_lcm`).
    /// None on a zero divisor or an inexact result.
    pub(crate) fn div_exact(&self, other: &BigDec) -> Option<BigDec> {
        if other.mag.is_zero() {
            return None;
        }
        // self/other = (ma/mb) * 10^(sb-sa).
        let (num, den) = if other.scale >= self.scale {
            let mut num = self.mag.clone();
            num.mul_pow10_assign((other.scale - self.scale) as u32);
            (num, other.mag.clone())
        } else {
            let mut den = other.mag.clone();
            den.mul_pow10_assign((self.scale - other.scale) as u32);
            (self.mag.clone(), den)
        };
        let (q, r) = num.div_rem(&den);
        if !r.is_zero() {
            return None;
        }
        Some(
            BigDec {
                neg: self.neg != other.neg,
                mag: q,
                scale: 0,
            }
            .normalize(),
        )
    }

    /// Convert to [`Numeric`]; None when the magnitude exceeds i128
    /// (PG raises 22003 "value overflows numeric format").
    pub(crate) fn to_numeric(&self) -> Option<Numeric> {
        let mag = self.mag.to_i128()?;
        let unscaled = if self.neg { mag.checked_neg()? } else { mag };
        Some(Numeric::new(unscaled, self.scale))
    }
}

// ---------------------------------------------------------------------------
// v0.62: PG19 transcendental numerics — `sqrt_var`, `exp_var`, `ln_var`,
// `log_var` and the fractional path of `power_var`.
//
// Implemented on the arbitrary-precision `BigDec` following PG19's own
// algorithms and result-scale selection:
//   sqrt: Newton iteration with ~20 guard digits, then PG half-away
//         rounding (agrees with PG's exact digit-by-digit `sqrt_var`).
//   exp:  argument halving to ~±0.01, Taylor series, repeated squaring.
//   ln:   repeated square-root reduction into (0.9, 1.1), then the
//         z + z^3/3 + z^5/5 + ... series with z = (x-1)/(x+1).
//   log:  two separately-scaled natural logarithms, divided.
//   power (fractional): exp(exp * ln(|base|)) with adaptive precision.
//
// The final value converts back to `Numeric` via the v0.63
// big-mantissa extension; only past PG's 131072-digit format limit
// is 22003 "value overflows numeric format".
// ---------------------------------------------------------------------------

/// v0.62: outcome of PG19 `exp_var` (and the exp step of `power_var`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExpOutcome {
    /// Finite result, rounded to the requested result scale.
    Finite(BigDec),
    /// Underflow: PG returns zero with dscale = rscale.
    Underflow,
    /// Overflow: PG raises 22003 "value overflows numeric format".
    Overflow,
}

/// v0.62: failure modes of PG19 `log_var`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogError {
    /// 22003 "value overflows numeric format".
    Overflow,
    /// 22012 "division by zero" (logarithm base 1).
    DivisionByZero,
}

/// v0.62: failure modes of the fractional path of PG19 `power_var`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PowerFracError {
    /// 22003 "value overflows numeric format".
    Overflow,
    /// 2201F "a negative number raised to a non-integer power yields a
    /// complex result".
    NegativeBase,
}

/// v0.62: PG19 `weight * DEC_DIGITS` (DEC_DIGITS = 4): the decimal
/// weight of the leading digit, rounded down to a multiple of 4.
/// Zero maps to 0, matching PG's normalized zero.
pub(crate) fn dec_w4(v: &BigDec) -> i64 {
    if v.mag.is_zero() {
        return 0;
    }
    let dec_w = v.mag.decimal_digits() as i64 - 1 - v.scale as i64;
    dec_w.div_euclid(4) * 4
}

/// v0.62: decimal digit count of a nonzero u32.
fn decimal_digits_u32(mut v: u32) -> u32 {
    debug_assert!(v > 0);
    let mut d = 0;
    while v > 0 {
        v /= 10;
        d += 1;
    }
    d
}

/// v0.62: leading `k` (1 <= k <= 8) decimal digits of a nonzero limb
/// slice, as a u64.
fn leading_decimal_digits(limbs: &[u32], k: u32) -> u64 {
    debug_assert!(k >= 1 && k <= 8 && !limbs.is_empty());
    let n = limbs.len();
    let d0 = decimal_digits_u32(limbs[n - 1]);
    if d0 >= k {
        (limbs[n - 1] / 10u32.pow(d0 - k)) as u64
    } else {
        // d0 < k <= 8 < d0 + 9, and d0 < k implies n >= 2.
        let need = k - d0;
        limbs[n - 1] as u64 * 10u64.pow(need) + (limbs[n - 2] / 10u32.pow(9 - need)) as u64
    }
}

/// v0.62: PG19 `estimate_ln_dweight` — the estimated decimal weight of
/// `ln(v)` for positive `v` (0 for non-positive input, which callers
/// reject separately). Uses the first two base-10000 digits as an f64,
/// exactly as PG does.
pub(crate) fn estimate_ln_dweight(v: &BigDec) -> i64 {
    if v.neg || v.mag.is_zero() {
        return 0;
    }
    // Near 1 (0.9 <= v <= 1.1), PG uses the decimal weight of (v - 1).
    let nine_tenths = BigDec::from_i64(9).div_small_round(10, 1);
    let eleven_tenths = BigDec::from_i64(11).div_small_round(10, 1);
    if let (Some(lo), Some(hi)) = (nine_tenths, eleven_tenths) {
        if v.cmp(&lo) != std::cmp::Ordering::Less && v.cmp(&hi) != std::cmp::Ordering::Greater {
            let x = v.sub(&BigDec::one());
            if x.mag.is_zero() {
                return 0;
            }
            return x.mag.decimal_digits() as i64 - 1 - x.scale as i64;
        }
    }
    // General case: v ~= D * 10^dweight with D holding PG's first two
    // base-10000 digits (d10 + 4 decimal digits); ln(v) ~= ln(D) +
    // dweight * ln(10). The C `(int)` truncates toward zero, as does
    // Rust's `as`.
    let t = v.mag.decimal_digits() as i64;
    let d10 = (t - 1) % 4 + 1;
    let k = t.min(d10 + 4) as u32;
    let d = leading_decimal_digits(&v.mag.limbs, k);
    let dweight = t - k as i64 - v.scale as i64;
    let ln_approx = (d as f64).ln() + dweight as f64 * 2.302585092994046;
    ln_approx.abs().log10() as i64
}

impl BigDec {
    /// v0.62: exact negation.
    pub(crate) fn neg(&self) -> BigDec {
        let mut r = self.clone();
        if !r.mag.is_zero() {
            r.neg = !r.neg;
        }
        r
    }

    /// v0.62: exact sum.
    pub(crate) fn add(&self, other: &BigDec) -> BigDec {
        self.sub(&other.neg())
    }

    /// v0.62: `self / other` rounded to `rscale` fractional digits,
    /// half away from zero (PG19 `div_var` with round=true).
    /// None on division by zero.
    pub(crate) fn div_round(&self, other: &BigDec, rscale: i32) -> Option<BigDec> {
        if other.mag.is_zero() {
            return None;
        }
        if self.mag.is_zero() {
            return Some(BigDec::zero().round_to_scale(rscale));
        }
        let neg = self.neg != other.neg;
        // Round(|MA| * 10^P / |MB|) with P = rscale - sa + sb, keeping
        // one guard digit for the half-away-from-zero rounding.
        let p = rscale as i64 - self.scale as i64 + other.scale as i64;
        let mut num = self.mag.clone();
        let mut den = other.mag.clone();
        if p >= 0 {
            num.mul_pow10_assign(u32::try_from(p + 1).ok()?);
        } else {
            // Fold the negative power into the denominator exactly:
            // num/den = (MA/MB)·10^(1+p) = true·10^(rscale+1).
            num.mul_pow10_assign(1);
            den.mul_pow10_assign(u32::try_from(-p).ok()?);
        }
        let (t, _) = num.div_rem(&den);
        let ten = BigUint::from_u64(10);
        let (mut q, d) = t.div_rem(&ten);
        if d.cmp(&BigUint::from_u64(5)) != std::cmp::Ordering::Less {
            q.add_small_assign(1);
        }
        Some(
            BigDec {
                neg: neg && !q.is_zero(),
                mag: q,
                scale: rscale,
            }
            .normalize(),
        )
    }

    /// v0.62: `self / d` rounded to `rscale` fractional digits, half
    /// away from zero (PG19 `div_var_int` with round=true).
    pub(crate) fn div_small_round(&self, d: u32, rscale: i32) -> Option<BigDec> {
        if d == 0 {
            return None;
        }
        self.div_round(&BigDec::from_i64(d as i64), rscale)
    }

    /// v0.62: finite BigDec -> f64 with PG19
    /// `numericvar_to_double_no_overflow` semantics: overflow maps to
    /// ±infinity, underflow to 0; never NaN for finite input.
    pub(crate) fn to_f64(&self) -> f64 {
        if self.mag.is_zero() {
            return 0.0;
        }
        // Leading 15 decimal digits as an exact f64 (< 2^53).
        let limbs = &self.mag.limbs;
        let n = limbs.len();
        let mut d = limbs[n - 1] as u64;
        let mut dig = decimal_digits_u32(limbs[n - 1]) as i64;
        if n >= 2 {
            d = d * 1_000_000_000 + limbs[n - 2] as u64;
            dig += 9;
        }
        while dig > 15 {
            d /= 10;
            dig -= 1;
        }
        // value ~= d * 10^(dec_exp - (dig - 1)).
        let dec_exp = self.mag.decimal_digits() as i64 - 1 - self.scale as i64;
        let mut e = dec_exp - (dig - 1);
        let mut r = d as f64;
        while e > 300 {
            r *= 1e300;
            e -= 300;
        }
        while e < -300 {
            r *= 1e-300;
            e += 300;
        }
        r *= 10f64.powi(e as i32);
        if self.neg { -r } else { r }
    }

    /// v0.62: square root rounded to `rscale` fractional digits, half
    /// away from zero. Newton iteration on a decimal-scaled argument
    /// with ~20 guard digits; the final rounding is then the correctly
    /// rounded value (PG's exact digit-by-digit `sqrt_var` agrees).
    /// The caller rejects negative inputs (PG raises 2201F).
    pub(crate) fn sqrt_round(&self, rscale: i32) -> Option<BigDec> {
        debug_assert!(!self.neg && !self.mag.is_zero());
        // Scale by an even power of ten into [1, 100): the f64 seed is
        // then in range, and sqrt(a * 10^2k) = sqrt(a) * 10^k exactly.
        let e10 = self.mag.decimal_digits() as i64 - 1 - self.scale as i64;
        let two_k = e10.div_euclid(2) * 2;
        let mut xs = self.clone();
        xs.scale = xs.scale.checked_add(i32::try_from(two_k).ok()?)?;
        // The Newton error is relative; scaling the root back up by
        // 10^(two_k/2) scales the absolute error too, so the working
        // scale needs two_k/2 extra fractional digits for large results
        // (e.g. the 34-digit sqrt boundary pair in PG's numeric.out,
        // whose roots sit ~1e-18 from the rounding boundary).
        let wscale = rscale
            .checked_add(20)?
            .checked_add(i32::try_from((two_k / 2).max(0)).ok()?)?;
        let xf = xs.to_f64();
        if !xf.is_finite() || xf <= 0.0 {
            return None;
        }
        // f64 seed (~15 correct digits), widened to the working scale.
        let seed = BigDec::parse_decimal(&format!("{:.17}", xf.sqrt())).ok()?;
        let mut y = seed.round_to_scale(wscale);
        // Newton doubles correct digits per step; bound the iterations.
        let mut have = 15.0f64;
        let need = wscale as f64 + 12.0;
        let mut iters = 2u32;
        while have < need {
            have *= 2.0;
            iters += 1;
        }
        for _ in 0..iters {
            let q = xs.div_round(&y, wscale)?;
            let ny = y.add(&q).div_small_round(2, wscale)?;
            if ny.cmp(&y) == std::cmp::Ordering::Equal {
                y = ny;
                break;
            }
            y = ny;
        }
        // Scale back and round to the target.
        y.scale = y.scale.checked_sub(i32::try_from(two_k / 2).ok()?)?;
        Some(y.round_to_scale(rscale))
    }
}

/// v0.62: PG19 `ln_var` — natural logarithm rounded to `rscale`
/// fractional digits. `None` on arithmetic overflow (caller maps to
/// 22003); the caller rejects non-positive inputs.
pub(crate) fn ln_var_inner(x: &BigDec, rscale: i32) -> Option<BigDec> {
    debug_assert!(!x.neg && !x.mag.is_zero());
    // Reduce into (0.9, 1.1) with repeated square roots, keeping about
    // rscale + 8 significant digits at each step.
    let nine_tenths = BigDec::from_i64(9).div_small_round(10, 1)?;
    let eleven_tenths = BigDec::from_i64(11).div_small_round(10, 1)?;
    let mut xs = x.clone();
    let mut fact = BigDec::from_i64(2);
    let mut nsqrt: u32 = 0;
    while xs.cmp(&nine_tenths) != std::cmp::Ordering::Greater
        || xs.cmp(&eleven_tenths) != std::cmp::Ordering::Less
    {
        // PG: local_rscale = rscale - x.weight * DEC_DIGITS / 2 + 8
        // (exact here since DEC_DIGITS = 4 is even).
        let local = rscale as i64 - dec_w4(&xs) / 2 + 8;
        let local_i32 = local.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        xs = xs.sqrt_round(local_i32)?;
        nsqrt += 1;
        fact = fact.mul_exact(&BigDec::from_i64(2))?;
    }
    // z = (xs - 1) / (xs + 1); sum z + z^3/3 + z^5/5 + ...
    // The series result is scaled by 2^(nsqrt+1), whose decimal weight
    // is (nsqrt+1) * log10(2): work with that many extra digits.
    let local_rscale = (rscale as i64 + ((nsqrt + 1) as f64 * 0.301029995663981) as i64 + 8).max(0);
    let local_rscale = local_rscale.clamp(0, i32::MAX as i64) as i32;
    let num = xs.sub(&BigDec::one());
    let den = xs.add(&BigDec::one());
    let z = num.div_round(&den, local_rscale)?;
    let z2 = z.mul_round(&z, local_rscale);
    let mut res = z.clone();
    let mut xx = z;
    let mut ni: u32 = 1;
    loop {
        ni = ni.checked_add(2)?;
        xx = xx.mul_round(&z2, local_rscale);
        let elem = xx.div_small_round(ni, local_rscale)?;
        if elem.is_zero() {
            break;
        }
        res = res.add(&elem);
        // PG stops once the terms are too small to affect the result
        // at local_rscale (weights are base-10000, hence the factor 4).
        if dec_w4(&elem) < dec_w4(&res) - 4 * (local_rscale as i64 / 2) {
            break;
        }
    }
    // Compensate for the range reduction, rounding to rscale.
    Some(res.mul_round(&fact, rscale))
}

/// v0.62: PG19 `exp_var` — e^x rounded to `rscale` fractional digits.
/// `dscale` is the argument's display scale (only used for working
/// precision selection).
pub(crate) fn exp_var_inner(x: &BigDec, dscale: i32, rscale: i32) -> ExpOutcome {
    // PG converts via numericvar_to_double_no_overflow: overflow ->
    // ±infinity, underflow -> 0.
    let xf = x.to_f64();
    // Guard against overflow/underflow (|x| >= 3000).
    if xf.abs() >= 3000.0 {
        if xf > 0.0 {
            return ExpOutcome::Overflow;
        }
        return ExpOutcome::Underflow;
    }
    // Decimal weight of the result: log10(e^x) = x * log10(e).
    let dweight = (xf * 0.434294481903252) as i64;
    // Reduce x into ~±0.01 by dividing by 2^ndiv2.
    let mut xs = x.clone();
    let mut ndiv2: u32 = 0;
    let mut v = xf;
    if v.abs() > 0.01 {
        ndiv2 = 1;
        v /= 2.0;
        while v.abs() > 0.01 {
            ndiv2 += 1;
            v /= 2.0;
        }
        // |x| < 3000 here, so ndiv2 <= 19 and the shift cannot overflow.
        debug_assert!(ndiv2 <= 19);
        let local = dscale + ndiv2 as i32;
        let div = 1u32 << ndiv2;
        match xs.div_small_round(div, local) {
            Some(q) => xs = q,
            None => return ExpOutcome::Overflow,
        }
    }
    // Working precision: the result has (dweight + rscale + 1)
    // significant digits, plus headroom for the squaring steps.
    let sig_digits =
        (1 + dweight + rscale as i64 + (ndiv2 as f64 * 0.301029995663981) as i64).max(0) + 8;
    let local_rscale = (sig_digits - 1).max(0).min(i32::MAX as i64) as i32;
    // exp(x) = 1 + x + x^2/2! + ...
    let mut elem = BigDec::one();
    let mut res = BigDec::one();
    let mut ni: u32 = 1;
    loop {
        elem = elem.mul_round(&xs, local_rscale);
        match elem.div_small_round(ni, local_rscale) {
            Some(q) => elem = q,
            None => return ExpOutcome::Overflow,
        }
        let nres = res.add(&elem);
        if nres.cmp(&res) == std::cmp::Ordering::Equal {
            res = nres;
            break;
        }
        res = nres;
        ni = match ni.checked_add(1) {
            Some(n) => n,
            None => return ExpOutcome::Overflow,
        };
    }
    // Compensate for the argument reduction by repeated squaring,
    // shrinking the working scale as the weight doubles (as PG does).
    let mut n = ndiv2;
    while n > 0 {
        n -= 1;
        let lr = (sig_digits - 2 * dec_w4(&res)).max(0).min(i32::MAX as i64) as i32;
        res = res.mul_round(&res, lr);
    }
    ExpOutcome::Finite(res.round_to_scale(rscale))
}

/// v0.62: PG19 `log_var` — logarithm of `num` in base `base`.
/// `dscale1`/`dscale2` are the inputs' display scales. Returns the
/// value and its result scale. `DivisionByZero` when `base` is 1
/// (PG raises 22012); `Overflow` when unrepresentable.
pub(crate) fn log_var_inner(
    base: &BigDec,
    num: &BigDec,
    dscale1: i32,
    dscale2: i32,
) -> Result<(BigDec, i32), LogError> {
    // Estimated dweights, exactly as PG19 computes them.
    let ln_base_dweight = estimate_ln_dweight(base);
    let ln_num_dweight = estimate_ln_dweight(num);
    let result_dweight = ln_num_dweight - ln_base_dweight;
    let rscale = (16 - result_dweight)
        .max(dscale1 as i64)
        .max(dscale2 as i64)
        .max(0)
        .min(1000) as i32;
    // Each logarithm gets more significant digits than the result.
    // (PG does not clamp these to 1000.)
    let ln_base_rscale = (rscale as i64 + result_dweight - ln_base_dweight + 8).max(0);
    let ln_num_rscale = (rscale as i64 + result_dweight - ln_num_dweight + 8).max(0);
    let ln_base = ln_var_inner(base, ln_base_rscale.clamp(0, i32::MAX as i64) as i32)
        .ok_or(LogError::Overflow)?;
    if ln_base.is_zero() {
        // log(1, x): PG's div_var raises 22012 division by zero.
        return Err(LogError::DivisionByZero);
    }
    let ln_num = ln_var_inner(num, ln_num_rscale.clamp(0, i32::MAX as i64) as i32)
        .ok_or(LogError::Overflow)?;
    let q = ln_num
        .div_round(&ln_base, rscale)
        .ok_or(LogError::Overflow)?;
    Ok((q, rscale))
}

/// v0.62: parity of an integral `Numeric` (true = odd); None when the
/// value is not an exact integer. Mirrors PG19 `power_var`'s
/// integral/odd tests on the exponent.
/// v0.63: big-mantissa aware — a big value's `unscaled` holds only the
/// sign, so parity comes from the magnitude's lowest bit.
pub(crate) fn integral_parity(n: &Numeric) -> Option<bool> {
    if n.special != NumericSpecial::Finite || n.unscaled == 0 {
        return Some(false);
    }
    if let Some(mag) = n.big.as_ref() {
        // Normalized: with scale > 0 there are no trailing zeros, so a
        // fractional big value is never integral; with scale <= 0 the
        // value is integral and parity is the magnitude's lowest bit.
        if n.scale > 0 {
            return Some(false);
        }
        return Some(mag.limbs.first().is_some_and(|l| l & 1 == 1));
    }
    if n.scale <= 0 {
        // value = unscaled * 10^-scale; a factor 10^k (k >= 1) is even.
        return Some(n.scale == 0 && (n.unscaled & 1) == 1);
    }
    // scale > 0: integral iff 10^scale divides unscaled.
    let p = 10i128.checked_pow(n.scale as u32)?;
    if n.unscaled % p != 0 {
        return None;
    }
    Some(((n.unscaled / p) & 1) == 1)
}

/// v0.62: PG19 `power_var` for exponents that do not fit the int32
/// fast path (fractional or huge integral exponents):
/// `exp(exp * ln(|base|))` with adaptive precision. The caller keeps
/// the int32-integral fast path and the SQL-level 2201F "zero raised
/// to a negative power" check.
pub(crate) fn power_var_frac(base: &Numeric, exp: &Numeric) -> Result<Numeric, PowerFracError> {
    let b = BigDec::from_numeric(base).ok_or(PowerFracError::Overflow)?;
    // 0 ^ x (for x != 0; 0^0 uses power_var_int) is zero, dscale 16.
    if b.is_zero() {
        return Ok(Numeric::zero().with_dscale(16));
    }
    // Negative base: the exponent must be an integer; the result is
    // negative for odd exponents. (Int32-integral exponents never
    // reach here.)
    let mut res_sign = false;
    let mut ab = b.clone();
    if b.neg {
        match integral_parity(exp) {
            None => return Err(PowerFracError::NegativeBase),
            Some(odd) => {
                res_sign = odd;
                ab.neg = false;
            }
        }
    }
    let e = BigDec::from_numeric(exp).ok_or(PowerFracError::Overflow)?;
    // Low-precision ln(base) (~8 significant digits) for scale
    // selection. (PG notes this scale may exceed 1000.)
    let ln_dweight = estimate_ln_dweight(&ab);
    let lo_rscale = (8 - ln_dweight).max(0);
    let lo_rscale_i32 = lo_rscale.clamp(0, i32::MAX as i64) as i32;
    let ln_base_lo = ln_var_inner(&ab, lo_rscale_i32).ok_or(PowerFracError::Overflow)?;
    let y_lo = ln_base_lo.mul_round(&e, lo_rscale_i32);
    let val = y_lo.to_f64();
    // Crude overflow/underflow test with a fuzz factor; exp_var
    // applies the exact threshold. Note PG's underflow zero carries
    // dscale 1000 here.
    if val.abs() > 3003.01 {
        if val > 0.0 {
            return Err(PowerFracError::Overflow);
        }
        return Ok(Numeric::zero().with_dscale(1000));
    }
    // Approximate decimal weight of the result.
    let vw = val * 0.434294481903252;
    // The C `(int)` truncates toward zero; Rust's `as` does the same and
    // saturates, so no clamp is needed (PG19 applies none here either).
    // |vw| <= 3003.01 * 0.4343 < 1305 after the fuzz-factor test above.
    let vw_i = vw as i64;
    // v0.63: PG19 uses the full declared dscales (no 50-cap); the
    // big-mantissa extension holds the resulting precision.
    let rscale = (16 - vw_i)
        .max(base.dscale as i64)
        .max(exp.dscale as i64)
        .max(0)
        .min(1000) as i32;
    // v0.62: PG's rscale is used uncapped for the BigDec computation
    // (BigUint handles arbitrary precision). v0.63: results keep
    // their exact magnitude via the big-mantissa extension.
    let sig_digits = (rscale as i64 + vw_i).max(0);
    let local_rscale = (sig_digits - ln_dweight + 8).max(0);
    // The real calculation.
    let local_rscale_i32 = local_rscale.clamp(0, i32::MAX as i64) as i32;
    let ln_base = ln_var_inner(&ab, local_rscale_i32).ok_or(PowerFracError::Overflow)?;
    let y = ln_base.mul_round(&e, local_rscale_i32);
    let mut res = match exp_var_inner(&y, local_rscale_i32, rscale) {
        ExpOutcome::Finite(d) => d,
        ExpOutcome::Underflow => BigDec::zero(),
        ExpOutcome::Overflow => return Err(PowerFracError::Overflow),
    };
    if res_sign && !res.is_zero() {
        res = res.neg();
    }
    // v0.63: big-mantissa results keep their exact magnitude
    // (e.g. 12.3 ^ 45.6) instead of 22003.
    Numeric::from_bigdec_exact(&res)
        .map(|n| {
            let nd = rscale.min(n.dscale);
            n.with_dscale(nd)
        })
        .ok_or(PowerFracError::Overflow)
}

impl Numeric {
    /// v0.62: PG19 `numeric_sqrt`. The result scale gives at least 16
    /// significant digits but is never less than the input dscale.
    /// None when the correctly-rounded result cannot be represented
    /// (PG raises 22003). The caller handles NaN/±Inf and negatives.
    pub(crate) fn sqrt_pg(&self) -> Option<Numeric> {
        debug_assert!(self.special == NumericSpecial::Finite);
        let x = BigDec::from_numeric(self)?;
        // sweight = arg.weight * DEC_DIGITS / 2 + 1 (exact: DEC_DIGITS
        // is even, so the C division needs no floor fixup).
        let sweight = dec_w4(&x) / 2 + 1;
        // v0.63: PG19 uses the full declared dscale (no 50-cap).
        let rscale = (16 - sweight).max(self.dscale as i64).max(0).min(1000) as i32;
        if x.is_zero() {
            return Some(Numeric::zero().with_dscale(rscale));
        }
        // v0.63: big-mantissa aware (no i128 narrowing).
        let r = x
            .sqrt_round(rscale)
            .and_then(|d| Self::from_bigdec_exact(&d))?;
        let d = rscale.min(r.dscale);
        Some(r.with_dscale(d))
    }

    /// v0.62: PG19 `numeric_exp`. None when the result cannot be
    /// represented (PG raises 22003). The caller handles NaN/±Inf.
    pub(crate) fn exp_pg(&self) -> Option<Numeric> {
        debug_assert!(self.special == NumericSpecial::Finite);
        // Result scale from the no-overflow float conversion, exactly
        // as PG19 computes it.
        let xf = BigDec::from_numeric(self)?.to_f64();
        let v = (xf * 0.434294481903252).clamp(-1000.0, 1000.0);
        // v0.63: PG19 uses the full declared dscale (no 50-cap).
        let rscale = (16 - v as i64).max(self.dscale as i64).max(0).min(1000) as i32;
        let x = BigDec::from_numeric(self)?;
        match exp_var_inner(&x, self.dscale, rscale) {
            ExpOutcome::Finite(d) => {
                // v0.63: big-mantissa aware (no i128 narrowing).
                let n = Self::from_bigdec_exact(&d)?;
                let nd = rscale.min(n.dscale);
                Some(n.with_dscale(nd))
            }
            ExpOutcome::Underflow => Some(Numeric::zero().with_dscale(rscale)),
            ExpOutcome::Overflow => None,
        }
    }

    /// v0.62: PG19 `numeric_ln`. None when unrepresentable (22003).
    /// The caller handles NaN/±Inf and non-positive inputs.
    pub(crate) fn ln_pg(&self) -> Option<Numeric> {
        debug_assert!(self.special == NumericSpecial::Finite);
        let x = BigDec::from_numeric(self)?;
        // v0.63: PG19 uses the full declared dscale (no 50-cap).
        let rscale = (16 - estimate_ln_dweight(&x))
            .max(self.dscale as i64)
            .max(0)
            .min(1000) as i32;
        // v0.63: big-mantissa aware (no i128 narrowing).
        let r = ln_var_inner(&x, rscale).and_then(|d| Self::from_bigdec_exact(&d))?;
        let rd = rscale.min(r.dscale);
        Some(r.with_dscale(rd))
    }

    /// v0.62: PG19 `log_var` for finite positive base/num.
    /// `DivisionByZero` when base is 1 (PG raises 22012).
    pub(crate) fn log_pg(base: &Numeric, num: &Numeric) -> Result<Numeric, LogError> {
        let b = BigDec::from_numeric(base).ok_or(LogError::Overflow)?;
        let n = BigDec::from_numeric(num).ok_or(LogError::Overflow)?;
        // v0.63: PG19 uses the full declared dscales (no 50-cap).
        let (r, rscale) = log_var_inner(&b, &n, base.dscale, num.dscale)?;
        // v0.63: big-mantissa results keep their exact magnitude
        // (e.g. log(1.234567e-89) needs ~100 fractional digits);
        // only past PG's 131072-digit format limit is 22003.
        let out = Self::from_bigdec_exact(&r).ok_or(LogError::Overflow)?;
        let od = rscale.min(out.dscale);
        Ok(out.with_dscale(od))
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
            Value::Numeric(_) => ColType::Numeric(None),
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

#[cfg(test)]
mod v059_division_tests {
    use super::*;
    use std::cmp::Ordering;

    fn num(s: &str) -> Numeric {
        Numeric::parse(s).unwrap()
    }

    fn div_text(a: &str, b: &str) -> String {
        num(a).checked_div(&num(b)).unwrap().to_text()
    }

    fn big(s: &str) -> BigUint {
        BigUint::from_decimal_str(s)
    }

    #[test]
    fn biguint_div_rem_basic() {
        let (q, r) = big("100").div_rem(&big("3"));
        assert_eq!(q.cmp(&big("33")), Ordering::Equal);
        assert_eq!(r.cmp(&big("1")), Ordering::Equal);
        // Exact division.
        let (q, r) = big("1000").div_rem(&big("8"));
        assert_eq!(q.cmp(&big("125")), Ordering::Equal);
        assert!(r.is_zero());
        // Dividend smaller than divisor.
        let (q, r) = big("7").div_rem(&big("1000"));
        assert!(q.is_zero());
        assert_eq!(r.cmp(&big("7")), Ordering::Equal);
        // Zero dividend.
        let (q, r) = big("0").div_rem(&big("12345"));
        assert!(q.is_zero());
        assert!(r.is_zero());
        // Large multi-limb dividend: q*divisor + rem == dividend.
        let dvd = big("123456789012345678901234567890");
        let (q, r) = dvd.div_rem(&big("123"));
        assert_eq!(q.cmp(&big("1003713731807688446351500551")), Ordering::Equal);
        assert_eq!(r.cmp(&big("117")), Ordering::Equal);
        let mut back = q.mul(&big("123"));
        back.add_assign(&r);
        assert_eq!(back.cmp(&dvd), Ordering::Equal);
        // Power-of-ten-heavy limbs (base-1e9 packing edge).
        let (q, r) = big("1000000000000000000000000").div_rem(&big("1000000000"));
        assert_eq!(q.cmp(&big("1000000000000000")), Ordering::Equal);
        assert!(r.is_zero());
    }

    #[test]
    fn pg19_division_regression_cases() {
        // The v0.58 failures, now exact per PG19 select_div_scale.
        // v0.61: 0.999999999999999999999 rounds to 1 at rscale 20,
        // and PG displays the full rscale.
        assert_eq!(
            div_text("999999999999999999999", "1000000000000000000000"),
            "1.00000000000000000000"
        );
        assert_eq!(
            div_text("12345678901234567890", "123"),
            "100371373180768845"
        );
        assert_eq!(div_text("1", "3"), "0.33333333333333333333");
        assert_eq!(div_text("2", "3"), "0.66666666666666666667");
        assert_eq!(div_text("22", "7"), "3.1428571428571429");
        assert_eq!(div_text("1", "6"), "0.16666666666666666667");
        assert_eq!(div_text("10", "3"), "3.3333333333333333");
    }

    #[test]
    fn division_unequal_scales() {
        // v0.61: PG19 select_div_scale rscales (qweight-aware):
        // 1.2/3 -> rscale 20, 12/0.3 -> 12, 1e3/0.001 -> 12,
        // 0.001/1e3 -> 24, 5/4 and 2.5/1 -> 16, 1/8 -> 20.
        assert_eq!(div_text("1.2", "3"), "0.40000000000000000000");
        assert_eq!(div_text("12", "0.3"), "40.0000000000000000");
        assert_eq!(div_text("1e3", "0.001"), "1000000.000000000000");
        assert_eq!(div_text("0.001", "1e3"), "0.000001000000000000000000");
        assert_eq!(div_text("5", "4"), "1.2500000000000000");
        assert_eq!(div_text("1", "8"), "0.12500000000000000000");
        assert_eq!(div_text("2.5", "1"), "2.5000000000000000");
    }

    #[test]
    fn division_signs_and_specials() {
        // v0.61: rscale 16 per PG19 select_div_scale.
        assert_eq!(div_text("-7", "2"), "-3.5000000000000000");
        assert_eq!(div_text("7", "-2"), "-3.5000000000000000");
        assert_eq!(div_text("-7", "-2"), "3.5000000000000000");
        // NaN propagates.
        let r = num("nan").checked_div(&num("1")).unwrap();
        assert!(r.is_nan());
        // Infinity / finite keeps the sign.
        let r = num("inf").checked_div(&num("2")).unwrap();
        assert_eq!(r.special, NumericSpecial::PosInf);
        let r = num("-inf").checked_div(&num("2")).unwrap();
        assert_eq!(r.special, NumericSpecial::NegInf);
        // Finite / Infinity = 0.
        let r = num("5").checked_div(&num("inf")).unwrap();
        assert!(r.is_zero());
        // Division by zero is None (caller raises 22012).
        assert!(num("1").checked_div(&num("0")).is_none());
        assert!(num("0").checked_div(&num("0")).is_none());
    }

    #[test]
    fn div_trunc_matches_pg() {
        // div() truncates toward zero at scale 0, never rounds.
        let t = |a: &str, b: &str| num(a).div_trunc(&num(b)).unwrap().to_text();
        assert_eq!(t("7", "2"), "3");
        assert_eq!(t("-7", "2"), "-3");
        assert_eq!(t("7", "-2"), "-3");
        assert_eq!(t("-7", "-2"), "3");
        assert_eq!(t("5", "2"), "2");
        assert_eq!(t("1", "2"), "0");
        assert_eq!(t("0", "5"), "0");
    }

    #[test]
    fn round_widening_is_noop() {
        // v0.61: PG's round(x, s) sets the display scale to s (pads with
        // zeros); widening never overflows i128 because the stored value
        // is unchanged.
        assert_eq!(
            num("3.14").round_to(40).unwrap().to_text(),
            "3.14".to_string() + &"0".repeat(38)
        );
        assert_eq!(num("1.5").round_to(1).unwrap().to_text(), "1.5");
        // Narrowing still rounds half away from zero.
        assert_eq!(num("2.5").round_to(0).unwrap().to_text(), "3");
        assert_eq!(num("-2.5").round_to(0).unwrap().to_text(), "-3");
        assert_eq!(num("2.4").round_to(0).unwrap().to_text(), "2");
    }

    #[test]
    fn numeric_typmod_pg19() {
        // v0.60: PG19 `apply_typmod` for numeric(p,s) assignment
        // (bundled PG19 numeric.out cases).
        let t = |s: &str, p: u32, sc: i32| num(s).apply_typmod(p, sc);
        // numeric(4,4): round to scale, then overflow.
        assert_eq!(t("0.99994", 4, 4).unwrap().to_text(), "0.9999");
        assert_eq!(t("0.00017", 4, 4).unwrap().to_text(), "0.0002");
        assert_eq!(t("1.0", 4, 4), Err(TypmodError::Overflow));
        assert_eq!(t("0.99995", 4, 4), Err(TypmodError::Overflow));
        // +/-Infinity rejected, NaN allowed.
        assert_eq!(t("Infinity", 4, 4), Err(TypmodError::Infinite));
        assert_eq!(t("-Infinity", 4, 4), Err(TypmodError::Infinite));
        assert!(t("NaN", 4, 4).unwrap().is_nan());
        // numeric(3,-6): round left of the point, keep scale 0.
        assert_eq!(t("123456", 3, -6).unwrap().to_text(), "0");
        assert_eq!(t("654321", 3, -6).unwrap().to_text(), "1000000");
        assert_eq!(t("999500000", 3, -6), Err(TypmodError::Overflow));
        assert_eq!(t("999499999", 3, -6).unwrap().to_text(), "999000000");
        // numeric(3,3) keeps the value; (3,0) rounds fractions.
        assert_eq!(t("0.123", 3, 3).unwrap().to_text(), "0.123");
        assert_eq!(t("3.7", 3, 0).unwrap().to_text(), "4");
        assert_eq!(t("999", 3, 0).unwrap().to_text(), "999");
        assert_eq!(t("1000", 3, 0), Err(TypmodError::Overflow));
        // Ties round away from zero, like PG.
        assert_eq!(t("0.5", 3, 0).unwrap().to_text(), "1");
        assert_eq!(t("-0.5", 3, 0).unwrap().to_text(), "-1");
        // Zero (and values that round to zero) always fit, even when
        // precision - scale is negative (PG stores 0.000000 in
        // numeric(3,6)). But a nonzero value must be strictly less than
        // 10^(precision-scale): PG rejects 0.0009995 (rounds to 0.001)
        // and 0.5 in numeric(3,6).
        // v0.61: PG pads typmod-coerced results to the declared scale:
        // 0.000000123::numeric(3,6) displays as 0.000000.
        assert_eq!(t("0.000000123", 3, 6).unwrap().to_text(), "0.000000");
        assert_eq!(t("0.0009994", 3, 6).unwrap().to_text(), "0.000999");
        assert_eq!(t("0.0009995", 3, 6), Err(TypmodError::Overflow));
        assert_eq!(t("0.5", 3, 6), Err(TypmodError::Overflow));
        assert_eq!(t("5.0", 3, 6), Err(TypmodError::Overflow));
        // Unconstrained-equivalent typmods are value-preserving;
        // v0.61: PG pads to the declared scale on output.
        assert_eq!(t("123.456", 10, 5).unwrap().to_text(), "123.45600");
    }

    #[test]
    fn underscore_input_rules() {
        // PG19 set_var_from_str: leading/trailing/doubled/adjacent
        // underscores are rejected.
        assert!(Numeric::parse("_123").is_err());
        assert!(Numeric::parse("123_").is_err());
        assert!(Numeric::parse("123._456").is_err());
        assert!(Numeric::parse("1.2e_34").is_err());
        assert!(Numeric::parse("1__2").is_err());
        assert!(Numeric::parse("1_.2").is_err());
        // Interior underscores are fine.
        assert_eq!(num("1_2").to_text(), "12");
        assert_eq!(num("1_2.5_6").to_text(), "12.56");
    }
}

#[cfg(test)]
mod v061_power_dscale_tests {
    use super::*;

    fn num(s: &str) -> Numeric {
        Numeric::parse(s).unwrap()
    }

    fn pow_text(base: &str, exp: i64, exp_dscale: i32) -> String {
        num(base).power_int(exp, exp_dscale).unwrap().to_text()
    }

    #[test]
    fn power_var_int_pg_vectors() {
        // PG19 regression vectors (numeric.out).
        assert_eq!(pow_text("3.789", 21, 16), "1409343026052.8716016316022141");
        assert_eq!(
            pow_text("3.789", 35, 16),
            "177158169650516670809.3820586142670135"
        );
        assert_eq!(pow_text("1.2", 345, 0), "2077446682327378559843444695.6");
        assert_eq!(pow_text("0.12", -20, 0), "2608405330458882702.55");
        assert_eq!(pow_text("0.12", -25, 0), "104825960103961013959336.50");
        assert_eq!(pow_text("0.5678", -85, 0), "782333637740774446257.7719");
        assert_eq!(
            pow_text("1.000000000123", -2147483648, 0),
            "0.7678656556403084"
        );
    }

    #[test]
    fn power_underflow_zero_at_dscale_1000() {
        let z = num("10.0").power_int(-2147483648, 0).unwrap();
        assert_eq!(z.to_text(), "0.".to_string() + &"0".repeat(1000));
        let z2 = num("10.0").power_int(-2147483647, 0).unwrap();
        assert_eq!(z2.to_text(), "0.".to_string() + &"0".repeat(1000));
    }

    #[test]
    fn power_overflow_and_identities() {
        // v0.63: f = 5678*log10(1.234) ~ 518 — exact via the big
        // mantissa; matches PG19's numeric.out digit-for-digit.
        let big = num("1.234").power_int(5678, 0).unwrap();
        // Full 523-character expected value from PG19's numeric.out.
        assert_eq!(
            big.to_text(),
            "307239295662090741644584872593956173493568238595074141254349565406661439636598896798876823220904084953233015553994854875890890858118656468658643918169805277399402542281777901029346337707622181574346585989613344285010764501017625366742865066948856161360224801370482171458030533346309750557140549621313515752078638620714732831815297168231790779296290266207315344008883935010274044001522606235576584215999260117523114297033944018699691024106823438431754073086813382242140602291215149759520833200152654884259619588924545324.597"
        );
        // exp == 0 -> exactly 1 with dscale rscale.
        let one = num("3.789").power_int(0, 16).unwrap();
        assert_eq!(one.to_text(), "1.0000000000000000");
        // exp == 1 -> round(base, rscale).
        let id = num("3.789").power_int(1, 16).unwrap();
        assert_eq!(id.to_text(), "3.7890000000000000");
        // 2^100 fits i128 exactly (the old i128 path gave up here).
        assert_eq!(pow_text("2", 100, 0), "1267650600228229401496703205376");
        // negative base, odd/even exponents keep the sign; PG's rscale
        // is max(16 - int(f), ...): (-2)^3 has f=0.9 -> 16 digits,
        // (-2)^4 has f=1.2 -> 15 digits.
        assert_eq!(pow_text("-2", 3, 0), "-8.0000000000000000");
        assert_eq!(pow_text("-2", 4, 0), "16.000000000000000");
    }

    #[test]
    fn dscale_parse_and_display() {
        // Declared scale survives normalization.
        assert_eq!(num("1.50").dscale, 2);
        assert_eq!(num("1.50").to_text(), "1.50");
        assert_eq!(num("0.00").dscale, 2);
        assert_eq!(num("0.00").to_text(), "0.00");
        assert_eq!(num("-13.000000000000000").dscale, 15);
        assert_eq!(num("21.00").to_text(), "21.00");
        assert_eq!(num("1e200").dscale, 0);
        // Negative-scale magnitudes print in scientific notation
        // (pre-existing to_text behavior, unchanged by v0.61).
        assert_eq!(num("1e200").to_text(), "1e+200");
        // Equality is by value, not by display scale.
        assert_eq!(num("1.50"), num("1.5"));
    }

    #[test]
    fn dscale_arithmetic_propagation() {
        // add/sub: max; mul: sum; div: rscale; round: target.
        let s = num("1.50").checked_add(&num("2.5")).unwrap();
        assert_eq!(s.dscale, 2);
        assert_eq!(s.to_text(), "4.00");
        let m = num("1.50").checked_mul(&num("2.0")).unwrap();
        assert_eq!(m.dscale, 3);
        assert_eq!(m.to_text(), "3.000");
        let d = num("1").checked_div(&num("3")).unwrap();
        // PG19 select_div_scale: 1/3 has rscale 20.
        assert_eq!(d.dscale, 20);
        assert_eq!(d.to_text(), "0.33333333333333333333");
        let r = num("3.14").round_to(4).unwrap();
        assert_eq!(r.dscale, 4);
        assert_eq!(r.to_text(), "3.1400");
        // neg/abs preserve.
        assert_eq!(num("1.50").neg().to_text(), "-1.50");
        assert_eq!(num("-1.50").abs().to_text(), "1.50");
    }

    #[test]
    fn bigdec_round_trip_helpers() {
        // round_to_scale half away from zero.
        let b = BigDec::from_numeric(&num("2.5")).unwrap().round_to_scale(0);
        assert_eq!(b.to_numeric().unwrap().to_text(), "3");
        let b = BigDec::from_numeric(&num("-2.5"))
            .unwrap()
            .round_to_scale(0);
        assert_eq!(b.to_numeric().unwrap().to_text(), "-3");
        // recip_round: 1/8 = 0.125.
        let r = BigDec::from_numeric(&num("8"))
            .unwrap()
            .recip_round(3)
            .unwrap();
        assert_eq!(r.to_numeric().unwrap().to_text(), "0.125");
        // rem drives the Euclidean algorithm.
        let a = BigDec::from_numeric(&num("43312.5")).unwrap();
        let c = BigDec::from_numeric(&num("4637.5")).unwrap();
        let g = {
            let (mut x, mut y) = (a, c);
            while !y.is_zero() {
                let t = x.rem(&y);
                x = y;
                y = t;
            }
            x
        };
        assert_eq!(g.to_numeric().unwrap().to_text(), "87.5");
        // div_exact: 42328.2 / 47.4 = 893 exactly.
        let q = BigDec::from_numeric(&num("42328.2"))
            .unwrap()
            .div_exact(&BigDec::from_numeric(&num("47.4")).unwrap())
            .unwrap();
        assert_eq!(q.to_numeric().unwrap().to_text(), "893");
    }
}

#[cfg(test)]
mod v062_transcendental_tests {
    use super::*;

    fn num(s: &str) -> Numeric {
        Numeric::parse(s).unwrap()
    }

    fn dec(s: &str) -> BigDec {
        BigDec::from_numeric(&num(s)).unwrap()
    }

    fn div_text(a: &str, b: &str, p: i32) -> String {
        dec(a)
            .div_round(&dec(b), p)
            .unwrap()
            .to_numeric()
            .unwrap()
            .to_text()
    }

    #[test]
    fn div_round_basic_vectors() {
        assert_eq!(div_text("1", "3", 16), "0.3333333333333333");
        assert_eq!(div_text("22", "7", 6), "3.142857");
        assert_eq!(div_text("1", "8", 3), "0.125");
        assert_eq!(div_text("2", "3", 0), "1");
        // Half away from zero.
        assert_eq!(div_text("5", "2", 0), "3");
        assert_eq!(div_text("-5", "2", 0), "-3");
        assert_eq!(div_text("-1", "8", 3), "-0.125");
    }

    #[test]
    fn div_round_negative_power_branch() {
        // p = rscale - sa + sb < 0 here. 3e-17/1 at rscale 16 must be
        // zero (the denominator power was off by one before the fix,
        // yielding 3e-16).
        assert_eq!(
            div_text("0.00000000000000003", "1", 16),
            "0.0000000000000000"
        );
        // 9e-17/1 at rscale 16 rounds up to 1e-16 through the same branch.
        assert_eq!(
            div_text("0.00000000000000009", "1", 16),
            "0.0000000000000001"
        );
    }

    #[test]
    fn sqrt_pg_vectors() {
        // PG19 numeric.out vectors, including the 34-digit rounding
        // boundary pair (roots sit ~1e-18 from x.5).
        let s = |x: &str| num(x).sqrt_pg().unwrap().to_text();
        assert_eq!(s("1.000000000000003"), "1.000000000000001");
        assert_eq!(s("96627521408608.56340355805"), "9829929.87811248648");
        assert_eq!(s("96627521408608.56340355806"), "9829929.87811248649");
        assert_eq!(
            s("515549506212297735.073688290367"),
            "718017761.766585921184"
        );
        assert_eq!(
            s("515549506212297735.073688290368"),
            "718017761.766585921185"
        );
        assert_eq!(s("8015491789940783531003294973900306"), "89529278953540017");
        assert_eq!(s("8015491789940783531003294973900307"), "89529278953540018");
        // sqrt(4) = 2 at the adaptive scale (dscale 0 -> rscale 15).
        assert_eq!(s("4"), "2.000000000000000");
        // sqrt(2) at the adaptive scale.
        assert_eq!(s("2"), "1.414213562373095");
    }

    #[test]
    fn exp_pg_vectors() {
        let e = |x: &str| num(x).exp_pg().unwrap().to_text();
        assert_eq!(e("32.999"), "214429043492155.053");
        assert_eq!(e("-32.999"), "0.000000000000004663547361468248");
        assert_eq!(
            e("-123.456"),
            "0.000000000000000000000000000000000000000000000000000002419582541264601"
        );
        assert_eq!(e("0"), "1.0000000000000000");
        assert_eq!(e("1"), "2.7182818284590452");
        // |x| >= 3000: positive overflows, negative underflows to zero.
        assert_eq!(num("3000").exp_pg(), None);
        assert_eq!(
            num("-3000").exp_pg().unwrap().to_text(),
            "0.".to_string() + &"0".repeat(1000)
        );
        // v0.63: 51-digit result (PG's exp(123.456)) — exact via the
        // big mantissa; matches PG19's numeric.out digit-for-digit.
        assert_eq!(
            e("123.456"),
            "413294435277809344957685441227343146614594393746575438.725"
        );
    }

    #[test]
    fn ln_pg_vectors() {
        let l = |x: &str| num(x).ln_pg().unwrap().to_text();
        assert_eq!(l("0.99949452"), "-0.00050560779808326467");
        assert_eq!(l("1.00049687395"), "0.00049675054901370394");
        assert_eq!(l("1234.567890123456789"), "7.1184763012977896");
        assert_eq!(l("5.80397490724e5"), "13.271468476626518");
        assert_eq!(l("9.342536355e34"), "80.522470935524187");
        assert_eq!(l("1"), "0.0000000000000000");
        // 38-digit result (PG regression literal), and the 32-digit
        // truncation of PG's 48-digit literal (full mantissa exceeds
        // i128; expected value independently verified at 80 digits).
        assert_eq!(
            l("1.2345678e-28"),
            "-64.26166165451762991204894255882820859"
        );
        assert_eq!(
            l("0.34987394835935402949394830974571"),
            "-1.05018233691208277569399169797975"
        );
    }

    #[test]
    fn log_pg_vectors() {
        let lg = |b: &str, x: &str| Numeric::log_pg(&num(b), &num(x)).unwrap().to_text();
        assert_eq!(lg("10", "9.999999999999999999"), "1.000000000000000000");
        assert_eq!(lg("10", "10.00000000000000000"), "1.00000000000000000");
        assert_eq!(lg("10", "10.00000000000000001"), "1.00000000000000000");
        assert_eq!(lg("10", "590489.45235237"), "5.771212144411727");
        assert_eq!(lg("0.99923", "4.58934e34"), "-103611.55579544132");
        assert_eq!(lg("1.000016", "8.452010e18"), "2723830.2877097365");
        // log(1, x): PG's div_var raises 22012 division by zero.
        assert_eq!(
            Numeric::log_pg(&num("1"), &num("10")),
            Err(LogError::DivisionByZero)
        );
    }

    #[test]
    fn power_frac_vectors() {
        let p = |b: &str, e: &str| power_var_frac(&num(b), &num(e)).unwrap().to_text();
        assert_eq!(p("32.1", "9.8"), "580429286790711.10");
        assert_eq!(p("32.1", "-9.8"), "0.000000000000001722862754788209");
        // Smoke-probe cases (PG19 values).
        assert_eq!(p("2", "4.2"), "18.379173679952560");
        assert_eq!(p("4.2", "4.2"), "414.61691860129675");
        assert_eq!(p("10", "0.5"), "3.1622776601683793");
        assert_eq!(
            p("12.3", "-45.6"),
            "0.00000000000000000000000000000000000000000000000001996764828785491"
        );
        // Negative base to a non-integer power: 2201F.
        assert_eq!(
            power_var_frac(&num("-2"), &num("2.5")),
            Err(PowerFracError::NegativeBase)
        );
        // 0 ^ positive fraction: zero with dscale 16.
        assert_eq!(
            power_var_frac(&num("0"), &num("2.5")).unwrap().to_text(),
            "0.0000000000000000"
        );
        // Huge integral exponents that miss the int32 fast path keep
        // their sign through the frac path.
        assert_eq!(
            power_var_frac(&num("-1"), &num("4000000000"))
                .unwrap()
                .to_text(),
            "1.0000000000000000"
        );
        assert_eq!(
            power_var_frac(&num("-1"), &num("4000000001"))
                .unwrap()
                .to_text(),
            "-1.0000000000000000"
        );
    }

    #[test]
    fn taylor_series_exp_small_arg() {
        // exp_var_inner Taylor path directly (|x| <= 0.01, no halving).
        let t = match exp_var_inner(&dec("0.01"), 2, 20) {
            ExpOutcome::Finite(d) => d.to_numeric().unwrap().to_text(),
            _ => panic!("exp(0.01) should be finite"),
        };
        assert_eq!(t, "1.01005016708416805754");
    }

    #[test]
    fn taylor_series_ln_atanh() {
        // ln_var_inner atanh series directly (1.1 needs no sqrt reduction).
        let t = ln_var_inner(&dec("1.1"), 20)
            .unwrap()
            .to_numeric()
            .unwrap()
            .to_text();
        assert_eq!(t, "0.09531017980432486004");
    }

    #[test]
    fn high_dscale_power_ln_regression() {
        // v0.62 regression: conformance INSERT
        //   POWER(numeric '10', LN(ABS(round(val,200))))
        // failed with 22003 because round(val,200) sets dscale=200 and
        // PG's dscale-driven rscale demanded 200-digit intermediates.
        // The i128-backed Numeric honestly narrows the fractional
        // precision (keeping the integer part exact) instead of
        // erroring.
        let v = num("74881");
        let r = v.round_to(200).unwrap();
        assert_eq!(r.dscale, 200);
        let y = r.ln_pg().expect("ln of round(val,200) must succeed");
        // y ≈ 11.223..., narrowed from 200-digit PG scale to i128 range.
        let ten = num("10");
        let p = power_var_frac(&ten, &y).expect("power must succeed");
        // 10^ln(74881) = 10^11.223... ≈ 1.6736e11; the integer part
        // must be exact (narrowing only drops fractional digits).
        let text = p.to_text();
        assert!(
            text.starts_with("167361463828."),
            "expected 10^ln(74881)≈167361463828, got {text}"
        );
    }

    #[test]
    fn taylor_series_terminates() {
        // Adversarial inputs for the Taylor loops: ln just above 1
        // (many atanh terms) and exp at the halving boundary.
        let l = ln_var_inner(&dec("1.0000000001"), 30).unwrap();
        assert!(!l.is_zero());
        let r = exp_var_inner(&dec("0.0100000001"), 10, 30);
        assert!(matches!(r, ExpOutcome::Finite(_)));
    }
}

#[cfg(test)]
mod v063_big_mantissa_tests {
    use super::*;

    fn num(s: &str) -> Numeric {
        Numeric::parse(s).unwrap()
    }

    #[test]
    fn big_parse_and_render_roundtrip() {
        let n = num("12345678901234567890123456789012345678901234567890");
        assert!(n.is_big());
        assert_eq!(
            n.to_text(),
            "12345678901234567890123456789012345678901234567890"
        );
        // WAL roundtrip: to_text -> parse is exact.
        assert_eq!(Numeric::parse(&n.to_text()).unwrap(), n);
        // Negative big.
        let m = num("-99999999999999999999999999999999999999999");
        assert!(m.is_big());
        assert!(m.unscaled < 0);
        assert_eq!(m.to_text(), "-99999999999999999999999999999999999999999");
    }

    #[test]
    fn big_arithmetic_exact() {
        let a = num("99999999999999999999999999999999999999");
        let b = num("99999999999999999999999999999999999999");
        assert_eq!(
            a.checked_mul(&b).unwrap().to_text(),
            "9999999999999999999999999999999999999800000000000000000000000000000000000001"
        );
        assert_eq!(
            num("100000000000000000000000000000000000000")
                .checked_rem(&num("3"))
                .unwrap()
                .to_text(),
            "1"
        );
        assert_eq!(
            num("-100000000000000000000000000000000000007")
                .checked_rem(&num("3"))
                .unwrap()
                .to_text(),
            "-2"
        );
        // Big division at scale (10^39 is big; 10^38 still fits i128).
        assert_eq!(
            num("1000000000000000000000000000000000000000")
                .div_at_scale(&num("3"), 2)
                .unwrap()
                .to_text(),
            "333333333333333333333333333333333333333.33"
        );
        // Small-huge quotient no longer saturates (exact division).
        assert_eq!(
            num("100000000000000000000000000000000000000")
                .div_at_scale(&num("3"), 2)
                .unwrap()
                .to_text(),
            "33333333333333333333333333333333333333.33"
        );
    }

    #[test]
    fn big_ordering_and_equality() {
        let big = num("100000000000000000000000000000000000000");
        let small = num("1");
        assert!(big > small);
        assert!(small < big);
        assert_eq!(big, num("100000000000000000000000000000000000000"));
        assert_ne!(big, num("99999999999999999999999999999999999999"));
        // Big vs zero.
        assert!(big > Numeric::zero());
        assert!(Numeric::zero() < big);
        // Negative big.
        let neg = num("-100000000000000000000000000000000000000");
        assert!(neg < small);
        assert!(neg < big);
    }

    #[test]
    fn big_integral_parity() {
        // 10^39 is even; 10^39+1 is odd.
        assert_eq!(
            integral_parity(&num("1000000000000000000000000000000000000000")),
            Some(false)
        );
        assert_eq!(
            integral_parity(&num("1000000000000000000000000000000000000001")),
            Some(true)
        );
        // Fractional big is not integral.
        assert_eq!(
            integral_parity(&num("1000000000000000000000000000000000000000.5")),
            Some(false)
        );
    }

    #[test]
    fn big_toast_bytes_carry_magnitude() {
        let n = num("12345678901234567890123456789012345678901234567890");
        let b = n.toast_bytes();
        assert_eq!(b.len(), n.toast_len());
        assert!(b.len() > 21);
        // Magnitude rides along as decimal bytes after the header.
        assert!(b.ends_with(b"12345678901234567890123456789012345678901234567890"));
        // Small values keep the 21-byte encoding.
        let s = num("1.5");
        assert_eq!(s.toast_len(), 21);
        assert_eq!(s.toast_bytes().len(), 21);
    }

    #[test]
    fn big_power_int_exact() {
        // (10^40)^2 = 10^80 exactly (big base, integer exponent).
        let base = num("10000000000000000000000000000000000000000");
        assert!(base.is_big());
        let p = base.power_int(2, 0).unwrap();
        assert_eq!(p.to_text(), "1".to_string() + &"0".repeat(80));
        // (10^40)^-1 = 10^-40.
        let q = base.power_int(-1, 0).unwrap();
        assert!(
            q.to_text()
                .starts_with("0.0000000000000000000000000000000000000001")
        );
    }

    #[test]
    fn numeric_limits_still_enforced() {
        // Beyond 131072 integer digits -> None (exec maps to 22003).
        assert!(
            Numeric::from_big(false, BigUint::from_decimal_str(&"9".repeat(131073)), 0, 0)
                .is_none()
        );
        // Exactly at the limit is fine.
        assert!(
            Numeric::from_big(false, BigUint::from_decimal_str(&"9".repeat(131072)), 0, 0)
                .is_some()
        );
    }
}
