// v1.78 mechanical split: moved verbatim from src/exec.rs (60352-60502, 66204-66277).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

pub(crate) fn info_columns_schema() -> Vec<QCol> {
    [
        ("table_catalog", ColType::Text),
        ("table_schema", ColType::Text),
        ("table_name", ColType::Text),
        ("column_name", ColType::Text),
        ("ordinal_position", ColType::Int),
        ("column_default", ColType::Text),
        ("is_nullable", ColType::Text),
        ("data_type", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "information_schema.columns".to_string(),
        name: n.to_string(),
        ty,

        hidden: false,
        src_ord: 0,
    })
    .collect()
}

pub(crate) fn info_columns_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = info_columns_schema();
    let mut rows = Vec::new();
    let mut tables: Vec<(String, Table)> = db
        .tables
        .iter()
        .filter(|(_, vs)| {
            vs.iter()
                .any(|t| crate::storage::table_visible(t, snap, &[own]))
        })
        .map(|(n, vs)| {
            let t = vs
                .iter()
                .find(|t| crate::storage::table_visible(t, snap, &[own]))
                .expect("filtered")
                .clone();
            (n.clone(), t)
        })
        .collect();
    tables.sort_by(|a, b| a.0.cmp(&b.0));
    for (tn, t) in tables {
        for (i, (cn, ty)) in t.columns.iter().enumerate() {
            let default = match &t.defaults[i] {
                Some(d) => Value::text(format!("{:?}", d)),
                None => Value::Null,
            };
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::text("rustgres"),
                    Value::text("public"),
                    Value::text(tn.as_str()),
                    Value::text(cn.as_str()),
                    // v1.44: PG19 information_schema.columns defines
                    // ordinal_position as CAST(a.attnum AS
                    // cardinal_number) — the attnum gap survives
                    // DROP COLUMN (PG never reuses attnums), so this is
                    // NOT the dense column position. Same
                    // .get(i)-with-dense-fallback idiom as v1.41's
                    // pg_attribute.attnum.
                    Value::Int(t.attnums.get(i).copied().unwrap_or(i as i16 + 1) as i64),
                    default,
                    Value::text(if t.not_null[i] { "NO" } else { "YES" }),
                    Value::text(format!("{:?}", ty)),
                ]),
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}

// ---------------------------------------------------------------------------
// v0.98: information_schema.sequences (virtual). PG19's information_schema
// sequences view: sequence_catalog, sequence_schema, sequence_name,
// data_type, numeric_precision, numeric_precision_radix, numeric_scale,
// start_value, minimum_value, maximum_value, increment, cycle_option.
// numeric_precision follows the sequence data type (16/32/64); radix is 2
// (binary) and scale 0 for the exact integer types, matching PG19's
// information_schema. cycle_option is 'YES'/'NO'.
// ---------------------------------------------------------------------------

pub(crate) fn info_sequences_schema() -> Vec<QCol> {
    [
        ("sequence_catalog", ColType::Text),
        ("sequence_schema", ColType::Text),
        ("sequence_name", ColType::Text),
        ("data_type", ColType::Text),
        ("numeric_precision", ColType::Int),
        ("numeric_precision_radix", ColType::Int),
        ("numeric_scale", ColType::Int),
        ("start_value", ColType::Text),
        ("minimum_value", ColType::Text),
        ("maximum_value", ColType::Text),
        ("increment", ColType::Text),
        ("cycle_option", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "information_schema.sequences".to_string(),
        name: n.to_string(),
        ty,

        hidden: false,
        src_ord: 0,
    })
    .collect()
}

pub(crate) fn info_sequences_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = info_sequences_schema();
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
        // v0.99: data_type/precision follow the stored sequence type
        // (PG19 seqtypid), not inferred bounds.
        let (data_type, precision) = match s.seq_type {
            crate::sql::SeqType::SmallInt => ("smallint", 16),
            crate::sql::SeqType::Integer => ("integer", 32),
            crate::sql::SeqType::BigInt => ("bigint", 64),
        };
        rows.push(QRow {
            cells: Row::new(vec![
                Value::text("rustgres"),
                Value::text("public"),
                Value::text(name.as_str()),
                Value::text(data_type),
                Value::Int(precision),
                Value::Int(2),
                Value::Int(0),
                Value::text(s.start.to_string()),
                Value::text(s.min_value.to_string()),
                Value::text(s.max_value.to_string()),
                Value::text(s.increment.to_string()),
                Value::text(if s.cycle { "YES" } else { "NO" }),
            ]),
            prov: Vec::new(),
        });
    }
    (schema, rows)
}

