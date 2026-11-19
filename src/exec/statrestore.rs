//! v2.33: the PG19 statistics import/export functions —
//! pg_restore_relation_stats / pg_clear_relation_stats /
//! pg_restore_attribute_stats / pg_clear_attribute_stats /
//! pg_restore_extended_stats / pg_clear_extended_stats.
//!
//! Semantics grounded in the PG19 sources
//! (src/backend/statistics/{stat_utils,relation_stats,
//! attribute_stats,extended_stats_funcs}.c):
//! - variadic name/value pairs; names are case-insensitive text;
//!   a NULL value means "leave unchanged"; `version` is ignored;
//!   an unrecognized name or an exact-type mismatch is a WARNING
//!   and makes the call return false, with the remaining valid
//!   arguments still applied;
//! - odd pair counts, NULL names and non-text names are ERRORs
//!   (no errcode in PG: the default XX000 class);
//! - missing required arguments are ERROR 22023
//!   (`argument "%s" must not be null`);
//! - relation resolution failures are 42P01, wrong relkinds 42809,
//!   missing schemas (attribute path) 3F000, missing columns 42703,
//!   system columns 0A000, missing MAINTAIN privilege 42501;
//! - the extended functions report recovery failures (missing
//!   objects, wrong relation, kind mismatches) as WARNING + false
//!   instead of errors.
//!
//! WARNING texts are pushed through `push_notice` (the schedule
//! harness does not score warning text, but the wire behavior is
//! PG's); return values and error codes are exact.

use super::*;
use crate::storage::{ExtExprStat, ExtStatData, ExtStatItem};

/// Expected-type tags for the pair-fill (PG's StatsArgInfo).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tag {
    Text,
    Int2,
    Int4,
    Float4,
    Bool,
    TextArray,
    Float4Array,
    Float8Array,
    /// pg_ndistinct / pg_dependencies / jsonb payloads: carried as
    /// text (or jsonb) values in this engine; accepted verbatim.
    Payload,
}

impl Tag {
    fn display(self) -> &'static str {
        match self {
            Tag::Text => "text",
            Tag::Int2 => "smallint",
            Tag::Int4 => "integer",
            Tag::Float4 => "real",
            Tag::Bool => "boolean",
            Tag::TextArray => "text[]",
            Tag::Float4Array => "real[]",
            Tag::Float8Array => "double precision[]",
            Tag::Payload => "text",
        }
    }
    fn matches(self, v: &Value) -> bool {
        match (self, v) {
            (Tag::Text, Value::Text(_)) => true,
            (Tag::Int2, Value::SmallInt(_)) => true,
            (Tag::Int4, Value::Int(_)) => true,
            (Tag::Float4, Value::Float4(_)) => true,
            (Tag::Bool, Value::Bool(_)) => true,
            (Tag::TextArray, Value::Array(av)) => {
                av.elem == crate::storage::ArrayElem::of(&ColType::Text)
            }
            (Tag::Float4Array, Value::Array(av)) => {
                av.elem == crate::storage::ArrayElem::of(&ColType::Float4)
            }
            (Tag::Float8Array, Value::Array(av)) => {
                av.elem == crate::storage::ArrayElem::of(&ColType::Float)
            }
            (Tag::Payload, Value::Text(_)) => true,
            (Tag::Payload, Value::Jsonb(_)) => true,
            _ => false,
        }
    }
}

/// PG's format_type_be spelling for a value, for warning text.
fn value_type_name(v: &Value) -> String {
    match v {
        Value::SmallInt(_) => "smallint".into(),
        Value::Int(_) => "integer".into(),
        Value::BigInt(_) => "bigint".into(),
        Value::Float4(_) => "real".into(),
        Value::Float(_) => "double precision".into(),
        Value::Numeric(_) => "numeric".into(),
        Value::Text(_) | Value::BpChar(_) => "text".into(),
        Value::Bool(_) => "boolean".into(),
        Value::Array(av) => format!("{}[]", elem_type_name(av.elem)),
        Value::Jsonb(_) => "jsonb".into(),
        Value::Date(_) => "date".into(),
        Value::Timestamp(_) => "timestamp without time zone".into(),
        Value::Timestamptz(_) => "timestamp with time zone".into(),
        Value::Interval(_) => "interval".into(),
        Value::Bytea(_) => "bytea".into(),
        Value::Uuid(_) => "uuid".into(),
        Value::Range(_) => "range".into(),
        Value::Multirange(_) => "multirange".into(),
        Value::TsVector(_) => "tsvector".into(),
        Value::Record(_) => "record".into(),
        _ => "text".into(),
    }
}

fn elem_type_name(e: crate::storage::ArrayElem) -> &'static str {
    use crate::storage::ArrayElem as A;
    match e {
        A::Bool => "boolean",
        A::SmallInt => "smallint",
        A::Int => "integer",
        A::BigInt => "bigint",
        A::Float4 => "real",
        A::Float => "double precision",
        A::Numeric => "numeric",
        A::Text => "text",
        A::Char => "character",
        A::Date => "date",
        A::Timestamp => "timestamp without time zone",
        A::Timestamptz => "timestamp with time zone",
        A::Interval => "interval",
        A::Bytea => "bytea",
        A::Uuid => "uuid",
        A::Json => "json",
        _ => "text",
    }
}

fn warn(msg: String) {
    push_notice(msg);
}

/// The filled pair map plus the aggregate success flag (PG's
/// stats_fill_fcinfo_from_arg_pairs result).
struct Filled {
    map: std::collections::HashMap<String, Value>,
    ok: bool,
}

impl Filled {
    fn get(&self, name: &str) -> Option<&Value> {
        self.map.get(name)
    }
    fn text(&self, name: &str) -> Option<String> {
        match self.map.get(name) {
            Some(Value::Text(s)) => Some(s.to_string()),
            _ => None,
        }
    }
}

/// Evaluate and fill PG's variadic name/value pairs against an
/// arginfo table. Errors follow stat_utils.c exactly.
fn fill_pairs(
    q: &mut Q,
    scopes: &[Scope],
    args: &[Expr],
    arginfo: &[(&str, Tag)],
) -> Result<Filled, ExecError> {
    if args.len() % 2 != 0 {
        return Err(exec_err_hint(
            "XX000",
            "variadic arguments must be name/value pairs",
            "Provide an even number of variadic arguments that can be divided into pairs.",
        ));
    }
    let mut vals: Vec<Value> = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_expr(q, scopes, a)?);
    }
    let mut out = Filled {
        map: std::collections::HashMap::new(),
        ok: true,
    };
    let mut i = 0usize;
    while i < vals.len() {
        let pos = i + 1; // 1-based variadic position of the name
        let name_v = &vals[i];
        if matches!(name_v, Value::Null) {
            return Err(exec_err(
                "XX000",
                format!("name at variadic position {} is null", pos),
            ));
        }
        let name = match name_v {
            Value::Text(s) => s.to_string(),
            other => {
                return Err(exec_err(
                    "XX000",
                    format!(
                        "name at variadic position {} has type {}, expected type text",
                        pos,
                        value_type_name(other)
                    ),
                ));
            }
        };
        let value = vals[i + 1].clone();
        i += 2;
        if matches!(value, Value::Null) {
            continue; // NULL value: argument left unset
        }
        if name.eq_ignore_ascii_case("version") {
            continue; // accepted but ignored
        }
        let Some((_, tag)) = arginfo.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)) else {
            warn(format!("unrecognized argument name: \"{}\"", name));
            out.ok = false;
            continue;
        };
        if !tag.matches(&value) {
            warn(format!(
                "argument \"{}\" has type {}, expected type {}",
                name.to_lowercase(),
                value_type_name(&value),
                tag.display()
            ));
            out.ok = false;
            continue;
        }
        out.map.insert(name.to_lowercase(), value);
    }
    Ok(out)
}

fn required_text(f: &Filled, name: &str) -> Result<String, ExecError> {
    f.text(name)
        .ok_or_else(|| exec_err("22023", format!("argument \"{}\" must not be null", name)))
}

fn required_bool(f: &Filled, name: &str) -> Result<bool, ExecError> {
    match f.get(name) {
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(exec_err(
            "22023",
            format!("argument \"{}\" must not be null", name),
        )),
    }
}

// ---------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------

