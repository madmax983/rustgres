//! Connection handling: startup handshake + simple/extended protocol loops.
//!
//! v0.2 adds the extended query protocol: Parse/Bind/Describe/Execute/Close
//! with named/unnamed prepared statements and portals, `$N` parameters,
//! and PortalSuspended for row-limited Execute. After any extended-protocol
//! error the server discards input until Sync, per the protocol spec.
//!
//! v0.3 adds transactions: each session may hold an explicit transaction
//! (a private working copy of the database). Uncommitted writes are visible
//! only to their own session; COMMIT swaps the working copy into the shared
//! database, ROLLBACK discards it. SAVEPOINTs snapshot the working copy.
//! ReadyForQuery carries the real transaction status: 'I' idle, 'T' in
//! transaction, 'E' in a failed (aborted) transaction.

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::exec::{self, ExecError, ExecResult};
use crate::protocol::{read_message, read_startup, Cursor, MsgBuilder};
use crate::sql::{self, Stmt};
use crate::storage::{ColType, Database, Table, Value};
use crate::wal::{self, Wal, WalRecord};

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
}

struct PendingRows {
    rows: Vec<Vec<Value>>,
    pos: usize,
}

struct Session {
    stmts: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// After an extended-protocol error: discard input until Sync.
    in_error: bool,
    /// Explicit transaction state; None = autocommit.
    txn: Option<Txn>,
}

/// One explicit transaction: a private working copy of the database.
/// Uncommitted writes live here and are visible only to this session.
struct Txn {
    working: Database,
    /// (name, snapshot of `working`) stack for SAVEPOINT.
    savepoints: Vec<(String, Database)>,
    /// A failed statement aborts the transaction (Postgres semantics):
    /// only ROLLBACK / ROLLBACK TO / COMMIT are accepted afterwards.
    failed: bool,
}

impl Session {
    fn new() -> Self {
        Session {
            stmts: HashMap::new(),
            portals: HashMap::new(),
            in_error: false,
            txn: None,
        }
    }
}

pub fn handle_connection(
    stream: TcpStream,
    db: Arc<Mutex<Database>>,
    wal: Arc<Mutex<Wal>>,
) {
    // Reads use the raw socket; writes go through a BufWriter so each
    // message's small write_all calls (type byte, length, payload) — and,
    // crucially, thousands of DataRows — coalesce into a few large TCP
    // segments. Without this, TCP_NODELAY turns every row into its own
    // packet (see benches/BASELINE.md).
    let writer = BufWriter::new(stream.try_clone().expect("try_clone failed"));
    if let Err(e) = run_connection(stream, writer, db, wal) {
        eprintln!("connection closed with error: {}", e);
    }
}

