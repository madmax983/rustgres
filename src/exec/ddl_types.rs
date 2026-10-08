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
                    Some(st) if st.composite.is_some() => {}
                    _ => {
                        return Err(exec_err(
                            "42704",
                            format!("type \"{}\" does not exist", nested_name),
                        ));
                    }
                }
            }
        }
        eng.db.types.insert(
            name.to_string(),
            ShellType {
                like_base: None,
                composite: Some(fields.to_vec()),
                domain: None,
            },
        );
        ctx.writes.push(WriteOp::CreateType {
            name: name.to_string(),
            prev,
        });
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
                },
            );
        }
    }
    ctx.writes.push(WriteOp::CreateType {
        name: name.to_string(),
        prev,
    });
    Ok(ExecResult::Command {
        tag: "CREATE TYPE".to_string(),
    })
}

/// v0.22: bounded DROP TYPE. Dependents are not tracked (CASCADE is
/// accepted and ignored); only the registry entry is removed.
pub(crate) fn exec_drop_type(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        let prev = eng.db.types.remove(name);
        if prev.is_none() && !if_exists {
            return Err(exec_err(
                "42704",
                format!("type \"{}\" does not exist", name),
            ));
        }
        ctx.writes.push(WriteOp::DropType {
            name: name.clone(),
            prev,
        });
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
    if method != CastMethod::Binary {
        return Err(exec_err(
            "0A000",
            "CREATE CAST WITH FUNCTION / WITH INOUT is not implemented; only WITHOUT FUNCTION (binary) casts are supported".to_string(),
        ));
    }
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
) -> Result<ExecResult, ExecError> {
    if eng.db.types.contains_key(name) {
        return Err(exec_err(
            "42710",
            format!("type \"{}\" already exists", name),
        ));
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
            domain: Some(crate::storage::DomainDef {
                base: base_ty,
                base_named: resolved_named,
                base_domain,
                checks: checks.to_vec(),
                not_null,
                default: default.cloned(),
            }),
        },
    );
    // v0.82's CreateType WAL path reads the live type map, so the domain
    // definition is WAL-logged (and checkpointed) automatically.
    ctx.writes.push(WriteOp::CreateType {
        name: name.to_string(),
        prev: None,
    });
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
        AlterDomainAction::AddConstraint { name: cname, expr } => {
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
            dom.checks.push(crate::sql::CheckDef {
                name: cname,
                expr: expr.clone(),
                not_valid: false,
                kind: crate::sql::CheckKind::Check,
            });
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