/// A statistics-bearing relation resolved for the restore/clear
/// functions (independent of any pre-existing statistics).
struct Target {
    /// Key into Database.stats / stats_inh / rel_stats.
    key: String,
    /// Display (base) name.
    name: String,
    schema: String,
    is_index: bool,
    partitioned: bool,
    /// Privilege subject storage key (parent table for indexes).
    table_key: String,
    owner: String,
    /// (attnum, attname, type) in attnum order.
    cols: Vec<(i64, String, ColType)>,
    /// Domain name per `cols` entry (the column's declared domain,
    /// for PG's getBaseType unwrap); parallel to `cols`.
    domains: Vec<Option<String>>,
    /// Composite type name per `cols` entry (the column's declared
    /// composite type, for record_in parsing of MCV/histogram text);
    /// parallel to `cols`.
    composites: Vec<Option<String>>,
}

impl Target {
    fn default_rel_stats(&self) -> RelStats {
        if self.partitioned {
            RelStats {
                relpages: -1,
                ..RelStats::default()
            }
        } else if self.is_index {
            RelStats {
                relpages: 1,
                reltuples: 0.0,
                relallvisible: 0,
                relallfrozen: 0,
            }
        } else {
            RelStats::default()
        }
    }
    fn col_by_name(&self, attname: &str) -> Option<&(i64, String, ColType)> {
        self.cols.iter().find(|(_, n, _)| n == attname)
    }
    fn col_by_num(&self, attnum: i64) -> Option<&(i64, String, ColType)> {
        self.cols.iter().find(|(a, _, _)| *a == attnum)
    }
    /// The declared domain name of the column with `attnum`, if any.
    fn domain_for(&self, attnum: i64) -> Option<String> {
        let pos = self.cols.iter().position(|(a, _, _)| *a == attnum)?;
        self.domains.get(pos).cloned().flatten()
    }
    /// The declared composite type name of the column with
    /// `attnum`, if any.
    fn composite_for(&self, attnum: i64) -> Option<String> {
        let pos = self.cols.iter().position(|(a, _, _)| *a == attnum)?;
        self.composites.get(pos).cloned().flatten()
    }
    /// The stats-map key for one column: column name for tables,
    /// `expr:<attnum>` for index expression columns, the column
    /// name for index key columns.
    fn stats_key_of(&self, attnum: i64, attname: &str) -> String {
        if self.is_index && attname == "expr" {
            format!("expr:{}", attnum)
        } else {
            attname.to_string()
        }
    }
}

fn table_target(key: &str, t: &Table, schema: &str) -> Target {
    let mut cols = Vec::new();
    let mut domains = Vec::new();
    let mut composites = Vec::new();
    for (i, (cn, ty)) in t.columns.iter().enumerate() {
        let attnum = t.attnums.get(i).copied().unwrap_or(i as i16 + 1) as i64;
        cols.push((attnum, cn.clone(), ty.clone()));
        domains.push(t.domain_types.get(i).cloned().flatten());
        composites.push(t.composite_types.get(i).cloned().flatten());
    }
    Target {
        key: key.to_string(),
        name: key.rsplit('.').next().unwrap_or(key).to_string(),
        schema: schema.to_string(),
        is_index: false,
        partitioned: t.partition.as_ref().is_some_and(|p| p.is_partitioned),
        table_key: key.to_string(),
        owner: t.owner.clone(),
        cols,
        domains,
        composites,
    }
}

fn resolve_target(q: &Q, schema: &str, name: &str) -> Option<Target> {
    let db = &q.eng.db;
    // Temp tables live in the per-session store.
    if schema == "pg_temp" {
        if let Some(t) = db.temp_tables.get(&q.session).and_then(|m| m.get(name)) {
            return Some(table_target(name, t, "pg_temp"));
        }
    }
    for (key, _) in db.tables.iter() {
        let Some(t) = db.find_table(key, q.snap, &q.all_xids, q.session) else {
            continue;
        };
        let tschema = db
            .obj_schema
            .get(key)
            .cloned()
            .unwrap_or_else(|| "public".to_string());
        let tname = key.rsplit('.').next().unwrap_or(key.as_str());
        if tschema == schema && tname == name {
            return Some(table_target(key, t, &tschema));
        }
    }
    // Indexes (visible defs only).
    let mut inames: Vec<&String> = db.indexes.keys().collect();
    inames.sort();
    for iname in inames {
        let ix = &db.indexes[iname];
        let def = &ix.def;
        if def.dropped_xmax != 0 {
            continue;
        }
        let ischema = db
            .obj_schema
            .get(&def.table)
            .cloned()
            .unwrap_or_else(|| "public".to_string());
        let base = iname.rsplit('.').next().unwrap_or(iname.as_str());
        if ischema != schema || base != name {
            continue;
        }
        let table = db.find_table(&def.table, q.snap, &q.all_xids, q.session);
        let mut cols = Vec::new();
        let mut domains = Vec::new();
        let mut composites = Vec::new();
        for (i, cn) in def.col_names.iter().enumerate() {
            let attnum = i as i64 + 1;
            if def.exprs.get(i).and_then(|e| e.as_ref()).is_some() {
                cols.push((attnum, "expr".to_string(), ColType::Text));
                domains.push(None);
                composites.push(None);
            } else {
                let ty = table
                    .and_then(|t| def.cols.get(i).and_then(|p| t.columns.get(*p)))
                    .map(|(_, ty)| ty.clone())
                    .unwrap_or(ColType::Text);
                cols.push((attnum, cn.clone(), ty));
                domains.push(table.and_then(|t| {
                    def.cols
                        .get(i)
                        .and_then(|p| t.domain_types.get(*p).cloned().flatten())
                }));
                composites.push(table.and_then(|t| {
                    def.cols
                        .get(i)
                        .and_then(|p| t.composite_types.get(*p).cloned().flatten())
                }));
            }
        }
        let partitioned =
            table.is_some_and(|t| t.partition.as_ref().is_some_and(|p| p.is_partitioned));
        return Some(Target {
            key: iname.clone(),
            name: base.to_string(),
            schema: ischema,
            is_index: true,
            partitioned,
            table_key: def.table.clone(),
            owner: table.map(|t| t.owner.clone()).unwrap_or_default(),
            cols,
            domains,
            composites,
        });
    }
    None
}

/// MAINTAIN privilege on the stats target (PG's rule for all six
/// functions): owner, superuser, or an explicit MAINTAIN grant.
fn check_maintain(q: &Q, t: &Target) -> Result<(), ExecError> {
    if crate::storage::is_superuser_snap(&q.eng.db, q.role, q.snap, q.own) || q.role == "postgres" {
        return Ok(());
    }
    if t.owner == q.role {
        return Ok(());
    }
    if let Some(tab) = q
        .eng
        .db
        .find_table(&t.table_key, q.snap, &q.all_xids, q.session)
    {
        let closure = crate::storage::role_closure(&q.eng.db, q.role, q.snap, q.own);
        let privs = crate::storage::table_privs_in(&q.eng.db, q.role, tab, &closure, q.snap, q.own);
        if privs & PRIV_MAINTAIN != 0 {
            return Ok(());
        }
    }
    Err(exec_err(
        "42501",
        format!("permission denied for table {}", t.name),
    ))
}

/// Resolve (schema, name) with PG's error surfaces: 42P01 when
/// nothing by that name exists, 42809 when it exists but cannot
/// carry statistics (view, sequence). `schema_check` (the
/// attribute path) first raises 3F000 for an unknown schema.
fn resolve_for_stats(
    q: &Q,
    schema: &str,
    name: &str,
    schema_check: bool,
) -> Result<Target, ExecError> {
    let db = &q.eng.db;
    if schema_check
        && schema != "pg_temp"
        && schema != "public"
        && !db.schemas.iter().any(|s| s == schema)
    {
        return Err(exec_err(
            "3F000",
            format!("schema \"{}\" does not exist", schema),
        ));
    }
    if let Some(t) = resolve_target(q, schema, name) {
        return Ok(t);
    }
    let key = if schema == "public" || schema == "pg_temp" {
        name.to_string()
    } else {
        format!("{}.{}", schema, name)
    };
    let wrong_kind = db.find_view(&key, q.snap, q.own).is_some()
        || db.sequences.contains_key(&key)
        || db.sequences.contains_key(name);
    if wrong_kind {
        return Err(exec_err(
            "42809",
            format!("cannot modify statistics for relation \"{}\"", name),
        ));
    }
    Err(exec_err(
        "42P01",
        format!("relation \"{}.{}\" does not exist", schema, name),
    ))
}

