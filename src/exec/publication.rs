// v2.02: publications (minimal catalog) + the replica-identity guard
// (PG19 commands/publicationcmds.c, executor/execReplication.c
// CheckCmdReplicaIdentity, called from execMain.c
// CheckValidResultRel).
//
// The catalog lives on `Database::publications` — transactional
// via the statement undo log (`WriteOp::*Publication`), which
// object_address.sql observably relies on.
// v2.04: publications are WAL-logged (CreatePublication /
// DropPublication / AlterPublication records, RGSWAL29) and written
// in CHECKPOINT (v25+), so they survive restart; and the
// pg_publication / pg_publication_rel / pg_publication_tables /
// pg_publication_namespace / pg_publication_sequences catalog views
// plus the pg_get_publication_tables SRF are implemented below and
// in catalog.rs (PG19 catalog/pg_publication.h,
// catalog/pg_publication_rel.h, catalog/system_views.sql).
//
// Guard semantics (DEFAULT replica identity only): a table publishes
// updates/deletes when some publication covers it — FOR ALL TABLES
// (minus EXCEPT), an explicit FOR TABLE entry, a TABLES IN SCHEMA
// entry for the table's schema, or an ancestor partitioned table so
// covered — and that publication's publish actions include the
// command. A covered UPDATE/DELETE against a table with no PRIMARY
// KEY (the decidable DEFAULT replica identity; REPLICA IDENTITY
// INDEX/FULL syntax stays 42601) errors 55000 exactly as PG19.
// Partitioned parents are skipped (PG skips partitioned relkind);
// leaves are checked individually by the callers.
use super::*;
use crate::storage::{PubTableSpec, Publication};

pub(crate) fn base_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// PG19 pg_publication.c check_publication_add_relation, shared by
/// FOR TABLE entries and FOR ALL TABLES EXCEPT entries: the relation
/// must exist (42P01); EXCEPT entries additionally must not be an
/// individual partition (22023 — this is what keeps publication.sql's
/// testpub9 from ever existing upstream); catalog and temporary
/// relations are rejected (22023) in both roles.
fn validate_pub_relation(
    eng: &Engine,
    ctx: &StmtCtx,
    t: &str,
    except: bool,
) -> Result<(), ExecError> {
    let base = base_name(t);
    let msg = if except {
        format!(
            "cannot specify relation \"{}\" in the publication EXCEPT clause",
            t
        )
    } else {
        format!("cannot add relation \"{}\" to publication", base)
    };
    if eng
        .db
        .temp_tables
        .get(&ctx.session)
        .is_some_and(|m| m.contains_key(base))
    {
        return Err(exec_err("22023", msg));
    }
    if let Some(tbl) = eng.db.find_table(t, ctx.snap, &ctx.all_xids, ctx.session) {
        if eng
            .db
            .obj_schema
            .get(base)
            .is_some_and(|s| s.eq_ignore_ascii_case("pg_catalog"))
        {
            return Err(exec_err("22023", msg));
        }
        if except
            && tbl
                .partition
                .as_ref()
                .is_some_and(|pi| pi.parent.is_some() && !pi.is_partitioned)
        {
            return Err(exec_err("22023", msg));
        }
        return Ok(());
    }
    if eng.db.find_view(t, ctx.snap, ctx.own).is_some() {
        return Err(exec_err("22023", msg));
    }
    Err(exec_err(
        "42P01",
        format!("relation \"{}\" does not exist", t),
    ))
}

/// PG19 publicationcmds.c validation for a FOR TABLE entry.
fn validate_pub_table(eng: &Engine, ctx: &StmtCtx, t: &str) -> Result<(), ExecError> {
    validate_pub_relation(eng, ctx, t, false)
}

/// PG19: EXCEPT entries get the same validation, with the
/// individual-partition rejection on top.
fn validate_pub_except(eng: &Engine, ctx: &StmtCtx, tables: &[String]) -> Result<(), ExecError> {
    for t in tables {
        validate_pub_relation(eng, ctx, t, true)?;
    }
    Ok(())
}

/// PG19: a TABLES IN SCHEMA entry must name an existing schema;
/// system schemas cannot be added (check_publication_add_schema).
fn validate_pub_schemas(eng: &Engine, schemas: &[String]) -> Result<(), ExecError> {
    for s in schemas {
        if s.eq_ignore_ascii_case("pg_catalog") || s.eq_ignore_ascii_case("pg_toast") {
            return Err(exec_err(
                "22023",
                format!("cannot add schema \"{}\" to publication", s),
            ));
        }
        if !eng.db.schemas.iter().any(|x| x.eq_ignore_ascii_case(s)) {
            return Err(exec_err(
                "42P01",
                format!("schema \"{}\" does not exist", s),
            ));
        }
    }
    Ok(())
}

/// PG19: an unquoted CURRENT_SCHEMA entry in a publication spec is
/// resolved to the current schema at DDL time (the first existing
/// schema on search_path, $user resolving to the role); when no
/// schema has been selected that is an error. (The parser folds the
/// unquoted keyword to lowercase "current_schema"; a quoted
/// "CURRENT_SCHEMA" arrives verbatim and stays a literal name.)
/// Storing the literal instead would make the entry match every
/// schema in the replica-identity guard.
fn resolve_pub_schemas(
    eng: &Engine,
    ctx: &StmtCtx,
    schemas: &[String],
) -> Result<Vec<String>, ExecError> {
    let mut out: Vec<String> = Vec::new();
    for s in schemas {
        let resolved = if s == "current_schema" {
            let sp = crate::exec::installed_guc("search_path").unwrap_or_default();
            let mut found: Option<String> = None;
            for raw in sp.split(',') {
                let e = raw.trim().trim_matches('"');
                if e.is_empty() {
                    continue;
                }
                let cand = if e == "$user" {
                    ctx.role.to_string()
                } else {
                    e.to_string()
                };
                if eng.db.schemas.iter().any(|x| x == &cand) {
                    found = Some(cand);
                    break;
                }
            }
            match found {
                Some(f) => f,
                None => {
                    return Err(exec_err(
                        "22023",
                        "no schema has been selected for CURRENT_SCHEMA".to_string(),
                    ));
                }
            }
        } else {
            s.clone()
        };
        if !out.iter().any(|x| x.eq_ignore_ascii_case(&resolved)) {
            out.push(resolved);
        }
    }
    Ok(out)
}

/// Does publication `p` cover `table` (a storage key, unqualified) for
/// the schema attribution `obj_schema` records?
fn pub_covers(eng: &Engine, p: &Publication, table: &str) -> bool {
    if p.all_tables
        && !p
            .except_tables
            .iter()
            .any(|e| base_name(e).eq_ignore_ascii_case(table))
    {
        return true;
    }
    if p.tables
        .iter()
        .any(|t| base_name(t).eq_ignore_ascii_case(table))
    {
        return true;
    }
    if !p.schemas.is_empty() {
        let schema = eng
            .db
            .obj_schema
            .get(table)
            .map(|s| s.as_str())
            .unwrap_or("public");
        if p.schemas.iter().any(|s| s.eq_ignore_ascii_case(schema)) {
            return true;
        }
    }
    false
}

/// v2.03: the table's replica identity columns (PG19
/// RelationGetReplicaIndex / relreplident). `RiCols::Full` means every
/// column is in the identity. The bool is "has a replica identity at
/// all" for the 55000 check: DEFAULT = has PK, FULL/INDEX = yes,
/// NOTHING = no.
#[derive(Clone, Debug)]
pub(crate) enum RiCols {
    Full,
    Cols(std::collections::HashSet<String>),
}

pub(crate) fn replica_identity_cols(
    eng: &Engine,
    ctx: &StmtCtx,
    t: &Table,
    table: &str,
) -> (bool, RiCols) {
    use crate::storage::ReplicaIdentity;
    match &t.replica_identity {
        ReplicaIdentity::Default => match &t.pkey {
            Some(pk) => (
                true,
                RiCols::Cols(pk.cols.iter().map(|c| c.to_lowercase()).collect()),
            ),
            None => (false, RiCols::Cols(Default::default())),
        },
        ReplicaIdentity::Full => (true, RiCols::Full),
        ReplicaIdentity::Nothing => (false, RiCols::Cols(Default::default())),
        ReplicaIdentity::Index(name) => {
            for ix in eng
                .db
                .visible_indexes_for(table, ctx.snap, &ctx.all_xids, ctx.session)
            {
                if ix.def.name.eq_ignore_ascii_case(name) {
                    return (
                        true,
                        RiCols::Cols(ix.def.col_names.iter().map(|c| c.to_lowercase()).collect()),
                    );
                }
            }
            (false, RiCols::Cols(Default::default()))
        }
    }
}

