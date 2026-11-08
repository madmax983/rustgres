// v2.23: the operator-class registry (CREATE / DROP / ALTER
// OPERATOR CLASS) and the partition-key opclass resolution used
// by HASH routing. Semantics grounded in PG19
// commands/opclasscmds.c (DefineOpClass / RemoveOpClass) and
// catalog/pg_opclass: a class is identified by (name, access
// method); the name may be schema-qualified. Only FUNCTION 2
// (the extended hash function) is wired to execution; OPERATOR 1
// is recorded for catalog fidelity.
use super::*;
use crate::storage::OpClassDef;

/// Find a class by the name as referenced (possibly unqualified)
/// and access method. An unqualified reference matches a class in
/// any schema (the engine's single-namespace approximation, like
/// its table registry); a qualified reference must match exactly.
pub(crate) fn find_opclass(db: &Database, name: &str, method: &str) -> Option<OpClassDef> {
    let qualified = name.contains('.');
    db.opclasses
        .iter()
        .find(|c| {
            c.method.eq_ignore_ascii_case(method)
                && (c.name == name || (!qualified && c.base_name() == name))
        })
        .cloned()
}

/// PG19's type display for opclass validation messages: the
/// canonical short name of a key column type.
pub(crate) fn key_type_display(ty: &ColType) -> String {
    match ty {
        ColType::SmallInt => "smallint".to_string(),
        ColType::Int => "integer".to_string(),
        ColType::BigInt => "bigint".to_string(),
        ColType::Float4 => "real".to_string(),
        ColType::Float => "double precision".to_string(),
        ColType::Numeric(_) => "numeric".to_string(),
        ColType::Text => "text".to_string(),
        ColType::Varchar(_) => "character varying".to_string(),
        ColType::Char(_) => "character".to_string(),
        ColType::Bool => "boolean".to_string(),
        ColType::Date => "date".to_string(),
        ColType::Timestamp => "timestamp without time zone".to_string(),
        ColType::Timestamptz => "timestamp with time zone".to_string(),
        ColType::Time => "time without time zone".to_string(),
        ColType::TimeTz => "time with time zone".to_string(),
        ColType::Interval => "interval".to_string(),
        ColType::Bytea => "bytea".to_string(),
        ColType::Uuid => "uuid".to_string(),
        ColType::SingleChar => "\"char\"".to_string(),
        ColType::Name => "name".to_string(),
        other => format!("{:?}", other).to_lowercase(),
    }
}

/// v2.23: validate one partition key's opclass at CREATE time.
/// `ty` is the key column's type for plain-column keys (`None`
/// for expression keys — PG resolves the class against the
/// expression's result type; the engine checks name existence
/// only there). Unknown class -> 42704 (PG19: "operator class
/// \"%s\" does not exist for access method \"%s\""); a class
/// defined FOR a different type than the key's -> 42883
/// (PG19 raises it at parse analysis as an opclass/type
/// mismatch; no census statement pins the exact code).
pub(crate) fn validate_key_opclass(
    eng: &Engine,
    pk: &crate::storage::PartKey,
    ty: Option<ColType>,
) -> Result<(), ExecError> {
    let Some(ocname) = &pk.opclass else {
        return Ok(());
    };
    let class = find_opclass(&eng.db, ocname, "hash").ok_or_else(|| {
        exec_err(
            "42704",
            format!(
                "operator class \"{}\" does not exist for access method \"hash\"",
                ocname
            ),
        )
    })?;
    if let Some(ty) = ty {
        let want = key_type_display(&ty);
        if canon_func_type_name(&class.for_type) != canon_func_type_name(&want)
            && class.for_type != want
        {
            return Err(exec_err(
                "42883",
                format!(
                    "operator class \"{}\" is for type {}, not {}",
                    ocname, class.for_type, want
                ),
            ));
        }
    }
    Ok(())
}

