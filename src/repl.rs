//! Streaming replication: the walsender protocol + logical decoding (v0.13).
//!
//! A client that connects with `replication=true` in its startup packet
//! becomes a *replication client*. After the normal authentication and
//! startup handshake it speaks the walsender dialect instead of SQL:
//!
//! - `IDENTIFY_SYSTEM` — one row: system id, timeline, WAL position, db name.
//! - `CREATE_REPLICATION_SLOT name LOGICAL rustgres_decoding`
//! - `DROP_REPLICATION_SLOT name`
//! - `START_REPLICATION SLOT name LOGICAL 0/0` — switches the connection
//!   into CopyBoth mode and streams logical changes until the client sends
//!   CopyDone.
//!
//! Slots are cluster-global and durable: creation, drops, and flush
//! positions are WAL-logged (`ReplSlot*` records) and checkpointed, so they
//! survive restarts. The single built-in output plugin is
//! `rustgres_decoding`, a simple text format (documented in README.md):
//!
//! ```text
//! BEGIN 0/1A2B
//! INSERT t id=1 name='alice'
//! UPDATE t OLD id=1 name='alice' NEW id=1 name='bob'
//! DELETE t id=1 name='bob'
//! DDL CREATE_TABLE t
//! COMMIT 0/1A2B
//! ```
//!
//! Deliberate simplifications vs PostgreSQL 19 (all documented):
//! - One timeline (1) forever; `TIMELINE_HISTORY` is rejected.
//! - No base backup (`BASE_BACKUP` is rejected); physical slots can be
//!   created but `START_REPLICATION ... PHYSICAL` is rejected.
//! - Replication connections require a superuser role (PostgreSQL would
//!   also allow roles with the REPLICATION attribute).
//! - `replication=database` is treated as a normal SQL connection.
//! - The walsender polls the WAL; it does not get push-notified of
//!   commits, so sub-second latency depends on the poll interval.
//! - The decoder reads the *live* table definition for column names; a
//!   table dropped after its changes were logged decodes with
//!   positional `col1..colN` names.

use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::protocol::{Cursor, MsgBuilder, read_message};
use crate::server::{
    Session, Writer, lock_engine, lock_wal, send_data_row, send_error, send_ready,
    send_row_description,
};
use crate::storage::{ColType, Engine, ReplSlot, Value};
use crate::wal::{Wal, WalRecord};

/// Timeline id: rustgres never branches history, so this is always 1.
const TIMELINE: i32 = 1;
/// The single built-in logical decoding plugin (v0.13).
pub const DECODING_PLUGIN: &str = "rustgres_decoding";
/// How often the streaming loop re-reads the WAL for new frames.
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How often the walsender sends a keepalive when otherwise idle.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// Standby flush positions are WAL-logged at most this often (plus once
/// when the stream ends); the in-memory position advances immediately.
const FLUSH_LOG_INTERVAL: Duration = Duration::from_secs(1);
/// Microseconds between the Unix epoch and PostgreSQL's 2000-01-01 epoch.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// Format an LSN the way PostgreSQL displays `pg_lsn`: `HIGH/LOW` hex.
pub fn format_lsn(lsn: u64) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn & 0xFFFF_FFFF)
}

/// Parse `HIGH/LOW` hex (also accepts a bare decimal or hex integer).
fn parse_lsn(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some((hi, lo)) = s.split_once('/') {
        let hi = u64::from_str_radix(hi.trim(), 16).ok()?;
        let lo = u64::from_str_radix(lo.trim(), 16).ok()?;
        Some((hi << 32) | lo)
    } else if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// PostgreSQL slot-name rules: 1-63 chars of lowercase letters, digits,
/// underscore.
fn valid_slot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Send time for XLogData/keepalive: microseconds since 2000-01-01.
fn pg_now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64 - PG_EPOCH_MICROS)
        .unwrap_or(0)
}

/// A replication-command failure: SQLSTATE + message for ErrorResponse.
#[derive(Debug)]
struct ReplError {
    code: &'static str,
    message: String,
}

impl ReplError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        ReplError {
            code,
            message: message.into(),
        }
    }
}

fn send_command_complete(writer: &mut Writer, session: &Session, tag: &str) -> io::Result<()> {
    MsgBuilder::new(b'C').cstr(tag).send(writer)?;
    send_ready(writer, session)?;
    writer.flush()?;
    Ok(())
}

