//! Write-ahead log, checkpoints, and crash recovery (v0.4; v0.5 record format).
//!
//! # Design: logical row-version WAL
//!
//! The storage engine is MVCC: tables hold `RowVersion`s carrying xmin/xmax.
//! The WAL records *logical* changes at the granularity the engine actually
//! mutates, with the version metadata needed to rebuild identical version
//! chains on recovery:
//!
//! - `CreateTable { name, columns, xmin }` — a table was created
//! - `InsertRows { table, rows }` — row versions were appended; each row
//!   carries its stable `id`, `xmin`, and values
//! - `DropTable { name, xmax }` — a table was dropped
//! - `DeleteRows { table, ids, xmax }` — row versions were marked deleted
//!   (covers DELETE and the delete half of UPDATE)
//!
//! Records are produced from the committing transaction's write log, in
//! order, so replay rebuilds exactly the published version chains with
//! identical xmin/xmax — and therefore identical visibility.
//!
//! Format version 4 (`RGSWAL04` / `RGSCHK04`) is NOT compatible with v0.7
//! files: v0.8 refuses to start on a v0.7 data directory with a clear
//! error instead of misreading it. v0.8 adds index DDL records and
//! serializes index definitions + entries in checkpoints.
//!
//! Records are grouped into per-commit *batches*. A batch is one
//! length-prefixed, CRC32-checked frame:
//!
//! ```text
//! u32 frame_len | u64 txn_id | u32 nrecords | records... | u32 crc32
//! ```
//!
//! `frame_len` counts every byte after itself (txn_id through crc
//! inclusive). The CRC covers txn_id + nrecords + records. Recovery stops
//! at the first frame that fails to decode — a torn tail from a crash
//! mid-`write_all` can only ever be a short/truncated frame, so whole
//! committed batches are replayed atomically and partial ones are
//! dropped. Uncommitted transactions never reach the WAL at all: their
//! changes live in the session-private working copy and are diffed into
//! logical records only at COMMIT time.
//!
//! # Why diff-at-commit
//!
//! v0.3 transactions commit by swapping the session's whole working copy
//! into the shared database (last-writer-wins). The WAL must reproduce
//! *exactly what the server published*, so at COMMIT we diff the
//! pre-commit database against the working copy and log the delta. This
//! is provably consistent under concurrency: each commit's delta is
//! relative to the then-current committed state, so replaying the deltas
//! in order reproduces every published state — including the case where a
//! transaction's working copy predates another transaction's commit (the
//! diff degrades to a `FullTable` image, which overwrites on replay just
//! as the swap overwrote in memory).
//!
//! Autocommit statements are their own implicit transactions: the WAL
//! records are derived from the statement itself while the database lock
//! is held (INSERT logs the appended row suffix, CREATE/DROP log their
//! schema/name), which avoids a full-database clone per statement.
//!
//! # Checkpoints and WAL generations
//!
//! `checkpoint.dat` holds a full database image plus the *logical* WAL
//! offset it covers:
//!
//! ```text
//! "RGSCHK01" | u32 version=1 | u64 wal_end | <database image>
//! ```
//!
//! `wal.log` always begins with a 16-byte header:
//!
//! ```text
//! "RGSWAL01" | u64 base_lsn
//! ```
//!
//! A frame's logical sequence number is
//! `base_lsn + (physical_offset_of_frame - 16)`. Checkpointing writes to
//! `checkpoint.dat.tmp`, fsyncs it, atomically renames it over
//! `checkpoint.dat`, fsyncs the directory, then *resets* `wal.log` to a
//! fresh header whose `base_lsn` equals the checkpoint's `wal_end`.
//!
//! Why logical offsets instead of physical ones: the reset truncates the
//! file, so post-checkpoint physical offsets restart at 16 while a stored
//! pre-checkpoint physical offset keeps its old (large) value —
//! comparing the two would skip or misread the new generation's frames.
//! Logical LSNs survive the truncation: recovery replays exactly the
//! frames with `lsn >= wal_end` and skips older ones (a crash between
//! the checkpoint rename and the WAL reset leaves the old generation in
//! place; its frames are `lsn < wal_end` and are skipped rather than
//! re-applied, which matters because `InsertRows` is not idempotent).
//! A crash *during* the WAL reset leaves an empty file or a torn header;
//! open detects both and starts a fresh generation with
//! `base_lsn = wal_end`, which loses nothing because the snapshot covers
//! everything before `wal_end` and no frames newer than it can exist yet
//! (the reset runs under the database lock, so no commit can interleave).
//!
//! The database image is `u32 n_tables`, then per table: name, columns,
//! rows (same value encoding as WAL records).
//!
//! # fsync discipline
//!
//! - COMMIT path (`append_batch`): `write_all` the frame, then
//!   `sync_all` (fsync) on `wal.log`, *then* publish to the in-memory
//!   database and return. COMMIT never returns before its records are
//!   durable. One fsync per commit; no group commit in v0.5 (measured
//!   honestly in benches/BASELINE.md).
//! - Checkpoint: fsync the tmp image file, atomic rename, fsync the
//!   data-directory fd (so the rename itself is durable), then reset
//!   `wal.log` to a fresh 16-byte header (`base_lsn` = checkpoint's
//!   logical end) and fsync it.
//! - Reads, SELECTs, ROLLBACKs: no fsync, nothing durable to record.
//!   (ROLLBACK needs no WAL undo because uncommitted data was never logged.)

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::index::{Index, IndexDef};
use crate::storage::{ColType, Engine, RowVersion, Table, Value, WriteOp};

const WAL_NAME: &str = "wal.log";
const CHKPT_NAME: &str = "checkpoint.dat";
const CHKPT_TMP: &str = "checkpoint.dat.tmp";
const CHKPT_MAGIC: &[u8; 8] = b"RGSCHK04";
const CHKPT_VERSION: u32 = 4;
/// WAL file header: magic + base_lsn (u64, big-endian). Every frame's
/// logical sequence number is base_lsn + (physical offset - HEADER_LEN).
const WAL_MAGIC: &[u8; 8] = b"RGSWAL04";
const WAL_HEADER_LEN: u64 = 16;

