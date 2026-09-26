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
//! Format version 19 (`RGSWAL19` / `RGSCHK15`) is NOT compatible with v1.04
//! or earlier: v1.05 WAL-logs the lazy toast-table reltoastrelid link
//! (`WalRecord::SetToastRelid`, record tag 28). The checkpoint format is
//! unchanged. Like every format bump, old data directories are refused
//! with a clear error instead of being misread.
//!
//! Format version 18 (`RGSWAL18` / `RGSCHK15`) is NOT compatible with v0.98
//! or earlier: v0.99 WAL-logs and checkpoints the sequence data type
//! (`WalSequence.seq_type`). Like every format bump, old data directories
//! are refused with a clear error instead of being misread.
//!
//! Format version 17 (`RGSWAL17` / `RGSCHK14`) is NOT compatible with v0.97
//! or earlier: v0.98 WAL-logs and checkpoints the sequence CACHE size
//! (`WalSequence.cache`). Like every format bump, old data directories
//! are refused with a clear error instead of being misread.
//!
//! Format version 16 (`RGSWAL16` / `RGSCHK13`) is NOT compatible with v0.95
//! or earlier: v0.96 WAL-logs table inheritance links (`inherits` on
//! `CreateTable`/`AlterTable`) and checkpoints them in table images.
//! Like every format bump, old data directories are refused with a
//! clear error instead of being misread.
//!
//! Format version 14 (`RGSWAL14` / `RGSCHK12`) is NOT compatible with v0.85
//! or earlier: v0.86 WAL-logs function/operator DDL (`CreateFunction` /
//! `DropFunction` / `CreateOperator` / `DropOperator`) and checkpoints
//! the function/operator catalogs. Like every format bump, old data
//! directories are refused with a clear error instead of being misread.
//! v0.85 was `RGSWAL13` / `RGSCHK11`; v0.82 was `RGSWAL12` / `RGSCHK10`;
//! v0.72 was `RGSWAL11` / `RGSCHK09`.
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
use crate::sql::{ArithOp, Expr, Literal};
use crate::storage::{
    ArrayElem, ArrayVal, ColType, Engine, Row, RowVersion, Table, Value, WriteOp,
};

const WAL_NAME: &str = "wal.log";
const CHKPT_NAME: &str = "checkpoint.dat";
const CHKPT_TMP: &str = "checkpoint.dat.tmp";
const CHKPT_MAGIC: &[u8; 8] = b"RGSCHK15";
/// v0.72: version 11 adds the `is_partitioned` flag to partition
/// metadata. v10 checkpoints are refused; remove
/// the data directory to start fresh (same policy as prior bumps).
/// v0.82: version 12 adds the named/shell type catalog (`eng.db.types`),
/// so CREATE TYPE survives checkpoint/restart. v11 checkpoints are
/// refused; remove the data directory to start fresh.
/// v0.85: version 13 adds domain definitions to the type catalog and
/// per-column composite/domain type use to table images. v12
/// checkpoints are refused; remove the data directory to start fresh.
/// v0.86: version 14 adds the function/operator catalogs
/// (`eng.db.functions` / `eng.db.operators`). v13 checkpoints are
/// refused; remove the data directory to start fresh.
/// v0.88: version 15 adds per-column direction / null-placement /
/// expression sources and the partial predicate to index images. v14
/// checkpoints are refused; remove the data directory to start fresh.
/// v0.96: version 16 adds the inheritance parent links (`inherits`) to
/// table images. v15 checkpoints are refused; remove the data
/// directory to start fresh.
/// v0.98: version 17 adds the sequence CACHE size to sequence
/// images. v16 checkpoints are refused; remove the data directory
/// to start fresh.
const CHKPT_VERSION: u32 = 17;
/// WAL file header: magic + base_lsn (u64, big-endian). Every frame's
/// logical sequence number is base_lsn + (physical offset - HEADER_LEN).
/// v0.13: `RGSWAL07` — DeleteRows now carries old row values, plus new
/// UpdateRows / ReplSlot* records. Old `RGSWAL06` files are refused loudly
/// (see read_wal_header); remove the data directory to start fresh.
/// v0.37: `RGSWAL08` — CreateTable carries the table OID and its
/// toast-table OID (reltoastrelid), AlterTable carries TOAST metadata.
/// Old `RGSWAL07` files are refused loudly.
/// v0.39: `RGSWAL09` — row versions carry their per-cell toast flags and
/// the (value id, compressed) provenance for value ids introduced by the
/// row, so TOAST metadata survives crash recovery via WAL replay (not
/// just checkpoints). Old `RGSWAL08` files are refused loudly.
/// v0.41: `RGSWAL10` — TOAST metadata carries the compression *method*
/// (`pglz`/`lz4`) per value id and per column (`col_compression`),
/// not just a compressed flag. Old `RGSWAL09` files are refused loudly.
/// v0.72: `RGSWAL11` — partition metadata carries the `is_partitioned`
/// flag. Old `RGSWAL10` files are refused loudly.
/// v0.85: `RGSWAL13` — `CreateType` carries the domain definition
/// (base type + s-expr CHECKs/DEFAULT); `CreateTable`/`AlterTable`
/// carry per-column `composite_types`/`domain_types`/`domain_elem`.
/// Old `RGSWAL12` files are refused loudly.
/// v0.86: `RGSWAL14` — function/operator DDL records
/// (`CreateFunction`/`DropFunction`/`CreateOperator`/`DropOperator`,
/// tags 24-27). Old `RGSWAL13` files are refused loudly.
/// v0.88: `RGSWAL15` — `CreateIndex` carries per-column direction /
/// null-placement / expression sources plus the partial predicate.
/// Old `RGSWAL14` files are refused loudly.
/// v0.96: `RGSWAL16` — `CreateTable`/`AlterTable` carry the
/// inheritance parent links (`inherits`). Old `RGSWAL15` files are
/// refused loudly.
/// v0.99: `RGSWAL18` — sequence records carry the data type.
/// v1.05: `RGSWAL19` — new `SetToastRelid` record (tag 28).
/// Old `RGSWAL18` files are refused loudly.
const WAL_MAGIC: &[u8; 8] = b"RGSWAL19";
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
///
/// v0.39: `toast` carries the per-cell toast value ids, parallel to
/// `values` (`RowVersion::toast`); `toast_meta` carries the
/// (value id, compressed) provenance for value ids *introduced* by
/// this row, so replay can rebuild `Table::toast_info`. Both are empty
/// for rows that never touched TOAST.
#[derive(Clone, Debug, PartialEq)]
pub struct WalRow {
    pub id: u64,
    pub xmin: u64,
    pub values: Row,
    pub toast: Vec<u32>,
    pub toast_meta: Vec<(u32, u8)>,
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
        /// v0.37: table OID (pg_class.oid).
        oid: u32,
        /// v0.37: toast table OID (pg_class.reltoastrelid).
        toast_relid: u32,
        /// v0.41: per-column explicit compression methods as
        /// ToastCompression codes, 0 = default (`col_compression`).
        col_compression: Vec<u8>,
        /// v0.85: named composite type per column (`composite_types`);
        /// empty on old records.
        composite_types: Vec<Option<String>>,
        /// v0.85: domain type per column (`domain_types`); empty on old
        /// records.
        domain_types: Vec<Option<String>>,
        /// v0.85: domain applies to array elements (`domain_elem`); empty
        /// on old records.
        domain_elem: Vec<bool>,
        /// v0.96: inheritance parent links (`inherits`); empty on old
        /// records.
        inherits: Vec<String>,
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
        /// v0.88: per-key-column DESC flags (parallel to `columns`).
        desc: Vec<bool>,
        /// v0.88: per-key-column NULLS FIRST flags (parallel to
        /// `columns`).
        nulls_first: Vec<bool>,
        /// v0.88: per-key-column expression source (`Some`) vs plain
        /// column (`None`); parallel to `columns`.
        exprs: Vec<Option<String>>,
        /// v0.88: partial-index predicate source, if any.
        predicate: Option<String>,
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
        /// v0.37: TOAST metadata (SET STORAGE / SET (...) alters).
        oid: u32,
        toast_relid: u32,
        toast_target: u32,
        col_storage: Vec<u8>,
        /// v0.41: per-column explicit compression methods as
        /// ToastCompression codes, 0 = default (`col_compression`).
        col_compression: Vec<u8>,
        /// v0.85: named composite type per column (`composite_types`);
        /// empty on old records.
        composite_types: Vec<Option<String>>,
        /// v0.85: domain type per column (`domain_types`); empty on old
        /// records.
        domain_types: Vec<Option<String>>,
        /// v0.85: domain applies to array elements (`domain_elem`); empty
        /// on old records.
        domain_elem: Vec<bool>,
        /// v0.96: inheritance parent links (`inherits`); empty on old
        /// records.
        inherits: Vec<String>,
        next_value_id: u32,
        /// (value_id, compression-method-code) pairs.
        toast_info: Vec<(u32, u8)>,
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
    // --- v0.82: type DDL. `CREATE TYPE` carries the full definition so
    // replay rebuilds the catalog entry exactly; `DROP TYPE` removes it.
    CreateType {
        name: String,
        like_base: Option<String>,
        composite: Option<Vec<(String, ColType, Option<String>)>>,
        /// v0.85: domain definition (None for shell/LIKE/composite types).
        domain: Option<WalDomain>,
        xmin: u64,
    },
    DropType {
        name: String,
        xmax: u64,
    },
    // --- v0.86: function/operator DDL. CREATE carries the full
    // definition so replay rebuilds the catalog entry exactly; DROP
    // removes it. The function body travels as the raw string and is
    // re-parsed on replay.
    CreateFunction {
        name: String,
        arg_names: Vec<Option<String>>,
        arg_types: Vec<String>,
        ret_type: String,
        returns_set: bool,
        lang: u8,
        body: String,
        volatility: u8,
        strict: bool,
        xmin: u64,
    },
    DropFunction {
        name: String,
        /// v0.87: the specific overload's argument types.
        arg_types: Vec<String>,
        xmax: u64,
    },
    CreateOperator {
        name: String,
        defs: Vec<WalOperDef>,
        xmin: u64,
    },
    DropOperator {
        name: String,
        xmax: u64,
    },
    // --- v1.05: the lazy toast-table safety net's reltoastrelid link
    // (`pg_class.reltoastrelid`). Staged by
    // `WriteOp::SetToastRelid`; replay sets the link on the table's
    // live version (creating nothing — the toast table itself rides in
    // its own CreateTable record).
    SetToastRelid {
        name: String,
        toast_relid: u32,
        xmin: u64,
    },
}

