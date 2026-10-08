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
//! Format version 20 (`RGSWAL20` / `RGSCHK16`) is NOT compatible with v1.40
//! or earlier: v1.41 WAL-logs and checkpoints the real PG attribute
//! numbers (`attnums`/`next_attnum`) and the `fillfactor` storage
//! parameter on `CreateTable`/`AlterTable` records and table images.
//! Like every format bump, old data directories are refused with a
//! clear error instead of being misread.
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

pub(crate) use std::fs::{self, File, OpenOptions};
pub(crate) use std::io::{Read, Seek, SeekFrom, Write};
pub(crate) use std::path::{Path, PathBuf};

pub(crate) use crate::index::{Index, IndexDef};
pub(crate) use crate::sql::{ArithOp, Expr, Literal};
pub(crate) use crate::storage::{
    ArrayElem, ArrayVal, ColType, Engine, Row, RowVersion, Table, Value, WriteOp,
};

mod encoding;
mod records;
mod recovery;
mod writer;

#[cfg(test)]
#[path = "test_mods/tests.rs"]
mod tests;

pub(crate) use encoding::*;
pub(crate) use records::*;
pub(crate) use recovery::*;
pub(crate) use writer::*;