/// Entry point from eval_func's engine-access intercept.
pub(crate) fn eval_stat_restore_func(
    q: &mut Q,
    scopes: &[Scope],
    name: &str,
    args: &[Expr],
) -> Result<Value, ExecError> {
    match name {
        "pg_restore_relation_stats" => restore_relation_stats(q, scopes, args),
        "pg_clear_relation_stats" => clear_relation_stats(q, scopes, args),
        "pg_restore_attribute_stats" => restore_attribute_stats(q, scopes, args),
        "pg_clear_attribute_stats" => clear_attribute_stats(q, scopes, args),
        "pg_restore_extended_stats" => restore_extended_stats(q, scopes, args),
        "pg_clear_extended_stats" => clear_extended_stats(q, scopes, args),
        _ => Err(exec_err(
            "42883",
            format!("function {} does not exist", name),
        )),
    }
}

// ---------------------------------------------------------------------
// Relation statistics
// ---------------------------------------------------------------------

const REL_ARGINFO: &[(&str, Tag)] = &[
    ("schemaname", Tag::Text),
    ("relname", Tag::Text),
    ("relpages", Tag::Int4),
    ("reltuples", Tag::Float4),
    ("relallvisible", Tag::Int4),
    ("relallfrozen", Tag::Int4),
];

fn restore_relation_stats(q: &mut Q, scopes: &[Scope], args: &[Expr]) -> Result<Value, ExecError> {
    let filled = fill_pairs(q, scopes, args, REL_ARGINFO)?;
    let schema = required_text(&filled, "schemaname")?;
    let relname = required_text(&filled, "relname")?;
    let rel = resolve_for_stats(q, &schema, &relname, false)?;
    check_maintain(q, &rel)?;
    let mut ok = filled.ok;
    let mut rs = q
        .eng
        .db
        .rel_stats
        .get(&rel.key)
        .copied()
        .unwrap_or_else(|| rel.default_rel_stats());
    if let Some(Value::Int(n)) = filled.get("relpages") {
        rs.relpages = *n;
    }
    if let Some(Value::Float4(f)) = filled.get("reltuples") {
        let f = *f;
        if !f.is_finite() {
            // PG: WARNING + the field is rejected, others apply.
            warn("argument \"reltuples\" must be a finite value".into());
            ok = false;
        } else if f < -1.0 {
            warn("argument \"reltuples\" must not be less than -1.0".into());
            ok = false;
        } else {
            rs.reltuples = f;
        }
    }
    if let Some(Value::Int(n)) = filled.get("relallvisible") {
        rs.relallvisible = *n;
    }
    if let Some(Value::Int(n)) = filled.get("relallfrozen") {
        rs.relallfrozen = *n;
    }
    q.eng.db.rel_stats.insert(rel.key.clone(), rs);
    Ok(Value::Bool(ok))
}

/// The two text arguments of a clear function (already reordered
/// to positional by named-argument resolution).
fn clear_text_args(
    q: &mut Q,
    scopes: &[Scope],
    args: &[Expr],
    names: &[&str],
    sig: &str,
) -> Result<Vec<Option<String>>, ExecError> {
    if args.len() != names.len() {
        return Err(exec_err(
            "42883",
            format!("function {} does not exist", sig),
        ));
    }
    let mut out = Vec::new();
    for (a, n) in args.iter().zip(names.iter()) {
        let v = eval_expr(q, scopes, a)?;
        match v {
            Value::Text(s) => out.push(Some(s.to_string())),
            Value::Null => {
                return Err(exec_err(
                    "22023",
                    format!("argument \"{}\" must not be null", n),
                ));
            }
            _ => {
                return Err(exec_err(
                    "42883",
                    format!("function {} does not exist", sig),
                ));
            }
        }
    }
    Ok(out)
}

fn clear_relation_stats(q: &mut Q, scopes: &[Scope], args: &[Expr]) -> Result<Value, ExecError> {
    let a = clear_text_args(
        q,
        scopes,
        args,
        &["schemaname", "relname"],
        "pg_clear_relation_stats(text, text)",
    )?;
    let schema = a[0].clone().unwrap();
    let relname = a[1].clone().unwrap();
    let rel = resolve_for_stats(q, &schema, &relname, false)?;
    check_maintain(q, &rel)?;
    q.eng.db.rel_stats.insert(
        rel.key.clone(),
        RelStats {
            relpages: 0,
            reltuples: -1.0,
            relallvisible: 0,
            relallfrozen: 0,
        },
    );
    Ok(Value::Null)
}

// ---------------------------------------------------------------------
// Attribute statistics
// ---------------------------------------------------------------------

const ATT_ARGINFO: &[(&str, Tag)] = &[
    ("schemaname", Tag::Text),
    ("relname", Tag::Text),
    ("attname", Tag::Text),
    ("attnum", Tag::Int2),
    ("inherited", Tag::Bool),
    ("null_frac", Tag::Float4),
    ("avg_width", Tag::Int4),
    ("n_distinct", Tag::Float4),
    ("most_common_vals", Tag::Text),
    ("most_common_freqs", Tag::Float4Array),
    ("histogram_bounds", Tag::Text),
    ("correlation", Tag::Float4),
    ("most_common_elems", Tag::Text),
    ("most_common_elem_freqs", Tag::Float4Array),
    ("elem_count_histogram", Tag::Float4Array),
    ("range_length_histogram", Tag::Text),
    ("range_empty_frac", Tag::Float4),
    ("range_bounds_histogram", Tag::Text),
];

const SYSTEM_COLS: &[&str] = &[
    "ctid", "xmin", "xmax", "cmin", "cmax", "tableoid", "cmult", "cchunk",
];

/// Domain-unwrap a column type (PG's getBaseType): a domain column
/// behaves as its base type for statistics purposes. The domain name
/// travels outside `ColType` in this engine (TableDef::domain_types),
/// so callers pass it in; `DomainDef::base` was resolved at CREATE
/// DOMAIN time, with `base_domain` naming an inner domain to recurse
/// into first.
fn unwrap_domain(db: &Database, ct: &ColType, dom: Option<&str>) -> ColType {
    let Some(mut name) = dom.map(str::to_string) else {
        return ct.clone();
    };
    for _ in 0..8 {
        let Some(d) = db.find_type(&name).and_then(|st| st.domain.as_ref()) else {
            return ct.clone();
        };
        if let Some(inner) = &d.base_domain {
            name = inner.clone();
            continue;
        }
        return d.base.clone();
    }
    ct.clone()
}

/// Parse a 1-D array literal of `ty` elements (PG's array_in via
/// statatt_build_stavalues). Errors carry PG's input messages.
/// v2.35: array_in for a statistics column. A composite-typed
/// column's elements are record literals: parse the literal as
/// text elements and run each through record_in against the
/// column's declared composite type (PG's array_in over the
/// composite's input function). Everything else defers to
/// array_in_typed.
fn array_in_column(
    db: &Database,
    text: &str,
    ty: &ColType,
    comp: Option<&str>,
) -> Result<Vec<Value>, ExecError> {
    if matches!(ty, ColType::Composite) {
        if let Some(name) = comp {
            if let Some(st) = db.find_type(name) {
                if let Some(fields) = &st.composite {
                    let av = parse_array_literal(text, crate::storage::ArrayElem::Text)?;
                    if av.dims.len() > 1 {
                        return Err(exec_err(
                            "22P02",
                            "could not parse array: incorrect number of dimensions".to_string(),
                        ));
                    }
                    let mut out = Vec::with_capacity(av.elems.len());
                    for e in &av.elems {
                        match e {
                            Value::Null => out.push(Value::Null),
                            Value::Text(s) => {
                                let vals =
                                    super::eval::parse_record_literal(s, name, fields, &db.types)?;
                                out.push(Value::Record(
                                    fields.iter().map(|(n, _, _)| n.clone()).zip(vals).collect(),
                                ));
                            }
                            _ => {
                                return Err(exec_err(
                                    "22P02",
                                    "could not parse array: element is not text".to_string(),
                                ));
                            }
                        }
                    }
                    return Ok(out);
                }
            }
        }
    }
    array_in_typed(text, ty)
}

