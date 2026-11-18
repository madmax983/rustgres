//! v2.34: the pg_stat_* monitoring family (PG19 pgstat.c model).
//!
//! Counter state lives in three places, mirroring PG19:
//! - `Database::pgstat_rel` / `pgstat_func` / `pgstat_shared` — the
//!   committed ("shared") counters the pg_stat_* views and the
//!   pg_stat_get_* accessors read. In-memory only, like
//!   `Database::rel_stats`: a restart zeroes them (PG persists
//!   stats across restarts; the vendored schedule's restart cycle
//!   does not score continuity — documented hold-back).
//! - `Engine::pgstat_xact` — per-(session, top-xid) transaction
//!   counters, merged into the committed state by the server's
//!   commit/abort hooks (`pgstat_commit` / `pgstat_abort` below),
//!   with savepoint snapshots taken by the server's SAVEPOINT hook
//!   and applied by `pgstat_rollback_to`.
//! - `Engine::pgstat_func_pending` — function-call counts are NOT
//!   transactional in PG: they accumulate as "pending" and merge
//!   into the committed function stats only on
//!   pg_stat_force_next_flush() (or at transaction end in PG's
//!   stats reporting; the suite's observable contract is the
//!   flush function, so the merge happens exactly there).
//!
//! GUCs (track_counts / track_functions / stats_fetch_consistency)
//! are read through the per-statement installed-GUC snapshot the
//! server already maintains for current_setting().

use super::*;
use crate::fxhash::FxBuildHasher;
use crate::storage::{PgFuncStat, PgIoCounts, PgRelStat, PgXactRel};

pub(crate) const SHOBJ_OID: i64 = 2396;
pub(crate) const SHOBJ_KEY: &str = "pg_shdescription";

fn now() -> i64 {
    crate::datetime::now_micros()
}

fn ts(v: Option<i64>) -> Value {
    v.map(Value::Timestamptz).unwrap_or(Value::Null)
}

fn guc(name: &str) -> Option<String> {
    installed_guc(name)
}

fn guc_bool(name: &str, default: bool) -> bool {
    match guc(name) {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "false" | "no" | "0"
        ),
        None => default,
    }
}

/// track_functions GUC: "none" (default) | "pl" | "all".
pub(crate) fn track_functions_mode() -> String {
    guc("track_functions")
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "none".to_string())
}

/// stats_fetch_consistency GUC: "none" | "cache" (default) |
/// "snapshot".
fn consistency_mode() -> String {
    guc("stats_fetch_consistency")
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "cache".to_string())
}

// ---------------------------------------------------------------------------
// Relation keys
// ---------------------------------------------------------------------------

/// Storage key for a session's temp relation.
pub(crate) fn pgstat_temp_key(session: u64, name: &str) -> String {
    format!("temp:{session}:{name}")
}

pub(crate) fn is_temp_key(key: &str) -> bool {
    key.starts_with("temp:")
}

/// v2.34: tablespace existence (built-ins + CREATE TABLESPACE
/// registry) for SET TABLESPACE validation.
pub(crate) fn tablespace_exists(db: &Database, name: &str) -> bool {
    name == "pg_default" || name == "pg_global" || db.tablespaces.contains(name)
}

/// The pgstat key for a relation name as seen by `session`: a temp
/// table shadowing a permanent one keys under the temp prefix.
pub(crate) fn rel_key(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    name: &str,
) -> String {
    let is_temp = db
        .temp_tables
        .get(&session)
        .map(|m| m.contains_key(name))
        .unwrap_or(false);
    if is_temp {
        let _ = (snap, own);
        pgstat_temp_key(session, name)
    } else {
        name.to_string()
    }
}

/// Display (schemaname, relname) for a pgstat key.
fn key_parts(db: &Database, key: &str) -> (String, String) {
    if key == SHOBJ_KEY {
        return ("pg_catalog".to_string(), "pg_shdescription".to_string());
    }
    if let Some(rest) = key.strip_prefix("temp:") {
        let name = rest.split_once(':').map(|(_, n)| n).unwrap_or(rest);
        return (temp_schema_name(db), name.to_string());
    }
    catalog_name_parts(key)
}

/// The session's temp schema name (PG: pg_temp_<backend id>; this
/// engine's client backend id is 1 — see pg_stat_get_backend_idset).
fn temp_schema_name(_db: &Database) -> String {
    "pg_temp_1".to_string()
}

// ---------------------------------------------------------------------------
// Enumeration of relations / indexes / sequences visible to a session
// ---------------------------------------------------------------------------

pub(crate) struct RelInfo {
    pub key: String,
    pub oid: i64,
    pub schemaname: String,
    pub relname: String,
    /// 'r' table / 'p' partitioned / 'S' sequence.
    pub kind: u8,
}

/// All tables (permanent + this session's temp) visible under the
/// snapshot, in deterministic (key) order.
pub(crate) fn list_tables(db: &Database, snap: &Snapshot, own: u64, session: u64) -> Vec<RelInfo> {
    let mut out = Vec::new();
    let mut names: Vec<&String> = db.tables.keys().collect();
    names.sort();
    for name in names {
        let Some(t) = db.find_table(name, snap, &[own], session) else {
            continue;
        };
        let kind = if t.partition.as_ref().is_some_and(|p| p.is_partitioned) {
            b'p'
        } else {
            b'r'
        };
        let (sch, rel) = catalog_name_parts(name);
        out.push(RelInfo {
            key: name.clone(),
            oid: t.oid as i64,
            schemaname: sch,
            relname: rel,
            kind,
        });
    }
    if let Some(tm) = db.temp_tables.get(&session) {
        let mut tnames: Vec<&String> = tm.keys().collect();
        tnames.sort();
        for name in tnames {
            let Some(t) = db.find_table(name, snap, &[own], session) else {
                continue;
            };
            out.push(RelInfo {
                key: pgstat_temp_key(session, name),
                oid: t.oid as i64,
                schemaname: temp_schema_name(db),
                relname: name.rsplit('.').next().unwrap_or(name.as_str()).to_string(),
                kind: b'r',
            });
        }
    }
    out
}

pub(crate) struct IdxInfo {
    pub key: String,
    pub oid: i64,
    pub table_key: String,
    pub schemaname: String,
    pub relname: String,
    pub indexrelname: String,
}

pub(crate) fn list_indexes(db: &Database, snap: &Snapshot, own: u64, session: u64) -> Vec<IdxInfo> {
    let oids = index_oid_map(db, snap, own, session);
    let mut out = Vec::new();
    for d in visible_index_defs(db, snap, own, session) {
        // visible_index_defs merges session temp indexes; those
        // are listed by the temp loop below under their temp
        // pgstat keys (listing them here too duplicates them).
        if db.is_temp_table(session, &d.table) {
            continue;
        }
        let (sch, rel) = catalog_name_parts(&d.table);
        out.push(IdxInfo {
            key: d.name.clone(),
            oid: oids.get(&d.name).copied().unwrap_or(0),
            table_key: d.table.clone(),
            schemaname: sch,
            relname: rel,
            indexrelname: d
                .name
                .rsplit('.')
                .next()
                .unwrap_or(d.name.as_str())
                .to_string(),
        });
    }
    if let Some(tm) = db.temp_indexes.get(&session) {
        let mut tnames: Vec<&String> = tm.keys().collect();
        tnames.sort();
        for name in tnames {
            let d = &tm[name].def;
            out.push(IdxInfo {
                key: pgstat_temp_key(session, &d.name),
                oid: oids.get(&d.name).copied().unwrap_or(0),
                table_key: pgstat_temp_key(session, &d.table),
                schemaname: temp_schema_name(db),
                relname: d
                    .table
                    .rsplit('.')
                    .next()
                    .unwrap_or(d.table.as_str())
                    .to_string(),
                indexrelname: d
                    .name
                    .rsplit('.')
                    .next()
                    .unwrap_or(d.name.as_str())
                    .to_string(),
            });
        }
    }
    out
}

pub(crate) fn list_sequences(db: &Database, snap: &Snapshot, own: u64) -> Vec<RelInfo> {
    let mut names: Vec<&String> = db.sequences.keys().collect();
    names.sort();
    let mut out = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if db.find_sequence(name, snap, own).is_none() {
            continue;
        }
        let (sch, rel) = catalog_name_parts(name);
        out.push(RelInfo {
            key: (*name).clone(),
            // Sequences carry no oid in this engine's catalog;
            // synthesize a stable one above the index band.
            oid: 60000 + i as i64,
            schemaname: sch,
            relname: rel,
            kind: b'S',
        });
    }
    out
}

/// Resolve an oid (or a relation name, for regclass-typed args) to
/// a pgstat key + kind. Kind: 't' table, 'i' index, 'S' sequence,
/// 'h' the shared pg_shdescription pseudo-relation.
pub(crate) fn resolve_oid(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    v: &Value,
) -> Option<(String, u8)> {
    match v {
        Value::Int(oid) => {
            if *oid == SHOBJ_OID {
                return Some((SHOBJ_KEY.to_string(), b'h'));
            }
            for t in list_tables(db, snap, own, session) {
                if t.oid == *oid {
                    return Some((t.key, b't'));
                }
            }
            for ix in list_indexes(db, snap, own, session) {
                if ix.oid == *oid {
                    return Some((ix.key, b'i'));
                }
            }
            for s in list_sequences(db, snap, own) {
                if s.oid == *oid {
                    return Some((s.key, b'S'));
                }
            }
            None
        }
        Value::Text(name) => {
            // A regclass value whose display fell back to the
            // numeric OID (unknown band) still resolves by OID.
            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(n) = name.parse::<i64>() {
                    return resolve_oid(db, snap, own, session, &Value::Int(n));
                }
            }
            // regclass values are the relation's display name.
            let bare = name.rsplit('.').next().unwrap_or(&**name);
            for t in list_tables(db, snap, own, session) {
                if t.relname == bare || t.key == **name {
                    return Some((t.key, b't'));
                }
            }
            for ix in list_indexes(db, snap, own, session) {
                if ix.indexrelname == bare || ix.key == **name {
                    return Some((ix.key, b'i'));
                }
            }
            for s in list_sequences(db, snap, own) {
                if s.relname == bare || s.key == **name {
                    return Some((s.key, b'S'));
                }
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Snapshot timestamp + shared-entry materialization
// ---------------------------------------------------------------------------

/// Stamp the stats snapshot time for this transaction on first
/// stats access (PG19 pgstat: with cache/snapshot consistency the
/// snapshot is taken at first access; with "none" no snapshot
/// exists). Also materializes the shared counters' stats_reset
/// times (PG sets them at entry creation).
pub(crate) fn touch_snapshot(eng: &mut Engine, session: u64, own: u64) {
    let t = now();
    {
        let sh = &mut eng.db.pgstat_shared;
        if sh.db_stats_reset.is_none() {
            sh.db_stats_reset = Some(t);
        }
        if sh.wal_stats_reset.is_none() {
            sh.wal_stats_reset = Some(t);
        }
        if sh.ckpt_stats_reset.is_none() {
            sh.ckpt_stats_reset = Some(t);
        }
        if sh.bgwriter_stats_reset.is_none() {
            sh.bgwriter_stats_reset = Some(t);
        }
        if sh.archiver_stats_reset.is_none() {
            sh.archiver_stats_reset = Some(t);
        }
        if sh.recovery_prefetch_stats_reset.is_none() {
            sh.recovery_prefetch_stats_reset = Some(t);
        }
        if sh.lock_stats_reset.is_none() {
            sh.lock_stats_reset = Some(t);
        }
        if sh.io_stats_reset.is_none() {
            sh.io_stats_reset = Some(t);
        }
        for n in SLRU_NAMES {
            sh.slru_stats_reset.entry(n.to_string()).or_insert(Some(t));
        }
    }
    if consistency_mode() == "snapshot" {
        eng.pgstat_snapshot.entry((session, own)).or_insert(t);
    }
}

pub(crate) fn clear_snapshot(eng: &mut Engine, session: u64, own: u64) {
    eng.pgstat_snapshot.remove(&(session, own));
}

// ---------------------------------------------------------------------------
// Recording (transaction-local counters)
// ---------------------------------------------------------------------------

fn xact_entry<'e>(eng: &'e mut Engine, session: u64, own: u64, key: &str) -> &'e mut PgXactRel {
    eng.pgstat_xact
        .entry((session, own))
        .or_default()
        .entry(key.to_string())
        .or_insert_with(|| PgXactRel {
            dropped: false,
            gens: vec![Default::default()],
            dead_from_rollback: 0,
            seq_scan: 0,
            seq_tup_read: 0,
            idx_scan: 0,
            idx_tup_fetch: 0,
        })
}

fn committed_entry<'a>(db: &'a mut Database, key: &str) -> &'a mut PgRelStat {
    db.pgstat_rel
        .entry(key.to_string())
        .or_insert_with(PgRelStat::default)
}

fn io_bump(
    db: &mut Database,
    backend_type: &str,
    object: &str,
    context: &str,
    f: impl FnOnce(&mut PgIoCounts),
) {
    let e = db
        .pgstat_shared
        .io
        .entry((
            backend_type.to_string(),
            object.to_string(),
            context.to_string(),
        ))
        .or_default();
    f(e);
}

/// WAL bytes attributed to a row operation (bounded model: fixed
/// per-row record charge; only monotonic growth is scored).
const WAL_BYTES_PER_ROW: i64 = 64;

fn wal_bump(db: &mut Database, session: u64, rows: i64) {
    if rows <= 0 {
        return;
    }
    let bytes = rows * WAL_BYTES_PER_ROW;
    let sh = &mut db.pgstat_shared;
    sh.wal_records += rows;
    sh.wal_bytes += bytes;
    *sh.backend_wal_bytes.entry(session).or_insert(0) += bytes;
}

pub(crate) fn record_insert(eng: &mut Engine, session: u64, own: u64, key: &str, n: i64) {
    if n <= 0 || !guc_bool("track_counts", true) {
        return;
    }
    let temp = is_temp_key(key);
    if own == 0 {
        // No transaction context (direct exec / unit tests): the
        // counts commit with the statement. (Server autocommit
        // statements carry an ephemeral xid and merge through
        // pgstat_commit from autocommit_execute.)
        let c = committed_entry(&mut eng.db, key);
        c.n_tup_ins += n;
        c.n_live_tup += n;
        c.n_mod_since_analyze += n;
        c.n_ins_since_vacuum += n;
    } else {
        let e = xact_entry(eng, session, own, key);
        e.cur().ins += n;
        e.cur().live_delta += n;
    }
    let obj = if temp { "temp relation" } else { "relation" };
    io_bump(&mut eng.db, "client backend", obj, "normal", |c| {
        c.extends += n;
        c.extend_bytes += n * 8192;
    });
    bump_backend_io(&mut eng.db, session, obj, "normal", |c| {
        c.extends += n;
        c.extend_bytes += n * 8192;
    });
    wal_bump(&mut eng.db, session, n);
    if temp {
        temp_pressure(eng, session, own, key);
    }
}

