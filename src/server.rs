//! v0.5 transaction core: MVCC statement execution, commit/abort paths,
//! savepoints, VACUUM, and the snapshot helper shared with the extended
//! protocol's parameter resolution.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::{self, BufWriter, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::copy;
use crate::exec::{self, ExecError, ExecResult, StmtCtx};
use crate::protocol::{Cursor, MsgBuilder, read_message, read_startup};
use crate::repl;
use crate::sql::{self, CopyFormat, CopyOptions, FetchDir, IsolationLevel, SetValue, Stmt};
use crate::storage::{ColType, Engine, Row, Snapshot, Value, WriteOp, undo_write_op};
use crate::wal::{self, Wal};

/// Buffered sink for server→client traffic. Reads still go through the
/// raw `TcpStream`; every message-loop iteration ends with a `flush()`.
pub(crate) type Writer = BufWriter<TcpStream>;

const SSL_REQUEST_CODE: i32 = 80877103;
/// v0.12: CancelRequest's magic protocol number. We don't support query
/// cancellation — PG would forward this to the target backend; we close
/// quietly instead (documented in README limitations).
const CANCEL_REQUEST_CODE: i32 = 80877102;
const PROTOCOL_V3: i32 = 196608;

/// A prepared statement created by SQL-level PREPARE (parameters bound
/// by EXECUTE). Separate namespace from the extended-protocol `stmts`.
#[derive(Clone)]
struct SqlPrepared {
    /// Declared parameter types (arity-checked at EXECUTE).
    types: Vec<String>,
    stmt: Stmt,
}

/// A prepared statement created by Parse (parameters not yet bound).
#[derive(Clone)]
struct Prepared {
    /// None = the Parse query string was empty.
    stmt: Option<Stmt>,
    /// Parameter OIDs declared in the Parse message (may be empty/partial).
    declared_oids: Vec<i32>,
}

/// A portal created by Bind: prepared statement with parameters already
/// substituted, plus any suspended SELECT rows for row-limited Execute.
struct Portal {
    stmt_name: String,
    /// None = empty query. Parameters substituted at Bind time.
    stmt: Option<Stmt>,
    pending: Option<PendingRows>,
    done: bool,
    last_tag: Option<String>,
    /// v0.8: the portal ran EXPLAIN — its completion tag is "EXPLAIN".
    explain: bool,
    /// v0.10: the portal ran INSERT/UPDATE/DELETE — its completion tag.
    dml_tag: Option<String>,
}

struct PendingRows {
    rows: Vec<Row>,
    pos: usize,
}

pub(crate) struct Session {
    /// Stable per-connection id (v0.9: session-local currval()).
    sid: u64,
    /// v0.11: authenticated role name (lowercased). Every statement's
    /// privilege checks key off this.
    role: String,
    stmts: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// v0.74: SQL-level `PREPARE name AS ...` statements, separate from
    /// the extended-protocol namespace (like PostgreSQL).
    sql_prepared: HashMap<String, SqlPrepared>,
    /// After an extended-protocol error: discard input until Sync.
    in_error: bool,
    /// Explicit transaction state; None = autocommit.
    txn: Option<Txn>,
    /// v0.16: open SQL cursors (DECLARE), keyed by cursor name.
    cursors: HashMap<String, SqlCursor>,
    // --- v0.17: session transaction defaults ---------------------------
    // SET SESSION CHARACTERISTICS AS TRANSACTION ... stores the defaults
    // for the next explicit transaction (like PG). None = not specified.
    default_txn_level: Option<IsolationLevel>,
    default_txn_read_only: Option<bool>,
    default_txn_deferrable: Option<bool>,
    // v0.17: SET TRANSACTION outside a transaction block stores
    // characteristics for the NEXT transaction only (one-shot).
    next_txn_level: Option<IsolationLevel>,
    next_txn_read_only: Option<bool>,
    next_txn_deferrable: Option<bool>,
    /// v0.29: `bytea_output` GUC (hex|escape). Controls bytea wire output
    /// format, like PG. Default is hex.
    bytea_output: crate::storage::ByteaOutput,
    /// v0.41: `default_toast_compression` GUC (PG19 enum GUC). Which
    /// compressor TOAST uses for columns without an explicit
    /// `COMPRESSION` method. Default is pglz.
    default_toast_compression: crate::storage::ToastCompression,
    /// v0.81: parsed-AST cache for the extended protocol. Keyed by the
    /// exact query text of a Parse message; each entry records the
    /// catalog epoch it was parsed under. A hit (epoch matches) skips
    /// `sql::parse_statement` entirely and clones the cached `Stmt`;
    /// parameter substitution still happens fresh per Bind. Bounded
    /// (`PARSE_CACHE_CAP`); `parse_cache_order` is FIFO eviction order.
    parse_cache: HashMap<String, CachedParse>,
    parse_cache_order: std::collections::VecDeque<String>,
    /// v0.81: parse-cache hit/miss counters (for benchmarks/tests).
    parse_cache_hits: u64,
    parse_cache_misses: u64,
}

/// v0.81: one cached parsed statement: the AST plus the catalog epoch it
/// was parsed under. `Stmt` is purely syntactic, so the epoch is
/// conservative invalidation (mirrors PG's plan-cache versioning).
struct CachedParse {
    epoch: u64,
    stmt: Stmt,
}

/// v0.81: maximum parsed ASTs cached per session.
const PARSE_CACHE_CAP: usize = 256;

/// v0.17: honest version identity. rustgres reports its OWN version, not
/// a PostgreSQL version it claims to be: the server speaks protocol 3.0
/// but is not feature-identical to any PG release, so advertising "16.0"
/// was misleading (it implied PG 16 compatibility we do not have).
/// `SERVER_VERSION_NUM` applies PG's XXYYZZ scheme to our own version.
/// `SHOW server_version`, `SHOW server_version_num`, `version()`, and the
/// startup ParameterStatus all read these constants so they agree.
/// v0.18-repair: the startup banners in main.rs/wal.rs now read this
/// constant too, instead of hardcoding a stale version string.
pub(crate) const SERVER_VERSION: &str = "0.20.0";
pub(crate) const SERVER_VERSION_NUM: &str = "2000";

/// Connection ids; process-local is fine (currval is in-memory only).
static NEXT_SID: AtomicU64 = AtomicU64::new(1);

/// One explicit transaction: an xid registered in the engine, an optional
/// fixed snapshot (REPEATABLE READ / SERIALIZABLE), and the log of
/// uncommitted writes. There is no private database copy: uncommitted
/// versions live in the shared tables stamped with our xid, invisible to
/// every other snapshot.
struct Txn {
    xid: u64,
    level: IsolationLevel,
    /// v0.15: READ ONLY / READ WRITE mode (None = not specified).
    /// v0.17: enforced — writes fail with 25006 (see
    /// `check_read_only` in run_statement).
    read_only: Option<bool>,
    /// v0.15: [NOT] DEFERRABLE mode (None = not specified).
    /// Parsed for PostgreSQL compatibility; not yet enforced.
    deferrable: Option<bool>,
    /// Pinned at the first data statement for RR/SERIALIZABLE.
    snapshot: Option<Snapshot>,
    /// Uncommitted writes, in order: the undo log (abort / ROLLBACK TO)
    /// and the commit-time WAL source.
    writes: Vec<WriteOp>,
    /// (name, writes.len(), row-lock count) stack for SAVEPOINT: rolling
    /// back to a savepoint undoes staged writes AND releases the row locks
    /// taken after it, like Postgres.
    savepoints: Vec<(String, usize, usize)>,
    /// v0.16: cursor marks in lockstep with `savepoints`.
    cursor_marks: Vec<CursorMark>,
    /// A failed statement aborts the transaction (Postgres semantics):
    /// only ROLLBACK / ROLLBACK TO / COMMIT are accepted afterwards.
    failed: bool,
    /// v0.66: GUC change stack for SET / SET LOCAL / RESET transaction
    /// semantics (PG19 guc.c: GUC_ACTION_SET persists at commit but
    /// reverts on abort; GUC_ACTION_LOCAL reverts at commit AND abort).
    /// Each entry records the value in effect when the change was made.
    guc_stack: Vec<GucStackEntry>,
    /// v0.66: `guc_stack` lengths in lockstep with `savepoints` — a
    /// ROLLBACK TO SAVEPOINT cancels SET/SET LOCAL effects made after
    /// the savepoint, like Postgres.
    guc_marks: Vec<usize>,
}

/// v0.66: snapshot of one stateful GUC's session value, for the
/// transaction GUC stack.
#[derive(Clone, Copy)]
enum SavedGuc {
    ReadOnly(Option<bool>),
    ByteaOutput(crate::storage::ByteaOutput),
    Toast(crate::storage::ToastCompression),
}

/// v0.66: one in-transaction GUC change.
struct GucStackEntry {
    /// GUC name (a string literal; only stateful GUCs push entries).
    name: &'static str,
    /// true = SET LOCAL (reverts at commit and abort);
    /// false = SET/RESET (persists at commit, reverts on abort).
    is_local: bool,
    /// Value in effect when this entry was pushed.
    saved: SavedGuc,
}

/// v0.66: read the current session value of a stateful GUC. None for
/// accepted no-op GUCs (no state to save or restore).
fn guc_saved_value(session: &Session, name: &str) -> Option<SavedGuc> {
    match name {
        "default_transaction_read_only" => Some(SavedGuc::ReadOnly(session.default_txn_read_only)),
        "bytea_output" => Some(SavedGuc::ByteaOutput(session.bytea_output)),
        "default_toast_compression" => Some(SavedGuc::Toast(session.default_toast_compression)),
        _ => None,
    }
}

/// v0.66: write a saved GUC value back into the session.
fn restore_saved_guc(session: &mut Session, name: &str, saved: SavedGuc) {
    match (name, saved) {
        ("default_transaction_read_only", SavedGuc::ReadOnly(v)) => {
            session.default_txn_read_only = v;
        }
        ("bytea_output", SavedGuc::ByteaOutput(v)) => {
            session.bytea_output = v;
        }
        ("default_toast_compression", SavedGuc::Toast(v)) => {
            session.default_toast_compression = v;
        }
        _ => {}
    }
}

/// v0.66: record the pre-change value of a stateful GUC on the
/// transaction's GUC stack. No-op in autocommit (nothing to revert).
fn push_guc_entry(session: &mut Session, name: &'static str, is_local: bool) {
    let saved = match guc_saved_value(session, name) {
        Some(s) => s,
        None => return,
    };
    if let Some(t) = session.txn.as_mut() {
        t.guc_stack.push(GucStackEntry {
            name,
            is_local,
            saved,
        });
    }
}

/// v0.66: transaction end. On abort (`commit=false`) every stacked
/// change reverts (PG: SET/SET LOCAL effects disappear with a
/// rollback). On commit, only SET LOCAL entries revert; SET/RESET
/// persist for the session.
fn revert_guc_stack(session: &mut Session, stack: Vec<GucStackEntry>, commit: bool) {
    for e in stack.into_iter().rev() {
        if commit && !e.is_local {
            continue;
        }
        restore_saved_guc(session, e.name, e.saved);
    }
}

impl Session {
    /// v0.81: parsed-AST cache lookup. Returns the cached `Stmt` clone on
    /// a hit (epoch matches), or parses, caches, and returns on a miss.
    /// Failed parses are not cached. FIFO-evicted at `PARSE_CACHE_CAP`.
    pub(crate) fn cached_parse(
        &mut self,
        engine: &mut crate::storage::Engine,
        query: &str,
    ) -> Result<crate::sql::Stmt, crate::sql::SqlError> {
        let epoch = engine.catalog_epoch;
        match self.parse_cache.get(query) {
            Some(cached) if cached.epoch == epoch => {
                self.parse_cache_hits += 1;
                Ok(cached.stmt.clone())
            }
            _ => {
                self.parse_cache_misses += 1;
                let s = crate::sql::parse_statement(query)?;
                if !self.parse_cache.contains_key(query) {
                    if self.parse_cache.len() >= PARSE_CACHE_CAP {
                        if let Some(old) = self.parse_cache_order.pop_front() {
                            self.parse_cache.remove(&old);
                        }
                    }
                    self.parse_cache_order.push_back(query.to_string());
                }
                self.parse_cache.insert(
                    query.to_string(),
                    CachedParse {
                        epoch,
                        stmt: s.clone(),
                    },
                );
                Ok(s)
            }
        }
    }

    pub(crate) fn new(role: String) -> Self {
        Session {
            sid: NEXT_SID.fetch_add(1, Ordering::Relaxed),
            role,
            stmts: HashMap::new(),
            portals: HashMap::new(),
            sql_prepared: HashMap::new(),
            in_error: false,
            txn: None,
            cursors: HashMap::new(),
            default_txn_level: None,
            default_txn_read_only: None,
            default_txn_deferrable: None,
            next_txn_level: None,
            next_txn_read_only: None,
            next_txn_deferrable: None,
            bytea_output: crate::storage::ByteaOutput::default(),
            default_toast_compression: crate::storage::ToastCompression::default(),
            // v0.81: parsed-AST cache starts empty.
            parse_cache: HashMap::new(),
            parse_cache_order: std::collections::VecDeque::new(),
            parse_cache_hits: 0,
            parse_cache_misses: 0,
        }
    }
}

/// v0.16: an open SQL cursor. The DECLARE-time query result is
/// materialized (columns + rows + position); FETCH advances `pos`.
/// (Named `SqlCursor` — `protocol::Cursor` is the wire-protocol cursor.)
pub(crate) struct SqlCursor {
    /// v0.89: lazy query — `Some` until the first FETCH materializes
    /// it (PG19 defers query errors to FETCH, not DECLARE). `None`
    /// once materialized.
    query: Option<sql::SelectStmt>,
    cols: Vec<(String, ColType)>,
    rows: Vec<Row>,
    /// Current row index: -1 = before the first row, rows.len() = after
    /// the last row (Postgres positions the cursor on the last row
    /// retrieved).
    pos: i64,
    with_hold: bool,
    /// v0.89: set when a FETCH raised an error — the portal is "dead
    /// to the world" (PG19); further FETCHes get `portal cannot be run`
    /// instead of re-executing.
    dead: bool,
}

/// v0.16: cursor state captured at SAVEPOINT time, kept in lockstep with
/// `Txn::savepoints`. On ROLLBACK TO, cursor positions rewind and cursors
/// created after the savepoint are closed — like Postgres.
struct CursorMark {
    // v0.89: fetch positions are NOT rewound by ROLLBACK TO (PG19
    // keeps them); only the cursor set is restored, closing cursors
    // declared after the savepoint.
    names: HashSet<String>,
}

/// v0.11: live connection counts per role, enforcing CONNECTION LIMIT.
/// Incremented after successful authentication, decremented when the
/// connection closes.
static SESSION_COUNTS: std::sync::OnceLock<Mutex<HashMap<String, usize>>> =
    std::sync::OnceLock::new();

/// v0.11: live connection counts per role (see SESSION_COUNTS).
/// v0.11: RAII guard for a CONNECTION LIMIT slot. Releasing the slot in
/// `Drop` frees it on *every* exit path — clean Terminate, client EOF,
/// read errors, and I/O failures between authentication and the session
/// loop alike — so a slot can never leak.
struct ConnSlot {
    role: String,
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut counts = lock_counts();
        if let Some(n) = counts.get_mut(&self.role) {
            *n = n.saturating_sub(1);
        }
    }
}

fn session_counts() -> &'static Mutex<HashMap<String, usize>> {
    SESSION_COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// v0.12: poison-recovering lock helpers.
///
/// Every worker thread shares the global engine/wal mutexes, so a panic
/// in one statement poisons the mutex and — with a plain `.unwrap()` —
/// would wedge the whole server: every subsequent connection thread
/// would panic on lock and die. Recovering with `into_inner()` keeps the
/// server alive (per-statement undo paths make a mid-statement panic
/// leave consistent state in the common cases) and logs loudly so the
/// underlying bug is not silent. This is the pragmatic middle ground
/// between "wedge forever" and PostgreSQL's "restart the postmaster".
pub(crate) fn lock_engine(engine: &Arc<Mutex<Engine>>) -> std::sync::MutexGuard<'_, Engine> {
    match engine.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            eprintln!(
                "v0.12 WARNING: engine lock was poisoned by a panicking worker thread; \
                 recovering the lock so the server stays up"
            );
            poisoned.into_inner()
        }
    }
}

pub(crate) fn lock_wal(wal: &Arc<Mutex<Wal>>) -> std::sync::MutexGuard<'_, Wal> {
    match wal.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            eprintln!(
                "v0.12 WARNING: wal lock was poisoned by a panicking worker thread; \
                 recovering the lock so the server stays up"
            );
            poisoned.into_inner()
        }
    }
}

