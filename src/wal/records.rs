// v1.78 mechanical split: moved verbatim from src/wal.rs (302-806).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// Logical WAL records
// ---------------------------------------------------------------------------

/// One row version for the WAL: stable id, creator xid, values.
/// (xmax is always 0 at insert time; deletes are separate records.)
///
/// v0.39: `toast` carries the per-cell toast value ids, parallel to
/// `values` (`RowVersion::toast`); `toast_meta` carries the
/// (value id, compressed) provenance for value ids *introduced* by
/// this row, so replay can rebuild `Table::toast_info`. Both are empty
/// for rows that never touched TOAST.
#[derive(Clone, Debug, PartialEq)]
pub struct WalRow {
    pub id: u64,
    pub xmin: u64,
    pub values: Row,
    pub toast: Vec<u32>,
    pub toast_meta: Vec<(u32, u8)>,
}

/// One logical change to the committed database.
#[derive(Clone, Debug, PartialEq)]
pub enum WalRecord {
    CreateTable {
        name: String,
        columns: Vec<(String, ColType)>,
        /// v0.9: s-expr encoded constraints/defaults (sql::encode_constraints).
        constraints: String,
        /// v0.11: creating role.
        owner: String,
        /// v0.11: explicit GRANT entries.
        acl: Vec<WalAcl>,
        /// v0.11: column-level GRANT entries.
        col_acl: Vec<WalColAcl>,
        /// v0.37: table OID (pg_class.oid).
        oid: u32,
        /// v0.37: toast table OID (pg_class.reltoastrelid).
        toast_relid: u32,
        /// v0.41: per-column explicit compression methods as
        /// ToastCompression codes, 0 = default (`col_compression`).
        col_compression: Vec<u8>,
        /// v0.85: named composite type per column (`composite_types`);
        /// empty on old records.
        composite_types: Vec<Option<String>>,
        /// v0.85: domain type per column (`domain_types`); empty on old
        /// records.
        domain_types: Vec<Option<String>>,
        /// v0.85: domain applies to array elements (`domain_elem`); empty
        /// on old records.
        domain_elem: Vec<bool>,
        /// v0.96: inheritance parent links (`inherits`); empty on old
        /// records.
        inherits: Vec<String>,
        /// v1.41: real PG attribute numbers per column (`attnums`) and
        /// the never-reused next-attnum counter (`next_attnum`).
        attnums: Vec<i16>,
        next_attnum: i16,
        /// v1.41: `fillfactor` storage parameter (10–100).
        fillfactor: u8,
        xmin: u64,
    },
    InsertRows {
        table: String,
        rows: Vec<WalRow>,
    },
    DropTable {
        name: String,
        xmax: u64,
    },
    DeleteRows {
        table: String,
        ids: Vec<u64>,
        /// v0.13: old row values, parallel to `ids` — what the logical
        /// decoder reports for DELETE. (Encoding changed with the
        /// `RGSWAL07` magic bump; old files are refused loudly.)
        old_rows: Vec<WalRow>,
        xmax: u64,
    },
    // --- v0.13: UPDATE as a first-class record (paired old/new values).
    // Replay applies it as delete-old + insert-new, exactly like the
    // DeleteRows+InsertRows pair it replaces; the logical decoder
    // reports it as a single UPDATE with old and new tuples.
    UpdateRows {
        table: String,
        old: Vec<WalRow>,
        new: Vec<WalRow>,
        xmax: u64,
    },
    // --- v0.8: index DDL. Index *contents* need no records: replay
    // rebuilds them from the row records (CreateIndex scans the table;
    // InsertRows maintains live indexes), and checkpoints serialize them.
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
        /// v0.9: constraint-owned backing index.
        internal: bool,
        /// v0.88: per-key-column DESC flags (parallel to `columns`).
        desc: Vec<bool>,
        /// v0.88: per-key-column NULLS FIRST flags (parallel to
        /// `columns`).
        nulls_first: Vec<bool>,
        /// v0.88: per-key-column expression source (`Some`) vs plain
        /// column (`None`); parallel to `columns`.
        exprs: Vec<Option<String>>,
        /// v0.88: partial-index predicate source, if any.
        predicate: Option<String>,
        xmin: u64,
    },
    DropIndex {
        name: String,
        xmax: u64,
    },
    // --- v0.9: ALTER TABLE swaps the table version (new columns and/or
    // constraint metadata); the row data is unchanged.
    AlterTable {
        name: String,
        columns: Vec<(String, ColType)>,
        constraints: String,
        /// True for pure-metadata alters: replay copies the old version's
        /// rows. False for ADD/DROP COLUMN: rows are rewritten and flow
        /// through InsertRows records instead.
        copy_rows: bool,
        /// v0.11: owner and ACL travel with every alter (grants and
        /// OWNER TO reuse the alter machinery).
        owner: String,
        acl: Vec<WalAcl>,
        /// v0.11: column-level GRANT entries.
        col_acl: Vec<WalColAcl>,
        /// v0.37: TOAST metadata (SET STORAGE / SET (...) alters).
        oid: u32,
        toast_relid: u32,
        toast_target: u32,
        col_storage: Vec<u8>,
        /// v0.41: per-column explicit compression methods as
        /// ToastCompression codes, 0 = default (`col_compression`).
        col_compression: Vec<u8>,
        /// v0.85: named composite type per column (`composite_types`);
        /// empty on old records.
        composite_types: Vec<Option<String>>,
        /// v0.85: domain type per column (`domain_types`); empty on old
        /// records.
        domain_types: Vec<Option<String>>,
        /// v0.85: domain applies to array elements (`domain_elem`); empty
        /// on old records.
        domain_elem: Vec<bool>,
        /// v0.96: inheritance parent links (`inherits`); empty on old
        /// records.
        inherits: Vec<String>,
        /// v1.41: real PG attribute numbers per column (`attnums`),
        /// the never-reused next-attnum counter, and `fillfactor`.
        attnums: Vec<i16>,
        next_attnum: i16,
        fillfactor: u8,
        next_value_id: u32,
        /// (value_id, compression-method-code) pairs.
        toast_info: Vec<(u32, u8)>,
        xmin: u64,
    },
    CreateView {
        name: String,
        query: String,
        col_aliases: Vec<String>,
        deps: Vec<String>,
        /// v0.11: creating role.
        owner: String,
        xmin: u64,
    },
    DropView {
        name: String,
        xmax: u64,
    },
    CreateSequence {
        name: String,
        seq: WalSequence,
        xmin: u64,
    },
    DropSequence {
        name: String,
        xmax: u64,
    },
    AlterSequence {
        name: String,
        seq: WalSequence,
        xmin: u64,
    },
    /// A committed nextval advance. Not transactional by design (like
    /// Postgres): replay always applies it.
    SeqAdvance {
        name: String,
        last_value: i64,
        is_called: bool,
    },
    // --- v0.11: roles and database ACL ---
    CreateRole {
        name: String,
        role: WalRole,
        xmin: u64,
    },
    DropRole {
        name: String,
        xmax: u64,
    },
    AlterRole {
        name: String,
        role: WalRole,
        xmin: u64,
    },
    DbAcl {
        acl: Vec<WalAcl>,
        xmin: u64,
    },
    // --- v0.13: replication slot metadata (non-transactional, like
    // SeqAdvance: replay always applies). Slots are cluster-global.
    ReplSlotCreate {
        name: String,
        plugin: String,
        slot_type: String,
        restart_lsn: u64,
    },
    ReplSlotDrop {
        name: String,
    },
    ReplSlotFlush {
        name: String,
        restart_lsn: u64,
        confirmed_flush_lsn: u64,
    },
    // --- v0.82: type DDL. `CREATE TYPE` carries the full definition so
    // replay rebuilds the catalog entry exactly; `DROP TYPE` removes it.
    CreateType {
        name: String,
        like_base: Option<String>,
        composite: Option<Vec<(String, ColType, Option<String>)>>,
        /// v0.85: domain definition (None for shell/LIKE/composite types).
        domain: Option<WalDomain>,
        xmin: u64,
    },
    DropType {
        name: String,
        xmax: u64,
    },
    // --- v0.86: function/operator DDL. CREATE carries the full
    // definition so replay rebuilds the catalog entry exactly; DROP
    // removes it. The function body travels as the raw string and is
    // re-parsed on replay.
    CreateFunction {
        name: String,
        arg_names: Vec<Option<String>>,
        arg_types: Vec<String>,
        ret_type: String,
        returns_set: bool,
        lang: u8,
        body: String,
        volatility: u8,
        strict: bool,
        xmin: u64,
    },
    DropFunction {
        name: String,
        /// v0.87: the specific overload's argument types.
        arg_types: Vec<String>,
        xmax: u64,
    },
    CreateOperator {
        name: String,
        defs: Vec<WalOperDef>,
        xmin: u64,
    },
    DropOperator {
        name: String,
        xmax: u64,
    },
    // --- v1.05: the lazy toast-table safety net's reltoastrelid link
    // (`pg_class.reltoastrelid`). Staged by
    // `WriteOp::SetToastRelid`; replay sets the link on the table's
    // live version (creating nothing — the toast table itself rides in
    // its own CreateTable record).
    SetToastRelid {
        name: String,
        toast_relid: u32,
        xmin: u64,
    },
}

