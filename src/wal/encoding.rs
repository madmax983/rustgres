// v1.78 mechanical split: moved verbatim from src/wal.rs (264-301, 807-3102).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// CRC32 (IEEE 802.3), table-driven — no tables crate, no dependencies:
// the 256-entry table is built at compile time by a const fn, so there
// is zero runtime init cost. (Callgrind on the v0.4 commit path showed
// the old bitwise loop at ~1.2% of instructions; this is ~8x faster for
// the same checksum.)
// ---------------------------------------------------------------------------

pub(crate) const CRC32_TABLE: [u32; 256] = build_crc32_table();

pub(crate) const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc = CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// Binary encoding helpers (big-endian, length-prefixed strings)
// ---------------------------------------------------------------------------

pub(crate) struct Enc {
    pub(crate) buf: Vec<u8>,
}

/// v0.69: encode a RangeBound for checkpoints.
pub(crate) fn encode_range_bound(body: &mut Enc, rb: &crate::storage::RangeBound) {
    match rb {
        crate::storage::RangeBound::Min => body.u8(0),
        crate::storage::RangeBound::Max => body.u8(1),
        crate::storage::RangeBound::Val(v) => {
            body.u8(2);
            body.value(v);
        }
    }
}

/// v0.69: parse a partition key expression from its debug format.
/// Currently returns None (expression keys don't survive checkpoints —
/// documented gap). The method exists so the format is versioned.
pub(crate) fn parse_partition_expr(s: &str) -> Option<crate::sql::Expr> {
    // v0.70: parse back the derived-`Debug` form written by
    // `encode_partition_key`. v0.69 wrote `format!("{:?}", expr)` but never
    // parsed it, so expression-keyed partitions (e.g.
    // `PARTITION BY RANGE (lower(a))`) lost their key expression on
    // checkpoint reload and routed every row wrong. The parser below covers
    // the expression shapes the partition key builder accepts: `Column`,
    // `Literal` (int/float/text/bool/date/timestamp/timestamptz/null and
    // their small/big/decimal spellings), `Arith` and `Func`. Anything else
    // (or malformed input) returns `None` — the checkpoint still loads, and
    // the key degrades to the v0.69 behavior rather than refusing to start.
    let mut p = DbgParser {
        s: s.as_bytes(),
        pos: 0,
    };
    let e = p.parse_expr()?;
    p.skip_ws();
    if p.pos != p.s.len() {
        return None;
    }
    Some(e)
}

