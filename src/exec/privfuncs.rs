// v1.96: the has_*_privilege function family, pg_has_role,
// current_setting, and the acl.c surface (makeaclitem, acldefault,
// aclcontains, pg_get_acl), plus regrole/aclitem cast support.
//
// Semantics grounded in PG19 src/backend/utils/adt/acl.c and
// src/backend/catalog/aclchk.c (see hidden_files/v196/scope.md).
use super::*;
use crate::storage::{
    PRIV_ALL_TABLE, PRIV_DELETE, PRIV_INSERT, PRIV_REFERENCES, PRIV_SELECT, PRIV_TRIGGER,
    PRIV_TRUNCATE, PRIV_UPDATE, PRIV_USAGE,
};

pub(crate) const PRIV_MAINTAIN: u32 = 512;

/// Privilege letters in aclitem canonical order (acl.c AclMode bits).
pub(crate) fn priv_letter(bit: u32) -> char {
    match bit {
        PRIV_INSERT => 'a',
        PRIV_SELECT => 'r',
        PRIV_UPDATE => 'w',
        PRIV_DELETE => 'd',
        PRIV_TRUNCATE => 'D',
        PRIV_REFERENCES => 'x',
        PRIV_TRIGGER => 't',
        _ => '?',
    }
}

/// Render a table-ACL bitmask as aclitem privilege letters.
pub(crate) fn table_priv_letters(bits: u32) -> String {
    let mut s = String::new();
    for bit in [
        PRIV_INSERT,
        PRIV_SELECT,
        PRIV_UPDATE,
        PRIV_DELETE,
        PRIV_TRUNCATE,
        PRIV_REFERENCES,
        PRIV_TRIGGER,
    ] {
        if bits & bit == bit {
            s.push(priv_letter(bit));
        }
    }
    if bits & PRIV_MAINTAIN == PRIV_MAINTAIN {
        s.push('m');
    }
    s
}

/// Quote a role name for aclitem output (acl.c getid/quote logic):
/// quote when it contains anything but [a-z0-9_], doubling quotes.
pub(crate) fn acl_quote_name(name: &str) -> String {
    if !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Render one aclitem: `grantee=privs/grantor` (empty grantee = PUBLIC).
pub(crate) fn aclitem_text(grantee: &str, letters: &str, grantor: &str) -> String {
    format!(
        "{}={}/{}",
        if grantee == "public" {
            String::new()
        } else {
            acl_quote_name(grantee)
        },
        letters,
        acl_quote_name(grantor)
    )
}

/// Parse a privilege specification string ("SELECT", "insert",
/// "SELECT WITH GRANT OPTION", comma lists) into (bits, grant_option)
/// for the table family. Unknown words are 22023 (acl.c).
pub(crate) fn parse_table_priv_spec(s: &str) -> Result<(u32, bool), ExecError> {
    let mut bits = 0u32;
    let mut grant_option = false;
    for part in s.split(',') {
        let p = part.trim().to_ascii_uppercase();
        let (word, go) = match p.strip_suffix(" WITH GRANT OPTION") {
            Some(w) => (w.trim().to_string(), true),
            None => (p, false),
        };
        grant_option |= go;
        bits |= match word.as_str() {
            "SELECT" => PRIV_SELECT,
            "INSERT" => PRIV_INSERT,
            "UPDATE" => PRIV_UPDATE,
            "DELETE" => PRIV_DELETE,
            "TRUNCATE" => PRIV_TRUNCATE,
            "REFERENCES" => PRIV_REFERENCES,
            "TRIGGER" => PRIV_TRIGGER,
            "MAINTAIN" => PRIV_MAINTAIN,
            "ALL" | "ALL PRIVILEGES" => PRIV_ALL_TABLE | PRIV_MAINTAIN,
            _ => {
                return Err(exec_err(
                    "22023",
                    format!(
                        "unrecognized privilege type: \"{}\"",
                        s.trim().to_lowercase()
                    ),
                ));
            }
        };
    }
    Ok((bits, grant_option))
}

/// Generic single-word privilege check for the non-table families.
pub(crate) fn parse_named_priv(s: &str, valid: &[(&str, u32)]) -> Result<(u32, bool), ExecError> {
    let mut bits = 0u32;
    let mut grant_option = false;
    for part in s.split(',') {
        let p = part.trim().to_ascii_uppercase();
        let (word, go) = match p.strip_suffix(" WITH GRANT OPTION") {
            Some(w) => (w.trim().to_string(), true),
            None => (p, false),
        };
        grant_option |= go;
        match valid.iter().find(|(n, _)| *n == word) {
            Some((_, b)) => bits |= b,
            None => {
                return Err(exec_err(
                    "22023",
                    format!(
                        "unrecognized privilege type: \"{}\"",
                        s.trim().to_lowercase()
                    ),
                ));
            }
        }
    }
    Ok((bits, grant_option))
}

/// Resolve a role argument (name text or OID int) for has_* functions.
/// `Ok(None)` means "return NULL/false per the oid form": PG returns
/// NULL from the *table*-oid-miss path but false for a missing role
/// OID in most has_* forms — callers use the `missing_oid_result`.
pub(crate) fn resolve_role_arg(q: &Q, v: &Value) -> Result<Option<String>, ExecError> {
    match v {
        Value::Null => Ok(None),
        Value::Text(s) | Value::BpChar(s) => {
            let name = s.to_string();
            if q.eng.db.find_role(&name, q.snap, q.own).is_some() {
                Ok(Some(name))
            } else {
                Err(exec_err(
                    "42704",
                    format!("role \"{}\" does not exist", name),
                ))
            }
        }
        Value::Int(i) | Value::BigInt(i) => Ok(role_name_for_oid(q, *i as u32)),
        Value::SmallInt(i) => Ok(role_name_for_oid(q, *i as u32)),
        other => Err(exec_err(
            "42846",
            format!("cannot cast type {} to oid", other.type_name()),
        )),
    }
}

/// Reverse OID -> role name. Role OIDs are name_oid() hashes, except
/// the bootstrap superuser whose PG OID is 10.
pub(crate) fn role_name_for_oid(q: &Q, oid: u32) -> Option<String> {
    if oid == 10 {
        return Some("postgres".to_string());
    }
    let mut names: Vec<&String> = q.eng.db.roles.keys().collect();
    names.sort();
    for n in names {
        if name_oid(n) == oid {
            return Some(n.clone());
        }
    }
    None
}

/// Role OID for a name (name_oid hash; postgres = 10 like PG).
pub(crate) fn role_oid(name: &str) -> u32 {
    if name == "postgres" {
        10
    } else {
        name_oid(name)
    }
}

pub(crate) enum TableRef {
    Real(String),
    Catalog(String),
}

/// Resolve a table argument (name text or OID) for has_table_privilege.
/// Name-miss is 42P01; OID-miss is None (caller returns NULL).
pub(crate) fn resolve_table_arg(q: &Q, v: &Value) -> Result<Option<TableRef>, ExecError> {
    match v {
        Value::Null => Ok(None),
        Value::Text(s) | Value::BpChar(s) => {
            let raw = s.to_string();
            let name = raw.rsplit('.').next().unwrap_or(&raw).to_string();
            if q.eng
                .db
                .find_table(&raw, q.snap, &q.all_xids, q.session)
                .is_some()
            {
                return Ok(Some(TableRef::Real(raw)));
            }
            if q.eng
                .db
                .find_table(&name, q.snap, &q.all_xids, q.session)
                .is_some()
            {
                return Ok(Some(TableRef::Real(name)));
            }
            if q.eng.db.find_view(&raw, q.snap, q.own).is_some()
                || q.eng.db.find_view(&name, q.snap, q.own).is_some()
            {
                return Ok(Some(TableRef::Catalog(format!("view:{}", name))));
            }
            if is_catalog_relation(&name) {
                return Ok(Some(TableRef::Catalog(name)));
            }
            Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", raw),
            ))
        }
        Value::Int(i) | Value::BigInt(i) => {
            let oid = *i as u32;
            for (n, vs) in &q.eng.db.tables {
                if vs
                    .iter()
                    .any(|t| t.oid == oid && crate::storage::table_visible(t, q.snap, &q.all_xids))
                {
                    return Ok(Some(TableRef::Real(n.clone())));
                }
            }
            if oid == 1259 {
                return Ok(Some(TableRef::Catalog("pg_class".to_string())));
            }
            // v1.96: well-known catalog oids (mirrors pg_class_scan).
            let cat = match oid {
                1260 => Some("pg_authid"),
                1247 => Some("pg_type"),
                1255 => Some("pg_proc"),
                2615 => Some("pg_namespace"),
                2328 => Some("pg_default_acl"),
                1211 => Some("pg_shdepend"),
                2613 => Some("pg_largeobject"),
                2995 => Some("pg_largeobject_metadata"),
                _ => None,
            };
            if let Some(c) = cat {
                return Ok(Some(TableRef::Catalog(c.to_string())));
            }
            Ok(None)
        }
        Value::SmallInt(i) => resolve_table_arg(q, &Value::Int(*i as i64)),
        other => Err(exec_err(
            "42846",
            format!("cannot cast type {} to regclass", other.type_name()),
        )),
    }
}