/// v0.86: one operator definition in WAL/checkpoint form.
#[derive(Clone, Debug, PartialEq)]
pub struct WalOperDef {
    pub procedure: String,
    pub leftarg: Option<String>,
    pub rightarg: Option<String>,
    pub commutator: Option<String>,
    pub negator: Option<String>,
    pub hashes: bool,
    pub merges: bool,
}

/// One GRANT entry in WAL/checkpoint form (v0.11).
#[derive(Clone, Debug, PartialEq)]
pub struct WalAcl {
    pub role: String,
    pub privs: u32,
}

/// v0.85: a domain definition in WAL/checkpoint form. CHECKs and the
/// DEFAULT travel as s-expr strings (`sql::encode_domain_checks` /
/// `encode_domain_default`); the base type uses the binary `col_type`
/// codec.
#[derive(Clone, Debug, PartialEq)]
pub struct WalDomain {
    pub base: ColType,
    pub base_named: Option<String>,
    pub base_domain: Option<String>,
    pub checks: String,
    pub not_null: bool,
    pub default: String,
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
    /// v0.99: sequence data type (0 = smallint, 1 = integer, 2 = bigint).
    pub seq_type: u8,
    pub start: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
    /// v0.98: CACHE size (Postgres default 1). Stored for catalog
    /// fidelity; the engine hands out values one at a time.
    pub cache: i64,
    /// Last value returned by nextval; i64::MIN sentinel = never called.
    pub current: i64,
    pub current_is_set: bool,
    pub is_called: bool,
    /// v0.11: owning role and explicit USAGE grants.
    pub owner: String,
    pub acl: Vec<WalAcl>,
    /// v0.65: serial ownership (table, column, temp_session).
    pub owned_by: Option<(String, String, Option<u64>)>,
}

impl WalSequence {
    pub fn of(s: &crate::storage::Sequence) -> Self {
        WalSequence {
            name: s.name.clone(),
            seq_type: match s.seq_type {
                crate::sql::SeqType::SmallInt => 0,
                crate::sql::SeqType::Integer => 1,
                crate::sql::SeqType::BigInt => 2,
            },
            start: s.start,
            increment: s.increment,
            min_value: s.min_value,
            max_value: s.max_value,
            cycle: s.cycle,
            cache: s.cache,
            current: s.current.unwrap_or(i64::MIN),
            current_is_set: s.current.is_some(),
            is_called: s.is_called,
            owner: s.owner.clone(),
            acl: s.acl.iter().map(WalAcl::of).collect(),
            owned_by: s.owned_by.clone(),
        }
    }

    pub fn into_sequence(self, created_xmin: u64) -> crate::storage::Sequence {
        crate::storage::Sequence {
            name: self.name,
            seq_type: match self.seq_type {
                0 => crate::sql::SeqType::SmallInt,
                1 => crate::sql::SeqType::Integer,
                _ => crate::sql::SeqType::BigInt,
            },
            start: self.start,
            increment: self.increment,
            min_value: self.min_value,
            max_value: self.max_value,
            cycle: self.cycle,
            cache: self.cache,
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
            owned_by: self.owned_by,
        }
    }
}

// ---------------------------------------------------------------------------
// Binary encoding helpers (big-endian, length-prefixed strings)
// ---------------------------------------------------------------------------

struct Enc {
    buf: Vec<u8>,
}