pub(crate) fn record_update(
    eng: &mut Engine,
    session: u64,
    own: u64,
    key: &str,
    n: i64,
    hot: i64,
    newpage: i64,
) {
    if n <= 0 || !guc_bool("track_counts", true) {
        return;
    }
    if own == 0 {
        let c = committed_entry(&mut eng.db, key);
        c.n_tup_upd += n;
        c.n_tup_hot_upd += hot;
        c.n_tup_newpage_upd += newpage;
        c.n_dead_tup += n - hot;
        c.n_mod_since_analyze += n;
    } else {
        let e = xact_entry(eng, session, own, key);
        let c = e.cur();
        c.upd += n;
        c.hot += hot;
        c.newpage += newpage;
    }
    wal_bump(&mut eng.db, session, n);
}

pub(crate) fn record_delete(eng: &mut Engine, session: u64, own: u64, key: &str, n: i64) {
    if n <= 0 || !guc_bool("track_counts", true) {
        return;
    }
    if own == 0 {
        let c = committed_entry(&mut eng.db, key);
        c.n_tup_del += n;
        c.n_live_tup = (c.n_live_tup - n).max(0);
        c.n_dead_tup += n;
        c.n_mod_since_analyze += n;
    } else {
        let e = xact_entry(eng, session, own, key);
        let c = e.cur();
        c.del += n;
        // Deleting removes a live tuple (net live delta; the
        // dead-tuple side lands at commit via `del`).
        c.live_delta -= n;
    }
    wal_bump(&mut eng.db, session, n);
}

/// TRUNCATE inside a transaction: a new generation begins (PG:
/// the pre-truncate xact counts are superseded for the cumulative
/// columns; committed counters merge at commit).
pub(crate) fn record_truncate(eng: &mut Engine, session: u64, own: u64, key: &str) {
    if own == 0 {
        // Autocommit TRUNCATE: live/dead restart immediately;
        // cumulative counters survive (PG19).
        let c = committed_entry(&mut eng.db, key);
        c.n_live_tup = 0;
        c.n_dead_tup = 0;
        return;
    }
    let e = xact_entry(eng, session, own, key);
    e.gens.push(Default::default());
}

/// A completed scan of a table (from build_source's access path).
/// `index` = Some(index key) for an index scan.
pub(crate) fn record_scan(
    eng: &mut Engine,
    session: u64,
    own: u64,
    key: &str,
    index: Option<&str>,
    rows: i64,
) {
    if !guc_bool("track_counts", true) {
        return;
    }
    let t = now();
    let temp = is_temp_key(key);
    match index {
        None => {
            if own != 0 {
                let e = xact_entry(eng, session, own, key);
                e.seq_scan += 1;
                e.seq_tup_read += rows;
            }
            let c = committed_entry(&mut eng.db, key);
            c.seq_scan += 1;
            c.last_seq_scan = Some(t);
            c.seq_tup_read += rows;
            c.tup_fetched += rows;
            c.blks_hit += rows;
            let obj = if temp { "temp relation" } else { "relation" };
            io_bump(&mut eng.db, "client backend", obj, "normal", |cc| {
                cc.reads += rows;
                cc.read_bytes += rows * 8192;
                cc.hits += rows;
            });
            bump_backend_io(&mut eng.db, session, obj, "normal", |cc| {
                cc.reads += rows;
                cc.read_bytes += rows * 8192;
                cc.hits += rows;
            });
        }
        Some(ikey) => {
            if own != 0 {
                // The table's xact entry carries idx_scan for
                // pg_stat_xact_all_tables; the index's own entry
                // backs pg_stat_get_xact_numscans(index_oid).
                let e = xact_entry(eng, session, own, key);
                e.idx_scan += 1;
                e.idx_tup_fetch += rows;
                let e = xact_entry(eng, session, own, ikey);
                e.idx_scan += 1;
                e.idx_tup_fetch += rows;
            }
            let c = committed_entry(&mut eng.db, ikey);
            c.idx_scan += 1;
            c.last_idx_scan = Some(t);
            c.idx_tup_read += rows;
            c.idx_tup_fetch += rows;
            c.blks_hit += rows;
            // The table's entry carries the same scan (PG counts
            // idx_scan / last_idx_scan / idx_tup_fetch on the table
            // as well as on the index), plus the heap fetches.
            let tc = committed_entry(&mut eng.db, key);
            tc.idx_scan += 1;
            tc.last_idx_scan = Some(t);
            tc.idx_tup_fetch += rows;
            tc.tup_fetched += rows;
            let obj = if temp { "temp relation" } else { "relation" };
            io_bump(&mut eng.db, "client backend", obj, "normal", |cc| {
                cc.reads += rows;
                cc.read_bytes += rows * 8192;
                cc.hits += rows;
            });
            bump_backend_io(&mut eng.db, session, obj, "normal", |cc| {
                cc.reads += rows;
                cc.read_bytes += rows * 8192;
                cc.hits += rows;
            });
        }
    }
}

fn bump_backend_io(
    db: &mut Database,
    session: u64,
    object: &str,
    context: &str,
    f: impl FnOnce(&mut PgIoCounts),
) {
    let per = db.pgstat_shared.backend_io.entry(session).or_default();
    let c = per
        .entry((
            "client backend".to_string(),
            object.to_string(),
            context.to_string(),
        ))
        .or_default();
    f(c);
}

/// temp_buffers in 8kB blocks (the stored GUC value is
/// normalized to "<n>kB" by the server; bare numbers are blocks).
fn temp_buffer_blocks() -> i64 {
    let Some(v) = guc("temp_buffers") else {
        return 1024;
    };
    let v = v.trim().to_ascii_lowercase();
    let parsed = if let Some(p) = v.strip_suffix("kb") {
        p.trim().parse::<i64>().map(|kb| kb / 8)
    } else if let Some(p) = v.strip_suffix("mb") {
        p.trim().parse::<i64>().map(|mb| mb * 128)
    } else if let Some(p) = v.strip_suffix("gb") {
        p.trim().parse::<i64>().map(|gb| gb * 131072)
    } else {
        v.parse::<i64>()
    };
    parsed.unwrap_or(1024).max(1)
}

/// Local-buffer pressure for temp relations: once a temp
/// relation's estimated size exceeds temp_buffers, the overflow
/// pages have been evicted (and written) at least once. Tracked
/// as a high-water mark per relation so the counters only grow.
fn temp_pressure(eng: &mut Engine, session: u64, own: u64, key: &str) {
    let allowed = temp_buffer_blocks();
    let mut rows = {
        let e = entry_or_default(&eng.db, key);
        e.n_live_tup + e.n_dead_tup
    };
    if own != 0 {
        if let Some(e) = eng
            .pgstat_xact
            .get(&(session, own))
            .and_then(|m| m.get(key))
        {
            rows += e.cur_ref().live_delta;
        }
    }
    // ~200 bytes per row -> ~40 rows per 8kB page.
    let pages = (rows.max(0) + 39) / 40;
    let excess = pages - allowed;
    let hwm = entry_or_default(&eng.db, key).temp_evict_hwm;
    if excess > hwm {
        let d = excess - hwm;
        committed_entry(&mut eng.db, key).temp_evict_hwm = excess;
        io_bump(
            &mut eng.db,
            "client backend",
            "temp relation",
            "normal",
            |c| {
                c.evictions += d;
                c.writes += d;
                c.write_bytes += d * 8192;
            },
        );
        bump_backend_io(&mut eng.db, session, "temp relation", "normal", |c| {
            c.evictions += d;
            c.writes += d;
            c.write_bytes += d * 8192;
        });
    }
}

/// CTAS / COPY-style bulk writes extend the relation under the
/// bulkwrite IO context (PG19 BAS_BULKWRITE).
pub(crate) fn record_bulkwrite_extends(eng: &mut Engine, session: u64, key: &str, n: i64) {
    if n <= 0 {
        return;
    }
    let obj = if is_temp_key(key) {
        "temp relation"
    } else {
        "relation"
    };
    io_bump(&mut eng.db, "client backend", obj, "bulkwrite", |c| {
        c.extends += n;
        c.extend_bytes += n * 8192;
    });
    bump_backend_io(&mut eng.db, session, obj, "bulkwrite", |c| {
        c.extends += n;
        c.extend_bytes += n * 8192;
    });
}

/// ALTER TABLE ... SET TABLESPACE on a temp relation rewrites
/// it in PG: every page is written out through the local buffer
/// pool (pg_stat_io temp relation writes grow by the page count).
pub(crate) fn record_temp_rewrite(eng: &mut Engine, session: u64, key: &str) {
    let e = entry_or_default(&eng.db, key);
    let rows = (e.n_live_tup + e.n_dead_tup).max(0);
    let pages = ((rows + 39) / 40).max(1);
    io_bump(
        &mut eng.db,
        "client backend",
        "temp relation",
        "normal",
        |c| {
            c.writes += pages;
            c.write_bytes += pages * 8192;
        },
    );
    bump_backend_io(&mut eng.db, session, "temp relation", "normal", |c| {
        c.writes += pages;
        c.write_bytes += pages * 8192;
    });
}

/// A scan that touched more relations than the per-backend
/// fastpath lock capacity (16) exceeded the fastpath for the
/// excess (PG19 pg_stat_lock.fastpath_exceeded model).
pub(crate) fn record_lock_fastpath(eng: &mut Engine, n: i64) {
    eng.db.pgstat_shared.lock_fastpath_exceeded += n;
}

/// CHECKPOINT (from the server's txn_checkpoint).
pub(crate) fn record_checkpoint(eng: &mut Engine) {
    let sh = &mut eng.db.pgstat_shared;
    sh.ckpt_requested += 1;
    sh.ckpt_done += 1;
    // The checkpoint record itself is WAL (pg_stat_wal growth).
    sh.wal_records += 1;
    sh.wal_bytes += 512;
    io_bump(&mut eng.db, "checkpointer", "wal", "normal", |c| {
        c.writes += 1;
        c.write_bytes += 8192;
        c.fsyncs += 1;
    });
    // A checkpoint writes and fsyncs dirty relation buffers.
    io_bump(&mut eng.db, "checkpointer", "relation", "normal", |c| {
        c.writes += 1;
        c.write_bytes += 8192;
        c.fsyncs += 1;
    });
}

/// A new backend session (pg_stat_database.sessions).
pub(crate) fn record_session_start(eng: &mut Engine) {
    eng.db.pgstat_shared.sessions += 1;
}

/// Disconnect cleanup: the session's temp-relation counters and
/// per-backend IO/WAL state die with the session (PG drops temp
/// relations' stats entries when the backend exits).
pub(crate) fn pgstat_session_cleanup(eng: &mut Engine, session: u64) {
    let prefix = format!("temp:{session}:");
    eng.db.pgstat_rel.retain(|k, _| !k.starts_with(&prefix));
    eng.pgstat_xact.retain(|(s, _), _| *s != session);
    eng.pgstat_func_xact.retain(|(s, _), _| *s != session);
    eng.pgstat_snapshot.retain(|(s, _), _| *s != session);
    eng.db.pgstat_shared.backend_io.remove(&session);
    eng.db.pgstat_shared.backend_wal_bytes.remove(&session);
}

/// VACUUM (explicit, from the server's txn_vacuum).
pub(crate) fn record_vacuum(eng: &mut Engine, key: &str) {
    let t = now();
    let c = committed_entry(&mut eng.db, key);
    c.last_vacuum = Some(t);
    c.vacuum_count += 1;
    c.n_ins_since_vacuum = 0;
    io_bump(&mut eng.db, "client backend", "relation", "vacuum", |cc| {
        cc.reads += 1;
        cc.read_bytes += 8192;
        cc.reuses += 1;
    });
}

/// ANALYZE (from exec_analyze's per-table completion).
pub(crate) fn record_analyze(eng: &mut Engine, key: &str) {
    let t = now();
    let c = committed_entry(&mut eng.db, key);
    c.last_analyze = Some(t);
    c.analyze_count += 1;
    c.n_mod_since_analyze = 0;
}

/// A user-function call (pending + per-xact counts; PG function
/// stats are not transactional).
pub(crate) fn record_function_call(eng: &mut Engine, session: u64, own: u64, name: &str, oid: i64) {
    let mode = track_functions_mode();
    if mode == "none" {
        return;
    }
    *eng.pgstat_func_pending.entry(name.to_string()).or_insert(0) += 1;
    if own != 0 {
        *eng.pgstat_func_xact
            .entry((session, own))
            .or_default()
            .entry(name.to_string())
            .or_insert(0) += 1;
    }
    let t = now();
    eng.db
        .pgstat_func
        .entry(name.to_string())
        .or_insert_with(|| PgFuncStat {
            oid,
            calls: 0,
            total_time: 0.0,
            self_time: 0.0,
            stats_reset: Some(t),
        });
}

/// pg_stat_force_next_flush(): merge pending function counts.
pub(crate) fn flush_function_stats(eng: &mut Engine) {
    let pending = std::mem::take(&mut eng.pgstat_func_pending);
    for (name, n) in pending {
        if let Some(e) = eng.db.pgstat_func.get_mut(&name) {
            e.calls += n;
        }
    }
}

// ---------------------------------------------------------------------------
// Transaction end: merge rules (PG19 pgstat_xact model; derivation in
// hidden_files/v234/scope.md)
// ---------------------------------------------------------------------------

pub(crate) fn pgstat_commit(eng: &mut Engine, session: u64, xid: u64) {
    if let Some(map) = eng.pgstat_xact.remove(&(session, xid)) {
        for (key, e) in map {
            if e.dropped {
                // The DROP committed: the committed entry is
                // removed by the dropped cleanup below.
                continue;
            }
            let cur = e.cur_ref().clone();
            let truncated = e.gens.len() > 1;
            let c = committed_entry(&mut eng.db, &key);
            c.n_tup_ins += cur.ins;
            c.n_tup_upd += cur.upd;
            c.n_tup_del += cur.del;
            c.n_tup_hot_upd += cur.hot;
            c.n_tup_newpage_upd += cur.newpage;
            if truncated {
                // A committed TRUNCATE restarts the live/dead
                // counts from the post-truncate generation.
                c.n_live_tup = cur.live_delta.max(0);
                c.n_dead_tup = e.dead_from_rollback + cur.del + (cur.upd - cur.hot);
            } else {
                c.n_live_tup += cur.live_delta;
                c.n_dead_tup += e.dead_from_rollback + cur.del + (cur.upd - cur.hot);
            }
            if c.n_live_tup < 0 {
                c.n_live_tup = 0;
            }
            if c.n_dead_tup < 0 {
                c.n_dead_tup = 0;
            }
            c.n_mod_since_analyze += cur.ins + cur.upd + cur.del;
            c.n_ins_since_vacuum += cur.ins;
        }
    }
    eng.pgstat_func_xact.remove(&(session, xid));
    eng.pgstat_snapshot.remove(&(session, xid));
    // Stats entries of relations/functions whose DROP committed
    // in this transaction are removed now (PG19 pgstat: the drop
    // takes effect for stats at commit).
    let dropped_rel: Vec<String> = eng
        .pgstat_dropped_rel
        .iter()
        .filter(|(s, x, _)| *s == session && *x == xid)
        .map(|(_, _, k)| k.clone())
        .collect();
    for k in dropped_rel {
        eng.db.pgstat_rel.remove(&k);
        eng.pgstat_dropped_rel.remove(&(session, xid, k));
    }
    let dropped_func: Vec<String> = eng
        .pgstat_dropped_func
        .iter()
        .filter(|(s, x, _)| *s == session && *x == xid)
        .map(|(_, _, k)| k.clone())
        .collect();
    for n in dropped_func {
        if let Some(e) = eng.db.pgstat_func.remove(&n) {
            // The function's OID is retired: the name-sorted OID
            // scheme would reassign it to a surviving function,
            // and the pg_stat_get_function_* accessors must keep
            // answering NULL for the dead OID (PG OIDs are
            // stable and never reassigned).
            eng.db.pgstat_shared.func_retired_oids.insert(e.oid);
        }
        eng.pgstat_func_pending.remove(&n);
        eng.pgstat_dropped_func.remove(&(session, xid, n));
    }
    eng.db.pgstat_shared.xact_commit += 1;
}

