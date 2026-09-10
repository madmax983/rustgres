//! rustgres v0.4 — a from-scratch PostgreSQL-compatible server in pure Rust.
//!
//! Listens on 127.0.0.1:5433, one thread per connection, shared in-memory
//! database backed by a write-ahead log. Zero external crates: builds
//! offline with plain `cargo build`.

mod exec;
mod net;
mod protocol;
mod server;
mod sql;
mod storage;
mod wal;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Where the WAL and checkpoints live. `--data-dir PATH` (or
/// `--data-dir=PATH`) wins, then `RUSTGRES_DATA_DIR`, then
/// `./rustgres-data` (created if missing).
fn data_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data-dir" {
            if let Some(val) = args.next() {
                return PathBuf::from(val);
            }
        } else if let Some(val) = arg.strip_prefix("--data-dir=") {
            return PathBuf::from(val);
        }
    }
    if let Ok(val) = std::env::var("RUSTGRES_DATA_DIR") {
        if !val.is_empty() {
            return PathBuf::from(val);
        }
    }
    PathBuf::from("./rustgres-data")
}

fn main() {
    let data_dir = data_dir();
    // Crash recovery: load the latest checkpoint, replay WAL frames after
    // it. A missing/empty data dir yields an empty database — the server
    // keeps its in-memory behavior when there is no state.
    let (db, wal) = match wal::Wal::open(&data_dir) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!(
                "recovery failed for {}: {}",
                data_dir.display(),
                e
            );
            std::process::exit(1);
        }
    };
    let listener = net::bind_listen("127.0.0.1:5433".parse().unwrap())
        .expect("failed to bind 127.0.0.1:5433");
    println!("rustgres v0.4 listening on 127.0.0.1:5433");
    let db = Arc::new(Mutex::new(db));
    let wal = Arc::new(Mutex::new(wal));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Disable Nagle: the wire protocol is a strict request/response
                // ping-pong, and Nagle + the client's delayed ACKs add ~40 ms
                // of dead latency to every round trip (measured in
                // benches/BASELINE.md).
                if let Err(e) = stream.set_nodelay(true) {
                    eprintln!("set_nodelay failed: {}", e);
                }
                let db = Arc::clone(&db);
                let wal = Arc::clone(&wal);
                std::thread::spawn(move || server::handle_connection(stream, db, wal));
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
}