// ============================================================================
// v0.9: information_schema / pg_catalog virtual tables (`\d`-style
// introspection over the real catalog).
// ============================================================================

pub(crate) fn info_tables_schema() -> Vec<QCol> {
    [
        ("table_catalog", ColType::Text),
        ("table_schema", ColType::Text),
        ("table_name", ColType::Text),
        ("table_type", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "information_schema.tables".to_string(),
        name: n.to_string(),
        ty,

        hidden: false,
        src_ord: 0,
    })
    .collect()
}

pub(crate) fn info_tables_scan(db: &Database, snap: &Snapshot, own: u64) -> (Vec<QCol>, Vec<QRow>) {
    let schema = info_tables_schema();
    let mut rows = Vec::new();
    let mut names: Vec<String> = db
        .tables
        .iter()
        .filter(|(_, vs)| {
            vs.iter()
                .any(|t| crate::storage::table_visible(t, snap, &[own]))
        })
        .map(|(n, _)| n.clone())
        .collect();
    names.sort();
    for tn in names {
        rows.push(QRow {
            cells: Row::new(vec![
                Value::text("rustgres"),
                Value::text("public"),
                Value::text(tn),
                Value::text("BASE TABLE"),
            ]),
            prov: Vec::new(),
        });
    }
    let mut vnames: Vec<String> = db
        .views
        .iter()
        .filter(|(_, vs)| {
            vs.iter()
                .any(|v| crate::storage::view_visible(v, snap, own))
        })
        .map(|(n, _)| n.clone())
        .collect();
    vnames.sort();
    for vn in vnames {
        rows.push(QRow {
            cells: Row::new(vec![
                Value::text("rustgres"),
                Value::text("public"),
                Value::text(vn),
                Value::text("VIEW"),
            ]),
            prov: Vec::new(),
        });
    }
    (schema, rows)
}
// ========================================================================

// ---------------------------------------------------------------------------
// v1.96: information_schema privilege views (PG19 information_schema
// definitions over the ACL store). All take the current role: the
// views only expose grants to roles the user belongs to (plus the
// owner rows for tables the user owns).
// ---------------------------------------------------------------------------

fn priv_type_names(bits: u32) -> Vec<(&'static str, u32)> {
    vec![
        ("SELECT", crate::storage::PRIV_SELECT),
        ("INSERT", crate::storage::PRIV_INSERT),
        ("UPDATE", crate::storage::PRIV_UPDATE),
        ("DELETE", crate::storage::PRIV_DELETE),
        ("TRUNCATE", crate::storage::PRIV_TRUNCATE),
        ("REFERENCES", crate::storage::PRIV_REFERENCES),
        ("TRIGGER", crate::storage::PRIV_TRIGGER),
    ]
    .into_iter()
    .filter(|(_, b)| bits & b == *b)
    .collect()
}

fn info_qcol(view: &str, n: &str, ty: ColType) -> QCol {
    QCol {
        qual: view.to_string(),
        name: n.to_string(),
        ty,
        hidden: false,
        src_ord: 0,
    }
}