/// DROP TABLE / DROP INDEX executed inside a transaction: the
/// xact stats entry dies immediately (PG19: pg_stat_get_xact_*
/// returns 0 for the rest of the transaction), and the committed
/// entry is scheduled for removal at commit.
pub(crate) fn note_drop_rel(eng: &mut Engine, session: u64, own: u64, key: &str) {
    // PG19: the xact entry is flagged dropped (xact accessors
    // answer 0, the xact views hide it) but KEPT — if the
    // transaction rolls back, its counts merge like any other
    // aborted entry; the committed entry is removed at commit
    // via pgstat_dropped_rel below.
    if let Some(map) = eng.pgstat_xact.get_mut(&(session, own)) {
        if let Some(e) = map.get_mut(key) {
            e.dropped = true;
        }
    }
    eng.pgstat_dropped_rel
        .insert((session, own, key.to_string()));
}

/// DROP FUNCTION executed inside a transaction.
pub(crate) fn note_drop_func(eng: &mut Engine, session: u64, own: u64, name: &str) {
    eng.pgstat_dropped_func
        .insert((session, own, name.to_string()));
}

pub(crate) fn pgstat_abort(eng: &mut Engine, session: u64, xid: u64) {
    if let Some(map) = eng.pgstat_xact.remove(&(session, xid)) {
        for (key, e) in map {
            // Whole-transaction abort: PG merges the FIRST
            // generation (a mid-xact TRUNCATE never happened);
            // its inserts count, and its live inserts become dead.
            let first = e.gens.first().cloned().unwrap_or_default();
            let c = committed_entry(&mut eng.db, &key);
            c.n_tup_ins += first.ins;
            c.n_tup_upd += first.upd;
            c.n_tup_del += first.del;
            c.n_tup_hot_upd += first.hot;
            c.n_tup_newpage_upd += first.newpage;
            c.n_dead_tup += first.ins.max(0) + e.dead_from_rollback;
            c.n_mod_since_analyze += first.ins + first.upd + first.del;
            c.n_ins_since_vacuum += first.ins;
        }
    }
    eng.pgstat_func_xact.remove(&(session, xid));
    eng.pgstat_snapshot.remove(&(session, xid));
    eng.pgstat_dropped_rel
        .retain(|(s, x, _)| !(*s == session && *x == xid));
    eng.pgstat_dropped_func
        .retain(|(s, x, _)| !(*s == session && *x == xid));
    eng.db.pgstat_shared.xact_rollback += 1;
}