/// v0.86: one operator definition in WAL/checkpoint form.
#[derive(Clone, Debug, PartialEq)]
pub struct WalOperDef {
    pub procedure: String,
    pub leftarg: Option<String>,
    pub rightarg: Option<String>,
    pub commutator: Option<String>,
    pub negator: Option<String>,
    pub hashes: bool,
    pub merges: bool,
}

/// One GRANT entry in WAL/checkpoint form (v0.11).
#[derive(Clone, Debug, PartialEq)]
pub struct WalAcl {
    pub role: String,
    pub privs: u32,
}

/// v0.85: a domain definition in WAL/checkpoint form. CHECKs and the
/// DEFAULT travel as s-expr strings (`sql::encode_domain_checks` /
/// `encode_domain_default`); the base type uses the binary `col_type`
/// codec.
#[derive(Clone, Debug, PartialEq)]
pub struct WalDomain {
    pub base: ColType,
    pub base_named: Option<String>,
    pub base_domain: Option<String>,
    pub checks: String,
    pub not_null: bool,
    pub default: String,
}

impl WalAcl {
    pub fn of(e: &crate::storage::AclEntry) -> Self {
        WalAcl {
            role: e.role.clone(),
            privs: e.privs,
        }
    }
    pub fn into_entry(self) -> crate::storage::AclEntry {
        crate::storage::AclEntry {
            role: self.role,
            privs: self.privs,
        }
    }
}

