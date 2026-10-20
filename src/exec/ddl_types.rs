// v1.78 mechanical split: moved verbatim from src/exec.rs (60886-61480).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

/// v0.22: true for builtin type names and registered shell types.
pub(crate) fn type_name_known(eng: &Engine, name: &str) -> bool {
    if crate::sql::coltype_by_name(name).is_ok() {
        return true;
    }
    eng.db.types.contains_key(name)
}

/// v0.22: bounded CREATE TYPE. The bare form registers a shell type;
/// the parenthesized form completes it as an alias of LIKE = <base>,
/// which must name a builtin type or an existing shell type. Other
/// attributes are accepted by the parser and ignored here (no I/O
/// functions exist in v0.22).
/// v1.96: record a type/domain owner (GRANT ... ON TYPE owner
/// checks and the type-USAGE owner exemption read type_owners).
pub(crate) fn record_type_owner(eng: &mut Engine, ctx: &mut StmtCtx, name: &str) {
    if eng.db.type_owners.get(name).map(|o| o.as_str()) == Some(ctx.role) {
        return;
    }
    let prev_fn = eng.db.fn_acls.clone();
    let prev_type = eng.db.type_acls.clone();
    let prev_lang = eng.db.lang_acls.clone();
    let prev_fn_owners = eng.db.fn_owners.clone();
    let prev_type_owners = eng.db.type_owners.clone();
    let is_new = !prev_type_owners.contains_key(name);
    eng.db
        .type_owners
        .insert(name.to_string(), ctx.role.to_string());
    // v1.96: a brand-new type/domain takes its initial ACL from
    // ALTER DEFAULT PRIVILEGES rules for the creator when any exist
    // (kind 'T'; a REVOKE USAGE ... FROM public rule erases the
    // default PUBLIC USAGE).
    if is_new {
        if let Some(init) = crate::exec::privilege::adp_initial_acl(
            &eng.db,
            ctx.role,
            'T',
            name,
            crate::storage::PRIV_USAGE,
            crate::storage::PRIV_USAGE,
        ) {
            eng.db.type_acls.insert(name.to_string(), init);
        }
    }
    ctx.writes.push(WriteOp::FnTypeAcls {
        prev_fn,
        prev_type,
        prev_lang,
        prev_fn_owners,
        prev_type_owners,
    });
}

pub(crate) fn exec_create_type(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    like_base: Option<&str>,
    composite: Option<&[(String, ColType, Option<String>)]>,
) -> Result<ExecResult, ExecError> {
    let prev = eng.db.types.get(name).cloned();
    // v0.81: `CREATE TYPE name AS (field type, ...)` — named composite.
    // Redefining any existing type is 42710 (like the shell form).
    // Field types that are themselves composites must name defined
    // composite types (42704 otherwise, like PG's "type does not exist").
    if let Some(fields) = composite {
        if prev.is_some() {
            return Err(exec_err(
                "42710",
                format!("type \"{}\" already exists", name),
            ));
        }
        for (_, fty, nested) in fields {
            if *fty == ColType::Composite {
                let nested_name = nested.as_deref().unwrap_or(name);
                match eng.db.types.get(nested_name) {
                    // v1.96: a field may be of composite OR domain
                    // type (both ride the Composite marker).
                    Some(st) if st.composite.is_some() || st.domain.is_some() => {}
                    _ => {
                        return Err(exec_err(
                            "42704",
                            format!("type \"{}\" does not exist", nested_name),
                        ));
                    }
                }
                // v1.96: USAGE on the field's type (PG19).
                check_type_usage(eng, ctx, nested_name)?;
            }
        }
        eng.db.types.insert(
            name.to_string(),
            ShellType {
                like_base: None,
                composite: Some(fields.to_vec()),
                domain: None,
                range: None,
            },
        );
        ctx.writes.push(WriteOp::CreateType {
            name: name.to_string(),
            prev,
        });
        record_type_owner(eng, ctx, name);
        return Ok(ExecResult::Command {
            tag: "CREATE TYPE".to_string(),
        });
    }
    match like_base {
        None => {
            // Bare `CREATE TYPE name;`: a shell. Redefining is 42710.
            if prev.is_some() {
                return Err(exec_err(
                    "42710",
                    format!("type \"{}\" already exists", name),
                ));
            }
            eng.db.types.insert(
                name.to_string(),
                ShellType {
                    like_base: None,
                    composite: None,
                    domain: None,
                    range: None,
                },
            );
        }
        Some(base) => {
            // Completion form: LIKE = <base> must name a known type.
            if !type_name_known(eng, base) {
                return Err(exec_err(
                    "42704",
                    format!("type \"{}\" does not exist", base),
                ));
            }
            if matches!(
                prev,
                Some(ShellType {
                    like_base: Some(_),
                    ..
                })
            ) {
                return Err(exec_err(
                    "42710",
                    format!("type \"{}\" already exists", name),
                ));
            }
            eng.db.types.insert(
                name.to_string(),
                ShellType {
                    like_base: Some(base.to_string()),
                    composite: None,
                    domain: None,
                    range: None,
                },
            );
        }
    }
    ctx.writes.push(WriteOp::CreateType {
        name: name.to_string(),
        prev,
    });
    record_type_owner(eng, ctx, name);
    Ok(ExecResult::Command {
        tag: "CREATE TYPE".to_string(),
    })
}

/// v1.88: PG's multirange companion name for a range type
/// (GetMultirangeTypeName): a trailing "range" is replaced by
/// "multirange"; otherwise "multirange" is appended.
pub(crate) fn multirange_name_for(range_name: &str) -> String {
    match range_name.strip_suffix("range") {
        Some(stem) => format!("{stem}multirange"),
        None => format!("{range_name}multirange"),
    }
}

/// v1.88: `CREATE TYPE name AS RANGE (subtype = T, ...)`. Registers the
/// range type and its multirange companion in the type catalog
/// (ShellType.range). The subtype must resolve to a RangeElem family —
/// directly, or through a domain over one; composite/array/varbit
/// subtypes are 0A000 (documented hold-back).
pub(crate) fn exec_create_range_type(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    subty: ColType,
    sub_named: Option<&str>,
    multi_name: Option<&str>,
) -> Result<ExecResult, ExecError> {
    if eng.db.types.contains_key(name) {
        return Err(exec_err(
            "42710",
            format!("type \"{}\" already exists", name),
        ));
    }
    // Resolve the subtype to a range elem.
    let mut elem = RangeElem::of_coltype(&subty);
    if elem.is_none() {
        if let Some(sn) = sub_named {
            if let Some(st) = eng.db.types.get(sn) {
                if let Some(dom) = &st.domain {
                    elem = RangeElem::of_coltype(&dom.base);
                }
            }
        }
    }
    let elem = match elem {
        Some(e) => e,
        None => {
            return Err(exec_err(
                "0A000",
                "range types over this subtype are not supported".to_string(),
            ));
        }
    };
    let mname = multi_name
        .map(str::to_string)
        .unwrap_or_else(|| multirange_name_for(name));
    if eng.db.types.contains_key(&mname) {
        return Err(exec_err(
            "42710",
            format!("type \"{}\" already exists", mname),
        ));
    }
    let prev = eng.db.types.get(name).cloned();
    eng.db.types.insert(
        name.to_string(),
        ShellType {
            like_base: None,
            composite: None,
            domain: None,
            range: Some((elem, false)),
        },
    );
    ctx.writes.push(WriteOp::CreateType {
        name: name.to_string(),
        prev,
    });
    let prev_m = eng.db.types.get(&mname).cloned();
    eng.db.types.insert(
        mname.clone(),
        ShellType {
            like_base: None,
            composite: None,
            domain: None,
            range: Some((elem, true)),
        },
    );
    ctx.writes.push(WriteOp::CreateType {
        name: mname.clone(),
        prev: prev_m,
    });
    record_type_owner(eng, ctx, name);
    record_type_owner(eng, ctx, &mname);
    Ok(ExecResult::Command {
        tag: "CREATE TYPE".to_string(),
    })
}

/// v0.22: bounded DROP TYPE. Dependents are not tracked (CASCADE is
/// accepted and ignored); only the registry entry is removed.
/// v1.96: remove a dropped type/domain's owner + ACL records
/// (stale entries blocked DROP ROLE for long-dropped types).
pub(crate) fn clean_type_owner_acl(eng: &mut Engine, ctx: &mut StmtCtx, name: &str) {
    if eng.db.type_owners.contains_key(name) || eng.db.type_acls.contains_key(name) {
        let prev_fn = eng.db.fn_acls.clone();
        let prev_type = eng.db.type_acls.clone();
        let prev_lang = eng.db.lang_acls.clone();
        let prev_fn_owners = eng.db.fn_owners.clone();
        let prev_type_owners = eng.db.type_owners.clone();
        eng.db.type_owners.remove(name);
        eng.db.type_acls.remove(name);
        ctx.writes.push(WriteOp::FnTypeAcls {
            prev_fn,
            prev_type,
            prev_lang,
            prev_fn_owners,
            prev_type_owners,
        });
    }
}