/// ROLLBACK TO SAVEPOINT: apply the pgstat rules against the
/// savepoint mark (a clone of the xact map taken at SAVEPOINT).
pub(crate) fn pgstat_rollback_to(
    eng: &mut Engine,
    session: u64,
    xid: u64,
    mark: &HashMap<String, PgXactRel, FxBuildHasher>,
) {
    if let Some(map) = eng.pgstat_xact.get_mut(&(session, xid)) {
        // Entries dropped after the savepoint (DROP TABLE removes the
        // xact entry) come back from the mark.
        for (k, v) in mark.iter() {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        let keys: Vec<String> = map.keys().cloned().collect();
        for key in keys {
            let Some(e) = map.get_mut(&key) else { continue };
            match mark.get(&key) {
                None => {
                    // A DROP after the savepoint is undone too.
                    e.dropped = false;
                    // First touched after the savepoint: cumulative
                    // counts survive; live inserts become dead at
                    // commit (PG19 pgstat: rolled-back inserts are
                    // dead tuples once the subxact is gone). A TRUNCATE
                    // after the savepoint collapses back to the first
                    // generation: the post-truncate generation belongs
                    // to the rolled-back truncate and counts nothing.
                    if e.gens.len() > 1 {
                        e.gens.truncate(1);
                    }
                    let live = e.cur_ref().live_delta;
                    if live > 0 {
                        e.dead_from_rollback += live;
                        e.cur().live_delta = 0;
                    }
                }
                Some(m) => {
                    e.dropped = m.dropped;
                    // A TRUNCATE after the savepoint pushed extra
                    // generations: drop them (the rolled-back
                    // truncate's generation counts nothing), then
                    // apply the same-generation rule to the surviving
                    // generation — keep its cumulative counts (the
                    // pre-truncate inserts past the savepoint DO count,
                    // e.g. stats.sql's trunc_stats_test3 expects
                    // n_tup_ins=4), restore the live delta to the
                    // mark, and convert the live inserts rolled back
                    // past the mark into dead-at-commit.
                    if e.gens.len() > m.gens.len() {
                        e.gens.truncate(m.gens.len());
                    }
                    let cur_live = e.cur_ref().live_delta;
                    let mark_live = m.cur_ref().live_delta;
                    if cur_live > mark_live {
                        e.dead_from_rollback += cur_live - mark_live;
                    }
                    e.cur().live_delta = mark_live;
                }
            }
        }
    }
    // Drop marks whose DROP this rollback undid must not fire
    // at commit (the relation's committed stats entry would be
    // deleted for a drop that never committed).
    let map = eng.pgstat_xact.get(&(session, xid));
    eng.pgstat_dropped_rel.retain(|(s, x, k)| {
        !(*s == session && *x == xid) || map.is_some_and(|m| m.get(k).is_some_and(|e| e.dropped))
    });
}

/// Snapshot of the xact map for a SAVEPOINT mark.
pub(crate) fn pgstat_mark(
    eng: &Engine,
    session: u64,
    xid: u64,
) -> HashMap<String, PgXactRel, FxBuildHasher> {
    eng.pgstat_xact
        .get(&(session, xid))
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// View schemas + scans (PG19 system_views.sql shapes)
// ---------------------------------------------------------------------------

fn cols(name: &str, spec: &[(&str, ColType)]) -> Vec<QCol> {
    spec.iter()
        .map(|(n, ty)| QCol {
            qual: name.to_string(),
            name: n.to_string(),
            ty: *ty,
            hidden: false,
            src_ord: 0,
        })
        .collect()
}

const TABLES_SPEC: &[(&str, ColType)] = &[
    ("relid", ColType::Int),
    ("schemaname", ColType::Text),
    ("relname", ColType::Text),
    ("seq_scan", ColType::BigInt),
    ("last_seq_scan", ColType::Timestamptz),
    ("seq_tup_read", ColType::BigInt),
    ("idx_scan", ColType::BigInt),
    ("last_idx_scan", ColType::Timestamptz),
    ("idx_tup_fetch", ColType::BigInt),
    ("n_tup_ins", ColType::BigInt),
    ("n_tup_upd", ColType::BigInt),
    ("n_tup_del", ColType::BigInt),
    ("n_tup_hot_upd", ColType::BigInt),
    ("n_tup_newpage_upd", ColType::BigInt),
    ("n_live_tup", ColType::BigInt),
    ("n_dead_tup", ColType::BigInt),
    ("n_mod_since_analyze", ColType::BigInt),
    ("n_ins_since_vacuum", ColType::BigInt),
    ("last_vacuum", ColType::Timestamptz),
    ("last_autovacuum", ColType::Timestamptz),
    ("last_analyze", ColType::Timestamptz),
    ("last_autoanalyze", ColType::Timestamptz),
    ("vacuum_count", ColType::BigInt),
    ("autovacuum_count", ColType::BigInt),
    ("analyze_count", ColType::BigInt),
    ("autoanalyze_count", ColType::BigInt),
    ("total_vacuum_time", ColType::Float),
    ("total_autovacuum_time", ColType::Float),
    ("total_analyze_time", ColType::Float),
    ("total_autoanalyze_time", ColType::Float),
    ("stats_reset", ColType::Timestamptz),
];

const XACT_TABLES_SPEC: &[(&str, ColType)] = &[
    ("relid", ColType::Int),
    ("schemaname", ColType::Text),
    ("relname", ColType::Text),
    ("seq_scan", ColType::BigInt),
    ("seq_tup_read", ColType::BigInt),
    ("idx_scan", ColType::BigInt),
    ("idx_tup_fetch", ColType::BigInt),
    ("n_tup_ins", ColType::BigInt),
    ("n_tup_upd", ColType::BigInt),
    ("n_tup_del", ColType::BigInt),
    ("n_tup_hot_upd", ColType::BigInt),
    ("n_tup_newpage_upd", ColType::BigInt),
];

const IO_TABLES_SPEC: &[(&str, ColType)] = &[
    ("relid", ColType::Int),
    ("schemaname", ColType::Text),
    ("relname", ColType::Text),
    ("heap_blks_read", ColType::BigInt),
    ("heap_blks_hit", ColType::BigInt),
    ("idx_blks_read", ColType::BigInt),
    ("idx_blks_hit", ColType::BigInt),
    ("toast_blks_read", ColType::BigInt),
    ("toast_blks_hit", ColType::BigInt),
    ("tidx_blks_read", ColType::BigInt),
    ("tidx_blks_hit", ColType::BigInt),
    ("stats_reset", ColType::Timestamptz),
];

const INDEXES_SPEC: &[(&str, ColType)] = &[
    ("relid", ColType::Int),
    ("indexrelid", ColType::Int),
    ("schemaname", ColType::Text),
    ("relname", ColType::Text),
    ("indexrelname", ColType::Text),
    ("idx_scan", ColType::BigInt),
    ("last_idx_scan", ColType::Timestamptz),
    ("idx_tup_read", ColType::BigInt),
    ("idx_tup_fetch", ColType::BigInt),
    ("stats_reset", ColType::Timestamptz),
];

const IO_INDEXES_SPEC: &[(&str, ColType)] = &[
    ("relid", ColType::Int),
    ("indexrelid", ColType::Int),
    ("schemaname", ColType::Text),
    ("relname", ColType::Text),
    ("indexrelname", ColType::Text),
    ("idx_blks_read", ColType::BigInt),
    ("idx_blks_hit", ColType::BigInt),
    ("stats_reset", ColType::Timestamptz),
];

const IO_SEQ_SPEC: &[(&str, ColType)] = &[
    ("relid", ColType::Int),
    ("schemaname", ColType::Text),
    ("relname", ColType::Text),
    ("blks_read", ColType::BigInt),
    ("blks_hit", ColType::BigInt),
    ("stats_reset", ColType::Timestamptz),
];

const DATABASE_SPEC: &[(&str, ColType)] = &[
    ("datid", ColType::Int),
    ("datname", ColType::Text),
    ("numbackends", ColType::Int),
    ("xact_commit", ColType::BigInt),
    ("xact_rollback", ColType::BigInt),
    ("blks_read", ColType::BigInt),
    ("blks_hit", ColType::BigInt),
    ("tup_returned", ColType::BigInt),
    ("tup_fetched", ColType::BigInt),
    ("tup_inserted", ColType::BigInt),
    ("tup_updated", ColType::BigInt),
    ("tup_deleted", ColType::BigInt),
    ("conflicts", ColType::BigInt),
    ("temp_files", ColType::BigInt),
    ("temp_bytes", ColType::BigInt),
    ("deadlocks", ColType::BigInt),
    ("checksum_failures", ColType::BigInt),
    ("checksum_last_failure", ColType::Timestamptz),
    ("blk_read_time", ColType::Float),
    ("blk_write_time", ColType::Float),
    ("session_time", ColType::Float),
    ("active_time", ColType::Float),
    ("idle_in_transaction_time", ColType::Float),
    ("sessions", ColType::BigInt),
    ("sessions_abandoned", ColType::BigInt),
    ("sessions_fatal", ColType::BigInt),
    ("sessions_killed", ColType::BigInt),
    ("parallel_workers_to_launch", ColType::BigInt),
    ("parallel_workers_launched", ColType::BigInt),
    ("stats_reset", ColType::Timestamptz),
];

const CONFLICTS_SPEC: &[(&str, ColType)] = &[
    ("datid", ColType::Int),
    ("datname", ColType::Text),
    ("confl_tablespace", ColType::BigInt),
    ("confl_lock", ColType::BigInt),
    ("confl_snapshot", ColType::BigInt),
    ("confl_bufferpin", ColType::BigInt),
    ("confl_deadlock", ColType::BigInt),
    ("confl_active_logicalslot", ColType::BigInt),
    ("stats_reset", ColType::Timestamptz),
];

const FUNCTIONS_SPEC: &[(&str, ColType)] = &[
    ("funcid", ColType::Int),
    ("schemaname", ColType::Text),
    ("funcname", ColType::Text),
    ("calls", ColType::BigInt),
    ("total_time", ColType::Float),
    ("self_time", ColType::Float),
    ("stats_reset", ColType::Timestamptz),
];

const XACT_FUNCTIONS_SPEC: &[(&str, ColType)] = &[
    ("funcid", ColType::Int),
    ("schemaname", ColType::Text),
    ("funcname", ColType::Text),
    ("calls", ColType::BigInt),
    ("total_time", ColType::Float),
    ("self_time", ColType::Float),
];

pub(crate) const IO_SPEC: &[(&str, ColType)] = &[
    ("backend_type", ColType::Text),
    ("object", ColType::Text),
    ("context", ColType::Text),
    ("reads", ColType::BigInt),
    ("read_bytes", ColType::BigInt),
    ("read_time", ColType::Float),
    ("writes", ColType::BigInt),
    ("write_bytes", ColType::BigInt),
    ("write_time", ColType::Float),
    ("writebacks", ColType::BigInt),
    ("writeback_time", ColType::Float),
    ("extends", ColType::BigInt),
    ("extend_bytes", ColType::BigInt),
    ("extend_time", ColType::Float),
    ("hits", ColType::BigInt),
    ("evictions", ColType::BigInt),
    ("reuses", ColType::BigInt),
    ("fsyncs", ColType::BigInt),
    ("fsync_time", ColType::Float),
    ("stats_reset", ColType::Timestamptz),
];

/// The PG19 pg_stat_io (backend_type, object, context) matrix
/// (pgstat_io.c; the stats suite's first query expects exactly
/// these 79 rows).
pub(crate) const IO_MATRIX: &[(&str, &str, &str)] = &[
    ("autovacuum launcher", "relation", "bulkread"),
    ("autovacuum launcher", "relation", "init"),
    ("autovacuum launcher", "relation", "normal"),
    ("autovacuum launcher", "wal", "init"),
    ("autovacuum launcher", "wal", "normal"),
    ("autovacuum worker", "relation", "bulkread"),
    ("autovacuum worker", "relation", "init"),
    ("autovacuum worker", "relation", "normal"),
    ("autovacuum worker", "relation", "vacuum"),
    ("autovacuum worker", "wal", "init"),
    ("autovacuum worker", "wal", "normal"),
    ("background worker", "relation", "bulkread"),
    ("background worker", "relation", "bulkwrite"),
    ("background worker", "relation", "init"),
    ("background worker", "relation", "normal"),
    ("background worker", "relation", "vacuum"),
    ("background worker", "temp relation", "normal"),
    ("background worker", "wal", "init"),
    ("background worker", "wal", "normal"),
    ("background writer", "relation", "init"),
    ("background writer", "relation", "normal"),
    ("background writer", "wal", "init"),
    ("background writer", "wal", "normal"),
    ("checkpointer", "relation", "init"),
    ("checkpointer", "relation", "normal"),
    ("checkpointer", "wal", "init"),
    ("checkpointer", "wal", "normal"),
    ("client backend", "relation", "bulkread"),
    ("client backend", "relation", "bulkwrite"),
    ("client backend", "relation", "init"),
    ("client backend", "relation", "normal"),
    ("client backend", "relation", "vacuum"),
    ("client backend", "temp relation", "normal"),
    ("client backend", "wal", "init"),
    ("client backend", "wal", "normal"),
    ("io worker", "relation", "bulkread"),
    ("io worker", "relation", "bulkwrite"),
    ("io worker", "relation", "init"),
    ("io worker", "relation", "normal"),
    ("io worker", "relation", "vacuum"),
    ("io worker", "temp relation", "normal"),
    ("io worker", "wal", "init"),
    ("io worker", "wal", "normal"),
    ("slotsync worker", "relation", "bulkread"),
    ("slotsync worker", "relation", "bulkwrite"),
    ("slotsync worker", "relation", "init"),
    ("slotsync worker", "relation", "normal"),
    ("slotsync worker", "relation", "vacuum"),
    ("slotsync worker", "temp relation", "normal"),
    ("slotsync worker", "wal", "init"),
    ("slotsync worker", "wal", "normal"),
    ("standalone backend", "relation", "bulkread"),
    ("standalone backend", "relation", "bulkwrite"),
    ("standalone backend", "relation", "init"),
    ("standalone backend", "relation", "normal"),
    ("standalone backend", "relation", "vacuum"),
    ("standalone backend", "wal", "init"),
    ("standalone backend", "wal", "normal"),
    ("startup", "relation", "bulkread"),
    ("startup", "relation", "bulkwrite"),
    ("startup", "relation", "init"),
    ("startup", "relation", "normal"),
    ("startup", "relation", "vacuum"),
    ("startup", "wal", "init"),
    ("startup", "wal", "normal"),
    ("walreceiver", "wal", "init"),
    ("walreceiver", "wal", "normal"),
    ("walsender", "relation", "bulkread"),
    ("walsender", "relation", "bulkwrite"),
    ("walsender", "relation", "init"),
    ("walsender", "relation", "normal"),
    ("walsender", "relation", "vacuum"),
    ("walsender", "temp relation", "normal"),
    ("walsender", "wal", "init"),
    ("walsender", "wal", "normal"),
    ("walsummarizer", "wal", "init"),
    ("walsummarizer", "wal", "normal"),
    ("walwriter", "wal", "init"),
    ("walwriter", "wal", "normal"),
];

pub(crate) const SLRU_NAMES: &[&str] = &[
    "commit_timestamp",
    "multixact_member",
    "multixact_offset",
    "notify",
    "serializable",
    "subtransaction",
    "transaction",
    "other",
];

pub(crate) const LOCK_TYPES: &[&str] = &[
    "relation",
    "extend",
    "frozenid",
    "page",
    "tuple",
    "transactionid",
    "virtualxid",
    "spectoken",
    "object",
    "userlock",
    "advisory",
    "applytransaction",
];

/// Bare view names this module serves (pg_stat_activity stays with
/// its existing catalog implementation).
pub(crate) fn is_pgstat_view(name: &str) -> bool {
    matches!(
        name,
        "pg_stat_all_tables"
            | "pg_stat_sys_tables"
            | "pg_stat_user_tables"
            | "pg_stat_xact_all_tables"
            | "pg_stat_xact_sys_tables"
            | "pg_stat_xact_user_tables"
            | "pg_statio_all_tables"
            | "pg_statio_sys_tables"
            | "pg_statio_user_tables"
            | "pg_stat_all_indexes"
            | "pg_stat_sys_indexes"
            | "pg_stat_user_indexes"
            | "pg_statio_all_indexes"
            | "pg_statio_sys_indexes"
            | "pg_statio_user_indexes"
            | "pg_statio_all_sequences"
            | "pg_statio_sys_sequences"
            | "pg_statio_user_sequences"
            | "pg_stat_database"
            | "pg_stat_database_conflicts"
            | "pg_stat_user_functions"
            | "pg_stat_xact_user_functions"
            | "pg_stat_archiver"
            | "pg_stat_bgwriter"
            | "pg_stat_checkpointer"
            | "pg_stat_io"
            | "pg_stat_wal"
            | "pg_stat_slru"
            | "pg_stat_lock"
            | "pg_stat_recovery_prefetch"
            | "pg_stat_replication"
            | "pg_stat_wal_receiver"
            | "pg_stat_recovery"
            | "pg_stat_subscription"
            | "pg_stat_ssl"
            | "pg_stat_gssapi"
            | "pg_stat_replication_slots"
            | "pg_stat_subscription_stats"
            | "pg_stat_progress_analyze"
            | "pg_stat_progress_vacuum"
            | "pg_stat_progress_cluster"
            | "pg_stat_progress_repack"
            | "pg_stat_progress_create_index"
            | "pg_stat_progress_basebackup"
            | "pg_stat_progress_copy"
            | "pg_stat_progress_data_checksums"
            | "pg_settings"
    )
}

pub(crate) fn pgstat_schema(name: &str) -> Vec<QCol> {
    match name {
        "pg_stat_all_tables" | "pg_stat_sys_tables" | "pg_stat_user_tables" => {
            cols(name, TABLES_SPEC)
        }
        "pg_stat_xact_all_tables" | "pg_stat_xact_sys_tables" | "pg_stat_xact_user_tables" => {
            cols(name, XACT_TABLES_SPEC)
        }
        "pg_statio_all_tables" | "pg_statio_sys_tables" | "pg_statio_user_tables" => {
            cols(name, IO_TABLES_SPEC)
        }
        "pg_stat_all_indexes" | "pg_stat_sys_indexes" | "pg_stat_user_indexes" => {
            cols(name, INDEXES_SPEC)
        }
        "pg_statio_all_indexes" | "pg_statio_sys_indexes" | "pg_statio_user_indexes" => {
            cols(name, IO_INDEXES_SPEC)
        }
        "pg_statio_all_sequences" | "pg_statio_sys_sequences" | "pg_statio_user_sequences" => {
            cols(name, IO_SEQ_SPEC)
        }
        "pg_stat_database" => cols(name, DATABASE_SPEC),
        "pg_stat_database_conflicts" => cols(name, CONFLICTS_SPEC),
        "pg_stat_user_functions" => cols(name, FUNCTIONS_SPEC),
        "pg_stat_xact_user_functions" => cols(name, XACT_FUNCTIONS_SPEC),
        "pg_stat_archiver" => cols(
            name,
            &[
                ("archived_count", ColType::BigInt),
                ("last_archived_wal", ColType::Text),
                ("last_archived_time", ColType::Timestamptz),
                ("failed_count", ColType::BigInt),
                ("last_failed_wal", ColType::Text),
                ("last_failed_time", ColType::Timestamptz),
                ("stats_reset", ColType::Timestamptz),
            ],
        ),
        "pg_stat_bgwriter" => cols(
            name,
            &[
                ("buffers_clean", ColType::BigInt),
                ("maxwritten_clean", ColType::BigInt),
                ("buffers_alloc", ColType::BigInt),
                ("stats_reset", ColType::Timestamptz),
            ],
        ),
        "pg_stat_checkpointer" => cols(
            name,
            &[
                ("num_timed", ColType::BigInt),
                ("num_requested", ColType::BigInt),
                ("num_done", ColType::BigInt),
                ("restartpoints_timed", ColType::BigInt),
                ("restartpoints_req", ColType::BigInt),
                ("restartpoints_done", ColType::BigInt),
                ("write_time", ColType::Float),
                ("sync_time", ColType::Float),
                ("buffers_written", ColType::BigInt),
                ("slru_written", ColType::BigInt),
                ("stats_reset", ColType::Timestamptz),
            ],
        ),
        "pg_stat_io" => cols(name, IO_SPEC),
        "pg_stat_wal" => cols(
            name,
            &[
                ("wal_records", ColType::BigInt),
                ("wal_fpi", ColType::BigInt),
                ("wal_bytes", ColType::Numeric(None)),
                ("wal_fpi_bytes", ColType::BigInt),
                ("wal_buffers_full", ColType::BigInt),
                ("stats_reset", ColType::Timestamptz),
            ],
        ),
        "pg_stat_slru" => cols(
            name,
            &[
                ("name", ColType::Text),
                ("blks_zeroed", ColType::BigInt),
                ("blks_hit", ColType::BigInt),
                ("blks_read", ColType::BigInt),
                ("blks_written", ColType::BigInt),
                ("blks_exists", ColType::BigInt),
                ("flushes", ColType::BigInt),
                ("truncates", ColType::BigInt),
                ("stats_reset", ColType::Timestamptz),
            ],
        ),
        "pg_stat_lock" => cols(
            name,
            &[
                ("locktype", ColType::Text),
                ("waits", ColType::BigInt),
                ("wait_time", ColType::Float),
                ("fastpath_exceeded", ColType::BigInt),
                ("stats_reset", ColType::Timestamptz),
            ],
        ),
        "pg_stat_recovery_prefetch" => cols(
            name,
            &[
                ("stats_reset", ColType::Timestamptz),
                ("prefetch", ColType::BigInt),
                ("hit", ColType::BigInt),
                ("skip_init", ColType::BigInt),
                ("skip_new", ColType::BigInt),
                ("skip_fpw", ColType::BigInt),
                ("skip_rep", ColType::BigInt),
                ("wal_distance", ColType::Float),
                ("block_distance", ColType::Float),
                ("io_depth", ColType::BigInt),
            ],
        ),
        "pg_stat_replication" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("usesysid", ColType::Int),
                ("usename", ColType::Text),
                ("application_name", ColType::Text),
                ("client_addr", ColType::Text),
                ("client_hostname", ColType::Text),
                ("client_port", ColType::Int),
                ("backend_start", ColType::Timestamptz),
                ("backend_xmin", ColType::Int),
                ("state", ColType::Text),
                ("sent_lsn", ColType::Text),
                ("write_lsn", ColType::Text),
                ("flush_lsn", ColType::Text),
                ("replay_lsn", ColType::Text),
                ("write_lag", ColType::Float),
                ("flush_lag", ColType::Float),
                ("replay_lag", ColType::Float),
                ("sync_priority", ColType::Int),
                ("sync_state", ColType::Text),
                ("reply_time", ColType::Timestamptz),
            ],
        ),
        "pg_stat_wal_receiver" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("status", ColType::Text),
                ("receive_start_lsn", ColType::Text),
                ("receive_start_tli", ColType::Int),
                ("written_lsn", ColType::Text),
                ("flushed_lsn", ColType::Text),
                ("received_tli", ColType::Int),
                ("last_msg_send_time", ColType::Timestamptz),
                ("last_msg_receipt_time", ColType::Timestamptz),
                ("latest_end_lsn", ColType::Text),
                ("latest_end_time", ColType::Timestamptz),
                ("slot_name", ColType::Text),
                ("sender_host", ColType::Text),
                ("sender_port", ColType::Int),
                ("conninfo", ColType::Text),
            ],
        ),
        "pg_stat_recovery" => cols(
            name,
            &[
                ("promote_triggered", ColType::Bool),
                ("last_replayed_read_lsn", ColType::Text),
                ("last_replayed_end_lsn", ColType::Text),
                ("last_replayed_tli", ColType::Int),
                ("replay_end_lsn", ColType::Text),
                ("replay_end_tli", ColType::Int),
                ("recovery_last_xact_time", ColType::Timestamptz),
                ("current_chunk_start_time", ColType::Timestamptz),
                ("pause_state", ColType::Text),
            ],
        ),
        "pg_stat_subscription" => cols(
            name,
            &[
                ("subid", ColType::Int),
                ("subname", ColType::Text),
                ("worker_type", ColType::Text),
                ("pid", ColType::Int),
                ("leader_pid", ColType::Int),
                ("relid", ColType::Int),
                ("received_lsn", ColType::Text),
                ("last_msg_send_time", ColType::Timestamptz),
                ("last_msg_receipt_time", ColType::Timestamptz),
                ("latest_end_lsn", ColType::Text),
                ("latest_end_time", ColType::Timestamptz),
            ],
        ),
        "pg_stat_ssl" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("ssl", ColType::Bool),
                ("version", ColType::Text),
                ("cipher", ColType::Text),
                ("bits", ColType::Int),
                ("client_dn", ColType::Text),
                ("client_serial", ColType::Numeric(None)),
                ("issuer_dn", ColType::Text),
            ],
        ),
        "pg_stat_gssapi" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("gss_authenticated", ColType::Bool),
                ("principal", ColType::Text),
                ("encrypted", ColType::Bool),
                ("credentials_delegated", ColType::Bool),
            ],
        ),
        "pg_stat_replication_slots" => cols(
            name,
            &[
                ("slot_name", ColType::Text),
                ("spill_txns", ColType::BigInt),
                ("spill_count", ColType::BigInt),
                ("spill_bytes", ColType::BigInt),
                ("stream_txns", ColType::BigInt),
                ("stream_count", ColType::BigInt),
                ("stream_bytes", ColType::BigInt),
                ("mem_exceeded_count", ColType::BigInt),
                ("total_txns", ColType::BigInt),
                ("total_bytes", ColType::BigInt),
                ("slotsync_skip_count", ColType::BigInt),
                ("slotsync_last_skip", ColType::Timestamptz),
                ("stats_reset", ColType::Timestamptz),
            ],
        ),
        "pg_stat_subscription_stats" => cols(
            name,
            &[
                ("subid", ColType::Int),
                ("subname", ColType::Text),
                ("apply_error_count", ColType::BigInt),
                ("sync_seq_error_count", ColType::BigInt),
                ("sync_table_error_count", ColType::BigInt),
                ("confl_insert_exists", ColType::BigInt),
                ("confl_update_origin_differs", ColType::BigInt),
                ("confl_update_exists", ColType::BigInt),
                ("confl_update_deleted", ColType::BigInt),
                ("confl_update_missing", ColType::BigInt),
                ("confl_delete_origin_differs", ColType::BigInt),
                ("confl_delete_missing", ColType::BigInt),
                ("confl_multiple_unique_conflicts", ColType::BigInt),
                ("stats_reset", ColType::Timestamptz),
            ],
        ),
        "pg_stat_progress_analyze" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("datid", ColType::Int),
                ("datname", ColType::Text),
                ("relid", ColType::Int),
                ("phase", ColType::Text),
                ("sample_blks_total", ColType::BigInt),
                ("sample_blks_scanned", ColType::BigInt),
                ("ext_stats_total", ColType::BigInt),
                ("ext_stats_computed", ColType::BigInt),
                ("child_tables_total", ColType::BigInt),
                ("child_tables_done", ColType::BigInt),
                ("current_child_table_relid", ColType::Int),
                ("delay_time", ColType::Float),
                ("started_by", ColType::Text),
            ],
        ),
        "pg_stat_progress_vacuum" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("datid", ColType::Int),
                ("datname", ColType::Text),
                ("relid", ColType::Int),
                ("phase", ColType::Text),
                ("heap_blks_total", ColType::BigInt),
                ("heap_blks_scanned", ColType::BigInt),
                ("heap_blks_vacuumed", ColType::BigInt),
                ("index_vacuum_count", ColType::BigInt),
                ("max_dead_tuple_bytes", ColType::BigInt),
                ("dead_tuple_bytes", ColType::BigInt),
                ("num_dead_item_ids", ColType::BigInt),
                ("indexes_total", ColType::BigInt),
                ("indexes_processed", ColType::BigInt),
                ("delay_time", ColType::Float),
                ("mode", ColType::Text),
                ("started_by", ColType::Text),
            ],
        ),
        "pg_stat_progress_cluster" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("datid", ColType::Int),
                ("datname", ColType::Text),
                ("relid", ColType::Int),
                ("command", ColType::Text),
                ("phase", ColType::Text),
                ("cluster_index_relid", ColType::Int),
                ("heap_tuples_scanned", ColType::BigInt),
                ("heap_tuples_written", ColType::BigInt),
                ("heap_blks_total", ColType::BigInt),
                ("heap_blks_scanned", ColType::BigInt),
                ("index_rebuild_count", ColType::BigInt),
            ],
        ),
        "pg_stat_progress_repack" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("datid", ColType::Int),
                ("datname", ColType::Text),
                ("relid", ColType::Int),
                ("command", ColType::Text),
                ("phase", ColType::Text),
                ("repack_index_relid", ColType::Int),
                ("heap_tuples_scanned", ColType::BigInt),
                ("heap_tuples_inserted", ColType::BigInt),
                ("heap_tuples_updated", ColType::BigInt),
                ("heap_tuples_deleted", ColType::BigInt),
                ("heap_blks_total", ColType::BigInt),
                ("heap_blks_scanned", ColType::BigInt),
                ("index_rebuild_count", ColType::BigInt),
            ],
        ),
        "pg_stat_progress_create_index" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("datid", ColType::Int),
                ("datname", ColType::Text),
                ("relid", ColType::Int),
                ("index_relid", ColType::Int),
                ("command", ColType::Text),
                ("phase", ColType::Text),
                ("lockers_total", ColType::BigInt),
                ("lockers_done", ColType::BigInt),
                ("current_locker_pid", ColType::Int),
                ("blocks_total", ColType::BigInt),
                ("blocks_done", ColType::BigInt),
                ("tuples_total", ColType::BigInt),
                ("tuples_done", ColType::BigInt),
                ("partitions_total", ColType::BigInt),
                ("partitions_done", ColType::BigInt),
            ],
        ),
        "pg_stat_progress_basebackup" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("phase", ColType::Text),
                ("backup_total", ColType::BigInt),
                ("backup_streamed", ColType::BigInt),
                ("tablespaces_total", ColType::BigInt),
                ("tablespaces_streamed", ColType::BigInt),
                ("backup_type", ColType::Text),
            ],
        ),
        "pg_stat_progress_copy" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("datid", ColType::Int),
                ("datname", ColType::Text),
                ("relid", ColType::Int),
                ("command", ColType::Text),
                ("type", ColType::Text),
                ("bytes_processed", ColType::BigInt),
                ("bytes_total", ColType::BigInt),
                ("tuples_processed", ColType::BigInt),
                ("tuples_excluded", ColType::BigInt),
                ("tuples_skipped", ColType::BigInt),
            ],
        ),
        "pg_stat_progress_data_checksums" => cols(
            name,
            &[
                ("pid", ColType::Int),
                ("datid", ColType::Int),
                ("datname", ColType::Text),
                ("phase", ColType::Text),
                ("databases_total", ColType::BigInt),
                ("databases_done", ColType::BigInt),
                ("relations_total", ColType::BigInt),
                ("relations_done", ColType::BigInt),
                ("blocks_total", ColType::BigInt),
                ("blocks_done", ColType::BigInt),
            ],
        ),
        "pg_settings" => cols(
            name,
            &[
                ("name", ColType::Text),
                ("setting", ColType::Text),
                ("unit", ColType::Text),
                ("category", ColType::Text),
                ("short_desc", ColType::Text),
                ("extra_desc", ColType::Text),
                ("context", ColType::Text),
                ("vartype", ColType::Text),
                ("source", ColType::Text),
                ("min_val", ColType::Text),
                ("max_val", ColType::Text),
                ("enumvals", ColType::Text),
                ("boot_val", ColType::Text),
                ("reset_val", ColType::Text),
                ("sourcefile", ColType::Text),
                ("sourceline", ColType::Int),
                ("pending_restart", ColType::Bool),
            ],
        ),
        _ => Vec::new(),
    }
}

