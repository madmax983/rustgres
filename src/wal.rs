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
//! - `DeleteRows { table, ids, old_rows, xmax }` — row versions were
//!   marked deleted; `old_rows` carries the deleted values for the
//!   logical decoder (v0.13)
//! - `UpdateRows { table, old, new, xmax }` — row versions were updated;
//!   old/new row vectors make UPDATE a first-class record (v0.13)
//! - `ReplSlotCreate / ReplSlotDrop / ReplSlotFlush` — replication slot
//!   metadata, non-transactional (v0.13)
//!
//! Records are produced from the committing transaction's write log, in
//! order, so replay rebuilds exactly the published version chains with
//! identical xmin/xmax — and therefore identical visibility.
//!
//! Format version 7 (`RGSWAL07` / `RGSCHK07`) is NOT compatible with v0.12
//! or earlier: v0.13 adds old row values to DeleteRows, first-class
//! UpdateRows, replication slot records (CREATE/DROP/FLUSH), and
//! replication slots in the checkpoint image. Like every format bump,
//! old data directories are refused with a clear error instead of
//! being misread. v0.12 was `RGSWAL06` / `RGSCHK06`.
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
const CHKPT_MAGIC: &[u8; 8] = b"RGSCHK07";
const CHKPT_VERSION: u32 = 6;
/// WAL file header: magic + base_lsn (u64, big-endian). Every frame's
/// logical sequence number is base_lsn + (physical offset - HEADER_LEN).
/// v0.13: `RGSWAL07` — DeleteRows now carries old row values, plus new
/// UpdateRows / ReplSlot* records. Old `RGSWAL06` files are refused loudly
/// (see read_wal_header); remove the data directory to start fresh.
const WAL_MAGIC: &[u8; 8] = b"RGSWAL07";
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
    // A full 16-byte header with the wrong magic (e.g. an older
    // `RGSWAL05` file, whose record format is incompatible) is a loud
    // error: silently treating it as empty would lose data.
    if &hdr[..8] != WAL_MAGIC {
        return Err(io_err(
            "wal.log has an unrecognized magic; this rustgres cannot read older data - remove the data directory".to_string(),
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
        /// v0.9: s-expr encoded constraints/defaults (sql::encode_constraints).
        constraints: String,
        /// v0.11: creating role.
        owner: String,
        /// v0.11: explicit GRANT entries.
        acl: Vec<WalAcl>,
        /// v0.11: column-level GRANT entries.
        col_acl: Vec<WalColAcl>,
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
        /// v0.13: old row values, parallel to `ids` — what the logical
        /// decoder reports for DELETE. (Encoding changed with the
        /// `RGSWAL07` magic bump; old files are refused loudly.)
        old_rows: Vec<WalRow>,
        xmax: u64,
    },
    // --- v0.13: UPDATE as a first-class record (paired old/new values).
    // Replay applies it as delete-old + insert-new, exactly like the
    // DeleteRows+InsertRows pair it replaces; the logical decoder
    // reports it as a single UPDATE with old and new tuples.
    UpdateRows {
        table: String,
        old: Vec<WalRow>,
        new: Vec<WalRow>,
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
        /// v0.9: constraint-owned backing index.
        internal: bool,
        xmin: u64,
    },
    DropIndex {
        name: String,
        xmax: u64,
    },
    // --- v0.9: ALTER TABLE swaps the table version (new columns and/or
    // constraint metadata); the row data is unchanged.
    AlterTable {
        name: String,
        columns: Vec<(String, ColType)>,
        constraints: String,
        /// True for pure-metadata alters: replay copies the old version's
        /// rows. False for ADD/DROP COLUMN: rows are rewritten and flow
        /// through InsertRows records instead.
        copy_rows: bool,
        /// v0.11: owner and ACL travel with every alter (grants and
        /// OWNER TO reuse the alter machinery).
        owner: String,
        acl: Vec<WalAcl>,
        /// v0.11: column-level GRANT entries.
        col_acl: Vec<WalColAcl>,
        xmin: u64,
    },
    CreateView {
        name: String,
        query: String,
        col_aliases: Vec<String>,
        deps: Vec<String>,
        /// v0.11: creating role.
        owner: String,
        xmin: u64,
    },
    DropView {
        name: String,
        xmax: u64,
    },
    CreateSequence {
        name: String,
        seq: WalSequence,
        xmin: u64,
    },
    DropSequence {
        name: String,
        xmax: u64,
    },
    AlterSequence {
        name: String,
        seq: WalSequence,
        xmin: u64,
    },
    /// A committed nextval advance. Not transactional by design (like
    /// Postgres): replay always applies it.
    SeqAdvance {
        name: String,
        last_value: i64,
        is_called: bool,
    },
    // --- v0.11: roles and database ACL ---
    CreateRole {
        name: String,
        role: WalRole,
        xmin: u64,
    },
    DropRole {
        name: String,
        xmax: u64,
    },
    AlterRole {
        name: String,
        role: WalRole,
        xmin: u64,
    },
    DbAcl {
        acl: Vec<WalAcl>,
        xmin: u64,
    },
    // --- v0.13: replication slot metadata (non-transactional, like
    // SeqAdvance: replay always applies). Slots are cluster-global.
    ReplSlotCreate {
        name: String,
        plugin: String,
        slot_type: String,
        restart_lsn: u64,
    },
    ReplSlotDrop {
        name: String,
    },
    ReplSlotFlush {
        name: String,
        restart_lsn: u64,
        confirmed_flush_lsn: u64,
    },
}

/// One GRANT entry in WAL/checkpoint form (v0.11).
#[derive(Clone, Debug, PartialEq)]
pub struct WalAcl {
    pub role: String,
    pub privs: u32,
}

impl WalAcl {
    pub fn of(e: &crate::storage::AclEntry) -> Self {
        WalAcl {
            role: e.role.clone(),
            privs: e.privs,
        }
    }
    pub fn into_entry(self) -> crate::storage::AclEntry {
        crate::storage::AclEntry {
            role: self.role,
            privs: self.privs,
        }
    }
}

/// One column-level GRANT entry in WAL/checkpoint form (v0.11).
#[derive(Clone, Debug, PartialEq)]
pub struct WalColAcl {
    pub role: String,
    pub privs: u32,
    pub columns: Vec<String>,
}

impl WalColAcl {
    pub fn of(e: &crate::storage::ColAclEntry) -> Self {
        WalColAcl {
            role: e.role.clone(),
            privs: e.privs,
            columns: e.columns.clone(),
        }
    }
    pub fn into_entry(self) -> crate::storage::ColAclEntry {
        crate::storage::ColAclEntry {
            role: self.role,
            privs: self.privs,
            columns: self.columns,
        }
    }
}

/// A SCRAM verifier in WAL/checkpoint form (v0.11).
#[derive(Clone, Debug, PartialEq)]
pub struct WalVerifier {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
}

impl WalVerifier {
    pub fn of(v: &crate::crypto::ScramVerifier) -> Self {
        WalVerifier {
            iterations: v.iterations,
            salt: v.salt.clone(),
            stored_key: v.stored_key,
            server_key: v.server_key,
        }
    }
    pub fn into_verifier(self) -> crate::crypto::ScramVerifier {
        crate::crypto::ScramVerifier {
            iterations: self.iterations,
            salt: self.salt,
            stored_key: self.stored_key,
            server_key: self.server_key,
        }
    }
}

/// A role's durable attributes in WAL/checkpoint form (v0.11). The name
/// itself rides in the enclosing record.
#[derive(Clone, Debug, PartialEq)]
pub struct WalRole {
    pub password: Option<WalVerifier>,
    pub can_login: bool,
    pub superuser: bool,
    pub connlimit: i32,
    pub memberships: Vec<WalMembership>,
    pub valid_until: Option<String>,
}

/// WAL form of one `GRANT group TO member` edge.
#[derive(Clone, Debug, PartialEq)]
pub struct WalMembership {
    pub role: String,
    pub grantor: String,
}

impl WalRole {
    pub fn of(r: &crate::storage::Role) -> Self {
        WalRole {
            password: r.password.as_ref().map(WalVerifier::of),
            can_login: r.can_login,
            superuser: r.superuser,
            connlimit: r.connlimit,
            memberships: r
                .memberships
                .iter()
                .map(|m| WalMembership {
                    role: m.role.clone(),
                    grantor: m.grantor.clone(),
                })
                .collect(),
            valid_until: r.valid_until.clone(),
        }
    }
}

/// Plain-data form of a Sequence for WAL records and checkpoints.
#[derive(Clone, Debug, PartialEq)]
pub struct WalSequence {
    pub name: String,
    pub start: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
    /// Last value returned by nextval; i64::MIN sentinel = never called.
    pub current: i64,
    pub current_is_set: bool,
    pub is_called: bool,
    /// v0.11: owning role and explicit USAGE grants.
    pub owner: String,
    pub acl: Vec<WalAcl>,
}

impl WalSequence {
    pub fn of(s: &crate::storage::Sequence) -> Self {
        WalSequence {
            name: s.name.clone(),
            start: s.start,
            increment: s.increment,
            min_value: s.min_value,
            max_value: s.max_value,
            cycle: s.cycle,
            current: s.current.unwrap_or(i64::MIN),
            current_is_set: s.current.is_some(),
            is_called: s.is_called,
            owner: s.owner.clone(),
            acl: s.acl.iter().map(WalAcl::of).collect(),
        }
    }

    pub fn into_sequence(self, created_xmin: u64) -> crate::storage::Sequence {
        crate::storage::Sequence {
            name: self.name,
            start: self.start,
            increment: self.increment,
            min_value: self.min_value,
            max_value: self.max_value,
            cycle: self.cycle,
            current: if self.current_is_set {
                Some(self.current)
            } else {
                None
            },
            is_called: self.is_called,
            created_xmin,
            dropped_xmax: 0,
            owner: self.owner,
            acl: self.acl.into_iter().map(WalAcl::into_entry).collect(),
        }
    }
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
                constraints,
                owner,
                acl,
                col_acl,
                xmin,
            } => {
                self.u8(1);
                self.str(name);
                self.columns(columns);
                self.str(constraints);
                self.str(owner);
                self.acl_list(acl);
                self.col_acl_list(col_acl);
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
                    for v in &old.values {
                        self.value(v);
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
                    self.u64(r.id);
                    self.u64(r.xmin);
                    self.u32(r.values.len() as u32);
                    for v in &r.values {
                        self.value(v);
                    }
                }
                self.u32(new.len() as u32);
                for r in new {
                    self.u64(r.id);
                    self.u64(r.xmin);
                    self.u32(r.values.len() as u32);
                    for v in &r.values {
                        self.value(v);
                    }
                }
                self.u64(*xmax);
            }
            WalRecord::CreateIndex {
                name,
                table,
                columns,
                unique,
                internal,
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
                self.u64(*xmin);
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
        }
    }

    fn acl_list(&mut self, acl: &[WalAcl]) {
        self.u32(acl.len() as u32);
        for e in acl {
            self.str(&e.role);
            self.u32(e.privs);
        }
    }

    fn col_acl_list(&mut self, acl: &[WalColAcl]) {
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

    fn verifier(&mut self, v: &Option<WalVerifier>) {
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

    fn wal_role(&mut self, r: &WalRole) {
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

    fn sequence(&mut self, s: &WalSequence) {
        self.str(&s.name);
        self.i64(s.start);
        self.i64(s.increment);
        self.i64(s.min_value);
        self.i64(s.max_value);
        self.u8(s.cycle as u8);
        self.i64(s.current);
        self.u8(s.current_is_set as u8);
        self.u8(s.is_called as u8);
        // v0.11
        self.str(&s.owner);
        self.acl_list(&s.acl);
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

    /// Decode one WAL row: id, xmin, value count, values.
    fn wal_row(&mut self) -> Result<WalRow, String> {
        let id = self.u64()?;
        let xmin = self.u64()?;
        let nv = self.u32()? as usize;
        let mut values = Vec::with_capacity(nv);
        for _ in 0..nv {
            values.push(self.value()?);
        }
        Ok(WalRow { id, xmin, values })
    }

    fn record(&mut self) -> Result<WalRecord, String> {
        match self.u8()? {
            1 => {
                let name = self.str()?;
                let columns = self.columns()?;
                let constraints = self.str()?;
                // v0.11
                let owner = self.str()?;
                let acl = self.acl_list()?;
                let col_acl = self.col_acl_list()?;
                let xmin = self.u64()?;
                Ok(WalRecord::CreateTable {
                    name,
                    columns,
                    constraints,
                    owner,
                    acl,
                    col_acl,
                    xmin,
                })
            }
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
                let mut old_rows = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = self.u64()?;
                    let xmin = self.u64()?;
                    let nv = self.u32()? as usize;
                    let mut values = Vec::with_capacity(nv);
                    for _ in 0..nv {
                        values.push(self.value()?);
                    }
                    ids.push(id);
                    old_rows.push(WalRow { id, xmin, values });
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
                let xmin = self.u64()?;
                Ok(WalRecord::CreateIndex {
                    name,
                    table,
                    columns,
                    unique,
                    internal,
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
                let xmin = self.u64()?;
                Ok(WalRecord::AlterTable {
                    name,
                    columns,
                    constraints,
                    copy_rows,
                    owner,
                    acl,
                    col_acl,
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
            t => Err(self.err(&format!("unknown record tag {}", t))),
        }
    }

    fn sequence(&mut self) -> Result<WalSequence, String> {
        Ok(WalSequence {
            name: self.str()?,
            start: self.i64()?,
            increment: self.i64()?,
            min_value: self.i64()?,
            max_value: self.i64()?,
            cycle: self.u8()? != 0,
            current: self.i64()?,
            current_is_set: self.u8()? != 0,
            is_called: self.u8()? != 0,
            // v0.11
            owner: self.str()?,
            acl: self.acl_list()?,
        })
    }

    fn acl_list(&mut self) -> Result<Vec<WalAcl>, String> {
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

    fn col_acl_list(&mut self) -> Result<Vec<WalColAcl>, String> {
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

    fn verifier(&mut self) -> Result<Option<WalVerifier>, String> {
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

    fn wal_role(&mut self) -> Result<WalRole, String> {
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
        | WalRecord::DropIndex { xmax: xmin, .. }
        | WalRecord::AlterTable { xmin, .. }
        | WalRecord::CreateView { xmin, .. }
        | WalRecord::DropView { xmax: xmin, .. }
        | WalRecord::CreateSequence { xmin, .. }
        | WalRecord::DropSequence { xmax: xmin, .. }
        | WalRecord::AlterSequence { xmin, .. } => {
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
        // v0.13: UPDATE carries old/new row ids — fold them into the
        // row-id counter so it resumes above every id the log used.
        WalRecord::UpdateRows {
            table: _,
            old,
            new,
            xmax,
        } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            for r in old.iter().chain(new.iter()) {
                if r.id >= eng.txns.next_row_id {
                    eng.txns.next_row_id = r.id + 1;
                }
            }
        }
        // v0.13: replication slots carry no xid (non-transactional).
        WalRecord::ReplSlotCreate { .. }
        | WalRecord::ReplSlotDrop { .. }
        | WalRecord::ReplSlotFlush { .. } => {}
        // v0.9: sequence advances carry no xid (non-transactional).
        WalRecord::SeqAdvance { .. } => {}
        // v0.11: roles.
        WalRecord::CreateRole { name, role, xmin } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            let r = crate::storage::Role {
                name: name.clone(),
                password: role.password.clone().map(WalVerifier::into_verifier),
                can_login: role.can_login,
                superuser: role.superuser,
                connlimit: role.connlimit,
                memberships: role
                    .memberships
                    .iter()
                    .map(|m| crate::storage::RoleMembership {
                        role: m.role.clone(),
                        grantor: m.grantor.clone(),
                    })
                    .collect(),
                valid_until: role.valid_until.clone(),
                created_xmin: *xmin,
                dropped_xmax: 0,
            };
            eng.db.roles.entry(name.clone()).or_default().push(r);
        }
        WalRecord::DropRole { name, xmax } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            match eng
                .db
                .roles
                .get_mut(name)
                .and_then(|vs| vs.iter_mut().find(|v| v.dropped_xmax == 0))
            {
                Some(r) => r.dropped_xmax = *xmax,
                None => eprintln!(
                    "WAL replay: skipping DropRole \"{}\": no live role version",
                    name
                ),
            }
        }
        WalRecord::AlterRole { name, role, xmin } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            let versions = eng.db.roles.entry(name.clone()).or_default();
            if let Some(prev) = versions.iter_mut().find(|v| v.dropped_xmax == 0) {
                prev.dropped_xmax = *xmin;
            }
            let r = crate::storage::Role {
                name: name.clone(),
                password: role.password.clone().map(WalVerifier::into_verifier),
                can_login: role.can_login,
                superuser: role.superuser,
                connlimit: role.connlimit,
                memberships: role
                    .memberships
                    .iter()
                    .map(|m| crate::storage::RoleMembership {
                        role: m.role.clone(),
                        grantor: m.grantor.clone(),
                    })
                    .collect(),
                valid_until: role.valid_until.clone(),
                created_xmin: *xmin,
                dropped_xmax: 0,
            };
            versions.push(r);
        }
        WalRecord::DbAcl { acl, xmin } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            eng.db.db_acl = acl.iter().cloned().map(WalAcl::into_entry).collect();
        }
    }
    match r {
        // v0.11: role records are applied by the match above; nothing left
        // to do here.
        WalRecord::CreateRole { .. }
        | WalRecord::DropRole { .. }
        | WalRecord::AlterRole { .. }
        | WalRecord::DbAcl { .. } => {}
        // v0.13: replication slot records are applied here.
        WalRecord::ReplSlotCreate {
            name,
            plugin,
            slot_type,
            restart_lsn,
        } => {
            eng.repl_slots.insert(
                name.clone(),
                crate::storage::ReplSlot {
                    name: name.clone(),
                    plugin: plugin.clone(),
                    slot_type: slot_type.clone(),
                    restart_lsn: *restart_lsn,
                    confirmed_flush_lsn: *restart_lsn,
                    active: false,
                },
            );
        }
        WalRecord::ReplSlotDrop { name } => {
            eng.repl_slots.remove(name);
        }
        WalRecord::ReplSlotFlush {
            name,
            restart_lsn,
            confirmed_flush_lsn,
        } => {
            if let Some(s) = eng.repl_slots.get_mut(name) {
                s.confirmed_flush_lsn = s.confirmed_flush_lsn.max(*confirmed_flush_lsn);
                s.restart_lsn = s.restart_lsn.max(*restart_lsn);
            }
        }
        WalRecord::CreateTable {
            name,
            columns,
            constraints,
            owner,
            acl,
            col_acl,
            xmin,
        } => {
            let mut t = Table::new(columns.clone(), *xmin);
            // v0.11
            t.owner = owner.clone();
            t.acl = acl.iter().cloned().map(WalAcl::into_entry).collect();
            t.col_acl = col_acl.iter().cloned().map(WalColAcl::into_entry).collect();
            match crate::sql::decode_constraints(constraints) {
                Ok(dc) => {
                    t.not_null = dc.not_null;
                    t.defaults = dc.defaults;
                    t.checks = dc.checks;
                    t.uniques = dc.uniques;
                    t.pkey = dc.pkey;
                    t.fks = dc.fks;
                }
                Err(e) => {
                    return Err(format!(
                        "WAL replay: bad constraints for table \"{}\": {}",
                        name, e
                    ));
                }
            }
            eng.db.tables.entry(name.clone()).or_default().push(t);
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
        WalRecord::DeleteRows {
            table,
            ids,
            old_rows: _,
            xmax,
        } => {
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
        // v0.13: UPDATE replays as delete-old + insert-new — the same
        // version-chain surgery live execution performs.
        WalRecord::UpdateRows {
            table,
            old,
            new,
            xmax,
        } => {
            let Some(t) = live_table(eng, table) else {
                eprintln!(
                    "WAL replay: skipping UpdateRows for \"{}\": no live table version",
                    table
                );
                return Ok(());
            };
            for r in old {
                match t.rows.iter_mut().find(|v| v.id == r.id) {
                    Some(v) => v.xmax = *xmax,
                    None => eprintln!(
                        "WAL replay: skipping UPDATE of missing row id {} in table \"{}\"",
                        r.id, table
                    ),
                }
            }
            for r in new {
                t.push_version(RowVersion {
                    id: r.id,
                    values: r.values.clone(),
                    xmin: *xmax,
                    xmax: 0,
                });
            }
            // Index the new versions, like live execution's
            // index_insert_row: recovery rebuilds indexes only for the
            // checkpoint image; replayed rows need their entries.
            let indexed: Vec<(u64, Vec<Value>)> =
                new.iter().map(|r| (r.id, r.values.clone())).collect();
            // `t`'s borrow ends at its last use above; eng.db is free again.
            for (id, values) in indexed {
                eng.db.index_insert_row(table, id, &values);
            }
        }
        // v0.13: replication slot metadata is applied by the match
        // above (like roles); nothing left to do here.
        WalRecord::CreateIndex {
            name,
            table,
            columns,
            unique,
            internal,
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
                    internal: *internal,
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
            None => eprintln!("WAL replay: skipping DropIndex \"{}\": no such index", name),
        },
        WalRecord::AlterTable {
            name,
            columns,
            constraints,
            copy_rows,
            owner,
            acl,
            col_acl,
            xmin,
        } => {
            let versions = eng.db.tables.entry(name.clone()).or_default();
            // The previous live version is superseded.
            if let Some(prev) = versions.iter_mut().find(|v| v.dropped_xmax == 0) {
                prev.dropped_xmax = *xmin;
            }
            let mut t = Table::new(columns.clone(), *xmin);
            match crate::sql::decode_constraints(constraints) {
                Ok(dc) => {
                    t.not_null = dc.not_null;
                    t.defaults = dc.defaults;
                    t.checks = dc.checks;
                    t.uniques = dc.uniques;
                    t.pkey = dc.pkey;
                    t.fks = dc.fks;
                }
                Err(e) => {
                    return Err(format!(
                        "WAL replay: bad constraints for table \"{}\": {}",
                        name, e
                    ));
                }
            }
            // Pure-metadata alters carry the rows forward; ADD/DROP COLUMN
            // rewrites them via InsertRows records.
            if *copy_rows {
                if let Some(prev) = versions.iter().find(|v| v.dropped_xmax == *xmin) {
                    t.rows = prev.rows.clone();
                }
            }
            // v0.11
            t.owner = owner.clone();
            t.acl = acl.iter().cloned().map(WalAcl::into_entry).collect();
            t.col_acl = col_acl.iter().cloned().map(WalColAcl::into_entry).collect();
            versions.push(t);
        }
        WalRecord::CreateView {
            name,
            query,
            col_aliases,
            deps,
            owner,
            xmin,
        } => {
            eng.db
                .views
                .entry(name.clone())
                .or_default()
                .push(crate::storage::ViewDef {
                    query: query.clone(),
                    col_aliases: col_aliases.clone(),
                    deps: deps.clone(),
                    created_xmin: *xmin,
                    dropped_xmax: 0,
                    // v0.11: owner comes from the record.
                    owner: owner.clone(),
                });
        }
        WalRecord::DropView { name, xmax } => {
            match eng
                .db
                .views
                .get_mut(name)
                .and_then(|vs| vs.iter_mut().find(|v| v.dropped_xmax == 0))
            {
                Some(v) => v.dropped_xmax = *xmax,
                None => eprintln!("WAL replay: skipping DropView \"{}\": no live view", name),
            }
        }
        WalRecord::CreateSequence { name, seq, xmin } => {
            eng.db
                .sequences
                .entry(name.clone())
                .or_default()
                .push(seq.clone().into_sequence(*xmin));
        }
        WalRecord::DropSequence { name, xmax } => {
            match eng
                .db
                .sequences
                .get_mut(name)
                .and_then(|vs| vs.iter_mut().find(|v| v.dropped_xmax == 0))
            {
                Some(s) => s.dropped_xmax = *xmax,
                None => eprintln!(
                    "WAL replay: skipping DropSequence \"{}\": no live sequence",
                    name
                ),
            }
        }
        WalRecord::AlterSequence { name, seq, xmin } => {
            let versions = eng.db.sequences.entry(name.clone()).or_default();
            let mut s = seq.clone().into_sequence(*xmin);
            // Carry the current value forward: ALTER SEQUENCE changes the
            // parameters, not the position.
            if let Some(prev) = versions.iter().find(|v| v.dropped_xmax == 0) {
                s.current = prev.current;
            }
            versions.push(s);
        }
        WalRecord::SeqAdvance {
            name,
            last_value,
            is_called,
        } => {
            match eng
                .db
                .sequences
                .get_mut(name)
                .and_then(|vs| vs.iter_mut().find(|v| v.dropped_xmax == 0))
            {
                Some(s) => {
                    s.current = Some(*last_value);
                    s.is_called = *is_called;
                }
                None => eprintln!(
                    "WAL replay: skipping SeqAdvance \"{}\": no live sequence",
                    name
                ),
            }
        }
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
                // v0.14: commit-time unique recheck. A concurrent txn may
                // have committed the same unique key after our
                // statement-time check ran; committing would create a
                // duplicate key. Fail the commit (40001 at the call site)
                // instead of corrupting the unique index.
                if let Some(cname) = eng
                    .db
                    .committed_unique_violation(&eng.txns, table, &values, *row_id, own)
                {
                    return Err(format!(
                        "duplicate key value violates unique constraint \"{}\" \
                         (committed by a concurrent transaction)",
                        cname
                    ));
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
                    // (v0.13: exec now emits UpdateRow for updates, so the
                    // paired_insert case below is vestigial — kept as a
                    // safety net in case any path still emits the pair.)
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
                // v0.13: the old values travel with the delete so the
                // logical decoder can report them without consulting
                // live (possibly vacuumed) storage.
                let old = WalRow {
                    id: *row_id,
                    xmin: v.xmin,
                    values: v.values.clone(),
                };
                match out.last_mut() {
                    Some(WalRecord::DeleteRows {
                        table: t,
                        ids,
                        old_rows,
                        xmax,
                    }) if t == table && *xmax == own => {
                        ids.push(*row_id);
                        old_rows.push(old);
                    }
                    _ => out.push(WalRecord::DeleteRows {
                        table: table.clone(),
                        ids: vec![*row_id],
                        old_rows: vec![old],
                        xmax: own,
                    }),
                }
            }
            // v0.13: first-class UPDATE op (see WriteOp::UpdateRow). The
            // write-write check mirrors the old paired-insert logic: if
            // a concurrent txn deleted the row, the update fails.
            WriteOp::UpdateRow {
                table,
                old_id,
                new_id,
                old_values,
                ..
            } => {
                let Some(v) = eng.db.find_row_version(*old_id) else {
                    i += 1;
                    continue;
                };
                if v.xmax != own {
                    return Err(format!(
                        "concurrent update on row id {} in table \"{}\"",
                        old_id, table
                    ));
                }
                let Some((new_values, table_live)) = own_row(eng, own, *new_id) else {
                    i += 1;
                    continue;
                };
                if !table_live {
                    i += 1;
                    continue; // table dropped by a committed concurrent txn
                }
                let old = WalRow {
                    id: *old_id,
                    xmin: v.xmin,
                    values: old_values.clone(),
                };
                let new = WalRow {
                    id: *new_id,
                    xmin: own,
                    values: new_values,
                };
                match out.last_mut() {
                    Some(WalRecord::UpdateRows {
                        table: t,
                        old: olds,
                        new: news,
                        xmax,
                    }) if t == table && *xmax == own => {
                        olds.push(old);
                        news.push(new);
                    }
                    _ => out.push(WalRecord::UpdateRows {
                        table: table.clone(),
                        old: vec![old],
                        new: vec![new],
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
                    constraints: crate::sql::encode_constraints(ours),
                    owner: ours.owner.clone(),
                    acl: ours.acl.iter().map(WalAcl::of).collect(),
                    col_acl: ours.col_acl.iter().map(WalColAcl::of).collect(),
                    xmin: own,
                });
            }
            // v0.9: ALTER TABLE swaps the table version; log the latest
            // version this transaction created.
            WriteOp::AlterTable {
                name, rewrite_rows, ..
            } => {
                let Some(ours) = eng
                    .db
                    .tables
                    .get(name)
                    .and_then(|vs| vs.iter().filter(|t| t.created_xmin == own).last())
                else {
                    i += 1;
                    continue;
                };
                out.push(WalRecord::AlterTable {
                    name: name.clone(),
                    columns: ours.columns.clone(),
                    constraints: crate::sql::encode_constraints(ours),
                    copy_rows: !rewrite_rows,
                    owner: ours.owner.clone(),
                    acl: ours.acl.iter().map(WalAcl::of).collect(),
                    col_acl: ours.col_acl.iter().map(WalColAcl::of).collect(),
                    xmin: own,
                });
            }
            WriteOp::CreateView { name } => {
                let Some(ours) = eng
                    .db
                    .views
                    .get(name)
                    .and_then(|vs| vs.iter().find(|v| v.created_xmin == own))
                else {
                    i += 1;
                    continue;
                };
                // First-committer-wins, like CREATE TABLE. v0.9: exclude
                // views we dropped ourselves in this transaction (OR REPLACE).
                let rival = eng.db.views.get(name).is_some_and(|vs| {
                    vs.iter().any(|v| {
                        v.created_xmin != own
                            && eng.xid_committed(v.created_xmin)
                            && (v.dropped_xmax == 0 || !eng.xid_committed(v.dropped_xmax))
                            && v.dropped_xmax != own
                    })
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateView {
                    name: name.clone(),
                    query: ours.query.clone(),
                    col_aliases: ours.col_aliases.clone(),
                    deps: ours.deps.clone(),
                    owner: ours.owner.clone(),
                    xmin: own,
                });
            }
            WriteOp::DropView { name, .. } => {
                let won = eng
                    .db
                    .views
                    .get(name)
                    .is_some_and(|vs| vs.iter().any(|v| v.dropped_xmax == own));
                if !won {
                    i += 1;
                    continue;
                }
                out.push(WalRecord::DropView {
                    name: name.clone(),
                    xmax: own,
                });
            }
            WriteOp::CreateSequence { name } => {
                let Some(ours) = eng
                    .db
                    .sequences
                    .get(name)
                    .and_then(|vs| vs.iter().find(|s| s.created_xmin == own))
                else {
                    i += 1;
                    continue;
                };
                let rival = eng.db.sequences.get(name).is_some_and(|vs| {
                    vs.iter().any(|s| {
                        s.created_xmin != own
                            && eng.xid_committed(s.created_xmin)
                            && (s.dropped_xmax == 0 || !eng.xid_committed(s.dropped_xmax))
                    })
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateSequence {
                    name: name.clone(),
                    seq: WalSequence::of(ours),
                    xmin: own,
                });
            }
            WriteOp::DropSequence { name, .. } => {
                let won = eng
                    .db
                    .sequences
                    .get(name)
                    .is_some_and(|vs| vs.iter().any(|s| s.dropped_xmax == own));
                if !won {
                    i += 1;
                    continue;
                }
                out.push(WalRecord::DropSequence {
                    name: name.clone(),
                    xmax: own,
                });
            }
            WriteOp::AlterSequence { name, .. } => {
                let Some(ours) = eng
                    .db
                    .sequences
                    .get(name)
                    .and_then(|vs| vs.iter().filter(|s| s.created_xmin == own).last())
                else {
                    i += 1;
                    continue;
                };
                out.push(WalRecord::AlterSequence {
                    name: name.clone(),
                    seq: WalSequence::of(ours),
                    xmin: own,
                });
            }
            // v0.9: sequence advances are non-transactional (Postgres
            // semantics): the value at commit time is logged, and abort
            // never rolls it back.
            WriteOp::SeqAdvance { name, .. } => {
                let Some((cur, called)) = eng
                    .db
                    .sequences
                    .get(name)
                    .and_then(|vs| vs.iter().find(|s| s.dropped_xmax == 0))
                    .and_then(|s| s.current.map(|c| (c, s.is_called)))
                else {
                    i += 1;
                    continue;
                };
                out.push(WalRecord::SeqAdvance {
                    name: name.clone(),
                    last_value: cur,
                    is_called: called,
                });
            }
            // v0.11: role DDL.
            WriteOp::CreateRole { name } => {
                let Some(ours) = eng
                    .db
                    .roles
                    .get(name)
                    .and_then(|vs| vs.iter().find(|r| r.created_xmin == own))
                else {
                    i += 1;
                    continue;
                };
                // First-committer-wins, like CREATE TABLE.
                let rival = eng.db.roles.get(name).is_some_and(|vs| {
                    vs.iter().any(|r| {
                        r.created_xmin != own
                            && eng.xid_committed(r.created_xmin)
                            && (r.dropped_xmax == 0 || !eng.xid_committed(r.dropped_xmax))
                    })
                });
                if rival {
                    return Err(format!("role \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateRole {
                    name: name.clone(),
                    role: WalRole::of(ours),
                    xmin: own,
                });
            }
            WriteOp::DropRole { name, .. } => {
                let won = eng
                    .db
                    .roles
                    .get(name)
                    .is_some_and(|vs| vs.iter().any(|r| r.dropped_xmax == own));
                if !won {
                    i += 1;
                    continue;
                }
                out.push(WalRecord::DropRole {
                    name: name.clone(),
                    xmax: own,
                });
            }
            WriteOp::AlterRole { name, .. } => {
                let Some(ours) = eng
                    .db
                    .roles
                    .get(name)
                    .and_then(|vs| vs.iter().filter(|r| r.created_xmin == own).last())
                else {
                    i += 1;
                    continue;
                };
                out.push(WalRecord::AlterRole {
                    name: name.clone(),
                    role: WalRole::of(ours),
                    xmin: own,
                });
            }
            // v0.11: database ACL replaces wholesale.
            WriteOp::DbAcl { .. } => {
                out.push(WalRecord::DbAcl {
                    acl: eng.db.db_acl.iter().map(WalAcl::of).collect(),
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
                        && (o.def.dropped_xmax == 0 || !eng.xid_committed(o.def.dropped_xmax))
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateIndex {
                    name: name.clone(),
                    table: ix.def.table.clone(),
                    columns: ix.def.col_names.clone(),
                    unique: ix.def.unique,
                    internal: ix.def.internal,
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
                    // Not our row version (e.g. an old table version with
                    // reused row ids after ALTER); keep searching.
                    continue;
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
    /// v0.13: cluster-wide random identifier, stable across restarts
    /// (like PostgreSQL's system identifier from initdb). Stored in
    /// `system.id` inside the data directory.
    system_id: u64,
}

/// v0.13: the stable cluster identifier. Read from `system.id` in the
/// data directory, or mint a random one (persisted via a tmp-file
/// rename + dir fsync, like the checkpoint) on first boot.
fn load_or_create_system_id(dir: &Path) -> std::io::Result<u64> {
    const NAME: &str = "system.id";
    let path = dir.join(NAME);
    if let Ok(bytes) = fs::read(&path) {
        if bytes.len() == 8 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes);
            let id = u64::from_be_bytes(b);
            if id != 0 {
                return Ok(id);
            }
        }
    }
    // Mint: /dev/urandom when available, else a time/pid hash. Never 0.
    let mut id = 0u64;
    if let Ok(mut f) = File::open("/dev/urandom") {
        use std::io::Read;
        let mut b = [0u8; 8];
        if f.read_exact(&mut b).is_ok() {
            id = u64::from_be_bytes(b);
        }
    }
    if id == 0 {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        let mut x = t ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
        // xorshift64* — good enough for a non-secret identifier.
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        id = x.wrapping_mul(0x2545F4914F6CDD1D);
        if id == 0 {
            id = 0x9E3779B97F4A7C15;
        }
    }
    let tmp = dir.join("system.id.tmp");
    fs::write(&tmp, id.to_be_bytes())?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &path)?;
    File::open(dir)?.sync_all()?;
    Ok(id)
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
        // v0.13: the system identifier is stable per data directory
        // (PostgreSQL's initdb assigns it once). Read it, or mint one.
        let system_id = load_or_create_system_id(dir)?;
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
            "rustgres v0.13 recovery: {} table version(s), replayed {} WAL batch(es) / {} record(s) from {}",
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
                system_id,
            },
        ))
    }

    /// v0.13: the cluster's stable random identifier (IDENTIFY_SYSTEM).
    pub fn system_id(&self) -> u64 {
        self.system_id
    }

    /// v0.13: the current logical end of the WAL — the LSN a new
    /// replication slot starts from, and what IDENTIFY_SYSTEM reports.
    pub fn current_lsn(&self) -> u64 {
        self.base_lsn + self.len.saturating_sub(WAL_HEADER_LEN)
    }

    /// v0.13: read every committed frame at/after `from_lsn`, in order.
    /// Returns (frame_lsn, txn_id, records). The walsender uses this to
    /// stream changes to a replication slot's confirmed position.
    pub fn read_frames_since(
        &mut self,
        from_lsn: u64,
    ) -> std::io::Result<Vec<(u64, u64, Vec<WalRecord>)>> {
        let mut out = Vec::new();
        self.file.seek(SeekFrom::Start(WAL_HEADER_LEN))?;
        loop {
            let phys = self.file.stream_position()?;
            let frame = match read_frame(&mut self.file)? {
                None => break, // clean EOF or torn tail
                Some(f) => f,
            };
            let frame_lsn = self.base_lsn + (phys - WAL_HEADER_LEN);
            if frame_lsn < from_lsn {
                continue;
            }
            out.push((frame_lsn, frame.txn_id, frame.records));
        }
        Ok(out)
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
                // v0.9: constraint/default metadata.
                body.str(&crate::sql::encode_constraints(t));
                // v0.11: owner and ACL.
                body.str(&t.owner);
                body.acl_list(&t.acl.iter().map(WalAcl::of).collect::<Vec<_>>());
                body.col_acl_list(&t.col_acl.iter().map(WalColAcl::of).collect::<Vec<_>>());
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
            // v0.9: persist the constraint-owned flag.
            ix_body.u8(ix.def.internal as u8);
            ix_body.u64(ix.def.created_xmin);
            ix_body.u64(0); // live index: no committed drop
            n_indexes += 1;
        }
        img.u32(n_indexes);
        img.bytes(&ix_body.buf);

        // v0.9: views. Only live, committed versions.
        let mut v_names: Vec<&String> = eng.db.views.keys().collect();
        v_names.sort();
        let mut v_body = Enc::new();
        let mut n_views = 0u32;
        for name in v_names {
            for v in &eng.db.views[name] {
                if !eng.xid_committed(v.created_xmin) {
                    continue;
                }
                if committed_xmax(eng, v.dropped_xmax) != 0 {
                    continue;
                }
                v_body.str(name);
                v_body.u64(v.created_xmin);
                v_body.u64(0); // live view: no committed drop
                v_body.str(&v.query);
                v_body.u32(v.col_aliases.len() as u32);
                for a in &v.col_aliases {
                    v_body.str(a);
                }
                v_body.u32(v.deps.len() as u32);
                for d in &v.deps {
                    v_body.str(d);
                }
                // v0.11: owner.
                v_body.str(&v.owner);
                n_views += 1;
            }
        }
        img.u32(n_views);
        img.bytes(&v_body.buf);

        // v0.9: sequences, including their current values (crash-safe).
        // Only live, committed versions.
        let mut s_names: Vec<&String> = eng.db.sequences.keys().collect();
        s_names.sort();
        let mut s_body = Enc::new();
        let mut n_seqs = 0u32;
        for name in s_names {
            for s in &eng.db.sequences[name] {
                if !eng.xid_committed(s.created_xmin) {
                    continue;
                }
                if committed_xmax(eng, s.dropped_xmax) != 0 {
                    continue;
                }
                s_body.str(name);
                s_body.u64(s.created_xmin);
                s_body.u64(0); // live sequence: no committed drop
                s_body.sequence(&WalSequence::of(s));
                n_seqs += 1;
            }
        }
        img.u32(n_seqs);
        img.bytes(&s_body.buf);

        // v0.11: roles. Only live, committed versions. The bootstrap
        // `postgres` role (created_xmin = 0) is always committed, so it
        // is always serialized.
        let mut r_names: Vec<&String> = eng.db.roles.keys().collect();
        r_names.sort();
        let mut r_body = Enc::new();
        let mut n_roles = 0u32;
        for name in r_names {
            for r in &eng.db.roles[name] {
                if !eng.xid_committed(r.created_xmin) {
                    continue;
                }
                if committed_xmax(eng, r.dropped_xmax) != 0 {
                    continue;
                }
                r_body.str(name);
                r_body.u64(r.created_xmin);
                r_body.u64(0); // live role: no committed drop
                r_body.wal_role(&WalRole::of(r));
                n_roles += 1;
            }
        }
        img.u32(n_roles);
        img.bytes(&r_body.buf);

        // v0.11: database ACL (CONNECT grants).
        let mut a_body = Enc::new();
        a_body.acl_list(&eng.db.db_acl.iter().map(WalAcl::of).collect::<Vec<_>>());
        img.bytes(&a_body.buf);

        // v0.13: replication slots (cluster-global). `active` is not
        // persisted — a slot is never active across a restart.
        let mut s_body = Enc::new();
        let mut n_slots: u32 = 0;
        for s in eng.repl_slots.values() {
            s_body.str(&s.name);
            s_body.str(&s.plugin);
            s_body.str(&s.slot_type);
            s_body.u64(s.restart_lsn);
            s_body.u64(s.confirmed_flush_lsn);
            n_slots += 1;
        }
        img.u32(n_slots);
        img.bytes(&s_body.buf);

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
            "rustgres v0.11 checkpoint: {} table version(s), WAL reset (base_lsn={})",
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
            "bad magic (this checkpoint is from an older rustgres; remove the data directory)",
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
        // v0.9: constraint/default metadata.
        let constraints = d.str().map_err(|e| bad(&e))?;
        // v0.11: owner and ACL.
        let owner = d.str().map_err(|e| bad(&e))?;
        let acl: Vec<crate::storage::AclEntry> = d
            .acl_list()
            .map_err(|e| bad(&e))?
            .into_iter()
            .map(WalAcl::into_entry)
            .collect();
        let col_acl: Vec<crate::storage::ColAclEntry> = d
            .col_acl_list()
            .map_err(|e| bad(&e))?
            .into_iter()
            .map(WalColAcl::into_entry)
            .collect();
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
        // v0.11
        __t.owner = owner;
        __t.acl = acl;
        __t.col_acl = col_acl;
        match crate::sql::decode_constraints(&constraints) {
            Ok(dc) => {
                __t.not_null = dc.not_null;
                __t.defaults = dc.defaults;
                __t.checks = dc.checks;
                __t.uniques = dc.uniques;
                __t.pkey = dc.pkey;
                __t.fks = dc.fks;
            }
            Err(e) => {
                return Err(bad(&format!(
                    "bad constraints for table \"{}\": {}",
                    name, e
                )));
            }
        }
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
        // v0.9: constraint-owned flag.
        let internal = d.u8().map_err(|e| bad(&e))? != 0;
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
                internal,
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
    // v0.9: views.
    let n_views = d.u32().map_err(|e| bad(&e))? as usize;
    for _ in 0..n_views {
        let name = d.str().map_err(|e| bad(&e))?;
        let created_xmin = d.u64().map_err(|e| bad(&e))?;
        let dropped_xmax = d.u64().map_err(|e| bad(&e))?;
        let query = d.str().map_err(|e| bad(&e))?;
        let n_a = d.u32().map_err(|e| bad(&e))? as usize;
        let mut col_aliases = Vec::with_capacity(n_a);
        for _ in 0..n_a {
            col_aliases.push(d.str().map_err(|e| bad(&e))?);
        }
        let n_d = d.u32().map_err(|e| bad(&e))? as usize;
        let mut deps = Vec::with_capacity(n_d);
        for _ in 0..n_d {
            deps.push(d.str().map_err(|e| bad(&e))?);
        }
        // v0.11: owner.
        let owner = d.str().map_err(|e| bad(&e))?;
        eng.db
            .views
            .entry(name.clone())
            .or_default()
            .push(crate::storage::ViewDef {
                query,
                col_aliases,
                deps,
                created_xmin,
                dropped_xmax,
                owner,
            });
    }
    // v0.9: sequences, with their current values.
    let n_seqs = d.u32().map_err(|e| bad(&e))? as usize;
    for _ in 0..n_seqs {
        let name = d.str().map_err(|e| bad(&e))?;
        let created_xmin = d.u64().map_err(|e| bad(&e))?;
        let dropped_xmax = d.u64().map_err(|e| bad(&e))?;
        let ws = d.sequence().map_err(|e| bad(&e))?;
        let mut s = ws.into_sequence(created_xmin);
        s.dropped_xmax = dropped_xmax;
        eng.db.sequences.entry(name).or_default().push(s);
    }
    // v0.11: roles. Replaces the bootstrap map from Engine::new so the
    // checkpoint is the single source of truth.
    let n_roles = d.u32().map_err(|e| bad(&e))? as usize;
    eng.db.roles.clear();
    for _ in 0..n_roles {
        let name = d.str().map_err(|e| bad(&e))?;
        let created_xmin = d.u64().map_err(|e| bad(&e))?;
        let dropped_xmax = d.u64().map_err(|e| bad(&e))?;
        let wr = d.wal_role().map_err(|e| bad(&e))?;
        eng.db
            .roles
            .entry(name.clone())
            .or_default()
            .push(crate::storage::Role {
                name,
                password: wr.password.map(WalVerifier::into_verifier),
                can_login: wr.can_login,
                superuser: wr.superuser,
                connlimit: wr.connlimit,
                memberships: wr
                    .memberships
                    .into_iter()
                    .map(|m| crate::storage::RoleMembership {
                        role: m.role,
                        grantor: m.grantor,
                    })
                    .collect(),
                valid_until: wr.valid_until,
                created_xmin,
                dropped_xmax,
            });
    }
    // v0.11: database ACL.
    eng.db.db_acl = d
        .acl_list()
        .map_err(|e| bad(&e))?
        .into_iter()
        .map(WalAcl::into_entry)
        .collect();
    // v0.13: replication slots. (v6 images only; v5 and older are
    // refused loudly by the version check at the top of this function.)
    let n_slots = d.u32().map_err(|e| bad(&e))?;
    for _ in 0..n_slots {
        let name = d.str().map_err(|e| bad(&e))?;
        let plugin = d.str().map_err(|e| bad(&e))?;
        let slot_type = d.str().map_err(|e| bad(&e))?;
        let restart_lsn = d.u64().map_err(|e| bad(&e))?;
        let confirmed_flush_lsn = d.u64().map_err(|e| bad(&e))?;
        eng.repl_slots.insert(
            name.clone(),
            crate::storage::ReplSlot {
                name,
                plugin,
                slot_type,
                restart_lsn,
                confirmed_flush_lsn,
                active: false,
            },
        );
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
        let empty_constraints =
            "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))".to_string();
        let cases = vec![
            WalRecord::CreateTable {
                name: "t".into(),
                columns: vec![("a".into(), ColType::Int)],
                constraints: empty_constraints.clone(),
                owner: "postgres".into(),
                acl: vec![],
                col_acl: vec![],
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
                old_rows: vec![
                    WalRow {
                        id: 7,
                        xmin: 3,
                        values: vec![Value::Int(1)],
                    },
                    WalRow {
                        id: 8,
                        xmin: 4,
                        values: vec![Value::Int(2)],
                    },
                    WalRow {
                        id: 9,
                        xmin: 4,
                        values: vec![Value::Int(3)],
                    },
                ],
                xmax: 6,
            },
            // v0.13: first-class UPDATE with old+new tuples.
            WalRecord::UpdateRows {
                table: "t".into(),
                old: vec![WalRow {
                    id: 7,
                    xmin: 4,
                    values: vec![Value::Int(1), Value::Text("a".into())],
                }],
                new: vec![WalRow {
                    id: 10,
                    xmin: 6,
                    values: vec![Value::Int(1), Value::Text("b".into())],
                }],
                xmax: 6,
            },
            // v0.13: replication slot metadata.
            WalRecord::ReplSlotCreate {
                name: "s1".into(),
                plugin: "rustgres_decoding".into(),
                slot_type: "logical".into(),
                restart_lsn: 0x1_0000_0020,
            },
            WalRecord::ReplSlotFlush {
                name: "s1".into(),
                restart_lsn: 0x1_0000_0020,
                confirmed_flush_lsn: 0x1_0000_0100,
            },
            WalRecord::ReplSlotDrop { name: "s1".into() },
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
        let empty_constraints =
            "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))".to_string();
        apply_record(
            &mut eng,
            &WalRecord::CreateTable {
                name: "t".into(),
                columns: vec![("a".into(), ColType::Int)],
                constraints: empty_constraints,
                owner: "postgres".into(),
                acl: vec![],
                col_acl: vec![],
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
                old_rows: vec![WalRow {
                    id: 12,
                    xmin: 5,
                    values: vec![Value::Int(1)],
                }],
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
                old_rows: vec![WalRow {
                    id: 1,
                    xmin: 2,
                    values: vec![Value::Int(1)],
                }],
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

    #[test]
    fn apply_record_slot_lifecycle() {
        let mut eng = Engine::new();
        // Create.
        apply_record(
            &mut eng,
            &WalRecord::ReplSlotCreate {
                name: "s1".into(),
                plugin: "rustgres_decoding".into(),
                slot_type: "logical".into(),
                restart_lsn: 100,
            },
        )
        .unwrap();
        let s = eng.repl_slots.get("s1").unwrap();
        assert_eq!(s.plugin, "rustgres_decoding");
        assert_eq!(s.slot_type, "logical");
        assert_eq!(s.restart_lsn, 100);
        assert_eq!(s.confirmed_flush_lsn, 100);
        assert!(!s.active); // replay never resurrects active state
        // Flush forward, then backward (flush never moves backwards).
        apply_record(
            &mut eng,
            &WalRecord::ReplSlotFlush {
                name: "s1".into(),
                restart_lsn: 100,
                confirmed_flush_lsn: 200,
            },
        )
        .unwrap();
        assert_eq!(eng.repl_slots["s1"].confirmed_flush_lsn, 200);
        assert_eq!(eng.repl_slots["s1"].restart_lsn, 100);
        apply_record(
            &mut eng,
            &WalRecord::ReplSlotFlush {
                name: "s1".into(),
                restart_lsn: 150,
                confirmed_flush_lsn: 150,
            },
        )
        .unwrap();
        assert_eq!(eng.repl_slots["s1"].confirmed_flush_lsn, 200);
        // restart_lsn follows the flushed record forward (never back).
        assert_eq!(eng.repl_slots["s1"].restart_lsn, 150);
        // Flush of a missing slot is a no-op, not an error.
        apply_record(
            &mut eng,
            &WalRecord::ReplSlotFlush {
                name: "ghost".into(),
                restart_lsn: 0,
                confirmed_flush_lsn: 9,
            },
        )
        .unwrap();
        // Drop.
        apply_record(&mut eng, &WalRecord::ReplSlotDrop { name: "s1".into() }).unwrap();
        assert!(!eng.repl_slots.contains_key("s1"));
    }

    #[test]
    fn system_id_is_stable_per_data_directory() {
        let dir = std::env::temp_dir().join("rg13-system-id-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // First open mints a non-zero id and persists exactly 8 bytes.
        let id1 = load_or_create_system_id(&dir).unwrap();
        assert_ne!(id1, 0);
        let raw = std::fs::read(dir.join("system.id")).unwrap();
        assert_eq!(raw.len(), 8);
        assert_eq!(u64::from_be_bytes(raw.try_into().unwrap()), id1);
        // Reopening the same directory returns the identical id.
        let id2 = load_or_create_system_id(&dir).unwrap();
        assert_eq!(id1, id2);
        // A corrupt (short) file is replaced, not propagated.
        std::fs::write(dir.join("system.id"), b"junk").unwrap();
        let id3 = load_or_create_system_id(&dir).unwrap();
        assert_ne!(id3, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