/// One column-level GRANT entry in WAL/checkpoint form (v0.11).
#[derive(Clone, Debug, PartialEq)]
pub struct WalColAcl {
    pub role: String,
    pub privs: u32,
    pub columns: Vec<String>,
}

impl WalColAcl {
    pub fn of(e: &crate::storage::ColAclEntry) -> Self {
        WalColAcl {
            role: e.role.clone(),
            privs: e.privs,
            columns: e.columns.clone(),
        }
    }
    pub fn into_entry(self) -> crate::storage::ColAclEntry {
        crate::storage::ColAclEntry {
            role: self.role,
            privs: self.privs,
            columns: self.columns,
        }
    }
}

/// A SCRAM verifier in WAL/checkpoint form (v0.11).
#[derive(Clone, Debug, PartialEq)]
pub struct WalVerifier {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
}

impl WalVerifier {
    pub fn of(v: &crate::crypto::ScramVerifier) -> Self {
        WalVerifier {
            iterations: v.iterations,
            salt: v.salt.clone(),
            stored_key: v.stored_key,
            server_key: v.server_key,
        }
    }
    pub fn into_verifier(self) -> crate::crypto::ScramVerifier {
        crate::crypto::ScramVerifier {
            iterations: self.iterations,
            salt: self.salt,
            stored_key: self.stored_key,
            server_key: self.server_key,
        }
    }
}

