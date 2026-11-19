// v2.33: pg_statistic (virtual) + the PG19 slot model shared by
// pg_statistic and the pg_stats view, over the ANALYZE census in
// Database.stats / stats_inh. Slot assembly is a pure function of a
// ColStats + column type, so ANALYZE-built and pg_restore_* -built
// statistics render identically (the stats_import round-trip relies
// on this).
use super::*;

/// v2.33: the virtual-catalog spelling of a FROM name: a leading
/// `pg_catalog.` qualifier is stripped (PG resolves catalog
/// relations through the search path; the engine's virtual hooks
/// match on the bare name). Other schema qualifiers are untouched.
pub(crate) fn virt_catalog_name(name: &str) -> &str {
    name.strip_prefix("pg_catalog.").unwrap_or(name)
}

/// One pg_statistic slot (stakind/staop/stacoll/stanumbers/stavalues).
#[derive(Clone, Debug)]
pub(crate) struct StatSlot {
    pub kind: i16,
    pub op: i64,
    pub coll: i64,
    pub numbers: Option<Vec<f64>>,
    pub values: Option<Vec<Value>>,
}

/// PG19 default btree equality / less-than operator oids per type
/// (pg_operator.dat of the PG19 tree).
pub(crate) fn type_ops(ty: &ColType) -> (i64, i64) {
    match ty {
        ColType::Bool => (91, 58),
        ColType::SmallInt => (94, 95),
        ColType::Int => (96, 97),
        ColType::BigInt => (410, 412),
        ColType::Float4 => (620, 622),
        ColType::Float => (670, 672),
        ColType::Numeric(_) => (1752, 1754),
        ColType::Text | ColType::Varchar(_) | ColType::Char(_) => (98, 664),
        ColType::Name => (93, 660),
        ColType::Date => (1093, 1095),
        ColType::Timestamp => (2060, 2062),
        ColType::Timestamptz => (1320, 1322),
        ColType::Interval => (1330, 1332),
        ColType::Uuid => (2972, 2974),
        ColType::Bytea => (1955, 1957),
        ColType::Oid | ColType::Regclass | ColType::Regproc => (607, 609),
        ColType::Record => (2988, 2990),
        ColType::Range(_) | ColType::Multirange(_) => (3882, 3884),
        ColType::Array(_) => (1070, 1072),
        ColType::TsVector => (3629, 3627),
        _ => (0, 0),
    }
}

/// Element type of an array / range-ish column for MCELEM purposes:
/// (element ColType, element eq-op oid). None when the column type
/// has no usable element type (PG19 statatt_get_elem_type; tsvector
/// is special-cased to text).
pub(crate) fn elem_type_of(ty: &ColType) -> Option<(ColType, i64)> {
    match ty {
        ColType::Array(e) => {
            let et = coltype_of_elem(*e);
            let (eq, _) = type_ops(&et);
            if eq == 0 && !matches!(et, ColType::Text) {
                None
            } else {
                Some((et, eq))
            }
        }
        ColType::TsVector => Some((ColType::Text, 98)),
        _ => None,
    }
}

pub(crate) fn coltype_of_elem(e: crate::storage::ArrayElem) -> ColType {
    use crate::storage::ArrayElem as A;
    match e {
        A::Bool => ColType::Bool,
        A::Bytea => ColType::Bytea,
        A::SmallInt => ColType::SmallInt,
        A::Int => ColType::Int,
        A::BigInt => ColType::BigInt,
        A::Float4 => ColType::Float4,
        A::Float => ColType::Float,
        A::Text => ColType::Text,
        A::Char => ColType::Char(None),
        A::Varchar => ColType::Varchar(None),
        A::Name => ColType::Name,
        A::Date => ColType::Date,
        A::Timestamp => ColType::Timestamp,
        A::Timestamptz => ColType::Timestamptz,
        A::Time => ColType::Time,
        A::TimeTz => ColType::TimeTz,
        A::Interval => ColType::Interval,
        A::Numeric => ColType::Numeric(None),
        A::Uuid => ColType::Uuid,
        A::Record => ColType::Record,
        A::TsVector => ColType::TsVector,
        A::TsQuery => ColType::TsQuery,
        _ => ColType::Text,
    }
}