pub(crate) fn exec_drop_type(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        let prev = eng.db.types.remove(name);
        if prev.is_none() && !if_exists {
            return Err(exec_err(
                "42704",
                format!("type \"{}\" does not exist", name),
            ));
        }
        if prev.is_some() {
            clean_type_owner_acl(eng, ctx, name);
        }
        // v1.97: PG19 DROP TYPE CASCADE drops dependent types:
        // domains based (transitively) on the dropped type and
        // composites containing it as an attribute. (Tables with
        // columns of the type are NOT dropped — that deeper
        // cascade remains a documented narrowing, as before.)
        if cascade && prev.is_some() {
            let mut removed: Vec<String> = vec![name.clone()];
            loop {
                let mut victims: Vec<String> = Vec::new();
                let mut tnames: Vec<String> = eng.db.types.keys().cloned().collect();
                tnames.sort();
                for tn in tnames {
                    if removed.contains(&tn) {
                        continue;
                    }
                    let st = eng.db.types.get(&tn).expect("key");
                    let depends = match &st.domain {
                        Some(dom) => {
                            dom.base_named.as_ref().is_some_and(|b| removed.contains(b))
                                || dom
                                    .base_domain
                                    .as_ref()
                                    .is_some_and(|b| removed.contains(b))
                                || domain_base_composite(&eng.db, &tn)
                                    .is_some_and(|b| removed.contains(&b))
                        }
                        None => match &st.composite {
                            Some(fields) => fields.iter().any(|(_, _, named)| {
                                named.as_ref().is_some_and(|n| removed.contains(n))
                            }),
                            None => false,
                        },
                    };
                    if depends {
                        victims.push(tn);
                    }
                }
                if victims.is_empty() {
                    break;
                }
                for v in victims {
                    if let Some(prev_v) = eng.db.types.remove(&v) {
                        clean_type_owner_acl(eng, ctx, &v);
                        ctx.writes.push(WriteOp::DropType {
                            name: v.clone(),
                            prev: Some(prev_v),
                        });
                        removed.push(v);
                    }
                }
            }
        }
        // v1.88: dropping a range type also drops its multirange
        // companion (PG dependency semantics).
        let is_range = matches!(
            prev,
            Some(ShellType {
                range: Some((_, false)),
                ..
            })
        );
        ctx.writes.push(WriteOp::DropType {
            name: name.clone(),
            prev,
        });
        if is_range {
            let mname = multirange_name_for(name);
            if let Some(prev_m) = eng.db.types.remove(&mname) {
                if matches!(
                    prev_m,
                    ShellType {
                        range: Some((_, true)),
                        ..
                    }
                ) {
                    ctx.writes.push(WriteOp::DropType {
                        name: mname,
                        prev: Some(prev_m),
                    });
                } else {
                    // Not our companion after all; restore it.
                    eng.db.types.insert(mname, prev_m);
                }
            }
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP TYPE".to_string(),
    })
}

/// v1.38: physical representation of a type for binary-cast
/// compatibility (PG19 `get_typlenbyvalalign`): (typlen, typbyval,
/// typalign). LIKE-defined named types resolve to their base;
/// builtins cover the fixed-size pass-by-value numerics. Anything
/// else (varlena types, shells, composites, domains, unknown names)
/// is None — PG19-faithful for the bounded type set; a binary cast
/// involving them is 42P17.
pub(crate) fn type_phys_repr(eng: &Engine, name: &str) -> Option<(i16, bool, char)> {
    // LIKE types share their base's representation (that's the point
    // of the float4/float8 test types: `like = float4`).
    let mut cur = name;
    loop {
        if let Ok(ct) = crate::sql::coltype_by_name(cur) {
            return match ct {
                crate::storage::ColType::SmallInt => Some((2, true, 's')),
                crate::storage::ColType::Int => Some((4, true, 'i')),
                crate::storage::ColType::BigInt => Some((8, true, 'd')),
                crate::storage::ColType::Float4 => Some((4, true, 'i')),
                crate::storage::ColType::Float => Some((8, true, 'd')),
                _ => None,
            };
        }
        match eng.db.types.get(cur) {
            Some(st) => match &st.like_base {
                Some(base) => cur = base,
                // Shell (or composite/domain/unknown-base LIKE): no
                // physical representation.
                None => return None,
            },
            None => return None,
        }
    }
}

/// v1.38: canonical cast-catalog name for a type: builtins fold to
/// `ColType::sql_name()` (`float4` -> `real`, `int4` -> `integer`);
/// LIKE-defined named types keep their catalog name. Shells,
/// composites, domains, and unknown names are None.
pub(crate) fn cast_type_key(eng: &Engine, name: &str) -> Option<String> {
    if let Ok(ct) = crate::sql::coltype_by_name(name) {
        return Some(ct.sql_name().to_string());
    }
    match eng.db.types.get(name) {
        Some(st) if st.like_base.is_some() => Some(name.to_string()),
        _ => None,
    }
}

/// v1.38: `CREATE CAST (src AS dst) WITHOUT FUNCTION [AS
/// IMPLICIT|AS ASSIGNMENT]` — PG19 `CreateCast`, bounded to the
/// binary method. Validation mirrors PG19: both types must exist
/// (42704); binary casts need superuser (42501 — the conformance
/// runner connects as the bootstrap superuser `postgres`); no
/// pseudo/shell types (42809); no composites, arrays, ranges, enums,
/// or domains (42P17); source and target must be physically compatible
/// — same typlen/byval/align (42P17); source and target must differ
/// (42P17); duplicates are 42710. `WITH FUNCTION` / `WITH INOUT`
/// parse but are honestly unimplemented (0A000).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_cast(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    src: &str,
    dst: &str,
    method: crate::sql::CastMethod,
    context: crate::sql::CastContext,
) -> Result<ExecResult, ExecError> {
    use crate::sql::CastMethod;
    // PG19: both types must exist (typenameTypeId -> 42704).
    for name in [src, dst] {
        let known = crate::sql::coltype_by_name(name).is_ok() || eng.db.types.contains_key(name);
        if !known {
            return Err(exec_err(
                "42704",
                format!("type \"{}\" does not exist", name),
            ));
        }
    }
    // v1.96: naming a user type/domain in CREATE CAST needs USAGE
    // on both endpoints (PG19 parse-time type check).
    check_type_usage(eng, ctx, src)?;
    check_type_usage(eng, ctx, dst)?;
    if method != CastMethod::Binary {
        // v1.96: WITH FUNCTION / WITH INOUT casts are recorded in
        // the cast catalog (the function association itself is not
        // modeled: casts between domains evaluate through domain
        // coercion in this engine). Duplicates are 42710.
        let key = (src.to_string(), dst.to_string());
        if eng.db.casts.contains_key(&key) {
            return Err(exec_err(
                "42710",
                format!(
                    "cast from type \"{}\" to type \"{}\" already exists",
                    src, dst
                ),
            ));
        }
        let prev = eng
            .db
            .casts
            .insert(key.clone(), crate::storage::CastDef { context });
        ctx.writes.push(WriteOp::CreateCast {
            src: key.0,
            dst: key.1,
            prev,
        });
        return Ok(ExecResult::Command {
            tag: "CREATE CAST".to_string(),
        });
    }
    // PG19: must be superuser to create a cast WITHOUT FUNCTION.
    if !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Err(exec_err(
            "42501",
            "must be superuser to create a cast WITHOUT FUNCTION".to_string(),
        ));
    }
    // PG19: no pseudo-types. rustgres shells are the pseudo-type
    // analogue here (a placeholder with no representation).
    for (which, name) in [("source", src), ("target", dst)] {
        if let Some(st) = eng.db.types.get(name) {
            if st.like_base.is_none() && st.composite.is_none() && st.domain.is_none() {
                return Err(exec_err(
                    "42809",
                    format!("{} data type \"{}\" is a pseudo-type", which, name),
                ));
            }
            if st.composite.is_some() {
                return Err(exec_err(
                    "42P17",
                    "composite data types are not binary-compatible".to_string(),
                ));
            }
            if st.domain.is_some() {
                return Err(exec_err(
                    "42P17",
                    "domain data types must not be marked binary-compatible".to_string(),
                ));
            }
        }
    }
    // Canonical keys; composites/domains/shells/unknowns were handled
    // above or are unkeyable (LIKE types excepted).
    let src_key = cast_type_key(eng, src).ok_or_else(|| {
        exec_err(
            "42P17",
            format!(
                "source data type \"{}\" cannot participate in a binary cast",
                src
            ),
        )
    })?;
    let dst_key = cast_type_key(eng, dst).ok_or_else(|| {
        exec_err(
            "42P17",
            format!(
                "target data type \"{}\" cannot participate in a binary cast",
                dst
            ),
        )
    })?;
    // PG19: source and target must be physically compatible.
    let src_phys = type_phys_repr(eng, src);
    let dst_phys = type_phys_repr(eng, dst);
    if src_phys.is_none() || dst_phys.is_none() || src_phys != dst_phys {
        return Err(exec_err(
            "42P17",
            "source and target data types are not physically compatible".to_string(),
        ));
    }
    // PG19: source and target must differ (length-coercion functions
    // are the only exception; binary casts have none).
    if src_key == dst_key {
        return Err(exec_err(
            "42P17",
            "source data type and target data type are the same".to_string(),
        ));
    }
    let key = (src_key, dst_key);
    if eng.db.casts.contains_key(&key) {
        return Err(exec_err(
            "42710",
            format!(
                "cast from type \"{}\" to type \"{}\" already exists",
                src, dst
            ),
        ));
    }
    let prev = eng
        .db
        .casts
        .insert(key.clone(), crate::storage::CastDef { context });
    debug_assert!(prev.is_none());
    ctx.writes.push(WriteOp::CreateCast {
        src: key.0,
        dst: key.1,
        prev,
    });
    Ok(ExecResult::Command {
        tag: "CREATE CAST".to_string(),
    })
}

