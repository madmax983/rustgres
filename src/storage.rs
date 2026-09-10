//! MVCC storage for rustgres (v0.5).
//!
//! Every row version carries `xmin`/`xmax` transaction ids. A version is
//! visible to a snapshot iff its creator committed before the snapshot was
//! taken and no visible deleter has removed it:
//!
//! ```text
//! visible(v, snap, own) =
//!     (v.xmin == own || (v.xmin < snap.next_xid && v.xmin ∉ snap.active))
//!     && (v.xmax == 0 ||
//!         !(v.xmax == own || (v.xmax < snap.next_xid && v.xmax ∉ snap.active)))
//! ```
//!
//! This works because aborted transactions physically undo their writes
//! (see `WriteOp`): any xid below `next_xid` that is not in any active set
//! is therefore committed. Table existence is versioned the same way
//! (`created_xmin`/`dropped_xmax`), so DDL stays transactional without a
//! separate overlay.
//!
//! Xids come from a global atomic counter (`TxnManager::next_xid`, first
//! xid is 1). Row versions additionally carry a globally unique `id`, used
//! by the WAL to name deleted versions and by undo/vacuum to find them.

use std::collections::{HashMap, HashSet};

/// Column data types supported.
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

/// One version of one row. UPDATE = mark the old version's `xmax` and
/// append a new version; DELETE = mark `xmax`.
#[derive(Clone, Debug)]
pub struct RowVersion {
    /// Globally unique, never reused (survives crashes via the WAL).
    pub id: u64,
    pub values: Vec<Value>,
    /// Xid of the creating transaction.
    pub xmin: u64,
    /// Xid of the deleting/updating transaction; 0 = not deleted.
    pub xmax: u64,
}

/// One table version. Tables themselves are versioned so that
/// CREATE/DROP TABLE are transactional under MVCC.
#[derive(Clone, Debug)]
pub struct Table {
    pub columns: Vec<(String, ColType)>,
    pub rows: Vec<RowVersion>,
    /// Row-version id -> position in `rows`. Keeps id lookups O(1) so
    /// multi-row writes don't degrade to O(rows) per row.
    row_index: HashMap<u64, usize>,
    /// Xid of the CREATE TABLE transaction.
    pub created_xmin: u64,
    /// Xid of the DROP TABLE transaction; 0 = not dropped.
    pub dropped_xmax: u64,
}

impl Table {
    pub fn new(columns: Vec<(String, ColType)>, created_xmin: u64) -> Self {
        Table {
            columns,
            rows: Vec::new(),
            row_index: HashMap::new(),
            created_xmin,
            dropped_xmax: 0,
        }
    }

    /// Index of a column by (already lowercased) name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(n, _)| n == name)
    }

    /// Position of the row version with this id, if present (O(1)).
    pub fn row_pos(&self, id: u64) -> Option<usize> {
        let pos = self.row_index.get(&id).copied();
        debug_assert!(
            pos.map_or(true, |p| self.rows.get(p).is_some_and(|r| r.id == id)),
            "row_index out of sync with rows"
        );
        pos
    }

    /// Append a row version (O(1) amortized).
    pub fn push_version(&mut self, rv: RowVersion) {
        self.row_index.insert(rv.id, self.rows.len());
        self.rows.push(rv);
    }

    /// Remove the version at `pos` without preserving order; O(1).
    pub fn swap_remove_version(&mut self, pos: usize) -> RowVersion {
        let rv = self.rows.swap_remove(pos);
        self.row_index.remove(&rv.id);
        if pos < self.rows.len() {
            let moved_id = self.rows[pos].id;
            self.row_index.insert(moved_id, pos);
        }
        rv
    }

    /// Rebuild the id -> position index from scratch (O(rows)).
    pub fn rebuild_row_index(&mut self) {
        self.row_index.clear();
        for (i, r) in self.rows.iter().enumerate() {
            self.row_index.insert(r.id, i);
        }
    }
}

/// The whole database: table name -> versions of that table (usually one).
/// Multiple live versions of one name can only arise from concurrent
/// uncommitted CREATEs; the commit-time check in server.rs rejects the
/// second committer with 40001.
#[derive(Clone, Debug)]
pub struct Database {
    pub tables: HashMap<String, Vec<Table>>,
}

impl Database {
    pub fn new() -> Self {
        Database {
            tables: HashMap::new(),
        }
    }

    /// First table version with `name` visible to (`snap`, `own`).
    pub fn find_table(&self, name: &str, snap: &Snapshot, own: u64) -> Option<&Table> {
        self.tables
            .get(name)
            .and_then(|vs| vs.iter().find(|t| table_visible(t, snap, own)))
    }