/// Virtual catalog relations has_table_privilege knows about. PUBLIC
/// holds SELECT on all of them except the sensitive denylist
/// (aclchk.c: pg_authid has no PUBLIC privileges in PG19).
pub(crate) fn is_catalog_relation(name: &str) -> bool {
    // v1.96: exact catalog/view names only — a prefix match made
    // has_table_privilege('pg_shad') succeed where PG errors
    // (pg_shad is not a relation; pg_shadow is).
    if name.starts_with("information_schema.") {
        return true;
    }
    matches!(
        name,
        "pg_aggregate"
            | "pg_aios"
            | "pg_am"
            | "pg_attribute"
            | "pg_auth_members"
            | "pg_authid"
            | "pg_available_extension_versions"
            | "pg_available_extensions"
            | "pg_backend_memory_contexts"
            | "pg_cast"
            | "pg_class"
            | "pg_collation"
            | "pg_config"
            | "pg_constraint"
            | "pg_conversion"
            | "pg_cursors"
            | "pg_database"
            | "pg_default_acl"
            | "pg_description"
            | "pg_dsm_registry_allocations"
            | "pg_extension"
            | "pg_file_settings"
            | "pg_foreign_data_wrapper"
            | "pg_foreign_server"
            | "pg_foreign_table"
            | "pg_group"
            | "pg_index"
            | "pg_indexes"
            | "pg_inherits"
            | "pg_language"
            | "pg_largeobject"
            | "pg_largeobject_metadata"
            | "pg_locks"
            | "pg_matviews"
            | "pg_namespace"
            | "pg_opclass"
            | "pg_operator"
            | "pg_partitioned_table"
            | "pg_policies"
            | "pg_policy"
            | "pg_prepared_statements"
            | "pg_prepared_xacts"
            | "pg_proc"
            | "pg_publication"
            | "pg_publication_rel"
            | "pg_replication_origin"
            | "pg_replication_slots"
            | "pg_rewrite"
            | "pg_roles"
            | "pg_rules"
            | "pg_seclabel"
            | "pg_sequence"
            | "pg_sequences"
            | "pg_settings"
            | "pg_shadow"
            | "pg_shdepend"
            | "pg_shdescription"
            | "pg_shmem_allocations"
            | "pg_shmem_allocations_numa"
            | "pg_stat_activity"
            | "pg_stat_all_indexes"
            | "pg_stat_all_tables"
            | "pg_stat_archiver"
            | "pg_stat_bgwriter"
            | "pg_stat_checkpointer"
            | "pg_stat_database"
            | "pg_stat_database_conflicts"
            | "pg_stat_io"
            | "pg_stat_replication"
            | "pg_stat_slru"
            | "pg_stat_statements"
            | "pg_stat_subscription"
            | "pg_stat_user_functions"
            | "pg_stat_user_indexes"
            | "pg_stat_user_tables"
            | "pg_stat_wal"
            | "pg_stat_xact_user_functions"
            | "pg_stat_xact_user_tables"
            | "pg_statistic"
            | "pg_statistic_ext"
            | "pg_stats"
            | "pg_subscription"
            | "pg_tables"
            | "pg_tablespace"
            | "pg_timezone_abbrevs"
            | "pg_timezone_names"
            | "pg_trigger"
            | "pg_type"
            | "pg_user"
            | "pg_user_mapping"
            | "pg_user_mappings"
            | "pg_views"
            | "pg_wait_events"
    )
}

pub(crate) fn catalog_table_privs(name: &str, role: &str, q: &Q) -> u32 {
    if crate::storage::is_superuser_snap(&q.eng.db, role, q.snap, q.own) || role == "postgres" {
        return PRIV_ALL_TABLE | PRIV_MAINTAIN;
    }
    if name.starts_with("view:") {
        let vname = &name[5..];
        if let Some(v) = q.eng.db.find_view(vname, q.snap, q.own) {
            if v.owner == role {
                return PRIV_ALL_TABLE | PRIV_MAINTAIN;
            }
        }
        return 0;
    }
    match name {
        "pg_authid" | "pg_shadow" | "pg_statistic" | "pg_replication_origin" => 0,
        // v1.96: the PG19 stats views are readable only by members
        // of pg_read_all_stats (their pg_class ACLs grant SELECT to
        // that predefined role alone).
        "pg_aios"
        | "pg_backend_memory_contexts"
        | "pg_shmem_allocations"
        | "pg_shmem_allocations_numa"
        | "pg_dsm_registry_allocations" => {
            let closure = crate::storage::role_closure(&q.eng.db, role, q.snap, q.own);
            if closure.iter().any(|n| n == "pg_read_all_stats") {
                PRIV_SELECT
            } else {
                0
            }
        }
        _ => PRIV_SELECT,
    }
}

/// v1.96: column names of a virtual catalog relation, for
/// has_column_privilege validation. Covers the catalogs whose
/// schemas this engine materializes; None for the rest.
pub(crate) fn catalog_column_names(q: &Q, name: &str) -> Option<Vec<String>> {
    let names = |schema: Vec<crate::exec::QCol>| -> Vec<String> {
        schema.into_iter().map(|c| c.name).collect()
    };
    match name {
        "pg_authid" | "pg_roles" | "pg_user" | "pg_auth_members" => {
            let (schema, _) = pg_auth_scan(&q.eng.db, q.snap, q.own, q.role, name);
            Some(names(schema))
        }
        "pg_class" => Some(names(pg_class_schema())),
        "pg_attribute" => Some(names(pg_attribute_schema())),
        _ => None,
    }
}

/// Effective table privilege bits for `role` on a resolved table ref.
pub(crate) fn table_privs_for(q: &Q, role: &str, tref: &TableRef) -> u32 {
    match tref {
        TableRef::Real(name) => {
            if let Some(t) = q.eng.db.find_table(name, q.snap, &q.all_xids, q.session) {
                let closure = crate::storage::role_closure(&q.eng.db, role, q.snap, q.own);
                crate::storage::table_privs_in(&q.eng.db, role, t, &closure, q.snap, q.own)
            } else {
                0
            }
        }
        TableRef::Catalog(name) => catalog_table_privs(name, role, q),
    }
}

fn eval_args(q: &mut Q, scopes: &[Scope], args: &[Expr]) -> Result<Vec<Value>, ExecError> {
    args.iter().map(|a| eval_expr(q, scopes, a)).collect()
}

fn str_of(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) | Value::BpChar(s) => Some(s.to_string()),
        Value::Int(i) | Value::BigInt(i) => Some(i.to_string()),
        Value::SmallInt(i) => Some(i.to_string()),
        _ => None,
    }
}

/// The v1.96 privilege/ACL function family. Returns Some(result) when
/// `name` is one of ours, None to fall through to the normal dispatch.
pub(crate) fn eval_priv_func(
    q: &mut Q,
    scopes: &[Scope],
    name: &str,
    args: &[Expr],
) -> Option<Result<Value, ExecError>> {
    match name {
        "has_table_privilege"
        | "has_column_privilege"
        | "has_sequence_privilege"
        | "has_database_privilege"
        | "has_schema_privilege"
        | "has_function_privilege"
        | "has_type_privilege"
        | "has_parameter_privilege"
        | "has_any_column_privilege"
        | "has_largeobject_privilege"
        | "pg_has_role"
        | "pg_get_userbyid"
        | "lo_create"
        | "lo_creat"
        | "lo_open"
        | "lo_close"
        | "loread"
        | "lowrite"
        | "lo_truncate"
        | "lo_truncate64"
        | "lo_tell"
        | "lo_tell64"
        | "lo_lseek"
        | "lo_lseek64"
        | "lo_unlink"
        | "lo_from_bytea"
        | "lo_put"
        | "lo_get"
        | "current_setting"
        | "makeaclitem"
        | "acldefault"
        | "aclcontains"
        | "pg_get_acl" => {}
        _ => return None,
    }
    Some(eval_priv_func_inner(q, scopes, name, args))
}

