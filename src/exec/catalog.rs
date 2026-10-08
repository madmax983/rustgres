// v1.78 mechanical split: moved verbatim from src/exec.rs (22621-25973).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// v0.37: pg_class (virtual). A real table by the same name takes
// precedence, like pg_stats. Exposes oid, relname, and reltoastrelid
// for TOAST introspection. OIDs are Int (rustgres has no separate OID
// type); reltoastrelid is 0 when the table has no toast table.
// ---------------------------------------------------------------------------

pub(crate) fn pg_class_schema() -> Vec<QCol> {
    [
        ("oid", ColType::Int),
        ("relname", ColType::Text),
        ("reltoastrelid", ColType::Int),
        // v1.00: relkind — 'p' for partitioned tables (PG19
        // RELKIND_PARTITIONED_TABLE), 'r' for ordinary tables and leaf
        // partitions (RELKIND_RELATION).
        ("relkind", ColType::SingleChar),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "pg_class".to_string(),
        name: n.to_string(),
        ty,

        hidden: false,
        src_ord: 0,
    })
    .collect()
}

pub(crate) fn pg_class_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_class_schema();
    let mut rows = Vec::new();
    // Sort by name for deterministic output.
    let mut names: Vec<&String> = db.tables.keys().collect();
    names.sort();
    for name in names {
        // Only the live version is visible in pg_class.
        let Some(t) = db.find_table(name, snap, &[own], session) else {
            continue;
        };
        // Skip the toast tables themselves? No — PG lists them in
        // pg_class too. Include everything.
        // v1.00: relkind — 'p' for partitioned tables, 'r' otherwise.
        // v1.42: 't' for TOAST tables (PG19 RELKIND_TOASTVALUE,
        // pg_class.h: "for out-of-line values"). Toast tables are
        // named `pg_toast.pg_toast_<oid>` (see toast_table_name).
        let relkind = if t.partition.as_ref().is_some_and(|p| p.is_partitioned) {
            Value::SingleChar(b'p')
        } else if name.starts_with("pg_toast.pg_toast_") {
            Value::SingleChar(b't')
        } else {
            Value::SingleChar(b'r')
        };
        rows.push(QRow {
            cells: Row::new(vec![
                Value::Int(t.oid as i64),
                Value::text(name.as_str()),
                Value::Int(t.toast_relid as i64),
                relkind,
            ]),
            prov: Vec::new(),
        });
    }
    // v0.95: the virtual pg_class catalog lists itself (OID 1259, like
    // PG19), unless a real table shadows it.
    if !rows
        .iter()
        .any(|r| matches!(&r.cells[1], Value::Text(s) if &**s == "pg_class"))
    {
        rows.push(QRow {
            cells: Row::new(vec![
                Value::Int(1259),
                Value::text("pg_class"),
                Value::Int(0),
                // v1.00: the virtual pg_class itself is an ordinary
                // relation ('r').
                Value::SingleChar(b'r'),
            ]),
            prov: Vec::new(),
        });
        // Keep deterministic order.
        rows.sort_by(|a, b| {
            let an = match &a.cells[1] {
                Value::Text(s) => &**s,
                _ => "",
            };
            let bn = match &b.cells[1] {
                Value::Text(s) => &**s,
                _ => "",
            };
            an.cmp(bn)
        });
    }
    (schema, rows)
}
// ---------------------------------------------------------------------------
// v0.88: pg_attribute (virtual, bounded subset). A real table by the same
// name takes precedence, like pg_stats. Exposes attrelid (the table OID),
// attname, and attnum (1-based column position) — enough for catalog
// introspection queries such as the regression suite's
// `... right join pg_attribute a on a.attrelid = ss2.oid where ...
// and attnum = 1`. This is deliberately not the full PostgreSQL
// pg_attribute (no atttypid/atttypmod/attnotnull/...); those columns
// raise "column does not exist" rather than returning wrong data.
// ---------------------------------------------------------------------------

pub(crate) fn pg_attribute_schema() -> Vec<QCol> {
    [
        ("attrelid", ColType::Int),
        ("attname", ColType::Text),
        ("attnum", ColType::Int),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "pg_attribute".to_string(),
        name: n.to_string(),
        ty,

        hidden: false,
        src_ord: 0,
    })
    .collect()
}

pub(crate) fn pg_attribute_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_attribute_schema();
    let mut rows = Vec::new();
    // Sort by table name for deterministic output.
    let mut names: Vec<&String> = db.tables.keys().collect();
    names.sort();
    for name in names {
        // Only the live version is visible in pg_attribute.
        let Some(t) = db.find_table(name, snap, &[own], session) else {
            continue;
        };
        for (i, (col_name, _)) in t.columns.iter().enumerate() {
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::Int(t.oid as i64),
                    Value::text(col_name.as_str()),
                    // v1.41: the real PG attnum (never reused after
                    // DROP), not the 1-based column position.
                    Value::Int(t.attnums.get(i).copied().unwrap_or(i as i16 + 1) as i64),
                ]),
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}
// ---------------------------------------------------------------------------
// v0.96: pg_inherits (virtual). A real table by the same name takes
// precedence, like pg_class. Exposes inhrelid (child table OID),
// inhparent (parent table OID), and inhseqno (1-based position of the
// parent link in the child's INHERITS list) — the PostgreSQL 19
// inheritance catalog. Covers both permanent tables and the current
// session's temp tables, like the real catalog.
// ---------------------------------------------------------------------------

pub(crate) fn pg_inherits_schema() -> Vec<QCol> {
    vec![
        qcol("pg_inherits", "inhrelid", ColType::Int),
        qcol("pg_inherits", "inhparent", ColType::Int),
        qcol("pg_inherits", "inhseqno", ColType::Int),
    ]
}

pub(crate) fn pg_inherits_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_inherits_schema();
    let mut rows = Vec::new();
    // Permanent tables, then this session's temp tables, each in name
    // order for deterministic output.
    let mut names: Vec<&String> = db.tables.keys().collect();
    names.sort();
    if let Some(tmps) = db.temp_tables.get(&session) {
        let mut tnames: Vec<&String> = tmps.keys().collect();
        tnames.sort();
        names.extend(tnames);
    }
    for name in names {
        let Some(t) = db.find_table(name, snap, &[own], session) else {
            continue;
        };
        if t.inherits.is_empty() {
            continue;
        }
        let child_oid = t.oid as i64;
        for (i, parent_name) in t.inherits.iter().enumerate() {
            // The parent is always visible when the child is (a
            // permanent child cannot inherit from a temp parent, and a
            // temp parent lives in this session).
            let parent_oid = db
                .find_table(parent_name, snap, &[own], session)
                .map(|p| p.oid as i64)
                .unwrap_or(0);
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::Int(child_oid),
                    Value::Int(parent_oid),
                    Value::Int((i + 1) as i64),
                ]),
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}
// ---------------------------------------------------------------------------
// v0.98: pg_sequences (virtual). A real table by the same name takes
// precedence, like pg_class. Exposes PG19's pg_sequences view columns:
// sequencename, sequenceowner, data_type, start_value, min_value,
// max_value, increment_by, cycle, cache_size, last_value.
// v0.99: the column is `cycle` (PG19); the v0.98 `cycle_option` name was
// wrong (that name belongs to information_schema.sequences).
// data_type is the stored sequence type (PG19 seqtypid), not inferred
// from bounds.
// last_value is NULL until the sequence is first called in any session,
// matching pg_sequence_last_value().
// ---------------------------------------------------------------------------

pub(crate) fn pg_sequences_schema() -> Vec<QCol> {
    [
        ("schemaname", ColType::Text),
        ("sequencename", ColType::Text),
        ("sequenceowner", ColType::Text),
        ("data_type", ColType::Text),
        ("start_value", ColType::BigInt),
        ("min_value", ColType::BigInt),
        ("max_value", ColType::BigInt),
        ("increment_by", ColType::BigInt),
        ("cycle", ColType::Bool),
        ("cache_size", ColType::BigInt),
        ("last_value", ColType::BigInt),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "pg_sequences".to_string(),
        name: n.to_string(),
        ty,
        hidden: false,
        src_ord: 0,
    })
    .collect()
}

pub(crate) fn pg_sequences_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_sequences_schema();
    let mut rows = Vec::new();
    let mut names: Vec<&String> = db.sequences.keys().collect();
    names.sort();
    for name in names {
        let Some(s) = db.sequences.get(name).and_then(|vs| {
            vs.iter()
                .find(|s| crate::storage::seq_visible(s, snap, own))
        }) else {
            continue;
        };
        // v0.99: data_type is the stored sequence type (PG19 seqtypid).
        let data_type = s.seq_type.pg_name();
        rows.push(QRow {
            cells: Row::new(vec![
                // v0.98: single-schema engine; PG19's schemaname is
                // always "public" here.
                Value::text("public"),
                Value::text(name.as_str()),
                Value::text(s.owner.as_str()),
                Value::text(data_type),
                Value::BigInt(s.start),
                Value::BigInt(s.min_value),
                Value::BigInt(s.max_value),
                Value::BigInt(s.increment),
                Value::Bool(s.cycle),
                Value::BigInt(s.cache),
                s.current.map(Value::BigInt).unwrap_or(Value::Null),
            ]),
            prov: Vec::new(),
        });
    }
    (schema, rows)
}
// ---------------------------------------------------------------------------
// v0.11: role catalogs (virtual). A real table by the same name takes
// precedence, like pg_stats. `pg_authid` masks password verifiers from
// non-superusers, like PostgreSQL.
// ---------------------------------------------------------------------------

pub(crate) fn qcol(qual: &str, name: &str, ty: ColType) -> QCol {
    QCol {
        qual: qual.to_string(),
        name: name.to_string(),
        ty,
        hidden: false,
        src_ord: 0,
    }
}

pub(crate) fn pg_authid_schema() -> Vec<QCol> {
    vec![
        qcol("pg_authid", "rolname", ColType::Text),
        qcol("pg_authid", "rolsuper", ColType::Bool),
        qcol("pg_authid", "rolinherit", ColType::Bool),
        qcol("pg_authid", "rolcreaterole", ColType::Bool),
        qcol("pg_authid", "rolcreatedb", ColType::Bool),
        qcol("pg_authid", "rolcanlogin", ColType::Bool),
        qcol("pg_authid", "rolconnlimit", ColType::Int),
        qcol("pg_authid", "rolpassword", ColType::Text),
        qcol("pg_authid", "rolvaliduntil", ColType::Timestamp),
    ]
}

pub(crate) fn pg_roles_schema() -> Vec<QCol> {
    vec![
        qcol("pg_roles", "rolname", ColType::Text),
        qcol("pg_roles", "rolsuper", ColType::Bool),
        qcol("pg_roles", "rolinherit", ColType::Bool),
        qcol("pg_roles", "rolcreaterole", ColType::Bool),
        qcol("pg_roles", "rolcreatedb", ColType::Bool),
        qcol("pg_roles", "rolcanlogin", ColType::Bool),
        qcol("pg_roles", "rolconnlimit", ColType::Int),
        qcol("pg_roles", "rolvaliduntil", ColType::Timestamp),
    ]
}

pub(crate) fn pg_user_schema() -> Vec<QCol> {
    vec![
        qcol("pg_user", "usename", ColType::Text),
        qcol("pg_user", "usesysid", ColType::Int),
        qcol("pg_user", "usecreatedb", ColType::Bool),
        qcol("pg_user", "usesuper", ColType::Bool),
        qcol("pg_user", "usecatupd", ColType::Bool),
        qcol("pg_user", "passwd", ColType::Text),
        qcol("pg_user", "valuntil", ColType::Timestamp),
        qcol("pg_user", "useconfig", ColType::Text), // text[] has no ColType; always NULL
    ]
}

/// Rows for pg_auth_members: one row per live membership edge whose
/// group role still exists. Oids are deterministic FNV-1a stand-ins
/// (see name_oid); admin_option is always false (WITH ADMIN OPTION is
/// parsed but not tracked).
pub(crate) fn pg_auth_members_rows(db: &Database, snap: &Snapshot, own: u64) -> Vec<QRow> {
    let mut names: Vec<&String> = db.roles.keys().collect();
    names.sort();
    let mut rows = Vec::new();
    for member_name in names {
        let Some(r) = db.roles[member_name]
            .iter()
            .find(|r| crate::storage::role_visible(r, snap, own))
        else {
            continue;
        };
        for m in &r.memberships {
            // Skip edges whose group role is gone (DROP ROLE cleans these
            // up; this is belt-and-braces for concurrent snapshots).
            if db.find_role(&m.role, snap, own).is_none() {
                continue;
            }
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::Int(name_oid(&m.role) as i64),
                    Value::Int(name_oid(member_name) as i64),
                    Value::Int(name_oid(&m.grantor) as i64),
                    Value::Bool(false),
                ]),
                prov: Vec::new(),
            });
        }
    }
    rows
}

/// Rows for pg_authid / pg_roles / pg_user. `kind` selects the column
/// layout. Roles are snapshot-filtered like every other catalog.
pub(crate) fn pg_auth_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    role: &str,
    kind: &str,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = match kind {
        "pg_roles" => pg_roles_schema(),
        "pg_user" => pg_user_schema(),
        "pg_auth_members" => pg_auth_members_schema(),
        _ => pg_authid_schema(),
    };
    if kind == "pg_auth_members" {
        return (schema, pg_auth_members_rows(db, snap, own));
    }
    let viewer_super = crate::storage::is_superuser_snap(db, role, snap, own);
    let mut names: Vec<&String> = db.roles.keys().collect();
    names.sort();
    let mut rows = Vec::new();
    for rn in names {
        let Some(r) = db.roles[rn]
            .iter()
            .find(|r| crate::storage::role_visible(r, snap, own))
        else {
            continue;
        };
        let valid_until = match &r.valid_until {
            None => Value::Null,
            Some(vu) => match crate::datetime::parse_timestamp(vu) {
                Ok(t) => Value::Timestamp(t),
                Err(_) => Value::Null,
            },
        };
        let passwd = match &r.password {
            None => Value::Null,
            Some(v) => {
                if viewer_super {
                    Value::text(v.encode())
                } else {
                    Value::text("********")
                }
            }
        };
        let cells = match kind {
            "pg_roles" => vec![
                Value::text(r.name.as_str()),
                Value::Bool(r.superuser),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(r.can_login),
                Value::Int(r.connlimit as i64),
                valid_until.clone(),
            ],
            "pg_user" => vec![
                Value::text(r.name.as_str()),
                // No stable oids in rustgres; hash the name for a
                // deterministic stand-in.
                Value::Int(name_oid(&r.name) as i64),
                Value::Bool(false),
                Value::Bool(r.superuser),
                Value::Bool(false),
                passwd,
                valid_until.clone(),
                Value::Null,
            ],
            _ => vec![
                Value::text(r.name.as_str()),
                Value::Bool(r.superuser),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(r.can_login),
                Value::Int(r.connlimit as i64),
                passwd,
                valid_until,
            ],
        };
        rows.push(QRow {
            cells: Row::new(cells),
            prov: Vec::new(),
        });
    }
    (schema, rows)
}

pub(crate) fn pg_auth_members_schema() -> Vec<QCol> {
    vec![
        qcol("pg_auth_members", "roleid", ColType::Int),
        qcol("pg_auth_members", "member", ColType::Int),
        qcol("pg_auth_members", "grantor", ColType::Int),
        qcol("pg_auth_members", "admin_option", ColType::Bool),
    ]
}