    /// Mutable variant of [`Database::find_table`].
    pub fn find_table_mut(
        &mut self,
        name: &str,
        snap: &Snapshot,
        own: u64,
    ) -> Option<&mut Table> {
        self.tables
            .get_mut(name)
            .and_then(|vs| vs.iter_mut().find(|t| table_visible(t, snap, own)))
    }

    /// The table version created by `own` (for WAL logging of DDL).
    /// Any row version with this id, wherever it lives (ids are global).
    /// Used by undo and WAL replay; both run with the engine lock held.
    pub fn find_row_version_mut(&mut self, id: u64) -> Option<&mut RowVersion> {
        for vs in self.tables.values_mut() {
            for t in vs {
                if let Some(pos) = t.row_pos(id) {
                    return Some(&mut t.rows[pos]);
                }
            }
        }
        None
    }

    /// Immutable twin, for commit-time WAL record validation.
    pub fn find_row_version(&self, id: u64) -> Option<&RowVersion> {
        for vs in self.tables.values() {
            for t in vs {
                if let Some(pos) = t.row_pos(id) {
                    return Some(&t.rows[pos]);
                }
            }
        }
        None
    }
}

/// A transaction's consistent view: the xids active when it was taken plus
/// the next xid to be assigned. A version created by an xid below
/// `next_xid` that is absent from `active` is committed.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub active: Vec<u64>,
    pub next_xid: u64,
}

/// Global transaction state, kept beside the database inside [`Engine`].
#[derive(Clone, Debug)]
pub struct TxnManager {
    /// Next xid to hand out. Starts at 1; 0 is never a real xid.
    pub next_xid: u64,
    /// Next row-version id to hand out. Starts at 1.
    pub next_row_id: u64,
    /// Xids of currently running transactions.
    pub active: HashSet<u64>,
    /// Current snapshot per active xid (for VACUUM's dead-to-all check).
    pub snapshots: HashMap<u64, Snapshot>,
}

/// Everything shared between connections: the versioned database plus
/// the transaction manager, behind a single mutex in main.rs. One lock
/// for the whole engine keeps the locking discipline trivial (engine ->
/// wal) at the cost of serializing statements — acceptable for v0.5.
#[derive(Debug)]
pub struct Engine {
    pub db: Database,
    pub txns: TxnManager,
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            db: Database::new(),
            txns: TxnManager {
                next_xid: 1,
                next_row_id: 1,
                active: HashSet::new(),
                snapshots: HashMap::new(),
            },
        }
    }

    pub fn alloc_xid(&mut self) -> u64 {
        let xid = self.txns.next_xid;
        self.txns.next_xid += 1;
        xid
    }

    /// Start a transaction: allocate an xid and register it active.
    pub fn begin_txn(&mut self) -> u64 {
        let xid = self.alloc_xid();
        self.txns.active.insert(xid);
        xid
    }

    /// End a transaction (commit or abort): unregister the xid and drop
    /// its registered snapshot, if any.
    pub fn end_txn(&mut self, xid: u64) {
        self.txns.active.remove(&xid);
        self.txns.snapshots.remove(&xid);
    }

    /// Register a long-lived snapshot (REPEATABLE READ / SERIALIZABLE).
    /// The snapshot pins the versions it can see against VACUUM until
    /// the transaction ends.
    pub fn register_snapshot(&mut self, xid: u64, snap: Snapshot) {
        self.txns.snapshots.insert(xid, snap);
    }

    pub fn alloc_row_id(&mut self) -> u64 {
        let id = self.txns.next_row_id;
        self.txns.next_row_id += 1;
        id
    }

    /// A fresh snapshot of the currently active transactions.
    pub fn take_snapshot(&self) -> Snapshot {
        let mut active: Vec<u64> = self.txns.active.iter().copied().collect();
        active.sort_unstable();
        Snapshot {
            active,
            next_xid: self.txns.next_xid,
        }
    }

    /// Is `xid` a committed transaction from the engine's point of view?
    /// (Aborted transactions undo their writes, so anything below
    /// next_xid that is not active has committed.)
    pub fn xid_committed(&self, xid: u64) -> bool {
        xid < self.txns.next_xid && !self.txns.active.contains(&xid)
    }

    /// Physically remove dead versions from one table's versions.
    /// Returns the number of versions removed.
    pub fn vacuum_table(&mut self, name: &str) -> usize {
        let mut removed = 0;
        // Borrow the manager immutably for the dead-to-all test while the
        // table versions are borrowed mutably: the two are disjoint.
        let txns = &self.txns;
        if let Some(versions) = self.db.tables.get_mut(name) {
            for t in versions {
                let before = t.rows.len();
                t.rows.retain(|v| !version_dead_to_all(txns, v));
                let gone = before - t.rows.len();
                if gone > 0 {
                    t.rebuild_row_index();
                }
                removed += gone;
            }
        }
        removed
    }

    /// Vacuum every table. Returns (table, removed) per table touched.
    pub fn vacuum_all(&mut self) -> Vec<(String, usize)> {
        let names: Vec<String> = self.db.tables.keys().cloned().collect();
        let mut out = Vec::new();
        for name in names {
            let n = self.vacuum_table(&name);
            if n > 0 {
                out.push((name, n));
            }
        }
        out.sort();
        out
    }
}