fn is_sys_schema(schemaname: &str) -> bool {
    schemaname == "pg_catalog"
        || schemaname == "information_schema"
        || schemaname.starts_with("pg_toast")
}

fn schema_filter(name: &str, schemaname: &str) -> bool {
    if name.contains("_sys_") {
        is_sys_schema(schemaname)
    } else if name.contains("_user_") {
        !is_sys_schema(schemaname)
    } else {
        true
    }
}

fn entry_or_default(db: &Database, key: &str) -> PgRelStat {
    db.pgstat_rel.get(key).cloned().unwrap_or_default()
}

fn qrow(cells: Vec<Value>) -> QRow {
    QRow {
        cells: Row::new(cells),
        prov: Vec::new(),
    }
}

/// The scan entry point (called with the engine so the snapshot
/// timestamp and shared reset times materialize on first access).
pub(crate) fn pgstat_scan(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    name: &str,
) -> (Vec<QCol>, Vec<QRow>) {
    touch_snapshot(eng, session, own);
    let schema = pgstat_schema(name);
    let db = &eng.db;
    let rows = match name {
        "pg_stat_all_tables" | "pg_stat_sys_tables" | "pg_stat_user_tables" => {
            scan_tables(db, snap, own, session, name)
        }
        "pg_stat_xact_all_tables" | "pg_stat_xact_sys_tables" | "pg_stat_xact_user_tables" => {
            scan_xact_tables(eng, snap, own, session, name)
        }
        "pg_statio_all_tables" | "pg_statio_sys_tables" | "pg_statio_user_tables" => {
            scan_io_tables(db, snap, own, session, name)
        }
        "pg_stat_all_indexes" | "pg_stat_sys_indexes" | "pg_stat_user_indexes" => {
            scan_indexes(db, snap, own, session, name)
        }
        "pg_statio_all_indexes" | "pg_statio_sys_indexes" | "pg_statio_user_indexes" => {
            scan_io_indexes(db, snap, own, session, name)
        }
        "pg_statio_all_sequences" | "pg_statio_sys_sequences" | "pg_statio_user_sequences" => {
            scan_io_sequences(db, snap, own, name)
        }
        "pg_stat_database" => scan_database(db),
        "pg_stat_database_conflicts" => vec![qrow(vec![
            Value::Int(5),
            Value::text("postgres"),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            ts(db.pgstat_shared.db_stats_reset),
        ])],
        "pg_stat_user_functions" => scan_functions(db, snap, own),
        "pg_stat_xact_user_functions" => scan_xact_functions(eng, snap, own, session),
        "pg_stat_archiver" => {
            let sh = &db.pgstat_shared;
            vec![qrow(vec![
                Value::BigInt(sh.archiver_count),
                Value::Null,
                Value::Null,
                Value::BigInt(sh.archiver_failed),
                Value::Null,
                Value::Null,
                ts(sh.archiver_stats_reset),
            ])]
        }
        "pg_stat_bgwriter" => {
            let sh = &db.pgstat_shared;
            vec![qrow(vec![
                Value::BigInt(sh.bgwriter_clean),
                Value::BigInt(sh.bgwriter_maxwritten),
                Value::BigInt(sh.buf_alloc),
                ts(sh.bgwriter_stats_reset),
            ])]
        }
        "pg_stat_checkpointer" => {
            let sh = &db.pgstat_shared;
            vec![qrow(vec![
                Value::BigInt(sh.ckpt_timed),
                Value::BigInt(sh.ckpt_requested),
                Value::BigInt(sh.ckpt_done),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::Float(0.0),
                Value::Float(0.0),
                Value::BigInt(sh.ckpt_done * 8),
                Value::BigInt(0),
                ts(sh.ckpt_stats_reset),
            ])]
        }
        "pg_stat_io" => scan_io(db),
        "pg_stat_wal" => {
            let sh = &db.pgstat_shared;
            vec![qrow(vec![
                Value::BigInt(sh.wal_records),
                Value::BigInt(sh.wal_fpi),
                Value::Numeric(Numeric::from_i64(sh.wal_bytes)),
                Value::BigInt(0),
                Value::BigInt(sh.wal_buffers_full),
                ts(sh.wal_stats_reset),
            ])]
        }
        "pg_stat_slru" => SLRU_NAMES
            .iter()
            .map(|n| {
                qrow(vec![
                    Value::text(*n),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    Value::BigInt(0),
                    ts(db.pgstat_shared.slru_stats_reset.get(*n).copied().flatten()),
                ])
            })
            .collect(),
        "pg_stat_lock" => LOCK_TYPES
            .iter()
            .map(|n| {
                qrow(vec![
                    Value::text(*n),
                    Value::BigInt(0),
                    Value::Float(0.0),
                    Value::BigInt(if *n == "relation" {
                        db.pgstat_shared.lock_fastpath_exceeded
                    } else {
                        0
                    }),
                    ts(db.pgstat_shared.lock_stats_reset),
                ])
            })
            .collect(),
        "pg_stat_recovery_prefetch" => vec![qrow(vec![
            ts(db.pgstat_shared.recovery_prefetch_stats_reset),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::BigInt(0),
        ])],
        "pg_settings" => scan_settings(),
        _ => Vec::new(),
    };
    (schema, rows)
}

fn scan_tables(db: &Database, snap: &Snapshot, own: u64, session: u64, view: &str) -> Vec<QRow> {
    let mut rows = Vec::new();
    // The shared pseudo-relation (pg_shdescription counters).
    if schema_filter(view, "pg_catalog") {
        let e = entry_or_default(db, SHOBJ_KEY);
        rows.push(table_row(SHOBJ_OID, "pg_catalog", "pg_shdescription", &e));
    }
    for t in list_tables(db, snap, own, session) {
        if t.kind == b'p' {
            // Partitioned parents carry no heap counters in PG.
            if schema_filter(view, &t.schemaname) {
                rows.push(table_row(
                    t.oid,
                    &t.schemaname,
                    &t.relname,
                    &PgRelStat::default(),
                ));
            }
            continue;
        }
        if !schema_filter(view, &t.schemaname) {
            continue;
        }
        let e = entry_or_default(db, &t.key);
        rows.push(table_row(t.oid, &t.schemaname, &t.relname, &e));
    }
    rows
}

fn table_row(oid: i64, schemaname: &str, relname: &str, e: &PgRelStat) -> QRow {
    qrow(vec![
        Value::Int(oid),
        Value::text(schemaname),
        Value::text(relname),
        Value::BigInt(e.seq_scan),
        ts(e.last_seq_scan),
        Value::BigInt(e.seq_tup_read),
        Value::BigInt(e.idx_scan),
        ts(e.last_idx_scan),
        Value::BigInt(e.idx_tup_fetch),
        Value::BigInt(e.n_tup_ins),
        Value::BigInt(e.n_tup_upd),
        Value::BigInt(e.n_tup_del),
        Value::BigInt(e.n_tup_hot_upd),
        Value::BigInt(e.n_tup_newpage_upd),
        Value::BigInt(e.n_live_tup),
        Value::BigInt(e.n_dead_tup),
        Value::BigInt(e.n_mod_since_analyze),
        Value::BigInt(e.n_ins_since_vacuum),
        ts(e.last_vacuum),
        ts(e.last_autovacuum),
        ts(e.last_analyze),
        ts(e.last_autoanalyze),
        Value::BigInt(e.vacuum_count),
        Value::BigInt(e.autovacuum_count),
        Value::BigInt(e.analyze_count),
        Value::BigInt(e.autoanalyze_count),
        Value::Float(0.0),
        Value::Float(0.0),
        Value::Float(0.0),
        Value::Float(0.0),
        ts(e.stats_reset),
    ])
}

fn scan_xact_tables(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    view: &str,
) -> Vec<QRow> {
    let db = &eng.db;
    let mut rows = Vec::new();
    let Some(map) = eng.pgstat_xact.get(&(session, own)) else {
        return rows;
    };
    let tables = list_tables(db, snap, own, session);
    let indexes = list_indexes(db, snap, own, session);
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for key in keys {
        // Index keys appear in the xact map for accessor support;
        // the xact *tables* view lists relations only.
        if indexes.iter().any(|i| &i.key == key) {
            continue;
        }
        let e = &map[key];
        if e.dropped {
            continue;
        }
        let (oid, schemaname, relname) = if key == SHOBJ_KEY {
            (
                SHOBJ_OID,
                "pg_catalog".to_string(),
                "pg_shdescription".to_string(),
            )
        } else if let Some(t) = tables.iter().find(|t| &t.key == key) {
            (t.oid, t.schemaname.clone(), t.relname.clone())
        } else {
            let (s, r) = key_parts(db, key);
            (0, s, r)
        };
        if !schema_filter(view, &schemaname) {
            continue;
        }
        rows.push(qrow(vec![
            Value::Int(oid),
            Value::text(schemaname.as_str()),
            Value::text(relname.as_str()),
            Value::BigInt(e.seq_scan),
            Value::BigInt(e.seq_tup_read),
            Value::BigInt(e.idx_scan),
            Value::BigInt(e.idx_tup_fetch),
            Value::BigInt(e.xact_ins()),
            Value::BigInt(e.xact_upd()),
            Value::BigInt(e.xact_del()),
            Value::BigInt(e.xact_hot()),
            Value::BigInt(e.xact_newpage()),
        ]));
    }
    rows
}

fn scan_io_tables(db: &Database, snap: &Snapshot, own: u64, session: u64, view: &str) -> Vec<QRow> {
    let indexes = list_indexes(db, snap, own, session);
    let mut rows = Vec::new();
    for t in list_tables(db, snap, own, session) {
        if !schema_filter(view, &t.schemaname) {
            continue;
        }
        let e = entry_or_default(db, &t.key);
        let (mut idx_read, mut idx_hit) = (0i64, 0i64);
        for ix in indexes.iter().filter(|i| i.table_key == t.key) {
            let ie = entry_or_default(db, &ix.key);
            idx_read += ie.blks_read;
            idx_hit += ie.blks_hit;
        }
        rows.push(qrow(vec![
            Value::Int(t.oid),
            Value::text(t.schemaname.as_str()),
            Value::text(t.relname.as_str()),
            Value::BigInt(e.blks_read),
            Value::BigInt(e.blks_hit),
            Value::BigInt(idx_read),
            Value::BigInt(idx_hit),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            ts(e.stats_reset),
        ]));
    }
    rows
}