/// v2.03: ALTER TABLE ... REPLICA IDENTITY (PG19
/// tablecmds.c ATExecReplicaIdentity).
pub(crate) fn alter_replica_identity(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    kind: crate::sql::ReplicaKind,
    index: Option<&str>,
) -> Result<ExecResult, ExecError> {
    use crate::sql::ReplicaKind;
    use crate::storage::ReplicaIdentity;
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let ri = match kind {
        ReplicaKind::Default => ReplicaIdentity::Default,
        ReplicaKind::Full => ReplicaIdentity::Full,
        ReplicaKind::Nothing => ReplicaIdentity::Nothing,
        ReplicaKind::Index => {
            let iname = index.unwrap_or_default();
            let base = base_name(name);
            let found = eng
                .db
                .visible_indexes_for(name, ctx.snap, &ctx.all_xids, ctx.session)
                .into_iter()
                .find(|ix| ix.def.name.eq_ignore_ascii_case(iname))
                .map(|ix| ix.def.clone());
            let def = match found {
                Some(d) => d,
                None => {
                    // An index of that name on another table is a
                    // wrong-object error, not "does not exist".
                    if eng.db.indexes.contains_key(iname)
                        || eng
                            .db
                            .temp_indexes
                            .get(&ctx.session)
                            .is_some_and(|m| m.contains_key(iname))
                    {
                        return Err(exec_err(
                            "42809",
                            format!("\"{}\" is not an index for table \"{}\"", iname, base),
                        ));
                    }
                    return Err(exec_err(
                        "42704",
                        format!("index \"{}\" for table \"{}\" does not exist", iname, base),
                    ));
                }
            };
            if !def.unique {
                return Err(exec_err(
                    "42809",
                    format!(
                        "cannot use non-unique index \"{}\" as replica identity",
                        iname
                    ),
                ));
            }
            if def.exprs.iter().any(|e| e.is_some()) {
                return Err(exec_err(
                    "0A000",
                    format!(
                        "cannot use expression index \"{}\" as replica identity",
                        iname
                    ),
                ));
            }
            if def.predicate.is_some() {
                return Err(exec_err(
                    "0A000",
                    format!("cannot use partial index \"{}\" as replica identity", iname),
                ));
            }
            for cn in &def.col_names {
                if let Some(pos) = t.columns.iter().position(|(n, _)| n == cn) {
                    if !t.not_null.get(pos).copied().unwrap_or(false) {
                        return Err(exec_err(
                            "42809",
                            format!(
                                "index \"{}\" cannot be used as replica identity because column \"{}\" is nullable",
                                iname, cn
                            ),
                        ));
                    }
                }
            }
            ReplicaIdentity::Index(iname.to_string())
        }
    };
    let mut next = t;
    next.replica_identity = ri;
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

fn ri_validity_err(is_update: bool, base: &str, detail: &str) -> ExecError {
    let mut e = exec_err(
        "42P10",
        format!(
            "cannot {} table \"{}\"",
            if is_update { "update" } else { "delete from" },
            base
        ),
    );
    e.detail = Some(detail.to_string());
    e
}

const SYSTEM_COLS: [&str; 6] = ["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

/// v2.03: validate a publication column list against the table
/// (PG19 pg_publication.c publication_add_relation / pub_form_cols_map).
pub(crate) fn validate_column_list(
    t: &Table,
    table: &str,
    cols: &[String],
) -> Result<(), ExecError> {
    let mut seen = std::collections::HashSet::new();
    for c in cols {
        let lc = c.to_lowercase();
        if SYSTEM_COLS.contains(&lc.as_str()) {
            return Err(exec_err(
                "42P10",
                format!(
                    "cannot use system column \"{}\" in publication column list",
                    c
                ),
            ));
        }
        if !seen.insert(lc.clone()) {
            return Err(exec_err(
                "42701",
                format!("duplicate column \"{}\" in publication column list", c),
            ));
        }
        let pos = match t
            .columns
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(c))
        {
            Some(p) => p,
            None => {
                return Err(exec_err(
                    "42703",
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        c,
                        base_name(table)
                    ),
                ));
            }
        };
        if let Some(Some((_, stored))) = t.gen_cols.get(pos) {
            if !stored {
                return Err(exec_err(
                    "0A000",
                    format!(
                        "cannot use virtual generated column \"{}\" in publication column list",
                        c
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) const IMMUTABLE_FUNCS: &[&str] = &[
    "length",
    "char_length",
    "character_length",
    "octet_length",
    "bit_length",
    "lower",
    "upper",
    "initcap",
    "trim",
    "ltrim",
    "rtrim",
    "btrim",
    "substr",
    "substring",
    "left",
    "right",
    "reverse",
    "repeat",
    "replace",
    "split_part",
    "concat",
    "concat_ws",
    "abs",
    "ceil",
    "ceiling",
    "floor",
    "round",
    "trunc",
    "power",
    "sqrt",
    "exp",
    "ln",
    "log",
    "mod",
    "sign",
    "greatest",
    "least",
    "coalesce",
    "nullif",
    "to_hex",
    "to_oct",
    "to_bin",
    "ascii",
    "chr",
    "md5",
    "encode",
    "decode",
    "starts_with",
    "position",
    "overlay",
    "cast",
    "pg_typeof",
    "format",
    "div",
    "width_bucket",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "sinh",
    "cosh",
    "tanh",
    "degrees",
    "radians",
    "pi",
    "log10",
    "cbrt",
    "factorial",
    "gcd",
    "lcm",
    "min_scale",
    "trim_scale",
    "date_part",
    "extract",
    "age",
    "justify_days",
    "justify_hours",
    "justify_interval",
    "make_date",
    "make_time",
    "make_timestamp",
    "to_char",
    "to_date",
    "to_timestamp",
    "to_number",
    "array_length",
    "array_dims",
    "cardinality",
    "array_ndims",
    "string_to_array",
    "array_to_string",
    "regexp_replace",
    "regexp_match",
    "regexp_matches",
    "upper_inf",
    "lower_inf",
    "isempty",
    "jsonb_typeof",
    "json_typeof",
    "uuid_extract_version",
    "uuid_extract_timestamp",
    "macaddr8_set7bit",
    "inet_merge",
    "family",
    "host",
    "masklen",
    "netmask",
    "network",
    "set_masklen",
    "text",
    "bool",
    "int2",
    "int4",
    "int8",
    "float4",
    "float8",
    "numeric",
    "varchar",
    "bpchar",
    "name",
    "oid",
    "xmlexists",
    "xml_is_document",
    "get_bit",
    "get_byte",
    "unicode",
    "normalize",
    "translate",
    "unistr",
    "b64encode",
    "b64decode",
    "quote_literal",
    "quote_ident",
    "quote_nullable",
    "pg_input_is_valid",
    "to_json",
    "to_jsonb",
    "row_to_json",
    "array_agg",
    "string_agg",
];

fn filter_walk(
    eng: &Engine,
    t: &Table,
    e: &Expr,
    cols_out: &mut Vec<String>,
) -> Result<(), ExecError> {
    let invalid = |detail: &str| {
        exec_err(
            "0A000",
            format!("invalid publication WHERE expression: {}", detail),
        )
    };
    match e {
        Expr::Column { name, .. } => {
            let lc = name.to_lowercase();
            if SYSTEM_COLS.contains(&lc.as_str()) {
                return Err(invalid("System columns are not allowed."));
            }
            let pos = t
                .columns
                .iter()
                .position(|(n, _)| n.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    exec_err(
                        "42703",
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            name,
                            t.columns.first().map(|_| "").unwrap_or("")
                        ),
                    )
                })?;
            let _ = pos;
            if t.composite_types
                .get(pos)
                .and_then(|o| o.as_ref())
                .is_some()
                || t.domain_types.get(pos).and_then(|o| o.as_ref()).is_some()
            {
                return Err(invalid("User-defined types are not allowed."));
            }
            cols_out.push(lc);
            Ok(())
        }
        Expr::Literal(_) | Expr::Param(_) | Expr::ResolvedCol { .. } => Ok(()),
        Expr::Agg { .. } | Expr::WithinGroup { .. } => Err(exec_err(
            "42803",
            "aggregate functions are not allowed in WHERE".to_string(),
        )),
        Expr::Window { .. } => Err(invalid(
            "Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed.",
        )),
        Expr::ScalarSub(_)
        | Expr::InSub { .. }
        | Expr::Exists { .. }
        | Expr::Quantified { .. }
        | Expr::ArraySubquery(_)
        | Expr::JsonArraySubq { .. } => Err(invalid(
            "Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed.",
        )),
        Expr::UserOp { op, left, right } => {
            // Builtin operators that reach here un-desugared
            // (array containment, regex, ...) are fine if their
            // operands are; genuine user operators (procedure
            // volatility unknown) are rejected as mutable.
            const BUILTIN_OPS: &[&str] = &[
                "<@", "@>", "&&", "~", "~*", "!~", "!~*", "@<", "@>", "<", ">", "<=", ">=", "=",
                "<>", "!=", "||", "@?", "@@", "@@@",
            ];
            if !BUILTIN_OPS.contains(&op.as_str()) {
                return Err(invalid(
                    "User-defined or built-in mutable functions are not allowed.",
                ));
            }
            filter_walk(eng, t, left, cols_out)?;
            filter_walk(eng, t, right, cols_out)
        }
        Expr::WholeRow { .. } => Err(invalid(
            "Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed.",
        )),
        Expr::Func { name, args } => {
            let ln = name.to_lowercase();
            if eng.db.functions.contains_key(&ln) {
                return Err(invalid(
                    "User-defined or built-in mutable functions are not allowed.",
                ));
            }
            if !IMMUTABLE_FUNCS.contains(&ln.as_str()) {
                return Err(invalid(
                    "User-defined or built-in mutable functions are not allowed.",
                ));
            }
            for a in args {
                filter_walk(eng, t, a, cols_out)?;
            }
            Ok(())
        }
        Expr::CastNamed { expr, name } => {
            // A cast to a user-defined type is not allowed; builtin
            // type names arrive as Cast, named composites/domains as
            // CastNamed.
            let ln = name.to_lowercase();
            const BUILTINS: &[&str] = &[
                "text",
                "varchar",
                "bpchar",
                "char",
                "int",
                "integer",
                "int2",
                "int4",
                "int8",
                "smallint",
                "bigint",
                "float4",
                "float8",
                "real",
                "double precision",
                "numeric",
                "decimal",
                "bool",
                "boolean",
                "date",
                "time",
                "timestamp",
                "timestamptz",
                "interval",
                "uuid",
                "json",
                "jsonb",
                "bytea",
                "oid",
                "name",
                "xml",
                "money",
                "inet",
                "cidr",
                "macaddr",
                "bit",
                "varbit",
                "record",
            ];
            if !BUILTINS.contains(&ln.as_str()) {
                return Err(invalid("User-defined types are not allowed."));
            }
            filter_walk(eng, t, expr, cols_out)
        }
        Expr::Cast { expr, .. } => filter_walk(eng, t, expr, cols_out),
        Expr::NamedArg { expr, .. } => filter_walk(eng, t, expr, cols_out),
        Expr::Arith { left, right, .. } | Expr::Cmp { left, right, .. } => {
            filter_walk(eng, t, left, cols_out)?;
            filter_walk(eng, t, right, cols_out)
        }
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            filter_walk(eng, t, a, cols_out)?;
            filter_walk(eng, t, b, cols_out)
        }
        Expr::JsonOp { left, right, .. } | Expr::IsDistinctFrom { left, right, .. } => {
            filter_walk(eng, t, left, cols_out)?;
            filter_walk(eng, t, right, cols_out)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            filter_walk(eng, t, expr, cols_out)?;
            filter_walk(eng, t, pattern, cols_out)?;
            if let Some(e) = escape {
                filter_walk(eng, t, e, cols_out)?;
            }
            Ok(())
        }
        Expr::Regex { expr, pattern, .. } => {
            filter_walk(eng, t, expr, cols_out)?;
            filter_walk(eng, t, pattern, cols_out)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            filter_walk(eng, t, expr, cols_out)?;
            filter_walk(eng, t, low, cols_out)?;
            filter_walk(eng, t, high, cols_out)
        }
        Expr::IsBool { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Not(expr)
        | Expr::BitNot(expr)
        | Expr::Neg(expr) => filter_walk(eng, t, expr, cols_out),
        Expr::Extract { from, .. } => filter_walk(eng, t, from, cols_out),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                filter_walk(eng, t, o, cols_out)?;
            }
            for (k, r) in whens {
                filter_walk(eng, t, k, cols_out)?;
                filter_walk(eng, t, r, cols_out)?;
            }
            if let Some(e) = else_ {
                filter_walk(eng, t, e, cols_out)?;
            }
            Ok(())
        }
        Expr::Row(elems) | Expr::ArrayCtor { elems, .. } => {
            for e in elems {
                filter_walk(eng, t, e, cols_out)?;
            }
            Ok(())
        }
        Expr::FieldAccess { expr, .. } => filter_walk(eng, t, expr, cols_out),
        Expr::AtTimeZone { expr, zone } => {
            filter_walk(eng, t, expr, cols_out)?;
            if let Some(z) = zone {
                filter_walk(eng, t, z, cols_out)?;
            }
            Ok(())
        }
        Expr::Subscript { array, indices } => {
            filter_walk(eng, t, array, cols_out)?;
            for i in indices {
                filter_walk(eng, t, i, cols_out)?;
            }
            Ok(())
        }
        Expr::Slice { array, bounds } => {
            filter_walk(eng, t, array, cols_out)?;
            for (l, u) in bounds {
                if let Some(l) = l {
                    filter_walk(eng, t, l, cols_out)?;
                }
                if let Some(u) = u {
                    filter_walk(eng, t, u, cols_out)?;
                }
            }
            Ok(())
        }
    }
}

fn filter_is_boolean(t: &Table, e: &Expr) -> bool {
    match e {
        Expr::Cmp { .. }
        | Expr::And(..)
        | Expr::Or(..)
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::IsBool { .. }
        | Expr::IsDistinctFrom { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::Regex { .. }
        | Expr::InSub { .. }
        | Expr::Exists { .. } => true,
        Expr::Literal(Literal::Bool(_)) => true,
        Expr::Column { name, .. } => t
            .columns
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .is_some_and(|(_, ty)| *ty == ColType::Bool),
        Expr::Case { whens, else_, .. } => {
            whens.iter().all(|(_, r)| filter_is_boolean(t, r))
                && else_.as_ref().is_none_or(|e| filter_is_boolean(t, e))
        }
        Expr::JsonOp { op, .. } => matches!(
            op,
            JsonOp::Contains
                | JsonOp::ContainedBy
                | JsonOp::Exists
                | JsonOp::ExistsAny
                | JsonOp::ExistsAll
                | JsonOp::PathExists
                | JsonOp::PathMatch
        ),
        Expr::UserOp { op, .. } => matches!(
            op.as_str(),
            "<@" | "@>"
                | "&&"
                | "~"
                | "~*"
                | "!~"
                | "!~*"
                | "<"
                | ">"
                | "<="
                | ">="
                | "="
                | "<>"
                | "!="
                | "@?"
                | "@@"
        ),
        Expr::Func { name, .. } => matches!(
            name.to_lowercase().as_str(),
            "pg_input_is_valid"
                | "xmlexists"
                | "xml_is_document"
                | "isempty"
                | "upper_inf"
                | "lower_inf"
        ),
        _ => false,
    }
}

/// v2.03: validate a row filter at publication DDL time (PG19
/// publicationcmds.c check_publication_add_relation + the
/// transformExpr simplicity walk). Returns the referenced columns.
pub(crate) fn validate_row_filter(
    eng: &Engine,
    t: &Table,
    table: &str,
    e: &Expr,
) -> Result<Vec<String>, ExecError> {
    if !filter_is_boolean(t, e) {
        return Err(exec_err(
            "42804",
            "argument of PUBLICATION WHERE must be type boolean, not type integer".to_string(),
        ));
    }
    let mut cols = Vec::new();
    // Column-existence errors should name the table (the walker uses a
    // placeholder); re-report with the real name.
    match filter_walk(eng, t, e, &mut cols) {
        Ok(()) => Ok(cols),
        Err(mut err) => {
            if err.code == "42703" {
                err.message = err.message.replace(
                    "of relation \"\"",
                    &format!("of relation \"{}\"", base_name(table)),
                );
            }
            Err(err)
        }
    }
}

/// PG19 CheckCmdReplicaIdentity (v2.03, full form): the publication
/// validity checks (row-filter columns in the replica identity,
/// column list covering the replica identity, generated columns in
/// the replica identity published) run BEFORE the no-replica-identity
/// 55000 check, exactly as execReplication.c orders them. Partitioned
/// parents are skipped here — callers check the leaves.
pub(crate) fn check_replica_identity(
    eng: &Engine,
    ctx: &StmtCtx,
    t: &Table,
    table: &str,
    is_update: bool,
) -> Result<(), ExecError> {
    if t.partition.as_ref().is_some_and(|pi| pi.is_partitioned) {
        return Ok(());
    }
    if eng.db.publications.is_empty() {
        return Ok(());
    }
    if eng
        .db
        .temp_tables
        .get(&ctx.session)
        .is_some_and(|m| m.contains_key(base_name(table)))
    {
        return Ok(());
    }
    let base = base_name(table);
    let (has_ri, ri_cols) = replica_identity_cols(eng, ctx, t, table);
    let ri_set: Option<&std::collections::HashSet<String>> = match &ri_cols {
        RiCols::Cols(s) => Some(s),
        RiCols::Full => None,
    };
    // Walk the table and its partition ancestors; for each covering
    // publication that publishes this command, run the validity
    // checks against the spec stored for that level's name.
    let mut any_pub_action = false;
    let mut cur: Option<String> = Some(table.to_string());
    let mut guard = 0;
    while let Some(name) = cur {
        guard += 1;
        if guard > 16 {
            break;
        }
        let lvl_base = base_name(&name).to_lowercase();
        for p in eng.db.publications.values() {
            if !pub_covers(eng, p, base_name(&name)) {
                continue;
            }
            let publishes = if is_update { p.pubupdate } else { p.pubdelete };
            if !publishes {
                continue;
            }
            any_pub_action = true;
            let spec = p.table_specs.get(&lvl_base);
            // v2.05 (PG19 CheckCmdReplicaIdentity): a column list or
            // row filter stored on a partitioned ANCESTOR cannot be
            // applied to this leaf when the publication does not
            // publish via the partition root — reachable when
            // publish_via_partition_root is flipped to false after
            // the spec was added (DDL-time checks catch the rest).
            if lvl_base != base.to_lowercase() && !p.via_partition_root {
                if let Some(sp) = spec {
                    if sp.filter.is_some() {
                        return Err(ri_validity_err(
                            is_update,
                            base,
                            &format!(
                                "The publication contains a WHERE clause for partitioned table \"{}\", which is not allowed when \"publish_via_partition_root\" is false.",
                                base_name(&name)
                            ),
                        ));
                    }
                    if sp.cols.is_some() {
                        return Err(ri_validity_err(
                            is_update,
                            base,
                            &format!(
                                "The publication contains a column list for partitioned table \"{}\", which is not allowed when \"publish_via_partition_root\" is false.",
                                base_name(&name)
                            ),
                        ));
                    }
                }
            }
            // 1. Row filter columns must be in the replica identity.
            if let Some(f) = spec.and_then(|s| s.filter.as_ref()) {
                let mut fcols = Vec::new();
                let _ = filter_walk(eng, t, f, &mut fcols);
                if let Some(set) = ri_set {
                    if fcols.iter().any(|c| !set.contains(c)) {
                        return Err(ri_validity_err(
                            is_update,
                            base,
                            "Column used in the publication WHERE expression is not part of the replica identity.",
                        ));
                    }
                }
            }
            // 2. A column list must cover the replica identity; a
            // FULL identity is never covered by a list.
            if let Some(cols) = spec.and_then(|s| s.cols.as_ref()) {
                let colset: std::collections::HashSet<String> =
                    cols.iter().map(|c| c.to_lowercase()).collect();
                let covered = match ri_set {
                    None => false,
                    Some(set) => set.iter().all(|c| colset.contains(c)),
                };
                if !covered {
                    return Err(ri_validity_err(
                        is_update,
                        base,
                        "Column list used by the publication does not cover the replica identity.",
                    ));
                }
            }
            // 3. Generated columns in the replica identity must be
            // published (explicitly listed, or stored-generated via
            // publish_generated_columns = stored).
            for (i, g) in t.gen_cols.iter().enumerate() {
                let Some((_, stored)) = g else { continue };
                // Defensive: gen_cols must stay parallel to columns
                // (ALTER maintains it); never index blindly.
                let Some((colname, _)) = t.columns.get(i) else {
                    continue;
                };
                let cname = colname.to_lowercase();
                let in_ri = match ri_set {
                    None => true,
                    Some(set) => set.contains(&cname),
                };
                if !in_ri {
                    continue;
                }
                let listed = spec
                    .and_then(|s| s.cols.as_ref())
                    .is_some_and(|cols| cols.iter().any(|c| c.eq_ignore_ascii_case(&cname)));
                let via_opt = spec.and_then(|s| s.cols.as_ref()).is_none()
                    && *stored
                    && p.publish_generated_columns == "stored";
                if !listed && !via_opt {
                    return Err(ri_validity_err(
                        is_update,
                        base,
                        "Replica identity must not contain unpublished generated columns.",
                    ));
                }
            }
        }
        cur = eng
            .db
            .find_table(&name, ctx.snap, &ctx.all_xids, ctx.session)
            .and_then(|tt| tt.partition.as_ref())
            .and_then(|pi| pi.parent.clone());
    }
    if !has_ri && any_pub_action {
        if is_update {
            return Err(exec_err(
                "55000",
                format!(
                    "cannot update table \"{}\" because it does not have a replica identity and publishes updates",
                    base
                ),
            ));
        }
        return Err(exec_err(
            "55000",
            format!(
                "cannot delete from table \"{}\" because it does not have a replica identity and publishes deletes",
                base
            ),
        ));
    }
    Ok(())
}

/// Resolve `table` (when it exists) and run the guard on it. A
/// missing table is left to the caller's own 42P01 path.
pub(crate) fn check_replica_identity_for(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    is_update: bool,
) -> Result<(), ExecError> {
    if eng.db.publications.is_empty() {
        return Ok(());
    }
    // PG19: temp relations are never publishable
    // (is_publishable_class requires permanent persistence), so the
    // guard never fires for them — combocid.sql's combocidtest is
    // the canonical case (a TEMP table updated while FOR ALL TABLES
    // publications from earlier suites are live in the database).
    if eng
        .db
        .temp_tables
        .get(&ctx.session)
        .is_some_and(|m| m.contains_key(base_name(table)))
    {
        return Ok(());
    }
    if let Some(t) = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        let t = t.clone();
        check_replica_identity(eng, ctx, &t, table, is_update)?;
    }
    Ok(())
}

/// v2.03: validate a batch of per-table specs (column lists + row
/// filters) and build the catalog map. Also enforces the
/// conflicting-duplicate rule and (optionally) the partitioned-table
/// row-filter rule against `via_root`.
pub(crate) fn build_table_specs(
    eng: &Engine,
    ctx: &StmtCtx,
    specs: &[(String, Option<Vec<String>>, Option<Expr>)],
    via_root: bool,
) -> Result<
    std::collections::HashMap<String, crate::storage::PubTableSpec, crate::fxhash::FxBuildHasher>,
    ExecError,
> {
    let mut out: std::collections::HashMap<
        String,
        crate::storage::PubTableSpec,
        crate::fxhash::FxBuildHasher,
    > = Default::default();
    let mut seen: std::collections::HashMap<String, (bool, bool)> = Default::default();
    for (name, cols, filter) in specs {
        let key = base_name(name).to_lowercase();
        let has_cf = cols.is_some() || filter.is_some();
        if let Some((prev_cf, _)) = seen.get(&key) {
            if *prev_cf || has_cf {
                // PG distinguishes the two messages by which side
                // carries the clause; either error satisfies the
                // statement, so pick by the new entry.
                let what = if filter.is_some() {
                    "WHERE clauses"
                } else {
                    "column lists"
                };
                return Err(exec_err(
                    "42710",
                    format!(
                        "conflicting or redundant {} for table \"{}\"",
                        what,
                        base_name(name)
                    ),
                ));
            }
        }
        seen.insert(key.clone(), (has_cf, filter.is_some()));
        let t = eng
            .db
            .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
            .cloned();
        if let Some(t) = t {
            if let Some(cols) = cols {
                validate_column_list(&t, name, cols)?;
                // v2.05 (PG19 CheckPubRelationColumnList): a column
                // list on a partitioned table requires
                // publish_via_partition_root.
                if !via_root && t.partition.as_ref().is_some_and(|pi| pi.is_partitioned) {
                    return Err(exec_err(
                        "22023",
                        format!(
                            "cannot use column list for relation \"{}\" in publication",
                            name
                        ),
                    ));
                }
            }
            if let Some(f) = filter {
                validate_row_filter(eng, &t, name, f)?;
                if !via_root && t.partition.as_ref().is_some_and(|pi| pi.is_partitioned) {
                    return Err(exec_err(
                        "0A000",
                        format!(
                            "cannot use publication WHERE clause for relation \"{}\"",
                            base_name(name)
                        ),
                    ));
                }
            }
        }
        out.insert(
            key,
            crate::storage::PubTableSpec {
                cols: cols.clone(),
                filter: filter.clone(),
            },
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Publication DDL. Runtime-catalog mutations (see the module docs):
// they take effect immediately, like replication-slot DDL.
// ---------------------------------------------------------------------------

// v2.05: PG19 publicationcmds.c permission model.
// CREATE privilege on the database rides the db_acl REFERENCES bit
// (see roles.rs GrantObject::Database mapping); the database owner
// and superusers hold it implicitly. PUBLIC holds no CREATE by
// default (a fresh PG database grants PUBLIC only CONNECT+TEMP).
pub(crate) fn pub_is_super(eng: &Engine, ctx: &StmtCtx) -> bool {
    crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
}

pub(crate) fn pub_has_db_create(eng: &Engine, ctx: &StmtCtx, role: &str) -> bool {
    if crate::storage::is_superuser_snap(&eng.db, role, ctx.snap, ctx.own) {
        return true;
    }
    if role == eng.db.db_owner {
        return true;
    }
    let closure = crate::storage::role_closure(&eng.db, role, ctx.snap, ctx.own);
    eng.db.db_acl.iter().any(|e| {
        (e.role == "public" || closure.iter().any(|n| n == &e.role))
            && e.privs & crate::storage::PRIV_REFERENCES != 0
    })
}

/// PG19 object_ownercheck: the actor owns the object when they are
/// the owner or a member of the owning role; superusers pass.
pub(crate) fn pub_ownercheck(eng: &Engine, ctx: &StmtCtx, owner: &str) -> bool {
    if pub_is_super(eng, ctx) {
        return true;
    }
    crate::storage::membership_closure(&eng.db, ctx.role, ctx.snap, ctx.own)
        .iter()
        .any(|n| n == owner)
}

pub(crate) fn pub_require_owner(eng: &Engine, ctx: &StmtCtx, name: &str) -> Result<(), ExecError> {
    let owner = eng
        .db
        .publications
        .get(name)
        .map(|p| p.owner.clone())
        .unwrap_or_default();
    if !pub_ownercheck(eng, ctx, &owner) {
        return Err(exec_err(
            "42501",
            format!("must be owner of publication {}", name),
        ));
    }
    Ok(())
}

/// PublicationAddTables' table-ownership rule (also used at CREATE).
pub(crate) fn pub_require_table_owner(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
) -> Result<(), ExecError> {
    if let Some(t) = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        if !pub_ownercheck(eng, ctx, &t.owner.clone()) {
            return Err(exec_err(
                "42501",
                format!("must be owner of table {}", base_name(table)),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_publication(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    all_tables: bool,
    all_sequences: bool,
    except_tables: &[String],
    tables: &[String],
    schemas: &[String],
    publish: &Option<(bool, bool, bool, bool)>,
    via_partition_root: &Option<bool>,
    table_specs: &[(String, Option<Vec<String>>, Option<Expr>)],
    publish_generated_columns: &Option<String>,
) -> Result<ExecResult, ExecError> {
    // v2.05 (PG19 CreatePublication): CREATE privilege on the
    // database first, then the superuser-only shapes.
    if !pub_has_db_create(eng, ctx, ctx.role) {
        return Err(exec_err(
            "42501",
            "permission denied for database postgres".to_string(),
        ));
    }
    if !pub_is_super(eng, ctx) {
        if all_tables || all_sequences {
            return Err(exec_err(
                "42501",
                "must be superuser to create a FOR ALL TABLES or ALL SEQUENCES publication"
                    .to_string(),
            ));
        }
        if !schemas.is_empty() {
            return Err(exec_err(
                "42501",
                "must be superuser to create FOR TABLES IN SCHEMA publication".to_string(),
            ));
        }
    }
    if eng.db.publications.contains_key(name) {
        return Err(exec_err(
            "42710",
            format!("publication \"{}\" already exists", name),
        ));
    }
    // v2.05: PublicationAddTables ownership applies at CREATE too.
    for t in tables.iter().chain(except_tables.iter()) {
        // Missing tables keep their existing 42P01 path below;
        // ownership is only checked for tables that exist.
        if eng
            .db
            .find_table(t, ctx.snap, &ctx.all_xids, ctx.session)
            .is_some()
        {
            pub_require_table_owner(eng, ctx, t)?;
        }
    }
    // Validate named tables and schemas (publicationcmds.c).
    // v2.03: a duplicate table is only an error when neither entry
    // carries a column list / row filter (the conflicting-clause rule
    // in build_table_specs fires first for those).
    {
        let spec_has_cf: std::collections::HashMap<String, bool> = table_specs
            .iter()
            .map(|(n, c, f)| (base_name(n).to_lowercase(), c.is_some() || f.is_some()))
            .collect();
        let mut seen = HashSet::new();
        for t in tables {
            let key = base_name(t).to_lowercase();
            if !seen.insert(key.clone()) {
                if !spec_has_cf.get(&key).copied().unwrap_or(false) {
                    return Err(exec_err(
                        "42710",
                        format!(
                            "relation \"{}\" is already part of publication \"{}\"",
                            base_name(t),
                            name
                        ),
                    ));
                }
            }
            validate_pub_table(eng, ctx, t)?;
        }
    }
    // v2.03: column lists cannot coexist with TABLES IN SCHEMA.
    if !schemas.is_empty() && table_specs.iter().any(|(_, c, _)| c.is_some()) {
        let bad = table_specs
            .iter()
            .find(|(_, c, _)| c.is_some())
            .map(|(n, _, _)| n.clone())
            .unwrap_or_default();
        return Err(exec_err(
            "0A000",
            format!(
                "cannot use column list for relation \"{}\" in publication \"{}\"",
                bad, name
            ),
        ));
    }
    let built_specs =
        build_table_specs(eng, ctx, table_specs, via_partition_root.unwrap_or(false))?;
    let schemas = &resolve_pub_schemas(eng, ctx, schemas)?;
    validate_pub_schemas(eng, schemas)?;
    if all_tables {
        validate_pub_except(eng, ctx, except_tables)?;
    }
    let (pubinsert, pubupdate, pubdelete, pubtruncate) =
        publish.unwrap_or((true, true, true, true));
    // v2.04: allocate the publication's OID (pg_publication.oid).
    let oid = eng.db.next_oid;
    eng.db.next_oid += 1;
    eng.db.publications.insert(
        name.to_string(),
        Publication {
            name: name.to_string(),
            oid,
            comment: None,
            owner: ctx.role.to_string(),
            all_tables,
            all_sequences,
            except_tables: except_tables.to_vec(),
            tables: tables.to_vec(),
            schemas: schemas.to_vec(),
            pubinsert,
            pubupdate,
            pubdelete,
            pubtruncate,
            via_partition_root: via_partition_root.unwrap_or(false),
            publish_generated_columns: publish_generated_columns
                .clone()
                .unwrap_or_else(|| "none".to_string()),
            table_specs: built_specs,
        },
    );
    ctx.writes.push(crate::storage::WriteOp::CreatePublication {
        name: name.to_string(),
        prev: None,
    });
    Ok(ExecResult::Command {
        tag: "CREATE PUBLICATION".to_string(),
    })
}

pub(crate) fn exec_drop_publication(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    // v2.05: DROP PUBLICATION requires ownership (PG19 RemoveObjects
    // owner check), per existing publication in the list.
    for name in names {
        if eng.db.publications.contains_key(name) {
            pub_require_owner(eng, ctx, name)?;
        }
    }
    for name in names {
        match eng.db.publications.remove(name) {
            Some(p) => {
                ctx.writes.push(crate::storage::WriteOp::DropPublication {
                    name: name.clone(),
                    prev: Some(p),
                });
            }
            None => {
                if if_exists {
                    ctx.notices
                        .push(format!("publication \"{}\" does not exist, skipping", name));
                } else {
                    return Err(exec_err(
                        "42P01",
                        format!("publication \"{}\" does not exist", name),
                    ));
                }
            }
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP PUBLICATION".to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_alter_publication(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    action: char,
    spec_all_tables: bool,
    spec_all_sequences: bool,
    spec_except: &[String],
    tables: &[String],
    schemas: &[String],
    publish: &Option<(bool, bool, bool, bool)>,
    via_partition_root: &Option<bool>,
    new_value: &Option<String>,
    table_specs: &[(String, Option<Vec<String>>, Option<Expr>)],
    publish_generated_columns: &Option<String>,
) -> Result<ExecResult, ExecError> {
    if !eng.db.publications.contains_key(name) {
        return Err(exec_err(
            "42P01",
            format!("publication \"{}\" does not exist", name),
        ));
    }
    // Captured for the statement undo log (v2.02): every successful
    // mutation below records the pre-statement entry.
    let prev_pub = eng.db.publications.get(name).cloned();
    // v2.05 (PG19 AlterPublication / CheckAlterPublication /
    // AlterPublicationOwner_internal): ownership + superuser gates.
    pub_require_owner(eng, ctx, name)?;
    if !pub_is_super(eng, ctx) {
        if (action == 'a' || action == 's') && !schemas.is_empty() {
            return Err(exec_err(
                "42501",
                "must be superuser to add or set schemas".to_string(),
            ));
        }
        if action == 's' && spec_all_tables {
            return Err(exec_err(
                "42501",
                "must be superuser to set ALL TABLES".to_string(),
            ));
        }
        if action == 's' && spec_all_sequences {
            return Err(exec_err(
                "42501",
                "must be superuser to set ALL SEQUENCES".to_string(),
            ));
        }
        if action == 'o' {
            let new_owner = new_value.clone().unwrap_or_default();
            let can_set = crate::storage::membership_closure(&eng.db, ctx.role, ctx.snap, ctx.own)
                .iter()
                .any(|n| n == &new_owner);
            let new_has_create = pub_has_db_create(eng, ctx, &new_owner);
            let new_is_super =
                crate::storage::is_superuser_snap(&eng.db, &new_owner, ctx.snap, ctx.own);
            let p = eng.db.publications.get(name).expect("checked above");
            let needs_super_owner = p.all_tables || p.all_sequences || !p.schemas.is_empty();
            if !can_set || !new_has_create || (needs_super_owner && !new_is_super) {
                return Err(exec_err(
                    "42501",
                    format!(
                        "permission denied to change owner of publication \"{}\"",
                        name
                    ),
                ));
            }
        }
    }
    if action == 'a' || action == 's' {
        for t in tables {
            if eng
                .db
                .find_table(t, ctx.snap, &ctx.all_xids, ctx.session)
                .is_some()
            {
                pub_require_table_owner(eng, ctx, t)?;
            }
        }
    }
    match action {
        'r' => {
            let new_name = new_value.clone().unwrap_or_default();
            if eng.db.publications.contains_key(&new_name) {
                return Err(exec_err(
                    "42710",
                    format!("publication \"{}\" already exists", new_name),
                ));
            }
            let mut p = eng.db.publications.remove(name).expect("checked above");
            p.name = new_name.clone();
            eng.db.publications.insert(new_name.clone(), p);
            ctx.writes.push(crate::storage::WriteOp::DropPublication {
                name: new_name,
                prev: None,
            });
            ctx.writes.push(crate::storage::WriteOp::CreatePublication {
                name: name.to_string(),
                prev: prev_pub,
            });
        }
        'o' => {
            if let Some(p) = eng.db.publications.get_mut(name) {
                p.owner = new_value.clone().unwrap_or_default();
            }
            ctx.writes.push(crate::storage::WriteOp::AlterPublication {
                name: name.to_string(),
                prev: prev_pub,
            });
        }
        'p' => {
            // v2.03: publish_via_partition_root=false is refused
            // while a partitioned table in the publication has a
            // row filter (PG19 publicationcmds.c).
            if via_partition_root == &Some(false) {
                let p = eng.db.publications.get(name).expect("checked above");
                for (key, spec) in &p.table_specs {
                    if spec.filter.is_some() {
                        if let Some(t) =
                            eng.db.find_table(key, ctx.snap, &ctx.all_xids, ctx.session)
                        {
                            if t.partition.as_ref().is_some_and(|pi| pi.is_partitioned) {
                                return Err(exec_err(
                                    "0A000",
                                    format!(
                                        "cannot set parameter \"publish_via_partition_root\" to false for publication \"{}\"",
                                        name
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
            if let Some(p) = eng.db.publications.get_mut(name) {
                if let Some((i, u, d, t)) = publish {
                    p.pubinsert = *i;
                    p.pubupdate = *u;
                    p.pubdelete = *d;
                    p.pubtruncate = *t;
                }
                if let Some(v) = via_partition_root {
                    p.via_partition_root = *v;
                }
                if let Some(g) = publish_generated_columns {
                    p.publish_generated_columns = g.clone();
                }
            }
            ctx.writes.push(crate::storage::WriteOp::AlterPublication {
                name: name.to_string(),
                prev: prev_pub,
            });
        }
        'a' | 'd' | 's' => {
            // CURRENT_SCHEMA entries resolve at DDL time (PG19).
            let schemas = &resolve_pub_schemas(eng, ctx, schemas)?;
            let is_all = eng.db.publications.get(name).is_some_and(|p| p.all_tables);
            // PG19: tables/schemas cannot be added to, dropped from,
            // or set on a FOR ALL TABLES publication.
            if is_all && (!tables.is_empty() || !schemas.is_empty()) {
                return Err(exec_err(
                    "55000",
                    format!("publication \"{}\" is defined as FOR ALL TABLES", name),
                ));
            }
            if action == 's' && (spec_all_tables || spec_all_sequences) {
                // SET ALL TABLES/SEQUENCES requires the publication
                // to already be FOR ALL TABLES/SEQUENCES or empty.
                let p = eng.db.publications.get(name).expect("checked above");
                let empty = !p.all_tables
                    && !p.all_sequences
                    && p.tables.is_empty()
                    && p.schemas.is_empty();
                if !p.all_tables && !p.all_sequences && !empty {
                    let what = if spec_all_tables {
                        "ALL TABLES"
                    } else {
                        "ALL SEQUENCES"
                    };
                    return Err(exec_err(
                        "55000",
                        format!(
                            "publication \"{}\" does not support {} operations",
                            name, what
                        ),
                    ));
                }
            }
            // v2.03: DROP with a WHERE clause is an error; ADD/SET
            // validate column lists + row filters and the
            // column-list/schema coexistence rules.
            if action == 'd' && table_specs.iter().any(|(_, _, f)| f.is_some()) {
                return Err(exec_err(
                    "0A000",
                    "cannot use a WHERE clause when removing a table from a publication"
                        .to_string(),
                ));
            }
            let cur_via = eng
                .db
                .publications
                .get(name)
                .map(|p| p.via_partition_root)
                .unwrap_or(false);
            let built_specs = if action == 'd' {
                Default::default()
            } else {
                build_table_specs(eng, ctx, table_specs, cur_via)?
            };
            if action == 'a' || action == 's' {
                let new_has_cols = table_specs.iter().any(|(_, c, _)| c.is_some());
                let p = eng.db.publications.get(name).expect("checked above");
                let existing_has_cols = p.table_specs.values().any(|sp| sp.cols.is_some());
                if new_has_cols
                    && (action == 's' && !schemas.is_empty()
                        || action == 'a' && !p.schemas.is_empty())
                {
                    let bad = table_specs
                        .iter()
                        .find(|(_, c, _)| c.is_some())
                        .map(|(n, _, _)| n.clone())
                        .unwrap_or_default();
                    return Err(exec_err(
                        "0A000",
                        format!(
                            "cannot use column list for relation \"{}\" in publication \"{}\"",
                            bad, name
                        ),
                    ));
                }
                if !schemas.is_empty() && action != 's' && existing_has_cols {
                    return Err(exec_err(
                        "0A000",
                        format!("cannot add schema to publication \"{}\"", name),
                    ));
                }
                if action == 's' && !schemas.is_empty() && new_has_cols {
                    // covered above (SET with both) — same error.
                }
            }
            // Validate entries (PG errors on missing relations/schemas
            // for ADD and SET; DROP of a missing schema also errors,
            // DROP of a missing table is a no-op in PG only when the
            // table exists but is not a member — a missing relation
            // errors too, so validate uniformly).
            for t in tables {
                validate_pub_table(eng, ctx, t)?;
            }
            validate_pub_schemas(eng, schemas)?;
            if action == 's' && spec_all_tables {
                validate_pub_except(eng, ctx, spec_except)?;
            }
            // PG19: DROP of a non-member table/schema is an error.
            if action == 'd' {
                let p = eng.db.publications.get(name).expect("checked above");
                for t in tables {
                    if !p
                        .tables
                        .iter()
                        .any(|x| base_name(x).eq_ignore_ascii_case(base_name(t)))
                    {
                        return Err(exec_err(
                            "55000",
                            format!(
                                "relation \"{}\" is not part of the publication",
                                base_name(t)
                            ),
                        ));
                    }
                }
                for s in schemas {
                    if !p.schemas.iter().any(|x| x.eq_ignore_ascii_case(s)) {
                        return Err(exec_err(
                            "55000",
                            format!(
                                "tables from schema \"{}\" are not part of the publication",
                                s
                            ),
                        ));
                    }
                }
            }
            if let Some(p) = eng.db.publications.get_mut(name) {
                match action {
                    'a' => {
                        for t in tables {
                            if !p.tables.iter().any(|x| x == t) {
                                p.tables.push(t.clone());
                            }
                        }
                        for s in schemas {
                            if !p.schemas.iter().any(|x| x == s) {
                                p.schemas.push(s.clone());
                            }
                        }
                        for (k, v) in built_specs {
                            p.table_specs.insert(k, v);
                        }
                    }
                    'd' => {
                        p.tables.retain(|x| !tables.iter().any(|t| t == x));
                        p.schemas.retain(|x| !schemas.iter().any(|s| s == x));
                        for t in tables {
                            p.table_specs.remove(&base_name(t).to_lowercase());
                        }
                    }
                    _ => {
                        if spec_all_tables || spec_all_sequences {
                            p.all_tables = spec_all_tables;
                            p.all_sequences = spec_all_sequences;
                            p.except_tables = spec_except.to_vec();
                            p.tables.clear();
                            p.schemas.clear();
                            p.table_specs.clear();
                        } else {
                            p.tables = tables.to_vec();
                            p.schemas = schemas.to_vec();
                            p.all_tables = false;
                            p.all_sequences = false;
                            p.except_tables.clear();
                            p.table_specs = built_specs;
                        }
                    }
                }
            }
            ctx.writes.push(crate::storage::WriteOp::AlterPublication {
                name: name.to_string(),
                prev: prev_pub,
            });
        }
        _ => {}
    }
    Ok(ExecResult::Command {
        tag: "ALTER PUBLICATION".to_string(),
    })
}

// ---------------------------------------------------------------------------
// v2.04: catalog-view support — the effective published-relation list
// of a publication (PG19 pg_publication.c GetPublicationRelations /
// GetAllPublicationRelations + the pg_get_publication_tables worker),
// shared by the pg_publication_tables view and the
// pg_get_publication_tables set-returning function.
// ---------------------------------------------------------------------------

/// One effective published relation: the relation a subscriber sees.
pub(crate) struct PubRelRow {
    /// Storage key of the effective relation (root when published via
    /// partition root, the partition itself otherwise).
    pub rel_key: String,
    pub schemaname: String,
    pub tablename: String,
    /// Published column names, in relation column order.
    pub attnames: Vec<String>,
    /// Attnums parallel to `attnames` (pg_get_publication_tables'
    /// int2vector payload).
    pub attnums: Vec<i16>,
    /// Row filter of the defining entry, if any.
    pub filter: Option<Expr>,
}

fn pub_table_of<'e>(
    db: &'e Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    key: &str,
) -> Option<&'e crate::storage::Table> {
    db.find_table(key, snap, &[own], session)
}

fn pub_schema_of(db: &Database, key: &str) -> String {
    if let Some((s, _)) = key.rsplit_once('.') {
        return s.to_string();
    }
    db.obj_schema
        .get(key)
        .cloned()
        .unwrap_or_else(|| "public".to_string())
}

/// All descendant partition keys of `key` (recursive, cycle-safe).
fn pub_descendants(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    key: &str,
    out: &mut Vec<String>,
) {
    let Some(t) = pub_table_of(db, snap, own, session, key) else {
        return;
    };
    let children = t
        .partition
        .as_ref()
        .map(|p| p.children.clone())
        .unwrap_or_default();
    for c in children {
        if !out.contains(&c) {
            out.push(c.clone());
            pub_descendants(db, snap, own, session, &c, out);
        }
    }
}

/// Partition ancestors of `key`, nearest first (storage keys).
fn pub_ancestors(db: &Database, snap: &Snapshot, own: u64, session: u64, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = key.to_string();
    for _ in 0..32 {
        let Some(t) = pub_table_of(db, snap, own, session, &cur) else {
            break;
        };
        let Some(parent) = t.partition.as_ref().and_then(|p| p.parent.clone()) else {
            break;
        };
        // Parent names are stored as written; resolve to the storage
        // key by base-name match against the tables map.
        let pkey = db
            .tables
            .keys()
            .find(|k| base_name(k).eq_ignore_ascii_case(base_name(&parent)))
            .cloned()
            .unwrap_or(parent);
        if out.contains(&pkey) {
            break;
        }
        out.push(pkey.clone());
        cur = pkey;
    }
    out
}

fn pub_is_partitioned(db: &Database, snap: &Snapshot, own: u64, session: u64, key: &str) -> bool {
    pub_table_of(db, snap, own, session, key)
        .and_then(|t| t.partition.as_ref())
        .is_some_and(|p| p.is_partitioned)
}

/// The effective published relations of `p` (see the section header).
pub(crate) fn publication_rel_rows(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    p: &Publication,
) -> Vec<PubRelRow> {
    // 1. Candidate entry keys (storage keys), in deterministic order.
    let mut entries: Vec<String> = Vec::new();
    let push_entry = |entries: &mut Vec<String>, key: String| {
        if !entries.contains(&key) {
            entries.push(key);
        }
    };
    if p.all_tables {
        let mut keys: Vec<&String> = db.tables.keys().collect();
        keys.sort();
        for k in keys {
            let base = base_name(k);
            if base.starts_with("pg_toast") || k.starts_with("pg_toast.") {
                continue;
            }
            if pub_schema_of(db, k).eq_ignore_ascii_case("pg_catalog") {
                continue;
            }
            if pub_table_of(db, snap, own, session, k).is_none() {
                continue;
            }
            push_entry(&mut entries, k.clone());
        }
    } else {
        for t in &p.tables {
            if let Some(k) = db
                .tables
                .keys()
                .find(|k| base_name(k).eq_ignore_ascii_case(base_name(t)))
            {
                push_entry(&mut entries, k.clone());
            }
        }
        if !p.schemas.is_empty() {
            let mut keys: Vec<&String> = db.tables.keys().collect();
            keys.sort();
            for k in keys {
                let base = base_name(k);
                if base.starts_with("pg_toast") || k.starts_with("pg_toast.") {
                    continue;
                }
                if p.schemas
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(&pub_schema_of(db, k)))
                    && pub_table_of(db, snap, own, session, k).is_some()
                {
                    push_entry(&mut entries, k.clone());
                }
            }
        }
    }
    // EXCEPT removes the named relations.
    if !p.except_tables.is_empty() {
        entries.retain(|k| {
            !p.except_tables
                .iter()
                .any(|e| base_name(e).eq_ignore_ascii_case(base_name(k)))
        });
    }
    // 2. Partition resolution.
    let mut effective: Vec<String> = Vec::new();
    if p.via_partition_root {
        for e in &entries {
            // Published via the topmost published ancestor, if any.
            let anc = pub_ancestors(db, snap, own, session, e);
            let top = anc.iter().rev().find(|a| entries.contains(a)).cloned();
            match top {
                Some(root) => {
                    if !effective.contains(&root) {
                        effective.push(root);
                    }
                }
                None => {
                    // An unpublished ancestor chain: if any ancestor
                    // is an entry the loop above found it; a partition
                    // whose root is not published stands alone unless
                    // a nearer ancestor is an entry (handled by top).
                    let nearer = anc.iter().find(|a| entries.contains(a)).cloned();
                    match nearer {
                        Some(root) => {
                            if !effective.contains(&root) {
                                effective.push(root);
                            }
                        }
                        None => {
                            if !effective.contains(e) {
                                effective.push(e.clone());
                            }
                        }
                    }
                }
            }
        }
    } else {
        for e in &entries {
            if pub_is_partitioned(db, snap, own, session, e) {
                // Expand to descendant leaves: descendants that are
                // not themselves partitioned-with-children.
                let mut desc = Vec::new();
                pub_descendants(db, snap, own, session, e, &mut desc);
                let mut leaves: Vec<String> = desc
                    .into_iter()
                    .filter(|d| {
                        let has_children = pub_table_of(db, snap, own, session, d)
                            .and_then(|t| t.partition.as_ref())
                            .is_some_and(|pi| !pi.children.is_empty());
                        !has_children
                    })
                    .collect();
                if leaves.is_empty() {
                    // Childless partitioned table: it is its own row.
                    leaves.push(e.clone());
                }
                for l in leaves {
                    if !effective.contains(&l) {
                        effective.push(l);
                    }
                }
            } else if !effective.contains(e) {
                effective.push(e.clone());
            }
        }
        // A partition explicitly entered whose partitioned ancestor
        // was also entered and expanded already includes it; an
        // excepted ancestor's partitions stay excluded (entries were
        // filtered before expansion only for exact names, so re-check:
        // partitions of an EXCEPTed root are not published).
        if !p.except_tables.is_empty() {
            let excepted: Vec<String> = p
                .except_tables
                .iter()
                .filter_map(|e| {
                    db.tables
                        .keys()
                        .find(|k| base_name(k).eq_ignore_ascii_case(base_name(e)))
                        .cloned()
                })
                .collect();
            effective.retain(|k| {
                !excepted.contains(k)
                    && !pub_ancestors(db, snap, own, session, k)
                        .iter()
                        .any(|a| excepted.contains(a))
            });
        }
    }
    // EXCEPT under via-root: partitions of an excepted root are out
    // (their effective root was the excepted entry, already removed,
    // so they may have leaked in as standalone rows).
    if p.via_partition_root && !p.except_tables.is_empty() {
        let excepted: Vec<String> = p
            .except_tables
            .iter()
            .filter_map(|e| {
                db.tables
                    .keys()
                    .find(|k| base_name(k).eq_ignore_ascii_case(base_name(e)))
                    .cloned()
            })
            .collect();
        effective.retain(|k| {
            !excepted.contains(k)
                && !pub_ancestors(db, snap, own, session, k)
                    .iter()
                    .any(|a| excepted.contains(a))
        });
    }
    effective.sort();
    // 3. Specs + column sets.
    let mut rows = Vec::new();
    for key in effective {
        let Some(t) = pub_table_of(db, snap, own, session, &key) else {
            continue;
        };
        // The defining spec: own entry first, then partition
        // ancestors (PG: a parent entry's column list / row filter
        // governs partitions published through it).
        let mut spec: Option<&PubTableSpec> = None;
        let mut chain = vec![key.clone()];
        chain.extend(pub_ancestors(db, snap, own, session, &key));
        for cand in &chain {
            if let Some(s) = p.table_specs.get(&base_name(cand).to_lowercase()) {
                // Only explicit FOR TABLE entries carry specs.
                if p.tables
                    .iter()
                    .any(|e| base_name(e).eq_ignore_ascii_case(base_name(cand)))
                {
                    spec = Some(s);
                    break;
                }
            }
        }
        let (attnames, attnums): (Vec<String>, Vec<i16>) = match spec.and_then(|s| s.cols.as_ref())
        {
            Some(cols) => {
                let mut names = Vec::new();
                let mut nums = Vec::new();
                for c in cols {
                    if let Some(pos) = t
                        .columns
                        .iter()
                        .position(|(n, _)| n.eq_ignore_ascii_case(c))
                    {
                        names.push(t.columns[pos].0.clone());
                        nums.push(t.attnums.get(pos).copied().unwrap_or(pos as i16 + 1));
                    }
                }
                (names, nums)
            }
            None => (
                t.columns.iter().map(|(n, _)| n.clone()).collect(),
                t.columns
                    .iter()
                    .enumerate()
                    .map(|(i, _)| t.attnums.get(i).copied().unwrap_or(i as i16 + 1))
                    .collect(),
            ),
        };
        rows.push(PubRelRow {
            schemaname: pub_schema_of(db, &key),
            tablename: base_name(&key).to_string(),
            rel_key: key,
            attnames,
            attnums,
            filter: spec.and_then(|s| s.filter.clone()),
        });
    }
    rows
}

/// v2.04: enumerate pg_publication_rel rows (explicit + EXCEPT
/// entries only, as PG) with deterministic synthetic OIDs. The OID
/// is a pure function of catalog position, so pg_describe_object can
/// resolve it by re-enumerating within the same statement.
pub(crate) fn publication_rel_entries(
    db: &Database,
) -> Vec<(u32, String, String, bool, Option<Expr>, Option<Vec<String>>)> {
    // (oid, pubname, table-as-written, prexcept, filter, cols)
    let mut pubs: Vec<&Publication> = db.publications.values().collect();
    pubs.sort_by_key(|p| p.oid);
    let mut out = Vec::new();
    let mut idx: u32 = 0;
    for p in pubs {
        let mut ents: Vec<(String, bool)> = p.tables.iter().map(|t| (t.clone(), false)).collect();
        ents.extend(p.except_tables.iter().map(|t| (t.clone(), true)));
        for (t, except) in ents {
            idx += 1;
            let spec = p.table_specs.get(&base_name(&t).to_lowercase());
            out.push((
                610_600 + idx,
                p.name.clone(),
                t,
                except,
                if except {
                    None
                } else {
                    spec.and_then(|s| s.filter.clone())
                },
                if except {
                    None
                } else {
                    spec.and_then(|s| s.cols.clone())
                },
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// v2.04: Expr -> SQL text deparsing for row filters, in PG's
// pg_get_expr style (the whole qual parenthesized, precedence-driven
// inner parens). Covers the expression shapes publication row
// filters use in practice; exotic shapes fall back to a bare
// rendering of their head form.
// ---------------------------------------------------------------------------

fn deparse_prec(e: &Expr) -> u8 {
    match e {
        Expr::Or(..) => 1,
        Expr::And(..) => 2,
        Expr::Not(_) => 3,
        Expr::Cmp { .. }
        | Expr::IsNull { .. }
        | Expr::IsBool { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::IsDistinctFrom { .. } => 4,
        Expr::Arith { op, .. } => match op {
            ArithOp::Add | ArithOp::Sub => 5,
            ArithOp::Mul | ArithOp::Div | ArithOp::Mod => 6,
            _ => 7,
        },
        _ => 9,
    }
}

fn deparse_child(e: &Expr, parent_prec: u8) -> String {
    let s = deparse_inner(e);
    if deparse_prec(e) < parent_prec {
        format!("({})", s)
    } else {
        s
    }
}

fn deparse_lit(l: &Literal) -> String {
    match l {
        Literal::Int(v) => v.to_string(),
        Literal::BigInt(v) => v.to_string(),
        Literal::SmallInt(v) => v.to_string(),
        Literal::Float(v) => format!("{}", v),
        Literal::Decimal(s) => s.clone(),
        Literal::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Literal::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Literal::Null => "NULL".to_string(),
        _ => "NULL".to_string(),
    }
}

fn deparse_inner(e: &Expr) -> String {
    match e {
        Expr::Column { name, .. } => name.clone(),
        Expr::ResolvedCol { .. } => "?".to_string(),
        Expr::Literal(l) => deparse_lit(l),
        Expr::Arith { op, left, right } => {
            let p = deparse_prec(e);
            format!(
                "{} {} {}",
                deparse_child(left, p),
                op.sql(),
                deparse_child(right, p)
            )
        }
        Expr::Cmp { op, left, right } => {
            let os = match op {
                CmpOp::Eq => "=",
                CmpOp::Ne => "<>",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
                CmpOp::ImageEq => "*=",
            };
            format!(
                "{} {} {}",
                deparse_child(left, 4),
                os,
                deparse_child(right, 4)
            )
        }
        Expr::And(l, r) => format!("{} AND {}", deparse_child(l, 2), deparse_child(r, 2)),
        Expr::Or(l, r) => format!("{} OR {}", deparse_child(l, 1), deparse_child(r, 1)),
        Expr::Not(inner) => format!("NOT {}", deparse_child(inner, 3)),
        Expr::IsNull { expr, neg } => format!(
            "{} IS {}NULL",
            deparse_child(expr, 4),
            if *neg { "NOT " } else { "" }
        ),
        Expr::Func { name, args } => {
            let parts: Vec<String> = args.iter().map(deparse_inner).collect();
            format!("{}({})", name, parts.join(", "))
        }
        Expr::Cast { expr, to, .. } => {
            format!("{}::{}", deparse_child(expr, 9), to.sql_name())
        }
        Expr::ArrayCtor { elems, .. } => {
            let parts: Vec<String> = elems.iter().map(deparse_inner).collect();
            format!("ARRAY[{}]", parts.join(", "))
        }
        Expr::UserOp { op, left, right } => format!(
            "{} {} {}",
            deparse_child(left, 4),
            op,
            deparse_child(right, 4)
        ),
        Expr::JsonOp { op, left, right } => format!(
            "{} {} {}",
            deparse_child(left, 4),
            op.sql(),
            deparse_child(right, 4)
        ),
        _ => "true".to_string(),
    }
}

/// PG pg_get_expr form of a publication row filter: parenthesized.
pub(crate) fn deparse_filter(e: &Expr) -> String {
    format!("({})", deparse_inner(e))
}

// ---------------------------------------------------------------------------
// v2.04: pg_get_publication_tables (PG19 pg_publication.c) — the
// set-returning function behind pg_publication_tables and the
// publication suite's test_gpt helper. One-argument form returns
// (relid, attrs, qual) for every effective relation of every named
// publication; the two-argument form additionally filters to one
// relation and reports (pubid, relid, attrs, qual). attrs is the
// int2vector payload rendered as text ("1 2 3"); qual is the row
// filter in pg_get_expr text form.
// ---------------------------------------------------------------------------

pub(crate) fn gpt_col_names(nargs: usize) -> Vec<String> {
    let names: &[&str] = if nargs >= 2 {
        &["pubid", "relid", "attrs", "qual"]
    } else {
        &["relid", "attrs", "qual"]
    };
    names.iter().map(|s| s.to_string()).collect()
}

pub(crate) fn gpt_col_types(nargs: usize) -> Vec<ColType> {
    if nargs >= 2 {
        vec![ColType::Int, ColType::Int, ColType::Text, ColType::Text]
    } else {
        vec![ColType::Int, ColType::Text, ColType::Text]
    }
}

pub(crate) fn pg_get_publication_tables_rows(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    arg_vals: &[Value],
) -> Result<Vec<Row>, ExecError> {
    let Some(first) = arg_vals.first() else {
        return Err(exec_err(
            "42883",
            "function pg_get_publication_tables() does not exist".to_string(),
        ));
    };
    let pubnames: Vec<String> = match first {
        Value::Array(a) => a
            .elems
            .iter()
            .filter_map(|v| match v {
                Value::Text(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        // Array parameters substitute as `{...}` text literals in
        // function bodies (params.rs value_to_literal); parse the
        // literal back, like downstream coercion would.
        Value::Text(s) => {
            match super::eval::parse_array_literal(s, crate::storage::ArrayElem::Text) {
                Ok(a) => a
                    .elems
                    .iter()
                    .filter_map(|v| match v {
                        Value::Text(s2) => Some(s2.to_string()),
                        _ => None,
                    })
                    .collect(),
                Err(_) => {
                    return Err(exec_err(
                        "42883",
                        "function pg_get_publication_tables() does not exist".to_string(),
                    ));
                }
            }
        }
        Value::Null => Vec::new(),
        _ => {
            return Err(exec_err(
                "42883",
                "function pg_get_publication_tables() does not exist".to_string(),
            ));
        }
    };
    let want_relid: Option<i64> = if arg_vals.len() >= 2 {
        match arg_vals[1] {
            Value::Int(v) => Some(v),
            Value::Null => None,
            _ => None,
        }
    } else {
        None
    };
    let mut out = Vec::new();
    for name in &pubnames {
        let Some(p) = db.publications.get(name) else {
            continue; // PG: unknown publications contribute no rows.
        };
        for r in publication_rel_rows(db, snap, own, session, p) {
            let Some(t) = pub_table_of(db, snap, own, session, &r.rel_key) else {
                continue;
            };
            let relid = t.oid as i64;
            if let Some(w) = want_relid {
                if w != relid {
                    continue;
                }
            }
            let attrs = r
                .attnums
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let qual = match &r.filter {
                Some(f) => Value::text(deparse_filter(f)),
                None => Value::Null,
            };
            if arg_vals.len() >= 2 {
                out.push(Row::new(vec![
                    Value::Int(p.oid as i64),
                    Value::Int(relid),
                    Value::text(attrs),
                    qual,
                ]));
            } else {
                out.push(Row::new(vec![Value::Int(relid), Value::text(attrs), qual]));
            }
        }
    }
    Ok(out)
}

/// v2.04: COMMENT ON PUBLICATION — stores the comment on the
/// publication record (PG keeps it in pg_description; here
/// obj_description reads it back from the record). Transactional
/// via the AlterPublication undo entry, WAL-logged and checkpointed
/// with the rest of the record.
pub(crate) fn exec_comment_publication(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    comment: &Option<String>,
) -> Result<ExecResult, ExecError> {
    let Some(p) = eng.db.publications.get_mut(name) else {
        return Err(exec_err(
            "42P01",
            format!("publication \"{}\" does not exist", name),
        ));
    };
    let prev = p.clone();
    p.comment = comment.clone();
    ctx.writes.push(crate::storage::WriteOp::AlterPublication {
        name: name.to_string(),
        prev: Some(prev),
    });
    Ok(ExecResult::Command {
        tag: "COMMENT".to_string(),
    })
}
