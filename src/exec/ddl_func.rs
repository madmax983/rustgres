// v1.78 mechanical split: moved verbatim from src/exec.rs (61481-63192).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

/// DROP FUNCTION / CREATE OPERATOR), plus the call paths. SQL-language
/// bodies are parsed once at CREATE time; named argument references
/// (`t.col` / `t` where `t` is an argument name) are rewritten to
/// positional `$n` parameters at CREATE time, then bound per call via
/// the existing `subst_params` machinery.

/// v1.09: `ALTER FUNCTION name(argtypes) {VOLATILE|STABLE|IMMUTABLE}`
/// (PG19 AlterFunctionStmt, volatility action only). Finds the overload
/// by signature (like DROP FUNCTION) and updates its volatility in
/// place. The function catalog is checkpointed wholesale, so the change
/// persists like CREATE FUNCTION.
pub(crate) fn exec_alter_function(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    arg_types: &[String],
    volatility: crate::sql::FuncVolatility,
    owner: Option<&str>,
    security: Option<bool>,
) -> Result<ExecResult, ExecError> {
    // v1.96: ALTER FUNCTION ... OWNER TO — reassign fn_owners
    // (must be current owner or superuser; new owner must exist).
    if let Some(new_owner) = owner {
        if !eng.db.functions.contains_key(name) && !eng.db.aggregates.contains_key(name) {
            return Err(exec_err(
                "42883",
                format!("function {}() does not exist", name),
            ));
        }
        if eng.db.find_role(new_owner, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", new_owner),
            ));
        }
        let cur = eng
            .db
            .fn_owners
            .get(name)
            .cloned()
            .unwrap_or_else(|| "postgres".to_string());
        let is_super = eng
            .db
            .find_role(&ctx.role, ctx.snap, ctx.own)
            .map(|r| r.superuser)
            .unwrap_or(false);
        // v1.96 finishing pass: PG19's owner-change rule (same as
        // tables): the executor must hold the current owner's
        // privileges (inheritance counts) AND be able to SET ROLE to
        // the new owner — a plain non-member cannot give ownership
        // away (alter_generic: OWNER TO user2 fails "no role
        // membership", OWNER TO user3 succeeds via IN ROLE).
        let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
        if !is_super && !closure.iter().any(|r| r == &cur) {
            return Err(exec_err(
                "42501",
                format!("must be owner of function {}", name),
            ));
        }
        if !is_super
            && ctx.role != new_owner
            && !crate::storage::member_can_set_role(&eng.db, ctx.role, new_owner, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!(
                    "permission denied to change owner of function {}: must be able to SET ROLE to the new owner",
                    name
                ),
            ));
        }
        let prev_fn = eng.db.fn_acls.clone();
        let prev_type = eng.db.type_acls.clone();
        let prev_lang = eng.db.lang_acls.clone();
        let prev_fn_owners = eng.db.fn_owners.clone();
        let prev_type_owners = eng.db.type_owners.clone();
        eng.db
            .fn_owners
            .insert(name.to_string(), new_owner.to_string());
        ctx.writes.push(WriteOp::FnTypeAcls {
            prev_fn,
            prev_type,
            prev_lang,
            prev_fn_owners,
            prev_type_owners,
        });
        return Ok(ExecResult::Command {
            tag: "ALTER FUNCTION".to_string(),
        });
    }
    let overloads = eng
        .db
        .functions
        .get_mut(name)
        .ok_or_else(|| exec_err("42883", format!("function {}() does not exist", name)))?;
    let slot = overloads
        .iter_mut()
        .find(|f| {
            f.arg_types.len() == arg_types.len()
                && f.arg_types
                    .iter()
                    .zip(arg_types.iter())
                    .all(|(a, b)| canon_func_type_name(a) == canon_func_type_name(b))
        })
        .ok_or_else(|| exec_err("42883", format!("function {}() does not exist", name)))?;
    // v1.96 finishing pass: PG19 distinguishes routines by kind —
    // ALTER FUNCTION on a procedure is 42809.
    if slot.is_procedure {
        return Err(exec_err(
            "42809",
            format!("{}({}) is not a function", name, arg_types.join(", ")),
        ));
    }
    // v2.09: a SECURITY-only ALTER passes the Volatile default from
    // the parser; it must not clobber a stored STABLE/IMMUTABLE.
    if security.is_none() {
        slot.volatility = volatility;
    }
    // v2.09: SECURITY DEFINER/INVOKER flips the stored flag (the flag
    // already exists on FuncDef from CREATE; only the ALTER action
    // was missing).
    if let Some(definer) = security {
        slot.security_definer = definer;
    }
    Ok(ExecResult::Command {
        tag: "ALTER FUNCTION".to_string(),
    })
}

/// v0.86: canonicalize a type name for function/operator signature
/// matching: builtin aliases fold together (`int8`/`bigint`,
/// `int4`/`int`/`integer`, `bool`/`boolean`, ...); anything else
/// (named composites, domains, table rowtypes) compares by name.
pub(crate) fn canon_func_type_name(name: &str) -> &str {
    match name {
        "int8" | "bigint" => "bigint",
        "int4" | "int" | "integer" => "integer",
        "int2" | "smallint" => "smallint",
        "bool" | "boolean" => "boolean",
        "float8" | "double precision" => "double precision",
        "float4" | "real" => "real",
        "varchar" | "character varying" => "varchar",
        "timestamp" | "timestamp without time zone" => "timestamp",
        "timestamptz" | "timestamp with time zone" => "timestamptz",
        _ => name,
    }
}

/// v0.86: resolve a function signature type name against builtins, the
/// named-type catalog, and table rowtypes (every table has a rowtype,
/// like PG19). Returns the ColType; named composites/domains/tables
/// resolve to `ColType::Composite` (the name itself disambiguates).
pub(crate) fn resolve_func_type_name(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    name: &str,
) -> Result<ColType, ExecError> {
    if let Ok(ct) = crate::sql::coltype_by_name(name) {
        return Ok(ct);
    }
    // v1.38: `cstring` is PG19's C-string pseudo-type (the argument
    // type of internal input functions like `int4in`). rustgres has no
    // Cstring value — internal functions receive the text directly —
    // so it resolves to the defer-triggering `Composite` placeholder,
    // like shell types below.
    if name.eq_ignore_ascii_case("cstring") {
        return Ok(ColType::Composite);
    }
    // v2.12: `refcursor` is a portal name held as text (PG19: the
    // refcursor type's base representation is a CString name).
    if name.eq_ignore_ascii_case("refcursor") {
        return Ok(ColType::Text);
    }
    // v1.96: `void` is PG19's return type for procedures and
    // no-result functions. It resolves to the defer-triggering
    // Composite placeholder like shell types: FuncDef keeps the
    // "void" string, and call sites special-case it.
    if name.eq_ignore_ascii_case("void") {
        return Ok(ColType::Composite);
    }
    // v2.04: `record` is PG19's polymorphic pseudo-type (the return
    // of RETURNS TABLE functions) — same defer placeholder.
    if name.eq_ignore_ascii_case("record") {
        return Ok(ColType::Composite);
    }
    // v2.04: array types (`text[]`, `int[][]`) exist for every
    // element type in PG; resolve the element and defer to the
    // placeholder (call-time coercion is name-based).
    if let Some(elem) = name.strip_suffix("[]") {
        let elem = elem.trim();
        if !elem.is_empty() {
            resolve_func_type_name(eng, snap, own, session, elem)?;
            return Ok(ColType::Composite);
        }
    }
    if let Some(st) = eng.db.types.get(name) {
        if st.composite.is_some() {
            return Ok(ColType::Composite);
        }
        if let Some(dom) = &st.domain {
            return Ok(dom.base.clone());
        }
        // Shell/LIKE types: LIKE-completed aliases resolve to their base.
        if let Some(base) = &st.like_base {
            if let Ok(ct) = crate::sql::coltype_by_name(base) {
                return Ok(ct);
            }
        }
        // v1.38: a bare shell type (like_base/composite/domain all
        // None) is still a valid function signature type in PG19 — the
        // shell exists precisely so later objects can reference the
        // not-yet-defined type. `ColType::Composite` is the placeholder
        // the static SQL-function checks already defer on
        // (`Ok(Composite) | Err(_) => return Ok(())`), and call-time
        // coercion is name-based, so no passing path changes.
        if st.like_base.is_none() && st.composite.is_none() && st.domain.is_none() {
            return Ok(ColType::Composite);
        }
        return Err(exec_err(
            "42704",
            format!("type \"{}\" does not exist", name),
        ));
    }
    // Table rowtype (PG19: every table has a composite rowtype).
    if eng.db.find_table(name, snap, &[own], session).is_some() {
        return Ok(ColType::Composite);
    }
    Err(exec_err(
        "42704",
        format!("type \"{}\" does not exist", name),
    ))
}

/// v1.32: resolve a `LANGUAGE <name>` clause to a `FuncLang` (PG19
/// `CreateFunction` resolves the language before anything else, via
/// proclang.c `get_language_oid`). Unknown languages are 42883
/// `language "%s" does not exist` — PG-verbatim.
pub(crate) fn resolve_func_lang(lang_name: &str) -> Result<crate::sql::FuncLang, ExecError> {
    match lang_name.to_ascii_lowercase().as_str() {
        "sql" => Ok(crate::sql::FuncLang::Sql),
        "internal" => Ok(crate::sql::FuncLang::Internal),
        // v0.97: bounded plpgsql (single-RETURN bodies, see
        // desugar_plpgsql_body); richer bodies take the
        // multi-statement path (v1.01).
        "plpgsql" => Ok(crate::sql::FuncLang::Plpgsql),
        _ => Err(exec_err(
            "42883",
            format!("language \"{}\" does not exist", lang_name),
        )),
    }
}

/// v1.32: the SQLSTATE-42P13 detail phrases below mirror PG19
/// `check_sql_fn_retval` (executor/functions.c); v1.32 rejected
/// non-SELECT body statements with 0A000 since the body executor had
/// no Q-level DML write path — v1.33 adds it (see `QWrite`), so
/// INSERT/UPDATE/DELETE bodies are accepted and only utility/DDL
/// statements stay 0A000.
pub(crate) fn sql_func_body_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Select(_) => "SELECT",
        Stmt::Insert { .. } => "INSERT",
        Stmt::Update { .. } => "UPDATE",
        Stmt::Delete { .. } => "DELETE",
        Stmt::Merge { .. } => "MERGE",
        _ => "utility statement",
    }
}

/// v1.33: PG19 `check_sql_fn_retval` — the final statement must be
/// SELECT or INSERT/UPDATE/DELETE/MERGE with RETURNING, else 42P13
/// with PG's verbatim detail. (MERGE has no engine support; the
/// message is PG's own.)
pub(crate) fn sql_func_final_42p13(ret_type: &str) -> ExecError {
    exec_err_detail(
        "42P13",
        format!(
            "return type mismatch in function declared to return {}",
            ret_type
        ),
        "Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING.",
    )
}

/// v1.34: PG19 `check_sql_fn_retval` / `coerce_fn_result_column`
/// failure — 42P13 with PG's verbatim detail naming the actual column
/// type (`format_type_be` of the final statement's output type).
pub(crate) fn sql_func_coercion_42p13(ret_type: &str, actual_type: &str) -> ExecError {
    exec_err_detail(
        "42P13",
        format!(
            "return type mismatch in function declared to return {}",
            ret_type
        ),
        format!("Actual return type is {}.", actual_type),
    )
}

/// v1.34: a representative non-null value of a column type, used to
/// probe `eval_cast` for a *type-level* cast path (see
/// `can_assignment_coerce`). Each dummy mirrors the runtime
/// representation of its type: `regclass` values are `Text` at runtime
/// (`eval_regclass_cast`), `xid` values are `Int`, and `char(n)`
/// values are blank-padded `BpChar`.
pub(crate) fn dummy_value_of(ty: &ColType) -> Value {
    match ty {
        ColType::SmallInt => Value::SmallInt(1),
        ColType::Int => Value::Int(1),
        ColType::BigInt => Value::BigInt(1),
        ColType::Float4 => Value::Float4(1.0),
        ColType::Float => Value::Float(1.0),
        ColType::Numeric(_) => Value::Numeric(crate::storage::Numeric::new(1, 0)),
        ColType::Char(_) => Value::BpChar("x".into()),
        ColType::Text | ColType::Varchar(_) | ColType::Name | ColType::Json => Value::text("x"),
        // v1.83: a valid canonical jsonb dummy.
        ColType::Jsonb => Value::jsonb("{}"),
        // v1.85: the root path is the honest jsonpath dummy.
        ColType::Jsonpath => Value::jsonpath("$"),
        // v1.88: empty range / multirange dummies.
        ColType::Range(e) => Value::range(super::ranges::empty_range(*e, None)),
        ColType::Multirange(e) => Value::multirange(crate::storage::MultiVal {
            elem: *e,
            name: None,
            ranges: Vec::new(),
        }),
        // v1.94: empty tsvector / tsquery dummies.
        ColType::TsVector => Value::tsvector(crate::storage::TsVectorVal {
            entries: Vec::new(),
        }),
        ColType::TsQuery => Value::tsquery(crate::storage::TsQueryVal { root: None }),
        ColType::SingleChar => Value::SingleChar(b'x'),
        ColType::Bool => Value::Bool(true),
        ColType::Date => Value::Date(1),
        ColType::Timestamp => Value::Timestamp(1),
        ColType::Timestamptz => Value::Timestamptz(1),
        // v1.86: one second past midnight / a one-second interval.
        ColType::Time => Value::Time(1_000_000),
        ColType::TimeTz => Value::TimeTz(1_000_000, 0),
        ColType::Interval => Value::Interval(crate::datetime::Interval {
            months: 0,
            days: 0,
            micros: 1_000_000,
        }),
        ColType::Bytea => Value::Bytea(vec![0]),
        // v1.39: one set bit.
        ColType::Bit => Value::BitString(crate::storage::BitString {
            bitlen: 1,
            bytes: vec![0x80],
        }),
        ColType::Uuid => Value::Uuid([0; 16]),
        ColType::PgLsn => Value::PgLsn(1),
        ColType::Regclass => Value::text("x"),
        ColType::Xid => Value::Int(1),
        ColType::Tid => Value::Tid(0, 0), // v1.40
        ColType::Record | ColType::Composite => Value::Record(vec![]),
        ColType::Array(elem) => Value::Array(Box::new(crate::storage::ArrayVal {
            elem: *elem,
            dims: vec![1],
            lower: vec![1],
            elems: vec![dummy_value_of(&elem_scalar_type(*elem))],
        })),
    }
}

/// v1.34: PG19 `coerce_fn_result_column` (executor/functions.c) — does
/// an *assignment* cast path exist from the source column type to the
/// target type? Probed through the engine's own runtime cast:
/// `eval_cast` reports a missing cast path as 42846 ("cannot cast
/// type X to Y") while value-level failures (22P02 bad input, 22003
/// out of range, ...) mean the path exists — exactly PG's
/// type-level-vs-value-level distinction, which accepts e.g.
/// `RETURNING 'hello'` for `RETURNS integer` at CREATE (the cast then
/// fails at runtime instead). Typmods never block an assignment cast
/// (they constrain the value), so they are normalized away; array
/// casts retype element-wise (PG19), hence the recursion.
pub(crate) fn can_assignment_coerce(src: &ColType, dst: &ColType) -> bool {
    fn norm(t: ColType) -> ColType {
        match t {
            ColType::Numeric(_) => ColType::Numeric(None),
            ColType::Char(_) => ColType::Char(None),
            ColType::Varchar(_) => ColType::Varchar(None),
            _ => t,
        }
    }
    let (src, dst) = (norm(*src), norm(*dst));
    if src == dst {
        return true;
    }
    if let (ColType::Array(se), ColType::Array(de)) = (src, dst) {
        return can_assignment_coerce(&elem_scalar_type(se), &elem_scalar_type(de));
    }
    match eval_cast(&dummy_value_of(&src), dst) {
        Ok(_) => true,
        // 42846 is eval_cast's type-level "no cast path" signal; any
        // other code is a value-level failure of this probe value —
        // the cast path exists.
        Err(e) => e.code != "42846",
    }
}