pub(crate) fn info_table_privileges_schema() -> Vec<QCol> {
    [
        ("grantor", ColType::Text),
        ("grantee", ColType::Text),
        ("table_catalog", ColType::Text),
        ("table_schema", ColType::Text),
        ("table_name", ColType::Text),
        ("privilege_type", ColType::Text),
        ("is_grantable", ColType::Text),
        ("with_hierarchy", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| info_qcol("information_schema.table_privileges", n, ty))
    .collect()
}

pub(crate) fn info_table_privileges_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    role: &str,
    role_table_grants: bool,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = if role_table_grants {
        [
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
            ("with_hierarchy", ColType::Text),
        ]
        .into_iter()
        .map(|(n, ty)| info_qcol("information_schema.role_table_grants", n, ty))
        .collect()
    } else {
        info_table_privileges_schema()
    };
    let closure = crate::storage::role_closure(db, role, snap, own);
    let mut rows = Vec::new();
    let mut names: Vec<String> = db
        .tables
        .keys()
        .filter(|n| db.find_table(n, snap, &[own], 0).is_some())
        .cloned()
        .collect();
    names.sort();
    for tn in names {
        let Some(t) = db.find_table(&tn, snap, &[own], 0) else {
            continue;
        };
        // (grantor, grantee, bits, grant-option bits). v1.96: the
        // owner's row reflects its materialized ACL item when one
        // exists (a REVOKE FROM the owner subtracts from it); the
        // item itself is skipped in the entry loop below.
        let owner_item = t
            .acl
            .iter()
            .find(|e| e.role == t.owner && (e.grantor.is_empty() || e.grantor == t.owner));
        // The owner's grantability is inherent (PG renders the
        // owner aclitem without stars yet reports is_grantable=YES),
        // so only the privilege bits come from the stored item.
        let (obits, ogo) = match owner_item {
            Some(e) => (e.privs, crate::storage::PRIV_ALL_TABLE),
            None => (
                crate::storage::PRIV_ALL_TABLE,
                crate::storage::PRIV_ALL_TABLE,
            ),
        };
        let mut grants: Vec<(String, String, u32, u32)> =
            vec![(t.owner.clone(), t.owner.clone(), obits, ogo)];
        for e in &t.acl {
            if e.role == t.owner && (e.grantor.is_empty() || e.grantor == t.owner) {
                continue;
            }
            // v1.96: the row's grantor is the entry's own grantor
            // (empty = the owner), and is_grantable follows the
            // entry's grant-option bits per privilege.
            let gtor = if e.grantor.is_empty() {
                t.owner.clone()
            } else {
                e.grantor.clone()
            };
            grants.push((gtor, e.role.clone(), e.privs, e.grant_option));
        }
        // v1.96: a superuser has privileges of every role, so every
        // row is visible to it (PG19 information_schema semantics).
        let is_super = db
            .find_role(role, snap, own)
            .map(|r| r.superuser)
            .unwrap_or(false);
        for (grantor, grantee, bits, go_bits) in grants {
            // PG: table_privileges shows grants whose grantor or
            // grantee is an enabled role of the current user (or
            // PUBLIC); the owner's own rows show when the user owns
            // the table.
            let visible = is_super
                || grantee == "public"
                || closure.iter().any(|n| n == &grantee)
                || closure.iter().any(|n| n == &grantor)
                || grantee == role
                || grantor == role;
            if !visible {
                continue;
            }
            if role_table_grants && grantor == grantee {
                continue; // role_table_grants excludes owner self-rows
            }
            for (pname, pbit) in priv_type_names(bits) {
                let grantable = go_bits & pbit == pbit;
                let mut cells = vec![
                    Value::text(grantor.as_str()),
                    Value::text(if grantee == "public" {
                        "PUBLIC"
                    } else {
                        grantee.as_str()
                    }),
                    Value::text("rustgres"),
                    Value::text("public"),
                    Value::text(tn.as_str()),
                    Value::text(pname),
                    Value::text(if grantable { "YES" } else { "NO" }),
                ];
                // with_hierarchy: YES only for SELECT (PG19 view def).
                cells.push(Value::text(if pname == "SELECT" { "YES" } else { "NO" }));
                rows.push(QRow {
                    cells: Row::new(cells),
                    prov: Vec::new(),
                });
            }
        }
    }
    (schema, rows)
}

