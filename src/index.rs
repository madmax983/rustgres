//! B-tree secondary indexes (v0.8).
//!
//! Design notes:
//! - One index = one ordered map `key -> [row-version ids]`. Entries are
//!   keyed by **row-version id**, never by heap position: an UPDATE
//!   appends a new version (new id, new entry) and marks the old one's
//!   `xmax`; the old entry stays until VACUUM, exactly like PostgreSQL's
//!   heap/index split. Visibility is always resolved through the version
//!   chain at scan time, so index entries need no MVCC metadata of their
//!   own — including uncommitted entries, which are invisible to everyone
//!   except their creator's snapshot.
//! - Entries are only ever *added* by DML. They are *removed* when a row
//!   version disappears entirely: ROLLBACK of an INSERT (via the write
//!   log's undo) and VACUUM of dead versions. DELETE/UPDATE never remove
//!   entries — the version chain still exists, just invisible.
//! - Canonical key ordering puts NULLs **high** (after every non-NULL),
//!   matching PostgreSQL's default btree: a forward scan yields
//!   NULLS LAST (ASC default) and a reverse scan NULLS FIRST (DESC
//!   default). Non-null comparison mirrors the executor's ORDER BY
//!   semantics; a total fallback on type tags keeps the ordering total
//!   even for values that could never share one column.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::storage::{Numeric, Value};

/// Canonical comparison for index keys: NULL sorts after every non-NULL;
/// non-NULL values compare exactly like the executor's ORDER BY.
pub fn index_key_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (x, y) if is_exact_numeric(x) && is_exact_numeric(y) => {
            match (exact_as_i64(x), exact_as_i64(y)) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => exact_numeric(x).cmp(&exact_numeric(y)),
            }
        }
        (Value::Float4(x), Value::Float4(y)) => (*x as f64).total_cmp(&(*y as f64)),
        (Value::Float4(x), Value::Float(y)) => (*x as f64).total_cmp(y),
        (Value::Float(x), Value::Float4(y)) => x.total_cmp(&(*y as f64)),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (x, y @ (Value::Float4(_) | Value::Float(_))) if is_exact_numeric(x) => {
            (exact_to_f64(x)).total_cmp(&float_val(y))
        }
        (x @ (Value::Float4(_) | Value::Float(_)), y) if is_exact_numeric(y) => {
            float_val(x).total_cmp(&exact_to_f64(y))
        }
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x.cmp(y),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        // Total fallback: values of different types can never share one
        // indexed column, but the B-tree still needs a total order.
        _ => type_tag(a).cmp(&type_tag(b)),
    }
}

fn is_exact_numeric(v: &Value) -> bool {
    matches!(
        v,
        Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) | Value::Numeric(_)
    )
}

/// The i64 value of an exact numeric, when it is a plain integer type.
/// SmallInt/Int/BigInt all fit in i64, so same- and mixed-width integer
/// comparisons reduce to one integer compare instead of NUMERIC
/// normalization (which needs i128 checked arithmetic).
fn exact_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::SmallInt(i) => Some(*i as i64),
        Value::Int(i) => Some(*i as i64),
        Value::BigInt(i) => Some(*i),
        _ => None,
    }
}

fn exact_numeric(v: &Value) -> Numeric {
    match v {
        Value::SmallInt(i) => Numeric::new(*i as i128, 0),
        Value::Int(i) => Numeric::new(*i as i128, 0),
        Value::BigInt(i) => Numeric::new(*i as i128, 0),
        Value::Numeric(n) => n.clone(),
        _ => unreachable!("exact_numeric called on non-exact value"),
    }
}

fn exact_to_f64(v: &Value) -> f64 {
    match v {
        Value::SmallInt(i) => *i as f64,
        Value::Int(i) => *i as f64,
        Value::BigInt(i) => *i as f64,
        Value::Numeric(n) => n.to_f64(),
        _ => unreachable!("exact_to_f64 called on non-exact value"),
    }
}