/// v1.32: parse + validate a SQL-language function body at CREATE time
/// (PG19 `fmgr_sql_validator`: `pg_parse_query` over the whole body,
/// then `check_sql_fn_statements` / `check_sql_fn_retval`). Returns the
/// parsed statements; the executor runs them in order and takes the
/// last statement's result as the return value (PG19 `fmgr_sql`).
/// v1.33: INSERT/UPDATE/DELETE statements are accepted (they execute
/// via the statement's write context, see `QWrite`); utility/DDL
/// statements stay an honest 0A000. Named argument references are
/// rewritten to `$n` per statement, exactly as the old
/// single-statement path did.
pub(crate) fn parse_sql_func_body(
    eng: &Engine,
    snap: &Snapshot,
    xids: &[u64],
    own: u64,
    session: u64,
    body: &str,
    args: &[crate::sql::FuncArg],
    ret_type: &str,
    returns_set: bool,
    table_cols: &[(String, String)],
    // v2.13: run the declared-rowtype final-statement check. True
    // only for LANGUAGE sql bodies — PG19 never applies
    // check_sql_fn_retval to plpgsql, whose single-RETURN bodies
    // also flow through here via the v0.97 desugar (their one
    // expression IS the whole result value).
    check_rowtype: bool,
) -> Result<Vec<Stmt>, ExecError> {
    let arg_names: Vec<Option<String>> = args.iter().map(|a| a.name.clone()).collect();
    let mut stmts = Vec::new();
    for chunk in crate::sql::split_statements(body) {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let mut stmt = crate::sql::parse_statement(chunk).map_err(|e| {
            // PG19 transposes body syntax errors onto the CREATE
            // FUNCTION statement (sql_function_parse_error_callback).
            exec_err(
                e.code,
                format!("syntax error in function body: {}", e.message),
            )
        })?;
        // v1.96: void functions/procedures may carry GRANT/REVOKE
        // statements (PG19 SQL bodies accept utility statements;
        // the privileges suite's sro tests rely on GRANT in a body).
        let is_void = ret_type.eq_ignore_ascii_case("void");
        if !matches!(
            stmt,
            Stmt::Select(_)
                | Stmt::Insert { .. }
                | Stmt::Update { .. }
                | Stmt::Delete { .. }
                | Stmt::Merge { .. }
        ) && !(is_void
            && matches!(
                stmt,
                Stmt::Grant { .. }
                    | Stmt::Revoke { .. }
                    | Stmt::GrantRole { .. }
                    | Stmt::RevokeRole { .. }
            ))
        {
            return Err(exec_err(
                "0A000",
                format!(
                    "{} is not supported in SQL function bodies (only SELECT, INSERT, UPDATE, DELETE)",
                    sql_func_body_kind(&stmt)
                ),
            ));
        }
        rewrite_func_arg_refs(&mut stmt, &arg_names);
        stmts.push(stmt);
    }
    // v1.96: void functions and procedures have no return value to
    // check — PG19 skips check_sql_fn_retval for them entirely.
    if ret_type.eq_ignore_ascii_case("void") {
        return Ok(stmts);
    }
    // PG19 check_sql_fn_retval: an empty body behaves like "the last
    // query rewrote to nothing" — return type mismatch.
    let Some(last) = stmts.last() else {
        return Err(sql_func_final_42p13(ret_type));
    };
    // PG19 check_sql_fn_retval: the final statement must be SELECT or
    // INSERT/UPDATE/DELETE/MERGE with RETURNING. (A final DML without
    // RETURNING returns no rows, so it cannot satisfy a declared
    // return type.)
    let final_returns_rows = match last {
        Stmt::Select(_) => true,
        Stmt::Insert { returning, .. }
        | Stmt::Update { returning, .. }
        | Stmt::Delete { returning, .. }
        | Stmt::Merge { returning, .. } => !returning.is_empty(),
        // Unreachable: non-SELECT/DML statements are rejected above.
        _ => false,
    };
    if !final_returns_rows {
        return Err(sql_func_final_42p13(ret_type));
    }
    // v2.13: PG19 check_sql_fn_retval, composite case — when the
    // expected result tupdesc is known at validate time (OUT/INOUT
    // parameters, RETURNS TABLE columns, or a named composite /
    // table rowtype return), the final statement must match it
    // column-by-column. A bare RECORD return with no outputs has a
    // NULL rettupdesc in fmgr_sql_validator and skips this check
    // entirely (any column count is accepted at CREATE).
    let expected_fields: Option<Vec<String>> = if !check_rowtype {
        None
    } else {
        let mut outs: Vec<String> = args
            .iter()
            .filter(|a| a.mode.is_output())
            .map(|a| a.type_name.clone())
            .collect();
        outs.extend(table_cols.iter().map(|(_, t)| t.clone()));
        if !outs.is_empty() {
            Some(outs)
        } else {
            named_rowtype_field_type_names(eng, snap, own, session, ret_type)
        }
    };
    if let Some(fields) = &expected_fields {
        check_final_against_tupdesc(eng, snap, own, session, last, args, ret_type, fields)?;
    }
    // PG19 check_sql_fn_retval, scalar case: the final statement must
    // return exactly one column. Enforced here only when the column
    // count is syntactically unambiguous (plain SELECT, no `*`,
    // no set operations; DML RETURNING list with no `*`); ambiguous
    // shapes defer to the existing call-time check.
    if !returns_set && expected_fields.is_none() && !ret_type.eq_ignore_ascii_case("record") {
        let unambiguous_count = match last {
            Stmt::Select(sel) => {
                let unambiguous = sel.set_op.is_none()
                    && !sel.items.iter().any(|i| {
                        matches!(
                            i,
                            crate::sql::SelectItem::All | crate::sql::SelectItem::AllOf(_)
                        )
                    });
                unambiguous.then_some(sel.items.len())
            }
            Stmt::Insert { returning, .. }
            | Stmt::Update { returning, .. }
            | Stmt::Delete { returning, .. }
            | Stmt::Merge { returning, .. } => {
                let unambiguous = !returning.iter().any(|i| {
                    matches!(
                        i,
                        crate::sql::SelectItem::All | crate::sql::SelectItem::AllOf(_)
                    )
                });
                unambiguous.then_some(returning.len())
            }
            // Unreachable (see above).
            _ => None,
        };
        if unambiguous_count.is_some_and(|n| n != 1) {
            return Err(sql_func_final_42p13(ret_type));
        }
        // v1.34: PG19 check_sql_fn_retval / coerce_fn_result_column —
        // for a scalar function whose final statement is DML...RETURNING
        // with one unambiguous item, the item's static type must admit
        // an assignment cast to the declared return type (42P13
        // "Actual return type is %s." otherwise). Anything not
        // statically decidable (unknown table, unresolvable expression,
        // named/composite return type) defers to the existing call-time
        // coercion, exactly like the arity check above.
        if let Stmt::Insert {
            table, returning, ..
        }
        | Stmt::Update {
            table, returning, ..
        }
        | Stmt::Delete {
            table, returning, ..
        } = last
        {
            let qual: &str = match last {
                Stmt::Update { alias, .. } | Stmt::Delete { alias, .. } => {
                    alias.as_deref().unwrap_or(table)
                }
                _ => table,
            };
            check_dml_returning_coercible(
                eng, snap, xids, own, session, table, qual, returning, args, ret_type,
            )?;
        }
        // v1.35: PG19 check_sql_fn_retval / coerce_fn_result_column —
        // for a scalar function whose final statement is a plain SELECT
        // with one unambiguous item, the item's static type must admit
        // an assignment cast to the declared return type (42P13
        // "Actual return type is %s." otherwise). Anything not
        // statically decidable (CTEs, subquery FROM items, unresolvable
        // expression, composite return type, $n of unresolvable type)
        // defers to the existing call-time coercion, exactly like the
        // arity check above.
        if let Stmt::Select(sel) = last {
            check_select_final_coercible(eng, snap, own, session, sel, args, ret_type)?;
        }
    }
    Ok(stmts)
}

/// v2.13: PG19 `check_sql_fn_retval` with a known rettupdesc — the
/// detail phrase for a column whose type cannot be coerced to the
/// expected field type.
pub(crate) fn sql_func_column_42p13(
    ret_type: &str,
    actual_type: &str,
    expected_type: &str,
    col: usize,
) -> ExecError {
    exec_err_detail(
        "42P13",
        format!(
            "return type mismatch in function declared to return {}",
            ret_type
        ),
        format!(
            "Final statement returns {} instead of {} at column {}.",
            actual_type, expected_type, col
        ),
    )
}

/// v2.13: the field type names of a named composite / table rowtype
/// return type, for PG19 `check_sql_fn_retval`'s composite case.
/// None for scalar, RECORD, VOID, domain-over-composite (PG treats
/// those through the scalar path), and unresolvable names.
pub(crate) fn named_rowtype_field_type_names(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    ret_type: &str,
) -> Option<Vec<String>> {
    if ret_type.eq_ignore_ascii_case("record") || ret_type.eq_ignore_ascii_case("void") {
        return None;
    }
    if let Ok(ct) = crate::sql::coltype_by_name(ret_type) {
        let _ = ct;
        return None; // builtin scalar
    }
    if let Some(st) = eng.db.types.get(ret_type) {
        if st.domain.is_some() {
            return None;
        }
        if let Some(fields) = &st.composite {
            return Some(
                fields
                    .iter()
                    .map(|(_, ct, _)| ct.sql_name().to_string())
                    .collect(),
            );
        }
        return None;
    }
    if let Some(t) = eng.db.find_table(ret_type, snap, &[own], session) {
        return Some(
            t.columns
                .iter()
                .map(|(_, ct)| ct.sql_name().to_string())
                .collect(),
        );
    }
    None
}