/// Is this type collatable (PG19: stacoll = the column collation,
/// else 0)? The default collation oid is 100.
fn type_collatable(ty: &ColType) -> bool {
    matches!(
        ty,
        ColType::Text | ColType::Varchar(_) | ColType::Char(_) | ColType::Name | ColType::TsVector
    )
}

fn coll_of(ty: &ColType) -> i64 {
    if type_collatable(ty) { 100 } else { 0 }
}

/// Build an anyarray Value from same-typed values (1-D, lower bound
/// 1). Array-typed values are flattened one level with NULL padding
/// to a rectangular 2-D shape, as PG stores MCV lists of array
/// columns.
pub(crate) fn anyarray_value(vals: &[Value], elem_ty: &ColType) -> Value {
    if vals.iter().any(|v| matches!(v, Value::Array(_))) {
        // 2-D flattening: dims [n, maxlen], elem = inner elem type.
        let inner: Vec<&ArrayVal> = vals
            .iter()
            .map(|v| match v {
                Value::Array(a) => &**a,
                _ => unreachable!(),
            })
            .collect();
        let maxlen = inner.iter().map(|a| a.elems.len()).max().unwrap_or(0);
        let elem = inner
            .first()
            .map(|a| a.elem)
            .unwrap_or(crate::storage::ArrayElem::Text);
        let mut elems: Vec<Value> = Vec::new();
        for a in &inner {
            for i in 0..maxlen {
                elems.push(a.elems.get(i).cloned().unwrap_or(Value::Null));
            }
        }
        return Value::Array(Box::new(ArrayVal {
            elem,
            dims: vec![vals.len() as i32, maxlen as i32],
            lower: vec![1, 1],
            elems,
        }));
    }
    Value::Array(Box::new(ArrayVal {
        elem: ArrayElem::of(elem_ty),
        dims: vec![vals.len() as i32],
        lower: vec![1],
        elems: vals.to_vec(),
    }))
}

pub(crate) fn float4_array(vals: &[f64]) -> Value {
    Value::Array(Box::new(ArrayVal {
        elem: ArrayElem::Float4,
        dims: vec![vals.len() as i32],
        lower: vec![1],
        elems: vals.iter().map(|f| Value::Float4(*f as f32)).collect(),
    }))
}

pub(crate) fn float8_array(vals: &[f64]) -> Value {
    Value::Array(Box::new(ArrayVal {
        elem: ArrayElem::Float,
        dims: vec![vals.len() as i32],
        lower: vec![1],
        elems: vals.iter().map(|f| Value::Float(*f)).collect(),
    }))
}