fn eval_priv_func_inner(
    q: &mut Q,
    scopes: &[Scope],
    name: &str,
    args: &[Expr],
) -> Result<Value, ExecError> {
    let vals = eval_args(q, scopes, args)?;
    if name.starts_with("lo_") || name == "loread" || name == "lowrite" {
        if let Some(r) = eval_lo_func(q, name, &vals) {
            return r;
        }
    }
    if name == "pg_get_userbyid" {
        let oid = match vals.first() {
            Some(Value::Int(i)) | Some(Value::BigInt(i)) => *i,
            Some(Value::SmallInt(i)) => *i as i64,
            _ => return Ok(Value::Null),
        };
        if oid == 10 {
            return Ok(Value::text("postgres"));
        }
        let mut names: Vec<&String> = q.eng.db.roles.keys().collect();
        names.sort();
        for rn in names {
            if role_oid(rn) as i64 == oid
                && q.eng
                    .db
                    .roles
                    .get(rn)
                    .map(|vs| {
                        vs.iter()
                            .any(|r| crate::storage::role_visible(r, q.snap, q.own))
                    })
                    .unwrap_or(false)
            {
                return Ok(Value::text(rn.as_str()));
            }
        }
        return Ok(Value::text(format!("unknown (OID={})", oid)));
    }
    match name {
        "current_setting" => {
            if vals.is_empty() || vals.len() > 2 {
                return Err(exec_err(
                    "42883",
                    "function current_setting() does not exist".to_string(),
                ));
            }
            if vals[0] == Value::Null {
                return Ok(Value::Null);
            }
            let missing_ok = matches!(vals.get(1), Some(Value::Bool(true)));
            let pname = str_of(&vals[0]).unwrap_or_default().to_ascii_lowercase();
            match current_setting_value(q, &pname) {
                Some(v) => Ok(Value::text(v)),
                None => {
                    if missing_ok {
                        Ok(Value::Null)
                    } else {
                        Err(exec_err(
                            "42704",
                            format!("unrecognized configuration parameter \"{}\"", pname),
                        ))
                    }
                }
            }
        }
        "has_table_privilege" => has_table_privilege(q, &vals),
        "has_column_privilege" => has_column_privilege(q, &vals),
        "has_sequence_privilege" => has_sequence_privilege(q, &vals),
        "has_database_privilege" => has_database_privilege(q, &vals),
        "has_schema_privilege" => has_schema_privilege(q, &vals),
        "has_function_privilege" => has_function_privilege(q, &vals),
        "has_type_privilege" => has_type_privilege(q, &vals),
        "has_parameter_privilege" => {
            // PG19 pg_parameter_acl: superusers hold SET+ALTER SYSTEM
            // on every parameter; predefined pg_read_all_settings etc.
            // are not parameter grants. Non-superusers: false.
            if vals.len() != 3 && vals.len() != 2 {
                return Err(exec_err(
                    "42883",
                    "function has_parameter_privilege() does not exist".to_string(),
                ));
            }
            let (role_v, priv_v) = if vals.len() == 3 {
                (&vals[0], &vals[2])
            } else {
                (&Value::Text(std::sync::Arc::from(q.role)), &vals[1])
            };
            if *role_v == Value::Null || *priv_v == Value::Null {
                return Ok(Value::Null);
            }
            let role = resolve_role_arg(q, role_v)?;
            let Some(role) = role else {
                return Ok(Value::Bool(false));
            };
            let priv_s = str_of(priv_v).unwrap_or_default();
            let _ = parse_named_priv(&priv_s, &[("SET", 1), ("ALTER SYSTEM", 2)])?;
            Ok(Value::Bool(crate::storage::is_superuser_snap(
                &q.eng.db, &role, q.snap, q.own,
            )))
        }
        "has_any_column_privilege" => {
            // (table, priv) or (role, table, priv): true iff the role
            // has the privilege on at least one column (or the table).
            let (role_v, tab_v, priv_v) = match vals.len() {
                2 => (None, &vals[0], &vals[1]),
                3 => (Some(&vals[0]), &vals[1], &vals[2]),
                _ => {
                    return Err(exec_err(
                        "42883",
                        "function has_any_column_privilege() does not exist".to_string(),
                    ));
                }
            };
            if tab_v == &Value::Null || priv_v == &Value::Null {
                return Ok(Value::Null);
            }
            let role = match role_v {
                Some(rv) => match resolve_role_arg(q, rv)? {
                    Some(r) => r,
                    None => return Ok(Value::Bool(false)),
                },
                None => q.role.to_string(),
            };
            let priv_s = str_of(priv_v).unwrap_or_default();
            let (bits, go) = parse_table_priv_spec(&priv_s)?;
            let tref = resolve_table_arg(q, tab_v)?;
            let Some(tref) = tref else {
                return Ok(Value::Null);
            };
            let have = table_privs_for(q, &role, &tref);
            if have & bits == bits {
                return Ok(Value::Bool(true));
            }
            if go {
                return Ok(Value::Bool(false));
            }
            // Any single column carrying all requested bits.
            if let TableRef::Real(tname) = &tref {
                if let Some(t) = q.eng.db.find_table(tname, q.snap, &q.all_xids, q.session) {
                    let closure = crate::storage::role_closure(&q.eng.db, &role, q.snap, q.own);
                    for (cn, _) in &t.columns {
                        let ch = crate::storage::column_privs_in(
                            &q.eng.db, &role, t, cn, &closure, q.snap, q.own,
                        );
                        if ch & bits == bits {
                            return Ok(Value::Bool(true));
                        }
                    }
                }
            }
            Ok(Value::Bool(false))
        }
        "has_largeobject_privilege" => {
            // PG19: [role,] oid, priv — unknown oid yields NULL.
            let (role, oid_v, priv_v) = if vals.len() == 3 {
                (
                    role_arg_name(q, &vals[0])?,
                    vals[1].clone(),
                    vals[2].clone(),
                )
            } else if vals.len() == 2 {
                (
                    q.priv_role_override
                        .clone()
                        .unwrap_or_else(|| q.role.to_string()),
                    vals[0].clone(),
                    vals[1].clone(),
                )
            } else {
                return Err(exec_err(
                    "42883",
                    "function has_largeobject_privilege() does not exist".to_string(),
                ));
            };
            let oid = match &oid_v {
                Value::Int(i) | Value::BigInt(i) => *i,
                Value::SmallInt(i) => *i as i64,
                _ => return Ok(Value::Null),
            };
            if !q.eng.db.large_objects.contains_key(&oid) {
                return Ok(Value::Null);
            }
            let priv_s = str_of(&priv_v).unwrap_or_default().to_ascii_lowercase();
            let need = match priv_s.as_str() {
                "select" => crate::storage::PRIV_SELECT,
                "update" => crate::storage::PRIV_UPDATE,
                _ => {
                    return Err(exec_err(
                        "22023",
                        format!("unrecognized privilege type: \"{}\"", priv_s),
                    ));
                }
            };
            let privs = lo_privs_for(q, &role, oid);
            // Owner's implicit grant option mirrors table semantics.
            Ok(Value::Bool(privs & need == need))
        }
        "pg_has_role" => pg_has_role(q, &vals),
        "makeaclitem" => {
            if vals.len() != 4 {
                return Err(exec_err(
                    "42883",
                    "function makeaclitem() does not exist".to_string(),
                ));
            }
            if vals.iter().any(|v| *v == Value::Null) {
                return Ok(Value::Null);
            }
            let grantee = role_arg_name(q, &vals[0])?;
            let grantor = role_arg_name(q, &vals[1])?;
            let priv_s = str_of(&vals[2]).unwrap_or_default();
            // v1.96: PG19 makeaclitem accepts a comma-separated
            // privilege list; the aclitem renders letters in
            // canonical ACL order (a r w d D x t C c T X U s A m),
            // each followed by '*' when grantable.
            let grantable = matches!(vals[3], Value::Bool(true));
            let mut ls: Vec<char> = Vec::new();
            for part in priv_s.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                let l = priv_name_letter(part).ok_or_else(|| {
                    exec_err(
                        "22023",
                        format!("unrecognized privilege type: \"{}\"", part.to_lowercase()),
                    )
                })?;
                if !ls.contains(&l) {
                    ls.push(l);
                }
            }
            const ORDER: &[char] = &[
                'a', 'r', 'w', 'd', 'D', 'x', 't', 'C', 'c', 'T', 'X', 'U', 's', 'A', 'm',
            ];
            ls.sort_by_key(|l| ORDER.iter().position(|o| o == l).unwrap_or(99));
            let mut letters = String::new();
            for l in ls {
                letters.push(l);
                if grantable {
                    letters.push('*');
                }
            }
            Ok(Value::text(aclitem_text(&grantee, &letters, &grantor)))
        }
        "acldefault" => {
            if vals.len() != 2 {
                return Err(exec_err(
                    "42883",
                    "function acldefault() does not exist".to_string(),
                ));
            }
            if vals.iter().any(|v| *v == Value::Null) {
                return Ok(Value::Null);
            }
            let objtype = str_of(&vals[0]).unwrap_or_default();
            let owner = role_arg_name(q, &vals[1])?;
            let items = acldefault_items(&objtype, &owner)?;
            Ok(Value::Array(Box::new(ArrayVal {
                elem: ArrayElem::Text,
                dims: vec![items.len() as i32],
                lower: vec![1],
                elems: items.into_iter().map(Value::text).collect(),
            })))
        }
        "aclcontains" => {
            if vals.len() != 2 {
                return Err(exec_err(
                    "42883",
                    "function aclcontains() does not exist".to_string(),
                ));
            }
            if vals.iter().any(|v| *v == Value::Null) {
                return Ok(Value::Null);
            }
            let needle = str_of(&vals[1]).unwrap_or_default();
            let needle = normalize_aclitem(&needle);
            let found = match &vals[0] {
                Value::Array(a) => a
                    .elems
                    .iter()
                    .any(|e| str_of(e).map(|s| normalize_aclitem(&s)) == Some(needle.clone())),
                Value::Text(s) | Value::BpChar(s) => normalize_aclitem(s) == needle,
                _ => false,
            };
            Ok(Value::Bool(found))
        }
        "pg_get_acl" => pg_get_acl(q, &vals),
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// Role argument as a NAME (for makeaclitem/acldefault): accepts the
/// regrole encoding (Int oid) or a name text.
fn role_arg_name(q: &Q, v: &Value) -> Result<String, ExecError> {
    match v {
        Value::Int(i) | Value::BigInt(i) => role_name_for_oid(q, *i as u32)
            .ok_or_else(|| exec_err("42704", format!("role with OID {} does not exist", i))),
        Value::SmallInt(i) => role_name_for_oid(q, *i as u32)
            .ok_or_else(|| exec_err("42704", format!("role with OID {} does not exist", i))),
        Value::Text(s) | Value::BpChar(s) => Ok(s.to_string()),
        other => Err(exec_err(
            "42846",
            format!("cannot cast type {} to regrole", other.type_name()),
        )),
    }
}

/// Single privilege name -> aclitem letter (acl.c privs in makeaclitem).
pub(crate) fn priv_name_letter(s: &str) -> Option<char> {
    match s.trim().to_ascii_uppercase().as_str() {
        "INSERT" => Some('a'),
        "SELECT" => Some('r'),
        "UPDATE" => Some('w'),
        "DELETE" => Some('d'),
        "TRUNCATE" => Some('D'),
        "REFERENCES" => Some('x'),
        "TRIGGER" => Some('t'),
        "EXECUTE" => Some('X'),
        "USAGE" => Some('U'),
        "CREATE" => Some('C'),
        "TEMPORARY" | "TEMP" => Some('T'),
        "CONNECT" => Some('c'),
        "MAINTAIN" => Some('m'),
        "SET" => Some('s'),
        "ALTER SYSTEM" => Some('A'),
        _ => None,
    }
}

/// acldefault(objtype, owner) item list (acl.c acldefault_sql).
pub(crate) fn acldefault_items(objtype: &str, owner: &str) -> Result<Vec<String>, ExecError> {
    let o = owner;
    let it = |grantee: &str, letters: &str| aclitem_text(grantee, letters, o);
    Ok(match objtype {
        "d" => vec![it(o, "CTc"), it("public", "Tc")],
        "f" => vec![it(o, "X"), it("public", "X")],
        "l" => vec![it(o, "U"), it("public", "U")],
        "n" => vec![it(o, "UC")],
        "r" => vec![it(o, "arwdDxtm")],
        "S" => vec![it(o, "rwU")],
        "T" => vec![it(o, "U")],
        "F" => vec![it(o, "U")],
        "v" => vec![it(o, "U")],
        "t" => vec![it(o, "C")],
        "L" => vec![it(o, "rw")],
        "p" => vec![it(o, "sA")],
        _ => {
            return Err(exec_err(
                "22023",
                format!("unrecognized object type: \"{}\"", objtype),
            ));
        }
    })
}

/// Canonicalize an aclitem text for comparison: parse
/// `grantee=privs/grantor` and re-render (letters in canonical order,
/// grantable stars preserved after their letter).
pub(crate) fn normalize_aclitem(s: &str) -> String {
    let Some((lhs, grantor)) = s.rsplit_once('/') else {
        return s.to_string();
    };
    let Some((grantee, privs)) = lhs.split_once('=') else {
        return s.to_string();
    };
    let grantee = grantee.trim_matches('"').replace("\"\"", "\"");
    let grantor = grantor.trim_matches('"').replace("\"\"", "\"");
    // Collect (letter, star) pairs and sort by canonical letter order.
    let order = "arwdDxtXUCTcmsA";
    let mut pairs: Vec<(char, bool)> = Vec::new();
    let mut chars = privs.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' {
            if let Some(last) = pairs.last_mut() {
                last.1 = true;
            }
            continue;
        }
        pairs.push((c, false));
    }
    pairs.sort_by_key(|(c, _)| order.find(*c).unwrap_or(99));
    let mut letters = String::new();
    for (c, star) in pairs {
        letters.push(c);
        if star {
            letters.push('*');
        }
    }
    aclitem_text(
        if grantee.is_empty() {
            "public"
        } else {
            &grantee
        },
        &letters,
        &grantor,
    )
}