fn array_in_typed(text: &str, ty: &ColType) -> Result<Vec<Value>, ExecError> {
    // v2.35: range/multirange elements have no ArrayElem variant
    // (ArrayElem::of maps them to Record, and text->record casts
    // fail); parse the literal as text elements and run each through
    // the range input function, like PG's array_in over range_in.
    if let ColType::Range(e) | ColType::Multirange(e) = ty {
        let e = *e;
        let av = parse_array_literal(text, crate::storage::ArrayElem::Text)?;
        if av.dims.len() > 1 {
            return Err(exec_err(
                "22P02",
                "could not parse array: incorrect number of dimensions".to_string(),
            ));
        }
        let mut out = Vec::with_capacity(av.elems.len());
        for el in &av.elems {
            match el {
                Value::Null => out.push(Value::Null),
                Value::Text(t) | Value::BpChar(t) => {
                    if matches!(ty, ColType::Range(_)) {
                        out.push(Value::range(super::ranges::parse_range(
                            e,
                            None,
                            &t.to_string(),
                        )?));
                    } else {
                        out.push(Value::multirange(super::ranges::parse_multirange(
                            e,
                            None,
                            &t.to_string(),
                        )?));
                    }
                }
                _ => {
                    return Err(exec_err(
                        "22P02",
                        "could not parse array: element is not text".to_string(),
                    ));
                }
            }
        }
        return Ok(out);
    }
    // v2.35: an array-typed column's statistics literal is a
    // multi-dimensional array datum (PG array_in over the column
    // type): parse with the SCALAR element kind and regroup the
    // flat row-major elements into one sub-array Value per
    // top-level element (e.g. histogram bounds of an int[]
    // expression: {"{1,1}","{2,1}"} -> two int[] values).
    if let ColType::Array(scalar) = ty {
        let av = match parse_array_literal(text, *scalar) {
            Ok(av) => av,
            Err(e) => {
                // v2.35: the quoted sub-array form array_out emits
                // for multi-dimensional data ({"{1,1}","{2,1}"}):
                // the scalar parse chokes on "{1,1}" as an integer.
                // Parse structurally as text; when every element is
                // itself an array literal, parse each as a sub-array
                // (PG array_in treats it as the nested datum).
                let regrouped: Option<Vec<Value>> = (|| {
                    let tav = parse_array_literal(text, crate::storage::ArrayElem::Text).ok()?;
                    if tav.dims.len() != 1 || tav.elems.is_empty() {
                        return None;
                    }
                    if !tav.elems.iter().all(|v| {
                        matches!(v, Value::Null)
                            || matches!(v, Value::Text(t) if t.trim_start().starts_with('{'))
                    }) {
                        return None;
                    }
                    let mut out = Vec::with_capacity(tav.elems.len());
                    let mut sub_dims: Option<Vec<i32>> = None;
                    for el in &tav.elems {
                        match el {
                            Value::Null => out.push(Value::Null),
                            Value::Text(t) => {
                                let sub = parse_array_literal(t.as_ref(), *scalar).ok()?;
                                if let Some(d) = &sub_dims {
                                    if *d != sub.dims {
                                        return None;
                                    }
                                } else {
                                    sub_dims = Some(sub.dims.clone());
                                }
                                out.push(Value::Array(Box::new(sub)));
                            }
                            _ => return None,
                        }
                    }
                    Some(out)
                })();
                match regrouped {
                    Some(out) => return Ok(out),
                    None => return Err(e),
                }
            }
        };
        if av.dims.len() > 1 {
            let sub: usize = av.dims[1..].iter().map(|d| *d as usize).product();
            if sub == 0 || av.elems.len() % sub != 0 {
                return Err(exec_err(
                    "22P02",
                    "could not parse array: incorrect number of dimensions".to_string(),
                ));
            }
            let mut out = Vec::with_capacity(av.elems.len() / sub);
            for chunk in av.elems.chunks(sub) {
                out.push(Value::Array(Box::new(crate::storage::ArrayVal {
                    elem: av.elem,
                    dims: av.dims[1..].to_vec(),
                    lower: av.lower[1..].to_vec(),
                    elems: chunk.to_vec(),
                })));
            }
            return Ok(out);
        }
        return Ok(av.elems);
    }
    let av = parse_array_literal(text, crate::storage::ArrayElem::of(ty))?;
    if av.dims.len() > 1 {
        return Err(exec_err(
            "22P02",
            "could not parse array: incorrect number of dimensions".to_string(),
        ));
    }
    Ok(av.elems)
}