fn lock_counts() -> std::sync::MutexGuard<'static, HashMap<String, usize>> {
    match session_counts().lock() {
        Ok(g) => g,
        Err(poisoned) => {
            eprintln!("v0.12 WARNING: session-count lock was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

/// v0.11: authentication mode. `RUSTGRES_AUTH=scram-sha-256` (or `scram`)
/// requires SCRAM-SHA-256 password auth; anything else (including unset)
/// is trust auth, like PostgreSQL's `trust` pg_hba line.
#[derive(PartialEq)]
enum AuthMode {
    Trust,
    Scram,
}

fn auth_mode() -> AuthMode {
    match std::env::var("RUSTGRES_AUTH").as_deref() {
        Ok("scram-sha-256") | Ok("scram") => AuthMode::Scram,
        _ => AuthMode::Trust,
    }
}

/// v0.11: parse the startup packet's key/value parameters. `buf` is the
/// raw packet body including the leading protocol int32.
fn parse_startup_params(buf: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if buf.len() < 4 {
        return out;
    }
    let mut parts = buf[4..].split(|b| *b == 0);
    loop {
        let k = parts.next();
        let v = parts.next();
        match (k, v) {
            (Some(k), Some(v)) => {
                if k.is_empty() {
                    break;
                }
                out.push((
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(v).into_owned(),
                ));
            }
            _ => break,
        }
    }
    out
}

pub fn handle_connection(stream: TcpStream, engine: Arc<Mutex<Engine>>, wal: Arc<Mutex<Wal>>) {
    // Reads use the raw socket; writes go through a BufWriter so each
    // message's small write_all calls (type byte, length, payload) — and,
    // crucially, thousands of DataRows — coalesce into a few large TCP
    // segments. Without this, TCP_NODELAY turns every row into its own
    // packet (see benches/BASELINE.md).
    let writer = BufWriter::new(stream.try_clone().expect("try_clone failed"));
    if let Err(e) = run_connection(stream, writer, engine, wal) {
        eprintln!("connection closed with error: {}", e);
    }
}

fn run_connection(
    mut reader: TcpStream,
    mut writer: Writer,
    engine: Arc<Mutex<Engine>>,
    wal: Arc<Mutex<Wal>>,
) -> io::Result<()> {
    // --- Startup handshake -------------------------------------------------
    let startup_params: Vec<(String, String)>;
    loop {
        let (proto, buf) = read_startup(&mut reader)?;
        if proto == SSL_REQUEST_CODE {
            // We don't do SSL; say so ('N') and wait for the real startup packet.
            writer.write_all(b"N")?;
            writer.flush()?;
            continue;
        }
        if proto == CANCEL_REQUEST_CODE {
            // No cancellation support: close quietly without an error log.
            return Ok(());
        }
        if proto != PROTOCOL_V3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported startup protocol {}", proto),
            ));
        }
        startup_params = parse_startup_params(&buf);
        break;
    }

    // v0.11: the startup packet's `user` selects the role (default
    // `postgres`, like PostgreSQL). Role names are case-folded.
    let user = startup_params
        .iter()
        .find(|(k, _)| k == "user")
        .map(|(_, v)| v.to_lowercase())
        .unwrap_or_else(|| "postgres".to_string());

    // v0.11: the startup packet's `database` selects the database. The
    // server hosts a single database ("postgres"); the name is honored in
    // authorization messages.
    let database = startup_params
        .iter()
        .find(|(k, _)| k == "database")
        .map(|(_, v)| v.to_lowercase())
        .unwrap_or_else(|| "postgres".to_string());

    // v0.11: authenticate (trust or SCRAM-SHA-256), then authorize
    // (CONNECT privilege + CONNECTION LIMIT). A `None` return means an
    // auth error was already sent; close the connection cleanly.
    let (role_name, _slot) =
        match authenticate(&mut reader, &mut writer, &engine, &user, &database)? {
            Some(pair) => pair,
            None => return Ok(()),
        };
    // `_slot` lives until the end of this function: its Drop releases the
    // CONNECTION LIMIT slot on every exit path, so no manual release is
    // needed (and none can be skipped by an early `?`).

    // ParameterStatus
    let params: &[(&str, &str)] = &[
        ("server_version", SERVER_VERSION),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("TimeZone", "UTC"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
    ];
    for (k, v) in params {
        MsgBuilder::new(b'S').cstr(k).cstr(v).send(&mut writer)?;
    }

    // BackendKeyData
    let pid = std::process::id() as i32;
    let secret = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i32)
        .unwrap_or(0);
    MsgBuilder::new(b'K')
        .i32(pid)
        .i32(secret)
        .send(&mut writer)?;

    // ReadyForQuery (idle)
    let mut session = Session::new(role_name.clone());

    // v0.13: `replication=true` connections speak the walsender protocol
    // instead of SQL (ParameterStatus/BackendKeyData above are shared).
    if repl::is_replication_startup(&startup_params) {
        if !repl::check_replication_role(&engine, &role_name) {
            send_error(
                &mut writer,
                "42501",
                "permission denied: replication connections require a superuser role",
            )?;
            writer.flush()?;
            return Ok(());
        }
        send_ready(&mut writer, &session)?;
        writer.flush()?;
        return repl::run_replication(&mut reader, &mut writer, &engine, &wal, role_name, database);
    }

    send_ready(&mut writer, &session)?;
    writer.flush()?;

    let result = message_loop(&mut reader, &mut writer, &engine, &wal, &mut session);
    // Disconnect cleanup: an open transaction must not leak its uncommitted
    // versions or its xid (both would pin snapshots and vacuum forever).
    // Postgres aborts the transaction on disconnect; do the same on every
    // exit path — clean Terminate, client EOF, and read errors alike.
    if session.txn.is_some() {
        let _ = txn_rollback(&engine, &mut session, false);
    }
    // v0.22: session-local temp tables vanish on disconnect (PostgreSQL
    // semantics), along with their indexes.
    {
        let mut guard = lock_engine(&engine);
        guard.db.drop_session_temps(session.sid);
    }
    result
}

/// v0.11: authenticate the startup-packet user. Returns the role name and
/// its connection-limit slot guard on success, or `None` after sending
/// an ErrorResponse (the connection is then closed cleanly, like
/// PostgreSQL). The guard must be held for the whole session so the
/// slot is released exactly once, on disconnect.
fn authenticate(
    reader: &mut TcpStream,
    writer: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    user: &str,
    database: &str,
) -> io::Result<Option<(String, ConnSlot)>> {
    // Snapshot the auth catalogs once for the whole exchange.
    let (snap, role) = {
        let guard = lock_engine(engine);
        let snap = guard.take_snapshot();
        let role = guard.db.find_role(user, &snap, 0).cloned();
        (snap, role)
    };
    match auth_mode() {
        AuthMode::Trust => {
            let r = match role {
                Some(r) => r,
                None => {
                    send_error(
                        writer,
                        "28P01",
                        &format!("role \"{}\" does not exist", user),
                    )?;
                    writer.flush()?;
                    return Ok(None);
                }
            };
            if !r.can_login {
                send_error(
                    writer,
                    "28000",
                    &format!("role \"{}\" is not permitted to log in", user),
                )?;
                writer.flush()?;
                return Ok(None);
            }
            // v0.11: authorize (CONNECT + CONNECTION LIMIT) before
            // AuthenticationOk, like PostgreSQL.
            let slot = finish_login(writer, engine, &r, &snap, database)?;
            if let Some(slot) = slot {
                MsgBuilder::new(b'R').i32(0).send(writer)?;
                Ok(Some((r.name.clone(), slot)))
            } else {
                Ok(None)
            }
        }
        AuthMode::Scram => scram_auth(reader, writer, engine, user, role, &snap, database),
    }
}

/// v0.11: post-authentication authorization shared by both auth modes:
/// the CONNECT privilege on the database, then the role's CONNECTION
/// LIMIT. On success the connection-limit slot is returned as a guard
/// that releases it when dropped.
fn finish_login(
    writer: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    role: &crate::storage::Role,
    snap: &crate::storage::Snapshot,
    database: &str,
) -> io::Result<Option<ConnSlot>> {
    {
        let guard = lock_engine(engine);
        if !crate::storage::db_connect_allowed(&guard.db, &role.name, snap, 0) {
            send_error(
                writer,
                "42501",
                &format!("permission denied for database \"{}\"", database),
            )?;
            writer.flush()?;
            return Ok(None);
        }
    }
    if role.connlimit >= 0 {
        let counts = lock_counts();
        let n = counts.get(&role.name).copied().unwrap_or(0);
        if n as i64 >= role.connlimit as i64 {
            send_error(
                writer,
                "53300",
                &format!("too many connections for role \"{}\"", role.name),
            )?;
            writer.flush()?;
            return Ok(None);
        }
    }
    // The guard's Drop releases the slot on every exit path.
    let mut counts = lock_counts();
    *counts.entry(role.name.clone()).or_insert(0) += 1;
    drop(counts);
    Ok(Some(ConnSlot {
        role: role.name.clone(),
    }))
}

/// v0.11: full SCRAM-SHA-256 exchange (RFC 7677, like PostgreSQL).
/// Unknown users get a dummy verifier so they are not enumerable by
/// timing; the "role does not exist" error is only sent after the
/// crypto verifies, exactly like PostgreSQL.
fn scram_auth(
    reader: &mut TcpStream,
    writer: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    user: &str,
    role: Option<crate::storage::Role>,
    snap: &crate::storage::Snapshot,
    database: &str,
) -> io::Result<Option<(String, ConnSlot)>> {
    // AuthenticationSASL, mechanisms = [SCRAM-SHA-256]. The list is a
    // sequence of C strings terminated by a single zero byte (like PG).
    MsgBuilder::new(b'R')
        .i32(10)
        .cstr("SCRAM-SHA-256")
        .u8(0)
        .send(writer)?;
    writer.flush()?;

    // SASLInitialResponse ('p'): mechanism name + initial client data.
    let msg = read_message(reader)?;
    if msg.typ != b'p' {
        send_error(writer, "08P01", "expected SASLInitialResponse")?;
        writer.flush()?;
        return Ok(None);
    }
    let mut cur = Cursor::new(&msg.payload);
    let mech = cur.read_cstring()?;
    if mech != "SCRAM-SHA-256" {
        send_error(
            writer,
            "08P01",
            &format!("unsupported SASL mechanism \"{}\"", mech),
        )?;
        writer.flush()?;
        return Ok(None);
    }
    let len = cur.read_i32()?;
    if len < 0 {
        send_error(writer, "08P01", "bad SASLInitialResponse length")?;
        writer.flush()?;
        return Ok(None);
    }
    let data = cur.read_bytes(len as usize)?;
    let client_first = String::from_utf8_lossy(&data).into_owned();

    let verifier = role
        .as_ref()
        .and_then(|r| r.password.clone())
        .unwrap_or_else(crate::crypto::dummy_verifier);
    let (exchange, server_first) =
        match crate::crypto::ScramExchange::begin(verifier, &client_first) {
            Ok(x) => x,
            Err(e) => {
                send_error(writer, "28P01", &e)?;
                writer.flush()?;
                return Ok(None);
            }
        };
    // AuthenticationSASLContinue.
    MsgBuilder::new(b'R')
        .i32(11)
        .bytes(server_first.as_bytes())
        .send(writer)?;
    writer.flush()?;

    // SASLResponse ('p'): client-final message.
    let msg = read_message(reader)?;
    if msg.typ != b'p' {
        send_error(writer, "08P01", "expected SASLResponse")?;
        writer.flush()?;
        return Ok(None);
    }
    let client_final = String::from_utf8_lossy(&msg.payload).into_owned();
    let server_final = match exchange.verify(&client_final) {
        Ok(s) => s,
        Err(_) => {
            // Wrong password or tampered messages: 28P01, like PG.
            send_error(writer, "28P01", "password authentication failed")?;
            writer.flush()?;
            return Ok(None);
        }
    };
    // Now that the crypto is done, resolve the role for real.
    let r = match role {
        Some(r) => r,
        None => {
            send_error(
                writer,
                "28P01",
                &format!("role \"{}\" does not exist", user),
            )?;
            writer.flush()?;
            return Ok(None);
        }
    };
    if r.password.is_none() {
        // SCRAM mode with no stored password can never succeed.
        send_error(writer, "28P01", "password authentication failed")?;
        writer.flush()?;
        return Ok(None);
    }
    if !r.can_login {
        send_error(
            writer,
            "28000",
            &format!("role \"{}\" is not permitted to log in", user),
        )?;
        writer.flush()?;
        return Ok(None);
    }
    // v0.11: VALID UNTIL enforcement (password expiry). Only checked for
    // password (SCRAM) authentication, like PostgreSQL; trust mode has no
    // password to expire.
    if crate::storage::password_expired(&r.valid_until) {
        send_error(
            writer,
            "28P01",
            &format!("password expired for role \"{}\"", user),
        )?;
        writer.flush()?;
        return Ok(None);
    }
    // AuthenticationSASLFinal, then (on successful authorization)
    // AuthenticationOk.
    MsgBuilder::new(b'R')
        .i32(12)
        .bytes(server_final.as_bytes())
        .send(writer)?;
    let slot = finish_login(writer, engine, &r, &snap, database)?;
    if let Some(slot) = slot {
        MsgBuilder::new(b'R').i32(0).send(writer)?;
        Ok(Some((r.name.clone(), slot)))
    } else {
        Ok(None)
    }
}

/// The per-message dispatch loop, extracted so `run_connection` can run
/// disconnect cleanup exactly once no matter how the loop exits.
fn message_loop(
    reader: &mut TcpStream,
    writer: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
) -> io::Result<()> {
    loop {
        let msg = read_message(reader)?;
        // After an extended-protocol error, discard everything until Sync.
        if session.in_error {
            match msg.typ {
                b'S' => {
                    send_ready(writer, session)?;
                    writer.flush()?;
                    session.in_error = false;
                }
                b'X' => break, // Terminate still terminates
                _ => {}
            }
            continue;
        }
        match msg.typ {
            b'Q' => {
                let mut cur = Cursor::new(&msg.payload);
                let sql_text = cur.read_cstring()?;
                handle_query(reader, writer, engine, wal, session, &sql_text)?;
            }
            b'P' => handle_parse(writer, engine, session, &msg.payload)?,
            b'B' => handle_bind(writer, engine, session, &msg.payload)?,
            b'D' => handle_describe(writer, engine, session, &msg.payload)?,
            b'E' => handle_execute(reader, writer, engine, wal, session, &msg.payload)?,
            b'C' => handle_close(writer, session, &msg.payload)?,
            b'X' => break, // Terminate
            b'H' => {
                writer.flush()?;
            }
            b'S' => {
                // Sync: ReadyForQuery with the real transaction status.
                send_ready(writer, session)?;
                writer.flush()?;
            }
            other => {
                // v0.12: PostgreSQL treats an unknown frontend message type
                // as FATAL (ERRCODE_PROTOCOL_VIOLATION): send the error and
                // close the connection rather than soldiering on.
                send_error(
                    writer,
                    "08P01",
                    &format!("invalid frontend message type '{}'", other as char),
                )?;
                writer.flush()?;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "protocol violation: unknown message type '{}'",
                        other as char
                    ),
                ));
            }
        }
        // Bulk responses (e.g. 10k DataRows) accumulate in the BufWriter
        // above; a single flush per message keeps the wire moving without
        // a syscall per row — and without it the client would hang waiting
        // for bytes we haven't pushed yet.
        writer.flush()?;
    }
    Ok(())
}

/// ReadyForQuery transaction status byte: 'I' idle, 'T' in transaction,
/// 'E' in a failed (aborted) transaction.
fn txn_status(session: &Session) -> u8 {
    match &session.txn {
        None => b'I',
        Some(t) if t.failed => b'E',
        Some(_) => b'T',
    }
}

pub(crate) fn send_ready(stream: &mut Writer, session: &Session) -> io::Result<()> {
    MsgBuilder::new(b'Z').u8(txn_status(session)).send(stream)
}

pub(crate) fn send_error(stream: &mut Writer, code: &str, message: &str) -> io::Result<()> {
    send_error_detail(stream, code, message, None)
}

/// v0.71: ErrorResponse with an optional PG-style DETAIL (`D`) field.
pub(crate) fn send_error_detail(
    stream: &mut Writer,
    code: &str,
    message: &str,
    detail: Option<&str>,
) -> io::Result<()> {
    let mut b = MsgBuilder::new(b'E');
    b.u8(b'S')
        .cstr("ERROR")
        .u8(b'V')
        .cstr("ERROR")
        .u8(b'C')
        .cstr(code)
        .u8(b'M')
        .cstr(message);
    if let Some(d) = detail {
        b.u8(b'D').cstr(d);
    }
    b.u8(0);
    b.send(stream)
}

/// v0.71: send an `ExecError`, including its DETAIL line when present.
pub(crate) fn send_exec_error(stream: &mut Writer, e: &exec::ExecError) -> io::Result<()> {
    send_error_detail(stream, e.code, &e.message, e.detail.as_deref())
}

/// Extended-protocol failure: ErrorResponse, then discard input until Sync.
/// Like Postgres, a mid-transaction error aborts the transaction too.
fn protocol_error(
    stream: &mut Writer,
    session: &mut Session,
    code: &str,
    message: &str,
) -> io::Result<()> {
    send_error(stream, code, message)?;
    stream.flush()?;
    if let Some(t) = session.txn.as_mut() {
        t.failed = true;
    }
    session.in_error = true;
    Ok(())
}