/// v2.13: PG19 `check_sql_fn_retval` composite case (see
/// `parse_sql_func_body`): the final statement's unambiguous column
/// count must equal the expected field count, and each column must
/// admit an assignment cast to its field type. A single
/// composite/record-typed column satisfies any field count (PG's
/// whole-tuple result provision). Fails open whenever a shape is
/// not statically decidable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_final_against_tupdesc(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    last: &Stmt,
    args: &[crate::sql::FuncArg],
    ret_type: &str,
    fields: &[String],
) -> Result<(), ExecError> {
    // Unambiguous output items of the final statement, if any.
    let items: &[crate::sql::SelectItem] = match last {
        Stmt::Select(sel) => {
            if sel.set_op.is_some()
                || sel.items.iter().any(|i| {
                    matches!(
                        i,
                        crate::sql::SelectItem::All | crate::sql::SelectItem::AllOf(_)
                    )
                })
            {
                return Ok(());
            }
            &sel.items
        }
        Stmt::Insert { returning, .. }
        | Stmt::Update { returning, .. }
        | Stmt::Delete { returning, .. }
        | Stmt::Merge { returning, .. } => {
            if returning.iter().any(|i| {
                matches!(
                    i,
                    crate::sql::SelectItem::All | crate::sql::SelectItem::AllOf(_)
                )
            }) {
                return Ok(());
            }
            returning
        }
        _ => return Ok(()),
    };
    if items.len() != fields.len() {
        // PG's whole-tuple provision: one composite/record column
        // is the result, whatever the field count.
        if items.len() == 1 {
            if let Stmt::Select(sel) = last {
                if sel.with.is_empty() {
                    if let crate::sql::SelectItem::Expr { expr, .. } = &items[0] {
                        // A bare $n whose declared type is the result
                        // rowtype itself (or any composite/record) is
                        // the whole tuple (the row_image shape).
                        if let Expr::Param(n) = expr {
                            if let Some(a) = args.get(*n as usize - 1) {
                                let is_tuple = a.type_name.eq_ignore_ascii_case(ret_type)
                                    || a.type_name.eq_ignore_ascii_case("record")
                                    || matches!(
                                        resolve_func_type_name(
                                            eng,
                                            snap,
                                            own,
                                            session,
                                            &a.type_name
                                        ),
                                        Ok(ColType::Composite | ColType::Record)
                                    );
                                if is_tuple {
                                    return Ok(());
                                }
                            }
                        }
                        let mut typed = expr.clone();
                        if substitute_func_params(&mut typed, args, eng, snap, own, session) {
                            if let Ok(schemas) =
                                from_schemas(eng, snap, own, session, &sel.from, &[], &[], &[], &[])
                            {
                                let refs: Vec<&[QCol]> =
                                    schemas.iter().map(Vec::as_slice).collect();
                                if let Ok(ct) =
                                    expr_type(eng, snap, own, session, &refs, &[], &[], &typed)
                                {
                                    if matches!(ct, ColType::Composite | ColType::Record) {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let detail = if items.len() > fields.len() {
            "Final statement returns too many columns."
        } else {
            "Final statement returns too few columns."
        };
        return Err(exec_err_detail(
            "42P13",
            format!(
                "return type mismatch in function declared to return {}",
                ret_type
            ),
            detail,
        ));
    }
    // Per-column assignment coercion, SELECT finals only (DML
    // RETURNING type checks defer to call time, as before).
    let Stmt::Select(sel) = last else {
        return Ok(());
    };
    if !sel.with.is_empty() {
        return Ok(());
    }
    let schemas = match from_schemas(eng, snap, own, session, &sel.from, &[], &[], &[], &[]) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let refs: Vec<&[QCol]> = schemas.iter().map(Vec::as_slice).collect();
    for (i, item) in items.iter().enumerate() {
        let crate::sql::SelectItem::Expr { expr, .. } = item else {
            continue;
        };
        let dst = match resolve_func_type_name(eng, snap, own, session, &fields[i]) {
            Ok(crate::storage::ColType::Composite) | Err(_) => continue,
            Ok(ct) => ct,
        };
        let mut typed = expr.clone();
        if !substitute_func_params(&mut typed, args, eng, snap, own, session) {
            continue;
        }
        let src = match expr_type(eng, snap, own, session, &refs, &[], &[], &typed) {
            Ok(ct) => ct,
            Err(_) => continue,
        };
        if matches!(src, ColType::Composite | ColType::Record) {
            continue;
        }
        if !can_assignment_coerce(&src, &dst) {
            return Err(sql_func_column_42p13(
                ret_type,
                &src.sql_name(),
                &dst.sql_name(),
                i + 1,
            ));
        }
    }
    Ok(())
}

/// v1.34: PG19 `check_sql_fn_retval` / `coerce_fn_result_column` for a
/// scalar SQL function's final DML...RETURNING statement (see
/// `parse_sql_func_body`). Fails open (Ok) whenever the coercibility
/// question is not statically decidable — the call-time
/// `coerce_to_type_name` then applies, unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_dml_returning_coercible(
    eng: &Engine,
    snap: &Snapshot,
    xids: &[u64],
    own: u64,
    session: u64,
    table: &str,
    qual: &str,
    returning: &[crate::sql::SelectItem],
    args: &[crate::sql::FuncArg],
    ret_type: &str,
) -> Result<(), ExecError> {
    // A single unambiguous RETURNING item (the caller enforced the
    // arity); `*` shapes defer to the call-time check.
    let [crate::sql::SelectItem::Expr { expr, .. }] = returning else {
        return Ok(());
    };
    // Declared return type: builtin (or domain-over-builtin, which PG
    // checks as its scalar base) or defer. Composite/table rowtypes
    // keep the existing call-time `eval_cast_named` path.
    let dst = match resolve_func_type_name(eng, snap, own, session, ret_type) {
        Ok(crate::storage::ColType::Composite) | Err(_) => return Ok(()),
        Ok(ct) => ct,
    };
    // The single output column's static type. `$n` references (named
    // arguments were rewritten by the caller) take the declared
    // argument type; anything unresolvable defers to call time.
    let src = match expr {
        crate::sql::Expr::Param(n) => {
            let Some(arg) = args.get(*n as usize - 1) else {
                return Ok(());
            };
            match resolve_func_type_name(eng, snap, own, session, &arg.type_name) {
                Ok(crate::storage::ColType::Composite) | Err(_) => return Ok(()),
                Ok(ct) => ct,
            }
        }
        _ => {
            let Some(t) = eng.db.find_table(table, snap, xids, session) else {
                return Ok(());
            };
            let schema: Vec<QCol> = t
                .columns
                .iter()
                .map(|(n, ty)| QCol {
                    qual: qual.to_string(),
                    name: n.clone(),
                    ty: *ty,
                    hidden: false,
                    src_ord: 0,
                })
                .collect();
            let schemas = [schema.as_slice()];
            match expr_type(eng, snap, own, session, &schemas, &[], &[], expr) {
                Ok(ct) => ct,
                Err(_) => return Ok(()),
            }
        }
    };
    if !can_assignment_coerce(&src, &dst) {
        return Err(sql_func_coercion_42p13(ret_type, &src.sql_name()));
    }
    Ok(())
}

/// v1.35: PG19 `check_sql_fn_retval` / `coerce_fn_result_column` for a
/// scalar SQL function's final plain-SELECT statement (see
/// `parse_sql_func_body`). Fails open (Ok) whenever the coercibility
/// question is not statically decidable — the call-time coercion then
/// applies, unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_select_final_coercible(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    sel: &crate::sql::SelectStmt,
    args: &[crate::sql::FuncArg],
    ret_type: &str,
) -> Result<(), ExecError> {
    // One unambiguous output column (the caller enforced the arity):
    // plain SELECT, no set operations, no `*` wildcards. Anything
    // else defers to the call-time check.
    if sel.set_op.is_some() || sel.items.len() != 1 {
        return Ok(());
    }
    let crate::sql::SelectItem::Expr { expr, .. } = &sel.items[0] else {
        return Ok(());
    };
    // Declared return type: builtin (or domain-over-builtin, which PG
    // checks as its scalar base) or defer. Composite/table rowtypes
    // keep the existing call-time `eval_cast_named` path.
    let dst = match resolve_func_type_name(eng, snap, own, session, ret_type) {
        Ok(crate::storage::ColType::Composite) | Err(_) => return Ok(()),
        Ok(ct) => ct,
    };
    // WITH queries need CTE schema bindings the static check does not
    // build; defer those to call time.
    if !sel.with.is_empty() {
        return Ok(());
    }
    // `$n` references (named arguments were rewritten by the caller)
    // take the declared argument type as a typed NULL; anything the
    // substitution cannot type defers to call time.
    let mut typed = expr.clone();
    if !substitute_func_params(&mut typed, args, eng, snap, own, session) {
        return Ok(());
    }
    // The FROM range schemas; anything `from_schemas` cannot build
    // (missing table, untypeable derived table, ...) defers.
    let schemas = match from_schemas(eng, snap, own, session, &sel.from, &[], &[], &[], &[]) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let refs: Vec<&[QCol]> = schemas.iter().map(Vec::as_slice).collect();
    let src = match expr_type(eng, snap, own, session, &refs, &[], &[], &typed) {
        Ok(ct) => ct,
        Err(_) => return Ok(()),
    };
    if !can_assignment_coerce(&src, &dst) {
        return Err(sql_func_coercion_42p13(ret_type, &src.sql_name()));
    }
    Ok(())
}

/// v1.35: replace `Expr::Param(n)` with a typed-NULL cast carrying the
/// declared argument type, so `expr_type` can type a SQL function
/// body's final SELECT (it reports 42P02 for bare params). Returns
/// false — fail open — when a referenced argument's type is not a
/// resolvable builtin scalar. The walk is deliberately partial:
/// variants it does not recurse into keep their `Param`, which makes
/// `expr_type` fail and the check defer; it can only ever
/// under-approximate, never mis-type.
#[allow(clippy::too_many_arguments)]
pub(crate) fn substitute_func_params(
    e: &mut Expr,
    args: &[crate::sql::FuncArg],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    match e {
        Expr::Param(n) => {
            let ct = args
                .get(*n as usize - 1)
                .and_then(|a| resolve_func_type_name(eng, snap, own, session, &a.type_name).ok())
                .filter(|ct| !matches!(ct, crate::storage::ColType::Composite));
            match ct {
                Some(ct) => {
                    *e = Expr::Cast {
                        expr: Box::new(Expr::Literal(crate::sql::Literal::Null)),
                        to: ct,
                        written: None,
                    };
                    true
                }
                None => false,
            }
        }
        Expr::FieldAccess { expr, .. } => {
            substitute_func_params(expr, args, eng, snap, own, session)
        }
        Expr::Arith { left, right, .. } | Expr::Cmp { left, right, .. } => {
            substitute_func_params(left, args, eng, snap, own, session)
                && substitute_func_params(right, args, eng, snap, own, session)
        }
        Expr::And(left, right) | Expr::Or(left, right) | Expr::Concat(left, right) => {
            substitute_func_params(left, args, eng, snap, own, session)
                && substitute_func_params(right, args, eng, snap, own, session)
        }
        Expr::JsonOp { left, right, .. } => {
            substitute_func_params(left, args, eng, snap, own, session)
                && substitute_func_params(right, args, eng, snap, own, session)
        }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::BitNot(inner) => {
            substitute_func_params(inner, args, eng, snap, own, session)
        }
        Expr::IsNull { expr, .. } | Expr::IsBool { expr, .. } => {
            substitute_func_params(expr, args, eng, snap, own, session)
        }
        Expr::IsDistinctFrom { left, right, .. } => {
            substitute_func_params(left, args, eng, snap, own, session)
                && substitute_func_params(right, args, eng, snap, own, session)
        }
        Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => {
            substitute_func_params(expr, args, eng, snap, own, session)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            substitute_func_params(expr, args, eng, snap, own, session)
                && substitute_func_params(pattern, args, eng, snap, own, session)
                && escape
                    .as_mut()
                    .is_none_or(|esc| substitute_func_params(esc, args, eng, snap, own, session))
        }
        Expr::Regex { expr, pattern, .. } => {
            substitute_func_params(expr, args, eng, snap, own, session)
                && substitute_func_params(pattern, args, eng, snap, own, session)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            substitute_func_params(expr, args, eng, snap, own, session)
                && substitute_func_params(low, args, eng, snap, own, session)
                && substitute_func_params(high, args, eng, snap, own, session)
        }
        Expr::Func { args: fargs, .. } => fargs
            .iter_mut()
            .all(|a| substitute_func_params(a, args, eng, snap, own, session)),
        Expr::NamedArg { expr, .. } => substitute_func_params(expr, args, eng, snap, own, session),
        Expr::Extract { from, .. } => substitute_func_params(from, args, eng, snap, own, session),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand
                .as_mut()
                .is_none_or(|op| substitute_func_params(op, args, eng, snap, own, session))
                && whens.iter_mut().all(|(k, v)| {
                    substitute_func_params(k, args, eng, snap, own, session)
                        && substitute_func_params(v, args, eng, snap, own, session)
                })
                && else_
                    .as_mut()
                    .is_none_or(|el| substitute_func_params(el, args, eng, snap, own, session))
        }
        Expr::Row(exprs) => exprs
            .iter_mut()
            .all(|x| substitute_func_params(x, args, eng, snap, own, session)),
        Expr::ArrayCtor { elems, .. } => elems
            .iter_mut()
            .all(|x| substitute_func_params(x, args, eng, snap, own, session)),
        Expr::Subscript { array, indices } => {
            substitute_func_params(array, args, eng, snap, own, session)
                && indices
                    .iter_mut()
                    .all(|i| substitute_func_params(i, args, eng, snap, own, session))
        }
        Expr::Slice { array, bounds } => {
            substitute_func_params(array, args, eng, snap, own, session)
                && bounds.iter_mut().all(|(lo, hi)| {
                    lo.as_mut()
                        .is_none_or(|l| substitute_func_params(l, args, eng, snap, own, session))
                        && hi.as_mut().is_none_or(|h| {
                            substitute_func_params(h, args, eng, snap, own, session)
                        })
                })
        }
        Expr::UserOp { left, right, .. } => {
            substitute_func_params(left, args, eng, snap, own, session)
                && substitute_func_params(right, args, eng, snap, own, session)
        }
        // Subquery forms: substitute the outer expression only, never
        // inside the subquery (its own params belong to it).
        Expr::InSub { expr, .. } => substitute_func_params(expr, args, eng, snap, own, session),
        Expr::Quantified { left, .. } => {
            substitute_func_params(left, args, eng, snap, own, session)
        }
        Expr::Agg { arg, arg2, .. } => {
            arg.as_mut()
                .is_none_or(|a| substitute_func_params(a, args, eng, snap, own, session))
                && arg2
                    .as_mut()
                    .is_none_or(|a| substitute_func_params(a, args, eng, snap, own, session))
        }
        Expr::Window {
            args: wargs,
            partition_by,
            order_by,
            ..
        } => {
            wargs
                .iter_mut()
                .chain(partition_by.iter_mut())
                .all(|a| substitute_func_params(a, args, eng, snap, own, session))
                && order_by
                    .iter_mut()
                    .all(|o| substitute_func_params(&mut o.expr, args, eng, snap, own, session))
        }
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            direct_args
                .iter_mut()
                .all(|a| substitute_func_params(a, args, eng, snap, own, session))
                && within_order_by
                    .iter_mut()
                    .all(|o| substitute_func_params(&mut o.expr, args, eng, snap, own, session))
                && filter
                    .as_mut()
                    .is_none_or(|f| substitute_func_params(f, args, eng, snap, own, session))
        }
        // Anything else keeps its `Param`s; `expr_type` then fails and
        // the check defers to call time (fail open).
        _ => true,
    }
}

/// v0.86: `CREATE [OR REPLACE] FUNCTION`. Validates the signature
/// types resolve (42704), parses the SQL body once (42601 on a bad
/// body), rewrites named argument references to `$n`, and registers
/// the definition transactionally. Duplicate names are 42723 unless
/// OR REPLACE was given.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
/// v2.12 finishing (PG19 pl_gram.y getdiag_target): a GET
/// DIAGNOSTICS assignment target must be a scalar variable; a
/// record/composite-typed variable (DTYPE_REC / DTYPE_ROW in PG)
/// is a CREATE-time syntax error. `rec_names` holds the variable
/// and argument names the caller resolved to record/composite
/// types. Targets may be label-qualified (`outer.x`) or fielded
/// (`r.f`); either end segment naming a record variable fires.
fn check_getdiag_targets(
    stmts: &[crate::sql::PlpgsqlStmt],
    handlers: &[crate::sql::PlpgsqlHandler],
    rec_names: &[String],
) -> Result<(), ExecError> {
    for s in stmts {
        match s {
            crate::sql::PlpgsqlStmt::GetDiagnostics { assigns, .. } => {
                for (t, _) in assigns {
                    // Only a whole-variable target can be a record;
                    // a field target (`rec.field`) denotes a scalar
                    // field datum in PG19 and stays legal.
                    if let crate::sql::PlTarget::Var(name) = t {
                        if rec_names.iter().any(|n| n == name) {
                            return Err(exec_err(
                                "42601",
                                format!("\"{}\" is not a scalar variable", name),
                            ));
                        }
                    }
                }
            }
            crate::sql::PlpgsqlStmt::If {
                branches,
                else_body,
            }
            | crate::sql::PlpgsqlStmt::Case {
                branches,
                else_body,
            } => {
                for (_, b) in branches {
                    check_getdiag_targets(b, &[], rec_names)?;
                }
                check_getdiag_targets(else_body, &[], rec_names)?;
            }
            crate::sql::PlpgsqlStmt::ForQuery { body, .. }
            | crate::sql::PlpgsqlStmt::ForRange { body, .. }
            | crate::sql::PlpgsqlStmt::ForCursor { body, .. }
            | crate::sql::PlpgsqlStmt::While { body, .. }
            | crate::sql::PlpgsqlStmt::Loop { body } => {
                check_getdiag_targets(body, &[], rec_names)?;
            }
            crate::sql::PlpgsqlStmt::Block {
                stmts, handlers, ..
            } => {
                check_getdiag_targets(stmts, handlers, rec_names)?;
            }
            _ => {}
        }
    }
    for h in handlers {
        check_getdiag_targets(&h.stmts, &[], rec_names)?;
    }
    Ok(())
}

pub(crate) fn exec_create_function(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    args: &[crate::sql::FuncArg],
    ret_type: &str,
    returns_set: bool,
    lang_name: &str,
    body: &str,
    or_replace: bool,
    volatility: crate::sql::FuncVolatility,
    strict: bool,
    security_definer: bool,
    procedure: bool,
    table_cols: &[(String, String)],
) -> Result<ExecResult, ExecError> {
    // v1.32: PG19 resolves the language first (before return-type
    // resolution): unknown LANGUAGE is 42883, not a parse error.
    let lang = resolve_func_lang(lang_name)?;
    // v2.12: PG19 VARIADIC rules (functioncmds.c): a VARIADIC
    // parameter must be the last input parameter and its declared
    // type must be an array type.
    for (i, a) in args.iter().enumerate() {
        if a.mode == crate::sql::ArgMode::Variadic {
            if args[i + 1..].iter().any(|x| x.mode.is_input()) {
                return Err(exec_err(
                    "42P13",
                    "VARIADIC parameter must be the last input parameter".to_string(),
                ));
            }
            let t = canon_func_type_name(&a.type_name);
            if !(t.ends_with("[]") || t == "anyarray") {
                return Err(exec_err(
                    "42P13",
                    "VARIADIC parameter must be an array".to_string(),
                ));
            }
        }
    }
    // v1.96 finishing pass: PG19 parameter-default rules
    // (functioncmds.c): only input parameters may have defaults,
    // and once an input parameter has one, every later input
    // parameter must have one too. v2.12: arg modes are parsed, so
    // OUT detection uses the real mode.
    {
        let mut seen_default = false;
        for a in args {
            let is_out = a.mode == crate::sql::ArgMode::Out;
            if is_out && a.default.is_some() {
                return Err(exec_err(
                    "42P13",
                    "only input parameters can have default values".to_string(),
                ));
            }
            if is_out {
                continue;
            }
            if a.default.is_some() {
                seen_default = true;
            } else if seen_default {
                return Err(exec_err(
                    "42P13",
                    "input parameters after one with a default value must also have defaults"
                        .to_string(),
                ));
            }
        }
    }
    // v2.12 finishing: PG19 duplicate parameter names
    // (functioncmds.c interpret_function_parameter_list): a name may
    // be shared by a pure-IN/VARIADIC parameter and a pure-OUT/TABLE
    // parameter ("pure in doesn't conflict with pure out"); any
    // other repetition — including INOUT against anything — is
    // 42P13.
    {
        // class: 0 = in-only (IN/VARIADIC), 1 = out-only (OUT /
        // RETURNS TABLE column), 2 = both (INOUT).
        let mut named: Vec<(&str, u8)> = Vec::new();
        for a in args {
            if let Some(n) = a.name.as_deref() {
                let cls = match a.mode {
                    crate::sql::ArgMode::In | crate::sql::ArgMode::Variadic => 0,
                    crate::sql::ArgMode::Out => 1,
                    crate::sql::ArgMode::Inout => 2,
                };
                named.push((n, cls));
            }
        }
        for (n, _) in table_cols {
            named.push((n.as_str(), 1));
        }
        for i in 0..named.len() {
            for j in i + 1..named.len() {
                if named[i].0 == named[j].0 {
                    let (x, y) = (named[i].1, named[j].1);
                    if (x == 0 && y == 1) || (x == 1 && y == 0) {
                        continue;
                    }
                    return Err(exec_err(
                        "42P13",
                        format!("parameter name \"{}\" used more than once", named[i].0),
                    ));
                }
            }
        }
    }
    // v2.12 finishing: PG19 RETURNS-vs-OUT consistency
    // (functioncmds.c: prorettype must equal requiredResultType —
    // the single output's type, or RECORD for several outputs).
    // An omitted RETURNS was already synthesized by the parser to
    // exactly that type, so this only fires on an explicit clause.
    // RETURNS TABLE defines its own outputs; skip it.
    if !procedure && table_cols.is_empty() {
        let outs: Vec<&str> = args
            .iter()
            .filter(|a| a.mode.is_output())
            .map(|a| a.type_name.as_str())
            .collect();
        if !outs.is_empty() && !ret_type.is_empty() {
            let rt = canon_func_type_name(ret_type);
            if outs.len() == 1 {
                if rt != canon_func_type_name(outs[0]) {
                    return Err(exec_err(
                        "42P13",
                        format!(
                            "function result type must be {} because of OUT parameters",
                            canon_func_type_name(outs[0])
                        ),
                    ));
                }
            } else if rt != "record" {
                return Err(exec_err(
                    "42P13",
                    "function result type must be record because of OUT parameters".to_string(),
                ));
            }
        }
    }
    // v1.96: language USAGE (aclchk.c): trusted languages default to
    // PUBLIC USAGE (revocable via lang_acls); untrusted (c/internal)
    // are superuser-only.
    check_language_usage(eng, ctx, lang_name)?;
    // v1.96: USAGE on argument/return types (domains, composites).
    for a in args {
        check_type_usage(eng, ctx, &a.type_name)?;
        // v1.96: and on types named by casts inside arg DEFAULT
        // expressions (priv_testfunc7a shape).
        if let Some(d) = &a.default {
            check_expr_cast_usages(eng, ctx, d)?;
        }
    }
    check_type_usage(eng, ctx, ret_type)?;
    // PG19: the return type must exist at CREATE time.
    // v1.00: `RETURNS trigger` is PG19's trigger pseudo-type — valid
    // only for trigger functions (which must be LANGUAGE plpgsql here).
    let is_trigger_fn = ret_type.eq_ignore_ascii_case("trigger");
    if is_trigger_fn {
        if lang != crate::sql::FuncLang::Plpgsql {
            return Err(exec_err(
                "0A000",
                "trigger functions must use LANGUAGE plpgsql".to_string(),
            ));
        }
        if returns_set {
            return Err(exec_err(
                "42601",
                "trigger functions cannot return a set".to_string(),
            ));
        }
        // Validate the body against the bounded trigger-body grammar
        // now (like PG's plpgsql validator at CREATE time) — but do NOT
        // run it through the bounded PL/pgSQL desugar; trigger bodies
        // are a separate, deliberately small grammar.
        crate::sql::parse_trigger_body(body).map_err(|e| {
            exec_err(
                e.code,
                format!("invalid trigger function body: {}", e.message),
            )
        })?;
    } else {
        resolve_func_type_name(eng, ctx.snap, ctx.own, ctx.session, ret_type)?;
    }
    for a in args {
        resolve_func_type_name(eng, ctx.snap, ctx.own, ctx.session, &a.type_name)?;
    }
    // Parse the SQL body now so a bad body fails at CREATE (like PG's
    // parse analysis), not at first call. Internal functions name a C
    // symbol instead of SQL text.
    // v0.97: bounded plpgsql — a single-RETURN body desugars to a SQL
    // SELECT here.
    // v1.01: richer plpgsql bodies (statement sequences + EXCEPTION
    // blocks) take the multi-statement path: the original body text is
    // stored and the parsed `PlpgsqlBody` goes in `FuncDef.plpgsql`.
    // v1.00: trigger functions bypass the desugar entirely (their raw
    // body was validated against the trigger grammar above).
    // v1.96 finishing pass: a void plpgsql function must not
    // RETURN a value (PG19 rejects it at CREATE). The single-RETURN
    // desugar below would otherwise smuggle it past the body
    // validator, so run the validator explicitly for void
    // functions; only the void-RETURN error is fatal here (other
    // parse gaps belong to the desugar path's subset).
    if lang == crate::sql::FuncLang::Plpgsql && ret_type.eq_ignore_ascii_case("void") {
        if let Err(e) =
            crate::sql::parse_plpgsql_body(body, returns_set, true, false, &func_arg_names(args))
        {
            if e.message.contains("RETURN cannot have a parameter") {
                return Err(exec_err(e.code, format!("plpgsql: {}", e.message)));
            }
        }
    }
    let owned_body: String;
    let mut plpgsql_body: Option<crate::sql::PlpgsqlBody> = None;
    let body: &str = if is_trigger_fn {
        body
    } else if lang == crate::sql::FuncLang::Plpgsql {
        // v1.03: SETOF bodies skip the v0.97 single-RETURN desugar so a
        // bare RETURN is honestly rejected (42601) instead of silently
        // becoming a SQL SELECT body.
        // v2.12 finishing: bodies of functions WITH output parameters
        // skip it too — the desugar would bypass the plpgsql grammar
        // validation entirely (e.g. `RETURN expr` in an OUT function
        // must error "RETURN cannot have a parameter in function
        // with OUT parameters", which only parse_plpgsql_body knows).
        let has_out_pre = !table_cols.is_empty() || args.iter().any(|a| a.mode.is_output());
        let desugared = if returns_set || has_out_pre {
            None
        } else {
            crate::sql::desugar_plpgsql_body(body).ok()
        };
        match desugared {
            Some(sel) => {
                owned_body = sel;
                &owned_body
            }
            None => {
                // v2.12: output parameters (OUT/INOUT args or
                // RETURNS TABLE columns) make bare RETURN / bare
                // RETURN NEXT legal shapes.
                let has_out = !table_cols.is_empty() || args.iter().any(|a| a.mode.is_output());
                // v2.12: RETURNS TABLE column names are visible in
                // the body exactly like argument names.
                let mut vis_names = func_arg_names(args);
                vis_names.extend(table_cols.iter().map(|(n, _)| n.clone()));
                let mut pb = crate::sql::parse_plpgsql_body(
                    body,
                    returns_set,
                    ret_type.eq_ignore_ascii_case("void"),
                    has_out,
                    &vis_names,
                )
                .map_err(|e| exec_err(e.code, format!("plpgsql: {}", e.message)))?;
                // v1.03: resolve DECLAREd variable types now (like PG's
                // plpgsql validator at CREATE time).
                // v2.11: ALIAS decls take the aliased argument's type;
                // record / %TYPE / %ROWTYPE / refcursor / cursor decls
                // have no catalog type to resolve against.
                for d in &mut pb.decls {
                    if let Some(target) = d.alias_for.clone() {
                        let ty = if let Some(digits) = target.strip_prefix('$') {
                            digits
                                .parse::<usize>()
                                .ok()
                                .and_then(|n| args.get(n - 1))
                                .map(|a| a.type_name.clone())
                        } else {
                            args.iter()
                                .find(|a| a.name.as_deref() == Some(target.as_str()))
                                .map(|a| a.type_name.clone())
                        };
                        if let Some(ty) = ty {
                            d.type_name = ty;
                        }
                        continue;
                    }
                    if d.is_cursor
                        || d.type_name.contains('%')
                        || d.type_name.eq_ignore_ascii_case("record")
                        || d.type_name.eq_ignore_ascii_case("refcursor")
                    {
                        continue;
                    }
                    resolve_func_type_name(eng, ctx.snap, ctx.own, ctx.session, &d.type_name)
                        .map_err(|e| exec_err(e.code, format!("plpgsql: {}", e.message)))?;
                }
                // v2.12 finishing: GET DIAGNOSTICS targets must be
                // scalar variables (PG19 pl_gram.y getdiag_target).
                // Record-typed names: arguments or declarations
                // whose type resolves to a composite/record, plus
                // `record` and %ROWTYPE declarations (the fx case in
                // plpgsql.sql: an argument of a composite type).
                {
                    let mut rec_names: Vec<String> = Vec::new();
                    for a in args {
                        if let Some(n) = &a.name {
                            if let Ok(ct) = resolve_func_type_name(
                                eng,
                                ctx.snap,
                                ctx.own,
                                ctx.session,
                                &a.type_name,
                            ) {
                                if matches!(ct, ColType::Composite | ColType::Record) {
                                    rec_names.push(n.clone());
                                }
                            }
                        }
                    }
                    for d in &pb.decls {
                        let tn = d.type_name.as_str();
                        let is_rec = tn.eq_ignore_ascii_case("record")
                            || tn.to_ascii_uppercase().contains("%ROWTYPE")
                            || matches!(
                                resolve_func_type_name(eng, ctx.snap, ctx.own, ctx.session, tn),
                                Ok(ColType::Composite | ColType::Record)
                            );
                        if is_rec {
                            rec_names.push(d.name.clone());
                        }
                    }
                    if !rec_names.is_empty() {
                        check_getdiag_targets(&pb.stmts, &pb.handlers, &rec_names)?;
                    }
                }
                // v2.12: a variable name colliding with an argument
                // name is NOT an error — PG19 allows the declaration
                // to shadow the argument (with an extra_warnings
                // notice this engine does not emit); the rewrite map
                // gives the declaration the name in its block. (The
                // v1.03/v2.11 rejection broke use_refcursor,
                // shadowtest, and pl_qual_names, all of which PG
                // creates successfully.)
                let arg_names: Vec<Option<String>> = args.iter().map(|a| a.name.clone()).collect();
                let arg_modes: Vec<u8> = args.iter().map(|a| a.mode.code()).collect();
                inject_plpgsql_arg_vars(&mut pb, &arg_names, &arg_modes, table_cols);
                rewrite_plpgsql_body_refs(&mut pb, &arg_names);
                plpgsql_body = Some(pb);
                body
            }
        }
    } else {
        body
    };
    let mut parsed: Option<Vec<Stmt>> = None;
    if !is_trigger_fn
        && (lang == crate::sql::FuncLang::Sql || lang == crate::sql::FuncLang::Plpgsql)
        && plpgsql_body.is_none()
    {
        // v1.32: multi-statement SQL bodies (PG19 fmgr_sql_validator):
        // parse every statement, validate the final one determines the
        // return type (42P13 on mismatch).
        parsed = Some(parse_sql_func_body(
            eng,
            ctx.snap,
            &ctx.all_xids,
            ctx.own,
            ctx.session,
            body,
            args,
            ret_type,
            returns_set,
            table_cols,
            lang == crate::sql::FuncLang::Sql,
        )?);
    } else if lang == crate::sql::FuncLang::Internal {
        // v0.86: validate the internal symbol at CREATE (like PG's
        // fmgr lookup); unknown symbols are 0A000. v1.38: the int4/int8
        // I/O symbols (PG19 int.c) join int4eq — the float4/float8
        // conformance types are defined in terms of them.
        if !matches!(body, "int4eq" | "int4in" | "int4out" | "int8in" | "int8out")
            && super::funcs::internal_cmp_symbol(body).is_none()
        {
            return Err(exec_err(
                "0A000",
                format!("unsupported internal function \"{}\"", body),
            ));
        }
    }
    // v0.87: overload-aware CREATE. `prev` is the specific overload
    // being replaced (by signature), if any.
    // v2.12: the signature is the INPUT argument types (PG19
    // proargtypes); find_function_by_signature also accepts a full
    // type list for legacy callers.
    let new_arg_types: Vec<String> = args.iter().map(|a| a.type_name.clone()).collect();
    let new_input_types: Vec<String> = args
        .iter()
        .filter(|a| a.mode.is_input())
        .map(|a| a.type_name.clone())
        .collect();
    // v2.12 finishing: CREATE / CREATE OR REPLACE identity is the
    // INPUT argument list alone (PG19 proargtypes). The shared
    // finder's full-list fallback must NOT fire here: a routine
    // whose inputs are (int,int) does not block creating one whose
    // inputs are (int,int,int) merely because the first routine's
    // full list (OUT included) also has three entries (the ptest10
    // pair in create_procedure.sql).
    let prev = eng
        .db
        .functions
        .get(name)
        .and_then(|overloads| {
            overloads.iter().find(|f| {
                let ins = f.input_arg_types();
                ins.len() == new_input_types.len()
                    && ins
                        .iter()
                        .zip(new_input_types.iter())
                        .all(|(a, b)| canon_func_type_name(a) == canon_func_type_name(b))
            })
        })
        .cloned();
    if prev.is_some() && !or_replace {
        // v1.32: PG-verbatim message (pg_proc.c ProcedureCreate).
        return Err(exec_err(
            "42723",
            format!(
                "function \"{}\" already exists with same argument types",
                name
            ),
        ));
    }
    // PG19: OR REPLACE with a different return type is 42P13.
    if let Some(p) = &prev {
        if or_replace && canon_func_type_name(&p.ret_type) != canon_func_type_name(ret_type) {
            return Err(exec_err(
                "42P13",
                format!(
                    "cannot change return type of existing function \"{}\"",
                    name
                ),
            ));
        }
        // v2.13: PG19 pg_proc.c ProcedureCreate — OR REPLACE cannot
        // rename an input parameter, nor drop its name (adding a
        // name where none existed is allowed).
        if or_replace {
            let old_inputs: Vec<Option<&str>> = (0..p.arg_types.len())
                .filter(|i| p.arg_mode(*i).is_input())
                .map(|i| p.arg_names.get(i).and_then(|n| n.as_deref()))
                .collect();
            let new_inputs: Vec<Option<&str>> = args
                .iter()
                .filter(|a| a.mode.is_input())
                .map(|a| a.name.as_deref())
                .collect();
            for (j, old) in old_inputs.iter().enumerate() {
                if let Some(old_name) = old {
                    if new_inputs.get(j).copied().flatten() != Some(*old_name) {
                        return Err(exec_err(
                            "42P13",
                            format!("cannot change name of input parameter \"{}\"", old_name),
                        ));
                    }
                }
            }
        }
    }
    let def = crate::storage::FuncDef {
        name: name.to_string(),
        arg_names: args.iter().map(|a| a.name.clone()).collect(),
        arg_types: args.iter().map(|a| a.type_name.clone()).collect(),
        ret_type: ret_type.to_string(),
        returns_set,
        lang,
        body: body.to_string(),
        parsed,
        // v1.01: multi-statement plpgsql bodies (statement sequences +
        // EXCEPTION blocks); None for the v0.97 single-RETURN form.
        plpgsql: plpgsql_body,
        volatility,
        strict,
        security_definer,
        is_procedure: procedure,
        table_cols: table_cols.to_vec(),
        // v2.12: argument modes (IN/OUT/INOUT/VARIADIC), parallel
        // to arg_types.
        arg_modes: args.iter().map(|a| a.mode.code()).collect(),
    };
    // v1.96: record the function owner (DROP FUNCTION / GRANT checks).
    {
        let prev_fn = eng.db.fn_acls.clone();
        let prev_type = eng.db.type_acls.clone();
        let prev_lang = eng.db.lang_acls.clone();
        let prev_fn_owners = eng.db.fn_owners.clone();
        let prev_type_owners = eng.db.type_owners.clone();
        if prev_fn_owners.get(name) != Some(&ctx.role.to_string()) {
            eng.db
                .fn_owners
                .insert(name.to_string(), ctx.role.to_string());
            // v1.96: a brand-new routine takes its initial ACL from
            // ALTER DEFAULT PRIVILEGES rules when any exist for the
            // creator (PG: acldefault modified by the rules — a
            // REVOKE ... FROM public rule erases the default PUBLIC
            // EXECUTE). CREATE OR REPLACE keeps the existing ACL.
            if !prev_fn_owners.contains_key(name) {
                if let Some(init) = crate::exec::privilege::adp_initial_acl(
                    &eng.db,
                    ctx.role,
                    'f',
                    name,
                    crate::storage::PRIV_USAGE,
                    crate::storage::PRIV_USAGE,
                ) {
                    eng.db.fn_acls.insert(name.to_string(), init);
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
    }
    // v0.87: insert or replace the specific overload.
    {
        let overloads = eng.db.functions.entry(name.to_string()).or_default();
        if let Some(p) = &prev {
            if let Some(slot) = overloads.iter_mut().find(|f| {
                f.arg_types.len() == p.arg_types.len()
                    && f.arg_types
                        .iter()
                        .zip(p.arg_types.iter())
                        .all(|(a, b)| canon_func_type_name(a) == canon_func_type_name(b))
            }) {
                *slot = def;
            } else {
                overloads.push(def);
            }
        } else {
            overloads.push(def);
        }
    }
    // v0.87: the CreateFunction WAL path reads the live function map, so
    // the definition is WAL-logged (and checkpointed) automatically.
    // `added` is the specific overload for precise undo.
    let added = eng
        .db
        .functions
        .get(name)
        .and_then(|ovs| {
            ovs.iter().find(|f| {
                f.arg_types.len() == new_arg_types.len()
                    && f.arg_types
                        .iter()
                        .zip(new_arg_types.iter())
                        .all(|(a, b)| canon_func_type_name(a) == canon_func_type_name(b))
            })
        })
        .cloned()
        .expect("overload just inserted");
    ctx.writes.push(WriteOp::CreateFunction {
        name: name.to_string(),
        added,
        prev,
    });
    // v1.96 finishing pass: schema attribution for DROP SCHEMA CASCADE.
    note_obj_schema(eng, ctx, name, name);
    Ok(ExecResult::Command {
        tag: "CREATE FUNCTION".to_string(),
    })
}

/// v0.86: `DROP FUNCTION [IF EXISTS] name (types)`. Signature matching
/// is by arity and canonical type name (42883 on mismatch, like PG's
/// "function ... does not exist").
/// v0.87: `DROP FUNCTION [IF EXISTS] name (types) [CASCADE|RESTRICT]`.
/// Signature matching is by arity and canonical type name (42883 on
/// mismatch). PG19 RESTRICT (the default) fails with 2BP01 if an operator
/// still uses the function as its procedure; CASCADE drops those
/// operators too.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_drop_function(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    arg_types: &[String],
    arg_modes: &[u8],
    if_exists: bool,
    cascade: bool,
    procedure: bool,
    args_omitted: bool,
    any_kind: bool,
) -> Result<ExecResult, ExecError> {
    // v0.87: drop the specific overload by signature.
    // v1.96 finishing pass: with the argument list omitted, resolve
    // by name alone — exactly one routine of the addressed kind must
    // exist (PG19: otherwise "function name is not unique", 42725).
    let prev = if args_omitted {
        let cands: Vec<crate::storage::FuncDef> = eng
            .db
            .functions
            .get(name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|f| any_kind || f.is_procedure == procedure)
            .collect();
        match cands.len() {
            0 => None,
            1 => Some(cands.into_iter().next().unwrap()),
            _ => {
                return Err(exec_err(
                    "42725",
                    format!(
                        "function name \"{}\" is not unique; specify the argument list to identify the routine to drop",
                        name
                    ),
                ));
            }
        }
    } else if arg_modes.is_empty() {
        // Internal callers (roles.rs dependency cascades) pass a
        // stored def's full type list without mode information:
        // keep the legacy single-match lookup for them.
        find_function_by_signature(eng, name, arg_types)
    } else {
        // v2.12 finishing (PG19 LookupFuncName as used by DROP):
        // the written list is matched BOTH as the input-argument
        // list and as the full argument list (OUT included); a
        // written mode must match the stored mode, a modeless entry
        // (u8::MAX) is a wildcard. The union of both interpretations
        // must name exactly one routine — e.g. with ptest10
        // (OUT int,int,int) and (IN int,int,int) both present, the
        // bare list (int,int,int) matches one by inputs and both by
        // full list, so PG19 reports the name not unique (42725);
        // (out int,int,int) matches only the first.
        let overloads = eng.db.functions.get(name).cloned().unwrap_or_default();
        let mut cands: Vec<crate::storage::FuncDef> = Vec::new();
        for f in &overloads {
            let types_eq = |stored: &[String]| {
                stored.len() == arg_types.len()
                    && stored
                        .iter()
                        .zip(arg_types.iter())
                        .all(|(a, b)| canon_func_type_name(a) == canon_func_type_name(b))
            };
            let mut matched = false;
            if types_eq(&f.arg_types) {
                matched = arg_modes
                    .iter()
                    .zip(f.arg_modes.iter())
                    .all(|(g, s)| *g == u8::MAX || g == s);
            }
            if !matched {
                let ins = f.input_arg_types();
                if types_eq(&ins) {
                    let in_modes: Vec<u8> = f
                        .arg_modes
                        .iter()
                        .copied()
                        .filter(|m| crate::sql::ArgMode::from_code(*m).is_input())
                        .collect();
                    matched = arg_modes
                        .iter()
                        .zip(in_modes.iter())
                        .all(|(g, s)| *g == u8::MAX || g == s);
                }
            }
            if matched {
                cands.push(f.clone());
            }
        }
        match cands.len() {
            0 => None,
            1 => Some(cands.into_iter().next().unwrap()),
            _ => {
                return Err(exec_err(
                    "42725",
                    format!(
                        "{} name \"{}\" is not unique; specify the argument list to identify the routine to drop",
                        if procedure { "procedure" } else { "function" },
                        name
                    ),
                ));
            }
        }
    };
    let Some(dropped) = prev else {
        if !if_exists {
            return Err(exec_err(
                "42883",
                format!("function {}() does not exist", name),
            ));
        }
        return Ok(ExecResult::Command {
            tag: if procedure {
                "DROP PROCEDURE".to_string()
            } else {
                "DROP FUNCTION".to_string()
            },
        });
    };
    // v1.96: DROP FUNCTION and DROP PROCEDURE address disjoint
    // namespaces in PG19. (DROP ROUTINE addresses either.)
    if !any_kind && dropped.is_procedure != procedure {
        if if_exists {
            return Ok(ExecResult::Command {
                tag: if procedure {
                    "DROP PROCEDURE".to_string()
                } else {
                    "DROP FUNCTION".to_string()
                },
            });
        }
        return Err(exec_err(
            "42883",
            format!(
                "{} {}() does not exist",
                if procedure { "procedure" } else { "function" },
                name
            ),
        ));
    }
    // v1.96: only the owner (or a superuser) may drop a routine.
    {
        let owner = eng
            .db
            .fn_owners
            .get(name)
            .cloned()
            .unwrap_or_else(|| "postgres".to_string());
        if owner != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!(
                    "must be owner of {} {}",
                    if procedure { "procedure" } else { "function" },
                    name
                ),
            ));
        }
    }
    // v0.87: PG19 dependency checking — an operator whose procedure is
    // this function blocks RESTRICT (2BP01); CASCADE drops it.
    let dependent_ops: Vec<(String, OperDef)> = eng
        .db
        .operators
        .iter()
        .flat_map(|(op_name, defs)| {
            defs.iter()
                .filter(|d| {
                    d.procedure == name
                        && find_function_by_signature(eng, name, &dropped.arg_types)
                            .is_some_and(|f| f.arg_types == dropped.arg_types)
                })
                .map(|d| (op_name.clone(), d.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    if !dependent_ops.is_empty() && !cascade {
        return Err(exec_err(
            "2BP01",
            format!(
                "cannot drop function {}() because operator {} depends on it",
                name, dependent_ops[0].0,
            ),
        ));
    }
    // CASCADE: drop the dependent operators first.
    for (op_name, op_def) in &dependent_ops {
        let prev_ops = eng.db.operators.get(op_name).cloned().unwrap_or_default();
        let next: Vec<OperDef> = prev_ops
            .iter()
            .filter(|d| {
                !(d.procedure == name
                    && d.leftarg == op_def.leftarg
                    && d.rightarg == op_def.rightarg)
            })
            .cloned()
            .collect();
        if next.is_empty() {
            eng.db.operators.remove(op_name);
        } else {
            eng.db.operators.insert(op_name.clone(), next);
        }
        ctx.writes.push(WriteOp::DropOperator {
            name: op_name.clone(),
            prev: prev_ops,
        });
    }
    {
        let overloads = eng
            .db
            .functions
            .get_mut(name)
            .expect("overload found above");
        // v2.12 finishing: the removal key is the full identity —
        // types AND modes AND kind. Keying on types alone deleted
        // every same-full-types overload: the ptest10 pair
        // (OUT int,int,int) / (IN int,int,int) share the type list
        // (int,int,int), so dropping one dropped both.
        overloads.retain(|f| {
            !(f.is_procedure == dropped.is_procedure
                && f.arg_modes == dropped.arg_modes
                && f.arg_types.len() == dropped.arg_types.len()
                && f.arg_types
                    .iter()
                    .zip(dropped.arg_types.iter())
                    .all(|(a, b)| canon_func_type_name(a) == canon_func_type_name(b)))
        });
        if overloads.is_empty() {
            eng.db.functions.remove(name);
        }
    }
    // v1.96: when the last overload goes, the owner/ACL records go
    // too (PG drops the pg_proc row; stale fn_owners entries were
    // blocking DROP ROLE for long-dropped routines).
    if !eng.db.functions.contains_key(name)
        && (eng.db.fn_owners.contains_key(name) || eng.db.fn_acls.contains_key(name))
    {
        let prev_fn = eng.db.fn_acls.clone();
        let prev_type = eng.db.type_acls.clone();
        let prev_lang = eng.db.lang_acls.clone();
        let prev_fn_owners = eng.db.fn_owners.clone();
        let prev_type_owners = eng.db.type_owners.clone();
        eng.db.fn_owners.remove(name);
        eng.db.fn_acls.remove(name);
        ctx.writes.push(WriteOp::FnTypeAcls {
            prev_fn,
            prev_type,
            prev_lang,
            prev_fn_owners,
            prev_type_owners,
        });
        forget_obj_schema(eng, ctx, name);
    }
    ctx.writes.push(WriteOp::DropFunction {
        name: name.to_string(),
        prev: Some(dropped),
    });
    Ok(ExecResult::Command {
        tag: if procedure {
            "DROP PROCEDURE".to_string()
        } else {
            "DROP FUNCTION".to_string()
        },
    })
}

/// v1.96: CALL proc(args) — evaluate the procedure body for its side
/// effects; the (single) result row, if any, is discarded.
pub(crate) fn exec_call_proc(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    args: &[crate::sql::Expr],
) -> Result<ExecResult, ExecError> {
    let base = name.rsplit('.').next().unwrap_or(name);
    let overloads = eng
        .db
        .functions
        .get(name)
        .or_else(|| eng.db.functions.get(base));
    let Some(overloads) = overloads else {
        return Err(exec_err(
            "42883",
            format!("procedure {}() does not exist", name),
        ));
    };
    let fdef = overloads
        .iter()
        .find(|f| f.is_procedure && f.arg_types.len() == args.len())
        .cloned();
    let Some(fdef) = fdef else {
        return Err(exec_err(
            "42883",
            format!("procedure {}() does not exist", name),
        ));
    };
    // EXECUTE privilege (PG19: CALL checks the procedure ACL).
    let privs = function_privs_for(&eng.db, ctx.role, &fdef.name, ctx.snap, ctx.own);
    if privs & crate::storage::PRIV_USAGE != crate::storage::PRIV_USAGE {
        return Err(exec_err(
            "42501",
            format!("permission denied for procedure {}", fdef.name),
        ));
    }
    let mut lock_ids: Vec<(String, u64)> = Vec::new();
    let mut q = Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: ctx.read_only,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        priv_role_override: None,
        hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        pending_updates: None,
        write: Some(qwrite_from_ctx(
            &mut *ctx.writes,
            ctx.write_xid,
            ctx.level,
            ctx.default_toast_compression,
        )),
    };
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_expr(&mut q, &[], a)?);
    }
    let _ = call_user_function(&mut q, &[], &fdef, &vals)?;
    Ok(ExecResult::Command {
        tag: "CALL".to_string(),
    })
}

/// v1.96: `DO <body>` — run an anonymous plpgsql block (no args, no
/// return value; the tag is DO). The body parses through the same
/// plpgsql parser as function bodies, so EXECUTE/PERFORM/RAISE and
/// EXCEPTION handlers behave identically.
pub(crate) fn exec_do_block(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    body: &str,
) -> Result<ExecResult, ExecError> {
    let mut pb = crate::sql::parse_plpgsql_body(body, false, true, false, &[]).map_err(sql_err)?;
    // v2.11: DO blocks get the same name rewrite as function bodies
    // (no arguments; declared variables become $N slots).
    rewrite_plpgsql_body_refs(&mut pb, &[]);
    let mut lock_ids: Vec<(String, u64)> = Vec::new();
    let mut q = Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: ctx.read_only,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        priv_role_override: None,
        hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        pending_updates: None,
        write: Some(qwrite_from_ctx(
            &mut *ctx.writes,
            ctx.write_xid,
            ctx.level,
            ctx.default_toast_compression,
        )),
    };
    let mut run = PlpgsqlRun::new(vec![None; pb.decls.len()], 0);
    init_plpgsql_decls(&mut q, &[], &pb, &mut run)?;
    let mark = plpgsql_write_mark(&q);
    match run_plpgsql_stmts(
        &mut q,
        &[],
        &pb.stmts,
        &pb,
        &mut run,
        "void",
        "<anonymous block>",
    ) {
        Ok(_) => Ok(ExecResult::Command {
            tag: "DO".to_string(),
        }),
        Err(e) => {
            let handler = pb.handlers.iter().find(|h| {
                h.sqlstates
                    .iter()
                    .any(|c| crate::sql::plpgsql_condition_matches(c, &e.code))
            });
            match handler {
                Some(h) => {
                    plpgsql_write_rollback(&mut q, mark);
                    run.last_error = Some((e.code.to_string(), e.message.clone()));
                    run_plpgsql_stmts(
                        &mut q,
                        &[],
                        &h.stmts,
                        &pb,
                        &mut run,
                        "void",
                        "<anonymous block>",
                    )?;
                    Ok(ExecResult::Command {
                        tag: "DO".to_string(),
                    })
                }
                None => Err(e),
            }
        }
    }
}

/// v1.00/v2.09: `CREATE TRIGGER` (PG19 `CreateTrigStmt`). The
/// trigger is stored on the table's catalog entry (`Table.triggers`),
/// so creation is transactional (ROLLBACK-safe), WAL-logged, and
/// checkpointed via the normal table-version machinery (v2.09 added
/// the triggers section to the constraints codec — before that the
/// in-memory version was the only copy). BEFORE ROW triggers fire
/// for INSERT/UPDATE/DELETE, AFTER ROW triggers fire after the row
/// change, WHEN is evaluated per row, and DEFERRABLE constraint
/// triggers queue into the v2.06 deferred machinery (see
/// exec/deferred.rs + exec/partition.rs firing code).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_trigger(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    table: &str,
    timing: crate::sql::TriggerTiming,
    events: u8,
    for_each_row: bool,
    function: &str,
    args: &[String],
    when: Option<crate::sql::Expr>,
    is_constraint: bool,
    deferrable: bool,
    initially_deferred: bool,
    enforced: bool,
    constr_rel: Option<String>,
    update_cols: Vec<String>,
    has_transition: bool,
) -> Result<ExecResult, ExecError> {
    // v0.11: creating a trigger needs table ownership (like ALTER).
    require_table_owner(eng, ctx, table)?;
    let prev = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?
        .clone();
    // PG19: duplicate trigger names on one table are 42723.
    if prev.triggers.iter().any(|t| t.name == name) {
        return Err(exec_err(
            "42723",
            format!(
                "trigger \"{}\" for relation \"{}\" already exists",
                name, table
            ),
        ));
    }
    // v2.09: the FROM referenced table of a constraint trigger must
    // exist (PG19 CreateTrigger checks the constrrel relation).
    if let Some(cr) = &constr_rel {
        if eng
            .db
            .find_table(cr, ctx.snap, &ctx.all_xids, ctx.session)
            .is_none()
        {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", cr),
            ));
        }
    }
    // v2.09: UPDATE OF columns must exist on the table (PG19 42703).
    for c in &update_cols {
        if prev.column_index(c).is_none() {
            return Err(exec_err(
                "42703",
                format!("column \"{}\" of relation \"{}\" does not exist", c, table),
            ));
        }
    }
    // v2.09 PG19 trigger.c CreateTrigger validations.
    if has_transition {
        let n_events = [
            crate::sql::trig_event::INSERT,
            crate::sql::trig_event::UPDATE,
            crate::sql::trig_event::DELETE,
            crate::sql::trig_event::TRUNCATE,
        ]
        .iter()
        .filter(|b| events & **b != 0)
        .count();
        if n_events != 1 {
            return Err(exec_err(
                "0A000",
                "transition tables cannot be specified for triggers with more than one event"
                    .to_string(),
            ));
        }
        if !update_cols.is_empty() {
            return Err(exec_err(
                "0A000",
                "transition tables cannot be specified for triggers with column lists".to_string(),
            ));
        }
        if for_each_row {
            // Partitioned parent, or a partition itself: PG rejects
            // ROW triggers with transition tables on both.
            let is_parent = prev.partition.as_ref().is_some_and(|p| p.is_partitioned);
            let is_child = prev
                .partition
                .as_ref()
                .and_then(|p| p.parent.as_ref())
                .is_some();
            if is_parent {
                return Err(exec_err(
                    "0A000",
                    format!(
                        "\"{}\" is a partitioned table: ROW triggers with transition tables are not supported on partitioned tables",
                        table
                    ),
                ));
            }
            if is_child {
                return Err(exec_err(
                    "0A000",
                    "ROW triggers with transition tables are not supported on partitions"
                        .to_string(),
                ));
            }
        }
    }
    if let Some(w) = &when {
        // PG19: WHEN may reference OLD/NEW row variables only.
        let refs_old = expr_refs_rowvar(w, "old");
        let refs_new = expr_refs_rowvar(w, "new");
        if !for_each_row && (refs_old || refs_new) {
            return Err(exec_err(
                "42P17",
                "statement trigger's WHEN condition cannot reference column values".to_string(),
            ));
        }
        if events & crate::sql::trig_event::INSERT != 0 && refs_old {
            return Err(exec_err(
                "42P17",
                "INSERT trigger's WHEN condition cannot reference OLD values".to_string(),
            ));
        }
        if events & crate::sql::trig_event::DELETE != 0 && refs_new {
            return Err(exec_err(
                "42P17",
                "DELETE trigger's WHEN condition cannot reference NEW values".to_string(),
            ));
        }
        if timing == crate::sql::TriggerTiming::Before && expr_refs_new_syscol(w) {
            return Err(exec_err(
                "0A000",
                "BEFORE trigger's WHEN condition cannot reference NEW system columns".to_string(),
            ));
        }
    }
    // The function must exist and be a trigger function (PG19:
    // `RETURNS trigger`). Trigger functions take no arguments.
    let fdef = find_function_by_signature(eng, function, &[])
        .ok_or_else(|| exec_err("42883", format!("function {}() does not exist", function)))?;
    if !fdef.ret_type.eq_ignore_ascii_case("trigger") {
        return Err(exec_err(
            "42P17",
            format!("function \"{}\" must return type \"trigger\"", function),
        ));
    }
    let mut next = prev.clone();
    next.triggers.push(crate::sql::TriggerDef {
        name: name.to_string(),
        timing,
        events,
        for_each_row,
        function: function.to_string(),
        args: args.to_vec(),
        is_constraint,
        deferrable,
        initially_deferred,
        enforced,
        constr_rel,
        update_cols,
        when_expr: when,
        has_transition,
    });
    commit_table_version(eng, ctx, table, prev, next);
    Ok(ExecResult::Command {
        tag: "CREATE TRIGGER".to_string(),
    })
}