/// The float elements of a float4[] argument value.
fn f4_elems(v: &Value) -> Vec<Option<f64>> {
    match v {
        Value::Array(av) => av
            .elems
            .iter()
            .map(|e| match e {
                Value::Float4(f) => Some(*f as f64),
                Value::Float(f) => Some(*f),
                Value::Int(i) => Some(*i as f64),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse a float8 array literal (range_length_histogram stanumbers
/// are stored from a text array of float spellings, including
/// "Infinity").
fn parse_f8_list(text: &str) -> Result<Vec<f64>, ExecError> {
    let t = text.trim();
    let inner = t
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| exec_err("22P02", format!("malformed array literal: {:?}", text)))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for tok in inner.split(',') {
        let tok = tok.trim().trim_matches('"');
        let f = crate::exec::pg_input::pg_float8(tok).ok_or_else(|| {
            exec_err(
                "22P02",
                format!("invalid input syntax for type double precision: {:?}", tok),
            )
        })?;
        out.push(f);
    }
    Ok(out)
}

/// Import the statistics kinds of one attribute restore into `cs`.
/// Every failed kind warns and flips the aggregate result, leaving
/// the kind unchanged (PG's statatt_* import discipline).
fn import_att_kinds(
    db: &Database,
    attname: &str,
    ty: &ColType,
    dom: Option<&str>,
    comp: Option<&str>,
    filled: &Filled,
    cs: &mut ColStats,
) -> bool {
    let mut ok = true;
    let base_ty = unwrap_domain(db, ty, dom);
    // Scalars (stored verbatim, no range checks).
    if let Some(Value::Float4(f)) = filled.get("null_frac") {
        cs.null_frac = *f as f64;
    }
    if let Some(Value::Int(n)) = filled.get("avg_width") {
        cs.avg_width = *n as i32;
    }
    if let Some(Value::Float4(f)) = filled.get("n_distinct") {
        cs.distinct_raw = *f as f64;
        cs.n_distinct = if *f > 0.0 { *f as f64 } else { 0.0 };
    }
    if let Some(Value::Float4(f)) = filled.get("correlation") {
        cs.correlation = Some(*f as f64);
    }
    // MCV pair.
    let has_vals = filled.get("most_common_vals").is_some();
    let has_freqs = filled.get("most_common_freqs").is_some();
    if has_vals != has_freqs {
        let (missing, given) = if has_vals {
            ("most_common_freqs", "most_common_vals")
        } else {
            ("most_common_vals", "most_common_freqs")
        };
        warn(format!(
            "argument \"{}\" must be specified when argument \"{}\" is specified",
            missing, given
        ));
        ok = false;
    } else if has_vals {
        let text = filled.text("most_common_vals").unwrap();
        match array_in_column(db, &text, &base_ty, comp) {
            Err(e) => {
                warn(e.message.clone());
                ok = false;
            }
            Ok(vals) => {
                let freqs = f4_elems(filled.get("most_common_freqs").unwrap());
                if freqs.iter().any(|f| f.is_none()) {
                    warn("could not parse array \"most_common_freqs\": NULL value found".into());
                    ok = false;
                } else if vals.len() != freqs.len() {
                    warn("could not parse \"most_common_vals\": incorrect number of elements (same as \"most_common_freqs\" required)".into());
                    ok = false;
                } else {
                    cs.mcv = vals
                        .into_iter()
                        .zip(freqs.iter().map(|f| f.unwrap()))
                        .collect();
                }
            }
        }
    }
    // Histogram.
    if let Some(text) = filled.text("histogram_bounds") {
        match array_in_column(db, &text, &base_ty, comp) {
            Err(e) => {
                warn(e.message.clone());
                ok = false;
            }
            Ok(vals) => {
                if vals.iter().any(|v| matches!(v, Value::Null)) {
                    warn("could not parse array \"histogram_bounds\": NULL value found".into());
                    ok = false;
                } else {
                    cs.hist_bounds = vals;
                }
            }
        }
    }
    // Element kinds need an element type (array / tsvector).
    let elem_ty = elem_type_of(&base_ty).map(|(et, _)| et);
    let has_elems = filled.get("most_common_elems").is_some();
    let has_efreqs = filled.get("most_common_elem_freqs").is_some();
    if has_elems != has_efreqs {
        let (missing, given) = if has_elems {
            ("most_common_elem_freqs", "most_common_elems")
        } else {
            ("most_common_elems", "most_common_elem_freqs")
        };
        warn(format!(
            "argument \"{}\" must be specified when argument \"{}\" is specified",
            missing, given
        ));
        ok = false;
    } else if has_elems {
        match &elem_ty {
            None => {
                warn(format!(
                    "could not determine element type of column \"{}\"",
                    attname
                ));
                ok = false;
            }
            Some(ety) => {
                let text = filled.text("most_common_elems").unwrap();
                match array_in_typed(&text, ety) {
                    Err(e) => {
                        warn(e.message.clone());
                        ok = false;
                    }
                    Ok(vals) => {
                        let freqs = f4_elems(filled.get("most_common_elem_freqs").unwrap());
                        if freqs.iter().any(|f| f.is_none()) {
                            warn("could not parse array \"most_common_elem_freqs\": NULL value found".into());
                            ok = false;
                        } else {
                            // v2.35: PG19 imposes NO length relation
                            // between most_common_elems and its freqs
                            // (tsvector freqs legitimately carry extra
                            // entries); the old vals+3 rule rejected
                            // valid restores (stats_import test_dom_ts).
                            cs.mcelem = Some((vals, freqs.iter().map(|f| f.unwrap()).collect()));
                        }
                    }
                }
            }
        }
    }
    if let Some(v) = filled.get("elem_count_histogram") {
        match &elem_ty {
            None => {
                warn(format!(
                    "could not determine element type of column \"{}\"",
                    attname
                ));
                ok = false;
            }
            Some(_) => {
                let freqs = f4_elems(v);
                if freqs.iter().any(|f| f.is_none()) {
                    warn("could not parse array \"elem_count_histogram\": NULL value found".into());
                    ok = false;
                } else {
                    cs.dechist = Some(freqs.iter().map(|f| f.unwrap()).collect());
                }
            }
        }
    }
    // Range kinds.
    let is_range = matches!(base_ty, ColType::Range(_) | ColType::Multirange(_));
    let has_len = filled.get("range_length_histogram").is_some();
    let has_empty = filled.get("range_empty_frac").is_some();
    if has_len != has_empty {
        let (missing, given) = if has_len {
            ("range_empty_frac", "range_length_histogram")
        } else {
            ("range_length_histogram", "range_empty_frac")
        };
        warn(format!(
            "argument \"{}\" must be specified when argument \"{}\" is specified",
            missing, given
        ));
        ok = false;
    } else if has_len {
        if !is_range {
            warn(format!("column \"{}\" is not a range type", attname));
            ok = false;
        } else {
            let text = filled.text("range_length_histogram").unwrap();
            match parse_f8_list(&text) {
                Err(e) => {
                    warn(e.message.clone());
                    ok = false;
                }
                Ok(bounds) => {
                    cs.range_length_hist = Some(bounds);
                    if let Some(Value::Float4(f)) = filled.get("range_empty_frac") {
                        cs.range_empty_frac = Some(*f as f64);
                    }
                }
            }
        }
    }
    if let Some(text) = filled.text("range_bounds_histogram") {
        if !is_range {
            warn(format!("column \"{}\" is not a range type", attname));
            ok = false;
        } else {
            // Bounds are values of the RANGE type (for a multirange
            // column, of its range type).
            let rty = match &base_ty {
                ColType::Multirange(e) => ColType::Range(*e),
                _ => base_ty.clone(),
            };
            match array_in_typed(&text, &rty) {
                Err(e) => {
                    warn(e.message.clone());
                    ok = false;
                }
                Ok(vals) => {
                    if vals.iter().any(|v| matches!(v, Value::Null)) {
                        warn(
                            "could not parse array \"range_bounds_histogram\": NULL value found"
                                .into(),
                        );
                        ok = false;
                    } else {
                        cs.bounds_hist = Some(vals);
                    }
                }
            }
        }
    }
    ok
}

/// Resolve the target column of an attribute restore/clear from
/// the attname/attnum pair, with PG's error surfaces.
fn resolve_att_col(
    target: &Target,
    attname: Option<&str>,
    attnum: Option<i64>,
    clear: bool,
) -> Result<(i64, String, ColType, Option<String>, Option<String>), ExecError> {
    if attname.is_some() && attnum.is_some() {
        return Err(exec_err(
            "22023",
            "cannot specify both \"attname\" and \"attnum\"",
        ));
    }
    if attname.is_none() && attnum.is_none() {
        return Err(exec_err(
            "22023",
            "must specify either \"attname\" or \"attnum\"",
        ));
    }
    let verb = if clear { "clear" } else { "modify" };
    if let Some(an) = attname {
        if SYSTEM_COLS.contains(&an) {
            return Err(exec_err(
                "0A000",
                format!("cannot {} statistics on system column \"{}\"", verb, an),
            ));
        }
        if let Some(c) = target.col_by_name(an) {
            let dom = target.domain_for(c.0);
            let comp = target.composite_for(c.0);
            return Ok((c.0, c.1.clone(), c.2.clone(), dom, comp));
        }
        return Err(exec_err(
            "42703",
            format!(
                "column \"{}\" of relation \"{}\" does not exist",
                an, target.name
            ),
        ));
    }
    let an = attnum.unwrap();
    if an <= 0 {
        let sysname = match an {
            -1 => "ctid",
            -2 => "oid",
            -3 => "xmin",
            -4 => "cmin",
            -5 => "xmax",
            -6 => "cmax",
            -7 => "tableoid",
            _ => "",
        };
        if !sysname.is_empty() {
            return Err(exec_err(
                "0A000",
                format!(
                    "cannot {} statistics on system column \"{}\"",
                    verb, sysname
                ),
            ));
        }
        return Err(exec_err(
            "42703",
            format!(
                "column {} of relation \"{}\" does not exist",
                an, target.name
            ),
        ));
    }
    if let Some(c) = target.col_by_num(an) {
        let dom = target.domain_for(c.0);
        let comp = target.composite_for(c.0);
        return Ok((c.0, c.1.clone(), c.2.clone(), dom, comp));
    }
    Err(exec_err(
        "42703",
        format!(
            "column {} of relation \"{}\" does not exist",
            an, target.name
        ),
    ))
}

fn restore_attribute_stats(q: &mut Q, scopes: &[Scope], args: &[Expr]) -> Result<Value, ExecError> {
    let filled = fill_pairs(q, scopes, args, ATT_ARGINFO)?;
    let schema = required_text(&filled, "schemaname")?;
    let relname = required_text(&filled, "relname")?;
    let target = resolve_for_stats(q, &schema, &relname, true)?;
    check_maintain(q, &target)?;
    let attname = filled.text("attname");
    let attnum = match filled.get("attnum") {
        Some(Value::SmallInt(n)) => Some(*n as i64),
        _ => None,
    };
    let (attnum_v, attname_v, ty, dom, comp) =
        resolve_att_col(&target, attname.as_deref(), attnum, false)?;
    let inherited = required_bool(&filled, "inherited")?;
    let skey = target.stats_key_of(attnum_v, &attname_v);
    // The existing ColStats is cloned out first: import_att_kinds
    // borrows the db immutably, which cannot overlap the stats map's
    // mutable borrow.
    let mut cs = {
        let map = if inherited {
            &q.eng.db.stats_inh
        } else {
            &q.eng.db.stats
        };
        map.get(&target.key)
            .and_then(|ts| ts.cols.get(&skey))
            .cloned()
            .unwrap_or_default()
    };
    let kinds_ok = import_att_kinds(
        &q.eng.db,
        &attname_v,
        &ty,
        dom.as_deref(),
        comp.as_deref(),
        &filled,
        &mut cs,
    );
    let map = if inherited {
        &mut q.eng.db.stats_inh
    } else {
        &mut q.eng.db.stats
    };
    map.entry(target.key.clone())
        .or_default()
        .cols
        .insert(skey, cs);
    Ok(Value::Bool(filled.ok && kinds_ok))
}

fn clear_attribute_stats(q: &mut Q, scopes: &[Scope], args: &[Expr]) -> Result<Value, ExecError> {
    if args.len() != 5 {
        return Err(exec_err(
            "42883",
            "function pg_clear_attribute_stats(text, text, text, smallint, boolean) does not exist",
        ));
    }
    let mut vals: Vec<Value> = Vec::with_capacity(5);
    for a in args {
        vals.push(eval_expr(q, scopes, a)?);
    }
    let text_arg = |i: usize, n: &str| -> Result<String, ExecError> {
        match &vals[i] {
            Value::Text(s) => Ok(s.to_string()),
            Value::Null => Err(exec_err(
                "22023",
                format!("argument \"{}\" must not be null", n),
            )),
            _ => Err(exec_err(
                "42883",
                "function pg_clear_attribute_stats does not exist",
            )),
        }
    };
    let schema = text_arg(0, "schemaname")?;
    let relname = text_arg(1, "relname")?;
    let attname = match &vals[2] {
        Value::Text(s) => Some(s.to_string()),
        Value::Null => None,
        _ => {
            return Err(exec_err(
                "42883",
                "function pg_clear_attribute_stats does not exist",
            ));
        }
    };
    let attnum = match &vals[3] {
        Value::SmallInt(n) => Some(*n as i64),
        Value::Int(n) => Some(*n),
        Value::Null => None,
        _ => {
            return Err(exec_err(
                "42883",
                "function pg_clear_attribute_stats does not exist",
            ));
        }
    };
    let inherited = match &vals[4] {
        Value::Bool(b) => *b,
        Value::Null => return Err(exec_err("22023", "argument \"inherited\" must not be null")),
        _ => {
            return Err(exec_err(
                "42883",
                "function pg_clear_attribute_stats does not exist",
            ));
        }
    };
    let target = resolve_for_stats(q, &schema, &relname, false)?;
    check_maintain(q, &target)?;
    let (attnum_v, attname_v, _, _, _) =
        resolve_att_col(&target, attname.as_deref(), attnum, true)?;
    let skey = target.stats_key_of(attnum_v, &attname_v);
    let map = if inherited {
        &mut q.eng.db.stats_inh
    } else {
        &mut q.eng.db.stats
    };
    if let Some(ts) = map.get_mut(&target.key) {
        ts.cols.remove(&skey);
    }
    Ok(Value::Null)
}

// ---------------------------------------------------------------------
// Extended statistics
// ---------------------------------------------------------------------

const EXT_ARGINFO: &[(&str, Tag)] = &[
    ("schemaname", Tag::Text),
    ("relname", Tag::Text),
    ("statistics_schemaname", Tag::Text),
    ("statistics_name", Tag::Text),
    ("inherited", Tag::Bool),
    ("n_distinct", Tag::Payload),
    ("dependencies", Tag::Payload),
    ("most_common_vals", Tag::TextArray),
    ("most_common_freqs", Tag::Float8Array),
    ("most_common_base_freqs", Tag::Float8Array),
    ("exprs", Tag::Payload),
];

fn expr_stat_of_colstats(cs: &ColStats) -> ExtExprStat {
    ExtExprStat {
        null_frac: cs.null_frac,
        avg_width: cs.avg_width,
        n_distinct: cs.n_distinct,
        mcv: cs.mcv.clone(),
        distinct_raw: cs.distinct_raw,
        hist_bounds: cs.hist_bounds.clone(),
        correlation: cs.correlation,
        mcelem: cs.mcelem.clone(),
        dechist: cs.dechist.clone(),
        range_length_hist: cs.range_length_hist.clone(),
        range_empty_frac: cs.range_empty_frac,
        bounds_hist: cs.bounds_hist.clone(),
    }
}

/// The shared front half of the extended restore/clear: required
/// args (22023), relation resolution (3F000 / 42P01 / 42809
/// errors), MAINTAIN (42501), then the warning-path object
/// resolution. Returns None when a warning path already reported
/// (the caller returns false / void).
fn resolve_ext_target(
    q: &mut Q,
    filled: &Filled,
    verb: &str,
) -> Result<Option<(Target, usize)>, ExecError> {
    let schema = required_text(filled, "schemaname")?;
    let relname = required_text(filled, "relname")?;
    let sschema = required_text(filled, "statistics_schemaname")?;
    let sname = required_text(filled, "statistics_name")?;
    let _ = required_bool(filled, "inherited")?;
    let target = resolve_for_stats(q, &schema, &relname, true)?;
    check_maintain(q, &target)?;
    let db = &q.eng.db;
    if sschema != "public" && !db.schemas.iter().any(|s| s == &sschema) {
        warn(format!("could not find schema \"{}\"", sschema));
        return Ok(None);
    }
    let Some(pos) = db
        .ext_stats
        .iter()
        .position(|o| o.name == sname && o.schema == sschema)
    else {
        warn(format!(
            "could not find extended statistics object \"{}.{}\"",
            sschema, sname
        ));
        return Ok(None);
    };
    let obj = &db.ext_stats[pos];
    let (oschema, oname) = match obj.table.rsplit_once('.') {
        Some((s, n)) => (Some(s.to_string()), n.to_string()),
        None => (None, obj.table.clone()),
    };
    if oname != target.name || oschema.is_some_and(|s| s != target.schema) {
        warn(format!(
            "could not {} extended statistics object \"{}.{}\": incorrect relation \"{}.{}\" specified",
            verb, sschema, sname, schema, relname
        ));
        return Ok(None);
    }
    Ok(Some((target, pos)))
}

fn payload_text(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.to_string()),
        Value::Jsonb(s) => Some(s.to_string()),
        _ => None,
    }
}

fn json_num(v: &JsonbVal) -> Option<f64> {
    match v {
        JsonbVal::Number(n) => Some(n.to_f64()),
        _ => None,
    }
}

fn json_ints(v: &JsonbVal) -> Option<Vec<i32>> {
    match v {
        JsonbVal::Array(items) => {
            let mut out = Vec::new();
            for it in items {
                out.push(json_num(it)? as i32);
            }
            Some(out)
        }
        _ => None,
    }
}

fn obj_get<'a>(o: &'a [(String, JsonbVal)], key: &str) -> Option<&'a JsonbVal> {
    o.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Validate + canonicalize an ndistinct / dependencies payload
/// against the object's attribute-number set. On failure the
/// caller warns and rejects the argument wholesale.
fn import_ndistinct(
    text: &str,
    valid: &std::collections::HashSet<i32>,
) -> Option<(Vec<(Vec<i32>, f64)>, String)> {
    let parsed = parse_jsonb(text).ok()?;
    let JsonbVal::Array(items) = parsed else {
        return None;
    };
    let mut out = Vec::new();
    let mut parts = Vec::new();
    for it in &items {
        let JsonbVal::Object(o) = it else { return None };
        let atts = json_ints(obj_get(o, "attributes")?)?;
        if atts.iter().any(|a| !valid.contains(a)) {
            return None;
        }
        let nd = json_num(obj_get(o, "ndistinct")?)?;
        parts.push(format!(
            "{{\"attributes\": [{}], \"ndistinct\": {}}}",
            atts.iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            fmt_num(nd)
        ));
        out.push((atts, nd));
    }
    Some((out, format!("[{}]", parts.join(", "))))
}

fn import_dependencies(
    text: &str,
    valid: &std::collections::HashSet<i32>,
) -> Option<(Vec<(Vec<i32>, i32, f64)>, String)> {
    let parsed = parse_jsonb(text).ok()?;
    let JsonbVal::Array(items) = parsed else {
        return None;
    };
    let mut out = Vec::new();
    let mut parts = Vec::new();
    for it in &items {
        let JsonbVal::Object(o) = it else { return None };
        let atts = json_ints(obj_get(o, "attributes")?)?;
        let dep = json_num(obj_get(o, "dependency")?)? as i32;
        if atts.iter().any(|a| !valid.contains(a)) || !valid.contains(&dep) {
            return None;
        }
        let degree = json_num(obj_get(o, "degree")?)?;
        parts.push(format!(
            "{{\"attributes\": [{}], \"dependency\": {}, \"degree\": {:.6}}}",
            atts.iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            dep,
            degree
        ));
        out.push((atts, dep, degree));
    }
    Some((out, format!("[{}]", parts.join(", "))))
}

/// PG float output for integral/fractional payload numbers.
fn fmt_num(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{}", f)
    }
}