fn run_connection(
    mut reader: TcpStream,
    mut writer: Writer,
    db: Arc<Mutex<Database>>,
    wal: Arc<Mutex<Wal>>,
) -> io::Result<()> {
    // --- Startup handshake -------------------------------------------------
    loop {
        let (proto, _params) = read_startup(&mut reader)?;
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
        break;
    }

    // AuthenticationOk (trust auth in v0.2)
    MsgBuilder::new(b'R').i32(0).send(&mut writer)?;

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
    let mut session = Session::new();
    send_ready(&mut writer, &session)?;
    writer.flush()?;

    // --- Message loop -------------------------------------------------------

    loop {
        let msg = read_message(&mut reader)?;
        // After an extended-protocol error, discard everything until Sync.
        if session.in_error {
            match msg.typ {
                b'S' => {
                    send_ready(&mut writer, &session)?;
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
                handle_query(&mut writer, &db, &wal, &mut session, &sql_text)?;
            }
            b'P' => handle_parse(&mut writer, &mut session, &msg.payload)?,
            b'B' => handle_bind(&mut writer, &db, &mut session, &msg.payload)?,
            b'D' => handle_describe(&mut writer, &db, &mut session, &msg.payload)?,
            b'E' => handle_execute(&mut writer, &db, &wal, &mut session, &msg.payload)?,
            b'C' => handle_close(&mut writer, &mut session, &msg.payload)?,
            b'X' => break, // Terminate
            b'H' => {
                writer.flush()?;
            }
            b'S' => {
                // Sync: ReadyForQuery with the real transaction status.
                send_ready(&mut writer, &session)?;
                writer.flush()?;
            }
            other => {
                send_error(
                    &mut writer,
                    "0A000",
                    &format!("unimplemented message type '{}'", other as char),
                )?;
                send_ready(&mut writer, &session)?;
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
    b.u8(b'S').cstr("ERROR");
    b.u8(b'V').cstr("ERROR");
    b.u8(b'C').cstr(code);
    b.u8(b'M').cstr(message);
    b.u8(0); // terminator
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
// Simple query protocol (v0.1 behavior + v0.3 transactions)
// ---------------------------------------------------------------------------

/// Run one simple-protocol Query message. The message may hold several
/// `;`-separated statements; each runs in its own implicit transaction
/// (statement-atomic), or inside the session's explicit transaction when
/// one is open. On the first error the remaining statements are skipped,
/// like Postgres. Exactly one ReadyForQuery closes the message.
fn handle_query(
    stream: &mut Writer,
    db: &Arc<Mutex<Database>>,
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
                send_error(stream, "42601", &format!("syntax error: {}", e.message))?;
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
        match run_statement(db, wal, session, &stmt) {
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
            Ok(ExecResult::Command { tag }) => {
                MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
            }
        }
    }

    send_ready(stream, session)?;
    stream.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// v0.3: transactions
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

/// Execute one statement with transaction semantics:
/// - transaction control statements manipulate the session's txn state;
/// - `CHECKPOINT` (v0.4) snapshots the committed database and truncates
///   the WAL; refused inside a transaction block, like Postgres;
/// - inside an explicit transaction, data statements run against the
///   session-private working copy (a failure aborts the transaction);
/// - in autocommit they run directly against the committed database, and
///   every write is WAL-logged + fsynced before it returns (each
///   autocommit statement is its own durable mini-transaction).
///   Statement atomicity in autocommit comes from the executor's
///   validate-then-apply discipline: every statement fully validates
///   before mutating anything, so a failed statement leaves no trace —
///   and nothing is logged for it.
fn run_statement(
    db: &Arc<Mutex<Database>>,
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
        Stmt::Begin => txn_begin(db, session),
        Stmt::Commit => txn_commit(db, wal, session),
        Stmt::Rollback => txn_rollback(session),
        Stmt::Savepoint { name } => txn_savepoint(session, name),
        Stmt::RollbackTo { name } => txn_rollback_to(session, name),
        Stmt::Release { name } => txn_release(session, name),
        Stmt::Checkpoint => txn_checkpoint(db, wal, session),
        _ => {
            if let Some(t) = session.txn.as_mut() {
                match exec::execute(&mut t.working, stmt) {
                    Ok(r) => Ok(r),
                    Err(e) => {
                        t.failed = true;
                        Err(e)
                    }
                }
            } else {
                autocommit_execute(db, wal, stmt)
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

/// Before-image of an autocommit write, used both to derive the WAL
/// records and to undo the in-memory mutation if the WAL append/fsync
/// fails. A failed WAL write must leave the committed database exactly
/// as it was: the client gets a 58000 error and nothing undurable is
/// ever visible.
enum Undo {
    /// INSERT: truncate the table's rows back to this length.
    Truncate { table: String, len: usize },
    /// CREATE TABLE: remove the newly created table.
    Remove { name: String },
    /// DROP TABLE: restore the removed table.
    Restore { name: String, table: Table },
    /// Read-only statement, or a write that cannot have mutated.
    None,
}

impl Undo {
    fn apply(self, db: &mut Database) {
        match self {
            Undo::Truncate { table, len } => {
                if let Some(t) = db.tables.get_mut(&table) {
                    t.rows.truncate(len);
                }
            }
            Undo::Remove { name } => {
                db.tables.remove(&name);
            }
            Undo::Restore { name, table } => {
                db.tables.insert(name, table);
            }
            Undo::None => {}
        }
    }
}

/// Autocommit write path (v0.4): the statement executes against the
/// committed database under the db lock, then its logical change is
/// appended to the WAL and fsynced *before* we return. Each autocommit
/// statement is its own durable mini-transaction. The records are derived
/// from the before-image (rather than diffing the whole database), which
/// keeps this O(changed rows): INSERT logs the appended row suffix via a
/// before/after row count, CREATE/DROP log their schema/name.
fn autocommit_execute(
    db: &Arc<Mutex<Database>>,
    wal: &Arc<Mutex<Wal>>,
    stmt: &Stmt,
) -> Result<ExecResult, ExecError> {
    let mut db_guard = db.lock().unwrap();
    // Capture the before-image while holding the db lock (no one else
    // can mutate concurrently).
    let undo = match stmt {
        Stmt::Insert { table, .. } => Undo::Truncate {
            table: table.clone(),
            len: db_guard.tables.get(table).map(|t| t.rows.len()).unwrap_or(0),
        },
        Stmt::CreateTable { name, .. } => Undo::Remove { name: name.clone() },
        Stmt::DropTable { name, .. } => match db_guard.tables.get(name) {
            Some(t) => Undo::Restore {
                name: name.clone(),
                table: t.clone(),
            },
            None => Undo::None, // execute will fail (or be a no-op); nothing to undo
        },
        _ => Undo::None,
    };
    let result = exec::execute(&mut db_guard, stmt)?;
    // Derive the WAL records from the same before-image.
    let records: Vec<WalRecord> = match &undo {
        Undo::Truncate { table, len } => match db_guard.tables.get(table) {
            Some(t) if t.rows.len() > *len => vec![WalRecord::InsertRows {
                table: table.clone(),
                rows: t.rows[*len..].to_vec(),
            }],
            _ => Vec::new(),
        },
        Undo::Remove { name } => match stmt {
            Stmt::CreateTable { columns, .. } => vec![WalRecord::CreateTable {
                name: name.clone(),
                columns: columns.clone(),
            }],
            _ => Vec::new(),
        },
        Undo::Restore { name, .. } => vec![WalRecord::DropTable { name: name.clone() }],
        Undo::None => Vec::new(),
    };
    // Lock order is always db -> wal.
    if !records.is_empty() {
        if let Err(e) = wal.lock().unwrap().append_batch(&records) {
            // WAL is not durable: roll back the in-memory mutation so a
            // failed commit never becomes visible, then report 58000.
            undo.apply(&mut db_guard);
            return Err(wal_err(e));
        }
    }
    Ok(result)
}

fn txn_begin(
    db: &Arc<Mutex<Database>>,
    session: &mut Session,
) -> Result<ExecResult, ExecError> {
    if session.txn.is_some() {
        // Postgres: WARNING "there is already a transaction in progress",
        // otherwise a no-op. v0.3 has no NOTICE channel, so plain no-op.
        return Ok(cmd("BEGIN"));
    }
    let working = db.lock().unwrap().clone();
    session.txn = Some(Txn {
        working,
        savepoints: Vec::new(),
        failed: false,
    });
    Ok(cmd("BEGIN"))
}

fn txn_commit(
    db: &Arc<Mutex<Database>>,
    wal: &Arc<Mutex<Wal>>,
    session: &mut Session,
) -> Result<ExecResult, ExecError> {
    match session.txn.take() {
        None => Ok(cmd("COMMIT")), // Postgres: WARNING, no-op
        Some(t) if t.failed => Ok(cmd("ROLLBACK")), // COMMIT of an aborted txn rolls back
        Some(t) => {
            let mut db_guard = db.lock().unwrap();
            // Diff the pre-commit database against the working copy and
            // make the delta durable BEFORE publishing it: COMMIT returns
            // only after the WAL frame is fsynced.
            let records = wal::diff_dbs(&db_guard, &t.working);
            if !records.is_empty() {
                // Lock order is always db -> wal.
                if let Err(e) = wal.lock().unwrap().append_batch(&records) {
                    // Nothing was published; hand the transaction back so
                    // the session can retry the COMMIT or ROLLBACK.
                    session.txn = Some(t);
                    return Err(wal_err(e));
                }
            }
            // Publish the working copy. (Concurrent transactions are
            // last-writer-wins in v0.4; real MVCC is a later milestone.)
            *db_guard = t.working;
            Ok(cmd("COMMIT"))
        }
    }
}

/// v0.4: snapshot the committed database and truncate the WAL.
/// Refused inside a transaction block, like Postgres.
fn txn_checkpoint(
    db: &Arc<Mutex<Database>>,
    wal: &Arc<Mutex<Wal>>,
    session: &Session,
) -> Result<ExecResult, ExecError> {
    if session.txn.is_some() {
        return Err(ExecError {
            code: "25001",
            message: "CHECKPOINT cannot be executed inside a transaction block".to_string(),
        });
    }
    let db_guard = db.lock().unwrap();
    wal.lock()
        .unwrap()
        .checkpoint(&db_guard)
        .map_err(wal_err)?;
    Ok(cmd("CHECKPOINT"))
}

fn txn_rollback(session: &mut Session) -> Result<ExecResult, ExecError> {
    session.txn.take();
    Ok(cmd("ROLLBACK"))
}

fn txn_savepoint(session: &mut Session, name: &str) -> Result<ExecResult, ExecError> {
    match session.txn.as_mut() {
        None => Err(err_25001("SAVEPOINT can only be used in transaction blocks")),
        Some(t) => {
            t.savepoints.push((name.to_string(), t.working.clone()));
            Ok(cmd("SAVEPOINT"))
        }
    }
}

fn txn_rollback_to(session: &mut Session, name: &str) -> Result<ExecResult, ExecError> {
    let t = session.txn.as_mut().ok_or_else(|| {
        err_25001("ROLLBACK TO SAVEPOINT can only be used in transaction blocks")
    })?;
    let idx = t
        .savepoints
        .iter()
        .rposition(|(n, _)| n == name)
        .ok_or_else(|| ExecError {
            code: "3B001",
            message: format!("no such savepoint \"{}\"", name),
        })?;
    t.working = t.savepoints[idx].1.clone();
    // Savepoints established after the named one are destroyed; the named
    // one stays valid. Rolling back also recovers from an aborted txn.
    t.savepoints.truncate(idx + 1);
    t.failed = false;
    Ok(cmd("ROLLBACK"))
}

fn txn_release(session: &mut Session, name: &str) -> Result<ExecResult, ExecError> {
    let t = session.txn.as_mut().ok_or_else(|| {
        err_25001("RELEASE SAVEPOINT can only be used in transaction blocks")
    })?;
    let idx = t
        .savepoints
        .iter()
        .rposition(|(n, _)| n == name)
        .ok_or_else(|| ExecError {
            code: "3B001",
            message: format!("no such savepoint \"{}\"", name),
        })?;
    // Destroys the named savepoint and all established after it.
    t.savepoints.truncate(idx);
    Ok(cmd("RELEASE"))
}

/// Read-only view of the data visible to a session: its transaction's
/// working copy when in a transaction, else the committed database.
enum DbView<'a> {
    Committed(std::sync::MutexGuard<'a, Database>),
    Working(&'a Database),
}

impl<'a> std::ops::Deref for DbView<'a> {
    type Target = Database;
    fn deref(&self) -> &Database {
        match self {
            DbView::Committed(g) => g,
            DbView::Working(w) => w,
        }
    }
}

fn db_view<'a>(db: &'a Arc<Mutex<Database>>, session: &'a Session) -> DbView<'a> {
    match &session.txn {
        Some(t) => DbView::Working(&t.working),
        None => DbView::Committed(db.lock().unwrap()),
    }
}

const ABORTED_MSG: &str =
    "current transaction is aborted, commands ignored until end of transaction block";

// ---------------------------------------------------------------------------
// Extended query protocol (v0.2)
// ---------------------------------------------------------------------------

/// Parse: statement name, query string, param OIDs. Stores the parsed AST.
fn handle_parse(
    stream: &mut Writer,
    session: &mut Session,
    payload: &[u8],
) -> io::Result<()> {
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
                )
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
    db: &Arc<Mutex<Database>>,
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
            )
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
        return protocol_error(
            stream,
            session,
            "0A000",
            "binary format not yet supported",
        );
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
                let view = db_view(db, session);
                exec::bind_params(&s, &prep.declared_oids, &raw, &view)
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
    db: &Arc<Mutex<Database>>,
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
                )
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
                    )
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
                    )
                }
            }
        }
        _ => {
            return protocol_error(
                stream,
                session,
                "08P01",
                "invalid describe target (expected 'S' or 'P')",
            )
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
                    let view = db_view(db, session);
                    exec::resolve_param_types(stmt, &prep.declared_oids, &view)
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

    match describe_prepared(&prep, db, session) {
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
    db: &Arc<Mutex<Database>>,
    session: &Session,
) -> Result<Option<Vec<(String, ColType)>>, exec::ExecError> {
    match &prep.stmt {
        None => Ok(None),
        Some(stmt) => {
            let view = db_view(db, session);
            exec::describe_columns(stmt, &prep.declared_oids, &view)
        }
    }
}

/// Execute: portal name, max rows (0 = all). Honors max-rows via
/// PortalSuspended; the final CommandComplete carries the total row count.
fn handle_execute(
    stream: &mut Writer,
    db: &Arc<Mutex<Database>>,
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
    if session.portals[&portal_name].done
        && session.portals[&portal_name].pending.is_none()
    {
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
        match run_statement(db, wal, session, &stmt) {
            Err(e) => return protocol_error(stream, session, e.code, &e.message),
            Ok(ExecResult::Select { rows, .. }) => {
                session.portals.get_mut(&portal_name).unwrap().pending =
                    Some(PendingRows { rows, pos: 0 });
            }
            Ok(ExecResult::Command { tag }) => {
                MsgBuilder::new(b'C').cstr(&tag).send(stream)?;
                let portal = session.portals.get_mut(&portal_name).unwrap();
                portal.done = true;
                portal.last_tag = Some(tag);
                stream.flush()?;
                return Ok(());
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
        let tag = format!("SELECT {}", total);
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
fn handle_close(
    stream: &mut Writer,
    session: &mut Session,
    payload: &[u8],
) -> io::Result<()> {
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
            )
        }
    }
    MsgBuilder::new(b'3').send(stream)?; // CloseComplete
    stream.flush()?;
    Ok(())
}

fn send_row_description(
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
    use crate::storage::ColType;

    fn sample_db() -> Database {
        let mut db = Database::new();
        db.tables.insert(
            "t".to_string(),
            Table {
                columns: vec![("a".to_string(), ColType::Int)],
                rows: vec![vec![Value::Int(1)]],
            },
        );
        db
    }

    #[test]
    fn undo_truncate_restores_row_count() {
        let mut db = sample_db();
        // Simulate a successful INSERT of two rows followed by WAL failure.
        db.tables.get_mut("t").unwrap().rows.push(vec![Value::Int(2)]);
        db.tables.get_mut("t").unwrap().rows.push(vec![Value::Int(3)]);
        Undo::Truncate {
            table: "t".into(),
            len: 1,
        }
        .apply(&mut db);
        assert_eq!(db.tables["t"].rows, vec![vec![Value::Int(1)]]);
    }

    #[test]
    fn undo_remove_drops_created_table() {
        let mut db = sample_db();
        db.tables.insert(
            "new".to_string(),
            Table {
                columns: vec![],
                rows: vec![],
            },
        );
        Undo::Remove { name: "new".into() }.apply(&mut db);
        assert!(!db.tables.contains_key("new"));
        assert!(db.tables.contains_key("t")); // untouched
    }

    #[test]
    fn undo_restore_brings_back_dropped_table() {
        let mut db = sample_db();
        let t = db.tables.remove("t").unwrap();
        Undo::Restore {
            name: "t".into(),
            table: t,
        }
        .apply(&mut db);
        assert_eq!(db.tables["t"].rows, vec![vec![Value::Int(1)]]);
    }
}
