//! v0.5 transaction core: MVCC statement execution, commit/abort paths,
//! savepoints, VACUUM, and the snapshot helper shared with the extended
//! protocol's parameter resolution.

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::copy;
use crate::exec::{self, ExecError, ExecResult, StmtCtx};
use crate::protocol::{Cursor, MsgBuilder, read_message, read_startup};
use crate::sql::{self, CopyFormat, CopyOptions, IsolationLevel, Stmt};
use crate::storage::{ColType, Engine, Snapshot, Value, WriteOp, undo_write_op};
use crate::wal::{self, Wal};

/// Buffered sink for server→client traffic. Reads still go through the
/// raw `TcpStream`; every message-loop iteration ends with a `flush()`.
type Writer = BufWriter<TcpStream>;

const SSL_REQUEST_CODE: i32 = 80877103;
const PROTOCOL_V3: i32 = 196608;

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
    rows: Vec<Vec<Value>>,
    pos: usize,
}

struct Session {
    /// Stable per-connection id (v0.9: session-local currval()).
    sid: u64,
    /// v0.11: authenticated role name (lowercased). Every statement's
    /// privilege checks key off this.
    role: String,
    stmts: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// After an extended-protocol error: discard input until Sync.
    in_error: bool,
    /// Explicit transaction state; None = autocommit.
    txn: Option<Txn>,
}

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
    /// Pinned at the first data statement for RR/SERIALIZABLE.
    snapshot: Option<Snapshot>,
    /// Uncommitted writes, in order: the undo log (abort / ROLLBACK TO)
    /// and the commit-time WAL source.
    writes: Vec<WriteOp>,
    /// (name, writes.len(), row-lock count) stack for SAVEPOINT: rolling
    /// back to a savepoint undoes staged writes AND releases the row locks
    /// taken after it, like Postgres.
    savepoints: Vec<(String, usize, usize)>,
    /// A failed statement aborts the transaction (Postgres semantics):
    /// only ROLLBACK / ROLLBACK TO / COMMIT are accepted afterwards.
    failed: bool,
}

impl Session {
    fn new(role: String) -> Self {
        Session {
            sid: NEXT_SID.fetch_add(1, Ordering::Relaxed),
            role,
            stmts: HashMap::new(),
            portals: HashMap::new(),
            in_error: false,
            txn: None,
        }
    }
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
        let mut counts = session_counts().lock().unwrap();
        if let Some(n) = counts.get_mut(&self.role) {
            *n = n.saturating_sub(1);
        }
    }
}