/// v1.96: DROP CAST (source AS target) — removes a user cast from
/// the catalog (either key spelling: the as-written names used by
/// WITH FUNCTION casts, or the canonical sql_name keys used by
/// binary casts). Missing casts are 42883 unless IF EXISTS.
pub(crate) fn exec_drop_cast(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    src: &str,
    dst: &str,
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    let mut key = (src.to_string(), dst.to_string());
    if !eng.db.casts.contains_key(&key) {
        if let (Some(s), Some(d)) = (cast_type_key(eng, src), cast_type_key(eng, dst)) {
            key = (s, d);
        }
    }
    match eng.db.casts.remove(&key) {
        Some(prev) => {
            ctx.writes.push(WriteOp::CreateCast {
                src: key.0,
                dst: key.1,
                prev: Some(prev),
            });
            Ok(ExecResult::Command {
                tag: "DROP CAST".to_string(),
            })
        }
        None => {
            if if_exists {
                Ok(ExecResult::Command {
                    tag: "DROP CAST".to_string(),
                })
            } else {
                Err(exec_err(
                    "42883",
                    format!(
                        "cast from type \"{}\" to type \"{}\" does not exist",
                        src, dst
                    ),
                ))
            }
        }
    }
}

/// v1.96: ALTER TYPE name ADD ATTRIBUTE — appends a field to a
/// composite type (PG19: USAGE on the attribute's type is checked;
/// duplicates are 42701).
/// v1.96 finishing pass: does composite type `root` transitively
/// contain `target` as an attribute type (PG19's "composite type
/// cannot be made a member of itself" rule)? Range types over
/// composite subtypes are not constructible in this engine, so
/// only composite attribute chains are walked.
pub(crate) fn composite_type_contains(
    db: &crate::storage::Database,
    root: &str,
    target: &str,
    depth: usize,
) -> bool {
    if depth > 32 {
        return false;
    }
    if root == target {
        return true;
    }
    // Composite attributes from CREATE TYPE definitions.
    if let Some(st) = db.types.get(root) {
        if let Some(attrs) = &st.composite {
            for (_, _, nested) in attrs {
                if let Some(n) = nested {
                    if composite_type_contains(db, n, target, depth + 1) {
                        return true;
                    }
                }
            }
        }
    }
    // A table's rowtype is its composite form too (PG19): walk the
    // table's own named-composite columns.
    let from_table: Vec<Option<String>> = db
        .tables
        .get(root)
        .and_then(|vs| vs.last())
        .map(|t| t.composite_types.clone())
        .or_else(|| {
            db.temp_tables
                .values()
                .find_map(|m| m.get(root))
                .map(|t| t.composite_types.clone())
        })
        .unwrap_or_default();
    for n in from_table.into_iter().flatten() {
        if composite_type_contains(db, &n, target, depth + 1) {
            return true;
        }
    }
    false
}

/// v1.97: resolve a domain (through domain-over-domain chains) to
/// the composite type at its base, if any.
pub(crate) fn domain_base_composite(db: &crate::storage::Database, dname: &str) -> Option<String> {
    let mut cur = dname.to_string();
    for _ in 0..16 {
        let st = db.types.get(&cur)?;
        let dom = st.domain.as_ref()?;
        if let Some(inner) = &dom.base_domain {
            cur = inner.clone();
            continue;
        }
        return match &dom.base_named {
            Some(n) if db.types.get(n).is_some_and(|t| t.composite.is_some()) => Some(n.clone()),
            _ => None,
        };
    }
    None
}

/// v1.97: the composite type a table column effectively stores:
/// the column's own named composite, or the composite at the base
/// of its domain. None for non-composite columns.
pub(crate) fn column_effective_composite(
    db: &crate::storage::Database,
    t: &Table,
    i: usize,
) -> Option<String> {
    if let Some(Some(n)) = t.composite_types.get(i) {
        if db.types.get(n).is_some_and(|st| st.composite.is_some()) {
            return Some(n.clone());
        }
        if db.types.get(n).is_some_and(|st| st.domain.is_some()) {
            return domain_base_composite(db, n);
        }
    }
    if let Some(Some(d)) = t.domain_types.get(i) {
        return domain_base_composite(db, d);
    }
    None
}

/// v1.97: snapshot of every visible table (permanent catalog plus
/// this session's temp tables) as (name, table) pairs, sorted by
/// name for deterministic error reporting.
pub(crate) fn all_tables_snapshot(eng: &Engine, ctx: &StmtCtx) -> Vec<(String, Table)> {
    let mut out: Vec<(String, Table)> = Vec::new();
    let mut names: Vec<String> = eng.db.tables.keys().cloned().collect();
    names.sort();
    for n in names {
        if let Some(t) = eng.db.find_table(&n, ctx.snap, &ctx.all_xids, ctx.session) {
            out.push((n, t.clone()));
        }
    }
    if let Some(tmps) = eng.db.temp_tables.get(&ctx.session) {
        let mut tnames: Vec<String> = tmps.keys().cloned().collect();
        tnames.sort();
        for n in tnames {
            if out.iter().any(|(x, _)| x == &n) {
                continue;
            }
            if let Some(t) = eng.db.find_table(&n, ctx.snap, &ctx.all_xids, ctx.session) {
                out.push((n, t.clone()));
            }
        }
    }
    out
}

/// v1.96 finishing pass: guards on the INCOMING attribute type for
/// ALTER TYPE attribute actions (PG19 typecmds.c): the type must
/// exist, and a composite type cannot (transitively) become a
/// member of itself — including through a range/multirange
/// companion named after it, whose subtype is the type itself.
/// (The v1.96 column-use block that also lived here moved into the
/// per-action semantics in v1.97: PG blocks ALTER ATTRIBUTE with
/// 0A000, allows ADD, and handles DROP via attribute dependency
/// checks — see exec_alter_type.)
pub(crate) fn alter_type_named_guards(
    eng: &Engine,
    type_name: &str,
    new_named: Option<&str>,
) -> Result<(), ExecError> {
    if let Some(n) = new_named {
        const BUILTIN_RANGES: [&str; 12] = [
            "int4range",
            "int8range",
            "numrange",
            "tsrange",
            "tstzrange",
            "daterange",
            "int4multirange",
            "int8multirange",
            "nummultirange",
            "tsmultirange",
            "tstzmultirange",
            "datemultirange",
        ];
        if !eng.db.types.contains_key(n) && !BUILTIN_RANGES.contains(&n) {
            let companion_of_self = n.strip_suffix("multirange") == Some(type_name)
                || n.strip_suffix("range") == Some(type_name);
            if companion_of_self {
                return Err(exec_err(
                    "42P16",
                    format!(
                        "composite type {} cannot be made a member of itself",
                        type_name
                    ),
                ));
            }
            return Err(exec_err("42704", format!("type \"{}\" does not exist", n)));
        }
        if composite_type_contains(&eng.db, n, type_name, 0) {
            return Err(exec_err(
                "42P16",
                format!(
                    "composite type {} cannot be made a member of itself",
                    type_name
                ),
            ));
        }
    }
    Ok(())
}

