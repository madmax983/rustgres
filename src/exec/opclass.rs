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

/// v2.48: OPERATOR FAMILY statements are catalog no-ops — families
/// are not modeled (opclasses record no family membership, and
/// FK/index equality resolution is type-driven), so CREATE / DROP /
/// ALTER ... ADD|DROP only carry their PG command tags.
/// v2.48: access methods known to the operator-family DDL
/// validation (PG19 pg_am). Anything else is 42704, checked before
/// family existence — including under DROP ... IF EXISTS.
pub(crate) fn opfamily_method_ok(method: &str) -> bool {
    matches!(
        method,
        "btree" | "hash" | "gist" | "gin" | "spgist" | "brin"
    )
}

/// Find a registered family by (name as written, method).
pub(crate) fn find_opfamily(
    db: &Database,
    name: &str,
    method: &str,
) -> Option<crate::storage::OpFamilyDef> {
    db.opfamilies
        .iter()
        .find(|f| f.name == name && f.method == method)
        .cloned()
}

/// Register or replace a family, logging the undo op.
fn set_opfamily(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    def: Option<crate::storage::OpFamilyDef>,
    name: &str,
    method: &str,
) {
    let prev = find_opfamily(&eng.db, name, method);
    eng.db
        .opfamilies
        .retain(|f| !(f.name == name && f.method == method));
    if let Some(d) = def {
        eng.db.opfamilies.push(d);
    }
    ctx.writes.push(WriteOp::OpFamilySet {
        name: name.to_string(),
        method: method.to_string(),
        prev,
    });
}

/// v2.48: CREATE OPERATOR FAMILY (PG19 CreateOpFamily). The access
/// method must exist (42704); a duplicate (name, method) is 42710.
pub(crate) fn exec_create_operator_family(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    method: &str,
) -> Result<ExecResult, ExecError> {
    let method_lc = method.to_ascii_lowercase();
    if !opfamily_method_ok(&method_lc) {
        return Err(exec_err(
            "42704",
            format!("access method \"{}\" does not exist", method),
        ));
    }
    if find_opfamily(&eng.db, name, &method_lc).is_some() {
        return Err(exec_err(
            "42710",
            format!(
                "operator family \"{}\" for access method \"{}\" already exists",
                name, method_lc
            ),
        ));
    }
    set_opfamily(
        eng,
        ctx,
        Some(crate::storage::OpFamilyDef {
            name: name.to_string(),
            method: method_lc.clone(),
            owner: ctx.role.to_string(),
            operators: Vec::new(),
            functions: Vec::new(),
            classes: Vec::new(),
        }),
        name,
        &method_lc,
    );
    Ok(ExecResult::Command {
        tag: "CREATE OPERATOR FAMILY".to_string(),
    })
}

/// v2.48: DROP OPERATOR FAMILY (PG19 RemoveOpFamily). The access
/// method is validated even under IF EXISTS; a missing family is
/// 42704 (or a notice under IF EXISTS); a family still used by an
/// operator class blocks RESTRICT with 2BP01 and drops with the
/// class under CASCADE.
pub(crate) fn exec_drop_operator_family(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    method: &str,
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    let method_lc = method.to_ascii_lowercase();
    if !opfamily_method_ok(&method_lc) {
        return Err(exec_err(
            "42704",
            format!("access method \"{}\" does not exist", method),
        ));
    }
    let Some(fam) = find_opfamily(&eng.db, name, &method_lc) else {
        if if_exists {
            ctx.notices.push(format!(
                "operator family \"{}\" for access method \"{}\" does not exist, skipping",
                name, method_lc
            ));
            return Ok(ExecResult::Command {
                tag: "DROP OPERATOR FAMILY".to_string(),
            });
        }
        return Err(exec_err(
            "42704",
            format!(
                "operator family \"{}\" for access method \"{}\" does not exist",
                name, method_lc
            ),
        ));
    };
    if !fam.classes.is_empty() && !cascade {
        return Err(exec_err(
            "2BP01",
            format!(
                "cannot drop operator family \"{}\" for access method \"{}\" because operator class \"{}\" depends on it",
                name, method_lc, fam.classes[0]
            ),
        ));
    }
    if cascade {
        for cname in &fam.classes {
            if let Some(prev) = find_opclass(&eng.db, cname, &method_lc) {
                eng.db
                    .opclasses
                    .retain(|c| !(c.name == prev.name && c.method == method_lc));
                ctx.writes.push(WriteOp::OpClassSet {
                    name: prev.name.clone(),
                    method: method_lc.clone(),
                    prev: Some(prev),
                });
            }
        }
    }
    set_opfamily(eng, ctx, None, name, &method_lc);
    Ok(ExecResult::Command {
        tag: "DROP OPERATOR FAMILY".to_string(),
    })
}