/// v2.09: does the expression reference `<qual>.<anything>`?
fn expr_refs_rowvar(e: &crate::sql::Expr, qual: &str) -> bool {
    let mut found = false;
    crate::exec::eval::walk_expr(e, &mut |x| {
        if let crate::sql::Expr::Column { table: Some(t), .. } = x {
            if t.eq_ignore_ascii_case(qual) {
                found = true;
            }
        }
        if let crate::sql::Expr::WholeRow { qual: q } = x {
            if q.eq_ignore_ascii_case(qual) {
                found = true;
            }
        }
    });
    found
}

/// v2.09: does the expression reference NEW.<system column>?
/// (tableoid is the one system column the trigger scope serves.)
fn expr_refs_new_syscol(e: &crate::sql::Expr) -> bool {
    let mut found = false;
    crate::exec::eval::walk_expr(e, &mut |x| {
        if let crate::sql::Expr::Column {
            table: Some(t),
            name,
        } = x
        {
            if t.eq_ignore_ascii_case("new") && name == "tableoid" {
                found = true;
            }
        }
    });
    found
}

/// v2.09: `ALTER TRIGGER name ON table RENAME TO new_name`
/// (PG19 RenameStmt OBJECT_TRIGGER): missing trigger 42704,
/// duplicate new name 42723. Versions the table like CREATE/DROP.
pub(crate) fn exec_alter_trigger_rename(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    table: &str,
    new_name: &str,
) -> Result<ExecResult, ExecError> {
    require_table_owner(eng, ctx, table)?;
    let prev = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?
        .clone();
    if !prev.triggers.iter().any(|t| t.name == name) {
        // v2.09 PG19 trigger.c renametrig: a trigger inherited from
        // a partitioned parent (tgparentid set) cannot be renamed on
        // the partition — rename it on the parent instead (0A000).
        let mut ancestor = prev.partition.as_ref().and_then(|p| p.parent.clone());
        while let Some(a) = ancestor {
            let Some(at) = eng
                .db
                .find_table(&a, ctx.snap, &ctx.all_xids, ctx.session)
                .cloned()
            else {
                break;
            };
            if at.triggers.iter().any(|t| t.name == name) {
                return Err(exec_err(
                    "0A000",
                    format!(
                        "cannot rename trigger \"{}\" on table \"{}\": rename the trigger on the partitioned table \"{}\" instead",
                        name, table, a
                    ),
                ));
            }
            ancestor = at.partition.as_ref().and_then(|p| p.parent.clone());
        }
        return Err(exec_err(
            "42704",
            format!(
                "trigger \"{}\" for relation \"{}\" does not exist",
                name, table
            ),
        ));
    }
    // v2.09 PG19: the rename cascades to partition clones, so the
    // new name must not collide with a trigger defined directly on
    // any descendant partition either (42723).
    {
        let mut stack: Vec<String> = prev
            .partition
            .as_ref()
            .map(|p| p.children.clone())
            .unwrap_or_default();
        while let Some(child) = stack.pop() {
            if let Some(ct) = eng
                .db
                .find_table(&child, ctx.snap, &ctx.all_xids, ctx.session)
                .cloned()
            {
                if ct.triggers.iter().any(|t| t.name == new_name) {
                    return Err(exec_err(
                        "42723",
                        format!(
                            "trigger \"{}\" for relation \"{}\" already exists",
                            new_name, child
                        ),
                    ));
                }
                if let Some(cp) = &ct.partition {
                    stack.extend(cp.children.iter().cloned());
                }
            }
        }
    }
    if prev.triggers.iter().any(|t| t.name == new_name) {
        return Err(exec_err(
            "42723",
            format!(
                "trigger \"{}\" for relation \"{}\" already exists",
                new_name, table
            ),
        ));
    }
    let mut next = prev.clone();
    for t in next.triggers.iter_mut() {
        if t.name == name {
            t.name = new_name.to_string();
        }
    }
    commit_table_version(eng, ctx, table, prev, next);
    Ok(ExecResult::Command {
        tag: "ALTER TRIGGER".to_string(),
    })
}