pub(crate) fn info_column_privileges_schema() -> Vec<QCol> {
    [
        ("grantor", ColType::Text),
        ("grantee", ColType::Text),
        ("table_catalog", ColType::Text),
        ("table_schema", ColType::Text),
        ("table_name", ColType::Text),
        ("column_name", ColType::Text),
        ("privilege_type", ColType::Text),
        ("is_grantable", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| info_qcol("information_schema.column_privileges", n, ty))
    .collect()
}

pub(crate) fn info_column_privileges_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    role: &str,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = info_column_privileges_schema();
    let closure = crate::storage::role_closure(db, role, snap, own);
    let mut rows = Vec::new();
    let mut names: Vec<String> = db
        .tables
        .keys()
        .filter(|n| db.find_table(n, snap, &[own], 0).is_some())
        .cloned()
        .collect();
    names.sort();
    for tn in names {
        let Some(t) = db.find_table(&tn, snap, &[own], 0) else {
            continue;
        };
        for (cn, _) in &t.columns {
            // (grantee, bits, grantable): owner's implicit row, table
            // ACL rows (they cover every column), column ACL rows.
            // v1.96: a materialized owner item supplies the bits and
            // is not repeated as a grant row.
            let owner_bits = t
                .acl
                .iter()
                .find(|e| e.role == t.owner && (e.grantor.is_empty() || e.grantor == t.owner))
                .map(|e| e.privs)
                .unwrap_or(crate::storage::PRIV_ALL_TABLE);
            let mut grants: Vec<(String, u32, bool)> = vec![(t.owner.clone(), owner_bits, true)];
            for e in &t.acl {
                if e.role == t.owner && (e.grantor.is_empty() || e.grantor == t.owner) {
                    continue;
                }
                grants.push((e.role.clone(), e.privs, false));
            }
            for e in &t.col_acl {
                if e.columns.iter().any(|c| c == cn) {
                    grants.push((e.role.clone(), e.privs, false));
                }
            }
            for (grantee, bits, grantable) in grants {
                let visible =
                    grantee == "public" || closure.iter().any(|n| n == &grantee) || grantee == role;
                if !visible {
                    continue;
                }
                for (pname, _) in priv_type_names(bits)
                    .into_iter()
                    .filter(|(n, _)| ["SELECT", "INSERT", "UPDATE", "REFERENCES"].contains(n))
                {
                    rows.push(QRow {
                        cells: Row::new(vec![
                            Value::text(t.owner.as_str()),
                            Value::text(if grantee == "public" {
                                "PUBLIC"
                            } else {
                                grantee.as_str()
                            }),
                            Value::text("rustgres"),
                            Value::text("public"),
                            Value::text(tn.as_str()),
                            Value::text(cn.as_str()),
                            Value::text(pname),
                            Value::text(if grantable { "YES" } else { "NO" }),
                        ]),
                        prov: Vec::new(),
                    });
                }
            }
        }
    }
    (schema, rows)
}

pub(crate) fn info_enabled_roles_schema() -> Vec<QCol> {
    vec![info_qcol(
        "information_schema.enabled_roles",
        "role_name",
        ColType::Text,
    )]
}

pub(crate) fn info_applicable_roles_schema() -> Vec<QCol> {
    vec![
        info_qcol(
            "information_schema.applicable_roles",
            "grantee",
            ColType::Text,
        ),
        info_qcol(
            "information_schema.applicable_roles",
            "role_name",
            ColType::Text,
        ),
        info_qcol(
            "information_schema.applicable_roles",
            "is_grantable",
            ColType::Text,
        ),
    ]
}

/// enabled_roles / applicable_roles (PG19 information_schema.sql):
/// enabled_roles = every role the current user has USAGE of
/// (pg_has_role USAGE: itself plus its inherit closure, plus
/// pg_database_owner when the user has the database owner);
/// applicable_roles = one row per membership edge whose MEMBER
/// (grantee) is enabled, plus the synthetic edge from the database
/// owner to pg_database_owner. is_grantable is the edge's own
/// admin option.
pub(crate) fn info_roles_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    role: &str,
    applicable: bool,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = if applicable {
        info_applicable_roles_schema()
    } else {
        info_enabled_roles_schema()
    };
    let superuser = crate::storage::is_superuser_snap(db, role, snap, own);
    let mut enabled: Vec<String> = if superuser {
        db.roles
            .keys()
            .filter(|n| db.find_role(n, snap, own).is_some())
            .cloned()
            .collect()
    } else {
        crate::storage::role_closure(db, role, snap, own)
    };
    if (superuser || enabled.iter().any(|n| n == &db.db_owner))
        && !enabled.iter().any(|n| n == "pg_database_owner")
    {
        enabled.push("pg_database_owner".to_string());
    }
    enabled.sort();
    enabled.dedup();
    let mut rows = Vec::new();
    if !applicable {
        for n in &enabled {
            rows.push(QRow {
                cells: Row::new(vec![Value::text(n.as_str())]),
                prov: Vec::new(),
            });
        }
        return (schema, rows);
    }
    for g in &enabled {
        let mut edges: Vec<(String, bool)> = Vec::new();
        if let Some(r) = db.find_role(g, snap, own) {
            for m in &r.memberships {
                if db.find_role(&m.role, snap, own).is_some() {
                    edges.push((m.role.clone(), m.admin));
                }
            }
        }
        if g == &db.db_owner {
            edges.push(("pg_database_owner".to_string(), false));
        }
        for (role_name, admin) in edges {
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::text(g.as_str()),
                    Value::text(role_name.as_str()),
                    Value::text(if admin { "YES" } else { "NO" }),
                ]),
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}