fn scan_indexes(db: &Database, snap: &Snapshot, own: u64, session: u64, view: &str) -> Vec<QRow> {
    let tables = list_tables(db, snap, own, session);
    let mut rows = Vec::new();
    for ix in list_indexes(db, snap, own, session) {
        if !schema_filter(view, &ix.schemaname) {
            continue;
        }
        let relid = tables
            .iter()
            .find(|t| t.key == ix.table_key)
            .map(|t| t.oid)
            .unwrap_or(0);
        let e = entry_or_default(db, &ix.key);
        rows.push(qrow(vec![
            Value::Int(relid),
            Value::Int(ix.oid),
            Value::text(ix.schemaname.as_str()),
            Value::text(ix.relname.as_str()),
            Value::text(ix.indexrelname.as_str()),
            Value::BigInt(e.idx_scan),
            ts(e.last_idx_scan),
            Value::BigInt(e.idx_tup_read),
            Value::BigInt(e.idx_tup_fetch),
            ts(e.stats_reset),
        ]));
    }
    rows
}

fn scan_io_indexes(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    view: &str,
) -> Vec<QRow> {
    let tables = list_tables(db, snap, own, session);
    let mut rows = Vec::new();
    for ix in list_indexes(db, snap, own, session) {
        if !schema_filter(view, &ix.schemaname) {
            continue;
        }
        let relid = tables
            .iter()
            .find(|t| t.key == ix.table_key)
            .map(|t| t.oid)
            .unwrap_or(0);
        let e = entry_or_default(db, &ix.key);
        rows.push(qrow(vec![
            Value::Int(relid),
            Value::Int(ix.oid),
            Value::text(ix.schemaname.as_str()),
            Value::text(ix.relname.as_str()),
            Value::text(ix.indexrelname.as_str()),
            Value::BigInt(e.blks_read),
            Value::BigInt(e.blks_hit),
            ts(e.stats_reset),
        ]));
    }
    rows
}

fn scan_io_sequences(db: &Database, snap: &Snapshot, own: u64, view: &str) -> Vec<QRow> {
    let mut rows = Vec::new();
    for s in list_sequences(db, snap, own) {
        if !schema_filter(view, &s.schemaname) {
            continue;
        }
        let e = entry_or_default(db, &s.key);
        rows.push(qrow(vec![
            Value::Int(s.oid),
            Value::text(s.schemaname.as_str()),
            Value::text(s.relname.as_str()),
            Value::BigInt(e.blks_read),
            Value::BigInt(e.blks_hit),
            ts(e.stats_reset),
        ]));
    }
    rows
}

fn scan_database(db: &Database) -> Vec<QRow> {
    let sh = &db.pgstat_shared;
    let (mut fetched, mut hit, mut returned, mut tfetch, mut ins, mut upd, mut del) =
        (0i64, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    for (k, e) in db.pgstat_rel.iter() {
        if is_temp_key(k) {
            continue;
        }
        fetched += e.blks_read + e.blks_hit;
        hit += e.blks_hit;
        returned += e.seq_tup_read + e.idx_tup_read;
        tfetch += e.tup_fetched;
        ins += e.n_tup_ins;
        upd += e.n_tup_upd;
        del += e.n_tup_del;
    }
    let shared = entry_or_default(db, SHOBJ_KEY);
    let db_row = |datid: i64,
                  datname: Value,
                  numbackends: i64,
                  xc: i64,
                  xr: i64,
                  sess: i64,
                  fr: i64,
                  fh: i64| {
        qrow(vec![
            Value::Int(datid),
            datname,
            Value::Int(numbackends),
            Value::BigInt(xc),
            Value::BigInt(xr),
            Value::BigInt(fr),
            Value::BigInt(fh),
            Value::BigInt(returned),
            Value::BigInt(tfetch),
            Value::BigInt(ins),
            Value::BigInt(upd),
            Value::BigInt(del),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(sh.deadlocks),
            Value::BigInt(0),
            Value::Null,
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::BigInt(sess),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            ts(sh.db_stats_reset),
        ])
    };
    vec![
        // datid 0: shared-relation row (PG: datname NULL).
        qrow(vec![
            Value::Int(0),
            Value::Null,
            Value::Int(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(shared.blks_read),
            Value::BigInt(shared.blks_hit),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::Null,
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            ts(sh.db_stats_reset),
        ]),
        db_row(
            5,
            Value::text("postgres"),
            1,
            sh.xact_commit,
            sh.xact_rollback,
            sh.sessions,
            fetched - hit,
            hit,
        ),
    ]
}

fn scan_functions(db: &Database, snap: &Snapshot, own: u64) -> Vec<QRow> {
    let mut rows = Vec::new();
    let mut names: Vec<&String> = db.pgstat_func.keys().collect();
    names.sort();
    for name in names {
        let e = &db.pgstat_func[name];
        if e.calls <= 0 {
            continue;
        }
        // PG: the view lists only functions that still exist
        // (DROP FUNCTION removes the definition eagerly in this
        // engine, with undo on rollback).
        let _ = (snap, own);
        if !db.functions.contains_key(name) {
            continue;
        }
        let (sch, fname) = catalog_name_parts(name);
        rows.push(qrow(vec![
            Value::Int(e.oid),
            Value::text(sch.as_str()),
            Value::text(fname.as_str()),
            Value::BigInt(e.calls),
            Value::Float(e.total_time),
            Value::Float(e.self_time),
            ts(e.stats_reset),
        ]));
    }
    rows
}

fn scan_xact_functions(eng: &Engine, snap: &Snapshot, own: u64, session: u64) -> Vec<QRow> {
    let db = &eng.db;
    let mut rows = Vec::new();
    let Some(map) = eng.pgstat_func_xact.get(&(session, own)) else {
        return rows;
    };
    let oids = func_oid_map(db, snap, own);
    let mut names: Vec<&String> = map.keys().collect();
    names.sort();
    for name in names {
        let (sch, fname) = catalog_name_parts(name);
        rows.push(qrow(vec![
            Value::Int(oids.get(name).copied().unwrap_or(0)),
            Value::text(sch.as_str()),
            Value::text(fname.as_str()),
            Value::BigInt(map[name]),
            Value::Float(0.0),
            Value::Float(0.0),
        ]));
    }
    rows
}

fn scan_io(db: &Database) -> Vec<QRow> {
    let sh = &db.pgstat_shared;
    IO_MATRIX
        .iter()
        .map(|(bt, obj, ctx)| {
            let c = sh
                .io
                .get(&((*bt).to_string(), (*obj).to_string(), (*ctx).to_string()))
                .cloned()
                .unwrap_or_default();
            qrow(vec![
                Value::text(*bt),
                Value::text(*obj),
                Value::text(*ctx),
                Value::BigInt(c.reads),
                Value::BigInt(c.read_bytes),
                Value::Float(0.0),
                Value::BigInt(c.writes),
                Value::BigInt(c.write_bytes),
                Value::Float(0.0),
                Value::BigInt(c.writebacks),
                Value::Float(0.0),
                Value::BigInt(c.extends),
                Value::BigInt(c.extend_bytes),
                Value::Float(0.0),
                Value::BigInt(c.hits),
                Value::BigInt(c.evictions),
                Value::BigInt(c.reuses),
                Value::BigInt(c.fsyncs),
                Value::Float(0.0),
                ts(sh.io_stats_reset),
            ])
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Function oid scheme: the engine's regproc oids are 40000 + the
// name-sorted index over function names (see cast.rs regproc_display)
// ---------------------------------------------------------------------------

pub(crate) fn func_oid_map(db: &Database, _snap: &Snapshot, _own: u64) -> HashMap<String, i64> {
    let mut names: Vec<&String> = db.functions.keys().collect();
    names.sort();
    names
        .into_iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), 40000 + i as i64))
        .collect()
}

fn func_name_for_oid(db: &Database, snap: &Snapshot, own: u64, oid: i64) -> Option<String> {
    func_oid_map(db, snap, own)
        .into_iter()
        .find(|(_, o)| *o == oid)
        .map(|(n, _)| n)
}

// ---------------------------------------------------------------------------
// Scalar pg_stat_get_* accessors + control functions.
// PG19 pgstatfuncs.c rules: relation int accessors return 0 when
// there is no entry (never NULL); timestamp accessors return NULL;
// function accessors return NULL when there is no entry.
// ---------------------------------------------------------------------------

fn eval_args(q: &mut Q, args: &[Expr], scopes: &[Scope]) -> Result<Vec<Value>, ExecError> {
    args.iter().map(|a| eval_expr(q, scopes, a)).collect()
}

fn oid_of(db: &Database, v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::BigInt(i) => Some(*i),
        // A regprocedure/regproc value rides as display text;
        // resolve signatures and bare function names back to OIDs.
        Value::Text(s) | Value::BpChar(s) => crate::exec::regprocedure_resolve(db, s)
            .ok()
            .map(|o| o as i64),
        _ => None,
    }
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.to_string()),
        _ => None,
    }
}

/// Committed relation entry for an accessor argument (oid int or
/// regclass text).
fn rel_entry_for(q: &Q, v: &Value) -> Option<(String, u8, PgRelStat)> {
    let (key, kind) = resolve_oid(&q.eng.db, q.snap, q.own, q.session, v)?;
    let e = entry_or_default(&q.eng.db, &key);
    Some((key, kind, e))
}

pub(crate) fn is_pgstat_func(name: &str) -> bool {
    name.starts_with("pg_stat_get_")
        || name.starts_with("pg_stat_reset")
        || matches!(
            name,
            "pg_stat_force_next_flush"
                | "pg_stat_clear_snapshot"
                | "pg_stat_have_stats"
                | "pg_backend_pid"
                | "current_schemas"
                | "shobj_description"
        )
}

/// Returns Some(result) when `name` is one of this family's scalar
/// functions; the caller (eval_func) falls through on None.
pub(crate) fn eval_pgstat_func(
    q: &mut Q,
    name: &str,
    args: &[Expr],
    scopes: &[Scope],
) -> Option<Result<Value, ExecError>> {
    if !is_pgstat_func(name) {
        return None;
    }
    Some(eval_pgstat_func_inner(q, name, args, scopes))
}

fn bi(v: i64) -> Value {
    Value::BigInt(v)
}

