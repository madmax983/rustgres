// v1.78 mechanical split: moved verbatim from src/wal.rs (159-263, 4753-6185).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

pub(crate) const WAL_NAME: &str = "wal.log";
pub(crate) const CHKPT_NAME: &str = "checkpoint.dat";
pub(crate) const CHKPT_TMP: &str = "checkpoint.dat.tmp";
pub(crate) const CHKPT_MAGIC: &[u8; 8] = b"RGSCHK16";
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
pub(crate) const CHKPT_VERSION: u32 = 17;
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
/// v1.41: `RGSWAL20` — `CreateTable`/`AlterTable` carry `attnums`,
/// `next_attnum`, and `fillfactor`. Old `RGSWAL19` files are refused
/// loudly.
pub(crate) const WAL_MAGIC: &[u8; 8] = b"RGSWAL20";
pub(crate) const WAL_HEADER_LEN: u64 = 16;

/// Encode a WAL file header for a generation starting at `base_lsn`.
pub(crate) fn encode_wal_header(base_lsn: u64) -> [u8; 16] {
    let mut hdr = [0u8; 16];
    hdr[..8].copy_from_slice(WAL_MAGIC);
    hdr[8..].copy_from_slice(&base_lsn.to_be_bytes());
    hdr
}

/// Read the WAL header. Returns `Ok(None)` when the file is empty or the
/// header is torn (short file) — both mean "start a fresh generation";
/// see the module docs for why that is safe. A complete header with the
/// wrong magic is an error (incompatible format, e.g. v0.4 data).
pub(crate) fn read_wal_header(file: &mut File) -> std::io::Result<Option<u64>> {
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
// The WAL itself
// ---------------------------------------------------------------------------

/// Append-only write-ahead log in `<data_dir>/wal.log`, with
/// `<data_dir>/checkpoint.dat` snapshots.
pub struct Wal {
    pub(crate) dir: PathBuf,
    pub(crate) file: File,
    /// Physical bytes of wal.log accounted for (includes the header).
    pub(crate) len: u64,
    /// Logical LSN of physical offset WAL_HEADER_LEN: frame LSNs are
    /// `base_lsn + (physical_offset - WAL_HEADER_LEN)`.
    pub(crate) base_lsn: u64,
    pub(crate) next_txn: u64,
    /// v0.13: cluster-wide random identifier, stable across restarts
    /// (like PostgreSQL's system identifier from initdb). Stored in
    /// `system.id` inside the data directory.
    pub(crate) system_id: u64,
}

/// v0.13: the stable cluster identifier. Read from `system.id` in the
/// data directory, or mint a random one (persisted via a tmp-file
/// rename + dir fsync, like the checkpoint) on first boot.
pub(crate) fn load_or_create_system_id(dir: &Path) -> std::io::Result<u64> {
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

pub(crate) fn io_err(what: String) -> std::io::Error {
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
                // v1.41: real PG attnums, the next-attnum counter, and
                // fillfactor.
                body.u32(t.attnums.len() as u32);
                for a in &t.attnums {
                    body.i16(*a);
                }
                body.i16(t.next_attnum);
                body.u8(t.fillfactor);
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
pub(crate) fn committed_xmax(eng: &Engine, xmax: u64) -> u64 {
    if xmax != 0 && eng.xid_committed(xmax) {
        xmax
    } else {
        0
    }
}

pub(crate) struct Frame {
    pub(crate) txn_id: u64,
    pub(crate) records: Vec<WalRecord>,
}

/// Read one WAL frame. `Ok(None)` = clean EOF (no bytes) or a torn tail
/// (short read / bad CRC): recovery stops, the partial batch is dropped.
pub(crate) fn read_frame(file: &mut File) -> std::io::Result<Option<Frame>> {
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
pub(crate) fn load_checkpoint(dir: &Path) -> std::io::Result<(Engine, u64)> {
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
        // v1.41: real PG attnums, the next-attnum counter, fillfactor.
        let n_attnums = d.u32().map_err(|e| bad(&e))? as usize;
        let mut attnums = Vec::with_capacity(n_attnums);
        for _ in 0..n_attnums {
            attnums.push(d.i16().map_err(|e| bad(&e))?);
        }
        let next_attnum = d.i16().map_err(|e| bad(&e))?;
        let fillfactor = d.u8().map_err(|e| bad(&e))?;
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
        // v1.41: restore attnums, next_attnum, fillfactor.
        __t.attnums = attnums;
        __t.next_attnum = next_attnum;
        __t.fillfactor = fillfactor;
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