/// Free-function twin of [`Engine::version_dead_to_all`], so callers
/// that already hold a mutable borrow of the tables (VACUUM) can test
/// deadness against an immutably borrowed manager.
pub fn version_dead_to_all(txns: &TxnManager, v: &RowVersion) -> bool {
    if v.xmax == 0 {
        return false;
    }
    if txns.active.contains(&v.xmin) || txns.active.contains(&v.xmax) {
        return false;
    }
    // xmax committed (not active, below next_xid by construction of a
    // version that exists). Dead unless some active snapshot predates
    // the deleter's commit and could still see the old version.
    for snap in txns.snapshots.values() {
        if !(v.xmax < snap.next_xid && !snap.active.contains(&v.xmax)) {
            return false;
        }
    }
    true
}

/// Row-version visibility for (`snap`, `own`).
pub fn row_visible(v: &RowVersion, snap: &Snapshot, own: u64) -> bool {
    let xmin_ok = v.xmin == own || (v.xmin < snap.next_xid && !snap.active.contains(&v.xmin));
    if !xmin_ok {
        return false;
    }
    if v.xmax == 0 {
        return true;
    }
    if v.xmax == own {
        return false;
    }
    !(v.xmax < snap.next_xid && !snap.active.contains(&v.xmax))
}

/// Table-version visibility for (`snap`, `own`).
pub fn table_visible(t: &Table, snap: &Snapshot, own: u64) -> bool {
    let created_ok = t.created_xmin == own
        || (t.created_xmin < snap.next_xid && !snap.active.contains(&t.created_xmin));
    if !created_ok {
        return false;
    }
    if t.dropped_xmax == 0 {
        return true;
    }
    if t.dropped_xmax == own {
        return false;
    }
    !(t.dropped_xmax < snap.next_xid && !snap.active.contains(&t.dropped_xmax))
}

// ---------------------------------------------------------------------------
// Write log: per-transaction undo + commit-time WAL records
// ---------------------------------------------------------------------------

/// One uncommitted change, enough to undo it (abort / ROLLBACK TO /
/// failed statement) and, at commit, to derive the WAL records.
#[derive(Clone, Debug)]
pub enum WriteOp {
    InsertRow { table: String, row_id: u64 },
    DeleteRow {
        table: String,
        row_id: u64,
        prev_xmax: u64,
    },
    CreateTable { name: String },
    DropTable { name: String, prev_xmax: u64 },
}