/// current_setting lookup: the server-installed per-statement GUC map
/// first, then exec-visible specials.
pub(crate) fn current_setting_value(q: &Q, name: &str) -> Option<String> {
    if let Some(v) = crate::exec::installed_guc(name) {
        return Some(v);
    }
    match name {
        "role" => {
            let su = crate::exec::session_user_name(q.role);
            if su == q.role {
                Some("none".to_string())
            } else {
                Some(q.role.to_string())
            }
        }
        "session_authorization" => Some(crate::exec::session_user_name(q.role)),
        "server_version" => Some("19.0".to_string()),
        "search_path" => Some("\"$user\", public".to_string()),
        "client_encoding" => Some("UTF8".to_string()),
        "datestyle" => Some("ISO, MDY".to_string()),
        "timezone" => Some("UTC".to_string()),
        "row_security" => Some("on".to_string()),
        "default_transaction_isolation" => Some("read committed".to_string()),
        "transaction_isolation" => Some("read committed".to_string()),
        "max_parallel_workers_per_gather" => Some("2".to_string()),
        "standard_conforming_strings" => Some("on".to_string()),
        _ => None,
    }
}

/// v1.96: bits `role` holds WITH GRANT OPTION over an ACL list
/// (owner/superuser hold every bit with grant option).
pub(crate) fn grant_option_bits(
    db: &Database,
    role: &str,
    owner: &str,
    acl: &[crate::storage::AclEntry],
    snap: &Snapshot,
    own: u64,
) -> u32 {
    if role == owner || crate::storage::is_superuser_snap(db, role, snap, own) {
        return u32::MAX;
    }
    let closure = crate::storage::role_closure(db, role, snap, own);
    let mut go = 0u32;
    for e in acl {
        if closure.iter().any(|n| n == &e.role) {
            go |= e.grant_option;
        }
    }
    go
}

fn has_table_privilege(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    let (role_v, tab_v, priv_v) = match vals.len() {
        2 => (None, &vals[0], &vals[1]),
        3 => (Some(&vals[0]), &vals[1], &vals[2]),
        _ => {
            return Err(exec_err(
                "42883",
                "function has_table_privilege() does not exist".to_string(),
            ));
        }
    };
    if tab_v == &Value::Null || priv_v == &Value::Null {
        return Ok(Value::Null);
    }
    let role = match role_v {
        Some(rv) => {
            if rv == &Value::Null {
                return Ok(Value::Null);
            }
            match resolve_role_arg(q, rv)? {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            }
        }
        None => q.role.to_string(),
    };
    let priv_s = str_of(priv_v).unwrap_or_default();
    let (bits, go) = parse_table_priv_spec(&priv_s)?;
    let tref = resolve_table_arg(q, tab_v)?;
    let Some(tref) = tref else {
        return Ok(Value::Null);
    };
    let mut have = table_privs_for(q, &role, &tref);
    // v1.96 finishing pass: PG19 pg_class_aclmask_ext falls
    // through to the per-column ACLs for bits the table ACL does
    // not grant — a column-level grant makes the table-level
    // has_*_privilege true (ACLMASK_ANY per bit).
    if have & bits != bits {
        if let TableRef::Real(n) = &tref {
            if let Some(t) = q
                .eng
                .db
                .find_table(n, q.snap, &q.all_xids, q.session)
                .cloned()
            {
                let closure = crate::storage::role_closure(&q.eng.db, &role, q.snap, q.own);
                for (cname, _) in &t.columns {
                    have |= crate::storage::column_privs_in(
                        &q.eng.db, &role, &t, cname, &closure, q.snap, q.own,
                    );
                }
            }
        }
    }
    if go {
        let gobits = match &tref {
            TableRef::Real(n) => match q.eng.db.find_table(n, q.snap, &q.all_xids, q.session) {
                Some(t) => grant_option_bits(&q.eng.db, &role, &t.owner, &t.acl, q.snap, q.own),
                None => 0,
            },
            TableRef::Catalog(_) => {
                if role == "postgres" {
                    u32::MAX
                } else {
                    0
                }
            }
        };
        return Ok(Value::Bool(gobits & bits == bits));
    }
    Ok(Value::Bool(have & bits == bits))
}

fn has_column_privilege(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    let (role_v, tab_v, col_v, priv_v) = match vals.len() {
        3 => (None, &vals[0], &vals[1], &vals[2]),
        4 => (Some(&vals[0]), &vals[1], &vals[2], &vals[3]),
        _ => {
            return Err(exec_err(
                "42883",
                "function has_column_privilege() does not exist".to_string(),
            ));
        }
    };
    if tab_v == &Value::Null || col_v == &Value::Null || priv_v == &Value::Null {
        return Ok(Value::Null);
    }
    let role = match role_v {
        Some(rv) => {
            if rv == &Value::Null {
                return Ok(Value::Null);
            }
            match resolve_role_arg(q, rv)? {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            }
        }
        None => q.role.to_string(),
    };
    let priv_s = str_of(priv_v).unwrap_or_default();
    let (bits, go) = parse_table_priv_spec(&priv_s)?;
    // Table by OID -> NULL on miss; by name -> 42P01 on miss.
    let tref = resolve_table_arg(q, tab_v)?;
    let Some(tref) = tref else {
        return Ok(Value::Null);
    };
    // v1.96: catalog relations — validate the column against the
    // catalog's schema (42703 for an unknown name, NULL for an
    // out-of-range attnum) and answer from the catalog table privs
    // (catalogs carry no per-column ACLs in PG either).
    if let TableRef::Catalog(cname) = &tref {
        let cols = catalog_column_names(q, cname);
        let Some(cols) = cols else {
            return Ok(Value::Bool(false));
        };
        match col_v {
            Value::Text(s) | Value::BpChar(s) => {
                let c = s.to_string();
                if c.starts_with("........pg.dropped.") {
                    return Ok(Value::Null);
                }
                if !cols.iter().any(|n| n == &c) {
                    return Err(exec_err(
                        "42703",
                        format!("column \"{}\" of relation \"{}\" does not exist", c, cname),
                    ));
                }
            }
            Value::Int(i) | Value::BigInt(i) => {
                if *i < 1 || *i as usize > cols.len() {
                    return Ok(Value::Null);
                }
            }
            Value::SmallInt(i) => {
                if *i < 1 || *i as usize > cols.len() {
                    return Ok(Value::Null);
                }
            }
            _ => return Ok(Value::Null),
        }
        if go {
            return Ok(Value::Bool(crate::storage::is_superuser_snap(
                &q.eng.db, &role, q.snap, q.own,
            )));
        }
        let have = catalog_table_privs(cname, &role, q);
        return Ok(Value::Bool(have & bits == bits));
    }
    let TableRef::Real(tname) = tref else {
        return Ok(Value::Bool(false));
    };
    let t = q
        .eng
        .db
        .find_table(&tname, q.snap, &q.all_xids, q.session)
        .cloned();
    let Some(t) = t else {
        return Ok(Value::Bool(false));
    };
    // Column by name or attnum.
    let colname: String = match col_v {
        Value::Text(s) | Value::BpChar(s) => {
            let c = s.to_string();
            if c.starts_with("........pg.dropped.") {
                return Ok(Value::Null);
            }
            if !t.columns.iter().any(|(n, _)| n == &c)
                && !["tableoid", "ctid", "xmin", "xmax", "cmin", "cmax"].contains(&c.as_str())
            {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", c, tname),
                ));
            }
            c
        }
        Value::Int(i) | Value::BigInt(i) => {
            let attnum = *i as i16;
            match t.attnums.iter().position(|a| *a == attnum) {
                Some(idx) => t.columns[idx].0.clone(),
                None => return Ok(Value::Null),
            }
        }
        Value::SmallInt(i) => {
            let attnum = *i;
            match t.attnums.iter().position(|a| *a == attnum) {
                Some(idx) => t.columns[idx].0.clone(),
                None => return Ok(Value::Null),
            }
        }
        _ => return Ok(Value::Null),
    };
    if go {
        return Ok(Value::Bool(
            t.owner == role || crate::storage::is_superuser_snap(&q.eng.db, &role, q.snap, q.own),
        ));
    }
    let closure = crate::storage::role_closure(&q.eng.db, &role, q.snap, q.own);
    let have = if ["tableoid", "ctid", "xmin", "xmax", "cmin", "cmax"].contains(&colname.as_str()) {
        // System columns: table-level bits plus their own column grants.
        let mut h = crate::storage::table_privs_in(&q.eng.db, &role, &t, &closure, q.snap, q.own);
        for e in &t.col_acl {
            if (e.role == "public" || closure.iter().any(|n| n == &e.role))
                && e.columns.iter().any(|c| c == &colname)
            {
                h |= e.privs;
            }
        }
        h
    } else {
        crate::storage::column_privs_in(&q.eng.db, &role, &t, &colname, &closure, q.snap, q.own)
    };
    Ok(Value::Bool(have & bits == bits))
}