/// v1.97: does expression `e` contain a FieldAccess on `field`
/// whose access chain roots at one of `root_cols` (or at any root
/// when `root_cols` is None — domain CHECKs, whose only root is
/// VALUE)?
fn expr_field_ref(e: &Expr, field: &str, root_cols: Option<&[String]>) -> bool {
    fn root_column(e: &Expr) -> Option<String> {
        match e {
            Expr::FieldAccess { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::CastNamed { expr, .. } => root_column(expr),
            Expr::Column { name, .. } => Some(name.clone()),
            _ => None,
        }
    }
    let mut found = false;
    walk_expr(e, &mut |node| {
        if let Expr::FieldAccess { expr, field: f } = node {
            if f == field {
                match root_cols {
                    None => found = true,
                    Some(cols) => {
                        if root_column(expr).is_some_and(|r| cols.iter().any(|c| c == &r)) {
                            found = true;
                        }
                    }
                }
            }
        }
    });
    found
}

/// v1.97: rename FieldAccess(field) occurrences in a stored
/// expression (domain CHECKs and CHECK/DEFAULT expressions over
/// composite-typed columns) for ALTER TYPE ... RENAME ATTRIBUTE.
/// Bounded walker over the expression families constraint
/// expressions may contain (validate_constraint_expr already
/// restricts them); unknown shapes are left untouched.
fn rename_field_in_expr(e: &mut Expr, old: &str, new: &str, root_cols: Option<&[String]>) {
    fn root_column(e: &Expr) -> Option<String> {
        match e {
            Expr::FieldAccess { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::CastNamed { expr, .. } => root_column(expr),
            Expr::Column { name, .. } => Some(name.clone()),
            _ => None,
        }
    }
    match e {
        Expr::FieldAccess { expr, field } => {
            let hit = *field == old
                && match root_cols {
                    None => true,
                    Some(cols) => root_column(expr).is_some_and(|r| cols.iter().any(|c| c == &r)),
                };
            if hit {
                *field = new.to_string();
            }
            rename_field_in_expr(expr, old, new, root_cols);
        }
        Expr::NamedArg { expr, .. } => rename_field_in_expr(expr, old, new, root_cols),
        Expr::Arith { left, right, .. } => {
            rename_field_in_expr(left, old, new, root_cols);
            rename_field_in_expr(right, old, new, root_cols);
        }
        Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => {
            rename_field_in_expr(expr, old, new, root_cols)
        }
        Expr::AtTimeZone { expr, zone } => {
            rename_field_in_expr(expr, old, new, root_cols);
            if let Some(z) = zone {
                rename_field_in_expr(z, old, new, root_cols);
            }
        }
        Expr::Row(elems) => {
            for e in elems {
                rename_field_in_expr(e, old, new, root_cols);
            }
        }
        Expr::Concat(a, b)
        | Expr::JsonOp {
            left: a, right: b, ..
        }
        | Expr::And(a, b)
        | Expr::Or(a, b) => {
            rename_field_in_expr(a, old, new, root_cols);
            rename_field_in_expr(b, old, new, root_cols);
        }
        Expr::Not(a) | Expr::BitNot(a) | Expr::Neg(a) => {
            rename_field_in_expr(a, old, new, root_cols)
        }
        Expr::Func { args, .. } => {
            for a in args {
                rename_field_in_expr(a, old, new, root_cols);
            }
        }
        Expr::Cmp { left, right, .. }
        | Expr::IsDistinctFrom { left, right, .. }
        | Expr::UserOp { left, right, .. } => {
            rename_field_in_expr(left, old, new, root_cols);
            rename_field_in_expr(right, old, new, root_cols);
        }
        Expr::Extract { from, .. } => rename_field_in_expr(from, old, new, root_cols),
        Expr::Slice { array, bounds } => {
            rename_field_in_expr(array, old, new, root_cols);
            for (lo, hi) in bounds {
                if let Some(e) = lo {
                    rename_field_in_expr(e, old, new, root_cols);
                }
                if let Some(e) = hi {
                    rename_field_in_expr(e, old, new, root_cols);
                }
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                rename_field_in_expr(o, old, new, root_cols);
            }
            for (k, r) in whens {
                rename_field_in_expr(k, old, new, root_cols);
                rename_field_in_expr(r, old, new, root_cols);
            }
            if let Some(el) = else_ {
                rename_field_in_expr(el, old, new, root_cols);
            }
        }
        Expr::Like { expr, pattern, .. } | Expr::Regex { expr, pattern, .. } => {
            rename_field_in_expr(expr, old, new, root_cols);
            rename_field_in_expr(pattern, old, new, root_cols);
        }
        Expr::IsNull { expr, .. } | Expr::IsBool { expr, .. } => {
            rename_field_in_expr(expr, old, new, root_cols)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            rename_field_in_expr(expr, old, new, root_cols);
            rename_field_in_expr(low, old, new, root_cols);
            rename_field_in_expr(high, old, new, root_cols);
        }
        Expr::ArrayCtor { elems, .. } => {
            for e in elems {
                rename_field_in_expr(e, old, new, root_cols);
            }
        }
        Expr::Subscript { array, indices } => {
            rename_field_in_expr(array, old, new, root_cols);
            for i in indices {
                rename_field_in_expr(i, old, new, root_cols);
            }
        }
        _ => {}
    }
}

/// v1.97: an attribute-level operation on stored composite values.
/// Composite values are name-keyed Records (possibly nested inside
/// arrays or other composites), so ADD appends a NULL field, DROP
/// removes it, and RENAME renames the key — in every stored value
/// whose type is (or contains) the altered composite.
#[derive(Clone)]
enum AttrValueOp {
    Add(String),
    Drop(String),
    Rename(String, String),
}

fn transform_typed_value(
    db: &crate::storage::Database,
    target: &str,
    target_fields: &[(String, ColType, Option<String>)],
    v: &mut Value,
    tyname: &str,
    op: &AttrValueOp,
    depth: usize,
) {
    if depth > 24 {
        return;
    }
    match v {
        Value::Array(a) => {
            for e in &mut a.elems {
                transform_typed_value(db, target, target_fields, e, tyname, op, depth + 1);
            }
        }
        Value::Record(fields) => {
            if tyname == target {
                match op {
                    AttrValueOp::Add(f) => {
                        if !fields.iter().any(|(n, _)| n == f) {
                            fields.push((f.clone(), Value::Null));
                        }
                    }
                    AttrValueOp::Drop(f) => fields.retain(|(n, _)| n != f),
                    AttrValueOp::Rename(o, n) => {
                        for (fn_, _) in fields.iter_mut() {
                            if fn_ == o {
                                *fn_ = n.clone();
                            }
                        }
                    }
                }
            }
            // Recurse into nested composite fields.
            let defs: Vec<(String, Option<String>)> = if tyname == target {
                target_fields
                    .iter()
                    .map(|(n, _, named)| (n.clone(), named.clone()))
                    .collect()
            } else {
                db.types
                    .get(tyname)
                    .and_then(|st| st.composite.clone())
                    .map(|fs| {
                        fs.iter()
                            .map(|(n, _, named)| (n.clone(), named.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            for (fname, fval) in fields.iter_mut() {
                if let Some((_, Some(nested))) = defs.iter().find(|(n, _)| n == fname) {
                    let nested = nested.clone();
                    transform_typed_value(db, target, target_fields, fval, &nested, op, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// v1.97: apply an AttrValueOp to every stored row of every table
/// holding a column whose effective composite is `target`.
fn transform_stored_records(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    target: &str,
    target_fields: &[(String, ColType, Option<String>)],
    op: &AttrValueOp,
) -> Result<(), ExecError> {
    let tables = all_tables_snapshot(eng, ctx);
    for (tname, t) in tables {
        let cols: Vec<usize> = (0..t.columns.len())
            .filter(|&i| column_effective_composite(&eng.db, &t, i).as_deref() == Some(target))
            .collect();
        if cols.is_empty() {
            continue;
        }
        let next = t.clone();
        let mut new_rows: Vec<RowVersion> = Vec::new();
        for rv in t
            .rows
            .iter()
            .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
        {
            let mut cells: Vec<Value> = rv.values.iter().cloned().collect();
            for &ci in &cols {
                if let Some(cell) = cells.get_mut(ci) {
                    let mut v = std::mem::replace(cell, Value::Null);
                    transform_typed_value(&eng.db, target, target_fields, &mut v, target, op, 0);
                    *cell = v;
                }
            }
            let mut rv2 = rv.clone();
            rv2.values = Row::new(cells);
            // v1.97: stamp with the current write xid (see the
            // ALTER COLUMN TYPE rewrite) so the commit-time WAL
            // conversion logs the transformed row; a plain clone
            // (old xmin) was silently dropped from the WAL batch.
            rv2.xmin = ctx.write_xid;
            rv2.xmax = 0;
            new_rows.push(rv2);
        }
        alter_swap(eng, ctx, &tname, None, next, Some(new_rows))?;
    }
    Ok(())
}

/// v1.97: typed tables of `type_name` plus all their inheritance /
/// partition descendants (the CASCADE propagation targets).
fn typed_propagation_targets(eng: &Engine, ctx: &StmtCtx, type_name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut typed: Vec<String> = eng
        .db
        .typed_tables
        .iter()
        .filter(|(_, v)| v.as_str() == type_name)
        .map(|(k, _)| k.clone())
        .collect();
    typed.sort();
    for t in typed {
        if !out.contains(&t) {
            out.push(t.clone());
        }
        for d in rename_descendants(eng, ctx, &t) {
            if !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

fn typed_table_error(type_name: &str) -> ExecError {
    exec_err(
        "2BP01",
        format!(
            "cannot alter type \"{}\" because it is the type of a typed table",
            type_name
        ),
    )
}

/// v1.97: sample (non-NULL, correctly typed) value for revalidating
/// domain CHECK expressions against a prospective attribute type.
fn sample_value_for(
    db: &crate::storage::Database,
    ty: &ColType,
    named: Option<&str>,
    depth: usize,
) -> Value {
    if depth > 8 {
        return Value::Null;
    }
    match ty {
        ColType::SmallInt => Value::SmallInt(1),
        ColType::Int => Value::Int(1),
        ColType::BigInt => Value::BigInt(1),
        ColType::Float4 => Value::Float4(1.0),
        ColType::Float => Value::Float(1.0),
        ColType::Numeric(_) => Value::Float(1.0),
        ColType::Text | ColType::Varchar(_) => Value::Text("x".into()),
        ColType::Char(_) => Value::BpChar("x".into()),
        ColType::Bool => Value::Bool(true),
        ColType::Date => Value::Date(0),
        ColType::Composite => match named.and_then(|n| db.types.get(n)) {
            Some(st) => match &st.composite {
                Some(fields) => Value::Record(
                    fields
                        .iter()
                        .map(|(n, t, named)| {
                            (
                                n.clone(),
                                sample_value_for(db, t, named.as_deref(), depth + 1),
                            )
                        })
                        .collect(),
                ),
                None => Value::Null,
            },
            None => Value::Null,
        },
        ColType::Array(elem) => {
            let inner = sample_value_for(db, &array_elem_coltype(*elem), named, depth + 1);
            Value::Array(Box::new(crate::storage::ArrayVal {
                elem: *elem,
                dims: vec![1],
                lower: vec![1],
                elems: vec![inner],
            }))
        }
        _ => Value::Null,
    }
}

/// v1.97: PG19 ALTER TYPE over composite attribute actions
/// (typecmds.c / tablecmds.c ATExec* on the type's relation):
/// attribute changes propagate into dependent stored definitions —
/// typed tables (RESTRICT refuses, CASCADE alters them too),
/// stored composite values, dependent domain CHECKs (revalidated
/// on ALTER ATTRIBUTE; field references block DROP ATTRIBUTE), and
/// stored CHECK/DEFAULT expressions (rewritten on RENAME).
pub(crate) fn exec_alter_type(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    actions: &[crate::sql::AlterTypeAction],
) -> Result<ExecResult, ExecError> {
    use crate::sql::AlterTypeAction;
    let orig = match eng.db.types.get(name) {
        Some(st) => st.clone(),
        None => {
            return Err(exec_err(
                "42704",
                format!("type \"{}\" does not exist", name),
            ));
        }
    };
    if orig.composite.is_none() {
        return Err(exec_err(
            "42809",
            format!("type \"{}\" is not a composite type", name),
        ));
    }
    let mut next = orig.clone();
    for action in actions {
        let fields = next.composite.clone().expect("composite checked");
        match action {
            AlterTypeAction::AddAttribute {
                attr,
                col_type,
                named_type,
                cascade,
            } => {
                if let Some(n) = named_type {
                    check_type_usage(eng, ctx, n)?;
                }
                alter_type_named_guards(eng, name, named_type.as_deref())?;
                if fields.iter().any(|(f, _, _)| f == attr) {
                    return Err(exec_err(
                        "42701",
                        format!("attribute \"{}\" of type \"{}\" already exists", attr, name),
                    ));
                }
                let typed = typed_propagation_targets(eng, ctx, name);
                let has_typed = eng.db.typed_tables.values().any(|v| v.as_str() == name);
                if has_typed && !cascade {
                    return Err(typed_table_error(name));
                }
                // Stored composite values gain a NULL field.
                transform_stored_records(eng, ctx, name, &fields, &AttrValueOp::Add(attr.clone()))?;
                if *cascade {
                    for tname in &typed {
                        alter_add_column_ext(
                            eng,
                            ctx,
                            tname,
                            attr,
                            col_type,
                            None,
                            false,
                            &None,
                            None,
                            &None,
                            &[],
                            &[],
                            &None,
                            &[],
                            true,
                        )?;
                    }
                }
                next.composite.as_mut().expect("composite").push((
                    attr.clone(),
                    *col_type,
                    named_type.clone(),
                ));
            }
            AlterTypeAction::AlterAttribute {
                attr,
                col_type,
                named_type,
                cascade,
            } => {
                if let Some(n) = named_type {
                    check_type_usage(eng, ctx, n)?;
                }
                alter_type_named_guards(eng, name, named_type.as_deref())?;
                let pos = fields
                    .iter()
                    .position(|(f, _, _)| f == attr)
                    .ok_or_else(|| {
                        exec_err(
                            "42703",
                            format!("attribute \"{}\" of type \"{}\" does not exist", attr, name),
                        )
                    })?;
                let has_typed = eng.db.typed_tables.values().any(|v| v.as_str() == name);
                if has_typed && !cascade {
                    return Err(typed_table_error(name));
                }
                // PG19 find_composite_type_dependencies: a stored
                // column using the type (directly or through a
                // domain) blocks with 0A000.
                let tables = all_tables_snapshot(eng, ctx);
                for (tname, t) in &tables {
                    for i in 0..t.columns.len() {
                        if column_effective_composite(&eng.db, t, i).as_deref() == Some(name) {
                            return Err(exec_err(
                                "0A000",
                                format!(
                                    "cannot alter type \"{}\" because column \"{}.{}\" uses it",
                                    name, tname, t.columns[i].0
                                ),
                            ));
                        }
                    }
                }
                // Index expressions using the type block too; PG
                // names the offender "<index>.row".
                let mut idx_names: Vec<String> = eng.db.indexes.keys().cloned().collect();
                idx_names.sort();
                for ix_name in idx_names {
                    let ix = eng.db.indexes.get(&ix_name).expect("key");
                    let mut uses = false;
                    for src in ix.def.exprs.iter().flatten() {
                        let Ok(e) = crate::sql::parse_expr_str(src) else {
                            continue;
                        };
                        let mut names = Vec::new();
                        crate::sql::collect_cast_type_names(&e, &mut names);
                        if names.iter().any(|n| n == name) {
                            uses = true;
                        }
                    }
                    if uses {
                        return Err(exec_err(
                            "0A000",
                            format!(
                                "cannot alter type \"{}\" because column \"{}.row\" uses it",
                                name, ix_name
                            ),
                        ));
                    }
                }
                // Revalidate dependent domain CHECKs against the
                // new attribute type (PG19 re-analyzes them; an
                // operator-resolution failure aborts the ALTER).
                revalidate_domains_for_alter(eng, ctx, name, &fields, pos, col_type, named_type)?;
                if *cascade && has_typed {
                    let typed = typed_propagation_targets(eng, ctx, name);
                    for tname in &typed {
                        alter_column_type_ext(
                            eng,
                            ctx,
                            tname,
                            attr,
                            col_type,
                            named_type.as_deref(),
                            None,
                            true,
                            true,
                        )?;
                    }
                }
                let slot = &mut next.composite.as_mut().expect("composite")[pos];
                slot.1 = *col_type;
                slot.2 = named_type.clone();
            }
            AlterTypeAction::DropAttribute {
                attr,
                if_exists,
                cascade,
            } => {
                let pos = fields.iter().position(|(f, _, _)| f == attr);
                let pos = match pos {
                    Some(p) => p,
                    None => {
                        if *if_exists {
                            continue;
                        }
                        return Err(exec_err(
                            "42703",
                            format!("attribute \"{}\" of type \"{}\" does not exist", attr, name),
                        ));
                    }
                };
                let has_typed = eng.db.typed_tables.values().any(|v| v.as_str() == name);
                if has_typed && !cascade {
                    return Err(typed_table_error(name));
                }
                // Field references from dependent stored
                // definitions: domain CHECKs over this type, and
                // CHECK / DEFAULT / index expressions of tables
                // holding the composite.
                let mut dom_refs: Vec<String> = Vec::new();
                let mut type_names: Vec<String> = eng.db.types.keys().cloned().collect();
                type_names.sort();
                for dname in &type_names {
                    if domain_base_composite(&eng.db, dname).as_deref() != Some(name) {
                        continue;
                    }
                    if let Some(dom) = eng.db.types.get(dname).and_then(|st| st.domain.clone()) {
                        if dom
                            .checks
                            .iter()
                            .any(|c| expr_field_ref(&c.expr, attr, None))
                        {
                            dom_refs.push(dname.clone());
                        }
                    }
                }
                // (table, kind, key): kind 0 = table CHECK by name,
                // 1 = column DEFAULT by index, 2 = index by name.
                let mut tbl_refs: Vec<(String, u8, String)> = Vec::new();
                let tables = all_tables_snapshot(eng, ctx);
                for (tname, t) in &tables {
                    let comp_cols: Vec<String> = (0..t.columns.len())
                        .filter(|&i| {
                            column_effective_composite(&eng.db, t, i).as_deref() == Some(name)
                        })
                        .map(|i| t.columns[i].0.clone())
                        .collect();
                    if comp_cols.is_empty() {
                        continue;
                    }
                    for c in &t.checks {
                        if expr_field_ref(&c.expr, attr, Some(&comp_cols)) {
                            tbl_refs.push((tname.clone(), 0, c.name.clone()));
                        }
                    }
                    for (i, d) in t.defaults.iter().enumerate() {
                        if comp_cols.iter().any(|c| c == &t.columns[i].0) {
                            if let Some(crate::sql::DefaultExpr::Expr(e)) = d {
                                if expr_field_ref(e, attr, None) {
                                    tbl_refs.push((tname.clone(), 1, i.to_string()));
                                }
                            }
                        }
                    }
                }
                let mut idx_refs: Vec<String> = Vec::new();
                let mut idx_names: Vec<String> = eng.db.indexes.keys().cloned().collect();
                idx_names.sort();
                for ix_name in idx_names {
                    let ix = eng.db.indexes.get(&ix_name).expect("key");
                    let Some(t) = tables.iter().find(|(n, _)| n == &ix.def.table) else {
                        continue;
                    };
                    let comp_cols: Vec<String> = (0..t.1.columns.len())
                        .filter(|&i| {
                            column_effective_composite(&eng.db, &t.1, i).as_deref() == Some(name)
                        })
                        .map(|i| t.1.columns[i].0.clone())
                        .collect();
                    if comp_cols.is_empty() {
                        continue;
                    }
                    for src in ix.def.exprs.iter().flatten() {
                        let Ok(e) = crate::sql::parse_expr_str(src) else {
                            continue;
                        };
                        if expr_field_ref(&e, attr, Some(&comp_cols)) {
                            idx_refs.push(ix_name.clone());
                            break;
                        }
                    }
                }
                if !dom_refs.is_empty() || !tbl_refs.is_empty() || !idx_refs.is_empty() {
                    if !*cascade {
                        return Err(exec_err(
                            "2BP01",
                            format!(
                                "cannot drop column {} of composite type {} because other objects depend on it",
                                attr, name
                            ),
                        ));
                    }
                    // CASCADE: drop the dependent definitions.
                    for dname in &dom_refs {
                        let prev = eng.db.types.get(dname).expect("domain").clone();
                        let mut dom = prev.domain.clone().expect("domain");
                        dom.checks.retain(|c| !expr_field_ref(&c.expr, attr, None));
                        eng.db.types.insert(
                            dname.clone(),
                            crate::storage::ShellType {
                                like_base: prev.like_base.clone(),
                                composite: prev.composite.clone(),
                                domain: Some(dom),
                                range: prev.range.clone(),
                            },
                        );
                        ctx.writes.push(WriteOp::CreateType {
                            name: dname.clone(),
                            prev: Some(prev),
                        });
                    }
                    for (tname, kind, key) in &tbl_refs {
                        let t = eng
                            .db
                            .find_table(tname, ctx.snap, &ctx.all_xids, ctx.session)
                            .expect("snapshot")
                            .clone();
                        let mut nt = t;
                        match kind {
                            0 => nt.checks.retain(|c| &c.name != key),
                            1 => {
                                let i: usize = key.parse().expect("index");
                                nt.defaults[i] = None;
                            }
                            _ => {}
                        }
                        alter_swap(eng, ctx, tname, None, nt, None)?;
                    }
                    for ix_name in &idx_refs {
                        drop_index_internal(eng, ctx, ix_name)?;
                    }
                }
                // Stored composite values lose the field.
                transform_stored_records(
                    eng,
                    ctx,
                    name,
                    &fields,
                    &AttrValueOp::Drop(attr.clone()),
                )?;
                if *cascade && has_typed {
                    let typed = typed_propagation_targets(eng, ctx, name);
                    for tname in &typed {
                        alter_drop_column_ext(eng, ctx, tname, attr, true, true, true)?;
                    }
                }
                next.composite.as_mut().expect("composite").remove(pos);
            }
            AlterTypeAction::RenameAttribute { old, new, cascade } => {
                let pos = fields
                    .iter()
                    .position(|(f, _, _)| f == old)
                    .ok_or_else(|| {
                        exec_err(
                            "42703",
                            format!("attribute \"{}\" of type \"{}\" does not exist", old, name),
                        )
                    })?;
                if fields.iter().any(|(f, _, _)| f == new) {
                    return Err(exec_err(
                        "42701",
                        format!("attribute \"{}\" of type \"{}\" already exists", new, name),
                    ));
                }
                let has_typed = eng.db.typed_tables.values().any(|v| v.as_str() == name);
                if has_typed && !cascade {
                    return Err(typed_table_error(name));
                }
                // Stored composite values rename the key.
                transform_stored_records(
                    eng,
                    ctx,
                    name,
                    &fields,
                    &AttrValueOp::Rename(old.clone(), new.clone()),
                )?;
                // Stored expressions referencing the field follow:
                // domain CHECKs over this type (root VALUE) and
                // CHECK/DEFAULT expressions over composite columns.
                let mut type_names: Vec<String> = eng.db.types.keys().cloned().collect();
                type_names.sort();
                for dname in &type_names {
                    if domain_base_composite(&eng.db, dname).as_deref() != Some(name) {
                        continue;
                    }
                    let prev = eng.db.types.get(dname).expect("type").clone();
                    if let Some(mut dom) = prev.domain.clone() {
                        let mut changed = false;
                        for c in &mut dom.checks {
                            let before = format!("{:?}", c.expr);
                            rename_field_in_expr(&mut c.expr, old, new, None);
                            if format!("{:?}", c.expr) != before {
                                changed = true;
                            }
                        }
                        if changed {
                            eng.db.types.insert(
                                dname.clone(),
                                crate::storage::ShellType {
                                    like_base: prev.like_base.clone(),
                                    composite: prev.composite.clone(),
                                    domain: Some(dom),
                                    range: prev.range.clone(),
                                },
                            );
                            ctx.writes.push(WriteOp::CreateType {
                                name: dname.clone(),
                                prev: Some(prev),
                            });
                        }
                    }
                }
                let tables = all_tables_snapshot(eng, ctx);
                for (tname, t) in &tables {
                    let comp_cols: Vec<String> = (0..t.columns.len())
                        .filter(|&i| {
                            column_effective_composite(&eng.db, t, i).as_deref() == Some(name)
                        })
                        .map(|i| t.columns[i].0.clone())
                        .collect();
                    if comp_cols.is_empty() {
                        continue;
                    }
                    let mut nt = t.clone();
                    let mut changed = false;
                    for c in &mut nt.checks {
                        let before = format!("{:?}", c.expr);
                        rename_field_in_expr(&mut c.expr, old, new, Some(&comp_cols));
                        if format!("{:?}", c.expr) != before {
                            changed = true;
                        }
                    }
                    for d in &mut nt.defaults {
                        if let Some(crate::sql::DefaultExpr::Expr(e)) = d {
                            let before = format!("{:?}", *e);
                            rename_field_in_expr(e, old, new, Some(&comp_cols));
                            if format!("{:?}", *e) != before {
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        alter_swap(eng, ctx, tname, None, nt, None)?;
                    }
                }
                if *cascade && has_typed {
                    let typed = typed_propagation_targets(eng, ctx, name);
                    for tname in &typed {
                        rename_column_one_table(eng, ctx, tname, old, new)?;
                    }
                }
                next.composite.as_mut().expect("composite")[pos].0 = new.clone();
            }
        }
    }
    eng.db.types.insert(name.to_string(), next);
    ctx.writes.push(WriteOp::CreateType {
        name: name.to_string(),
        prev: Some(orig),
    });
    Ok(ExecResult::Command {
        tag: "ALTER TYPE".to_string(),
    })
}

/// v1.97: PG display name of a column type (for 42883 messages).
fn pg_type_display(t: &ColType) -> String {
    match t {
        ColType::Int => "integer".to_string(),
        ColType::BigInt => "bigint".to_string(),
        ColType::SmallInt => "smallint".to_string(),
        ColType::Float => "double precision".to_string(),
        ColType::Float4 => "real".to_string(),
        ColType::Numeric(_) => "numeric".to_string(),
        ColType::Text => "text".to_string(),
        ColType::Varchar(_) => "character varying".to_string(),
        ColType::Char(_) => "character".to_string(),
        ColType::Bool => "boolean".to_string(),
        _ => format!("{:?}", t).to_lowercase(),
    }
}

/// v1.97: does `e` contain an explicit cast whose subtree
/// field-accesses `attr`? (PG19 analysis accepts those: the cast
/// exists, only runtime values could fail.)
fn expr_has_cast_on_field(e: &Expr, attr: &str) -> bool {
    fn subtree_has_field(e: &Expr, attr: &str) -> bool {
        let mut found = false;
        walk_expr(e, &mut |n| {
            if matches!(n, Expr::FieldAccess { field, .. } if field == attr) {
                found = true;
            }
        });
        found
    }
    let mut found = false;
    walk_expr(e, &mut |n| match n {
        Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => {
            if subtree_has_field(expr, attr) {
                found = true;
            }
        }
        _ => {}
    });
    found
}

/// v1.97: symbol of the first comparison whose subtree
/// field-accesses `attr` (for the 42883 message).
fn first_cmp_op_on_field(e: &Expr, attr: &str) -> Option<String> {
    fn subtree_has_field(e: &Expr, attr: &str) -> bool {
        let mut found = false;
        walk_expr(e, &mut |n| {
            if matches!(n, Expr::FieldAccess { field, .. } if field == attr) {
                found = true;
            }
        });
        found
    }
    let mut out: Option<String> = None;
    walk_expr(e, &mut |n| {
        if out.is_none() {
            if let Expr::Cmp { op, left, right } = n {
                if subtree_has_field(left, attr) || subtree_has_field(right, attr) {
                    out = Some(op.sql().to_string());
                }
            }
        }
    });
    out
}

/// v1.97: revalidate the CHECK expressions of domains based on
/// composite `name` after attribute `pos` changes to a new type.
/// PG19 re-analyzes the constraint expressions; the observable
/// failure is operator resolution (42883). We evaluate each check
/// against a sample composite whose altered field carries a
/// sample of the NEW type and propagate only 42883 failures —
/// value-dependent outcomes are not analysis failures.
fn revalidate_domains_for_alter(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    fields: &[(String, ColType, Option<String>)],
    pos: usize,
    new_type: &ColType,
    new_named: &Option<String>,
) -> Result<(), ExecError> {
    let mut type_names: Vec<String> = eng.db.types.keys().cloned().collect();
    type_names.sort();
    let mut domains: Vec<String> = Vec::new();
    for dname in &type_names {
        if domain_base_composite(&eng.db, dname).as_deref() == Some(name) {
            domains.push(dname.clone());
        }
    }
    if domains.is_empty() {
        return Ok(());
    }
    // Sample composite as typed BEFORE the change (the domain
    // check was valid against it by construction).
    let old_sample = Value::Record(
        fields
            .iter()
            .map(|(n, t, named)| (n.clone(), sample_value_for(&eng.db, t, named.as_deref(), 0)))
            .collect(),
    );
    // Sample composite with the altered field re-typed.
    let sample_fields: Vec<Value> = fields
        .iter()
        .enumerate()
        .map(|(i, (_, t, named))| {
            if i == pos {
                sample_value_for(&eng.db, new_type, new_named.as_deref(), 0)
            } else {
                sample_value_for(&eng.db, t, named.as_deref(), 0)
            }
        })
        .collect();
    let sample = Value::Record(
        fields
            .iter()
            .map(|(n, _, _)| n.clone())
            .zip(sample_fields)
            .collect(),
    );
    for dname in &domains {
        let dom = eng
            .db
            .types
            .get(dname)
            .and_then(|st| st.domain.clone())
            .expect("domain");
        for c in &dom.checks {
            // Evaluate against BOTH the old-typed and new-typed
            // samples. PG19 fails the ALTER at analysis time when
            // the expression no longer resolves (42883). This
            // engine resolves some cross-type comparisons by
            // coercion at value time, surfacing PG's analysis
            // failure as 22P02 on the synthetic sample instead —
            // a NEW 22P02 (absent under the old type) with no
            // explicit cast around the field is that analysis
            // failure, and is reported as PG's 42883.
            let eval_with = |eng2: &mut Engine, sample2: &Value| -> Result<Value, ExecError> {
                let mut lock_ids2: Vec<(String, u64)> = Vec::new();
                let mut q2 = Q {
                    eng: eng2,
                    snap: ctx.snap,
                    own: ctx.own,
                    all_xids: ctx.all_xids.clone(),
                    session: ctx.session,
                    role: ctx.role,
                    read_only: true,
                    depth: 0,
                    lock_ids: &mut lock_ids2,
                    ctes: Vec::new(),
                    wctx: None,
                    srf_vals: Vec::new(),
                    priv_scopes: Vec::new(),
                    priv_role_override: None,
                    hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                    pending_updates: None,
                    write: None,
                };
                let schema2 = vec![QCol {
                    qual: String::new(),
                    name: "value".to_string(),
                    ty: ColType::Composite,
                    hidden: false,
                    src_ord: 0,
                }];
                let frame2 = Scope {
                    schema: &schema2,
                    row: std::slice::from_ref(sample2),
                    prov: None,
                };
                eval_expr(&mut q2, &[frame2], &c.expr)
            };
            if let Err(e) = eval_with(&mut *eng, &sample) {
                if e.code == "42883" {
                    return Err(e);
                }
                if e.code == "22P02" {
                    let attr = &fields[pos].0;
                    let old_err = eval_with(&mut *eng, &old_sample).err();
                    let old_also = old_err.is_some_and(|oe| oe.code == "22P02");
                    if !old_also && !expr_has_cast_on_field(&c.expr, attr) {
                        return Err(exec_err(
                            "42883",
                            format!(
                                "operator does not exist: {} {} {}",
                                pg_type_display(new_type),
                                first_cmp_op_on_field(&c.expr, attr)
                                    .unwrap_or_else(|| ">".to_string()),
                                pg_type_display(&fields[pos].1),
                            ),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// v0.85: CREATE DOMAIN — a named type over a base type with CHECK
/// constraints (PG19 `coerce_to_domain`). The base resolves against
/// the type catalog: builtins pass through, a named composite base
/// must be a defined composite (42704 otherwise), and a named domain
/// base chains (`base_domain`) with the inner base flattened in.
/// Domain CHECK expressions may only reference `value` (PG19
/// DefineDomain / domain_check).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_domain(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    base: &ColType,
    base_named: Option<&str>,
    checks: &[CheckDef],
    not_null: bool,
    default: Option<&DefaultExpr>,
    base_bit_typmod: Option<(i32, bool)>,
) -> Result<ExecResult, ExecError> {
    if eng.db.types.contains_key(name) {
        return Err(exec_err(
            "42710",
            format!("type \"{}\" already exists", name),
        ));
    }
    // v1.96: USAGE on the base type and on cast targets inside the
    // domain's CHECK / DEFAULT expressions (PG19 parse-time checks).
    if let Some(bn) = base_named {
        check_type_usage(eng, ctx, bn)?;
    }
    for c in checks {
        check_expr_cast_usages(eng, ctx, &c.expr)?;
    }
    if let Some(crate::sql::DefaultExpr::Expr(e)) = default {
        check_expr_cast_usages(eng, ctx, e)?;
    }
    let (base_ty, resolved_named, base_domain) = match base_named {
        None => (base.clone(), None, None),
        Some(bn) => match eng.db.types.get(bn) {
            Some(st) if st.composite.is_some() => {
                if *base != ColType::Composite {
                    return Err(exec_err(
                        "0A000",
                        format!("domain base type mismatch for \"{}\"", name),
                    ));
                }
                (ColType::Composite, Some(bn.to_string()), None)
            }
            Some(st) if st.domain.is_some() => {
                // Domain over domain: flatten the inner base, chain the
                // checks via `base_domain` (PG19 checks innermost first).
                let inner = st.domain.as_ref().expect("domain branch has a def");
                (
                    inner.base.clone(),
                    inner.base_named.clone(),
                    Some(bn.to_string()),
                )
            }
            Some(st) if st.like_base.is_some() => {
                // LIKE-completed alias: resolve to the builtin it names.
                match crate::sql::coltype_by_name(st.like_base.as_deref().unwrap_or("")) {
                    Ok(bt) => (bt, None, None),
                    Err(_) => {
                        return Err(exec_err("42704", format!("type \"{}\" does not exist", bn)));
                    }
                }
            }
            _ => {
                return Err(exec_err("42704", format!("type \"{}\" does not exist", bn)));
            }
        },
    };
    // Array-of-domain bases (`CREATE DOMAIN d AS e[]` with `e` a domain)
    // are not supported yet — honest 0A000 instead of mis-checking.
    if matches!(base_ty, ColType::Array(_)) && base_named.is_some() {
        return Err(exec_err(
            "0A000",
            format!(
                "domain over array of domain \"{}\" is not supported yet",
                name
            ),
        ));
    }
    // PG19: domain CHECK expressions may only reference VALUE.
    for c in checks {
        let mut refs = Vec::new();
        crate::sql::collect_col_refs(&c.expr, &mut refs);
        for (_, r) in &refs {
            if r != "value" {
                return Err(exec_err(
                    "42601",
                    format!(
                        "cannot use column reference \"{}\" in domain check constraint",
                        r
                    ),
                ));
            }
        }
    }
    eng.db.types.insert(
        name.to_string(),
        ShellType {
            like_base: None,
            composite: None,
            range: None,
            domain: Some(crate::storage::DomainDef {
                base: base_ty,
                base_named: resolved_named,
                base_domain,
                checks: checks.to_vec(),
                not_null,
                default: default.cloned(),
                bit_typmod: base_bit_typmod,
            }),
        },
    );
    // v0.82's CreateType WAL path reads the live type map, so the domain
    // definition is WAL-logged (and checkpointed) automatically.
    ctx.writes.push(WriteOp::CreateType {
        name: name.to_string(),
        prev: None,
    });
    record_type_owner(eng, ctx, name);
    Ok(ExecResult::Command {
        tag: "CREATE DOMAIN".to_string(),
    })
}

/// v0.85: DROP DOMAIN [IF EXISTS] name [, ...] — mirrors DROP TYPE
/// (dependents are not tracked, like the bounded type support).
pub(crate) fn exec_drop_domain(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        let prev = eng.db.types.remove(name);
        let is_domain = prev.as_ref().is_some_and(|st| st.domain.is_some());
        if !is_domain {
            // Restore a non-domain type (a domain drop must not remove
            // a composite/shell type); PG reports "type does not exist"
            // for a non-domain name here.
            if let Some(st) = prev {
                eng.db.types.insert(name.clone(), st);
            }
            if !if_exists {
                return Err(exec_err(
                    "42704",
                    format!("type \"{}\" does not exist", name),
                ));
            }
            continue;
        }
        ctx.writes.push(WriteOp::DropType {
            name: name.clone(),
            prev,
        });
        clean_type_owner_acl(eng, ctx, name);
    }
    Ok(ExecResult::Command {
        tag: "DROP DOMAIN".to_string(),
    })
}

/// v0.97: ALTER DOMAIN (PG19 AlterDomainStmt, bounded). Mutates the
/// domain definition in place; the old `ShellType` is carried on
/// `WriteOp::CreateType.prev` so rollback restores it exactly, and the
/// WAL encoder reads the live type map — so the alteration is
/// transactional, WAL-logged and checkpointed with no new record
/// kinds. PG19 does not validate existing stored data when a domain
/// constraint is added (constraints apply to future writes), and
/// neither do we.
/// v1.97: does domain `start`'s base_domain chain include
/// `target` (or is `start` == `target`)?
fn domain_chain_includes(db: &crate::storage::Database, start: &str, target: &str) -> bool {
    let mut cur = start.to_string();
    for _ in 0..16 {
        if cur == target {
            return true;
        }
        match db
            .types
            .get(&cur)
            .and_then(|st| st.domain.as_ref())
            .and_then(|d| d.base_domain.clone())
        {
            Some(inner) => cur = inner,
            None => return false,
        }
    }
    false
}

/// v1.97: composite types that (transitively) contain an attribute
/// whose type resolves to domain `dname` — the "container" set for
/// PG19's find_composite_type_dependencies rule: ALTER DOMAIN ADD
/// CONSTRAINT over such a domain cannot be validated against
/// stored rows (the domain value is buried inside composite
/// values), so PG refuses with 0A000 when a stored column uses
/// one of these composites.
fn composites_containing_domain(db: &crate::storage::Database, dname: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    loop {
        let mut grew = false;
        let mut names: Vec<String> = db.types.keys().cloned().collect();
        names.sort();
        for n in names {
            if out.contains(&n) {
                continue;
            }
            let Some(fields) = db.types.get(&n).and_then(|st| st.composite.clone()) else {
                continue;
            };
            let hit = fields.iter().any(|(_, _, named)| match named {
                Some(a) => {
                    a == dname || domain_chain_includes(db, a, dname) || out.iter().any(|c| c == a)
                }
                None => false,
            });
            if hit {
                out.push(n);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    out
}

/// v1.97: evaluate ONE prospective domain check against one stored
/// value (PG19 validateDomainCheckConstraint). FALSE violates;
/// NULL passes (checks only run on non-null domain values).
fn domain_check_holds(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    base: &ColType,
    check: &crate::sql::CheckDef,
    value: &Value,
) -> Result<bool, ExecError> {
    let mut lock_ids: Vec<(String, u64)> = Vec::new();
    let mut q = Q {
        eng: &mut *eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: true,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        priv_role_override: None,
        hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new())),
        immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(
            std::collections::HashMap::new(),
        )),
        plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new())),
        hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        pending_updates: None,
        write: None,
    };
    let schema = vec![QCol {
        qual: String::new(),
        name: "value".to_string(),
        ty: *base,
        hidden: false,
        src_ord: 0,
    }];
    let frame = Scope {
        schema: &schema,
        row: std::slice::from_ref(value),
        prov: None,
    };
    match eval_expr(&mut q, &[frame], &check.expr)? {
        Value::Bool(false) => Ok(false),
        _ => Ok(true),
    }
}

/// v1.97: PG19 AlterDomainAddConstraint validation
/// (typecmds.c validateDomainCheckConstraint): existing stored
/// values in columns of this domain (or of domains derived from
/// it) must satisfy the new check — 23514 naming the first
/// violating column. When the domain reaches a stored column only
/// through a composite container, PG refuses outright (0A000,
/// find_composite_type_dependencies).
fn validate_domain_new_check(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    dname: &str,
    base: &ColType,
    check: &crate::sql::CheckDef,
) -> Result<(), ExecError> {
    let containers = composites_containing_domain(&eng.db, dname);
    let tables = all_tables_snapshot(eng, ctx);
    // Container refusal first (PG checks dependencies before
    // scanning data).
    if !containers.is_empty() {
        for (tname, t) in &tables {
            for i in 0..t.columns.len() {
                if let Some(ec) = column_effective_composite(&eng.db, t, i) {
                    if containers.iter().any(|c| c == &ec) {
                        return Err(exec_err(
                            "0A000",
                            format!(
                                "cannot alter type \"{}\" because column \"{}.{}\" uses it",
                                dname, tname, t.columns[i].0
                            ),
                        ));
                    }
                }
            }
        }
    }
    for (tname, t) in &tables {
        for i in 0..t.columns.len() {
            let Some(Some(cd)) = t.domain_types.get(i) else {
                continue;
            };
            if !domain_chain_includes(&eng.db, cd, dname) {
                continue;
            }
            let is_elem = t.domain_elem.get(i).copied().unwrap_or(false);
            let mut values: Vec<Value> = Vec::new();
            for rv in t
                .rows
                .iter()
                .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
            {
                let Some(cell) = rv.values.get(i) else {
                    continue;
                };
                if is_elem {
                    if let Value::Array(a) = cell {
                        values.extend(a.elems.iter().cloned());
                    }
                } else {
                    values.push(cell.clone());
                }
            }
            for v in &values {
                if matches!(v, Value::Null) {
                    continue;
                }
                if !domain_check_holds(eng, ctx, base, check, v)? {
                    return Err(exec_err(
                        "23514",
                        format!(
                            "column \"{}\" of table \"{}\" contains values that violate the new constraint",
                            t.columns[i].0, tname
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn exec_alter_domain(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    action: &crate::sql::AlterDomainAction,
) -> Result<ExecResult, ExecError> {
    use crate::sql::AlterDomainAction;
    let old = eng
        .db
        .types
        .get(name)
        .cloned()
        .ok_or_else(|| exec_err("42704", format!("type \"{}\" does not exist", name)))?;
    let mut dom = old
        .domain
        .clone()
        .ok_or_else(|| exec_err("42809", format!("\"{}\" is not a domain", name)))?;
    match action {
        AlterDomainAction::AddConstraint {
            name: cname,
            expr,
            not_valid,
        } => {
            // PG19: domain CHECK expressions may only reference VALUE
            // (same rule as CREATE DOMAIN).
            let mut refs = Vec::new();
            crate::sql::collect_col_refs(expr, &mut refs);
            for (_, r) in &refs {
                if r != "value" {
                    return Err(exec_err(
                        "42601",
                        format!(
                            "cannot use column reference \"{}\" in domain check constraint",
                            r
                        ),
                    ));
                }
            }
            // Auto-name `<domain>_check[N]` when CONSTRAINT name was
            // omitted (PG19 ChooseConstraintName, same as CREATE).
            let cname = cname.clone().unwrap_or_else(|| {
                let mut n = format!("{}_check", name);
                let mut i = 1;
                while dom.checks.iter().any(|c| c.name == n) {
                    n = format!("{}_check{}", name, i);
                    i += 1;
                }
                n
            });
            if dom.checks.iter().any(|c| c.name == cname) {
                return Err(exec_err(
                    "42710",
                    format!(
                        "constraint \"{}\" of domain \"{}\" already exists",
                        cname, name
                    ),
                ));
            }
            let new_check = crate::sql::CheckDef {
                name: cname,
                expr: expr.clone(),
                not_valid: *not_valid,
                kind: crate::sql::CheckKind::Check,
                no_inherit: false,
            };
            // v1.97: PG19 validates existing stored data against a
            // new domain constraint (validateDomainCheckConstraint)
            // unless it is added NOT VALID.
            if !*not_valid {
                validate_domain_new_check(eng, ctx, name, &dom.base, &new_check)?;
            }
            dom.checks.push(new_check);
        }
        AlterDomainAction::ValidateConstraint { name: cname } => {
            // v1.97: VALIDATE CONSTRAINT scans the stored data like
            // the ADD-time validation, then clears not_valid.
            let pos = dom
                .checks
                .iter()
                .position(|c| &c.name == cname)
                .ok_or_else(|| {
                    exec_err(
                        "42704",
                        format!(
                            "constraint \"{}\" of domain \"{}\" does not exist",
                            cname, name
                        ),
                    )
                })?;
            let check = dom.checks[pos].clone();
            validate_domain_new_check(eng, ctx, name, &dom.base, &check)?;
            dom.checks[pos].not_valid = false;
        }
        AlterDomainAction::DropConstraint {
            name: cname,
            if_exists,
        } => {
            let pos = dom.checks.iter().position(|c| &c.name == cname);
            match pos {
                Some(i) => {
                    dom.checks.remove(i);
                }
                None => {
                    if !if_exists {
                        return Err(exec_err(
                            "42704",
                            format!(
                                "constraint \"{}\" of domain \"{}\" does not exist",
                                cname, name
                            ),
                        ));
                    }
                }
            }
        }
        AlterDomainAction::SetNotNull => {
            dom.not_null = true;
        }
        AlterDomainAction::DropNotNull => {
            dom.not_null = false;
        }
        AlterDomainAction::SetDefault(d) => {
            dom.default = Some(d.clone());
        }
        AlterDomainAction::DropDefault => {
            dom.default = None;
        }
    }
    let prev = old.clone();
    eng.db.types.insert(
        name.to_string(),
        crate::storage::ShellType {
            like_base: old.like_base,
            composite: old.composite,
            domain: Some(dom),
            range: old.range,
        },
    );
    // Transactional: undo restores `prev`; the WAL encoder reads the
    // live (new) definition from the type map.
    ctx.writes.push(WriteOp::CreateType {
        name: name.to_string(),
        prev: Some(prev),
    });
    Ok(ExecResult::Command {
        tag: "ALTER DOMAIN".to_string(),
    })
}
