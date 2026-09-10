//! rustgres v0.3 — a from-scratch PostgreSQL-compatible server in pure Rust.
//!
//! Listens on 127.0.0.1:5433, one thread per connection, shared in-memory
//! database. Zero external crates: builds offline with plain `cargo build`.

mod exec;
mod protocol;
mod server;
mod sql;
mod storage;

use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use storage::Database;

fn main() {
    let listener =
        TcpListener::bind("127.0.0.1:5433").expect("failed to bind 127.0.0.1:5433");
    println!("rustgres v0.3 listening on 127.0.0.1:5433");
    let db = Arc::new(Mutex::new(Database::new()));
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
                std::thread::spawn(move || server::handle_connection(stream, db));
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }
}