/// v1.00: `DROP TRIGGER [IF EXISTS] name ON table [CASCADE|RESTRICT]`
/// (PG19). Like creation, this versions the table, so it is
/// transactional, WAL-logged, and checkpointed.
pub(crate) fn exec_drop_trigger(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    table: &str,
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    require_table_owner(eng, ctx, table)?;
    let prev = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?
        .clone();
    if !prev.triggers.iter().any(|t| t.name == name) {
        if !if_exists {
            return Err(exec_err(
                "42704",
                format!(
                    "trigger \"{}\" for relation \"{}\" does not exist",
                    name, table
                ),
            ));
        }
        // PG19: IF EXISTS on a missing trigger is a NOTICE, not silent.
        ctx.notices.push(format!(
            "trigger \"{}\" for relation \"{}\" does not exist, skipping",
            name, table
        ));
        return Ok(ExecResult::Command {
            tag: "DROP TRIGGER".to_string(),
        });
    }
    // v1.00: no trigger dependents are tracked (constraint triggers are
    // cataloged but inert), so CASCADE/RESTRICT have no observable
    // difference yet.
    let _ = cascade;
    let mut next = prev.clone();
    next.triggers.retain(|t| t.name != name);
    commit_table_version(eng, ctx, table, prev, next);
    Ok(ExecResult::Command {
        tag: "DROP TRIGGER".to_string(),
    })
}