/// Undo a single write op. Each undo is conditional on the version still
/// being ours: a concurrent transaction may have overwritten xmax after
/// us (last-writer-wins, no row locking in v0.5), in which case their
/// op owns the version now and ours must not clobber it.
pub fn undo_write_op(eng: &mut Engine, own: u64, op: &WriteOp) {
    match op {
        WriteOp::InsertRow { table, row_id } => {
            if let Some(versions) = eng.db.tables.get_mut(table) {
                for t in versions {
                    if let Some(pos) = t.row_pos(*row_id) {
                        if t.rows[pos].xmin == own {
                            t.swap_remove_version(pos);
                        }
                        break;
                    }
                }
            }
        }
        WriteOp::DeleteRow {
            table: _,
            row_id,
            prev_xmax,
        } => {
            if let Some(v) = eng.db.find_row_version_mut(*row_id) {
                if v.xmax == own {
                    v.xmax = *prev_xmax;
                }
            }
        }
        WriteOp::CreateTable { name } => {
            if let Some(versions) = eng.db.tables.get_mut(name) {
                if let Some(pos) = versions.iter().position(|t| t.created_xmin == own) {
                    versions.swap_remove(pos);
                }
                if versions.is_empty() {
                    eng.db.tables.remove(name);
                }
            }
        }
        WriteOp::DropTable { name, prev_xmax } => {
            if let Some(versions) = eng.db.tables.get_mut(name) {
                if let Some(t) = versions.iter_mut().find(|t| t.dropped_xmax == own) {
                    t.dropped_xmax = *prev_xmax;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with_table() -> Engine {
        let mut eng = Engine::new();
        eng.db.tables.insert(
            "t".to_string(),
            vec![{
                let mut t = Table::new(vec![("a".to_string(), ColType::Int)], 1);
                t.push_version(RowVersion {
                    id: 1,
                    values: vec![Value::Int(1)],
                    xmin: 1,
                    xmax: 0,
                });
                t
            }],
        );
        eng.txns.next_xid = 10;
        eng.txns.next_row_id = 2;
        eng
    }

    fn snap(active: &[u64], next_xid: u64) -> Snapshot {
        Snapshot {
            active: active.to_vec(),
            next_xid,
        }
    }

    #[test]
    fn committed_version_visible_to_later_snapshot() {
        let eng = engine_with_table();
        let v = &eng.db.tables["t"][0].rows[0];
        // xid 1 committed (below next_xid=10, not active).
        assert!(row_visible(v, &snap(&[], 10), 99));
    }

    #[test]
    fn uncommitted_version_invisible_to_others() {
        let eng = engine_with_table();
        let mut v = eng.db.tables["t"][0].rows[0].clone();
        v.xmin = 7;
        // xid 7 still active: invisible to everyone else...
        assert!(!row_visible(&v, &snap(&[7], 10), 99));
        // ...but visible to its owner.
        assert!(row_visible(&v, &snap(&[7], 10), 7));
    }

    #[test]
    fn deleted_version_hidden_once_deleter_commits() {
        let eng = engine_with_table();
        let mut v = eng.db.tables["t"][0].rows[0].clone();
        v.xmax = 8; // deleter still active: delete not yet visible
        assert!(row_visible(&v, &snap(&[8], 10), 99));
        // Deleter committed (gone from active): version invisible.
        assert!(!row_visible(&v, &snap(&[], 10), 99));
        // But a snapshot taken while the deleter was active still sees it.
        assert!(row_visible(&v, &snap(&[8], 10), 99));
    }

    #[test]
    fn own_delete_hides_from_self() {
        let eng = engine_with_table();
        let mut v = eng.db.tables["t"][0].rows[0].clone();
        v.xmax = 7;
        assert!(!row_visible(&v, &snap(&[7], 10), 7));
    }

    #[test]
    fn version_dead_to_all_respects_snapshots() {
        let mut eng = engine_with_table();
        eng.txns.active.insert(20);
        // Snapshot taken while deleter 8 was still active.
        eng.txns.snapshots.insert(
            20,
            snap(&[8, 20], 10),
        );
        let mut v = eng.db.tables["t"][0].rows[0].clone();
        v.xmax = 8;
        // Deleter 8 has committed since, but txn 20's old snapshot could
        // still see the version: not dead.
        assert!(!version_dead_to_all(&eng.txns, &v));
        // Once txn 20 goes away, the version is dead.
        eng.txns.snapshots.remove(&20);
        eng.txns.active.remove(&20);
        assert!(version_dead_to_all(&eng.txns, &v));
    }

    #[test]
    fn vacuum_table_removes_only_dead_versions() {
        let mut eng = engine_with_table();
        // Add a dead version (deleted by committed xid 8) and a live one.
        let t = &mut eng.db.tables.get_mut("t").unwrap()[0];
        t.push_version(RowVersion {
            id: 2,
            values: vec![Value::Int(2)],
            xmin: 1,
            xmax: 8,
        });
        t.push_version(RowVersion {
            id: 3,
            values: vec![Value::Int(3)],
            xmin: 1,
            xmax: 0,
        });
        assert_eq!(eng.vacuum_table("t"), 1);
        let ids: Vec<u64> = eng.db.tables["t"][0].rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn undo_insert_removes_version() {
        let mut eng = engine_with_table();
        eng.db.tables.get_mut("t").unwrap()[0].push_version(RowVersion {
            id: 9,
            values: vec![Value::Int(9)],
            xmin: 7,
            xmax: 0,
        });
        undo_write_op(
            &mut eng,
            7,
            &WriteOp::InsertRow {
                table: "t".to_string(),
                row_id: 9,
            },
        );
        assert_eq!(eng.db.tables["t"][0].rows.len(), 1);
    }

    #[test]
    fn undo_delete_restores_xmax_only_if_still_ours() {
        let mut eng = engine_with_table();
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = 7;
        let op = WriteOp::DeleteRow {
            table: "t".to_string(),
            row_id: 1,
            prev_xmax: 0,
        };
        // Someone else overwrote xmax after us: our undo must not clobber.
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = 8;
        undo_write_op(&mut eng, 7, &op);
        assert_eq!(eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax, 8);
        // Still ours: restore.
        eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax = 7;
        undo_write_op(&mut eng, 7, &op);
        assert_eq!(eng.db.tables.get_mut("t").unwrap()[0].rows[0].xmax, 0);
    }
}