/// Estimated row count for the planner over virtual role catalogs.
pub(crate) fn virtual_role_catalog_rows(
    db: &Database,
    name: &str,
    snap: &Snapshot,
    own: u64,
) -> u64 {
    if name == "pg_auth_members" {
        let mut n = 0u64;
        for vs in db.roles.values() {
            if let Some(r) = vs
                .iter()
                .find(|r| crate::storage::role_visible(r, snap, own))
            {
                n += r.memberships.len() as u64;
            }
        }
        return n;
    }
    db.roles.len() as u64
}

// ---------------------------------------------------------------------------
// v0.13: pg_replication_slots (virtual). Mirrors PostgreSQL's view over the
// live slot map: slot_name, plugin, slot_type, active, restart_lsn,
// confirmed_flush_lsn. LSNs render in PostgreSQL's `pg_lsn` text form.
// A real table by the same name takes precedence, like the other virtual
// catalogs.
// ---------------------------------------------------------------------------

pub(crate) fn pg_replication_slots_schema() -> Vec<QCol> {
    vec![
        qcol("pg_replication_slots", "slot_name", ColType::Text),
        qcol("pg_replication_slots", "plugin", ColType::Text),
        qcol("pg_replication_slots", "slot_type", ColType::Text),
        qcol("pg_replication_slots", "active", ColType::Bool),
        qcol("pg_replication_slots", "restart_lsn", ColType::Text),
        qcol("pg_replication_slots", "confirmed_flush_lsn", ColType::Text),
    ]
}

pub(crate) fn pg_replication_slots_rows(eng: &Engine) -> Vec<QRow> {
    let mut names: Vec<&String> = eng.repl_slots.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let s = &eng.repl_slots[n];
            QRow {
                cells: Row::new(vec![
                    Value::text(s.name.as_str()),
                    Value::text(s.plugin.as_str()),
                    Value::text(s.slot_type.as_str()),
                    Value::Bool(s.active),
                    Value::text(crate::repl::format_lsn(s.restart_lsn)),
                    Value::text(crate::repl::format_lsn(s.confirmed_flush_lsn)),
                ]),
                prov: Vec::new(),
            }
        })
        .collect()
}

/// Deterministic stand-in oid for pg_user.usesysid (FNV-1a of the name).
pub(crate) fn name_oid(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---------------------------------------------------------------------------
// v0.6 query engine
// ---------------------------------------------------------------------------

/// One column of a query's working schema: qualifier (table alias, or the
/// table name when there is no alias; "" for no-FROM / output rows),
/// column name, and type.
#[derive(Clone, Debug)]
pub struct QCol {
    pub qual: String,
    pub name: String,
    pub ty: ColType,
    /// v0.23: hidden from `*` and unqualified name resolution, but still
    /// reachable by qualified reference and `qual.*`. Used for the
    /// preserved originals of merged `JOIN ... USING` / `NATURAL` key
    /// columns and for `USING (...) AS alias` exposure copies.
    pub hidden: bool,
    /// v0.23: position of this column within its source relation's
    /// schema. `qual.*` expands a qualifier's columns in source order
    /// (PostgreSQL), which differs from merged-join output order because
    /// the hidden key originals sort after the pass-through columns.
    pub src_ord: u32,
}

/// v0.23: indices of `schema` whose qualifier is `qual`, in the
/// qualifier's source-column order. PostgreSQL expands `qual.*` in the
/// table's own column order; for a merged `USING`/`NATURAL` join the
/// hidden key originals carry their side's source ordinals, so this
/// sorts them back into side order instead of merged output order.
pub(crate) fn qual_star_order(schema: &[QCol], qual: &str) -> Vec<usize> {
    let mut idx: Vec<usize> = schema
        .iter()
        .enumerate()
        .filter(|(_, c)| c.qual == qual)
        .map(|(i, _)| i)
        .collect();
    // Stable: columns without a meaningful ordinal (plain tables built
    // before v0.23 call sites, virtual catalogs) keep schema order.
    idx.sort_by_key(|&i| schema[i].src_ord);
    idx
}

/// v1.43: the qualifier's column values in `qual_star_order` — the flat
/// expansion of `qual.*` used by `ROW(qual.*, ...)` (PG19 expands the
/// star into the row's field list; it is not a nested whole-row value).
/// Shares the innermost-first scope walk with `eval_wholerow`, so the
/// `JOIN ... USING ... AS alias` quirk (alias exposes only merged keys)
/// is honored identically. Unknown qualifier is PG19's 42703.
pub(crate) fn wholerow_flat_values(scopes: &[Scope], qual: &str) -> Result<Vec<Value>, ExecError> {
    for sc in scopes.iter().rev() {
        let idx = qual_star_order(sc.schema, qual);
        if idx.is_empty() {
            continue;
        }
        return Ok(idx.into_iter().map(|i| sc.row[i].clone()).collect());
    }
    Err(exec_err(
        "42703",
        format!("missing FROM-clause entry for table \"{qual}\""),
    ))
}

/// v0.73: evaluate a PG19 whole-row Var (`tbl` / `tbl.*` in expression
/// position) to a composite `Value::Record`. The field order is the
/// qualifier's source-column order — exactly matching target-list
/// `tbl.*` expansion via `qual_star_order`. Hidden join-original columns
/// are skipped the same way. An unknown qualifier is PG19's 42703.
pub(crate) fn eval_wholerow(scopes: &[Scope], qual: &str) -> Result<Value, ExecError> {
    for sc in scopes.iter().rev() {
        let idx = qual_star_order(sc.schema, qual);
        if idx.is_empty() {
            continue;
        }
        let fields: Vec<(String, Value)> = idx
            .into_iter()
            .map(|i| {
                let c = &sc.schema[i];
                (c.name.clone(), sc.row[i].clone())
            })
            .collect();
        return Ok(Value::Record(fields));
    }
    Err(exec_err(
        "42703",
        format!("missing FROM-clause entry for table \"{qual}\""),
    ))
}

/// v0.73: `row_to_json(record)` (json.c): one JSON object, field names as
/// keys, values per PG's `datum_to_json` — strings quoted with JSON
/// escapes, numbers/bools bare, NULL as `null`. Nested records recurse.
/// Carried as `Value::Text` typed `ColType::Json`.
pub(crate) fn row_to_json_text(fields: &[(String, Value)]) -> String {
    fn esc(s: &str, out: &mut String) {
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
    }
    fn val(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Record(fs) => out.push_str(&row_to_json_text(fs)),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            // v0.35: bpchar renders rtrimmed in casts, but json.c uses the
            // value's output text; keep the stored text form here.
            Value::Text(s) | Value::BpChar(s) => {
                out.push('"');
                esc(s, out);
                out.push('"');
            }
            other => match other.to_text() {
                Some(t) => {
                    // Numeric-likes render bare; anything else is quoted.
                    let bare = matches!(
                        other,
                        Value::SmallInt(_)
                            | Value::Int(_)
                            | Value::BigInt(_)
                            | Value::Float4(_)
                            | Value::Float(_)
                            | Value::Numeric(_)
                    );
                    if bare {
                        out.push_str(&t);
                    } else {
                        out.push('"');
                        esc(&t, out);
                        out.push('"');
                    }
                }
                None => out.push_str("null"),
            },
        }
    }
    let mut out = String::from("{");
    for (i, (name, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        esc(name, &mut out);
        out.push_str("\":");
        val(v, &mut out);
    }
    out.push('}');
    out
}

/// v0.76: `json_array(...)` — the SQL/JSON array constructor (PG19).
/// Elements render like json.c values (numbers bare, strings quoted,
/// NULL as `null`), joined with ", " inside brackets.
pub(crate) fn json_array_text(vals: &[Value]) -> String {
    fn esc(s: &str, out: &mut String) {
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
    }
    fn val(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Text(s) | Value::BpChar(s) => {
                out.push('"');
                esc(s, out);
                out.push('"');
            }
            other => match other.to_text() {
                Some(t) => {
                    let bare = matches!(
                        other,
                        Value::SmallInt(_)
                            | Value::Int(_)
                            | Value::BigInt(_)
                            | Value::Float4(_)
                            | Value::Float(_)
                            | Value::Numeric(_)
                    );
                    if bare {
                        out.push_str(&t);
                    } else {
                        out.push('"');
                        esc(&t, out);
                        out.push('"');
                    }
                }
                None => out.push_str("null"),
            },
        }
    }
    let mut out = String::from("[");
    for (i, v) in vals.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        val(v, &mut out);
    }
    out.push(']');
    out
}

/// v1.17: one base-table row's provenance: the range qualifier (alias) at
/// scan time, the table (or partition leaf / inheritance child) holding
/// the row, and the row-version id. The qualifier lets `a.xmin` pick the
/// right row when the same table appears twice in the FROM list; the
/// table name keeps `tableoid`, `FOR UPDATE`, and `pg_column_compression`
/// working as before.
#[derive(Clone, Debug)]
pub struct RowProv {
    pub qual: String,
    pub table: String,
    pub row_id: u64,
}

/// One working row: cell values parallel to the schema, plus provenance —
/// one [`RowProv`] per contributing base-table row — used by
/// SELECT ... FOR UPDATE (and, since v1.17, by `tableoid`/`xmin`/`xmax`).
#[derive(Clone, Debug, Default)]
pub struct QRow {
    pub cells: Row,
    pub prov: Vec<RowProv>,
}