/// v0.86: `CREATE OPERATOR`. The procedure must name an existing
/// function (42883); the argument types must resolve (42704); a
/// duplicate (name, argtypes) is 42723.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_operator(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    procedure: &str,
    leftarg: Option<&str>,
    rightarg: Option<&str>,
    commutator: Option<&str>,
    negator: Option<&str>,
    hashes: bool,
    merges: bool,
) -> Result<ExecResult, ExecError> {
    // v0.87: the procedure must exist with a signature matching the
    // operator's argument types (PG19).
    {
        let proc_args: Vec<String> = [leftarg, rightarg]
            .into_iter()
            .flatten()
            .map(|s| s.to_string())
            .collect();
        if find_function_by_signature(eng, procedure, &proc_args).is_none() {
            return Err(exec_err(
                "42883",
                format!("function \"{}\" does not exist", procedure),
            ));
        }
    }
    if let Some(t) = leftarg {
        resolve_func_type_name(eng, ctx.snap, ctx.own, ctx.session, t)?;
    }
    if let Some(t) = rightarg {
        resolve_func_type_name(eng, ctx.snap, ctx.own, ctx.session, t)?;
    }
    let prev = eng.db.operators.get(name).cloned().unwrap_or_default();
    let dup = prev
        .iter()
        .any(|d| d.leftarg.as_deref() == leftarg && d.rightarg.as_deref() == rightarg);
    if dup {
        return Err(exec_err(
            "42723",
            format!("operator {} already exists", name),
        ));
    }
    let mut defs = prev.clone();
    defs.push(crate::storage::OperDef {
        name: name.to_string(),
        procedure: procedure.to_string(),
        leftarg: leftarg.map(|s| s.to_string()),
        rightarg: rightarg.map(|s| s.to_string()),
        commutator: commutator.map(|s| s.to_string()),
        negator: negator.map(|s| s.to_string()),
        hashes,
        merges,
    });
    eng.db.operators.insert(name.to_string(), defs);
    ctx.writes.push(WriteOp::CreateOperator {
        name: name.to_string(),
        prev,
    });
    Ok(ExecResult::Command {
        tag: "CREATE OPERATOR".to_string(),
    })
}

/// v0.86: `DROP OPERATOR [IF EXISTS] name (lefttype, righttype)`.
/// Removes the matching (name, argtypes) definition (42883 when absent
/// and IF EXISTS was not given, like PG19).
/// v0.87: `DROP OPERATOR [IF EXISTS] name (ltype, rtype) [CASCADE|RESTRICT]`.
/// PG19 RESTRICT (the default) fails with 2BP01 if dependent objects exist;
/// CASCADE drops them. (Operators currently have no dependents in the
/// catalog, so RESTRICT and CASCADE behave the same.)
pub(crate) fn exec_drop_operator(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    leftarg: Option<&str>,
    rightarg: Option<&str>,
    if_exists: bool,
    _cascade: bool,
) -> Result<ExecResult, ExecError> {
    let prev = eng.db.operators.get(name).cloned().unwrap_or_default();
    let pos = prev
        .iter()
        .position(|d| d.leftarg.as_deref() == leftarg && d.rightarg.as_deref() == rightarg);
    match pos {
        None => {
            if !if_exists {
                return Err(exec_err(
                    "42883",
                    format!("operator {} does not exist", name),
                ));
            }
        }
        Some(i) => {
            let mut next = prev.clone();
            next.remove(i);
            if next.is_empty() {
                eng.db.operators.remove(name);
            } else {
                eng.db.operators.insert(name.to_string(), next);
            }
            ctx.writes.push(WriteOp::DropOperator {
                name: name.to_string(),
                prev,
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP OPERATOR".to_string(),
    })
}

/// v1.02: rewrite one expression's named argument references to
/// positional parameters (`argname` -> `Param(n)`; `argname.col` ->
/// `FieldAccess(Param(n), col)`). Hoisted out of
/// `rewrite_func_arg_refs` so RAISE args — stored as bare `Expr`s,
/// not wrapped SELECTs — get the identical rewrite at CREATE and on
/// WAL-replay rebuild.
///
/// v1.02 also widens the recursion: the v1.01 version only descended
/// into Column/FieldAccess/Arith/And/Or/Concat, so a named arg inside
/// a comparison (`return x > y`) survived as a column reference and
/// failed at call time with 42703. The rewriter now descends through
/// every scalar expression form. Deliberate boundary (unchanged from
/// v1.01): it does not descend into subqueries (`ScalarSub`,
/// `InSub.sub`, `Quantified.sub`, `Exists`, `ArraySubquery`) — the
/// bounded subset keeps those out of reach of the rewrite, and a
/// `Column` matching an arg name inside a subquery keeps PG's
/// column-first resolution.
pub(crate) fn rewrite_func_arg_expr(e: &mut Expr, arg_names: &[Option<String>]) {
    fn arg_pos(arg_names: &[Option<String>], name: &str) -> Option<u32> {
        arg_names
            .iter()
            .position(|n| n.as_deref() == Some(name))
            .map(|i| (i + 1) as u32)
    }
    match e {
        Expr::Column { table, name } => {
            if let Some(t) = table {
                if let Some(n) = arg_pos(arg_names, t) {
                    *e = Expr::FieldAccess {
                        expr: Box::new(Expr::Param(n)),
                        field: std::mem::take(name),
                    };
                    return;
                }
            } else if let Some(n) = arg_pos(arg_names, name) {
                *e = Expr::Param(n);
                return;
            }
        }
        Expr::FieldAccess { expr, .. } => rewrite_func_arg_expr(expr, arg_names),
        Expr::Arith { left, right, .. } => {
            rewrite_func_arg_expr(left, arg_names);
            rewrite_func_arg_expr(right, arg_names);
        }
        Expr::Cmp { left, right, .. } => {
            rewrite_func_arg_expr(left, arg_names);
            rewrite_func_arg_expr(right, arg_names);
        }
        Expr::And(left, right) | Expr::Or(left, right) | Expr::Concat(left, right) => {
            rewrite_func_arg_expr(left, arg_names);
            rewrite_func_arg_expr(right, arg_names);
        }
        Expr::JsonOp { left, right, .. } => {
            rewrite_func_arg_expr(left, arg_names);
            rewrite_func_arg_expr(right, arg_names);
        }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::BitNot(inner) => {
            rewrite_func_arg_expr(inner, arg_names);
        }
        Expr::IsNull { expr, .. } | Expr::IsBool { expr, .. } => {
            rewrite_func_arg_expr(expr, arg_names);
        }
        Expr::IsDistinctFrom { left, right, .. } => {
            rewrite_func_arg_expr(left, arg_names);
            rewrite_func_arg_expr(right, arg_names);
        }
        Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => {
            rewrite_func_arg_expr(expr, arg_names);
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            rewrite_func_arg_expr(expr, arg_names);
            rewrite_func_arg_expr(pattern, arg_names);
            if let Some(esc) = escape {
                rewrite_func_arg_expr(esc, arg_names);
            }
        }
        Expr::Regex { expr, pattern, .. } => {
            rewrite_func_arg_expr(expr, arg_names);
            rewrite_func_arg_expr(pattern, arg_names);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            rewrite_func_arg_expr(expr, arg_names);
            rewrite_func_arg_expr(low, arg_names);
            rewrite_func_arg_expr(high, arg_names);
        }
        Expr::Func { args, .. } => {
            for a in args {
                rewrite_func_arg_expr(a, arg_names);
            }
        }
        Expr::NamedArg { expr, .. } => rewrite_func_arg_expr(expr, arg_names),
        Expr::Extract { from, .. } => rewrite_func_arg_expr(from, arg_names),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(op) = operand {
                rewrite_func_arg_expr(op, arg_names);
            }
            for (k, v) in whens {
                rewrite_func_arg_expr(k, arg_names);
                rewrite_func_arg_expr(v, arg_names);
            }
            if let Some(el) = else_ {
                rewrite_func_arg_expr(el, arg_names);
            }
        }
        Expr::Row(exprs) => {
            for x in exprs {
                rewrite_func_arg_expr(x, arg_names);
            }
        }
        Expr::ArrayCtor { elems, .. } => {
            for x in elems {
                rewrite_func_arg_expr(x, arg_names);
            }
        }
        Expr::Subscript { array, indices } => {
            rewrite_func_arg_expr(array, arg_names);
            for i in indices {
                rewrite_func_arg_expr(i, arg_names);
            }
        }
        Expr::Slice { array, bounds } => {
            rewrite_func_arg_expr(array, arg_names);
            for (lo, hi) in bounds {
                if let Some(l) = lo {
                    rewrite_func_arg_expr(l, arg_names);
                }
                if let Some(h) = hi {
                    rewrite_func_arg_expr(h, arg_names);
                }
            }
        }
        Expr::UserOp { left, right, .. } => {
            rewrite_func_arg_expr(left, arg_names);
            rewrite_func_arg_expr(right, arg_names);
        }
        // Subquery forms: rewrite the outer expression only, never
        // the subquery (see the doc comment above).
        Expr::InSub { expr, .. } => rewrite_func_arg_expr(expr, arg_names),
        Expr::Quantified { left, .. } => rewrite_func_arg_expr(left, arg_names),
        Expr::Agg { arg, arg2, .. } => {
            if let Some(a) = arg {
                rewrite_func_arg_expr(a, arg_names);
            }
            if let Some(a) = arg2 {
                rewrite_func_arg_expr(a, arg_names);
            }
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args.iter_mut().chain(partition_by.iter_mut()) {
                rewrite_func_arg_expr(a, arg_names);
            }
            for o in order_by {
                rewrite_func_arg_expr(&mut o.expr, arg_names);
            }
        }
        // v1.30: ordered-set aggregate — rewrite named-arg refs in
        // direct args, the WITHIN GROUP sort keys, and the FILTER.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            for a in direct_args {
                rewrite_func_arg_expr(a, arg_names);
            }
            for o in within_order_by {
                rewrite_func_arg_expr(&mut o.expr, arg_names);
            }
            if let Some(f) = filter {
                rewrite_func_arg_expr(f, arg_names);
            }
        }
        _ => {}
    }
}

/// v2.11: map-based counterpart of `rewrite_func_arg_expr` (see
/// `rewrite_plpgsql_body_refs`).
/// v2.11: block label -> the name -> $n map of the block carrying
/// that label (keys lowercased). `label.var` references resolve
/// against the labeled block's own scope, not the current one.
pub(crate) type LabelScopes =
    std::collections::HashMap<String, std::collections::HashMap<String, u32>>;

pub(crate) fn rewrite_expr_map(
    e: &mut Expr,
    map: &std::collections::HashMap<String, u32>,
    labels: &LabelScopes,
) {
    fn arg_pos(map: &std::collections::HashMap<String, u32>, name: &str) -> Option<u32> {
        map.get(name).copied()
    }
    match e {
        Expr::Column { table, name } => {
            if let Some(t) = table {
                if let Some(n) = arg_pos(map, t) {
                    *e = Expr::FieldAccess {
                        expr: Box::new(Expr::Param(n)),
                        field: std::mem::take(name),
                    };
                    return;
                }
                // v2.11: label-qualified variable reference
                // (`outer.rec` or `"outer".rec.field` shapes): the
                // qualifier is a block label, the next component is
                // the variable.
                if let Some(lmap) = labels.get(&t.to_ascii_lowercase()) {
                    if let Some(n) = lmap.get(name.as_str()).copied() {
                        *e = Expr::Param(n);
                        return;
                    }
                }
                if let Some((lab, var)) = t.split_once('.') {
                    if let Some(lmap) = labels.get(&lab.to_ascii_lowercase()) {
                        if let Some(n) = lmap.get(var).copied() {
                            let field = std::mem::take(name);
                            *e = Expr::FieldAccess {
                                expr: Box::new(Expr::Param(n)),
                                field,
                            };
                            return;
                        }
                    }
                }
            } else if let Some(n) = arg_pos(map, name) {
                *e = Expr::Param(n);
                return;
            }
        }
        Expr::FieldAccess { expr, .. } => rewrite_expr_map(expr, map, labels),
        Expr::Arith { left, right, .. } => {
            rewrite_expr_map(left, map, labels);
            rewrite_expr_map(right, map, labels);
        }
        Expr::Cmp { left, right, .. } => {
            rewrite_expr_map(left, map, labels);
            rewrite_expr_map(right, map, labels);
        }
        Expr::And(left, right) | Expr::Or(left, right) | Expr::Concat(left, right) => {
            rewrite_expr_map(left, map, labels);
            rewrite_expr_map(right, map, labels);
        }
        Expr::JsonOp { left, right, .. } => {
            rewrite_expr_map(left, map, labels);
            rewrite_expr_map(right, map, labels);
        }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::BitNot(inner) => {
            rewrite_expr_map(inner, map, labels);
        }
        Expr::IsNull { expr, .. } | Expr::IsBool { expr, .. } => {
            rewrite_expr_map(expr, map, labels);
        }
        Expr::IsDistinctFrom { left, right, .. } => {
            rewrite_expr_map(left, map, labels);
            rewrite_expr_map(right, map, labels);
        }
        Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => {
            rewrite_expr_map(expr, map, labels);
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            rewrite_expr_map(expr, map, labels);
            rewrite_expr_map(pattern, map, labels);
            if let Some(esc) = escape {
                rewrite_expr_map(esc, map, labels);
            }
        }
        Expr::Regex { expr, pattern, .. } => {
            rewrite_expr_map(expr, map, labels);
            rewrite_expr_map(pattern, map, labels);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            rewrite_expr_map(expr, map, labels);
            rewrite_expr_map(low, map, labels);
            rewrite_expr_map(high, map, labels);
        }
        Expr::Func { args, .. } => {
            for a in args {
                rewrite_expr_map(a, map, labels);
            }
        }
        Expr::NamedArg { expr, .. } => rewrite_expr_map(expr, map, labels),
        Expr::Extract { from, .. } => rewrite_expr_map(from, map, labels),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(op) = operand {
                rewrite_expr_map(op, map, labels);
            }
            for (k, v) in whens {
                rewrite_expr_map(k, map, labels);
                rewrite_expr_map(v, map, labels);
            }
            if let Some(el) = else_ {
                rewrite_expr_map(el, map, labels);
            }
        }
        Expr::Row(exprs) => {
            for x in exprs {
                rewrite_expr_map(x, map, labels);
            }
        }
        Expr::ArrayCtor { elems, .. } => {
            for x in elems {
                rewrite_expr_map(x, map, labels);
            }
        }
        Expr::Subscript { array, indices } => {
            rewrite_expr_map(array, map, labels);
            for i in indices {
                rewrite_expr_map(i, map, labels);
            }
        }
        Expr::Slice { array, bounds } => {
            rewrite_expr_map(array, map, labels);
            for (lo, hi) in bounds {
                if let Some(l) = lo {
                    rewrite_expr_map(l, map, labels);
                }
                if let Some(h) = hi {
                    rewrite_expr_map(h, map, labels);
                }
            }
        }
        Expr::UserOp { left, right, .. } => {
            rewrite_expr_map(left, map, labels);
            rewrite_expr_map(right, map, labels);
        }
        // Subquery forms: rewrite the outer expression only, never
        // the subquery (see the doc comment above).
        Expr::InSub { expr, .. } => rewrite_expr_map(expr, map, labels),
        Expr::Quantified { left, .. } => rewrite_expr_map(left, map, labels),
        Expr::Agg { arg, arg2, .. } => {
            if let Some(a) = arg {
                rewrite_expr_map(a, map, labels);
            }
            if let Some(a) = arg2 {
                rewrite_expr_map(a, map, labels);
            }
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args.iter_mut().chain(partition_by.iter_mut()) {
                rewrite_expr_map(a, map, labels);
            }
            for o in order_by {
                rewrite_expr_map(&mut o.expr, map, labels);
            }
        }
        // v1.30: ordered-set aggregate — rewrite named-arg refs in
        // direct args, the WITHIN GROUP sort keys, and the FILTER.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            for a in direct_args {
                rewrite_expr_map(a, map, labels);
            }
            for o in within_order_by {
                rewrite_expr_map(&mut o.expr, map, labels);
            }
            if let Some(f) = filter {
                rewrite_expr_map(f, map, labels);
            }
        }
        _ => {}
    }
}

/// v0.86: rewrite named argument references in a parsed SQL function
/// body to positional parameters: `argname.col` becomes
/// `FieldAccess(Param(n), col)` and a bare `argname` becomes
/// `Param(n)`. Only qualifiers/names that exactly match an argument
/// name are rewritten; real table references are untouched.
pub(crate) fn rewrite_func_arg_refs(stmt: &mut Stmt, arg_names: &[Option<String>]) {
    // Bodies are SELECT/INSERT/UPDATE/DELETE (validated at CREATE);
    // walk the top-level expression positions. A full Stmt-wide walk
    // is unnecessary for the bounded body shapes we accept (no
    // subquery recursion, like the SELECT path).
    match stmt {
        Stmt::Select(sel) => rewrite_select_refs(sel, arg_names),
        Stmt::Insert {
            rows,
            select,
            on_conflict,
            returning,
            ..
        } => {
            for row in rows {
                for v in row {
                    if let crate::sql::InsertValue::Expr(e) = v {
                        rewrite_func_arg_expr(e, arg_names);
                    }
                }
            }
            if let Some(sel) = select {
                rewrite_select_refs(sel, arg_names);
            }
            if let Some(oc) = on_conflict {
                if let crate::sql::ConflictAction::DoUpdate { sets, where_ } = &mut oc.action {
                    for (_, e) in sets.iter_mut() {
                        rewrite_func_arg_expr(e, arg_names);
                    }
                    if let Some(w) = where_ {
                        rewrite_func_arg_expr(w, arg_names);
                    }
                }
            }
            rewrite_returning_refs(returning, arg_names);
        }
        Stmt::Update {
            sets,
            from,
            where_,
            returning,
            ..
        } => {
            for (_, e) in sets.iter_mut() {
                rewrite_func_arg_expr(e, arg_names);
            }
            rewrite_from_fn_refs(from, arg_names);
            if let Some(w) = where_ {
                rewrite_func_arg_expr(w, arg_names);
            }
            rewrite_returning_refs(returning, arg_names);
        }
        Stmt::Delete {
            using,
            where_,
            returning,
            ..
        } => {
            rewrite_from_fn_refs(using, arg_names);
            if let Some(w) = where_ {
                rewrite_func_arg_expr(w, arg_names);
            }
            rewrite_returning_refs(returning, arg_names);
        }
        // v2.01: MERGE bodies — ON, WHEN conditions, SET exprs, INSERT
        // values, source FROM items, RETURNING.
        Stmt::Merge {
            source,
            on,
            whens,
            returning,
            ..
        } => {
            rewrite_from_fn_refs(source, arg_names);
            rewrite_func_arg_expr(on, arg_names);
            for w in whens.iter_mut() {
                if let Some(c) = &mut w.condition {
                    rewrite_func_arg_expr(c, arg_names);
                }
                match &mut w.action {
                    crate::sql::MergeAction::Update { sets } => {
                        for (_, e) in sets.iter_mut() {
                            rewrite_func_arg_expr(e, arg_names);
                        }
                    }
                    crate::sql::MergeAction::Insert { values, .. } => {
                        if let Some(vs) = values {
                            for e in vs.iter_mut() {
                                rewrite_func_arg_expr(e, arg_names);
                            }
                        }
                    }
                    _ => {}
                }
            }
            rewrite_returning_refs(returning, arg_names);
        }
        _ => {}
    }
}