/// The slot assembly: PG19 kind order as ANALYZE assigns them —
/// MCV, HISTOGRAM, CORRELATION, then MCELEM, DECHIST for
/// array/tsvector columns, and BOUNDS_HISTOGRAM before
/// RANGE_LENGTH_HISTOGRAM for range types (whose scalar slots are
/// empty by construction).
pub(crate) fn att_slots(cs: &ColStats, ty: &ColType) -> [Option<StatSlot>; 5] {
    let (eq_op, lt_op) = type_ops(ty);
    let coll = coll_of(ty);
    let mut slots: [Option<StatSlot>; 5] = Default::default();
    let mut next = 0usize;
    let push = |slots: &mut [Option<StatSlot>; 5], next: &mut usize, s: StatSlot| {
        if *next < 5 {
            slots[*next] = Some(s);
            *next += 1;
        }
    };
    if !cs.mcv.is_empty() {
        push(
            &mut slots,
            &mut next,
            StatSlot {
                kind: 1,
                op: eq_op,
                coll,
                numbers: Some(cs.mcv.iter().map(|(_, f)| *f).collect()),
                values: Some(cs.mcv.iter().map(|(v, _)| v.clone()).collect()),
            },
        );
    }
    if !cs.hist_bounds.is_empty() {
        push(
            &mut slots,
            &mut next,
            StatSlot {
                kind: 2,
                op: lt_op,
                coll,
                numbers: None,
                values: Some(cs.hist_bounds.clone()),
            },
        );
    }
    if let Some(c) = cs.correlation {
        push(
            &mut slots,
            &mut next,
            StatSlot {
                kind: 3,
                op: lt_op,
                coll,
                numbers: Some(vec![c]),
                values: None,
            },
        );
    }
    if let Some((vals, numbers)) = &cs.mcelem {
        let (_, eop) = elem_type_of(ty).unwrap_or((ColType::Text, eq_op));
        push(
            &mut slots,
            &mut next,
            StatSlot {
                kind: 4,
                op: eop,
                coll,
                numbers: Some(numbers.clone()),
                values: Some(vals.clone()),
            },
        );
    }
    if let Some(nums) = &cs.dechist {
        let (_, eop) = elem_type_of(ty).unwrap_or((ColType::Text, eq_op));
        push(
            &mut slots,
            &mut next,
            StatSlot {
                kind: 5,
                op: eop,
                coll,
                numbers: Some(nums.clone()),
                values: None,
            },
        );
    }
    if let Some(bounds) = &cs.bounds_hist {
        push(
            &mut slots,
            &mut next,
            StatSlot {
                kind: 7,
                op: 0,
                coll: 0,
                numbers: None,
                values: Some(bounds.clone()),
            },
        );
    }
    if let Some(lens) = &cs.range_length_hist {
        push(
            &mut slots,
            &mut next,
            StatSlot {
                kind: 6,
                op: 672, // Float8LessOperator
                coll: 0,
                numbers: Some(vec![cs.range_empty_frac.unwrap_or(0.0)]),
                values: Some(lens.iter().map(|f| Value::Float(*f)).collect()),
            },
        );
    }
    slots
}

/// Find the slot of a given kind (PG's get_attstatsslot search).
pub(crate) fn slot_of_kind<'a>(
    slots: &'a [Option<StatSlot>; 5],
    kind: i16,
) -> Option<&'a StatSlot> {
    slots.iter().flatten().find(|s| s.kind == kind)
}

/// A ColStats carrying no information at all (the ANALYZE-of-empty
/// shape): PG19 creates no pg_statistic row for it.
pub(crate) fn col_stats_pristine(cs: &ColStats) -> bool {
    cs.null_frac == 0.0
        && cs.distinct_raw == 0.0
        && cs.avg_width == 0
        && cs.n_distinct == 0.0
        && cs.mcv.is_empty()
        && cs.hist_bounds.is_empty()
        && cs.correlation.is_none()
        && cs.mcelem.is_none()
        && cs.dechist.is_none()
        && cs.range_length_hist.is_none()
        && cs.range_empty_frac.is_none()
        && cs.bounds_hist.is_none()
}

/// One resolved statistics-bearing relation for the catalog scans:
/// either a table (columns from its TableDef) or an index with
/// expression statistics (columns are the "expr:<attnum>" keys).
pub(crate) struct StatRel {
    pub oid: i64,
    pub name: String,
    pub schema: String,
    /// (attnum, display name, type, ColStats) in attnum order.
    pub cols: Vec<(i64, String, ColType, ColStats)>,
}