/// v0.37: does this statement (or anything nested inside it, including
/// subqueries and CTE bodies) call `pg_column_compression`? Used to
/// decide `need_prov` for the FROM scan. Descending into subqueries is a
/// safe over-approximation for the outer level (it only populates
/// provenance that may go unused) and it is *required* for correlated
/// subqueries, whose `pg_column_compression(t.c)` needs the outer
/// query's rows to carry provenance; each nested level also computes
/// its own `need_prov` from its own statement.
pub(crate) fn stmt_uses_pg_column_compression(stmt: &SelectStmt) -> bool {
    fn expr_uses(e: &Expr) -> bool {
        match e {
            Expr::Func { name, args } => {
                if name == "pg_column_compression" {
                    return true;
                }
                args.iter().any(expr_uses)
            }
            // v0.95: named args are transparent to inspection.
            Expr::NamedArg { expr, .. } => expr_uses(expr),
            // v0.79: array constructors/subscripts/slices recurse into
            // their operands.
            Expr::ArrayCtor { elems, .. } => elems.iter().any(expr_uses),
            Expr::Subscript { array, indices } => expr_uses(array) || indices.iter().any(expr_uses),
            Expr::Slice { array, bounds } => {
                expr_uses(array)
                    || bounds.iter().any(|(l, u)| {
                        l.as_deref().map(expr_uses).unwrap_or(false)
                            || u.as_deref().map(expr_uses).unwrap_or(false)
                    })
            }
            Expr::Column { .. }
            | Expr::ResolvedCol { .. }
            | Expr::WholeRow { .. }
            | Expr::Literal(_)
            | Expr::Param(_) => false,
            Expr::Arith { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Cast { expr, .. } => expr_uses(expr),
            // v0.81: row constructors, named casts and field accesses
            // recurse into their operand(s).
            Expr::CastNamed { expr, .. } => expr_uses(expr),
            Expr::Row(elems) => elems.iter().any(expr_uses),
            Expr::FieldAccess { expr, .. } => expr_uses(expr),
            Expr::Concat(a, b) => expr_uses(a) || expr_uses(b),
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => expr_uses(expr) || expr_uses(pattern) || escape.as_deref().is_some_and(expr_uses),
            // v0.68: regex match visits both operands.
            Expr::Regex { expr, pattern, .. } => expr_uses(expr) || expr_uses(pattern),
            Expr::Between {
                expr, low, high, ..
            } => expr_uses(expr) || expr_uses(low) || expr_uses(high),
            Expr::IsBool { expr, .. } => expr_uses(expr),
            Expr::Extract { from, .. } => expr_uses(from),
            Expr::Cmp { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::And(a, b) | Expr::Or(a, b) => expr_uses(a) || expr_uses(b),
            Expr::Not(e) | Expr::BitNot(e) | Expr::Neg(e) => expr_uses(e),
            // v0.55: CASE (any arm may reference the column).
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_deref().is_some_and(expr_uses)
                    || whens.iter().any(|(k, r)| expr_uses(k) || expr_uses(r))
                    || else_.as_deref().is_some_and(expr_uses)
            }
            Expr::IsNull { expr, .. } => expr_uses(expr),
            Expr::IsDistinctFrom { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Agg { arg, arg2, .. } => {
                arg.as_deref().is_some_and(expr_uses) || arg2.as_deref().is_some_and(expr_uses)
            }
            // v1.30: ordered-set aggregates — scan direct args, the
            // WITHIN GROUP sort keys, and the FILTER.
            Expr::WithinGroup {
                direct_args,
                within_order_by,
                filter,
                ..
            } => {
                direct_args.iter().any(expr_uses)
                    || within_order_by.iter().any(|o| expr_uses(&o.expr))
                    || filter.as_deref().is_some_and(expr_uses)
            }
            Expr::ScalarSub(s) => stmt_uses_pg_column_compression(s),
            Expr::ArraySubquery(s) => stmt_uses_pg_column_compression(s),
            Expr::InSub { expr, sub, .. } => {
                expr_uses(expr) || stmt_uses_pg_column_compression(sub)
            }
            // v0.87: quantified comparison and user operator.
            Expr::Quantified { left, sub, .. } => {
                expr_uses(left) || stmt_uses_pg_column_compression(sub)
            }
            Expr::UserOp { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Exists { sub, .. } => stmt_uses_pg_column_compression(sub),
            Expr::Window {
                args,
                partition_by,
                order_by,
                ..
            } => {
                args.iter().any(expr_uses)
                    || partition_by.iter().any(expr_uses)
                    || order_by.iter().any(|o| expr_uses(&o.expr))
            }
        }
    }
    stmt.items.iter().any(|it| match it {
        SelectItem::Expr { expr, .. } => expr_uses(expr),
        _ => false,
    }) || stmt.where_.as_ref().is_some_and(expr_uses)
        || stmt.group_by.iter().flatten().any(expr_uses)
        || stmt.having.as_ref().is_some_and(expr_uses)
        || stmt.order_by.iter().any(|o| expr_uses(&o.expr))
        // v0.52: DISTINCT ON expressions can call it too.
        || stmt.distinct_on.iter().any(expr_uses)
        || stmt.with.iter().any(|c| match &c.body {
            CteBody::Simple(s) => stmt_uses_pg_column_compression(s),
            CteBody::Union { left, right, .. } => {
                stmt_uses_pg_column_compression(left) || stmt_uses_pg_column_compression(right)
            }
            // v1.39: a data-modifying CTE body runs through exec_insert
            // (which handles its own RETURNING provenance); it never
            // forces provenance on the outer query.
            CteBody::Dml(_) => false,
        })
}

/// v1.13: does this statement (or anything nested inside it) reference the
/// `tableoid` system column? Used to decide `need_prov` for the FROM scan,
/// following the `stmt_uses_pg_column_compression` pattern. The `tableoid`
/// value varies per row (the OID of the partition leaf holding the row),
/// so provenance must be populated when it is referenced.
pub(crate) fn stmt_uses_tableoid(stmt: &SelectStmt) -> bool {
    fn expr_uses(e: &Expr) -> bool {
        match e {
            Expr::Column { name, .. } => name == "tableoid",
            Expr::Func { args, .. } => args.iter().any(expr_uses),
            Expr::NamedArg { expr, .. } => expr_uses(expr),
            Expr::ArrayCtor { elems, .. } => elems.iter().any(expr_uses),
            Expr::Subscript { array, indices } => expr_uses(array) || indices.iter().any(expr_uses),
            Expr::Slice { array, bounds } => {
                expr_uses(array)
                    || bounds.iter().any(|(l, u)| {
                        l.as_deref().is_some_and(expr_uses) || u.as_deref().is_some_and(expr_uses)
                    })
            }
            Expr::ResolvedCol { .. }
            | Expr::WholeRow { .. }
            | Expr::Literal(_)
            | Expr::Param(_) => false,
            Expr::Arith { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Cast { expr, .. } => expr_uses(expr),
            Expr::CastNamed { expr, .. } => expr_uses(expr),
            Expr::Row(elems) => elems.iter().any(expr_uses),
            Expr::FieldAccess { expr, .. } => expr_uses(expr),
            Expr::Concat(a, b) => expr_uses(a) || expr_uses(b),
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => expr_uses(expr) || expr_uses(pattern) || escape.as_deref().is_some_and(expr_uses),
            Expr::Regex { expr, pattern, .. } => expr_uses(expr) || expr_uses(pattern),
            Expr::Between {
                expr, low, high, ..
            } => expr_uses(expr) || expr_uses(low) || expr_uses(high),
            Expr::IsBool { expr, .. } => expr_uses(expr),
            Expr::Extract { from, .. } => expr_uses(from),
            Expr::Cmp { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::And(a, b) | Expr::Or(a, b) => expr_uses(a) || expr_uses(b),
            Expr::Not(e) | Expr::BitNot(e) | Expr::Neg(e) => expr_uses(e),
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_deref().is_some_and(expr_uses)
                    || whens.iter().any(|(k, r)| expr_uses(k) || expr_uses(r))
                    || else_.as_deref().is_some_and(expr_uses)
            }
            Expr::IsNull { expr, .. } => expr_uses(expr),
            Expr::IsDistinctFrom { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Agg { arg, arg2, .. } => {
                arg.as_deref().is_some_and(expr_uses) || arg2.as_deref().is_some_and(expr_uses)
            }
            // v1.30: ordered-set aggregates — scan direct args, the
            // WITHIN GROUP sort keys, and the FILTER.
            Expr::WithinGroup {
                direct_args,
                within_order_by,
                filter,
                ..
            } => {
                direct_args.iter().any(expr_uses)
                    || within_order_by.iter().any(|o| expr_uses(&o.expr))
                    || filter.as_deref().is_some_and(expr_uses)
            }
            Expr::ScalarSub(s) => stmt_uses_tableoid(s),
            Expr::ArraySubquery(s) => stmt_uses_tableoid(s),
            Expr::InSub { expr, sub, .. } => expr_uses(expr) || stmt_uses_tableoid(sub),
            Expr::Quantified { left, sub, .. } => expr_uses(left) || stmt_uses_tableoid(sub),
            Expr::UserOp { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Exists { sub, .. } => stmt_uses_tableoid(sub),
            Expr::Window {
                args,
                partition_by,
                order_by,
                ..
            } => {
                args.iter().any(expr_uses)
                    || partition_by.iter().any(expr_uses)
                    || order_by.iter().any(|o| expr_uses(&o.expr))
            }
        }
    }
    stmt.items.iter().any(|it| match it {
        SelectItem::Expr { expr, .. } => expr_uses(expr),
        _ => false,
    }) || stmt.where_.as_ref().is_some_and(expr_uses)
        || stmt.group_by.iter().flatten().any(expr_uses)
        || stmt.having.as_ref().is_some_and(expr_uses)
        || stmt.order_by.iter().any(|o| expr_uses(&o.expr))
        || stmt.distinct_on.iter().any(expr_uses)
        || stmt.with.iter().any(|c| match &c.body {
            CteBody::Simple(s) => stmt_uses_tableoid(s),
            CteBody::Union { left, right, .. } => {
                stmt_uses_tableoid(left) || stmt_uses_tableoid(right)
            }
            // v1.39: see above — a DML CTE body never forces outer provenance.
            CteBody::Dml(_) => false,
        })
}

/// v1.17: does this statement (or anything nested inside it) reference
/// the `xmin`/`xmax` system columns? Used to decide `need_prov` for the
/// FROM scan, following the `stmt_uses_tableoid` pattern — the values
/// vary per row (the row's MVCC version header), so provenance must be
/// populated when they are referenced.
/// v1.40: also covers `ctid`/`cmin`/`cmax`, which read the same
/// per-row provenance.
pub(crate) fn stmt_uses_xmin_xmax(stmt: &SelectStmt) -> bool {
    fn expr_uses(e: &Expr) -> bool {
        match e {
            Expr::Column { name, .. } => {
                name == "xmin"
                    || name == "xmax"
                    || name == "ctid"
                    || name == "cmin"
                    || name == "cmax"
            }
            Expr::Func { args, .. } => args.iter().any(expr_uses),
            Expr::NamedArg { expr, .. } => expr_uses(expr),
            Expr::ArrayCtor { elems, .. } => elems.iter().any(expr_uses),
            Expr::Subscript { array, indices } => expr_uses(array) || indices.iter().any(expr_uses),
            Expr::Slice { array, bounds } => {
                expr_uses(array)
                    || bounds.iter().any(|(l, u)| {
                        l.as_deref().is_some_and(expr_uses) || u.as_deref().is_some_and(expr_uses)
                    })
            }
            Expr::ResolvedCol { .. }
            | Expr::WholeRow { .. }
            | Expr::Literal(_)
            | Expr::Param(_) => false,
            Expr::Arith { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Cast { expr, .. } => expr_uses(expr),
            Expr::CastNamed { expr, .. } => expr_uses(expr),
            Expr::Row(elems) => elems.iter().any(expr_uses),
            Expr::FieldAccess { expr, .. } => expr_uses(expr),
            Expr::Concat(a, b) => expr_uses(a) || expr_uses(b),
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => expr_uses(expr) || expr_uses(pattern) || escape.as_deref().is_some_and(expr_uses),
            Expr::Regex { expr, pattern, .. } => expr_uses(expr) || expr_uses(pattern),
            Expr::Between {
                expr, low, high, ..
            } => expr_uses(expr) || expr_uses(low) || expr_uses(high),
            Expr::IsBool { expr, .. } => expr_uses(expr),
            Expr::Extract { from, .. } => expr_uses(from),
            Expr::Cmp { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::And(a, b) | Expr::Or(a, b) => expr_uses(a) || expr_uses(b),
            Expr::Not(e) | Expr::BitNot(e) | Expr::Neg(e) => expr_uses(e),
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_deref().is_some_and(expr_uses)
                    || whens.iter().any(|(k, r)| expr_uses(k) || expr_uses(r))
                    || else_.as_deref().is_some_and(expr_uses)
            }
            Expr::IsNull { expr, .. } => expr_uses(expr),
            Expr::IsDistinctFrom { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Agg { arg, arg2, .. } => {
                arg.as_deref().is_some_and(expr_uses) || arg2.as_deref().is_some_and(expr_uses)
            }
            // v1.30: ordered-set aggregates — scan direct args, the
            // WITHIN GROUP sort keys, and the FILTER.
            Expr::WithinGroup {
                direct_args,
                within_order_by,
                filter,
                ..
            } => {
                direct_args.iter().any(expr_uses)
                    || within_order_by.iter().any(|o| expr_uses(&o.expr))
                    || filter.as_deref().is_some_and(expr_uses)
            }
            Expr::ScalarSub(s) => stmt_uses_xmin_xmax(s),
            Expr::ArraySubquery(s) => stmt_uses_xmin_xmax(s),
            Expr::InSub { expr, sub, .. } => expr_uses(expr) || stmt_uses_xmin_xmax(sub),
            Expr::Quantified { left, sub, .. } => expr_uses(left) || stmt_uses_xmin_xmax(sub),
            Expr::UserOp { left, right, .. } => expr_uses(left) || expr_uses(right),
            Expr::Exists { sub, .. } => stmt_uses_xmin_xmax(sub),
            Expr::Window {
                args,
                partition_by,
                order_by,
                ..
            } => {
                args.iter().any(expr_uses)
                    || partition_by.iter().any(expr_uses)
                    || order_by.iter().any(|o| expr_uses(&o.expr))
            }
        }
    }
    stmt.items.iter().any(|it| match it {
        SelectItem::Expr { expr, .. } => expr_uses(expr),
        _ => false,
    }) || stmt.where_.as_ref().is_some_and(expr_uses)
        || stmt.group_by.iter().flatten().any(expr_uses)
        || stmt.having.as_ref().is_some_and(expr_uses)
        || stmt.order_by.iter().any(|o| expr_uses(&o.expr))
        || stmt.distinct_on.iter().any(expr_uses)
        || stmt.with.iter().any(|c| match &c.body {
            CteBody::Simple(s) => stmt_uses_xmin_xmax(s),
            CteBody::Union { left, right, .. } => {
                stmt_uses_xmin_xmax(left) || stmt_uses_xmin_xmax(right)
            }
            // v1.39: see above — a DML CTE body never forces outer provenance.
            CteBody::Dml(_) => false,
        })
}

/// One scope in the column-resolution chain: a FROM source's schema+row,
/// or an outer query's row for correlated subqueries. Scopes are searched
/// innermost-first, like Postgres.
#[derive(Clone, Copy)]
pub(crate) struct Scope<'a> {
    pub(crate) schema: &'a [QCol],
    pub(crate) row: &'a [Value],
    /// v0.37: row provenance, from QRow.prov. Used by
    /// `pg_column_compression` to find the row's toast flags, and since
    /// v1.17 by `tableoid`/`xmin`/`xmax`. None for derived/computed scopes.
    pub(crate) prov: Option<&'a [RowProv]>,
}

/// Resolve `qual.name` / `name` across the scope chain (innermost first).
/// Unqualified names fall through to outer scopes (correlation); qualified
/// names must resolve in the innermost scope that contains the qualifier.
pub(crate) fn resolve_col(
    scopes: &[Scope],
    qual: Option<&str>,
    name: &str,
) -> Result<(usize, usize), ExecError> {
    // v1.26: PG19 `scanNameSpaceForRefname` checks the range name before
    // the column: inside a LATERAL namespace, a qualifier naming two
    // visible items is 42P09 `table reference "x" is ambiguous` — even
    // when the column itself would resolve cleanly, or exists in
    // neither. References resolving strictly outside the namespace (an
    // outer query level) keep PG's shadowing rule.
    if let Some(q) = qual {
        check_lateral_table_ambiguous(scopes, q)?;
    }
    for (si, sc) in scopes.iter().enumerate().rev() {
        match qual {
            Some(q) => {
                if !sc.schema.iter().any(|c| c.qual == q) {
                    continue; // qualifier not in this scope: try outward
                }
                let mut found = None;
                for (ci, c) in sc.schema.iter().enumerate() {
                    if c.qual == q && c.name == name {
                        if found.is_some() {
                            return Err(exec_err(
                                "42702",
                                format!("column reference \"{}.{}\" is ambiguous", q, name),
                            ));
                        }
                        found = Some(ci);
                    }
                }
                return found
                    .map(|ci| (si, ci))
                    .ok_or_else(|| exec_err("42703", format!("column {q}.{name} does not exist")));
            }
            None => {
                let mut found = None;
                for (ci, c) in sc.schema.iter().enumerate() {
                    // v0.23: hidden columns (preserved USING/NATURAL key
                    // originals, USING-alias copies) are invisible to
                    // unqualified references and `*`; they resolve only
                    // through a qualified reference or `qual.*`.
                    if c.hidden {
                        continue;
                    }
                    if c.name == name {
                        if found.is_some() {
                            return Err(exec_err(
                                "42702",
                                format!("column reference \"{}\" is ambiguous", name),
                            ));
                        }
                        found = Some(ci);
                    }
                }
                if let Some(ci) = found {
                    return Ok((si, ci));
                }
                // No match here: an outer scope may provide it (correlation).
            }
        }
    }
    Err(exec_err(
        "42703",
        format!("column \"{}\" does not exist", name),
    ))
}

/// v1.26: PG19 `scanNameSpaceForRefname` — inside a LATERAL namespace, a
/// qualified reference must resolve to exactly one namespace item; two
/// items sharing the qualifier is 42P09 `table reference "x" is
/// ambiguous`. Only the innermost live marker applies (PG resolves at
/// the nearest query level holding a match). PG checks the range name
/// before the column, so the qualifier need not resolve to any column.
/// A qualifier resolving strictly before the marker's base belongs to an
/// outer query level and keeps PG's shadowing rule. A marker whose base
/// lies past the end of a rebuilt shorter scope chain is stale and
/// ignored.
pub(crate) fn check_lateral_table_ambiguous(scopes: &[Scope], qual: &str) -> Result<(), ExecError> {
    let marker = LATERAL_NS.with(|s| s.borrow().last().copied());
    let Some((ns_base, _)) = marker else {
        return Ok(());
    };
    if ns_base > scopes.len() {
        return Ok(());
    }
    // Innermost scope carrying the qualifier; none means the 42703 path
    // below handles it (PG: missing FROM-clause entry — a pre-existing
    // separate divergence, not this check's business).
    let Some(si) = scopes
        .iter()
        .rposition(|sc| sc.schema.iter().any(|c| c.qual == qual))
    else {
        return Ok(());
    };
    if si < ns_base {
        return Ok(());
    }
    // PG checks the range (table) name, not the column: two namespace
    // items sharing the qualifier are ambiguous even when only one of
    // them carries the referenced column.
    let mut hits = 0;
    for sc in &scopes[ns_base..] {
        if sc.schema.iter().any(|c| c.qual == qual) {
            hits += 1;
            if hits >= 2 {
                return Err(exec_err(
                    "42P09",
                    format!("table reference \"{qual}\" is ambiguous"),
                ));
            }
        }
    }
    Ok(())
}

/// Pre-resolve every column reference at this query level of `pred` into a
/// positional `ResolvedCol`, for one fixed scope shape (`schemas`,
/// outermost first). Callgrind showed per-pair name resolution
/// (`resolve_col` + string compares) costing ~20% of all instructions in a
/// join workload — the schemas don't change across the loop, so resolving
/// once up front removes it from the hot path.
///
/// Subqueries are their own query level and are left untouched: their inner
/// references resolve at runtime, still able to correlate against the live
/// outer frames. Errors are identical to per-row evaluation because the
/// same `resolve_col` does the work — against prototype scopes with empty
/// rows, since resolution only inspects schemas. Like per-row evaluation,
/// callers must only resolve when the loop would actually evaluate the
/// predicate (non-empty inputs), so error timing is unchanged.
pub(crate) fn resolve_predicate_columns(
    pred: &Expr,
    schemas: &[&[QCol]],
) -> Result<Expr, ExecError> {
    // Prototype scopes: resolve_col never touches `row`, only `schema`
    // (expr_type already uses this trick).
    let scopes: Vec<Scope> = schemas
        .iter()
        .map(|s| Scope {
            schema: s,
            row: &[],
            prov: None,
        })
        .collect();
    resolve_predicate_columns_in(pred, &scopes)
}

pub(crate) fn resolve_predicate_columns_in(
    pred: &Expr,
    scopes: &[Scope],
) -> Result<Expr, ExecError> {
    let r = |e: &Expr| resolve_predicate_columns_in(e, scopes);
    match pred {
        Expr::Column { table, name } => {
            match resolve_col(scopes, table.as_deref(), name) {
                Ok((frame, idx)) => Ok(Expr::ResolvedCol { frame, idx }),
                Err(e) => {
                    // v0.73: PG19 whole-row fallback at plan time (see
                    // eval_expr): a bare range name becomes a whole-row Var.
                    if table.is_none()
                        && e.code == "42703"
                        && scopes
                            .iter()
                            .any(|sc| sc.schema.iter().any(|c| c.qual == *name))
                    {
                        return Ok(Expr::WholeRow { qual: name.clone() });
                    }
                    Err(e)
                }
            }
        }
        // v0.73: whole-row refs validate the qualifier at plan time
        // (PG19 42703), then resolve per row at runtime.
        Expr::WholeRow { qual } => {
            if !scopes
                .iter()
                .any(|sc| sc.schema.iter().any(|c| c.qual == *qual))
            {
                return Err(exec_err(
                    "42703",
                    format!("missing FROM-clause entry for table \"{qual}\""),
                ));
            }
            Ok(pred.clone())
        }
        // Leaves and already-resolved nodes pass through (idempotent).
        Expr::ResolvedCol { .. } | Expr::Literal(_) | Expr::Param(_) => Ok(pred.clone()),
        // Separate query levels: runtime resolution (correlation intact).
        Expr::ScalarSub(_) | Expr::ArraySubquery(_) | Expr::InSub { .. } | Expr::Exists { .. } => {
            Ok(pred.clone())
        }
        // v0.87: quantified comparison — resolve the outer `left`, leave
        // the subquery for runtime resolution (like InSub).
        Expr::Quantified {
            left,
            op,
            quant,
            sub,
        } => Ok(Expr::Quantified {
            left: Box::new(r(left)?),
            op: op.clone(),
            quant: *quant,
            sub: sub.clone(),
        }),
        Expr::UserOp { op, left, right } => Ok(Expr::UserOp {
            op: op.clone(),
            left: Box::new(r(left)?),
            right: Box::new(r(right)?),
        }),
        Expr::Arith { op, left, right } => Ok(Expr::Arith {
            op: *op,
            left: Box::new(r(left)?),
            right: Box::new(r(right)?),
        }),
        Expr::Concat(a, b) => Ok(Expr::Concat(Box::new(r(a)?), Box::new(r(b)?))),
        // v0.79: array expressions rebuild with resolved operands.
        Expr::ArrayCtor { elems, nested } => Ok(Expr::ArrayCtor {
            elems: elems.iter().map(r).collect::<Result<Vec<_>, _>>()?,
            nested: *nested,
        }),
        Expr::Subscript { array, indices } => Ok(Expr::Subscript {
            array: Box::new(r(array)?),
            indices: indices.iter().map(r).collect::<Result<Vec<_>, _>>()?,
        }),
        Expr::Slice { array, bounds } => Ok(Expr::Slice {
            array: Box::new(r(array)?),
            bounds: bounds
                .iter()
                .map(|(l, u)| {
                    Ok((
                        l.as_deref().map(r).transpose()?.map(Box::new),
                        u.as_deref().map(r).transpose()?.map(Box::new),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
        }),
        Expr::Cast { expr, to, written } => Ok(Expr::Cast {
            expr: Box::new(r(expr)?),
            to: *to,
            written: written.clone(),
        }),
        // v0.81: named casts, row constructors and field accesses
        // rebuild with resolved operands.
        Expr::CastNamed { expr, name } => Ok(Expr::CastNamed {
            expr: Box::new(r(expr)?),
            name: name.clone(),
        }),
        Expr::Row(elems) => Ok(Expr::Row(
            elems.iter().map(r).collect::<Result<Vec<_>, _>>()?,
        )),
        Expr::FieldAccess { expr, field } => Ok(Expr::FieldAccess {
            expr: Box::new(r(expr)?),
            field: field.clone(),
        }),
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => Ok(Expr::Like {
            expr: Box::new(r(expr)?),
            pattern: Box::new(r(pattern)?),
            not: *not,
            ilike: *ilike,
            escape: escape.as_ref().map(|e| r(e)).transpose()?.map(Box::new),
        }),
        // v0.68: regex match rewrite.
        Expr::Regex {
            expr,
            pattern,
            not,
            case_insensitive,
        } => Ok(Expr::Regex {
            expr: Box::new(r(expr)?),
            pattern: Box::new(r(pattern)?),
            not: *not,
            case_insensitive: *case_insensitive,
        }),
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => Ok(Expr::Between {
            expr: Box::new(r(expr)?),
            low: Box::new(r(low)?),
            high: Box::new(r(high)?),
            neg: *neg,
        }),
        Expr::IsBool { expr, neg, val } => Ok(Expr::IsBool {
            expr: Box::new(r(expr)?),
            neg: *neg,
            val: *val,
        }),
        Expr::Func { name, args } => Ok(Expr::Func {
            name: name.clone(),
            args: args.iter().map(r).collect::<Result<Vec<_>, _>>()?,
        }),
        // v0.95: named args are transparent to column resolution.
        Expr::NamedArg { name, expr } => Ok(Expr::NamedArg {
            name: name.clone(),
            expr: Box::new(r(expr)?),
        }),
        Expr::Extract { field, from } => Ok(Expr::Extract {
            field: field.clone(),
            from: Box::new(r(from)?),
        }),
        Expr::Cmp { op, left, right } => Ok(Expr::Cmp {
            op: *op,
            left: Box::new(r(left)?),
            right: Box::new(r(right)?),
        }),
        Expr::And(a, b) => Ok(Expr::And(Box::new(r(a)?), Box::new(r(b)?))),
        Expr::Or(a, b) => Ok(Expr::Or(Box::new(r(a)?), Box::new(r(b)?))),
        Expr::Not(x) => Ok(Expr::Not(Box::new(r(x)?))),
        Expr::BitNot(x) => Ok(Expr::BitNot(Box::new(r(x)?))),
        Expr::Neg(x) => Ok(Expr::Neg(Box::new(r(x)?))),
        // v0.55: resolve columns inside every CASE arm.
        Expr::Case {
            operand,
            whens,
            else_,
        } => Ok(Expr::Case {
            operand: operand.as_deref().map(r).transpose()?.map(Box::new),
            whens: whens
                .iter()
                .map(|(k, v)| Ok((Box::new(r(k)?), Box::new(r(v)?))))
                .collect::<Result<Vec<_>, _>>()?,
            else_: else_.as_deref().map(r).transpose()?.map(Box::new),
        }),
        Expr::IsNull { expr, neg } => Ok(Expr::IsNull {
            expr: Box::new(r(expr)?),
            neg: *neg,
        }),
        Expr::IsDistinctFrom { left, right, neg } => Ok(Expr::IsDistinctFrom {
            left: Box::new(r(left)?),
            right: Box::new(r(right)?),
            neg: *neg,
        }),
        // Can't occur in a JOIN ON (rejected by validation), but resolve
        // the argument rather than choke if one ever arrives.
        Expr::Agg {
            func,
            arg,
            distinct,
            arg2,
            agg_order_by,
            filter,
        } => Ok(Expr::Agg {
            func: *func,
            arg: arg.as_ref().map(|a| r(a).map(Box::new)).transpose()?,
            distinct: *distinct,
            arg2: arg2.as_ref().map(|a| r(a).map(Box::new)).transpose()?,
            // v0.92: resolve columns in the in-aggregate ORDER BY.
            agg_order_by: agg_order_by
                .iter()
                .map(|o| {
                    Ok(OrderTerm {
                        expr: r(&o.expr)?,
                        desc: o.desc,
                        nulls_first: o.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            // v1.29: resolve columns in FILTER (same row scope as the
            // aggregate arguments).
            filter: filter.as_ref().map(|f| r(f).map(Box::new)).transpose()?,
        }),
        // v1.30: resolve columns in an ordered-set aggregate — the
        // direct args, the WITHIN GROUP sort keys, and the FILTER
        // (all in the same row scope as ordinary aggregate args).
        Expr::WithinGroup {
            func,
            direct_args,
            within_order_by,
            filter,
        } => Ok(Expr::WithinGroup {
            func: *func,
            direct_args: direct_args
                .iter()
                .map(|a| r(a))
                .collect::<Result<Vec<_>, _>>()?,
            within_order_by: within_order_by
                .iter()
                .map(|o| {
                    Ok(OrderTerm {
                        expr: r(&o.expr)?,
                        desc: o.desc,
                        nulls_first: o.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            filter: filter.as_ref().map(|f| r(f).map(Box::new)).transpose()?,
        }),
        // v0.10: resolve columns inside window inputs.
        Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            wid,
            filter,
            exclusion,
        } => Ok(Expr::Window {
            func: func.clone(),
            args: args.iter().map(|a| r(a)).collect::<Result<Vec<_>, _>>()?,
            distinct: *distinct,
            partition_by: partition_by
                .iter()
                .map(|p| r(p))
                .collect::<Result<Vec<_>, _>>()?,
            order_by: order_by
                .iter()
                .map(|o| {
                    Ok(OrderTerm {
                        expr: r(&o.expr)?,
                        desc: o.desc,
                        nulls_first: o.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            frame: frame.clone(),
            wid: *wid,
            // v1.29: resolve columns in FILTER (windowed aggregates only).
            filter: filter.as_ref().map(|f| r(f).map(Box::new)).transpose()?,
            // v1.31: exclusion is a plain enum — nothing to resolve.
            exclusion: exclusion.clone(),
        }),
    }
}

/// Per-query evaluation state threaded through the v0.6 engine.
pub(crate) struct Q<'a, 'b> {
    pub(crate) eng: &'a mut Engine,
    pub(crate) snap: &'b Snapshot,
    pub(crate) own: u64,
    /// v1.21: every xid owned by the transaction (top + sub-xids), for
    /// visibility. Cloned from the StmtCtx (tiny vec).
    pub(crate) all_xids: Vec<u64>,
    /// v0.9: server-assigned session id, for session-local `currval`.
    pub(crate) session: u64,
    /// Subquery nesting depth (0 = top level).
    pub(crate) depth: usize,
    /// Sink for (table, row-version id) pairs named by FOR UPDATE, at any
    /// query level. The top-level `execute` acquires them all at once.
    pub(crate) lock_ids: &'a mut Vec<(String, u64)>,
    /// v0.10: materialized CTE bindings visible at this query level
    /// (innermost last). Shared by reference-counting so subqueries
    /// inherit them cheaply.
    pub(crate) ctes: Vec<Rc<CteBinding>>,
    /// v0.10: active window-function evaluation context. Set by the
    /// window pre-pass before projection; `Expr::Window` evaluates by
    /// looking up `values[wid][row]`.
    pub(crate) wctx: Option<WindowCtx>,
    /// v1.27: active SRF fan-out values for PG19 ProjectSet-over-Agg.
    /// `project_group_expanded` sets this per fanned row to the
    /// (SRF call, current value) pairs; `eval_grouped`'s `Expr::Func`
    /// arm returns the bound value instead of raising 42883. Empty
    /// outside the fan-out. Saved/restored across subquery boundaries
    /// (a subquery is its own query level).
    pub(crate) srf_vals: Vec<(Expr, Value)>,
    /// v0.11: acting role, for privilege checks during scans and
    /// sequence-function evaluation.
    pub(crate) role: &'a str,
    /// v0.17: session is read-only — nextval/setval fail with 25006.
    pub(crate) read_only: bool,
    /// v0.11: qualifier -> real table name per query level (innermost
    /// last), for the column-privilege pre-pass. `None` = not a base
    /// table (view/CTE/derived); those are checked at their own level.
    pub(crate) priv_scopes: Vec<Vec<(String, Option<String>)>>,
    /// v0.80: hashed EXISTS subplan cache, shared across nested query
    /// levels by Rc (like CTE bindings): (table, column) -> materialized
    /// inner key set. Built lazily on first probe; the statement
    /// snapshot is fixed so one build serves every row.
    pub(crate) hashed_exists: Rc<RefCell<HashMap<(String, String), Rc<HashedExists>>>>,
    /// v0.80: uncorrelated IN-subquery result cache, shared the same
    /// way: previously seen subqueries and their materialized outputs.
    /// Keyed by the `SelectStmt`'s address rather than AST equality
    /// (Bolt, 2026-09-29): `sub: &SelectStmt` is always the same borrowed
    /// AST node — part of the statement's own WHERE-clause tree, walked
    /// by reference on every row, never cloned per row — for the entire
    /// lifetime of one statement's execution (the only scope this cache
    /// is ever shared across, via `Rc::clone`, never persisted past it).
    /// A pointer compare is equivalent to the AST-equality compare it
    /// replaces for that lookup and avoids re-walking the whole subquery
    /// tree on every probed row.
    pub(crate) hashed_in: Rc<RefCell<Vec<(*const SelectStmt, Rc<HashedIn>)>>>,
    /// v1.36: IMMUTABLE SQL-function result cache (PG19
    /// `evaluate_function`, optimizer/util/clauses.c: constant-folding
    /// calls whose arguments are all row-constant and whose body is
    /// provably row-independent and side-effect-free). Keyed by
    /// (function name, canonical argument values); shared across nested
    /// query levels by Rc exactly like `hashed_exists`. The statement
    /// snapshot is fixed, so one body execution serves every identical
    /// call in the statement. Nothing is cached on error; anything not
    /// provably fold-safe fails open to the per-row path.
    pub(crate) immutable_fn_cache: Rc<RefCell<HashMap<ImmutableFnKey, Value>>>,
    /// v1.37: planner-time Const substitution for IMMUTABLE SQL-function
    /// calls (PG19 `eval_const_expressions`/`evaluate_function`): a
    /// per-statement call-site memo keyed by the `Expr::Func` node's
    /// address. Once a fold-eligible node evaluates, it behaves as a
    /// Const for the rest of the statement — later evaluations skip
    /// argument evaluation, the eligibility walk, and the value-keyed
    /// cache entirely. Sound because a statement-AST node's scope
    /// chain, visible CTE set, and statement snapshot are fixed for the
    /// whole execution, so the first evaluation's eligibility verdict
    /// holds for every later one. Pointer keys are never dereferenced,
    /// only compared/hashed (the v1.29 `hashed_in` precedent). Fresh
    /// per statement entry, cloned across nested query levels;
    /// `run_func_body` installs a FRESH map (never the shared one) —
    /// body ASTs are cloned per body execution, so a shared map could
    /// see a dangling pointer from a dropped clone alias a later
    /// clone's node.
    pub(crate) plan_fold_memo: Rc<RefCell<HashMap<*const Expr, Value>>>,
    /// v0.89: statement-local UPDATE overlay — (destination table,
    /// row-version id, new cell values) for rows already processed by
    /// the in-flight UPDATE. Only *volatile* SQL function bodies see
    /// it (PG19: a volatile function observes the statement's own
    /// earlier row updates, while stable/immutable ones see the
    /// statement snapshot). Plain subqueries and stable/immutable
    /// function bodies always get `None`. The overlay is a plan, not
    /// a mutation: nothing is written to storage until the whole
    /// UPDATE succeeds, so statement atomicity is preserved.
    pub(crate) pending_updates: Option<Rc<RefCell<Vec<(String, u64, Row)>>>>,
    /// v1.33: statement write context for DML inside SQL function
    /// bodies (see `QWrite`). Forwarded through every nested query
    /// level of the same statement; `None` only for
    /// statement-detached evaluations (partition-key probes, column
    /// defaults, constraint checks). A SQL function with a DML body
    /// called from such a level fails with an honest 0A000.
    pub(crate) write: Option<QWrite<'a>>,
}

/// v1.36: cache key for the IMMUTABLE SQL-function result cache:
/// (function name, canonical argument values). The raw `Value` has no
/// `Eq + Hash`, so arguments are encoded canonically: floats by bit
/// pattern (`-0.0` vs `0.0` and NaN payloads, which `PartialEq` and
/// text rendering conflate, hash distinctly).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ImmutableFnKey {
    pub(crate) name: String,
    pub(crate) args: Vec<FnKeyVal>,
}

/// v1.36: canonical, hashable encoding of one argument `Value`.
/// The `match` in `fn_key_val` is exhaustive, so adding a variant to
/// `Value` without extending this enum is a compile error — a new
/// variant can never silently alias an old one's key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FnKeyVal {
    SmallInt(i16),
    Int(i64),
    BigInt(i64),
    Float4(u32),
    Float(u64),
    Numeric {
        unscaled: i128,
        scale: i32,
        dscale: i32,
        special: u8,
        big: Option<Box<crate::storage::BigUint>>,
    },
    Text(String),
    BpChar(String),
    SingleChar(u8),
    Bool(bool),
    Date(i32),
    Timestamp(i64),
    Timestamptz(i64),
    Bytea(Vec<u8>),
    // v1.39: bit length matters (trailing bits are padding).
    BitString {
        bitlen: u32,
        bytes: Vec<u8>,
    },
    Uuid([u8; 16]),
    PgLsn(u64),
    Tid(u32, u32), // v1.40
    Record(Vec<(String, FnKeyVal)>),
    Array {
        elem: u8,
        dims: Vec<i32>,
        lower: Vec<i32>,
        elems: Vec<FnKeyVal>,
    },
    Null,
}

/// v1.36: encode one argument value into its canonical cache-key form.
pub(crate) fn fn_key_val(v: &Value) -> FnKeyVal {
    match v {
        Value::SmallInt(x) => FnKeyVal::SmallInt(*x),
        Value::Int(x) => FnKeyVal::Int(*x),
        Value::BigInt(x) => FnKeyVal::BigInt(*x),
        Value::Float4(x) => FnKeyVal::Float4(x.to_bits()),
        Value::Float(x) => FnKeyVal::Float(x.to_bits()),
        Value::Numeric(n) => FnKeyVal::Numeric {
            unscaled: n.unscaled,
            scale: n.scale,
            dscale: n.dscale,
            special: match n.special {
                NumericSpecial::Finite => 0,
                NumericSpecial::NaN => 1,
                NumericSpecial::PosInf => 2,
                NumericSpecial::NegInf => 3,
            },
            big: n.big.clone(),
        },
        Value::Text(s) => FnKeyVal::Text(s.to_string()),
        Value::BpChar(s) => FnKeyVal::BpChar(s.to_string()),
        Value::SingleChar(c) => FnKeyVal::SingleChar(*c),
        Value::Bool(b) => FnKeyVal::Bool(*b),
        Value::Date(d) => FnKeyVal::Date(*d),
        Value::Timestamp(t) => FnKeyVal::Timestamp(*t),
        Value::Timestamptz(t) => FnKeyVal::Timestamptz(*t),
        Value::Bytea(b) => FnKeyVal::Bytea(b.clone()),
        // v1.39
        Value::BitString(b) => FnKeyVal::BitString {
            bitlen: b.bitlen,
            bytes: b.bytes.clone(),
        },
        Value::Uuid(u) => FnKeyVal::Uuid(*u),
        Value::PgLsn(l) => FnKeyVal::PgLsn(*l),
        Value::Tid(b, o) => FnKeyVal::Tid(*b, *o), // v1.40
        Value::Record(fields) => FnKeyVal::Record(
            fields
                .iter()
                .map(|(n, fv)| (n.clone(), fn_key_val(fv)))
                .collect(),
        ),
        Value::Array(a) => FnKeyVal::Array {
            elem: a.elem as u8,
            dims: a.dims.clone(),
            lower: a.lower.clone(),
            elems: a.elems.iter().map(fn_key_val).collect(),
        },
        Value::Null => FnKeyVal::Null,
    }
}

/// v1.36: behaviorally-volatile builtins in this engine: repeated
/// evaluation with identical arguments can return different values.
/// Hoisted from `expr_is_volatile` (v1.22) so the IMMUTABLE-fold body
/// scan shares the single canonical list; the nested definition there
/// now calls this (no behavior change).
pub(crate) fn volatile_builtin(name: &str) -> bool {
    matches!(
        name,
        "nextval"
            | "currval"
            | "setval"
            | "lastval"
            | "random"
            | "setseed"
            | "now"
            | "current_timestamp"
            | "clock_timestamp"
            | "statement_timestamp"
            | "transaction_timestamp"
            | "current_date"
    )
}

/// v1.54: PG19-volatility for CTE inlining (`SS_process_ctes` /
/// `contain_volatile_functions`, subselect.c). PG's catalog marks
/// `now()`, `current_timestamp`, `statement_timestamp`,
/// `transaction_timestamp`, and `current_date` STABLE — they do NOT
/// block inlining (corpus target `stable-inline`). Only true
/// volatiles (`random`, `nextval`, ..., `clock_timestamp`) do. This is
/// deliberately narrower than rustgres's `volatile_builtin` (which
/// also blocks IMMUTABLE const-folding); do not unify them.
pub(crate) fn pg_volatile_builtin(name: &str) -> bool {
    matches!(
        name,
        "nextval" | "currval" | "setval" | "lastval" | "random" | "setseed" | "clock_timestamp"
    )
}

/// v1.54: does this statement contain a PG-volatile function call
/// anywhere (items, quals, FROM, CTE bodies, subqueries)? Gate for
/// CTE inlining — PG's `contain_volatile_functions` check.
pub(crate) fn stmt_has_pg_volatile(s: &SelectStmt) -> bool {
    let mut found = false;
    walk_stmt_exprs(s, &mut |e| {
        if found {
            return;
        }
        if let Expr::Func { name, .. } = e {
            if pg_volatile_builtin(name) {
                found = true;
            }
        }
    });
    found
}

/// v1.54: does this statement contain a data-modifying CTE anywhere?
/// Gate for CTE inlining — PG's `contain_dml` check.
pub(crate) fn stmt_has_dml(s: &SelectStmt) -> bool {
    fn fi_has_dml(fi: &FromItem) -> bool {
        match fi {
            FromItem::Derived { sub, .. } => stmt_has_dml(sub),
            FromItem::Join { left, right, .. } => fi_has_dml(left) || fi_has_dml(right),
            FromItem::Table { .. } | FromItem::Values { .. } | FromItem::Function { .. } => false,
        }
    }
    s.with.iter().any(|c| match &c.body {
        CteBody::Simple(b) => stmt_has_dml(b),
        CteBody::Union { left, right, .. } => stmt_has_dml(left) || stmt_has_dml(right),
        CteBody::Dml(_) => true,
    }) || s.from.iter().any(fi_has_dml)
        || s.set_op.as_ref().is_some_and(|op| {
            stmt_has_dml(&op.left) || op.chain.iter().any(|b| stmt_has_dml(&b.right))
        })
}

/// v1.54: can this CTE be inlined by `inline_cte_refs`? Mirrors PG19's
/// `inline_cte` gates (subselect.c:1139): not recursive, not
/// `AS MATERIALIZED`, simple SELECT body, no DML, no PG-volatile
/// functions. The single-reference requirement is checked separately
/// (`count_cte_refs`); multi-ref CTEs never inline here — stricter
/// than PG's `AS NOT MATERIALIZED` exception, untestable in the
/// corpus (test 7's oracle is unmatchable anyway).
pub(crate) fn cte_inlineable(cte: &CteDef) -> bool {
    // `WITH RECURSIVE` marks every CTE recursive (parser) — PG's
    // `SS_process_ctes` skips those.
    if cte.recursive {
        return false;
    }
    if cte.materialized == CteMaterialize::Materialized {
        return false;
    }
    let body = match &cte.body {
        CteBody::Simple(b) => b,
        _ => return false,
    };
    if stmt_has_dml(body) {
        return false;
    }
    if stmt_has_pg_volatile(body) {
        return false;
    }
    true
}

/// v1.54: scope-aware CTE reference visitor for inlining. Calls
/// `visit` for every `FromItem::Table{name}` with whether it binds to
/// the target CTE. `target_idx` is `Some(i)` when the target is
/// `stmt.with[i]` at the current level, `None` when it comes from an
/// outer level. Shadowing follows PG's `ctelevelsup` rule: an inner
/// `WITH name` rebinds the name (and a target is only visible in
/// sibling CTE bodies defined AFTER it).
pub(crate) fn visit_cte_refs(
    stmt: &SelectStmt,
    target_name: &str,
    target_idx: Option<usize>,
    sees_target: bool,
    visit: &mut impl FnMut(bool),
) {
    // Outer-level target shadowed by this level's WITH.
    let sees = if target_idx.is_none() && stmt.with.iter().any(|c| c.name == target_name) {
        false
    } else {
        sees_target
    };
    for fi in &stmt.from {
        visit_cte_refs_from(fi, target_name, target_idx, sees, visit);
    }
    for (j, cte) in stmt.with.iter().enumerate() {
        let s = match target_idx {
            Some(i) => sees && j > i,
            None => sees,
        };
        match &cte.body {
            CteBody::Simple(body) => visit_cte_refs(body, target_name, None, s, visit),
            CteBody::Union { left, right, .. } => {
                visit_cte_refs(left, target_name, None, s, visit);
                visit_cte_refs(right, target_name, None, s, visit);
            }
            CteBody::Dml(_) => {}
        }
    }
    if let Some(op) = &stmt.set_op {
        visit_cte_refs(&op.left, target_name, target_idx, sees, visit);
        for b in &op.chain {
            visit_cte_refs(&b.right, target_name, target_idx, sees, visit);
        }
    }
}

pub(crate) fn visit_cte_refs_from(
    fi: &FromItem,
    target_name: &str,
    target_idx: Option<usize>,
    sees_target: bool,
    visit: &mut impl FnMut(bool),
) {
    match fi {
        FromItem::Table { name, .. } => visit(sees_target && name == target_name),
        FromItem::Derived { sub, .. } => visit_cte_refs(sub, target_name, None, sees_target, visit),
        FromItem::Join { left, right, .. } => {
            visit_cte_refs_from(left, target_name, target_idx, sees_target, visit);
            visit_cte_refs_from(right, target_name, target_idx, sees_target, visit);
        }
        FromItem::Values { .. } | FromItem::Function { .. } => {}
    }
}

/// v1.54: count `FromItem::Table{target_name}` refs in `stmt`'s subtree
/// that bind to `stmt.with[target_idx]`.
pub(crate) fn count_cte_refs(stmt: &SelectStmt, target_name: &str, target_idx: usize) -> usize {
    let mut n = 0;
    visit_cte_refs(stmt, target_name, Some(target_idx), true, &mut |binds| {
        if binds {
            n += 1;
        }
    });
    n
}

/// v1.54: replace the `FromItem::Table` refs binding to the target CTE
/// with a clone of `replacement`. Mirrors `visit_cte_refs`'s scope
/// logic. Returns the number replaced.
pub(crate) fn replace_cte_refs(
    stmt: &mut SelectStmt,
    target_name: &str,
    target_idx: Option<usize>,
    sees_target: bool,
    replacement: &FromItem,
) -> usize {
    let sees = if target_idx.is_none() && stmt.with.iter().any(|c| c.name == target_name) {
        false
    } else {
        sees_target
    };
    let mut n = 0;
    for fi in &mut stmt.from {
        n += replace_cte_refs_from(fi, target_name, target_idx, sees, replacement);
    }
    for (j, cte) in stmt.with.iter_mut().enumerate() {
        let s = match target_idx {
            Some(i) => sees && j > i,
            None => sees,
        };
        match &mut cte.body {
            CteBody::Simple(body) => n += replace_cte_refs(body, target_name, None, s, replacement),
            CteBody::Union { left, right, .. } => {
                n += replace_cte_refs(left, target_name, None, s, replacement);
                n += replace_cte_refs(right, target_name, None, s, replacement);
            }
            CteBody::Dml(_) => {}
        }
    }
    if let Some(op) = &mut stmt.set_op {
        n += replace_cte_refs(&mut op.left, target_name, target_idx, sees, replacement);
        for b in &mut op.chain {
            n += replace_cte_refs(&mut b.right, target_name, target_idx, sees, replacement);
        }
    }
    n
}

pub(crate) fn replace_cte_refs_from(
    fi: &mut FromItem,
    target_name: &str,
    target_idx: Option<usize>,
    sees_target: bool,
    replacement: &FromItem,
) -> usize {
    match fi {
        FromItem::Table { name, .. } => {
            if sees_target && name == target_name {
                *fi = replacement.clone();
                1
            } else {
                0
            }
        }
        FromItem::Derived { sub, .. } => {
            replace_cte_refs(sub, target_name, None, sees_target, replacement)
        }
        FromItem::Join { left, right, .. } => {
            replace_cte_refs_from(left, target_name, target_idx, sees_target, replacement)
                + replace_cte_refs_from(right, target_name, target_idx, sees_target, replacement)
        }
        FromItem::Values { .. } | FromItem::Function { .. } => 0,
    }
}

/// v1.54: PG19 `SS_process_ctes` (subselect.c) as an AST rewrite —
/// inline simple non-recursive CTEs BEFORE `pull_up_simple_subqueries`
/// (PG's planner order). A CTE inlines iff `cte_inlineable` holds and
/// exactly one `FromItem::Table` binds to it; the ref becomes
/// `FromItem::Derived{ body }` and the CTE leaves `with`, so the
/// existing pullup flattens it. Runs top-down (this level first, then
/// recurses into subqueries and CTE bodies), to a fixpoint at each
/// level (each inlining shrinks `with`, so it terminates).
pub(crate) fn inline_cte_refs(stmt: &SelectStmt) -> SelectStmt {
    let mut out = stmt.clone();
    loop {
        let mut progressed = false;
        let mut i = 0;
        while i < out.with.len() {
            let cte = out.with[i].clone();
            if !cte_inlineable(&cte) {
                i += 1;
                continue;
            }
            if count_cte_refs(&out, &cte.name, i) != 1 {
                i += 1;
                continue;
            }
            let body = match &cte.body {
                CteBody::Simple(b) => b.clone(),
                _ => {
                    i += 1;
                    continue;
                }
            };
            let replacement = FromItem::Derived {
                sub: Box::new(body),
                alias: cte.name.clone(),
                col_aliases: cte.col_aliases.clone(),
                lateral: false,
            };
            let replaced = replace_cte_refs(&mut out, &cte.name, Some(i), true, &replacement);
            if replaced != 1 {
                // Count/replace disagree — never inline (safety).
                i += 1;
                continue;
            }
            out.with.remove(i);
            progressed = true;
            break;
        }
        if !progressed {
            break;
        }
    }
    for fi in &mut out.from {
        inline_cte_refs_from(fi);
    }
    for cte in &mut out.with {
        match &mut cte.body {
            CteBody::Simple(body) => *body = inline_cte_refs(body),
            CteBody::Union { left, right, .. } => {
                **left = inline_cte_refs(left);
                **right = inline_cte_refs(right);
            }
            CteBody::Dml(_) => {}
        }
    }
    if let Some(op) = &mut out.set_op {
        *op.left = inline_cte_refs(&op.left);
        for b in &mut op.chain {
            *b.right = inline_cte_refs(&b.right);
        }
    }
    out
}

pub(crate) fn inline_cte_refs_from(fi: &mut FromItem) {
    match fi {
        FromItem::Derived { sub, .. } => **sub = inline_cte_refs(sub),
        FromItem::Join { left, right, .. } => {
            inline_cte_refs_from(left);
            inline_cte_refs_from(right);
        }
        FromItem::Table { .. } | FromItem::Values { .. } | FromItem::Function { .. } => {}
    }
}

/// v1.36: does this expression read per-row query state? Column and
/// whole-row references, already-resolved column positions,
/// parameters, subqueries (which may correlate), aggregates, and
/// window functions all vary row to row (or level to level), so an
/// argument expression mentioning any of them is not a constant for
/// IMMUTABLE folding — PG19's `evaluate_function` only folds when
/// every argument is a Const node.
pub(crate) fn expr_mentions_row_input(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |sub| {
        if found {
            return;
        }
        match sub {
            Expr::Column { .. }
            | Expr::ResolvedCol { .. }
            | Expr::WholeRow { .. }
            | Expr::Param(_)
            | Expr::ScalarSub(_)
            | Expr::ArraySubquery(_)
            | Expr::InSub { .. }
            | Expr::Exists { .. }
            | Expr::Quantified { .. }
            | Expr::Agg { .. }
            | Expr::WithinGroup { .. }
            | Expr::Window { .. } => {
                found = true;
            }
            _ => {}
        }
    });
    found
}

/// v1.36: PG19 `evaluate_function` folds a call only when every
/// argument is a Const node. The engine's looser-but-sound analogue:
/// no per-row input and no volatile subexpression anywhere inside (a
/// STABLE call of constants is statement-constant by contract, hence
/// foldable; VOLATILE never is).
pub(crate) fn expr_is_row_const(db: &Database, e: &Expr) -> bool {
    !expr_mentions_row_input(e) && !expr_is_volatile(db, e)
}

/// v1.36: visit every expression in a SELECT statement, including the
/// positions `walk_select` skips (FROM items, CTE bodies, set-operation
/// branches): the IMMUTABLE-fold body scan must see column references
/// wherever they hide.
pub(crate) fn walk_stmt_exprs(s: &SelectStmt, f: &mut impl FnMut(&Expr)) {
    for cte in &s.with {
        match &cte.body {
            CteBody::Simple(body) => walk_stmt_exprs(body, f),
            CteBody::Union { left, right, .. } => {
                walk_stmt_exprs(left, f);
                walk_stmt_exprs(right, f);
            }
            // v1.39: a data-modifying CTE body is a separate statement
            // (run via execute_inner when materialized); the fold scan
            // does not descend into it (fail-open, per the fn docs).
            CteBody::Dml(_) => {}
        }
    }
    for d in &s.distinct_on {
        walk_expr(d, f);
    }
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk_expr(expr, f);
        }
    }
    for fi in &s.from {
        walk_from_item_exprs(fi, f);
    }
    if let Some(w) = &s.where_ {
        walk_expr(w, f);
    }
    for g in s.group_by.iter().flatten() {
        walk_expr(g, f);
    }
    if let Some(h) = &s.having {
        walk_expr(h, f);
    }
    for t in &s.order_by {
        walk_expr(&t.expr, f);
    }
    if let Some(op) = &s.set_op {
        walk_stmt_exprs(&op.left, f);
        for b in &op.chain {
            walk_stmt_exprs(&b.right, f);
        }
        for t in &op.order_by {
            walk_expr(&t.expr, f);
        }
    }
    // LIMIT / OFFSET are plain `Option<i64>` — no expressions to visit.
}

/// v1.36: FROM-item half of `walk_stmt_exprs`.
pub(crate) fn walk_from_item_exprs(fi: &FromItem, f: &mut impl FnMut(&Expr)) {
    match fi {
        FromItem::Table { .. } => {}
        FromItem::Derived { sub, .. } => walk_stmt_exprs(sub, f),
        FromItem::Values { rows, .. } => {
            for r in rows {
                for e in r {
                    walk_expr(e, f);
                }
            }
        }
        FromItem::Function { args, .. } => {
            for a in args {
                walk_expr(a, f);
            }
        }
        FromItem::Join {
            left, right, on, ..
        } => {
            walk_from_item_exprs(left, f);
            walk_from_item_exprs(right, f);
            if let Some(o) = on {
                walk_expr(o, f);
            }
        }
    }
}

/// v1.37: visit every FROM item of one SELECT statement: this level's
/// FROM list, CTE bodies, and set-operation branches. Scalar-subquery
/// expressions are NOT descended into — `select_foldable` recurses into
/// those explicitly, so each level's FROM items are checked exactly
/// once per level.
pub(crate) fn walk_stmt_from_items(s: &SelectStmt, f: &mut impl FnMut(&FromItem)) {
    fn walk_fi(fi: &FromItem, f: &mut impl FnMut(&FromItem)) {
        f(fi);
        match fi {
            FromItem::Derived { sub, .. } => walk_stmt_from_items(sub, f),
            FromItem::Join { left, right, .. } => {
                walk_fi(left, f);
                walk_fi(right, f);
            }
            FromItem::Table { .. } | FromItem::Values { .. } | FromItem::Function { .. } => {}
        }
    }
    for cte in &s.with {
        match &cte.body {
            CteBody::Simple(body) => walk_stmt_from_items(body, f),
            CteBody::Union { left, right, .. } => {
                walk_stmt_from_items(left, f);
                walk_stmt_from_items(right, f);
            }
            // v1.39: see above — the DML body is a separate statement.
            CteBody::Dml(_) => {}
        }
    }
    for fi in &s.from {
        walk_fi(fi, f);
    }
    if let Some(op) = &s.set_op {
        walk_stmt_from_items(&op.left, f);
        for b in &op.chain {
            walk_stmt_from_items(&b.right, f);
        }
    }
}

/// v1.37: does this SELECT (one body level; subqueries get their own
/// `select_foldable` call) name a relation that a caller-visible CTE
/// shadows? FROM resolves CTE-first (CTE, then information_schema,
/// then view, then table — see `subplan_inner_is_base_table`), so such
/// a name reads caller state, which can differ per call site within
/// one statement (a caller CTE shadowing a catalog table name). The
/// v1.36 value cache is keyed by (name, arg values) and shared across
/// call sites, so folding here would leak one site's value into
/// another's — fail open. Body-local CTEs and catalog tables/views
/// never match the caller's CTE list and keep folding.
pub(crate) fn select_refs_caller_cte(sel: &SelectStmt, ctes: &[Rc<CteBinding>]) -> bool {
    let mut found = false;
    walk_stmt_from_items(sel, &mut |fi| {
        if found {
            return;
        }
        if let FromItem::Table { name, .. } = fi {
            if ctes.iter().any(|c| c.name == *name) {
                found = true;
            }
        }
    });
    found
}

/// v1.36: can calls to this SQL function be constant-folded within the
/// current statement (PG19 `evaluate_function`,
/// optimizer/util/clauses.c:5205)? Every condition must hold; anything
/// else fails open to today's per-row execution.
///
/// * SQL language, IMMUTABLE volatility, scalar result, parsed body
///   present, no plpgsql body. (STABLE folds only in PG's estimation
///   mode — never in normal planning — and VOLATILE never folds.)
/// * Every body statement is a SELECT. DML bodies stay per-row: their
///   write-log effects are row-count-dependent, and PG19 performs no
///   volatility-vs-body validation at CREATE either, so failing open
///   is the faithful mirror.
/// * No volatile calls anywhere in the body — no volatile builtin, no
///   set-returning builtin in scalar position, no VOLATILE-marked user
///   function (checked transitively, cycle-guarded) — so the body is
///   deterministic under the statement's fixed snapshot.
/// * No reference to the caller's query level: every `Column` is
///   probed with `resolve_col` against the call-site scopes and every
///   `WholeRow` qualifier against the scopes' schemas; resolving there
///   names caller row state, which varies per row. (rustgres lets SQL
///   bodies see caller scopes; PG19 rejects such bodies at CREATE, so
///   PG has no equivalent case.)
/// * Every call-site argument is row-constant (`expr_is_row_const`):
///   PG folds only all-Const argument lists.
/// * v1.37: no body relation name is shadowed by a caller-visible CTE
///   (`select_refs_caller_cte`): FROM resolves CTE-first, so such a
///   name reads caller state that can differ per call site — the
///   per-statement cache would leak one site's value into another's.
pub(crate) fn immutable_fold_eligible(
    db: &Database,
    scopes: &[Scope],
    args: &[Expr],
    fdef: &crate::storage::FuncDef,
    ctes: &[Rc<CteBinding>],
) -> bool {
    if fdef.lang != crate::sql::FuncLang::Sql
        || fdef.volatility != crate::sql::FuncVolatility::Immutable
        || fdef.returns_set
        || fdef.plpgsql.is_some()
    {
        return false;
    }
    let body = match fdef.parsed.as_ref() {
        Some(b) => b,
        None => return false,
    };
    if !args.iter().all(|a| expr_is_row_const(db, a)) {
        return false;
    }
    let mut visiting: Vec<(String, Vec<String>)> = Vec::new();
    sql_body_foldable(db, scopes, fdef, body, &mut visiting, 0, ctes)
}

/// v1.36: body half of `immutable_fold_eligible`, recursed into
/// IMMUTABLE/STABLE callees. `visiting` guards direct and mutual
/// recursion (a recursive body is never folded); `depth` bounds
/// pathological catalog chains.
pub(crate) fn sql_body_foldable(
    db: &Database,
    scopes: &[Scope],
    fdef: &crate::storage::FuncDef,
    body: &[Stmt],
    visiting: &mut Vec<(String, Vec<String>)>,
    depth: usize,
    ctes: &[Rc<CteBinding>],
) -> bool {
    if depth > 8 {
        return false;
    }
    let id = (fdef.name.clone(), fdef.arg_types.clone());
    if visiting.contains(&id) {
        return false;
    }
    visiting.push(id);
    let ok = body.iter().all(|s| match s {
        Stmt::Select(sel) => select_foldable(db, scopes, sel, visiting, depth, ctes),
        // v1.36: DML bodies never fold (see `immutable_fold_eligible`).
        _ => false,
    });
    visiting.pop();
    ok
}

/// v1.36: one SELECT statement of a fold-candidate body: every
/// expression must be free of volatile calls and caller-level
/// references. User-defined operators (and the quantified/array-ANY
/// forms that resolve one at runtime) are rejected outright — their
/// procedure's volatility is invisible to static inspection.
/// Subqueries get the full statement check recursively
/// (`walk_expr`'s `walk_select` descent skips their FROM items, CTEs,
/// and set-operation branches, so the explicit recursion here — not
/// the incidental descent — is what covers them; re-visits are
/// harmless to the monotonic flag).
pub(crate) fn select_foldable(
    db: &Database,
    scopes: &[Scope],
    sel: &SelectStmt,
    visiting: &mut Vec<(String, Vec<String>)>,
    depth: usize,
    ctes: &[Rc<CteBinding>],
) -> bool {
    // v1.37: a body relation name shadowed by a caller-visible CTE
    // reads per-call-site caller state — never fold (fail open).
    if select_refs_caller_cte(sel, ctes) {
        return false;
    }
    let mut ok = true;
    walk_stmt_exprs(sel, &mut |e| {
        if !ok {
            return;
        }
        match e {
            Expr::Func { name, args } => {
                if volatile_builtin(name) || is_builtin_srf(name) {
                    ok = false;
                    return;
                }
                // v1.36: the hidden `__any_all_array` builtin carries
                // its operator as a text argument; a `user:`-prefixed
                // operator resolves its (possibly VOLATILE) procedure
                // at runtime, invisibly to static inspection.
                if name.as_str() == "__any_all_array"
                    && args.iter().any(
                        |a| matches!(a, Expr::Literal(Literal::Text(s)) if s.starts_with("user:")),
                    )
                {
                    ok = false;
                    return;
                }
                if let Some(overloads) = db.functions.get(name) {
                    for cand in overloads {
                        if cand.arg_types.len() != args.len() {
                            continue;
                        }
                        // A VOLATILE callee makes the body
                        // non-deterministic; anything else must itself
                        // be fold-safe (transitive check, cycle-guarded).
                        // Unknown names/arities fail deterministic
                        // catalog errors (42883) identically on every
                        // call, so they are fold-safe.
                        if cand.volatility == crate::sql::FuncVolatility::Volatile {
                            ok = false;
                            return;
                        }
                        let cbody = match cand.parsed.as_ref() {
                            Some(b) => b,
                            None => continue,
                        };
                        if cand.lang != crate::sql::FuncLang::Sql
                            || cand.returns_set
                            || cand.plpgsql.is_some()
                            || !sql_body_foldable(
                                db,
                                scopes,
                                cand,
                                cbody,
                                visiting,
                                depth + 1,
                                ctes,
                            )
                        {
                            ok = false;
                            return;
                        }
                    }
                }
            }
            Expr::ScalarSub(sub) | Expr::ArraySubquery(sub) | Expr::Exists { sub, .. } => {
                if !select_foldable(db, scopes, sub, visiting, depth, ctes) {
                    ok = false;
                }
            }
            Expr::InSub { sub, .. } => {
                if !select_foldable(db, scopes, sub, visiting, depth, ctes) {
                    ok = false;
                }
            }
            Expr::Quantified { op, sub, .. } => {
                // v1.36: a user-defined quantified operator resolves
                // its (possibly VOLATILE) procedure at runtime.
                if matches!(op, QuantOp::User(_)) {
                    ok = false;
                    return;
                }
                if !select_foldable(db, scopes, sub, visiting, depth, ctes) {
                    ok = false;
                }
            }
            // v1.36: user-defined operators resolve their procedure
            // from the operand types at runtime (`eval_user_op`), so a
            // VOLATILE procedure is invisible to static inspection —
            // fail open.
            Expr::UserOp { .. } => {
                ok = false;
            }
            Expr::Column { table, name } => {
                // Resolving against the call-site scopes means this
                // reference names caller row state (correlation); the
                // body is not row-independent. Anything else — the
                // body's own FROM, or a deterministic 42703 — is fine.
                if resolve_col(scopes, table.as_deref(), name).is_ok() {
                    ok = false;
                }
            }
            Expr::WholeRow { qual } => {
                if scopes
                    .iter()
                    .any(|sc| sc.schema.iter().any(|c| c.qual == *qual))
                {
                    ok = false;
                }
            }
            // `ResolvedCol` never occurs in stored parses (it is built
            // per execution); seeing one means this AST did not come
            // from the catalog — refuse to reason about it.
            Expr::ResolvedCol { .. } => {
                ok = false;
            }
            // `Param` is the function's own argument (replaced by
            // `subst_params` before the body runs); aggregates, window
            // calls, and operators are walked into node by node above.
            _ => {}
        }
    });
    ok
}

/// v1.33: statement write context, threaded through `Q` so SQL
/// function bodies can execute DML. PG19 `fmgr_sql`
/// (executor/functions.c) runs *every* body statement — including
/// INSERT/UPDATE/DELETE — in the caller's transaction, but the body
/// executor only carries `&mut Q`, so the statement's write log and
/// write-path parameters ride along on `Q`. The `&mut` reborrow —
/// never `Rc<RefCell>` — keeps nested function DML (a DML-bodied
/// function called while evaluating another function body's DML
/// statement) sound: reborrows nest linearly, so write order is
/// preserved and no runtime borrow panic is possible.
pub(crate) struct QWrite<'a> {
    pub(crate) writes: &'a mut Vec<WriteOp>,
    pub(crate) write_xid: u64,
    pub(crate) level: crate::sql::IsolationLevel,
    pub(crate) default_toast_compression: crate::storage::ToastCompression,
}

impl<'a> QWrite<'a> {
    /// Reborrow for a child query level.
    pub(crate) fn reborrow(&mut self) -> QWrite<'_> {
        QWrite {
            writes: &mut *self.writes,
            write_xid: self.write_xid,
            level: self.level,
            default_toast_compression: self.default_toast_compression,
        }
    }
}

/// v1.33: build the statement write context from the statement
/// context (statement entry points). Takes the individual fields —
/// NOT `&mut StmtCtx` — so the mutable borrow covers only
/// `ctx.writes` and the caller's other `ctx` fields (`own`,
/// `session`, `role`, ...) stay usable while the `Q` is alive.
pub(crate) fn qwrite_from_ctx<'a>(
    writes: &'a mut Vec<WriteOp>,
    write_xid: u64,
    level: IsolationLevel,
    default_toast_compression: crate::storage::ToastCompression,
) -> QWrite<'a> {
    QWrite {
        writes,
        write_xid,
        level,
        default_toast_compression,
    }
}

/// v0.10: a materialized Common Table Expression: name, output schema and
/// rows. Recursive CTEs hold the fixpoint result.
#[derive(Clone, Debug)]
pub(crate) struct CteBinding {
    pub(crate) name: String,
    pub(crate) schema: Vec<QCol>,
    pub(crate) rows: Vec<QRow>,
}

/// v0.10: window-function evaluation state for one query level.
/// `values[wid]` holds one value per input row (non-agg path) or per
/// group (agg path); `row` is the input row / group currently being
/// projected.
#[derive(Clone, Debug, Default)]
pub(crate) struct WindowCtx {
    pub(crate) values: Vec<Vec<Value>>,
    pub(crate) row: usize,
}

pub(crate) struct SelectOut {
    pub(crate) columns: Vec<(String, ColType)>,
    pub(crate) rows: Vec<Row>,
}

/// One projected output row mid-pipeline: projected cells, provenance for
/// FOR UPDATE, and (only when an ORDER BY may need it) the full
/// pre-projection cells.
pub(crate) struct OutRow {
    pub(crate) cells: Row,
    pub(crate) prov: Vec<RowProv>,
    pub(crate) full: Option<Row>,
    /// Precomputed ORDER BY keys. Aggregated queries fill this in during
    /// exec_agg, where group-level ORDER BY expressions (aggregates, GROUP
    /// BY columns not in the select list) can still be evaluated; plain
    /// queries compute keys later in apply_order.
    pub(crate) sort_keys: Option<Vec<Value>>,
    /// v0.10: pre-projection input row index, for ORDER BY terms that
    /// contain window functions (set only when the query uses windows).
    pub(crate) win_idx: Option<usize>,
    /// v1.28: per-row SRF bindings from plain-path ProjectSet fan-out,
    /// for ORDER BY terms that name an SRF call textually (PG19 evaluates
    /// ORDER BY after the ProjectSet). Empty for non-fanned rows.
    pub(crate) srf: Vec<(Expr, Value)>,
}

/// v0.10: `WITH [RECURSIVE] ...` — materialize every CTE of this query
/// level (in definition order, so later CTEs see earlier ones), run the
/// inner query, then pop the bindings. The bindings live on the shared
/// query context, so nested subqueries, derived tables and views see them
/// too; sibling and outer levels are unaffected after the pop.
/// v0.95: PG19 degenerate grouping — a HAVING clause with no GROUP BY
/// and no aggregates anywhere. The FROM/WHERE is not evaluated (PG19
/// planner's `is_degenerate_grouping`); the query produces a single
/// group iff HAVING is true.
pub(crate) fn is_degenerate_grouping(stmt: &SelectStmt) -> bool {
    if stmt.having.is_none() || !stmt.group_by.is_empty() {
        return false;
    }
    let has_agg = stmt
        .items
        .iter()
        .any(|i| matches!(i, SelectItem::Expr { expr, .. } if contains_agg(expr)))
        || stmt.order_by.iter().any(|o| contains_agg(&o.expr))
        || stmt.distinct_on.iter().any(|e| contains_agg(e))
        || stmt.having.as_ref().is_some_and(contains_agg);
    !has_agg
}

pub(crate) fn run_select(
    q: &mut Q,
    stmt: &SelectStmt,
    outer: &[Scope],
) -> Result<SelectOut, ExecError> {
    // v1.22: plan-time CASE constant folding (PG19 `eval_const_expressions`).
    // Runs before anything else at each query level; subqueries and CTE
    // bodies get their own call via recursion. Gated on `contains_case`
    // so queries without CASE pay only a single cheap walk.
    if contains_case(stmt) {
        const_fold_check(q, stmt)?;
    }
    let base = q.ctes.len();
    if !stmt.with.is_empty() {
        materialize_ctes(q, &stmt.with)?;
    }
    // v0.44: set operations. The carrier's WITH is materialized above and
    // visible to every branch; each branch runs as its own query level
    // with its own privilege checks.
    if stmt.set_op.is_some() {
        let out = eval_set_op(q, stmt, outer);
        q.ctes.truncate(base);
        q.wctx = None;
        return out;
    }
    // v0.11: column privileges. Push this level's qualifier map, check
    // every column this level reads, then pop (balanced across view
    // expansion, which reuses `q`).
    q.priv_scopes.push(priv_scope_for(q, stmt));
    let chk = check_select_col_privs(q, stmt);
    q.priv_scopes.pop();
    chk?;
    let out = run_select_inner(q, stmt, outer);
    q.ctes.truncate(base);
    // v0.10: the window context is per query level; never leak it.
    q.wctx = None;
    out
}

/// v0.44: evaluate a set-operation (`UNION` / `INTERSECT` / `EXCEPT`)
/// carrier. The left branch is evaluated, then the chain is folded
/// left-to-right (equal-precedence operators are left-associative;
/// tighter `INTERSECT`s were nested during parsing). Column names come
/// from the leftmost branch; types are resolved per column and every
/// branch's rows are coerced to them. The root `ORDER BY` / `OFFSET` /
/// `LIMIT` apply to the combined result.
pub(crate) fn eval_set_op(
    q: &mut Q,
    stmt: &SelectStmt,
    outer: &[Scope],
) -> Result<SelectOut, ExecError> {
    let root = stmt
        .set_op
        .as_ref()
        .ok_or_else(|| exec_err("XX000", "eval_set_op called on a plain SELECT"))?;
    let mut acc = run_select(q, &root.left, outer)?;
    for b in &root.chain {
        let right = run_select(q, &b.right, outer)?;
        acc = combine_set_branches(b.op, b.all, acc, right)?;
    }
    apply_setop_tail(outer, root, acc)
}

/// v0.44: combine two branch results with one set operator.
pub(crate) fn combine_set_branches(
    op: SetOpKind,
    all: bool,
    left: SelectOut,
    right: SelectOut,
) -> Result<SelectOut, ExecError> {
    let op_name = match op {
        SetOpKind::Union => "UNION",
        SetOpKind::Intersect => "INTERSECT",
        SetOpKind::Except => "EXCEPT",
    };
    if left.columns.len() != right.columns.len() {
        return Err(exec_err(
            "42804",
            format!("each {op_name} query must have the same number of columns"),
        ));
    }
    // Resolve a common type per column; names come from the left branch.
    let mut out_cols: Vec<(String, ColType)> = Vec::with_capacity(left.columns.len());
    for ((lname, lty), (_, rty)) in left.columns.iter().zip(right.columns.iter()) {
        let common = common_supertype(op_name, lty, rty)?;
        out_cols.push((lname.clone(), common));
    }
    let left_rows = coerce_set_rows(left.rows, &out_cols)?;
    let right_rows = coerce_set_rows(right.rows, &out_cols)?;
    let rows = match op {
        SetOpKind::Union => {
            if all {
                let mut rows = left_rows;
                rows.extend(right_rows);
                rows
            } else {
                dedup_rows(left_rows.into_iter().chain(right_rows).collect())?
            }
        }
        SetOpKind::Intersect => {
            if all {
                intersect_all_rows(left_rows, right_rows)?
            } else {
                let right_keys: std::collections::HashSet<Vec<u8>> =
                    right_rows.iter().map(setop_row_key).collect();
                let mut filtered = Vec::new();
                for r in left_rows {
                    if right_keys.contains(&setop_row_key(&r)) {
                        filtered.push(r);
                    }
                }
                dedup_rows(filtered)?
            }
        }
        SetOpKind::Except => {
            if all {
                except_all_rows(left_rows, right_rows)?
            } else {
                let right_keys: std::collections::HashSet<Vec<u8>> =
                    right_rows.iter().map(setop_row_key).collect();
                let mut filtered = Vec::new();
                for r in left_rows {
                    if !right_keys.contains(&setop_row_key(&r)) {
                        filtered.push(r);
                    }
                }
                dedup_rows(filtered)?
            }
        }
    };
    Ok(SelectOut {
        columns: out_cols,
        rows,
    })
}

/// v0.44: numeric kind rank for set-operation type widening.
pub(crate) const fn setop_numeric_rank(t: ColType) -> Option<u8> {
    match t {
        ColType::SmallInt => Some(0),
        ColType::Int => Some(1),
        ColType::BigInt => Some(2),
        ColType::Float4 => Some(3),
        // v0.94: PG19 parity (parse_coerce.c select_common_type):
        // float8 is the PREFERRED type of the numeric category
        // (pg_type.dat typispreferred), numeric->float8 is an implicit
        // cast while float8->numeric is assignment-only, so float8
        // beats numeric in common-type resolution (CASE, UNION, ...).
        // The old order ranked numeric above float8, mistyping e.g.
        // `CASE ... THEN 'NaN'::float8 ... ELSE -0.0 END` as numeric.
        ColType::Numeric(..) => Some(4),
        ColType::Float => Some(5),
        _ => None,
    }
}

/// v0.44: character-family check for set-operation type resolution.
/// v0.57: `name` joins the family — PG's select_common_type(name, text)
/// is text (name+name is handled by the a == b fast path above).
pub(crate) const fn setop_is_char_family(t: ColType) -> bool {
    matches!(
        t,
        ColType::Text
            | ColType::Char(_)
            | ColType::Varchar(_)
            | ColType::SingleChar
            | ColType::Name
    )
}

/// v0.44: Postgres `select_common_type` for set operations, restricted to
/// the type pairs this engine can produce. Numeric kinds widen toward
/// `NUMERIC`; the character family resolves to `TEXT`; anything else
/// must match exactly.
pub(crate) fn common_supertype(
    op_name: &str,
    a: &ColType,
    b: &ColType,
) -> Result<ColType, ExecError> {
    if a == b {
        return Ok(*a);
    }
    // v1.11: arrays resolve by element type (PG19's select_common_type
    // recurses into the element types); e.g. integer[] + bigint[] is
    // bigint[]. array_elem_coltype never returns an array (ArrayElem is
    // flat), so the recursion terminates.
    if let (ColType::Array(ea), ColType::Array(eb)) = (a, b) {
        let common = common_supertype(op_name, &array_elem_coltype(*ea), &array_elem_coltype(*eb))?;
        return Ok(ColType::Array(ArrayElem::of(&common)));
    }
    if let (Some(ra), Some(rb)) = (setop_numeric_rank(*a), setop_numeric_rank(*b)) {
        // v0.94: ranks mirror setop_numeric_rank — float8 (rank 5) is
        // PG19's preferred numeric-category type and beats numeric.
        return Ok(match ra.max(rb) {
            0 => ColType::SmallInt,
            1 => ColType::Int,
            2 => ColType::BigInt,
            3 => ColType::Float4,
            4 => ColType::Numeric(None),
            _ => ColType::Float,
        });
    }
    if setop_is_char_family(*a) && setop_is_char_family(*b) {
        // PG resolves varchar/bpchar unions to text (typmod is dropped).
        return Ok(ColType::Text);
    }
    Err(exec_err(
        "42804",
        format!("{op_name} types {a:?} and {b:?} cannot be matched"),
    ))
}

/// v0.55: resolve a CASE expression's result type — PG19's
/// `select_common_type` over all result arms plus ELSE. Untyped NULL
/// literals and unknown (text) literals do not constrain the common
/// type (PG coerces unknown literals to the resolved type); if every
/// arm is unknown the result is text.
#[allow(clippy::too_many_arguments)]
pub(crate) fn case_result_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    ctes: &[CteDef],
    whens: &[(Box<Expr>, Box<Expr>)],
    else_: &Option<Box<Expr>>,
) -> Result<ColType, ExecError> {
    fn unknown(e: &Expr) -> bool {
        matches!(
            e,
            Expr::Literal(Literal::Null) | Expr::Literal(Literal::Text(_))
        )
    }
    let mut acc: Option<ColType> = None;
    let mut arms: Vec<&Expr> = Vec::with_capacity(whens.len() + 1);
    for (_, r) in whens {
        arms.push(r);
    }
    if let Some(e) = else_.as_deref() {
        arms.push(e);
    }
    for e in arms {
        if unknown(e) {
            continue;
        }
        let t = expr_type(eng, snap, own, session, schemas, &[], ctes, e)?;
        acc = Some(match acc {
            Some(a) => common_supertype("CASE", &a, &t)?,
            None => t,
        });
    }
    Ok(acc.unwrap_or(ColType::Text))
}

/// v0.44: coerce every cell of each row to the resolved output types
/// (reuses the INSERT/UPDATE coercion rules).
pub(crate) fn coerce_set_rows(
    rows: Vec<Row>,
    out_cols: &[(String, ColType)],
) -> Result<Vec<Row>, ExecError> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let cells_in = row.into_cells();
        let mut cells = Vec::with_capacity(out_cols.len());
        for (cell, (name, ty)) in cells_in.into_iter().zip(out_cols.iter()) {
            cells.push(coerce_value(cell, ty, name)?);
        }
        out.push(Row::new(cells));
    }
    Ok(out)
}

/// v0.44: canonical key for a set-operation row. Uses `value_key` for
/// most types, but normalizes `Numeric` by stripping trailing zeros so
/// that `1.0` and `1.00` (equal by `compare_values`) get the same key.
pub(crate) fn setop_row_key(row: &Row) -> Vec<u8> {
    let mut out = Vec::new();
    for v in row.iter() {
        match v {
            Value::Numeric(n) if n.special == crate::storage::NumericSpecial::Finite => {
                // v0.63: big-mantissa aware — the key is the normalized
                // (sign, scale, magnitude bytes).
                let (unscaled, scale) = if n.is_big() {
                    (n.unscaled, n.scale)
                } else {
                    let mut unscaled = n.unscaled;
                    let mut scale = n.scale;
                    while scale > 0 && unscaled % 10 == 0 {
                        unscaled /= 10;
                        scale -= 1;
                    }
                    (unscaled, scale)
                };
                out.push(8);
                out.extend_from_slice(&unscaled.to_be_bytes());
                out.extend_from_slice(&scale.to_be_bytes());
                if n.is_big() {
                    // Big values are already normalized; unscaled holds
                    // the sign, so append the exact magnitude bytes.
                    let mag = n.mag();
                    out.extend_from_slice(&(mag.decimal_digits()).to_be_bytes());
                    out.extend_from_slice(mag.to_decimal_string().as_bytes());
                }
            }
            _ => value_key(v, &mut out),
        }
    }
    out
}

/// v0.44: remove duplicate rows, keeping first-occurrence order.
/// Uses a HashSet of canonical keys for O(n) performance.
pub(crate) fn dedup_rows(rows: Vec<Row>) -> Result<Vec<Row>, ExecError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let key = setop_row_key(&r);
        if seen.insert(key) {
            out.push(r);
        }
    }
    Ok(out)
}

/// v0.44: `INTERSECT ALL`: each distinct row appears
/// `min(left_count, right_count)` times. Uses HashMap for O(n).
pub(crate) fn intersect_all_rows(left: Vec<Row>, right: Vec<Row>) -> Result<Vec<Row>, ExecError> {
    let mut right_counts: std::collections::HashMap<Vec<u8>, usize> =
        std::collections::HashMap::new();
    for r in &right {
        *right_counts.entry(setop_row_key(r)).or_insert(0) += 1;
    }
    let mut out = Vec::new();
    for l in left {
        let key = setop_row_key(&l);
        if let Some(count) = right_counts.get_mut(&key) {
            if *count > 0 {
                *count -= 1;
                out.push(l);
            }
        }
    }
    Ok(out)
}

/// v0.44: `EXCEPT ALL`: each left row is emitted unless a matching right
/// row exists, consuming one right match per emitted suppression.
/// Uses HashMap for O(n).
pub(crate) fn except_all_rows(left: Vec<Row>, right: Vec<Row>) -> Result<Vec<Row>, ExecError> {
    let mut right_counts: std::collections::HashMap<Vec<u8>, usize> =
        std::collections::HashMap::new();
    for r in &right {
        *right_counts.entry(setop_row_key(r)).or_insert(0) += 1;
    }
    let mut out = Vec::new();
    for l in left {
        let key = setop_row_key(&l);
        if let Some(count) = right_counts.get_mut(&key) {
            if *count > 0 {
                *count -= 1;
                continue;
            }
        }
        out.push(l);
    }
    Ok(out)
}

/// v0.44: apply the set-operation root's `ORDER BY` / `OFFSET` / `LIMIT`
/// to the combined rows.
pub(crate) fn apply_setop_tail(
    outer: &[Scope],
    root: &SetOpRoot,
    mut acc: SelectOut,
) -> Result<SelectOut, ExecError> {
    if !root.order_by.is_empty() {
        // v0.46: validate the sort terms once up front, before touching any
        // rows. PG resolves set-operation ORDER BY at analysis time against
        // the leftmost branch's output names (see transformSetOperationStmt:
        // "make lists of the dummy vars and their names for use in parsing
        // ORDER BY"), so an invalid term must fail even when the combined
        // result is empty (e.g. `... EXCEPT ...` with no surviving rows).
        for term in &root.order_by {
            validate_setop_sort_term(term, &acc.columns)?;
        }
        // Build a scope over the combined output so ORDER BY expressions
        // can reference output columns by name.
        let schema: Vec<QCol> = acc
            .columns
            .iter()
            .map(|(n, ty)| QCol {
                qual: String::new(),
                name: n.clone(),
                ty: *ty,
                hidden: false,
                src_ord: 0,
            })
            .collect();
        let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(acc.rows.len());
        for row in acc.rows.drain(..) {
            let scope = Scope {
                schema: &schema,
                row: &row,
                prov: None,
            };
            let scopes = [scope];
            let mut keys = Vec::with_capacity(root.order_by.len());
            for term in &root.order_by {
                keys.push(eval_setop_sort_key(&scopes, outer, term, &acc.columns)?);
            }
            keyed.push((keys, row));
        }
        let mut sort_err: Option<ExecError> = None;
        keyed.sort_by(|(ka, _), (kb, _)| {
            if sort_err.is_some() {
                return std::cmp::Ordering::Equal;
            }
            for (i, term) in root.order_by.iter().enumerate() {
                match compare_values(&ka[i], &kb[i], term.desc, term.nulls_first) {
                    Ok(std::cmp::Ordering::Equal) => {}
                    Ok(ord) => return ord,
                    Err(e) => {
                        sort_err = Some(e);
                        return std::cmp::Ordering::Equal;
                    }
                }
            }
            std::cmp::Ordering::Equal
        });
        if let Some(e) = sort_err {
            return Err(e);
        }
        acc.rows = keyed.into_iter().map(|(_, r)| r).collect();
    }
    if let Some(off) = root.offset {
        let n = usize::try_from(off.max(0)).unwrap_or(usize::MAX);
        if n < acc.rows.len() {
            acc.rows.drain(..n);
        } else {
            acc.rows.clear();
        }
    }
    if let Some(lim) = root.limit {
        let n = usize::try_from(lim.max(0)).unwrap_or(usize::MAX);
        acc.rows.truncate(n);
    }
    Ok(acc)
}

/// v0.46: validate one set-operation ORDER BY term against the combined
/// output columns, without needing any row data. Split out of
/// `eval_setop_sort_key` so the terms are checked once up front in
/// `apply_setop_tail` — PG raises 42703/42803 at analysis time, i.e. even
/// when the result is empty.
pub(crate) fn validate_setop_sort_term(
    term: &OrderTerm,
    columns: &[(String, ColType)],
) -> Result<(), ExecError> {
    let ordinal: Option<usize> = match &term.expr {
        Expr::Literal(Literal::Int(n) | Literal::BigInt(n)) if *n >= 1 => usize::try_from(*n).ok(),
        _ => None,
    };
    if let Some(n) = ordinal {
        if n <= columns.len() {
            return Ok(());
        }
        return Err(exec_err(
            "42803",
            format!("ORDER BY position {n} is not in select list"),
        ));
    }
    // Bare column name: must match an output column.
    if let Expr::Column { name, .. } = &term.expr {
        if columns
            .iter()
            .any(|(col_name, _)| col_name.eq_ignore_ascii_case(name))
        {
            return Ok(());
        }
        return Err(exec_err(
            "42703",
            format!("column \"{name}\" does not exist"),
        ));
    }
    // Arbitrary expressions are not allowed in set-operation ORDER BY.
    Err(exec_err(
        "42703",
        "ORDER BY expressions must be output column names or ordinals".to_string(),
    ))
}

/// v0.44: evaluate one ORDER BY term of a set operation. PG restricts
/// set-operation ORDER BY to output-column ordinals (integer literals)
/// or bare output-column names. Anything else — including names that are
/// not output columns — is an error (SQLSTATE 42703).
pub(crate) fn eval_setop_sort_key(
    scopes: &[Scope],
    _outer: &[Scope],
    term: &OrderTerm,
    columns: &[(String, ColType)],
) -> Result<Value, ExecError> {
    // Terms were validated up front in `apply_setop_tail`; re-validate
    // here so this function's contract holds for every caller.
    validate_setop_sort_term(term, columns)?;
    if let Expr::Literal(Literal::Int(n) | Literal::BigInt(n)) = &term.expr {
        let i = usize::try_from(*n).unwrap_or(0);
        if i >= 1 && i <= columns.len() {
            return Ok(scopes[0].row[i - 1].clone());
        }
    }
    if let Expr::Column { name, .. } = &term.expr {
        for (i, (col_name, _)) in columns.iter().enumerate() {
            if col_name.eq_ignore_ascii_case(name) {
                return Ok(scopes[0].row[i].clone());
            }
        }
    }
    // Unreachable: the term validated above.
    Err(exec_err(
        "42703",
        "ORDER BY expressions must be output column names or ordinals".to_string(),
    ))
}

/// v0.11: qualifier -> real-table map for one query level (see
/// `Q::priv_scopes`). CTEs shadow real tables; views and derived
/// tables map to `None` (checked at their own query level).
pub(crate) fn priv_scope_for(q: &Q, stmt: &SelectStmt) -> Vec<(String, Option<String>)> {
    fn walk(q: &Q, item: &FromItem, out: &mut Vec<(String, Option<String>)>) {
        match item {
            FromItem::Table { name, alias, .. } => {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let is_cte = q.ctes.iter().rev().any(|b| b.name == *name);
                let real = if is_cte {
                    None
                } else {
                    q.eng
                        .db
                        .find_table(name, q.snap, &q.all_xids, q.session)
                        .map(|_| name.clone())
                };
                out.push((qual, real));
            }
            FromItem::Derived { alias, .. } | FromItem::Values { alias, .. } => {
                out.push((alias.clone(), None));
            }
            // v0.32: table function — no underlying table; privilege is
            // checked at the function's own level (none needed).
            FromItem::Function { name, alias, .. } => {
                out.push((alias.clone().unwrap_or_else(|| name.clone()), None))
            }
            FromItem::Join {
                left, right, alias, ..
            } => {
                walk(q, left, out);
                walk(q, right, out);
                // v0.23: `(a JOIN b ...) AS x` requalifies the output to
                // `x`; the using-alias (if any) is checked at its own
                // level like a derived table. Underlying tables remain
                // listed so unqualified refs keep their privilege checks.
                if let Some(x) = alias {
                    out.push((x.clone(), None));
                }
            }
        }
    }
    let mut out = Vec::new();
    for item in &stmt.from {
        walk(q, item, &mut out);
    }
    out
}

/// v0.11: column-level SELECT enforcement. Every column read at this
/// query level must be covered by table-level SELECT or a column-level
/// SELECT grant. Unresolvable references (unknown tables, view/CTE
/// columns) are skipped here: unknown relations fail later as 42P01,
/// and views/CTEs/derived tables are checked at their own level when
/// expanded. Correlated references resolve against outer levels via
/// `q.priv_scopes` (innermost last, current level included).
pub(crate) fn check_select_col_privs(q: &Q, stmt: &SelectStmt) -> Result<(), ExecError> {
    use crate::sql::SelectItem;
    let mut refs: Vec<(Option<String>, String)> = Vec::new();
    let mut star = false;
    let mut star_quals: Vec<String> = Vec::new();
    for item in &stmt.items {
        match item {
            SelectItem::All => star = true,
            SelectItem::AllOf(qual) => star_quals.push(qual.clone()),
            SelectItem::Expr { expr, .. } => crate::sql::collect_col_refs(expr, &mut refs),
        }
    }
    if let Some(w) = &stmt.where_ {
        crate::sql::collect_col_refs(w, &mut refs);
    }
    for g in stmt.group_by.iter().flatten() {
        crate::sql::collect_col_refs(g, &mut refs);
    }
    if let Some(h) = &stmt.having {
        crate::sql::collect_col_refs(h, &mut refs);
    }
    for o in &stmt.order_by {
        crate::sql::collect_col_refs(&o.expr, &mut refs);
    }
    // v0.52: DISTINCT ON expressions read input columns too.
    for d in &stmt.distinct_on {
        crate::sql::collect_col_refs(d, &mut refs);
    }

    // All (table, column) pairs this level needs SELECT on.
    let mut needed: Vec<(String, String)> = Vec::new();
    let levels: Vec<&Vec<(String, Option<String>)>> = q.priv_scopes.iter().collect();
    // Does `table` currently have `col`?
    let has_col = |table: &str, col: &str| {
        q.eng
            .db
            .find_table(table, q.snap, &q.all_xids, q.session)
            .map(|t| t.columns.iter().any(|(n, _)| n == col))
            .unwrap_or(false)
    };
    if star {
        if let Some(level) = levels.last() {
            for (_, real) in level.iter() {
                if let Some(t) = real {
                    if let Some(tab) = q.eng.db.find_table(t, q.snap, &q.all_xids, q.session) {
                        for (cn, _) in &tab.columns {
                            needed.push((t.clone(), cn.clone()));
                        }
                    }
                }
            }
        }
    }
    for sq in &star_quals {
        if let Some((t, _)) = resolve_priv_qual(&levels, Some(sq)) {
            if let Some(tab) = q.eng.db.find_table(&t, q.snap, &q.all_xids, q.session) {
                for (cn, _) in &tab.columns {
                    needed.push((t.clone(), cn.clone()));
                }
            }
        }
    }
    for (qual, col) in &refs {
        for (t, _) in resolve_priv_ref(&levels, qual.as_deref(), col, &has_col) {
            needed.push((t, col.clone()));
        }
    }
    let closure = crate::storage::role_closure(&q.eng.db, q.role, q.snap, q.own);
    for (t, c) in &needed {
        let tab = q
            .eng
            .db
            .find_table(t, q.snap, &q.all_xids, q.session)
            .expect("resolved from a live table above");
        let have =
            crate::storage::column_privs_in(&q.eng.db, q.role, tab, c, &closure, q.snap, q.own);
        if have & crate::storage::PRIV_SELECT != crate::storage::PRIV_SELECT {
            return Err(exec_err(
                "42501",
                format!("permission denied for column \"{}\" of table \"{}\"", c, t),
            ));
        }
    }
    Ok(())
}

/// Resolve a qualified name to its table: the innermost level
/// containing the qualifier wins. Returns `None` when the qualifier
/// names a view/CTE/derived table (checked at its own level) or is
/// unknown (fails later as 42P01).
pub(crate) fn resolve_priv_qual(
    levels: &[&Vec<(String, Option<String>)>],
    qual: Option<&str>,
) -> Option<(String, ())> {
    let qual = qual?;
    for level in levels.iter().rev() {
        if let Some((_, real)) = level.iter().find(|(qn, _)| qn == qual) {
            return real.clone().map(|t| (t, ()));
        }
    }
    None
}

/// Resolve a column reference to the (table, column) pairs it may read,
/// mirroring the executor's innermost-first scope resolution:
/// a qualified ref binds to the innermost level holding the qualifier;
/// an unqualified ref binds to the innermost level with a table
/// carrying that column (all such tables, if several — the query is
/// ambiguous and fails anyway).
pub(crate) fn resolve_priv_ref(
    levels: &[&Vec<(String, Option<String>)>],
    qual: Option<&str>,
    col: &str,
    has_col: &dyn Fn(&str, &str) -> bool,
) -> Vec<(String, ())> {
    if qual.is_some() {
        return resolve_priv_qual(levels, qual).into_iter().collect();
    }
    for level in levels.iter().rev() {
        let mut found = Vec::new();
        for (_, real) in level.iter() {
            if let Some(t) = real {
                if has_col(t, col) {
                    found.push((t.clone(), ()));
                }
            }
        }
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}