pub(crate) fn info_table_privileges_scan_schema_rtg() -> Vec<QCol> {
    [
        ("grantor", ColType::Text),
        ("grantee", ColType::Text),
        ("table_catalog", ColType::Text),
        ("table_schema", ColType::Text),
        ("table_name", ColType::Text),
        ("privilege_type", ColType::Text),
        ("is_grantable", ColType::Text),
        ("with_hierarchy", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| info_qcol("information_schema.role_table_grants", n, ty))
    .collect()
}

// ---------------------------------------------------------------------------
// v2.14: information_schema.triggers (PG19 information_schema.sql).
// One row per (trigger, event) for INSERT/DELETE/UPDATE events
// (TRUNCATE intentionally omitted, as in PG), including the
// partition-clone rows PG exposes. action_order is the rank over
// (schema, table, event, row/statement, before/after/instead)
// ordered by trigger name; action_condition / action_statement are
// carved out of the pg_get_triggerdef text like PG's view does.
// ---------------------------------------------------------------------------

pub(crate) fn info_triggers_schema() -> Vec<QCol> {
    [
        ("trigger_catalog", ColType::Text),
        ("trigger_schema", ColType::Text),
        ("trigger_name", ColType::Text),
        ("event_manipulation", ColType::Text),
        ("event_object_catalog", ColType::Text),
        ("event_object_schema", ColType::Text),
        ("event_object_table", ColType::Text),
        ("action_order", ColType::Int),
        ("action_condition", ColType::Text),
        ("action_statement", ColType::Text),
        ("action_orientation", ColType::Text),
        ("action_timing", ColType::Text),
        ("action_reference_old_table", ColType::Text),
        ("action_reference_new_table", ColType::Text),
        ("action_reference_old_row", ColType::Text),
        ("action_reference_new_row", ColType::Text),
        ("created", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| info_qcol("information_schema.triggers", n, ty))
    .collect()
}

pub(crate) fn info_triggers_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = info_triggers_schema();
    // (table, trigger) sources: own defs on tables and views, plus
    // clone rows on partition descendants.
    let mut src: Vec<(String, crate::sql::TriggerDef)> = Vec::new();
    let mut names: Vec<String> = db
        .tables
        .iter()
        .filter(|(_, vs)| {
            vs.iter()
                .any(|t| crate::storage::table_visible(t, snap, &[own]))
        })
        .map(|(n, _)| n.clone())
        .collect();
    names.sort();
    fn descendants_of(
        db: &Database,
        snap: &Snapshot,
        own: u64,
        session: u64,
        root: &str,
        out: &mut Vec<String>,
        depth: usize,
    ) {
        if depth > 32 {
            return;
        }
        if let Some(t) = db.find_table(root, snap, &[own], session) {
            if let Some(pi) = &t.partition {
                for c in &pi.children {
                    out.push(c.clone());
                    descendants_of(db, snap, own, session, c, out, depth + 1);
                }
            }
        }
    }
    for tn in &names {
        let Some(t) = db.find_table(tn, snap, &[own], session) else {
            continue;
        };
        for tr in &t.triggers {
            src.push((tn.clone(), tr.clone()));
        }
        if t.partition.is_some() && !t.triggers.is_empty() {
            let mut desc = Vec::new();
            descendants_of(db, snap, own, session, tn, &mut desc, 0);
            desc.sort();
            desc.dedup();
            for d in desc {
                if &d == tn {
                    continue;
                }
                for tr in &t.triggers {
                    src.push((d.clone(), tr.clone()));
                }
            }
        }
    }
    let mut vnames: Vec<String> = db
        .views
        .iter()
        .filter(|(_, vs)| {
            vs.iter()
                .any(|v| crate::storage::view_visible(v, snap, own))
        })
        .map(|(n, _)| n.clone())
        .collect();
    vnames.sort();
    for vn in &vnames {
        if let Some(v) = db.find_view(vn, snap, own) {
            for tr in &v.triggers {
                src.push((vn.clone(), tr.clone()));
            }
        }
    }
    // Expand to (table, trigger, event) and compute action_order.
    #[derive(Clone)]
    struct EvRow {
        table: String,
        tr: crate::sql::TriggerDef,
        event: &'static str,
        tgtype: i64,
    }
    let mut evs: Vec<EvRow> = Vec::new();
    for (tn, tr) in &src {
        let tgtype = crate::exec::catalog::trigger_tgtype(tr);
        for (bit, nm) in [
            (crate::sql::trig_event::INSERT, "INSERT"),
            (crate::sql::trig_event::DELETE, "DELETE"),
            (crate::sql::trig_event::UPDATE, "UPDATE"),
        ] {
            if tr.events & bit != 0 {
                evs.push(EvRow {
                    table: tn.clone(),
                    tr: tr.clone(),
                    event: nm,
                    tgtype,
                });
            }
        }
    }
    let mut rows = Vec::new();
    for ev in &evs {
        // Rank within the PG partition key, ordered by tgname.
        let mut group: Vec<&str> = evs
            .iter()
            .filter(|o| {
                o.table == ev.table
                    && o.event == ev.event
                    && (o.tgtype & 1) == (ev.tgtype & 1)
                    && (o.tgtype & 66) == (ev.tgtype & 66)
            })
            .map(|o| o.tr.name.as_str())
            .collect();
        group.sort();
        group.dedup();
        let order = group.iter().position(|n| *n == ev.tr.name).unwrap_or(0) as i64 + 1;
        let deftext = crate::exec::catalog::trigger_def_text(&ev.table, &ev.tr);
        let condition = ev.tr.when_expr.as_ref().map(|_| {
            // Carve the WHEN (...) body out of the def text like
            // PG's regexp over pg_get_triggerdef.
            let start = deftext.find("WHEN (").map(|i| i + 6);
            let end = deftext.find(") EXECUTE FUNCTION");
            match (start, end) {
                (Some(a), Some(b)) if b > a => deftext[a..b].to_string(),
                _ => String::new(),
            }
        });
        let statement = deftext
            .find("EXECUTE FUNCTION")
            .map(|i| deftext[i..].to_string())
            .unwrap_or_default();
        let orientation = if ev.tr.for_each_row {
            "ROW"
        } else {
            "STATEMENT"
        };
        let timing = match ev.tr.timing {
            crate::sql::TriggerTiming::Before => "BEFORE",
            crate::sql::TriggerTiming::After => "AFTER",
            crate::sql::TriggerTiming::InsteadOf => "INSTEAD OF",
        };
        rows.push(QRow {
            cells: Row::new(vec![
                Value::text("rustgres"),
                Value::text("public"),
                Value::text(ev.tr.name.clone()),
                Value::text(ev.event),
                Value::text("rustgres"),
                Value::text("public"),
                Value::text(ev.table.clone()),
                Value::Int(order),
                match condition {
                    Some(c) => Value::text(c),
                    None => Value::Null,
                },
                Value::text(statement),
                Value::text(orientation),
                Value::text(timing),
                match &ev.tr.old_table {
                    Some(n) => Value::text(n.clone()),
                    None => Value::Null,
                },
                match &ev.tr.new_table {
                    Some(n) => Value::text(n.clone()),
                    None => Value::Null,
                },
                Value::Null,
                Value::Null,
                Value::Null,
            ]),
            prov: Vec::new(),
        });
    }
    (schema, rows)
}