/// Resolve the ColStats entries of one stats-map entry into a
/// StatRel, or None when the relation no longer exists.
fn resolve_stat_rel(db: &Database, name: &str, ts: &TableStats) -> Option<StatRel> {
    // Table path: latest live version in db.tables (or a temp table
    // is not visible here — scans are session-agnostic like the
    // existing pg_stats_scan).
    if let Some(vers) = db.tables.get(name) {
        if let Some(t) = vers.iter().rev().find(|t| t.dropped_xmax == 0) {
            let schema = db
                .obj_schema
                .get(name)
                .cloned()
                .unwrap_or_else(|| "public".to_string());
            let mut cols: Vec<(i64, String, ColType, ColStats)> = Vec::new();
            for (i, (cn, ty)) in t.columns.iter().enumerate() {
                if let Some(cs) = ts.cols.get(cn) {
                    if col_stats_pristine(cs) {
                        continue;
                    }
                    let attnum = t.attnums.get(i).copied().unwrap_or(i as i16 + 1) as i64;
                    cols.push((attnum, cn.clone(), ty.clone(), cs.clone()));
                }
            }
            return Some(StatRel {
                oid: t.oid as i64,
                name: name.rsplit('.').next().unwrap_or(name).to_string(),
                schema,
                cols,
            });
        }
    }
    // v2.35: temp-table path — stats restored under schemaname
    // 'pg_temp' are keyed by the bare temp-table name; the table
    // itself lives in a session's temp map (the scan is
    // session-agnostic, so any session's map may hold it).
    for tm in db.temp_tables.values() {
        if let Some(t) = tm.get(name) {
            let mut cols: Vec<(i64, String, ColType, ColStats)> = Vec::new();
            for (i, (cn, ty)) in t.columns.iter().enumerate() {
                if let Some(cs) = ts.cols.get(cn) {
                    if col_stats_pristine(cs) {
                        continue;
                    }
                    let attnum = t.attnums.get(i).copied().unwrap_or(i as i16 + 1) as i64;
                    cols.push((attnum, cn.clone(), ty.clone(), cs.clone()));
                }
            }
            return Some(StatRel {
                oid: t.oid as i64,
                name: name.rsplit('.').next().unwrap_or(name).to_string(),
                schema: "pg_temp".to_string(),
                cols,
            });
        }
    }
    // Index path: expression statistics stored under the index name.
    if let Some(ix) = db.indexes.get(name) {
        let def = &ix.def;
        if def.dropped_xmax == 0 {
            // Synthetic oid: same 40000+n discipline as index_oid_map
            // (name-sorted over visible index defs).
            let mut names: Vec<&String> = db
                .indexes
                .iter()
                .filter(|(_, i)| i.def.dropped_xmax == 0)
                .map(|(n, _)| n)
                .collect();
            names.sort();
            let oid = names
                .iter()
                .position(|n| *n == name)
                .map(|p| 40000 + p as i64)
                .unwrap_or(0);
            let schema = db
                .obj_schema
                .get(&def.table)
                .cloned()
                .unwrap_or_else(|| "public".to_string());
            let mut cols: Vec<(i64, String, ColType, ColStats)> = Vec::new();
            for (key, cs) in &ts.cols {
                if col_stats_pristine(cs) {
                    continue;
                }
                if let Some(att) = key.strip_prefix("expr:") {
                    if let Ok(attnum) = att.parse::<i64>() {
                        // The expression's result type: recover from
                        // the first MCV/hist value when possible,
                        // else the value type of any stored datum;
                        // default to the type of the first non-null
                        // datum we can find in the stats themselves.
                        let ty = cs
                            .mcv
                            .first()
                            .map(|(v, _)| super::analyze::value_coltype(v))
                            .or_else(|| {
                                cs.hist_bounds
                                    .first()
                                    .map(|v| super::analyze::value_coltype(v))
                            })
                            .unwrap_or(ColType::Text);
                        cols.push((attnum, "expr".to_string(), ty, cs.clone()));
                    }
                }
            }
            cols.sort_by_key(|(a, _, _, _)| *a);
            return Some(StatRel {
                oid,
                name: name.rsplit('.').next().unwrap_or(name).to_string(),
                schema,
                cols,
            });
        }
    }
    None
}

