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
    _ctx: &mut StmtCtx,
    name: &str,
    arg_types: &[String],
    volatility: crate::sql::FuncVolatility,
) -> Result<ExecResult, ExecError> {
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
    slot.volatility = volatility;
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
        ColType::SingleChar => Value::SingleChar(b'x'),
        ColType::Bool => Value::Bool(true),
        ColType::Date => Value::Date(1),
        ColType::Timestamp => Value::Timestamp(1),
        ColType::Timestamptz => Value::Timestamptz(1),
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
        if !matches!(
            stmt,
            Stmt::Select(_) | Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. }
        ) {
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
    // PG19 check_sql_fn_retval: an empty body behaves like "the last
    // query rewrote to nothing" — return type mismatch. (No VOID
    // carve-out: RETURNS void is 42704 in this engine today, a
    // separate type-system gap.)
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
        | Stmt::Delete { returning, .. } => !returning.is_empty(),
        // Unreachable: non-SELECT/DML statements are rejected above.
        _ => false,
    };
    if !final_returns_rows {
        return Err(sql_func_final_42p13(ret_type));
    }
    // PG19 check_sql_fn_retval, scalar case: the final statement must
    // return exactly one column. Enforced here only when the column
    // count is syntactically unambiguous (plain SELECT, no `*`,
    // no set operations; DML RETURNING list with no `*`); ambiguous
    // shapes defer to the existing call-time check.
    if !returns_set {
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
            | Stmt::Delete { returning, .. } => {
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
) -> Result<ExecResult, ExecError> {
    // v1.32: PG19 resolves the language first (before return-type
    // resolution): unknown LANGUAGE is 42883, not a parse error.
    let lang = resolve_func_lang(lang_name)?;
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
    let owned_body: String;
    let mut plpgsql_body: Option<crate::sql::PlpgsqlBody> = None;
    let body: &str = if is_trigger_fn {
        body
    } else if lang == crate::sql::FuncLang::Plpgsql {
        // v1.03: SETOF bodies skip the v0.97 single-RETURN desugar so a
        // bare RETURN is honestly rejected (42601) instead of silently
        // becoming a SQL SELECT body.
        let desugared = if returns_set {
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
                let mut pb = crate::sql::parse_plpgsql_body(body, returns_set)
                    .map_err(|e| exec_err(e.code, format!("plpgsql: {}", e.message)))?;
                // v1.03: resolve DECLAREd variable types now (like PG's
                // plpgsql validator at CREATE time).
                for d in &pb.decls {
                    resolve_func_type_name(eng, ctx.snap, ctx.own, ctx.session, &d.type_name)
                        .map_err(|e| exec_err(e.code, format!("plpgsql: {}", e.message)))?;
                }
                // v1.03: a variable name colliding with an argument name
                // is ambiguous - reject (PG: the variable would shadow
                // the argument, which the bounded subset forbids).
                for d in &pb.decls {
                    if args
                        .iter()
                        .any(|a| a.name.as_deref() == Some(d.name.as_str()))
                    {
                        return Err(exec_err(
                            "42601",
                            format!("plpgsql: variable \"{}\" is already defined", d.name),
                        ));
                    }
                }
                let arg_names: Vec<Option<String>> = args.iter().map(|a| a.name.clone()).collect();
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
        )?);
    } else if lang == crate::sql::FuncLang::Internal {
        // v0.86: validate the internal symbol at CREATE (like PG's
        // fmgr lookup); unknown symbols are 0A000. v1.38: the int4/int8
        // I/O symbols (PG19 int.c) join int4eq — the float4/float8
        // conformance types are defined in terms of them.
        if !matches!(body, "int4eq" | "int4in" | "int4out" | "int8in" | "int8out") {
            return Err(exec_err(
                "0A000",
                format!("unsupported internal function \"{}\"", body),
            ));
        }
    }
    // v0.87: overload-aware CREATE. `prev` is the specific overload
    // being replaced (by signature), if any.
    let new_arg_types: Vec<String> = args.iter().map(|a| a.type_name.clone()).collect();
    let prev = find_function_by_signature(eng, name, &new_arg_types);
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
    };
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
pub(crate) fn exec_drop_function(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    arg_types: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    // v0.87: drop the specific overload by signature.
    let prev = find_function_by_signature(eng, name, arg_types);
    let Some(dropped) = prev else {
        if !if_exists {
            return Err(exec_err(
                "42883",
                format!("function {}() does not exist", name),
            ));
        }
        return Ok(ExecResult::Command {
            tag: "DROP FUNCTION".to_string(),
        });
    };
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
                        && find_function_by_signature(eng, name, arg_types)
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
        overloads.retain(|f| {
            !(f.arg_types.len() == dropped.arg_types.len()
                && f.arg_types
                    .iter()
                    .zip(dropped.arg_types.iter())
                    .all(|(a, b)| canon_func_type_name(a) == canon_func_type_name(b)))
        });
        if overloads.is_empty() {
            eng.db.functions.remove(name);
        }
    }
    ctx.writes.push(WriteOp::DropFunction {
        name: name.to_string(),
        prev: Some(dropped),
    });
    Ok(ExecResult::Command {
        tag: "DROP FUNCTION".to_string(),
    })
}