/// v0.69: encode a RangeBound for checkpoints.
fn encode_range_bound(body: &mut Enc, rb: &crate::storage::RangeBound) {
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
fn parse_partition_expr(s: &str) -> Option<crate::sql::Expr> {
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
struct DbgParser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> DbgParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        self.skip_ws();
        if self.eat(b) { Some(()) } else { None }
    }

    /// Parse a bare identifier (`Column`, `Add`, `None`, ...).
    fn ident(&mut self) -> Option<&'a str> {
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

    fn expect_ident(&mut self, want: &str) -> Option<()> {
        if self.ident()? == want {
            Some(())
        } else {
            None
        }
    }

    /// Parse `"..."` with Rust `escape_debug` escapes.
    fn string(&mut self) -> Option<String> {
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

    fn number(&mut self) -> Option<&'a str> {
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
    fn field(&mut self, name: &str) -> Option<()> {
        self.expect_ident(name)?;
        self.expect(b':')
    }

    fn parse_expr(&mut self) -> Option<Expr> {
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

    fn parse_literal(&mut self) -> Option<Literal> {
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
fn decode_range_bound(d: &mut Dec) -> Result<crate::storage::RangeBound, String> {
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

fn bad(e: &dyn std::fmt::Display) -> String {
    format!("bad checkpoint: {}", e)
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

    /// v0.39: encode one row version, including its per-cell toast flags
    /// (parallel to the values) and the (value id, compressed) provenance
    /// for value ids introduced by this row. Used by every record that
    /// carries row versions (InsertRows, DeleteRows, UpdateRows).
    fn wal_row(&mut self, r: &WalRow) {
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
                oid,
                toast_relid,
                col_compression,
                composite_types,
                domain_types,
                domain_elem,
                inherits,
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
                self.u64(*xmin);
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
                next_value_id,
                toast_info,
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
                self.u32(*next_value_id);
                self.u32(toast_info.len() as u32);
                for (k, c) in toast_info {
                    self.u32(*k);
                    self.u8(*c);
                }
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

    fn acl_list(&mut self, acl: &[WalAcl]) {
        self.u32(acl.len() as u32);
        for e in acl {
            self.str(&e.role);
            self.u32(e.privs);
        }
    }

    /// v0.85: encode a `Vec<Option<String>>` (composite/domain type names).
    fn opt_str_list(&mut self, v: &[Option<String>]) {
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
    fn bool_list(&mut self, v: &[bool]) {
        self.u32(v.len() as u32);
        for b in v {
            self.u8(if *b { 1 } else { 0 });
        }
    }

    /// v0.96: encode a `Vec<String>` (inheritance parent links).
    fn str_list(&mut self, v: &[String]) {
        self.u32(v.len() as u32);
        for s in v {
            self.str(s);
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

    /// Read a string straight into an `Arc<str>`. This copies the bytes
    /// one time. `str()` then `Value::text()` copies them two times.
    fn str_arc(&mut self) -> Result<std::sync::Arc<str>, String> {
        let n = self.u32()? as usize;
        let b = self.take(n)?;
        match std::str::from_utf8(b) {
            Ok(s) => Ok(s.into()),
            Err(_) => Err(self.err("invalid utf-8 in string")),
        }
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

    fn value(&mut self) -> Result<Value, String> {
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
                Ok(Value::Array(ArrayVal {
                    elem,
                    dims,
                    lower,
                    elems,
                }))
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
            t => Err(self.err(&format!("unknown value tag {}", t))),
        }
    }

    /// v0.79: decode the PG array OID used by both `col_type` tag 22
    /// and `value` tag 17 into the element type.
    fn array_elem(&mut self) -> Result<ArrayElem, String> {
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
            t => Err(self.err(&format!("unknown array element OID {}", t))),
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
                let xmin = self.u64()?;
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
                let next_value_id = self.u32()?;
                let n_ti = self.u32()? as usize;
                let mut toast_info = Vec::with_capacity(n_ti);
                for _ in 0..n_ti {
                    toast_info.push((self.u32()?, self.u8()?));
                }
                let xmin = self.u64()?;
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
                    next_value_id,
                    toast_info,
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

    fn sequence(&mut self) -> Result<WalSequence, String> {
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

    /// v0.85: decode a `Vec<Option<String>>` (composite/domain type names).
    fn opt_str_list_d(&mut self) -> Result<Vec<Option<String>>, String> {
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
    fn bool_list_d(&mut self) -> Result<Vec<bool>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.u8()? != 0);
        }
        Ok(out)
    }

    /// v0.96: decode a `Vec<String>` (inheritance parent links).
    fn str_list_d(&mut self) -> Result<Vec<String>, String> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.str()?);
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
        | WalRecord::AlterSequence { xmin, .. }
        // v1.05: the lazy reltoastrelid link carries its xid too.
        | WalRecord::SetToastRelid { xmin, .. } => {
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
        // v0.82: type DDL replay — rebuild the catalog entry exactly.
        WalRecord::CreateType {
            name,
            like_base,
            composite,
            domain,
            xmin,
        } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            // v0.85: restore the domain definition from the s-expr
            // strings. A corrupt record fails replay loudly rather than
            // silently dropping constraints.
            let domain = match domain {
                Some(d) => Some(crate::storage::DomainDef {
                    base: d.base.clone(),
                    base_named: d.base_named.clone(),
                    base_domain: d.base_domain.clone(),
                    checks: crate::sql::decode_domain_checks(&d.checks)
                        .map_err(|e| format!("corrupt domain checks in WAL: {}", e))?,
                    not_null: d.not_null,
                    default: crate::sql::decode_domain_default(&d.default)
                        .map_err(|e| format!("corrupt domain default in WAL: {}", e))?,
                }),
                None => None,
            };
            eng.db.types.insert(
                name.clone(),
                crate::storage::ShellType {
                    like_base: like_base.clone(),
                    composite: composite.clone(),
                    domain,
                },
            );
        }
        WalRecord::DropType { name, xmax } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            eng.db.types.remove(name);
        }
        // v0.86: function/operator DDL replay — rebuild the catalog
        // entries exactly. The body is re-parsed; a corrupt body fails
        // replay loudly.
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
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            let lang = match lang {
                0 => crate::sql::FuncLang::Sql,
                1 => crate::sql::FuncLang::Internal,
                2 => crate::sql::FuncLang::Plpgsql,
                _ => return Err(format!("corrupt function language in WAL: {}", lang)),
            };
            let volatility = match volatility {
                0 => crate::sql::FuncVolatility::Volatile,
                1 => crate::sql::FuncVolatility::Stable,
                2 => crate::sql::FuncVolatility::Immutable,
                _ => {
                    return Err(format!(
                        "corrupt function volatility in WAL: {}",
                        volatility
                    ));
                }
            };
            let (parsed, plpgsql) =
                crate::exec::rebuild_function_bodies(lang, arg_names, body, *returns_set);
            // v0.87: replay appends to the overload list (replacing any
            // existing overload with the same signature).
            {
                let def = crate::storage::FuncDef {
                    name: name.clone(),
                    arg_names: arg_names.clone(),
                    arg_types: arg_types.clone(),
                    ret_type: ret_type.clone(),
                    returns_set: *returns_set,
                    lang,
                    body: body.clone(),
                    parsed,
                    // v1.01: multi-statement plpgsql bodies.
                    plpgsql,
                    volatility,
                    strict: *strict,
                };
                let overloads = eng.db.functions.entry(name.clone()).or_default();
                if let Some(slot) = overloads.iter_mut().find(|f| f.arg_types == def.arg_types) {
                    *slot = def;
                } else {
                    overloads.push(def);
                }
            }
        }
        WalRecord::DropFunction {
            name,
            arg_types,
            xmax,
        } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            // v0.87: remove the specific overload by signature.
            if let Some(overloads) = eng.db.functions.get_mut(name) {
                overloads.retain(|f| &f.arg_types != arg_types);
                if overloads.is_empty() {
                    eng.db.functions.remove(name);
                }
            }
        }
        WalRecord::CreateOperator { name, defs, xmin } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            eng.db.operators.insert(
                name.clone(),
                defs.iter()
                    .map(|d| crate::storage::OperDef {
                        name: name.clone(),
                        procedure: d.procedure.clone(),
                        leftarg: d.leftarg.clone(),
                        rightarg: d.rightarg.clone(),
                        commutator: d.commutator.clone(),
                        negator: d.negator.clone(),
                        hashes: d.hashes,
                        merges: d.merges,
                    })
                    .collect(),
            );
        }
        WalRecord::DropOperator { name, xmax } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            eng.db.operators.remove(name);
        }
    }
    match r {
        // v0.11: role records are applied by the match above; nothing left
        // to do here. v0.82: type records likewise.
        WalRecord::CreateRole { .. }
        | WalRecord::DropRole { .. }
        | WalRecord::AlterRole { .. }
        | WalRecord::DbAcl { .. }
        | WalRecord::CreateType { .. }
        | WalRecord::DropType { .. }
        // v0.86: function/operator records likewise.
        | WalRecord::CreateFunction { .. }
        | WalRecord::DropFunction { .. }
        | WalRecord::CreateOperator { .. }
        | WalRecord::DropOperator { .. } => {}
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
            oid,
            toast_relid,
            col_compression,
            composite_types,
            domain_types,
            domain_elem,
            inherits,
            xmin,
        } => {
            let mut t = Table::new(columns.clone(), *xmin);
            // v0.11
            t.owner = owner.clone();
            t.acl = acl.iter().cloned().map(WalAcl::into_entry).collect();
            t.col_acl = col_acl.iter().cloned().map(WalColAcl::into_entry).collect();
            // v0.37: restore the table OID and its toast table link.
            t.oid = *oid;
            t.toast_relid = *toast_relid;
            // v0.96: restore inheritance parent links.
            t.inherits = inherits.clone();
            // v0.85: restore composite/domain type use (empty vecs on old
            // records mean "no domain use").
            t.composite_types = composite_types.clone();
            t.domain_types = domain_types.clone();
            t.domain_elem = domain_elem.clone();
            // v0.41: restore per-column compression methods (0 = default).
            t.col_compression = col_compression
                .iter()
                .map(|&code| {
                    if code == 0 {
                        Ok(None)
                    } else {
                        crate::storage::ToastCompression::from_code(code)
                            .map(Some)
                            .ok_or_else(|| {
                                format!("WAL replay: unknown toast compression code {}", code)
                            })
                    }
                })
                .collect::<Result<Vec<_>, String>>()?;
            // v0.37: advance the OID counter past every restored OID, or
            // the next CREATE after replay would hand out a colliding OID.
            eng.db.next_oid = eng
                .db
                .next_oid
                .max(oid.wrapping_add(1))
                .max(toast_relid.wrapping_add(1));
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
            let inserted: Vec<(u64, Row)> = {
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
                    // v0.39: restore the per-cell toast flags (they were
                    // WAL-logged per row) and rebuild the table's
                    // toast_info provenance from the record's metadata.
                    let mut rv = RowVersion::plain(row.id, row.values.clone(), row.xmin);
                    if row.toast.len() == row.values.len() {
                        rv.toast = row.toast.clone();
                    }
                    for (vid, compressed) in &row.toast_meta {
                        // v0.41: the byte is a method code (0 = plain).
                        let info = decode_toast_method(*compressed)?;
                        t.toast_info.insert(*vid, info);
                        if t.next_value_id <= *vid {
                            t.next_value_id = vid + 1;
                        }
                    }
                    t.push_version(rv);
                    out.push((row.id, row.values.clone()));
                }
                out
            };
            for (id, values) in &inserted {
                eng.db
                    .index_insert_row(table, *id, values, crate::storage::NO_SESSION);
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
                // v0.22: O(1) via row_index — the old linear scan made
                // crash recovery quadratic in version count (a 10 MB WAL
                // replayed in ~53 s; now ~1 s).
                match t.row_pos(*id).and_then(|p| t.rows.get_mut(p)) {
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
                // v0.22: O(1) via row_index — see DeleteRows above.
                match t.row_pos(r.id).and_then(|p| t.rows.get_mut(p)) {
                    Some(v) => v.xmax = *xmax,
                    None => eprintln!(
                        "WAL replay: skipping UPDATE of missing row id {} in table \"{}\"",
                        r.id, table
                    ),
                }
            }
            for r in new {
                // v0.39: restore toast flags and metadata, like InsertRows.
                let mut rv = RowVersion::plain(r.id, r.values.clone(), *xmax);
                if r.toast.len() == r.values.len() {
                    rv.toast = r.toast.clone();
                }
                for (vid, compressed) in &r.toast_meta {
                    // v0.41: the byte is a method code (0 = plain).
                    let info = decode_toast_method(*compressed)?;
                    t.toast_info.insert(*vid, info);
                    if t.next_value_id <= *vid {
                        t.next_value_id = vid + 1;
                    }
                }
                t.push_version(rv);
            }
            // Index the new versions, like live execution's
            // index_insert_row: recovery rebuilds indexes only for the
            // checkpoint image; replayed rows need their entries.
            let indexed: Vec<(u64, Row)> = new.iter().map(|r| (r.id, r.values.clone())).collect();
            // `t`'s borrow ends at its last use above; eng.db is free again.
            for (id, values) in indexed {
                eng.db
                    .index_insert_row(table, id, &values, crate::storage::NO_SESSION);
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
            desc,
            nulls_first,
            exprs,
            predicate,
            xmin,
        } => {
            // Rebuild the index from the table's current rows: at recovery
            // every row present comes from a committed batch, so indexing
            // all versions is correct (visibility filters at scan time).
            // v0.88: expression / partial indexes are catalog-only (never
            // built); their definitions still round-trip for fidelity.
            let built: Option<Index> = (|| {
                let t = live_table(eng, table)?;
                let n = columns.len();
                let mut cols = Vec::with_capacity(n);
                let mut any_expr = false;
                for (i, c) in columns.iter().enumerate() {
                    let is_expr = exprs.get(i).and_then(|e| e.as_ref()).is_some();
                    if is_expr {
                        any_expr = true;
                        cols.push(usize::MAX);
                    } else {
                        cols.push(t.column_index(c)?);
                    }
                }
                let pad_bool = |v: &[bool]| {
                    let mut out = v.to_vec();
                    out.resize(n, false);
                    out
                };
                let mut ex = exprs.clone();
                ex.resize(n, None);
                let planner_usable = !any_expr && predicate.is_none();
                let mut ix = Index::new(IndexDef {
                    name: name.clone(),
                    table: table.clone(),
                    cols,
                    col_names: columns.clone(),
                    unique: *unique,
                    internal: *internal,
                    created_xmin: *xmin,
                    dropped_xmax: 0,
                    desc: pad_bool(desc),
                    nulls_first: pad_bool(nulls_first),
                    exprs: ex,
                    predicate: predicate.clone(),
                    planner_usable,
                });
                if planner_usable {
                    for r in &t.rows {
                        let key = ix.key_for(&r.values);
                        ix.insert(key, r.id);
                    }
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
            oid,
            toast_relid,
            toast_target,
            col_storage,
            col_compression,
            composite_types,
            domain_types,
            domain_elem,
            inherits,
            next_value_id,
            toast_info,
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
            // v0.37: TOAST metadata.
            t.oid = *oid;
            t.toast_relid = *toast_relid;
            // v0.37: keep the OID counter ahead of restored OIDs (see
            // the CreateTable replay above).
            eng.db.next_oid = eng
                .db
                .next_oid
                .max(oid.wrapping_add(1))
                .max(toast_relid.wrapping_add(1));
            t.toast_target = *toast_target;
            t.col_storage = col_storage.clone();
            // v0.85: restore composite/domain type use (empty vecs on old
            // records mean "no domain use").
            t.composite_types = composite_types.clone();
            t.domain_types = domain_types.clone();
            t.domain_elem = domain_elem.clone();
            // v0.96: restore inheritance parent links.
            t.inherits = inherits.clone();
            // v0.41: per-column compression methods (0 = default).
            t.col_compression = col_compression
                .iter()
                .map(|&code| {
                    if code == 0 {
                        Ok(None)
                    } else {
                        crate::storage::ToastCompression::from_code(code)
                            .map(Some)
                            .ok_or_else(|| {
                                format!("WAL replay: unknown toast compression code {}", code)
                            })
                    }
                })
                .collect::<Result<Vec<_>, String>>()?;
            t.next_value_id = (*next_value_id).max(1);
            t.toast_info = toast_info
                .iter()
                .map(|(k, c)| {
                    // v0.41: the byte is a method code (0 = plain).
                    decode_toast_method(*c).map(|info| (*k, info))
                })
                .collect::<Result<_, _>>()?;
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
        // v1.05: replay the lazy reltoastrelid link onto the table's
        // live version. The toast table itself was created by its own
        // CreateTable record earlier in the log (op order is
        // preserved), so only the link needs restoring here.
        WalRecord::SetToastRelid {
            name, toast_relid, ..
        } => match live_table(eng, name) {
            Some(t) => {
                t.toast_relid = *toast_relid;
            }
            None => eprintln!(
                "WAL replay: skipping SetToastRelid \"{}\": no live table",
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
    session: u64,
) -> Result<Vec<WalRecord>, String> {
    let mut out: Vec<WalRecord> = Vec::new();
    let mut i = 0;
    while i < writes.len() {
        let op = &writes[i];
        match op {
            WriteOp::InsertRow { table, row_id } => {
                // v0.22: temp-table DML is never WAL-logged (PostgreSQL
                // doesn't log temp-table data either): it is
                // session-local and dies with the session. Row ids are
                // globally unique, so this test is exact.
                if eng.db.row_id_in_temp(*row_id) {
                    i += 1;
                    continue;
                }
                let Some((values, toast, table_live)) = own_row(eng, own, *row_id) else {
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
                    .committed_unique_violation(&eng.txns, table, &values, *row_id, own, session)
                {
                    return Err(format!(
                        "duplicate key value violates unique constraint \"{}\" \
                         (committed by a concurrent transaction)",
                        cname
                    ));
                }
                // v0.39: the row's toast flags and value-id provenance
                // ride along so replay restores TOAST metadata.
                let toast_meta = toast_meta_for(eng, table, &toast);
                let row = WalRow {
                    id: *row_id,
                    xmin: own,
                    values,
                    toast,
                    toast_meta,
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
                // v0.22: temp-table DML is never WAL-logged (see above).
                if eng.db.row_id_in_temp(*row_id) {
                    i += 1;
                    continue;
                }
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
                // live (possibly vacuumed) storage. v0.39: the old
                // toast flags ride along too (no new provenance: the
                // value ids were introduced by an earlier record).
                let old = WalRow {
                    id: *row_id,
                    xmin: v.xmin,
                    values: v.values.clone(),
                    toast: v.toast.clone(),
                    toast_meta: Vec::new(),
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
                // v0.22: temp-table DML is never WAL-logged (see above).
                // Both versions live in the same table; one check suffices.
                if eng.db.row_id_in_temp(*old_id) {
                    i += 1;
                    continue;
                }
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
                let Some((new_values, new_toast, table_live)) = own_row(eng, own, *new_id) else {
                    i += 1;
                    continue;
                };
                if !table_live {
                    i += 1;
                    continue; // table dropped by a committed concurrent txn
                }
                // v0.39: old rows carry their toast flags (provenance was
                // logged when the value ids were introduced); the new row
                // carries its flags plus fresh provenance.
                let old = WalRow {
                    id: *old_id,
                    xmin: v.xmin,
                    values: old_values.clone(),
                    toast: v.toast.clone(),
                    toast_meta: Vec::new(),
                };
                let new_toast_meta = toast_meta_for(eng, table, &new_toast);
                let new = WalRow {
                    id: *new_id,
                    xmin: own,
                    values: new_values,
                    toast: new_toast,
                    toast_meta: new_toast_meta,
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
                    oid: ours.oid,
                    toast_relid: ours.toast_relid,
                    // v0.41: compression method codes, 0 = default.
                    col_compression: ours
                        .col_compression
                        .iter()
                        .map(|m| m.map(|m| m.code()).unwrap_or(0))
                        .collect(),
                    // v0.85: composite/domain type use.
                    composite_types: ours.composite_types.clone(),
                    domain_types: ours.domain_types.clone(),
                    domain_elem: ours.domain_elem.clone(),
                    // v0.96: inheritance parent links.
                    inherits: ours.inherits.clone(),
                    xmin: own,
                });
            }
            // v1.05: the lazy reltoastrelid link. Only log when the
            // table's live version still carries the link we set
            // (conditional like the other ops); replay restores it.
            WriteOp::SetToastRelid {
                table, toast_relid, ..
            } => {
                let linked = eng
                    .db
                    .tables
                    .get(table)
                    .and_then(|vs| vs.iter().find(|t| t.dropped_xmax == 0))
                    .is_some_and(|t| t.toast_relid == *toast_relid);
                if linked {
                    out.push(WalRecord::SetToastRelid {
                        name: table.clone(),
                        toast_relid: *toast_relid,
                        xmin: own,
                    });
                }
                i += 1;
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
                    // v0.37: TOAST metadata.
                    oid: ours.oid,
                    toast_relid: ours.toast_relid,
                    toast_target: ours.toast_target,
                    col_storage: ours.col_storage.clone(),
                    // v0.41: compression method codes, 0 = default.
                    col_compression: ours
                        .col_compression
                        .iter()
                        .map(|m| m.map(|m| m.code()).unwrap_or(0))
                        .collect(),
                    // v0.85: composite/domain type use.
                    composite_types: ours.composite_types.clone(),
                    domain_types: ours.domain_types.clone(),
                    domain_elem: ours.domain_elem.clone(),
                    // v0.96: inheritance parent links.
                    inherits: ours.inherits.clone(),
                    next_value_id: ours.next_value_id,
                    toast_info: ours
                        .toast_info
                        .iter()
                        .map(|(k, v)| (*k, if v.compressed { v.method.code() } else { 0 }))
                        .collect(),
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
            // v0.22: temp-table DDL is session-local and never WAL-logged.
            // The temp table itself is already in `temp_tables`; the op
            // only exists for statement-atomic undo.
            WriteOp::CreateTempTable { .. }
            | WriteOp::DropTempTable { .. }
            | WriteOp::AlterTempTable { .. } => {
                i += 1;
                continue;
            }
            // v0.82: type DDL is WAL-logged (definitions survive
            // checkpoint/restart). The new definition is read from the
            // type map, which the executor updated; absent means a later
            // DROP TYPE in the same txn removed it, so only the drop is
            // logged. DROP TYPE logs the removal.
            WriteOp::CreateType { name, .. } => {
                if let Some(st) = eng.db.types.get(name) {
                    out.push(WalRecord::CreateType {
                        name: name.clone(),
                        like_base: st.like_base.clone(),
                        composite: st.composite.clone(),
                        // v0.85: domain definitions travel as s-expr
                        // strings (sql::encode_domain_checks /
                        // encode_domain_default).
                        domain: st.domain.as_ref().map(|d| WalDomain {
                            base: d.base.clone(),
                            base_named: d.base_named.clone(),
                            base_domain: d.base_domain.clone(),
                            checks: crate::sql::encode_domain_checks(&d.checks),
                            not_null: d.not_null,
                            default: crate::sql::encode_domain_default(&d.default),
                        }),
                        xmin: own,
                    });
                }
                i += 1;
            }
            WriteOp::DropType { name, .. } => {
                out.push(WalRecord::DropType {
                    name: name.clone(),
                    xmax: own,
                });
                i += 1;
            }
            // v0.86: function/operator DDL is WAL-logged (definitions
            // survive checkpoint/restart). The new definition is read
            // from the catalog maps, which the executor updated; absent
            // means a later DROP in the same txn removed it, so only
            // the drop is logged.
            // v0.87: WAL-log the specific overload that was added.
            WriteOp::CreateFunction { name, added, .. } => {
                if eng
                    .db
                    .functions
                    .get(name)
                    .is_some_and(|ovs| ovs.iter().any(|f| f.arg_types == added.arg_types))
                {
                    let f = added;
                    out.push(WalRecord::CreateFunction {
                        name: name.clone(),
                        arg_names: f.arg_names.clone(),
                        arg_types: f.arg_types.clone(),
                        ret_type: f.ret_type.clone(),
                        returns_set: f.returns_set,
                        lang: match f.lang {
                            crate::sql::FuncLang::Sql => 0,
                            crate::sql::FuncLang::Internal => 1,
                            // v0.97: bounded plpgsql (body desugared to
                            // SQL at CREATE; the lang tag round-trips so
                            // restored catalogs keep the declared
                            // language).
                            crate::sql::FuncLang::Plpgsql => 2,
                        },
                        body: f.body.clone(),
                        volatility: match f.volatility {
                            crate::sql::FuncVolatility::Volatile => 0,
                            crate::sql::FuncVolatility::Stable => 1,
                            crate::sql::FuncVolatility::Immutable => 2,
                        },
                        strict: f.strict,
                        xmin: own,
                    });
                }
                i += 1;
            }
            // v0.87: WAL-log the specific overload's signature.
            WriteOp::DropFunction { name, prev, .. } => {
                let arg_types = prev
                    .as_ref()
                    .map(|f| f.arg_types.clone())
                    .unwrap_or_default();
                out.push(WalRecord::DropFunction {
                    name: name.clone(),
                    arg_types,
                    xmax: own,
                });
                i += 1;
            }
            WriteOp::CreateOperator { name, .. } => {
                if let Some(defs) = eng.db.operators.get(name) {
                    out.push(WalRecord::CreateOperator {
                        name: name.clone(),
                        defs: defs
                            .iter()
                            .map(|d| WalOperDef {
                                procedure: d.procedure.clone(),
                                leftarg: d.leftarg.clone(),
                                rightarg: d.rightarg.clone(),
                                commutator: d.commutator.clone(),
                                negator: d.negator.clone(),
                                hashes: d.hashes,
                                merges: d.merges,
                            })
                            .collect(),
                        xmin: own,
                    });
                }
                i += 1;
            }
            WriteOp::DropOperator { name, .. } => {
                out.push(WalRecord::DropOperator {
                    name: name.clone(),
                    xmax: own,
                });
                i += 1;
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
                    desc: ix.def.desc.clone(),
                    nulls_first: ix.def.nulls_first.clone(),
                    exprs: ix.def.exprs.clone(),
                    predicate: ix.def.predicate.clone(),
                    xmin: own,
                });
            }
            // v0.87: temp index DDL is never WAL-logged (session-local,
            // dies with the session, like temp tables).
            WriteOp::CreateTempIndex { .. } | WriteOp::DropTempIndex { .. } => {
                i += 1;
                continue;
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
/// v0.39: also returns the row version's per-cell toast flags, so the
/// commit path can WAL-log them (and the value-id provenance).
fn own_row(eng: &Engine, own: u64, row_id: u64) -> Option<(Row, Vec<u32>, bool)> {
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
                return Some((v.values.clone(), v.toast.clone(), !dropped));
            }
        }
    }
    None
}

/// v0.39: collect the (value id, compressed) provenance for the nonzero
/// toast flags of one row, from the live table's `toast_info`. Returns an
/// empty vec when the row carries no toasted cells.
/// v0.41: the byte is the compression *method* code (0 = not
/// compressed, `b'p'`/`b'l'`), so the method survives WAL replay.
fn toast_meta_for(eng: &Engine, table: &str, flags: &[u32]) -> Vec<(u32, u8)> {
    if flags.iter().all(|f| *f == 0) {
        return Vec::new();
    }
    let Some(t) = eng.db.tables.get(table).and_then(|v| v.last()) else {
        return Vec::new();
    };
    let mut out: Vec<(u32, u8)> = Vec::new();
    for &vid in flags {
        if vid == 0 || out.iter().any(|(k, _)| *k == vid) {
            continue;
        }
        let code = t
            .toast_info
            .get(&vid)
            .map(|i| if i.compressed { i.method.code() } else { 0 })
            .unwrap_or(0);
        out.push((vid, code));
    }
    out
}

/// v0.41: decode a WAL/checkpoint toast method byte (0 = not
/// compressed, otherwise a `ToastCompression` code). Unknown codes are
/// a loud decode error — never silently reinterpreted.
fn decode_toast_method(byte: u8) -> Result<crate::storage::ToastInfo, String> {
    if byte == 0 {
        return Ok(crate::storage::ToastInfo {
            compressed: false,
            method: crate::storage::ToastCompression::Pglz,
        });
    }
    match crate::storage::ToastCompression::from_code(byte) {
        Some(method) => Ok(crate::storage::ToastInfo {
            compressed: true,
            method,
        }),
        None => Err(format!("unknown toast compression code {}", byte)),
    }
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
            "rustgres v{} recovery: {} table version(s), replayed {} WAL batch(es) / {} record(s) from {}",
            crate::server::SERVER_VERSION,
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
        // v0.37: table OID counter.
        img.u32(eng.db.next_oid);
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
                // v0.37: TOAST metadata.
                body.u32(t.oid);
                body.u32(t.toast_relid);
                body.u32(t.toast_target);
                body.u32(t.col_storage.len() as u32);
                for s in &t.col_storage {
                    body.u8(*s);
                }
                // v0.41: per-column explicit compression methods
                // (ToastCompression codes, `p`/`l`), parallel to columns.
                body.u32(t.col_compression.len() as u32);
                for m in &t.col_compression {
                    body.u8(m.map(|m| m.code()).unwrap_or(0));
                }
                // v0.85: composite/domain type use per column.
                body.opt_str_list(&t.composite_types);
                body.opt_str_list(&t.domain_types);
                body.bool_list(&t.domain_elem);
                let mut toast_keys: Vec<u32> = t.toast_info.keys().copied().collect();
                toast_keys.sort_unstable();
                body.u32(toast_keys.len() as u32);
                for k in &toast_keys {
                    body.u32(*k);
                    // v0.41: method code (0 = not compressed).
                    let info = &t.toast_info[k];
                    body.u8(if info.compressed {
                        info.method.code()
                    } else {
                        0
                    });
                }
                body.u32(t.next_value_id);
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
                    for v in r.values.iter() {
                        body.value(v);
                    }
                    // v0.37: per-cell toast flags (parallel to values).
                    for f in r.toast.iter() {
                        body.u32(*f);
                    }
                }
                // v0.69: partition metadata.
                if let Some(p) = &t.partition {
                    body.u8(1);
                    body.u8(match p.method {
                        crate::storage::PartMethod::Range => 0,
                        crate::storage::PartMethod::List => 1,
                        crate::storage::PartMethod::Hash => 2,
                    });
                    body.u32(p.key.len() as u32);
                    for k in &p.key {
                        body.u64(k.col as u64);
                        if let Some(e) = &k.expr {
                            body.u8(1);
                            // v0.69: serialize the key expression as
                            // debug string; parsed back on load for the
                            // common cases (column, arith, func).
                            // FULL support is a gap (see decode below).
                            body.str(&format!("{:?}", e));
                        } else {
                            body.u8(0);
                        }
                    }
                    if let Some(b) = &p.bound {
                        body.u8(1);
                        match b {
                            crate::storage::PartBound::List { values, has_null } => {
                                body.u8(0);
                                body.u32(values.len() as u32);
                                for v in values {
                                    body.value(v);
                                }
                                body.u8(if *has_null { 1 } else { 0 });
                            }
                            crate::storage::PartBound::Range { lower, upper } => {
                                body.u8(1);
                                body.u32(lower.len() as u32);
                                for rb in lower {
                                    encode_range_bound(&mut body, rb);
                                }
                                body.u32(upper.len() as u32);
                                for rb in upper {
                                    encode_range_bound(&mut body, rb);
                                }
                            }
                            crate::storage::PartBound::Hash { modulus, remainder } => {
                                body.u8(2);
                                body.u32(*modulus);
                                body.u32(*remainder);
                            }
                        }
                    } else {
                        body.u8(0);
                    }
                    body.u8(if p.is_default { 1 } else { 0 });
                    if let Some(par) = &p.parent {
                        body.u8(1);
                        body.str(par);
                    } else {
                        body.u8(0);
                    }
                    body.u32(p.children.len() as u32);
                    for c in &p.children {
                        body.str(c);
                    }
                    // v0.72: whether this table is itself partitioned
                    // (a childless partitioned table is not a leaf).
                    body.u8(if p.is_partitioned { 1 } else { 0 });
                } else {
                    body.u8(0);
                }
                // v0.96: inheritance parent links.
                body.str_list(&t.inherits);
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
            // v0.88: per-column direction / null placement / expression
            // sources, plus the partial predicate.
            ix_body.bool_list(&ix.def.desc);
            ix_body.bool_list(&ix.def.nulls_first);
            ix_body.opt_str_list(&ix.def.exprs);
            match &ix.def.predicate {
                Some(p) => {
                    ix_body.u8(1);
                    ix_body.str(p);
                }
                None => ix_body.u8(0),
            }
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

        // v0.82: named/shell types (`eng.db.types`). Sorted for a
        // deterministic image. Only committed state is meaningful here,
        // but types carry no xid (v0.22 design); snapshotting the live
        // map is strictly better than the old behavior (types vanished
        // entirely). An uncommitted CREATE TYPE caught by a checkpoint
        // is a known minor gap, documented in the ShellType docs.
        let mut t_names: Vec<&String> = eng.db.types.keys().collect();
        t_names.sort();
        img.u32(t_names.len() as u32);
        for name in t_names {
            let st = &eng.db.types[name];
            img.str(name);
            match &st.like_base {
                Some(b) => {
                    img.u8(1);
                    img.str(b);
                }
                None => img.u8(0),
            }
            match &st.composite {
                Some(fields) => {
                    img.u8(1);
                    img.u32(fields.len() as u32);
                    for (fname, fty, nested) in fields {
                        img.str(fname);
                        img.col_type(fty);
                        match nested {
                            Some(n) => {
                                img.u8(1);
                                img.str(n);
                            }
                            None => img.u8(0),
                        }
                    }
                }
                None => img.u8(0),
            }
            // v0.85: domain definition (present only for domains).
            match &st.domain {
                Some(d) => {
                    img.u8(1);
                    img.col_type(&d.base);
                    match &d.base_named {
                        Some(n) => {
                            img.u8(1);
                            img.str(n);
                        }
                        None => img.u8(0),
                    }
                    match &d.base_domain {
                        Some(n) => {
                            img.u8(1);
                            img.str(n);
                        }
                        None => img.u8(0),
                    }
                    img.u8(if d.not_null { 1 } else { 0 });
                    img.str(&crate::sql::encode_domain_checks(&d.checks));
                    img.str(&crate::sql::encode_domain_default(&d.default));
                }
                None => img.u8(0),
            }
        }

        // v0.86: user-defined functions (`eng.db.functions`). Sorted
        // for a deterministic image. Like types, functions carry no
        // xid; snapshotting the live map is strictly better than
        // dropping them (an uncommitted CREATE FUNCTION caught by a
        // checkpoint is a known minor gap, as with types).
        // v0.87: checkpoint each overload.
        let mut f_names: Vec<&String> = eng.db.functions.keys().collect();
        f_names.sort();
        img.u32(f_names.len() as u32);
        for name in f_names {
            let overloads = &eng.db.functions[name];
            img.str(name);
            img.u32(overloads.len() as u32);
            for f in overloads {
                img.u32(f.arg_names.len() as u32);
                for n in &f.arg_names {
                    match n {
                        Some(s) => {
                            img.u8(1);
                            img.str(s);
                        }
                        None => img.u8(0),
                    }
                }
                img.u32(f.arg_types.len() as u32);
                for t in &f.arg_types {
                    img.str(t);
                }
                img.str(&f.ret_type);
                img.u8(if f.returns_set { 1 } else { 0 });
                img.u8(match f.lang {
                    crate::sql::FuncLang::Sql => 0,
                    crate::sql::FuncLang::Internal => 1,
                    // v0.97: bounded plpgsql.
                    crate::sql::FuncLang::Plpgsql => 2,
                });
                img.str(&f.body);
                img.u8(match f.volatility {
                    crate::sql::FuncVolatility::Volatile => 0,
                    crate::sql::FuncVolatility::Stable => 1,
                    crate::sql::FuncVolatility::Immutable => 2,
                });
                img.u8(if f.strict { 1 } else { 0 });
            }
        }

        // v0.86: user-defined operators (`eng.db.operators`), same
        // treatment as functions.
        let mut o_names: Vec<&String> = eng.db.operators.keys().collect();
        o_names.sort();
        img.u32(o_names.len() as u32);
        for name in o_names {
            let defs = &eng.db.operators[name];
            img.str(name);
            img.u32(defs.len() as u32);
            for d in defs {
                img.str(&d.procedure);
                match &d.leftarg {
                    Some(s) => {
                        img.u8(1);
                        img.str(s);
                    }
                    None => img.u8(0),
                }
                match &d.rightarg {
                    Some(s) => {
                        img.u8(1);
                        img.str(s);
                    }
                    None => img.u8(0),
                }
                match &d.commutator {
                    Some(s) => {
                        img.u8(1);
                        img.str(s);
                    }
                    None => img.u8(0),
                }
                match &d.negator {
                    Some(s) => {
                        img.u8(1);
                        img.str(s);
                    }
                    None => img.u8(0),
                }
                img.u8(if d.hashes { 1 } else { 0 });
                img.u8(if d.merges { 1 } else { 0 });
            }
        }

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
    // v0.37: table OID counter.
    eng.db.next_oid = d
        .u32()
        .map_err(|e| bad(&e))?
        .max(crate::storage::toast_consts::FIRST_USER_OID);
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
        // v0.37: TOAST metadata.
        let oid = d.u32().map_err(|e| bad(&e))?;
        let toast_relid = d.u32().map_err(|e| bad(&e))?;
        let toast_target = d.u32().map_err(|e| bad(&e))?;
        let n_storage = d.u32().map_err(|e| bad(&e))? as usize;
        let mut col_storage = Vec::with_capacity(n_storage);
        for _ in 0..n_storage {
            col_storage.push(d.u8().map_err(|e| bad(&e))?);
        }
        // v0.41: per-column explicit compression methods (0 = default).
        let n_compression = d.u32().map_err(|e| bad(&e))? as usize;
        let mut col_compression = Vec::with_capacity(n_compression);
        for _ in 0..n_compression {
            let code = d.u8().map_err(|e| bad(&e))?;
            let method = if code == 0 {
                None
            } else {
                Some(
                    crate::storage::ToastCompression::from_code(code)
                        .ok_or_else(|| bad(&format!("unknown toast compression code {}", code)))?,
                )
            };
            col_compression.push(method);
        }
        // v0.85: composite/domain type use per column.
        let n_ct = d.u32().map_err(|e| bad(&e))? as usize;
        let mut composite_types = Vec::with_capacity(n_ct);
        for _ in 0..n_ct {
            composite_types.push(if d.u8().map_err(|e| bad(&e))? != 0 {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            });
        }
        let n_dt = d.u32().map_err(|e| bad(&e))? as usize;
        let mut domain_types = Vec::with_capacity(n_dt);
        for _ in 0..n_dt {
            domain_types.push(if d.u8().map_err(|e| bad(&e))? != 0 {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            });
        }
        let n_de = d.u32().map_err(|e| bad(&e))? as usize;
        let mut domain_elem = Vec::with_capacity(n_de);
        for _ in 0..n_de {
            domain_elem.push(d.u8().map_err(|e| bad(&e))? != 0);
        }
        let n_toast_info = d.u32().map_err(|e| bad(&e))? as usize;
        let mut toast_info = std::collections::HashMap::new();
        for _ in 0..n_toast_info {
            let k = d.u32().map_err(|e| bad(&e))?;
            let code = d.u8().map_err(|e| bad(&e))?;
            // v0.41: the byte is a method code (0 = plain).
            let info = decode_toast_method(code).map_err(|e| bad(&e))?;
            toast_info.insert(k, info);
        }
        let next_value_id = d.u32().map_err(|e| bad(&e))?.max(1);
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
            // v0.37: per-cell toast flags.
            let mut toast = Vec::with_capacity(n_vals);
            for _ in 0..n_vals {
                toast.push(d.u32().map_err(|e| bad(&e))?);
            }
            if id >= eng.txns.next_row_id {
                eng.txns.next_row_id = id + 1;
            }
            let mut __rv = RowVersion::plain(id, Row::new(values), xmin);
            __rv.xmax = xmax;
            __rv.toast = toast;
            rows.push(__rv);
        }
        let mut __t = Table::new(columns, created_xmin);
        __t.dropped_xmax = dropped_xmax;
        // v0.11
        __t.owner = owner;
        __t.acl = acl;
        __t.col_acl = col_acl;
        // v0.37
        __t.oid = oid;
        __t.toast_relid = toast_relid;
        __t.toast_target = toast_target;
        __t.col_storage = col_storage;
        __t.col_compression = col_compression;
        __t.toast_info = toast_info;
        __t.next_value_id = next_value_id;
        // v0.85: composite/domain type use.
        __t.composite_types = composite_types;
        __t.domain_types = domain_types;
        __t.domain_elem = domain_elem;
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
        // v0.69: partition metadata.
        let has_partition = d.u8().map_err(|e| bad(&e))? != 0;
        if has_partition {
            let method_tag = d.u8().map_err(|e| bad(&e))?;
            let method = match method_tag {
                0 => crate::storage::PartMethod::Range,
                1 => crate::storage::PartMethod::List,
                2 => crate::storage::PartMethod::Hash,
                _ => return Err(bad("bad partition method tag")),
            };
            let n_keys = d.u32().map_err(|e| bad(&e))? as usize;
            let mut key = Vec::with_capacity(n_keys);
            for _ in 0..n_keys {
                let col = d.u64().map_err(|e| bad(&e))? as usize;
                let has_expr = d.u8().map_err(|e| bad(&e))? != 0;
                let expr = if has_expr {
                    let s = d.str().map_err(|e| bad(&e))?;
                    // v0.69: parse the debug-format expression back.
                    // Only the common cases are supported; others become
                    // None (routing will fail — documented gap).
                    parse_partition_expr(&s)
                } else {
                    None
                };
                key.push(crate::storage::PartKey { col, expr });
            }
            let has_bound = d.u8().map_err(|e| bad(&e))? != 0;
            let bound = if has_bound {
                let bound_tag = d.u8().map_err(|e| bad(&e))?;
                match bound_tag {
                    0 => {
                        let n_vals = d.u32().map_err(|e| bad(&e))? as usize;
                        let mut values = Vec::with_capacity(n_vals);
                        for _ in 0..n_vals {
                            values.push(d.value().map_err(|e| bad(&e))?);
                        }
                        let has_null = d.u8().map_err(|e| bad(&e))? != 0;
                        Some(crate::storage::PartBound::List { values, has_null })
                    }
                    1 => {
                        let n_lo = d.u32().map_err(|e| bad(&e))? as usize;
                        let mut lower = Vec::with_capacity(n_lo);
                        for _ in 0..n_lo {
                            lower.push(decode_range_bound(&mut d).map_err(|e| bad(&e))?);
                        }
                        let n_hi = d.u32().map_err(|e| bad(&e))? as usize;
                        let mut upper = Vec::with_capacity(n_hi);
                        for _ in 0..n_hi {
                            upper.push(decode_range_bound(&mut d).map_err(|e| bad(&e))?);
                        }
                        Some(crate::storage::PartBound::Range { lower, upper })
                    }
                    2 => {
                        let modulus = d.u32().map_err(|e| bad(&e))?;
                        let remainder = d.u32().map_err(|e| bad(&e))?;
                        Some(crate::storage::PartBound::Hash { modulus, remainder })
                    }
                    _ => return Err(bad("bad partition bound tag")),
                }
            } else {
                None
            };
            let is_default = d.u8().map_err(|e| bad(&e))? != 0;
            let has_parent = d.u8().map_err(|e| bad(&e))? != 0;
            let parent = if has_parent {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            };
            let n_children = d.u32().map_err(|e| bad(&e))? as usize;
            let mut children = Vec::with_capacity(n_children);
            for _ in 0..n_children {
                children.push(d.str().map_err(|e| bad(&e))?);
            }
            // v0.72: the is_partitioned flag (format version 11).
            let is_partitioned = d.u8().map_err(|e| bad(&e))? != 0;
            __t.partition = Some(crate::storage::PartitionInfo {
                method,
                key,
                bound,
                is_default,
                parent,
                children,
                is_partitioned,
            });
        }
        // v0.96: inheritance parent links (format version 16).
        __t.inherits = d.str_list_d().map_err(|e| bad(&e))?;
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
        // v0.88: per-column direction / null placement / expression
        // sources, plus the partial predicate.
        let mut desc = d.bool_list_d().map_err(|e| bad(&e))?;
        let mut nulls_first = d.bool_list_d().map_err(|e| bad(&e))?;
        let mut exprs = d.opt_str_list_d().map_err(|e| bad(&e))?;
        let has_predicate = d.u8().map_err(|e| bad(&e))? != 0;
        let predicate = if has_predicate {
            Some(d.str().map_err(|e| bad(&e))?)
        } else {
            None
        };
        desc.resize(n_cols, false);
        nulls_first.resize(n_cols, false);
        exprs.resize(n_cols, None);
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
            let mut any_expr = false;
            for (i, c) in col_names.iter().enumerate() {
                if exprs[i].is_some() {
                    any_expr = true;
                    cols.push(usize::MAX);
                } else {
                    cols.push(t.column_index(c).ok_or_else(|| {
                        bad_idx(format!("unknown column \"{}\" in table \"{}\"", c, table))
                    })?);
                }
            }
            // v0.88: expression / partial indexes are catalog-only (never
            // built); their definitions still round-trip for fidelity.
            let planner_usable = !any_expr && predicate.is_none();
            let mut ix = Index::new(IndexDef {
                name: name.clone(),
                table: table.clone(),
                cols,
                col_names,
                unique,
                internal,
                created_xmin,
                dropped_xmax,
                desc,
                nulls_first,
                exprs,
                predicate,
                planner_usable,
            });
            if planner_usable {
                for r in &t.rows {
                    let key = ix.key_for(&r.values);
                    ix.insert(key, r.id);
                }
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
    // v0.82: named/shell types (v12 images only; v11 and older are
    // refused loudly by the version check at the top of this function).
    let n_types = d.u32().map_err(|e| bad(&e))?;
    for _ in 0..n_types {
        let name = d.str().map_err(|e| bad(&e))?;
        let like_base = if d.u8().map_err(|e| bad(&e))? != 0 {
            Some(d.str().map_err(|e| bad(&e))?)
        } else {
            None
        };
        let composite = if d.u8().map_err(|e| bad(&e))? != 0 {
            let n = d.u32().map_err(|e| bad(&e))? as usize;
            let mut fields = Vec::with_capacity(n);
            for _ in 0..n {
                let fname = d.str().map_err(|e| bad(&e))?;
                let fty = d.col_type().map_err(|e| bad(&e))?;
                let nested = if d.u8().map_err(|e| bad(&e))? != 0 {
                    Some(d.str().map_err(|e| bad(&e))?)
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
        let domain = if d.u8().map_err(|e| bad(&e))? != 0 {
            let base = d.col_type().map_err(|e| bad(&e))?;
            let base_named = if d.u8().map_err(|e| bad(&e))? != 0 {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            };
            let base_domain = if d.u8().map_err(|e| bad(&e))? != 0 {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            };
            let not_null = d.u8().map_err(|e| bad(&e))? != 0;
            let checks = d.str().map_err(|e| bad(&e))?;
            let default = d.str().map_err(|e| bad(&e))?;
            Some(crate::storage::DomainDef {
                base,
                base_named,
                base_domain,
                checks: crate::sql::decode_domain_checks(&checks).map_err(|e| bad(&e))?,
                not_null,
                default: crate::sql::decode_domain_default(&default).map_err(|e| bad(&e))?,
            })
        } else {
            None
        };
        eng.db.types.insert(
            name,
            crate::storage::ShellType {
                like_base,
                composite,
                domain,
            },
        );
    }
    // v0.87: user-defined functions (overload lists).
    let n_funcs = d.u32().map_err(|e| bad(&e))? as usize;
    for _ in 0..n_funcs {
        let name = d.str().map_err(|e| bad(&e))?;
        let n_overloads = d.u32().map_err(|e| bad(&e))? as usize;
        let mut overloads = Vec::with_capacity(n_overloads);
        for _ in 0..n_overloads {
            let n = d.u32().map_err(|e| bad(&e))? as usize;
            let mut arg_names = Vec::with_capacity(n);
            for _ in 0..n {
                arg_names.push(if d.u8().map_err(|e| bad(&e))? != 0 {
                    Some(d.str().map_err(|e| bad(&e))?)
                } else {
                    None
                });
            }
            let n = d.u32().map_err(|e| bad(&e))? as usize;
            let mut arg_types = Vec::with_capacity(n);
            for _ in 0..n {
                arg_types.push(d.str().map_err(|e| bad(&e))?);
            }
            let ret_type = d.str().map_err(|e| bad(&e))?;
            let returns_set = d.u8().map_err(|e| bad(&e))? != 0;
            let lang = match d.u8().map_err(|e| bad(&e))? {
                0 => crate::sql::FuncLang::Sql,
                1 => crate::sql::FuncLang::Internal,
                2 => crate::sql::FuncLang::Plpgsql,
                b => return Err(bad(&format!("corrupt function language {}", b))),
            };
            let body = d.str().map_err(|e| bad(&e))?;
            let volatility = match d.u8().map_err(|e| bad(&e))? {
                0 => crate::sql::FuncVolatility::Volatile,
                1 => crate::sql::FuncVolatility::Stable,
                2 => crate::sql::FuncVolatility::Immutable,
                b => return Err(bad(&format!("corrupt function volatility {}", b))),
            };
            let strict = d.u8().map_err(|e| bad(&e))? != 0;
            let (parsed, plpgsql) =
                crate::exec::rebuild_function_bodies(lang, &arg_names, &body, returns_set);
            overloads.push(crate::storage::FuncDef {
                name: name.clone(),
                arg_names,
                arg_types,
                ret_type,
                returns_set,
                lang,
                body,
                parsed,
                // v1.01: multi-statement plpgsql bodies.
                plpgsql,
                volatility,
                strict,
            });
        }
        eng.db.functions.insert(name, overloads);
    }
    // v0.86: user-defined operators.
    let n_ops = d.u32().map_err(|e| bad(&e))? as usize;
    for _ in 0..n_ops {
        let name = d.str().map_err(|e| bad(&e))?;
        let n = d.u32().map_err(|e| bad(&e))? as usize;
        let mut defs = Vec::with_capacity(n);
        for _ in 0..n {
            let procedure = d.str().map_err(|e| bad(&e))?;
            let leftarg = if d.u8().map_err(|e| bad(&e))? != 0 {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            };
            let rightarg = if d.u8().map_err(|e| bad(&e))? != 0 {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            };
            let commutator = if d.u8().map_err(|e| bad(&e))? != 0 {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            };
            let negator = if d.u8().map_err(|e| bad(&e))? != 0 {
                Some(d.str().map_err(|e| bad(&e))?)
            } else {
                None
            };
            let hashes = d.u8().map_err(|e| bad(&e))? != 0;
            let merges = d.u8().map_err(|e| bad(&e))? != 0;
            defs.push(crate::storage::OperDef {
                name: name.clone(),
                procedure,
                leftarg,
                rightarg,
                commutator,
                negator,
                hashes,
                merges,
            });
        }
        eng.db.operators.insert(name, defs);
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
                t.push_version(RowVersion::plain(1, Row::new(vec![Value::Int(10)]), 1));
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

    /// v0.70: partition key expressions must survive the checkpoint
    /// round-trip: the encoder writes `format!("{:?}", expr)` and
    /// `parse_partition_expr` must read it back exactly.
    #[test]
    fn partition_expr_debug_roundtrip() {
        use crate::sql::Expr;
        let cases: Vec<Expr> = vec![
            Expr::Column {
                table: None,
                name: "a".to_string(),
            },
            Expr::Column {
                table: Some("t".to_string()),
                name: "b".to_string(),
            },
            Expr::Literal(Literal::Int(42)),
            Expr::Literal(Literal::BigInt(-9_000_000_000)),
            Expr::Literal(Literal::Text("o'brien \"x\"".to_string().into())),
            Expr::Literal(Literal::Bool(true)),
            Expr::Literal(Literal::Null),
            Expr::Literal(Literal::Float(1.5)),
            Expr::Arith {
                op: ArithOp::Add,
                left: Box::new(Expr::Column {
                    table: None,
                    name: "b".to_string(),
                }),
                right: Box::new(Expr::Literal(Literal::Int(0))),
            },
            Expr::Func {
                name: "lower".to_string(),
                args: vec![Expr::Column {
                    table: None,
                    name: "a".to_string(),
                }],
            },
            Expr::Func {
                name: "abs".to_string(),
                args: vec![Expr::Arith {
                    op: ArithOp::Mul,
                    left: Box::new(Expr::Column {
                        table: None,
                        name: "b".to_string(),
                    }),
                    right: Box::new(Expr::Literal(Literal::Int(-1))),
                }],
            },
        ];
        for e in &cases {
            let s = format!("{:?}", e);
            let back =
                parse_partition_expr(&s).unwrap_or_else(|| panic!("failed to parse {:?}", s));
            assert_eq!(format!("{:?}", back), s, "round-trip mismatch");
        }
        // Malformed input degrades to None, never panics.
        assert!(parse_partition_expr("").is_none());
        assert!(parse_partition_expr("Garbage { foo").is_none());
        assert!(parse_partition_expr("Column { table: None, name: \"a\" } trailing").is_none());
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
                oid: 16384,
                toast_relid: 16385,
                col_compression: vec![0],
                composite_types: vec![None],
                domain_types: vec![None],
                domain_elem: vec![false],
                inherits: vec![],
                xmin: 3,
            },
            WalRecord::InsertRows {
                table: "t".into(),
                rows: vec![
                    WalRow {
                        id: 7,
                        xmin: 4,
                        values: Row::new(vec![Value::Int(1), Value::Null]),
                        toast: vec![0; 2],
                        toast_meta: vec![],
                    },
                    WalRow {
                        id: 8,
                        xmin: 4,
                        values: Row::new(vec![Value::Int(2), Value::text("x")]),
                        toast: vec![0; 2],
                        toast_meta: vec![],
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
                        values: Row::new(vec![Value::Int(1)]),
                        toast: vec![0; 1],
                        toast_meta: vec![],
                    },
                    WalRow {
                        id: 8,
                        xmin: 4,
                        values: Row::new(vec![Value::Int(2)]),
                        toast: vec![0; 1],
                        toast_meta: vec![],
                    },
                    WalRow {
                        id: 9,
                        xmin: 4,
                        values: Row::new(vec![Value::Int(3)]),
                        toast: vec![0; 1],
                        toast_meta: vec![],
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
                    values: Row::new(vec![Value::Int(1), Value::text("a")]),
                    toast: vec![0; 2],
                    toast_meta: vec![],
                }],
                new: vec![WalRow {
                    id: 10,
                    xmin: 6,
                    values: Row::new(vec![Value::Int(1), Value::text("b")]),
                    toast: vec![0; 2],
                    toast_meta: vec![],
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
            // v1.05: lazy reltoastrelid link.
            WalRecord::SetToastRelid {
                name: "t".into(),
                toast_relid: 16385,
                xmin: 42,
            },
        ];
        for c in &cases {
            assert_eq!(&roundtrip(c), c);
        }
    }

    #[test]
    fn v96_create_table_inherits_roundtrip() {
        // v0.96: the `inherits` parent links survive WAL encode/decode
        // on both CreateTable and AlterTable records.
        let empty_constraints =
            "(constraints (notnull) (defaults) (checks) (uniques) (pkey -) (fks))".to_string();
        let create = WalRecord::CreateTable {
            name: "c".into(),
            columns: vec![("a".into(), ColType::Int)],
            constraints: empty_constraints.clone(),
            owner: "postgres".into(),
            acl: vec![],
            col_acl: vec![],
            oid: 16384,
            toast_relid: 0,
            col_compression: vec![0],
            composite_types: vec![None],
            domain_types: vec![None],
            domain_elem: vec![false],
            inherits: vec!["p1".to_string(), "p2".to_string()],
            xmin: 3,
        };
        assert_eq!(&roundtrip(&create), &create);
        let alter = WalRecord::AlterTable {
            name: "c".into(),
            columns: vec![("a".into(), ColType::Int)],
            constraints: empty_constraints,
            copy_rows: true,
            owner: "postgres".into(),
            acl: vec![],
            col_acl: vec![],
            oid: 16384,
            toast_relid: 0,
            toast_target: 0,
            col_storage: vec![0],
            col_compression: vec![0],
            composite_types: vec![None],
            domain_types: vec![None],
            domain_elem: vec![false],
            inherits: vec!["p1".to_string()],
            next_value_id: 1,
            toast_info: vec![],
            xmin: 4,
        };
        assert_eq!(&roundtrip(&alter), &alter);
    }

    #[test]
    fn records_for_commit_groups_inserts() {
        let mut eng = engine_with_committed_table();
        let xid = eng.begin_txn();
        // Stage two inserts into t.
        for (id, v) in [(2u64, 20i64), (3, 30)] {
            eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
                id,
                values: Row::new(vec![Value::Int(v)]),
                xmin: xid,
                xmax: 0,
                toast: Vec::new(),
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
        let recs = records_for_commit(&eng, xid, &writes, 0).unwrap();
        assert_eq!(recs.len(), 1);
        match &recs[0] {
            WalRecord::InsertRows { table, rows } => {
                assert_eq!(table, "t");
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].values, Row::new(vec![Value::Int(20)]));
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
        let err = records_for_commit(&eng, xid, &writes, 0).unwrap_err();
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
        let recs = records_for_commit(&eng, xid, &writes, 0).unwrap();
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
            values: Row::new(vec![Value::Int(99)]),
            xmin: xid,
            xmax: 0,
            toast: Vec::new(),
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
        let err = records_for_commit(&eng, xid, &writes, 0).unwrap_err();
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
                oid: 16384,
                toast_relid: 0,
                col_compression: vec![0],
                composite_types: vec![None],
                domain_types: vec![None],
                domain_elem: vec![false],
                inherits: vec![],
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
                    values: Row::new(vec![Value::Int(1)]),
                    toast: vec![0; 1],
                    toast_meta: vec![],
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
                    values: Row::new(vec![Value::Int(1)]),
                    toast: vec![0; 1],
                    toast_meta: vec![],
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
                    values: Row::new(vec![Value::Int(1)]),
                    toast: vec![0; 1],
                    toast_meta: vec![],
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
    fn v105_set_toast_relid_apply_and_emit() {
        // v1.05: the lazy reltoastrelid link replays onto the live table
        // version (and folds its xid), and records_for_commit emits it
        // from the write op — but skips a stale op whose link moved on.
        let mut eng = Engine::new();
        eng.db
            .tables
            .entry("t".into())
            .or_default()
            .push(Table::new(vec![("a".into(), ColType::Int)], 4));
        apply_record(
            &mut eng,
            &WalRecord::SetToastRelid {
                name: "t".into(),
                toast_relid: 16385,
                xmin: 7,
            },
        )
        .unwrap();
        let t = eng.db.tables["t"]
            .iter()
            .find(|t| t.dropped_xmax == 0)
            .unwrap();
        assert_eq!(t.toast_relid, 16385);
        assert!(eng.txns.next_xid >= 8);
        // Missing table: warning, not an error.
        apply_record(
            &mut eng,
            &WalRecord::SetToastRelid {
                name: "nope".into(),
                toast_relid: 1,
                xmin: 7,
            },
        )
        .unwrap();

        let writes = vec![WriteOp::SetToastRelid {
            table: "t".into(),
            toast_relid: 16385,
            prev: 0,
        }];
        let recs = records_for_commit(&eng, 9, &writes, 0).unwrap();
        assert_eq!(
            recs,
            vec![WalRecord::SetToastRelid {
                name: "t".into(),
                toast_relid: 16385,
                xmin: 9,
            }]
        );
        // Stale op: the link no longer matches, so nothing is logged.
        eng.db
            .tables
            .get_mut("t")
            .unwrap()
            .last_mut()
            .unwrap()
            .toast_relid = 0;
        let recs = records_for_commit(&eng, 9, &writes, 0).unwrap();
        assert!(recs.is_empty());
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

    #[test]
    fn v060_numeric_typmod_coltype_roundtrip() {
        // v0.60: constrained numeric(p,s) survives the WAL coltype
        // codec (tag 18); unconstrained keeps the legacy tag 7.
        for tm in [
            ColType::Numeric(None),
            ColType::Numeric(Some((10, 2))),
            ColType::Numeric(Some((3, 6))),
            ColType::Numeric(Some((5, -2))),
        ] {
            let mut e = Enc::new();
            e.col_type(&tm);
            let mut d = Dec::new(&e.buf);
            assert_eq!(d.col_type().unwrap(), tm);
            d.end().unwrap();
        }
    }

    #[test]
    fn v078_array_coltype_roundtrip() {
        // v0.78: array column types survive the WAL codec (tag 22 +
        // the PG array OID identifying the element type).
        use crate::storage::ArrayElem;
        for elem in [
            ArrayElem::Bool,
            ArrayElem::Int,
            ArrayElem::Text,
            ArrayElem::Numeric,
            ArrayElem::Timestamp,
            ArrayElem::Uuid,
            ArrayElem::Json,
        ] {
            let t = ColType::Array(elem);
            let mut e = Enc::new();
            e.col_type(&t);
            let mut d = Dec::new(&e.buf);
            assert_eq!(d.col_type().unwrap(), t);
            d.end().unwrap();
        }
    }

    #[test]
    fn v088_create_index_metadata_roundtrip() {
        // v0.88: CreateIndex carries per-column direction / null-placement
        // / expression sources plus the partial predicate (RGSWAL15).
        let rec = WalRecord::CreateIndex {
            name: "ix".into(),
            table: "t".into(),
            columns: vec!["a".into(), "(a + b)".into()],
            unique: true,
            internal: false,
            desc: vec![true, false],
            nulls_first: vec![false, true],
            exprs: vec![None, Some("a + b".into())],
            predicate: Some("b > 0".into()),
            xmin: 42,
        };
        let mut e = Enc::new();
        e.record(&rec);
        let mut d = Dec::new(&e.buf);
        match d.record().unwrap() {
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
                assert_eq!(name, "ix");
                assert_eq!(table, "t");
                assert_eq!(columns, vec!["a".to_string(), "(a + b)".to_string()]);
                assert!(unique);
                assert!(!internal);
                assert_eq!(desc, vec![true, false]);
                assert_eq!(nulls_first, vec![false, true]);
                assert_eq!(exprs, vec![None, Some("a + b".to_string())]);
                assert_eq!(predicate, Some("b > 0".to_string()));
                assert_eq!(xmin, 42);
            }
            other => panic!("expected CreateIndex, got {:?}", other),
        }
        d.end().unwrap();
    }
}