/// Parse an extended-protocol message payload with `parse`; a structural
/// failure (payload shorter than its counts claim, unterminated cstring)
/// becomes a PG-style 08P01 protocol-violation ErrorResponse with the
/// connection surviving — PostgreSQL's `pq_getmsgint` raises
/// ERRCODE_PROTOCOL_VIOLATION the same way instead of dropping the
/// connection. Returns `Ok(None)` after sending the error.
fn parse_msg<T>(
    stream: &mut Writer,
    session: &mut Session,
    payload: &[u8],
    parse: impl FnOnce(&mut Cursor) -> Result<T, (String, String)>,
) -> io::Result<Option<T>> {
    let mut cur = Cursor::new(payload);
    match parse(&mut cur) {
        Ok(v) => Ok(Some(v)),
        Err((code, msg)) => {
            protocol_error(stream, session, &code, &msg)?;
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Simple query protocol (v0.1 behavior + v0.5 MVCC transactions)
// ---------------------------------------------------------------------------

/// Run one simple-protocol Query message. The message may hold several
/// `;`-separated statements; each runs in its own implicit transaction
/// (statement-atomic), or inside the session's explicit transaction when
/// one is open. On the first error the remaining statements are skipped,
/// like Postgres. Exactly one ReadyForQuery closes the message.
fn handle_query(
    reader: &mut TcpStream,
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    sql_text: &str,
) -> io::Result<()> {
    if sql_text.trim().is_empty() {
        MsgBuilder::new(b'I').send(stream)?; // EmptyQueryResponse
        send_ready(stream, session)?;
        stream.flush()?;
        return Ok(());
    }

    for one in sql::split_statements(sql_text) {
        let stmt = match sql::parse_statement(&one) {
            Err(e) => {
                let msg = if e.code == "42601" {
                    format!("syntax error: {}", e.message)
                } else {
                    e.message.clone()
                };
                send_error(stream, e.code, &msg)?;
                // v0.77: PG aborts the transaction on ANY error —
                // including a simple-protocol parse error — so mark the
                // txn failed before skipping the rest of the message.
                if let Some(t) = session.txn.as_mut() {
                    t.failed = true;
                }
                break;
            }
            Ok(s) => s,
        };
        // The simple protocol has no parameters: a `$N` here is 42P02.
        let mut stmt = stmt;
        if let Err(e) = exec::subst_params(&mut stmt, &[]) {
            send_exec_error(stream, &e)?;
            // v0.77: same abort rule for the parameter-substitution
            // error as for parse errors above.
            if let Some(t) = session.txn.as_mut() {
                t.failed = true;
            }
            break;
        }
        // v0.10: COPY needs the protocol stream (CopyIn/CopyOut exchange).
        if let Stmt::Copy {
            table,
            columns,
            to_stdout,
            options,
        } = &stmt
        {
            // v0.17: READ ONLY enforcement — COPY FROM is a write.
            // (COPY TO is a read and stays allowed.)
            if !to_stdout && effective_read_only(session) {
                send_error(
                    stream,
                    "25006",
                    "cannot execute COPY in a read-only transaction",
                )?;
                break;
            }
            let copy_ok = if *to_stdout {
                handle_copy_to(stream, engine, wal, session, table, columns, options)
            } else {
                handle_copy_from(
                    reader, stream, engine, wal, session, table, columns, options,
                )
            };
            if let Err(e) = copy_ok {
                // I/O errors abort the connection; SQL errors were already
                // reported as ErrorResponse inside the handler.
                if e.kind() != io::ErrorKind::InvalidData {
                    return Err(e);
                }
            }
            continue;
        }
        match run_statement(engine, wal, session, &stmt) {
            Err(e) => {
                send_exec_error(stream, &e)?;
                break;
            }
            Ok(ExecResult::Select { columns, rows }) => {
                send_row_description(stream, &columns)?;
                let mut row_buf = Vec::new();
                for row in &rows {
                    send_data_row_buf(stream, row, &mut row_buf, session.bytea_output)?;
                }
                MsgBuilder::new(b'C')
                    .cstr(&format!("SELECT {}", rows.len()))
                    .send(stream)?;
            }
            Ok(ExecResult::Explain { columns, rows }) => {
                send_row_description(stream, &columns)?;
                let mut row_buf = Vec::new();
                for row in &rows {
                    send_data_row_buf(stream, row, &mut row_buf, session.bytea_output)?;
                }
                MsgBuilder::new(b'C').cstr("EXPLAIN").send(stream)?;
            }
            Ok(ExecResult::Command { tag }) => {
                MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
            }
            // v0.10: DML with RETURNING sends rows like a SELECT, then the
            // completion tag; without RETURNING it is just the tag.
            // (RowDescription is sent even for zero rows, like Postgres.)
            Ok(ExecResult::Dml { tag, columns, rows }) => {
                if !columns.is_empty() {
                    send_row_description(stream, &columns)?;
                    let mut row_buf = Vec::new();
                    for row in &rows {
                        send_data_row_buf(stream, row, &mut row_buf, session.bytea_output)?;
                    }
                }
                MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
            }
        }
    }

    send_ready(stream, session)?;
    stream.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// v0.10: COPY protocol
// ---------------------------------------------------------------------------

/// v0.17: outcome of reading the COPY FROM data stream.
enum CopyFromRead {
    /// CopyDone received — the raw accumulated bytes.
    Done(Vec<u8>),
    /// CopyFail or an unexpected message — (SQLSTATE, message) for the
    /// caller to report through its protocol path.
    SqlError(String, String),
}

/// v0.17: read CopyData until CopyDone / CopyFail — shared by the simple
/// and extended protocol COPY FROM paths. A client Terminate ('X') or a
/// transport failure is an io error (abort the connection); anything else
/// unexpected becomes an 08P01 SQL error for the caller to report.
fn copy_from_read(reader: &mut TcpStream) -> io::Result<CopyFromRead> {
    let mut data = Vec::new();
    loop {
        let msg = read_message(reader)?;
        match msg.typ {
            b'd' => {
                data.extend_from_slice(&msg.payload);
            }
            b'c' => return Ok(CopyFromRead::Done(data)), // CopyDone
            b'f' => {
                // CopyFail: the client reports why.
                let reason = String::from_utf8_lossy(&msg.payload);
                return Ok(CopyFromRead::SqlError(
                    "57000".to_string(),
                    format!("COPY failed: {}", reason),
                ));
            }
            b'X' => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client terminated during COPY",
                ));
            }
            _ => {
                return Ok(CopyFromRead::SqlError(
                    "08P01".to_string(),
                    format!(
                        "unexpected message '{}' during COPY FROM STDIN",
                        msg.typ as char
                    ),
                ));
            }
        }
    }
}

/// v0.17: COPY TO data fetch — shared by the simple and extended
/// protocol paths. Runs under the session's transaction context
/// (aborted txn -> 25P02; data errors mark the txn failed, like
/// run_statement). Returns ExecError for the caller to report through
/// its own protocol path.
fn copy_to_fetch(
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    table: &str,
    columns: &Option<Vec<String>>,
) -> Result<(Vec<(String, ColType)>, Vec<Row>), exec::ExecError> {
    if let Some(t) = &session.txn {
        if t.failed {
            return Err(exec::ExecError {
                detail: None,
                code: "25P02",
                message:
                    "current transaction is aborted, commands ignored until end of transaction block"
                        .to_string(),
            });
        }
    }
    let mut guard = lock_engine(engine);
    if session.txn.is_some() {
        let t = session.txn.as_mut().unwrap();
        let (snap, xid, level) = stmt_snapshot(&mut guard, Some(&mut *t));
        let mut ctx = StmtCtx {
            snap: &snap,
            own: xid,
            level,
            session: session.sid,
            role: &session.role,
            read_only: t.read_only == Some(true),
            writes: &mut t.writes,
            default_toast_compression: session.default_toast_compression,
        };
        match exec::copy_to_rows(&mut *guard, &mut ctx, table, columns) {
            Ok(r) => Ok(r),
            Err(e) => {
                t.failed = true;
                Err(e)
            }
        }
    } else {
        let xid = guard.begin_txn();
        let snap = guard.take_snapshot();
        let mut writes: Vec<WriteOp> = Vec::new();
        let result = {
            let mut ctx = StmtCtx {
                snap: &snap,
                own: xid,
                level: IsolationLevel::ReadCommitted,
                session: session.sid,
                role: &session.role,
                read_only: session.default_txn_read_only == Some(true),
                default_toast_compression: session.default_toast_compression,
                writes: &mut writes,
            };
            exec::copy_to_rows(&mut *guard, &mut ctx, table, columns)
        };
        // COPY TO is read-only; retire the xid.
        retire_txn(&mut guard, xid);
        result
    }
}

/// v0.17: send CopyOutResponse + CopyData (header + rows) + CopyDone —
/// shared by the simple and extended protocol COPY TO paths.
fn send_copy_out(
    stream: &mut Writer,
    cols: &[(String, ColType)],
    rows: &[Row],
    options: &CopyOptions,
) -> io::Result<()> {
    // CopyOutResponse: format (0=text, 1=csv), column count, per-column
    // format codes (all text=0).
    let fmt: i8 = match options.format {
        CopyFormat::Text => 0,
        CopyFormat::Csv => 1,
    };
    let mut b = MsgBuilder::new(b'H');
    b.u8(fmt as u8).i16(cols.len() as i16);
    for _ in cols {
        b.i16(0);
    }
    b.send(stream)?;
    // Header row (column names) when requested.
    if options.header {
        let mut data = Vec::new();
        let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
        let nulls = vec![false; names.len()];
        copy::format_row(&names, &nulls, &mut data, options);
        MsgBuilder::new(b'd').bytes(&data).send(stream)?;
    }
    // Data rows.
    let mut data = Vec::new();
    for row in rows {
        data.clear();
        let mut fields = Vec::with_capacity(row.len());
        let mut nulls = Vec::with_capacity(row.len());
        for v in row.iter() {
            match v.to_text() {
                None => {
                    fields.push(String::new());
                    nulls.push(true);
                }
                Some(s) => {
                    fields.push(s);
                    nulls.push(false);
                }
            }
        }
        copy::format_row(&fields, &nulls, &mut data, options);
        MsgBuilder::new(b'd').bytes(&data).send(stream)?;
    }
    MsgBuilder::new(b'c').send(stream)?; // CopyDone
    Ok(())
}

/// v0.17: resolve the COPY FROM column count (validates table/columns) —
/// shared by the simple and extended protocol paths.
fn copy_from_ncols(
    engine: &Arc<Mutex<Engine>>,
    session: u64,
    table: &str,
    columns: &Option<Vec<String>>,
) -> Result<usize, exec::ExecError> {
    let mut guard = lock_engine(engine);
    let snap = guard.take_snapshot();
    // Use a throwaway xid for the snapshot owner (read-only).
    let xid = guard.begin_txn();
    let r = exec::copy_ncols(&*guard, &snap, xid, session, table, columns);
    retire_txn(&mut guard, xid);
    r
}

/// v0.17: parse COPY FROM bytes and insert — shared by the simple and
/// extended protocol paths. A parse error rolls back the transaction
/// (if any), like the v0.10 simple-protocol behavior; an insert error
/// marks it failed. Returns the inserted row count.
fn copy_from_ingest(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    table: &str,
    columns: &Option<Vec<String>>,
    options: &CopyOptions,
    ncols: usize,
    data: &[u8],
) -> Result<u64, exec::ExecError> {
    // Parse (line-numbered errors).
    let parsed = match copy::parse_rows(data, options, ncols) {
        Ok(r) => r,
        Err(e) => {
            // The failed COPY aborts the transaction (like Postgres).
            if session.txn.is_some() {
                let _ = txn_rollback(engine, session, false);
            }
            return Err(exec::ExecError {
                detail: None,
                code: "22P04",
                message: format!("COPY error on line {}: {}", e.line, e.message),
            });
        }
    };
    // Insert (transactional: uses the session txn if any, else autocommit).
    if session.txn.is_some() {
        let mut guard = lock_engine(engine);
        let t = session.txn.as_mut().unwrap();
        let (snap, xid, level) = stmt_snapshot(&mut guard, Some(&mut *t));
        let r = {
            let mut ctx = StmtCtx {
                snap: &snap,
                own: xid,
                level,
                session: session.sid,
                role: &session.role,
                read_only: t.read_only == Some(true),
                default_toast_compression: session.default_toast_compression,
                writes: &mut t.writes,
            };
            exec::copy_from_rows(&mut *guard, &mut ctx, table, columns, parsed)
        };
        if r.is_err() {
            t.failed = true;
        }
        r
    } else {
        let mut guard = lock_engine(engine);
        let xid = guard.begin_txn();
        let snap = guard.take_snapshot();
        let mut writes: Vec<WriteOp> = Vec::new();
        let r = {
            let mut ctx = StmtCtx {
                snap: &snap,
                own: xid,
                level: IsolationLevel::ReadCommitted,
                session: session.sid,
                role: &session.role,
                read_only: session.default_txn_read_only == Some(true),
                default_toast_compression: session.default_toast_compression,
                writes: &mut writes,
            };
            exec::copy_from_rows(&mut *guard, &mut ctx, table, columns, parsed)
        };
        // Mirror autocommit_execute: undo+retire on failure; WAL+retire on
        // success.
        match r {
            Ok(n) => {
                let records = match wal::records_for_commit(&guard, xid, &writes, session.sid) {
                    Ok(r) => r,
                    Err(msg) => {
                        undo_all(&mut guard, xid, &writes);
                        retire_txn(&mut guard, xid);
                        auto_vacuum(&mut guard, &writes);
                        return Err(exec::ExecError {
                            detail: None,
                            code: "40001",
                            message: format!(
                                "could not serialize access due to concurrent update: {}",
                                msg
                            ),
                        });
                    }
                };
                if let Err(e) = lock_wal(wal).append_batch(&records) {
                    undo_all(&mut guard, xid, &writes);
                    retire_txn(&mut guard, xid);
                    auto_vacuum(&mut guard, &writes);
                    return Err(exec::ExecError {
                        detail: None,
                        code: "58000",
                        message: format!("WAL write failed: {}", e),
                    });
                }
                retire_txn(&mut guard, xid);
                auto_vacuum(&mut guard, &writes);
                Ok(n)
            }
            Err(e) => {
                undo_all(&mut guard, xid, &writes);
                retire_txn(&mut guard, xid);
                auto_vacuum(&mut guard, &writes);
                Err(e)
            }
        }
    }
}

/// v0.10: `COPY table TO STDOUT` — CopyOutResponse, CopyData rows,
/// CopyDone, then `COPY n`.
fn handle_copy_to(
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    _wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    table: &str,
    columns: &Option<Vec<String>>,
    options: &CopyOptions,
) -> io::Result<()> {
    let (cols, rows) = match copy_to_fetch(engine, session, table, columns) {
        Ok(r) => r,
        Err(e) => {
            send_exec_error(stream, &e)?;
            stream.flush()?;
            return Ok(());
        }
    };
    send_copy_out(stream, &cols, &rows, options)?;
    MsgBuilder::new(b'C')
        .cstr(&format!("COPY {}", rows.len()))
        .send(stream)?;
    stream.flush()?;
    Ok(())
}

/// v0.10: `COPY table FROM STDIN` — CopyInResponse, then CopyData until
/// CopyDone (or CopyFail), then parse+insert and `COPY n`.
fn handle_copy_from(
    reader: &mut TcpStream,
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    table: &str,
    columns: &Option<Vec<String>>,
    options: &CopyOptions,
) -> io::Result<()> {
    if let Some(t) = &session.txn {
        if t.failed {
            send_error(
                stream,
                "25P02",
                "current transaction is aborted, commands ignored until end of transaction block",
            )?;
            stream.flush()?;
            return Ok(());
        }
    }
    // Resolve the column count first (validates table/columns).
    let ncols = match copy_from_ncols(engine, session.sid, table, columns) {
        Ok(n) => n,
        Err(e) => {
            send_exec_error(stream, &e)?;
            stream.flush()?;
            return Ok(());
        }
    };
    // CopyInResponse.
    let fmt: i8 = match options.format {
        CopyFormat::Text => 0,
        CopyFormat::Csv => 1,
    };
    let mut b = MsgBuilder::new(b'G');
    b.u8(fmt as u8).i16(ncols as i16);
    for _ in 0..ncols {
        b.i16(0);
    }
    b.send(stream)?;
    stream.flush()?;
    // Collect CopyData until CopyDone / CopyFail.
    let data = match copy_from_read(reader)? {
        CopyFromRead::Done(d) => d,
        CopyFromRead::SqlError(code, msg) => {
            send_error(stream, &code, &msg)?;
            stream.flush()?;
            return Ok(());
        }
    };
    // Parse + insert.
    match copy_from_ingest(engine, wal, session, table, columns, options, ncols, &data) {
        Ok(n) => {
            MsgBuilder::new(b'C')
                .cstr(&format!("COPY {}", n))
                .send(stream)?;
        }
        Err(e) => {
            send_exec_error(stream, &e)?;
        }
    }
    stream.flush()?;
    Ok(())
}

/// v0.17: extended-protocol COPY — driven from `handle_execute` when the
/// portal's statement is `Stmt::Copy`. Uses the shared fetch/format/ingest
/// helpers; errors go through `protocol_error` (ErrorResponse + mark the
/// transaction failed + wait for Sync), matching extended-protocol
/// recovery semantics.
///
/// Wire sequence for COPY TO: CopyOutResponse ('H'), CopyData ('d') * N,
/// CopyDone ('c'), CommandComplete ('C').
/// Wire sequence for COPY FROM: CopyInResponse ('G'), then the server
/// reads CopyData ('d') / CopyDone ('c') / CopyFail ('f'), then
/// CommandComplete ('C') or ErrorResponse.
fn handle_copy_extended(
    reader: &mut TcpStream,
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    portal_name: &str,
    table: &str,
    columns: &Option<Vec<String>>,
    to_stdout: bool,
    options: &CopyOptions,
) -> io::Result<()> {
    // v0.17: READ ONLY enforcement applies to extended COPY too (COPY
    // FROM is a write; COPY TO is a read).
    if !to_stdout && effective_read_only(session) {
        return protocol_error(
            stream,
            session,
            "25006",
            "cannot execute COPY in a read-only transaction",
        );
    }
    if to_stdout {
        let (cols, rows) = match copy_to_fetch(engine, session, table, columns) {
            Ok(r) => r,
            Err(e) => return protocol_error(stream, session, e.code, &e.message),
        };
        send_copy_out(stream, &cols, &rows, options)?;
        let tag = format!("COPY {}", rows.len());
        MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
        let portal = session.portals.get_mut(portal_name).unwrap();
        portal.done = true;
        portal.last_tag = Some(tag);
        stream.flush()?;
        Ok(())
    } else {
        // Resolve the column count first (validates table/columns).
        let ncols = match copy_from_ncols(engine, session.sid, table, columns) {
            Ok(n) => n,
            Err(e) => return protocol_error(stream, session, e.code, &e.message),
        };
        // CopyInResponse.
        let fmt: i8 = match options.format {
            CopyFormat::Text => 0,
            CopyFormat::Csv => 1,
        };
        let mut b = MsgBuilder::new(b'G');
        b.u8(fmt as u8).i16(ncols as i16);
        for _ in 0..ncols {
            b.i16(0);
        }
        b.send(stream)?;
        stream.flush()?;
        // Collect CopyData until CopyDone / CopyFail.
        let data = match copy_from_read(reader)? {
            CopyFromRead::Done(d) => d,
            CopyFromRead::SqlError(code, msg) => {
                return protocol_error(stream, session, &code, &msg);
            }
        };
        // Parse + insert.
        match copy_from_ingest(engine, wal, session, table, columns, options, ncols, &data) {
            Ok(n) => {
                let tag = format!("COPY {}", n);
                MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
                let portal = session.portals.get_mut(portal_name).unwrap();
                portal.done = true;
                portal.last_tag = Some(tag);
                stream.flush()?;
                Ok(())
            }
            Err(e) => protocol_error(stream, session, e.code, &e.message),
        }
    }
}

// ---------------------------------------------------------------------------
// v0.5: MVCC transactions
// ---------------------------------------------------------------------------

fn cmd(tag: &str) -> ExecResult {
    ExecResult::Command {
        tag: tag.to_string(),
    }
}

fn err_25001(msg: impl Into<String>) -> ExecError {
    ExecError {
        detail: None,
        code: "25001",
        message: msg.into(),
    }
}

/// Statements that may still run while the transaction is aborted: they end
/// it or rewind it to a savepoint. Everything else is 25P02.
fn allowed_in_aborted(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Rollback { .. } | Stmt::RollbackTo { .. } | Stmt::Commit { .. }
    )
}

// ---------------------------------------------------------------------------
// v0.16: SQL cursors (DECLARE / FETCH / CLOSE / MOVE)
// ---------------------------------------------------------------------------