/// v2.48: ALTER OPERATOR FAMILY ... ADD|DROP items (PG19
/// AlterOpFamilyStmt + opclass.c validation). ADD/DROP requires
/// superuser. Strategy/support ranges are per access method;
/// duplicate items (within the statement or against the family)
/// are 42710; dropping an absent item is 42704.
pub(crate) fn exec_alter_operator_family_items(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    method: &str,
    drop: bool,
    items: &[crate::sql::OpFamilyItem],
) -> Result<ExecResult, ExecError> {
    let method_lc = method.to_ascii_lowercase();
    if !opfamily_method_ok(&method_lc) {
        return Err(exec_err(
            "42704",
            format!("access method \"{}\" does not exist", method),
        ));
    }
    let Some(mut fam) = find_opfamily(&eng.db, name, &method_lc) else {
        return Err(exec_err(
            "42704",
            format!(
                "operator family \"{}\" for access method \"{}\" does not exist",
                name, method_lc
            ),
        ));
    };
    if !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Err(exec_err(
            "42501",
            "permission denied: must be superuser to alter an operator family".to_string(),
        ));
    }
    // Per-method maxima (PG19 opclass.c / the AM's pg_amproc
    // numbering): btree strategies 1-5, support 1-6 (per the
    // alter_generic.sql commentary); hash strategy 1, support 1-2;
    // gist/gin/spgist/brin use their documented ceilings.
    let (max_op, max_fn) = match method_lc.as_str() {
        "btree" => (5u32, 6u32),
        "hash" => (1, 2),
        "gist" => (20, 11),
        "gin" => (6, 6),
        "spgist" => (5, 5),
        _ => (10, 10), // brin
    };
    let norm = |s: &str| -> String { s.rsplit('.').next().unwrap_or(s).to_ascii_lowercase() };
    let ty = |s: &str| -> String { crate::exec::canon_func_type_name(s).to_string() };
    let mut new_ops: Vec<(u32, String, String)> = Vec::new();
    let mut new_fns: Vec<(u32, String, Vec<String>)> = Vec::new();
    for item in items {
        match item {
            crate::sql::OpFamilyItem::Operator {
                strategy,
                name: oname,
                types,
                purpose,
            } => {
                if *strategy < 1 || *strategy > max_op {
                    return Err(exec_err(
                        "42P16",
                        format!(
                            "invalid strategy number {}: must be between 1 and {}",
                            strategy, max_op
                        ),
                    ));
                }
                if drop {
                    if types.is_empty() || types.len() > 2 {
                        return Err(exec_err(
                            "42P16",
                            "operator argument types must be specified as (type, type)".to_string(),
                        ));
                    }
                    let (l, r) = if types.len() == 2 {
                        (ty(&types[0]), ty(&types[1]))
                    } else {
                        (ty(&types[0]), ty(&types[0]))
                    };
                    let key = (*strategy, l, r);
                    if !fam.operators.contains(&key) {
                        return Err(exec_err(
                            "42704",
                            "operator does not exist in operator family".to_string(),
                        ));
                    }
                    fam.operators.retain(|o| o != &key);
                } else {
                    if types.len() != 2 {
                        return Err(exec_err(
                            "42P16",
                            "operator argument types are required for ADD OPERATOR".to_string(),
                        ));
                    }
                    if let Some(p) = purpose {
                        if p != "search" && method_lc != "gist" {
                            return Err(exec_err(
                                "0A000",
                                "FOR ORDER BY is only supported for GiST operator families"
                                    .to_string(),
                            ));
                        }
                    }
                    let key = (*strategy, ty(&types[0]), ty(&types[1]));
                    if fam.operators.contains(&key) || new_ops.contains(&key) {
                        return Err(exec_err(
                            "42710",
                            "operator already exists in operator family".to_string(),
                        ));
                    }
                    let _ = oname;
                    new_ops.push(key);
                }
            }
            crate::sql::OpFamilyItem::Function {
                number,
                lead_types,
                name: fname,
                types,
            } => {
                if *number < 1 || *number > max_fn {
                    return Err(exec_err(
                        "42P16",
                        format!(
                            "invalid function number {}: must be between 1 and {}",
                            number, max_fn
                        ),
                    ));
                }
                if drop {
                    let key_types: Vec<String> = types.iter().map(|t| ty(t)).collect();
                    let pos = fam
                        .functions
                        .iter()
                        .position(|(n, _, ts)| n == number && *ts == key_types);
                    match pos {
                        Some(i) => {
                            fam.functions.remove(i);
                        }
                        None => {
                            return Err(exec_err(
                                "42704",
                                "function does not exist in operator family".to_string(),
                            ));
                        }
                    }
                } else {
                    let fname = fname.as_deref().unwrap_or("");
                    let base = norm(fname);
                    let arg_tys: Vec<String> = types.iter().map(|t| ty(t)).collect();
                    // The function must exist with the declared
                    // signature (user registry or builtin).
                    let sig = find_function_by_signature(eng, &base, types)
                        .or_else(|| find_function_by_signature(eng, &base, &arg_tys));
                    // pg_proc's built-in comparison support
                    // functions (btint42cmp, int48cmp, ...) are not
                    // in the SQL builtin registry but are valid
                    // opfamily members returning integer.
                    let pg_proc_cmp = base.ends_with("cmp")
                        && (base.starts_with("bt") || base.starts_with("int"));
                    let ret_ty: Option<String> = match &sig {
                        Some(f) => Some(f.ret_type.clone()),
                        None if crate::sql::is_builtin_fn(&base) => None,
                        None if pg_proc_cmp => Some("integer".to_string()),
                        None => {
                            return Err(exec_err(
                                "42883",
                                format!("function \"{}\" does not exist", fname),
                            ));
                        }
                    };
                    // Per-AM signature rules (PG19 opclass.c):
                    // btree comparison (1) takes two args and
                    // returns integer; hash (1) takes one arg and
                    // returns integer; gist cross-type functions
                    // need the leading (types) list.
                    if method_lc == "btree" && *number == 1 {
                        if types.len() != 2 {
                            return Err(exec_err(
                                "42P16",
                                "btree comparison functions must have two arguments".to_string(),
                            ));
                        }
                        if let Some(r) = &ret_ty {
                            if crate::exec::canon_func_type_name(r) != "integer" {
                                return Err(exec_err(
                                    "42P16",
                                    "btree comparison functions must return integer".to_string(),
                                ));
                            }
                        }
                    }
                    if method_lc == "hash" && *number == 1 {
                        if types.len() != 1 {
                            return Err(exec_err(
                                "42P16",
                                "hash functions must have one argument".to_string(),
                            ));
                        }
                        if let Some(r) = &ret_ty {
                            if crate::exec::canon_func_type_name(r) != "integer" {
                                return Err(exec_err(
                                    "42P16",
                                    "hash functions must return integer".to_string(),
                                ));
                            }
                        }
                    }
                    if method_lc == "gist" && lead_types.is_none() && types.len() > 1 {
                        return Err(exec_err(
                            "42P16",
                            "function argument types must be specified with (types) for cross-type GiST support functions".to_string(),
                        ));
                    }
                    // v2.48: btree support 5 is the opclass options
                    // parsing function: its signature must be
                    // (internal) RETURNS void (alter_generic.sql
                    // alt_opf19); anything else is 42P16.
                    if method_lc == "btree" && *number == 5 {
                        let sig_ok = types.len() == 1
                            && crate::exec::canon_func_type_name(&types[0]) == "internal";
                        if !sig_ok {
                            return Err(exec_err(
                                "42P16",
                                "invalid operator class options parsing function".to_string(),
                            ));
                        }
                    }
                    // The registration key carries the ASSOCIATED
                    // types: the leading (types) list when spelled
                    // (one type means left = right = that type),
                    // else the function's argument types (PG19
                    // pg_amproc is keyed by left/right type).
                    let assoc_tys: Vec<String> = match lead_types {
                        Some(lt) if lt.len() == 1 => {
                            let t = crate::exec::canon_func_type_name(&lt[0]).to_string();
                            vec![t.clone(), t]
                        }
                        Some(lt) => lt
                            .iter()
                            .map(|t| crate::exec::canon_func_type_name(t).to_string())
                            .collect(),
                        None => arg_tys.clone(),
                    };
                    let key = (*number, base, assoc_tys);
                    if fam
                        .functions
                        .iter()
                        .any(|(n, nm, ts)| *n == key.0 && *nm == key.1 && *ts == key.2)
                        || new_fns.contains(&key)
                    {
                        return Err(exec_err(
                            "42710",
                            "function already exists in operator family".to_string(),
                        ));
                    }
                    new_fns.push(key);
                }
            }
        }
    }
    fam.operators.extend(new_ops);
    fam.functions.extend(new_fns);
    set_opfamily(eng, ctx, Some(fam), name, &method_lc);
    Ok(ExecResult::Command {
        tag: "ALTER OPERATOR FAMILY".to_string(),
    })
}

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
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_operator_class(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    is_default: bool,
    for_type: &str,
    method: &str,
    family: Option<&str>,
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
    // v2.48: a FAMILY clause registers the family if absent (PG19
    // DefineOpClass creates it implicitly) and records the class as
    // a member for DROP OPERATOR FAMILY dependency checks.
    if let Some(fam_name) = family {
        let mut fam =
            find_opfamily(&eng.db, fam_name, &method_lc).unwrap_or(crate::storage::OpFamilyDef {
                name: fam_name.to_string(),
                method: method_lc.clone(),
                owner: ctx.role.to_string(),
                operators: Vec::new(),
                functions: Vec::new(),
                classes: Vec::new(),
            });
        if !fam.classes.iter().any(|c| c == name) {
            fam.classes.push(name.to_string());
        }
        set_opfamily(eng, ctx, Some(fam), fam_name, &method_lc);
    }
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