/// v0.70: tiny parser for Rust derived-`Debug` output. Only understands
/// the shapes `parse_partition_expr` needs; returns `None` on any error.
pub(crate) struct DbgParser<'a> {
    pub(crate) s: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> DbgParser<'a> {
    pub(crate) fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    pub(crate) fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub(crate) fn expect(&mut self, b: u8) -> Option<()> {
        self.skip_ws();
        if self.eat(b) { Some(()) } else { None }
    }

    /// Parse a bare identifier (`Column`, `Add`, `None`, ...).
    pub(crate) fn ident(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.s.len()
            && (self.s[self.pos].is_ascii_alphanumeric() || self.s[self.pos] == b'_')
        {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.pos]).ok()
    }

    pub(crate) fn expect_ident(&mut self, want: &str) -> Option<()> {
        if self.ident()? == want {
            Some(())
        } else {
            None
        }
    }

    /// Parse `"..."` with Rust `escape_debug` escapes.
    pub(crate) fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let b = self.peek()?;
            match b {
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.pos += 1;
                    match self.peek()? {
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'\\' => out.push('\\'),
                        b'"' => out.push('"'),
                        b'0' => out.push('\0'),
                        b'\'' => out.push('\''),
                        b'u' => {
                            // \u{XXXX}
                            self.pos += 1;
                            self.expect(b'{')?;
                            let start = self.pos;
                            while self.peek()? != b'}' {
                                self.pos += 1;
                            }
                            let hex = std::str::from_utf8(&self.s[start..self.pos]).ok()?;
                            self.pos += 1; // '}'
                            let cp = u32::from_str_radix(hex, 16).ok()?;
                            out.push(char::from_u32(cp)?);
                            continue;
                        }
                        _ => return None,
                    }
                    self.pos += 1;
                }
                _ => {
                    // Raw UTF-8 bytes (escape_debug leaves printable
                    // Unicode unescaped).
                    let rest = std::str::from_utf8(&self.s[self.pos..]).ok()?;
                    let ch = rest.chars().next()?;
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    pub(crate) fn number(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some(b'-') || self.peek() == Some(b'+') {
            self.pos += 1;
        }
        let mut any = false;
        while self.pos < self.s.len()
            && (self.s[self.pos].is_ascii_digit() || self.s[self.pos] == b'.')
        {
            any = true;
            self.pos += 1;
        }
        // f64 Debug may print like `1e300`.
        if self.pos < self.s.len() && (self.s[self.pos] == b'e' || self.s[self.pos] == b'E') {
            self.pos += 1;
            if self.peek() == Some(b'-') || self.peek() == Some(b'+') {
                self.pos += 1;
            }
            while self.pos < self.s.len() && self.s[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if !any || self.pos == start {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.pos]).ok()
    }

    /// `field_name :` inside a `{ ... }` struct.
    pub(crate) fn field(&mut self, name: &str) -> Option<()> {
        self.expect_ident(name)?;
        self.expect(b':')
    }

    pub(crate) fn parse_expr(&mut self) -> Option<Expr> {
        let tag = self.ident()?;
        match tag {
            "Column" => {
                self.expect(b'{')?;
                self.field("table")?;
                self.skip_ws();
                let table = if self.peek() == Some(b'N') {
                    self.expect_ident("None")?;
                    None
                } else {
                    self.expect_ident("Some")?;
                    self.expect(b'(')?;
                    let t = self.string()?;
                    self.expect(b')')?;
                    Some(t)
                };
                self.expect(b',')?;
                self.field("name")?;
                let name = self.string()?;
                self.expect(b'}')?;
                Some(Expr::Column { table, name })
            }
            "Literal" => {
                self.expect(b'(')?;
                let lit = self.parse_literal()?;
                self.expect(b')')?;
                Some(Expr::Literal(lit))
            }
            "Arith" => {
                self.expect(b'{')?;
                self.field("op")?;
                let op = match self.ident()? {
                    "Add" => ArithOp::Add,
                    "Sub" => ArithOp::Sub,
                    "Mul" => ArithOp::Mul,
                    "Div" => ArithOp::Div,
                    "Mod" => ArithOp::Mod,
                    "Pow" => ArithOp::Pow,
                    "BitAnd" => ArithOp::BitAnd,
                    "BitOr" => ArithOp::BitOr,
                    "BitXor" => ArithOp::BitXor,
                    "Shl" => ArithOp::Shl,
                    "Shr" => ArithOp::Shr,
                    _ => return None,
                };
                self.expect(b',')?;
                self.field("left")?;
                let left = self.parse_expr()?;
                self.expect(b',')?;
                self.field("right")?;
                let right = self.parse_expr()?;
                self.expect(b'}')?;
                Some(Expr::Arith {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                })
            }
            "Func" => {
                self.expect(b'{')?;
                self.field("name")?;
                let name = self.string()?;
                self.expect(b',')?;
                self.field("args")?;
                self.expect(b'[')?;
                let mut args = Vec::new();
                loop {
                    self.skip_ws();
                    if self.eat(b']') {
                        break;
                    }
                    if !args.is_empty() {
                        self.expect(b',')?;
                    }
                    args.push(self.parse_expr()?);
                }
                self.expect(b'}')?;
                Some(Expr::Func { name, args })
            }
            _ => None,
        }
    }

    pub(crate) fn parse_literal(&mut self) -> Option<Literal> {
        let tag = self.ident()?;
        match tag {
            "Null" => Some(Literal::Null),
            "Bool" => {
                self.expect(b'(')?;
                let b = self.ident()?;
                self.expect(b')')?;
                match b {
                    "true" => Some(Literal::Bool(true)),
                    "false" => Some(Literal::Bool(false)),
                    _ => None,
                }
            }
            "Int" => {
                self.expect(b'(')?;
                let n: i64 = self.number()?.parse().ok()?;
                self.expect(b')')?;
                Some(Literal::Int(n))
            }
            "BigInt" => {
                self.expect(b'(')?;
                let n: i64 = self.number()?.parse().ok()?;
                self.expect(b')')?;
                Some(Literal::BigInt(n))
            }
            "SmallInt" => {
                self.expect(b'(')?;
                let n: i16 = self.number()?.parse().ok()?;
                self.expect(b')')?;
                Some(Literal::SmallInt(n))
            }
            "Float" => {
                self.expect(b'(')?;
                let f: f64 = self.number()?.parse().ok()?;
                self.expect(b')')?;
                Some(Literal::Float(f))
            }
            "Decimal" => {
                self.expect(b'(')?;
                let d = self.string()?;
                self.expect(b')')?;
                Some(Literal::Decimal(d))
            }
            "Text" => {
                self.expect(b'(')?;
                let t = self.string()?;
                self.expect(b')')?;
                Some(Literal::Text(t.into()))
            }
            "Date" => {
                self.expect(b'(')?;
                let n: i32 = self.number()?.parse().ok()?;
                self.expect(b')')?;
                Some(Literal::Date(n))
            }
            "Timestamp" => {
                self.expect(b'(')?;
                let n: i64 = self.number()?.parse().ok()?;
                self.expect(b')')?;
                Some(Literal::Timestamp(n))
            }
            "Timestamptz" => {
                self.expect(b'(')?;
                let n: i64 = self.number()?.parse().ok()?;
                self.expect(b')')?;
                Some(Literal::Timestamptz(n))
            }
            _ => None,
        }
    }
}

/// v0.69: decode a RangeBound from a checkpoint.
pub(crate) fn decode_range_bound(d: &mut Dec) -> Result<crate::storage::RangeBound, String> {
    let tag = d.u8().map_err(|e| bad(&e))?;
    match tag {
        0 => Ok(crate::storage::RangeBound::Min),
        1 => Ok(crate::storage::RangeBound::Max),
        2 => {
            let v = d.value().map_err(|e| bad(&e))?;
            Ok(crate::storage::RangeBound::Val(v))
        }
        _ => Err("bad range bound tag".into()),
    }
}

pub(crate) fn bad(e: &dyn std::fmt::Display) -> String {
    format!("bad checkpoint: {}", e)
}

impl Enc {
    pub(crate) fn new() -> Self {
        Enc { buf: Vec::new() }
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub(crate) fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.bytes(s.as_bytes());
    }

    /// v0.39: encode one row version, including its per-cell toast flags
    /// (parallel to the values) and the (value id, compressed) provenance
    /// for value ids introduced by this row. Used by every record that
    /// carries row versions (InsertRows, DeleteRows, UpdateRows).
    pub(crate) fn wal_row(&mut self, r: &WalRow) {
        self.u64(r.id);
        self.u64(r.xmin);
        self.u32(r.values.len() as u32);
        for v in r.values.iter() {
            self.value(v);
        }
        for i in 0..r.values.len() {
            self.u32(r.toast.get(i).copied().unwrap_or(0));
        }
        self.u32(r.toast_meta.len() as u32);
        for (k, c) in &r.toast_meta {
            self.u32(*k);
            self.u8(*c);
        }
    }

    pub(crate) fn col_type(&mut self, t: &ColType) {
        // Tags 0-3 are the v0.6 layout; new v0.7 types append after.
        self.u8(match t {
            ColType::Int => 0,
            ColType::Float => 1,
            ColType::Text => 2,
            ColType::Bool => 3,
            ColType::SmallInt => 4,
            ColType::BigInt => 5,
            ColType::Float4 => 6,
            // v0.60: numeric carries an optional (precision, scale)
            // typmod. Tag 7 keeps its v0.7 meaning (unconstrained) for
            // old WALs; tag 18 appends after v0.57's with the typmod.
            ColType::Numeric(tm) => match tm {
                None => 7,
                Some((p, s)) => {
                    self.u8(18);
                    self.i32(*p as i32);
                    self.i32(*s);
                    return;
                }
            },
            ColType::Date => 8,
            ColType::Timestamp => 9,
            ColType::Timestamptz => 10,
            ColType::Bytea => 11,
            ColType::Uuid => 12,
            // v0.35: new tags append after (v0.7 layout); the typmod is
            // stored as i32, -1 for "no typmod".
            ColType::Char(n) => {
                self.u8(13);
                self.i32(n.unwrap_or(-1));
                return;
            }
            ColType::Varchar(n) => {
                self.u8(14);
                self.i32(n.unwrap_or(-1));
                return;
            }
            // v0.36: the one-byte "char" type; tag appends after v0.35's.
            ColType::SingleChar => {
                self.u8(15);
                return;
            }
            // v0.37: regclass; tag appends after v0.36's.
            ColType::Regclass => {
                self.u8(16);
                return;
            }
            // v0.57: name; tag appends after v0.37's.
            ColType::Name => {
                self.u8(17);
                return;
            }
            // v0.64: pg_lsn; tag appends after v0.57's.
            ColType::PgLsn => {
                self.u8(19);
                return;
            }
            // v0.73: record and json; tags append after v0.64's.
            ColType::Record => {
                self.u8(20);
                return;
            }
            // v0.81: named composite; tag 23. The name is not stored
            // (WAL keeps the marker; the catalog is rebuilt from SQL).
            ColType::Composite => {
                self.u8(23);
                return;
            }
            ColType::Json => {
                self.u8(21);
                return;
            }
            // v1.17: xid; tag appends after v0.81's.
            ColType::Xid => {
                self.u8(24);
                return;
            }
            // v1.39: bit; tag appends after v1.17's.
            ColType::Bit => {
                self.u8(25);
                return;
            }
            // v1.40: tid; tag appends after v1.39's.
            ColType::Tid => {
                self.u8(26);
                return;
            }
            // v0.78: array; tag appends after v0.73's, then the PG array
            // OID (which identifies the element type). Arrays never
            // appear as table columns — no DDL support — but the codec
            // must stay exhaustive.
            ColType::Array(elem) => {
                self.u8(22);
                self.u32(elem.array_oid());
                return;
            }
        });
    }

    pub(crate) fn value(&mut self, v: &Value) {
        // Tags 0-4 are the v0.6 layout; new v0.7 values append after.
        match v {
            Value::Null => self.u8(0),
            Value::Int(i) => {
                self.u8(1);
                self.i64(*i);
            }
            Value::Float(f) => {
                self.u8(2);
                self.f64(*f);
            }
            Value::Text(s) => {
                self.u8(3);
                self.str(s);
            }
            Value::Bool(b) => {
                self.u8(4);
                self.u8(*b as u8);
            }
            Value::SmallInt(i) => {
                self.u8(5);
                self.i16(*i);
            }
            Value::BigInt(i) => {
                self.u8(6);
                self.i64(*i);
            }
            Value::Float4(f) => {
                self.u8(7);
                self.f32(*f);
            }
            Value::Numeric(n) => {
                self.u8(8);
                self.str(&n.to_text());
            }
            Value::Date(d) => {
                self.u8(9);
                self.i32(*d);
            }
            Value::Timestamp(m) => {
                self.u8(10);
                self.i64(*m);
            }
            Value::Timestamptz(m) => {
                self.u8(11);
                self.i64(*m);
            }
            Value::Bytea(b) => {
                self.u8(12);
                self.u32(b.len() as u32);
                self.bytes(b);
            }
            Value::Uuid(u) => {
                self.u8(13);
                self.bytes(u);
            }
            // v0.35: blank-padded char values; tag appends after v0.7's.
            Value::BpChar(s) => {
                self.u8(14);
                self.str(s);
            }
            // v0.36: one-byte "char" values; tag appends after v0.35's.
            Value::SingleChar(b) => {
                self.u8(15);
                self.u8(*b);
            }
            // v0.64: pg_lsn values; tag appends after v0.36's.
            Value::PgLsn(lsn) => {
                self.u8(16);
                self.u64(*lsn);
            }
            // v0.79: real array values. Tag 17, then the PG array OID
            // identifying the element type (same table as col_type tag
            // 22), dims, lower bounds, and each element as a nested
            // value — elements are always scalar, so no cycle.
            Value::Array(a) => {
                self.u8(17);
                self.u32(a.elem.array_oid());
                self.u32(a.dims.len() as u32);
                for d in &a.dims {
                    self.i32(*d);
                }
                for l in &a.lower {
                    self.i32(*l);
                }
                self.u32(a.elems.len() as u32);
                for e in &a.elems {
                    self.value(e);
                }
            }
            // v0.84: composite values (tag 18) — field count, then
            // (name, value) pairs. Reachable for whole-column
            // composite inserts (the v0.73 panic wrongly assumed
            // records never persist) and for composite arrays built
            // by INSERT target indirection.
            Value::Record(fields) => {
                self.u8(18);
                self.u32(fields.len() as u32);
                for (name, val) in fields {
                    self.str(name);
                    self.value(val);
                }
            }
            // v1.39: bit-string values (tag 19) — bit length, then the
            // bytes (the length matters: trailing bits are padding).
            Value::BitString(b) => {
                self.u8(19);
                self.u32(b.bitlen);
                self.u32(b.bytes.len() as u32);
                self.bytes(&b.bytes);
            }
            // v1.40: tid values; tag appends after v1.39's.
            Value::Tid(b, o) => {
                self.u8(20);
                self.u32(*b);
                self.u32(*o);
            }
        }
    }

    pub(crate) fn columns(&mut self, cols: &[(String, ColType)]) {
        self.u32(cols.len() as u32);
        for (name, typ) in cols {
            self.str(name);
            self.col_type(typ);
        }
    }

    pub(crate) fn record(&mut self, r: &WalRecord) {
        match r {
            WalRecord::CreateTable {
                name,
                columns,
                constraints,
                owner,
                acl,
                col_acl,
                oid,
                toast_relid,
                col_compression,
                composite_types,
                domain_types,
                domain_elem,
                inherits,
                attnums,
                next_attnum,
                fillfactor,
                partition,
                xmin,
            } => {
                self.u8(1);
                self.str(name);
                self.columns(columns);
                self.str(constraints);
                self.str(owner);
                self.acl_list(acl);
                self.col_acl_list(col_acl);
                self.u32(*oid);
                self.u32(*toast_relid);
                // v0.41: per-column compression methods.
                self.u32(col_compression.len() as u32);
                for m in col_compression {
                    self.u8(*m);
                }
                // v0.85: composite/domain type use per column.
                self.opt_str_list(composite_types);
                self.opt_str_list(domain_types);
                self.bool_list(domain_elem);
                // v0.96: inheritance parent links.
                self.str_list(inherits);
                // v1.41: attnums (i16 list), next_attnum, fillfactor.
                self.u32(attnums.len() as u32);
                for a in attnums {
                    self.i16(*a);
                }
                self.i16(*next_attnum);
                self.u8(*fillfactor);
                self.u64(*xmin);
                // v1.80 (RGSWAL21): partition metadata, last so an
                // RGSWAL20 record is an exact prefix of this layout.
                self.partition(partition);
            }
            WalRecord::InsertRows { table, rows } => {
                self.u8(2);
                self.str(table);
                self.u32(rows.len() as u32);
                for r in rows {
                    self.wal_row(r);
                }
            }
            WalRecord::DropTable { name, xmax } => {
                self.u8(3);
                self.str(name);
                self.u64(*xmax);
            }
            WalRecord::DeleteRows {
                table,
                ids,
                old_rows,
                xmax,
            } => {
                self.u8(4);
                self.str(table);
                self.u32(ids.len() as u32);
                // v0.13: old values ride alongside the ids (parallel vecs).
                for (id, old) in ids.iter().zip(old_rows.iter()) {
                    self.u64(*id);
                    self.u64(old.xmin);
                    self.u32(old.values.len() as u32);
                    for v in old.values.iter() {
                        self.value(v);
                    }
                    // v0.39: per-cell toast flags, parallel to the values.
                    for i in 0..old.values.len() {
                        self.u32(old.toast.get(i).copied().unwrap_or(0));
                    }
                    // v0.39: (value id, compressed) provenance.
                    self.u32(old.toast_meta.len() as u32);
                    for (k, c) in &old.toast_meta {
                        self.u32(*k);
                        self.u8(*c);
                    }
                }
                self.u64(*xmax);
            }
            // v0.13: UPDATE as one record — old and new row vectors.
            WalRecord::UpdateRows {
                table,
                old,
                new,
                xmax,
            } => {
                self.u8(18);
                self.str(table);
                self.u32(old.len() as u32);
                for r in old {
                    self.wal_row(r);
                }
                self.u32(new.len() as u32);
                for r in new {
                    self.wal_row(r);
                }
                self.u64(*xmax);
            }
            WalRecord::CreateIndex {
                name,
                table,
                columns,
                unique,
                internal,
                desc,
                nulls_first,
                exprs,
                predicate,
                xmin,
            } => {
                self.u8(5);
                self.str(name);
                self.str(table);
                self.u32(columns.len() as u32);
                for c in columns {
                    self.str(c);
                }
                self.u8(*unique as u8);
                self.u8(*internal as u8);
                // v0.88: per-column direction / null placement /
                // expression sources, plus the partial predicate.
                self.bool_list(desc);
                self.bool_list(nulls_first);
                self.opt_str_list(exprs);
                match predicate {
                    Some(p) => {
                        self.u8(1);
                        self.str(p);
                    }
                    None => self.u8(0),
                }
                self.u64(*xmin);
            }
            WalRecord::DropIndex { name, xmax } => {
                self.u8(6);
                self.str(name);
                self.u64(*xmax);
            }
            WalRecord::AlterTable {
                name,
                columns,
                constraints,
                copy_rows,
                owner,
                acl,
                col_acl,
                oid,
                toast_relid,
                toast_target,
                col_storage,
                col_compression,
                composite_types,
                domain_types,
                domain_elem,
                inherits,
                attnums,
                next_attnum,
                fillfactor,
                next_value_id,
                toast_info,
                partition,
                xmin,
            } => {
                self.u8(7);
                self.str(name);
                self.columns(columns);
                self.str(constraints);
                self.u8(*copy_rows as u8);
                self.str(owner);
                self.acl_list(acl);
                self.col_acl_list(col_acl);
                // v0.37: TOAST metadata.
                self.u32(*oid);
                self.u32(*toast_relid);
                self.u32(*toast_target);
                self.u32(col_storage.len() as u32);
                for s in col_storage {
                    self.u8(*s);
                }
                // v0.41: per-column compression methods.
                self.u32(col_compression.len() as u32);
                for m in col_compression {
                    self.u8(*m);
                }
                // v0.85: composite/domain type use per column.
                self.opt_str_list(composite_types);
                self.opt_str_list(domain_types);
                self.bool_list(domain_elem);
                // v0.96: inheritance parent links.
                self.str_list(inherits);
                // v1.41: attnums (i16 list), next_attnum, fillfactor.
                self.u32(attnums.len() as u32);
                for a in attnums {
                    self.i16(*a);
                }
                self.i16(*next_attnum);
                self.u8(*fillfactor);
                self.u32(*next_value_id);
                self.u32(toast_info.len() as u32);
                for (k, c) in toast_info {
                    self.u32(*k);
                    self.u8(*c);
                }
                self.u64(*xmin);
                // v1.80 (RGSWAL21): partition metadata, last (see CreateTable).
                self.partition(partition);
            }
            WalRecord::CreateView {
                name,
                query,
                col_aliases,
                deps,
                owner,
                xmin,
            } => {
                self.u8(8);
                self.str(name);
                self.str(query);
                self.u32(col_aliases.len() as u32);
                for a in col_aliases {
                    self.str(a);
                }
                self.u32(deps.len() as u32);
                for d in deps {
                    self.str(d);
                }
                self.str(owner);
                self.u64(*xmin);
            }
            WalRecord::DropView { name, xmax } => {
                self.u8(9);
                self.str(name);
                self.u64(*xmax);
            }
            WalRecord::CreateSequence { name, seq, xmin } => {
                self.u8(10);
                self.str(name);
                self.sequence(seq);
                self.u64(*xmin);
            }
            WalRecord::DropSequence { name, xmax } => {
                self.u8(11);
                self.str(name);
                self.u64(*xmax);
            }
            WalRecord::AlterSequence { name, seq, xmin } => {
                self.u8(12);
                self.str(name);
                self.sequence(seq);
                self.u64(*xmin);
            }
            WalRecord::SeqAdvance {
                name,
                last_value,
                is_called,
            } => {
                self.u8(13);
                self.str(name);
                self.i64(*last_value);
                self.u8(*is_called as u8);
            }
            WalRecord::CreateRole { name, role, xmin } => {
                self.u8(14);
                self.str(name);
                self.wal_role(role);
                self.u64(*xmin);
            }
            WalRecord::DropRole { name, xmax } => {
                self.u8(15);
                self.str(name);
                self.u64(*xmax);
            }
            WalRecord::AlterRole { name, role, xmin } => {
                self.u8(16);
                self.str(name);
                self.wal_role(role);
                self.u64(*xmin);
            }
            WalRecord::DbAcl { acl, xmin } => {
                self.u8(17);
                self.acl_list(acl);
                self.u64(*xmin);
            }
            // v0.13: replication slot metadata.
            WalRecord::ReplSlotCreate {
                name,
                plugin,
                slot_type,
                restart_lsn,
            } => {
                self.u8(19);
                self.str(name);
                self.str(plugin);
                self.str(slot_type);
                self.u64(*restart_lsn);
            }
            WalRecord::ReplSlotDrop { name } => {
                self.u8(20);
                self.str(name);
            }
            WalRecord::ReplSlotFlush {
                name,
                restart_lsn,
                confirmed_flush_lsn,
            } => {
                self.u8(21);
                self.str(name);
                self.u64(*restart_lsn);
                self.u64(*confirmed_flush_lsn);
            }
            // v0.82: type DDL (tags 22/23).
            WalRecord::CreateType {
                name,
                like_base,
                composite,
                domain,
                xmin,
            } => {
                self.u8(22);
                self.str(name);
                match like_base {
                    Some(b) => {
                        self.u8(1);
                        self.str(b);
                    }
                    None => self.u8(0),
                }
                match composite {
                    Some(fields) => {
                        self.u8(1);
                        self.u32(fields.len() as u32);
                        for (fname, fty, nested) in fields {
                            self.str(fname);
                            self.col_type(fty);
                            match nested {
                                Some(n) => {
                                    self.u8(1);
                                    self.str(n);
                                }
                                None => self.u8(0),
                            }
                        }
                    }
                    None => self.u8(0),
                }
                // v0.85: domain definition (present only for domains).
                match domain {
                    Some(d) => {
                        self.u8(1);
                        self.col_type(&d.base);
                        match &d.base_named {
                            Some(n) => {
                                self.u8(1);
                                self.str(n);
                            }
                            None => self.u8(0),
                        }
                        match &d.base_domain {
                            Some(n) => {
                                self.u8(1);
                                self.str(n);
                            }
                            None => self.u8(0),
                        }
                        self.u8(if d.not_null { 1 } else { 0 });
                        self.str(&d.checks);
                        self.str(&d.default);
                    }
                    None => self.u8(0),
                }
                self.u64(*xmin);
            }
            WalRecord::DropType { name, xmax } => {
                self.u8(23);
                self.str(name);
                self.u64(*xmax);
            }
            // v0.86: function/operator DDL (tags 24/25/26/27).
            WalRecord::CreateFunction {
                name,
                arg_names,
                arg_types,
                ret_type,
                returns_set,
                lang,
                body,
                volatility,
                strict,
                xmin,
            } => {
                self.u8(24);
                self.str(name);
                self.u32(arg_names.len() as u32);
                for n in arg_names {
                    match n {
                        Some(s) => {
                            self.u8(1);
                            self.str(s);
                        }
                        None => self.u8(0),
                    }
                }
                self.u32(arg_types.len() as u32);
                for t in arg_types {
                    self.str(t);
                }
                self.str(ret_type);
                self.u8(if *returns_set { 1 } else { 0 });
                self.u8(*lang);
                self.str(body);
                self.u8(*volatility);
                self.u8(if *strict { 1 } else { 0 });
                self.u64(*xmin);
            }
            WalRecord::DropFunction {
                name,
                arg_types,
                xmax,
            } => {
                self.u8(25);
                self.str(name);
                self.u32(arg_types.len() as u32);
                for t in arg_types {
                    self.str(t);
                }
                self.u64(*xmax);
            }
            WalRecord::CreateOperator { name, defs, xmin } => {
                self.u8(26);
                self.str(name);
                self.u32(defs.len() as u32);
                for d in defs {
                    self.str(&d.procedure);
                    match &d.leftarg {
                        Some(s) => {
                            self.u8(1);
                            self.str(s);
                        }
                        None => self.u8(0),
                    }
                    match &d.rightarg {
                        Some(s) => {
                            self.u8(1);
                            self.str(s);
                        }
                        None => self.u8(0),
                    }
                    match &d.commutator {
                        Some(s) => {
                            self.u8(1);
                            self.str(s);
                        }
                        None => self.u8(0),
                    }
                    match &d.negator {
                        Some(s) => {
                            self.u8(1);
                            self.str(s);
                        }
                        None => self.u8(0),
                    }
                    self.u8(if d.hashes { 1 } else { 0 });
                    self.u8(if d.merges { 1 } else { 0 });
                }
                self.u64(*xmin);
            }
            WalRecord::DropOperator { name, xmax } => {
                self.u8(27);
                self.str(name);
                self.u64(*xmax);
            }
            // v1.05: lazy reltoastrelid link (RGSWAL19).
            WalRecord::SetToastRelid {
                name,
                toast_relid,
                xmin,
            } => {
                self.u8(28);
                self.str(name);
                self.u32(*toast_relid);
                self.u64(*xmin);
            }
        }
    }

    pub(crate) fn acl_list(&mut self, acl: &[WalAcl]) {
        self.u32(acl.len() as u32);
        for e in acl {
            self.str(&e.role);
            self.u32(e.privs);
        }
    }

    /// v0.85: encode a `Vec<Option<String>>` (composite/domain type names).
    pub(crate) fn opt_str_list(&mut self, v: &[Option<String>]) {
        self.u32(v.len() as u32);
        for o in v {
            match o {
                Some(s) => {
                    self.u8(1);
                    self.str(s);
                }
                None => self.u8(0),
            }
        }
    }

    /// v0.85: encode a `Vec<bool>` (domain_elem).
    pub(crate) fn bool_list(&mut self, v: &[bool]) {
        self.u32(v.len() as u32);
        for b in v {
            self.u8(if *b { 1 } else { 0 });
        }
    }

    /// v0.96: encode a `Vec<String>` (inheritance parent links).
    /// v1.80: partition metadata, shared by checkpoint table entries and
    /// the WAL `CreateTable`/`AlterTable` records (one codec, one format).
    /// Layout: presence byte, method, key (col + optional expression),
    /// optional bound, default flag, optional parent, children, and the
    /// v0.72 `is_partitioned` flag.
    pub(crate) fn partition(&mut self, part: &Option<crate::storage::PartitionInfo>) {
        let Some(p) = part else {
            self.u8(0);
            return;
        };
        self.u8(1);
        self.u8(match p.method {
            crate::storage::PartMethod::Range => 0,
            crate::storage::PartMethod::List => 1,
            crate::storage::PartMethod::Hash => 2,
        });
        self.u32(p.key.len() as u32);
        for k in &p.key {
            self.u64(k.col as u64);
            if let Some(e) = &k.expr {
                self.u8(1);
                // v0.69: serialize the key expression as
                // debug string; parsed back on load for the
                // common cases (column, arith, func).
                // FULL support is a gap (see decode below).
                self.str(&format!("{:?}", e));
            } else {
                self.u8(0);
            }
        }
        if let Some(b) = &p.bound {
            self.u8(1);
            match b {
                crate::storage::PartBound::List { values, has_null } => {
                    self.u8(0);
                    self.u32(values.len() as u32);
                    for v in values {
                        self.value(v);
                    }
                    self.u8(if *has_null { 1 } else { 0 });
                }
                crate::storage::PartBound::Range { lower, upper } => {
                    self.u8(1);
                    self.u32(lower.len() as u32);
                    for rb in lower {
                        encode_range_bound(self, rb);
                    }
                    self.u32(upper.len() as u32);
                    for rb in upper {
                        encode_range_bound(self, rb);
                    }
                }
                crate::storage::PartBound::Hash { modulus, remainder } => {
                    self.u8(2);
                    self.u32(*modulus);
                    self.u32(*remainder);
                }
            }
        } else {
            self.u8(0);
        }
        self.u8(if p.is_default { 1 } else { 0 });
        if let Some(par) = &p.parent {
            self.u8(1);
            self.str(par);
        } else {
            self.u8(0);
        }
        self.u32(p.children.len() as u32);
        for c in &p.children {
            self.str(c);
        }
        // v0.72: whether this table is itself partitioned
        // (a childless partitioned table is not a leaf).
        self.u8(if p.is_partitioned { 1 } else { 0 });
    }

    pub(crate) fn str_list(&mut self, v: &[String]) {
        self.u32(v.len() as u32);
        for s in v {
            self.str(s);
        }
    }

    pub(crate) fn col_acl_list(&mut self, acl: &[WalColAcl]) {
        self.u32(acl.len() as u32);
        for e in acl {
            self.str(&e.role);
            self.u32(e.privs);
            self.u32(e.columns.len() as u32);
            for c in &e.columns {
                self.str(c);
            }
        }
    }

    pub(crate) fn verifier(&mut self, v: &Option<WalVerifier>) {
        match v {
            None => self.u8(0),
            Some(v) => {
                self.u8(1);
                self.u32(v.iterations);
                self.u32(v.salt.len() as u32);
                self.bytes(&v.salt);
                self.bytes(&v.stored_key);
                self.bytes(&v.server_key);
            }
        }
    }

    pub(crate) fn wal_role(&mut self, r: &WalRole) {
        self.verifier(&r.password);
        self.u8(r.can_login as u8);
        self.u8(r.superuser as u8);
        self.i32(r.connlimit);
        self.u32(r.memberships.len() as u32);
        for m in &r.memberships {
            self.str(&m.role);
            self.str(&m.grantor);
        }
        match &r.valid_until {
            Some(v) => {
                self.u8(1);
                self.str(v);
            }
            None => self.u8(0),
        }
    }

    pub(crate) fn sequence(&mut self, s: &WalSequence) {
        self.str(&s.name);
        // v0.99: sequence data type.
        self.u8(s.seq_type);
        self.i64(s.start);
        self.i64(s.increment);
        self.i64(s.min_value);
        self.i64(s.max_value);
        self.u8(s.cycle as u8);
        // v0.98: sequence cache size.
        self.i64(s.cache);
        self.i64(s.current);
        self.u8(s.current_is_set as u8);
        self.u8(s.is_called as u8);
        // v0.11
        self.str(&s.owner);
        self.acl_list(&s.acl);
        // v0.65: serial ownership.
        match &s.owned_by {
            None => self.u8(0),
            Some((t, c, sess)) => {
                self.u8(1);
                self.str(t);
                self.str(c);
                match sess {
                    None => self.u8(0),
                    Some(v) => {
                        self.u8(1);
                        self.u64(*v);
                    }
                }
            }
        }
    }
}

pub(crate) struct Dec<'a> {
    pub(crate) buf: &'a [u8],
    pub(crate) pos: usize,
    /// v1.80: WAL format version of the frames being decoded (the number
    /// in the `RGSWALnn` magic). Records gain trailing fields across
    /// versions; decoders read them only when the log is new enough.
    /// Checkpoints and in-memory round trips use the current version.
    pub(crate) wal_version: u32,
}