/// v1.00: `CREATE TRIGGER` (bounded, PG19 `CreateTrigStmt`). The
/// trigger is stored on the table's catalog entry (`Table.triggers`),
/// so creation is transactional (ROLLBACK-safe), WAL-logged, and
/// checkpointed via the normal table-version machinery. Only BEFORE
/// INSERT FOR EACH ROW triggers fire in v1.00; AFTER triggers, other
/// events, WHEN clauses, and constraint triggers are cataloged but
/// inert (documented gaps).
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
    when: Option<&crate::sql::Expr>,
    is_constraint: bool,
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
    // v1.00: WHEN (...) is parsed and stored for catalog fidelity, but
    // the executor does not evaluate it (documented gap).
    let _ = when;
    let mut next = prev.clone();
    next.triggers.push(crate::sql::TriggerDef {
        name: name.to_string(),
        timing,
        events,
        for_each_row,
        function: function.to_string(),
        args: args.to_vec(),
    });
    let _ = is_constraint;
    commit_table_version(eng, ctx, table, prev, next);
    Ok(ExecResult::Command {
        tag: "CREATE TRIGGER".to_string(),
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
        if let crate::sql::FromItem::Function { args, .. } = f {
            for a in args.iter_mut() {
                rewrite_func_arg_expr(a, arg_names);
            }
        }
    }
}

/// v1.03: rewrite named references in a parsed plpgsql body to
/// positional parameters: function argument names become `$n`
/// (existing), and DECLAREd variable names become `$(nargs+i+1)`
/// (new). This reuses `rewrite_func_arg_refs` /
/// `rewrite_func_arg_expr` verbatim by appending the variable names
/// after the argument names - the resulting 1-based positions are
/// exactly the runtime slot layout (`params[nargs + i]`, see
/// `run_plpgsql_body`). Applied at CREATE and on WAL/checkpoint
/// rebuild; idempotent (an already-rewritten `Param` is untouched).
/// EXPLAIN inner queries are recursed into so FOR loops over EXPLAIN
/// see the same bindings.
pub(crate) fn rewrite_plpgsql_body_refs(
    pb: &mut crate::sql::PlpgsqlBody,
    arg_names: &[Option<String>],
) {
    let mut all_names: Vec<Option<String>> = arg_names.to_vec();
    all_names.extend(pb.decls.iter().map(|d| Some(d.name.clone())));
    fn rewrite_query(stmt: &mut crate::sql::Stmt, all_names: &[Option<String>]) {
        match stmt {
            crate::sql::Stmt::Explain { stmt: inner, .. } => rewrite_query(inner, all_names),
            crate::sql::Stmt::Select(sel) => {
                // v1.03: FOR queries may reference args/vars in FROM
                // function args (e.g. `generate_series(1, n)`); the
                // base rewriter only walks the select list + WHERE.
                for item in &mut sel.items {
                    if let crate::sql::SelectItem::Expr { expr, .. } = item {
                        rewrite_func_arg_expr(expr, all_names);
                    }
                }
                if let Some(w) = &mut sel.where_ {
                    rewrite_func_arg_expr(w, all_names);
                }
                for fi in &mut sel.from {
                    if let crate::sql::FromItem::Function { args, .. } = fi {
                        for a in args {
                            rewrite_func_arg_expr(a, all_names);
                        }
                    }
                }
            }
            _ => rewrite_func_arg_refs(stmt, all_names),
        }
    }
    fn rewrite_stmt(s: &mut crate::sql::PlpgsqlStmt, all_names: &[Option<String>]) {
        match s {
            crate::sql::PlpgsqlStmt::Return(sel) | crate::sql::PlpgsqlStmt::ReturnNext(sel) => {
                rewrite_func_arg_refs(sel, all_names);
            }
            crate::sql::PlpgsqlStmt::Assign { select, .. } => {
                rewrite_func_arg_refs(select, all_names);
            }
            crate::sql::PlpgsqlStmt::ForQuery { query, body, .. } => {
                rewrite_query(query, all_names);
                for b in body {
                    rewrite_stmt(b, all_names);
                }
            }
            crate::sql::PlpgsqlStmt::Raise { args, .. } => {
                for a in args {
                    rewrite_func_arg_expr(a, all_names);
                }
            }
            crate::sql::PlpgsqlStmt::Utility(_) => {}
        }
    }
    for s in pb
        .stmts
        .iter_mut()
        .chain(pb.handlers.iter_mut().flat_map(|h| h.stmts.iter_mut()))
    {
        rewrite_stmt(s, &all_names);
    }
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
    body: &str,
    returns_set: bool,
) -> (Option<Vec<Stmt>>, Option<crate::sql::PlpgsqlBody>) {
    if lang == crate::sql::FuncLang::Plpgsql {
        // v1.03: SETOF bodies skip the single-RETURN desugar exactly as
        // CREATE does (see exec_create_function).
        let desugared = if returns_set {
            None
        } else {
            crate::sql::desugar_plpgsql_body(body).ok()
        };
        if desugared.is_none() {
            if let Ok(mut pb) = crate::sql::parse_plpgsql_body(body, returns_set) {
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