fn has_sequence_privilege(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    let (role_v, seq_v, priv_v) = match vals.len() {
        2 => (None, &vals[0], &vals[1]),
        3 => (Some(&vals[0]), &vals[1], &vals[2]),
        _ => {
            return Err(exec_err(
                "42883",
                "function has_sequence_privilege() does not exist".to_string(),
            ));
        }
    };
    if seq_v == &Value::Null || priv_v == &Value::Null {
        return Ok(Value::Null);
    }
    let role = match role_v {
        Some(rv) => {
            if rv == &Value::Null {
                return Ok(Value::Null);
            }
            match resolve_role_arg(q, rv)? {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            }
        }
        None => q.role.to_string(),
    };
    let priv_s = str_of(priv_v).unwrap_or_default();
    let (bits, go) = parse_named_priv(
        &priv_s,
        &[
            ("USAGE", PRIV_USAGE),
            ("SELECT", PRIV_SELECT),
            ("UPDATE", PRIV_UPDATE),
        ],
    )?;
    let s = match seq_v {
        Value::Text(s) | Value::BpChar(s) => {
            let raw = s.to_string();
            let name = raw.rsplit('.').next().unwrap_or(&raw).to_string();
            match q
                .eng
                .db
                .find_sequence(&raw, q.snap, q.own)
                .or_else(|| q.eng.db.find_sequence(&name, q.snap, q.own))
            {
                Some(s) => s.clone(),
                None => {
                    return Err(exec_err(
                        "42P01",
                        format!("relation \"{}\" does not exist", raw),
                    ));
                }
            }
        }
        Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_) => {
            // Sequences carry no OID in this catalog: OID form -> NULL.
            return Ok(Value::Null);
        }
        _ => return Ok(Value::Null),
    };
    if go {
        return Ok(Value::Bool(
            s.owner == role || crate::storage::is_superuser_snap(&q.eng.db, &role, q.snap, q.own),
        ));
    }
    // v1.96 finishing pass: sequence_privs owns the owner/superuser
    // rules (materialized owner item is authoritative).
    let have = crate::storage::sequence_privs(&q.eng.db, &role, &s, q.snap, q.own);
    Ok(Value::Bool(have & bits == bits))
}

fn has_database_privilege(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    let (role_v, db_v, priv_v) = match vals.len() {
        2 => (None, &vals[0], &vals[1]),
        3 => (Some(&vals[0]), &vals[1], &vals[2]),
        _ => {
            return Err(exec_err(
                "42883",
                "function has_database_privilege() does not exist".to_string(),
            ));
        }
    };
    if db_v == &Value::Null || priv_v == &Value::Null {
        return Ok(Value::Null);
    }
    let role = match role_v {
        Some(rv) => {
            if rv == &Value::Null {
                return Ok(Value::Null);
            }
            match resolve_role_arg(q, rv)? {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            }
        }
        None => q.role.to_string(),
    };
    let dbname = match db_v {
        Value::Text(s) | Value::BpChar(s) => s.to_string(),
        Value::Int(i) | Value::BigInt(i) => {
            if *i as u32 == name_oid("postgres") || *i == 1 {
                "postgres".to_string()
            } else {
                return Ok(Value::Null);
            }
        }
        _ => return Ok(Value::Null),
    };
    if dbname != "postgres" && dbname != "rustgres" {
        return Err(exec_err(
            "3D000",
            format!("database \"{}\" does not exist", dbname),
        ));
    }
    let priv_s = str_of(priv_v).unwrap_or_default();
    let (bits, _go) = parse_named_priv(
        &priv_s,
        &[("CREATE", 1), ("CONNECT", 2), ("TEMPORARY", 4), ("TEMP", 4)],
    )?;
    let super_or_owner =
        role == "postgres" || crate::storage::is_superuser_snap(&q.eng.db, &role, q.snap, q.own);
    let mut have = 0u32;
    if super_or_owner {
        have = 7;
    } else {
        // PG default database ACL: PUBLIC gets CONNECT + TEMPORARY.
        have |= 4;
        if crate::storage::db_connect_allowed(&q.eng.db, &role, q.snap, q.own) {
            have |= 2;
        }
    }
    Ok(Value::Bool(have & bits == bits))
}