fn float_val(v: &Value) -> f64 {
    match v {
        Value::Float4(f) => *f as f64,
        Value::Float(f) => *f,
        _ => unreachable!("float_val called on non-float value"),
    }
}

/// Stable per-type tag for the total-order fallback.
fn type_tag(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::SmallInt(_) => 1,
        Value::Int(_) => 2,
        Value::BigInt(_) => 3,
        Value::Float4(_) => 4,
        Value::Float(_) => 5,
        Value::Numeric(_) => 6,
        Value::Text(_) => 7,
        Value::Bool(_) => 8,
        Value::Date(_) => 9,
        Value::Timestamp(_) => 10,
        Value::Timestamptz(_) => 11,
        Value::Bytea(_) => 12,
        Value::Uuid(_) => 13,
    }
}

/// One composite index key: the indexed column values in index-column
/// order. Ordered lexicographically with [`index_key_cmp`].
#[derive(Clone, Debug)]
pub struct IndexKey(pub Vec<Value>);

impl PartialEq for IndexKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for IndexKey {}

impl PartialOrd for IndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IndexKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .iter()
            .zip(other.0.iter())
            .map(|(a, b)| index_key_cmp(a, b))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or_else(|| self.0.len().cmp(&other.0.len()))
    }
}

/// DDL definition of one index. DDL is transactional: the definition
/// carries creator/deleter xids like a table version, and the planner
/// only considers definitions visible to the reading snapshot.
#[derive(Clone, Debug)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub cols: Vec<usize>,
    pub col_names: Vec<String>,
    pub unique: bool,
    pub created_xmin: u64,
    pub dropped_xmax: u64,
}

/// One live index: its definition plus the ordered key -> row-version-id
/// map.
#[derive(Clone, Debug)]
pub struct Index {
    pub def: IndexDef,
    pub tree: BTreeMap<IndexKey, Vec<u64>>,
}

impl Index {
    pub fn new(def: IndexDef) -> Self {
        Index {
            def,
            tree: BTreeMap::new(),
        }
    }

    /// Build the key for a row's values under this index's column list.
    pub fn key_for(&self, values: &[Value]) -> IndexKey {
        IndexKey(self.def.cols.iter().map(|&c| values[c].clone()).collect())
    }

    pub fn insert(&mut self, key: IndexKey, row_id: u64) {
        self.tree.entry(key).or_default().push(row_id);
    }

    /// Remove one row-version id from a key's list; drop the key when its
    /// list empties.
    pub fn remove(&mut self, key: &IndexKey, row_id: u64) {
        if let Some(ids) = self.tree.get_mut(key) {
            if let Some(pos) = ids.iter().position(|&id| id == row_id) {
                ids.swap_remove(pos);
            }
            if ids.is_empty() {
                self.tree.remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nulls_sort_high() {
        assert_eq!(
            index_key_cmp(&Value::Null, &Value::Int(1)),
            Ordering::Greater
        );
        assert_eq!(index_key_cmp(&Value::Int(1), &Value::Null), Ordering::Less);
        assert_eq!(
            index_key_cmp(&Value::Null, &Value::Null),
            Ordering::Equal
        );
    }

    #[test]
    fn mixed_exact_numerics_compare() {
        let a = Value::Int(5);
        let b = Value::Numeric(Numeric::new(50, 1));
        assert_eq!(index_key_cmp(&a, &b), Ordering::Equal);
        assert_eq!(
            index_key_cmp(&Value::SmallInt(1), &Value::BigInt(2)),
            Ordering::Less
        );
    }

    #[test]
    fn key_ord_is_lexicographic() {
        let a = IndexKey(vec![Value::Int(1), Value::Text("z".into())]);
        let b = IndexKey(vec![Value::Int(2), Value::Text("a".into())]);
        assert!(a < b);
        let c = IndexKey(vec![Value::Int(1), Value::Text("a".into())]);
        assert!(c < a);
    }
}