/// Encode a WAL file header for a generation starting at `base_lsn`.
fn encode_wal_header(base_lsn: u64) -> [u8; 16] {
    let mut hdr = [0u8; 16];
    hdr[..8].copy_from_slice(WAL_MAGIC);
    hdr[8..].copy_from_slice(&base_lsn.to_be_bytes());
    hdr
}

/// Read the WAL header. Returns `Ok(None)` when the file is empty or the
/// header is torn (short file) — both mean "start a fresh generation";
/// see the module docs for why that is safe. A complete header with the
/// wrong magic is an error (incompatible format, e.g. v0.4 data).
fn read_wal_header(file: &mut File) -> std::io::Result<Option<u64>> {
    let mut hdr = [0u8; 16];
    file.seek(SeekFrom::Start(0))?;
    let mut got = 0;
    while got < 16 {
        match file.read(&mut hdr[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => return Err(e),
        }
    }
    if got == 0 {
        return Ok(None); // fresh data dir
    }
    if got < 16 {
        return Ok(None); // torn header: crash during the WAL reset
    }
    // A full 16-byte header with the wrong magic (e.g. a v0.6 `RGSWAL02`
    // file, whose record format is incompatible) is a loud error:
    // silently treating it as empty would lose data.
    if &hdr[..8] != WAL_MAGIC {
        return Err(io_err(
            "wal.log has an unrecognized magic; rustgres v0.7 cannot read v0.6 data - remove the data directory".to_string(),
        ));
    }
    Ok(Some(u64::from_be_bytes(hdr[8..].try_into().unwrap())))
}

// ---------------------------------------------------------------------------
// CRC32 (IEEE 802.3), table-driven — no tables crate, no dependencies:
// the 256-entry table is built at compile time by a const fn, so there
// is zero runtime init cost. (Callgrind on the v0.4 commit path showed
// the old bitwise loop at ~1.2% of instructions; this is ~8x faster for
// the same checksum.)
// ---------------------------------------------------------------------------

const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
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

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc = CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// Logical WAL records
// ---------------------------------------------------------------------------

/// One row version for the WAL: stable id, creator xid, values.
/// (xmax is always 0 at insert time; deletes are separate records.)
#[derive(Clone, Debug, PartialEq)]
pub struct WalRow {
    pub id: u64,
    pub xmin: u64,
    pub values: Vec<Value>,
}

/// One logical change to the committed database.
#[derive(Clone, Debug, PartialEq)]
pub enum WalRecord {
    CreateTable {
        name: String,
        columns: Vec<(String, ColType)>,
        xmin: u64,
    },
    InsertRows {
        table: String,
        rows: Vec<WalRow>,
    },
    DropTable {
        name: String,
        xmax: u64,
    },
    DeleteRows {
        table: String,
        ids: Vec<u64>,
        xmax: u64,
    },
    // --- v0.8: index DDL. Index *contents* need no records: replay
    // rebuilds them from the row records (CreateIndex scans the table;
    // InsertRows maintains live indexes), and checkpoints serialize them.
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
        xmin: u64,
    },
    DropIndex {
        name: String,
        xmax: u64,
    },
}

// ---------------------------------------------------------------------------
// Binary encoding helpers (big-endian, length-prefixed strings)
// ---------------------------------------------------------------------------

struct Enc {
    buf: Vec<u8>,
}

impl Enc {
    fn new() -> Self {
        Enc { buf: Vec::new() }
    }

    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.bytes(s.as_bytes());
    }

    fn col_type(&mut self, t: &ColType) {
        // Tags 0-3 are the v0.6 layout; new v0.7 types append after.
        self.u8(match t {
            ColType::Int => 0,
            ColType::Float => 1,
            ColType::Text => 2,
            ColType::Bool => 3,
            ColType::SmallInt => 4,
            ColType::BigInt => 5,
            ColType::Float4 => 6,
            ColType::Numeric => 7,
            ColType::Date => 8,
            ColType::Timestamp => 9,
            ColType::Timestamptz => 10,
            ColType::Bytea => 11,
            ColType::Uuid => 12,
        });
    }

    fn value(&mut self, v: &Value) {
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
        }
    }

    fn columns(&mut self, cols: &[(String, ColType)]) {
        self.u32(cols.len() as u32);
        for (name, typ) in cols {
            self.str(name);
            self.col_type(typ);
        }
    }

    fn record(&mut self, r: &WalRecord) {
        match r {
            WalRecord::CreateTable {
                name,
                columns,
                xmin,
            } => {
                self.u8(1);
                self.str(name);
                self.columns(columns);
                self.u64(*xmin);
            }
            WalRecord::InsertRows { table, rows } => {
                self.u8(2);
                self.str(table);
                self.u32(rows.len() as u32);
                for r in rows {
                    self.u64(r.id);
                    self.u64(r.xmin);
                    self.u32(r.values.len() as u32);
                    for v in &r.values {
                        self.value(v);
                    }
                }
            }
            WalRecord::DropTable { name, xmax } => {
                self.u8(3);
                self.str(name);
                self.u64(*xmax);
            }
            WalRecord::DeleteRows { table, ids, xmax } => {
                self.u8(4);
                self.str(table);
                self.u32(ids.len() as u32);
                for id in ids {
                    self.u64(*id);
                }
                self.u64(*xmax);
            }
            WalRecord::CreateIndex {
                name,
                table,
                columns,
                unique,
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
                self.u64(*xmin);
            }
            WalRecord::DropIndex { name, xmax } => {
                self.u8(6);
                self.str(name);
                self.u64(*xmax);
            }
        }
    }
}

struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Dec { buf, pos: 0 }
    }

    fn err(&self, what: &str) -> String {
        format!("{} at offset {}", what, self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.buf.len() {
            return Err(self.err("truncated value"));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i16(&mut self) -> Result<i16, String> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn i32(&mut self) -> Result<i32, String> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f32(&mut self) -> Result<f32, String> {
        let b = self.take(4)?;
        Ok(f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64(&mut self) -> Result<f64, String> {
        let b = self.take(8)?;
        Ok(f64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn str(&mut self) -> Result<String, String> {
        let n = self.u32()? as usize;
        let b = self.take(n)?;
        std::str::from_utf8(b)
            .map(|s| s.to_string())
            .map_err(|_| self.err("invalid utf-8 in string"))
    }

    fn col_type(&mut self) -> Result<ColType, String> {
        match self.u8()? {
            0 => Ok(ColType::Int),
            1 => Ok(ColType::Float),
            2 => Ok(ColType::Text),
            3 => Ok(ColType::Bool),
            4 => Ok(ColType::SmallInt),
            5 => Ok(ColType::BigInt),
            6 => Ok(ColType::Float4),
            7 => Ok(ColType::Numeric),
            8 => Ok(ColType::Date),
            9 => Ok(ColType::Timestamp),
            10 => Ok(ColType::Timestamptz),
            11 => Ok(ColType::Bytea),
            12 => Ok(ColType::Uuid),
            t => Err(self.err(&format!("unknown column type {}", t))),
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.u8()? {
            0 => Ok(Value::Null),
            1 => Ok(Value::Int(self.i64()?)),
            2 => Ok(Value::Float(self.f64()?)),
            3 => Ok(Value::Text(self.str()?)),
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
            t => Err(self.err(&format!("unknown value tag {}", t))),
        }
    }

    fn columns(&mut self) -> Result<Vec<(String, ColType)>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.str()?;
            let typ = self.col_type()?;
            out.push((name, typ));
        }
        Ok(out)
    }

    fn record(&mut self) -> Result<WalRecord, String> {
        match self.u8()? {
            1 => Ok(WalRecord::CreateTable {
                name: self.str()?,
                columns: self.columns()?,
                xmin: self.u64()?,
            }),
            2 => {
                let table = self.str()?;
                let n = self.u32()? as usize;
                let mut rows = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = self.u64()?;
                    let xmin = self.u64()?;
                    let m = self.u32()? as usize;
                    let mut values = Vec::with_capacity(m);
                    for _ in 0..m {
                        values.push(self.value()?);
                    }
                    rows.push(WalRow { id, xmin, values });
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
                for _ in 0..n {
                    ids.push(self.u64()?);
                }
                let xmax = self.u64()?;
                Ok(WalRecord::DeleteRows { table, ids, xmax })
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
                let xmin = self.u64()?;
                Ok(WalRecord::CreateIndex {
                    name,
                    table,
                    columns,
                    unique,
                    xmin,
                })
            }
            6 => {
                let name = self.str()?;
                let xmax = self.u64()?;
                Ok(WalRecord::DropIndex { name, xmax })
            }
            t => Err(self.err(&format!("unknown record tag {}", t))),
        }
    }

    fn end(&self) -> Result<(), String> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(self.err("trailing bytes"))
        }
    }
}

// ---------------------------------------------------------------------------
// Applying records during recovery
// ---------------------------------------------------------------------------

/// Find the table version that a committed DML record targets: the live
/// (not dropped) version with `name`. Returns `None` when there is no
/// live version — possible if a concurrent transaction's DDL won a race
/// after the record's own commit validation (see `records_for_commit`);
/// the caller skips the record with a warning rather than failing
/// recovery over it.
fn live_table<'e>(eng: &'e mut Engine, name: &str) -> Option<&'e mut Table> {
    eng.db
        .tables
        .get_mut(name)
        .and_then(|vs| vs.iter_mut().find(|t| t.dropped_xmax == 0))
}

/// Apply one record during recovery. Records replay in commit order, so
/// the version chains — and therefore visibility — are rebuilt exactly.
pub fn apply_record(eng: &mut Engine, r: &WalRecord) -> Result<(), String> {
    // Fold the record's xids / row ids into the counters first, so they
    // resume above every id the log ever used even when the record below
    // is skipped as a no-op.
    match r {
        WalRecord::CreateTable { xmin, .. }
        | WalRecord::DropTable { xmax: xmin, .. }
        | WalRecord::CreateIndex { xmin, .. }
        | WalRecord::DropIndex { xmax: xmin, .. } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
        }
        WalRecord::InsertRows { rows, .. } => {
            for row in rows {
                if row.xmin >= eng.txns.next_xid {
                    eng.txns.next_xid = row.xmin + 1;
                }
                if row.id >= eng.txns.next_row_id {
                    eng.txns.next_row_id = row.id + 1;
                }
            }
        }
        WalRecord::DeleteRows { xmax, .. } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
        }
    }
    match r {
        WalRecord::CreateTable {
            name,
            columns,
            xmin,
        } => {
            eng.db
                .tables
                .entry(name.clone())
                .or_default()
                .push(Table::new(columns.clone(), *xmin));
        }
        WalRecord::InsertRows { table, rows } => {
            // Collect the actually-inserted (id, values) first: the index
            // maintenance below needs `eng` mutably, which conflicts with
            // the table borrow.
            let inserted: Vec<(u64, Vec<Value>)> = {
                let Some(t) = live_table(eng, table) else {
                    eprintln!(
                        "WAL replay: skipping InsertRows for \"{}\": no live table version",
                        table
                    );
                    return Ok(());
                };
                let mut out = Vec::new();
                for row in rows {
                    if t.rows.iter().any(|r| r.id == row.id) {
                        eprintln!(
                            "WAL replay: skipping duplicate row id {} in table \"{}\"",
                            row.id, table
                        );
                        continue;
                    }
                    t.push_version(RowVersion {
                        id: row.id,
                        values: row.values.clone(),
                        xmin: row.xmin,
                        xmax: 0,
                    });
                    out.push((row.id, row.values.clone()));
                }
                out
            };
            for (id, values) in &inserted {
                eng.db.index_insert_row(table, *id, values);
            }
        }
        WalRecord::DropTable { name, xmax } => match live_table(eng, name) {
            Some(t) => t.dropped_xmax = *xmax,
            None => eprintln!(
                "WAL replay: skipping DropTable \"{}\": no live table version",
                name
            ),
        },
        WalRecord::DeleteRows { table, ids, xmax } => {
            let Some(t) = live_table(eng, table) else {
                eprintln!(
                    "WAL replay: skipping DeleteRows for \"{}\": no live table version",
                    table
                );
                return Ok(());
            };
            for id in ids {
                match t.rows.iter_mut().find(|r| r.id == *id) {
                    Some(r) => r.xmax = *xmax,
                    None => eprintln!(
                        "WAL replay: skipping DELETE of missing row id {} in table \"{}\"",
                        id, table
                    ),
                }
            }
            // No index maintenance: the entries stay (visibility filters
            // them), exactly as in live execution.
        }
        WalRecord::CreateIndex {
            name,
            table,
            columns,
            unique,
            xmin,
        } => {
            // Rebuild the index from the table's current rows: at recovery
            // every row present comes from a committed batch, so indexing
            // all versions is correct (visibility filters at scan time).
            let built: Option<Index> = (|| {
                let t = live_table(eng, table)?;
                let mut cols = Vec::with_capacity(columns.len());
                for c in columns {
                    cols.push(t.column_index(c)?);
                }
                let mut ix = Index::new(IndexDef {
                    name: name.clone(),
                    table: table.clone(),
                    cols,
                    col_names: columns.clone(),
                    unique: *unique,
                    created_xmin: *xmin,
                    dropped_xmax: 0,
                });
                for r in &t.rows {
                    let key = ix.key_for(&r.values);
                    ix.insert(key, r.id);
                }
                Some(ix)
            })();
            match built {
                Some(ix) => {
                    eng.db.indexes.insert(name.clone(), ix);
                }
                None => eprintln!(
                    "WAL replay: skipping CreateIndex \"{}\": no live table version \
                     or unknown column",
                    name
                ),
            }
        }
        WalRecord::DropIndex { name, xmax } => match eng.db.indexes.get_mut(name) {
            Some(ix) => ix.def.dropped_xmax = *xmax,
            None => eprintln!(
                "WAL replay: skipping DropIndex \"{}\": no such index",
                name
            ),
        },
    }
    Ok(())
}

