//! In-memory table storage for rustgres.
//!
//! v0.4 adds durability: the in-memory image is backed by a write-ahead
//! log plus checkpoints (see `wal.rs`). The committed database is still
//! shared across connection threads behind a `Mutex`.

use std::collections::HashMap;

/// Column data types supported in v0.1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColType {
    Int,
    Float,
    Text,
    Bool,
}

impl ColType {
    /// PostgreSQL type OID used in RowDescription.
    pub fn oid(&self) -> i32 {
        match self {
            ColType::Int => 23,   // INT4
            ColType::Text => 25,  // TEXT
            ColType::Bool => 16,  // BOOL
            ColType::Float => 701, // FLOAT8
        }
    }

    pub fn sql_name(&self) -> &'static str {
        match self {
            ColType::Int => "integer",
            ColType::Float => "double precision",
            ColType::Text => "text",
            ColType::Bool => "boolean",
        }
    }
}

/// A single cell value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Null,
}

impl Value {
    /// Text-format encoding for the wire protocol (`None` = NULL).
    /// Matches what psql prints: ints/floats via Display, bools as t/f.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::Int(i) => Some(i.to_string()),
            Value::Float(f) => Some(float_text(*f)),
            Value::Text(s) => Some(s.clone()),
            Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
            Value::Null => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "integer",
            Value::Float(_) => "double precision",
            Value::Text(_) => "text",
            Value::Bool(_) => "boolean",
            Value::Null => "unknown",
        }
    }
}

/// Shortest round-trip float rendering, Postgres-style.
fn float_text(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    // `{}` on f64 already prints the shortest string that round-trips.
    format!("{}", f)
}

/// One table: schema + rows (each row holds one Value per column).
#[derive(Clone, Debug)]
pub struct Table {
    pub columns: Vec<(String, ColType)>,
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    /// Index of a column by (already lowercased) name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(n, _)| n == name)
    }
}

/// The whole database: tables keyed by (lowercased) name.
#[derive(Clone, Debug)]
pub struct Database {
    pub tables: HashMap<String, Table>,
}

impl Database {
    pub fn new() -> Self {
        Database {
            tables: HashMap::new(),
        }
    }
}