impl<'a> Dec<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Dec {
            buf,
            pos: 0,
            wal_version: crate::wal::writer::WAL_VERSION,
        }
    }

    /// v1.80: decode frames written under an older WAL format version.
    pub(crate) fn with_wal_version(buf: &'a [u8], wal_version: u32) -> Self {
        Dec {
            buf,
            pos: 0,
            wal_version,
        }
    }

    pub(crate) fn err(&self, what: &str) -> String {
        format!("{} at offset {}", what, self.pos)
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.buf.len() {
            return Err(self.err("truncated value"));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, String> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, String> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub(crate) fn i16(&mut self) -> Result<i16, String> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, String> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn f32(&mut self) -> Result<f32, String> {
        let b = self.take(4)?;
        Ok(f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn f64(&mut self) -> Result<f64, String> {
        let b = self.take(8)?;
        Ok(f64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub(crate) fn str(&mut self) -> Result<String, String> {
        let n = self.u32()? as usize;
        let b = self.take(n)?;
        std::str::from_utf8(b)
            .map(|s| s.to_string())
            .map_err(|_| self.err("invalid utf-8 in string"))
    }

    /// Read a string straight into an `Arc<str>`. This copies the bytes
    /// one time. `str()` then `Value::text()` copies them two times.
    pub(crate) fn str_arc(&mut self) -> Result<std::sync::Arc<str>, String> {
        let n = self.u32()? as usize;
        let b = self.take(n)?;
        match std::str::from_utf8(b) {
            Ok(s) => Ok(s.into()),
            Err(_) => Err(self.err("invalid utf-8 in string")),
        }
    }

    pub(crate) fn col_type(&mut self) -> Result<ColType, String> {
        match self.u8()? {
            0 => Ok(ColType::Int),
            1 => Ok(ColType::Float),
            2 => Ok(ColType::Text),
            3 => Ok(ColType::Bool),
            4 => Ok(ColType::SmallInt),
            5 => Ok(ColType::BigInt),
            6 => Ok(ColType::Float4),
            7 => Ok(ColType::Numeric(None)),
            8 => Ok(ColType::Date),
            9 => Ok(ColType::Timestamp),
            10 => Ok(ColType::Timestamptz),
            11 => Ok(ColType::Bytea),
            12 => Ok(ColType::Uuid),
            // v0.35: typmod stored as i32, -1 = no typmod.
            13 => {
                let n = self.i32()?;
                Ok(ColType::Char(if n < 0 { None } else { Some(n) }))
            }
            14 => {
                let n = self.i32()?;
                Ok(ColType::Varchar(if n < 0 { None } else { Some(n) }))
            }
            // v0.36: the one-byte "char" type.
            15 => Ok(ColType::SingleChar),
            // v0.37: regclass.
            16 => Ok(ColType::Regclass),
            // v0.57: name.
            17 => Ok(ColType::Name),
            // v0.64: pg_lsn.
            19 => Ok(ColType::PgLsn),
            // v0.73: record, json.
            20 => Ok(ColType::Record),
            // v0.81: named composite marker.
            23 => Ok(ColType::Composite),
            // v1.17: xid.
            24 => Ok(ColType::Xid),
            // v1.39: bit.
            25 => Ok(ColType::Bit),
            // v1.40: tid.
            26 => Ok(ColType::Tid),
            21 => Ok(ColType::Json),
            // v0.78: array, then the PG array OID identifying the
            // element type.
            22 => {
                let elem = match self.u32()? {
                    1000 => ArrayElem::Bool,
                    1001 => ArrayElem::Bytea,
                    1002 => ArrayElem::SingleChar,
                    1003 => ArrayElem::Name,
                    1005 => ArrayElem::SmallInt,
                    1007 => ArrayElem::Int,
                    1009 => ArrayElem::Text,
                    1014 => ArrayElem::Char,
                    1015 => ArrayElem::Varchar,
                    1016 => ArrayElem::BigInt,
                    1021 => ArrayElem::Float4,
                    1022 => ArrayElem::Float,
                    1182 => ArrayElem::Date,
                    1115 => ArrayElem::Timestamp,
                    1185 => ArrayElem::Timestamptz,
                    1231 => ArrayElem::Numeric,
                    2951 => ArrayElem::Uuid,
                    2206 => ArrayElem::Regclass,
                    199 => ArrayElem::Json,
                    2287 => ArrayElem::Record,
                    3221 => ArrayElem::PgLsn,
                    1011 => ArrayElem::Xid,
                    // v1.39: _bit.
                    1561 => ArrayElem::Bit,
                    t => return Err(self.err(&format!("unknown array element OID {}", t))),
                };
                Ok(ColType::Array(elem))
            }
            // v0.60: numeric with typmod (precision, scale).
            18 => {
                let p = self.i32()?;
                let s = self.i32()?;
                Ok(ColType::Numeric(Some((p as u32, s))))
            }
            t => Err(self.err(&format!("unknown column type {}", t))),
        }
    }

    pub(crate) fn value(&mut self) -> Result<Value, String> {
        match self.u8()? {
            0 => Ok(Value::Null),
            1 => Ok(Value::Int(self.i64()?)),
            2 => Ok(Value::Float(self.f64()?)),
            3 => Ok(Value::Text(self.str_arc()?)),
            4 => Ok(Value::Bool(self.u8()? != 0)),
            5 => Ok(Value::SmallInt(self.i16()?)),
            6 => Ok(Value::BigInt(self.i64()?)),
            7 => Ok(Value::Float4(self.f32()?)),
            8 => {
                let s = self.str()?;
                crate::storage::Numeric::parse(&s)
                    .map(Value::Numeric)
                    .map_err(|e| self.err(&format!("bad numeric in WAL: {:?}", e)))
            }
            9 => Ok(Value::Date(self.i32()?)),
            10 => Ok(Value::Timestamp(self.i64()?)),
            11 => Ok(Value::Timestamptz(self.i64()?)),
            12 => {
                let n = self.u32()? as usize;
                Ok(Value::Bytea(self.take(n)?.to_vec()))
            }
            13 => {
                let b = self.take(16)?;
                let mut u = [0u8; 16];
                u.copy_from_slice(b);
                Ok(Value::Uuid(u))
            }
            // v0.35: blank-padded char values.
            14 => Ok(Value::BpChar(self.str()?.into())),
            // v0.36: one-byte "char" values.
            15 => Ok(Value::SingleChar(self.u8()?)),
            // v0.64: pg_lsn.
            16 => Ok(Value::PgLsn(self.u64()?)),
            // v1.40: tid.
            20 => Ok(Value::Tid(self.u32()?, self.u32()?)),
            // v0.79: real array values (tag 17; mirrors the encoder).
            17 => {
                let elem = self.array_elem()?;
                let ndim = self.u32()? as usize;
                let mut dims = Vec::with_capacity(ndim);
                for _ in 0..ndim {
                    dims.push(self.i32()?);
                }
                let mut lower = Vec::with_capacity(ndim);
                for _ in 0..ndim {
                    lower.push(self.i32()?);
                }
                let nelems = self.u32()? as usize;
                let mut elems = Vec::with_capacity(nelems);
                for _ in 0..nelems {
                    elems.push(self.value()?);
                }
                Ok(Value::Array(Box::new(ArrayVal {
                    elem,
                    dims,
                    lower,
                    elems,
                })))
            }
            // v0.84: composite values (tag 18; mirrors the encoder).
            18 => {
                let n = self.u32()? as usize;
                let mut fields = Vec::with_capacity(n);
                for _ in 0..n {
                    let name = self.str()?;
                    let val = self.value()?;
                    fields.push((name, val));
                }
                Ok(Value::Record(fields))
            }
            // v1.39: bit-string values (tag 19; mirrors the encoder).
            19 => {
                let bitlen = self.u32()?;
                let n = self.u32()? as usize;
                let bytes = self.take(n)?.to_vec();
                Ok(Value::BitString(crate::storage::BitString {
                    bitlen,
                    bytes,
                }))
            }
            t => Err(self.err(&format!("unknown value tag {}", t))),
        }
    }

    /// v0.79: decode the PG array OID used by both `col_type` tag 22
    /// and `value` tag 17 into the element type.
    pub(crate) fn array_elem(&mut self) -> Result<ArrayElem, String> {
        match self.u32()? {
            1000 => Ok(ArrayElem::Bool),
            1001 => Ok(ArrayElem::Bytea),
            1002 => Ok(ArrayElem::SingleChar),
            1003 => Ok(ArrayElem::Name),
            1005 => Ok(ArrayElem::SmallInt),
            1007 => Ok(ArrayElem::Int),
            1009 => Ok(ArrayElem::Text),
            1014 => Ok(ArrayElem::Char),
            1015 => Ok(ArrayElem::Varchar),
            1016 => Ok(ArrayElem::BigInt),
            1021 => Ok(ArrayElem::Float4),
            1022 => Ok(ArrayElem::Float),
            1182 => Ok(ArrayElem::Date),
            1115 => Ok(ArrayElem::Timestamp),
            1185 => Ok(ArrayElem::Timestamptz),
            1231 => Ok(ArrayElem::Numeric),
            2951 => Ok(ArrayElem::Uuid),
            2206 => Ok(ArrayElem::Regclass),
            199 => Ok(ArrayElem::Json),
            2287 => Ok(ArrayElem::Record),
            3221 => Ok(ArrayElem::PgLsn),
            // v1.39: _bit.
            1561 => Ok(ArrayElem::Bit),
            // v1.40: _tid.
            1010 => Ok(ArrayElem::Tid),
            t => Err(self.err(&format!("unknown array element OID {}", t))),
        }
    }

    pub(crate) fn columns(&mut self) -> Result<Vec<(String, ColType)>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.str()?;
            let typ = self.col_type()?;
            out.push((name, typ));
        }
        Ok(out)
    }

    /// Decode one WAL row: id, xmin, value count, values.
    pub(crate) fn wal_row(&mut self) -> Result<WalRow, String> {
        let id = self.u64()?;
        let xmin = self.u64()?;
        let nv = self.u32()? as usize;
        let mut values = Vec::with_capacity(nv);
        for _ in 0..nv {
            values.push(self.value()?);
        }
        // v0.39: per-cell toast flags, parallel to the values.
        let mut toast = Vec::with_capacity(nv);
        for _ in 0..nv {
            toast.push(self.u32()?);
        }
        // v0.39: (value id, compressed) provenance for value ids
        // introduced by this row.
        let nm = self.u32()? as usize;
        let mut toast_meta = Vec::with_capacity(nm);
        for _ in 0..nm {
            toast_meta.push((self.u32()?, self.u8()?));
        }
        Ok(WalRow {
            id,
            xmin,
            values: Row::new(values),
            toast,
            toast_meta,
        })
    }

    pub(crate) fn record(&mut self) -> Result<WalRecord, String> {
        match self.u8()? {
            1 => {
                let name = self.str()?;
                let columns = self.columns()?;
                let constraints = self.str()?;
                // v0.11
                let owner = self.str()?;
                let acl = self.acl_list()?;
                let col_acl = self.col_acl_list()?;
                // v0.37: oid, toast_relid precede xmin, matching encode order.
                let oid = self.u32()?;
                let toast_relid = self.u32()?;
                // v0.41: per-column compression methods.
                let n_compression = self.u32()? as usize;
                let mut col_compression = Vec::with_capacity(n_compression);
                for _ in 0..n_compression {
                    col_compression.push(self.u8()?);
                }
                // v0.85: composite/domain type use per column.
                let composite_types = self.opt_str_list_d()?;
                let domain_types = self.opt_str_list_d()?;
                let domain_elem = self.bool_list_d()?;
                // v0.96: inheritance parent links.
                let inherits = self.str_list_d()?;
                // v1.41: attnums (i16 list), next_attnum, fillfactor.
                let n_attnums = self.u32()? as usize;
                let mut attnums = Vec::with_capacity(n_attnums);
                for _ in 0..n_attnums {
                    attnums.push(self.i16()?);
                }
                let next_attnum = self.i16()?;
                let fillfactor = self.u8()?;
                let xmin = self.u64()?;
                // v1.80 (RGSWAL21): trailing partition metadata.
                let partition = if self.wal_version >= 21 {
                    self.partition()?
                } else {
                    None
                };
                Ok(WalRecord::CreateTable {
                    name,
                    columns,
                    constraints,
                    owner,
                    acl,
                    col_acl,
                    oid,
                    toast_relid,
                    col_compression,
                    composite_types,
                    domain_types,
                    domain_elem,
                    inherits,
                    attnums,
                    next_attnum,
                    fillfactor,
                    partition,
                    xmin,
                })
            }
            2 => {
                let table = self.str()?;
                let n = self.u32()? as usize;
                let mut rows = Vec::with_capacity(n);
                for _ in 0..n {
                    rows.push(self.wal_row()?);
                }
                Ok(WalRecord::InsertRows { table, rows })
            }
            3 => Ok(WalRecord::DropTable {
                name: self.str()?,
                xmax: self.u64()?,
            }),
            4 => {
                let table = self.str()?;
                let n = self.u32()? as usize;
                let mut ids = Vec::with_capacity(n);
                let mut old_rows = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = self.u64()?;
                    let xmin = self.u64()?;
                    let nv = self.u32()? as usize;
                    let mut values = Vec::with_capacity(nv);
                    for _ in 0..nv {
                        values.push(self.value()?);
                    }
                    // v0.39: per-cell toast flags, parallel to the values.
                    let mut toast = Vec::with_capacity(nv);
                    for _ in 0..nv {
                        toast.push(self.u32()?);
                    }
                    // v0.39: (value id, compressed) provenance.
                    let nm = self.u32()? as usize;
                    let mut toast_meta = Vec::with_capacity(nm);
                    for _ in 0..nm {
                        toast_meta.push((self.u32()?, self.u8()?));
                    }
                    ids.push(id);
                    old_rows.push(WalRow {
                        id,
                        xmin,
                        values: Row::new(values),
                        toast,
                        toast_meta,
                    });
                }
                let xmax = self.u64()?;
                Ok(WalRecord::DeleteRows {
                    table,
                    ids,
                    old_rows,
                    xmax,
                })
            }
            5 => {
                let name = self.str()?;
                let table = self.str()?;
                let n = self.u32()? as usize;
                let mut columns = Vec::with_capacity(n);
                for _ in 0..n {
                    columns.push(self.str()?);
                }
                let unique = self.u8()? != 0;
                let internal = self.u8()? != 0;
                // v0.88 (RGSWAL15): per-column direction / null
                // placement / expression sources, partial predicate.
                let desc = self.bool_list_d()?;
                let nulls_first = self.bool_list_d()?;
                let exprs = self.opt_str_list_d()?;
                let has_predicate = self.u8()? != 0;
                let predicate = if has_predicate {
                    Some(self.str()?)
                } else {
                    None
                };
                let xmin = self.u64()?;
                Ok(WalRecord::CreateIndex {
                    name,
                    table,
                    columns,
                    unique,
                    internal,
                    desc,
                    nulls_first,
                    exprs,
                    predicate,
                    xmin,
                })
            }
            6 => {
                let name = self.str()?;
                let xmax = self.u64()?;
                Ok(WalRecord::DropIndex { name, xmax })
            }
            7 => {
                let name = self.str()?;
                let columns = self.columns()?;
                let constraints = self.str()?;
                let copy_rows = self.u8()? != 0;
                // v0.11
                let owner = self.str()?;
                let acl = self.acl_list()?;
                let col_acl = self.col_acl_list()?;
                // v0.37: TOAST metadata.
                let oid = self.u32()?;
                let toast_relid = self.u32()?;
                let toast_target = self.u32()?;
                let n_storage = self.u32()? as usize;
                let mut col_storage = Vec::with_capacity(n_storage);
                for _ in 0..n_storage {
                    col_storage.push(self.u8()?);
                }
                // v0.41: per-column compression methods.
                let n_compression = self.u32()? as usize;
                let mut col_compression = Vec::with_capacity(n_compression);
                for _ in 0..n_compression {
                    col_compression.push(self.u8()?);
                }
                // v0.85: composite/domain type use per column.
                let composite_types = self.opt_str_list_d()?;
                let domain_types = self.opt_str_list_d()?;
                let domain_elem = self.bool_list_d()?;
                // v0.96: inheritance parent links.
                let inherits = self.str_list_d()?;
                // v1.41: attnums (i16 list), next_attnum, fillfactor.
                let n_attnums = self.u32()? as usize;
                let mut attnums = Vec::with_capacity(n_attnums);
                for _ in 0..n_attnums {
                    attnums.push(self.i16()?);
                }
                let next_attnum = self.i16()?;
                let fillfactor = self.u8()?;
                let next_value_id = self.u32()?;
                let n_ti = self.u32()? as usize;
                let mut toast_info = Vec::with_capacity(n_ti);
                for _ in 0..n_ti {
                    toast_info.push((self.u32()?, self.u8()?));
                }
                let xmin = self.u64()?;
                // v1.80 (RGSWAL21): trailing partition metadata.
                let partition = if self.wal_version >= 21 {
                    self.partition()?
                } else {
                    None
                };
                Ok(WalRecord::AlterTable {
                    name,
                    columns,
                    constraints,
                    copy_rows,
                    owner,
                    acl,
                    col_acl,
                    oid,
                    toast_relid,
                    toast_target,
                    col_storage,
                    col_compression,
                    composite_types,
                    domain_types,
                    domain_elem,
                    inherits,
                    attnums,
                    next_attnum,
                    fillfactor,
                    next_value_id,
                    toast_info,
                    partition,
                    xmin,
                })
            }
            8 => {
                let name = self.str()?;
                let query = self.str()?;
                let n = self.u32()? as usize;
                let mut col_aliases = Vec::with_capacity(n);
                for _ in 0..n {
                    col_aliases.push(self.str()?);
                }
                let m = self.u32()? as usize;
                let mut deps = Vec::with_capacity(m);
                for _ in 0..m {
                    deps.push(self.str()?);
                }
                // v0.11
                let owner = self.str()?;
                let xmin = self.u64()?;
                Ok(WalRecord::CreateView {
                    name,
                    query,
                    col_aliases,
                    deps,
                    owner,
                    xmin,
                })
            }
            9 => Ok(WalRecord::DropView {
                name: self.str()?,
                xmax: self.u64()?,
            }),
            10 => {
                let name = self.str()?;
                let seq = self.sequence()?;
                let xmin = self.u64()?;
                Ok(WalRecord::CreateSequence { name, seq, xmin })
            }
            11 => Ok(WalRecord::DropSequence {
                name: self.str()?,
                xmax: self.u64()?,
            }),
            12 => {
                let name = self.str()?;
                let seq = self.sequence()?;
                let xmin = self.u64()?;
                Ok(WalRecord::AlterSequence { name, seq, xmin })
            }
            13 => Ok(WalRecord::SeqAdvance {
                name: self.str()?,
                last_value: self.i64()?,
                is_called: self.u8()? != 0,
            }),
            14 => {
                let name = self.str()?;
                let role = self.wal_role()?;
                let xmin = self.u64()?;
                Ok(WalRecord::CreateRole { name, role, xmin })
            }
            15 => Ok(WalRecord::DropRole {
                name: self.str()?,
                xmax: self.u64()?,
            }),
            16 => {
                let name = self.str()?;
                let role = self.wal_role()?;
                let xmin = self.u64()?;
                Ok(WalRecord::AlterRole { name, role, xmin })
            }
            17 => {
                let acl = self.acl_list()?;
                let xmin = self.u64()?;
                Ok(WalRecord::DbAcl { acl, xmin })
            }
            18 => {
                let table = self.str()?;
                let no = self.u32()? as usize;
                let mut old = Vec::with_capacity(no);
                for _ in 0..no {
                    old.push(self.wal_row()?);
                }
                let nn = self.u32()? as usize;
                let mut new = Vec::with_capacity(nn);
                for _ in 0..nn {
                    new.push(self.wal_row()?);
                }
                let xmax = self.u64()?;
                Ok(WalRecord::UpdateRows {
                    table,
                    old,
                    new,
                    xmax,
                })
            }
            19 => {
                let name = self.str()?;
                let plugin = self.str()?;
                let slot_type = self.str()?;
                let restart_lsn = self.u64()?;
                Ok(WalRecord::ReplSlotCreate {
                    name,
                    plugin,
                    slot_type,
                    restart_lsn,
                })
            }
            20 => {
                let name = self.str()?;
                Ok(WalRecord::ReplSlotDrop { name })
            }
            21 => {
                let name = self.str()?;
                let restart_lsn = self.u64()?;
                let confirmed_flush_lsn = self.u64()?;
                Ok(WalRecord::ReplSlotFlush {
                    name,
                    restart_lsn,
                    confirmed_flush_lsn,
                })
            }
            // v0.82: type DDL.
            22 => {
                let name = self.str()?;
                let like_base = if self.u8()? != 0 {
                    Some(self.str()?)
                } else {
                    None
                };
                let composite = if self.u8()? != 0 {
                    let n = self.u32()? as usize;
                    let mut fields = Vec::with_capacity(n);
                    for _ in 0..n {
                        let fname = self.str()?;
                        let fty = self.col_type()?;
                        let nested = if self.u8()? != 0 {
                            Some(self.str()?)
                        } else {
                            None
                        };
                        fields.push((fname, fty, nested));
                    }
                    Some(fields)
                } else {
                    None
                };
                // v0.85: domain definition.
                let domain = if self.u8()? != 0 {
                    let base = self.col_type()?;
                    let base_named = if self.u8()? != 0 {
                        Some(self.str()?)
                    } else {
                        None
                    };
                    let base_domain = if self.u8()? != 0 {
                        Some(self.str()?)
                    } else {
                        None
                    };
                    let not_null = self.u8()? != 0;
                    let checks = self.str()?;
                    let default = self.str()?;
                    Some(WalDomain {
                        base,
                        base_named,
                        base_domain,
                        checks,
                        not_null,
                        default,
                    })
                } else {
                    None
                };
                let xmin = self.u64()?;
                Ok(WalRecord::CreateType {
                    name,
                    like_base,
                    composite,
                    domain,
                    xmin,
                })
            }
            23 => {
                let name = self.str()?;
                let xmax = self.u64()?;
                Ok(WalRecord::DropType { name, xmax })
            }
            // v0.86: function/operator DDL.
            24 => {
                let name = self.str()?;
                let n = self.u32()? as usize;
                let mut arg_names = Vec::with_capacity(n);
                for _ in 0..n {
                    arg_names.push(if self.u8()? != 0 {
                        Some(self.str()?)
                    } else {
                        None
                    });
                }
                let n = self.u32()? as usize;
                let mut arg_types = Vec::with_capacity(n);
                for _ in 0..n {
                    arg_types.push(self.str()?);
                }
                let ret_type = self.str()?;
                let returns_set = self.u8()? != 0;
                let lang = self.u8()?;
                let body = self.str()?;
                let volatility = self.u8()?;
                let strict = self.u8()? != 0;
                let xmin = self.u64()?;
                Ok(WalRecord::CreateFunction {
                    name,
                    arg_names,
                    arg_types,
                    ret_type,
                    returns_set,
                    lang,
                    body,
                    volatility,
                    strict,
                    xmin,
                })
            }
            25 => {
                let name = self.str()?;
                let n_args = self.u32()? as usize;
                let mut arg_types = Vec::with_capacity(n_args);
                for _ in 0..n_args {
                    arg_types.push(self.str()?);
                }
                let xmax = self.u64()?;
                Ok(WalRecord::DropFunction {
                    name,
                    arg_types,
                    xmax,
                })
            }
            26 => {
                let name = self.str()?;
                let n = self.u32()? as usize;
                let mut defs = Vec::with_capacity(n);
                for _ in 0..n {
                    let procedure = self.str()?;
                    let leftarg = if self.u8()? != 0 {
                        Some(self.str()?)
                    } else {
                        None
                    };
                    let rightarg = if self.u8()? != 0 {
                        Some(self.str()?)
                    } else {
                        None
                    };
                    let commutator = if self.u8()? != 0 {
                        Some(self.str()?)
                    } else {
                        None
                    };
                    let negator = if self.u8()? != 0 {
                        Some(self.str()?)
                    } else {
                        None
                    };
                    let hashes = self.u8()? != 0;
                    let merges = self.u8()? != 0;
                    defs.push(WalOperDef {
                        procedure,
                        leftarg,
                        rightarg,
                        commutator,
                        negator,
                        hashes,
                        merges,
                    });
                }
                let xmin = self.u64()?;
                Ok(WalRecord::CreateOperator { name, defs, xmin })
            }
            27 => {
                let name = self.str()?;
                let xmax = self.u64()?;
                Ok(WalRecord::DropOperator { name, xmax })
            }
            // v1.05: lazy reltoastrelid link (RGSWAL19).
            28 => {
                let name = self.str()?;
                let toast_relid = self.u32()?;
                let xmin = self.u64()?;
                Ok(WalRecord::SetToastRelid {
                    name,
                    toast_relid,
                    xmin,
                })
            }
            t => Err(self.err(&format!("unknown record tag {}", t))),
        }
    }

    pub(crate) fn sequence(&mut self) -> Result<WalSequence, String> {
        let name = self.str()?;
        // v0.99: sequence data type.
        let seq_type = self.u8()?;
        let start = self.i64()?;
        let increment = self.i64()?;
        let min_value = self.i64()?;
        let max_value = self.i64()?;
        let cycle = self.u8()? != 0;
        // v0.98: sequence cache size.
        let cache = self.i64()?;
        let current = self.i64()?;
        let current_is_set = self.u8()? != 0;
        let is_called = self.u8()? != 0;
        // v0.11
        let owner = self.str()?;
        let acl = self.acl_list()?;
        // v0.65: serial ownership (written last by the encoder).
        let owned_by = if self.u8()? != 0 {
            let t = self.str()?;
            let c = self.str()?;
            let sess = if self.u8()? != 0 {
                Some(self.u64()?)
            } else {
                None
            };
            Some((t, c, sess))
        } else {
            None
        };
        Ok(WalSequence {
            name,
            seq_type,
            start,
            increment,
            min_value,
            max_value,
            cycle,
            cache,
            current,
            current_is_set,
            is_called,
            owner,
            acl,
            owned_by,
        })
    }

    pub(crate) fn acl_list(&mut self) -> Result<Vec<WalAcl>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(WalAcl {
                role: self.str()?,
                privs: self.u32()?,
            });
        }
        Ok(out)
    }

    /// v0.85: decode a `Vec<Option<String>>` (composite/domain type names).
    pub(crate) fn opt_str_list_d(&mut self) -> Result<Vec<Option<String>>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(if self.u8()? != 0 {
                Some(self.str()?)
            } else {
                None
            });
        }
        Ok(out)
    }

    /// v0.85: decode a `Vec<bool>` (domain_elem).
    pub(crate) fn bool_list_d(&mut self) -> Result<Vec<bool>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.u8()? != 0);
        }
        Ok(out)
    }

    /// v0.96: decode a `Vec<String>` (inheritance parent links).
    /// v1.80: decode [`Enc::partition`].
    pub(crate) fn partition(&mut self) -> Result<Option<crate::storage::PartitionInfo>, String> {
        let has_partition = self.u8()? != 0;
        if !has_partition {
            return Ok(None);
        }
        {
            let method_tag = self.u8()?;
            let method = match method_tag {
                0 => crate::storage::PartMethod::Range,
                1 => crate::storage::PartMethod::List,
                2 => crate::storage::PartMethod::Hash,
                _ => return Err(self.err("bad partition method tag")),
            };
            let n_keys = self.u32()? as usize;
            let mut key = Vec::with_capacity(n_keys);
            for _ in 0..n_keys {
                let col = self.u64()? as usize;
                let has_expr = self.u8()? != 0;
                let expr = if has_expr {
                    let s = self.str()?;
                    // v0.69: parse the debug-format expression back.
                    // Only the common cases are supported; others become
                    // None (routing will fail — documented gap).
                    parse_partition_expr(&s)
                } else {
                    None
                };
                key.push(crate::storage::PartKey { col, expr });
            }
            let has_bound = self.u8()? != 0;
            let bound = if has_bound {
                let bound_tag = self.u8()?;
                match bound_tag {
                    0 => {
                        let n_vals = self.u32()? as usize;
                        let mut values = Vec::with_capacity(n_vals);
                        for _ in 0..n_vals {
                            values.push(self.value()?);
                        }
                        let has_null = self.u8()? != 0;
                        Some(crate::storage::PartBound::List { values, has_null })
                    }
                    1 => {
                        let n_lo = self.u32()? as usize;
                        let mut lower = Vec::with_capacity(n_lo);
                        for _ in 0..n_lo {
                            lower.push(decode_range_bound(self)?);
                        }
                        let n_hi = self.u32()? as usize;
                        let mut upper = Vec::with_capacity(n_hi);
                        for _ in 0..n_hi {
                            upper.push(decode_range_bound(self)?);
                        }
                        Some(crate::storage::PartBound::Range { lower, upper })
                    }
                    2 => {
                        let modulus = self.u32()?;
                        let remainder = self.u32()?;
                        Some(crate::storage::PartBound::Hash { modulus, remainder })
                    }
                    _ => return Err(self.err("bad partition bound tag")),
                }
            } else {
                None
            };
            let is_default = self.u8()? != 0;
            let has_parent = self.u8()? != 0;
            let parent = if has_parent { Some(self.str()?) } else { None };
            let n_children = self.u32()? as usize;
            let mut children = Vec::with_capacity(n_children);
            for _ in 0..n_children {
                children.push(self.str()?);
            }
            // v0.72: the is_partitioned flag (format version 11).
            let is_partitioned = self.u8()? != 0;
            Ok(Some(crate::storage::PartitionInfo {
                method,
                key,
                bound,
                is_default,
                parent,
                children,
                is_partitioned,
            }))
        }
    }

    pub(crate) fn str_list_d(&mut self) -> Result<Vec<String>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.str()?);
        }
        Ok(out)
    }

    pub(crate) fn col_acl_list(&mut self) -> Result<Vec<WalColAcl>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let role = self.str()?;
            let privs = self.u32()?;
            let m = self.u32()? as usize;
            let mut columns = Vec::with_capacity(m);
            for _ in 0..m {
                columns.push(self.str()?);
            }
            out.push(WalColAcl {
                role,
                privs,
                columns,
            });
        }
        Ok(out)
    }

    pub(crate) fn verifier(&mut self) -> Result<Option<WalVerifier>, String> {
        match self.u8()? {
            0 => Ok(None),
            1 => {
                let iterations = self.u32()?;
                let salt_len = self.u32()? as usize;
                let salt = self.take(salt_len)?.to_vec();
                let stored_key: [u8; 32] = self
                    .take(32)?
                    .try_into()
                    .map_err(|_| self.err("bad stored key"))?;
                let server_key: [u8; 32] = self
                    .take(32)?
                    .try_into()
                    .map_err(|_| self.err("bad server key"))?;
                Ok(Some(WalVerifier {
                    iterations,
                    salt,
                    stored_key,
                    server_key,
                }))
            }
            b => Err(self.err(&format!("bad verifier tag {}", b))),
        }
    }

    pub(crate) fn wal_role(&mut self) -> Result<WalRole, String> {
        let password = self.verifier()?;
        let can_login = self.u8()? != 0;
        let superuser = self.u8()? != 0;
        let connlimit = self.i32()?;
        let n_members = self.u32()? as usize;
        let mut memberships = Vec::with_capacity(n_members);
        for _ in 0..n_members {
            memberships.push(WalMembership {
                role: self.str()?,
                grantor: self.str()?,
            });
        }
        let valid_until = if self.u8()? != 0 {
            Some(self.str()?)
        } else {
            None
        };
        Ok(WalRole {
            password,
            can_login,
            superuser,
            connlimit,
            memberships,
            valid_until,
        })
    }

    pub(crate) fn end(&self) -> Result<(), String> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(self.err("trailing bytes"))
        }
    }
}