/// v2.35: targeted static typing for statistics expressions,
/// covering the shapes expr_type cannot resolve because QCol
/// schemas do not carry composite type NAMES: field access on a
/// composite-typed column of the target table (via
/// TableDef.composite_types) or on a named cast, and array
/// constructors over those. Anything else returns None and the
/// caller falls back to expr_type / first-row evaluation.
fn statics_expr_type(q: &Q, t: &Table, e: &Expr) -> Option<ColType> {
    match e {
        Expr::Column { name, .. } => t
            .columns
            .iter()
            .find(|(cn, _)| cn == name)
            .map(|(_, ty)| ty.clone()),
        Expr::FieldAccess { expr, field } => {
            let type_name: Option<String> = match &**expr {
                Expr::Column { name, .. } => t
                    .columns
                    .iter()
                    .position(|(cn, _)| cn == name)
                    .and_then(|i| t.composite_types.get(i).cloned().flatten()),
                Expr::CastNamed { name, .. } => Some(name.clone()),
                _ => None,
            };
            let st = q.eng.db.find_type(&type_name?)?;
            let fields = st.composite.as_ref()?;
            fields
                .iter()
                .find(|(fname, _, _)| fname == field)
                .map(|(_, fty, _)| fty.clone())
        }
        Expr::ArrayCtor { elems, .. } => {
            let schema: Vec<QCol> = t
                .columns
                .iter()
                .enumerate()
                .map(|(i, (cn, ty))| QCol {
                    qual: String::new(),
                    name: cn.clone(),
                    ty: ty.clone(),
                    hidden: false,
                    src_ord: i as u32,
                })
                .collect();
            let mut acc: Option<ColType> = None;
            for el in elems {
                if matches!(el, Expr::Literal(Literal::Null)) {
                    continue;
                }
                let et = statics_expr_type(q, t, el).or_else(|| {
                    super::describe::expr_type(
                        &q.eng,
                        q.snap,
                        q.own,
                        q.session,
                        &[schema.as_slice()],
                        &[],
                        &[],
                        el,
                    )
                    .ok()
                })?;
                acc = Some(match acc {
                    None => et,
                    Some(prev) => {
                        if prev == et {
                            prev
                        } else {
                            return None;
                        }
                    }
                });
            }
            acc.map(|et| ColType::Array(crate::storage::ArrayElem::of(&et)))
        }
        Expr::Cast { to, .. } => Some(to.clone()),
        Expr::CastNamed { name, .. } => {
            let st = q.eng.db.find_type(name)?;
            if st.composite.is_some() {
                Some(ColType::Composite)
            } else if let Some(dom) = &st.domain {
                Some(dom.base.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The result type of one statistics expression item, for the
/// exprs import validation: a CAST's target type when the item is
/// a cast, else the type of the value the expression produces on
/// the table's first live row, else text.
fn expr_item_type(q: &mut Q, target: &Target, item: &ExtStatItem) -> ColType {
    let ExtStatItem::Expr(_, ast) = item else {
        return ColType::Text;
    };
    if let Expr::Cast { to, .. } = ast {
        return to.clone();
    }
    let tbl = q
        .eng
        .db
        .find_table(&target.table_key, q.snap, &q.all_xids, q.session)
        .cloned();
    // v2.35: type the expression STATICALLY against the table
    // schema first — the first-row evaluation below yields no type
    // for an EMPTY table (the stats_import clone tables), and the
    // Text fallback then accepted invalid MCV/histogram payloads
    // (BADMCV / BADHIST) that PG rejects.
    if let Some(t) = &tbl {
        let schema: Vec<QCol> = t
            .columns
            .iter()
            .enumerate()
            .map(|(i, (cn, ty))| QCol {
                qual: String::new(),
                name: cn.clone(),
                ty: ty.clone(),
                hidden: false,
                src_ord: i as u32,
            })
            .collect();
        let typed = super::describe::expr_type(
            &q.eng,
            q.snap,
            q.own,
            q.session,
            &[schema.as_slice()],
            &[],
            &[],
            ast,
        );
        match typed {
            Ok(ty) if !matches!(ty, ColType::Record) => return ty,
            _ => {
                // v2.35: expr_type cannot name composite fields
                // (QCol carries no type name) — it returns Record
                // for `(comp).a` and errors inside ARRAY[...].
                if let Some(ty) = statics_expr_type(q, t, ast) {
                    return ty;
                }
                if let Ok(ty) = typed {
                    return ty;
                }
            }
        }
    }
    // Evaluate over the first live row, mirroring the ANALYZE
    // build's evaluation pattern (analyze_index_exprs / eval_items).
    if let Some(t) = tbl {
        let first = t.rows.iter().find(|r| row_visible(r, q.snap, &q.all_xids));
        if let Some(row) = first {
            let schema: Vec<QCol> = t
                .columns
                .iter()
                .enumerate()
                .map(|(i, (cn, ty))| QCol {
                    qual: String::new(),
                    name: cn.clone(),
                    ty: ty.clone(),
                    hidden: false,
                    src_ord: i as u32,
                })
                .collect();
            let frame = Scope {
                schema: &schema,
                row: &row.values,
                prov: None,
            };
            if let Ok(v) = eval_expr(q, std::slice::from_ref(&frame), ast) {
                if !matches!(v, Value::Null) {
                    return super::analyze::value_coltype(&v);
                }
            }
        }
    }
    ColType::Text
}

/// Import one `exprs` array element (a JSON object of attribute
/// argument name -> string-or-null) into an ExtExprStat.
fn import_one_expr(
    q: &mut Q,
    target: &Target,
    item: &ExtStatItem,
    elem: &JsonbVal,
    exprnum: usize,
    existing: &ExtExprStat,
) -> Option<ExtExprStat> {
    let JsonbVal::Object(pairs) = elem else {
        warn(format!(
            "could not parse \"exprs\": invalid element in expression {}",
            exprnum
        ));
        return None;
    };
    // v2.35 (PG19 import_pg_statistic): the three range statistics
    // must be all strings or all nulls in an expression's object —
    // any disagreement fails the whole expression (and with it the
    // restore's return value). The attribute-stats path only pairs
    // length<->empty; the trio rule is exprs-only.
    {
        let found = |k: &str| {
            pairs
                .iter()
                .any(|(n, v)| n == k && matches!(v, JsonbVal::Str(_)))
        };
        let len_f = found("range_length_histogram");
        let empty_f = found("range_empty_frac");
        let bounds_f = found("range_bounds_histogram");
        if len_f != empty_f || len_f != bounds_f {
            warn(format!(
                "could not parse \"exprs\": invalid element in expression {}",
                exprnum
            ));
            return None;
        }
    }
    // Translate the JSON object into a Filled of typed values,
    // applying the attribute arginfo types to the string payloads.
    let mut filled = Filled {
        map: std::collections::HashMap::new(),
        ok: true,
    };
    for (k, v) in pairs {
        let Some((_, tag)) = ATT_ARGINFO.iter().find(|(n, _)| *n == k.as_str()) else {
            warn(format!(
                "could not import element in expression {}: invalid key name",
                exprnum
            ));
            return None;
        };
        let s = match v {
            JsonbVal::Null => continue,
            JsonbVal::Str(s) => s.clone(),
            _ => {
                warn(format!(
                    "could not parse \"exprs\": invalid element type in expression {}",
                    exprnum
                ));
                return None;
            }
        };
        let typed: Value = match tag {
            Tag::Float4 => match s.parse::<f32>() {
                Ok(f) => Value::Float4(f),
                Err(_) => {
                    warn(format!(
                        "could not parse \"exprs\": invalid element in expression {}",
                        exprnum
                    ));
                    return None;
                }
            },
            Tag::Int4 => match s.parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => {
                    warn(format!(
                        "could not parse \"exprs\": invalid element in expression {}",
                        exprnum
                    ));
                    return None;
                }
            },
            Tag::Float4Array => {
                match parse_array_literal(&s, crate::storage::ArrayElem::of(&ColType::Float4)) {
                    Ok(av) => Value::Array(Box::new(av)),
                    Err(_) => {
                        warn(format!(
                            "could not parse \"exprs\": invalid element in expression {}",
                            exprnum
                        ));
                        return None;
                    }
                }
            }
            _ => Value::text(s),
        };
        filled.map.insert(k.clone(), typed);
    }
    let ty = expr_item_type(q, target, item);
    // v2.35 (PG19 import_pg_statistic): an expression's datum is
    // built FRESH (statatt_init_empty_tuple) — fields absent from
    // the JSON become NULL, REPLACING the previous statistics
    // wholesale (the older-version dump emulation in stats_import
    // restores with all range fields absent and expects them NULL
    // afterwards). The attribute-stats path merges into the
    // existing tuple instead; that asymmetry is PG's own.
    let _ = existing;
    let mut cs = ColStats::default();
    if !import_att_kinds(&q.eng.db, "expr", &ty, None, None, &filled, &mut cs) {
        return None;
    }
    Some(expr_stat_of_colstats(&cs))
}

fn restore_extended_stats(q: &mut Q, scopes: &[Scope], args: &[Expr]) -> Result<Value, ExecError> {
    let filled = fill_pairs(q, scopes, args, EXT_ARGINFO)?;
    let Some((target, pos)) = resolve_ext_target(q, &filled, "restore")? else {
        return Ok(Value::Bool(false));
    };
    let inherited = required_bool(&filled, "inherited")?;
    let mut ok = filled.ok;
    let obj = q.eng.db.ext_stats[pos].clone();
    let valid: std::collections::HashSet<i32> = obj
        .items
        .iter()
        .enumerate()
        .map(|(i, _)| attnum_of(&obj, i))
        .collect();
    let nexprs = obj
        .items
        .iter()
        .filter(|i| matches!(i, ExtStatItem::Expr(_, _)))
        .count();
    let has_nd = filled.get("n_distinct").is_some();
    let has_dep = filled.get("dependencies").is_some();
    let has_vals = filled.get("most_common_vals").is_some();
    let has_freqs = filled.get("most_common_freqs").is_some();
    let has_base = filled.get("most_common_base_freqs").is_some();
    let has_exprs = filled.get("exprs").is_some();
    let mut applied = false;

    // Work on a cloned payload, then commit it back.
    let mut data: ExtStatData = if inherited {
        obj.data_inh.clone().unwrap_or_default()
    } else {
        obj.data.clone()
    };

    if has_nd {
        if !obj.kinds.contains(&'d') {
            warn("cannot specify parameter \"n_distinct\"".into());
            ok = false;
        } else if let Some(text) = payload_text(filled.get("n_distinct").unwrap()) {
            match import_ndistinct(&text, &valid) {
                Some((items, canon)) => {
                    data.ndistinct_items = items;
                    data.ndistinct = Some(canon);
                    applied = true;
                }
                None => {
                    warn("could not parse \"n_distinct\": invalid attribute number".into());
                    ok = false;
                }
            }
        }
    }
    if has_dep {
        if !obj.kinds.contains(&'f') {
            warn("cannot specify parameter \"dependencies\"".into());
            ok = false;
        } else if let Some(text) = payload_text(filled.get("dependencies").unwrap()) {
            match import_dependencies(&text, &valid) {
                Some((items, canon)) => {
                    data.deps_items = items;
                    data.dependencies = Some(canon);
                    applied = true;
                }
                None => {
                    warn("could not parse \"dependencies\": invalid attribute number".into());
                    ok = false;
                }
            }
        }
    }
    if has_vals || has_freqs || has_base {
        if !obj.kinds.contains(&'m') {
            warn("cannot specify parameters \"most_common_vals\", \"most_common_freqs\", or \"most_common_base_freqs\"".into());
            ok = false;
        } else if !(has_vals && has_freqs && has_base) {
            warn("could not use \"most_common_vals\", \"most_common_freqs\", and \"most_common_base_freqs\": missing one or more parameters".into());
            ok = false;
        } else {
            let vals_v = filled.get("most_common_vals").unwrap().clone();
            let freqs = match filled.get("most_common_freqs").unwrap() {
                Value::Array(av) => av
                    .elems
                    .iter()
                    .map(|e| match e {
                        Value::Float(f) => Some(*f),
                        Value::Float4(f) => Some(*f as f64),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let base = match filled.get("most_common_base_freqs").unwrap() {
                Value::Array(av) => av
                    .elems
                    .iter()
                    .map(|e| match e {
                        Value::Float(f) => Some(*f),
                        Value::Float4(f) => Some(*f as f64),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let mut mcv_ok = true;
            let mut items: Vec<(Vec<Option<Value>>, f64, f64)> = Vec::new();
            if let Value::Array(av) = &vals_v {
                if av.dims.len() != 2 {
                    warn("could not parse array \"most_common_vals\": incorrect number of dimensions (2 required)".into());
                    mcv_ok = false;
                } else {
                    let nrows = av.dims[0] as usize;
                    let ncols = av.dims[1] as usize;
                    if ncols != obj.items.len() {
                        warn(format!(
                            "could not parse array \"most_common_vals\": found {} attributes but expected {}",
                            ncols,
                            obj.items.len()
                        ));
                        mcv_ok = false;
                    } else if nrows > 10000 {
                        warn(format!(
                            "could not parse array \"most_common_vals\": number of items ({}) exceeds maximum (10000)",
                            nrows
                        ));
                        mcv_ok = false;
                    } else if freqs.len() != nrows || base.len() != nrows {
                        warn("could not parse \"most_common_vals\": incorrect number of elements (same as \"most_common_freqs\" required)".into());
                        mcv_ok = false;
                    } else {
                        for r in 0..nrows {
                            let mut row = Vec::new();
                            for c in 0..ncols {
                                let v = &av.elems[r * ncols + c];
                                row.push(if matches!(v, Value::Null) {
                                    None
                                } else {
                                    Some(v.clone())
                                });
                            }
                            items.push((row, freqs[r].unwrap_or(0.0), base[r].unwrap_or(0.0)));
                        }
                    }
                }
            }
            if mcv_ok {
                data.mcv = items;
                applied = true;
            } else {
                ok = false;
            }
        }
    }
    if has_exprs {
        if nexprs == 0 {
            warn("cannot specify parameter \"exprs\"".into());
            ok = false;
        } else if let Some(text) = payload_text(filled.get("exprs").unwrap()) {
            match parse_jsonb(&text) {
                Ok(JsonbVal::Array(elems)) => {
                    if elems.len() != nexprs {
                        warn(format!(
                            "could not parse \"exprs\": incorrect number of elements ({} required)",
                            nexprs
                        ));
                        ok = false;
                    } else {
                        if data.expr_stats.len() != nexprs {
                            data.expr_stats = vec![ExtExprStat::default(); nexprs];
                        }
                        let mut ei = 0usize;
                        for item in &obj.items {
                            let ExtStatItem::Expr(_, _) = item else {
                                continue;
                            };
                            let existing = data.expr_stats[ei].clone();
                            // v2.35 (PG19 import_expressions): a JSON
                            // null element is a valid placeholder —
                            // it counts as imported (the expression
                            // simply has no statistics) and does NOT
                            // fail the restore.
                            if matches!(elems[ei], JsonbVal::Null) {
                                data.expr_stats[ei] = ExtExprStat::default();
                                applied = true;
                                ei += 1;
                                continue;
                            }
                            match import_one_expr(q, &target, item, &elems[ei], ei + 1, &existing) {
                                Some(new) => {
                                    data.expr_stats[ei] = new;
                                    applied = true;
                                }
                                None => {
                                    ok = false;
                                }
                            }
                            ei += 1;
                        }
                    }
                }
                _ => {
                    warn("could not parse \"exprs\": root-level array required".into());
                    ok = false;
                }
            }
        }
    }

    if applied || (!has_nd && !has_dep && !has_vals && !has_freqs && !has_base && !has_exprs) {
        data.built = true;
    }
    if inherited {
        q.eng.db.ext_stats[pos].data_inh = Some(data);
    } else {
        q.eng.db.ext_stats[pos].data = data;
    }
    Ok(Value::Bool(ok))
}

fn clear_extended_stats(q: &mut Q, scopes: &[Scope], args: &[Expr]) -> Result<Value, ExecError> {
    if args.len() != 5 {
        return Err(exec_err(
            "42883",
            "function pg_clear_extended_stats(text, text, text, text, boolean) does not exist",
        ));
    }
    // Rebuild a Filled by hand: the clear functions take plain
    // positional arguments (named notation already resolved).
    let mut filled = Filled {
        map: std::collections::HashMap::new(),
        ok: true,
    };
    let names = [
        "schemaname",
        "relname",
        "statistics_schemaname",
        "statistics_name",
        "inherited",
    ];
    for (a, n) in args.iter().zip(names.iter()) {
        let v = eval_expr(q, scopes, a)?;
        if !matches!(v, Value::Null) {
            filled.map.insert(n.to_string(), v);
        }
    }
    let Some((_, pos)) = resolve_ext_target(q, &filled, "clear")? else {
        return Ok(Value::Null);
    };
    let inherited = required_bool(&filled, "inherited")?;
    if inherited {
        q.eng.db.ext_stats[pos].data_inh = None;
    } else {
        q.eng.db.ext_stats[pos].data = ExtStatData::default();
    }
    Ok(Value::Null)
}