/// ErrorResponse for a replication command: like the simple-query path,
/// every error is followed by ReadyForQuery so the client can continue.
fn send_repl_error(
    writer: &mut Writer,
    session: &Session,
    code: &str,
    message: &str,
) -> io::Result<()> {
    send_error(writer, code, message)?;
    send_ready(writer, session)?;
    writer.flush()?;
    Ok(())
}

/// Column names for `table`: the live definition if the table still
/// exists, else positional `col1..colN` (the table may have been dropped
/// after its changes were WAL-logged).
fn table_columns(eng: &Engine, table: &str, n: usize) -> Vec<String> {
    if let Some(versions) = eng.db.tables.get(table) {
        if let Some(t) = versions.last() {
            return t.columns.iter().map(|(name, _)| name.clone()).collect();
        }
    }
    (1..=n).map(|i| format!("col{}", i)).collect()
}

/// Render one value for the logical stream: SQL-ish text. `Text` is
/// single-quoted with `''` escapes; NULL stays NULL; everything else uses
/// the same text form as the wire protocol's DataRow.
fn logical_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.to_text().unwrap_or_else(|| "NULL".to_string()),
    }
}

fn row_text(cols: &[String], values: &[Value]) -> String {
    cols.iter()
        .zip(values.iter())
        .map(|(c, v)| format!("{}={}", c, logical_value(v)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Decode one committed WAL frame into `rustgres_decoding` lines. Frames
/// with no table changes (sequence advances, slot metadata, ...) produce
/// no output — but the caller still advances past their LSN.
fn decode_frame(eng: &Engine, frame_lsn: u64, records: &[WalRecord]) -> Vec<String> {
    let mut changes = Vec::new();
    for r in records {
        match r {
            WalRecord::InsertRows { table, rows } => {
                let cols = table_columns(
                    eng,
                    table,
                    rows.first().map(|r| r.values.len()).unwrap_or(0),
                );
                for row in rows {
                    changes.push(format!("INSERT {} {}", table, row_text(&cols, &row.values)));
                }
            }
            WalRecord::UpdateRows {
                table, old, new, ..
            } => {
                let n = old
                    .first()
                    .map(|r| r.values.len())
                    .unwrap_or_else(|| new.first().map(|r| r.values.len()).unwrap_or(0));
                let cols = table_columns(eng, table, n);
                for (o, nrow) in old.iter().zip(new.iter()) {
                    changes.push(format!(
                        "UPDATE {} OLD {} NEW {}",
                        table,
                        row_text(&cols, &o.values),
                        row_text(&cols, &nrow.values)
                    ));
                }
            }
            WalRecord::DeleteRows {
                table, old_rows, ..
            } => {
                let cols = table_columns(
                    eng,
                    table,
                    old_rows.first().map(|r| r.values.len()).unwrap_or(0),
                );
                for row in old_rows {
                    changes.push(format!("DELETE {} {}", table, row_text(&cols, &row.values)));
                }
            }
            WalRecord::CreateTable { name, .. } => {
                changes.push(format!("DDL CREATE_TABLE {}", name));
            }
            WalRecord::DropTable { name, .. } => {
                changes.push(format!("DDL DROP_TABLE {}", name));
            }
            WalRecord::AlterTable { name, .. } => {
                changes.push(format!("DDL ALTER_TABLE {}", name));
            }
            // Non-table records (sequences, roles, ACLs, slot metadata)
            // are invisible to logical decoding in v0.13.
            _ => {}
        }
    }
    if changes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(changes.len() + 2);
    out.push(format!("BEGIN {}", format_lsn(frame_lsn)));
    out.extend(changes);
    out.push(format!("COMMIT {}", format_lsn(frame_lsn)));
    out
}

// ---------------------------------------------------------------------------
// Replication slot lifecycle (durable, WAL-logged, checkpointed)
// ---------------------------------------------------------------------------

/// Create a slot. Non-transactional, like PostgreSQL: the slot exists as
/// soon as the WAL record is fsynced.
fn create_slot(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    name: &str,
    slot_type: &str,
    plugin: &str,
) -> Result<u64, ReplError> {
    if !valid_slot_name(name) {
        return Err(ReplError::new(
            "42602",
            format!(
                "invalid replication slot name \"{}\": use 1-63 lowercase letters, digits, underscores",
                name
            ),
        ));
    }
    if slot_type != "logical" && slot_type != "physical" {
        return Err(ReplError::new(
            "42602",
            format!(
                "invalid slot type \"{}\": expected LOGICAL or PHYSICAL",
                slot_type
            ),
        ));
    }
    if slot_type == "logical" && plugin != DECODING_PLUGIN {
        return Err(ReplError::new(
            "0A000",
            format!(
                "output plugin \"{}\" is not supported; v0.13 only has \"{}\"",
                plugin, DECODING_PLUGIN
            ),
        ));
    }
    let mut eng = lock_engine(engine);
    if eng.repl_slots.contains_key(name) {
        return Err(ReplError::new(
            "42710",
            format!("replication slot \"{}\" already exists", name),
        ));
    }
    let restart_lsn = lock_wal(wal).current_lsn();
    eng.repl_slots.insert(
        name.to_string(),
        ReplSlot {
            name: name.to_string(),
            plugin: if slot_type == "logical" {
                plugin.to_string()
            } else {
                String::new()
            },
            slot_type: slot_type.to_string(),
            restart_lsn,
            confirmed_flush_lsn: restart_lsn,
            active: false,
        },
    );
    drop(eng);
    // Durability: the slot survives a crash between CREATE and the next
    // checkpoint via the WAL; the checkpoint image carries it afterwards.
    // If the WAL append fails, roll the in-memory insert back so the
    // catalog never claims a slot that won't survive a restart.
    if let Err(e) = lock_wal(wal).append_batch(&[WalRecord::ReplSlotCreate {
        name: name.to_string(),
        plugin: if slot_type == "logical" {
            plugin.to_string()
        } else {
            String::new()
        },
        slot_type: slot_type.to_string(),
        restart_lsn,
    }]) {
        lock_engine(engine).repl_slots.remove(name);
        return Err(ReplError::new(
            "58000",
            format!("could not persist replication slot: {}", e),
        ));
    }
    Ok(restart_lsn)
}

fn drop_slot(
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    name: &str,
) -> Result<(), ReplError> {
    let mut eng = lock_engine(engine);
    match eng.repl_slots.get(name) {
        None => {
            return Err(ReplError::new(
                "42704",
                format!("replication slot \"{}\" does not exist", name),
            ));
        }
        Some(s) if s.active => {
            return Err(ReplError::new(
                "55006",
                format!("replication slot \"{}\" is active", name),
            ));
        }
        _ => {}
    }
    let removed = eng.repl_slots.remove(name);
    drop(eng);
    // If the WAL append fails, restore the in-memory slot so the catalog
    // never drops a slot that will reappear after a restart.
    if let Err(e) = lock_wal(wal).append_batch(&[WalRecord::ReplSlotDrop {
        name: name.to_string(),
    }]) {
        if let Some(slot) = removed {
            lock_engine(engine)
                .repl_slots
                .insert(name.to_string(), slot);
        }
        return Err(ReplError::new(
            "58000",
            format!("could not persist slot drop: {}", e),
        ));
    }
    Ok(())
}

/// WAL-log a slot's flush position (called at most once per second while
/// streaming, and once when the stream ends).
fn log_slot_flush(
    wal: &Arc<Mutex<Wal>>,
    name: &str,
    restart_lsn: u64,
    confirmed_flush_lsn: u64,
) -> Result<(), ReplError> {
    lock_wal(wal)
        .append_batch(&[WalRecord::ReplSlotFlush {
            name: name.to_string(),
            restart_lsn,
            confirmed_flush_lsn,
        }])
        .map_err(|e| ReplError::new("58000", format!("could not persist slot flush: {}", e)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Replication commands (simple-protocol Query messages)
// ---------------------------------------------------------------------------

/// `IDENTIFY_SYSTEM`: one row — systemid, timeline, xlogpos, dbname.
fn cmd_identify_system(
    writer: &mut Writer,
    session: &Session,
    wal: &Arc<Mutex<Wal>>,
    database: &str,
) -> io::Result<()> {
    let system_id = lock_wal(wal).system_id();
    let xlogpos = format_lsn(lock_wal(wal).current_lsn());
    send_row_description(
        writer,
        &[
            ("systemid".to_string(), ColType::Text),
            ("timeline".to_string(), ColType::Int),
            ("xlogpos".to_string(), ColType::Text),
            ("dbname".to_string(), ColType::Text),
        ],
    )?;
    send_data_row(
        writer,
        &[
            Value::Text(system_id.to_string()),
            Value::Int(TIMELINE as i64),
            Value::Text(xlogpos),
            Value::Text(database.to_string()),
        ],
    )?;
    send_command_complete(writer, session, "IDENTIFY_SYSTEM")
}

/// `CREATE_REPLICATION_SLOT name LOGICAL plugin`: one row — slot_name,
/// consistent_point, snapshot_name, output_plugin.
fn cmd_create_slot(
    writer: &mut Writer,
    session: &Session,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    words: &[&str],
) -> io::Result<()> {
    let parsed: Result<(String, String, String), ReplError> = (|| {
        if words.len() < 3 {
            return Err(ReplError::new(
                "42601",
                "syntax error: expected CREATE_REPLICATION_SLOT name { LOGICAL plugin | PHYSICAL }",
            ));
        }
        let name = words[1].to_string();
        let mut i = 2;
        // Optional TEMPORARY (not supported in v0.13).
        if words[i].eq_ignore_ascii_case("temporary") {
            return Err(ReplError::new(
                "0A000",
                "TEMPORARY replication slots are not supported in v0.13",
            ));
        }
        let slot_type = if words[i].eq_ignore_ascii_case("logical") {
            "logical"
        } else if words[i].eq_ignore_ascii_case("physical") {
            "physical"
        } else {
            return Err(ReplError::new(
                "42601",
                "syntax error: expected LOGICAL or PHYSICAL",
            ));
        };
        i += 1;
        let plugin = if slot_type == "logical" {
            if i >= words.len() {
                return Err(ReplError::new(
                    "42601",
                    "syntax error: LOGICAL slots need an output plugin name",
                ));
            }
            let p = words[i].to_string();
            i += 1;
            p
        } else {
            // Optional RESERVE_WAL — accepted and ignored (we always
            // reserve from the slot's restart_lsn).
            if i < words.len() && words[i].eq_ignore_ascii_case("reserve_wal") {
                i += 1;
            }
            String::new()
        };
        // Trailing options (EXPORT_SNAPSHOT etc.) are accepted and ignored.
        let _ = i;
        Ok((name, slot_type.to_string(), plugin))
    })();
    let (name, slot_type, plugin) = match parsed {
        Ok(t) => t,
        Err(e) => return send_repl_error(writer, session, e.code, &e.message),
    };
    match create_slot(engine, wal, &name, &slot_type, &plugin) {
        Ok(consistent_point) => {
            send_row_description(
                writer,
                &[
                    ("slot_name".to_string(), ColType::Text),
                    ("consistent_point".to_string(), ColType::Text),
                    ("snapshot_name".to_string(), ColType::Text),
                    ("output_plugin".to_string(), ColType::Text),
                ],
            )?;
            send_data_row(
                writer,
                &[
                    Value::Text(name),
                    Value::Text(format_lsn(consistent_point)),
                    // No exported snapshot in v0.13: the consistent point
                    // is the slot's restart LSN.
                    Value::Null,
                    Value::Text(if slot_type == "logical" {
                        plugin
                    } else {
                        String::new()
                    }),
                ],
            )?;
            send_command_complete(writer, session, "CREATE_REPLICATION_SLOT")
        }
        Err(e) => return send_repl_error(writer, session, e.code, &e.message),
    }
}

fn cmd_drop_slot(
    writer: &mut Writer,
    session: &Session,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    words: &[&str],
) -> io::Result<()> {
    if words.len() != 2 {
        return send_repl_error(
            writer,
            session,
            "42601",
            "syntax error: expected DROP_REPLICATION_SLOT name",
        );
    }
    match drop_slot(engine, wal, words[1]) {
        Ok(()) => send_command_complete(writer, session, "DROP_REPLICATION_SLOT"),
        Err(e) => send_repl_error(writer, session, e.code, &e.message),
    }
}

// ---------------------------------------------------------------------------
// START_REPLICATION: the walsender streaming loop (CopyBoth)
// ---------------------------------------------------------------------------

/// What ended the streaming loop.
enum StreamEnd {
    /// Client sent CopyDone (or the stream otherwise finished cleanly):
    /// back to the replication command loop.
    Done,
    /// Client sent Terminate: close the connection.
    Terminate,
}

/// Send one CopyData message wrapping an XLogData ('d') or keepalive
/// ('w') payload builder. The inner builder's type byte is part of the
/// payload (XLogData/keepalive kind); CopyData's own 'd' is the outer byte.
fn send_copy_data(writer: &mut Writer, inner: MsgBuilder) -> io::Result<()> {
    let mut b = MsgBuilder::new(b'd');
    b.u8(inner.kind()).bytes(&inner.into_payload());
    b.send(writer)
}

fn xlog_data_payload(data_start: u64, wal_end: u64, line: &str) -> MsgBuilder {
    let mut p = MsgBuilder::new(b'd');
    p.i64(data_start as i64)
        .i64(wal_end as i64)
        .i64(pg_now_micros())
        .bytes(line.as_bytes());
    p
}

fn keepalive_payload(wal_end: u64, reply_requested: bool) -> MsgBuilder {
    let mut p = MsgBuilder::new(b'w');
    p.i64(wal_end as i64)
        .i64(pg_now_micros())
        .u8(u8::from(reply_requested));
    p
}

/// Parse `START_REPLICATION SLOT name LOGICAL lsn [(options)]`.
fn parse_start_replication(words: &[&str]) -> Result<(String, u64), ReplError> {
    if words.len() < 5 || !words[1].eq_ignore_ascii_case("slot") {
        return Err(ReplError::new(
            "42601",
            "syntax error: expected START_REPLICATION SLOT name LOGICAL start_lsn",
        ));
    }
    let name = words[2].to_string();
    if !words[3].eq_ignore_ascii_case("logical") {
        return Err(ReplError::new(
            "0A000",
            "only LOGICAL replication is supported in v0.13 (PHYSICAL needs base backup)",
        ));
    }
    let lsn = parse_lsn(words[4])
        .ok_or_else(|| ReplError::new("42602", format!("invalid start LSN \"{}\"", words[4])))?;
    // Any parenthesized options are accepted and ignored.
    Ok((name, lsn))
}

/// Stream logical changes for `slot` from `start_lsn` until the client
/// sends CopyDone or disconnects.
///
/// The slot is deactivated and the socket read timeout restored on
/// EVERY exit path (normal end, client disconnect, I/O error): the
/// streaming loop lives in `stream_loop` and this wrapper runs one
/// cleanup sequence after it returns, so an early `?` can never leave
/// the slot stuck `active` or the socket in streaming mode.
fn stream_changes(
    reader: &mut TcpStream,
    writer: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    slot_name: &str,
    start_lsn: u64,
) -> io::Result<StreamEnd> {
    // Activate the slot (check-and-set under one lock hold).
    let next_lsn = {
        let mut eng = lock_engine(engine);
        let slot = eng.repl_slots.get_mut(slot_name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("replication slot \"{}\" does not exist", slot_name),
            )
        })?;
        if slot.active {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("replication slot \"{}\" is already active", slot_name),
            ));
        }
        if slot.slot_type != "logical" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("replication slot \"{}\" is not a logical slot", slot_name),
            ));
        }
        slot.active = true;
        start_lsn.max(slot.confirmed_flush_lsn)
    };

    // CopyBothResponse: overall format 0 (text), zero columns.
    MsgBuilder::new(b'W').u8(0).i16(0).send(writer)?;
    writer.flush()?;
    reader.set_read_timeout(Some(POLL_INTERVAL))?;

    let end = stream_loop(reader, writer, engine, wal, slot_name, next_lsn);

    // Single cleanup exit path: deactivates the slot and persists the
    // final flush position, then restores command-mode socket behavior.
    {
        let mut eng = lock_engine(engine);
        if let Some(s) = eng.repl_slots.get_mut(slot_name) {
            s.active = false;
            let (rs, cf) = (s.restart_lsn, s.confirmed_flush_lsn);
            drop(eng);
            if let Err(e) = log_slot_flush(wal, slot_name, rs, cf) {
                eprintln!("walsender: {}", e.message);
            }
        }
    }
    reader.set_read_timeout(None).ok();
    writer.flush().ok();
    end
}