/// Resolve a FETCH/MOVE direction to `(start, end, new_pos)` against a
/// materialized cursor with `len` rows. `pos` is the current row index
/// (`-1` = before the first row, `len` = after the last row): Postgres
/// leaves the cursor *on* the last row retrieved, so RELATIVE moves from
/// the current row, BACKWARD excludes it, and NEXT skips past it.
/// Indices are clamped into `[0, len]`; an empty window parks the cursor
/// after the last row (forward) or before the first row (backward).
fn cursor_window(dir: &FetchDir, pos: i64, len: usize) -> (usize, usize, i64) {
    let n = len as i64;
    enum Kind {
        Fwd,
        Bwd,
        At,
    }
    let (s, e, kind): (i64, i64, Kind) = match dir {
        FetchDir::Forward(None) => (pos + 1, n, Kind::Fwd),
        FetchDir::Forward(Some(c)) if *c >= 0 => (pos + 1, pos + 1 + c, Kind::Fwd),
        // A negative count reverses direction, like Postgres.
        FetchDir::Forward(Some(c)) => (pos + c, pos, Kind::Bwd),
        FetchDir::Backward(None) => (0, pos, Kind::Bwd),
        FetchDir::Backward(Some(c)) if *c >= 0 => (pos - c, pos, Kind::Bwd),
        FetchDir::Backward(Some(c)) => (pos + 1, pos + 1 - c, Kind::Fwd),
        FetchDir::Absolute(c) => {
            // ABSOLUTE 0 parks before the first row and returns nothing.
            if *c == 0 {
                return (0, 0, -1);
            }
            let t = if *c > 0 { c - 1 } else { n + c };
            (t, t + 1, Kind::At)
        }
        FetchDir::Relative(c) => (pos + c, pos + c + 1, Kind::At),
        FetchDir::First => (0, 1, Kind::At),
        FetchDir::Last => (n - 1, n, Kind::At),
    };
    let neg = s < 0;
    let s = s.clamp(0, n);
    let e = e.clamp(0, n);
    if s >= e {
        let new = match kind {
            Kind::Fwd => n,
            Kind::Bwd => -1,
            Kind::At => {
                if neg {
                    -1
                } else {
                    n
                }
            }
        };
        return (0, 0, new);
    }
    let new = match kind {
        // v0.83: PG leaves the cursor AFTER the last row when a forward
        // fetch/move runs to the end (e == n), not ON the last row —
        // otherwise a following BACKWARD ALL would miss the last row.
        Kind::Fwd => {
            if e == n {
                n
            } else {
                e - 1
            }
        }
        // v0.83: PG leaves the cursor BEFORE the first row when a backward
        // fetch/move runs to the start (s == 0), not ON the first row.
        Kind::Bwd => {
            if s == 0 {
                -1
            } else {
                s
            }
        }
        Kind::At => e - 1,
    };
    (s as usize, e as usize, new)
}

#[cfg(test)]
pub(crate) fn cursor_window_for_test(
    dir: &crate::sql::FetchDir,
    pos: i64,
    len: usize,
) -> (usize, usize, i64) {
    cursor_window(dir, pos, len)
}

fn cursor_declare(
    session: &mut Session,
    name: &str,
    query: &sql::SelectStmt,
    with_hold: bool,
) -> Result<ExecResult, ExecError> {
    if session.txn.is_none() && !with_hold {
        return Err(err_25001(
            "DECLARE CURSOR can only be used in transaction blocks",
        ));
    }
    if session.cursors.contains_key(name) {
        return Err(ExecError {
            detail: None,
            code: "42P11",
            message: format!("cursor \"{}\" already exists", name),
        });
    }
    // v0.89: PG19 does not execute the query at DECLARE — the portal is
    // created lazily and query errors (e.g. division by zero) surface
    // at the first FETCH, not here.
    session.cursors.insert(
        name.to_string(),
        SqlCursor {
            query: Some(query.clone()),
            cols: Vec::new(),
            rows: Vec::new(),
            pos: -1,
            with_hold,
            dead: false,
        },
    );
    Ok(cmd("DECLARE CURSOR"))
}

fn cursor_fetch(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    name: &str,
    dir: &FetchDir,
    is_move: bool,
) -> Result<ExecResult, ExecError> {
    // v0.89: existence and liveness check; take the lazy query out so
    // the materialization below can borrow `session` mutably.
    let query = {
        let cur = session.cursors.get_mut(name).ok_or_else(|| ExecError {
            detail: None,
            code: "34000",
            message: format!("cursor \"{}\" does not exist", name),
        })?;
        if cur.dead {
            // PG19: a portal that raised an error is "dead to the
            // world" — it stays present but cannot be run (55000).
            return Err(ExecError {
                detail: None,
                code: "55000",
                message: format!("portal \"{}\" cannot be run", name),
            });
        }
        cur.query.take()
    };
    // v0.89: first FETCH materializes the DECLARE query. An execution
    // error (e.g. division by zero) marks the portal dead and aborts
    // the transaction (via txn_execute), like PG19.
    if let Some(query) = query {
        let sel = Stmt::Select(query);
        let result = if session.txn.is_some() {
            txn_execute(engine, session, &sel)
        } else {
            autocommit_execute(
                engine,
                wal,
                session.sid,
                &session.role,
                session.default_txn_read_only == Some(true),
                session.default_toast_compression,
                &sel,
            )
        };
        let cur = session.cursors.get_mut(name).expect("cursor checked above");
        match result {
            Ok(ExecResult::Select { columns, rows }) => {
                cur.cols = columns;
                cur.rows = rows;
            }
            Ok(_) => {
                cur.dead = true;
                return Err(ExecError {
                    detail: None,
                    code: "XX000",
                    message: "internal error: DECLARE query did not return rows".to_string(),
                });
            }
            Err(e) => {
                cur.dead = true;
                return Err(e);
            }
        }
    }
    let cur = session.cursors.get_mut(name).ok_or_else(|| ExecError {
        detail: None,
        code: "34000",
        message: format!("cursor \"{}\" does not exist", name),
    })?;
    let (s, e, new_pos) = cursor_window(dir, cur.pos, cur.rows.len());
    let mut rows: Vec<Row> = cur.rows[s..e].to_vec();
    // v0.83: PG returns BACKWARD fetch rows in reverse (newest-first) order.
    if matches!(dir, FetchDir::Backward(_)) {
        rows.reverse();
    }
    let cols = cur.cols.clone();
    cur.pos = new_pos;
    let n = rows.len();
    if is_move {
        return Ok(cmd(&format!("MOVE {}", n)));
    }
    // Dml carries rows like a SELECT but completes with the FETCH tag.
    Ok(ExecResult::Dml {
        tag: format!("FETCH {}", n),
        columns: cols,
        rows,
    })
}

fn cursor_close(session: &mut Session, name: Option<&str>) -> Result<ExecResult, ExecError> {
    match name {
        Some(n) => {
            if session.cursors.remove(n).is_none() {
                return Err(ExecError {
                    detail: None,
                    code: "34000",
                    message: format!("cursor \"{}\" does not exist", n),
                });
            }
        }
        None => session.cursors.clear(),
    }
    Ok(cmd("CLOSE CURSOR"))
}

/// Execute one statement with MVCC transaction semantics:
/// - transaction control statements manipulate the session's txn state;
/// - `CHECKPOINT` snapshots the committed engine and truncates the WAL;
///   refused inside a transaction block, like Postgres;
/// - `VACUUM` reclaims dead row versions; refused inside a transaction
///   block, like Postgres (automatic cleanup still runs after every
///   commit/abort);
/// - inside an explicit transaction, data statements run under the
///   transaction's snapshot with its xid as owner (a failure aborts the
///   transaction);
/// - in autocommit each statement is its own transaction: it gets an xid,
///   runs, its write set is WAL-logged + fsynced, and the xid retires.
///   Statement atomicity comes from the executor's validate-then-apply
///   discipline: a failed statement stages no writes, so there is nothing
///   to undo — and nothing is logged for it.
/// v0.17: effective read-only-ness for the next statement. Inside an
/// explicit transaction the transaction's own mode governs (set at BEGIN
/// from the statement's modes or the session defaults); in autocommit
/// the `default_transaction_read_only` GUC governs.
fn effective_read_only(session: &Session) -> bool {
    match &session.txn {
        Some(t) => t.read_only == Some(true),
        None => session.default_txn_read_only == Some(true),
    }
}

/// v0.17: classify a statement for read-only enforcement. Returns the
/// command name for the 25006 error when the statement writes or locks
/// rows and the session is read-only; None when the statement is a
/// pure read (or transaction/session control, which must stay usable).
/// v0.17: which DML/DDL statements a read-only transaction rejects.
/// v0.89: PostgreSQL lets a read-only transaction write TEMPORARY
/// tables (only permanent relations are protected), so statements
/// whose every written target is a session-temp table are allowed;
/// `CREATE TEMP TABLE` / `CREATE TEMP ... AS` are allowed too.
fn read_only_violation(
    db: &crate::storage::Database,
    session: &Session,
    stmt: &Stmt,
) -> Option<&'static str> {
    /// Every name in `names` resolves to a session-temp table.
    fn all_temp(db: &crate::storage::Database, session: &Session, names: &[String]) -> bool {
        !names.is_empty() && names.iter().all(|n| db.is_temp_table(session.sid, n))
    }
    match stmt {
        Stmt::Insert { table, .. } if all_temp(db, session, std::slice::from_ref(table)) => None,
        Stmt::Insert { .. } => Some("INSERT"),
        Stmt::Update { table, .. } if all_temp(db, session, std::slice::from_ref(table)) => None,
        Stmt::Update { .. } => Some("UPDATE"),
        Stmt::Delete { table, .. } if all_temp(db, session, std::slice::from_ref(table)) => None,
        Stmt::Delete { .. } => Some("DELETE"),
        // COPY FROM STDIN writes; COPY TO STDOUT is a read.
        Stmt::Copy { to_stdout, .. } => {
            if *to_stdout {
                None
            } else {
                Some("COPY")
            }
        }
        Stmt::Truncate { tables, .. } if all_temp(db, session, tables) => None,
        Stmt::Truncate { .. } => Some("TRUNCATE"),
        Stmt::Select(s) if s.for_update => Some("SELECT FOR UPDATE"),
        // DDL (schema and privilege changes are writes).
        Stmt::CreateTable { temp: true, .. } => None,
        Stmt::CreateTable { .. } => Some("CREATE TABLE"),
        // v0.48: CTAS is a write too (PG19: "cannot execute CREATE
        // TABLE AS in a read-only transaction").
        Stmt::CreateTableAs { temp: true, .. } => None,
        Stmt::CreateTableAs { .. } => Some("CREATE TABLE AS"),
        Stmt::AlterTable { name, .. } if all_temp(db, session, std::slice::from_ref(name)) => None,
        Stmt::AlterTable { .. } => Some("ALTER TABLE"),
        Stmt::DropTable { names, .. } if all_temp(db, session, names) => None,
        Stmt::DropTable { .. } => Some("DROP TABLE"),
        Stmt::CreateIndex { table, .. } if all_temp(db, session, std::slice::from_ref(table)) => {
            None
        }
        Stmt::CreateIndex { .. } => Some("CREATE INDEX"),
        Stmt::DropIndex { .. } => Some("DROP INDEX"),
        Stmt::CreateView { .. } => Some("CREATE VIEW"),
        Stmt::DropView { .. } => Some("DROP VIEW"),
        Stmt::CreateSequence { .. } => Some("CREATE SEQUENCE"),
        Stmt::AlterSequence { .. } => Some("ALTER SEQUENCE"),
        Stmt::DropSequence { .. } => Some("DROP SEQUENCE"),
        // v0.22: bounded CREATE TYPE.
        Stmt::CreateType { .. } => Some("CREATE TYPE"),
        Stmt::DropType { .. } => Some("DROP TYPE"),
        // v0.85: domain DDL.
        Stmt::CreateDomain { .. } => Some("CREATE DOMAIN"),
        Stmt::DropDomain { .. } => Some("DROP DOMAIN"),
        Stmt::CreateRole { .. } => Some("CREATE ROLE"),
        Stmt::AlterRole { .. } => Some("ALTER ROLE"),
        Stmt::DropRole { .. } => Some("DROP ROLE"),
        Stmt::Grant { .. } => Some("GRANT"),
        Stmt::Revoke { .. } => Some("REVOKE"),
        Stmt::GrantRole { .. } => Some("GRANT"),
        Stmt::RevokeRole { .. } => Some("REVOKE"),
        _ => None,
    }
}

/// v0.17: GUC metadata for the tiny supported set. Only
/// `default_transaction_read_only` is honored; anything else is 42704.
/// v0.29: `bytea_output` added.
fn guc_value(session: &Session, name: &str) -> Option<String> {
    match name {
        "default_transaction_read_only" => Some(
            if session.default_txn_read_only == Some(true) {
                "on"
            } else {
                "off"
            }
            .to_string(),
        ),
        "bytea_output" => Some(
            match session.bytea_output {
                crate::storage::ByteaOutput::Hex => "hex",
                crate::storage::ByteaOutput::Escape => "escape",
            }
            .to_string(),
        ),
        // v0.41: `default_toast_compression` is a real session GUC now
        // (PG19 enum GUC): pglz or lz4, default pglz (matching PG19).
        "default_toast_compression" => Some(session.default_toast_compression.name().to_string()),
        "server_version" => Some(SERVER_VERSION.to_string()),
        "server_version_num" => Some(SERVER_VERSION_NUM.to_string()),
        "transaction_isolation" => {
            let level = session
                .txn
                .as_ref()
                .map(|t| t.level)
                .or(session.default_txn_level)
                .unwrap_or(IsolationLevel::ReadCommitted);
            Some(
                match level {
                    IsolationLevel::ReadCommitted => "read committed",
                    IsolationLevel::RepeatableRead => "repeatable read",
                    IsolationLevel::Serializable => "serializable",
                }
                .to_string(),
            )
        }
        _ => None,
    }
}

/// v0.17: `SET TRANSACTION mode [, ...]` — applies the modes to the
/// current transaction (like PG). With no transaction open it is a
/// no-op returning the SET tag (PG would emit a WARNING; we have no
/// NOTICE channel). Unlike PG we accept it after the first statement;
/// the new modes apply going forward.
fn stmt_set_transaction(
    session: &mut Session,
    level: Option<IsolationLevel>,
    read_only: Option<bool>,
    deferrable: Option<bool>,
) -> Result<ExecResult, ExecError> {
    if let Some(t) = session.txn.as_mut() {
        // Inside a transaction: apply to the current transaction (PG).
        if let Some(l) = level {
            t.level = l;
        }
        if let Some(ro) = read_only {
            t.read_only = Some(ro);
        }
        if let Some(d) = deferrable {
            t.deferrable = Some(d);
        }
    } else {
        // Outside: store for the NEXT transaction (one-shot).
        if level.is_some() {
            session.next_txn_level = level;
        }
        if read_only.is_some() {
            session.next_txn_read_only = read_only;
        }
        if deferrable.is_some() {
            session.next_txn_deferrable = deferrable;
        }
    }
    Ok(ExecResult::Command {
        tag: "SET".to_string(),
    })
}