fn has_schema_privilege(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    let (role_v, sch_v, priv_v) = match vals.len() {
        2 => (None, &vals[0], &vals[1]),
        3 => (Some(&vals[0]), &vals[1], &vals[2]),
        _ => {
            return Err(exec_err(
                "42883",
                "function has_schema_privilege() does not exist".to_string(),
            ));
        }
    };
    if sch_v == &Value::Null || priv_v == &Value::Null {
        return Ok(Value::Null);
    }
    let role = match role_v {
        Some(rv) => {
            if rv == &Value::Null {
                return Ok(Value::Null);
            }
            match resolve_role_arg(q, rv)? {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            }
        }
        None => q.role.to_string(),
    };
    let schema = match sch_v {
        Value::Text(s) | Value::BpChar(s) => s.to_string(),
        Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    // Known schemas: the built-ins plus every registered schema
    // (db.schemas — v1.96: CREATE SCHEMA records even empty ones)
    // plus any dotted-prefix of a live table/view/sequence name.
    let mut known = matches!(
        schema.as_str(),
        "public" | "pg_catalog" | "pg_toast" | "information_schema"
    ) || q.eng.db.schemas.iter().any(|s| s == &schema);
    if !known {
        let prefix = format!("{}.", schema);
        known = q.eng.db.tables.keys().any(|n| n.starts_with(&prefix))
            || q.eng.db.views.keys().any(|n| n.starts_with(&prefix))
            || q.eng.db.sequences.keys().any(|n| n.starts_with(&prefix));
    }
    if !known {
        return Err(exec_err(
            "3F000",
            format!("schema \"{}\" does not exist", schema),
        ));
    }
    // v1.96: real schema privilege model (PG19 pg_namespace_aclmask):
    // superuser and the owner hold CREATE|USAGE; ACL entries count
    // for the role, its inherit closure, and PUBLIC; members of
    // pg_read_all_data / pg_write_all_data hold USAGE on every
    // schema; the built-in schemas grant USAGE to everyone (public
    // also follows pg_database_owner for CREATE, PG15+).
    let priv_s = str_of(priv_v).unwrap_or_default();
    let (bits, _go) = parse_named_priv(
        &priv_s,
        &[("USAGE", PRIV_USAGE), ("CREATE", PRIV_REFERENCES)],
    )?;
    let db = &q.eng.db;
    let owner = db
        .schema_owners
        .get(&schema)
        .cloned()
        .unwrap_or_else(|| db.db_owner.clone());
    let closure = crate::storage::role_closure(db, &role, q.snap, q.own);
    let mut have = 0u32;
    if crate::storage::is_superuser_snap(db, &role, q.snap, q.own) || role == owner {
        have = PRIV_USAGE | PRIV_REFERENCES;
    }
    if let Some(entries) = db.schema_acls.get(&schema) {
        for e in entries {
            if e.role == "public" || closure.iter().any(|n| n == &e.role) {
                have |= e.privs;
            }
        }
    }
    if matches!(
        schema.as_str(),
        "public" | "pg_catalog" | "information_schema"
    ) {
        have |= PRIV_USAGE;
    }
    if closure
        .iter()
        .any(|n| n == "pg_read_all_data" || n == "pg_write_all_data")
    {
        have |= PRIV_USAGE;
    }
    Ok(Value::Bool(have & bits == bits))
}

/// Function ACL resolution shared by has_function_privilege and the
/// GRANT/REVOKE ON FUNCTION exec path. Default: PUBLIC EXECUTE.
pub(crate) fn function_privs_for(
    db: &Database,
    role: &str,
    fname: &str,
    snap: &Snapshot,
    own: u64,
) -> u32 {
    if crate::storage::is_superuser_snap(db, role, snap, own) {
        return PRIV_USAGE; // EXECUTE rides the USAGE bit in fn_acls.
    }
    let closure = crate::storage::role_closure(db, role, snap, own);
    match db.fn_acls.get(fname) {
        None => PRIV_USAGE, // default PUBLIC EXECUTE
        Some(entries) => {
            // v1.96: a materialized ACL is authoritative (the
            // default PUBLIC item is seeded into it at first
            // GRANT/REVOKE, PG acldefault semantics); no PUBLIC
            // entry means PUBLIC holds nothing.
            let mut privs = match entries.iter().find(|e| e.role == "public") {
                Some(e) => e.privs,
                None => 0,
            };
            for e in entries {
                if e.role != "public" && closure.iter().any(|n| n == &e.role) {
                    privs |= e.privs;
                }
            }
            privs
        }
    }
}

/// Type ACL resolution (USAGE bit), default PUBLIC USAGE.
pub(crate) fn type_privs_for(
    db: &Database,
    role: &str,
    tname: &str,
    snap: &Snapshot,
    own: u64,
) -> u32 {
    if crate::storage::is_superuser_snap(db, role, snap, own) {
        return PRIV_USAGE;
    }
    let closure = crate::storage::role_closure(db, role, snap, own);
    match db.type_acls.get(tname) {
        None => PRIV_USAGE,
        Some(entries) => {
            // v1.96: materialized ACL is authoritative (see
            // function_privs_for).
            let mut privs = match entries.iter().find(|e| e.role == "public") {
                Some(e) => e.privs,
                None => 0,
            };
            for e in entries {
                if e.role != "public" && closure.iter().any(|n| n == &e.role) {
                    privs |= e.privs;
                }
            }
            privs
        }
    }
}

fn has_function_privilege(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    let (role_v, fn_v, priv_v) = match vals.len() {
        2 => (None, &vals[0], &vals[1]),
        3 => (Some(&vals[0]), &vals[1], &vals[2]),
        _ => {
            return Err(exec_err(
                "42883",
                "function has_function_privilege() does not exist".to_string(),
            ));
        }
    };
    if fn_v == &Value::Null || priv_v == &Value::Null {
        return Ok(Value::Null);
    }
    let role = match role_v {
        Some(rv) => {
            if rv == &Value::Null {
                return Ok(Value::Null);
            }
            match resolve_role_arg(q, rv)? {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            }
        }
        None => q.role.to_string(),
    };
    let priv_s = str_of(priv_v).unwrap_or_default();
    let _ = parse_named_priv(&priv_s, &[("EXECUTE", PRIV_USAGE)])?;
    let fname = match fn_v {
        Value::Text(s) | Value::BpChar(s) => {
            let spec = s.to_string();
            let base = spec.split('(').next().unwrap_or(&spec).trim().to_string();
            let stripped = base.rsplit('.').next().unwrap_or(&base).to_string();
            // v1.96: schema-qualified specs resolve against the
            // qualified catalog key first (testns.foo).
            if q.eng.db.functions.contains_key(&base) || q.eng.db.aggregates.contains_key(&base) {
                base
            } else {
                stripped
            }
        }
        Value::Int(i) | Value::BigInt(i) => {
            // Function OIDs are name_oid hashes of the function name.
            let oid = *i as u32;
            match q.eng.db.functions.keys().find(|n| name_oid(n) == oid) {
                Some(n) => n.clone(),
                None => return Ok(Value::Null),
            }
        }
        _ => return Ok(Value::Null),
    };
    if !q.eng.db.functions.contains_key(&fname)
        && !q.eng.db.aggregates.contains_key(&fname)
        && !crate::sql::is_builtin_fn(&fname)
    {
        return Err(exec_err(
            "42883",
            format!("function \"{}\" does not exist", fname),
        ));
    }
    let have = function_privs_for(&q.eng.db, &role, &fname, q.snap, q.own);
    Ok(Value::Bool(have & PRIV_USAGE == PRIV_USAGE))
}

fn has_type_privilege(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    let (role_v, ty_v, priv_v) = match vals.len() {
        2 => (None, &vals[0], &vals[1]),
        3 => (Some(&vals[0]), &vals[1], &vals[2]),
        _ => {
            return Err(exec_err(
                "42883",
                "function has_type_privilege() does not exist".to_string(),
            ));
        }
    };
    if ty_v == &Value::Null || priv_v == &Value::Null {
        return Ok(Value::Null);
    }
    let role = match role_v {
        Some(rv) => {
            if rv == &Value::Null {
                return Ok(Value::Null);
            }
            match resolve_role_arg(q, rv)? {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            }
        }
        None => q.role.to_string(),
    };
    let priv_s = str_of(priv_v).unwrap_or_default();
    let _ = parse_named_priv(&priv_s, &[("USAGE", PRIV_USAGE)])?;
    let (tname, raw_name) = match ty_v {
        Value::Text(s) | Value::BpChar(s) => {
            let raw = s.to_string();
            (raw.rsplit('.').next().unwrap_or(&raw).to_string(), raw)
        }
        Value::Int(_) | Value::BigInt(_) | Value::SmallInt(_) => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    let builtin = matches!(
        tname.as_str(),
        "int"
            | "integer"
            | "int2"
            | "int4"
            | "int8"
            | "bigint"
            | "smallint"
            | "text"
            | "varchar"
            | "bool"
            | "boolean"
            | "float4"
            | "float8"
            | "real"
            | "numeric"
            | "date"
            | "timestamp"
            | "timestamptz"
            | "uuid"
            | "json"
            | "jsonb"
            | "bytea"
    );
    // v1.96: types are keyed bare when created unqualified and
    // schema-qualified when created qualified — accept either.
    let type_exists = q.eng.db.types.contains_key(&tname) || q.eng.db.types.contains_key(&raw_name);
    if !builtin && !type_exists {
        return Err(exec_err(
            "42704",
            format!("type \"{}\" does not exist", tname),
        ));
    }
    let key = if q.eng.db.type_acls.contains_key(&raw_name) {
        &raw_name
    } else {
        &tname
    };
    let have = type_privs_for(&q.eng.db, &role, key, q.snap, q.own);
    Ok(Value::Bool(have & PRIV_USAGE == PRIV_USAGE))
}

fn pg_has_role(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    if vals.len() != 3 {
        return Err(exec_err(
            "42883",
            "function pg_has_role() does not exist".to_string(),
        ));
    }
    if vals.iter().any(|v| *v == Value::Null) {
        return Ok(Value::Null);
    }
    let user = match resolve_role_arg(q, &vals[0])? {
        Some(r) => r,
        None => return Ok(Value::Bool(false)),
    };
    let role = match resolve_role_arg(q, &vals[1])? {
        Some(r) => r,
        None => return Ok(Value::Bool(false)),
    };
    let priv_s = str_of(&vals[2]).unwrap_or_default().to_ascii_uppercase();
    let (mode, with_admin) = match priv_s.trim() {
        "MEMBER" => ("member", false),
        "MEMBER WITH ADMIN OPTION" => ("member", true),
        "USAGE" => ("usage", false),
        "USAGE WITH GRANT OPTION" => ("usage", true),
        "SET" => ("set", false),
        "SET WITH GRANT OPTION" => ("set", true),
        _ => {
            return Err(exec_err(
                "22023",
                format!("unrecognized privilege type: \"{}\"", priv_s.to_lowercase()),
            ));
        }
    };
    // pg_database_owner: its only implicit member is the database's
    // owning role (db.db_owner, ALTER DATABASE ... OWNER TO) — so
    // every question about it is the same question about that role.
    if role == "pg_database_owner" {
        let role = q.eng.db.db_owner.clone();
        let db = &q.eng.db;
        if user == role {
            return Ok(Value::Bool(true));
        }
        let result = match mode {
            "member" => crate::storage::membership_closure(db, &user, q.snap, q.own)
                .iter()
                .any(|n| n == &role),
            _ => {
                crate::storage::is_superuser_snap(db, &user, q.snap, q.own)
                    || crate::storage::role_closure(db, &user, q.snap, q.own)
                        .iter()
                        .any(|n| n == &role)
            }
        };
        let _ = with_admin;
        return Ok(Value::Bool(result && !with_admin));
    }
    let db = &q.eng.db;
    let superuser = crate::storage::is_superuser_snap(db, &user, q.snap, q.own);
    if user == role {
        return Ok(Value::Bool(true));
    }
    let result = match mode {
        "member" => {
            let closure = crate::storage::membership_closure(db, &user, q.snap, q.own);
            if !closure.iter().any(|n| n == &role) {
                false
            } else if with_admin {
                // Direct edge with admin, or superuser.
                superuser
                    || db
                        .find_role(&user, q.snap, q.own)
                        .map(|r| r.memberships.iter().any(|m| m.role == role && m.admin))
                        .unwrap_or(false)
            } else {
                true
            }
        }
        "usage" => {
            // USAGE = membership through inherit-enabled edges.
            let closure = crate::storage::role_closure(db, &user, q.snap, q.own);
            closure.iter().any(|n| n == &role)
        }
        "set" => {
            // SET: superuser, or a direct membership with set_option
            // (PG19: also the role itself / session user identity).
            if superuser {
                true
            } else {
                db.find_role(&user, q.snap, q.own)
                    .map(|r| r.memberships.iter().any(|m| m.role == role && m.set))
                    .unwrap_or(false)
            }
        }
        _ => false,
    };
    Ok(Value::Bool(result))
}

fn pg_get_acl(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    if vals.len() != 3 {
        return Err(exec_err(
            "42883",
            "function pg_get_acl() does not exist".to_string(),
        ));
    }
    if vals.iter().any(|v| *v == Value::Null) {
        return Ok(Value::Null);
    }
    let classid = str_of(&vals[0]).unwrap_or_default();
    let is_class =
        classid == "pg_class" || matches!(&vals[0], Value::Int(1259) | Value::BigInt(1259));
    let is_attr =
        classid == "pg_attribute" || matches!(&vals[0], Value::Int(1249) | Value::BigInt(1249));
    if !is_class && !is_attr {
        return Ok(Value::Null);
    }
    let objoid = match &vals[1] {
        Value::Int(i) | Value::BigInt(i) => *i as u32,
        Value::Text(s) | Value::BpChar(s) => {
            // A regclass display text: resolve by name.
            match q.eng.db.find_table(s, q.snap, &q.all_xids, q.session) {
                Some(t) => t.oid,
                None => return Ok(Value::Null),
            }
        }
        _ => return Ok(Value::Null),
    };
    if objoid == 0 {
        return Ok(Value::Null);
    }
    let attnum = match &vals[2] {
        Value::Int(i) | Value::BigInt(i) => *i as i64,
        Value::SmallInt(i) => *i as i64,
        _ => return Ok(Value::Null),
    };
    let t = q
        .eng
        .db
        .tables
        .values()
        .flat_map(|vs| vs.iter())
        .find(|t| t.oid == objoid && crate::storage::table_visible(t, q.snap, &q.all_xids))
        .cloned();
    let Some(t) = t else { return Ok(Value::Null) };
    let owner = t.owner.clone();
    let mut items: Vec<String> = Vec::new();
    if attnum == 0 {
        // PG relacl: NULL until the first table-level grant; the
        // owner's implicit all-privileges item leads once it exists.
        if t.acl.is_empty() {
            return Ok(Value::Null);
        }
        // v1.96: a materialized owner item (seeded at first
        // GRANT/REVOKE) supplies the owner's letters — a REVOKE FROM
        // the owner may have subtracted from them.
        let owner_item = t
            .acl
            .iter()
            .find(|e| e.role == owner && (e.grantor.is_empty() || e.grantor == owner));
        match owner_item {
            Some(e) => items.push(aclitem_text_for(e, &owner)),
            None => items.push(aclitem_text(
                &owner,
                &table_priv_letters(PRIV_ALL_TABLE | PRIV_MAINTAIN),
                &owner,
            )),
        }
        for e in &t.acl {
            if e.role == owner && (e.grantor.is_empty() || e.grantor == owner) {
                continue;
            }
            items.push(aclitem_text_for(e, &owner));
        }
    } else {
        // PG attacl: ONLY this column's own grants (no owner item,
        // no table-level merge). No grants -> empty array.
        let colname = match t.attnums.iter().position(|a| *a as i64 == attnum) {
            Some(idx) => t.columns[idx].0.clone(),
            None => return Ok(Value::Null),
        };
        for e in &t.col_acl {
            if e.columns.iter().any(|c| c == &colname) {
                let as_acl = crate::storage::AclEntry {
                    role: e.role.clone(),
                    privs: e.privs,
                    grant_option: e.grant_option,
                    grantor: e.grantor.clone(),
                };
                items.push(aclitem_text_for(&as_acl, &owner));
            }
        }
    }
    Ok(Value::Array(Box::new(ArrayVal {
        elem: ArrayElem::Text,
        dims: vec![items.len() as i32],
        lower: vec![1],
        elems: items.into_iter().map(Value::text).collect(),
    })))
}

// ---------------------------------------------------------------------
// v1.96: large objects. Data lives in Database::large_objects with
// owners in lo_owners and ACLs in lo_acls; open descriptors are
// session state (thread-local, one thread per connection).
// ---------------------------------------------------------------------

pub(crate) struct LoFd {
    pub(crate) oid: i64,
    pub(crate) pos: u64,
    pub(crate) mode: i32,
}

thread_local! {
    pub(crate) static LO_FDS: std::cell::RefCell<std::collections::HashMap<i32, LoFd>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    pub(crate) static LO_NEXT_FD: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// v1.96: the session's lo_compat_privileges GUC (installed per
/// statement by the server). When on, PG skips LO permission checks
/// entirely (pre-9.0 behavior).
pub(crate) fn lo_compat_on() -> bool {
    matches!(
        crate::exec::installed_guc("lo_compat_privileges").as_deref(),
        Some("on") | Some("true") | Some("1")
    )
}

/// Effective LO privilege bits for `role` (SELECT/UPDATE positions).
pub(crate) fn lo_privs_for(q: &Q, role: &str, oid: i64) -> u32 {
    if lo_compat_on() {
        return crate::storage::PRIV_SELECT | crate::storage::PRIV_UPDATE;
    }
    let db = &q.eng.db;
    let owner = db
        .lo_owners
        .get(&oid)
        .cloned()
        .unwrap_or_else(|| "postgres".to_string());
    if owner == role || crate::storage::is_superuser_snap(db, role, q.snap, q.own) {
        return crate::storage::PRIV_SELECT | crate::storage::PRIV_UPDATE;
    }
    let closure = crate::storage::role_closure(db, role, q.snap, q.own);
    let mut bits = 0u32;
    if closure.iter().any(|n| n == "pg_read_all_data") {
        bits |= crate::storage::PRIV_SELECT;
    }
    if closure.iter().any(|n| n == "pg_write_all_data") {
        bits |= crate::storage::PRIV_UPDATE;
    }
    if let Some(entries) = db.lo_acls.get(&oid) {
        for e in entries {
            if e.role == "public" || closure.iter().any(|n| n == &e.role) {
                bits |= e.privs;
            }
        }
    }
    bits
}

pub(crate) fn lo_stage_write(q: &mut Q) {
    if let Some(w) = &mut q.write {
        w.writes.push(WriteOp::LargeObjects {
            prev: q.eng.db.large_objects.clone(),
            prev_owners: q.eng.db.lo_owners.clone(),
            prev_acls: q.eng.db.lo_acls.clone(),
        });
    }
}

pub(crate) fn lo_err_no_such(oid: i64) -> ExecError {
    exec_err("42704", format!("large object {} does not exist", oid))
}

pub(crate) fn lo_err_denied(oid: i64) -> ExecError {
    exec_err(
        "42501",
        format!("permission denied for large object {}", oid),
    )
}

/// v1.96 finishing pass: PG19 forbids LO writes in a read-only
/// transaction (25006), checked before any object lookup.
pub(crate) fn lo_ro_guard(q: &Q, what: &str) -> Option<ExecError> {
    if q.read_only {
        Some(exec_err(
            "25006",
            format!("cannot execute {} in a read-only transaction", what),
        ))
    } else {
        None
    }
}

/// INV_READ / INV_WRITE mode bits (largeobject.h).
pub(crate) const LO_INV_WRITE: i32 = 0x20000;
pub(crate) const LO_INV_READ: i32 = 0x40000;

pub(crate) fn eval_lo_func(
    q: &mut Q,
    name: &str,
    vals: &[Value],
) -> Option<Result<Value, ExecError>> {
    let role: String = q
        .priv_role_override
        .clone()
        .unwrap_or_else(|| q.role.to_string());
    let role: &str = &role;
    let int_of = |v: &Value| -> Option<i64> {
        match v {
            Value::Int(i) | Value::BigInt(i) => Some(*i),
            Value::SmallInt(i) => Some(*i as i64),
            _ => None,
        }
    };
    let bytes_of = |v: &Value| -> Option<Vec<u8>> {
        match v {
            Value::Bytea(b) => Some(b.clone()),
            Value::Text(s) | Value::BpChar(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    };
    match name {
        "lo_create" | "lo_creat" => {
            if let Some(e) = lo_ro_guard(q, &format!("{}()", name)) {
                return Some(Err(e));
            }
            let want = if name == "lo_create" {
                vals.first().and_then(int_of).unwrap_or(0)
            } else {
                0
            };
            let oid = if want == 0 {
                let mut cand = 16384i64;
                while q.eng.db.large_objects.contains_key(&cand) {
                    cand += 1;
                }
                cand
            } else {
                want
            };
            if q.eng.db.large_objects.contains_key(&oid) {
                return Some(Err(exec_err(
                    "42710",
                    format!("large object {} already exists", oid),
                )));
            }
            lo_stage_write(q);
            q.eng.db.large_objects.insert(oid, Vec::new());
            q.eng.db.lo_owners.insert(oid, role.to_string());
            // v1.96: ALTER DEFAULT PRIVILEGES ... ON LARGE OBJECTS
            // rules for the creator seed the new LO's ACL.
            let seeded: Vec<crate::storage::AclEntry> = q
                .eng
                .db
                .default_acls
                .iter()
                .filter(|d| d.kind == 'l' && d.creator == role && d.privs != 0)
                .map(|d| {
                    let mut e = crate::storage::AclEntry::new(
                        &d.grantee,
                        d.privs & (crate::storage::PRIV_SELECT | crate::storage::PRIV_UPDATE),
                    );
                    e.grantor = role.to_string();
                    e
                })
                .filter(|e| e.privs != 0)
                .collect();
            if !seeded.is_empty() {
                q.eng.db.lo_acls.insert(oid, seeded);
            }
            Some(Ok(Value::Int(oid)))
        }
        "lo_open" => {
            let oid = vals.first().and_then(int_of)?;
            let mode = vals.get(1).and_then(int_of).unwrap_or(0) as i32;
            if !q.eng.db.large_objects.contains_key(&oid) {
                return Some(Err(lo_err_no_such(oid)));
            }
            if mode & LO_INV_WRITE != 0 {
                if let Some(e) = lo_ro_guard(q, "lo_open(INV_WRITE)") {
                    return Some(Err(e));
                }
            }
            let privs = lo_privs_for(q, role, oid);
            if mode & LO_INV_READ != 0
                && privs & crate::storage::PRIV_SELECT != crate::storage::PRIV_SELECT
            {
                return Some(Err(lo_err_denied(oid)));
            }
            if mode & LO_INV_WRITE != 0
                && privs & crate::storage::PRIV_UPDATE != crate::storage::PRIV_UPDATE
            {
                return Some(Err(lo_err_denied(oid)));
            }
            let fd = LO_NEXT_FD.with(|c| {
                let f = c.get();
                c.set(f + 1);
                f
            });
            LO_FDS.with(|m| {
                m.borrow_mut().insert(fd, LoFd { oid, pos: 0, mode });
            });
            Some(Ok(Value::Int(fd as i64)))
        }
        "lo_close" => {
            let fd = vals.first().and_then(int_of)? as i32;
            LO_FDS.with(|m| {
                m.borrow_mut().remove(&fd);
            });
            Some(Ok(Value::Int(0)))
        }
        "loread" | "lo_read" => {
            let fd = vals.first().and_then(int_of)? as i32;
            let len = vals.get(1).and_then(int_of).unwrap_or(0).max(0) as usize;
            let (oid, pos) = LO_FDS.with(|m| m.borrow().get(&fd).map(|f| (f.oid, f.pos)))?;
            let data = q.eng.db.large_objects.get(&oid)?;
            let start = (pos as usize).min(data.len());
            let end = (start + len).min(data.len());
            let out = data[start..end].to_vec();
            LO_FDS.with(|m| {
                if let Some(f) = m.borrow_mut().get_mut(&fd) {
                    f.pos = end as u64;
                }
            });
            Some(Ok(Value::Bytea(out)))
        }
        "lowrite" | "lo_write" => {
            if let Some(e) = lo_ro_guard(q, "lowrite()") {
                return Some(Err(e));
            }
            let fd = vals.first().and_then(int_of)? as i32;
            let bytes = bytes_of(vals.get(1)?)?;
            let (oid, pos, mode) =
                LO_FDS.with(|m| m.borrow().get(&fd).map(|f| (f.oid, f.pos, f.mode)))?;
            if mode & LO_INV_WRITE == 0 {
                return Some(Err(exec_err(
                    "55000",
                    format!("large object descriptor {} was not opened for writing", fd),
                )));
            }
            lo_stage_write(q);
            let data = q.eng.db.large_objects.get_mut(&oid)?;
            let start = pos as usize;
            if data.len() < start {
                data.resize(start, 0);
            }
            let end = start + bytes.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[start..end].copy_from_slice(&bytes);
            LO_FDS.with(|m| {
                if let Some(f) = m.borrow_mut().get_mut(&fd) {
                    f.pos = end as u64;
                }
            });
            Some(Ok(Value::Int(bytes.len() as i64)))
        }
        "lo_truncate" | "lo_truncate64" => {
            if let Some(e) = lo_ro_guard(q, &format!("{}()", name)) {
                return Some(Err(e));
            }
            let fd = vals.first().and_then(int_of)? as i32;
            let len = vals.get(1).and_then(int_of).unwrap_or(0).max(0) as usize;
            let (oid, mode) = LO_FDS.with(|m| m.borrow().get(&fd).map(|f| (f.oid, f.mode)))?;
            if mode & LO_INV_WRITE == 0 {
                return Some(Err(exec_err(
                    "55000",
                    format!("large object descriptor {} was not opened for writing", fd),
                )));
            }
            lo_stage_write(q);
            let data = q.eng.db.large_objects.get_mut(&oid)?;
            data.resize(len, 0);
            Some(Ok(Value::Int(0)))
        }
        "lo_tell" | "lo_tell64" => {
            let fd = vals.first().and_then(int_of)? as i32;
            let pos = LO_FDS.with(|m| m.borrow().get(&fd).map(|f| f.pos))?;
            Some(Ok(Value::BigInt(pos as i64)))
        }
        "lo_lseek" | "lo_lseek64" => {
            let fd = vals.first().and_then(int_of)? as i32;
            let off = vals.get(1).and_then(int_of).unwrap_or(0);
            let whence = vals.get(2).and_then(int_of).unwrap_or(0);
            let data_len = LO_FDS
                .with(|m| m.borrow().get(&fd).map(|f| f.oid))
                .and_then(|o| q.eng.db.large_objects.get(&o))
                .map(|d| d.len() as i64)
                .unwrap_or(0);
            let out = LO_FDS.with(|m| {
                let mut map = m.borrow_mut();
                let f = map.get_mut(&fd)?;
                let base = match whence {
                    1 => f.pos as i64,
                    2 => data_len,
                    _ => 0,
                };
                let np = (base + off).max(0) as u64;
                f.pos = np;
                Some(np as i64)
            })?;
            Some(Ok(Value::BigInt(out)))
        }
        "lo_unlink" => {
            if let Some(e) = lo_ro_guard(q, "lo_unlink()") {
                return Some(Err(e));
            }
            let oid = vals.first().and_then(int_of)?;
            if !q.eng.db.large_objects.contains_key(&oid) {
                return Some(Err(lo_err_no_such(oid)));
            }
            let owner = q
                .eng
                .db
                .lo_owners
                .get(&oid)
                .cloned()
                .unwrap_or_else(|| "postgres".to_string());
            if owner != role
                && !crate::storage::is_superuser_snap(&q.eng.db, role, q.snap, q.own)
                && !lo_compat_on()
            {
                return Some(Err(exec_err(
                    "42501",
                    format!("must be owner of large object {}", oid),
                )));
            }
            lo_stage_write(q);
            q.eng.db.large_objects.remove(&oid);
            q.eng.db.lo_owners.remove(&oid);
            q.eng.db.lo_acls.remove(&oid);
            Some(Ok(Value::Int(1)))
        }
        "lo_from_bytea" => {
            if let Some(e) = lo_ro_guard(q, "lo_from_bytea()") {
                return Some(Err(e));
            }
            let want = vals.first().and_then(int_of).unwrap_or(0);
            let bytes = bytes_of(vals.get(1)?)?;
            let oid = if want == 0 {
                let mut cand = 16384i64;
                while q.eng.db.large_objects.contains_key(&cand) {
                    cand += 1;
                }
                cand
            } else {
                want
            };
            if q.eng.db.large_objects.contains_key(&oid) {
                let privs = lo_privs_for(q, role, oid);
                if privs & crate::storage::PRIV_UPDATE != crate::storage::PRIV_UPDATE {
                    return Some(Err(lo_err_denied(oid)));
                }
            } else {
                lo_stage_write(q);
                q.eng.db.lo_owners.insert(oid, role.to_string());
            }
            lo_stage_write(q);
            q.eng.db.large_objects.insert(oid, bytes);
            Some(Ok(Value::Int(oid)))
        }
        "lo_put" => {
            if let Some(e) = lo_ro_guard(q, "lo_put()") {
                return Some(Err(e));
            }
            let oid = vals.first().and_then(int_of)?;
            let off = vals.get(1).and_then(int_of).unwrap_or(0).max(0) as usize;
            let bytes = bytes_of(vals.get(2)?)?;
            if !q.eng.db.large_objects.contains_key(&oid) {
                return Some(Err(lo_err_no_such(oid)));
            }
            let privs = lo_privs_for(q, role, oid);
            if privs & crate::storage::PRIV_UPDATE != crate::storage::PRIV_UPDATE {
                return Some(Err(lo_err_denied(oid)));
            }
            lo_stage_write(q);
            let data = q.eng.db.large_objects.get_mut(&oid)?;
            if data.len() < off {
                data.resize(off, 0);
            }
            let end = off + bytes.len();
            if data.len() < end {
                data.resize(end, 0);
            }
            data[off..end].copy_from_slice(&bytes);
            Some(Ok(Value::Null))
        }
        "lo_get" => {
            let oid = vals.first().and_then(int_of)?;
            if !q.eng.db.large_objects.contains_key(&oid) {
                return Some(Err(lo_err_no_such(oid)));
            }
            let privs = lo_privs_for(q, role, oid);
            if privs & crate::storage::PRIV_SELECT != crate::storage::PRIV_SELECT {
                return Some(Err(lo_err_denied(oid)));
            }
            let data = q.eng.db.large_objects.get(&oid)?.clone();
            let off = vals.get(1).and_then(int_of).unwrap_or(0).max(0) as usize;
            let out = if vals.len() > 2 {
                let len = vals.get(2).and_then(int_of).unwrap_or(0).max(0) as usize;
                let s = off.min(data.len());
                data[s..(s + len).min(data.len())].to_vec()
            } else if off > 0 {
                data[off.min(data.len())..].to_vec()
            } else {
                data
            };
            Some(Ok(Value::Bytea(out)))
        }
        _ => None,
    }
}

/// v1.96: render a stored AclEntry as aclitem text for a given object
/// family. Letters: SELECT=r, UPDATE=w (table family); for large
/// objects PG uses the table letters too (r/w). Grant-option bits
/// star their letter. Empty grantor = the object owner.
pub(crate) fn aclitem_text_for(e: &crate::storage::AclEntry, owner: &str) -> String {
    let grantor = if e.grantor.is_empty() {
        owner
    } else {
        e.grantor.as_str()
    };
    let pairs: [(u32, char); 9] = [
        (crate::storage::PRIV_INSERT, 'a'),
        (crate::storage::PRIV_SELECT, 'r'),
        (crate::storage::PRIV_UPDATE, 'w'),
        (crate::storage::PRIV_DELETE, 'd'),
        (crate::storage::PRIV_TRUNCATE, 'D'),
        (crate::storage::PRIV_REFERENCES, 'x'),
        (crate::storage::PRIV_TRIGGER, 't'),
        (crate::storage::PRIV_USAGE, 'X'),
        (512, 'm'),
    ];
    let mut letters = String::new();
    for (bit, ch) in pairs {
        if e.privs & bit == bit {
            letters.push(ch);
            if e.grant_option & bit == bit {
                letters.push('*');
            }
        }
    }
    aclitem_text(&e.role, &letters, grantor)
}