/// All (StatRel, inherited) pairs with at least one live column.
pub(crate) fn stat_rels(db: &Database) -> Vec<(StatRel, bool)> {
    let mut out = Vec::new();
    let mut names: Vec<&String> = db.stats.keys().collect();
    names.sort();
    for n in names {
        if let Some(rel) = resolve_stat_rel(db, n, &db.stats[n]) {
            if !rel.cols.is_empty() {
                out.push((rel, false));
            }
        }
    }
    let mut names: Vec<&String> = db.stats_inh.keys().collect();
    names.sort();
    for n in names {
        if let Some(rel) = resolve_stat_rel(db, n, &db.stats_inh[n]) {
            if !rel.cols.is_empty() {
                out.push((rel, true));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// pg_statistic (virtual, 31 columns, PG19 pg_statistic.h order)
// ---------------------------------------------------------------------------

pub(crate) fn pg_statistic_schema() -> Vec<QCol> {
    let mut cols: Vec<(&str, ColType)> = vec![
        ("starelid", ColType::Int),
        ("staattnum", ColType::SmallInt),
        ("stainherit", ColType::Bool),
        ("stanullfrac", ColType::Float4),
        ("stawidth", ColType::Int),
        ("stadistinct", ColType::Float4),
    ];
    for i in 1..=5 {
        cols.push((
            match i {
                1 => "stakind1",
                2 => "stakind2",
                3 => "stakind3",
                4 => "stakind4",
                _ => "stakind5",
            },
            ColType::SmallInt,
        ));
    }
    for i in 1..=5 {
        cols.push((
            match i {
                1 => "staop1",
                2 => "staop2",
                3 => "staop3",
                4 => "staop4",
                _ => "staop5",
            },
            ColType::Int,
        ));
    }
    for i in 1..=5 {
        cols.push((
            match i {
                1 => "stacoll1",
                2 => "stacoll2",
                3 => "stacoll3",
                4 => "stacoll4",
                _ => "stacoll5",
            },
            ColType::Int,
        ));
    }
    for i in 1..=5 {
        cols.push((
            match i {
                1 => "stanumbers1",
                2 => "stanumbers2",
                3 => "stanumbers3",
                4 => "stanumbers4",
                _ => "stanumbers5",
            },
            ColType::Array(ArrayElem::Float4),
        ));
    }
    for _ in 1..=5 {
        cols.push(("stavalues", ColType::Text));
    }
    // Fix the stavalues names (built in a loop above for brevity).
    let names = [
        "stavalues1",
        "stavalues2",
        "stavalues3",
        "stavalues4",
        "stavalues5",
    ];
    for (j, n) in names.iter().enumerate() {
        cols[26 + j].0 = n;
    }
    cols.into_iter()
        .map(|(n, ty)| QCol {
            qual: "pg_statistic".to_string(),
            name: n.to_string(),
            ty,
            hidden: false,
            src_ord: 0,
        })
        .collect()
}

pub(crate) fn pg_statistic_scan(db: &Database) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_statistic_schema();
    let mut rows = Vec::new();
    for (rel, inh) in stat_rels(db) {
        for (attnum, _attname, ty, cs) in &rel.cols {
            let slots = att_slots(cs, ty);
            let mut cells: Vec<Value> = vec![
                Value::Int(rel.oid),
                Value::Int(*attnum),
                Value::Bool(inh),
                Value::Float4(cs.null_frac as f32),
                Value::Int(cs.avg_width as i64),
                Value::Float4(cs.distinct_raw as f32),
            ];
            for s in &slots {
                cells.push(Value::Int(s.as_ref().map(|s| s.kind as i64).unwrap_or(0)));
            }
            for s in &slots {
                cells.push(Value::Int(s.as_ref().map(|s| s.op).unwrap_or(0)));
            }
            for s in &slots {
                cells.push(Value::Int(s.as_ref().map(|s| s.coll).unwrap_or(0)));
            }
            for s in &slots {
                cells.push(match s {
                    Some(s) => match &s.numbers {
                        Some(nums) => float4_array(nums),
                        None => Value::Null,
                    },
                    None => Value::Null,
                });
            }
            for s in &slots {
                cells.push(match s {
                    Some(s) => match &s.values {
                        Some(vals) => {
                            // stavalues element type: the column type,
                            // except MCELEM (element type) and
                            // RANGE_LENGTH (float8).
                            let ety = match s.kind {
                                4 => elem_type_of(ty).map(|(et, _)| et).unwrap_or(ColType::Text),
                                6 => ColType::Float,
                                _ => ty.clone(),
                            };
                            anyarray_value(vals, &ety)
                        }
                        None => Value::Null,
                    },
                    None => Value::Null,
                });
            }
            rows.push(QRow {
                cells: Row::new(cells),
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}

// ---------------------------------------------------------------------------
// pg_stats (virtual, PG19 system_views.sql: 19 columns)
// ---------------------------------------------------------------------------

pub(crate) fn pg_stats_schema_v233() -> Vec<QCol> {
    [
        ("schemaname", ColType::Text),
        ("tablename", ColType::Text),
        ("tableid", ColType::Int),
        ("attname", ColType::Text),
        ("attnum", ColType::Int),
        ("inherited", ColType::Bool),
        ("null_frac", ColType::Float4),
        ("avg_width", ColType::Int),
        ("n_distinct", ColType::Float4),
        ("most_common_vals", ColType::Text),
        ("most_common_freqs", ColType::Array(ArrayElem::Float4)),
        ("histogram_bounds", ColType::Text),
        ("correlation", ColType::Float4),
        ("most_common_elems", ColType::Text),
        ("most_common_elem_freqs", ColType::Array(ArrayElem::Float4)),
        ("elem_count_histogram", ColType::Array(ArrayElem::Float4)),
        ("range_length_histogram", ColType::Array(ArrayElem::Float)),
        ("range_empty_frac", ColType::Float4),
        ("range_bounds_histogram", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "pg_stats".to_string(),
        name: n.to_string(),
        ty,
        hidden: false,
        src_ord: 0,
    })
    .collect()
}

pub(crate) fn pg_stats_scan_v233(db: &Database) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_stats_schema_v233();
    let mut rows = Vec::new();
    for (rel, inh) in stat_rels(db) {
        for (attnum, attname, ty, cs) in &rel.cols {
            let slots = att_slots(cs, ty);
            let vals_of = |kind: i16, ety: &ColType| -> Value {
                match slot_of_kind(&slots, kind) {
                    Some(s) => match &s.values {
                        Some(vals) => anyarray_value(vals, ety),
                        None => Value::Null,
                    },
                    None => Value::Null,
                }
            };
            let nums_of = |kind: i16| -> Value {
                match slot_of_kind(&slots, kind) {
                    Some(s) => match &s.numbers {
                        Some(nums) => float4_array(nums),
                        None => Value::Null,
                    },
                    None => Value::Null,
                }
            };
            let elem_ty = elem_type_of(ty).map(|(et, _)| et).unwrap_or(ColType::Text);
            let corr = match slot_of_kind(&slots, 3) {
                Some(s) => s
                    .numbers
                    .as_ref()
                    .and_then(|n| n.first())
                    .map(|f| Value::Float4(*f as f32))
                    .unwrap_or(Value::Null),
                None => Value::Null,
            };
            let range_len = match slot_of_kind(&slots, 6) {
                Some(s) => match &s.values {
                    Some(vals) => float8_array(
                        &vals
                            .iter()
                            .map(|v| match v {
                                Value::Float(f) => *f,
                                _ => 0.0,
                            })
                            .collect::<Vec<_>>(),
                    ),
                    None => Value::Null,
                },
                None => Value::Null,
            };
            let range_empty = match slot_of_kind(&slots, 6) {
                Some(s) => s
                    .numbers
                    .as_ref()
                    .and_then(|n| n.first())
                    .map(|f| Value::Float4(*f as f32))
                    .unwrap_or(Value::Null),
                None => Value::Null,
            };
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::text(rel.schema.as_str()),
                    Value::text(rel.name.as_str()),
                    Value::Int(rel.oid),
                    Value::text(attname.as_str()),
                    Value::Int(*attnum),
                    Value::Bool(inh),
                    Value::Float4(cs.null_frac as f32),
                    Value::Int(cs.avg_width as i64),
                    Value::Float4(cs.distinct_raw as f32),
                    vals_of(1, ty),
                    nums_of(1),
                    vals_of(2, ty),
                    corr,
                    vals_of(4, &elem_ty),
                    nums_of(4),
                    nums_of(5),
                    range_len,
                    range_empty,
                    vals_of(7, ty),
                ]),
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}