/// v0.17: boolean-ish GUC spellings, mirroring PG's accepted variants.
fn parse_bool_guc(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// v0.17: `SET name = value`. Only `default_transaction_read_only` is
/// honored; unknown parameters are 42704 (undefined_object), like PG.
/// v0.29: `bytea_output` (hex|escape) added.
/// v0.66: `local` (from `SET LOCAL`) makes the change
/// transaction-scoped: it reverts when the transaction ends, whether
/// committed or aborted (PG19 guc.c semantics). `SET LOCAL` outside a
/// transaction block is a 25001 error, like PG.
fn stmt_set_guc(
    session: &mut Session,
    name: &str,
    value: &SetValue,
    local: bool,
) -> Result<ExecResult, ExecError> {
    // PG19 utility.c: "SET LOCAL can only be used within a transaction
    // block".
    if local && session.txn.is_none() {
        return Err(err_25001(
            "SET LOCAL can only be used within a transaction block",
        ));
    }
    match name {
        "default_transaction_read_only" => {
            let ro = match value {
                SetValue::Default => false,
                SetValue::Str(s) => parse_bool_guc(s).ok_or_else(|| ExecError {
                    detail: None,
                    code: "22023",
                    message: format!("invalid value for parameter \"{}\": \"{}\"", name, s),
                })?,
            };
            push_guc_entry(session, "default_transaction_read_only", local);
            session.default_txn_read_only = Some(ro);
            Ok(ExecResult::Command {
                tag: "SET".to_string(),
            })
        }
        "bytea_output" => {
            let v = match value {
                SetValue::Default => crate::storage::ByteaOutput::Hex,
                SetValue::Str(s) => match s.to_ascii_lowercase().as_str() {
                    "hex" => crate::storage::ByteaOutput::Hex,
                    "escape" => crate::storage::ByteaOutput::Escape,
                    _ => {
                        return Err(ExecError {
                            detail: None,
                            code: "22023",
                            message: format!("invalid value for parameter \"{}\": \"{}\"", name, s),
                        });
                    }
                },
            };
            push_guc_entry(session, "bytea_output", local);
            session.bytea_output = v;
            Ok(ExecResult::Command {
                tag: "SET".to_string(),
            })
        }
        // v0.41: `default_toast_compression` is a real session GUC
        // (PG19 enum GUC): pglz or lz4; DEFAULT (and RESET) restore the
        // compiled default (pglz, like PG19). Anything else
        // is 22023, like PG.
        "default_toast_compression" => {
            let v = match value {
                SetValue::Default => crate::storage::ToastCompression::default(),
                SetValue::Str(s) => {
                    crate::storage::ToastCompression::from_name(s).ok_or_else(|| ExecError {
                        detail: None,
                        code: "22023",
                        message: format!("invalid value for parameter \"{}\": \"{}\"", name, s),
                    })?
                }
            };
            push_guc_entry(session, "default_toast_compression", local);
            session.default_toast_compression = v;
            Ok(ExecResult::Command {
                tag: "SET".to_string(),
            })
        }
        // v0.64: parallel-planner GUCs are accepted as no-ops (we have
        // no cost-based planner or parallel scan to tune, but PG
        // accepts them and regression tests SET them). Values are
        // validated like PG's where cheap: costs are non-negative
        // numbers, worker counts are non-negative integers.
        "parallel_setup_cost" | "parallel_tuple_cost" => match value {
            SetValue::Default => Ok(ExecResult::Command {
                tag: "SET".to_string(),
            }),
            SetValue::Str(s) => {
                let ok = s.parse::<f64>().map(|f| f >= 0.0).unwrap_or(false);
                if ok {
                    Ok(ExecResult::Command {
                        tag: "SET".to_string(),
                    })
                } else {
                    Err(ExecError {
                        detail: None,
                        code: "22023",
                        message: format!("invalid value for parameter \"{}\": \"{}\"", name, s),
                    })
                }
            }
        },
        "max_parallel_workers_per_gather"
        | "max_parallel_workers"
        | "max_worker_processes"
        | "min_parallel_table_scan_size"
        | "min_parallel_index_scan_size" => match value {
            SetValue::Default => Ok(ExecResult::Command {
                tag: "SET".to_string(),
            }),
            SetValue::Str(s) => {
                let ok = s.parse::<i64>().map(|i| i >= 0).unwrap_or(false);
                if ok {
                    Ok(ExecResult::Command {
                        tag: "SET".to_string(),
                    })
                } else {
                    Err(ExecError {
                        detail: None,
                        code: "22023",
                        message: format!("invalid value for parameter \"{}\": \"{}\"", name, s),
                    })
                }
            }
        },
        // v0.68: read-only (PGC_INTERNAL, PG19 guc.c) GUCs are known
        // parameters, so SET on them is 55P02
        // ERRCODE_CANT_CHANGE_RUNTIME_PARAM (`parameter "x" cannot be
        // changed`), not 42704.
        "server_version" | "server_version_num" => Err(ExecError {
            detail: None,
            code: "55P02",
            message: format!("parameter \"{}\" cannot be changed", name),
        }),
        _ => Err(ExecError {
            detail: None,
            code: "42704",
            message: format!("unrecognized configuration parameter \"{}\"", name),
        }),
    }
}

/// v0.17: `SHOW name` — one row, one text column named for the
/// parameter, like PG.
fn stmt_show_guc(session: &Session, name: &str) -> Result<ExecResult, ExecError> {
    match guc_value(session, name) {
        Some(v) => Ok(ExecResult::Select {
            columns: vec![(name.to_string(), ColType::Text)],
            rows: vec![Row::new(vec![Value::text(v)])],
        }),
        None => Err(ExecError {
            detail: None,
            code: "42704",
            message: format!("unrecognized configuration parameter \"{}\"", name),
        }),
    }
}

/// v0.17: `RESET name` / `RESET ALL` (tag "RESET", like PG). Session
/// transaction characteristics are not GUCs and survive RESET ALL.
/// v0.29: `bytea_output` resets to hex.
/// v0.41: `default_toast_compression` resets to the compiled default
/// (lz4, like a PG19 LZ4 build).
/// v0.66: RESET inside a transaction pushes a stack entry, like SET —
/// a RESET in an aborted transaction reverts to the pre-transaction
/// value (PG19).
fn stmt_reset_guc(session: &mut Session, name: &str) -> Result<ExecResult, ExecError> {
    match name {
        "all" => {
            // PG's RESET ALL is a session-scoped reset of every GUC to
            // its default; record each stateful GUC so an abort
            // restores the pre-transaction values.
            push_guc_entry(session, "default_transaction_read_only", false);
            push_guc_entry(session, "bytea_output", false);
            push_guc_entry(session, "default_toast_compression", false);
            session.default_txn_read_only = None;
            session.bytea_output = crate::storage::ByteaOutput::Hex;
            session.default_toast_compression = crate::storage::ToastCompression::default();
            Ok(ExecResult::Command {
                tag: "RESET".to_string(),
            })
        }
        "default_transaction_read_only" => {
            push_guc_entry(session, "default_transaction_read_only", false);
            session.default_txn_read_only = None;
            Ok(ExecResult::Command {
                tag: "RESET".to_string(),
            })
        }
        "bytea_output" => {
            push_guc_entry(session, "bytea_output", false);
            session.bytea_output = crate::storage::ByteaOutput::Hex;
            Ok(ExecResult::Command {
                tag: "RESET".to_string(),
            })
        }
        // v0.41: `default_toast_compression` resets to the compiled
        // default (pglz), like PG.
        "default_toast_compression" => {
            push_guc_entry(session, "default_toast_compression", false);
            session.default_toast_compression = crate::storage::ToastCompression::default();
            Ok(ExecResult::Command {
                tag: "RESET".to_string(),
            })
        }
        // v0.68: RESET on a read-only GUC is 55P02 too — PG19's
        // set_config_option context check fires for every action.
        "server_version" | "server_version_num" => Err(ExecError {
            detail: None,
            code: "55P02",
            message: format!("parameter \"{}\" cannot be changed", name),
        }),
        _ => Err(ExecError {
            detail: None,
            code: "42704",
            message: format!("unrecognized configuration parameter \"{}\"", name),
        }),
    }
}

/// v0.74: resolve SQL-level `EXECUTE name (args)` to the stored prepared
/// statement with `$N` bound to the evaluated argument values, ready to
/// run through the normal statement path.
fn resolve_sql_execute(
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    name: &str,
    args: &[crate::sql::Expr],
) -> Result<Stmt, ExecError> {
    let prepared = session
        .sql_prepared
        .get(name)
        .cloned()
        .ok_or_else(|| ExecError {
            detail: None,
            code: "26000",
            message: format!("prepared statement \"{}\" does not exist", name),
        })?;
    if !prepared.types.is_empty() && args.len() != prepared.types.len() {
        return Err(ExecError {
            detail: None,
            code: "42P02",
            message: format!(
                "wrong number of parameters for prepared statement \"{}\": expected {}, got {}",
                name,
                prepared.types.len(),
                args.len()
            ),
        });
    }
    // Evaluate the argument expressions under a throwaway snapshot; the
    // bound statement itself runs later under its own statement snapshot.
    let mut guard = lock_engine(engine);
    let snap = guard.take_snapshot();
    let xid = guard.begin_txn();
    let mut params: Vec<Option<crate::storage::Value>> = Vec::with_capacity(args.len());
    for a in args {
        let v = exec::eval_execute_arg(&mut guard, &snap, xid, session.sid, &session.role, a)?;
        params.push(Some(v));
    }
    guard.end_txn(xid);
    drop(guard);
    let mut stmt = prepared.stmt;
    exec::subst_params(&mut stmt, &params)?;
    Ok(stmt)
}

fn run_statement(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    stmt: &Stmt,
) -> Result<ExecResult, ExecError> {
    // v0.74: SQL-level EXECUTE resolves to the stored prepared
    // statement with its arguments bound, so the checks below see the
    // real statement. (PREPARE/DEALLOCATE are handled in the match.)
    let owned;
    let stmt = match stmt {
        Stmt::Execute { name, args } => {
            owned = resolve_sql_execute(engine, session, name, args)?;
            &owned
        }
        other => other,
    };
    if let Some(t) = &session.txn {
        if t.failed && !allowed_in_aborted(stmt) {
            return Err(ExecError {
                detail: None,
                code: "25P02",
                message:
                    "current transaction is aborted, commands ignored until end of transaction block"
                        .to_string(),
            });
        }
    }
    // v0.17: READ ONLY enforcement (SQLSTATE 25006). Transaction and
    // session control stay usable so the mode can always be exited.
    // Like any statement error, this aborts the transaction.
    if effective_read_only(session) {
        // v0.89: temp-table awareness needs the catalog, so take the
        // engine lock briefly (the same lock run_statement would take
        // for execution anyway).
        let violation = {
            let guard = lock_engine(engine);
            read_only_violation(&guard.db, session, stmt)
        };
        if let Some(cmd) = violation {
            if let Some(t) = session.txn.as_mut() {
                t.failed = true;
            }
            return Err(ExecError {
                detail: None,
                code: "25006",
                message: format!("cannot execute {} in a read-only transaction", cmd),
            });
        }
    }
    // v0.81: capture the dispatch result so successful DDL can bump
    // the catalog epoch (invalidates sessions' parsed-AST caches).
    let result = match stmt {
        Stmt::Begin {
            level,
            read_only,
            deferrable,
        } => {
            // v0.17: statement modes win; then one-shot SET TRANSACTION
            // (cleared after use); then session defaults (SET SESSION
            // CHARACTERISTICS); otherwise PG defaults.
            let lvl = (*level)
                .or(session.next_txn_level.take())
                .or(session.default_txn_level)
                .unwrap_or(IsolationLevel::ReadCommitted);
            let ro = (*read_only)
                .or(session.next_txn_read_only.take())
                .or(session.default_txn_read_only);
            let def = (*deferrable)
                .or(session.next_txn_deferrable.take())
                .or(session.default_txn_deferrable);
            txn_begin(engine, session, lvl, ro, def)
        }
        Stmt::Commit { chain } => txn_commit(engine, wal, session, *chain),
        Stmt::Rollback { chain } => txn_rollback(engine, session, *chain),
        Stmt::Savepoint { name } => txn_savepoint(engine, session, name),
        Stmt::RollbackTo { name } => txn_rollback_to(engine, session, name),
        Stmt::Release { name } => txn_release(session, name),
        Stmt::Checkpoint => txn_checkpoint(engine, wal, session),
        // v0.74: SQL-level PREPARE / DEALLOCATE are session-state writes
        // (the statement text was already parsed); EXECUTE is resolved
        // to the bound inner statement before the match.
        Stmt::Prepare { name, types, stmt } => {
            if session.sql_prepared.contains_key(name) {
                return Err(ExecError {
                    detail: None,
                    code: "42P05",
                    message: format!("prepared statement \"{}\" already exists", name),
                });
            }
            session.sql_prepared.insert(
                name.clone(),
                SqlPrepared {
                    types: types.clone(),
                    stmt: (**stmt).clone(),
                },
            );
            Ok(ExecResult::Command {
                tag: "PREPARE".to_string(),
            })
        }
        Stmt::Deallocate { name } => {
            match name {
                Some(n) => {
                    session.sql_prepared.remove(n);
                }
                None => session.sql_prepared.clear(),
            }
            Ok(ExecResult::Command {
                tag: "DEALLOCATE".to_string(),
            })
        }
        Stmt::Execute { .. } => Err(ExecError {
            detail: None,
            code: "XX000",
            message: "internal error: EXECUTE reached the executor unresolved".to_string(),
        }),
        // v0.16: SQL cursors are session-level (the cursor map lives on
        // the Session, next to the prepared statements).
        Stmt::Declare {
            name,
            query,
            with_hold,
        } => cursor_declare(session, name, query, *with_hold),
        Stmt::Fetch { name, dir } => cursor_fetch(engine, wal, session, name, dir, false),
        Stmt::Close { name } => cursor_close(session, name.as_deref()),
        Stmt::Move { name, dir } => cursor_fetch(engine, wal, session, name, dir, true),
        Stmt::Vacuum {
            table,
            verbose,
            analyze,
        } => {
            let out = txn_vacuum(engine, session, table.as_deref(), *verbose)?;
            // v0.14: `VACUUM ANALYZE` also collects planner statistics.
            if *analyze {
                let a = Stmt::Analyze {
                    table: table.clone(),
                };
                autocommit_execute(
                    engine,
                    wal,
                    session.sid,
                    &session.role,
                    session.default_txn_read_only == Some(true),
                    session.default_toast_compression,
                    &a,
                )?;
            }
            Ok(out)
        }
        // --- v0.17: transaction characteristics / GUCs -------------------
        Stmt::SetTransaction {
            level,
            read_only,
            deferrable,
        } => stmt_set_transaction(session, *level, *read_only, *deferrable),
        Stmt::SetSessionCharacteristics {
            level,
            read_only,
            deferrable,
        } => {
            if let Some(l) = level {
                session.default_txn_level = Some(*l);
            }
            if let Some(ro) = read_only {
                session.default_txn_read_only = Some(*ro);
            }
            if let Some(d) = deferrable {
                session.default_txn_deferrable = Some(*d);
            }
            Ok(ExecResult::Command {
                tag: "SET".to_string(),
            })
        }
        Stmt::Set { name, value, local } => stmt_set_guc(session, name, value, *local),
        Stmt::Show { name } => stmt_show_guc(session, name),
        Stmt::Reset { name } => stmt_reset_guc(session, name),
        _ => {
            if session.txn.is_some() {
                txn_execute(engine, session, stmt)
            } else {
                autocommit_execute(
                    engine,
                    wal,
                    session.sid,
                    &session.role,
                    session.default_txn_read_only == Some(true),
                    session.default_toast_compression,
                    stmt,
                )
            }
        }
    };
    // v0.81: successful DDL bumps the catalog epoch, invalidating cached
    // parsed ASTs (they're syntactic, so this is conservative — but it
    // mirrors PG's plan-cache invalidation and keeps the cache honest if
    // parsing ever becomes catalog-sensitive).
    if result.is_ok() && stmt.is_catalog_changing() {
        engine.lock().unwrap().catalog_epoch += 1;
    }
    result
}

/// A WAL/filesystem failure becomes SQLSTATE 58000 (system_error).
fn wal_err(e: io::Error) -> ExecError {
    ExecError {
        detail: None,
        code: "58000",
        message: format!("WAL write failed: {}", e),
    }
}

/// The snapshot for one statement: the transaction's fixed snapshot for
/// REPEATABLE READ / SERIALIZABLE (taken at the first data statement and
/// registered, so VACUUM cannot reap what it can see), a fresh snapshot
/// for READ COMMITTED, or a fresh snapshot with owner 0 outside a
/// transaction. Establishing the snapshot here (rather than only at
/// Execute) means Bind/Describe see the same view the statement will.
fn stmt_snapshot(engine: &mut Engine, txn: Option<&mut Txn>) -> (Snapshot, u64, IsolationLevel) {
    match txn {
        Some(t) => {
            let xid = t.xid;
            let level = t.level;
            let snap = match level {
                IsolationLevel::ReadCommitted => engine.take_snapshot(),
                _ => match t.snapshot.clone() {
                    Some(s) => s,
                    None => {
                        let s = engine.take_snapshot();
                        engine.register_snapshot(xid, s.clone());
                        t.snapshot = Some(s.clone());
                        s
                    }
                },
            };
            (snap, xid, level)
        }
        None => (engine.take_snapshot(), 0, IsolationLevel::ReadCommitted),
    }
}

/// One data statement inside an explicit transaction: run under the
/// transaction's snapshot with its xid as owner. A failed statement
/// aborts the transaction (Postgres semantics); its staged writes stay
/// in the log until ROLLBACK/COMMIT undoes them.
fn txn_execute(
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    stmt: &Stmt,
) -> Result<ExecResult, ExecError> {
    let mut guard = lock_engine(engine);
    let t = session
        .txn
        .as_mut()
        .expect("txn_execute called without a transaction");
    let (snap, xid, level) = stmt_snapshot(&mut guard, Some(&mut *t));
    let result = {
        let mut ctx = StmtCtx {
            snap: &snap,
            own: xid,
            level,
            session: session.sid,
            role: &session.role,
            read_only: t.read_only == Some(true),
            default_toast_compression: session.default_toast_compression,
            writes: &mut t.writes,
        };
        exec::execute(&mut *guard, &mut ctx, stmt)
    };
    // v0.9: sequence advances are non-transactional: on success, stage
    // their commit-time WAL markers. (On failure the in-memory advance
    // still stands, like Postgres; there is just nothing to log yet.)
    if result.is_ok() {
        let advanced = std::mem::take(&mut guard.seq_advanced);
        let mut seen = std::collections::HashSet::new();
        for name in advanced {
            if seen.insert(name.clone()) {
                t.writes.push(WriteOp::SeqAdvance { name });
            }
        }
    } else {
        guard.seq_advanced.clear();
    }
    match result {
        Ok(r) => Ok(r),
        Err(e) => {
            t.failed = true;
            Err(e)
        }
    }
}

/// Autocommit: the statement is its own transaction. It gets an xid,
/// runs under a fresh snapshot, its write set is WAL-logged and fsynced,
/// and only then the xid retires. On any failure the staged writes are
/// undone and the xid retires without a trace.
fn autocommit_execute(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    sid: u64,
    role: &str,
    // v0.17: effective read-only for this implicit transaction (from
    // `default_transaction_read_only`); gates nextval/setval.
    read_only: bool,
    // v0.41: session's `default_toast_compression` GUC.
    default_toast_compression: crate::storage::ToastCompression,
    stmt: &Stmt,
) -> Result<ExecResult, ExecError> {
    let mut guard = lock_engine(engine);
    let xid = guard.begin_txn();
    let snap = guard.take_snapshot();
    let mut writes: Vec<WriteOp> = Vec::new();
    let result = {
        let mut ctx = StmtCtx {
            snap: &snap,
            own: xid,
            level: IsolationLevel::ReadCommitted,
            session: sid,
            role,
            read_only,
            default_toast_compression,
            writes: &mut writes,
        };
        exec::execute(&mut *guard, &mut ctx, stmt)
    };
    // v0.9: stage sequence-advance WAL markers on success (see
    // txn_execute for the semantics).
    if result.is_ok() {
        let advanced = std::mem::take(&mut guard.seq_advanced);
        let mut seen = std::collections::HashSet::new();
        for name in advanced {
            if seen.insert(name.clone()) {
                writes.push(WriteOp::SeqAdvance { name });
            }
        }
    } else {
        guard.seq_advanced.clear();
    }
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            undo_all(&mut guard, xid, &writes);
            retire_txn(&mut guard, xid);
            return Err(e);
        }
    };
    // Derive the WAL records, then fsync BEFORE the commit can become
    // visible: we still hold the engine lock, so no other session can
    // observe the in-memory mutation until the frame is durable.
    // A commit-time serialization conflict (e.g. a concurrent CREATE
    // TABLE of the same name won the race) aborts the statement: undo
    // the staged writes and retire the xid.
    let records = match wal::records_for_commit(&guard, xid, &writes, sid) {
        Ok(r) => r,
        Err(msg) => {
            undo_all(&mut guard, xid, &writes);
            retire_txn(&mut guard, xid);
            auto_vacuum(&mut guard, &writes);
            return Err(ExecError {
                detail: None,
                code: "40001",
                message: format!(
                    "could not serialize access due to concurrent update: {}",
                    msg
                ),
            });
        }
    };
    // Lock order is always engine -> wal.
    if let Err(e) = lock_wal(wal).append_batch(&records) {
        undo_all(&mut guard, xid, &writes);
        retire_txn(&mut guard, xid);
        auto_vacuum(&mut guard, &writes);
        return Err(wal_err(e));
    }
    retire_txn(&mut guard, xid);
    auto_vacuum(&mut guard, &writes);
    Ok(result)
}

/// Undo every op, newest first (abort / failed autocommit / WAL failure).
fn undo_all(engine: &mut Engine, own: u64, writes: &[WriteOp]) {
    for op in writes.iter().rev() {
        undo_write_op(engine, own, op);
    }
}

/// Retire `xid` and release every row lock it still holds (v0.6:
/// SELECT ... FOR UPDATE). Locks live until transaction end — commit,
/// rollback, failed autocommit, or disconnect cleanup — like Postgres.
fn retire_txn(engine: &mut Engine, xid: u64) {
    engine.end_txn(xid);
    engine.release_txn_locks(xid);
}