/// The walsender poll loop: CopyData out, standby-status in.
fn stream_loop(
    reader: &mut TcpStream,
    writer: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    slot_name: &str,
    mut next_lsn: u64,
) -> io::Result<StreamEnd> {
    let mut last_keepalive = Instant::now();
    let mut last_flush_log = Instant::now() - FLUSH_LOG_INTERVAL;
    let end = loop {
        // 1. Poll the WAL for new frames and decode them (locks held only
        //    while reading/decoding; the socket writes happen unlocked).
        let (frames, wal_end) = {
            let eng = lock_engine(engine);
            let mut w = lock_wal(wal);
            let wal_end = w.current_lsn();
            let raw = w.read_frames_since(next_lsn).map_err(|e| {
                io::Error::new(io::ErrorKind::Other, format!("WAL read failed: {}", e))
            })?;
            let mut frames = Vec::with_capacity(raw.len());
            for (flsn, _txn, records) in &raw {
                frames.push((*flsn, decode_frame(&eng, *flsn, records)));
            }
            (frames, wal_end)
        };
        let n = frames.len();
        for (i, (flsn, lines)) in frames.iter().enumerate() {
            // A frame's end LSN is the next frame's start (or the WAL end
            // for the last frame).
            let frame_end = if i + 1 < n { frames[i + 1].0 } else { wal_end };
            for line in lines {
                send_copy_data(writer, xlog_data_payload(*flsn, wal_end, line))?;
            }
            next_lsn = frame_end.max(next_lsn);
        }
        if n > 0 {
            // We made progress: the restart floor follows what we sent.
            // (confirmed_flush only moves on standby feedback.)
            let mut eng = lock_engine(engine);
            if let Some(s) = eng.repl_slots.get_mut(slot_name) {
                s.restart_lsn = s.restart_lsn.max(next_lsn);
            }
        }

        // 2. Read one client message (timed out = no input this round).
        match read_message(reader) {
            Ok(msg) => match msg.typ {
                b'd' => {
                    // CopyData from the client: 'r' = standby status.
                    let mut cur = Cursor::new(&msg.payload);
                    let kind = cur.read_u8().map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("{}", e))
                    })?;
                    if kind != b'r' {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "protocol violation: unexpected client CopyData",
                        ));
                    }
                    let _written = cur.read_i64().map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("{}", e))
                    })?;
                    let flushed = cur.read_i64().map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("{}", e))
                    })?;
                    let _applied = cur.read_i64().map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("{}", e))
                    })?;
                    let _send_time = cur.read_i64().map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("{}", e))
                    })?;
                    let flushed = flushed.max(0) as u64;
                    let mut eng = lock_engine(engine);
                    if let Some(s) = eng.repl_slots.get_mut(slot_name) {
                        if flushed > s.confirmed_flush_lsn {
                            s.confirmed_flush_lsn = flushed;
                            s.restart_lsn = s.restart_lsn.max(flushed);
                        }
                        let (rs, cf) = (s.restart_lsn, s.confirmed_flush_lsn);
                        drop(eng);
                        if last_flush_log.elapsed() >= FLUSH_LOG_INTERVAL {
                            last_flush_log = Instant::now();
                            if let Err(e) = log_slot_flush(wal, slot_name, rs, cf) {
                                eprintln!("walsender: {}", e.message);
                            }
                        }
                    }
                }
                b'c' => break StreamEnd::Done, // CopyDone
                b'X' => break StreamEnd::Terminate,
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "protocol violation: unexpected message '{}' during replication",
                            other as char
                        ),
                    ));
                }
            },
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock =>
            {
                // No client input this round; keep streaming.
            }
            Err(_) => break StreamEnd::Done, // client went away
        }

        // 3. Idle keepalive so the downstream knows we're alive (and its
        //    clock stays warm for lag tracking).
        if last_keepalive.elapsed() >= KEEPALIVE_INTERVAL {
            last_keepalive = Instant::now();
            let wal_end = lock_wal(wal).current_lsn();
            send_copy_data(writer, keepalive_payload(wal_end, false))?;
        }
        writer.flush()?;
    };
    Ok(end)
}