/// v1.33: the SELECT half of `rewrite_func_arg_refs` (select list +
/// WHERE), shared with DML bodies' nested SELECT sources.
pub(crate) fn rewrite_select_refs(sel: &mut crate::sql::SelectStmt, arg_names: &[Option<String>]) {
    for item in &mut sel.items {
        if let crate::sql::SelectItem::Expr { expr, .. } = item {
            rewrite_func_arg_expr(expr, arg_names);
        }
    }
    if let Some(w) = &mut sel.where_ {
        rewrite_func_arg_expr(w, arg_names);
    }
    // v2.04: FROM-clause function arguments can reference named
    // args too (`pg_get_publication_tables(pubnames, ...)`) — the
    // plpgsql rewriter already walks these; the SQL-body path
    // missed them, surfacing as 42703 at call time.
    rewrite_from_fn_refs(&mut sel.from, arg_names);
}

/// v1.33: rewrite named arg references in a DML RETURNING list.
pub(crate) fn rewrite_returning_refs(
    returning: &mut [crate::sql::SelectItem],
    arg_names: &[Option<String>],
) {
    for item in returning {
        if let crate::sql::SelectItem::Expr { expr, .. } = item {
            rewrite_func_arg_expr(expr, arg_names);
        }
    }
}

/// v1.33: rewrite named arg references in FROM/USING function call
/// arguments (e.g. `generate_series(1, n)`), mirroring the plpgsql
/// body rewriter's FROM walk.
pub(crate) fn rewrite_from_fn_refs(
    from: &mut [crate::sql::FromItem],
    arg_names: &[Option<String>],
) {
    for f in from {
        match f {
            crate::sql::FromItem::Function { args, .. } => {
                for a in args.iter_mut() {
                    rewrite_func_arg_expr(a, arg_names);
                }
            }
            // v2.04: function items nested in a JOIN tree (the
            // publication test_gpt shape: SRF JOIN pg_publication
            // JOIN pg_class) need the same walk.
            crate::sql::FromItem::Join { left, right, .. } => {
                rewrite_from_fn_refs(std::slice::from_mut(left), arg_names);
                rewrite_from_fn_refs(std::slice::from_mut(right), arg_names);
            }
            _ => {}
        }
    }
}

/// v2.11: statement-level named-reference rewrite driven by an
/// explicit name -> $n map (the plpgsql counterpart of
/// `rewrite_func_arg_refs`, which is slice-based and cannot express
/// ALIAS decls or block shadowing). Coverage mirrors
/// `rewrite_func_arg_refs` / `rewrite_select_refs` /
/// `rewrite_returning_refs` / `rewrite_from_fn_refs`.
pub(crate) fn rewrite_stmt_map(
    stmt: &mut crate::sql::Stmt,
    map: &std::collections::HashMap<String, u32>,
    labels: &LabelScopes,
) {
    use crate::sql::Stmt as S;
    match stmt {
        S::Select(sel) => rewrite_select_map(sel, map, labels),
        S::Explain { stmt: inner, .. } => rewrite_stmt_map(inner, map, labels),
        S::Insert {
            rows,
            select,
            on_conflict,
            returning,
            ..
        } => {
            for row in rows {
                for v in row {
                    if let crate::sql::InsertValue::Expr(e) = v {
                        rewrite_expr_map(e, map, labels);
                    }
                }
            }
            if let Some(sel) = select {
                rewrite_select_map(sel, map, labels);
            }
            if let Some(oc) = on_conflict {
                if let crate::sql::ConflictAction::DoUpdate { sets, where_ } = &mut oc.action {
                    for (_, e) in sets.iter_mut() {
                        rewrite_expr_map(e, map, labels);
                    }
                    if let Some(w) = where_ {
                        rewrite_expr_map(w, map, labels);
                    }
                }
            }
            for item in returning.iter_mut() {
                if let crate::sql::SelectItem::Expr { expr, .. } = item {
                    rewrite_expr_map(expr, map, labels);
                }
            }
        }
        S::Update {
            sets,
            from,
            where_,
            returning,
            ..
        } => {
            for (_, e) in sets.iter_mut() {
                rewrite_expr_map(e, map, labels);
            }
            rewrite_from_map(from, map, labels);
            if let Some(w) = where_ {
                rewrite_expr_map(w, map, labels);
            }
            for item in returning.iter_mut() {
                if let crate::sql::SelectItem::Expr { expr, .. } = item {
                    rewrite_expr_map(expr, map, labels);
                }
            }
        }
        S::Delete {
            using,
            where_,
            returning,
            ..
        } => {
            rewrite_from_map(using, map, labels);
            if let Some(w) = where_ {
                rewrite_expr_map(w, map, labels);
            }
            for item in returning.iter_mut() {
                if let crate::sql::SelectItem::Expr { expr, .. } = item {
                    rewrite_expr_map(expr, map, labels);
                }
            }
        }
        S::Merge {
            source,
            on,
            whens,
            returning,
            ..
        } => {
            rewrite_from_map(source, map, labels);
            rewrite_expr_map(on, map, labels);
            for w in whens.iter_mut() {
                if let Some(c) = &mut w.condition {
                    rewrite_expr_map(c, map, labels);
                }
                match &mut w.action {
                    crate::sql::MergeAction::Update { sets } => {
                        for (_, e) in sets.iter_mut() {
                            rewrite_expr_map(e, map, labels);
                        }
                    }
                    crate::sql::MergeAction::Insert { values, .. } => {
                        if let Some(vs) = values {
                            for e in vs.iter_mut() {
                                rewrite_expr_map(e, map, labels);
                            }
                        }
                    }
                    _ => {}
                }
            }
            for item in returning.iter_mut() {
                if let crate::sql::SelectItem::Expr { expr, .. } = item {
                    rewrite_expr_map(expr, map, labels);
                }
            }
        }
        _ => {}
    }
}

/// v2.11: the SELECT half of `rewrite_stmt_map`.
pub(crate) fn rewrite_select_map(
    sel: &mut crate::sql::SelectStmt,
    map: &std::collections::HashMap<String, u32>,
    labels: &LabelScopes,
) {
    for item in &mut sel.items {
        if let crate::sql::SelectItem::Expr { expr, .. } = item {
            rewrite_expr_map(expr, map, labels);
        }
    }
    if let Some(w) = &mut sel.where_ {
        rewrite_expr_map(w, map, labels);
    }
    rewrite_from_map(&mut sel.from, map, labels);
}

/// v2.11: FROM-clause function-argument rewrite for `rewrite_stmt_map`.
pub(crate) fn rewrite_from_map(
    from: &mut [crate::sql::FromItem],
    map: &std::collections::HashMap<String, u32>,
    labels: &LabelScopes,
) {
    for f in from {
        match f {
            crate::sql::FromItem::Function { args, .. } => {
                for a in args.iter_mut() {
                    rewrite_expr_map(a, map, labels);
                }
            }
            crate::sql::FromItem::Join { left, right, .. } => {
                rewrite_from_map(std::slice::from_mut(left), map, labels);
                rewrite_from_map(std::slice::from_mut(right), map, labels);
            }
            _ => {}
        }
    }
}

/// v1.03/v2.11: rewrite named references in a parsed plpgsql body to
/// positional parameters. v2.11 makes the rewrite map-based: a
/// name -> $n map is built from the arguments and the flat decl list
/// (ALIAS decls resolve to their target's slot; an implicit decl
/// whose name matches an argument yields to the argument), and each
/// nested block rewrites with a cloned map where its own decls
/// shadow outer names. Declaration defaults are rewritten with the
/// progressive map of the decls visible at their point. Applied at
/// CREATE and on WAL/checkpoint rebuild; idempotent (an
/// already-rewritten `Param` is untouched).
pub(crate) fn rewrite_plpgsql_body_refs(
    pb: &mut crate::sql::PlpgsqlBody,
    arg_names: &[Option<String>],
) {
    use std::collections::HashMap;
    let nargs = arg_names.len();
    // Label scopes for `label.var` resolution: the body label is
    // registered once the base map exists; block labels register
    // as the walk enters them.
    let mut label_scopes: LabelScopes = HashMap::new();

    // v2.12: substitute a cursor variable's name with its `$n` slot
    // form when the rewrite map knows it; unresolved names (dynamic
    // cases) pass through for the executor's name lookup.
    fn cursor_var_to_slot(var: &mut String, map: &HashMap<String, u32>) {
        if let Some(slot) = map.get(var.as_str()) {
            *var = format!("${}", slot);
        }
    }

    // Resolve one decl into `map` (first definition wins unless this
    // is a block-scope insert, handled by callers cloning the map).
    fn decl_slot(
        d: &crate::sql::PlpgsqlDecl,
        idx: usize,
        nargs: usize,
        map: &HashMap<String, u32>,
    ) -> Option<u32> {
        if let Some(target) = &d.alias_for {
            if let Some(digits) = target.strip_prefix('$') {
                if let Ok(n) = digits.parse::<u32>() {
                    return Some(n);
                }
            }
            return map.get(target).copied();
        }
        Some((nargs + idx + 1) as u32)
    }

    // Base map: arguments first, then decls in order (first wins).
    let mut base: HashMap<String, u32> = HashMap::new();
    for (i, n) in arg_names.iter().enumerate() {
        if let Some(name) = n {
            base.entry(name.clone()).or_insert((i + 1) as u32);
        }
    }
    if let Some(l) = &pb.label {
        label_scopes.insert(l.to_ascii_lowercase(), base.clone());
    }
    // Declaration defaults: rewrite each with the progressive map,
    // then fold the decl into both maps.
    // v2.12: a non-implicit, non-alias declaration whose name matches
    // an ARGUMENT shadows it (PG19): the first such declaration
    // replaces the argument's entry in both maps. Later declarations
    // with the same name (nested blocks) keep first-wins — the block
    // walk clones its own map for those.
    let arg_name_set: std::collections::HashSet<&str> =
        arg_names.iter().flatten().map(|s| s.as_str()).collect();
    let mut shadowed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let ndecls = pb.decls.len();
    let mut progressive = base.clone();
    for (idx, d) in pb.decls.iter_mut().enumerate() {
        if let Some(def) = &mut d.default {
            rewrite_stmt_map(def, &progressive, &label_scopes);
        }
        // v2.12: rewrite a bound cursor's query with its parameter
        // names mapped to the extra slots past the decl vector
        // (slot = nargs + total_decls + param_idx + 1); cursor
        // parameters shadow same-named arguments inside the query.
        if d.is_cursor {
            if let Some(q) = &mut d.cursor_query {
                let mut cmap = progressive.clone();
                for (j, (pn, _)) in d.cursor_params.iter().enumerate() {
                    cmap.insert(pn.clone(), (nargs + ndecls + j + 1) as u32);
                }
                rewrite_stmt_map(q, &cmap, &label_scopes);
            }
        }
        let slot = decl_slot(d, idx, nargs, &progressive);
        if d.alias_for.is_some() {
            // v2.11: record the resolved target slot so the executor
            // can resolve the alias without argument names.
            d.alias_slot = slot;
        }
        if let Some(slot) = slot {
            if d.implicit && base.contains_key(&d.name) {
                // An implicit FOR variable named like an argument:
                // the argument keeps the name (PG resolves the loop
                // variable to the argument in that case).
            } else if !d.implicit
                && d.alias_for.is_none()
                && arg_name_set.contains(d.name.as_str())
                && !shadowed.contains(&d.name)
            {
                base.insert(d.name.clone(), slot);
                progressive.insert(d.name.clone(), slot);
                shadowed.insert(d.name.clone());
            } else {
                base.entry(d.name.clone()).or_insert(slot);
            }
            progressive.entry(d.name.clone()).or_insert(slot);
        }
    }

    if let Some(l) = &pb.label {
        label_scopes.insert(l.to_ascii_lowercase(), base.clone());
    }

    fn walk(
        stmts: &mut [crate::sql::PlpgsqlStmt],
        map: &HashMap<String, u32>,
        labels: &mut LabelScopes,
        pb_decls: &[crate::sql::PlpgsqlDecl],
        nargs: usize,
    ) {
        for s in stmts {
            walk_stmt(s, map, labels, pb_decls, nargs);
        }
    }
    fn walk_stmt(
        s: &mut crate::sql::PlpgsqlStmt,
        map: &HashMap<String, u32>,
        labels: &mut LabelScopes,
        pb_decls: &[crate::sql::PlpgsqlDecl],
        nargs: usize,
    ) {
        use crate::sql::PlpgsqlStmt as P;
        // v2.11: `label.var := rhs` parses as a field assignment;
        // when the first component is a block label rather than a
        // declared record variable, it is an assignment to that
        // block's variable — convert it to a direct slot assignment
        // with the label scope's resolved slot.
        if let P::AssignField { var, field, .. } = s {
            let slot = labels
                .get(&var.to_ascii_lowercase())
                .and_then(|lm| lm.get(field.as_str()).copied());
            if let Some(slot) = slot {
                if !pb_decls.iter().any(|d| &d.name == var) {
                    let old = std::mem::replace(s, P::ReturnBare);
                    if let P::AssignField { select, .. } = old {
                        *s = P::AssignParam {
                            index: slot,
                            select,
                        };
                    }
                }
            }
        }
        match s {
            P::Return(sel) | P::ReturnNext(sel) | P::Perform(sel) => {
                rewrite_stmt_map(sel, map, &*labels);
            }
            P::ReturnNextBare => {}
            P::ReturnQuery(sel) => {
                rewrite_stmt_map(sel.as_mut(), map, &*labels);
            }
            P::ReturnQueryExecute { expr, using } => {
                rewrite_stmt_map(expr, map, &*labels);
                for u in using {
                    rewrite_stmt_map(u, map, &*labels);
                }
            }
            // v2.12: cursor statements — the cursor VARIABLE resolves
            // to its `$n` slot form when the name is known (declared
            // cursor, refcursor variable, or argument); embedded
            // expressions rewrite normally.
            P::OpenBound { var, args } => {
                cursor_var_to_slot(var, map);
                for (_, a) in args {
                    rewrite_stmt_map(a, map, &*labels);
                }
            }
            P::OpenFor {
                var,
                query,
                dynamic,
                using,
                ..
            } => {
                cursor_var_to_slot(var, map);
                if let Some(q) = query {
                    rewrite_stmt_map(q.as_mut(), map, &*labels);
                }
                if let Some(d) = dynamic {
                    rewrite_stmt_map(d, map, &*labels);
                }
                for u in using {
                    rewrite_stmt_map(u, map, &*labels);
                }
            }
            P::Fetch { var, count, .. } | P::Move { var, count, .. } => {
                cursor_var_to_slot(var, map);
                if let Some(c) = count {
                    rewrite_stmt_map(c, map, &*labels);
                }
            }
            P::Close { var } => {
                cursor_var_to_slot(var, map);
            }
            P::ForCursor {
                cursor, args, body, ..
            } => {
                cursor_var_to_slot(cursor, map);
                for (_, a) in args {
                    rewrite_stmt_map(a, map, &*labels);
                }
                walk(body, map, labels, pb_decls, nargs);
            }
            P::Assign { select, .. }
            | P::AssignField { select, .. }
            | P::AssignParam { select, .. } => {
                rewrite_stmt_map(select, map, &*labels);
            }
            P::Assert { cond, message } => {
                rewrite_stmt_map(cond, map, &*labels);
                if let Some(m) = message {
                    rewrite_stmt_map(m, map, &*labels);
                }
            }
            P::Raise {
                args,
                sqlstate,
                using_message,
                ..
            } => {
                for a in args {
                    rewrite_expr_map(a, map, &*labels);
                }
                if let Some(crate::sql::RaiseCode::Expr(st)) = sqlstate {
                    rewrite_stmt_map(st, map, &*labels);
                }
                if let Some(m) = using_message {
                    rewrite_stmt_map(m, map, &*labels);
                }
            }
            P::If {
                branches,
                else_body,
            }
            | P::Case {
                branches,
                else_body,
            } => {
                for (cond, body) in branches {
                    rewrite_stmt_map(cond, map, &*labels);
                    walk(body, map, labels, pb_decls, nargs);
                }
                walk(else_body, map, labels, pb_decls, nargs);
            }
            P::SelectInto { query, .. } => {
                rewrite_stmt_map(query, map, &*labels);
            }
            P::Dml { stmt, .. } => {
                rewrite_stmt_map(stmt, map, &*labels);
            }
            P::ExecuteInto { expr, using, .. } => {
                rewrite_stmt_map(expr, map, &*labels);
                for u in using {
                    rewrite_stmt_map(u, map, &*labels);
                }
            }
            P::PerformQuery(q) => {
                rewrite_stmt_map(q, map, &*labels);
            }
            P::Block {
                label,
                decl_idx,
                stmts,
                handlers,
            } => {
                let mut inner = map.clone();
                for &idx in decl_idx.iter() {
                    if let Some(d) = pb_decls.get(idx) {
                        if let Some(slot) = decl_slot(d, idx, nargs, &inner) {
                            // Block decls shadow outer names.
                            inner.insert(d.name.clone(), slot);
                        }
                    }
                }
                let saved = label
                    .as_ref()
                    .map(|l| labels.insert(l.to_ascii_lowercase(), inner.clone()));
                walk(stmts, &inner, labels, pb_decls, nargs);
                for h in handlers {
                    walk(&mut h.stmts, &inner, labels, pb_decls, nargs);
                }
                if let Some(l) = label {
                    match saved {
                        Some(Some(prev)) => {
                            labels.insert(l.to_ascii_lowercase(), prev);
                        }
                        Some(None) => {
                            labels.remove(&l.to_ascii_lowercase());
                        }
                        None => {}
                    }
                }
            }
            P::ForQuery { query, body, .. } => {
                rewrite_stmt_map(query, map, &*labels);
                walk(body, map, labels, pb_decls, nargs);
            }
            P::ForRange {
                low,
                high,
                step,
                body,
                ..
            } => {
                rewrite_stmt_map(low, map, &*labels);
                rewrite_stmt_map(high, map, &*labels);
                if let Some(st) = step {
                    rewrite_stmt_map(st, map, &*labels);
                }
                walk(body, map, labels, pb_decls, nargs);
            }
            P::While { cond, body } => {
                rewrite_stmt_map(cond, map, &*labels);
                walk(body, map, labels, pb_decls, nargs);
            }
            P::Loop { body } => {
                walk(body, map, labels, pb_decls, nargs);
            }
            P::Exit { when } | P::Continue { when } => {
                if let Some(w) = when {
                    rewrite_stmt_map(w, map, &*labels);
                }
            }
            P::Utility(_) | P::ReturnBare | P::GetDiagnostics { .. } | P::Reraise => {}
        }
    }

    walk(
        &mut pb.stmts,
        &base,
        &mut label_scopes,
        &pb.decls.clone(),
        nargs,
    );
    let decls_snapshot = pb.decls.clone();
    for h in &mut pb.handlers {
        walk(
            &mut h.stmts,
            &base,
            &mut label_scopes,
            &decls_snapshot,
            nargs,
        );
    }
}