/// Best-effort cleanup after a commit or abort: reclaim versions that are
/// dead to every snapshot. Only tables where the transaction may have
/// created dead versions (DELETE/UPDATE/DROP — never pure INSERT/CREATE)
/// are scanned, so insert-only workloads don't pay an O(table) scan per
/// commit. Never fails the statement it follows.
fn auto_vacuum(engine: &mut Engine, writes: &[WriteOp]) {
    let mut names: Vec<&str> = Vec::new();
    for op in writes {
        let name = match op {
            // Only deletes (UPDATE = delete+insert) and drops create dead
            // versions; inserts and creates never do.
            WriteOp::DeleteRow { table, .. } => table.as_str(),
            // v0.13: UPDATEs also leave a dead old version behind.
            WriteOp::UpdateRow { table, .. } => table.as_str(),
            WriteOp::DropTable { name, .. } => name.as_str(),
            // v0.9: ALTER TABLE swaps the table version; its rows may be
            // reaped too. Views/sequences create no dead row versions.
            WriteOp::AlterTable { name, .. } => name.as_str(),
            WriteOp::InsertRow { .. }
            | WriteOp::CreateTable { .. }
            | WriteOp::CreateIndex { .. }
            | WriteOp::DropIndex { .. }
            | WriteOp::CreateView { .. }
            | WriteOp::DropView { .. }
            | WriteOp::CreateSequence { .. }
            | WriteOp::DropSequence { .. }
            | WriteOp::AlterSequence { .. }
            | WriteOp::SeqAdvance { .. }
            | WriteOp::CreateRole { .. }
            | WriteOp::DropRole { .. }
            | WriteOp::AlterRole { .. }
            | WriteOp::DbAcl { .. }
            // v0.22: temp tables are single-version; their DDL leaves
            // no dead versions to reap. Shell types likewise.
            | WriteOp::CreateTempTable { .. }
            | WriteOp::DropTempTable { .. }
            | WriteOp::AlterTempTable { .. }
            | WriteOp::CreateType { .. }
            | WriteOp::DropType { .. }
            // v0.86: function/operator DDL likewise leaves no dead row
            // versions to reap.
            | WriteOp::CreateFunction { .. }
            | WriteOp::DropFunction { .. }
            | WriteOp::CreateOperator { .. }
            | WriteOp::DropOperator { .. }
            // v0.87: temp index DDL leaves no dead row versions to reap.
            | WriteOp::CreateTempIndex { .. }
            | WriteOp::DropTempIndex { .. } => continue,
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    for name in names {
        engine.vacuum_table(name);
    }
}

fn txn_begin(
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    level: IsolationLevel,
    read_only: Option<bool>,
    deferrable: Option<bool>,
) -> Result<ExecResult, ExecError> {
    if session.txn.is_some() {
        // Postgres: WARNING "there is already a transaction in progress",
        // otherwise a no-op. No NOTICE channel in v0.5, so plain no-op.
        return Ok(cmd("BEGIN"));
    }
    let xid = lock_engine(engine).begin_txn();
    session.txn = Some(Txn {
        xid,
        level,
        read_only,
        deferrable,
        snapshot: None,
        writes: Vec::new(),
        savepoints: Vec::new(),
        cursor_marks: Vec::new(),
        failed: false,
        // v0.66: fresh GUC stack per transaction.
        guc_stack: Vec::new(),
        guc_marks: Vec::new(),
    });
    Ok(cmd("BEGIN"))
}

fn txn_commit(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    chain: bool,
) -> Result<ExecResult, ExecError> {
    if session.txn.is_none() {
        // v0.89: plain COMMIT outside a transaction is PG's WARNING
        // no-op, but COMMIT AND CHAIN requires a transaction block
        // (PG19: 25001 "COMMIT AND CHAIN can only be used in transaction
        // blocks").
        if chain {
            return Err(ExecError {
                detail: None,
                code: "25001",
                message: "COMMIT AND CHAIN can only be used in transaction blocks".to_string(),
            });
        }
        return Ok(cmd("COMMIT"));
    }
    let t = session.txn.take().expect("transaction checked above");
    // Remember characteristics for AND CHAIN before the txn is consumed.
    let (level, read_only, deferrable) = (t.level, t.read_only, t.deferrable);
    let mut guard = lock_engine(engine);
    if t.failed {
        // COMMIT of an aborted transaction rolls back (v0.3 behavior).
        undo_all(&mut guard, t.xid, &t.writes);
        retire_txn(&mut guard, t.xid);
        auto_vacuum(&mut guard, &t.writes);
        drop(guard);
        // v0.16: every cursor dies with the aborted transaction
        // (even WITH HOLD ones — there is no committed data to hold).
        session.cursors.clear();
        // v0.66: an aborted transaction reverts every in-transaction
        // GUC change (SET and SET LOCAL), like PG19.
        revert_guc_stack(session, t.guc_stack, false);
        if chain {
            // AND CHAIN: new transaction with the same characteristics.
            return txn_begin(engine, session, level, read_only, deferrable);
        }
        return Ok(cmd("ROLLBACK"));
    }
    // Derive the records from the write log and make them durable BEFORE
    // the commit returns. The in-memory versions are already stamped with
    // our xid; retiring the xid (below) is what publishes them.
    // A commit-time serialization conflict aborts the transaction, like
    // Postgres: hand it back marked failed so the client can ROLLBACK.
    let records = match wal::records_for_commit(&guard, t.xid, &t.writes, session.sid) {
        Ok(r) => r,
        Err(msg) => {
            let mut t = t;
            t.failed = true;
            session.txn = Some(t);
            return Err(ExecError {
                detail: None,
                code: "40001",
                message: format!(
                    "could not serialize access due to concurrent update: {}",
                    msg
                ),
            });
        }
    };
    // Lock order is always engine -> wal.
    if let Err(e) = lock_wal(wal).append_batch(&records) {
        // Not durable: the transaction cannot commit. Hand it back marked
        // failed so the client can ROLLBACK (which undoes the staged
        // writes); a COMMIT retry would be meaningless.
        let mut t = t;
        t.failed = true;
        session.txn = Some(t);
        return Err(wal_err(e));
    }
    retire_txn(&mut guard, t.xid);
    auto_vacuum(&mut guard, &t.writes);
    drop(guard);
    // v0.16: plain cursors die at COMMIT; WITH HOLD cursors survive.
    session.cursors.retain(|_, c| c.with_hold);
    // v0.66: SET LOCAL effects end with the transaction (commit or
    // not); plain SET/RESET persist for the session, like PG19.
    revert_guc_stack(session, t.guc_stack, true);
    if chain {
        // AND CHAIN: immediately start a new transaction with the same
        // characteristics as the just-committed one (SQL standard). The
        // command tag stays COMMIT — PostgreSQL reports the command that
        // ran, not the implicitly started transaction.
        txn_begin(engine, session, level, read_only, deferrable)?;
    }
    Ok(cmd("COMMIT"))
}

/// Snapshot the committed engine and truncate the WAL.
/// Refused inside a transaction block, like Postgres.
fn txn_checkpoint(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &Session,
) -> Result<ExecResult, ExecError> {
    if session.txn.is_some() {
        return Err(err_25001(
            "CHECKPOINT cannot be executed inside a transaction block",
        ));
    }
    let guard = lock_engine(engine);
    lock_wal(wal).checkpoint(&guard).map_err(wal_err)?;
    Ok(cmd("CHECKPOINT"))
}

/// Explicit VACUUM. Refused inside a transaction block, like Postgres.
/// `VACUUM VERBOSE` returns one informational row per table vacuumed.
fn txn_vacuum(
    engine: &Arc<Mutex<Engine>>,
    session: &Session,
    table: Option<&str>,
    verbose: bool,
) -> Result<ExecResult, ExecError> {
    if session.txn.is_some() {
        return Err(err_25001(
            "VACUUM cannot be executed inside a transaction block",
        ));
    }
    let mut guard = lock_engine(engine);
    // v0.11: VACUUM requires ownership (or superuser), like PostgreSQL.
    // The session role is the authenticated role. VACUUM runs outside
    // any transaction, so build a fresh snapshot from the live state.
    let snap = crate::storage::Snapshot {
        active: guard.txns.active.iter().cloned().collect(),
        next_xid: guard.txns.next_xid,
    };
    let is_owner = |db: &crate::storage::Database, snap: &crate::storage::Snapshot, name: &str| {
        db.find_table(name, snap, u64::MAX, session.sid)
            .map(|t| {
                t.owner == session.role
                    || crate::storage::is_superuser_snap(db, &session.role, snap, u64::MAX)
            })
            .unwrap_or(false)
    };
    if let Some(name) = table {
        // v0.85: resolve temp tables too — `find_table` checks the
        // session's temp tables first, like ANALYZE does.
        if guard
            .db
            .find_table(name, &snap, u64::MAX, session.sid)
            .is_none()
        {
            return Err(ExecError {
                detail: None,
                code: "42P01",
                message: format!("table \"{}\" does not exist", name),
            });
        }
        if !is_owner(&guard.db, &snap, name) {
            return Err(ExecError {
                detail: None,
                code: "42501",
                message: format!("permission denied: must be owner of table \"{}\"", name),
            });
        }
    }
    // v0.85: vacuum a temp table when the name resolves to one; otherwise
    // the permanent table.
    let vacuum_one = |guard: &mut std::sync::MutexGuard<crate::storage::Engine>,
                      sid: u64,
                      name: &str|
     -> usize {
        if let Some(n) = guard.vacuum_temp_table(sid, name) {
            return n;
        }
        guard.vacuum_table(name)
    };
    if !verbose {
        match table {
            Some(name) => {
                vacuum_one(&mut guard, session.sid, name);
            }
            None => {
                // Plain VACUUM only touches tables this role owns
                // (superusers vacuum everything).
                let owned: Vec<String> = guard
                    .db
                    .tables
                    .keys()
                    .filter(|n| is_owner(&guard.db, &snap, n))
                    .cloned()
                    .collect();
                for n in owned {
                    guard.vacuum_table(&n);
                }
            }
        }
        return Ok(cmd("VACUUM"));
    }
    let mut rows: Vec<Row> = Vec::new();
    match table {
        Some(name) => {
            let n = vacuum_one(&mut guard, session.sid, name);
            rows.push(Row::new(vec![Value::text(format!(
                "table \"{}\": removed {} dead row version(s)",
                name, n
            ))]));
        }
        None => {
            for (name, n) in guard.vacuum_all() {
                rows.push(Row::new(vec![Value::text(format!(
                    "table \"{}\": removed {} dead row version(s)",
                    name, n
                ))]));
            }
            if rows.is_empty() {
                rows.push(Row::new(vec![Value::text("vacuum: nothing to remove")]));
            }
        }
    }
    Ok(ExecResult::Select {
        columns: vec![("vacuum".to_string(), ColType::Text)],
        rows,
    })
}

fn txn_rollback(
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    chain: bool,
) -> Result<ExecResult, ExecError> {
    // v0.89: ROLLBACK AND CHAIN outside a transaction block is 25001
    // in PG19 ("ROLLBACK AND CHAIN can only be used in transaction
    // blocks"); plain ROLLBACK stays the WARNING no-op.
    if session.txn.is_none() && chain {
        return Err(err_25001(
            "ROLLBACK AND CHAIN can only be used in transaction blocks",
        ));
    }
    // Remember characteristics for AND CHAIN before the txn is consumed.
    let chained = if let Some(t) = session.txn.take() {
        let chained = (t.level, t.read_only, t.deferrable);
        let mut guard = lock_engine(engine);
        undo_all(&mut guard, t.xid, &t.writes);
        retire_txn(&mut guard, t.xid);
        auto_vacuum(&mut guard, &t.writes);
        // v0.16: ROLLBACK closes every cursor, including WITH HOLD ones.
        session.cursors.clear();
        // v0.66: rollback reverts every in-transaction GUC change (SET
        // and SET LOCAL), like PG19.
        revert_guc_stack(session, t.guc_stack, false);
        Some(chained)
    } else {
        None
    };
    if chain {
        // AND CHAIN: new transaction with the same characteristics as the
        // just-rolled-back one (v0.89: chain without a transaction is
        // rejected above, so `chained` is always `Some` here). The
        // command tag stays ROLLBACK, like PostgreSQL.
        let (level, read_only, deferrable) =
            chained.expect("CHAIN without a transaction is rejected above");
        txn_begin(engine, session, level, read_only, deferrable)?;
    }
    Ok(cmd("ROLLBACK"))
}

fn txn_savepoint(
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    name: &str,
) -> Result<ExecResult, ExecError> {
    match session.txn.as_mut() {
        None => Err(err_25001(
            "SAVEPOINT can only be used in transaction blocks",
        )),
        Some(t) => {
            // A savepoint is a position in the write log plus the current
            // row-lock count — no copies.
            let xid = t.xid;
            let locks = lock_engine(engine).txn_lock_count(xid);
            t.savepoints.push((name.to_string(), t.writes.len(), locks));
            // v0.66: a ROLLBACK TO SAVEPOINT also cancels SET/SET LOCAL
            // effects made after the savepoint (PG19).
            t.guc_marks.push(t.guc_stack.len());
            // v0.16: also snapshot the cursor set (v0.89: positions are
            // no longer rewound — PG19 keeps them across ROLLBACK TO).
            t.cursor_marks.push(CursorMark {
                names: session.cursors.keys().cloned().collect(),
            });
            Ok(cmd("SAVEPOINT"))
        }
    }
}

fn txn_rollback_to(
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    name: &str,
) -> Result<ExecResult, ExecError> {
    let mut guard = lock_engine(engine);
    let t = session
        .txn
        .as_mut()
        .ok_or_else(|| err_25001("ROLLBACK TO SAVEPOINT can only be used in transaction blocks"))?;
    let idx = t
        .savepoints
        .iter()
        .rposition(|(n, _, _)| n == name)
        .ok_or_else(|| ExecError {
            detail: None,
            code: "3B001",
            message: format!("no such savepoint \"{}\"", name),
        })?;
    let to = t.savepoints[idx].1;
    let keep_locks = t.savepoints[idx].2;
    let xid = t.xid;
    // Undo everything staged after the savepoint, newest first. Each undo
    // is conditional on the version still being ours (see undo_write_op).
    // v0.9: sequence advances are non-transactional — they survive
    // ROLLBACK TO SAVEPOINT (like Postgres) and keep their WAL markers.
    let mut kept_seq: Vec<WriteOp> = Vec::new();
    for op in t.writes.drain(to..).rev() {
        if matches!(op, WriteOp::SeqAdvance { .. }) {
            kept_seq.push(op);
        } else {
            undo_write_op(&mut guard, xid, &op);
        }
    }
    t.writes.extend(kept_seq.into_iter().rev());
    // Row locks taken after the savepoint are released, like Postgres;
    // locks from before it stay held.
    guard.release_locks_after(xid, keep_locks);
    // Savepoints established after the named one are destroyed; the named
    // one stays valid. Rolling back also recovers from an aborted txn.
    t.savepoints.truncate(idx + 1);
    t.failed = false;
    // v0.66: SET/SET LOCAL effects after the savepoint are canceled,
    // newest first (PG19). The marks established after the named
    // savepoint die with it; the named mark stays valid.
    let guc_to = t.guc_marks[idx];
    let undone: Vec<GucStackEntry> = t.guc_stack.drain(guc_to..).rev().collect();
    t.guc_marks.truncate(idx + 1);
    // The GUC borrow ends here (`t` is dead); restore the canceled
    // effects newest-first, like a mini-abort of the savepoint tail.
    for e in undone {
        restore_saved_guc(session, e.name, e.saved);
    }
    // v0.16: close cursors created after the savepoint (their DECLARE
    // is undone, like Postgres). v0.89: fetch positions of surviving
    // cursors are NOT rewound — PG19 keeps the portal position across
    // ROLLBACK TO SAVEPOINT (verified against the authentic
    // transactions.out: `FETCH 10 FROM c` returns rows 10-19 after the
    // rollback, not 0-9 again).
    let names = session
        .txn
        .as_ref()
        .and_then(|t| t.cursor_marks.get(idx))
        .map(|m| m.names.clone());
    if let Some(names) = names {
        session.cursors.retain(|k, _| names.contains(k));
    }
    session
        .txn
        .as_mut()
        .expect("transaction checked above")
        .cursor_marks
        .truncate(idx + 1);
    Ok(cmd("ROLLBACK"))
}

fn txn_release(session: &mut Session, name: &str) -> Result<ExecResult, ExecError> {
    let t = session
        .txn
        .as_mut()
        .ok_or_else(|| err_25001("RELEASE SAVEPOINT can only be used in transaction blocks"))?;
    let idx = t
        .savepoints
        .iter()
        .rposition(|(n, _, _)| n == name)
        .ok_or_else(|| ExecError {
            detail: None,
            code: "3B001",
            message: format!("no such savepoint \"{}\"", name),
        })?;
    // Destroys the named savepoint and all established after it.
    t.savepoints.truncate(idx);
    // v0.16: cursor marks die with their savepoints.
    t.cursor_marks.truncate(idx);
    // v0.66: the GUC marks die with their savepoints too, but the
    // stacked GUC changes are NOT canceled by RELEASE (PG >= 8.3):
    // they still revert at transaction end.
    t.guc_marks.truncate(idx);
    Ok(cmd("RELEASE"))
}

const ABORTED_MSG: &str =
    "current transaction is aborted, commands ignored until end of transaction block";

// ---------------------------------------------------------------------------
// Extended query protocol (v0.2)
// ---------------------------------------------------------------------------

/// Parse: statement name, query string, param OIDs. Stores the parsed AST.
fn handle_parse(
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    payload: &[u8],
) -> io::Result<()> {
    // v0.83: structural payload errors become 08P01 with the connection
    // surviving (see handle_bind).
    let parsed = parse_msg(stream, session, payload, |cur| {
        let name = Cursor::or_08p01(cur.read_cstring())?;
        let query = Cursor::or_08p01(cur.read_cstring())?;
        let ntypes = Cursor::or_08p01(cur.read_i16())?;
        if ntypes < 0 {
            return Err((
                "08P01".to_string(),
                "invalid parameter-type count".to_string(),
            ));
        }
        let mut declared_oids = Vec::with_capacity(ntypes as usize);
        for _ in 0..ntypes {
            declared_oids.push(Cursor::or_08p01(cur.read_i32())?);
        }
        Ok((name, query, declared_oids))
    })?;
    let Some((name, query, declared_oids)) = parsed else {
        return Ok(()); // 08P01 already sent
    };

    // v0.81: parsed-AST cache via `Session::cached_parse`. A hit
    // clones the cached Stmt — parameter substitution still happens
    // fresh per Bind.
    let stmt = if query.trim().is_empty() {
        None
    } else {
        match session.cached_parse(&mut engine.lock().unwrap(), &query) {
            Ok(s) => Some(s),
            Err(e) => {
                return protocol_error(
                    stream,
                    session,
                    "42601",
                    &format!("syntax error: {}", e.message),
                );
            }
        }
    };
    // Named or unnamed (''): insert replaces any previous statement,
    // so the unnamed statement is implicitly replaced by the next Parse('').
    // In a failed transaction only ROLLBACK / ROLLBACK TO / COMMIT may run.
    if session.txn.as_ref().is_some_and(|t| t.failed)
        && !stmt.as_ref().is_some_and(allowed_in_aborted)
    {
        return protocol_error(stream, session, "25P02", ABORTED_MSG);
    }
    session.stmts.insert(
        name,
        Prepared {
            stmt,
            declared_oids,
        },
    );
    MsgBuilder::new(b'1').send(stream)?; // ParseComplete
    stream.flush()?;
    Ok(())
}

/// Bind: portal name, statement name, param formats + values, result formats.
/// Type-checks/coerces text params and substitutes them into a copy of the
/// statement stored on the portal.
fn handle_bind(
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    payload: &[u8],
) -> io::Result<()> {
    // v0.83: structural payload errors become 08P01 with the connection
    // surviving (PG's pq_getmsgint raises ERRCODE_PROTOCOL_VIOLATION the
    // same way); previously a short payload killed the connection with an
    // io "message truncated" error.
    let parsed = parse_msg(stream, session, payload, |cur| {
        let portal_name = Cursor::or_08p01(cur.read_cstring())?;
        let stmt_name = Cursor::or_08p01(cur.read_cstring())?;

        let nformats = Cursor::or_08p01(cur.read_i16())?;
        if nformats < 0 {
            return Err(("08P01".to_string(), "invalid format-code count".to_string()));
        }
        let mut pformats = Vec::with_capacity(nformats as usize);
        for _ in 0..nformats {
            pformats.push(Cursor::or_08p01(cur.read_i16())?);
        }

        let nparams = Cursor::or_08p01(cur.read_i16())?;
        if nparams < 0 {
            return Err(("08P01".to_string(), "invalid parameter count".to_string()));
        }
        let mut raw: Vec<Option<Vec<u8>>> = Vec::with_capacity(nparams as usize);
        for _ in 0..nparams {
            let len = Cursor::or_08p01(cur.read_i32())?;
            if len == -1 {
                raw.push(None); // NULL
            } else if len < -1 {
                return Err(("08P01".to_string(), "invalid parameter length".to_string()));
            } else {
                raw.push(Some(Cursor::or_08p01(cur.read_bytes(len as usize))?));
            }
        }

        let nresults = Cursor::or_08p01(cur.read_i16())?;
        if nresults < 0 {
            return Err((
                "08P01".to_string(),
                "invalid result-format count".to_string(),
            ));
        }
        let mut rformats = Vec::with_capacity(nresults as usize);
        for _ in 0..nresults {
            rformats.push(Cursor::or_08p01(cur.read_i16())?);
        }
        Ok((portal_name, stmt_name, pformats, raw, rformats))
    })?;
    let Some((portal_name, stmt_name, pformats, raw, rformats)) = parsed else {
        return Ok(()); // 08P01 already sent
    };

    // PG19 (postgres.c exec_bind_message): more than one format code must
    // match the parameter count; a single code applies to all parameters.
    if pformats.len() > 1 && pformats.len() != raw.len() {
        return protocol_error(
            stream,
            session,
            "08P01",
            &format!(
                "bind message has {} parameter formats but {} parameters",
                pformats.len(),
                raw.len()
            ),
        );
    }

    let prep = match session.stmts.get(&stmt_name).cloned() {
        Some(p) => p,
        None => {
            return protocol_error(
                stream,
                session,
                "26000",
                &format!("prepared statement \"{}\" does not exist", stmt_name),
            );
        }
    };

    // In a failed transaction only ROLLBACK / ROLLBACK TO / COMMIT may run.
    if session.txn.as_ref().is_some_and(|t| t.failed)
        && !prep.stmt.as_ref().is_some_and(allowed_in_aborted)
    {
        return protocol_error(stream, session, "25P02", ABORTED_MSG);
    }

    // v0.2 supports text format (0) only, for params and results alike.
    if pformats.iter().any(|&f| f != 0) || rformats.iter().any(|&f| f != 0) {
        return protocol_error(stream, session, "0A000", "binary format not yet supported");
    }

    let stmt = match prep.stmt {
        None => {
            if !raw.is_empty() {
                return protocol_error(
                    stream,
                    session,
                    "08P01",
                    "bind message supplies parameters for an empty query",
                );
            }
            None
        }
        Some(mut s) => {
            let need = s.max_param();
            if raw.len() != need {
                return protocol_error(
                    stream,
                    session,
                    "08P01",
                    &format!(
                        "bind message supplies {} parameters, but prepared statement \"{}\" requires {}",
                        raw.len(),
                        stmt_name,
                        need
                    ),
                );
            }
            let params = {
                // Type resolution sees the session's own uncommitted data.
                let mut guard = lock_engine(engine);
                let (snap, own, _) = stmt_snapshot(&mut guard, session.txn.as_mut());
                exec::bind_params(
                    &s,
                    &prep.declared_oids,
                    &raw,
                    &guard,
                    &snap,
                    own,
                    session.sid,
                )
            };
            let params = match params {
                Ok(p) => p,
                Err(e) => return protocol_error(stream, session, e.code, &e.message),
            };
            if let Err(e) = exec::subst_params(&mut s, &params) {
                return protocol_error(stream, session, e.code, &e.message);
            }
            Some(s)
        }
    };

    session.portals.insert(
        portal_name,
        Portal {
            stmt_name,
            stmt,
            pending: None,
            done: false,
            last_tag: None,
            explain: false,
            dml_tag: None,
        },
    );
    MsgBuilder::new(b'2').send(stream)?; // BindComplete
    stream.flush()?;
    Ok(())
}

/// Describe: 'S' name → ParameterDescription + RowDescription/NoData;
/// 'P' name → RowDescription/NoData.
fn handle_describe(
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    payload: &[u8],
) -> io::Result<()> {
    // v0.83: structural payload errors become 08P01 with the connection
    // surviving (see handle_bind).
    let parsed = parse_msg(stream, session, payload, |cur| {
        let kind = Cursor::or_08p01(cur.read_u8())?;
        let name = Cursor::or_08p01(cur.read_cstring())?;
        Ok((kind, name))
    })?;
    let Some((kind, name)) = parsed else {
        return Ok(()); // 08P01 already sent
    };

    let prep: Prepared = match kind {
        b'S' => match session.stmts.get(&name).cloned() {
            Some(p) => p,
            None => {
                return protocol_error(
                    stream,
                    session,
                    "26000",
                    &format!("prepared statement \"{}\" does not exist", name),
                );
            }
        },
        b'P' => {
            let stmt_name = match session.portals.get(&name) {
                Some(portal) => portal.stmt_name.clone(),
                None => {
                    return protocol_error(
                        stream,
                        session,
                        "34000",
                        &format!("portal \"{}\" does not exist", name),
                    );
                }
            };
            match session.stmts.get(&stmt_name).cloned() {
                Some(p) => p,
                None => {
                    return protocol_error(
                        stream,
                        session,
                        "26000",
                        &format!("prepared statement \"{}\" does not exist", stmt_name),
                    );
                }
            }
        }
        _ => {
            return protocol_error(
                stream,
                session,
                "08P01",
                "invalid describe target (expected 'S' or 'P')",
            );
        }
    };

    // In a failed transaction only ROLLBACK / ROLLBACK TO / COMMIT may run.
    if session.txn.as_ref().is_some_and(|t| t.failed)
        && !prep.stmt.as_ref().is_some_and(allowed_in_aborted)
    {
        return protocol_error(stream, session, "25P02", ABORTED_MSG);
    }

    if kind == b'S' {
        let oids: Vec<i32> = match &prep.stmt {
            None => Vec::new(),
            Some(stmt) => {
                let types = {
                    let mut guard = lock_engine(engine);
                    let (snap, own, _) = stmt_snapshot(&mut guard, session.txn.as_mut());
                    exec::resolve_param_types(
                        stmt,
                        &prep.declared_oids,
                        &guard,
                        &snap,
                        own,
                        session.sid,
                    )
                };
                match types {
                    Ok(types) => types.iter().map(|t| t.oid()).collect(),
                    Err(e) => return protocol_error(stream, session, e.code, &e.message),
                }
            }
        };
        let mut b = MsgBuilder::new(b't'); // ParameterDescription
        b.i16(oids.len() as i16);
        for oid in &oids {
            b.i32(*oid);
        }
        b.send(stream)?;
    }

    match describe_prepared(&prep, engine, session) {
        Err(e) => protocol_error(stream, session, e.code, &e.message),
        Ok(None) => {
            MsgBuilder::new(b'n').send(stream)?; // NoData
            stream.flush()?;
            Ok(())
        }
        Ok(Some(cols)) => {
            send_row_description(stream, &cols)?;
            stream.flush()?;
            Ok(())
        }
    }
}

fn describe_prepared(
    prep: &Prepared,
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
) -> Result<Option<Vec<(String, ColType)>>, exec::ExecError> {
    match &prep.stmt {
        None => Ok(None),
        // v0.16: Describe of a FETCH returns the open cursor's row
        // description (like Postgres); MOVE returns no rows.
        Some(Stmt::Fetch { name, .. }) => match session.cursors.get(name) {
            Some(cur) => Ok(Some(cur.cols.clone())),
            None => Err(exec::ExecError {
                detail: None,
                code: "34000",
                message: format!("cursor \"{}\" does not exist", name),
            }),
        },
        Some(Stmt::Move { .. }) => Ok(None),
        // v0.17: Describe of COPY returns NoData — the column formats
        // ride in the CopyInResponse/CopyOutResponse instead.
        Some(Stmt::Copy { .. }) => Ok(None),
        Some(stmt) => {
            let mut guard = lock_engine(engine);
            let (snap, own, _) = stmt_snapshot(&mut guard, session.txn.as_mut());
            exec::describe_columns(stmt, &prep.declared_oids, &guard, &snap, own, session.sid)
        }
    }
}

/// Execute: portal name, max rows (0 = all). Honors max-rows via
/// PortalSuspended; the final CommandComplete carries the total row count.
fn handle_execute(
    reader: &mut TcpStream,
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    payload: &[u8],
) -> io::Result<()> {
    // v0.83: structural payload errors become 08P01 with the connection
    // surviving (see handle_bind).
    let parsed = parse_msg(stream, session, payload, |cur| {
        let portal_name = Cursor::or_08p01(cur.read_cstring())?;
        let max_rows = Cursor::or_08p01(cur.read_i32())?;
        Ok((portal_name, max_rows))
    })?;
    let Some((portal_name, max_rows)) = parsed else {
        return Ok(()); // 08P01 already sent
    };

    if !session.portals.contains_key(&portal_name) {
        return protocol_error(
            stream,
            session,
            "34000",
            &format!("portal \"{}\" does not exist", portal_name),
        );
    }
    // In a failed transaction only ROLLBACK / ROLLBACK TO / COMMIT may run.
    let blocked = {
        let portal = &session.portals[&portal_name];
        session.txn.as_ref().is_some_and(|t| t.failed)
            && !portal.stmt.as_ref().is_some_and(allowed_in_aborted)
    };
    if blocked {
        return protocol_error(stream, session, "25P02", ABORTED_MSG);
    }
    // Empty-query portal: EmptyQueryResponse every time.
    if session.portals[&portal_name].stmt.is_none() {
        MsgBuilder::new(b'I').send(stream)?;
        stream.flush()?;
        return Ok(());
    }
    // Re-executing a completed portal: replay the final tag, no rows.
    if session.portals[&portal_name].done && session.portals[&portal_name].pending.is_none() {
        let tag = session.portals[&portal_name]
            .last_tag
            .clone()
            .unwrap_or_else(|| "SELECT 0".to_string());
        MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
        stream.flush()?;
        return Ok(());
    }

    // v0.17: extended-protocol COPY — bypass run_statement (which would
    // route Stmt::Copy to the executor's XX000 arm) and drive the COPY
    // sub-protocol directly, with extended-protocol error recovery.
    let copy_stmt = match &session.portals[&portal_name].stmt {
        Some(Stmt::Copy {
            table,
            columns,
            to_stdout,
            options,
        }) => Some((table.clone(), columns.clone(), *to_stdout, options.clone())),
        _ => None,
    };
    if let Some((table, columns, to_stdout, options)) = copy_stmt {
        return handle_copy_extended(
            reader,
            stream,
            engine,
            wal,
            session,
            &portal_name,
            &table,
            &columns,
            to_stdout,
            &options,
        );
    }

    // First Execute: run the statement, caching SELECT rows for suspension.
    if session.portals[&portal_name].pending.is_none() {
        let stmt = session.portals[&portal_name].stmt.clone().unwrap();
        match run_statement(engine, wal, session, &stmt) {
            Err(e) => return protocol_error(stream, session, e.code, &e.message),
            Ok(ExecResult::Select { rows, .. }) => {
                session.portals.get_mut(&portal_name).unwrap().pending =
                    Some(PendingRows { rows, pos: 0 });
            }
            // EXPLAIN rows are cached like SELECT rows; only the final
            // tag differs (set when the portal completes below).
            Ok(ExecResult::Explain { rows, .. }) => {
                session.portals.get_mut(&portal_name).unwrap().pending =
                    Some(PendingRows { rows, pos: 0 });
                session.portals.get_mut(&portal_name).unwrap().explain = true;
            }
            Ok(ExecResult::Command { tag }) => {
                MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
                let portal = session.portals.get_mut(&portal_name).unwrap();
                portal.done = true;
                portal.last_tag = Some(tag);
                stream.flush()?;
                return Ok(());
            }
            // v0.10: DML rows (RETURNING) are cached like SELECT rows;
            // the completion tag is the DML tag.
            Ok(ExecResult::Dml { tag, rows, .. }) => {
                let portal = session.portals.get_mut(&portal_name).unwrap();
                portal.pending = Some(PendingRows { rows, pos: 0 });
                portal.dml_tag = Some(tag);
            }
        }
    }

    // Emit up to max_rows (<= 0 means all rows).
    let portal = session.portals.get_mut(&portal_name).unwrap();
    let pending = portal.pending.as_mut().expect("pending rows");
    let remaining = pending.rows.len() - pending.pos;
    let n = if max_rows <= 0 {
        remaining
    } else {
        std::cmp::min(max_rows as usize, remaining)
    };
    let mut row_buf = Vec::new();
    for row in pending.rows.iter().skip(pending.pos).take(n) {
        send_data_row_buf(stream, row, &mut row_buf, session.bytea_output)?;
    }
    pending.pos += n;
    if pending.pos < pending.rows.len() {
        MsgBuilder::new(b's').send(stream)?; // PortalSuspended
    } else {
        let total = pending.rows.len();
        // v0.8: EXPLAIN completes with the "EXPLAIN" tag, like Postgres.
        // v0.10: DML (INSERT/UPDATE/DELETE) completes with its own tag.
        let tag = if portal.explain {
            "EXPLAIN".to_string()
        } else if let Some(t) = portal.dml_tag.clone() {
            t
        } else {
            format!("SELECT {}", total)
        };
        MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
        portal.pending = None;
        portal.done = true;
        portal.last_tag = Some(tag);
    }
    stream.flush()?;
    Ok(())
}

/// Close: 'S'/'P' + name → CloseComplete. Closing a nonexistent
/// statement/portal is silently ignored, like Postgres.
fn handle_close(stream: &mut Writer, session: &mut Session, payload: &[u8]) -> io::Result<()> {
    // v0.83: structural payload errors become 08P01 with the connection
    // surviving (see handle_bind).
    let parsed = parse_msg(stream, session, payload, |cur| {
        let kind = Cursor::or_08p01(cur.read_u8())?;
        let name = Cursor::or_08p01(cur.read_cstring())?;
        if kind != b'S' && kind != b'P' {
            return Err((
                "08P01".to_string(),
                "invalid close target (expected 'S' or 'P')".to_string(),
            ));
        }
        Ok((kind, name))
    })?;
    let Some((kind, name)) = parsed else {
        return Ok(()); // 08P01 already sent
    };
    match kind {
        b'S' => {
            session.stmts.remove(&name);
        }
        _ => {
            // b'P': kind was validated to be S or P above.
            session.portals.remove(&name);
        }
    }
    MsgBuilder::new(b'3').send(stream)?; // CloseComplete
    stream.flush()?;
    Ok(())
}

pub(crate) fn send_row_description(
    stream: &mut Writer,
    columns: &[(String, ColType)],
) -> io::Result<()> {
    let mut b = MsgBuilder::new(b'T');
    b.i16(columns.len() as i16);
    for (name, typ) in columns {
        b.cstr(name)
            .i32(0) // table OID (unknown in v0.2)
            .i16(0) // column attribute number
            .i32(typ.oid())
            .i16(-1) // type size (variable)
            .i32(-1) // type modifier
            .i16(0); // text format
    }
    b.send(stream)
}

pub(crate) fn send_data_row(
    stream: &mut Writer,
    row: &[Value],
    bytea_output: crate::storage::ByteaOutput,
) -> io::Result<()> {
    // Ordinary rows (a handful of short columns) fit well under this, so
    // the payload buffer is sized once here instead of growing by
    // doubling from Vec::new()'s 0 capacity as each field (count, then
    // per column a length prefix plus text) is appended.
    let mut b = MsgBuilder::with_capacity(b'D', 64);
    b.i16(row.len() as i16);
    for v in row {
        b.value_text(v, bytea_output);
    }
    b.send(stream)
}

/// Same wire encoding as `send_data_row`, but builds into `buf` (expected
/// empty on entry) instead of allocating a fresh payload buffer, and hands
/// `buf`'s allocation back (cleared) afterward. Callers sending many rows
/// for one result set declare `buf` once outside the row loop, so the
/// buffer's capacity is reused across rows instead of paying one
/// allocate/free cycle per row.
pub(crate) fn send_data_row_buf(
    stream: &mut Writer,
    row: &[Value],
    buf: &mut Vec<u8>,
    bytea_output: crate::storage::ByteaOutput,
) -> io::Result<()> {
    let mut b = MsgBuilder::from_payload(b'D', std::mem::take(buf));
    b.i16(row.len() as i16);
    for v in row {
        b.value_text(v, bytea_output);
    }
    let res = b.send(stream);
    *buf = b.into_payload();
    buf.clear();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Engine;

    fn session_with_txn(level: IsolationLevel) -> (Engine, Session) {
        let mut engine = Engine::new();
        let xid = engine.begin_txn();
        let session = Session {
            sid: 4242,
            role: "postgres".to_string(),
            stmts: HashMap::new(),
            portals: HashMap::new(),
            sql_prepared: HashMap::new(),
            in_error: false,
            cursors: HashMap::new(),
            default_txn_level: None,
            default_txn_read_only: None,
            default_txn_deferrable: None,
            next_txn_level: None,
            next_txn_read_only: None,
            next_txn_deferrable: None,
            bytea_output: crate::storage::ByteaOutput::default(),
            default_toast_compression: crate::storage::ToastCompression::default(),
            // v0.81: parse cache.
            parse_cache: HashMap::new(),
            parse_cache_order: std::collections::VecDeque::new(),
            parse_cache_hits: 0,
            parse_cache_misses: 0,
            txn: Some(Txn {
                xid,
                level,
                read_only: None,
                deferrable: None,
                snapshot: None,
                writes: Vec::new(),
                savepoints: Vec::new(),
                cursor_marks: Vec::new(),
                failed: false,
                guc_stack: Vec::new(),
                guc_marks: Vec::new(),
            }),
        };
        (engine, session)
    }

    #[test]
    fn rr_snapshot_pinned_at_first_use() {
        let (mut engine, mut session) = session_with_txn(IsolationLevel::RepeatableRead);
        let (s1, own1, _) = stmt_snapshot(&mut engine, session.txn.as_mut());
        // A concurrent transaction commits between the two calls...
        let other = engine.begin_txn();
        engine.end_txn(other);
        let (s2, own2, _) = stmt_snapshot(&mut engine, session.txn.as_mut());
        assert_eq!(s1.active, s2.active);
        assert_eq!(s1.next_xid, s2.next_xid);
        assert_eq!(own1, own2);
        // ...and the snapshot is registered so VACUUM respects it.
        assert!(engine.txns.snapshots.contains_key(&own1));
    }

    #[test]
    fn rc_takes_fresh_snapshot_each_time() {
        let (mut engine, mut session) = session_with_txn(IsolationLevel::ReadCommitted);
        let (s1, _, _) = stmt_snapshot(&mut engine, session.txn.as_mut());
        let other = engine.begin_txn();
        engine.end_txn(other);
        let (s2, _, _) = stmt_snapshot(&mut engine, session.txn.as_mut());
        assert!(s2.next_xid > s1.next_xid);
        // RC snapshots are ephemeral: never registered.
        assert!(engine.txns.snapshots.is_empty());
        // And the transaction itself stores none.
        assert!(session.txn.as_ref().unwrap().snapshot.is_none());
    }

    #[test]
    fn savepoint_records_write_log_position() {
        let (engine, mut session) = session_with_txn(IsolationLevel::ReadCommitted);
        let engine = Arc::new(Mutex::new(engine));
        let t = session.txn.as_mut().unwrap();
        t.writes.push(WriteOp::InsertRow {
            table: "t".into(),
            row_id: 1,
        });
        t.writes.push(WriteOp::InsertRow {
            table: "t".into(),
            row_id: 2,
        });
        txn_savepoint(&engine, &mut session, "sp1").unwrap();
        assert_eq!(
            session.txn.as_ref().unwrap().savepoints,
            vec![("sp1".to_string(), 2, 0)]
        );
    }

    #[test]
    fn undo_all_reverses_in_order() {
        let mut engine = Engine::new();
        engine.db.tables.insert(
            "t".into(),
            vec![crate::storage::Table::new(
                vec![("a".into(), ColType::Int)],
                1,
            )],
        );
        engine.txns.next_xid = 9;
        let xid = engine.begin_txn(); // xid 9
        // Simulate two staged inserts, then abort.
        let writes = vec![
            WriteOp::InsertRow {
                table: "t".into(),
                row_id: 1,
            },
            WriteOp::InsertRow {
                table: "t".into(),
                row_id: 2,
            },
        ];
        for (i, op) in writes.iter().enumerate() {
            if let WriteOp::InsertRow { row_id, .. } = op {
                engine.db.tables.get_mut("t").unwrap()[0].push_version(
                    crate::storage::RowVersion::plain(
                        *row_id,
                        Row::new(vec![Value::Int(i as i64)]),
                        xid,
                    ),
                );
            }
        }
        assert_eq!(engine.db.tables["t"][0].rows.len(), 2);
        undo_all(&mut engine, xid, &writes);
        assert!(engine.db.tables["t"][0].rows.is_empty());
    }

    #[test]
    fn aborted_txn_gate() {
        // Only ROLLBACK / ROLLBACK TO / COMMIT may run once failed.
        let rb = Stmt::Rollback { chain: false };
        let rbt = Stmt::RollbackTo { name: "sp".into() };
        let commit = Stmt::Commit { chain: false };
        let sel = Stmt::Checkpoint;
        assert!(allowed_in_aborted(&rb));
        assert!(allowed_in_aborted(&rbt));
        assert!(allowed_in_aborted(&commit));
        assert!(!allowed_in_aborted(&sel));
    }

    /// v0.12: a worker thread panicking while holding the global engine
    /// lock must not wedge the server — the helpers recover the lock.
    #[test]
    fn lock_helpers_recover_from_poison() {
        // Poison a real Arc<Mutex<Engine>> the way a panicking worker
        // thread holding the lock would.
        let engine = Arc::new(Mutex::new(Engine::new()));
        let victim = Arc::clone(&engine);
        let h = std::thread::spawn(move || {
            let _g = victim.lock().unwrap();
            panic!("simulated worker-thread panic");
        });
        assert!(h.join().is_err());
        assert!(engine.is_poisoned());
        // lock_engine recovers instead of panicking; the lock stays
        // usable afterwards (still flagged poisoned, but functional).
        {
            let _g = lock_engine(&engine);
        }
        {
            let _g = lock_engine(&engine);
        }
        assert!(engine.is_poisoned());

        // Same for the WAL lock, using a real Wal on a scratch dir.
        let dir = std::env::temp_dir().join("rg12-poison-wal-test");
        let _ = std::fs::remove_dir_all(&dir);
        let (_eng, wal_inner) = crate::wal::Wal::open(&dir).expect("wal open");
        let wal = Arc::new(Mutex::new(wal_inner));
        let victim = Arc::clone(&wal);
        let h = std::thread::spawn(move || {
            let _g = victim.lock().unwrap();
            panic!("simulated worker-thread panic");
        });
        assert!(h.join().is_err());
        assert!(wal.is_poisoned());
        {
            let _g = lock_wal(&wal);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // v0.66: SET / SET LOCAL / RESET transaction semantics (PG19 guc.c).
    // ------------------------------------------------------------------

    /// Session with no open transaction (autocommit).
    fn session_no_txn() -> Session {
        Session {
            sid: 4243,
            role: "postgres".to_string(),
            stmts: HashMap::new(),
            portals: HashMap::new(),
            sql_prepared: HashMap::new(),
            in_error: false,
            txn: None,
            cursors: HashMap::new(),
            default_txn_level: None,
            default_txn_read_only: None,
            default_txn_deferrable: None,
            next_txn_level: None,
            next_txn_read_only: None,
            next_txn_deferrable: None,
            bytea_output: crate::storage::ByteaOutput::default(),
            default_toast_compression: crate::storage::ToastCompression::default(),
            // v0.81: parse cache.
            parse_cache: HashMap::new(),
            parse_cache_order: std::collections::VecDeque::new(),
            parse_cache_hits: 0,
            parse_cache_misses: 0,
        }
    }

    fn scratch_wal(name: &str) -> Arc<Mutex<Wal>> {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let (_eng, wal_inner) = crate::wal::Wal::open(&dir).expect("wal open");
        Arc::new(Mutex::new(wal_inner))
    }

    fn show_text(session: &Session, name: &str) -> String {
        match stmt_show_guc(session, name).unwrap() {
            ExecResult::Select { rows, .. } => rows[0][0].to_text().unwrap(),
            other => panic!("SHOW returned {:?}", other),
        }
    }

    fn set(session: &mut Session, name: &str, val: &str, local: bool) {
        stmt_set_guc(session, name, &SetValue::Str(val.to_string()), local).unwrap();
    }

    #[test]
    fn v66_set_local_requires_transaction() {
        let mut session = session_no_txn();
        let err = stmt_set_guc(
            &mut session,
            "bytea_output",
            &SetValue::Str("escape".to_string()),
            true,
        )
        .unwrap_err();
        assert_eq!(err.code, "25001");
        assert_eq!(
            err.message,
            "SET LOCAL can only be used within a transaction block"
        );
        // The value is untouched.
        assert_eq!(show_text(&session, "bytea_output"), "hex");
    }

    #[test]
    fn v66_set_local_reverts_on_commit() {
        let engine = Arc::new(Mutex::new(Engine::new()));
        let wal = scratch_wal("rg66-guc-commit");
        let mut session = session_no_txn();
        // A session SET persists across transactions (PG).
        set(&mut session, "bytea_output", "escape", false);
        assert_eq!(show_text(&session, "bytea_output"), "escape");

        txn_begin(
            &engine,
            &mut session,
            IsolationLevel::ReadCommitted,
            None,
            None,
        )
        .unwrap();
        set(&mut session, "bytea_output", "hex", true);
        // SHOW sees the local value inside the transaction.
        assert_eq!(show_text(&session, "bytea_output"), "hex");
        txn_commit(&engine, &wal, &mut session, false).unwrap();
        // The LOCAL value is gone; the session SET survives.
        assert_eq!(show_text(&session, "bytea_output"), "escape");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("rg66-guc-commit"));
    }

    #[test]
    fn v66_set_local_reverts_on_rollback() {
        let engine = Arc::new(Mutex::new(Engine::new()));
        let mut session = session_no_txn();
        assert_eq!(show_text(&session, "bytea_output"), "hex");

        txn_begin(
            &engine,
            &mut session,
            IsolationLevel::ReadCommitted,
            None,
            None,
        )
        .unwrap();
        set(&mut session, "bytea_output", "escape", true);
        assert_eq!(show_text(&session, "bytea_output"), "escape");
        txn_rollback(&engine, &mut session, false).unwrap();
        assert_eq!(show_text(&session, "bytea_output"), "hex");
    }

    #[test]
    fn v66_set_then_set_local_commit_keeps_set() {
        // PG docs: "A special case is SET followed by SET LOCAL within a
        // single transaction: ... afterwards (if the transaction is
        // committed) the SET value will take effect."
        let engine = Arc::new(Mutex::new(Engine::new()));
        let wal = scratch_wal("rg66-guc-setlocal");
        let mut session = session_no_txn();

        txn_begin(
            &engine,
            &mut session,
            IsolationLevel::ReadCommitted,
            None,
            None,
        )
        .unwrap();
        set(&mut session, "bytea_output", "escape", false);
        set(&mut session, "bytea_output", "hex", true);
        assert_eq!(show_text(&session, "bytea_output"), "hex");
        txn_commit(&engine, &wal, &mut session, false).unwrap();
        assert_eq!(show_text(&session, "bytea_output"), "escape");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("rg66-guc-setlocal"));
    }

    #[test]
    fn v66_set_in_aborted_txn_reverts() {
        // PG docs: "If SET ... is issued within a transaction that is
        // later aborted, the effects of the SET command disappear."
        let engine = Arc::new(Mutex::new(Engine::new()));
        let mut session = session_no_txn();

        txn_begin(
            &engine,
            &mut session,
            IsolationLevel::ReadCommitted,
            None,
            None,
        )
        .unwrap();
        set(&mut session, "bytea_output", "escape", false);
        assert_eq!(show_text(&session, "bytea_output"), "escape");
        txn_rollback(&engine, &mut session, false).unwrap();
        assert_eq!(show_text(&session, "bytea_output"), "hex");
    }

    #[test]
    fn v66_rollback_to_savepoint_cancels_set_local() {
        // PG docs: "The effects of SET or SET LOCAL are also canceled
        // by rolling back to a savepoint that is earlier than the
        // command."
        let engine = Arc::new(Mutex::new(Engine::new()));
        let wal = scratch_wal("rg66-guc-savepoint");
        let mut session = session_no_txn();

        txn_begin(
            &engine,
            &mut session,
            IsolationLevel::ReadCommitted,
            None,
            None,
        )
        .unwrap();
        set(&mut session, "bytea_output", "escape", true);
        txn_savepoint(&engine, &mut session, "sp1").unwrap();
        set(&mut session, "bytea_output", "hex", true);
        assert_eq!(show_text(&session, "bytea_output"), "hex");
        txn_rollback_to(&engine, &mut session, "sp1").unwrap();
        // The post-savepoint SET LOCAL is canceled; the earlier one
        // is still in effect.
        assert_eq!(show_text(&session, "bytea_output"), "escape");
        txn_commit(&engine, &wal, &mut session, false).unwrap();
        // At commit the surviving SET LOCAL reverts too.
        assert_eq!(show_text(&session, "bytea_output"), "hex");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("rg66-guc-savepoint"));
    }

    #[test]
    fn v66_reset_all_in_txn_reverts_on_abort() {
        let engine = Arc::new(Mutex::new(Engine::new()));
        let mut session = session_no_txn();
        set(&mut session, "bytea_output", "escape", false);
        set(&mut session, "default_toast_compression", "lz4", false);

        txn_begin(
            &engine,
            &mut session,
            IsolationLevel::ReadCommitted,
            None,
            None,
        )
        .unwrap();
        stmt_reset_guc(&mut session, "all").unwrap();
        assert_eq!(show_text(&session, "bytea_output"), "hex");
        assert_eq!(show_text(&session, "default_toast_compression"), "pglz");
        txn_rollback(&engine, &mut session, false).unwrap();
        // The pre-transaction session values are restored.
        assert_eq!(show_text(&session, "bytea_output"), "escape");
        assert_eq!(show_text(&session, "default_toast_compression"), "lz4");
    }

    #[test]
    fn v68_set_readonly_guc_is_55p02() {
        // PG19 guc.c: SET on a PGC_INTERNAL GUC is 55P02
        // ERRCODE_CANT_CHANGE_RUNTIME_PARAM, not 42704.
        let mut session = session_no_txn();
        for name in ["server_version", "server_version_num"] {
            let err = stmt_set_guc(
                &mut session,
                name,
                &SetValue::Str("bogus".to_string()),
                false,
            )
            .unwrap_err();
            assert_eq!(err.code, "55P02");
            assert_eq!(
                err.message,
                format!("parameter \"{}\" cannot be changed", name)
            );
            // SHOW still works; the value is untouched.
            assert!(!show_text(&session, name).is_empty());
        }
    }

    #[test]
    fn v68_reset_readonly_guc_is_55p02() {
        // PG19's set_config_option context check fires for RESET too.
        let mut session = session_no_txn();
        let err = stmt_reset_guc(&mut session, "server_version").unwrap_err();
        assert_eq!(err.code, "55P02");
        assert_eq!(
            err.message,
            "parameter \"server_version\" cannot be changed"
        );
        // Unknown names are still 42704.
        let err = stmt_reset_guc(&mut session, "nosuchguc").unwrap_err();
        assert_eq!(err.code, "42704");
    }

    #[test]
    fn v68_set_unknown_guc_still_42704() {
        let mut session = session_no_txn();
        let err = stmt_set_guc(
            &mut session,
            "nosuchguc",
            &SetValue::Str("1".to_string()),
            false,
        )
        .unwrap_err();
        assert_eq!(err.code, "42704");
        assert_eq!(
            err.message,
            "unrecognized configuration parameter \"nosuchguc\""
        );
    }

    // v0.81: parsed-AST cache — hits, misses, DDL invalidation, capacity.
    #[test]
    fn v81_parse_cache_hit_and_miss() {
        let mut engine = Engine::new();
        let mut session = session_no_txn();
        // First Parse: miss.
        let stmt1 = session
            .cached_parse(&mut engine, "SELECT 1")
            .expect("parses");
        assert_eq!(session.parse_cache_misses, 1);
        assert_eq!(session.parse_cache_hits, 0);
        // Second Parse of identical text: hit, same AST shape.
        let stmt2 = session
            .cached_parse(&mut engine, "SELECT 1")
            .expect("parses");
        assert_eq!(session.parse_cache_misses, 1);
        assert_eq!(session.parse_cache_hits, 1);
        assert_eq!(format!("{:?}", stmt1), format!("{:?}", stmt2));
        // Different text: miss.
        let _ = session
            .cached_parse(&mut engine, "SELECT 2")
            .expect("parses");
        assert_eq!(session.parse_cache_misses, 2);
        assert_eq!(session.parse_cache_hits, 1);
    }

    #[test]
    fn v81_parse_cache_ddl_invalidation() {
        let mut engine = Engine::new();
        let mut session = session_no_txn();
        let _ = session
            .cached_parse(&mut engine, "SELECT * FROM t")
            .expect("parses");
        assert_eq!(session.parse_cache_misses, 1);
        // DDL bumps the catalog epoch; the cached entry is stale.
        engine.catalog_epoch += 1;
        let _ = session
            .cached_parse(&mut engine, "SELECT * FROM t")
            .expect("parses");
        assert_eq!(session.parse_cache_misses, 2);
        assert_eq!(session.parse_cache_hits, 0);
    }

    #[test]
    fn v81_parse_cache_fifo_capacity() {
        let mut engine = Engine::new();
        let mut session = session_no_txn();
        // Fill beyond capacity; the cache must stay bounded.
        for i in 0..(PARSE_CACHE_CAP + 10) {
            let sql = format!("SELECT {}", i);
            session.cached_parse(&mut engine, &sql).expect("parses");
        }
        assert!(session.parse_cache.len() <= PARSE_CACHE_CAP);
        assert_eq!(session.parse_cache_order.len(), session.parse_cache.len());
        // The earliest entries were evicted.
        assert!(!session.parse_cache.contains_key("SELECT 0"));
        // A recent entry is still cached: hit.
        let hits_before = session.parse_cache_hits;
        session
            .cached_parse(&mut engine, &format!("SELECT {}", PARSE_CACHE_CAP + 9))
            .expect("parses");
        assert_eq!(session.parse_cache_hits, hits_before + 1);
    }

    #[test]
    fn v81_is_catalog_changing_ddl() {
        use crate::sql::{Stmt, parse_statement};
        // DDL statements bump the catalog epoch.
        for sql in [
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN b int",
            "CREATE VIEW v AS SELECT 1",
            "DROP VIEW v",
            "CREATE TYPE t2 AS (x int)",
            "DROP TYPE t2",
        ] {
            let stmt = parse_statement(sql).expect("parses");
            assert!(
                stmt.is_catalog_changing(),
                "{} should be catalog-changing",
                sql
            );
        }
        // Non-DDL does not.
        for sql in ["SELECT 1", "INSERT INTO t VALUES (1)", "BEGIN", "COMMIT"] {
            let stmt = parse_statement(sql).expect("parses");
            assert!(
                !stmt.is_catalog_changing(),
                "{} should not be catalog-changing",
                sql
            );
        }
    }
}