fn session_counts() -> &'static Mutex<HashMap<String, usize>> {
    SESSION_COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
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
        ("server_version", "16.0"),
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
    let mut session = Session::new(role_name);
    send_ready(&mut writer, &session)?;
    writer.flush()?;

    let result = message_loop(&mut reader, &mut writer, &engine, &wal, &mut session);
    // Disconnect cleanup: an open transaction must not leak its uncommitted
    // versions or its xid (both would pin snapshots and vacuum forever).
    // Postgres aborts the transaction on disconnect; do the same on every
    // exit path — clean Terminate, client EOF, and read errors alike.
    if session.txn.is_some() {
        let _ = txn_rollback(&engine, &mut session);
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
        let guard = engine.lock().unwrap();
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
        let guard = engine.lock().unwrap();
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
        let counts = session_counts().lock().unwrap();
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
    let mut counts = session_counts().lock().unwrap();
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
            b'P' => handle_parse(writer, session, &msg.payload)?,
            b'B' => handle_bind(writer, engine, session, &msg.payload)?,
            b'D' => handle_describe(writer, engine, session, &msg.payload)?,
            b'E' => handle_execute(writer, engine, wal, session, &msg.payload)?,
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
                send_error(
                    writer,
                    "0A000",
                    &format!("unimplemented message type '{}'", other as char),
                )?;
                send_ready(writer, session)?;
                writer.flush()?;
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

fn send_ready(stream: &mut Writer, session: &Session) -> io::Result<()> {
    MsgBuilder::new(b'Z').u8(txn_status(session)).send(stream)
}

fn send_error(stream: &mut Writer, code: &str, message: &str) -> io::Result<()> {
    let mut b = MsgBuilder::new(b'E');
    b.u8(b'S')
        .cstr("ERROR")
        .u8(b'V')
        .cstr("ERROR")
        .u8(b'C')
        .cstr(code)
        .u8(b'M')
        .cstr(message)
        .u8(0);
    b.send(stream)
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
                break;
            }
            Ok(s) => s,
        };
        // The simple protocol has no parameters: a `$N` here is 42P02.
        let mut stmt = stmt;
        if let Err(e) = exec::subst_params(&mut stmt, &[]) {
            send_error(stream, e.code, &e.message)?;
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
                send_error(stream, e.code, &e.message)?;
                break;
            }
            Ok(ExecResult::Select { columns, rows }) => {
                send_row_description(stream, &columns)?;
                for row in &rows {
                    send_data_row(stream, row)?;
                }
                MsgBuilder::new(b'C')
                    .cstr(&format!("SELECT {}", rows.len()))
                    .send(stream)?;
            }
            Ok(ExecResult::Explain { columns, rows }) => {
                send_row_description(stream, &columns)?;
                for row in &rows {
                    send_data_row(stream, row)?;
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
                    for row in &rows {
                        send_data_row(stream, row)?;
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
    // Run the SELECT under the session's transaction context (like
    // run_statement: aborted txn -> 25P02).
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
    let (cols, rows) = {
        let mut guard = engine.lock().unwrap();
        // Snapshot/transaction handling mirrors txn_execute/autocommit.
        if session.txn.is_some() {
            let t = session.txn.as_mut().unwrap();
            let (snap, xid, level) = stmt_snapshot(&mut guard, Some(&mut *t));
            let mut ctx = StmtCtx {
                snap: &snap,
                own: xid,
                level,
                session: session.sid,
                role: &session.role,
                writes: &mut t.writes,
            };
            match exec::copy_to_rows(&mut *guard, &mut ctx, table, columns) {
                Ok(r) => r,
                Err(e) => {
                    t.failed = true;
                    send_error(stream, e.code, &e.message)?;
                    stream.flush()?;
                    return Ok(());
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
                    writes: &mut writes,
                };
                exec::copy_to_rows(&mut *guard, &mut ctx, table, columns)
            };
            // COPY TO is read-only; retire the xid.
            retire_txn(&mut guard, xid);
            match result {
                Ok(r) => r,
                Err(e) => {
                    send_error(stream, e.code, &e.message)?;
                    stream.flush()?;
                    return Ok(());
                }
            }
        }
    };
    // CopyOutResponse: format (0=text, 1=csv), column count, per-column
    // format codes (all text=0).
    let fmt: i8 = match options.format {
        CopyFormat::Text => 0,
        CopyFormat::Csv => 1,
    };
    let mut b = MsgBuilder::new(b'H');
    b.u8(fmt as u8).i16(cols.len() as i16);
    for _ in &cols {
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
    for row in &rows {
        data.clear();
        let mut fields = Vec::with_capacity(row.len());
        let mut nulls = Vec::with_capacity(row.len());
        for v in row {
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
    let ncols = {
        let mut guard = engine.lock().unwrap();
        let snap = guard.take_snapshot();
        // Use a throwaway xid for the snapshot owner (read-only).
        let xid = guard.begin_txn();
        let r = exec::copy_ncols(&*guard, &snap, xid, table, columns);
        retire_txn(&mut guard, xid);
        match r {
            Ok(n) => n,
            Err(e) => {
                send_error(stream, e.code, &e.message)?;
                stream.flush()?;
                return Ok(());
            }
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
    let mut data = Vec::new();
    loop {
        let msg = read_message(reader)?;
        match msg.typ {
            b'd' => {
                data.extend_from_slice(&msg.payload);
            }
            b'c' => break, // CopyDone
            b'f' => {
                // CopyFail: abort the copy.
                let reason = String::from_utf8_lossy(&msg.payload);
                send_error(stream, "57000", &format!("COPY failed: {}", reason))?;
                stream.flush()?;
                return Ok(());
            }
            b'X' => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "client terminated during COPY",
                ));
            }
            _ => {
                send_error(
                    stream,
                    "08P01",
                    &format!(
                        "unexpected message '{}' during COPY FROM STDIN",
                        msg.typ as char
                    ),
                )?;
                stream.flush()?;
                return Ok(());
            }
        }
    }
    // Parse (line-numbered errors).
    let parsed = match copy::parse_rows(&data, options, ncols) {
        Ok(r) => r,
        Err(e) => {
            send_error(
                stream,
                "22P04",
                &format!("COPY error on line {}: {}", e.line, e.message),
            )?;
            stream.flush()?;
            // The failed COPY aborts the transaction (like Postgres).
            if session.txn.is_some() {
                let _ = txn_rollback(engine, session);
            }
            return Ok(());
        }
    };
    // Insert (transactional: uses the session txn if any, else autocommit).
    let insert_result = if session.txn.is_some() {
        let mut guard = engine.lock().unwrap();
        let t = session.txn.as_mut().unwrap();
        let (snap, xid, level) = stmt_snapshot(&mut guard, Some(&mut *t));
        let r = {
            let mut ctx = StmtCtx {
                snap: &snap,
                own: xid,
                level,
                session: session.sid,
                role: &session.role,
                writes: &mut t.writes,
            };
            exec::copy_from_rows(&mut *guard, &mut ctx, table, columns, parsed)
        };
        if r.is_err() {
            t.failed = true;
        }
        r.map_err(|e| (e.code, e.message))
    } else {
        let mut guard = engine.lock().unwrap();
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
                writes: &mut writes,
            };
            exec::copy_from_rows(&mut *guard, &mut ctx, table, columns, parsed)
        };
        // Mirror autocommit_execute: undo+retire on failure; WAL+retire on
        // success.
        match r {
            Ok(n) => {
                let records = match wal::records_for_commit(&guard, xid, &writes) {
                    Ok(r) => r,
                    Err(msg) => {
                        undo_all(&mut guard, xid, &writes);
                        retire_txn(&mut guard, xid);
                        auto_vacuum(&mut guard, &writes);
                        send_error(
                            stream,
                            "40001",
                            &format!(
                                "could not serialize access due to concurrent update: {}",
                                msg
                            ),
                        )?;
                        stream.flush()?;
                        return Ok(());
                    }
                };
                if let Err(e) = wal.lock().unwrap().append_batch(&records) {
                    undo_all(&mut guard, xid, &writes);
                    retire_txn(&mut guard, xid);
                    auto_vacuum(&mut guard, &writes);
                    send_error(stream, "58000", &format!("WAL write failed: {}", e))?;
                    stream.flush()?;
                    return Ok(());
                }
                retire_txn(&mut guard, xid);
                auto_vacuum(&mut guard, &writes);
                Ok(n)
            }
            Err(e) => {
                undo_all(&mut guard, xid, &writes);
                retire_txn(&mut guard, xid);
                auto_vacuum(&mut guard, &writes);
                Err((e.code, e.message))
            }
        }
    };
    match insert_result {
        Ok(n) => {
            MsgBuilder::new(b'C')
                .cstr(&format!("COPY {}", n))
                .send(stream)?;
        }
        Err((code, msg)) => {
            send_error(stream, code, &msg)?;
        }
    }
    stream.flush()?;
    Ok(())
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
        code: "25001",
        message: msg.into(),
    }
}

/// Statements that may still run while the transaction is aborted: they end
/// it or rewind it to a savepoint. Everything else is 25P02.
fn allowed_in_aborted(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Rollback | Stmt::RollbackTo { .. } | Stmt::Commit
    )
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
fn run_statement(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    stmt: &Stmt,
) -> Result<ExecResult, ExecError> {
    if let Some(t) = &session.txn {
        if t.failed && !allowed_in_aborted(stmt) {
            return Err(ExecError {
                code: "25P02",
                message:
                    "current transaction is aborted, commands ignored until end of transaction block"
                        .to_string(),
            });
        }
    }
    match stmt {
        Stmt::Begin { level } => txn_begin(
            engine,
            session,
            level.unwrap_or(IsolationLevel::ReadCommitted),
        ),
        Stmt::Commit => txn_commit(engine, wal, session),
        Stmt::Rollback => txn_rollback(engine, session),
        Stmt::Savepoint { name } => txn_savepoint(engine, session, name),
        Stmt::RollbackTo { name } => txn_rollback_to(engine, session, name),
        Stmt::Release { name } => txn_release(session, name),
        Stmt::Checkpoint => txn_checkpoint(engine, wal, session),
        Stmt::Vacuum { table, verbose } => txn_vacuum(engine, session, table.as_deref(), *verbose),
        _ => {
            if session.txn.is_some() {
                txn_execute(engine, session, stmt)
            } else {
                autocommit_execute(engine, wal, session.sid, &session.role, stmt)
            }
        }
    }
}

/// A WAL/filesystem failure becomes SQLSTATE 58000 (system_error).
fn wal_err(e: io::Error) -> ExecError {
    ExecError {
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
    let mut guard = engine.lock().unwrap();
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
    stmt: &Stmt,
) -> Result<ExecResult, ExecError> {
    let mut guard = engine.lock().unwrap();
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
    let records = match wal::records_for_commit(&guard, xid, &writes) {
        Ok(r) => r,
        Err(msg) => {
            undo_all(&mut guard, xid, &writes);
            retire_txn(&mut guard, xid);
            auto_vacuum(&mut guard, &writes);
            return Err(ExecError {
                code: "40001",
                message: format!(
                    "could not serialize access due to concurrent update: {}",
                    msg
                ),
            });
        }
    };
    // Lock order is always engine -> wal.
    if let Err(e) = wal.lock().unwrap().append_batch(&records) {
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
            | WriteOp::DbAcl { .. } => continue,
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
) -> Result<ExecResult, ExecError> {
    if session.txn.is_some() {
        // Postgres: WARNING "there is already a transaction in progress",
        // otherwise a no-op. No NOTICE channel in v0.5, so plain no-op.
        return Ok(cmd("BEGIN"));
    }
    let xid = engine.lock().unwrap().begin_txn();
    session.txn = Some(Txn {
        xid,
        level,
        snapshot: None,
        writes: Vec::new(),
        savepoints: Vec::new(),
        failed: false,
    });
    Ok(cmd("BEGIN"))
}

fn txn_commit(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
) -> Result<ExecResult, ExecError> {
    let t = match session.txn.take() {
        None => return Ok(cmd("COMMIT")), // Postgres: WARNING, no-op
        Some(t) => t,
    };
    let mut guard = engine.lock().unwrap();
    if t.failed {
        // COMMIT of an aborted transaction rolls back (v0.3 behavior).
        undo_all(&mut guard, t.xid, &t.writes);
        retire_txn(&mut guard, t.xid);
        auto_vacuum(&mut guard, &t.writes);
        return Ok(cmd("ROLLBACK"));
    }
    // Derive the records from the write log and make them durable BEFORE
    // the commit returns. The in-memory versions are already stamped with
    // our xid; retiring the xid (below) is what publishes them.
    // A commit-time serialization conflict aborts the transaction, like
    // Postgres: hand it back marked failed so the client can ROLLBACK.
    let records = match wal::records_for_commit(&guard, t.xid, &t.writes) {
        Ok(r) => r,
        Err(msg) => {
            let mut t = t;
            t.failed = true;
            session.txn = Some(t);
            return Err(ExecError {
                code: "40001",
                message: format!(
                    "could not serialize access due to concurrent update: {}",
                    msg
                ),
            });
        }
    };
    // Lock order is always engine -> wal.
    if let Err(e) = wal.lock().unwrap().append_batch(&records) {
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
    let guard = engine.lock().unwrap();
    wal.lock().unwrap().checkpoint(&guard).map_err(wal_err)?;
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
    let mut guard = engine.lock().unwrap();
    // v0.11: VACUUM requires ownership (or superuser), like PostgreSQL.
    // The session role is the authenticated role. VACUUM runs outside
    // any transaction, so build a fresh snapshot from the live state.
    let snap = crate::storage::Snapshot {
        active: guard.txns.active.iter().cloned().collect(),
        next_xid: guard.txns.next_xid,
    };
    let is_owner = |db: &crate::storage::Database, snap: &crate::storage::Snapshot, name: &str| {
        db.find_table(name, snap, u64::MAX)
            .map(|t| {
                t.owner == session.role
                    || crate::storage::is_superuser_snap(db, &session.role, snap, u64::MAX)
            })
            .unwrap_or(false)
    };
    if let Some(name) = table {
        if !guard.db.tables.contains_key(name) {
            return Err(ExecError {
                code: "42P01",
                message: format!("table \"{}\" does not exist", name),
            });
        }
        if !is_owner(&guard.db, &snap, name) {
            return Err(ExecError {
                code: "42501",
                message: format!("permission denied: must be owner of table \"{}\"", name),
            });
        }
    }
    if !verbose {
        match table {
            Some(name) => {
                guard.vacuum_table(name);
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
    let mut rows: Vec<Vec<Value>> = Vec::new();
    match table {
        Some(name) => {
            let n = guard.vacuum_table(name);
            rows.push(vec![Value::Text(format!(
                "table \"{}\": removed {} dead row version(s)",
                name, n
            ))]);
        }
        None => {
            for (name, n) in guard.vacuum_all() {
                rows.push(vec![Value::Text(format!(
                    "table \"{}\": removed {} dead row version(s)",
                    name, n
                ))]);
            }
            if rows.is_empty() {
                rows.push(vec![Value::Text("vacuum: nothing to remove".to_string())]);
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
) -> Result<ExecResult, ExecError> {
    if let Some(t) = session.txn.take() {
        let mut guard = engine.lock().unwrap();
        undo_all(&mut guard, t.xid, &t.writes);
        retire_txn(&mut guard, t.xid);
        auto_vacuum(&mut guard, &t.writes);
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
            let locks = engine.lock().unwrap().txn_lock_count(xid);
            t.savepoints.push((name.to_string(), t.writes.len(), locks));
            Ok(cmd("SAVEPOINT"))
        }
    }
}

fn txn_rollback_to(
    engine: &Arc<Mutex<Engine>>,
    session: &mut Session,
    name: &str,
) -> Result<ExecResult, ExecError> {
    let mut guard = engine.lock().unwrap();
    let t = session
        .txn
        .as_mut()
        .ok_or_else(|| err_25001("ROLLBACK TO SAVEPOINT can only be used in transaction blocks"))?;
    let idx = t
        .savepoints
        .iter()
        .rposition(|(n, _, _)| n == name)
        .ok_or_else(|| ExecError {
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
            code: "3B001",
            message: format!("no such savepoint \"{}\"", name),
        })?;
    // Destroys the named savepoint and all established after it.
    t.savepoints.truncate(idx);
    Ok(cmd("RELEASE"))
}

const ABORTED_MSG: &str =
    "current transaction is aborted, commands ignored until end of transaction block";

// ---------------------------------------------------------------------------
// Extended query protocol (v0.2)
// ---------------------------------------------------------------------------

/// Parse: statement name, query string, param OIDs. Stores the parsed AST.
fn handle_parse(stream: &mut Writer, session: &mut Session, payload: &[u8]) -> io::Result<()> {
    let mut cur = Cursor::new(payload);
    let name = cur.read_cstring()?;
    let query = cur.read_cstring()?;
    let ntypes = cur.read_i16()?;
    if ntypes < 0 {
        return protocol_error(stream, session, "08P01", "invalid parameter-type count");
    }
    let mut declared_oids = Vec::with_capacity(ntypes as usize);
    for _ in 0..ntypes {
        declared_oids.push(cur.read_i32()?);
    }

    let stmt = if query.trim().is_empty() {
        None
    } else {
        match sql::parse_statement(&query) {
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
    let mut cur = Cursor::new(payload);
    let portal_name = cur.read_cstring()?;
    let stmt_name = cur.read_cstring()?;

    let nformats = cur.read_i16()?;
    if nformats < 0 {
        return protocol_error(stream, session, "08P01", "invalid format-code count");
    }
    let mut pformats = Vec::with_capacity(nformats as usize);
    for _ in 0..nformats {
        pformats.push(cur.read_i16()?);
    }

    let nparams = cur.read_i16()?;
    if nparams < 0 {
        return protocol_error(stream, session, "08P01", "invalid parameter count");
    }
    let mut raw: Vec<Option<Vec<u8>>> = Vec::with_capacity(nparams as usize);
    for _ in 0..nparams {
        let len = cur.read_i32()?;
        if len == -1 {
            raw.push(None); // NULL
        } else if len < -1 {
            return protocol_error(stream, session, "08P01", "invalid parameter length");
        } else {
            raw.push(Some(cur.read_bytes(len as usize)?));
        }
    }

    let nresults = cur.read_i16()?;
    if nresults < 0 {
        return protocol_error(stream, session, "08P01", "invalid result-format count");
    }
    let mut rformats = Vec::with_capacity(nresults as usize);
    for _ in 0..nresults {
        rformats.push(cur.read_i16()?);
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
                let mut guard = engine.lock().unwrap();
                let (snap, own, _) = stmt_snapshot(&mut guard, session.txn.as_mut());
                exec::bind_params(&s, &prep.declared_oids, &raw, &guard, &snap, own)
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
    let mut cur = Cursor::new(payload);
    let kind = cur.read_u8()?;
    let name = cur.read_cstring()?;

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
                    let mut guard = engine.lock().unwrap();
                    let (snap, own, _) = stmt_snapshot(&mut guard, session.txn.as_mut());
                    exec::resolve_param_types(stmt, &prep.declared_oids, &guard, &snap, own)
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
        Some(stmt) => {
            let mut guard = engine.lock().unwrap();
            let (snap, own, _) = stmt_snapshot(&mut guard, session.txn.as_mut());
            exec::describe_columns(stmt, &prep.declared_oids, &guard, &snap, own)
        }
    }
}

/// Execute: portal name, max rows (0 = all). Honors max-rows via
/// PortalSuspended; the final CommandComplete carries the total row count.
fn handle_execute(
    stream: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
    payload: &[u8],
) -> io::Result<()> {
    let mut cur = Cursor::new(payload);
    let portal_name = cur.read_cstring()?;
    let max_rows = cur.read_i32()?;

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
    for row in pending.rows.iter().skip(pending.pos).take(n) {
        send_data_row(stream, row)?;
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
    let mut cur = Cursor::new(payload);
    let kind = cur.read_u8()?;
    let name = cur.read_cstring()?;
    match kind {
        b'S' => {
            session.stmts.remove(&name);
        }
        b'P' => {
            session.portals.remove(&name);
        }
        _ => {
            return protocol_error(
                stream,
                session,
                "08P01",
                "invalid close target (expected 'S' or 'P')",
            );
        }
    }
    MsgBuilder::new(b'3').send(stream)?; // CloseComplete
    stream.flush()?;
    Ok(())
}

fn send_row_description(stream: &mut Writer, columns: &[(String, ColType)]) -> io::Result<()> {
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

fn send_data_row(stream: &mut Writer, row: &[Value]) -> io::Result<()> {
    let mut b = MsgBuilder::new(b'D');
    b.i16(row.len() as i16);
    for v in row {
        match v.to_text() {
            None => {
                b.i32(-1); // NULL
            }
            Some(s) => {
                b.i32(s.len() as i32);
                b.bytes(s.as_bytes());
            }
        }
    }
    b.send(stream)
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
            in_error: false,
            txn: Some(Txn {
                xid,
                level,
                snapshot: None,
                writes: Vec::new(),
                savepoints: Vec::new(),
                failed: false,
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
                    crate::storage::RowVersion {
                        id: *row_id,
                        values: vec![Value::Int(i as i64)],
                        xmin: xid,
                        xmax: 0,
                    },
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
        let rb = Stmt::Rollback;
        let rbt = Stmt::RollbackTo { name: "sp".into() };
        let commit = Stmt::Commit;
        let sel = Stmt::Checkpoint;
        assert!(allowed_in_aborted(&rb));
        assert!(allowed_in_aborted(&rbt));
        assert!(allowed_in_aborted(&commit));
        assert!(!allowed_in_aborted(&sel));
    }
}