/// v2.23: CREATE OPERATOR CLASS. PG19 checks: the type must
/// resolve (42704 via the type catalog), the hash FUNCTION 2
/// must exist with the declared signature (42883), a duplicate
/// (name, method) is 42710, and a second DEFAULT class for the
/// same (type, method) is 42710.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_operator_class(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    is_default: bool,
    for_type: &str,
    method: &str,
    eq_operator: Option<&str>,
    eq_leftarg: Option<&str>,
    eq_rightarg: Option<&str>,
    hash_function: Option<&str>,
    hash_arg_types: &[String],
) -> Result<ExecResult, ExecError> {
    let method_lc = method.to_ascii_lowercase();
    if method_lc != "hash" && method_lc != "btree" {
        return Err(exec_err(
            "42704",
            format!("access method \"{}\" does not exist", method),
        ));
    }
    // The FOR TYPE must resolve (builtin, domain, composite, or a
    // table rowtype — alter1.ctype is the census case).
    resolve_func_type_name(eng, ctx.snap, ctx.own, ctx.session, for_type)?;
    // FUNCTION 2 must name an existing function with the declared
    // signature (PG19 looks the procedure up in DefineOpClass).
    if let Some(fname) = hash_function {
        let base = fname.rsplit('.').next().unwrap_or(fname);
        if find_function_by_signature(eng, base, hash_arg_types).is_none() {
            return Err(exec_err(
                "42883",
                format!("function \"{}\" does not exist", fname),
            ));
        }
    }
    if find_opclass(&eng.db, name, &method_lc).is_some_and(|c| c.name == name) {
        return Err(exec_err(
            "42710",
            format!(
                "operator class \"{}\" for access method \"{}\" already exists",
                name, method_lc
            ),
        ));
    }
    if is_default
        && eng.db.opclasses.iter().any(|c| {
            c.method == method_lc
                && c.is_default
                && canon_func_type_name(&c.for_type) == canon_func_type_name(for_type)
        })
    {
        return Err(exec_err(
            "42710",
            format!(
                "could not make operator class \"{}\" be default for type {}",
                name, for_type
            ),
        ));
    }
    let (schema, _) = match name.rsplit_once('.') {
        Some((s, b)) => (s.to_string(), b.to_string()),
        None => ("public".to_string(), name.to_string()),
    };
    let def = OpClassDef {
        name: name.to_string(),
        schema,
        for_type: for_type.to_string(),
        method: method_lc.clone(),
        is_default,
        eq_operator: eq_operator.map(|s| s.to_string()),
        eq_leftarg: eq_leftarg.map(|s| s.to_string()),
        eq_rightarg: eq_rightarg.map(|s| s.to_string()),
        hash_function: hash_function.map(|s| s.to_string()),
        hash_arg_types: hash_arg_types.to_vec(),
        owner: ctx.role.to_string(),
    };
    eng.db.opclasses.push(def);
    ctx.writes.push(WriteOp::OpClassSet {
        name: name.to_string(),
        method: method_lc,
        prev: None,
    });
    Ok(ExecResult::Command {
        tag: "CREATE OPERATOR CLASS".to_string(),
    })
}

/// v2.23: DROP OPERATOR CLASS [IF EXISTS] name USING method.
/// Missing class -> 42704 (PG19 RemoveOpClass), or a notice with
/// IF EXISTS. CASCADE/RESTRICT are not distinguished (no
/// dependency tracking, the operators precedent).
pub(crate) fn exec_drop_operator_class(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    method: &str,
    if_exists: bool,
    _cascade: bool,
) -> Result<ExecResult, ExecError> {
    let method_lc = method.to_ascii_lowercase();
    // PG19 resolves the access method BEFORE the class lookup —
    // an unknown AM errors even under IF EXISTS (opclasscmds.c,
    // ERRCODE_UNDEFINED_OBJECT).
    if !matches!(
        method_lc.as_str(),
        "btree" | "hash" | "gist" | "gin" | "spgist" | "brin"
    ) {
        return Err(exec_err(
            "42704",
            format!("access method \"{}\" does not exist", method_lc),
        ));
    }
    match find_opclass(&eng.db, name, &method_lc) {
        None => {
            if !if_exists {
                return Err(exec_err(
                    "42704",
                    format!(
                        "operator class \"{}\" does not exist for access method \"{}\"",
                        name, method_lc
                    ),
                ));
            }
            ctx.notices.push(format!(
                "operator class \"{}\" does not exist for access method \"{}\", skipping",
                name, method_lc
            ));
        }
        Some(def) => {
            eng.db
                .opclasses
                .retain(|c| !(c.name == def.name && c.method == def.method));
            ctx.writes.push(WriteOp::OpClassSet {
                name: def.name.clone(),
                method: def.method.clone(),
                prev: Some(def),
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP OPERATOR CLASS".to_string(),
    })
}

/// v2.23: ALTER OPERATOR CLASS/FAMILY name USING method SET
/// SCHEMA s. PG19 moves the class (and its family) to the new
/// schema; the stored qualified name is rewritten. A missing
/// class is 42704; an unknown target schema is 3F000.
pub(crate) fn exec_alter_operator_class_schema(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    method: &str,
    new_schema: &str,
) -> Result<ExecResult, ExecError> {
    let method_lc = method.to_ascii_lowercase();
    let def = find_opclass(&eng.db, name, &method_lc).ok_or_else(|| {
        exec_err(
            "42704",
            format!(
                "operator class \"{}\" does not exist for access method \"{}\"",
                name, method_lc
            ),
        )
    })?;
    if !eng.db.schemas.iter().any(|s| s == new_schema) {
        return Err(exec_err(
            "3F000",
            format!("schema \"{}\" does not exist", new_schema),
        ));
    }
    let mut next = def.clone();
    next.schema = new_schema.to_string();
    next.name = format!("{}.{}", new_schema, def.base_name());
    eng.db
        .opclasses
        .retain(|c| !(c.name == def.name && c.method == def.method));
    eng.db.opclasses.push(next.clone());
    ctx.writes.push(WriteOp::OpClassSet {
        name: def.name.clone(),
        method: def.method.clone(),
        prev: Some(def),
    });
    // The move itself is a second undo step: undoing the ALTER
    // must remove the new-name entry too. Record it as a set with
    // prev = the moved entry under its new identity; the undo
    // chain applies in reverse, restoring the original.
    ctx.writes.push(WriteOp::OpClassSet {
        name: next.name.clone(),
        method: next.method.clone(),
        prev: None,
    });
    Ok(ExecResult::Command {
        tag: "ALTER OPERATOR CLASS".to_string(),
    })
}