/// v2.12: make a plpgsql body's named arguments and RETURNS TABLE
/// columns addressable as variables. Named arguments become implicit
/// ALIAS declarations pointing at their own parameter slot (so
/// `j := ...` on an OUT/INOUT argument, and assignments to IN
/// arguments, resolve through the ordinary decl machinery); RETURNS
/// TABLE columns become implicit plain declarations (their slots
/// are ordinary decl slots, initialized NULL). A name the body
/// already DECLAREs is left alone — the declaration shadows the
/// argument (PG19 shadowing rules; shadowtest / pl_qual_names).
pub(crate) fn inject_plpgsql_arg_vars(
    pb: &mut crate::sql::PlpgsqlBody,
    arg_names: &[Option<String>],
    arg_modes: &[u8],
    table_cols: &[(String, String)],
) {
    let _ = arg_modes;
    for (i, n) in arg_names.iter().enumerate() {
        let Some(name) = n else { continue };
        if pb.decls.iter().any(|d| &d.name == name) {
            continue;
        }
        pb.decls.push(crate::sql::PlpgsqlDecl {
            name: name.clone(),
            type_name: String::new(),
            default: None,
            alias_for: Some(name.clone()),
            is_cursor: false,
            implicit: true,
            alias_slot: Some((i + 1) as u32),
            cursor_query: None,
            cursor_params: Vec::new(),
            cursor_scroll: None,
            is_constant: false,
        });
    }
    for (cn, ct) in table_cols {
        if pb.decls.iter().any(|d| &d.name == cn) {
            continue;
        }
        pb.decls.push(crate::sql::PlpgsqlDecl {
            name: cn.clone(),
            type_name: ct.clone(),
            default: None,
            alias_for: None,
            is_cursor: false,
            implicit: true,
            alias_slot: None,
            cursor_query: None,
            cursor_params: Vec::new(),
            cursor_scroll: None,
            is_constant: false,
        });
    }
}

/// v2.12: the declared argument names of a CREATE FUNCTION arg
/// list (for plpgsql name validation).
pub(crate) fn func_arg_names(args: &[crate::sql::FuncArg]) -> Vec<String> {
    args.iter().filter_map(|a| a.name.clone()).collect()
}

/// v1.01: rebuild a function's parsed bodies from stored body text
/// (WAL replay / checkpoint restore). For plpgsql the multi-statement
/// form (original source) is tried first; the v0.97 single-RETURN form
/// stores the desugared `SELECT ...` text, which fails the plpgsql
/// parse and falls through to the plain statement parse. Named
/// argument references are rewritten to `$n` exactly as CREATE does
/// (this also closes a v1.00 replay gap where the rewrite was only
/// applied at CREATE time).
pub(crate) fn rebuild_function_bodies(
    lang: crate::sql::FuncLang,
    arg_names: &[Option<String>],
    arg_modes: &[u8],
    body: &str,
    returns_set: bool,
    is_void: bool,
    table_cols: &[(String, String)],
) -> (Option<Vec<Stmt>>, Option<crate::sql::PlpgsqlBody>) {
    if lang == crate::sql::FuncLang::Plpgsql {
        // v1.03: SETOF bodies skip the single-RETURN desugar exactly as
        // CREATE does (see exec_create_function).
        // v2.12 finishing: output-parameter bodies skip it too (see
        // exec_create_function for the validation-bypass rationale).
        let has_out_pre = !table_cols.is_empty()
            || arg_modes
                .iter()
                .any(|m| crate::sql::ArgMode::from_code(*m).is_output());
        let desugared = if returns_set || has_out_pre {
            None
        } else {
            crate::sql::desugar_plpgsql_body(body).ok()
        };
        if desugared.is_none() {
            let has_out = !table_cols.is_empty()
                || arg_modes
                    .iter()
                    .any(|m| crate::sql::ArgMode::from_code(*m).is_output());
            let rebuild_arg_names: Vec<String> = arg_names.iter().flatten().cloned().collect();
            if let Ok(mut pb) = crate::sql::parse_plpgsql_body(
                body,
                returns_set,
                is_void,
                has_out,
                &rebuild_arg_names,
            ) {
                inject_plpgsql_arg_vars(&mut pb, arg_names, arg_modes, table_cols);
                rewrite_plpgsql_body_refs(&mut pb, arg_names);
                return (None, Some(pb));
            }
        }
        if let Ok(mut stmt) = crate::sql::parse_statement(body) {
            rewrite_func_arg_refs(&mut stmt, arg_names);
            // v1.32: the desugared single SELECT is a one-element body.
            return (Some(vec![stmt]), None);
        }
        return (None, None);
    }
    // v1.32: mirror CREATE's multi-statement body parse (see
    // parse_sql_func_body); validation errors are impossible here
    // because CREATE already validated this body, so any parse failure
    // degrades to no parsed body.
    let mut stmts = Vec::new();
    for chunk in crate::sql::split_statements(body) {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let Ok(mut stmt) = crate::sql::parse_statement(chunk) else {
            return (None, None);
        };
        rewrite_func_arg_refs(&mut stmt, arg_names);
        stmts.push(stmt);
    }
    (Some(stmts), None)
}

/// v1.96: CREATE FUNCTION language permission (PG19 aclchk.c
/// language ACL): untrusted languages (c, internal) are
/// superuser-only; trusted ones default to PUBLIC USAGE unless
/// revoked (lang_acls).
pub(crate) fn check_language_usage(
    eng: &Engine,
    ctx: &StmtCtx,
    lang_name: &str,
) -> Result<(), ExecError> {
    let lname = lang_name.to_ascii_lowercase();
    if crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Ok(());
    }
    if lname == "c" || lname == "internal" {
        return Err(exec_err(
            "42501",
            format!("permission denied for language \"{}\"", lname),
        ));
    }
    let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
    let privs = match eng.db.lang_acls.get(&lname) {
        None => crate::storage::PRIV_USAGE,
        Some(entries) => {
            // v1.96: once a language HAS an ACL, it is authoritative
            // — no public entry means PUBLIC holds nothing (the
            // default applies only while the ACL is NULL, i.e. the
            // map has no key at all). This is what makes
            // CREATE FUNCTION fail for regress_priv_user2 after
            // REVOKE ALL ON LANGUAGE sql FROM PUBLIC.
            let mut p = match entries.iter().find(|e| e.role == "public") {
                Some(e) => e.privs,
                None => 0,
            };
            for e in entries {
                if e.role != "public" && closure.iter().any(|n| n == &e.role) {
                    p |= e.privs;
                }
            }
            p
        }
    };
    if privs & crate::storage::PRIV_USAGE != crate::storage::PRIV_USAGE {
        return Err(exec_err(
            "42501",
            format!("permission denied for language \"{}\"", lname),
        ));
    }
    Ok(())
}

/// v1.96: CREATE AGGREGATE — register the definition (the fold runs
/// in agg evaluation via the sfunc).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_aggregate(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    arg_types: &[String],
    sfunc: &str,
    stype: &str,
    finalfunc: &Option<String>,
    initcond: &Option<String>,
    msfunc: &Option<String>,
    minvfunc: &Option<String>,
    mstype: &Option<String>,
    parallel: &Option<String>,
) -> Result<ExecResult, ExecError> {
    let _ = msfunc;
    // v1.96 finishing pass: PG19 aggregate definition validation
    // (aggregate.c / functioncmds.c): PARALLEL takes a fixed value
    // set; the transition function's signature must be exactly
    // (stype, input...) -> stype with no coercion of the state
    // argument; the inverse transition function must return the
    // (moving) state type.
    if let Some(p) = parallel {
        let v = p.to_ascii_lowercase();
        if v != "safe" && v != "restricted" && v != "unsafe" {
            return Err(exec_err(
                "42P13",
                "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE".to_string(),
            ));
        }
    }
    // v2.04: PG19 pg_aggregate.c check_valid_polymorphic_signature —
    // a polymorphic transition type (anyarray & family) can only be
    // deduced from a polymorphic aggregate argument; with none (e.g.
    // `AGG(*) ... STYPE = anyarray`) CREATE fails 42P13. This check
    // was invisible while `SFUNC(int[])` definitions failed to parse:
    // the aggregates died earlier with 42883 and coincidentally
    // matched PG's expected ERROR.
    let is_pseudo = |s: &str| {
        let t = s.to_ascii_lowercase();
        let t = t.rsplit('.').next().unwrap_or(&t).trim();
        let t = t.strip_prefix("variadic ").unwrap_or(t);
        t.starts_with("any")
    };
    let any_pseudo_arg = arg_types.iter().any(|t| is_pseudo(t));
    if is_pseudo(stype) && !any_pseudo_arg {
        return Err(exec_err(
            "42P13",
            "cannot determine transition data type".to_string(),
        ));
    }
    if let Some(ms) = mstype {
        if is_pseudo(ms) && !any_pseudo_arg {
            return Err(exec_err(
                "42P13",
                "cannot determine transition data type".to_string(),
            ));
        }
    }
    // Polymorphic aggregates (anyelement & family) resolve their
    // state type at call time; PG19's static checks don't apply.
    let polymorphic = stype.starts_with("any") || arg_types.iter().any(|t| t.starts_with("any"));
    if !polymorphic {
        let canon = |s: &str| crate::sql::coltype_by_name(s).unwrap_or(ColType::Composite);
        let defs_of = |n: &str| {
            let k = n.to_ascii_lowercase();
            let b = k.rsplit('.').next().unwrap_or(&k).to_string();
            eng.db
                .functions
                .get(&k)
                .or_else(|| eng.db.functions.get(&b))
        };
        if let Some(defs) = defs_of(sfunc) {
            let want: Vec<ColType> = std::iter::once(stype.to_string())
                .chain(arg_types.iter().cloned())
                .map(|s| canon(&s))
                .collect();
            let st = canon(stype);
            let mut any_named = false;
            let mut exact = false;
            for d in defs.iter().filter(|d| !d.is_procedure) {
                any_named = true;
                let params: Vec<ColType> = d.arg_types.iter().map(|s| canon(s)).collect();
                if params == want {
                    if canon(&d.ret_type) != st {
                        return Err(exec_err(
                            "42P13",
                            format!(
                                "return type of transition function {} is not {}",
                                sfunc, stype
                            ),
                        ));
                    }
                    exact = true;
                }
            }
            if any_named && !exact {
                let sig = vec![stype; want.len()].join(", ");
                return Err(exec_err(
                    "42P13",
                    format!(
                        "function {}({}) requires run-time type coercion",
                        sfunc, sig
                    ),
                ));
            }
        }
        if let Some(inv) = minvfunc {
            if let Some(defs) = defs_of(inv) {
                let target_name = mstype.as_deref().unwrap_or(stype);
                let target = canon(target_name);
                if let Some(d) = defs.iter().find(|d| !d.is_procedure) {
                    if canon(&d.ret_type) != target {
                        return Err(exec_err(
                            "42P13",
                            format!(
                                "return type of inverse transition function {} is not {}",
                                inv, target_name
                            ),
                        ));
                    }
                }
            }
        }
    }
    let key = name.to_ascii_lowercase();
    if eng.db.aggregates.contains_key(&key) {
        return Err(exec_err(
            "42723",
            format!(
                "function \"{}\" already exists with same argument types",
                name
            ),
        ));
    }
    // v1.96: aggregate argument/state types need USAGE (PG19
    // parse-time type check, same as function args).
    for t in arg_types.iter().chain(std::iter::once(&stype.to_string())) {
        check_type_usage(eng, ctx, t)?;
    }
    // The sfunc must exist (PG19 resolves it at CREATE).
    let sfunc_key = sfunc.to_ascii_lowercase();
    let bare = sfunc_key
        .rsplit('.')
        .next()
        .unwrap_or(&sfunc_key)
        .to_string();
    // v1.96: the builtin int transition/accumulator functions
    // (int4pl & family) are pg_proc entries too.
    let builtin_agg_sfunc = matches!(
        bare.as_str(),
        "int2pl"
            | "int4pl"
            | "int8pl"
            | "int2mi"
            | "int4mi"
            | "int8mi"
            | "int2mul"
            | "int4mul"
            | "int8mul"
            | "int2div"
            | "int4div"
            | "int8div"
            | "int2_sum"
            | "int4_sum"
            | "int8_sum"
            | "int2_avg_accum"
            | "int4_avg_accum"
            | "int8_avg_accum"
            | "numeric_add"
            | "numeric_sub"
            | "numeric_mul"
            | "float8pl"
            | "float8mi"
            | "float8mul"
            | "float8div"
    );
    if !eng.db.functions.contains_key(&sfunc_key)
        && !eng.db.functions.contains_key(&bare)
        && !crate::sql::is_builtin_fn(&bare)
        && !builtin_agg_sfunc
    {
        return Err(exec_err(
            "42883",
            format!("function {} does not exist", sfunc),
        ));
    }
    eng.db.aggregates.insert(
        key.clone(),
        crate::storage::AggDef {
            sfunc: sfunc_key,
            stype: stype.to_string(),
            finalfunc: finalfunc.clone(),
            initcond: initcond.clone(),
            arg_types: arg_types.to_vec(),
            owner: ctx.role.to_string(),
        },
    );
    // v1.96: record routine ownership like functions (shared map).
    if !eng.db.fn_owners.contains_key(&key) {
        let prev_fn = eng.db.fn_acls.clone();
        let prev_type = eng.db.type_acls.clone();
        let prev_lang = eng.db.lang_acls.clone();
        let prev_fn_owners = eng.db.fn_owners.clone();
        let prev_type_owners = eng.db.type_owners.clone();
        eng.db.fn_owners.insert(key.clone(), ctx.role.to_string());
        // v1.96: ADP initial ACL for the new aggregate (kind 'f').
        if let Some(init) = crate::exec::privilege::adp_initial_acl(
            &eng.db,
            ctx.role,
            'f',
            &key,
            crate::storage::PRIV_USAGE,
            crate::storage::PRIV_USAGE,
        ) {
            eng.db.fn_acls.insert(key.clone(), init);
        }
        ctx.writes.push(WriteOp::FnTypeAcls {
            prev_fn,
            prev_type,
            prev_lang,
            prev_fn_owners,
            prev_type_owners,
        });
    }
    Ok(ExecResult::Command {
        tag: "CREATE AGGREGATE".to_string(),
    })
}

/// v1.96: DROP AGGREGATE.
pub(crate) fn exec_drop_aggregate(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    _arg_types: &[String],
    if_exists: bool,
    _cascade: bool,
) -> Result<ExecResult, ExecError> {
    let key = name.to_ascii_lowercase();
    if let Some(def) = eng.db.aggregates.get(&key) {
        if def.owner != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!("must be owner of function {}", name),
            ));
        }
    }
    if eng.db.aggregates.remove(&key).is_none() && !if_exists {
        return Err(exec_err(
            "42883",
            format!("function {} does not exist", name),
        ));
    }
    // v1.96: clean the shared owner/ACL records for the aggregate.
    if eng.db.fn_owners.contains_key(&key) || eng.db.fn_acls.contains_key(&key) {
        let prev_fn = eng.db.fn_acls.clone();
        let prev_type = eng.db.type_acls.clone();
        let prev_lang = eng.db.lang_acls.clone();
        let prev_fn_owners = eng.db.fn_owners.clone();
        let prev_type_owners = eng.db.type_owners.clone();
        eng.db.fn_owners.remove(&key);
        eng.db.fn_acls.remove(&key);
        ctx.writes.push(WriteOp::FnTypeAcls {
            prev_fn,
            prev_type,
            prev_lang,
            prev_fn_owners,
            prev_type_owners,
        });
    }
    Ok(ExecResult::Command {
        tag: "DROP AGGREGATE".to_string(),
    })
}