// ---------------------------------------------------------------------------
// The replication connection loop
// ---------------------------------------------------------------------------

/// Handle one `replication=true` connection: replication commands over the
/// simple protocol, then CopyBoth streaming for START_REPLICATION.
pub fn run_replication(
    reader: &mut TcpStream,
    writer: &mut Writer,
    engine: &Arc<Mutex<Engine>>,
    wal: &Arc<Mutex<Wal>>,
    role: String,
    database: String,
) -> io::Result<()> {
    let session = Session::new(role);
    loop {
        let msg = read_message(reader)?;
        match msg.typ {
            b'Q' => {
                let mut cur = Cursor::new(&msg.payload);
                let text = cur
                    .read_cstring()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}", e)))?;
                let words: Vec<&str> = text.split_whitespace().collect();
                let cmd = words.first().copied().unwrap_or("").to_ascii_uppercase();
                match cmd.as_str() {
                    "IDENTIFY_SYSTEM" => cmd_identify_system(writer, &session, wal, &database)?,
                    "CREATE_REPLICATION_SLOT" => {
                        cmd_create_slot(writer, &session, engine, wal, &words)?
                    }
                    "DROP_REPLICATION_SLOT" => {
                        cmd_drop_slot(writer, &session, engine, wal, &words)?
                    }
                    "START_REPLICATION" => match parse_start_replication(&words) {
                        Ok((name, lsn)) => {
                            // Slot pre-checks with precise SQLSTATEs.
                            let slot_ok = {
                                let eng = lock_engine(engine);
                                match eng.repl_slots.get(name.as_str()) {
                                    None => Err(ReplError::new(
                                        "42704",
                                        &format!("replication slot \"{}\" does not exist", name),
                                    )),
                                    Some(s) if s.active => Err(ReplError::new(
                                        "55006",
                                        &format!(
                                            "replication slot \"{}\" is active for another standby",
                                            name
                                        ),
                                    )),
                                    Some(s) if s.slot_type != "logical" => Err(ReplError::new(
                                        "0A000",
                                        &format!(
                                            "replication slot \"{}\" is not a logical slot",
                                            name
                                        ),
                                    )),
                                    _ => Ok(()),
                                }
                            };
                            match slot_ok {
                                Err(e) => send_repl_error(writer, &session, e.code, &e.message)?,
                                Ok(()) => {
                                    match stream_changes(reader, writer, engine, wal, &name, lsn) {
                                        // Back to command mode: ReadyForQuery
                                        // so the client can issue more commands.
                                        Ok(StreamEnd::Done) => {
                                            send_ready(writer, &session)?;
                                            writer.flush()?;
                                        }
                                        Ok(StreamEnd::Terminate) => return Ok(()),
                                        Err(e) => return Err(e),
                                    }
                                }
                            }
                        }
                        Err(e) => send_repl_error(writer, &session, e.code, &e.message)?,
                    },
                    "TIMELINE_HISTORY" => {
                        send_repl_error(
                            writer,
                            &session,
                            "0A000",
                            "TIMELINE_HISTORY is not supported: rustgres has a single timeline (1)",
                        )?;
                    }
                    "BASE_BACKUP" => {
                        send_repl_error(
                            writer,
                            &session,
                            "0A000",
                            "BASE_BACKUP is not supported in v0.13 (documented future work)",
                        )?;
                    }
                    _ => {
                        send_repl_error(
                            writer,
                            &session,
                            "42601",
                            &format!(
                                "syntax error: \"{}\" is not a replication command",
                                words.first().copied().unwrap_or("")
                            ),
                        )?;
                    }
                }
            }
            b'X' => break, // Terminate
            b'H' => {
                writer.flush()?;
            }
            other => {
                // Like the normal message loop: unknown message types are
                // FATAL protocol violations.
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
    }
    Ok(())
}

/// Whether a startup packet asks for a replication connection.
/// `replication=database` is a normal SQL connection (documented).
pub fn is_replication_startup(params: &[(String, String)]) -> bool {
    params.iter().any(|(k, v)| {
        k == "replication" && matches!(v.to_lowercase().as_str(), "true" | "1" | "on" | "yes")
    })
}

/// v0.13: replication connections require a superuser role (PostgreSQL
/// would also accept the REPLICATION attribute, which rustgres roles
/// don't have yet).
pub fn check_replication_role(engine: &Arc<Mutex<Engine>>, role: &str) -> bool {
    let eng = lock_engine(engine);
    let snap = eng.take_snapshot();
    crate::storage::is_superuser_snap(&eng.db, role, &snap, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Table;
    use crate::wal::{WalRecord, WalRow};

    fn engine_with_t() -> Engine {
        let mut eng = Engine::new();
        eng.db.tables.insert(
            "t".into(),
            vec![Table::new(
                vec![("id".into(), ColType::Int), ("name".into(), ColType::Text)],
                1,
            )],
        );
        eng
    }

    #[test]
    fn lsn_format_parse_roundtrip() {
        for lsn in [0u64, 1, 0xFFFF_FFFF, 0x1_0000_0020, u64::MAX] {
            assert_eq!(parse_lsn(&format_lsn(lsn)), Some(lsn), "lsn={}", lsn);
        }
        assert_eq!(format_lsn(0x1_0000_0020), "1/20");
        assert_eq!(parse_lsn("1/20"), Some(0x1_0000_0020));
        assert_eq!(parse_lsn("0x10"), Some(0x10));
        assert_eq!(parse_lsn("42"), Some(42));
        assert_eq!(parse_lsn(""), None);
        assert_eq!(parse_lsn("zz"), None);
        assert_eq!(parse_lsn("1/2/3"), None);
        assert_eq!(parse_lsn("1/"), None);
    }

    #[test]
    fn slot_name_validation() {
        assert!(valid_slot_name("myslot"));
        assert!(valid_slot_name("s3"));
        assert!(valid_slot_name("a_b_c"));
        assert!(valid_slot_name(&"a".repeat(63)));
        assert!(!valid_slot_name(""));
        assert!(!valid_slot_name("BadName"));
        assert!(!valid_slot_name("has-dash"));
        assert!(!valid_slot_name("has space"));
        assert!(!valid_slot_name(&"a".repeat(64)));
    }

    #[test]
    fn decode_frame_exact_lines() {
        let eng = engine_with_t();
        let lsn = 0x1_0000_0020u64;
        let recs = vec![
            WalRecord::CreateTable {
                name: "t".into(),
                columns: vec![],
                constraints: String::new(),
                owner: "postgres".into(),
                acl: vec![],
                col_acl: vec![],
                xmin: 1,
            },
            WalRecord::InsertRows {
                table: "t".into(),
                rows: vec![WalRow {
                    id: 1,
                    xmin: 2,
                    values: vec![Value::Int(1), Value::Text("o'clock".into())],
                }],
            },
            WalRecord::UpdateRows {
                table: "t".into(),
                old: vec![WalRow {
                    id: 1,
                    xmin: 2,
                    values: vec![Value::Int(1), Value::Text("a".into())],
                }],
                new: vec![WalRow {
                    id: 2,
                    xmin: 3,
                    values: vec![Value::Int(1), Value::Null],
                }],
                xmax: 3,
            },
            WalRecord::DeleteRows {
                table: "t".into(),
                ids: vec![2],
                old_rows: vec![WalRow {
                    id: 2,
                    xmin: 3,
                    values: vec![Value::Int(1), Value::Null],
                }],
                xmax: 4,
            },
        ];
        let lines = decode_frame(&eng, lsn, &recs);
        assert_eq!(
            lines,
            vec![
                "BEGIN 1/20",
                "DDL CREATE_TABLE t",
                "INSERT t id=1 name='o''clock'",
                "UPDATE t OLD id=1 name='a' NEW id=1 name=NULL",
                "DELETE t id=1 name=NULL",
                "COMMIT 1/20",
            ]
        );
    }

    #[test]
    fn decode_frame_skips_metadata_and_empty_frames() {
        let eng = Engine::new();
        // Slot metadata / sequence advances are invisible to the decoder.
        let recs = vec![
            WalRecord::SeqAdvance {
                name: "s".into(),
                last_value: 9,
                is_called: true,
            },
            WalRecord::ReplSlotCreate {
                name: "x".into(),
                plugin: "rustgres_decoding".into(),
                slot_type: "logical".into(),
                restart_lsn: 0,
            },
        ];
        assert!(decode_frame(&eng, 10, &recs).is_empty());
        assert!(decode_frame(&eng, 10, &[]).is_empty());
        // Unknown tables fall back to col1..colN names.
        let recs = vec![WalRecord::InsertRows {
            table: "ghost".into(),
            rows: vec![WalRow {
                id: 1,
                xmin: 2,
                values: vec![Value::Int(7)],
            }],
        }];
        assert_eq!(
            decode_frame(&eng, 0x10, &recs),
            vec!["BEGIN 0/10", "INSERT ghost col1=7", "COMMIT 0/10"]
        );
    }

    #[test]
    fn parse_start_replication_shapes() {
        let (name, lsn) =
            parse_start_replication(&["START_REPLICATION", "SLOT", "s1", "LOGICAL", "1/20"])
                .unwrap();
        assert_eq!(name, "s1");
        assert_eq!(lsn, 0x1_0000_0020);
        // "0/0" and bare numbers are accepted start positions.
        let (_, lsn) =
            parse_start_replication(&["START_REPLICATION", "SLOT", "s1", "LOGICAL", "0/0"])
                .unwrap();
        assert_eq!(lsn, 0);
        // Physical streaming is refused with a feature code.
        let e = parse_start_replication(&["START_REPLICATION", "SLOT", "s1", "PHYSICAL", "0/0"])
            .unwrap_err();
        assert_eq!(e.code, "0A000");
        let e = parse_start_replication(&["START_REPLICATION", "SLOT", "s1", "LOGICAL", "zz"])
            .unwrap_err();
        assert_eq!(e.code, "42602");
    }
}
