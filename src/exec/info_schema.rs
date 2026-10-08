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