fn eval_pgstat_func_inner(
    q: &mut Q,
    name: &str,
    args: &[Expr],
    scopes: &[Scope],
) -> Result<Value, ExecError> {
    let argv = eval_args(q, args, scopes)?;
    let arg = |i: usize| argv.get(i).cloned().unwrap_or(Value::Null);
    // PG19: accessing statistics builds the transaction's
    // snapshot on first access (cache/snapshot consistency).
    // The timestamp getter (a pure read) and clear are exempt.
    if !matches!(
        name,
        "pg_stat_get_snapshot_timestamp" | "pg_stat_clear_snapshot"
    ) {
        touch_snapshot(q.eng, q.session, q.own);
    }
    match name {
        // ---- snapshot / flush control ----
        "pg_stat_get_snapshot_timestamp" => {
            // PG19: a pure read — the getter never builds a
            // snapshot (before the first stats access in the
            // transaction it returns NULL, as after a clear, and
            // always under stats_fetch_consistency = none).
            if consistency_mode() == "none" {
                return Ok(Value::Null);
            }
            Ok(q.eng
                .pgstat_snapshot
                .get(&(q.session, q.own))
                .copied()
                .map(Value::Timestamptz)
                .unwrap_or(Value::Null))
        }
        "pg_stat_clear_snapshot" => {
            clear_snapshot(q.eng, q.session, q.own);
            Ok(Value::Null)
        }
        "pg_stat_force_next_flush" => {
            flush_function_stats(q.eng);
            Ok(Value::Null)
        }
        // ---- backend identity ----
        "pg_backend_pid" => Ok(Value::Int(2000 + (q.session % 100000) as i64)),
        "pg_stat_get_backend_pid" => {
            let beid = oid_of(&q.eng.db, &arg(0)).unwrap_or(-1);
            Ok(match beid {
                1 => Value::Int(2000 + (q.session % 100000) as i64),
                2..=6 => Value::Int(100 + beid - 1),
                _ => Value::Null,
            })
        }
        "current_schemas" => {
            let include_implicit = !matches!(arg(0), Value::Bool(false));
            let mut out: Vec<Value> = Vec::new();
            // PG: the temp namespace persists for the session's
            // life once created (even with no temp objects left).
            let has_temp = q.eng.db.temp_schema_sessions.contains(&q.session)
                || q.eng
                    .db
                    .temp_tables
                    .get(&q.session)
                    .map(|m| !m.is_empty())
                    .unwrap_or(false)
                || q.eng
                    .db
                    .temp_indexes
                    .get(&q.session)
                    .map(|m| !m.is_empty())
                    .unwrap_or(false);
            if include_implicit && has_temp {
                out.push(Value::text("pg_temp_1"));
            }
            if include_implicit {
                out.push(Value::text("pg_catalog"));
            }
            let path =
                installed_guc("search_path").unwrap_or_else(|| "\"$user\", public".to_string());
            for part in path.split(',') {
                let p = part.trim().trim_matches('"');
                if p == "$user" {
                    out.push(Value::text(q.role));
                } else if !p.is_empty() && p != "pg_catalog" {
                    out.push(Value::text(p));
                }
            }
            let n = out.len() as i32;
            Ok(Value::Array(Box::new(crate::storage::ArrayVal {
                elem: crate::storage::ArrayElem::Text,
                dims: vec![n],
                lower: vec![1],
                elems: out,
            })))
        }
        // ---- shared-object description (pg_shdescription) ----
        "shobj_description" => {
            let oid = oid_of(&q.eng.db, &arg(0)).unwrap_or(-1);
            let cat = text_of(&arg(1)).unwrap_or_default();
            if oid == 5 && cat == "pg_database" {
                Ok(q.eng
                    .db
                    .db_comment
                    .clone()
                    .map(|c| Value::text(c))
                    .unwrap_or(Value::Null))
            } else {
                Ok(Value::Null)
            }
        }
        // ---- record-shaped SRFs used in scalar position ----
        "pg_stat_get_replication_slot" | "pg_stat_get_subscription_stats" => Ok(Value::Null),
        // ---- resets ----
        "pg_stat_reset" => {
            if let Some(dbname) = text_of(&arg(0)) {
                if dbname != "postgres" {
                    return Err(exec_err(
                        "3D000",
                        format!("database \"{}\" does not exist", dbname),
                    ));
                }
            }
            let t = now();
            let sess_prefix = format!("temp:{}:", q.session);
            for (k, e) in q.eng.db.pgstat_rel.iter_mut() {
                if k == SHOBJ_KEY {
                    continue;
                }
                if is_temp_key(k) && !k.starts_with(&sess_prefix) {
                    continue;
                }
                *e = PgRelStat {
                    stats_reset: Some(t),
                    ..PgRelStat::default()
                };
            }
            for e in q.eng.db.pgstat_func.values_mut() {
                e.calls = 0;
                e.total_time = 0.0;
                e.self_time = 0.0;
                e.stats_reset = Some(t);
            }
            q.eng.db.pgstat_shared.db_stats_reset = Some(t);
            Ok(Value::Null)
        }
        "pg_stat_reset_shared" => pgstat_reset_shared(q, text_of(&arg(0))),
        "pg_stat_reset_slru" => {
            let t = now();
            match text_of(&arg(0)) {
                None => {
                    for n in SLRU_NAMES {
                        q.eng
                            .db
                            .pgstat_shared
                            .slru_stats_reset
                            .insert(n.to_string(), Some(t));
                    }
                    Ok(Value::Null)
                }
                Some(n) => {
                    if SLRU_NAMES.contains(&n.as_str()) {
                        q.eng.db.pgstat_shared.slru_stats_reset.insert(n, Some(t));
                        Ok(Value::Null)
                    } else {
                        Err(exec_err(
                            "22023",
                            format!("unrecognized SLRU name: \"{}\"", n),
                        ))
                    }
                }
            }
        }
        "pg_stat_reset_single_table_counters" => {
            if let Some((key, _kind)) = resolve_oid(&q.eng.db, q.snap, q.own, q.session, &arg(0)) {
                let t = now();
                let e = committed_entry(&mut q.eng.db, &key);
                *e = PgRelStat {
                    stats_reset: Some(t),
                    ..PgRelStat::default()
                };
                if key == SHOBJ_KEY {
                    // PG: resetting a shared relation's counters
                    // also bumps the datid=0 database reset time.
                    q.eng.db.pgstat_shared.db_stats_reset = Some(t);
                }
            }
            Ok(Value::Null)
        }
        "pg_stat_reset_single_function_counters" => {
            if let Some(oid) = oid_of(&q.eng.db, &arg(0)) {
                if let Some(fname) = func_name_for_oid(&q.eng.db, q.snap, q.own, oid) {
                    if let Some(e) = q.eng.db.pgstat_func.get_mut(&fname) {
                        e.calls = 0;
                        e.total_time = 0.0;
                        e.self_time = 0.0;
                        e.stats_reset = Some(now());
                    }
                }
            }
            Ok(Value::Null)
        }
        "pg_stat_reset_backend_stats" => {
            let pid = oid_of(&q.eng.db, &arg(0)).unwrap_or(-1);
            if pid == 2000 + (q.session % 100000) as i64 {
                // PG zeroes the backend's counters; the stats
                // entries keep existing (pg_stat_get_backend_io
                // still returns its rows, zeroed).
                if let Some(per) = q.eng.db.pgstat_shared.backend_io.get_mut(&q.session) {
                    for c in per.values_mut() {
                        *c = PgIoCounts::default();
                    }
                }
                q.eng
                    .db
                    .pgstat_shared
                    .backend_wal_bytes
                    .insert(q.session, 0);
            }
            Ok(Value::Null)
        }
        "pg_stat_have_stats" => pgstat_have_stats(q, &argv),
        // ---- function stats ----
        "pg_stat_get_function_calls"
        | "pg_stat_get_function_total_time"
        | "pg_stat_get_function_self_time"
        | "pg_stat_get_function_stat_reset_time" => {
            let Some(oid) = oid_of(&q.eng.db, &arg(0)) else {
                return Ok(Value::Null);
            };
            // A function dropped by THIS transaction still
            // answers via its stored stats entry (PG drops the
            // entry only at commit) — resolve via the STORED
            // entry first: the catalog resolution derives OIDs
            // from the name-sorted position, which shifts when a
            // function is dropped, so it would resolve the dead
            // OID to the WRONG (surviving) function. An OID
            // retired by a committed drop answers NULL forever.
            if q.eng.db.pgstat_shared.func_retired_oids.contains(&oid) {
                return Ok(Value::Null);
            }
            let fname = q
                .eng
                .db
                .pgstat_func
                .iter()
                .find_map(|(n, e)| if e.oid == oid { Some(n.clone()) } else { None })
                .or_else(|| func_name_for_oid(&q.eng.db, q.snap, q.own, oid));
            let Some(fname) = fname else {
                return Ok(Value::Null);
            };
            let Some(e) = q.eng.db.pgstat_func.get(&fname).cloned() else {
                return Ok(Value::Null);
            };
            Ok(match name {
                "pg_stat_get_function_calls" => bi(e.calls),
                "pg_stat_get_function_total_time" => Value::Float(e.total_time),
                "pg_stat_get_function_self_time" => Value::Float(e.self_time),
                _ => ts(e.stats_reset),
            })
        }
        "pg_stat_get_xact_function_calls"
        | "pg_stat_get_xact_function_total_time"
        | "pg_stat_get_xact_function_self_time" => {
            let Some(oid) = oid_of(&q.eng.db, &arg(0)) else {
                return Ok(Value::Null);
            };
            let Some(fname) = func_name_for_oid(&q.eng.db, q.snap, q.own, oid) else {
                return Ok(Value::Null);
            };
            let n = q
                .eng
                .pgstat_func_xact
                .get(&(q.session, q.own))
                .and_then(|m| m.get(&fname))
                .copied();
            Ok(match n {
                None => Value::Null,
                Some(n) => match name {
                    "pg_stat_get_xact_function_calls" => bi(n),
                    _ => Value::Float(0.0),
                },
            })
        }
        // ---- database accessors ----
        _ if name.starts_with("pg_stat_get_db_") => Ok(pgstat_db_accessor(q, name)),
        // ---- xact relation accessors (0 when no entry, PG19) ----
        _ if name.starts_with("pg_stat_get_xact_") => {
            let v = arg(0);
            if matches!(v, Value::Null) {
                return Ok(bi(0));
            }
            let Some((key, kind)) = resolve_oid(&q.eng.db, q.snap, q.own, q.session, &v) else {
                return Ok(bi(0));
            };
            let e = q
                .eng
                .pgstat_xact
                .get(&(q.session, q.own))
                .and_then(|m| m.get(&key))
                .filter(|e| !e.dropped)
                .cloned();
            let (ins, upd, del, hot, newpage, seq, seq_read, idx, idx_fetch) = match &e {
                Some(e) => (
                    e.xact_ins(),
                    e.xact_upd(),
                    e.xact_del(),
                    e.xact_hot(),
                    e.xact_newpage(),
                    e.seq_scan,
                    e.seq_tup_read,
                    e.idx_scan,
                    e.idx_tup_fetch,
                ),
                None => (0, 0, 0, 0, 0, 0, 0, 0, 0),
            };
            Ok(bi(match name {
                "pg_stat_get_xact_numscans" => {
                    if kind == b'i' {
                        idx
                    } else {
                        seq
                    }
                }
                "pg_stat_get_xact_tuples_returned" => {
                    if kind == b'i' {
                        // index xact entry carries idx_tup_fetch in
                        // idx_fetch; returned for indexes = read.
                        idx_fetch
                    } else {
                        seq_read
                    }
                }
                "pg_stat_get_xact_tuples_fetched" => idx_fetch,
                "pg_stat_get_xact_tuples_inserted" => ins,
                "pg_stat_get_xact_tuples_updated" => upd,
                "pg_stat_get_xact_tuples_deleted" => del,
                "pg_stat_get_xact_tuples_hot_updated" => hot,
                "pg_stat_get_xact_tuples_newpage_updated" => newpage,
                _ => 0,
            }))
        }
        // ---- committed relation accessors ----
        _ if name.starts_with("pg_stat_get_") => {
            let v = arg(0);
            let resolved = if matches!(v, Value::Null) {
                None
            } else {
                rel_entry_for(q, &v)
            };
            let e = resolved.as_ref().map(|(_, _, e)| e.clone());
            let kind = resolved.as_ref().map(|(_, k, _)| *k).unwrap_or(b't');
            let get =
                |f: &dyn Fn(&PgRelStat) -> i64| -> i64 { e.as_ref().map(|e| f(e)).unwrap_or(0) };
            Ok(match name {
                "pg_stat_get_numscans" => bi(get(&|e| {
                    if kind == b'i' { e.idx_scan } else { e.seq_scan }
                })),
                "pg_stat_get_tuples_returned" => bi(get(&|e| {
                    if kind == b'i' {
                        e.idx_tup_read
                    } else {
                        e.seq_tup_read
                    }
                })),
                "pg_stat_get_tuples_fetched" => bi(get(&|e| {
                    if kind == b'i' {
                        e.idx_tup_fetch
                    } else {
                        e.tup_fetched
                    }
                })),
                "pg_stat_get_tuples_inserted" => bi(get(&|e| e.n_tup_ins)),
                "pg_stat_get_tuples_updated" => bi(get(&|e| e.n_tup_upd)),
                "pg_stat_get_tuples_deleted" => bi(get(&|e| e.n_tup_del)),
                "pg_stat_get_tuples_hot_updated" => bi(get(&|e| e.n_tup_hot_upd)),
                "pg_stat_get_tuples_newpage_updated" => bi(get(&|e| e.n_tup_newpage_upd)),
                "pg_stat_get_live_tuples" => bi(get(&|e| e.n_live_tup)),
                "pg_stat_get_dead_tuples" => bi(get(&|e| e.n_dead_tup)),
                "pg_stat_get_mod_since_analyze" => bi(get(&|e| e.n_mod_since_analyze)),
                "pg_stat_get_ins_since_vacuum" => bi(get(&|e| e.n_ins_since_vacuum)),
                "pg_stat_get_vacuum_count" => bi(get(&|e| e.vacuum_count)),
                "pg_stat_get_autovacuum_count" => bi(get(&|e| e.autovacuum_count)),
                "pg_stat_get_analyze_count" => bi(get(&|e| e.analyze_count)),
                "pg_stat_get_autoanalyze_count" => bi(get(&|e| e.autoanalyze_count)),
                "pg_stat_get_blocks_fetched" => bi(get(&|e| e.blks_read + e.blks_hit)),
                "pg_stat_get_blocks_hit" => bi(get(&|e| e.blks_hit)),
                "pg_stat_get_total_vacuum_time"
                | "pg_stat_get_total_autovacuum_time"
                | "pg_stat_get_total_analyze_time"
                | "pg_stat_get_total_autoanalyze_time" => Value::Float(0.0),
                "pg_stat_get_last_vacuum_time" => ts(e.as_ref().and_then(|e| e.last_vacuum)),
                "pg_stat_get_last_autovacuum_time" => {
                    ts(e.as_ref().and_then(|e| e.last_autovacuum))
                }
                "pg_stat_get_last_analyze_time" => ts(e.as_ref().and_then(|e| e.last_analyze)),
                "pg_stat_get_last_autoanalyze_time" => {
                    ts(e.as_ref().and_then(|e| e.last_autoanalyze))
                }
                "pg_stat_get_lastscan" => {
                    let s = e.as_ref().and_then(|e| e.last_seq_scan);
                    let i = e.as_ref().and_then(|e| e.last_idx_scan);
                    ts(s.max(i))
                }
                "pg_stat_get_stat_reset_time" => ts(e.as_ref().and_then(|e| e.stats_reset)),
                _ => Value::Null,
            })
        }
        _ => Ok(Value::Null),
    }
}

fn pgstat_db_accessor(q: &mut Q, name: &str) -> Value {
    // The single database this engine hosts (oid 5); shared
    // objects live under oid 0. Other oids have no entry → 0/NULL.
    let sh = &q.eng.db.pgstat_shared;
    match name {
        "pg_stat_get_db_numbackends" => Value::Int(1),
        "pg_stat_get_db_xact_commit" => bi(sh.xact_commit),
        "pg_stat_get_db_xact_rollback" => bi(sh.xact_rollback),
        "pg_stat_get_db_sessions" => bi(sh.sessions),
        "pg_stat_get_db_deadlocks" => bi(sh.deadlocks),
        "pg_stat_get_db_stat_reset_time" => ts(sh.db_stats_reset),
        "pg_stat_get_db_blocks_fetched" => bi(q
            .eng
            .db
            .pgstat_rel
            .values()
            .map(|e| e.blks_read + e.blks_hit)
            .sum()),
        "pg_stat_get_db_blocks_hit" => bi(q.eng.db.pgstat_rel.values().map(|e| e.blks_hit).sum()),
        "pg_stat_get_db_tuples_returned" => bi(q
            .eng
            .db
            .pgstat_rel
            .values()
            .map(|e| e.seq_tup_read + e.idx_tup_read)
            .sum()),
        "pg_stat_get_db_tuples_fetched" => {
            bi(q.eng.db.pgstat_rel.values().map(|e| e.tup_fetched).sum())
        }
        "pg_stat_get_db_tuples_inserted" => {
            bi(q.eng.db.pgstat_rel.values().map(|e| e.n_tup_ins).sum())
        }
        "pg_stat_get_db_tuples_updated" => {
            bi(q.eng.db.pgstat_rel.values().map(|e| e.n_tup_upd).sum())
        }
        "pg_stat_get_db_tuples_deleted" => {
            bi(q.eng.db.pgstat_rel.values().map(|e| e.n_tup_del).sum())
        }
        "pg_stat_get_db_blk_read_time"
        | "pg_stat_get_db_blk_write_time"
        | "pg_stat_get_db_session_time"
        | "pg_stat_get_db_active_time"
        | "pg_stat_get_db_idle_in_transaction_time" => Value::Float(0.0),
        "pg_stat_get_db_checksum_last_failure" => Value::Null,
        _ => bi(0),
    }
}

fn pgstat_reset_shared(q: &mut Q, target: Option<String>) -> Result<Value, ExecError> {
    let t = now();
    let sh = &mut q.eng.db.pgstat_shared;
    let reset_one = |sh: &mut crate::storage::PgSharedStat, target: &str| match target {
        "archiver" => {
            sh.archiver_count = 0;
            sh.archiver_failed = 0;
            sh.archiver_stats_reset = Some(t);
        }
        "bgwriter" => {
            sh.bgwriter_clean = 0;
            sh.bgwriter_maxwritten = 0;
            sh.buf_alloc = 0;
            sh.bgwriter_stats_reset = Some(t);
        }
        "checkpointer" => {
            sh.ckpt_requested = 0;
            sh.ckpt_done = 0;
            sh.ckpt_stats_reset = Some(t);
        }
        "io" => {
            sh.io.clear();
            sh.io_stats_reset = Some(t);
        }
        "lock" => {
            sh.lock_fastpath_exceeded = 0;
            sh.lock_stats_reset = Some(t);
        }
        "recovery_prefetch" => {
            sh.recovery_prefetch_stats_reset = Some(t);
        }
        "slru" => {
            for n in SLRU_NAMES {
                sh.slru_stats_reset.insert(n.to_string(), Some(t));
            }
        }
        "wal" => {
            sh.wal_records = 0;
            sh.wal_fpi = 0;
            sh.wal_bytes = 0;
            sh.wal_buffers_full = 0;
            sh.wal_stats_reset = Some(t);
        }
        _ => {}
    };
    match target {
        None => {
            for target in [
                "archiver",
                "bgwriter",
                "checkpointer",
                "io",
                "lock",
                "recovery_prefetch",
                "slru",
                "wal",
            ] {
                reset_one(sh, target);
            }
            Ok(Value::Null)
        }
        Some(target) => {
            if [
                "archiver",
                "bgwriter",
                "checkpointer",
                "io",
                "lock",
                "recovery_prefetch",
                "slru",
                "wal",
            ]
            .contains(&target.as_str())
            {
                reset_one(sh, &target);
                Ok(Value::Null)
            } else {
                Err(ExecError {
                    code: "22023",
                    message: format!("unrecognized reset target: \"{}\"", target),
                    detail: None,
                    hint: Some(
                        "Target must be \"archiver\", \"bgwriter\", \"checkpointer\", \"io\", \
                         \"lock\", \"recovery_prefetch\", \"slru\", or \"wal\"."
                            .to_string(),
                    ),
                })
            }
        }
    }
}