/// A role's durable attributes in WAL/checkpoint form (v0.11). The name
/// itself rides in the enclosing record.
#[derive(Clone, Debug, PartialEq)]
pub struct WalRole {
    pub password: Option<WalVerifier>,
    pub can_login: bool,
    pub superuser: bool,
    pub connlimit: i32,
    pub memberships: Vec<WalMembership>,
    pub valid_until: Option<String>,
}

/// WAL form of one `GRANT group TO member` edge.
#[derive(Clone, Debug, PartialEq)]
pub struct WalMembership {
    pub role: String,
    pub grantor: String,
}

impl WalRole {
    pub fn of(r: &crate::storage::Role) -> Self {
        WalRole {
            password: r.password.as_ref().map(WalVerifier::of),
            can_login: r.can_login,
            superuser: r.superuser,
            connlimit: r.connlimit,
            memberships: r
                .memberships
                .iter()
                .map(|m| WalMembership {
                    role: m.role.clone(),
                    grantor: m.grantor.clone(),
                })
                .collect(),
            valid_until: r.valid_until.clone(),
        }
    }
}

/// Plain-data form of a Sequence for WAL records and checkpoints.
#[derive(Clone, Debug, PartialEq)]
pub struct WalSequence {
    pub name: String,
    /// v0.99: sequence data type (0 = smallint, 1 = integer, 2 = bigint).
    pub seq_type: u8,
    pub start: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    pub cycle: bool,
    /// v0.98: CACHE size (Postgres default 1). Stored for catalog
    /// fidelity; the engine hands out values one at a time.
    pub cache: i64,
    /// Last value returned by nextval; i64::MIN sentinel = never called.
    pub current: i64,
    pub current_is_set: bool,
    pub is_called: bool,
    /// v0.11: owning role and explicit USAGE grants.
    pub owner: String,
    pub acl: Vec<WalAcl>,
    /// v0.65: serial ownership (table, column, temp_session).
    pub owned_by: Option<(String, String, Option<u64>)>,
}

impl WalSequence {
    pub fn of(s: &crate::storage::Sequence) -> Self {
        WalSequence {
            name: s.name.clone(),
            seq_type: match s.seq_type {
                crate::sql::SeqType::SmallInt => 0,
                crate::sql::SeqType::Integer => 1,
                crate::sql::SeqType::BigInt => 2,
            },
            start: s.start,
            increment: s.increment,
            min_value: s.min_value,
            max_value: s.max_value,
            cycle: s.cycle,
            cache: s.cache,
            current: s.current.unwrap_or(i64::MIN),
            current_is_set: s.current.is_some(),
            is_called: s.is_called,
            owner: s.owner.clone(),
            acl: s.acl.iter().map(WalAcl::of).collect(),
            owned_by: s.owned_by.clone(),
        }
    }

    pub fn into_sequence(self, created_xmin: u64) -> crate::storage::Sequence {
        crate::storage::Sequence {
            name: self.name,
            seq_type: match self.seq_type {
                0 => crate::sql::SeqType::SmallInt,
                1 => crate::sql::SeqType::Integer,
                _ => crate::sql::SeqType::BigInt,
            },
            start: self.start,
            increment: self.increment,
            min_value: self.min_value,
            max_value: self.max_value,
            cycle: self.cycle,
            cache: self.cache,
            current: if self.current_is_set {
                Some(self.current)
            } else {
                None
            },
            is_called: self.is_called,
            created_xmin,
            dropped_xmax: 0,
            owner: self.owner,
            acl: self.acl.into_iter().map(WalAcl::into_entry).collect(),
            owned_by: self.owned_by,
        }
    }
}