/// Derive the commit-time WAL records from a transaction's write log.
///
/// Every op is re-validated against current engine state: only effects
/// that are still "ours" (`xmin`/`xmax`/`dropped_xmax`/`created_xmin`
/// equal to our xid) are logged. A concurrent transaction may have
/// overwritten our change after we made it (last-writer-wins — there is
/// no row locking in v0.5); logging the stale op would corrupt replay,
/// so it is skipped and the concurrent change wins. An op whose target
/// vanished (table dropped by a committed concurrent transaction) is
/// skipped for the same reason.
///
/// Two commit-ordering rules keep replay total:
/// - CREATE TABLE is first-committer-wins: if another committed live
///   version of the name exists, our commit fails with a serialization
///   error (the statement-time check could not see the concurrent
///   uncommitted CREATE).
/// - Records stay in op order (inserts grouped per table), so replay
///   sees every target its record needs.
///
/// Returns `Err` on a commit-time serialization conflict (SQLSTATE 40001
/// at the call site).
pub fn records_for_commit(
    eng: &Engine,
    own: u64,
    writes: &[WriteOp],
) -> Result<Vec<WalRecord>, String> {
    let mut out: Vec<WalRecord> = Vec::new();
    let mut i = 0;
    while i < writes.len() {
        let op = &writes[i];
        match op {
            WriteOp::InsertRow { table, row_id } => {
                let Some((values, table_live)) = own_row(eng, own, *row_id) else {
                    i += 1;
                    continue;
                };
                if !table_live {
                    i += 1;
                    continue; // table dropped by a committed concurrent txn
                }
                let row = WalRow {
                    id: *row_id,
                    xmin: own,
                    values,
                };
                match out.last_mut() {
                    Some(WalRecord::InsertRows { table: t, rows }) if t == table => rows.push(row),
                    _ => out.push(WalRecord::InsertRows {
                        table: table.clone(),
                        rows: vec![row],
                    }),
                }
            }
            WriteOp::DeleteRow { table, row_id, .. } => {
                let Some(v) = eng.db.find_row_version(*row_id) else {
                    i += 1;
                    continue;
                };
                if v.xmax != own {
                    // Our delete was overwritten by a concurrent txn
                    // (last-writer-wins). If the next op is our INSERT
                    // successor (an UPDATE pair), committing would leave
                    // BOTH versions live — a duplicate row. Fail the
                    // commit instead, like a write-write conflict.
                    let paired_insert =
                        matches!(writes.get(i + 1), Some(WriteOp::InsertRow { .. }));
                    if paired_insert {
                        return Err(format!(
                            "concurrent update on row id {} in table \"{}\"",
                            row_id, table
                        ));
                    }
                    i += 1;
                    continue; // pure DELETE lost; the row stays deleted
                }
                match out.last_mut() {
                    Some(WalRecord::DeleteRows {
                        table: t,
                        ids,
                        xmax,
                    }) if t == table && *xmax == own => ids.push(*row_id),
                    _ => out.push(WalRecord::DeleteRows {
                        table: table.clone(),
                        ids: vec![*row_id],
                        xmax: own,
                    }),
                }
            }
            WriteOp::CreateTable { name } => {
                let Some(versions) = eng.db.tables.get(name) else {
                    i += 1;
                    continue;
                };
                let Some(ours) = versions.iter().find(|t| t.created_xmin == own) else {
                    i += 1;
                    continue;
                };
                // First-committer-wins: a rival committed live version
                // means our CREATE lost the race.
                let rival = versions.iter().any(|t| {
                    t.created_xmin != own
                        && eng.xid_committed(t.created_xmin)
                        && (t.dropped_xmax == 0 || !eng.xid_committed(t.dropped_xmax))
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateTable {
                    name: name.clone(),
                    columns: ours.columns.clone(),
                    xmin: own,
                });
            }
            WriteOp::DropTable { name, .. } => {
                let won = eng
                    .db
                    .tables
                    .get(name)
                    .is_some_and(|vs| vs.iter().any(|t| t.dropped_xmax == own));
                if !won {
                    i += 1;
                    continue; // overwritten by a concurrent drop; theirs wins
                }
                out.push(WalRecord::DropTable {
                    name: name.clone(),
                    xmax: own,
                });
            }
            WriteOp::CreateIndex { name } => {
                let Some(ix) = eng.db.indexes.get(name) else {
                    i += 1;
                    continue;
                };
                if ix.def.created_xmin != own {
                    i += 1;
                    continue;
                }
                // First-committer-wins, mirroring CREATE TABLE: a rival
                // committed live definition means our CREATE lost the race.
                let rival = eng.db.indexes.values().any(|o| {
                    o.def.name == *name
                        && o.def.created_xmin != own
                        && eng.xid_committed(o.def.created_xmin)
                        && (o.def.dropped_xmax == 0
                            || !eng.xid_committed(o.def.dropped_xmax))
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateIndex {
                    name: name.clone(),
                    table: ix.def.table.clone(),
                    columns: ix.def.col_names.clone(),
                    unique: ix.def.unique,
                    xmin: own,
                });
            }
            WriteOp::DropIndex { name, .. } => {
                let won = eng
                    .db
                    .indexes
                    .get(name)
                    .is_some_and(|ix| ix.def.dropped_xmax == own);
                if !won {
                    i += 1;
                    continue; // overwritten by a concurrent drop; theirs wins
                }
                out.push(WalRecord::DropIndex {
                    name: name.clone(),
                    xmax: own,
                });
            }
        }
        i += 1;
    }
    Ok(out)
}

/// Our uncommitted row version, plus whether its table version is still
/// live (not dropped by a committed concurrent transaction). `None` when
/// the row is gone or no longer ours.
fn own_row(eng: &Engine, own: u64, row_id: u64) -> Option<(Vec<Value>, bool)> {
    for versions in eng.db.tables.values() {
        for t in versions {
            if let Some(pos) = t.row_pos(row_id) {
                let v = &t.rows[pos];
                if v.xmin != own {
                    return None;
                }
                let dropped = t.dropped_xmax != 0
                    && t.dropped_xmax != own
                    && eng.xid_committed(t.dropped_xmax);
                return Some((v.values.clone(), !dropped));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The WAL itself
// ---------------------------------------------------------------------------

/// Append-only write-ahead log in `<data_dir>/wal.log`, with
/// `<data_dir>/checkpoint.dat` snapshots.
pub struct Wal {
    dir: PathBuf,
    file: File,
    /// Physical bytes of wal.log accounted for (includes the header).
    len: u64,
    /// Logical LSN of physical offset WAL_HEADER_LEN: frame LSNs are
    /// `base_lsn + (physical_offset - WAL_HEADER_LEN)`.
    base_lsn: u64,
    next_txn: u64,
}

fn io_err(what: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

impl Wal {
    /// Open (creating) the data directory, load the latest checkpoint,
    /// and replay WAL frames after it. Returns the recovered database and
    /// the open WAL. A missing/empty data dir yields an empty database —
    /// the server keeps its in-memory behavior when there is no state.
    pub fn open(dir: &Path) -> std::io::Result<(Engine, Wal)> {
        fs::create_dir_all(dir)?;
        let (mut eng, wal_end) = load_checkpoint(dir)?;
        let wal_path = dir.join(WAL_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&wal_path)?;
        let mut file_len = file.metadata()?.len();

        // Establish this generation's base LSN. An empty file (fresh
        // data dir) or a torn header (crash during the WAL reset inside
        // checkpoint()) starts a new generation at the checkpoint's
        // logical end — safe per the module docs.
        let base_lsn = match read_wal_header(&mut file)? {
            Some(b) => b,
            None => {
                file.set_len(0)?;
                file.write_all(&encode_wal_header(wal_end))?;
                file.sync_all()?;
                file_len = WAL_HEADER_LEN;
                wal_end
            }
        };

        let mut batches = 0u64;
        let mut records = 0u64;
        let mut max_txn = 0u64;
        // Replay frames at/after the checkpoint's logical end; older
        // frames are already in the snapshot and must NOT be re-applied
        // (InsertRows is not idempotent).
        file.seek(SeekFrom::Start(WAL_HEADER_LEN))?;
        loop {
            let phys = file.stream_position()?;
            let frame = match read_frame(&mut file)? {
                None => break, // clean EOF or torn tail
                Some(f) => f,
            };
            let frame_lsn = base_lsn + (phys - WAL_HEADER_LEN);
            if frame_lsn < wal_end {
                continue; // already in the checkpoint image
            }
            max_txn = max_txn.max(frame.txn_id);
            for r in &frame.records {
                apply_record(&mut eng, r).map_err(io_err)?;
                records += 1;
            }
            batches += 1;
        }
        // No transaction survives a crash: anything still marked active
        // would be a lie. (Uncommitted work never reached the WAL.)
        eng.txns.active.clear();
        eng.txns.snapshots.clear();
        let tables: usize = eng.db.tables.values().map(|vs| vs.len()).sum();
        println!(
            "rustgres v0.8 recovery: {} table version(s), replayed {} WAL batch(es) / {} record(s) from {}",
            tables,
            batches,
            records,
            dir.display()
        );
        Ok((
            eng,
            Wal {
                dir: dir.to_path_buf(),
                file,
                len: file_len,
                base_lsn,
                next_txn: max_txn + 1,
            },
        ))
    }

    /// Durably append one commit's records: write the whole frame, fsync,
    /// and only then let the caller publish. Returns the batch's txn id.
    /// Empty record sets (read-only commits) skip the write entirely.
    pub fn append_batch(&mut self, records: &[WalRecord]) -> std::io::Result<u64> {
        if records.is_empty() {
            return Ok(self.next_txn);
        }
        let txn_id = self.next_txn;
        self.next_txn += 1;

        let mut body = Enc::new();
        body.u64(txn_id);
        body.u32(records.len() as u32);
        for r in records {
            body.record(r);
        }
        let crc = crc32(&body.buf);

        let mut frame = Enc::new();
        // frame_len counts everything after itself (txn_id..crc inclusive).
        frame.u32((body.buf.len() + 4) as u32);
        frame.bytes(&body.buf);
        frame.u32(crc);

        self.file.write_all(&frame.buf)?;
        // COMMIT durability: the frame is on stable storage before we
        // return. No group commit in v0.5 — one fsync per commit.
        self.file.sync_all()?;
        self.len += frame.buf.len() as u64;
        Ok(txn_id)
    }

    /// Snapshot the committed database and truncate the WAL. Holds the
    /// caller's engine lock for the whole procedure (writers block
    /// briefly); crash-safe at every step — see the module docs.
    /// Only committed versions are written to the image: versions
    /// created by still-active transactions are skipped, and
    /// xmax/dropped_xmax set by still-active transactions is masked to
    /// 0 — so a crash can never resurrect uncommitted work as committed.
    pub fn checkpoint(&mut self, eng: &Engine) -> std::io::Result<()> {
        // Logical end of this WAL generation: everything before it is
        // about to be snapshotted.
        let wal_end = self.base_lsn + (self.len - WAL_HEADER_LEN);

        // 1. Encode the image in memory: magic, version, WAL offset,
        // counters, then tables. Only *committed* state is snapshotted:
        // versions created by still-active transactions are skipped, and
        // xmax/dropped_xmax set by still-active transactions is masked to
        // 0. (Uncommitted work never reaches the WAL either, so recovery
        //   = this image + replay of later committed batches, exactly.)
        // The checkpoint holds the engine lock throughout, so no commit
        // can interleave: every commit is either fully before wal_end
        // (in the WAL, skipped on replay) or fully after (replayed).
        let mut img = Enc::new();
        img.bytes(CHKPT_MAGIC);
        img.u32(CHKPT_VERSION);
        img.u64(wal_end);
        img.u64(eng.txns.next_xid);
        img.u64(eng.txns.next_row_id);
        // Sort table names for a deterministic image (HashMap order is not).
        let mut names: Vec<&String> = eng.db.tables.keys().collect();
        names.sort();
        let mut n_versions = 0u32;
        let mut body = Enc::new();
        for name in names {
            for t in &eng.db.tables[name] {
                if !eng.xid_committed(t.created_xmin) {
                    continue; // uncommitted CREATE: not part of durable state
                }
                let dropped_xmax = committed_xmax(eng, t.dropped_xmax);
                body.str(name);
                body.u64(t.created_xmin);
                body.u64(dropped_xmax);
                body.columns(&t.columns);
                let live_rows: Vec<&RowVersion> = t
                    .rows
                    .iter()
                    .filter(|r| eng.xid_committed(r.xmin))
                    .collect();
                body.u32(live_rows.len() as u32);
                for r in live_rows {
                    body.u64(r.id);
                    body.u64(r.xmin);
                    body.u64(committed_xmax(eng, r.xmax));
                    body.u32(r.values.len() as u32);
                    for v in &r.values {
                        body.value(v);
                    }
                }
                n_versions += 1;
            }
        }
        img.u32(n_versions);
        img.bytes(&body.buf);

        // v0.8: index *definitions*. Only live, committed indexes; their
        // entries are rebuilt from the decoded tables on load (every row
        // in the image is committed, so the rebuild is exact).
        let mut ix_names: Vec<&String> = eng.db.indexes.keys().collect();
        ix_names.sort();
        let mut ix_body = Enc::new();
        let mut n_indexes = 0u32;
        for name in ix_names {
            let ix = &eng.db.indexes[name];
            if !eng.xid_committed(ix.def.created_xmin) {
                continue; // uncommitted CREATE INDEX: not durable state
            }
            if committed_xmax(eng, ix.def.dropped_xmax) != 0 {
                continue; // dropped by a committed txn: not durable state
            }
            ix_body.str(name);
            ix_body.str(&ix.def.table);
            ix_body.u32(ix.def.col_names.len() as u32);
            for c in &ix.def.col_names {
                ix_body.str(c);
            }
            ix_body.u8(ix.def.unique as u8);
            ix_body.u64(ix.def.created_xmin);
            ix_body.u64(0); // live index: no committed drop
            n_indexes += 1;
        }
        img.u32(n_indexes);
        img.bytes(&ix_body.buf);

        // 2. Write tmp file + fsync.
        let tmp_path = self.dir.join(CHKPT_TMP);
        {
            let mut tmp = File::create(&tmp_path)?;
            tmp.write_all(&img.buf)?;
            tmp.sync_all()?;
        }
        // 3. Atomic rename over the old checkpoint...
        fs::rename(&tmp_path, self.dir.join(CHKPT_NAME))?;
        // 4. ...fsync the directory so the rename itself is durable...
        File::open(&self.dir)?.sync_all()?;
        // 5. ...then reset the WAL to a fresh generation whose base LSN
        //    is the checkpoint's logical end. A crash before this step
        //    leaves the old generation in place and recovery skips its
        //    frames by LSN; a crash during it leaves an empty/torn file
        //    that open() turns into a fresh generation at wal_end.
        self.file.set_len(0)?;
        self.file.write_all(&encode_wal_header(wal_end))?;
        self.file.sync_all()?;
        self.len = WAL_HEADER_LEN;
        self.base_lsn = wal_end;
        println!(
            "rustgres v0.8 checkpoint: {} table version(s), WAL reset (base_lsn={})",
            n_versions, wal_end
        );
        Ok(())
    }
}

/// The effective xmax for durable state: the deleter's xid if that
/// delete committed, else 0 (the delete is not durable yet).
fn committed_xmax(eng: &Engine, xmax: u64) -> u64 {
    if xmax != 0 && eng.xid_committed(xmax) {
        xmax
    } else {
        0
    }
}

struct Frame {
    txn_id: u64,
    records: Vec<WalRecord>,
}

/// Read one WAL frame. `Ok(None)` = clean EOF (no bytes) or a torn tail
/// (short read / bad CRC): recovery stops, the partial batch is dropped.
fn read_frame(file: &mut File) -> std::io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match file.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            // 0 bytes read at a frame boundary = clean end. A 1-3 byte
            // read_exact failure means the length word itself is torn.
            // Either way the tail is not a complete batch: stop.
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    let frame_len = u32::from_be_bytes(len_buf) as usize;
    if frame_len < 8 + 4 + 4 || frame_len > 256 * 1024 * 1024 {
        // Absurd length: torn or corrupt tail — stop, don't trust it.
        return Ok(None);
    }
    let mut rest = vec![0u8; frame_len];
    if file.read_exact(&mut rest).is_err() {
        return Ok(None); // torn tail
    }
    let (body, crc_bytes) = rest.split_at(frame_len - 4);
    let want = u32::from_be_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
    if crc32(body) != want {
        return Ok(None); // torn tail
    }
    let mut d = Dec::new(body);
    let txn_id = d.u64().map_err(io_err)?;
    let n = d.u32().map_err(io_err)? as usize;
    let mut records = Vec::with_capacity(n);
    for _ in 0..n {
        records.push(d.record().map_err(io_err)?);
    }
    d.end().map_err(io_err)?;
    Ok(Some(Frame { txn_id, records }))
}

/// Load `<dir>/checkpoint.dat`. Returns (database, logical WAL offset the
/// checkpoint covers). Missing file = fresh database. A present-but-
/// undecodable file is a loud error: silently starting empty would lose
/// data, so the server refuses to start instead.
fn load_checkpoint(dir: &Path) -> std::io::Result<(Engine, u64)> {
    let path = dir.join(CHKPT_NAME);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Engine::new(), 0)),
        Err(e) => return Err(e),
    };
    let mut d = Dec::new(&bytes);
    let bad = |why: &str| {
        io_err(format!(
            "{} is corrupt ({}); refusing to start",
            path.display(),
            why
        ))
    };
    if d.take(8).map_err(|e| bad(&e))? != CHKPT_MAGIC {
        return Err(bad(
            "bad magic (a v0.7 checkpoint is not readable by v0.8; remove the data directory)",
        ));
    }
    if d.u32().map_err(|e| bad(&e))? != CHKPT_VERSION {
        return Err(bad("unsupported version"));
    }
    let wal_end = d.u64().map_err(|e| bad(&e))?;
    let mut eng = Engine::new();
    eng.txns.next_xid = d.u64().map_err(|e| bad(&e))?.max(1);
    eng.txns.next_row_id = d.u64().map_err(|e| bad(&e))?.max(1);
    let n_versions = d.u32().map_err(|e| bad(&e))? as usize;
    for _ in 0..n_versions {
        let name = d.str().map_err(|e| bad(&e))?;
        let created_xmin = d.u64().map_err(|e| bad(&e))?;
        let dropped_xmax = d.u64().map_err(|e| bad(&e))?;
        let columns = d.columns().map_err(|e| bad(&e))?;
        let n_rows = d.u32().map_err(|e| bad(&e))? as usize;
        let mut rows = Vec::with_capacity(n_rows);
        for _ in 0..n_rows {
            let id = d.u64().map_err(|e| bad(&e))?;
            let xmin = d.u64().map_err(|e| bad(&e))?;
            let xmax = d.u64().map_err(|e| bad(&e))?;
            let n_vals = d.u32().map_err(|e| bad(&e))? as usize;
            let mut values = Vec::with_capacity(n_vals);
            for _ in 0..n_vals {
                values.push(d.value().map_err(|e| bad(&e))?);
            }
            if values.len() != columns.len() {
                return Err(bad("row/column count mismatch"));
            }
            if id >= eng.txns.next_row_id {
                eng.txns.next_row_id = id + 1;
            }
            rows.push(RowVersion {
                id,
                values,
                xmin,
                xmax,
            });
        }
        let mut __t = Table::new(columns, created_xmin);
        __t.dropped_xmax = dropped_xmax;
        for __rv in rows {
            __t.push_version(__rv);
        }
        eng.db.tables.entry(name).or_default().push(__t);
    }
    // v0.8: index definitions, then rebuild entries from the decoded
    // tables (every row here is committed).
    let n_indexes = d.u32().map_err(|e| bad(&e))? as usize;
    for _ in 0..n_indexes {
        let name = d.str().map_err(|e| bad(&e))?;
        let table = d.str().map_err(|e| bad(&e))?;
        let n_cols = d.u32().map_err(|e| bad(&e))? as usize;
        let mut col_names = Vec::with_capacity(n_cols);
        for _ in 0..n_cols {
            col_names.push(d.str().map_err(|e| bad(&e))?);
        }
        let unique = d.u8().map_err(|e| bad(&e))? != 0;
        let created_xmin = d.u64().map_err(|e| bad(&e))?;
        let dropped_xmax = d.u64().map_err(|e| bad(&e))?;
        let bad_idx = |why: String| {
            io_err(format!(
                "{} is corrupt (index \"{}\": {}); refusing to start",
                path.display(),
                name,
                why
            ))
        };
        let ix: Index = {
            let versions = eng
                .db
                .tables
                .get(&table)
                .ok_or_else(|| bad_idx(format!("no such table \"{}\"", table)))?;
            let t = versions
                .iter()
                .find(|t| t.dropped_xmax == 0)
                .ok_or_else(|| bad_idx(format!("no live version of table \"{}\"", table)))?;
            let mut cols = Vec::with_capacity(col_names.len());
            for c in &col_names {
                cols.push(t.column_index(c).ok_or_else(|| {
                    bad_idx(format!("unknown column \"{}\" in table \"{}\"", c, table))
                })?);
            }
            let mut ix = Index::new(IndexDef {
                name: name.clone(),
                table: table.clone(),
                cols,
                col_names,
                unique,
                created_xmin,
                dropped_xmax,
            });
            for r in &t.rows {
                let key = ix.key_for(&r.values);
                ix.insert(key, r.id);
            }
            ix
        };
        eng.db.indexes.insert(name, ix);
    }
    d.end().map_err(|e| bad(&e))?;
    Ok((eng, wal_end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::WriteOp;

    fn engine_with_committed_table() -> Engine {
        let mut eng = Engine::new();
        eng.db.tables.insert(
            "t".into(),
            vec![{
                let mut t = Table::new(vec![("a".into(), ColType::Int)], 1);
                t.push_version(RowVersion {
                    id: 1,
                    values: vec![Value::Int(10)],
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

    #[test]
    fn crc32_known_answers() {
        // Standard check value; guards the table-driven rewrite.
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"rustgres"), 0x94E1_4388);
    }

    #[test]
    fn crc32_detects_single_bit_flip() {
        let a = crc32(b"hello world");
        let mut b = b"hello world".to_vec();
        b[5] ^= 1;
        assert_ne!(a, crc32(&b));
    }

    fn roundtrip(r: &WalRecord) -> WalRecord {
        let mut e = Enc::new();
        e.record(r);
        let mut d = Dec::new(&e.buf);
        let out = d.record().unwrap();
        d.end().unwrap();
        out
    }

    #[test]
    fn record_roundtrip_all_kinds() {
        let cases = vec![
            WalRecord::CreateTable {
                name: "t".into(),
                columns: vec![("a".into(), ColType::Int)],
                xmin: 3,
            },
            WalRecord::InsertRows {
                table: "t".into(),
                rows: vec![
                    WalRow {
                        id: 7,
                        xmin: 4,
                        values: vec![Value::Int(1), Value::Null],
                    },
                    WalRow {
                        id: 8,
                        xmin: 4,
                        values: vec![Value::Int(2), Value::Text("x".into())],
                    },
                ],
            },
            WalRecord::DropTable {
                name: "t".into(),
                xmax: 5,
            },
            WalRecord::DeleteRows {
                table: "t".into(),
                ids: vec![7, 8, 9],
                xmax: 6,
            },
        ];
        for c in &cases {
            assert_eq!(&roundtrip(c), c);
        }
    }

    #[test]
    fn records_for_commit_groups_inserts() {
        let mut eng = engine_with_committed_table();
        let xid = eng.begin_txn();
        // Stage two inserts into t.
        for (id, v) in [(2u64, 20i64), (3, 30)] {
            eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
                id,
                values: vec![Value::Int(v)],
                xmin: xid,
                xmax: 0,
            });
        }
        let writes = vec![
            WriteOp::InsertRow {
                table: "t".into(),
                row_id: 2,
            },
            WriteOp::InsertRow {
                table: "t".into(),
                row_id: 3,
            },
        ];
        let recs = records_for_commit(&eng, xid, &writes).unwrap();
        assert_eq!(recs.len(), 1);
        match &recs[0] {
            WalRecord::InsertRows { table, rows } => {
                assert_eq!(table, "t");
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].values, vec![Value::Int(20)]);
            }
            r => panic!("unexpected record: {:?}", r),
        }
    }

    #[test]
    fn records_for_commit_rejects_lost_create_race() {
        let mut eng = engine_with_committed_table();
        // Rival transaction created "t" and committed first.
        eng.db.tables.get_mut("t").unwrap()[0].created_xmin = 5;
        let xid = eng.begin_txn(); // 11
        // Our own uncommitted create of the same name.
        eng.db
            .tables
            .get_mut("t")
            .unwrap()
            .push(Table::new(vec![("a".into(), ColType::Int)], xid));
        let writes = vec![WriteOp::CreateTable { name: "t".into() }];
        let err = records_for_commit(&eng, xid, &writes).unwrap_err();
        assert!(err.contains("already exists"), "got: {}", err);
    }

    #[test]
    fn records_for_commit_skips_overwritten_delete() {
        let mut eng = engine_with_committed_table();
        let xid = eng.begin_txn();
        // We delete row 1...
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = xid;
        // ...but a concurrent txn overwrites our delete (last-writer-wins).
        let other = eng.begin_txn();
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = other;
        let writes = vec![WriteOp::DeleteRow {
            table: "t".into(),
            row_id: 1,
            prev_xmax: 0,
        }];
        let recs = records_for_commit(&eng, xid, &writes).unwrap();
        assert!(recs.is_empty());
    }

    #[test]
    fn records_for_commit_fails_lost_update() {
        // UPDATE/UPDATE race: our delete-half was overwritten, but our
        // insert-half survives -> committing would duplicate the row.
        let mut eng = engine_with_committed_table();
        let xid = eng.begin_txn();
        // Our UPDATE of row 1: delete-half + insert-half (adjacent pair).
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = xid;
        eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
            id: 2,
            values: vec![Value::Int(99)],
            xmin: xid,
            xmax: 0,
        });
        // Concurrent txn overwrites our delete-half.
        let other = eng.begin_txn();
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = other;
        let writes = vec![
            WriteOp::DeleteRow {
                table: "t".into(),
                row_id: 1,
                prev_xmax: 0,
            },
            WriteOp::InsertRow {
                table: "t".into(),
                row_id: 2,
            },
        ];
        let err = records_for_commit(&eng, xid, &writes).unwrap_err();
        assert!(err.contains("concurrent update"), "got: {}", err);
    }

    #[test]
    fn apply_record_rebuilds_versions_and_counters() {
        let mut eng = Engine::new();
        apply_record(
            &mut eng,
            &WalRecord::CreateTable {
                name: "t".into(),
                columns: vec![("a".into(), ColType::Int)],
                xmin: 4,
            },
        )
        .unwrap();
        apply_record(
            &mut eng,
            &WalRecord::InsertRows {
                table: "t".into(),
                rows: vec![WalRow {
                    id: 12,
                    xmin: 5,
                    values: vec![Value::Int(1)],
                }],
            },
        )
        .unwrap();
        apply_record(
            &mut eng,
            &WalRecord::DeleteRows {
                table: "t".into(),
                ids: vec![12],
                xmax: 6,
            },
        )
        .unwrap();
        let t = &eng.db.tables["t"][0];
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0].xmin, 5);
        assert_eq!(t.rows[0].xmax, 6);
        // Counters resume above every id the log used.
        assert_eq!(eng.txns.next_xid, 7);
        assert_eq!(eng.txns.next_row_id, 13);
    }

    #[test]
    fn apply_record_tolerates_missing_targets() {
        let mut eng = Engine::new();
        // No such table / row: warnings, not errors.
        apply_record(
            &mut eng,
            &WalRecord::DeleteRows {
                table: "nope".into(),
                ids: vec![1],
                xmax: 9,
            },
        )
        .unwrap();
        apply_record(
            &mut eng,
            &WalRecord::DropTable {
                name: "nope".into(),
                xmax: 9,
            },
        )
        .unwrap();
        assert_eq!(eng.txns.next_xid, 10);
    }
}