fn pgstat_have_stats(q: &mut Q, argv: &[Value]) -> Result<Value, ExecError> {
    let kind = argv.first().and_then(|v| text_of(v)).unwrap_or_default();
    let dboid = argv.get(1).and_then(|v| oid_of(&q.eng.db, v)).unwrap_or(0);
    let objid = argv.get(2).and_then(|v| oid_of(&q.eng.db, v)).unwrap_or(0);
    let valid = [
        "archiver",
        "backend",
        "bgwriter",
        "checkpointer",
        "database",
        "function",
        "io",
        "lock",
        "relation",
        "replslot",
        "slru",
        "subscription",
        "wal",
    ];
    if !valid.contains(&kind.as_str()) {
        return Err(exec_err(
            "22023",
            format!("invalid statistics kind: \"{}\"", kind),
        ));
    }
    let have = match kind.as_str() {
        "database" => objid == 0 && (dboid == 0 || dboid == 5),
        "relation" => {
            let v = Value::Int(objid);
            match resolve_oid(&q.eng.db, q.snap, q.own, q.session, &v) {
                Some((key, _)) => q.eng.db.pgstat_rel.contains_key(&key),
                None => false,
            }
        }
        "function" => {
            let v = Value::Int(objid);
            let _ = v;
            match func_name_for_oid(&q.eng.db, q.snap, q.own, objid) {
                Some(n) => q.eng.db.pgstat_func.contains_key(&n),
                None => false,
            }
        }
        "replslot" | "subscription" => false,
        // Fixed-amount kinds always have their entry.
        _ => true,
    };
    Ok(Value::Bool(have))
}

// ---------------------------------------------------------------------------
// Set-returning backend functions (FROM-clause SRFs)
// ---------------------------------------------------------------------------

pub(crate) fn is_pgstat_srf(name: &str) -> bool {
    matches!(
        name,
        "pg_stat_get_backend_io" | "pg_stat_get_backend_idset" | "pg_stat_get_backend_wal"
    )
}

pub(crate) fn eval_pgstat_srf(
    q: &mut Q,
    name: &str,
    args: &[Expr],
    scopes: &[Scope],
) -> Option<Result<(Vec<QCol>, Vec<QRow>), ExecError>> {
    if !is_pgstat_srf(name) {
        return None;
    }
    Some((|| {
        let argv = eval_args(q, args, scopes)?;
        let pid = argv.first().and_then(|v| oid_of(&q.eng.db, v));
        pgstat_srf_rows(q, name, pid)
    })())
}

/// Values-based core (target-list SRF expansion evaluates args
/// before dispatch).
pub(crate) fn eval_pgstat_srf_vals(
    q: &mut Q,
    name: &str,
    vals: &[Value],
) -> Result<Vec<Row>, ExecError> {
    let pid = vals.first().and_then(|v| oid_of(&q.eng.db, v));
    let (_, rows) = pgstat_srf_rows(q, name, pid)?;
    Ok(rows.into_iter().map(|r| r.cells).collect())
}

fn pgstat_srf_rows(
    q: &mut Q,
    name: &str,
    pid: Option<i64>,
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    {
        let _ = &pid;
        let my_pid = 2000 + (q.session % 100000) as i64;
        match name {
            "pg_stat_get_backend_idset" => {
                let schema = cols(
                    "pg_stat_get_backend_idset",
                    &[("pg_stat_get_backend_idset", ColType::Int)],
                );
                let rows = (1..=6i64).map(|i| qrow(vec![Value::Int(i)])).collect();
                Ok((schema, rows))
            }
            "pg_stat_get_backend_io" => {
                let schema = cols("pg_stat_get_backend_io", IO_SPEC);
                let mut rows = Vec::new();
                if pid == Some(my_pid) {
                    let sh = &q.eng.db.pgstat_shared;
                    if let Some(per) = sh.backend_io.get(&q.session) {
                        let mut keys: Vec<&(String, String, String)> = per.keys().collect();
                        keys.sort();
                        for (bt, obj, ctx) in keys {
                            let c = &per[&(bt.clone(), obj.clone(), ctx.clone())];
                            rows.push(qrow(vec![
                                Value::text(bt.as_str()),
                                Value::text(obj.as_str()),
                                Value::text(ctx.as_str()),
                                Value::BigInt(c.reads),
                                Value::BigInt(c.read_bytes),
                                Value::Float(0.0),
                                Value::BigInt(c.writes),
                                Value::BigInt(c.write_bytes),
                                Value::Float(0.0),
                                Value::BigInt(c.writebacks),
                                Value::Float(0.0),
                                Value::BigInt(c.extends),
                                Value::BigInt(c.extend_bytes),
                                Value::Float(0.0),
                                Value::BigInt(c.hits),
                                Value::BigInt(c.evictions),
                                Value::BigInt(c.reuses),
                                Value::BigInt(c.fsyncs),
                                Value::Float(0.0),
                                ts(sh.io_stats_reset),
                            ]));
                        }
                    }
                }
                Ok((schema, rows))
            }
            "pg_stat_get_backend_wal" => {
                let schema = cols(
                    "pg_stat_get_backend_wal",
                    &[
                        ("wal_records", ColType::BigInt),
                        ("wal_fpi", ColType::BigInt),
                        ("wal_bytes", ColType::Numeric(None)),
                        ("wal_fpi_bytes", ColType::BigInt),
                        ("wal_buffers_full", ColType::BigInt),
                    ],
                );
                let mut rows = Vec::new();
                if pid == Some(my_pid) {
                    let bytes = q
                        .eng
                        .db
                        .pgstat_shared
                        .backend_wal_bytes
                        .get(&q.session)
                        .copied()
                        .unwrap_or(0);
                    rows.push(qrow(vec![
                        Value::BigInt(bytes / WAL_BYTES_PER_ROW),
                        Value::BigInt(0),
                        Value::Numeric(Numeric::from_i64(bytes)),
                        Value::BigInt(0),
                        Value::BigInt(0),
                    ]));
                }
                Ok((schema, rows))
            }
            _ => unreachable!(),
        }
    }
}

// ---------------------------------------------------------------------------
// pg_settings (virtual view over the GUC registry)
// ---------------------------------------------------------------------------

struct GucRow {
    name: &'static str,
    boot: &'static str,
    vartype: &'static str,
    category: &'static str,
    unit: &'static str,
    min: &'static str,
    max: &'static str,
    enums: &'static str,
    context: &'static str,
}

/// The GUCs this engine knows (mirrors server.rs's registry for
/// the settings the stats/guc suites read; `setting` comes from
/// the installed per-statement snapshot, else the boot value).
const GUCS: &[GucRow] = &[
    GucRow {
        name: "allow_in_place_tablespaces",
        boot: "off",
        vartype: "bool",
        category: "Developer Options",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "superuser",
    },
    GucRow {
        name: "application_name",
        boot: "",
        vartype: "string",
        category: "Reporting and Logging / What to Log",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "autovacuum",
        boot: "on",
        vartype: "bool",
        category: "Autovacuum",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "sighup",
    },
    GucRow {
        name: "block_size",
        boot: "8192",
        vartype: "integer",
        category: "Preset Options",
        unit: "",
        min: "8192",
        max: "8192",
        enums: "",
        context: "internal",
    },
    GucRow {
        name: "bytea_output",
        boot: "hex",
        vartype: "enum",
        category: "Client Connection Defaults / Locale and Formatting",
        unit: "",
        min: "",
        max: "",
        enums: "{escape,hex}",
        context: "user",
    },
    GucRow {
        name: "client_encoding",
        boot: "UTF8",
        vartype: "string",
        category: "Client Connection Defaults / Locale and Formatting",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "client_min_messages",
        boot: "notice",
        vartype: "enum",
        category: "Client Connection Defaults / Statement Behavior",
        unit: "",
        min: "",
        max: "",
        enums: "{debug5,debug4,debug3,debug2,debug1,log,notice,warning,error}",
        context: "user",
    },
    GucRow {
        name: "datestyle",
        boot: "ISO, MDY",
        vartype: "string",
        category: "Client Connection Defaults / Locale and Formatting",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "default_transaction_isolation",
        boot: "read committed",
        vartype: "enum",
        category: "Client Connection Defaults / Statement Behavior",
        unit: "",
        min: "",
        max: "",
        enums: "{\"read uncommitted\",\"read committed\",\"repeatable read\",serializable}",
        context: "user",
    },
    GucRow {
        name: "enable_bitmapscan",
        boot: "on",
        vartype: "bool",
        category: "Query Tuning / Planner Method Configuration",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "enable_hashjoin",
        boot: "on",
        vartype: "bool",
        category: "Query Tuning / Planner Method Configuration",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "enable_indexonlyscan",
        boot: "on",
        vartype: "bool",
        category: "Query Tuning / Planner Method Configuration",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "enable_indexscan",
        boot: "on",
        vartype: "bool",
        category: "Query Tuning / Planner Method Configuration",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "enable_mergejoin",
        boot: "on",
        vartype: "bool",
        category: "Query Tuning / Planner Method Configuration",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "enable_nestloop",
        boot: "on",
        vartype: "bool",
        category: "Query Tuning / Planner Method Configuration",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "enable_seqscan",
        boot: "on",
        vartype: "bool",
        category: "Query Tuning / Planner Method Configuration",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "extra_float_digits",
        boot: "1",
        vartype: "integer",
        category: "Client Connection Defaults / Locale and Formatting",
        unit: "",
        min: "-15",
        max: "3",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "lc_messages",
        boot: "C",
        vartype: "string",
        category: "Client Connection Defaults / Locale and Formatting",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "superuser",
    },
    GucRow {
        name: "log_min_messages",
        boot: "warning",
        vartype: "enum",
        category: "Reporting and Logging / When to Log",
        unit: "",
        min: "",
        max: "",
        enums: "{debug5,debug4,debug3,debug2,debug1,info,notice,warning,error,log,fatal,panic}",
        context: "superuser",
    },
    GucRow {
        name: "max_connections",
        boot: "100",
        vartype: "integer",
        category: "Connections and Authentication / Connection Settings",
        unit: "",
        min: "1",
        max: "262143",
        enums: "",
        context: "postmaster",
    },
    GucRow {
        name: "max_locks_per_transaction",
        boot: "64",
        vartype: "integer",
        category: "Lock Management",
        unit: "",
        min: "10",
        max: "2147483647",
        enums: "",
        context: "postmaster",
    },
    GucRow {
        name: "row_security",
        boot: "on",
        vartype: "bool",
        category: "Client Connection Defaults / Statement Behavior",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "search_path",
        boot: "\"$user\", public",
        vartype: "string",
        category: "Client Connection Defaults / Statement Behavior",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "server_encoding",
        boot: "UTF8",
        vartype: "string",
        category: "Preset Options",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "internal",
    },
    GucRow {
        name: "server_version",
        boot: "19.0",
        vartype: "string",
        category: "Preset Options",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "internal",
    },
    GucRow {
        name: "session_replication_role",
        boot: "origin",
        vartype: "enum",
        category: "Client Connection Defaults / Statement Behavior",
        unit: "",
        min: "",
        max: "",
        enums: "{origin,replica,local}",
        context: "superuser",
    },
    GucRow {
        name: "shared_buffers",
        boot: "16384",
        vartype: "integer",
        category: "Resource Usage / Memory",
        unit: "8kB",
        min: "16",
        max: "1073741823",
        enums: "",
        context: "postmaster",
    },
    GucRow {
        name: "stats_fetch_consistency",
        boot: "cache",
        vartype: "enum",
        category: "Statistics / Monitoring",
        unit: "",
        min: "",
        max: "",
        enums: "{none,cache,snapshot}",
        context: "user",
    },
    GucRow {
        name: "synchronous_commit",
        boot: "on",
        vartype: "enum",
        category: "Write-Ahead Log / Settings",
        unit: "",
        min: "",
        max: "",
        enums: "{local,remote_write,remote_apply,on,off}",
        context: "user",
    },
    GucRow {
        name: "temp_buffers",
        boot: "1024",
        vartype: "integer",
        category: "Resource Usage / Memory",
        unit: "8kB",
        min: "100",
        max: "1073741823",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "timezone",
        boot: "GMT",
        vartype: "string",
        category: "Client Connection Defaults / Locale and Formatting",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "track_activities",
        boot: "on",
        vartype: "bool",
        category: "Statistics / Monitoring",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "superuser",
    },
    GucRow {
        name: "track_activity_query_size",
        boot: "1024",
        vartype: "integer",
        category: "Statistics / Monitoring",
        unit: "B",
        min: "100",
        max: "1048576",
        enums: "",
        context: "postmaster",
    },
    GucRow {
        name: "track_counts",
        boot: "on",
        vartype: "bool",
        category: "Statistics / Monitoring",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "superuser",
    },
    GucRow {
        name: "track_functions",
        boot: "none",
        vartype: "enum",
        category: "Statistics / Monitoring",
        unit: "",
        min: "",
        max: "",
        enums: "{none,pl,all}",
        context: "superuser",
    },
    GucRow {
        name: "track_io_timing",
        boot: "off",
        vartype: "bool",
        category: "Statistics / Monitoring",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "superuser",
    },
    GucRow {
        name: "track_wal_io_timing",
        boot: "off",
        vartype: "bool",
        category: "Statistics / Monitoring",
        unit: "",
        min: "",
        max: "",
        enums: "",
        context: "superuser",
    },
    GucRow {
        name: "transaction_isolation",
        boot: "read committed",
        vartype: "enum",
        category: "Client Connection Defaults / Statement Behavior",
        unit: "",
        min: "",
        max: "",
        enums: "{\"read uncommitted\",\"read committed\",\"repeatable read\",serializable}",
        context: "user",
    },
    GucRow {
        name: "wal_skip_threshold",
        boot: "2048",
        vartype: "integer",
        category: "Write-Ahead Log / Settings",
        unit: "kB",
        min: "0",
        max: "2147483647",
        enums: "",
        context: "user",
    },
    GucRow {
        name: "work_mem",
        boot: "4096",
        vartype: "integer",
        category: "Resource Usage / Memory",
        unit: "kB",
        min: "64",
        max: "2147483647",
        enums: "",
        context: "user",
    },
];

fn scan_settings() -> Vec<QRow> {
    GUCS.iter()
        .map(|g| {
            let setting = installed_guc(g.name).unwrap_or_else(|| g.boot.to_string());
            qrow(vec![
                Value::text(g.name),
                Value::text(setting.as_str()),
                if g.unit.is_empty() {
                    Value::Null
                } else {
                    Value::text(g.unit)
                },
                Value::text(g.category),
                Value::text(""),
                Value::Null,
                Value::text(g.context),
                Value::text(g.vartype),
                Value::text("default"),
                if g.min.is_empty() {
                    Value::Null
                } else {
                    Value::text(g.min)
                },
                if g.max.is_empty() {
                    Value::Null
                } else {
                    Value::text(g.max)
                },
                if g.enums.is_empty() {
                    Value::Null
                } else {
                    Value::text(g.enums)
                },
                Value::text(g.boot),
                Value::text(g.boot),
                Value::Null,
                Value::Null,
                Value::Bool(false),
            ])
        })
        .collect()
}
