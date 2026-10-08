// v1.78 mechanical split: moved verbatim from src/exec.rs (43220-51337).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// ||, LIKE, BETWEEN, IS TRUE/FALSE/UNKNOWN
// ---------------------------------------------------------------------------

/// `||`: array||array / array||element / element||array (PG19
/// array_cat/array_append/array_prepend); bytea||bytea -> bytea; anything
/// else coerces to text (Postgres' anynonarray || text behavior). NULL
/// propagates, except an untyped NULL scalar next to an array is a NULL
/// array *element* (PG's unknown-literal resolution).
///
/// v0.90: `__variadic` marker check — returns true if `e` is the
/// `__variadic(inner)` marker produced by the parser for `VARIADIC inner`.
pub(crate) fn is_variadic_marker(e: &Expr) -> Option<&Expr> {
    if let Expr::Func { name, args } = e {
        if name == "__variadic" && args.len() == 1 {
            return Some(&args[0]);
        }
    }
    None
}

/// v0.90: Expand a `VARIADIC` argument value. A NULL array means the
/// whole call returns NULL (PG: `concat(VARIADIC NULL)` is NULL);
/// an array expands to its elements. v0.91: anything else is a PG
/// type error (42821 "VARIADIC argument must be an array").
/// v1.24: `format` is the one exception — PG19's text_format() treats
/// a NULL VARIADIC argument as a zero-length array ("If argument is
/// NULL, we treat it as zero-length array", varlena.c), while
/// concat/concat_ws define VARIADIC NULL as NULL (concat_internal)
/// and json_build_*/jsonb_build_* PG_RETURN_NULL() on it. Callers
/// check `variadic_null_expands_empty(name)` on the None arm.
pub(crate) fn expand_variadic_value(v: Value) -> Result<Option<Vec<Value>>, ExecError> {
    match v {
        Value::Null => Ok(None),
        Value::Array(arr) => Ok(Some(arr.elems)),
        // v0.91: PG19 raises ERRCODE_DATATYPE_MISMATCH when the VARIADIC
        // argument is not an array ("VARIADIC argument must be an array").
        other => Err(exec_err(
            "42821",
            format!(
                "VARIADIC argument must be an array, not type {}",
                other.type_name()
            ),
        )),
    }
}

/// v1.24: PG19 function for which a NULL `VARIADIC` argument expands to
/// zero arguments instead of making the whole call NULL. Only
/// `format` behaves this way (varlena.c text_format: "If argument is
/// NULL, we treat it as zero-length array"); concat/concat_ws define
/// VARIADIC NULL as NULL and json_build_*/jsonb_build_* return NULL.
pub(crate) fn variadic_null_expands_empty(func_name: &str) -> bool {
    func_name.eq_ignore_ascii_case("format")
}

pub(crate) fn eval_concat(a: &Value, b: &Value) -> Result<Value, ExecError> {
    // v0.79: array concatenation takes precedence over the NULL
    // short-circuit below (so `ARRAY[1] || NULL` appends a NULL).
    if matches!(a, Value::Array(_)) || matches!(b, Value::Array(_)) {
        return eval_array_concat(a, b);
    }
    if a == &Value::Null || b == &Value::Null {
        return Ok(Value::Null);
    }
    match (a, b) {
        (Value::Bytea(x), Value::Bytea(y)) => {
            let mut r = Vec::with_capacity(x.len() + y.len());
            r.extend_from_slice(x);
            r.extend_from_slice(y);
            Ok(Value::Bytea(r))
        }
        _ => {
            // v0.90: PG19 only resolves `||` to text concatenation when
            // the implicit cast to text is allowed, i.e. when at least
            // one operand is text-like (text/varchar/char) or an unknown
            // literal (which becomes text). `SELECT 3 || 4.0` (integer ||
            // numeric) raises 42883. See PG19 text.out.
            let text_like =
                |v: &Value| matches!(v, Value::Text(_) | Value::BpChar(_) | Value::SingleChar(_));
            if !text_like(a) && !text_like(b) {
                return Err(exec_err(
                    "42883",
                    format!(
                        "operator does not exist: {} || {}",
                        value_type_name(a),
                        value_type_name(b)
                    ),
                ));
            }
            Ok(text_value_of(&[a, b]))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatTok {
    Lit(char),
    Any,  // `_`
    Star, // `%`
}

/// Tokenize a LIKE pattern; the escape character (default backslash)
/// escapes the next character. A trailing escape is a literal.
pub(crate) fn tokenize_like(pat: &[char], escape: char) -> Vec<PatTok> {
    let mut toks = Vec::new();
    let mut i = 0;
    while i < pat.len() {
        if pat[i] == escape && i + 1 < pat.len() {
            toks.push(PatTok::Lit(pat[i + 1]));
            i += 2;
        } else if pat[i] == '%' {
            toks.push(PatTok::Star);
            i += 1;
        } else if pat[i] == '_' {
            toks.push(PatTok::Any);
            i += 1;
        } else {
            toks.push(PatTok::Lit(pat[i]));
            i += 1;
        }
    }
    toks
}

/// Byte-oriented LIKE matcher for bytea (wildcards are ASCII % and _).
pub(crate) fn match_like_bytes(s: &[u8], pat: &[u8], escape: u8) -> bool {
    // Tokenize: 0 = literal byte follows, 1 = _, 2 = %.
    let mut toks: Vec<(u8, u8)> = Vec::new();
    let mut i = 0;
    while i < pat.len() {
        if pat[i] == escape && i + 1 < pat.len() {
            toks.push((0, pat[i + 1]));
            i += 2;
        } else if pat[i] == b'%' {
            toks.push((2, 0));
            i += 1;
        } else if pat[i] == b'_' {
            toks.push((1, 0));
            i += 1;
        } else {
            toks.push((0, pat[i]));
            i += 1;
        }
    }
    let (mut si, mut ti) = (0, 0);
    let mut star_ti: Option<usize> = None;
    let mut star_si = 0;
    while si < s.len() {
        if ti < toks.len() && (toks[ti].0 == 1 || (toks[ti].0 == 0 && toks[ti].1 == s[si])) {
            si += 1;
            ti += 1;
        } else if ti < toks.len() && toks[ti].0 == 2 {
            star_ti = Some(ti);
            star_si = si;
            ti += 1;
        } else if let Some(st) = star_ti {
            ti = st + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while ti < toks.len() && toks[ti].0 == 2 {
        ti += 1;
    }
    ti == toks.len()
}

/// Classic backtracking LIKE matcher over tokenized pattern.
pub(crate) fn match_like(s: &[char], toks: &[PatTok]) -> bool {
    let (mut si, mut ti) = (0, 0);
    let mut star_ti: Option<usize> = None;
    let mut star_si = 0;
    while si < s.len() {
        if ti < toks.len()
            && (toks[ti] == PatTok::Any || matches!(toks[ti], PatTok::Lit(c) if c == s[si]))
        {
            si += 1;
            ti += 1;
        } else if ti < toks.len() && toks[ti] == PatTok::Star {
            star_ti = Some(ti);
            star_si = si;
            ti += 1;
        } else if let Some(st) = star_ti {
            ti = st + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while ti < toks.len() && toks[ti] == PatTok::Star {
        ti += 1;
    }
    ti == toks.len()
}

/// v0.68: POSIX regex match operators `~` / `!~` / `~*` / `!~*`
/// (PG textregexeq etc., src/backend/utils/adt/regexp.c). Unanchored
/// search; NULL in either operand -> NULL; an invalid pattern is
/// 2201B like the regexp_* functions; non-text operands are 42883.
pub(crate) fn eval_regex_match(
    a: &Value,
    pattern: &Value,
    not: bool,
    case_insensitive: bool,
) -> Result<Value, ExecError> {
    let op = match (not, case_insensitive) {
        (false, false) => "~",
        (true, false) => "!~",
        (false, true) => "~*",
        (true, true) => "!~*",
    };
    // v0.68: PG coerces bpchar regex operands to text (rtrim1), like LIKE.
    let a_norm;
    let a = match a {
        Value::BpChar(s) => {
            a_norm = Value::text(crate::storage::rtrim_spaces(s));
            &a_norm
        }
        _ => a,
    };
    let p_norm;
    let pattern = match pattern {
        Value::BpChar(s) => {
            p_norm = Value::text(crate::storage::rtrim_spaces(s));
            &p_norm
        }
        _ => pattern,
    };
    match (a, pattern) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(s), Value::Text(p)) => {
            let re = crate::regex::compile(p, case_insensitive)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            let m = re.is_match(&sc);
            Ok(Value::Bool(if not { !m } else { m }))
        }
        _ => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                a.type_name(),
                op,
                pattern.type_name()
            ),
        )),
    }
}

pub(crate) fn eval_like(
    a: &Value,
    pattern: &Value,
    escape: Option<&Value>,
    not: bool,
    ilike: bool,
) -> Result<Value, ExecError> {
    // v0.35: PG coerces bpchar LIKE operands to text (rtrim1). Normalize
    // here since LIKE is an operator, not a function call.
    let a_norm;
    let a = match a {
        Value::BpChar(s) => {
            a_norm = Value::text(crate::storage::rtrim_spaces(s));
            &a_norm
        }
        _ => a,
    };
    let p_norm;
    let pattern = match pattern {
        Value::BpChar(s) => {
            p_norm = Value::text(crate::storage::rtrim_spaces(s));
            &p_norm
        }
        _ => pattern,
    };
    // No ESCAPE clause -> PG default escape is backslash.
    // ESCAPE NULL -> NULL result. ESCAPE must be a single character/byte.
    // Returns (text_escape, bytea_escape); only one is used per branch.
    let (esc_char, esc_byte): (Option<char>, Option<u8>) = match escape {
        None => (Some('\\'), Some(b'\\')),
        Some(Value::Null) => (None, None),
        Some(Value::Text(e)) => {
            let mut ch = e.chars();
            match (ch.next(), ch.next()) {
                (Some(c), None) => (Some(c), None),
                _ => {
                    return Err(exec_err(
                        "22023",
                        "ESCAPE string must be empty or one character",
                    ));
                }
            }
        }
        Some(Value::Bytea(b)) => {
            if b.len() == 1 {
                (None, Some(b[0]))
            } else {
                return Err(exec_err(
                    "22023",
                    "ESCAPE string must be empty or one character",
                ));
            }
        }
        Some(other) => {
            return Err(exec_err(
                "42883",
                format!("ESCAPE must be text, not {}", other.type_name()),
            ));
        }
    };
    let esc_is_null = esc_char.is_none() && esc_byte.is_none();
    match (a, pattern) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        _ if esc_is_null => Ok(Value::Null),
        (Value::Bytea(s), Value::Bytea(p)) => {
            // bytea LIKE: match on raw bytes; % and _ are ASCII wildcards.
            let m = match_like_bytes(s, p, esc_byte.unwrap_or(b'\\'));
            Ok(Value::Bool(if not { !m } else { m }))
        }
        (Value::Text(s), Value::Text(p)) => {
            let (s, p) = if ilike {
                (s.to_lowercase(), p.to_lowercase())
            } else {
                (s.to_string(), p.to_string())
            };
            let sc: Vec<char> = s.chars().collect();
            let pc: Vec<char> = p.chars().collect();
            let m = match_like(&sc, &tokenize_like(&pc, esc_char.unwrap_or('\\')));
            Ok(Value::Bool(if not { !m } else { m }))
        }
        _ => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                a.type_name(),
                if ilike { "~~*" } else { "~~" },
                pattern.type_name()
            ),
        )),
    }
}

/// `x BETWEEN lo AND hi` = `x >= lo AND x <= hi`; NULL in any operand
/// -> NULL; type mismatches surface as 42883 from the comparisons.
pub(crate) fn eval_between(
    v: &Value,
    lo: &Value,
    hi: &Value,
    neg: bool,
) -> Result<Value, ExecError> {
    if v == &Value::Null || lo == &Value::Null || hi == &Value::Null {
        return Ok(Value::Null);
    }
    let ge = eval_cmp_vals(CmpOp::Ge, v, lo)?;
    let le = eval_cmp_vals(CmpOp::Le, v, hi)?;
    match (ge, le) {
        (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(if neg { !(a && b) } else { a && b })),
        // Unreachable: non-null inputs compare to bools or raise.
        _ => Ok(Value::Null),
    }
}

/// `IS [NOT] TRUE/FALSE/UNKNOWN`. Non-boolean input is 42804.
pub(crate) fn eval_is_bool(v: &Value, neg: bool, val: Option<bool>) -> Result<Value, ExecError> {
    let matched = match (v, val) {
        (Value::Null, None) => true,
        (Value::Bool(b), Some(w)) => *b == w,
        (Value::Null, Some(_)) | (Value::Bool(_), None) => false,
        (other, _) => {
            return Err(exec_err(
                "42804",
                format!(
                    "argument of IS must be type boolean, not type {}",
                    other.type_name()
                ),
            ));
        }
    };
    Ok(Value::Bool(matched != neg))
}

// ---------------------------------------------------------------------------
// EXTRACT
// ---------------------------------------------------------------------------

pub(crate) fn eval_extract(field: &str, v: &Value) -> Result<Value, ExecError> {
    if v == &Value::Null {
        return Ok(Value::Null);
    }
    let field = field.to_ascii_lowercase();
    let r = match v {
        Value::Timestamp(m) | Value::Timestamptz(m) => crate::datetime::extract(&field, *m),
        Value::Date(d) => crate::datetime::extract_date(&field, *d),
        other => {
            return Err(exec_err(
                "42883",
                format!(
                    "function extract({} from {}) does not exist",
                    field,
                    other.type_name()
                ),
            ));
        }
    };
    match r {
        Ok(f) => Numeric::from_f64(f)
            .map(Value::Numeric)
            .map_err(|_| exec_err("22003", "value out of range for type numeric")),
        Err(e) => Err(exec_err("22023", e)),
    }
}

// ---------------------------------------------------------------------------
// Built-in scalar functions
// ---------------------------------------------------------------------------

pub(crate) fn func_arg_err(fname: &str, v: &Value) -> ExecError {
    exec_err(
        "42883",
        format!("function {}({}) does not exist", fname, v.type_name()),
    )
}

/// Strict text argument (no implicit casts, like Postgres' LIKE).
pub(crate) fn str_arg<'a>(fname: &str, v: &'a Value) -> Result<Option<&'a str>, ExecError> {
    match v {
        Value::Null => Ok(None),
        Value::Text(s) => Ok(Some(s)),
        // v0.35: bpchar text arguments are rtrimmed (PG's text(bpchar)).
        // The trimmed slice borrows from the padded value, so no copy.
        Value::BpChar(s) => Ok(Some(crate::storage::rtrim_spaces(s))),
        other => Err(func_arg_err(fname, other)),
    }
}

/// Strict integer-kind argument.
pub(crate) fn int_arg(fname: &str, v: &Value) -> Result<Option<i64>, ExecError> {
    match v {
        Value::Null => Ok(None),
        Value::SmallInt(i) => Ok(Some(*i as i64)),
        Value::Int(i) => Ok(Some(*i)),
        Value::BigInt(i) => Ok(Some(*i)),
        other => Err(func_arg_err(fname, other)),
    }
}

/// v0.35: model PG's parse-time coercion of bpchar function arguments
/// to text (`text(bpchar)` = rtrim1). A blank-padded char value becomes
/// rtrimmed text before scalar dispatch, except for the builtins where
/// PG keeps the padding (`octet_length`, `bit_length`) or the value's
/// own type (`coalesce`, `nullif`, `greatest`, `least`).
pub(crate) fn normalize_func_arg(name: &str, v: Value) -> Value {
    match v {
        Value::BpChar(s)
            if !matches!(
                name,
                "octet_length" | "bit_length" | "coalesce" | "nullif" | "greatest" | "least"
                // v0.76: pg_typeof must see the original type.
                | "pg_typeof"
            ) =>
        {
            Value::text(crate::storage::rtrim_spaces(&s))
        }
        _ => v,
    }
}

/// v0.86: user-defined function call machinery (CREATE FUNCTION).
///
/// SQL-language bodies are parsed once at CREATE (stored in
/// `FuncDef.parsed` with named argument references rewritten to `$n`);
/// each call coerces the arguments to the declared types, binds them
/// via the existing `subst_params`, runs the body as a SELECT, and
/// coerces the result to the declared return type. STRICT functions
/// return NULL without running the body when any argument is NULL
/// (PG19). Internal-language functions dispatch to a small table of
/// C-symbol equivalents (bounded: only symbols the engine implements).

/// v0.86: coerce one call argument to the function's declared type.
pub(crate) fn coerce_func_arg(q: &mut Q, v: &Value, type_name: &str) -> Result<Value, ExecError> {
    if v == &Value::Null {
        return Ok(Value::Null);
    }
    coerce_to_type_name(q, v, type_name)
}

/// v0.86: coerce a value to a declared type name (builtin or named).
/// Builtin names go through `coltype_by_name` + `eval_cast`; named
/// composites/domains use `eval_cast_named`.
pub(crate) fn coerce_to_type_name(
    q: &mut Q,
    v: &Value,
    type_name: &str,
) -> Result<Value, ExecError> {
    if let Ok(ct) = crate::sql::coltype_by_name(type_name) {
        return eval_cast(v, ct);
    }
    eval_cast_named(q, v, type_name)
}

/// v0.86: run a SQL-language function body with bound arguments.
/// Returns the raw SELECT output (columns, rows).
/// v0.89: the statement-local UPDATE overlay a function body may see.
/// Only VOLATILE functions observe the in-flight UPDATE's already
/// processed rows (PG19); STABLE and IMMUTABLE bodies see the
/// statement snapshot, so they get `None`.
pub(crate) fn volatile_pending(
    q: &Q,
    fdef: &crate::storage::FuncDef,
) -> Option<Rc<RefCell<Vec<(String, u64, Row)>>>> {
    if fdef.volatility == crate::sql::FuncVolatility::Volatile {
        q.pending_updates.clone()
    } else {
        None
    }
}

pub(crate) fn run_func_body(
    q: &mut Q,
    scopes: &[Scope],
    fdef: &crate::storage::FuncDef,
    args: &[Value],
    // v0.89: statement-local UPDATE overlay, already volatility-gated
    // by the caller (Some only for volatile functions).
    pending: Option<Rc<RefCell<Vec<(String, u64, Row)>>>>,
) -> Result<SelectOut, ExecError> {
    // Arity is checked by the caller; coerce each argument to the
    // declared type (PG19 function call coercion, assignment cast).
    let mut coerced = Vec::with_capacity(args.len());
    for (v, t) in args.iter().zip(fdef.arg_types.iter()) {
        coerced.push(coerce_func_arg(q, v, t)?);
    }
    if fdef.strict && coerced.iter().any(|v| v == &Value::Null) {
        // PG19: a STRICT function is not called on NULL input; the
        // scalar result is NULL. (For set-returning STRICT functions PG
        // returns zero rows; the scalar path below handles NULL.)
        return Ok(SelectOut {
            columns: vec![],
            rows: vec![],
        });
    }
    // v1.01: multi-statement plpgsql bodies (statement sequences +
    // EXCEPTION blocks) run on their own path; the single-SELECT path
    // below serves SQL-language and v0.97 single-RETURN plpgsql bodies.
    if let Some(pb) = &fdef.plpgsql {
        let params: Vec<Option<Value>> = coerced.into_iter().map(Some).collect();
        return run_plpgsql_body(q, scopes, fdef, pb, &params, pending);
    }
    let body = fdef.parsed.as_ref().ok_or_else(|| {
        exec_err(
            "0A000",
            format!("function \"{}\" has no parsed body", fdef.name),
        )
    })?;
    // v1.01: multi-statement plpgsql bodies never reach here — they
    // branch above via `fdef.plpgsql`. Reaching this point with
    // `parsed == None` is a corrupt catalog entry.
    // v1.32: SQL bodies are statement lists (PG19 fmgr_sql runs each
    // command in order); CREATE validated every statement is a SELECT.
    let params: Vec<Option<Value>> = coerced.into_iter().map(Some).collect();
    // Run the body one query level deeper (fresh CTE scope, like a
    // subquery); the caller's scopes stay visible for outer refs.
    let mut call_q = Q {
        eng: &mut *q.eng,
        snap: q.snap,
        own: q.own,
        all_xids: q.all_xids.clone(),
        session: q.session,
        role: q.role,
        read_only: q.read_only,
        depth: q.depth + 1,
        lock_ids: &mut *q.lock_ids,
        ctes: q.ctes.clone(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: q.priv_scopes.clone(),
        hashed_exists: q.hashed_exists.clone(),
        immutable_fn_cache: q.immutable_fn_cache.clone(),
        // v1.37: FRESH plan-fold memo, never the shared one: this Q
        // executes a per-execution clone of the body AST, so a
        // shared map could alias a dangling pointer from a dropped
        // clone (see `plan_fold_memo` field doc).
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: q.hashed_in.clone(),
        // v0.89: volatile function bodies see the in-flight UPDATE's
        // already-processed rows (caller-gated); stable/immutable
        // bodies see the statement snapshot (None).
        pending_updates: pending,
        write: q.write.as_mut().map(QWrite::reborrow),
    };
    // v1.32: PG19 executes each body statement in order in the
    // caller's snapshot; only the last statement's result is the
    // function result (non-final results are discarded).
    // v1.33: DML body statements execute through the statement's
    // write context (`QWrite`): their effects land in the
    // statement's write log, so statement atomicity (autocommit undo
    // / transaction-abort undo) and commit-time WAL cover them
    // exactly like inline DML. New row versions are stamped with
    // `write_xid` (a member of `all_xids`), so later body statements
    // see earlier writes under the statement snapshot — this
    // engine's equivalent of PG19's CommandCounterIncrement between
    // body statements (executor/functions.c).
    let mut last_out: Option<SelectOut> = None;
    for s in body.iter() {
        let mut stmt = s.clone();
        subst_params(&mut stmt, &params)?;
        match stmt {
            Stmt::Select(bound) => {
                last_out = Some(run_select(&mut call_q, &bound, scopes)?);
            }
            Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. } => {
                last_out = Some(run_func_dml(&mut call_q, &stmt, &fdef.name)?);
            }
            // Unreachable: CREATE validated every body statement is
            // SELECT/INSERT/UPDATE/DELETE (0A000 otherwise).
            _ => {
                return Err(exec_err(
                    "XX000",
                    format!(
                        "function \"{}\" body statement is not SELECT/INSERT/UPDATE/DELETE",
                        fdef.name
                    ),
                ));
            }
        }
    }
    // CREATE guarantees a non-empty statement list (42P13 on empty).
    last_out.ok_or_else(|| {
        exec_err(
            "XX000",
            format!("function \"{}\" has an empty parsed body", fdef.name),
        )
    })
}
/// v1.39: run a DML statement nested inside another query level — a
/// data-modifying CTE body or a function-body DML. Shared by
/// `run_func_dml` (v1.33) and CTE materialization: it builds a `StmtCtx`
/// from the calling query level's write context (the statement's
/// `write_xid`, so the DML's rows are stamped exactly like top-level
/// DML's) and deliberately runs `execute_inner` rather than `execute()`
/// so trigger/RAISE notices flow through the statement-scoped
/// `NOTICE_SINK` instead of being emitted eagerly. `what` names the
/// construct for error messages (e.g. `function "f"`, `CTE "x"`).
/// The v0.89 `pending_updates` overlay is NOT consulted by nested DML —
/// an honest bound of this version.
pub(crate) fn run_nested_dml(
    call_q: &mut Q,
    stmt: &Stmt,
    what: &str,
) -> Result<SelectOut, ExecError> {
    let Some(w) = call_q.write.as_mut().map(QWrite::reborrow) else {
        return Err(exec_err(
            "0A000",
            format!(
                "DML is not supported in {} here: the calling query level has no statement write context",
                what
            ),
        ));
    };
    // PG19: a read-only transaction rejects DML inside nested query
    // levels just like top-level DML (25006). The server's
    // statement-level `read_only_violation` check cannot see inside
    // nested DML, so enforce it here; the v0.89 temporary-table
    // carve-out applies (only permanent relations are protected).
    if call_q.read_only {
        let (cmd, target) = match stmt {
            Stmt::Insert { table, .. } => ("INSERT", table.as_str()),
            Stmt::Update { table, .. } => ("UPDATE", table.as_str()),
            Stmt::Delete { table, .. } => ("DELETE", table.as_str()),
            // Unreachable: run_nested_dml only takes DML.
            _ => ("DML", ""),
        };
        if !call_q.eng.db.is_temp_table(call_q.session, target) {
            return Err(exec_err(
                "25006",
                format!("cannot execute {} in a read-only transaction", cmd),
            ));
        }
    }
    let mut dml_ctx = StmtCtx {
        snap: call_q.snap,
        own: call_q.own,
        write_xid: w.write_xid,
        all_xids: call_q.all_xids.clone(),
        level: w.level,
        writes: &mut *w.writes,
        session: call_q.session,
        role: call_q.role,
        read_only: call_q.read_only,
        default_toast_compression: w.default_toast_compression,
        notices: Vec::new(),
    };
    let out = execute_inner(&mut *call_q.eng, &mut dml_ctx, stmt)?;
    // Deliver trigger/RAISE notices through the statement-scoped sink
    // (PG19 emits them as generated, even if the statement later
    // fails — the sink is drained by the outer `execute()`).
    for msg in dml_ctx.notices.drain(..) {
        push_notice(msg);
    }
    match out {
        ExecResult::Dml { columns, rows, .. } => Ok(SelectOut { columns, rows }),
        // Unreachable: the DML executors always return `Dml`.
        _ => Err(exec_err(
            "XX000",
            format!("{} body DML did not produce a DML result", what),
        )),
    }
}

/// v1.33: run a data-modifying statement (INSERT/UPDATE/DELETE) from
/// inside a SQL or plpgsql function body. Thin wrapper over
/// `run_nested_dml` (v1.39).
pub(crate) fn run_func_dml(
    call_q: &mut Q,
    stmt: &Stmt,
    func_name: &str,
) -> Result<SelectOut, ExecError> {
    run_nested_dml(call_q, stmt, &format!("function \"{}\"", func_name))
}

/// v1.03: how a plpgsql statement list finished executing. PG's
/// `exec_stmt_block` distinguishes "fell off the end" from the
/// `PLPGSQL_RC_RETURN` code; the bounded executor needs the same
/// distinction so a `FOR` loop body can `RETURN` out of the whole
/// function while `RETURN NEXT` merely accumulates a row.
pub(crate) enum PlFlow {
    /// `RETURN <expr>` - terminal; carries the SELECT output.
    Return(SelectOut),
    /// Fell off the end of the list. Only reachable in SETOF bodies
    /// (scalar bodies always end with RETURN - the parser appends an
    /// implicit `RETURN NULL`, PG19 `add_dummy_return`, pl_comp.c).
    Continue,
}

/// v1.03: mutable execution state for one plpgsql call.
pub(crate) struct PlpgsqlRun {
    /// Bound arguments followed by one slot per DECLAREd variable
    /// (`params[nargs + i]`). Named argument/variable references were
    /// rewritten to these positions at CREATE/rebuild time.
    pub(crate) params: Vec<Option<Value>>,
    pub(crate) nargs: usize,
    /// Accumulated `RETURN NEXT` rows (SETOF functions only).
    pub(crate) accum: Vec<Row>,
}

/// v1.03: evaluate a `SELECT <expr>` plpgsql fragment (RETURN, RETURN
/// NEXT, `:=` assignment) with the current parameter bindings.
pub(crate) fn run_plpgsql_select(
    q: &mut Q,
    scopes: &[Scope],
    sel: &Stmt,
    run: &PlpgsqlRun,
    what: &str,
) -> Result<SelectOut, ExecError> {
    let Stmt::Select(inner) = sel else {
        return Err(exec_err(
            "XX000",
            format!("plpgsql {} is not a SELECT", what),
        ));
    };
    let mut stmt = Stmt::Select(inner.clone());
    subst_params(&mut stmt, &run.params)?;
    let Stmt::Select(bound) = stmt else {
        return Err(exec_err(
            "XX000",
            format!("plpgsql {} lost its SELECT", what),
        ));
    };
    run_select(q, &bound, scopes)
}

/// v1.03: slot of a DECLAREd variable in [`PlpgsqlRun::params`].
/// Undeclared names are rejected at CREATE; this is a
/// catalog-corruption guard (XX000, never a user-facing code path).
pub(crate) fn plpgsql_var_index(
    pb: &crate::sql::PlpgsqlBody,
    var: &str,
) -> Result<usize, ExecError> {
    pb.decls.iter().position(|d| d.name == var).ok_or_else(|| {
        exec_err(
            "XX000",
            format!("plpgsql variable \"{}\" is not declared", var),
        )
    })
}

/// v1.01: run one statement list of a bounded PL/pgSQL body.
/// `RETURN` ends the function immediately (like PG's
/// `PLPGSQL_RC_RETURN`); `RETURN NEXT` appends a row and continues;
/// falling off the end is `PlFlow::Continue` (only SETOF bodies can do
/// this - scalar bodies end with RETURN).
pub(crate) fn run_plpgsql_stmts(
    q: &mut Q,
    scopes: &[Scope],
    stmts: &[crate::sql::PlpgsqlStmt],
    pb: &crate::sql::PlpgsqlBody,
    run: &mut PlpgsqlRun,
    ret_type: &str,
) -> Result<PlFlow, ExecError> {
    for s in stmts {
        match s {
            crate::sql::PlpgsqlStmt::Return(sel) => {
                let out = run_plpgsql_select(q, scopes, sel, run, "RETURN")?;
                return Ok(PlFlow::Return(out));
            }
            crate::sql::PlpgsqlStmt::Utility(u) => {
                // Utility statements execute for side effects; results
                // are discarded. Only ANALYZE is in the bounded subset
                // (enforced at CREATE); anything else is a corrupt
                // catalog entry.
                let crate::sql::Stmt::Analyze { table } = u else {
                    return Err(exec_err(
                        "XX000",
                        "unsupported utility statement in plpgsql body".to_string(),
                    ));
                };
                exec_analyze_core(&mut *q.eng, q.snap, q.own, q.session, q.role, table)?;
            }
            crate::sql::PlpgsqlStmt::Raise {
                level,
                format,
                args,
            } => {
                // v1.02: `RAISE NOTICE|EXCEPTION '<format>', <args>...`
                // (PG19 `exec_stmt_raise`, pl_exec.c). Each `%` consumes
                // the next argument in its text-cast form (`%%` is a
                // literal `%`); the shared `format_raise_message`
                // evaluator is the same one the v1.00 trigger bodies
                // use. Arguments are `$n`-substituted (named args were
                // rewritten at CREATE) and evaluated like any
                // expression. NOTICE goes to the statement-scoped sink
                // (`execute()` drains it into `StmtCtx.notices`);
                // EXCEPTION aborts the call, defaulting to P0001
                // (raise_exception) like PG, and is trappable by the
                // v1.01 WHEN handlers.
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    let mut e = a.clone();
                    subst_expr(&mut e, &run.params)?;
                    vals.push(eval_expr(q, scopes, &e)?);
                }
                let msg = format_raise_message(format, &vals);
                match level {
                    crate::sql::RaiseLevel::Notice => push_notice(msg),
                    crate::sql::RaiseLevel::Exception => {
                        return Err(exec_err("P0001", msg));
                    }
                }
            }
            crate::sql::PlpgsqlStmt::Assign { var, select } => {
                // v1.03: `var := <expr>` (PG19 `exec_stmt_assign`,
                // pl_exec.c): evaluate like RETURN, coerce to the
                // declared type (`exec_assign_value`), and write the
                // variable's slot. A 0-row SELECT assigns NULL (PG's
                // `SELECT ... INTO` strictness does not apply to `:=`).
                let idx = plpgsql_var_index(pb, var)?;
                let out = run_plpgsql_select(q, scopes, select, run, "assignment")?;
                let v = out
                    .rows
                    .first()
                    .map(|r| r[0].clone())
                    .unwrap_or(Value::Null);
                let v = coerce_to_type_name(q, &v, &pb.decls[idx].type_name)?;
                run.params[run.nargs + idx] = Some(v);
            }
            crate::sql::PlpgsqlStmt::ForQuery { var, query, body } => {
                // v1.03: `FOR var IN <query> LOOP` (PG19
                // `exec_stmt_fors`, pl_exec.c). The query is a
                // row-producing SELECT or EXPLAIN; each row binds the
                // (DECLAREd) variable to the row's first column,
                // coerced to its declared type.
                let idx = plpgsql_var_index(pb, var)?;
                let mut qq = query.clone();
                subst_params(&mut qq, &run.params)?;
                let rows: Vec<Row> = match &qq {
                    Stmt::Select(sel) => run_select(q, sel, scopes)?.rows,
                    Stmt::Explain {
                        stmt,
                        analyze,
                        costs,
                        opts,
                    } => {
                        let Stmt::Select(inner) = &**stmt else {
                            return Err(exec_err(
                                "XX000",
                                "plpgsql FOR over non-SELECT EXPLAIN".to_string(),
                            ));
                        };
                        // v1.46: thread VERBOSE through for `Output:`.
                        explain_rows(q, scopes, inner, *analyze, *costs, opts.verbose)?
                    }
                    _ => {
                        return Err(exec_err(
                            "XX000",
                            "plpgsql FOR query is not SELECT or EXPLAIN".to_string(),
                        ));
                    }
                };
                for row in rows {
                    let cell = row.first().cloned().unwrap_or(Value::Null);
                    let cell = coerce_to_type_name(q, &cell, &pb.decls[idx].type_name)?;
                    run.params[run.nargs + idx] = Some(cell);
                    match run_plpgsql_stmts(q, scopes, body, pb, run, ret_type)? {
                        PlFlow::Return(out) => return Ok(PlFlow::Return(out)),
                        PlFlow::Continue => {}
                    }
                }
            }
            crate::sql::PlpgsqlStmt::ReturnNext(sel) => {
                // v1.03: `RETURN NEXT <expr>` (PG19
                // `exec_stmt_return_next`, pl_exec.c): coerce to the
                // function's return type and append to the result set
                // without ending the function.
                let out = run_plpgsql_select(q, scopes, sel, run, "RETURN NEXT")?;
                for row in out.rows {
                    let v = row.first().cloned().unwrap_or(Value::Null);
                    let v = coerce_to_type_name(q, &v, ret_type)?;
                    run.accum.push(Row::new(vec![v]));
                }
            }
        }
    }
    Ok(PlFlow::Continue)
}

/// v1.01: run a bounded PL/pgSQL body: statement sequence with
/// EXCEPTION/WHEN handlers (PG19 `exec_stmt_block`, `pl_exec.c`).
///
/// The body runs statement by statement; the first statement raising
/// an error trapped by a WHEN clause diverts to that clause's
/// statements (first matching clause wins, PG's
/// `exception_matches_conditions`). An untrapped error propagates.
/// Errors raised *inside* a handler propagate untrapped (PG only wraps
/// the body in PG_TRY, not the handler).
///
/// Deliberate deviation from PG19: no subtransaction is opened around
/// the body, so a trapped error does NOT roll back the effects of
/// statements that already ran (rustgres has no subtransaction
/// machinery; ANALYZE has no transactional effects to roll back).
///
/// v1.03: DECLAREd variables get one `None` slot each appended to the
/// parameter vector; `RETURN NEXT` accumulates rows for SETOF
/// functions (the output column is named after the function, PG19
/// `pl_exec.c` `exec_stmt_return_next` + `do_compile` naming).
pub(crate) fn run_plpgsql_body(
    q: &mut Q,
    scopes: &[Scope],
    fdef: &crate::storage::FuncDef,
    pb: &crate::sql::PlpgsqlBody,
    params: &[Option<Value>],
    pending: Option<Rc<RefCell<Vec<(String, u64, Row)>>>>,
) -> Result<SelectOut, ExecError> {
    // Run the body one query level deeper (fresh CTE scope, like a
    // subquery); the caller's scopes stay visible for outer refs.
    let mut call_q = Q {
        eng: &mut *q.eng,
        snap: q.snap,
        own: q.own,
        all_xids: q.all_xids.clone(),
        session: q.session,
        role: q.role,
        read_only: q.read_only,
        depth: q.depth + 1,
        lock_ids: &mut *q.lock_ids,
        ctes: q.ctes.clone(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: q.priv_scopes.clone(),
        hashed_exists: q.hashed_exists.clone(),
        immutable_fn_cache: q.immutable_fn_cache.clone(),
        // v1.37: FRESH plan-fold memo (see field doc): plpgsql bodies execute
        // with statement-level variable state; never share call-site keys.
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: q.hashed_in.clone(),
        // v0.89: volatile function bodies see the in-flight UPDATE's
        // already-processed rows (caller-gated); stable/immutable
        // bodies see the statement snapshot (None).
        pending_updates: pending,
        write: q.write.as_mut().map(QWrite::reborrow),
    };
    let nargs = params.len();
    let mut run = PlpgsqlRun {
        params: params.to_vec(),
        nargs,
        accum: Vec::new(),
    };
    run.params
        .extend(std::iter::repeat(None).take(pb.decls.len()));
    let flow = match run_plpgsql_stmts(&mut call_q, scopes, &pb.stmts, pb, &mut run, &fdef.ret_type)
    {
        Ok(f) => f,
        Err(e) => {
            let handler = pb.handlers.iter().find(|h| {
                h.sqlstates
                    .iter()
                    .any(|c| crate::sql::plpgsql_condition_matches(c, &e.code))
            });
            match handler {
                Some(h) => {
                    run_plpgsql_stmts(&mut call_q, scopes, &h.stmts, pb, &mut run, &fdef.ret_type)?
                }
                None => return Err(e),
            }
        }
    };
    match flow {
        PlFlow::Return(out) => Ok(out),
        PlFlow::Continue => {
            // v1.03: SETOF bodies fall off the end and return the
            // accumulated RETURN NEXT rows (possibly zero). Scalar
            // bodies cannot reach here: the parser appends an implicit
            // `RETURN NULL`.
            if !fdef.returns_set {
                return Err(exec_err(
                    "XX000",
                    "plpgsql statement list ended without RETURN".to_string(),
                ));
            }
            let coltype = resolve_func_type_name(
                call_q.eng,
                call_q.snap,
                call_q.own,
                call_q.session,
                &fdef.ret_type,
            )?;
            Ok(SelectOut {
                columns: vec![(fdef.name.clone(), coltype)],
                rows: run.accum,
            })
        }
    }
}

/// v0.86: scalar call of a user function. SQL bodies run via
/// `run_func_body` (exactly one row, one column expected);
/// internal bodies dispatch to `eval_internal_function`.
pub(crate) fn call_user_function(
    q: &mut Q,
    scopes: &[Scope],
    fdef: &crate::storage::FuncDef,
    args: &[Value],
) -> Result<Value, ExecError> {
    if fdef.lang == crate::sql::FuncLang::Internal {
        return eval_internal_function(&fdef.body, args);
    }
    if fdef.returns_set {
        return Err(exec_err(
            "0A000",
            format!(
                "set-returning function \"{}\" used in scalar context",
                fdef.name
            ),
        ));
    }
    let out = run_func_body(q, scopes, fdef, args, volatile_pending(q, fdef))?;
    if out.rows.is_empty() && out.columns.is_empty() {
        // STRICT-on-NULL short-circuit from run_func_body.
        return Ok(Value::Null);
    }
    if out.rows.len() != 1 || out.columns.len() != 1 {
        return Err(exec_err(
            "42601",
            format!(
                "function \"{}\" must return exactly one row and one column in scalar context",
                fdef.name
            ),
        ));
    }
    // Coerce the body result to the declared return type (PG19 casts
    // the SELECT output to the function's return type).
    coerce_to_type_name(q, &out.rows[0][0], &fdef.ret_type)
}

/// v0.86: table-function call (FROM f(...)). Returns
/// (column names, column types, rows). A scalar function returning a
/// composite/table rowtype expands to its fields (PG19); a
/// set-returning function returns its rows directly.
pub(crate) fn call_table_function(
    q: &mut Q,
    scopes: &[Scope],
    fdef: &crate::storage::FuncDef,
    args: &[Value],
) -> Result<(Vec<String>, Vec<ColType>, Vec<Row>), ExecError> {
    if fdef.lang == crate::sql::FuncLang::Internal {
        return Err(exec_err(
            "0A000",
            format!("internal function \"{}\" cannot be used in FROM", fdef.name),
        ));
    }
    let out = run_func_body(q, scopes, fdef, args, volatile_pending(q, fdef))?;
    if out.rows.is_empty() && out.columns.is_empty() {
        // STRICT-on-NULL: zero rows.
        return Ok((vec![], vec![], vec![]));
    }
    if !fdef.returns_set {
        // Scalar function in FROM: PG19 expands a composite result into
        // columns; a non-composite scalar becomes one column.
        if out.rows.len() != 1 || out.columns.len() != 1 {
            return Err(exec_err(
                "42601",
                format!(
                    "function \"{}\" must return exactly one row in FROM",
                    fdef.name
                ),
            ));
        }
        let cell = coerce_to_type_name(q, &out.rows[0][0], &fdef.ret_type)?;
        if let Value::Record(fields) = cell {
            let (names, types) = func_rowtype_fields(q, &fdef.ret_type)?;
            let mut row = Vec::with_capacity(fields.len());
            for (_, fval) in &fields {
                row.push(fval.clone());
            }
            return Ok((names, types, vec![Row::new(row)]));
        }
        let colname = out.columns[0].0.clone();
        let coltype = out.columns[0].1.clone();
        return Ok((vec![colname], vec![coltype], out.rows));
    }
    // SETOF: rows as produced; column names from the SELECT output.
    let names: Vec<String> = out.columns.iter().map(|(n, _)| n.clone()).collect();
    let types: Vec<ColType> = out.columns.iter().map(|(_, t)| t.clone()).collect();
    Ok((names, types, out.rows))
}

/// v0.86: resolve a composite/table rowtype name to its
/// (field name, field type) list — table columns for table names,
/// declared fields for CREATE TYPE composites.
pub(crate) fn func_rowtype_fields(
    q: &mut Q,
    type_name: &str,
) -> Result<(Vec<String>, Vec<ColType>), ExecError> {
    if let Some(t) = q
        .eng
        .db
        .find_table(type_name, q.snap, &q.all_xids, q.session)
    {
        let names = t.columns.iter().map(|(n, _)| n.clone()).collect();
        let types = t.columns.iter().map(|(_, c)| c.clone()).collect();
        return Ok((names, types));
    }
    if let Some(st) = q.eng.db.types.get(type_name) {
        if let Some(fields) = &st.composite {
            let names = fields.iter().map(|(n, _, _)| n.clone()).collect();
            let types = fields.iter().map(|(_, c, _)| c.clone()).collect();
            return Ok((names, types));
        }
    }
    Err(exec_err(
        "42704",
        format!("type \"{}\" does not exist", type_name),
    ))
}

/// v0.86: internal-language function dispatch. Only a bounded set of C
/// symbols is implemented (the ones the conformance corpus needs);
/// anything else is an honest 0A000.
pub(crate) fn eval_internal_function(symbol: &str, args: &[Value]) -> Result<Value, ExecError> {
    match symbol {
        // int4eq(int4, int4) -> bool (PG19 internal equality).
        "int4eq" => {
            if args.len() != 2 {
                return Err(exec_err(
                    "42883",
                    format!("function int4eq expects 2 arguments, got {}", args.len()),
                ));
            }
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (a, b) => {
                    let ord = cmp_ordering(a, b, CmpOp::Eq)?;
                    Ok(Value::Bool(ord == Some(Ordering::Equal)))
                }
            }
        }
        // v1.38: int4in(cstring) -> int4 (PG19 int.c `int4in` via
        // `pg_strtoint32_safe`). `parse_int_text` implements the same
        // input grammar (base prefixes, underscores, 22P02/22003); the
        // i128 result is narrowed to int32 with PG's 22003 range error.
        // STRICT: NULL in -> NULL out.
        "int4in" => {
            if args.len() != 1 {
                return Err(exec_err(
                    "42883",
                    format!("function int4in expects 1 argument, got {}", args.len()),
                ));
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let v = parse_int_text(s)?;
                    let narrowed = i32::try_from(v).map_err(|_| {
                        exec_err(
                            "22003",
                            format!("value \"{}\" is out of range for type integer", s),
                        )
                    })?;
                    // rustgres keeps INT4 in `Value::Int(i64)`.
                    Ok(Value::Int(narrowed as i64))
                }
                other => Err(exec_err(
                    "42804",
                    format!(
                        "int4in: argument must be cstring, got {:?}",
                        other.col_type()
                    ),
                )),
            }
        }
        // v1.38: int4out(int4) -> cstring (PG19 int.c `int4out` via
        // `pg_ltoa`: plain decimal). STRICT.
        "int4out" => {
            if args.len() != 1 {
                return Err(exec_err(
                    "42883",
                    format!("function int4out expects 1 argument, got {}", args.len()),
                ));
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(v) => Ok(Value::Text(((*v as i32).to_string()).into())),
                other => Err(exec_err(
                    "42804",
                    format!(
                        "int4out: argument must be integer, got {:?}",
                        other.col_type()
                    ),
                )),
            }
        }
        // v1.38: int8in(cstring) -> int8 (PG19 int.c `int8in` via
        // `pg_strtoint64_safe`). STRICT.
        "int8in" => {
            if args.len() != 1 {
                return Err(exec_err(
                    "42883",
                    format!("function int8in expects 1 argument, got {}", args.len()),
                ));
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => {
                    let v = parse_int_text(s)?;
                    let narrowed = i64::try_from(v).map_err(|_| {
                        exec_err(
                            "22003",
                            format!("value \"{}\" is out of range for type bigint", s),
                        )
                    })?;
                    Ok(Value::BigInt(narrowed))
                }
                other => Err(exec_err(
                    "42804",
                    format!(
                        "int8in: argument must be cstring, got {:?}",
                        other.col_type()
                    ),
                )),
            }
        }
        // v1.38: int8out(int8) -> cstring (PG19 int.c `int8out` via
        // `pg_ltoa`: plain decimal). STRICT.
        "int8out" => {
            if args.len() != 1 {
                return Err(exec_err(
                    "42883",
                    format!("function int8out expects 1 argument, got {}", args.len()),
                ));
            }
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::BigInt(v) => Ok(Value::Text(v.to_string().into())),
                other => Err(exec_err(
                    "42804",
                    format!(
                        "int8out: argument must be bigint, got {:?}",
                        other.col_type()
                    ),
                )),
            }
        }
        _ => Err(exec_err(
            "0A000",
            format!("internal function symbol \"{}\" is not implemented", symbol),
        )),
    }
}

/// v0.95: parameter names for builtins that accept `name => expr`
/// named-argument notation (PG19 `pg_proc.proargnames`). Only functions
/// with a declared entry (or user functions, whose signatures live in
/// storage) support named notation; anything else using `=>` gets 42883,
/// like PG's "function ... does not exist".
pub(crate) fn builtin_param_names(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        // `parse_ident(qualname text, strict boolean DEFAULT true)`.
        "parse_ident" => &["qualname", "strict"],
        _ => return None,
    })
}

/// v0.95: resolve `name => expr` named arguments to positional order
/// (PG19 `reorder_function_arguments`). Positional args must precede
/// named ones (42601); unknown or duplicate names are 42883. rustgres
/// has no defaulted parameters except `parse_ident.strict`, which is
/// filled with `true` when omitted.
pub(crate) fn resolve_named_args(
    eng: &Engine,
    name: &str,
    args: &[Expr],
) -> Result<Vec<Expr>, ExecError> {
    let mut positional: Vec<&Expr> = Vec::new();
    let mut named: Vec<(&str, &Expr)> = Vec::new();
    let mut seen_named = false;
    for a in args {
        match a {
            Expr::NamedArg { name: n, expr } => {
                seen_named = true;
                named.push((n.as_str(), expr.as_ref()));
            }
            _ => {
                if seen_named {
                    return Err(exec_err(
                        "42601",
                        "positional argument cannot follow named argument".to_string(),
                    ));
                }
                positional.push(a);
            }
        }
    }
    // Parameter names: builtin table, else the user-function definition.
    let params: Vec<Option<String>> = if let Some(ns) = builtin_param_names(name) {
        ns.iter().map(|s| Some(s.to_string())).collect()
    } else if let Some(overloads) = eng.db.functions.get(name) {
        overloads
            .first()
            .map(|f| f.arg_names.clone())
            .unwrap_or_default()
    } else {
        return Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        ));
    };
    let n = params.len();
    if positional.len() > n {
        return Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        ));
    }
    let mut out: Vec<Option<&Expr>> = vec![None; n];
    for (i, e) in positional.iter().enumerate() {
        out[i] = Some(e);
    }
    for (nm, e) in named {
        let idx = params.iter().position(|p| p.as_deref() == Some(nm));
        match idx {
            None => {
                return Err(exec_err(
                    "42883",
                    format!("function {}({} => ...) does not exist", name, nm),
                ));
            }
            Some(i) => {
                if out[i].is_some() {
                    return Err(exec_err(
                        "42883",
                        format!("duplicate named argument {}", nm),
                    ));
                }
                out[i] = Some(e);
            }
        }
    }
    // Every parameter must be supplied, except `parse_ident.strict`,
    // which defaults to true (PG19).
    for (i, slot) in out.iter().enumerate() {
        if slot.is_none() {
            let is_strict_default = name == "parse_ident" && params[i].as_deref() == Some("strict");
            if !is_strict_default {
                return Err(exec_err(
                    "42883",
                    format!("function {}() does not exist", name),
                ));
            }
        }
    }
    let mut result = Vec::with_capacity(n);
    for slot in out {
        match slot {
            Some(e) => result.push(e.clone()),
            // `parse_ident.strict` default.
            None => result.push(Expr::Literal(crate::sql::Literal::Bool(true))),
        }
    }
    Ok(result)
}

/// v1.37: `node` is the call-site `Expr::Func` node's address, keying
/// the plan-fold memo (`Q::plan_fold_memo`). It always identifies the
/// original call site, even when named-argument reordering below
/// replaces the `args` slice.
pub(crate) fn eval_func(
    q: &mut Q,
    scopes: &[Scope],
    name: &str,
    args: &[Expr],
    node: *const Expr,
) -> Result<Value, ExecError> {
    // v0.95: resolve `name => expr` named arguments to positional order
    // (PG19 `reorder_function_arguments`). Zero-cost when absent.
    let owned: Vec<Expr>;
    let args: &[Expr] = if args.iter().any(|a| matches!(a, Expr::NamedArg { .. })) {
        owned = resolve_named_args(q.eng, name, args)?;
        &owned
    } else {
        args
    };
    // v0.97: pg_typeof is statically typed (PG19 resolves it at parse
    // analysis): a cast to a domain, or a reference to a domain-typed
    // column, reports the domain name — not the base type the runtime
    // value is stored as (domain identity is erased from `Value`).
    // Like PG19, the argument is still evaluated (its errors and
    // volatility side effects happen); only the reported name differs.
    // Wrong-arity calls fall through to the normal arity check below.
    if name == "pg_typeof" && args.len() == 1 {
        if let Some(dname) = pg_typeof_domain_name(q, scopes, &args[0]) {
            eval_expr(q, scopes, &args[0])?;
            return Ok(Value::text(dname));
        }
        let v = eval_expr(q, scopes, &args[0])?;
        return Ok(Value::text(v.type_name()));
    }
    // v0.80: `GROUPING(...)` is only meaningful at group level (handled
    // by `eval_grouped`). Reaching scalar evaluation means it sits in a
    // query without GROUP BY, inside a nested query level, or in a spot
    // validation missed — PG19 rejects all of these at analysis (42803).
    if name == "grouping" {
        return Err(exec_err(
            "42803",
            "arguments to GROUPING must be grouping expressions of the associated query level",
        ));
    }
    // v0.91: hidden `__any_all_array` builtin — the parser desugars
    // `expr op ANY|ALL|SOME (array_expr)` into it. Needs engine access
    // for user-defined operators, so it cannot go through the pure
    // eval_func_vals path.
    if name == "__any_all_array" {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(normalize_func_arg(name, eval_expr(q, scopes, a)?));
        }
        return eval_any_all_array(q, scopes, &vals);
    }
    // v0.9: sequence functions need engine + snapshot + session access;
    // they cannot go through the pure eval_func_vals path.
    if matches!(name, "nextval" | "currval" | "setval" | "lastval") {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(eval_expr(q, scopes, a)?);
        }
        check_builtin_arity(name, &vals)?;
        let (eng, snap, own, session) = (&mut *q.eng, &*q.snap, q.own, q.session);
        return eval_sequence_func(eng, snap, own, session, q.role, q.read_only, name, &vals);
    }
    // v0.37: pg_column_compression needs row provenance (not just the
    // value), so it cannot go through the pure eval_func_vals path.
    if name == "pg_column_compression" {
        if args.len() != 1 {
            return Err(exec_err(
                "42883",
                "function pg_column_compression() does not exist".to_string(),
            ));
        }
        let v = eval_expr(q, scopes, &args[0])?;
        return pg_column_compression_value(q, scopes, &args[0], &v, true);
    }
    // v0.37: pg_relation_size needs engine access.
    if name == "pg_relation_size" {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(eval_expr(q, scopes, a)?);
        }
        check_builtin_arity(name, &vals)?;
        return eval_pg_relation_size(q, &vals);
    }
    // v1.13: pg_size_pretty(bigint) -> text (PG19 misc.c).
    if name == "pg_size_pretty" {
        let mut vals = Vec::with_capacity(args.len());
        for a in args {
            vals.push(eval_expr(q, scopes, a)?);
        }
        check_builtin_arity(name, &vals)?;
        return eval_pg_size_pretty(&vals);
    }
    // v0.86: user-defined functions. A definition matching by name and
    // arity takes the call (raw argument values; the call coerces to
    // the declared types). Builtins keep precedence for names with no
    // user definition.
    // v0.87: user-defined functions — resolve the best overload by
    // (name, arg count, arg types). A definition matching by name and
    // arity takes the call (raw argument values; the call coerces to
    // the declared types). Builtins keep precedence for names with no
    // user definition.
    // v0.90: `VARIADIC expr` args are expanded here (before UDF
    // resolution) so both UDF and builtin paths see the expanded args.
    // A NULL variadic array makes the whole call NULL (PG semantics).
    {
        // v1.37: planner-time Const substitution (PG19
        // `eval_const_expressions`): probe the call-site memo BEFORE
        // evaluating arguments. A hit means this `Expr::Func` node
        // already folded this statement — it is now a Const: return it
        // without touching the arguments at all. The borrow ends here,
        // before any `&mut q` use below.
        if let Some(v) = q.plan_fold_memo.borrow().get(&node).cloned() {
            return Ok(v);
        }
        let mut raw_vals = Vec::with_capacity(args.len());
        for a in args {
            if let Some(inner) = is_variadic_marker(a) {
                let v = eval_expr(q, scopes, inner)?;
                match expand_variadic_value(v)? {
                    // v1.24: PG19 text_format() treats VARIADIC NULL as a
                    // zero-length array; every other variadic builtin
                    // makes the whole call NULL.
                    None if variadic_null_expands_empty(name) => {}
                    None => return Ok(Value::Null),
                    Some(expanded) => raw_vals.extend(expanded),
                }
            } else {
                raw_vals.push(eval_expr(q, scopes, a)?);
            }
        }
        if let Some(fdef) = resolve_function_overload(q.eng, name, &raw_vals) {
            // v1.36: IMMUTABLE SQL-function constant folding (PG19
            // `evaluate_function`, optimizer/util/clauses.c:5205). The
            // arguments above were evaluated per row exactly as before —
            // only the body execution is shared: with row-constant
            // arguments and a provably row-independent,
            // side-effect-free body, the first call's result is
            // identical to every per-row execution's, so it serves the
            // whole statement. Anything not provably safe fails open to
            // the per-row path below. Errors are never cached: a
            // failing first call raises here exactly as it would
            // without the cache.
            // v1.37: the eligibility check is caller-CTE-aware (see
            // `select_refs_caller_cte`); a folded call also populates
            // the call-site memo, substituting the evaluated Const into
            // the plan for the rest of the statement.
            if immutable_fold_eligible(&q.eng.db, scopes, args, &fdef, &q.ctes) {
                let key = ImmutableFnKey {
                    name: fdef.name.clone(),
                    args: raw_vals.iter().map(fn_key_val).collect(),
                };
                if let Some(hit) = q.immutable_fn_cache.borrow().get(&key).cloned() {
                    q.plan_fold_memo.borrow_mut().insert(node, hit.clone());
                    return Ok(hit);
                }
                let v = call_user_function(q, scopes, &fdef, &raw_vals)?;
                q.immutable_fn_cache.borrow_mut().insert(key, v.clone());
                q.plan_fold_memo.borrow_mut().insert(node, v.clone());
                return Ok(v);
            }
            return call_user_function(q, scopes, &fdef, &raw_vals);
        }
        let mut vals = Vec::with_capacity(raw_vals.len());
        for v in raw_vals {
            vals.push(normalize_func_arg(name, v));
        }
        return eval_func_vals(name, &vals);
    }
}

/// v0.37: `pg_column_compression(any)` — like PG19's implementation in
/// `varlena.c`: NULL for a NULL argument and for fixed-width types;
/// for a varlena value, the stored compression method name when the
/// value is compressed in its row, else NULL.
/// v0.41: reports the actual recorded method (`pglz` or `lz4`) instead
/// of always `pglz`; the stale "custom LZ77" note is gone since v0.40
/// made the PGLZ payload byte-identical to PG19's `pglz_compress()`.
///
/// The argument may be any expression (PG's function is not restricted
/// to simple column references): only a plain column reference on a
/// base table can carry provenance, and only in the row-wise path
/// (`use_prov`). Everywhere else the value at hand is necessarily
/// inline, so a non-null varlena yields NULL.
pub(crate) fn pg_column_compression_value(
    q: &mut Q,
    scopes: &[Scope],
    arg: &Expr,
    value: &Value,
    use_prov: bool,
) -> Result<Value, ExecError> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    // Resolve a plain column reference to its schema position, for the
    // type check and (row-wise) the provenance lookup. Columns usually
    // arrive as ResolvedCol after the resolver pass; handle the
    // unresolved shape too.
    let resolved: Option<(usize, usize)> = match arg {
        Expr::ResolvedCol { frame, idx } => Some((*frame, *idx)),
        Expr::Column { table, name } => Some(resolve_col(scopes, table.as_deref(), name)?),
        _ => None,
    };
    let is_varlena = match resolved {
        Some((si, ci)) => scopes
            .get(si)
            .and_then(|s| s.schema.get(ci))
            .map(|c| c.ty.is_toastable())
            .unwrap_or(false),
        // A computed value: varlena-ness from the value itself (PG
        // checks the argument's type the same way).
        None => matches!(
            value,
            Value::Text(_) | Value::BpChar(_) | Value::Bytea(_) | Value::Numeric(_)
        ),
    };
    if !is_varlena {
        return Ok(Value::Null);
    }
    let (Some((si, ci)), true) = (resolved, use_prov) else {
        // No base row to attribute (an expression, or grouped
        // evaluation): the value at hand is inline by construction.
        return Ok(Value::Null);
    };
    let scope = &scopes[si];
    let (Some(prov), Some(qcol)) = (scope.prov, scope.schema.get(ci)) else {
        return Ok(Value::Null);
    };
    if qcol.qual.is_empty() {
        // A merged USING/NATURAL key belongs to no single table.
        return Ok(Value::Null);
    }
    // Provenance entries run in FROM-item order, one per leaf item,
    // matching the schema's distinct non-empty qualifiers in
    // first-appearance order.
    let mut quals: Vec<&str> = Vec::new();
    for c in scope.schema {
        if !c.qual.is_empty() && !quals.contains(&c.qual.as_str()) {
            quals.push(c.qual.as_str());
        }
    }
    let entry = match quals
        .iter()
        .position(|qq| *qq == qcol.qual.as_str())
        .and_then(|pi| prov.get(pi))
    {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let (table_name, row_id) = (entry.table.as_str(), entry.row_id);
    let t = match q
        .eng
        .db
        .find_table(table_name, q.snap, &q.all_xids, q.session)
    {
        Some(t) => t,
        None => return Ok(Value::Null),
    };
    // The exact row version by id (it is the row under evaluation, so
    // no duplicate-value confusion is possible).
    let rv = match t.rows.iter().find(|r| r.id == row_id) {
        Some(rv) => rv,
        None => return Ok(Value::Null),
    };
    // `src_ord` is the column's index in its source table, preserved
    // through joins and (positional) column aliases.
    let flag = rv.toast.get(qcol.src_ord as usize).copied().unwrap_or(0);
    if flag == 0 {
        return Ok(Value::Null);
    }
    match t.toast_info.get(&flag) {
        Some(info) if info.compressed => Ok(Value::text(info.method.name())),
        _ => Ok(Value::Null),
    }
}

/// v1.41: PG19 `typlen`/`typalign` per `ColType`, grounded in
/// `src/include/catalog/pg_type.dat` of the local PG19 source.
/// Returns `(typlen, align_bytes)`; `typlen == -1` marks a varlena.
pub(crate) fn pg_typlen_align(ty: &ColType) -> (i32, usize) {
    match ty {
        ColType::Bool => (1, 1),        // 'c'
        ColType::SingleChar => (1, 1),  // "char": 1/'c'
        ColType::SmallInt => (2, 2),    // int2: 2/'s'
        ColType::Int => (4, 4),         // int4: 4/'i'
        ColType::BigInt => (8, 8),      // int8: 8/'d'
        ColType::Float4 => (4, 4),      // float4: 4/'i'
        ColType::Float => (8, 8),       // float8: 8/'d'
        ColType::Numeric(_) => (-1, 4), // numeric: -1/'i'
        ColType::Text => (-1, 4),       // text: -1/'i'
        ColType::Char(_) => (-1, 4),    // bpchar: -1/'i'
        ColType::Varchar(_) => (-1, 4), // varchar: -1/'i'
        ColType::Bytea => (-1, 4),      // bytea: -1/'i'
        ColType::Bit => (-1, 4),        // varbit: -1/'i'
        ColType::Json => (-1, 4),       // json: -1/'i'
        ColType::Array(_) => (-1, 4),   // array types: 'i'
        ColType::Date => (4, 4),        // date: 4/'i'
        ColType::Timestamp => (8, 8),   // timestamp: 8/'d'
        ColType::Timestamptz => (8, 8), // timestamptz: 8/'d'
        ColType::Uuid => (16, 1),       // uuid: 16/'c'
        ColType::Name => (64, 1),       // name: 64/'c'
        ColType::Regclass => (4, 4),    // regclass: 4/'i'
        ColType::Xid => (4, 4),         // xid: 4/'i'
        ColType::PgLsn => (8, 8),       // pg_lsn: 8/'d'
        ColType::Tid => (6, 2),         // tid: 6/'s'
        // Pseudo/composite types never persist in table columns; map
        // them to varlena so the layout code stays total.
        ColType::Record | ColType::Composite => (-1, 8), // record: -1/'d'
    }
}

/// v1.41: on-disk size of a PG `numeric` datum: the 8-byte NumericVar
/// header (ndigits/weight/sign/dscale) plus 2 bytes per base-10000
/// digit. Approximates PG's `numeric` varlena payload; the varlena
/// header itself is added by the caller.
pub(crate) fn pg_numeric_data_len(n: &Numeric) -> usize {
    let digits: usize = if let Some(big) = &n.big {
        big.decimal_digits() as usize
    } else if n.unscaled == 0 {
        0
    } else {
        let mut x = n.unscaled.unsigned_abs();
        let mut d = 0;
        while x > 0 {
            d += 1;
            x /= 10;
        }
        d
    };
    8 + 2 * ((digits + 3) / 4)
}

/// v1.41: PG19 heap tuple length (`t_len`) for one row version, per
/// `heap_form_tuple` (htup.c): `hoff = MAXALIGN(23 + bitmaplen)`,
/// per-column `typalign` layout, final `MAXALIGN`. NULLs contribute
/// only to the null bitmap (present iff any column is nullable).
/// Toasted cells count as the 18-byte toast pointer datum
/// (`TOAST_POINTER_SIZE` = `VARHDRSZ_EXTERNAL`(2) +
/// `sizeof(varatt_external)`(16), varatt.h/detoast.h). Short varlenas
/// (data <= 126 bytes) take the 1-byte header (`VARHDRSZ_SHORT`),
/// else the 4-byte header. Compressed-inline values' exact size is
/// not retained by the engine; the pointer size is a documented
/// approximation for those cells (bounded ~2KB underestimate).
pub(crate) fn pg_heap_tuple_len(t: &Table, rv: &RowVersion) -> usize {
    fn maxalign(x: usize) -> usize {
        (x + 7) & !7
    }
    let ncols = t.columns.len();
    let has_null = rv.values.iter().any(|v| matches!(v, Value::Null));
    let bitmaplen = if has_null { (ncols + 7) / 8 } else { 0 };
    let mut off = maxalign(23 + bitmaplen);
    for (i, (_, cty)) in t.columns.iter().enumerate() {
        let v = rv.values.get(i).unwrap_or(&Value::Null);
        if matches!(v, Value::Null) {
            continue;
        }
        let (typlen, align) = pg_typlen_align(cty);
        // Toasted/out-of-line cell: the inline datum is the pointer.
        let datum: usize = if rv.toast.get(i).copied().unwrap_or(0) != 0 {
            18
        } else if typlen >= 0 {
            typlen as usize
        } else {
            let data: usize = match v {
                Value::Text(s) | Value::BpChar(s) => s.len(),
                Value::Bytea(b) => b.len(),
                // varbit: int32 bit-length + payload bytes.
                Value::BitString(bs) => 4 + bs.bytes.len(),
                Value::Numeric(n) => pg_numeric_data_len(n),
                // v0.79: arrays persist; approximate from the literal.
                Value::Array(a) => a.to_literal().len(),
                _ => 0,
            };
            data + if data <= 126 { 1 } else { 4 }
        };
        off = (off + align - 1) & !(align - 1);
        off += datum;
    }
    maxalign(off)
}

/// v1.41: number of 8 KiB heap pages PG19 would use for the given
/// visible row versions, simulating `RelationGetBufferForTuple`
/// (hio.c) with `fillfactor` from the table:
///
/// * `saveFreeSpace = BLCKSZ * (100 - fillfactor) / 100`
///   (`RelationGetTargetPageFreeSpace`, rel.h);
/// * `nearlyEmptyFreeSpace = MaxHeapTupleSize -
///   MaxHeapTuplesPerPage/8 * sizeof(ItemIdData)` = 8160 - 144 = 8016
///   (htup_details.h: MaxHeapTupleSize = 8160, MaxHeapTuplesPerPage =
///   291): when `len + saveFreeSpace > nearlyEmptyFreeSpace` the
///   fillfactor reservation is dropped and `targetFreeSpace =
///   max(len, nearlyEmptyFreeSpace)` — large tuples still land on a
///   nearly-empty page instead of forcing a new one;
/// * placement tries the cached target block first, then the lowest
///   block with enough free space (the FSM's leftmost search is
///   approximated by first-fit in block order), else extends;
/// * the fit check is `targetFreeSpace <= pd_upper - pd_lower -
///   sizeof(ItemIdData)` (`PageGetHeapFreeSpace`).
///
/// Documented approximation: only *visible* row versions are placed —
/// rustgres has no VACUUM, so dead-tuple space is not modeled (PG
/// would still count dead tuples' blocks until vacuumed).
pub(crate) fn pg_heap_page_count(t: &Table, lens: &[usize]) -> u64 {
    const BLCKSZ: usize = 8192;
    const PAGE_HEADER: usize = 24; // SizeOfPageHeaderData
    const ITEM_ID: usize = 4; // sizeof(ItemIdData)
    const fn maxalign(x: usize) -> usize {
        (x + 7) & !7
    }
    // htup_details.h formulas, evaluated for BLCKSZ = 8192.
    const MAX_HEAP_TUPLE_SIZE: usize = BLCKSZ - maxalign(PAGE_HEADER + ITEM_ID); // 8160
    const MAX_HEAP_TUPLES_PER_PAGE: usize = (BLCKSZ - PAGE_HEADER) / (maxalign(23) + ITEM_ID); // 291
    const NEARLY_EMPTY: usize = MAX_HEAP_TUPLE_SIZE - (MAX_HEAP_TUPLES_PER_PAGE / 8 * ITEM_ID); // 8016

    let save_free = BLCKSZ * (100 - t.fillfactor as usize) / 100;
    // (pd_lower, pd_upper) per page.
    let mut pages: Vec<(usize, usize)> = Vec::new();
    let mut target: Option<usize> = None;
    for &tlen in lens {
        let len = maxalign(tlen);
        let target_free = if len + save_free > NEARLY_EMPTY {
            len.max(NEARLY_EMPTY)
        } else {
            len + save_free
        };
        let mut placed = false;
        // Cached target block first, then lowest block with room.
        let order = target
            .into_iter()
            .chain((0..pages.len()).filter(|&i| Some(i) != target));
        for i in order {
            let (lo, hi) = pages[i];
            if target_free <= hi - lo - ITEM_ID {
                pages[i] = (lo + ITEM_ID, hi - len);
                target = Some(i);
                placed = true;
                break;
            }
        }
        if !placed {
            pages.push((PAGE_HEADER + ITEM_ID, BLCKSZ - len));
            target = Some(pages.len() - 1);
        }
    }
    pages.len() as u64 * BLCKSZ as u64
}

/// v1.41: `pg_relation_size(regclass)` / `pg_relation_size(regclass,
/// text)` — PG19's heap page accounting: the number of 8 KiB pages
/// PG's `RelationGetBufferForTuple` would use for the table's visible
/// row versions (fillfactor-aware, with the "nearly empty page" rule
/// for large tuples), times 8192. Only the `main` fork is tracked.
///
/// Accepts a relation name, a regclass display value (text), or an OID
/// integer — like PG's `regclass` input, an all-digit string is read as
/// an OID. Unknown relations are 42P01; a non-`main` fork is 0A000.
pub(crate) fn eval_pg_relation_size(q: &mut Q, vals: &[Value]) -> Result<Value, ExecError> {
    let fork: Option<String> = match vals.get(1) {
        None => None,
        Some(Value::Text(s)) => Some(s.to_string()),
        Some(Value::Null) => return Ok(Value::Null),
        Some(other) => {
            return Err(exec_err(
                "42883",
                format!(
                    "function pg_relation_size({}) does not exist",
                    other.type_name()
                ),
            ));
        }
    };
    if let Some(f) = &fork {
        if f != "main" {
            return Err(exec_err(
                "0A000",
                format!("fork \"{}\" is not supported", f),
            ));
        }
    }
    // Resolve the relation: OID integer, all-digit text (an OID), or a
    // relation name — mirroring regclass input.
    let oid: Option<u32> = match &vals[0] {
        Value::Null => return Ok(Value::Null),
        Value::Int(i) => Some(*i as u32),
        Value::BigInt(i) => Some(*i as u32),
        Value::SmallInt(i) => Some(*i as u32),
        Value::Text(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            Some(s.parse::<u32>().unwrap_or(0))
        }
        Value::Text(_) => None,
        other => {
            return Err(exec_err(
                "42883",
                format!(
                    "function pg_relation_size({}) does not exist",
                    other.type_name()
                ),
            ));
        }
    };
    let t = match oid {
        Some(oid) => {
            let mut found = None;
            // Session temp tables shadow permanent ones, like find_table
            // (temp tables are session-local; find_table skips the
            // visibility check for them too).
            if let Some(tmps) = q.eng.db.temp_tables.get(&q.session) {
                found = tmps.values().find(|t| t.oid == oid);
            }
            if found.is_none() {
                found = q.eng.db.tables.values().find_map(|vs| {
                    vs.iter().find(|t| {
                        t.oid == oid && crate::storage::table_visible(t, q.snap, &q.all_xids)
                    })
                });
            }
            found.ok_or_else(|| {
                exec_err("42P01", format!("relation with OID {} does not exist", oid))
            })?
        }
        None => {
            let Value::Text(s) = &vals[0] else {
                return Err(exec_err("XX000", "internal error: relation name"));
            };
            q.eng
                .db
                .find_table(s, q.snap, &q.all_xids, q.session)
                .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", s)))?
        }
    };
    // v1.41: PG19 heap page accounting — per visible row version,
    // compute the PG heap tuple length and simulate
    // `RelationGetBufferForTuple` placement; the size is pages × 8192.
    let mut lens: Vec<usize> = Vec::new();
    for rv in &t.rows {
        if !crate::storage::row_visible(rv, q.snap, &q.all_xids) {
            continue;
        }
        lens.push(pg_heap_tuple_len(t, rv));
    }
    Ok(Value::BigInt(pg_heap_page_count(t, &lens) as i64))
}

/// v1.13: `pg_size_pretty(bigint) -> text` — PG19's human-readable size
/// formatting (src/backend/utils/adt/misc.c). Thresholds are 10 * 1024^n;
/// the displayed value is the integer division, like PG.
pub(crate) fn eval_pg_size_pretty(vals: &[Value]) -> Result<Value, ExecError> {
    let size: i64 = match &vals[0] {
        Value::Null => return Ok(Value::Null),
        Value::Int(i) => *i,
        Value::BigInt(i) => *i,
        Value::SmallInt(i) => *i as i64,
        other => {
            return Err(exec_err(
                "42883",
                format!(
                    "function pg_size_pretty({}) does not exist",
                    other.type_name()
                ),
            ));
        }
    };
    // PG19 misc.c pg_size_pretty: negative sizes print as bytes; the
    // limit starts at 10 KiB and shifts left 10 bits per unit.
    if size < 0 {
        return Ok(Value::text(format!("{} bytes", size)));
    }
    let mut limit: i64 = 10 * 1024;
    let mut mult: i64 = 1;
    let units = ["bytes", "kB", "MB", "GB", "TB"];
    for unit in units {
        if size < limit || unit == "TB" {
            if unit == "bytes" {
                return Ok(Value::text(format!("{} bytes", size)));
            }
            return Ok(Value::text(format!("{} {}", size / mult, unit)));
        }
        limit <<= 10;
        mult <<= 10;
    }
    Ok(Value::text(format!("{} TB", size / mult)))
}

/// Validate argument counts for scalar built-ins. A wrong count is
/// 42883 (undefined_function), like Postgres — never an index panic.
pub(crate) fn check_builtin_arity(name: &str, vals: &[Value]) -> Result<(), ExecError> {
    let n = vals.len();
    let ok = match name {
        "upper" | "lower" | "length" | "char_length" | "character_length" | "octet_length"
        | "abs" | "floor" | "ceil" | "ceiling" | "sqrt" => n == 1,
        "exp" | "ln" => n == 1,
        "log" => n == 1 || n == 2,
        "cbrt" | "factorial" | "numeric_inc" => n == 1,
        // v0.64: pg_lsn(numeric) -> pg_lsn display.
        "pg_lsn" => n == 1,
        // v0.73: row_to_json(record) takes exactly one argument.
        "row_to_json" => n == 1,
        "scale" | "trim_scale" => n == 1,
        "min_scale" => n == 1,
        "pi" | "random" => n == 0,
        "degrees" | "radians" => n == 1,
        // v0.21: float8 transcendental functions.
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "asinh"
        | "acosh" | "atanh" | "erf" | "erfc" | "gamma" | "lgamma" | "sind" | "cosd" | "tand"
        | "cotd" | "asind" | "acosd" | "atand" | "log10" | "float8send" | "float4send"
        | "float4recv" | "float8recv" => n == 1,
        // v0.56: trunc(x) and trunc(x, s) (PG's trunc(numeric, int)).
        "trunc" => n == 1 || n == 2,
        "atan2" | "atan2d" => n == 2,
        "setseed" => n == 1,
        "gcd" | "lcm" => n == 2,
        "div" => n == 2,
        "width_bucket" => n == 3 || n == 4,
        "round" => n == 1 || n == 2,
        "substring" | "substr" => n == 2 || n == 3,
        "power" | "mod" | "position" | "date_trunc" | "nullif" => n == 2,
        "replace" | "split_part" => n == 3,
        "encode" | "decode" => n == 2,
        "crc32" | "crc32c" => n == 1,
        "sha224" | "sha256" | "sha384" | "sha512" => n == 1,
        "strpos" => n == 2,
        "translate" => n == 3,
        "unistr" => n == 1,
        "overlay" => n == 3 || n == 4,
        "regexp_like" => (2..=3).contains(&n),
        "regexp_count" => (2..=4).contains(&n),
        "regexp_instr" => (2..=7).contains(&n),
        "regexp_substr" => (2..=6).contains(&n),
        "regexp_replace" => (3..=6).contains(&n),
        "regexp_split_to_array" => (2..=3).contains(&n),
        // v0.95: `parse_ident(qualname text, strict boolean DEFAULT true)`.
        "parse_ident" => (1..=2).contains(&n),
        // v0.32: regexp set-returning functions (PG 19).
        "regexp_matches" => (2..=3).contains(&n),
        "regexp_split_to_table" => (2..=3).contains(&n),
        // v0.46: generate_series(int,int[,int]) / (bigint,bigint[,bigint])
        // / (numeric,numeric[,numeric]) — PG19 int.c, int8.c, numeric.c.
        "generate_series" => (2..=3).contains(&n),
        // v0.79: array functions (PG19 arrayfuncs.c) and unnest.
        "array_length" | "array_lower" | "array_upper" => n == 2,
        "cardinality" | "array_dims" | "array_ndims" => n == 1,
        "unnest" => n == 1,
        // v0.29: pg_input_is_valid(input, type).
        "pg_input_is_valid" => n == 2,
        // v0.43: pg_input_error_info(input, type) -> record (table
        // function in FROM; PG19 misc.c).
        "pg_input_error_info" => n == 2,
        // v0.37: TOAST introspection functions.
        "pg_column_compression" => n == 1,
        "pg_relation_size" => n == 1 || n == 2,
        // v1.13: pg_size_pretty(bigint) -> text (PG19 misc.c).
        "pg_size_pretty" => n == 1,
        "similar_to" => (2..=3).contains(&n),
        "substring_similar" => (2..=3).contains(&n),
        "substring_from" => n == 2,
        "substring_from_for" => n == 3,
        // v0.28: bytea bit/byte functions.
        "get_bit" => n == 2,
        "set_bit" => n == 3,
        "get_byte" => n == 2,
        "set_byte" => n == 3,
        "bit_count" => n == 1,
        // v0.16: new string/math built-ins.
        "concat" => true, // concat() with no args is '' (Postgres).
        "concat_ws" => n >= 1,
        // v0.45: format(fmt, ...) is variadic (at least the format string).
        "format" => n >= 1,
        "to_hex" | "to_oct" | "to_bin" | "sign" | "reverse" => n == 1,
        // v0.24: missing string built-ins (pg_regress 42883 cluster).
        "repeat" => n == 2,
        // v1.15: quote_ident/quote_literal/quote_nullable (PG19 quote.c).
        "quote_ident" | "quote_literal" | "quote_nullable" => n == 1,
        "lpad" | "rpad" => n == 2 || n == 3,
        "ascii" | "chr" | "initcap" => n == 1,
        "ltrim" | "rtrim" => n == 1 || n == 2,
        // v0.33: btrim(text [, text]); btrim(bytea, bytea).
        "btrim" => n == 1 || n == 2,
        "left" | "right" => n == 2,
        // v0.76: PG19 allows the optional precision (0-6) only on
        // CURRENT_TIMESTAMP (special grammar syntax); now()/
        // clock_timestamp()/statement_timestamp()/
        // transaction_timestamp() are 0-argument pg_proc functions.
        "current_timestamp" => n <= 1,
        "now" | "clock_timestamp" | "statement_timestamp" | "transaction_timestamp" => n == 0,
        "current_date" => n == 0,
        // v0.76: pg_typeof(any) takes exactly one argument; json_array is
        // variadic (SQL/JSON constructor, like PG19's json_array).
        "pg_typeof" => n == 1,
        "json_array" => true,
        // v0.17: version() takes no arguments.
        "version" => n == 0,
        // v0.17: date/time built-in batch.
        "date_part" | "to_char" | "timezone" | "to_number" => n == 2,
        "to_date" => n == 2,
        "to_timestamp" => n == 1 || n == 2, // (float8) or (text, text)
        "make_date" => n == 3,
        "make_timestamp" => n == 6,
        "coalesce" | "greatest" | "least" => n >= 1,
        // v0.14: PostgreSQL internal operator-function aliases (pg_regress).
        "booleq" | "boolne" | "int4eq" | "texteq" => n == 2,
        // v0.9: sequence functions.
        "nextval" | "currval" => n == 1,
        "lastval" => n == 0,
        "setval" => n == 2 || n == 3,
        // EXTRACT and friends validate their own shapes; unknown names
        // fall through to the dispatch below which raises 42883.
        _ => true,
    };
    if ok {
        return Ok(());
    }
    let sig = vals
        .iter()
        .map(|v| v.type_name())
        .collect::<Vec<_>>()
        .join(", ");
    Err(exec_err(
        "42883",
        format!("function {}({}) does not exist", name, sig),
    ))
}

/// Dispatch on pre-evaluated argument values (used by the grouped path).
/// v0.95: port of PG 19 `parse_ident` (misc.c) — split `qualname`
/// into its component identifiers. Quoted components keep their exact
/// contents (doubled quotes collapse); unquoted components are
/// lowercased (never truncated). In non-strict mode, parsing stops at
/// the first syntax error and returns what was collected; in strict
/// mode any syntax error is 22P02 ("invalid name syntax").
pub(crate) fn parse_ident_parts(qualname: &str, strict: bool) -> Result<Vec<String>, ExecError> {
    fn is_ident_start(b: u8) -> bool {
        b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
    }
    fn is_ident_cont(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
    }
    let bytes = qualname.as_bytes();
    let n = bytes.len();
    let mut p = 0usize;
    let mut out: Vec<String> = Vec::new();
    // Skip leading whitespace.
    while p < n && bytes[p].is_ascii_whitespace() {
        p += 1;
    }
    // PG treats a syntax error in non-strict mode as "stop and return
    // what was parsed"; model it with a local macro-free closure style.
    let mut syntax_error = false;
    let mut done = false;
    while !done && !syntax_error {
        if p < n && bytes[p] == b'"' {
            // Quoted identifier: copy verbatim, collapsing "" -> ".
            p += 1;
            let mut buf: Vec<u8> = Vec::new();
            loop {
                if p >= n {
                    syntax_error = true;
                    break;
                }
                if bytes[p] == b'"' {
                    if p + 1 < n && bytes[p + 1] == b'"' {
                        buf.push(b'"');
                        p += 2;
                    } else {
                        p += 1; // skip closing quote
                        break;
                    }
                } else {
                    buf.push(bytes[p]);
                    p += 1;
                }
            }
            if !syntax_error {
                out.push(String::from_utf8_lossy(&buf).into_owned());
            }
        } else {
            // Unquoted identifier: must start with letter/underscore.
            if p >= n || !is_ident_start(bytes[p]) {
                syntax_error = true;
            } else {
                let start = p;
                while p < n && is_ident_cont(bytes[p]) {
                    p += 1;
                }
                // Downcase (ASCII only, like PG's downcase_identifier for
                // the common case); never truncate.
                let mut s = qualname[start..p].to_string();
                s.make_ascii_lowercase();
                out.push(s);
            }
        }
        if syntax_error {
            break;
        }
        // Skip trailing whitespace.
        while p < n && bytes[p].is_ascii_whitespace() {
            p += 1;
        }
        if p >= n {
            done = true;
        } else if bytes[p] == b'.' {
            p += 1;
            while p < n && bytes[p].is_ascii_whitespace() {
                p += 1;
            }
        } else {
            syntax_error = true;
        }
    }
    if syntax_error && strict {
        return Err(exec_err("22P02", "invalid name syntax".to_string()));
    }
    Ok(out)
}

pub(crate) fn eval_func_vals(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    check_builtin_arity(name, vals)?;
    // v0.16: `substr` is a true alias of `substring` (same semantics).
    let name = if name == "substr" { "substring" } else { name };
    // v0.36: PG's "char"->text cast is IMPLICIT, so a "char" argument
    // coerces to text for any function (e.g. length('\377'::"char") = 4).
    // Coerce up front; non-text functions will still reject the text
    // value with their usual type error, like PG.
    let coerced: Vec<Value>;
    let vals: &[Value] = if vals.iter().any(|v| matches!(v, Value::SingleChar(_))) {
        coerced = vals
            .iter()
            .map(|v| match v {
                Value::SingleChar(b) => Value::text(crate::storage::char_out(*b)),
                v => v.clone(),
            })
            .collect();
        &coerced
    } else {
        vals
    };
    match name {
        // v0.79: array functions (PG19 arrayfuncs.c). A text value
        // parses as a `{...}` literal (unknown-literal coercion); an
        // untyped NULL is NULL.
        "array_length" | "array_lower" | "array_upper" | "cardinality" | "array_dims"
        | "array_ndims" => return eval_array_func(name, vals),
        // v0.79: nested scalar unnest — the SRF path (eval_srf_vals)
        // only applies at the top level of the SELECT list. Nested
        // (e.g. inside pg_typeof(...)), unnest yields the array's
        // first element; NULL or an empty array yields NULL.
        "unnest" => {
            return Ok(unnest_rows(&vals[0])?
                .into_iter()
                .next()
                .unwrap_or(Value::Null));
        }
        "upper"
        | "lower"
        | "length"
        | "char_length"
        | "character_length"
        | "octet_length"
        | "substring"
        | "position"
        | "replace"
        | "split_part"
        | "concat"
        | "concat_ws"
        | "to_hex"
        | "to_oct"
        | "to_bin"
        | "left"
        | "right"
        | "reverse"
        | "encode"
        | "decode"
        | "crc32"
        | "crc32c"
        | "sha224"
        | "sha256"
        | "sha384"
        | "sha512"
        | "strpos"
        | "translate"
        | "unistr"
        | "overlay"
        | "repeat"
        | "lpad"
        | "rpad"
        | "ascii"
        | "chr"
        | "initcap"
        | "ltrim"
        | "rtrim"
        // v0.33: btrim(bytea, bytea).
        | "btrim"
        | "regexp_like"
        | "regexp_count"
        | "regexp_instr"
        | "regexp_substr"
        | "regexp_replace"
        | "regexp_split_to_array"
        // v0.32: regexp_matches (scalar context = first match row).
        | "regexp_matches"
        | "similar_to"
        | "substring_similar"
        | "substring_from"
        | "substring_from_for"
        | "get_bit"
        | "set_bit"
        | "get_byte"
        | "set_byte"
        | "bit_count"
        | "pg_input_is_valid"
        // v0.95: parse_ident returns text[].
        // v1.15: quote_ident/quote_literal/quote_nullable (PG19 quote.c).
        | "parse_ident" | "quote_ident" | "quote_literal" | "quote_nullable" => {
            eval_str_func(name, vals)
        }
        "abs" | "round" | "floor" | "ceil" | "ceiling" | "sqrt" | "power" | "mod" | "sign" => {
            eval_math_func(name, vals)
        }
        // v0.18: numeric functions.
        "exp" | "ln" | "log" | "cbrt" | "factorial" | "gcd" | "lcm" | "pi" | "degrees"
        | "radians" | "scale" | "min_scale" | "trim_scale" | "div" | "width_bucket"
        | "numeric_inc" | "pg_lsn" => {
            eval_math_func(name, vals)
        }
        // v0.21: float8 transcendental functions.
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "atan2" | "sinh" | "cosh" | "tanh"
        | "asinh" | "acosh" | "atanh" | "erf" | "erfc" | "gamma" | "lgamma" | "sind" | "cosd"
        | "tand" | "cotd" | "asind" | "acosd" | "atand" | "atan2d" | "trunc" | "log10"
        | "float8send" | "float4send" | "float4recv" | "float8recv" => eval_math_func(name, vals),
        // v0.26: to_number(text, text) -> numeric. Parses text with a
        // numeric format picture (PG's numeric_to_number).
        "to_number" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let fmt = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(f) => f,
            };
            let desc = match crate::numfmt::parse_numfmt(fmt) {
                Ok(d) => d,
                Err(e) => return Err(numfmt_exec_err(e)),
            };
            match crate::numfmt_fromchar::to_number(s, &desc) {
                Ok(n) => Ok(Value::Numeric(n)),
                Err(e) => Err(numfmt_exec_err(e)),
            }
        }
        "random" | "setseed" => eval_math_func(name, vals),
        "now"
        | "current_date"
        | "current_timestamp"
        | "date_trunc"
        | "date_part"
        | "to_date"
        | "to_timestamp"
        | "to_char"
        | "make_date"
        | "make_timestamp"
        | "timezone"
        | "clock_timestamp"
        | "statement_timestamp"
        | "transaction_timestamp" => eval_datetime_func(name, vals),
        // v0.17: version() reports our own version, not a PG version we
        // claim to be (see SERVER_VERSION in server.rs).
        "version" => Ok(Value::text(format!(
            "rustgres {} (PostgreSQL-compatible, protocol 3.0)",
            crate::server::SERVER_VERSION
        ))),
        "coalesce" | "nullif" | "greatest" | "least" => eval_cond_func(name, vals),
        // v0.45: format(fmt, ...) per PG19 text_format().
        "format" => pg_format_text(vals),
        // v0.14: PostgreSQL internal operator-function aliases (pg_regress
        // conformance): booleq(x,y) ≡ x = y, boolne(x,y) ≡ x <> y, etc.
        "booleq" | "int4eq" | "texteq" => eval_cmp_vals(CmpOp::Eq, &vals[0], &vals[1]),
        "boolne" => eval_cmp_vals(CmpOp::Ne, &vals[0], &vals[1]),
        // v0.73: row_to_json(record) -> json (json.c). The JSON document
        // is carried as text typed `ColType::Json`.
        "row_to_json" => match &vals[0] {
            Value::Null => Ok(Value::Null),
            Value::Record(fields) => Ok(Value::text(row_to_json_text(fields))),
            other => Err(func_arg_err(name, other)),
        },
        // v0.76: pg_typeof(any) -> regtype (reported as text here). Uses
        // the value's PG type name; NULL reports "unknown" like PG19.
        "pg_typeof" => Ok(Value::text(vals[0].type_name())),
        // v0.76: json_array(...) -> json (SQL/JSON constructor, PG19).
        // Elements render per json.c: numbers bare, strings quoted,
        // NULL as null. Carried as text typed `ColType::Json`.
        "json_array" => Ok(Value::text(json_array_text(vals))),
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// v0.24: output-size guards for string-building built-ins. Postgres
/// caps a single text value at ~1GB (MaxAllocSize -> 54000); the char
/// bound is the corresponding worst case for 4-byte UTF-8.
pub(crate) const MAX_STR_RESULT_BYTES: u64 = 1_000_000_000;
pub(crate) const MAX_STR_RESULT_CHARS: u64 = 250_000_000;

/// v0.24: parse PostgreSQL regexp flags (`i`, `n`/`m`, `s`, `x`, `g`).
/// Returns `(options, global)`. Anything else is 2201B, like PG's
/// `invalid regular expression option`. The `g` flag is only meaningful
/// for `regexp_replace`; the other regexp_* functions reject it with
/// PG's exact `does not support the "global" option` error.
pub(crate) fn parse_regexp_flags(
    flags: &str,
    func: &str,
    allow_global: bool,
) -> Result<(crate::regex::RegexOptions, bool), ExecError> {
    let mut opts = crate::regex::RegexOptions::default();
    let mut global = false;
    for c in flags.chars() {
        match c {
            'i' => opts.case_insensitive = true,
            'n' | 'm' => opts.newline_sensitive = true,
            's' => opts.newline_sensitive = false,
            'x' => opts.expanded = true,
            'g' => {
                if !allow_global {
                    // v0.32: PG uses 22023 for this rejection.
                    return Err(exec_err(
                        "22023",
                        format!("{func}() does not support the \"global\" option"),
                    ));
                }
                global = true;
            }
            _ => {
                return Err(exec_err(
                    "2201B",
                    format!("invalid regular expression option: \"{c}\""),
                ));
            }
        }
    }
    Ok((opts, global))
}

/// v0.24: integer `substring(s, start [, len])` — 1-based; start < 1
/// shifts the window (Postgres rule).
pub(crate) fn substring_int(s: &str, start: i64, len: Option<i64>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len() as i64;
    let from = start.max(1);
    let upto = match len {
        Some(n) => start + n,
        None => total + 1,
    };
    let lo = (from - 1).max(0).min(total) as usize;
    let hi = (upto - 1).max(0).min(total) as usize;
    let (lo, hi) = (lo.min(hi), hi);
    chars[lo..hi].iter().collect::<String>()
}

/// v0.33: `substring(bytea from S [for L])` — PG 19 `bytea_substring`
/// (src/backend/utils/adt/bytea.c). Byte-based (not char-based), 1-based.
/// S1 = max(S, 1); no length means to end; L < 0 is error 22011; S+L
/// overflowing int32 means to end; E = S+L < 1 means empty.
pub(crate) fn bytea_substring(
    data: &[u8],
    start: i64,
    len: Option<i64>,
) -> Result<Vec<u8>, ExecError> {
    // PG takes int4; clamp i64 inputs to int32 range first.
    let s = start.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let s1 = s.max(1);
    let l1: i64 = match len {
        None => -1, // to end
        Some(l) => {
            if l < 0 {
                return Err(exec_err("22011", "negative substring length not allowed"));
            }
            if l > i32::MAX as i64 {
                -1 // S + L would overflow int32 -> to end
            } else {
                let l = l as i32;
                match s.checked_add(l) {
                    None => -1, // overflow -> to end
                    Some(e) => {
                        if e < 1 {
                            return Ok(Vec::new());
                        }
                        (e - s1) as i64
                    }
                }
            }
        }
    };
    let start_idx = (s1 - 1) as usize;
    if start_idx >= data.len() {
        return Ok(Vec::new());
    }
    let end_idx = if l1 < 0 {
        data.len()
    } else {
        start_idx.saturating_add(l1 as usize).min(data.len())
    };
    Ok(data[start_idx..end_idx].to_vec())
}

/// v0.33: `overlay(bytea placing bytea from SP [for SL])` — PG 19
/// `bytea_overlay` (src/backend/utils/adt/bytea.c). SP <= 0 is error
/// 22011 (unlike text overlay which clamps); SP+SL overflowing int32
/// is error 22003; omitted SL defaults to len(replacement).
pub(crate) fn bytea_overlay(t1: &[u8], t2: &[u8], sp: i64, sl: i64) -> Result<Vec<u8>, ExecError> {
    if sp <= 0 {
        return Err(exec_err("22011", "negative substring length not allowed"));
    }
    if sl < 0 {
        return Err(exec_err("22011", "negative substring length not allowed"));
    }
    // PG takes int4; out-of-range inputs are "integer out of range".
    if sp > i32::MAX as i64 || sl > i32::MAX as i64 {
        return Err(exec_err("22003", "integer out of range"));
    }
    let sp = sp as i32;
    let sl = sl as i32;
    let sp_pl_sl = sp
        .checked_add(sl)
        .ok_or_else(|| exec_err("22003", "integer out of range"))?;
    let mut out = bytea_substring(t1, 1, Some((sp - 1) as i64))?;
    out.extend_from_slice(t2);
    out.extend_from_slice(&bytea_substring(t1, sp_pl_sl as i64, None)?);
    Ok(out)
}

/// v0.33: bytea `btrim`/`ltrim`/`rtrim` — PG 19 `dobyteatrim`
/// (src/backend/utils/adt/oracle_compat.c). If the string or the set is
/// empty, the string is returned unchanged; otherwise bytes in the set
/// are stripped from the left and/or right.
pub(crate) fn bytea_trim(data: &[u8], set: &[u8], doltrim: bool, dortrim: bool) -> Vec<u8> {
    if data.is_empty() || set.is_empty() {
        return data.to_vec();
    }
    let mut start = 0;
    let mut end = data.len();
    if doltrim {
        while start < end && set.contains(&data[start]) {
            start += 1;
        }
    }
    if dortrim {
        while end > start && set.contains(&data[end - 1]) {
            end -= 1;
        }
    }
    data[start..end].to_vec()
}

/// v0.31: validate a SIMILAR TO ESCAPE string per PostgreSQL 19
/// (`similar_escape_internal`): empty means "no escape character", a single
/// character is the escape, anything longer is error 22025.
pub(crate) fn similar_escape_opt(esc_s: &str) -> Result<Option<char>, ExecError> {
    let mut ch = esc_s.chars();
    match (ch.next(), ch.next()) {
        (None, None) => Ok(None),
        (Some(c), None) => Ok(Some(c)),
        _ => Err(exec_err(
            "22025",
            "invalid escape string: Escape string must be empty or one character.",
        )),
    }
}

/// v0.24: SIMILAR TO substring match — the pattern must match the
/// entire string; if it has a group, return the first group.
/// v0.31: uses the PG 19 `similar_escape_internal` translation, so the
/// escape-double-quoted part becomes capture group 1 (or, with no
/// escape-double-quotes, there are no groups and the whole match returns).
pub(crate) fn substring_similar_match(
    s: &str,
    pat: &str,
    escape: Option<char>,
) -> Result<Value, ExecError> {
    let regex_pat = similar_to_regex(pat, escape)
        .map_err(|e| exec_err("2201B", format!("invalid SIMILAR TO pattern: {}", e)))?;
    let re = crate::regex::compile(&regex_pat, false)
        .map_err(|e| exec_err("2201B", format!("invalid SIMILAR TO pattern: {}", e)))?;
    let sc: Vec<char> = s.chars().collect();
    match re.find_at(&sc, 0) {
        Some((ms, me, caps)) if ms == 0 && me == sc.len() => {
            if re.group_count() >= 1 {
                match caps.groups.get(1).copied().flatten() {
                    Some((gs, ge)) => Ok(Value::text(sc[gs..ge].iter().collect::<String>())),
                    None => Ok(Value::Null),
                }
            } else {
                Ok(Value::text(sc[ms..me].iter().collect::<String>()))
            }
        }
        _ => Ok(Value::Null),
    }
}

/// v0.24: validate a regexp `start` parameter — must be >= 1, like PG's
/// `invalid value for parameter "start": N`.
pub(crate) fn check_regexp_start(start: i64) -> Result<usize, ExecError> {
    if start < 1 {
        return Err(exec_err(
            "22023",
            format!("invalid value for parameter \"start\": {start}"),
        ));
    }
    Ok((start as u64).min(usize::MAX as u64) as usize - 1)
}

/// v0.24: validate a regexp occurrence/`n` parameter — must be >= 1.
pub(crate) fn check_regexp_n(n: i64, what: &str) -> Result<(), ExecError> {
    if n < 1 {
        return Err(exec_err(
            "22023",
            format!("invalid value for parameter \"{what}\": {n}"),
        ));
    }
    Ok(())
}

/// v0.32: shared PG-19 match loop, port of `setup_regexp_matches` in
/// `src/backend/utils/adt/regexp.c`.
///
/// Finds successive non-overlapping matches starting at or after each
/// previous match end. After a zero-length match the next search starts one
/// character later (PG's `start_search = end_search; if (start_search ==
/// end_search) start_search++`).
///
/// When `ignore_degenerate` is set (the split functions), zero-length
/// matches at the end of the string or immediately after the previous match
/// end are ignored entirely (PG: `if (so == eo && !(so < n && eo >
/// prev_match_end)) continue;`).
pub(crate) fn regexp_find_all(
    re: &crate::regex::Compiled,
    sc: &[char],
    global: bool,
    ignore_degenerate: bool,
) -> Vec<(usize, usize, crate::regex::Captures)> {
    let mut out = Vec::new();
    let mut prev_match_end = 0usize;
    let mut ss = 0usize;
    loop {
        let (ms, me, caps) = match re.find_at(sc, ss) {
            None => break,
            Some(m) => m,
        };
        let degenerate = ms == me && !(ms < sc.len() && me > prev_match_end);
        if !(ignore_degenerate && degenerate) {
            out.push((ms, me, caps));
        }
        prev_match_end = me;
        if !global {
            break;
        }
        ss = me;
        if ms == me {
            ss += 1;
        }
        if ss > sc.len() {
            break;
        }
    }
    out
}

/// v0.32: PG `array_out` quoting for text elements — quote when empty,
/// when containing whitespace or any of `,"\{}` (which would corrupt
/// parsing), or when equal to NULL in any case. Escapes `"` and `\`.
pub(crate) fn pg_quote_array_elem(p: &str) -> String {
    let needs = p.is_empty()
        || p.chars().any(|c| {
            c == ',' || c == '"' || c == '\\' || c == '{' || c == '}' || c.is_whitespace()
        })
        || p.eq_ignore_ascii_case("null");
    if !needs {
        return p.to_string();
    }
    let mut out = String::with_capacity(p.len() + 2);
    out.push('"');
    for c in p.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// v0.32: build a PG text[] literal; `None` renders as bare NULL.
pub(crate) fn pg_text_array_literal(elems: &[Option<String>]) -> String {
    let mut out = String::from("{");
    for (i, e) in elems.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match e {
            None => out.push_str("NULL"),
            Some(s) => out.push_str(&pg_quote_array_elem(s)),
        }
    }
    out.push('}');
    out
}

/// v0.32: port of PG 19 `build_regexp_split_result` in `regexp.c`.
/// Splits `sc` on the recorded delimiter matches; the pieces are the
/// unmatched segments before, between, and after the delimiters.
pub(crate) fn regexp_split_result(
    sc: &[char],
    matches: &[(usize, usize, crate::regex::Captures)],
) -> Vec<String> {
    let mut pieces = Vec::with_capacity(matches.len() + 1);
    let mut pos = 0usize;
    for (ms, me, _) in matches {
        pieces.push(sc[pos..*ms].iter().collect::<String>());
        pos = *me;
    }
    pieces.push(sc[pos..].iter().collect::<String>());
    pieces
}

/// v0.32: port of PG 19 `build_regexp_match_result` in `regexp.c`.
/// One `text[]` literal per match; element `i` is group `i`'s text, NULL
/// when the group did not participate. With no capture groups the literal
/// holds the whole match.
pub(crate) fn regexp_match_result(
    sc: &[char],
    re: &crate::regex::Compiled,
    ms: usize,
    me: usize,
    caps: &crate::regex::Captures,
) -> String {
    let ngroups = re.group_count();
    let elems: Vec<Option<String>> = if ngroups == 0 {
        vec![Some(sc[ms..me].iter().collect::<String>())]
    } else {
        (1..=ngroups)
            .map(|g| {
                caps.groups
                    .get(g)
                    .and_then(|slot| slot.as_ref())
                    .map(|&(gs, ge)| sc[gs..ge].iter().collect::<String>())
            })
            .collect()
    };
    pg_text_array_literal(&elems)
}

/// v0.32: `regexp_matches(string, pattern [, flags])` as a set-returning
/// function — one row per match (PG 19 `regexp_matches`). Flags are parsed
/// with 'g' allowed (global = all matches, else just the first).
/// NULL input yields zero rows.
pub(crate) fn regexp_matches_rows(vals: &[Value]) -> Result<Vec<Value>, ExecError> {
    let name = "regexp_matches";
    let s = match str_arg(name, &vals[0])? {
        None => return Ok(Vec::new()),
        Some(s) => s.to_string(),
    };
    let pat = match str_arg(name, &vals[1])? {
        None => return Ok(Vec::new()),
        Some(p) => p.to_string(),
    };
    let flags = if vals.len() > 2 {
        match str_arg(name, &vals[2])? {
            None => return Ok(Vec::new()),
            Some(f) => f.to_string(),
        }
    } else {
        String::new()
    };
    let (opts, global) = parse_regexp_flags(&flags, name, true)?;
    let re = crate::regex::compile_opts(&pat, opts)
        .map_err(|e| exec_err("2201B", format!("invalid regular expression: {e}")))?;
    let sc: Vec<char> = s.chars().collect();
    Ok(regexp_find_all(&re, &sc, global, false)
        .into_iter()
        .map(|(ms, me, caps)| Value::text(regexp_match_result(&sc, &re, ms, me, &caps)))
        .collect())
}

/// v0.32: table functions usable in FROM — currently
/// v0.46: classified `generate_series` argument (PG19 int.c, int8.c,
/// numeric.c). Only the numeric-capable signatures are supported.
pub(crate) enum GsArg {
    /// SmallInt/Int kinds (Value::Int always holds i32-range values).
    Int(i64),
    Big(i64),
    Num(Numeric),
}

impl GsArg {
    pub(crate) fn as_i64(&self) -> i64 {
        match self {
            GsArg::Int(i) | GsArg::Big(i) => *i,
            GsArg::Num(_) => unreachable!("int path takes no numeric args"),
        }
    }

    pub(crate) fn as_numeric(&self) -> Numeric {
        match self {
            GsArg::Int(i) | GsArg::Big(i) => Numeric::new(*i as i128, 0),
            GsArg::Num(n) => n.clone(),
        }
    }
}

/// v0.46: integer `generate_series` core, reproducing PG19's
/// `generate_series_step_int4` / `generate_series_step_int8` exactly:
/// step 0 is 22023, and when the successor of an emitted value would
/// overflow the C type, the emitted value is still the final row
/// (`pg_add_s32/s64_overflow` semantics). The i128 accumulator makes
/// the overflow test exact at both widths.
pub(crate) fn generate_series_int(
    args: &[GsArg],
    is_big: bool,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let start = args[0].as_i64() as i128;
    let finish = args[1].as_i64() as i128;
    let step = if args.len() > 2 {
        args[2].as_i64() as i128
    } else {
        1
    };
    if step == 0 {
        return Err(exec_err("22023", "step size cannot equal zero"));
    }
    let hi: i128 = if is_big {
        i64::MAX as i128
    } else {
        i32::MAX as i128
    };
    let lo: i128 = if is_big {
        i64::MIN as i128
    } else {
        i32::MIN as i128
    };
    let mut rows = Vec::new();
    let mut cur = start;
    if step > 0 {
        while cur <= finish {
            rows.push(vec![if is_big {
                Value::BigInt(cur as i64)
            } else {
                // v0.46: cur is within [start, finish], both i32-range.
                Value::Int(cur as i64)
            }]);
            // PG emits `cur`, then stops if `cur + step` overflows.
            if cur + step > hi {
                break;
            }
            cur += step;
        }
    } else {
        while cur >= finish {
            rows.push(vec![if is_big {
                Value::BigInt(cur as i64)
            } else {
                Value::Int(cur as i64)
            }]);
            if cur + step < lo {
                break;
            }
            cur += step;
        }
    }
    Ok(rows)
}

/// v0.46: numeric `generate_series`, per PG19 numeric.c
/// `generate_series_step_numeric`: NaN/infinity start/stop/step are
/// 22023 with PG's messages (checked start, stop, step, in that order,
/// before the zero-step check); otherwise emit while `cur` has not
/// passed `stop` in the step's direction. v0.64: like the int4/int8
/// series, emit `cur` then stop cleanly if `cur + step` overflows the
/// numeric format, instead of raising 22023 for the unneeded successor.
pub(crate) fn generate_series_numeric(args: &[GsArg]) -> Result<Vec<Vec<Value>>, ExecError> {
    let start = args[0].as_numeric();
    let stop = args[1].as_numeric();
    let step = if args.len() > 2 {
        args[2].as_numeric()
    } else {
        Numeric::new(1, 0)
    };
    for (label, v) in [("start", &start), ("stop", &stop)] {
        match v.special {
            NumericSpecial::NaN => {
                return Err(exec_err("22023", format!("{label} value cannot be NaN")));
            }
            NumericSpecial::PosInf | NumericSpecial::NegInf => {
                return Err(exec_err(
                    "22023",
                    format!("{label} value cannot be infinity"),
                ));
            }
            NumericSpecial::Finite => {}
        }
    }
    match step.special {
        NumericSpecial::NaN => {
            return Err(exec_err("22023", "step size cannot be NaN"));
        }
        NumericSpecial::PosInf | NumericSpecial::NegInf => {
            return Err(exec_err("22023", "step size cannot be infinity"));
        }
        NumericSpecial::Finite => {}
    }
    if step.is_zero() {
        return Err(exec_err("22023", "step size cannot equal zero"));
    }
    let step_pos = step.unscaled > 0;
    let mut rows = Vec::new();
    let mut cur = start;
    loop {
        let ord = cur.cmp(&stop);
        if step_pos && ord == Ordering::Greater {
            break;
        }
        if !step_pos && ord == Ordering::Less {
            break;
        }
        rows.push(vec![Value::Numeric(cur.clone())]);
        // v0.64: PG emits `cur`, then stops if `cur + step` overflows the
        // numeric format (like the int4/int8 series), instead of raising
        // 22023 for the unneeded successor.
        match cur.checked_add(&step) {
            Some(next) => cur = next,
            None => break,
        }
    }
    Ok(rows)
}

/// `regexp_split_to_table(string, pattern [, flags])` (PG 19). Returns one
/// value per split piece. NULL input yields zero rows; the 'g' flag is
/// rejected (the split is internally global, like PG).
/// v0.32: table functions. v0.43: returns the output column names plus one
/// row per output row (one `Value` per column), so multi-column functions
/// like `pg_input_error_info` work. `regexp_split_to_table` keeps its
/// single text column named for the function.
pub(crate) fn eval_table_function(
    name: &str,
    vals: &[Value],
) -> Result<(Vec<String>, Vec<Vec<Value>>), ExecError> {
    // v0.32: arity is validated like scalar builtins (42883, as PG's
    // "function ... does not exist" for a bad signature).
    check_builtin_arity(name, vals)?;
    match name {
        "regexp_split_to_table" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok((vec![name.to_string()], Vec::new())),
                Some(s) => s.to_string(),
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok((vec![name.to_string()], Vec::new())),
                Some(p) => p.to_string(),
            };
            let flags = if vals.len() > 2 {
                match str_arg(name, &vals[2])? {
                    None => return Ok((vec![name.to_string()], Vec::new())),
                    Some(f) => f.to_string(),
                }
            } else {
                String::new()
            };
            let (opts, _) = parse_regexp_flags(&flags, name, false)?;
            let re = crate::regex::compile_opts(&pat, opts)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {e}")))?;
            let sc: Vec<char> = s.chars().collect();
            let matches = regexp_find_all(&re, &sc, true, true);
            let rows = regexp_split_result(&sc, &matches)
                .into_iter()
                .map(|v| vec![Value::text(v)])
                .collect();
            Ok((vec![name.to_string()], rows))
        }
        // v0.43: pg_input_error_info(input text, type text), PG19 misc.c.
        // Returns the input function's soft error as PG's four OUT
        // columns (message, detail, hint, sql_error_code); a valid input
        // yields one row of NULLs. NULL arguments yield no rows, like the
        // other table functions. Hard errors (unknown type -> 42883,
        // malformed char typmod -> 22023) propagate.
        "pg_input_error_info" => {
            // PG19 pg_proc.dat OUT parameter names.
            let cols = vec![
                "message".to_string(),
                "detail".to_string(),
                "hint".to_string(),
                "sql_error_code".to_string(),
            ];
            let input = match str_arg(name, &vals[0])? {
                None => return Ok((cols, Vec::new())),
                Some(s) => s.to_string(),
            };
            let typ = match str_arg(name, &vals[1])? {
                None => return Ok((cols, Vec::new())),
                Some(s) => s.to_string(),
            };
            let row = match pg_input_validate("pg_input_error_info", &input, &typ) {
                Ok(()) => vec![Value::Null, Value::Null, Value::Null, Value::Null],
                Err(PgInputFailure::Soft(e)) => vec![
                    Value::text(e.message),
                    e.detail.map(Value::text).unwrap_or(Value::Null),
                    e.hint.map(Value::text).unwrap_or(Value::Null),
                    Value::text(e.code),
                ],
                Err(PgInputFailure::Hard(e)) => return Err(e),
            };
            Ok((cols, vec![row]))
        }
        // v0.46: generate_series(int,int[,int]) / (bigint,bigint[,bigint])
        // / (numeric,numeric[,numeric]) — PG19 int.c, int8.c, numeric.c
        // (`generate_series_step_int4/int8`, `generate_series_numeric`).
        // Strict: any NULL argument yields zero rows (proisstrict is the
        // initdb default 't', like abs()). The timestamp/interval forms
        // stay 42883 (unsupported), as do non-numeric argument types.
        "generate_series" => {
            let mut has_num = false;
            let mut has_big = false;
            let mut gs_args: Vec<GsArg> = Vec::with_capacity(vals.len());
            for v in vals {
                match v {
                    Value::Null => return Ok((vec![name.to_string()], Vec::new())),
                    Value::SmallInt(i) => gs_args.push(GsArg::Int(*i as i64)),
                    Value::Int(i) => gs_args.push(GsArg::Int(*i)),
                    Value::BigInt(i) => {
                        has_big = true;
                        gs_args.push(GsArg::Big(*i));
                    }
                    Value::Numeric(n) => {
                        has_num = true;
                        gs_args.push(GsArg::Num(n.clone()));
                    }
                    // v0.32 convention: strict, no implicit casts (like
                    // str_arg); PG's "function does not exist" for a bad
                    // signature is 42883.
                    other => return Err(func_arg_err(name, other)),
                }
            }
            // PG resolves mixed int/numeric to the numeric signature and
            // int4/int8 mixes to int8 (implicit casts); Value::Int always
            // holds i32-range values (literals outside it parse as
            // BigInt), so the int4 path needs no range check.
            let rows = if has_num {
                generate_series_numeric(&gs_args)?
            } else {
                generate_series_int(&gs_args, has_big)?
            };
            Ok((vec![name.to_string()], rows))
        }
        // v0.79: unnest(anyarray) — one row per element (PG19
        // arrayfuncs.c `unnest`). NULL yields zero rows; a text value
        // parses as a text[] literal (unknown-literal coercion).
        "unnest" => {
            let rows = unnest_rows(&vals[0])?
                .into_iter()
                .map(|v| vec![v])
                .collect();
            Ok((vec![name.to_string()], rows))
        }
        // v0.99: parse_ident as a scalar function in FROM — one row
        // holding the text[] value (PG19: a scalar function in FROM is
        // a one-row table).
        "parse_ident" => {
            let v = eval_str_func(name, vals)?;
            Ok((vec![name.to_string()], vec![vec![v]]))
        }
        _ => Err(exec_err(
            "42883",
            format!(
                "function {}({}) does not exist",
                name,
                vals.iter()
                    .map(|v| v.type_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// v0.32: set-returning functions that expand in the SELECT list (PG 19
/// SRF-in-targetlist semantics). v0.47: `generate_series` joins
/// `regexp_matches` — PG19's ProjectSet keeps SRFs at the top level of
/// the targetlist (planner.c `adjust_paths_for_srfs`), which is exactly
/// what this gate matches. Unknown names are not SRFs.
/// v1.09: also recognizes user-defined functions with RETURNS SETOF
/// (any overload), so `SELECT my_srf(...)` fans out like PG19.
pub(crate) fn is_srf(eng: &Engine, name: &str) -> bool {
    is_builtin_srf(name)
        || eng
            .db
            .functions
            .get(name)
            .map(|ovs| ovs.iter().any(|f| f.returns_set))
            .unwrap_or(false)
}

/// Builtin-only SRF check, for contexts without catalog access
/// (GROUP BY keys, INSERT ... VALUES) where user SRFs stay 0A000.
pub(crate) fn is_builtin_srf(name: &str) -> bool {
    matches!(name, "regexp_matches" | "generate_series" | "unnest")
}

/// v0.32: evaluate a set-returning function to its output rows (one Value
/// per row). Used by the SRF targetlist expansion.
/// v0.47: `generate_series` reuses the FROM-clause table-function core
/// (`eval_table_function`), flattened to one value per row — PG19's
/// `generate_series` returns a single column.
pub(crate) fn eval_srf_vals(name: &str, vals: &[Value]) -> Result<Vec<Value>, ExecError> {
    check_builtin_arity(name, vals)?;
    match name {
        "regexp_matches" => regexp_matches_rows(vals),
        // v0.79: unnest reuses the FROM-clause table-function core,
        // flattened to one value per row — PG19's unnest returns a
        // single column.
        "unnest" => {
            let (_, rows) = eval_table_function(name, vals)?;
            rows.into_iter()
                .map(|mut r| {
                    r.pop().ok_or_else(|| {
                        exec_err("XX000", "unnest returned a row with no columns".to_string())
                    })
                })
                .collect()
        }
        "generate_series" => {
            let (_, rows) = eval_table_function(name, vals)?;
            rows.into_iter()
                .map(|mut r| {
                    r.pop().ok_or_else(|| {
                        exec_err(
                            "XX000",
                            "generate_series returned a row with no columns".to_string(),
                        )
                    })
                })
                .collect()
        }
        _ => Err(exec_err(
            "42883",
            format!("function {name} is not a set-returning function"),
        )),
    }
}

/// v1.09: evaluate a user-defined RETURNS SETOF function to its output
/// values (one per row, first column), for SRF-in-targetlist expansion
/// (PG19's ProjectSet). The body runs via `run_func_body`; each row's
/// first column is coerced to the declared return type.
pub(crate) fn eval_user_srf_vals(
    q: &mut Q,
    scopes: &[Scope],
    name: &str,
    vals: &[Value],
) -> Result<Vec<Value>, ExecError> {
    let fdef = resolve_function_overload(q.eng, name, vals)
        .ok_or_else(|| exec_err("42883", format!("function {}() does not exist", name)))?;
    if !fdef.returns_set {
        return Err(exec_err(
            "0A000",
            format!(
                "set-returning function \"{}\" used in scalar context",
                fdef.name
            ),
        ));
    }
    if fdef.lang == crate::sql::FuncLang::Internal {
        return Err(exec_err(
            "0A000",
            format!("internal function \"{}\" is not set-returning", fdef.name),
        ));
    }
    let out = run_func_body(q, scopes, &fdef, vals, volatile_pending(q, &fdef))?;
    let mut result = Vec::with_capacity(out.rows.len());
    for row in out.rows {
        let v = row.iter().next().cloned().unwrap_or(Value::Null);
        result.push(coerce_to_type_name(q, &v, &fdef.ret_type)?);
    }
    Ok(result)
}

/// v0.72: is this VALUES cell a top-level set-returning function call?
/// (INSERT form — `InsertValue::Expr(Expr::Func)`; the SELECT
/// targetlist has its own SRF handling.)
pub(crate) fn insert_cell_is_srf(v: &InsertValue) -> bool {
    matches!(v, InsertValue::Expr(Expr::Func { name, .. }) if is_builtin_srf(name))
}

/// v0.72: expand top-level set-returning function calls in
/// `INSERT ... VALUES` rows (PG19 `ExecProjectSRF` semantics): each SRF
/// cell evaluates to its output set; the row fans out to one row per
/// SRF value, zipped positionally. An exhausted SRF NULL-pads
/// (`SELECT g(1,3), g(1,2)` -> (1,1),(2,2),(3,NULL)); scalar cells are
/// re-evaluated per output row (they repeat, never NULL-pad); if every
/// SRF in the row is empty, the row produces nothing (`VALUES
/// (generate_series(1,0))` inserts nothing). SRF outputs re-enter as
/// literals so the normal literal coercion/validation below applies
/// unchanged.
pub(crate) fn expand_insert_srf_rows(
    q: &mut Q,
    rows: &[Vec<InsertValue>],
) -> Result<Vec<Vec<InsertValue>>, ExecError> {
    if !rows.iter().flatten().any(insert_cell_is_srf) {
        return Ok(rows.to_vec());
    }
    let mut out: Vec<Vec<InsertValue>> = Vec::new();
    for row in rows {
        // Evaluate each cell to either one literal (scalar: repeats) or
        // the SRF's set (NULL-pads once exhausted).
        let mut cells: Vec<(Vec<InsertValue>, bool)> = Vec::with_capacity(row.len());
        let mut width = 0usize;
        let mut has_result = false;
        for v in row {
            if let InsertValue::Expr(Expr::Func { name, args, .. }) = v {
                if is_builtin_srf(name) {
                    let mut arg_vals = Vec::with_capacity(args.len());
                    for a in args {
                        arg_vals.push(eval_expr(q, &[], a)?);
                    }
                    let set = eval_srf_vals(name, &arg_vals)?;
                    has_result |= !set.is_empty();
                    width = width.max(set.len());
                    cells.push((
                        set.into_iter()
                            .map(|sv| InsertValue::Lit(value_to_literal(&Some(sv))))
                            .collect(),
                        true,
                    ));
                    continue;
                }
            }
            cells.push((vec![v.clone()], false));
        }
        // PG: "If all the SRFs returned ExprEndResult, we consider that
        // as no row being produced."
        if !has_result {
            continue;
        }
        for i in 0..width {
            out.push(
                cells
                    .iter()
                    .map(|(c, is_srf)| {
                        if *is_srf {
                            c.get(i).cloned().unwrap_or(InsertValue::Lit(Literal::Null))
                        } else {
                            c[0].clone()
                        }
                    })
                    .collect(),
            );
        }
    }
    Ok(out)
}

/// v0.72: expand top-level set-returning function calls in a
/// FROM-clause `VALUES` row (same PG19 `ExecProjectSRF` semantics as
/// [`expand_insert_srf_rows`], but the cells evaluate straight to
/// `Value`s for the scan): SRFs zip with NULL padding, scalars repeat,
/// all-empty SRFs produce no rows.
pub(crate) fn expand_values_srf_row(
    q: &mut Q,
    scopes: &[Scope],
    row: &[Expr],
) -> Result<Vec<Vec<Value>>, ExecError> {
    if !row
        .iter()
        .any(|e| matches!(e, Expr::Func { name, .. } if is_builtin_srf(name)))
    {
        return Ok(vec![
            row.iter()
                .map(|e| eval_expr(q, scopes, e))
                .collect::<Result<Vec<_>, _>>()?,
        ]);
    }
    let mut cells: Vec<(Vec<Value>, bool)> = Vec::with_capacity(row.len());
    let mut width = 0usize;
    let mut has_result = false;
    for e in row {
        if let Expr::Func { name, args, .. } = e {
            if is_builtin_srf(name) {
                let mut arg_vals = Vec::with_capacity(args.len());
                for a in args {
                    arg_vals.push(eval_expr(q, scopes, a)?);
                }
                let set = eval_srf_vals(name, &arg_vals)?;
                has_result |= !set.is_empty();
                width = width.max(set.len());
                cells.push((set, true));
                continue;
            }
        }
        cells.push((vec![eval_expr(q, scopes, e)?], false));
    }
    if !has_result {
        return Ok(Vec::new());
    }
    Ok((0..width)
        .map(|i| {
            cells
                .iter()
                .map(|(c, is_srf)| {
                    if *is_srf {
                        c.get(i).cloned().unwrap_or(Value::Null)
                    } else {
                        c[0].clone()
                    }
                })
                .collect()
        })
        .collect())
}

/// v1.15: PG19 keywords that force `quote_ident` to quote.
/// Generated from pg19-src/src/include/parser/kwlist.h:
/// every keyword whose category is not UNRESERVED_KEYWORD
/// (RESERVED_KEYWORD, COL_NAME_KEYWORD, TYPE_FUNC_NAME_KEYWORD).
/// `quote_identifier` (ruleutils.c) quotes identifiers that are
/// any of these words (case-insensitively, but the safe-shape
/// check already guarantees all-lowercase input). Sorted for
/// binary_search.
pub(crate) const QUOTE_IDENT_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// v1.15: PG19 `quote_identifier` (ruleutils.c).
/// Returns the identifier unchanged when it starts with `[a-z_]` and
/// contains only `[a-z0-9_]`, and is not a keyword (other than an
/// unreserved one); otherwise wraps it in double quotes, doubling any
/// embedded double quotes.
pub(crate) fn pg_quote_identifier_fn(ident: &str) -> String {
    let mut bytes = ident.bytes();
    // PG tests bytes with ASCII-only ctype checks; non-ASCII bytes are
    // never "safe" and fall through to quoting, same as here.
    let safe_shape = match bytes.next() {
        Some(b) if b.is_ascii_lowercase() || b == b'_' => true,
        _ => false,
    } && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    // PG's ScanKeywordLookup is case-insensitive, but the safe-shape
    // check already guarantees all-lowercase ASCII, so an exact match
    // against the (lowercase) keyword table is equivalent.
    let needs_quote = !safe_shape || QUOTE_IDENT_KEYWORDS.binary_search(&ident).is_ok();
    if !needs_quote {
        return ident.to_string();
    }
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for ch in ident.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// v1.15: PG19 `quote_literal_cstr` (quote.c).
/// Wraps the value in single quotes, doubling embedded quotes. When the
/// value contains a backslash the result is prefixed with `E` (escape
/// string syntax) and backslashes are doubled, so the output parses on
/// servers with `standard_conforming_strings = off`.
/// (Distinct from the pre-existing EXPLAIN-display `pg_quote_literal`,
/// which deliberately omits the E'' syntax for
/// standard_conforming_strings=on output.)
pub(crate) fn pg_quote_literal_cstr(s: &str) -> String {
    let escape = s.contains('\\');
    let mut out = String::with_capacity(s.len() + 3);
    if escape {
        out.push('E');
    }
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' || (escape && ch == '\\') {
            out.push(ch);
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

pub(crate) fn eval_str_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    match name {
        "upper" => Ok(match str_arg(name, &vals[0])? {
            None => Value::Null,
            Some(s) => Value::text(s.to_uppercase()),
        }),
        "lower" => Ok(match str_arg(name, &vals[0])? {
            None => Value::Null,
            Some(s) => Value::text(s.to_lowercase()),
        }),
        // v0.95: `parse_ident(qualname text [, strict bool])` (PG19
        // misc.c) — split a possibly-qualified identifier into its
        // component identifiers, as `text[]`.
        "parse_ident" => {
            let qual = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let strict = if vals.len() > 1 {
                match &vals[1] {
                    Value::Null => return Ok(Value::Null),
                    Value::Bool(b) => *b,
                    other => return Err(func_arg_err(name, other)),
                }
            } else {
                true
            };
            let parts = parse_ident_parts(qual, strict)?;
            Ok(Value::Array(Box::new(crate::storage::ArrayVal {
                elem: crate::storage::ArrayElem::Text,
                dims: vec![parts.len() as i32],
                lower: vec![1],
                elems: parts.into_iter().map(Value::text).collect(),
            })))
        }
        // v1.15: quote_ident/quote_literal/quote_nullable (PG19 quote.c).
        // quote_ident and quote_literal are STRICT (NULL -> NULL);
        // quote_nullable maps NULL to the text 'NULL'.
        "quote_ident" => Ok(match str_arg(name, &vals[0])? {
            None => Value::Null,
            Some(s) => Value::text(pg_quote_identifier_fn(s)),
        }),
        "quote_literal" => Ok(match str_arg(name, &vals[0])? {
            None => Value::Null,
            Some(s) => Value::text(pg_quote_literal_cstr(s)),
        }),
        "quote_nullable" => Ok(match str_arg(name, &vals[0])? {
            None => Value::text("NULL"),
            Some(s) => Value::text(pg_quote_literal_cstr(s)),
        }),
        "length" | "char_length" | "character_length" => {
            // v0.29: length(bytea) returns the byte count (PG).
            if let Value::Bytea(b) = &vals[0] {
                return Ok(Value::Int(b.len() as i64));
            }
            Ok(match str_arg(name, &vals[0])? {
                None => Value::Null,
                Some(s) => Value::Int(s.chars().count() as i64),
            })
        }
        // v0.29: octet_length returns the byte count. For bytea it's the
        // byte length; for text it's the UTF-8 byte length (PG).
        "octet_length" => Ok(match &vals[0] {
            Value::Null => Value::Null,
            Value::Bytea(b) => Value::Int(b.len() as i64),
            Value::Text(s) => Value::Int(s.len() as i64),
            // v0.35: PG's octet_length(bpchar) counts the padded bytes.
            Value::BpChar(s) => Value::Int(s.len() as i64),
            other => return Err(func_arg_err(name, other)),
        }),
        "substring" => {
            // v0.33: bytea form (PG bytea_substr) dispatches before str_arg.
            if let Value::Bytea(b) = &vals[0] {
                let start = match int_arg(name, &vals[1])? {
                    None => return Ok(Value::Null),
                    Some(n) => n,
                };
                let len = if vals.len() > 2 {
                    match int_arg(name, &vals[2])? {
                        None => return Ok(Value::Null),
                        Some(n) => Some(n),
                    }
                } else {
                    None
                };
                // bytea_substring raises 22011 for negative length.
                return Ok(Value::Bytea(bytea_substring(b, start, len)?));
            }
            if matches!(&vals[0], Value::Null) {
                // NULL subject: still validate other args for type errors?
                // PG is strict: NULL in -> NULL out.
                let _ = int_arg(name, &vals[1])?;
                if vals.len() > 2 {
                    let _ = int_arg(name, &vals[2])?;
                }
                return Ok(Value::Null);
            }
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let start = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let len = if vals.len() > 2 {
                match int_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(n) => {
                        if n < 0 {
                            return Err(exec_err("22011", "negative substring length not allowed"));
                        }
                        Some(n)
                    }
                }
            } else {
                None
            };
            // 1-based; start < 1 shifts the window (Postgres rule).
            Ok(Value::text(substring_int(s, start, len)))
        }
        "substring_from_for" => {
            // v0.24: `SUBSTRING(s FROM x FOR y)` — runtime dispatch.
            // Integer (start, len) vs SIMILAR (pattern, escape); PG
            // decides by type, so `-1`, `1+1`, etc. work as integers.
            // v0.33: bytea subject (PG bytea_substr) dispatches first.
            if let Value::Bytea(b) = &vals[0] {
                match (&vals[1], &vals[2]) {
                    (Value::Int(start), Value::Int(len)) => {
                        return Ok(Value::Bytea(bytea_substring(b, *start, Some(*len))?));
                    }
                    (Value::Null, _) | (_, Value::Null) => return Ok(Value::Null),
                    _ => return Err(func_arg_err(name, &vals[1])),
                }
            }
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            match (&vals[1], &vals[2]) {
                (Value::Int(start), Value::Int(len)) => {
                    if *len < 0 {
                        return Err(exec_err("22011", "negative substring length not allowed"));
                    }
                    Ok(Value::text(substring_int(s, *start, Some(*len))))
                }
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                _ => {
                    // SIMILAR form: pattern + escape char.
                    let pat = match str_arg(name, &vals[1])? {
                        None => return Ok(Value::Null),
                        Some(p) => p,
                    };
                    let esc_s = match str_arg(name, &vals[2])? {
                        None => return Ok(Value::Null),
                        Some(e) => e,
                    };
                    // v0.31: empty escape means "no escape character" (PG 19).
                    let escape = similar_escape_opt(&esc_s)?;
                    Ok(substring_similar_match(s, pat, escape)?)
                }
            }
        }
        "position" => {
            // bytea position: 1-based byte index, 0 if not found.
            if let (Value::Bytea(sub), Value::Bytea(s)) = (&vals[0], &vals[1]) {
                if sub.is_empty() {
                    return Ok(Value::Int(1));
                }
                let pos = s
                    .windows(sub.len())
                    .position(|w| w == sub.as_slice())
                    .map(|i| i as i64 + 1)
                    .unwrap_or(0);
                return Ok(Value::Int(pos));
            }
            if matches!(&vals[0], Value::Null) || matches!(&vals[1], Value::Null) {
                return Ok(Value::Null);
            }
            let sub = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let s = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            if sub.is_empty() {
                return Ok(Value::Int(1));
            }
            let sc: Vec<char> = s.chars().collect();
            let nc: Vec<char> = sub.chars().collect();
            let pos = sc
                .windows(nc.len())
                .position(|w| w == nc.as_slice())
                .map(|i| i as i64 + 1)
                .unwrap_or(0);
            Ok(Value::Int(pos))
        }
        "replace" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let from = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let to = match str_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            // Postgres: empty search string leaves the input unchanged.
            if from.is_empty() {
                return Ok(Value::text(s));
            }
            Ok(Value::text(s.replace(from, to)))
        }
        "split_part" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let delim = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let n = match int_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            if n == 0 {
                return Err(exec_err(
                    "22023",
                    "field position must be greater than zero",
                ));
            }
            let parts: Vec<&str> = if delim.is_empty() {
                // Empty delimiter: PG does not split; the whole string is
                // field 1 (and field -1).
                vec![s]
            } else {
                s.split(delim).collect()
            };
            let idx = if n > 0 { n - 1 } else { parts.len() as i64 + n };
            let r = if idx >= 0 {
                parts.get(idx as usize).copied().unwrap_or("")
            } else {
                ""
            };
            Ok(Value::text(r))
        }
        // --- v0.16: missing built-ins (pg_regress 42883 cluster) -----------
        "concat" => {
            // NULL arguments are ignored; every other type is coerced via
            // its text output, like Postgres.
            let mut out = String::new();
            for v in vals {
                if let Some(t) = v.to_text() {
                    out.push_str(&t);
                }
            }
            Ok(Value::text(out))
        }
        "concat_ws" => {
            // A NULL separator makes the whole result NULL; NULL
            // arguments are skipped (no stray separators), like Postgres.
            let sep = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let mut out = String::new();
            let mut first = true;
            for v in &vals[1..] {
                if let Some(t) = v.to_text() {
                    if !first {
                        out.push_str(sep);
                    }
                    out.push_str(&t);
                    first = false;
                }
            }
            Ok(Value::text(out))
        }
        "to_hex" | "to_oct" | "to_bin" => {
            // Postgres width rule: int2/int4 render negatives as 32-bit
            // two's complement, int8 as 64-bit two's complement; the
            // width comes from the *type* of the argument, not its range
            // (so -1234::bigint is 64-bit). Non-negative values render
            // with no leading zeros.
            let (wide, v): (bool, i64) = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::SmallInt(i) => (false, *i as i64),
                Value::Int(i) => (false, *i),
                Value::BigInt(i) => (true, *i),
                other => return Err(func_arg_err(name, other)),
            };
            let r = if wide {
                let u = v as u64;
                match name {
                    "to_hex" => format!("{:x}", u),
                    "to_oct" => format!("{:o}", u),
                    _ => format!("{:b}", u),
                }
            } else {
                let u = (v as i32) as u32;
                match name {
                    "to_hex" => format!("{:x}", u),
                    "to_oct" => format!("{:o}", u),
                    _ => format!("{:b}", u),
                }
            };
            Ok(Value::text(r))
        }
        "left" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let n = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            // Negative n drops the last |n| characters (Postgres rule).
            let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
            Ok(Value::text(
                chars[..take as usize].iter().collect::<String>(),
            ))
        }
        "right" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let n = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            // Negative n drops the first |n| characters (Postgres rule).
            let skip = if n >= 0 {
                (len - n).max(0)
            } else {
                (-n).min(len)
            };
            Ok(Value::text(
                chars[skip as usize..].iter().collect::<String>(),
            ))
        }
        "reverse" => Ok(match &vals[0] {
            Value::Null => Value::Null,
            Value::Bytea(b) => Value::Bytea(b.iter().rev().copied().collect()),
            v => match str_arg(name, v)? {
                None => Value::Null,
                Some(s) => Value::text(s.chars().rev().collect::<String>()),
            },
        }),
        // --- v0.24: missing string built-ins (pg_regress strings cluster) --
        "repeat" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let n = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            // Postgres: repeat(s, n <= 0) is '' (no error; PG 19
            // regression output confirms `repeat('Pg', -4)` -> '').
            if n <= 0 || s.is_empty() {
                return Ok(Value::text(String::new()));
            }
            let total = (n as u64).checked_mul(s.len() as u64);
            if !matches!(total, Some(t) if t <= MAX_STR_RESULT_BYTES) {
                return Err(exec_err("54000", "requested length too large"));
            }
            Ok(Value::text(s.repeat(n as usize)))
        }
        "lpad" | "rpad" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let n = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let fill = if vals.len() > 2 {
                match str_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(f) => f,
                }
            } else {
                " "
            };
            // Postgres: negative length -> ''.
            if n < 0 {
                return Ok(Value::text(String::new()));
            }
            if n as u64 > MAX_STR_RESULT_CHARS {
                return Err(exec_err("54000", "requested length too large"));
            }
            let n = n as usize;
            let chars: Vec<char> = s.chars().collect();
            if chars.len() >= n {
                return Ok(Value::text(chars[..n].iter().collect::<String>()));
            }
            let fill_chars: Vec<char> = fill.chars().collect();
            if fill_chars.is_empty() {
                // Postgres: empty fill -> no padding (truncation above
                // still applies).
                return Ok(Value::text(chars.iter().collect::<String>()));
            }
            let need = n - chars.len();
            // Cycle the fill to exactly `need` chars.
            let reps = need / fill_chars.len() + 1;
            let fill_s: String = fill_chars.iter().collect();
            let pad: String = fill_s.repeat(reps).chars().take(need).collect();
            let body: String = chars.iter().collect();
            Ok(Value::text(if name == "lpad" {
                format!("{pad}{body}")
            } else {
                format!("{body}{pad}")
            }))
        }
        "ascii" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            // Postgres: ascii('') is 0; otherwise the code point of the
            // first character (full astral code point in UTF-8).
            match s.chars().next() {
                None => Ok(Value::Int(0)),
                Some(c) => Ok(Value::Int(c as u32 as i64)),
            }
        }
        "chr" => {
            let n = match int_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            // Postgres' exact error split: negative -> 22023, zero or
            // out-of-range -> 54000.
            if n < 0 {
                return Err(exec_err("22023", "character number must be positive"));
            }
            if n == 0 {
                return Err(exec_err("54000", "null character not permitted"));
            }
            if n > 0x10FFFF {
                return Err(exec_err(
                    "54000",
                    "requested character too large for encoding",
                ));
            }
            match char::from_u32(n as u32) {
                Some(c) => Ok(Value::text(c.to_string())),
                None => Err(exec_err(
                    "54000",
                    "requested character not valid for encoding",
                )),
            }
        }
        "initcap" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            // First alphabetic char after a non-alphanumeric -> upper;
            // other alphabetics -> lower; the rest pass through.
            let mut out = String::with_capacity(s.len());
            let mut prev_alnum = false;
            for c in s.chars() {
                if c.is_alphabetic() {
                    if prev_alnum {
                        out.extend(c.to_lowercase());
                    } else {
                        out.extend(c.to_uppercase());
                    }
                } else {
                    out.push(c);
                }
                prev_alnum = c.is_alphanumeric();
            }
            Ok(Value::text(out))
        }
        "ltrim" | "rtrim" => {
            // v0.33: bytea form (PG bytealtrim/byteartrim).
            if let Value::Bytea(b) = &vals[0] {
                let set: &[u8] = if vals.len() > 1 {
                    match &vals[1] {
                        Value::Null => return Ok(Value::Null),
                        Value::Bytea(sb) => sb,
                        other => return Err(func_arg_err(name, other)),
                    }
                } else {
                    // PG has no 1-arg bytea ltrim/rtrim; treat as no-op set.
                    // (Unreachable via arity check for bytea, but be safe.)
                    &[]
                };
                let (doltrim, dortrim) = if name == "ltrim" {
                    (true, false)
                } else {
                    (false, true)
                };
                return Ok(Value::Bytea(bytea_trim(b, set, doltrim, dortrim)));
            }
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let set: Vec<char> = if vals.len() > 1 {
                match str_arg(name, &vals[1])? {
                    None => return Ok(Value::Null),
                    Some(c) => c.chars().collect(),
                }
            } else {
                vec![' ']
            };
            let t = if name == "ltrim" {
                s.trim_start_matches(|c| set.contains(&c))
            } else {
                s.trim_end_matches(|c| set.contains(&c))
            };
            Ok(Value::text(t.to_string()))
        }
        // v0.33: btrim(text [, text]) -> text; btrim(bytea, bytea) -> bytea
        // (PG btrim/btrim1/byteatrim). SQL trim syntax desugars to this.
        "btrim" => {
            // Bytea form.
            if let Value::Bytea(b) = &vals[0] {
                let set: &[u8] = if vals.len() > 1 {
                    match &vals[1] {
                        Value::Null => return Ok(Value::Null),
                        Value::Bytea(sb) => sb,
                        other => return Err(func_arg_err(name, other)),
                    }
                } else {
                    // No 1-arg bytea btrim in PG; arity check prevents this.
                    return Err(func_arg_err(name, &vals[0]));
                };
                return Ok(Value::Bytea(bytea_trim(b, set, true, true)));
            }
            if matches!(&vals[0], Value::Null) {
                return Ok(Value::Null);
            }
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let set: Vec<char> = if vals.len() > 1 {
                match str_arg(name, &vals[1])? {
                    None => return Ok(Value::Null),
                    Some(c) => c.chars().collect(),
                }
            } else {
                vec![' ']
            };
            Ok(Value::text(
                s.trim_matches(|c| set.contains(&c)).to_string(),
            ))
        }
        "encode" => {
            let data: Vec<u8> = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b.clone(),
                v => match str_arg(name, v)? {
                    None => return Ok(Value::Null),
                    // v0.24: unknown literals coerce to bytea via the
                    // bytea input function (`\x` hex or escape format),
                    // like PG.
                    Some(s) => match text_to_bytea(s) {
                        Some(b) => b,
                        None => {
                            return Err(exec_err(
                                "22P02",
                                format!("invalid input syntax for type bytea: {:?}", s),
                            ));
                        }
                    },
                },
            };
            let data: &[u8] = &data;
            let fmt_raw = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(f) => f,
            };
            let fmt = fmt_raw.to_lowercase();
            match fmt.as_str() {
                "hex" => Ok(Value::text(hex_encode(data))),
                "base64" => Ok(Value::text(base64_encode(data, false))),
                "base64url" => Ok(Value::text(base64_encode(data, true))),
                "base32hex" => Ok(Value::text(base32hex_encode(data))),
                "escape" => Ok(Value::text(bytea_encode_escape(data))),
                // v0.24: PG 19's exact message (original case).
                _ => Err(exec_err(
                    "22023",
                    format!("unrecognized encoding: \"{}\"", fmt_raw),
                )),
            }
        }
        "decode" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let fmt_raw = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(f) => f,
            };
            let fmt = fmt_raw.to_lowercase();
            match fmt.as_str() {
                "hex" => match hex_decode(s) {
                    Some(b) => Ok(Value::Bytea(b)),
                    None => Err(exec_err("22023", "invalid hex string")),
                },
                "base64" => match base64_decode(s, false) {
                    Some(b) => Ok(Value::Bytea(b)),
                    None => Err(exec_err("22023", "invalid base64 string")),
                },
                "base64url" => match base64url_decode(s) {
                    Ok(b) => Ok(Value::Bytea(b)),
                    Err(msg) => Err(exec_err("22023", msg)),
                },
                "base32hex" => match base32hex_decode(s) {
                    Some(b) => Ok(Value::Bytea(b)),
                    None => Err(exec_err("22023", "invalid base32hex string")),
                },
                "escape" => match bytea_unescape(s) {
                    Some(b) => Ok(Value::Bytea(b)),
                    None => Err(exec_err("22023", "invalid escape string")),
                },
                // v0.24: PG 19's exact message (original case).
                _ => Err(exec_err(
                    "22023",
                    format!("unrecognized encoding: \"{}\"", fmt_raw),
                )),
            }
        }
        // v0.28: bytea bit/byte accessors. PG numbers bits from the right
        // within each byte: bit 0 is the least significant bit of byte 0
        // ("bit 0 is the least significant bit of the first byte, and
        // bit 15 is the most significant bit of the second byte").
        "get_bit" => {
            let b = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b,
                other => return Err(func_arg_err(name, other)),
            };
            let bit = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            if bit < 0 || bit >= b.len() as i64 * 8 {
                return Err(exec_err(
                    "22000",
                    format!(
                        "index {} out of valid range, 0..{}",
                        bit,
                        b.len() as i64 * 8 - 1
                    ),
                ));
            }
            let bit = bit as usize;
            let byte_idx = bit / 8;
            let bit_idx = bit % 8;
            Ok(Value::Int(((b[byte_idx] >> bit_idx) & 1) as i64))
        }
        "set_bit" => {
            let b = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b.clone(),
                other => return Err(func_arg_err(name, other)),
            };
            let bit = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let val = match int_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            if bit < 0 || bit >= b.len() as i64 * 8 {
                return Err(exec_err(
                    "22000",
                    format!(
                        "index {} out of valid range, 0..{}",
                        bit,
                        b.len() as i64 * 8 - 1
                    ),
                ));
            }
            if val != 0 && val != 1 {
                return Err(exec_err("22000", "new bit must be 0 or 1"));
            }
            let bit = bit as usize;
            let byte_idx = bit / 8;
            let bit_idx = bit % 8;
            let mut out = b;
            if val == 1 {
                out[byte_idx] |= 1 << bit_idx;
            } else {
                out[byte_idx] &= !(1 << bit_idx);
            }
            Ok(Value::Bytea(out))
        }
        "get_byte" => {
            let b = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b,
                other => return Err(func_arg_err(name, other)),
            };
            let idx = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            if idx < 0 || idx >= b.len() as i64 {
                return Err(exec_err(
                    "22000",
                    format!(
                        "index {} out of valid range, 0..{}",
                        idx,
                        b.len() as i64 - 1
                    ),
                ));
            }
            Ok(Value::Int(b[idx as usize] as i64))
        }
        "set_byte" => {
            let b = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b.clone(),
                other => return Err(func_arg_err(name, other)),
            };
            let idx = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let val = match int_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            if idx < 0 || idx >= b.len() as i64 {
                return Err(exec_err(
                    "22000",
                    format!(
                        "index {} out of valid range, 0..{}",
                        idx,
                        b.len() as i64 - 1
                    ),
                ));
            }
            if val < 0 || val > 255 {
                return Err(exec_err("22000", "new byte must be 0..255"));
            }
            let mut out = b;
            out[idx as usize] = val as u8;
            Ok(Value::Bytea(out))
        }
        "bit_count" => {
            let b = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b,
                other => return Err(func_arg_err(name, other)),
            };
            Ok(Value::BigInt(b.iter().map(|x| x.count_ones() as i64).sum()))
        }
        // v0.29: pg_input_is_valid(input text, type text) -> bool.
        // v0.43: probes via the shared pg_input_validate core (PG19
        // misc.c pg_input_is_valid_common); soft input errors report
        // false, hard errors (unknown type -> 42883, malformed char
        // typmod -> 22023) propagate.
        "pg_input_is_valid" => {
            let input = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let typ = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            match pg_input_validate("pg_input_is_valid", input, typ) {
                Ok(()) => Ok(Value::Bool(true)),
                Err(PgInputFailure::Soft(_)) => Ok(Value::Bool(false)),
                Err(PgInputFailure::Hard(e)) => Err(e),
            }
        }
        "crc32" => {
            let data: &[u8] = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b,
                Value::Text(s) => s.as_bytes(),
                other => {
                    return Err(exec_err(
                        "42883",
                        format!("crc32({}) not supported", other.type_name()),
                    ));
                }
            };
            Ok(Value::BigInt(crc32(data) as i64))
        }
        "crc32c" => {
            let data: &[u8] = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b,
                Value::Text(s) => s.as_bytes(),
                other => {
                    return Err(exec_err(
                        "42883",
                        format!("crc32c({}) not supported", other.type_name()),
                    ));
                }
            };
            Ok(Value::BigInt(crc32c(data) as i64))
        }
        "sha224" | "sha256" | "sha384" | "sha512" => {
            let data: &[u8] = match &vals[0] {
                Value::Null => return Ok(Value::Null),
                Value::Bytea(b) => b,
                Value::Text(s) => s.as_bytes(),
                other => {
                    return Err(exec_err(
                        "42883",
                        format!("{}({}) not supported", name, other.type_name()),
                    ));
                }
            };
            let bytes: Vec<u8> = match name {
                "sha224" => crate::crypto::sha224(data).to_vec(),
                "sha256" => crate::crypto::sha256(data).to_vec(),
                "sha384" => crate::crypto::sha384(data).to_vec(),
                _ => crate::crypto::sha512(data).to_vec(),
            };
            Ok(Value::Bytea(bytes))
        }
        "strpos" => {
            // Alias of position(substring IN string); empty substring -> 1.
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let sub = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            if sub.is_empty() {
                return Ok(Value::Int(1));
            }
            let sc: Vec<char> = s.chars().collect();
            let nc: Vec<char> = sub.chars().collect();
            let pos = sc
                .windows(nc.len())
                .position(|w| w == nc.as_slice())
                .map(|i| i as i64 + 1)
                .unwrap_or(0);
            Ok(Value::Int(pos))
        }
        "translate" => {
            // translate(s, from, to): each char in `from` is replaced by the
            // char at the same position in `to`; chars with no counterpart
            // are deleted.
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let from = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let to = match str_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let from_c: Vec<char> = from.chars().collect();
            let to_c: Vec<char> = to.chars().collect();
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match from_c.iter().position(|&f| f == c) {
                    Some(i) => {
                        if let Some(&t) = to_c.get(i) {
                            out.push(t);
                        }
                        // else: char is deleted.
                    }
                    None => out.push(c),
                }
            }
            Ok(Value::text(out))
        }
        "unistr" => {
            // unistr(s): interpret \uXXXX and \UXXXXXXXX escapes.
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            match unistr_decode(s) {
                Some(t) => Ok(Value::text(t)),
                None => Err(exec_err("22023", "invalid Unicode escape")),
            }
        }
        // --- v0.19: regexp_* functions -----------------------------------
        "regexp_like" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let flags = if vals.len() > 2 {
                match str_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(f) => f,
                }
            } else {
                ""
            };
            // v0.24: strict flag validation; `g` is rejected like PG.
            let (opts, _) = parse_regexp_flags(flags, name, false)?;
            let re = crate::regex::compile_opts(pat, opts)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            Ok(Value::Bool(re.is_match(&sc)))
        }
        "regexp_count" => {
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let start = if vals.len() > 2 {
                match int_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(n) => n,
                }
            } else {
                1
            };
            let flags = if vals.len() > 3 {
                match str_arg(name, &vals[3])? {
                    None => return Ok(Value::Null),
                    Some(f) => f,
                }
            } else {
                ""
            };
            let (opts, _) = parse_regexp_flags(flags, name, false)?;
            let re = crate::regex::compile_opts(pat, opts)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            // v0.24: start < 1 is an error, not a clamp (PG: `invalid
            // value for parameter "start"`).
            let start_idx = check_regexp_start(start)?.min(sc.len());
            let mut count = 0i64;
            let mut pos = start_idx;
            while pos <= sc.len() {
                match re.find_at(&sc, pos) {
                    Some((ms, me, _)) => {
                        count += 1;
                        // Avoid infinite loop on empty matches.
                        pos = if me > ms { me } else { ms + 1 };
                    }
                    None => break,
                }
            }
            Ok(Value::Int(count))
        }
        "regexp_instr" => {
            // regexp_instr(s, pat [, start [, occurrence [, end_option [, flags [, subexpr]]]]])
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let start = get_int_arg(name, vals, 2, 1)?;
            let occurrence = get_int_arg(name, vals, 3, 1)?;
            let end_option = get_int_arg(name, vals, 4, 0)?;
            let flags = get_str_arg(name, vals, 5, "")?;
            let subexpr = get_int_arg(name, vals, 6, 0)?;
            // v0.24: strict PG validation (was: clamps and silent
            // acceptance).
            let start_idx = check_regexp_start(start)?;
            check_regexp_n(occurrence, "n")?;
            if end_option != 0 && end_option != 1 {
                return Err(exec_err(
                    "22023",
                    format!("invalid value for parameter \"endoption\": {end_option}"),
                ));
            }
            if subexpr < 0 {
                return Err(exec_err(
                    "22023",
                    format!("invalid value for parameter \"subexpr\": {subexpr}"),
                ));
            }
            let subexpr = subexpr as usize;
            let (opts, _) = parse_regexp_flags(flags, name, false)?;
            let re = crate::regex::compile_opts(pat, opts)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            let mut pos = start_idx.min(sc.len());
            let mut found = None;
            let mut matched = 0;
            for _ in 0..occurrence {
                match re.find_at(&sc, pos) {
                    Some((ms, me, caps)) => {
                        found = Some((ms, me, caps));
                        matched += 1;
                        pos = if me > ms { me } else { ms + 1 };
                    }
                    None => break,
                }
            }
            // Only return a result if we found the requested occurrence.
            if matched < occurrence {
                found = None;
            }
            match found {
                Some((ms, me, caps)) => {
                    // subexpr: 0 = whole match, N = Nth group.
                    let (rs, re_) = if subexpr == 0 {
                        (ms, me)
                    } else {
                        match caps.groups.get(subexpr).copied().flatten() {
                            Some((gs, ge)) => (gs, ge),
                            None => return Ok(Value::Int(0)),
                        }
                    };
                    // end_option: 0 = start position, 1 = end position + 1.
                    let result = if end_option == 0 { rs + 1 } else { re_ + 1 };
                    Ok(Value::Int(result as i64))
                }
                None => Ok(Value::Int(0)),
            }
        }
        "regexp_substr" => {
            // regexp_substr(s, pat [, start [, occurrence [, flags [, subexpr]]]])
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let start = get_int_arg(name, vals, 2, 1)?;
            let occurrence = get_int_arg(name, vals, 3, 1)?;
            let flags = get_str_arg(name, vals, 4, "")?;
            let subexpr = get_int_arg(name, vals, 5, 0)?;
            // v0.24: strict PG validation (was: clamps).
            let start_idx = check_regexp_start(start)?;
            check_regexp_n(occurrence, "n")?;
            if subexpr < 0 {
                return Err(exec_err(
                    "22023",
                    format!("invalid value for parameter \"subexpr\": {subexpr}"),
                ));
            }
            let subexpr = subexpr as usize;
            let (opts, _) = parse_regexp_flags(flags, name, false)?;
            let re = crate::regex::compile_opts(pat, opts)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            let mut pos = start_idx.min(sc.len());
            let mut found = None;
            let mut matched = 0;
            for _ in 0..occurrence {
                match re.find_at(&sc, pos) {
                    Some((ms, me, caps)) => {
                        found = Some((ms, me, caps));
                        matched += 1;
                        pos = if me > ms { me } else { ms + 1 };
                    }
                    None => break,
                }
            }
            // Only return a result if we found the requested occurrence.
            if matched < occurrence {
                found = None;
            }
            match found {
                Some((ms, me, caps)) => {
                    let (rs, re_) = if subexpr == 0 {
                        (ms, me)
                    } else {
                        match caps.groups.get(subexpr).copied().flatten() {
                            Some((gs, ge)) => (gs, ge),
                            None => return Ok(Value::Null),
                        }
                    };
                    Ok(Value::text(sc[rs..re_].iter().collect::<String>()))
                }
                None => Ok(Value::Null),
            }
        }
        "substring_similar" => {
            // SUBSTRING(s SIMILAR pat [ESCAPE 'c']): extract the substring
            // matching the pattern; if the pattern has a group, return the
            // first group.
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let escape = if vals.len() > 2 {
                match str_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(e) => similar_escape_opt(&e)?,
                }
            } else {
                // v0.31: omitted ESCAPE defaults to backslash (PG 19).
                Some('\\')
            };
            let regex_pat = similar_to_regex(pat, escape)
                .map_err(|e| exec_err("2201B", format!("invalid SIMILAR TO pattern: {}", e)))?;
            let re = crate::regex::compile(&regex_pat, false)
                .map_err(|e| exec_err("2201B", format!("invalid SIMILAR TO pattern: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            // Must match entire string.
            match re.find_at(&sc, 0) {
                Some((ms, me, caps)) if ms == 0 && me == sc.len() => {
                    // If there's a captured group, return it; else whole match.
                    if re.group_count() >= 1 {
                        match caps.groups.get(1).copied().flatten() {
                            Some((gs, ge)) => {
                                Ok(Value::text(sc[gs..ge].iter().collect::<String>()))
                            }
                            None => Ok(Value::Null),
                        }
                    } else {
                        Ok(Value::text(sc[ms..me].iter().collect::<String>()))
                    }
                }
                _ => Ok(Value::Null),
            }
        }
        "substring_from" => {
            // SUBSTRING(s FROM pat): POSIX regex substring. If the pattern
            // contains a group, return the first group; else the whole match.
            // v0.33: bytea subject with integer start (PG bytea_substr_no_len).
            if let Value::Bytea(b) = &vals[0] {
                match &vals[1] {
                    Value::Int(n) => {
                        return Ok(Value::Bytea(bytea_substring(b, *n, None)?));
                    }
                    Value::Null => return Ok(Value::Null),
                    _ => return Err(func_arg_err(name, &vals[1])),
                }
            }
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            // The second arg could be text (pattern) or int (start).
            // If it's an integer, fall back to substring(s, start).
            // Check the type directly to avoid int_arg's type error.
            match &vals[1] {
                Value::Int(n) => {
                    let sc: Vec<char> = s.chars().collect();
                    let total = sc.len() as i64;
                    let from = (*n).max(1);
                    let lo = (from - 1).max(0).min(total) as usize;
                    return Ok(Value::text(sc[lo..].iter().collect::<String>()));
                }
                Value::Null => return Ok(Value::Null),
                Value::Text(_) => {} // Fall through to pattern handling.
                other => return Err(func_arg_err(name, other)),
            }
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let re = crate::regex::compile(pat, false)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            match re.find_at(&sc, 0) {
                Some((ms, me, caps)) => {
                    if re.group_count() >= 1 {
                        match caps.groups.get(1).copied().flatten() {
                            Some((gs, ge)) => {
                                Ok(Value::text(sc[gs..ge].iter().collect::<String>()))
                            }
                            None => Ok(Value::Null),
                        }
                    } else {
                        Ok(Value::text(sc[ms..me].iter().collect::<String>()))
                    }
                }
                None => Ok(Value::Null),
            }
        }
        "similar_to" => {
            // SIMILAR TO pattern [ESCAPE 'c']: translate to regex and match.
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let escape = if vals.len() > 2 {
                match str_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(e) => similar_escape_opt(&e)?,
                }
            } else {
                // v0.31: omitted ESCAPE defaults to backslash (PG 19).
                Some('\\')
            };
            let regex_pat = similar_to_regex(pat, escape)
                .map_err(|e| exec_err("2201B", format!("invalid SIMILAR TO pattern: {}", e)))?;
            let re = crate::regex::compile(&regex_pat, false)
                .map_err(|e| exec_err("2201B", format!("invalid SIMILAR TO pattern: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            // SIMILAR TO must match the entire string.
            let matched = match re.find_at(&sc, 0) {
                Some((ms, me, _)) => ms == 0 && me == sc.len(),
                None => false,
            };
            Ok(Value::Bool(matched))
        }
        "regexp_replace" => {
            // v0.24: two PG forms.
            // Legacy: regexp_replace(s, pat, repl, flags) — 4th arg is
            // text (PG overload resolution); replaces the first match, or
            // all with 'g'.
            // Extended: regexp_replace(s, pat, repl [, start [, n [, flags]]])
            // — n=0 replaces all from start, n>0 replaces only the nth
            // match; an explicitly given n makes 'g' irrelevant.
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let repl = match str_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            // Legacy 4-text-arg form?
            if vals.len() == 4 && matches!(&vals[3], Value::Text(_)) {
                let flags = match str_arg(name, &vals[3])? {
                    None => return Ok(Value::Null),
                    Some(f) => f,
                };
                let (opts, global) = parse_regexp_flags(flags, name, true)?;
                let re = crate::regex::compile_opts(pat, opts)
                    .map_err(|e| exec_err("2201B", format!("invalid regular expression: {}", e)))?;
                let sc: Vec<char> = s.chars().collect();
                // v0.34: PG 19 legacy form — n defaults to 1 (first match
                // only) unless the 'g' flag requests all (n=0).
                let n = if global { 0 } else { 1 };
                let out = pg_replace_text_regexp(&sc, &re, repl, 0, n);
                return Ok(Value::text(out));
            }
            // Extended form.
            let start = get_int_arg(name, vals, 3, 1)?;
            let n_given = vals.len() > 4;
            let n = get_int_arg(name, vals, 4, 0)?;
            let flags = get_str_arg(name, vals, 5, "")?;
            // v0.24: strict PG validation (was: clamps).
            let start_idx = check_regexp_start(start)?;
            if n < 0 {
                return Err(exec_err(
                    "22023",
                    format!("invalid value for parameter \"n\": {n}"),
                ));
            }
            let (opts, has_g) = parse_regexp_flags(flags, name, true)?;
            let re = crate::regex::compile_opts(pat, opts)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {}", e)))?;
            let sc: Vec<char> = s.chars().collect();
            let start_idx = start_idx.min(sc.len());
            // v0.34: PG 19 n default — if N was not specified, n=0 (all)
            // only when the 'g' flag is present, else n=1 (first match).
            // An explicitly given n makes 'g' irrelevant.
            let n = if n_given {
                n
            } else if has_g {
                0
            } else {
                1
            };
            let out = pg_replace_text_regexp(&sc, &re, repl, start_idx, n);
            Ok(Value::text(out))
        }
        "regexp_split_to_array" => {
            // v0.32: port of PG 19 regexp_split_to_array — the split is
            // internally global, degenerate zero-length delimiter matches
            // are ignored (see regexp_find_all), and the result follows
            // build_regexp_split_result with PG array_out quoting.
            let name = "regexp_split_to_array";
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s.to_string(),
            };
            let pat = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(p) => p.to_string(),
            };
            let flags = if vals.len() > 2 {
                match str_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(f) => f.to_string(),
                }
            } else {
                String::new()
            };
            let (opts, _) = parse_regexp_flags(&flags, name, false)?;
            let re = crate::regex::compile_opts(&pat, opts)
                .map_err(|e| exec_err("2201B", format!("invalid regular expression: {e}")))?;
            let sc: Vec<char> = s.chars().collect();
            let matches = regexp_find_all(&re, &sc, true, true);
            let parts = regexp_split_result(&sc, &matches);
            let lit = pg_text_array_literal(&parts.into_iter().map(Some).collect::<Vec<_>>());
            Ok(Value::text(lit))
        }
        "regexp_matches" => {
            // v0.32: scalar context — first match row, or NULL when there
            // is no match. Set semantics live in eval_srf_vals.
            let rows = regexp_matches_rows(vals)?;
            Ok(rows.into_iter().next().unwrap_or(Value::Null))
        }
        "overlay" => {
            // overlay(s, replacement, start [, len]); omitted len defaults
            // to length(replacement). 1-based start; start < 1 is clamped.
            // v0.33: bytea form (PG byteaoverlay/byteaoverlay_no_len).
            // Unlike text, bytea errors on start <= 0 (22011).
            if let Value::Bytea(b) = &vals[0] {
                let r: &[u8] = match &vals[1] {
                    Value::Null => return Ok(Value::Null),
                    Value::Bytea(rb) => rb,
                    other => return Err(func_arg_err(name, other)),
                };
                let start = match int_arg(name, &vals[2])? {
                    None => return Ok(Value::Null),
                    Some(n) => n,
                };
                let len = if vals.len() > 3 {
                    match int_arg(name, &vals[3])? {
                        None => return Ok(Value::Null),
                        Some(n) => n,
                    }
                } else {
                    r.len() as i64
                };
                // bytea_overlay raises 22011/22003 as appropriate.
                return Ok(Value::Bytea(bytea_overlay(b, r, start, len)?));
            }
            if matches!(&vals[0], Value::Null) {
                // PG is strict: NULL subject -> NULL (no arg validation).
                return Ok(Value::Null);
            }
            let s = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let r = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let start = match int_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let len = if vals.len() > 3 {
                match int_arg(name, &vals[3])? {
                    None => return Ok(Value::Null),
                    Some(n) => {
                        if n < 0 {
                            return Err(exec_err("22011", "negative overlay length not allowed"));
                        }
                        n
                    }
                }
            } else {
                r.chars().count() as i64
            };
            let sc: Vec<char> = s.chars().collect();
            let total = sc.len() as i64;
            // 1-based start, clamped to [1, total+1].
            let from = start.max(1).min(total + 1) as usize;
            let upto = (from as i64 + len).min(total + 1).max(from as i64) as usize;
            let mut out: String = sc[..from - 1].iter().collect();
            out.push_str(r);
            out.extend(sc[upto - 1..].iter());
            Ok(Value::text(out))
        }
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// CRC32C (Castagnoli) checksum, as in PG's crc32c() function.
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    // Bit-by-bit implementation (no table); adequate for test-size inputs.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0x82F6_3B78;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// v0.30: CRC-32 (IEEE, as in PG 18+'s crc32() function).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    // Bit-by-bit implementation (no table); adequate for test-size inputs.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Lowercase hex encode.
pub(crate) fn hex_encode(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for &b in data {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Hex decode; None on invalid input.
pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// Base64 encode (standard or URL-safe alphabet, with padding).
pub(crate) fn base64_encode(data: &[u8], url_safe: bool) -> String {
    const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let alpha: &[u8; 64] = if url_safe { URL } else { STD };
    // v0.30: PG's base64url output is unpadded; standard base64 pads with '='.
    let pad = !url_safe;
    // v0.33: PG inserts '\n' after every 76 chars for standard base64
    // (not base64url), per pg_base64_encode_internal. Only full 3-byte
    // groups trigger the newline; the final partial group does not.
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    let mut i = 0;
    let mut col = 0;
    // Full groups.
    while i + 3 <= data.len() {
        let b0 = data[i];
        let b1 = data[i + 1];
        let b2 = data[i + 2];
        out.push(alpha[(b0 >> 2) as usize] as char);
        out.push(alpha[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        out.push(alpha[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        out.push(alpha[(b2 & 63) as usize] as char);
        i += 3;
        if !url_safe {
            col += 4;
            if col >= 76 {
                out.push('\n');
                col = 0;
            }
        }
    }
    // Partial group (1-2 bytes): no newline check (PG behavior).
    let rem = data.len() - i;
    if rem == 1 {
        let b0 = data[i];
        out.push(alpha[(b0 >> 2) as usize] as char);
        out.push(alpha[((b0 & 3) << 4) as usize] as char);
        if pad {
            out.push('=');
            out.push('=');
        }
    } else if rem == 2 {
        let b0 = data[i];
        let b1 = data[i + 1];
        out.push(alpha[(b0 >> 2) as usize] as char);
        out.push(alpha[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        out.push(alpha[((b1 & 15) << 2) as usize] as char);
        if pad {
            out.push('=');
        }
    }
    out
}

/// v0.30: PG 19's base64url decoder. Accepts unpadded input (length % 4 may be
/// 0, 2, or 3) and optional '=' padding. Returns PG's exact error messages.
pub(crate) fn base64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut vals: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    // Find where padding starts (if any); '=' must only appear at the end.
    while i < bytes.len() {
        let c = bytes[i];
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => {
                // '=' must be at the end; check remaining are all '='
                let mut j = i;
                while j < bytes.len() && bytes[j] == b'=' {
                    j += 1;
                }
                if j < bytes.len() {
                    return Err("unexpected \"=\" while decoding base64url sequence".to_string());
                }
                let pad_len = j - i;
                if pad_len > 2 {
                    return Err("invalid base64url end sequence".to_string());
                }
                // Padding is valid; stop processing (pad_len 1 or 2)
                break;
            }
            _ => {
                return Err(format!(
                    "invalid symbol \"{}\" found while decoding base64url sequence",
                    c as char
                ));
            }
        };
        vals.push(v);
        i += 1;
    }
    // Length % 4 == 1 is impossible (a single base64 char can't encode a byte)
    match vals.len() % 4 {
        1 => Err("invalid base64url end sequence".to_string()),
        _ => {
            // Decode, handling the final partial group
            let mut out = Vec::with_capacity(vals.len() / 4 * 3 + 3);
            let mut j = 0;
            while j + 4 <= vals.len() {
                let a = vals[j];
                let b = vals[j + 1];
                let c = vals[j + 2];
                let d = vals[j + 3];
                out.push((a << 2) | (b >> 4));
                out.push(((b & 15) << 4) | (c >> 2));
                out.push(((c & 3) << 6) | d);
                j += 4;
            }
            // Handle remainder (2 or 3 chars -> 1 or 2 bytes)
            match vals.len() - j {
                2 => {
                    let a = vals[j];
                    let b = vals[j + 1];
                    out.push((a << 2) | (b >> 4));
                }
                3 => {
                    let a = vals[j];
                    let b = vals[j + 1];
                    let c = vals[j + 2];
                    out.push((a << 2) | (b >> 4));
                    out.push(((b & 15) << 4) | (c >> 2));
                }
                _ => {}
            }
            Ok(out)
        }
    }
}

/// Base64 decode; None on invalid input.
pub(crate) fn base64_decode(s: &str, url_safe: bool) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(s.len());
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' if !url_safe => 62,
            b'/' if !url_safe => 63,
            b'-' if url_safe => 62,
            b'_' if url_safe => 63,
            b'=' => 64, // padding marker
            _ if c.is_ascii_whitespace() => continue,
            _ => return None,
        };
        vals.push(v);
    }
    if vals.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(vals.len() / 4 * 3);
    let mut i = 0;
    while i < vals.len() {
        let a = vals[i];
        let b = vals[i + 1];
        let c = vals[i + 2];
        let d = vals[i + 3];
        if a == 64 || b == 64 {
            return None;
        }
        out.push((a << 2) | (b >> 4));
        if c != 64 {
            out.push(((b & 15) << 4) | (c >> 2));
        }
        if d != 64 {
            out.push(((c & 3) << 6) | d);
        }
        i += 4;
    }
    Some(out)
}

/// v0.24: base32hex encode (RFC 4648 base32hex alphabet
/// `0-9A-V`, PG 19's `encode(x, 'base32hex')`).
pub(crate) fn base32hex_encode(data: &[u8]) -> String {
    const ALPHA: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";
    let mut out = String::with_capacity((data.len() + 4) / 5 * 8);
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };
        let b3 = if i + 3 < data.len() { data[i + 3] } else { 0 };
        let b4 = if i + 4 < data.len() { data[i + 4] } else { 0 };
        // 5 bytes -> eight 5-bit groups.
        let g = [
            b0 >> 3,
            ((b0 & 7) << 2) | (b1 >> 6),
            (b1 >> 1) & 31,
            ((b1 & 1) << 4) | (b2 >> 4),
            ((b2 & 15) << 1) | (b3 >> 7),
            (b3 >> 2) & 31,
            ((b3 & 3) << 3) | (b4 >> 5),
            b4 & 31,
        ];
        // How many output chars are real (rest are '=').
        let nchars = match data.len() - i {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };
        for k in 0..8 {
            if k < nchars {
                out.push(ALPHA[g[k] as usize] as char);
            } else {
                out.push('=');
            }
        }
        i += 5;
    }
    out
}

/// v0.24: base32hex decode. PG 19 is lenient: case-insensitive, `=`
/// padding optional, trailing partial quanta emit
/// `floor(bits/8)` bytes, non-zero pad bits accepted. Errors on invalid
/// characters, `=` outside the trailing pad, or a padded quantum whose
/// data length isn't 2/4/5/7.
pub(crate) fn base32hex_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'A'..=b'V' => Some(c - b'A' + 10),
            b'a'..=b'v' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    // '=' may only appear as a trailing pad.
    let pad_start = bytes.iter().position(|&b| b == b'=').unwrap_or(bytes.len());
    if bytes[pad_start..].iter().any(|&b| b != b'=') {
        return None;
    }
    let data = &bytes[..pad_start];
    let npad = bytes.len() - pad_start;
    for &b in data {
        if val(b).is_none() {
            return None;
        }
    }
    if npad > 0 {
        // Padded: the data length mod 8 must be a valid partial quantum.
        // (data == 0 with padding, e.g. "=", is an error.)
        match data.len() % 8 {
            2 | 4 | 5 | 7 => {}
            _ => return None,
        }
    }
    // Decode 5-bit groups into bytes, MSB first.
    let mut out = Vec::with_capacity(data.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in data {
        acc = (acc << 5) | val(b).unwrap() as u32;
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Leftover bits (< 8) are dropped — PG emits nothing for them
    // (e.g. decode('2') -> empty) and accepts non-zero pad bits.
    let _ = acc;
    Some(out)
}

/// v0.24: interpret a text value as bytea input (PG's unknown-literal ->
/// bytea coercion): `\x...` hex, otherwise escape format.
pub(crate) fn text_to_bytea(s: &str) -> Option<Vec<u8>> {
    crate::storage::parse_bytea(s).ok()
}

/// v0.33: `encode(bytea, 'escape')` — PG's observed output format from
/// CI-validated regression expected files (strings.out, PG 15/16/17/19):
/// 0x00 as `\000` octal, 0x01-0x1F as `\xNN`, 0x80-0xFF as octal,
/// backslash as `\\`, other printable ASCII as-is.
/// (Note: this differs from PG's `byteaout` display function, which uses
/// octal for all non-printables; `encode()` is a separate code path.)
pub(crate) fn bytea_encode_escape(data: &[u8]) -> String {
    let mut out = String::new();
    for &b in data {
        if b == b'\\' {
            out.push_str("\\\\");
        } else if b >= 0x20 && b < 0x7f {
            out.push(b as char);
        } else if b >= 0x01 && b < 0x20 {
            out.push_str(&format!("\\x{:02x}", b));
        } else {
            // 0x00 and 0x80-0xFF: octal
            out.push_str(&format!("\\{:03o}", b));
        }
    }
    out
}

/// v0.31: Port of PostgreSQL 19's `similar_escape_internal()` (from
/// `src/backend/utils/adt/regexp.c`, REL_19_STABLE).
///
/// Convert a SQL SIMILAR TO pattern to a POSIX-style regular expression.
/// `escape` is the ESCAPE character, or `None` for "no escape character"
/// (i.e. `ESCAPE ''`, which PG allows).
///
/// Returns the translated pattern including PG's `^(?:...)$` wrapper (and,
/// for SUBSTRING, the escape-double-quote part separators, yielding
/// `^(?:part1){1,1}?(part2){1,1}(?:part3)$`). Errors only when the pattern
/// contains more than two escape-double-quote separators.
pub(crate) fn similar_to_regex(pat: &str, escape: Option<char>) -> Result<String, String> {
    let mut out = String::with_capacity(pat.len() * 3 + 24);
    out.push_str("^(?:");

    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let mut afterescape = false;
    let mut nquotes = 0;
    let mut bracket_depth: i32 = 0; // square bracket nesting level
    let mut charclass_pos: i32 = 0; // position inside a character class

    while i < chars.len() {
        let pchar = chars[i];

        if afterescape {
            if pchar == '"' && bracket_depth < 1 {
                // escape-double-quote: emit the part separator for SUBSTRING.
                if nquotes == 0 {
                    out.push_str("){1,1}?(");
                } else if nquotes == 1 {
                    out.push_str("){1,1}(?:");
                } else {
                    return Err("SQL regular expression may not contain more than two escape-double-quote separators".to_string());
                }
                nquotes += 1;
            } else {
                // PG allows any character at all to be escaped; this also
                // gives access to POSIX class escapes such as `\d`.
                out.push('\\');
                out.push(pchar);
                // An escaped character inside a class ends the "beginning".
                charclass_pos = 3;
            }
            afterescape = false;
        } else if escape == Some(pchar) {
            // SQL escape character; do not send to output.
            afterescape = true;
        } else if bracket_depth > 0 {
            // Inside a character class: copy as-is (so `%`, `_`, `(`, `)`,
            // etc. stay literal), doubling backslashes.
            if pchar == '\\' {
                out.push('\\');
            }
            out.push(pchar);
            // Parse just enough to find the real end of the class.
            if pchar == ']' && charclass_pos > 2 {
                bracket_depth -= 1;
            } else if pchar == '[' {
                bracket_depth += 1;
                charclass_pos = 3;
            } else if pchar == '^' {
                charclass_pos += 1;
            } else {
                charclass_pos = 3;
            }
        } else if pchar == '[' {
            out.push(pchar);
            bracket_depth = 1;
            charclass_pos = 1;
        } else if pchar == '%' {
            out.push_str(".*");
        } else if pchar == '_' {
            out.push('.');
        } else if pchar == '(' {
            // Convert to non-capturing parenthesis.
            out.push_str("(?:");
        } else if pchar == '\\' || pchar == '.' || pchar == '^' || pchar == '$' {
            out.push('\\');
            out.push(pchar);
        } else {
            out.push(pchar);
        }
        i += 1;
    }

    out.push_str(")$");
    Ok(out)
}

/// Get an optional integer argument with a default value.
pub(crate) fn get_int_arg(
    fname: &str,
    vals: &[Value],
    idx: usize,
    default: i64,
) -> Result<i64, ExecError> {
    if idx >= vals.len() {
        return Ok(default);
    }
    match int_arg(fname, &vals[idx])? {
        None => Err(exec_err("22004", "null argument not allowed here")),
        Some(n) => Ok(n),
    }
}

/// Get an optional string argument with a default value.
pub(crate) fn get_str_arg<'a>(
    fname: &str,
    vals: &'a [Value],
    idx: usize,
    default: &'a str,
) -> Result<&'a str, ExecError> {
    if idx >= vals.len() {
        return Ok(default);
    }
    match str_arg(fname, &vals[idx])? {
        None => Err(exec_err("22004", "null argument not allowed here")),
        Some(s) => Ok(s),
    }
}

/// Expand `\1`-`\9` and `\&` in a regexp_replace replacement string.
/// `\\` is a literal backslash.
pub(crate) fn expand_replacement(repl: &str, s: &[char], caps: &crate::regex::Captures) -> String {
    let mut out = String::new();
    let b = repl.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            let c = repl[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        i += 1;
        if i >= b.len() {
            out.push('\\');
            break;
        }
        match b[i] {
            b'\\' => {
                out.push('\\');
                i += 1;
            }
            b'&' => {
                // Whole match.
                if let Some((ms, me)) = caps.groups[0] {
                    out.extend(s[ms..me].iter());
                }
                i += 1;
            }
            b'1'..=b'9' => {
                // v0.38: PG19 only treats \1..\9 as back-references
                // (varlena.c: `*p >= '1' && *p <= '9'`); \0 is an unknown
                // escape and keeps its backslash.
                let g = (b[i] - b'0') as usize;
                if let Some(Some((gs, ge))) = caps.groups.get(g) {
                    out.extend(s[*gs..*ge].iter());
                }
                i += 1;
            }
            _ => {
                // v0.24: unknown escape keeps the backslash (PG:
                // `X\Y\1Z\` -> `X\YoZ\`).
                out.push('\\');
                let c = repl[i..].chars().next().unwrap();
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// v0.45: `format(fmt, args...)` per PG19 `text_format()` in
/// `src/backend/utils/adt/varlena.c`. Printf-style formatting with `%s`
/// (string), `%I` (SQL identifier), `%L` (SQL literal), `%%` (percent),
/// positional `%n$`, `-` flag, and constant or `*`-indirect widths.
/// NULL fmt -> NULL. NULL with `%s` -> empty; with `%L` -> `NULL`;
/// with `%I` -> 22004 error. Too few args, bad specifiers, and
/// argument 0 all raise 22023, like PG.
pub(crate) fn pg_format_text(vals: &[Value]) -> Result<Value, ExecError> {
    // vals[0] is the format string; args are 1-based from vals[1..].
    let fmt = match &vals[0] {
        Value::Null => return Ok(Value::Null),
        Value::Text(s) => s.to_string(),
        Value::BpChar(s) => s.to_string(),
        // PG stringifies non-text fmt via the type output function.
        v => v.to_text().unwrap_or_default(),
    };
    let nargs = vals.len() - 1; // number of format arguments
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut arg: usize = 1; // next argument position (1-based)
    let mut i = 0;
    // Stringify a value like PG19's type output function
    // (varlena.c text_format_string_conversion calls OutputFunctionCall
    // for %s/%I/%L). v1.24: the old Bool special-case rendering
    // true/false was wrong — boolout renders t/f, and Value::to_text()
    // already does that.
    let stringify = |v: &Value| -> Option<String> { v.to_text() };
    // Fetch the 1-based argument, or raise "too few arguments".
    let get_arg = |pos: usize| -> Result<&Value, ExecError> {
        if pos < 1 || pos > nargs {
            return Err(exec_err("22023", "too few arguments for format()"));
        }
        Ok(&vals[pos])
    };
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            return Err(exec_err("22023", "unterminated format() type specifier"));
        }
        // Easy case: %% outputs a single %.
        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        // Parse [argpos$][flags][width] — the type char is not consumed here.
        let mut argpos: i64 = -1;
        let mut widthpos: i64 = -1;
        let mut minus_flag = false;
        let mut width: i64 = 0;
        // Try to identify the first number: argpos (n$) or width (n).
        let mut n: i64 = 0;
        let mut has_digits = false;
        while i < chars.len() && chars[i].is_ascii_digit() {
            has_digits = true;
            n = n
                .saturating_mul(10)
                .saturating_add((chars[i] as i64) - ('0' as i64));
            i += 1;
        }
        let mut width_done = false;
        if has_digits {
            if i < chars.len() && chars[i] == '$' {
                if n == 0 {
                    return Err(exec_err(
                        "22023",
                        "format specifies argument 0, but arguments are numbered from 1",
                    ));
                }
                argpos = n;
                i += 1;
            } else {
                width = n;
                width_done = true;
            }
        }
        if !width_done {
            // Flags (only '-' supported).
            while i < chars.len() && chars[i] == '-' {
                minus_flag = true;
                i += 1;
            }
            // Width: '*' (indirect) or digits (direct).
            if i < chars.len() && chars[i] == '*' {
                i += 1;
                // Optional n$ after '*'.
                let mut wn: i64 = 0;
                let mut whas = false;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    whas = true;
                    wn = wn
                        .saturating_mul(10)
                        .saturating_add((chars[i] as i64) - ('0' as i64));
                    i += 1;
                }
                if whas {
                    if i >= chars.len() || chars[i] != '$' {
                        return Err(exec_err(
                            "22023",
                            "width argument position must be ended by \"$\"",
                        ));
                    }
                    if wn == 0 {
                        return Err(exec_err(
                            "22023",
                            "format specifies argument 0, but arguments are numbered from 1",
                        ));
                    }
                    widthpos = wn;
                    i += 1;
                } else {
                    widthpos = 0; // take next arg as width
                }
            } else {
                let mut wn: i64 = 0;
                let mut whas = false;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    whas = true;
                    wn = wn
                        .saturating_mul(10)
                        .saturating_add((chars[i] as i64) - ('0' as i64));
                    i += 1;
                }
                if whas {
                    width = wn;
                }
            }
        }
        if i >= chars.len() {
            return Err(exec_err("22023", "unterminated format() type specifier"));
        }
        let conv = chars[i];
        i += 1;
        if !matches!(conv, 's' | 'I' | 'L') {
            return Err(exec_err(
                "22023",
                format!("unrecognized format() type specifier \"{conv}\""),
            ));
        }
        // If indirect width was specified, get its value.
        if widthpos >= 0 {
            if widthpos > 0 {
                arg = widthpos as usize;
            }
            let wval = get_arg(arg)?;
            arg += 1;
            width = match wval {
                Value::Null => 0,
                Value::SmallInt(x) => *x as i64,
                Value::Int(x) => *x,
                Value::BigInt(x) => *x,
                // PG converts other types via output then strtoint32.
                v => stringify(v).unwrap_or_default().parse::<i64>().unwrap_or(0),
            };
        }
        // Collect the specified or next argument position.
        if argpos > 0 {
            arg = argpos as usize;
        }
        let val = get_arg(arg)?;
        arg += 1;
        // Format the value.
        let mut s = match conv {
            's' => stringify(val).unwrap_or_default(),
            'I' => match stringify(val) {
                None => {
                    return Err(exec_err(
                        "22004",
                        "null values cannot be formatted as an SQL identifier",
                    ));
                }
                Some(st) => pg_quote_identifier(&st),
            },
            'L' => match stringify(val) {
                None => "NULL".to_string(),
                Some(st) => pg_quote_literal(&st),
            },
            _ => unreachable!(),
        };
        // Apply width padding (character count, like PG's pg_mbstrlen).
        if width != 0 {
            let len = s.chars().count() as i64;
            let (left, w) = if width < 0 {
                (true, width.saturating_abs())
            } else {
                (minus_flag, width)
            };
            if len < w {
                let pad = " ".repeat((w - len) as usize);
                if left {
                    s.push_str(&pad);
                } else {
                    s = pad + &s;
                }
            }
        }
        out.push_str(&s);
    }
    Ok(Value::text(out))
}

/// Quote a string as an SQL identifier, like PG's quote_identifier:
/// bare if it matches [a-z_][a-z0-9_$]*, else double-quoted with
/// embedded quotes doubled.
pub(crate) fn pg_quote_identifier(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .next()
            .map_or(false, |c| c == '_' || c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit() || c == '$');
    if bare {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// Quote a string as an SQL literal, like PG's quote_literal_cstr:
/// single-quoted with embedded quotes doubled; E'' syntax if the
/// string contains backslashes.
pub(crate) fn pg_quote_literal(s: &str) -> String {
    // v1.08: PG EXPLAIN uses standard_conforming_strings=on; backslashes
    // are literal in regular '...' strings (no E'' syntax in output).
    format!("'{}'", s.replace('\'', "''"))
}

/// Port of PostgreSQL 19 `replace_text_regexp`
/// (src/backend/utils/adt/varlena.c, REL_19_STABLE).
///
/// `search_start0` is the 0-based char offset where matching begins (text
/// before it is copied verbatim). `n == 0` replaces all matches from there;
/// `n > 0` replaces only the n'th match. The copy cursor (`data_pos`) only
/// advances over replaced/copied text, while the search cursor
/// (`search_start`) advances past every match -- a zero-width match advances
/// the search by one char without discarding source text.
pub(crate) fn pg_replace_text_regexp(
    sc: &[char],
    re: &crate::regex::Compiled,
    repl: &str,
    search_start0: usize,
    n: i64,
) -> String {
    let mut out = String::new();
    out.extend(sc[..search_start0].iter());
    let mut data_pos = search_start0;
    let mut search_start = search_start0;
    let mut match_no: i64 = 0;
    let replace_all = n == 0;
    while search_start <= sc.len() {
        match re.find_at(sc, search_start) {
            Some((ms, me, caps)) => {
                match_no += 1;
                if replace_all || match_no == n {
                    out.extend(sc[data_pos..ms].iter());
                    out.push_str(&expand_replacement(repl, sc, &caps));
                    data_pos = me;
                }
                // Advance the search past this match; a zero-width match
                // consumes one char of search space but no source text.
                search_start = if me > ms { me } else { ms + 1 };
                if !replace_all && match_no >= n {
                    break;
                }
            }
            None => break,
        }
    }
    out.extend(sc[data_pos.min(sc.len())..].iter());
    out
}

/// Decode `\uXXXX` and `\UXXXXXXXX` escapes (unistr and U&'' literals).
/// `\\` produces a literal backslash. Returns None on invalid escapes.
pub(crate) fn unistr_decode(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 1;
            if i >= b.len() {
                return None;
            }
            if b[i] == b'\\' {
                out.push('\\');
                i += 1;
            } else if b[i] == b'u' || b[i] == b'U' {
                let digits = if b[i] == b'u' { 4 } else { 8 };
                i += 1;
                if i + digits > b.len() {
                    return None;
                }
                let hex = &s[i..i + digits];
                let cp = u32::from_str_radix(hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
                i += digits;
            } else if b[i] == b'+' {
                // v0.34: PG unistr \+XXXXXX (6 hex digits).
                i += 1;
                if i + 6 > b.len() {
                    return None;
                }
                let hex = &s[i..i + 6];
                let cp = u32::from_str_radix(hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
                i += 6;
            } else if (b[i] as char).is_ascii_hexdigit() {
                // v0.34: PG unistr \XXXX (4 hex digits, no 'u').
                if i + 4 > b.len() {
                    return None;
                }
                let hex = &s[i..i + 4];
                let cp = u32::from_str_radix(hex, 16).ok()?;
                out.push(char::from_u32(cp)?);
                i += 4;
            } else {
                return None;
            }
        } else {
            // Copy one UTF-8 char.
            let c = s[i..].chars().next()?;
            out.push(c);
            i += c.len_utf8();
        }
    }
    Some(out)
}

/// Parse PG bytea `escape` format; None on invalid input.
pub(crate) fn bytea_unescape(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += 1;
            if i >= b.len() {
                return None;
            }
            if b[i] == b'\\' {
                out.push(b'\\');
                i += 1;
            } else if b[i] == b'x'
                && i + 2 < b.len()
                && (b[i + 1] as char).is_digit(16)
                && (b[i + 2] as char).is_digit(16)
            {
                // v0.33: \xNN hex escape (emitted by bytea_encode_escape).
                let hi = (b[i + 1] as char).to_digit(16).unwrap();
                let lo = (b[i + 2] as char).to_digit(16).unwrap();
                out.push((hi * 16 + lo) as u8);
                i += 3;
            } else if i + 2 < b.len()
                && (b[i] as char).is_digit(8)
                && (b[i + 1] as char).is_digit(8)
                && (b[i + 2] as char).is_digit(8)
            {
                let v = ((b[i] - b'0') as u32) * 64
                    + ((b[i + 1] - b'0') as u32) * 8
                    + ((b[i + 2] - b'0') as u32);
                out.push(v as u8);
                i += 3;
            } else {
                return None;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Shared `power(a, b)` / `a ^ b` implementation. `bad` builds the
/// type-mismatch error (42883), which differs between the function and
/// operator spellings.

// ---------------------------------------------------------------------------
// v0.61: PG19 `numeric_is_integral` plus the int32 range check that
// gates the `power_var_int` dispatch: the exponent must be integral
// (no fractional digits) and fit in int32, else the exponent falls
// through to the f64 transcendental path.
pub(crate) fn numeric_integral_i32(n: &crate::storage::Numeric) -> Option<i32> {
    if n.special != crate::storage::NumericSpecial::Finite {
        return None;
    }
    // v0.63: a big-mantissa value never fits i32 — with scale > 0 it is
    // never integral (normalized: no trailing zeros), and with
    // scale <= 0 its magnitude exceeds 10^38.
    if n.is_big() {
        return None;
    }
    let v = if n.scale <= 0 {
        // value = unscaled * 10^-scale; integral but may not fit i32.
        let mul = 10i128.checked_pow((-n.scale) as u32)?;
        n.unscaled.checked_mul(mul)?
    } else if n.unscaled == 0 {
        0
    } else {
        // scale > 38: 10^scale exceeds i128, so |unscaled| < 10^scale
        // and a non-zero unscaled is fractional.
        let p = 10i128.checked_pow(n.scale as u32)?;
        if n.unscaled % p != 0 {
            return None;
        }
        n.unscaled / p
    };
    i32::try_from(v).ok()
}

pub(crate) fn eval_power_op(
    a: &Value,
    b: &Value,
    bad: impl Fn(&Value) -> ExecError,
) -> Result<Value, ExecError> {
    if matches!(a, Value::Float4(_) | Value::Float(_))
        || matches!(b, Value::Float4(_) | Value::Float(_))
    {
        // v0.21: PG dpow domain checks on the float path: 0^-inf and
        // negative-base-to-fractional-exponent are 2201F, 0 to a
        // negative power is 22012, and overflow is 22003.
        let x = to_f64v(a);
        let y = to_f64v(b);
        if x == 0.0 && y == f64::NEG_INFINITY {
            return Err(exec_err(
                "2201F",
                "zero to the power of negative infinity is undefined",
            ));
        }
        if x == 0.0 && y < 0.0 {
            return Err(exec_err("22012", "division by zero"));
        }
        if x < 0.0 && y.is_finite() && y.fract() != 0.0 {
            return Err(exec_err(
                "2201F",
                "a negative number raised to a non-integer power yields a non-real result",
            ));
        }
        let r = x.powf(y);
        if r.is_infinite() && x.is_finite() && y.is_finite() {
            return Err(exec_err("22003", "value out of range: overflow"));
        }
        return Ok(Value::Float(r));
    }
    let base = to_numeric_opt(a).ok_or_else(|| bad(a))?;
    let exp = to_numeric_opt(b).ok_or_else(|| bad(b))?;
    // v0.18: handle special values in power before integer-exponent path.
    // PG: power('inf','-2')=0, power('-inf','3')=-Inf, power('-1','inf')=1,
    //     power('-2','inf')=Inf, power('inf','inf')=Inf, etc.
    use crate::storage::NumericSpecial;
    match (base.special, exp.special) {
        // v0.61: PG19 follows the POSIX pow(3) spec here: NaN ^ 0 = 1
        // and 1 ^ NaN = 1 (the base compares equal to one, so 1.00
        // counts); every other NaN combination yields NaN.
        (NumericSpecial::NaN, NumericSpecial::Finite) if exp.unscaled == 0 => {
            return Ok(Value::Numeric(crate::storage::Numeric::from_i64(1)));
        }
        (NumericSpecial::Finite, NumericSpecial::NaN) => {
            let is_one = base.scale >= 0
                && 10i128
                    .checked_pow(base.scale as u32)
                    .is_some_and(|p| base.unscaled == p);
            if is_one {
                return Ok(Value::Numeric(crate::storage::Numeric::from_i64(1)));
            }
            return Ok(Value::Numeric(crate::storage::Numeric::nan()));
        }
        (NumericSpecial::NaN, _) | (_, NumericSpecial::NaN) => {
            return Ok(Value::Numeric(crate::storage::Numeric::nan()));
        }
        (NumericSpecial::PosInf, NumericSpecial::PosInf) => {
            return Ok(Value::Numeric(crate::storage::Numeric::infinity()));
        }
        (NumericSpecial::PosInf, NumericSpecial::NegInf) => {
            return Ok(Value::Numeric(crate::storage::Numeric::from_i64(0)));
        }
        (NumericSpecial::NegInf, NumericSpecial::PosInf) => {
            return Ok(Value::Numeric(crate::storage::Numeric::infinity()));
        }
        (NumericSpecial::NegInf, NumericSpecial::NegInf) => {
            return Ok(Value::Numeric(crate::storage::Numeric::from_i64(0)));
        }
        (NumericSpecial::PosInf, NumericSpecial::Finite) => {
            // inf ^ x: x>0 -> inf, x<0 -> 0, x=0 -> 1
            if exp.unscaled == 0 {
                return Ok(Value::Numeric(crate::storage::Numeric::from_i64(1)));
            } else if exp.unscaled > 0 {
                return Ok(Value::Numeric(crate::storage::Numeric::infinity()));
            } else {
                return Ok(Value::Numeric(crate::storage::Numeric::from_i64(0)));
            }
        }
        (NumericSpecial::NegInf, NumericSpecial::Finite) => {
            // (-inf) ^ x: needs integer check for sign
            if exp.unscaled == 0 {
                return Ok(Value::Numeric(crate::storage::Numeric::from_i64(1)));
            }
            // v0.63: big exponents use magnitude parity (`unscaled` is
            // sign-only there); small exponents keep the exact v0.62
            // i64 gate.
            if exp.is_big() {
                match crate::storage::integral_parity(&exp) {
                    Some(odd) => {
                        if exp.unscaled > 0 {
                            return Ok(Value::Numeric(if odd {
                                crate::storage::Numeric::neg_infinity()
                            } else {
                                crate::storage::Numeric::infinity()
                            }));
                        }
                        return Ok(Value::Numeric(crate::storage::Numeric::from_i64(0)));
                    }
                    None => {
                        return Err(exec_err(
                            "2201F",
                            "a negative number raised to a non-integer power yields a non-real result",
                        ));
                    }
                }
            }
            // For non-integer, PG errors; for integer, sign depends on parity
            if exp.scale == 0 {
                if let Ok(e) = i64::try_from(exp.unscaled) {
                    if e > 0 {
                        if e % 2 == 0 {
                            return Ok(Value::Numeric(crate::storage::Numeric::infinity()));
                        } else {
                            return Ok(Value::Numeric(crate::storage::Numeric::neg_infinity()));
                        }
                    } else {
                        return Ok(Value::Numeric(crate::storage::Numeric::from_i64(0)));
                    }
                }
            }
            return Err(exec_err(
                "2201F",
                "a negative number raised to a non-integer power yields a non-real result",
            ));
        }
        (NumericSpecial::Finite, NumericSpecial::PosInf) => {
            // x ^ inf: |x|>1 -> inf, |x|<1 -> 0, |x|=1 -> 1, x=0 -> 1 (PG: 0^inf=0? actually 0^inf=0)
            // PG: power('-1','inf')=1, power('-2','inf')=Inf
            let abs_base = base.abs();
            let one = crate::storage::Numeric::from_i64(1);
            if abs_base == one {
                return Ok(Value::Numeric(crate::storage::Numeric::from_i64(1)));
            }
            let zero = crate::storage::Numeric::from_i64(0);
            if base == zero {
                return Ok(Value::Numeric(zero));
            }
            if abs_base > one {
                return Ok(Value::Numeric(crate::storage::Numeric::infinity()));
            } else {
                return Ok(Value::Numeric(crate::storage::Numeric::from_i64(0)));
            }
        }
        (NumericSpecial::Finite, NumericSpecial::NegInf) => {
            // x ^ -inf: |x|>1 -> 0, |x|<1 -> inf, |x|=1 -> 1.
            // v0.61: PG19 raises 2201F "zero raised to a negative power
            // is undefined" for 0 ^ -inf (checked before the inf rules).
            let abs_base = base.abs();
            let one = crate::storage::Numeric::from_i64(1);
            if abs_base == one {
                return Ok(Value::Numeric(crate::storage::Numeric::from_i64(1)));
            }
            let zero = crate::storage::Numeric::from_i64(0);
            if base == zero {
                return Err(exec_err(
                    "2201F",
                    "zero raised to a negative power is undefined",
                ));
            }
            if abs_base > one {
                return Ok(Value::Numeric(crate::storage::Numeric::from_i64(0)));
            } else {
                return Ok(Value::Numeric(crate::storage::Numeric::infinity()));
            }
        }
        (NumericSpecial::Finite, NumericSpecial::Finite) => {}
    }
    // v0.61: PG19 dispatches an integral exponent that fits int32 to
    // power_var_int (exact, adaptive precision); v0.62 routes anything
    // else through power_var_frac (exp(exp * ln(|base|))). PG checks
    // the SQL-level 2201F "zero raised to a negative power" here,
    // before power_var.
    if base.unscaled == 0 && exp.unscaled < 0 {
        return Err(exec_err(
            "2201F",
            "zero raised to a negative power is undefined",
        ));
    }
    if let Some(e) = numeric_integral_i32(&exp) {
        match base.power_int(e as i64, exp.dscale) {
            Ok(n) => return Ok(Value::Numeric(n)),
            Err(crate::storage::PowerError::Overflow) => {
                return Err(exec_err("22003", "value overflows numeric format"));
            }
        }
    }
    // v0.62: PG19 power_var for non-int32 exponents —
    // exp(exp * ln(|base|)) with adaptive precision. 2201F for a
    // negative base to a non-integer exponent; 22003 when the true
    // result cannot be represented.
    match crate::storage::power_var_frac(&base, &exp) {
        Ok(n) => Ok(Value::Numeric(n)),
        Err(crate::storage::PowerFracError::NegativeBase) => Err(exec_err(
            "2201F",
            "a negative number raised to a non-integer power yields a complex result",
        )),
        Err(crate::storage::PowerFracError::Overflow) => {
            Err(exec_err("22003", "value overflows numeric format"))
        }
    }
}

// ---------------------------------------------------------------------------
// v0.21: float8 transcendental functions (Postgres float.c semantics)
// ---------------------------------------------------------------------------

/// v0.21: argument fetch for the float8 math functions. Float4/Float go
/// straight through; other numeric-kinds are coerced via f64 (a
/// documented extension — Postgres would dispatch those to numeric
/// overloads, which rustgres does not implement). Anything else is
/// 42883.
pub(crate) fn float_math_arg(name: &str, v: &Value) -> Result<f64, ExecError> {
    if num_cat(v).is_none() {
        return Err(func_arg_err(name, v));
    }
    Ok(to_f64v(v))
}

/// v0.21: like PG's float.c — a ±Infinity input is 22003 "input is out
/// of range"; NaN flows into the f64 op and comes back NaN.
pub(crate) fn reject_inf_input(x: f64) -> Result<f64, ExecError> {
    if x.is_infinite() {
        return Err(exec_err("22003", "input is out of range"));
    }
    Ok(x)
}

/// v0.21: erf(x). Maclaurin series for |x| <= 1, `1 - erfc(x)` beyond.
pub(crate) fn erf_impl(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x.is_infinite() {
        return x.signum();
    }
    if x.abs() > 1.0 {
        return 1.0 - erfc_impl(x);
    }
    // erf(x) = 2/sqrt(pi) * sum (-1)^n x^(2n+1) / (n! (2n+1)).
    let xsq = x * x;
    let mut term = x;
    let mut sum = x;
    for n in 1..200u32 {
        term *= -xsq * f64::from(2 * n - 1) / (f64::from(n) * f64::from(2 * n + 1));
        sum += term;
        if term.abs() < 1e-17 * sum.abs() {
            break;
        }
    }
    sum * 2.0 / std::f64::consts::PI.sqrt()
}

/// v0.21: erfc(x). Laplace continued fraction (Lentz's algorithm) for
/// x >= 0, `2 - erfc(-x)` for x < 0; tiny results underflow to 0.
pub(crate) fn erfc_impl(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x.is_infinite() {
        return if x > 0.0 { 0.0 } else { 2.0 };
    }
    if x < 0.0 {
        return 2.0 - erfc_impl(-x);
    }
    if x == 0.0 {
        return 1.0;
    }
    // v0.21: the Laplace continued fraction below is designed for
    // large x and loses accuracy for small |x|; use 1 - erf(x) there.
    if x <= 1.0 {
        return 1.0 - erf_impl(x);
    }
    // erfc(x) = x*e^(-x^2)/sqrt(pi) / L, where L is
    // x^2 + (1/2)/(1 + 1/(x^2 + (3/2)/(1 + 2/(x^2 + ...)))).
    let tiny = 1e-300f64;
    let x2 = x * x;
    let mut f = if x2 == 0.0 { tiny } else { x2 };
    let mut c = f;
    let mut d = 0.0f64;
    for j in 1..500u32 {
        let a = f64::from(j) / 2.0;
        let b = if j % 2 == 1 { 1.0 } else { x2 };
        d = b + a * d;
        if d == 0.0 {
            d = tiny;
        }
        c = b + a / c;
        if c == 0.0 {
            c = tiny;
        }
        d = 1.0 / d;
        let cd = c * d;
        f *= cd;
        if (cd - 1.0).abs() < 1e-15 {
            break;
        }
    }
    x * (-x2).exp() / std::f64::consts::PI.sqrt() / f
}

/// v0.21: Lanczos approximation (g=7, 9 coefficients) for gamma.
pub(crate) const LANCZOS_GAMMA: [f64; 9] = [
    0.99999999999980993,
    676.5203681218851,
    -1259.1392167224028,
    771.32342877765313,
    -176.61502916214059,
    12.507343278686905,
    -0.13857109526572012,
    9.9843695780195716e-6,
    1.5056327351493116e-7,
];

/// Log-gamma for x > 0 via Lanczos in log space (no intermediate
/// overflow). For x < 0.5 the reflection formula is used instead —
/// the direct form divides by (z+1), which rounds to 0 for tiny x.
pub(crate) fn lanczos_lgamma(x: f64) -> f64 {
    let z = x - 1.0;
    let mut ag = LANCZOS_GAMMA[0];
    for (k, c) in LANCZOS_GAMMA.iter().enumerate().skip(1) {
        ag += c / (z + k as f64);
    }
    let t = z + 7.0 + 0.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (z + 0.5) * t.ln() - t + ag.ln()
}

/// v0.21: lgamma(x) = ln|gamma(x)|. Poles (0 and negative integers)
/// are 22003; lgamma(±inf) = +inf.
pub(crate) fn lgamma_impl(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Ok(f64::INFINITY);
    }
    if x == 0.0 || (x < 0.0 && x.fract() == 0.0) {
        return Err(exec_err(
            "22003",
            "lgamma pole: logarithm of gamma at a non-positive integer is undefined",
        ));
    }
    // v0.21: exact path for positive integers: lgamma(n) = ln((n-1)!).
    // This avoids Lanczos rounding (e.g. lgamma(1) = -8.8e-16).
    if x > 0.0 && x.fract() == 0.0 && x <= 171.0 {
        let n = x as u64;
        let mut log_fact = 0.0f64;
        for k in 2..n {
            log_fact += (k as f64).ln();
        }
        return Ok(log_fact);
    }
    let r = if x < 0.5 {
        // Reflection in log space:
        // ln|G(x)| = ln pi - ln|sin(pi x)| - lnG(1-x).
        let s = (std::f64::consts::PI * x).sin().abs();
        if s == 0.0 {
            return Err(exec_err("22003", "value out of range: overflow"));
        }
        std::f64::consts::PI.ln() - s.ln() - lanczos_lgamma(1.0 - x)
    } else {
        lanczos_lgamma(x)
    };
    // v0.21: lgamma(1e308) overflows float8 -> 22003 (PG behavior).
    if r.is_infinite() {
        return Err(exec_err("22003", "value out of range: overflow"));
    }
    Ok(r)
}

/// v0.21: gamma(x). Exact (n-1)! path for positive integers,
/// log-space Lanczos + reflection otherwise; poles, -inf, overflow
/// and underflow are 22003.
pub(crate) fn gamma_impl(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x == f64::INFINITY {
        return Ok(f64::INFINITY);
    }
    if x == f64::NEG_INFINITY {
        return Err(exec_err("22003", "gamma of negative infinity is undefined"));
    }
    if x == 0.0 || (x < 0.0 && x.fract() == 0.0) {
        return Err(exec_err(
            "22003",
            "gamma pole: gamma of a non-positive integer is undefined",
        ));
    }
    if x > 0.0 && x.fract() == 0.0 {
        // Exact positive-integer path: gamma(n) = (n-1)!.
        if x > 171.0 {
            return Err(exec_err("22003", "value out of range: overflow"));
        }
        let mut f = 1.0f64;
        for k in 2..x as u64 {
            f *= k as f64;
        }
        return Ok(f);
    }
    // Sign of gamma(x): positive for x > 0; for x < 0 it is the sign of
    // sin(pi x), since gamma(1-x) > 0 (x is not a pole here).
    let sign = if x > 0.0 {
        1.0
    } else {
        (std::f64::consts::PI * x).sin().signum()
    };
    let r = sign * lgamma_impl(x)?.exp();
    if r.is_infinite() {
        return Err(exec_err("22003", "value out of range: overflow"));
    }
    if r == 0.0 {
        return Err(exec_err("22003", "value out of range: underflow"));
    }
    Ok(r)
}

/// v0.21: degrees per radian-scale factor for the degree trig below.
pub(crate) const DEG_TO_RAD: f64 = std::f64::consts::PI / 180.0;

/// sin of `a` degrees for `a` in [0, 90]; exact at 30° (= 1/2).
pub(crate) fn sin_q(a: f64) -> f64 {
    if a == 30.0 {
        0.5
    } else {
        (a * DEG_TO_RAD).sin()
    }
}

/// cos of `a` degrees for `a` in [0, 90]; exact at 60° (= 1/2).
pub(crate) fn cos_q(a: f64) -> f64 {
    if a == 60.0 {
        0.5
    } else {
        (a * DEG_TO_RAD).cos()
    }
}

/// v0.21: sind — sine of degrees with quadrant reduction, so cardinal
/// angles are exact (sind(30)=0.5, sind(90)=1). NaN/Inf are rejected
/// by the caller.
pub(crate) fn sind_impl(x: f64) -> f64 {
    let r = x.rem_euclid(360.0);
    let q = (r / 90.0) as i32; // 0..=3
    let a = r - f64::from(q) * 90.0; // [0, 90)
    let v = match q {
        0 => sin_q(a),
        1 => cos_q(a),
        2 => -sin_q(a),
        _ => -cos_q(a),
    };
    if v == 0.0 { 0.0 } else { v } // normalize -0.0
}

/// v0.21: cosd(x) = sind(x + 90); inherits the exactness.
pub(crate) fn cosd_impl(x: f64) -> f64 {
    sind_impl(x + 90.0)
}

/// v0.21: tand — quadrant reduction with exact 45° (= ±1) and exact
/// infinities at the odd quadrants (tand(90)=Infinity).
pub(crate) fn tand_impl(x: f64) -> f64 {
    let r = x.rem_euclid(360.0);
    let q = (r / 90.0) as i32; // 0..=3
    let a = r - f64::from(q) * 90.0; // [0, 90)
    if a == 0.0 {
        return match q {
            0 | 2 => 0.0,
            1 => f64::INFINITY,
            _ => f64::NEG_INFINITY,
        };
    }
    if a == 45.0 {
        return match q {
            0 | 2 => 1.0,
            _ => -1.0,
        };
    }
    let t = (a * DEG_TO_RAD).tan();
    match q {
        0 | 2 => t,
        _ => -1.0 / t,
    }
}

/// v0.21: cotd(x) = tand(90 - x); exact at the cardinals.
pub(crate) fn cotd_impl(x: f64) -> f64 {
    tand_impl(90.0 - x)
}

/// v0.21: asind — degrees(asin(x)) with exactness at the nice values
/// (asind(0.5)=30). Domain is checked by the caller.
pub(crate) fn asind_impl(x: f64) -> f64 {
    if x == 0.5 {
        return 30.0;
    }
    if x == -0.5 {
        return -30.0;
    }
    if x == 1.0 {
        return 90.0;
    }
    if x == -1.0 {
        return -90.0;
    }
    x.asin().to_degrees()
}

/// v0.21: acosd — degrees(acos(x)) with exactness at the nice values
/// (acosd(0.5)=60). Domain is checked by the caller.
pub(crate) fn acosd_impl(x: f64) -> f64 {
    if x == 0.5 {
        return 60.0;
    }
    if x == -0.5 {
        return 120.0;
    }
    if x == 1.0 {
        return 0.0;
    }
    if x == -1.0 {
        return 180.0;
    }
    x.acos().to_degrees()
}

/// v0.21: atand — degrees(atan(x)); atand(±1) = ±45.
pub(crate) fn atand_impl(x: f64) -> f64 {
    if x == 1.0 {
        return 45.0;
    }
    if x == -1.0 {
        return -45.0;
    }
    x.atan().to_degrees()
}

pub(crate) fn eval_math_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    // v0.18: zero-argument functions must be handled before indexing vals[0].
    match name {
        "pi" => {
            // PG: pi() = 3.14159265358979323846...
            // (Parse is infallible for this literal; error is unreachable.)
            return match Numeric::parse("3.14159265358979323846264338327950288") {
                Ok(pi) => Ok(Value::Numeric(pi)),
                Err(_) => Err(exec_err("22003", "value overflows numeric format")),
            };
        }
        "random" => {
            // v0.18: random() -> float8 in [0,1). Uses xorshift64*.
            return Ok(Value::Float(random_f64()));
        }
        _ => {}
    }
    let v = &vals[0];
    if v == &Value::Null || (vals.len() > 1 && vals[1] == Value::Null) {
        return Ok(Value::Null);
    }
    match name {
        "abs" => match v {
            Value::SmallInt(i) => i
                .checked_abs()
                .map(Value::SmallInt)
                .ok_or_else(|| exec_err("22003", "smallint out of range")),
            Value::Int(i) => i
                .checked_abs()
                .map(Value::Int)
                .ok_or_else(|| exec_err("22003", "integer out of range")),
            Value::BigInt(i) => i
                .checked_abs()
                .map(Value::BigInt)
                .ok_or_else(|| exec_err("22003", "bigint out of range")),
            Value::Numeric(n) => Ok(Value::Numeric(n.abs())),
            Value::Float4(f) => Ok(Value::Float4(f.abs())),
            Value::Float(f) => Ok(Value::Float(f.abs())),
            other => Err(func_arg_err(name, other)),
        },
        "round" => {
            // v0.67: PG19 overloads — round(numeric) -> numeric,
            // round(float8) -> float8 (dround is rint, half to even),
            // round(numeric, int) -> numeric. A float4 input coerces to
            // float8 like PG's parser does. (The old code sent float8
            // through numeric, wrongly returning a 200-digit numeric
            // for round(1e200::float8) instead of float8 1.2345e+200.)
            if vals.len() == 1 {
                match v {
                    Value::Float4(f) => return Ok(Value::Float(f64::from(*f).round_ties_even())),
                    Value::Float(f) => return Ok(Value::Float(f.round_ties_even())),
                    _ => {}
                }
                let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
                return n
                    .round_to(0)
                    .map(Value::Numeric)
                    .ok_or_else(|| exec_err("22003", "numeric field overflow"));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            let s = int_arg(name, &vals[1])?.unwrap_or(0);
            round_scale(&n, s)
                .map(Value::Numeric)
                .ok_or_else(|| exec_err("22003", "numeric field overflow"))
        }
        "floor" | "ceil" | "ceiling" => {
            let is_floor = name == "floor";
            match v {
                Value::Float4(f) => Ok(Value::Float4(if is_floor { f.floor() } else { f.ceil() })),
                Value::Float(f) => Ok(Value::Float(if is_floor { f.floor() } else { f.ceil() })),
                other => {
                    let n = to_numeric_opt(other).ok_or_else(|| func_arg_err(name, other))?;
                    let r = if is_floor { n.floor() } else { n.ceil() };
                    r.map(Value::Numeric)
                        .ok_or_else(|| exec_err("22003", "numeric field overflow"))
                }
            }
        }
        "sqrt" => {
            if matches!(v, Value::Float4(_) | Value::Float(_)) {
                // v0.21: PG dsqrt — a negative float is 2201F (was NaN).
                let x = to_f64v(v);
                if x.is_nan() {
                    return Ok(Value::Float(f64::NAN));
                }
                if x < 0.0 {
                    return Err(exec_err(
                        "2201F",
                        "cannot take square root of a negative number",
                    ));
                }
                return Ok(Value::Float(x.sqrt()));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            // v0.62: PG19 numeric_sqrt — adaptive result scale. NaN and
            // +Inf duplicate; -Inf and negatives are 2201F.
            if n.is_nan() || n.special == crate::storage::NumericSpecial::PosInf {
                return Ok(Value::Numeric(n));
            }
            if n.special == crate::storage::NumericSpecial::NegInf
                || (n.special == crate::storage::NumericSpecial::Finite && n.unscaled < 0)
            {
                // Postgres numeric.c: ERRCODE_INVALID_ARGUMENT_FOR_POWER_FUNCTION.
                return Err(exec_err(
                    "2201F",
                    "cannot take square root of a negative number",
                ));
            }
            match n.sqrt_pg() {
                Some(r) => Ok(Value::Numeric(r)),
                None => Err(exec_err("22003", "value overflows numeric format")),
            }
        }
        "exp" => {
            if matches!(v, Value::Float4(_) | Value::Float(_)) {
                // v0.21: PG dexp — finite overflow/underflow is 22003
                // (was: +/-inf/0 silently); NaN/Inf pass through.
                let x = to_f64v(v);
                let r = x.exp();
                if x.is_finite() {
                    if r.is_infinite() {
                        return Err(exec_err("22003", "value out of range: overflow"));
                    }
                    if r == 0.0 {
                        return Err(exec_err("22003", "value out of range: underflow"));
                    }
                }
                return Ok(Value::Float(r));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            // Postgres special values: exp(NaN)=NaN, exp(Inf)=Inf, exp(-Inf)=0.
            if n.is_nan() {
                return Ok(Value::Numeric(Numeric::nan()));
            }
            if n.special == crate::storage::NumericSpecial::PosInf {
                return Ok(Value::Numeric(Numeric::infinity()));
            }
            if n.special == crate::storage::NumericSpecial::NegInf {
                return Ok(Value::Numeric(Numeric::zero()));
            }
            // v0.62: PG19 numeric_exp — adaptive result scale.
            match n.exp_pg() {
                Some(r) => Ok(Value::Numeric(r)),
                None => Err(exec_err("22003", "value overflows numeric format")),
            }
        }
        "ln" => {
            if matches!(v, Value::Float4(_) | Value::Float(_)) {
                let xv = to_f64v(v);
                // PG errors on ln(0)/ln(negative) even for float8.
                if xv <= 0.0 {
                    return Err(exec_err("2201E", "cannot take logarithm"));
                }
                return Ok(Value::Float(xv.ln()));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            // Postgres: ln(NaN)=NaN, ln(Inf)=Inf, ln(-Inf) and ln(<=0) error.
            if n.is_nan() {
                return Ok(Value::Numeric(Numeric::nan()));
            }
            if n.special == crate::storage::NumericSpecial::PosInf {
                return Ok(Value::Numeric(Numeric::infinity()));
            }
            if n.special == crate::storage::NumericSpecial::NegInf || n.unscaled <= 0 {
                return Err(exec_err(
                    "2201E",
                    "cannot take logarithm of a negative number",
                ));
            }
            if n.is_zero() {
                return Err(exec_err("2201E", "cannot take logarithm of zero"));
            }
            // v0.62: PG19 numeric_ln — adaptive result scale.
            match n.ln_pg() {
                Some(r) => Ok(Value::Numeric(r)),
                None => Err(exec_err("22003", "value overflows numeric format")),
            }
        }
        "log" => {
            // log(x) = ln(x)/ln(10); log(b, x) = ln(x)/ln(b).
            if matches!(v, Value::Float4(_) | Value::Float(_)) {
                let xv = to_f64v(v);
                // PG errors on log(0)/log(negative) even for float8.
                // (NaN passes through to f64, which yields NaN.)
                if vals.len() == 2 {
                    let bv = to_f64v(&vals[1]);
                    if xv <= 0.0 || bv <= 0.0 || bv == 1.0 {
                        return Err(exec_err("2201E", "cannot take logarithm"));
                    }
                    return Ok(Value::Float(xv.log(bv)));
                }
                if xv <= 0.0 {
                    return Err(exec_err("2201E", "cannot take logarithm"));
                }
                return Ok(Value::Float(xv.log10()));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            if vals.len() == 2 {
                let x = to_numeric_opt(&vals[1]).ok_or_else(|| func_arg_err(name, &vals[1]))?;
                // log(b, x): n is base (first arg), x is value (second arg).
                // log(b, x) = ln(x)/ln(b).
                return eval_log_base(&n, &x, name);
            }
            // v0.62: PG defines log(x) as log(10, x) (system_functions.sql).
            return eval_log_base(&Numeric::from_i64(10), &n, name);
        }
        "power" => eval_power_op(v, &vals[1], |w| func_arg_err(name, w)),
        // v0.18: numeric functions.
        "cbrt" => {
            if matches!(v, Value::Float4(_) | Value::Float(_)) {
                return Ok(Value::Float(to_f64v(v).cbrt()));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            if n.is_nan() {
                return Ok(Value::Numeric(Numeric::nan()));
            }
            // cbrt via f64 (PG uses float internally for cbrt).
            let f = n.to_f64().cbrt();
            // Convert back to numeric.
            match Numeric::parse(&format!("{:.15}", f)) {
                Ok(mut r) => {
                    // v0.62 fix: PG has no numeric cbrt — cbrt() is
                    // float8-only (`cbrt(64.0)` -> `4`) — so there is no
                    // PG numeric dscale to inherit. Strip the formatting
                    // scale so perfect cubes display as PG's float8
                    // cbrt does (`cbrt(8)` -> "2", not "2.000000000000000").
                    r.dscale = r.scale.max(0);
                    Ok(Value::Numeric(r))
                }
                Err(_) => Ok(Value::Float(f)),
            }
        }
        // v0.21: float8 transcendental functions (Postgres float.c
        // semantics). All return float8; NaN propagates; domain
        // violations raise like Postgres.
        "sin" | "cos" | "tan" => {
            let x = reject_inf_input(float_math_arg(name, v)?)?;
            Ok(Value::Float(match name {
                "sin" => x.sin(),
                "cos" => x.cos(),
                _ => x.tan(),
            }))
        }
        "asin" | "acos" => {
            let x = float_math_arg(name, v)?;
            if x.is_nan() {
                return Ok(Value::Float(f64::NAN));
            }
            // PG dasin/dacos: outside [-1, 1] is 22003.
            if x < -1.0 || x > 1.0 {
                return Err(exec_err("22003", "input is out of range"));
            }
            Ok(Value::Float(if name == "asin" {
                x.asin()
            } else {
                x.acos()
            }))
        }
        "atan" => {
            let x = float_math_arg(name, v)?;
            Ok(Value::Float(x.atan()))
        }
        "atan2" => {
            let y = float_math_arg(name, v)?;
            let x = float_math_arg(name, &vals[1])?;
            if y.is_nan() || x.is_nan() {
                return Ok(Value::Float(f64::NAN));
            }
            // PG datan2: atan2(0, 0) is 22012.
            if y == 0.0 && x == 0.0 {
                return Err(exec_err("22012", "division by zero"));
            }
            Ok(Value::Float(y.atan2(x)))
        }
        "sinh" | "cosh" | "tanh" => {
            let x = float_math_arg(name, v)?;
            let r = match name {
                "sinh" => x.sinh(),
                "cosh" => x.cosh(),
                _ => x.tanh(),
            };
            // PG CHECKFLOATVAL: infinite result from finite input.
            if r.is_infinite() && x.is_finite() {
                return Err(exec_err("22003", "value out of range: overflow"));
            }
            Ok(Value::Float(r))
        }
        "asinh" => {
            let x = float_math_arg(name, v)?;
            Ok(Value::Float(x.asinh()))
        }
        "acosh" => {
            let x = float_math_arg(name, v)?;
            if x.is_nan() {
                return Ok(Value::Float(f64::NAN));
            }
            // PG dacosh: x < 1 (other than NaN) is 22003.
            if x < 1.0 {
                return Err(exec_err("22003", "input is out of range"));
            }
            Ok(Value::Float(x.acosh()))
        }
        "atanh" => {
            let x = float_math_arg(name, v)?;
            if x.is_nan() {
                return Ok(Value::Float(f64::NAN));
            }
            // |x| > 1 is 22003; atanh(±1) = ±inf.
            if x.abs() > 1.0 {
                return Err(exec_err("22003", "input is out of range"));
            }
            Ok(Value::Float(x.atanh()))
        }
        "erf" => {
            let x = float_math_arg(name, v)?;
            Ok(Value::Float(erf_impl(x)))
        }
        "erfc" => {
            let x = float_math_arg(name, v)?;
            Ok(Value::Float(erfc_impl(x)))
        }
        "gamma" => {
            let x = float_math_arg(name, v)?;
            Ok(Value::Float(gamma_impl(x)?))
        }
        "lgamma" => {
            let x = float_math_arg(name, v)?;
            Ok(Value::Float(lgamma_impl(x)?))
        }
        "sind" | "cosd" | "tand" | "cotd" => {
            let x = float_math_arg(name, v)?;
            if x.is_nan() {
                return Ok(Value::Float(f64::NAN));
            }
            if x.is_infinite() {
                return Err(exec_err("22003", "input is out of range"));
            }
            Ok(Value::Float(match name {
                "sind" => sind_impl(x),
                "cosd" => cosd_impl(x),
                "tand" => tand_impl(x),
                _ => cotd_impl(x),
            }))
        }
        "asind" | "acosd" => {
            let x = float_math_arg(name, v)?;
            if x.is_nan() {
                return Ok(Value::Float(f64::NAN));
            }
            if x < -1.0 || x > 1.0 {
                return Err(exec_err("22003", "input is out of range"));
            }
            Ok(Value::Float(if name == "asind" {
                asind_impl(x)
            } else {
                acosd_impl(x)
            }))
        }
        "atand" => {
            let x = float_math_arg(name, v)?;
            Ok(Value::Float(atand_impl(x)))
        }
        "atan2d" => {
            let y = float_math_arg(name, v)?;
            let x = float_math_arg(name, &vals[1])?;
            if y.is_nan() || x.is_nan() {
                return Ok(Value::Float(f64::NAN));
            }
            if y == 0.0 && x == 0.0 {
                return Err(exec_err("22012", "division by zero"));
            }
            Ok(Value::Float(y.atan2(x).to_degrees()))
        }
        "trunc" => {
            // v0.22: overloaded like Postgres — numeric in -> exact
            // numeric out (trunc_to_scale(0), so trunc('1e200') is
            // exactly 1e+200); float in -> float out.
            // v0.56: two-argument form trunc(x, s), PG's
            // trunc(numeric, int) -> numeric. Like round's two-argument
            // form, a float input goes through numeric.
            if vals.len() == 2 {
                let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
                let s = int_arg(name, &vals[1])?.unwrap_or(0);
                let s32 =
                    i32::try_from(s).map_err(|_| exec_err("22003", "numeric field overflow"))?;
                Ok(Value::Numeric(n.trunc_to_scale(s32)))
            } else {
                match v {
                    Value::Numeric(n) => Ok(Value::Numeric(n.trunc_to_scale(0))),
                    _ => {
                        let x = float_math_arg(name, v)?;
                        Ok(Value::Float(x.trunc()))
                    }
                }
            }
        }
        "log10" => {
            // v0.21: log10(float8) -> float8; x <= 0 is 2201E, like
            // ln/log.
            let x = float_math_arg(name, v)?;
            if x.is_nan() {
                return Ok(Value::Float(f64::NAN));
            }
            if x <= 0.0 {
                return Err(exec_err("2201E", "cannot take logarithm"));
            }
            Ok(Value::Float(x.log10()))
        }
        "float8send" => {
            // v0.21: float8send(float8) -> bytea: the 8-byte big-endian
            // IEEE 754 binary representation, like PG's float8send.
            let x = float_math_arg(name, v)?;
            Ok(Value::Bytea(x.to_be_bytes().to_vec()))
        }
        "float4send" => {
            // v0.58: float4send(float4) -> bytea: the 4-byte big-endian
            // IEEE 754 binary representation, like PG's float4send
            // (REL_19_STABLE float.c: pq_sendfloat4 writes htonl of the
            // bit pattern). float4 -> float8 -> float4 is an exact
            // round-trip, so routing through float_math_arg (f64) and
            // narrowing with `as f32` is bit-identical to PG's send;
            // for non-float4 inputs this matches PG's implicit
            // float8 -> float4 coercion at the function boundary.
            // (Input parsing is correctly rounded via parse_f32_checked,
            // matching PG 19's float4in: PG's own float4 regression
            // expected values — e.g. '7038531e-32' -> 0x15ae43fd —
            // prove single correct rounding, where naive f64-then-narrow
            // double rounding would give 0x15ae43fe.)
            let x = float_math_arg(name, v)? as f32;
            Ok(Value::Bytea(x.to_be_bytes().to_vec()))
        }
        "float4recv" => {
            // v0.58: float4recv(bytea) -> float4, like PG's float4recv
            // (pq_getmsgfloat4): 4-byte big-endian IEEE 754; fewer than
            // 4 bytes is 22P03 "insufficient data left in message";
            // trailing bytes are ignored, as in PG.
            let b = match v {
                Value::Bytea(b) => b,
                _ => return Err(func_arg_err(name, v)),
            };
            if b.len() < 4 {
                return Err(exec_err("22P03", "insufficient data left in message"));
            }
            Ok(Value::Float4(f32::from_be_bytes([b[0], b[1], b[2], b[3]])))
        }
        "float8recv" => {
            // v0.58: float8recv(bytea) -> float8, like PG's float8recv
            // (pq_getmsgfloat8): 8-byte big-endian IEEE 754; fewer than
            // 8 bytes is 22P03; trailing bytes ignored.
            let b = match v {
                Value::Bytea(b) => b,
                _ => return Err(func_arg_err(name, v)),
            };
            if b.len() < 8 {
                return Err(exec_err("22P03", "insufficient data left in message"));
            }
            Ok(Value::Float(f64::from_be_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ])))
        }
        "numeric_inc" => {
            // v0.59: numeric_inc(numeric) -> numeric. PG19 numeric_inc:
            // NaN and both infinities are fixed points, otherwise x + 1.
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            if n.is_nan() || n.special != crate::storage::NumericSpecial::Finite {
                return Ok(Value::Numeric(n));
            }
            let one = Numeric::new(1, 0);
            match n.checked_add(&one) {
                Some(r) => Ok(Value::Numeric(r)),
                None => Err(exec_err("22003", "value overflows numeric format")),
            }
        }
        "factorial" => {
            // factorial(numeric) -> numeric. PG: 0! = 1, negative errors.
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            if n.is_nan() {
                return Ok(Value::Numeric(Numeric::nan()));
            }
            // Must be a non-negative integer.
            let iv = n
                .to_i64()
                .ok_or_else(|| exec_err("2201F", "factorial of non-integer"))?;
            if iv < 0 {
                return Err(exec_err("2201F", "factorial of negative value"));
            }
            if iv > 1000 {
                return Err(exec_err("22003", "value overflows numeric format"));
            }
            let mut result: i128 = 1;
            for i in 2..=iv as i128 {
                result = result
                    .checked_mul(i)
                    .ok_or_else(|| exec_err("22003", "value overflows numeric format"))?;
            }
            Ok(Value::Numeric(Numeric::new(result, 0)))
        }
        "gcd" | "lcm" => {
            // v0.25: PG has int4/int8 overloads that return the same type
            // and raise 22003 on overflow (e.g. gcd(-2^31, 0) overflows
            // int4). Numeric args use the numeric path.
            let int_gcd_lcm = |a: i64, b: i64, bits: u32| -> Result<i64, ExecError> {
                let mut x = (a as i128).unsigned_abs();
                let mut y = (b as i128).unsigned_abs();
                while y != 0 {
                    let t = x % y;
                    x = y;
                    y = t;
                }
                let g = x;
                let result = if name == "gcd" {
                    g
                } else {
                    if g == 0 {
                        0
                    } else {
                        (a as i128).unsigned_abs() / g * (b as i128).unsigned_abs()
                    }
                };
                // v0.25: gcd/lcm results are non-negative; check against
                // the type's max (min is irrelevant for unsigned result).
                let max = match bits {
                    16 => i16::MAX as u128,
                    32 => i32::MAX as u128,
                    _ => i64::MAX as u128,
                };
                if result > max {
                    return Err(exec_err("22003", "integer out of range"));
                }
                Ok(result as i64)
            };
            // Check for int2/int4/int8 args (both must be integer types).
            let int_args = match (v, &vals[1]) {
                (Value::SmallInt(a), Value::SmallInt(b)) => Some((*a as i64, *b as i64, 16)),
                (Value::Int(a), Value::Int(b)) => Some((*a, *b, 32)),
                (Value::BigInt(a), Value::BigInt(b)) => Some((*a, *b, 64)),
                _ => None,
            };
            if let Some((a, b, bits)) = int_args {
                let r = int_gcd_lcm(a, b, bits)?;
                return Ok(match bits {
                    16 => Value::SmallInt(r as i16),
                    32 => Value::Int(r),
                    _ => Value::BigInt(r),
                });
            }
            let a = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            let b = to_numeric_opt(&vals[1]).ok_or_else(|| func_arg_err(name, &vals[1]))?;
            // v0.61: PG19 numeric_gcd/numeric_lcm — any NaN or infinite
            // input yields NaN; otherwise the Euclidean algorithm runs
            // over the exact decimal values (fractionals included, e.g.
            // gcd(43312.5, 4637.5) = 87.5), the result is non-negative,
            // and the display scale is max(dscale(a), dscale(b)).
            use crate::storage::NumericSpecial;
            if a.special != NumericSpecial::Finite || b.special != NumericSpecial::Finite {
                return Ok(Value::Numeric(crate::storage::Numeric::nan()));
            }
            let da = crate::storage::BigDec::from_numeric(&a.abs()).expect("finite");
            let db = crate::storage::BigDec::from_numeric(&b.abs()).expect("finite");
            let dscale = a.dscale.max(b.dscale);
            // Euclidean algorithm on absolute values.
            let mut x = da;
            let mut y = db;
            while !y.is_zero() {
                let r = x.rem(&y);
                x = y;
                y = r;
            }
            // x is now gcd(|a|, |b|) >= 0.
            if name == "gcd" {
                // v0.67: the 131072-digit format limit (PG19
                // `make_result`), not the i128 narrowing — a gcd past
                // i128 is a legal numeric (e.g. gcd(10^50, 10^50)).
                let mut n = crate::storage::Numeric::from_bigdec(&x, dscale)
                    .ok_or_else(|| exec_err("22003", "value overflows numeric format"))?;
                n.dscale = dscale;
                Ok(Value::Numeric(n))
            } else {
                // lcm(x, y) = |x / gcd * y|; zero when either input is
                // zero (PG computes the division exactly, then rounds
                // the product to y's dscale — a no-op here — and reports
                // max(dscale(x), dscale(y))).
                if x.is_zero() {
                    return Ok(Value::Numeric(
                        crate::storage::Numeric::zero().with_dscale(dscale),
                    ));
                }
                let ax = crate::storage::BigDec::from_numeric(&a.abs()).expect("finite");
                let bx = crate::storage::BigDec::from_numeric(&b.abs()).expect("finite");
                let q = ax
                    .div_exact(&x)
                    .ok_or_else(|| exec_err("22003", "value overflows numeric format"))?;
                let l = q
                    .mul_exact(&bx)
                    .ok_or_else(|| exec_err("22003", "value overflows numeric format"))?;
                // v0.67: the 131072-digit format limit (PG19
                // `make_result`'s overflow check), not the i128
                // narrowing — e.g. lcm(10^50, 3) is a legal numeric.
                let mut n = crate::storage::Numeric::from_bigdec(&l, dscale)
                    .ok_or_else(|| exec_err("22003", "value overflows numeric format"))?;
                n.dscale = dscale;
                Ok(Value::Numeric(n))
            }
        }
        "degrees" => {
            // degrees(radians) = radians * 180/pi.
            if matches!(v, Value::Float4(_) | Value::Float(_)) {
                return Ok(Value::Float(to_f64v(v).to_degrees()));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            if n.is_nan() {
                return Ok(Value::Numeric(Numeric::nan()));
            }
            // n * 180 / pi.
            let pi = match Numeric::parse("3.14159265358979323846264338327950288") {
                Ok(v) => v,
                Err(_) => return Err(exec_err("22003", "value overflows numeric format")),
            };
            let n180 = n
                .checked_mul(&Numeric::new(180, 0))
                .ok_or_else(|| exec_err("22003", "value overflows numeric format"))?;
            // v0.18-repair: checked_div's 10-guard-digit rescale overflows i128
            // when the divisor has a large scale (pi is scale 38); div_at_scale
            // does the division in f64 at the engine's standard 10-digit scale.
            match n180.div_at_scale(&pi, 10) {
                Some(r) => Ok(Value::Numeric(r)),
                None => Err(exec_err("22003", "value overflows numeric format")),
            }
        }
        "radians" => {
            // radians(degrees) = degrees * pi/180.
            if matches!(v, Value::Float4(_) | Value::Float(_)) {
                return Ok(Value::Float(to_f64v(v).to_radians()));
            }
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            if n.is_nan() {
                return Ok(Value::Numeric(Numeric::nan()));
            }
            let pi = match Numeric::parse("3.14159265358979323846264338327950288") {
                Ok(v) => v,
                Err(_) => return Err(exec_err("22003", "value overflows numeric format")),
            };
            let npi = n
                .checked_mul(&pi)
                .ok_or_else(|| exec_err("22003", "value overflows numeric format"))?;
            // v0.18-repair: see degrees() — div_at_scale avoids the i128
            // rescale overflow in checked_div.
            match npi.div_at_scale(&Numeric::new(180, 0), 10) {
                Some(r) => Ok(Value::Numeric(r)),
                None => Err(exec_err("22003", "value overflows numeric format")),
            }
        }
        "scale" => {
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            // v0.61: PG19's numeric_scale returns the *display* scale
            // (NUMERIC_DSCALE): the declared scale of a literal
            // (scale('1.50') = 2), NULL for NaN/Infinity.
            if n.special != crate::storage::NumericSpecial::Finite {
                return Ok(Value::Null);
            }
            Ok(Value::Int(n.dscale as i64))
        }
        "min_scale" => {
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            // min_scale: minimum scale needed (trailing zeros removed).
            // PG returns NULL for NaN/Infinity.
            if n.special != crate::storage::NumericSpecial::Finite {
                return Ok(Value::Null);
            }
            let mut unscaled = n.unscaled.abs();
            let mut scale = n.scale;
            while scale > 0 && unscaled % 10 == 0 {
                unscaled /= 10;
                scale -= 1;
            }
            // v0.59: PG's get_min_scale clamps to zero when there are no
            // digits after the decimal point (e.g. min_scale(1e100) = 0).
            Ok(Value::Int(scale.max(0) as i64))
        }
        "trim_scale" => {
            let n = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            // trim_scale: remove trailing zeros.
            if n.special != crate::storage::NumericSpecial::Finite {
                return Ok(Value::Numeric(n.clone()));
            }
            // v0.63: big-mantissa values are already normalized (no
            // trailing zeros while scale > 0), so this is a no-op.
            if n.is_big() {
                return Ok(Value::Numeric(n.clone()));
            }
            let mut unscaled = n.unscaled;
            let mut scale = n.scale;
            while scale > 0 && unscaled % 10 == 0 {
                unscaled /= 10;
                scale -= 1;
            }
            Ok(Value::Numeric(Numeric::new(unscaled, scale)))
        }
        "div" => {
            // div(numeric, numeric): truncating division.
            let a = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            let b = to_numeric_opt(&vals[1]).ok_or_else(|| func_arg_err(name, &vals[1]))?;
            // v0.25: PG returns NaN if either operand is NaN, even when
            // dividing by zero (div('nan', '0') = NaN).
            use crate::storage::NumericSpecial;
            if a.special == NumericSpecial::NaN || b.special == NumericSpecial::NaN {
                return Ok(Value::Numeric(crate::storage::Numeric::nan()));
            }
            if b.is_zero() {
                return Err(exec_err("22012", "division by zero"));
            }
            if b.is_zero() {
                return Err(exec_err("22012", "division by zero"));
            }
            // v0.59: PG19 calls div_var with rscale 0 and no rounding
            // (truncation): the quotient is computed directly at scale 0,
            // not rounded at higher precision then truncated. div_trunc
            // keeps the NaN behavior above (NaN in -> NaN out).
            match a.div_trunc(&b) {
                Some(q) => Ok(Value::Numeric(q)),
                None => Err(exec_err("22003", "value overflows numeric format")),
            }
        }
        "width_bucket" => {
            // width_bucket(op, b1, b2, count) or width_bucket(op, thresholds).
            return eval_width_bucket(v, vals, name);
        }
        "pg_lsn" => {
            // v0.64: pg_lsn(numeric) -> pg_lsn display text (PG19).
            // NaN -> 0A000 "cannot convert NaN to pg_lsn";
            // Infinity -> 0A000 "cannot convert infinity to pg_lsn";
            // negative/fractional/>u64::MAX -> 22023 "pg_lsn out of range".
            // Display: HIGH/LOW with LOW zero-padded to 8 hex digits.
            let num = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            if num.is_nan() {
                return Err(exec_err("0A000", "cannot convert NaN to pg_lsn"));
            }
            if num.is_special() {
                return Err(exec_err("0A000", "cannot convert infinity to pg_lsn"));
            }
            let lsn = numeric_to_u64_exact(&num)
                .ok_or_else(|| exec_err("22023", "pg_lsn out of range"))?;
            // v0.64: return native pg_lsn type (OID 3220), not text.
            return Ok(Value::PgLsn(lsn));
        }
        "setseed" => {
            // v0.18: setseed(float) -> void (sets PRNG seed).
            let s = to_f64v(v);
            // PG: setseed takes float in [-1,1].
            if s < -1.0 || s > 1.0 {
                return Err(exec_err("2201F", "setseed parameter out of range"));
            }
            set_random_seed(s);
            return Ok(Value::Null);
        }
        "mod" => {
            // Postgres resolves mod() to numeric.
            let a = to_numeric_opt(v).ok_or_else(|| func_arg_err(name, v))?;
            let b = to_numeric_opt(&vals[1]).ok_or_else(|| func_arg_err(name, &vals[1]))?;
            if b.is_zero() {
                return Err(exec_err("22012", "division by zero"));
            }
            a.checked_rem(&b)
                .map(Value::Numeric)
                .ok_or_else(|| exec_err("22003", "numeric field overflow"))
        }
        // v0.16: sign() returns -1/0/1 in the input's own type, like
        // Postgres (NaN stays NaN for floats).
        "sign" => match v {
            Value::SmallInt(i) => Ok(Value::SmallInt(i.signum())),
            Value::Int(i) => Ok(Value::Int(i.signum())),
            Value::BigInt(i) => Ok(Value::BigInt(i.signum())),
            Value::Numeric(n) => {
                // v0.18: sign('NaN') is NaN, like PostgreSQL.
                if n.is_nan() {
                    return Ok(Value::Numeric(Numeric::nan()));
                }
                let s = match n.cmp(&Numeric::new(0, 0)) {
                    std::cmp::Ordering::Less => -1i128,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                };
                Ok(Value::Numeric(Numeric::new(s, 0)))
            }
            Value::Float4(f) => {
                // v0.21: sign(0) = 0 (Rust's signum() returns 1.0 for 0.0).
                let s = if *f == 0.0 { 0.0 } else { f.signum() };
                Ok(Value::Float4(s))
            }
            Value::Float(f) => {
                // v0.21: sign(0) = 0 (Rust's signum() returns 1.0 for 0.0).
                let s = if *f == 0.0 { 0.0 } else { f.signum() };
                Ok(Value::Float(s))
            }
            other => Err(func_arg_err(name, other)),
        },
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// round(n, s): v0.22 delegates negative scales to `round_to` directly
/// (rounding half away from zero, like PostgreSQL).
pub(crate) fn round_scale(n: &Numeric, s: i64) -> Option<Numeric> {
    n.round_to(i32::try_from(s).ok()?)
}

/// Map a format-string error to its SQLSTATE: unsupported pattern
/// elements are 0A000, bad input values are 22008.
pub(crate) fn fmt_exec_err(e: crate::datetime::FmtErr) -> ExecError {
    match e {
        crate::datetime::FmtErr::Unsupported(msg) => exec_err("0A000", msg),
        crate::datetime::FmtErr::Invalid(msg) => exec_err("22008", msg),
    }
}

/// v0.26: `to_char` for numeric values (PG's `numeric_to_char`).
pub(crate) fn num_to_char(
    n: &crate::storage::Numeric,
    fmt: &str,
    _name: &str,
) -> Result<Value, ExecError> {
    let desc = match crate::numfmt::parse_numfmt(fmt) {
        Ok(d) => d,
        Err(e) => return Err(numfmt_exec_err(e)),
    };
    match crate::numfmt_tochar::numeric_to_char(n, &desc) {
        Ok(s) => Ok(Value::text(s)),
        Err(e) => Err(numfmt_exec_err(e)),
    }
}

/// v0.93: `to_char` for integer inputs (PG19's `int4_to_char` /
/// `int8_to_char`; int2 promotes to int4, hence `bits = 32`). With a `V`
/// (multi) picture PG multiplies in the *input* type —
/// `value = int4mul(value, dtoi4(10^multi))` — raising 22003 "integer out
/// of range" on overflow, then grows the picture (`Num.pre +=
/// Num.multi`). The plain numeric path cannot reproduce that overflow
/// error, so integers take this checked path whenever `multi > 0` on the
/// plain (non-roman, non-EEEE) path.
pub(crate) fn int_to_char(v: i128, bits: u32, fmt: &str) -> Result<Value, ExecError> {
    let base_desc = match crate::numfmt::parse_numfmt(fmt) {
        Ok(d) => d,
        Err(e) => return Err(numfmt_exec_err(e)),
    };
    if base_desc.multi > 0 && !base_desc.roman && !base_desc.eeee {
        // dtoi4/dtoi8(10^multi): an out-of-range multiplier errors too;
        // 10^k is exact in float8 for k <= 22, so the int4 (k <= 9) and
        // int8 (k <= 18) multipliers are always exact.
        let max_multi = if bits == 32 { 9 } else { 18 };
        if base_desc.multi > max_multi {
            return Err(exec_err("22003", "integer out of range"));
        }
        let shifted = v * 10i128.pow(base_desc.multi as u32);
        let in_range = if bits == 32 {
            shifted >= i128::from(i32::MIN) && shifted <= i128::from(i32::MAX)
        } else {
            shifted >= i128::from(i64::MIN) && shifted <= i128::from(i64::MAX)
        };
        if !in_range {
            return Err(exec_err("22003", "integer out of range"));
        }
        // Mutates pre/multi, so this (rare: only `V`-picture integer
        // input) path clones out of the cached Rc instead of sharing it.
        let mut desc = (*base_desc).clone();
        desc.pre += desc.multi;
        desc.multi = 0;
        let n = crate::storage::Numeric::new(shifted, 0);
        return match crate::numfmt_tochar::numeric_to_char(&n, &desc) {
            Ok(s) => Ok(Value::text(s)),
            Err(e) => Err(numfmt_exec_err(e)),
        };
    }
    let n = crate::storage::Numeric::new(v, 0);
    match crate::numfmt_tochar::numeric_to_char(&n, &base_desc) {
        Ok(s) => Ok(Value::text(s)),
        Err(e) => Err(numfmt_exec_err(e)),
    }
}

/// v0.26: `to_char` for float values (PG's `float8_to_char`).
pub(crate) fn float_to_char(v: f64, fmt: &str, _name: &str) -> Result<Value, ExecError> {
    let desc = match crate::numfmt::parse_numfmt(fmt) {
        Ok(d) => d,
        Err(e) => return Err(numfmt_exec_err(e)),
    };
    match crate::numfmt_tochar::float8_to_char(v, &desc) {
        Ok(s) => Ok(Value::text(s)),
        Err(e) => Err(numfmt_exec_err(e)),
    }
}

/// v0.64: `to_char` for float4 values (PG's `float4_to_char` trims
/// post-decimal digits to FLT_DIG significant digits).
pub(crate) fn float4_to_char(v: f32, fmt: &str, _name: &str) -> Result<Value, ExecError> {
    let desc = match crate::numfmt::parse_numfmt(fmt) {
        Ok(d) => d,
        Err(e) => return Err(numfmt_exec_err(e)),
    };
    match crate::numfmt_tochar::float4_to_char(v, &desc) {
        Ok(s) => Ok(Value::text(s)),
        Err(e) => Err(numfmt_exec_err(e)),
    }
}

/// v0.26: map numeric-format errors to PG's codes: 42601
/// (syntax_error) for bad pictures, 22P02
/// (invalid_text_representation) for bad input, 0A000
/// for the unsupported EEEE input path.
pub(crate) fn numfmt_exec_err(e: crate::numfmt::NumFmtError) -> ExecError {
    match e {
        crate::numfmt::NumFmtError::Syntax(msg) => exec_err("42601", msg),
        crate::numfmt::NumFmtError::InvalidInput(msg) => exec_err("22P02", msg),
        crate::numfmt::NumFmtError::Unsupported(msg) => exec_err("0A000", msg),
    }
}

/// v0.76: PG19 AdjustTimestampForTypmod — round microseconds to `p`
/// fractional digits (0-6); ties away from zero, symmetric for negative
/// timestamps. `None` leaves the value untouched.
pub(crate) fn round_micros_to_precision(micros: i64, precision: Option<i64>) -> i64 {
    match precision {
        None => micros,
        Some(p) => {
            let scale = 10i64.pow((6 - p) as u32);
            let offset = scale / 2;
            if micros >= 0 {
                ((micros + offset) / scale) * scale
            } else {
                -(((-micros + offset) / scale) * scale)
            }
        }
    }
}

pub(crate) fn eval_datetime_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    // v0.76: optional precision arg (0-6) on CURRENT_TIMESTAMP, per
    // PG19. Rounds half away from zero (PG19 AdjustTimestampForTypmod
    // adds half the scale, symmetric for negative timestamps) — it does
    // not truncate. now()/clock_timestamp()/statement_timestamp()/
    // transaction_timestamp() take no arguments in PG19 (42883
    // otherwise; enforced by check_builtin_arity).
    let precision: Option<i64> = if name == "current_timestamp" && !vals.is_empty() {
        match int_arg(name, &vals[0])? {
            None => return Ok(Value::Null),
            Some(p) if (0..=6).contains(&p) => Some(p),
            Some(_) => {
                return Err(exec_err(
                    "22023",
                    format!(
                        "precision {} out of range for {}",
                        vals[0].to_text().unwrap_or_default(),
                        name
                    ),
                ));
            }
        }
    } else {
        None
    };
    let apply_precision = |micros: i64| -> i64 { round_micros_to_precision(micros, precision) };
    match name {
        "now" | "current_timestamp" => Ok(Value::Timestamptz(apply_precision(
            crate::datetime::now_micros(),
        ))),
        "clock_timestamp" | "statement_timestamp" | "transaction_timestamp" => Ok(
            Value::Timestamptz(apply_precision(crate::datetime::now_micros())),
        ),
        "current_date" => Ok(Value::Date(crate::datetime::today_days())),
        "date_trunc" => {
            let field = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s.to_ascii_lowercase(),
            };
            let v = &vals[1];
            if v == &Value::Null {
                return Ok(Value::Null);
            }
            let (micros, is_tz) = match v {
                Value::Date(d) => (
                    (*d as i64)
                        .checked_mul(86_400_000_000)
                        .ok_or_else(|| exec_err("22008", "datetime field overflow"))?,
                    false,
                ),
                Value::Timestamp(m) => (*m, false),
                Value::Timestamptz(m) => (*m, true),
                other => return Err(func_arg_err(name, other)),
            };
            match crate::datetime::date_trunc(&field, micros) {
                Ok(m) => Ok(if is_tz {
                    Value::Timestamptz(m)
                } else {
                    Value::Timestamp(m)
                }),
                Err(e) => Err(exec_err("22023", e)),
            }
        }
        // v0.17: function form of EXTRACT; identical semantics (and
        // identical numeric result) to `extract(field FROM x)`.
        "date_part" => {
            let field = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            eval_extract(field, &vals[1])
        }
        // v0.17: parse with a format string (documented pattern subset
        // in datetime.rs); unsupported patterns are 0A000, bad input
        // is 22008.
        "to_date" => {
            let input = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let fmt = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            match crate::datetime::to_date_parsed(input, fmt) {
                Ok(d) => Ok(Value::Date(d)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        "to_timestamp" => {
            if vals.len() == 1 {
                // to_timestamp(float8): Unix epoch seconds -> timestamptz.
                let v = &vals[0];
                if v == &Value::Null {
                    return Ok(Value::Null);
                }
                let secs = match v {
                    Value::SmallInt(_)
                    | Value::Int(_)
                    | Value::BigInt(_)
                    | Value::Numeric(_)
                    | Value::Float4(_)
                    | Value::Float(_) => to_f64v(v),
                    other => return Err(func_arg_err(name, other)),
                };
                let micros = secs * 1_000_000.0;
                if !micros.is_finite() || micros.abs() >= i64::MAX as f64 {
                    return Err(exec_err("22008", "timestamp out of range"));
                }
                return Ok(Value::Timestamptz(micros.round() as i64));
            }
            let input = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let fmt = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            match crate::datetime::to_timestamp_parsed(input, fmt) {
                Ok(m) => Ok(Value::Timestamp(m)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        // v0.17: format a date/timestamp/timestamptz with the same
        // documented pattern subset; renders in UTC (server is
        // UTC-only).
        "to_char" => {
            let v = &vals[0];
            if v == &Value::Null {
                return Ok(Value::Null);
            }
            let fmt = match str_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            let (days, tod) = match v {
                Value::Date(d) => (i64::from(*d), 0),
                Value::Timestamp(m) | Value::Timestamptz(m) => crate::datetime::split_micros(*m),
                // v0.26: numeric to_char overloads (PG's numeric_to_char /
                // float8_to_char). v0.93: integer inputs take the checked
                // int4/int8 path so a `V` (multi) picture reproduces PG's
                // overflow error instead of silently widening.
                Value::SmallInt(i) => {
                    return int_to_char(i128::from(*i), 32, fmt);
                }
                Value::Int(i) => {
                    return int_to_char(i128::from(*i), 32, fmt);
                }
                Value::BigInt(i) => {
                    return int_to_char(i128::from(*i), 64, fmt);
                }
                Value::Numeric(n) => return num_to_char(n, fmt, name),
                Value::Float4(f) => {
                    return float4_to_char(*f, fmt, name);
                }
                Value::Float(f) => return float_to_char(*f, fmt, name),
                other => return Err(func_arg_err(name, other)),
            };
            match crate::datetime::format_with_pattern(days, tod, fmt) {
                Ok(s) => Ok(Value::text(s)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        // v0.17: make_date(y, m, d) -> date; out-of-range parts are 22008.
        "make_date" => {
            let y = match int_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let m = match int_arg(name, &vals[1])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            let d = match int_arg(name, &vals[2])? {
                None => return Ok(Value::Null),
                Some(n) => n,
            };
            match crate::datetime::make_date_checked(y, m, d) {
                Ok(days) => Ok(Value::Date(days)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        // v0.17: make_timestamp(y, mo, d, h, mi, seconds float8) ->
        // timestamp. There is no TIME type in the engine, so
        // make_time() stays 42883 (documented).
        "make_timestamp" => {
            let mut ints = [0i64; 5];
            for (i, slot) in ints.iter_mut().enumerate() {
                *slot = match int_arg(name, &vals[i])? {
                    None => return Ok(Value::Null),
                    Some(n) => n,
                };
            }
            let secs_v = &vals[5];
            if secs_v == &Value::Null {
                return Ok(Value::Null);
            }
            let secs = match secs_v {
                Value::SmallInt(_)
                | Value::Int(_)
                | Value::BigInt(_)
                | Value::Numeric(_)
                | Value::Float4(_)
                | Value::Float(_) => to_f64v(secs_v),
                other => return Err(func_arg_err(name, other)),
            };
            let [y, mo, d, h, mi] = ints;
            match crate::datetime::make_timestamp_checked(y, mo, d, h, mi, secs) {
                Ok(m) => Ok(Value::Timestamp(m)),
                Err(e) => Err(fmt_exec_err(e)),
            }
        }
        // v0.17: timezone(text, timestamptz) -> timestamp and
        // timezone(text, timestamp) -> timestamptz. The server is
        // UTC-only, so only 'UTC' (case-insensitive) is accepted;
        // anything else is 0A000 (documented).
        "timezone" => {
            let zone = match str_arg(name, &vals[0])? {
                None => return Ok(Value::Null),
                Some(s) => s,
            };
            if !zone.eq_ignore_ascii_case("utc") {
                return Err(exec_err(
                    "0A000",
                    format!("time zone {zone:?} is not supported (server is UTC-only)"),
                ));
            }
            let v = &vals[1];
            if v == &Value::Null {
                return Ok(Value::Null);
            }
            match v {
                Value::Timestamptz(m) => Ok(Value::Timestamp(*m)),
                Value::Timestamp(m) => Ok(Value::Timestamptz(*m)),
                other => Err(func_arg_err(name, other)),
            }
        }
        // v0.17: the engine does not pin transaction-start time, so
        // statement_timestamp() and transaction_timestamp() return the
        // execution time, exactly like clock_timestamp() — a documented
        // deviation from Postgres, where now()/transaction_timestamp()
        // are frozen at transaction start.
        // (v0.76: the precision-aware arms above handle these; this
        // duplicate arm was removed.)
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

pub(crate) fn eval_cond_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    match name {
        "coalesce" => Ok(vals
            .iter()
            .find(|v| **v != Value::Null)
            .cloned()
            .unwrap_or(Value::Null)),
        "nullif" => {
            let (a, b) = (&vals[0], &vals[1]);
            if a == &Value::Null || b == &Value::Null {
                return Ok(a.clone());
            }
            match cmp_ordering(a, b, CmpOp::Eq)? {
                Some(Ordering::Equal) => Ok(Value::Null),
                _ => Ok(a.clone()),
            }
        }
        // Postgres: GREATEST/LEAST ignore NULLs; all-NULL -> NULL.
        "greatest" | "least" => {
            let want_greatest = name == "greatest";
            let mut best: Option<&Value> = None;
            for v in vals {
                if v == &Value::Null {
                    continue;
                }
                match best {
                    None => best = Some(v),
                    Some(cur) => {
                        let ord = cmp_ordering(cur, v, CmpOp::Eq)?.ok_or_else(|| {
                            exec_err("XX000", "internal error: null in greatest/least")
                        })?;
                        let take = if want_greatest {
                            ord == Ordering::Less
                        } else {
                            ord == Ordering::Greater
                        };
                        if take {
                            best = Some(v);
                        }
                    }
                }
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
        _ => Err(exec_err(
            "42883",
            format!("function {}() does not exist", name),
        )),
    }
}

/// Result-column type of a built-in function call (Describe path).
pub(crate) fn func_result_type(
    name: &str,
    args: &[Expr],
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    outer: &[&[QCol]],
    ctes: &[CteDef],
) -> Result<ColType, ExecError> {
    let arg0 = || expr_type(eng, snap, own, session, schemas, outer, ctes, &args[0]);
    // v0.33: bytea-returning string functions dispatch on input type.
    let bytea_if_arg0_bytea = || match arg0() {
        Ok(ColType::Bytea) => Ok(ColType::Bytea),
        Ok(_) => Ok(ColType::Text),
        Err(e) => Err(e),
    };
    match name {
        "upper" | "lower" | "replace" | "split_part" | "concat" | "concat_ws" | "to_hex"
        | "to_oct" | "to_bin" | "left" | "right" | "reverse" | "encode" | "repeat" | "lpad"
        | "rpad" | "chr" | "initcap"
        // v1.15: quote_ident/quote_literal/quote_nullable return text.
        | "quote_ident" | "quote_literal" | "quote_nullable" => Ok(ColType::Text),
        // v0.33: substring/overlay/ltrim/rtrim/btrim return bytea for bytea input.
        "substring" | "substr" | "substring_from" | "substring_from_for" | "overlay" | "ltrim"
        | "rtrim" | "btrim" => bytea_if_arg0_bytea(),
        "decode" => Ok(ColType::Bytea),
        "crc32" | "crc32c" => Ok(ColType::BigInt),
        // v0.28: bytea bit/byte functions.
        "get_bit" | "get_byte" => Ok(ColType::Int),
        "set_bit" | "set_byte" => Ok(ColType::Bytea),
        "bit_count" => Ok(ColType::BigInt),
        "sha224" | "sha256" | "sha384" | "sha512" => Ok(ColType::Bytea),
        "strpos" => Ok(ColType::Int),
        "ascii" => Ok(ColType::Int),
        "translate" | "unistr" => Ok(ColType::Text),
        "regexp_like" => Ok(ColType::Bool),
        "pg_input_is_valid" => Ok(ColType::Bool),
        // v0.37: TOAST introspection functions.
        "pg_column_compression" => Ok(ColType::Text),
        "pg_relation_size" => Ok(ColType::BigInt),
        // v1.13: pg_size_pretty(bigint) -> text.
        "pg_size_pretty" => Ok(ColType::Text),
        "regexp_count" | "regexp_instr" => Ok(ColType::Int),
        // v0.80: `GROUPING(...)` returns integer (int4), like PG19.
        "grouping" => Ok(ColType::Int),
        "regexp_substr" | "regexp_replace" | "regexp_split_to_array" | "regexp_matches" => {
            Ok(ColType::Text)
        }
        // v0.95: `parse_ident` returns `text[]` (PG19).
        "parse_ident" => Ok(ColType::Array(crate::storage::ArrayElem::Text)),
        "similar_to" => Ok(ColType::Bool),
        "substring_similar" => Ok(ColType::Text),
        "length" | "char_length" | "character_length" | "octet_length" | "position" => {
            Ok(ColType::Int)
        }
        // v0.79: array functions (PG19 arrayfuncs.c) — the runtime
        // dispatches these in eval_func_vals, but inference fell
        // through to 42883. scalar unnest(array) yields the element
        // type.
        "array_length" | "array_lower" | "array_upper" | "cardinality" | "array_ndims" => {
            Ok(ColType::Int)
        }
        "array_dims" => Ok(ColType::Text),
        "unnest" => match arg0() {
            Ok(ColType::Array(e)) => Ok(elem_scalar_type(e)),
            Ok(other) => Err(exec_err(
                "42883",
                format!("function unnest({}) does not exist", other.sql_name()),
            )),
            Err(e) => Err(e),
        },
        "abs" | "sign" => arg0(),
        // v0.67: round is overloaded like trunc — float in -> float8
        // out (PG dround), numeric in -> numeric out (mirrors
        // eval_math_func). The two-argument form is numeric-only.
        "round" => {
            if args.len() == 2 {
                return Ok(ColType::Numeric(None));
            }
            for a in args {
                match expr_type(eng, snap, own, session, schemas, outer, ctes, a)? {
                    ColType::Float4 | ColType::Float => return Ok(ColType::Float),
                    _ => {}
                }
            }
            Ok(ColType::Numeric(None))
        }
        "mod" => Ok(ColType::Numeric(None)),
        "floor" | "ceil" | "ceiling" => match arg0()? {
            ColType::Float4 => Ok(ColType::Float4),
            ColType::Float => Ok(ColType::Float),
            _ => Ok(ColType::Numeric(None)),
        },
        // Any float argument -> Float, else Numeric (documented:
        // Postgres returns numeric for sqrt(real)).
        "sqrt" | "power" => {
            for a in args {
                match expr_type(eng, snap, own, session, schemas, outer, ctes, a)? {
                    ColType::Float4 | ColType::Float => return Ok(ColType::Float),
                    _ => {}
                }
            }
            Ok(ColType::Numeric(None))
        }
        // v0.18: exp/ln/log return numeric (or float if any arg is float).
        "exp" | "ln" | "log" => {
            for a in args {
                match expr_type(eng, snap, own, session, schemas, outer, ctes, a)? {
                    ColType::Float4 | ColType::Float => return Ok(ColType::Float),
                    _ => {}
                }
            }
            Ok(ColType::Numeric(None))
        }
        // v0.18: numeric function batch. These must mirror eval_math_func's
        // actual return types exactly: without them, type resolution raises
        // 42883 before evaluation is ever reached.
        "cbrt" | "degrees" | "radians" => {
            for a in args {
                match expr_type(eng, snap, own, session, schemas, outer, ctes, a)? {
                    ColType::Float4 | ColType::Float => return Ok(ColType::Float),
                    _ => {}
                }
            }
            Ok(ColType::Numeric(None))
        }
        "factorial" | "gcd" | "lcm" | "pi" | "trim_scale" | "div" | "numeric_inc" => {
            Ok(ColType::Numeric(None))
        }
        // v0.22: trunc is overloaded like Postgres — numeric in ->
        // numeric out, float in -> float out (mirrors eval_math_func).
        // v0.56: the two-argument form is PG's trunc(numeric, int) ->
        // numeric (a float input goes through numeric, like round's
        // two-argument form).
        "trunc" => {
            if args.len() == 2 {
                return Ok(ColType::Numeric(None));
            }
            for a in args {
                match expr_type(eng, snap, own, session, schemas, outer, ctes, a)? {
                    ColType::Float4 | ColType::Float => return Ok(ColType::Float),
                    _ => {}
                }
            }
            Ok(ColType::Numeric(None))
        }
        "scale" | "min_scale" | "width_bucket" => Ok(ColType::Int),
        // v0.64: pg_lsn returns display text (pg_lsn type not yet a Value).
        "pg_lsn" => Ok(ColType::PgLsn), // v0.64: native pg_lsn type (OID 3220)
        // v0.73: row_to_json returns json (OID 114).
        "row_to_json" => Ok(ColType::Json),
        // v0.76: pg_typeof returns regtype (reported as text); json_array
        // returns json (OID 114).
        "pg_typeof" => Ok(ColType::Text),
        "json_array" => Ok(ColType::Json),
        // v0.91: hidden quantified-array-comparison desugar; boolean.
        "__any_all_array" => Ok(ColType::Bool),
        "random" => Ok(ColType::Float),
        // v0.21: float8 transcendental functions always return float8.
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "atan2" | "sinh" | "cosh" | "tanh"
        | "asinh" | "acosh" | "atanh" | "erf" | "erfc" | "gamma" | "lgamma" | "sind" | "cosd"
        | "tand" | "cotd" | "asind" | "acosd" | "atand" | "atan2d" | "log10" => Ok(ColType::Float),
        // v0.21: float8send returns bytea.
        "float8send" => Ok(ColType::Bytea),
        // v0.58: float4send returns bytea; float4recv returns float4;
        // float8recv returns float8.
        "float4send" => Ok(ColType::Bytea),
        "float4recv" => Ok(ColType::Float4),
        "float8recv" => Ok(ColType::Float),
        // setseed() returns void in Postgres (OID 2278); rustgres has no void
        // ColType and the implementation always returns NULL, so declare Text
        // like the NULL literal type inference does (see Value::Null arm).
        "setseed" => Ok(ColType::Text),
        "now" | "current_timestamp" => Ok(ColType::Timestamptz),
        // v0.17: transaction/clock timestamps are timestamptz.
        "clock_timestamp" | "statement_timestamp" | "transaction_timestamp" => {
            Ok(ColType::Timestamptz)
        }
        "current_date" => Ok(ColType::Date),
        "date_trunc" => match expr_type(eng, snap, own, session, schemas, outer, ctes, &args[1])? {
            ColType::Timestamptz => Ok(ColType::Timestamptz),
            _ => Ok(ColType::Timestamp),
        },
        // v0.17: date/time built-in batch.
        "date_part" => Ok(ColType::Numeric(None)),
        "to_date" | "make_date" => Ok(ColType::Date),
        "to_timestamp" => Ok(ColType::Timestamptz),
        "to_char" => Ok(ColType::Text),
        // v0.26: to_number(text, text) returns numeric.
        "to_number" => Ok(ColType::Numeric(None)),
        // v0.17: version() returns text.
        "version" => Ok(ColType::Text),
        "make_timestamp" => Ok(ColType::Timestamp),
        "timezone" => match expr_type(eng, snap, own, session, schemas, outer, ctes, &args[1])? {
            ColType::Timestamptz => Ok(ColType::Timestamp),
            ColType::Timestamp => Ok(ColType::Timestamptz),
            // Other inputs are a runtime 42883; Describe still needs a
            // type, so fall back to timestamptz.
            _ => Ok(ColType::Timestamptz),
        },
        "coalesce" | "nullif" | "greatest" | "least" => arg0(),
        // v0.9: sequence functions return bigint (INT here).
        // v0.98: lastval() returns bigint like the other sequence functions.
        "nextval" | "currval" | "setval" | "lastval" => Ok(ColType::Int),
        // v0.14: PostgreSQL internal operator-function aliases return boolean.
        "booleq" | "boolne" | "int4eq" | "texteq" => Ok(ColType::Bool),
        // v0.45: format() returns text.
        "format" => Ok(ColType::Text),
        // v0.47: SELECT-list SRF typing mirrors the FROM-clause table
        // function (`table_function_col_types`): any numeric argument
        // resolves to numeric, any bigint argument to bigint, otherwise
        // int4 (unknown literals type as int4, like PG). Arity is
        // checked here so Describe raises 42883 like PG's analysis.
        "generate_series" => {
            if !(2..=3).contains(&args.len()) {
                return Err(exec_err(
                    "42883",
                    "function generate_series() does not exist".to_string(),
                ));
            }
            let mut ty = ColType::Int;
            for a in args {
                match expr_type(eng, snap, own, session, schemas, outer, ctes, a)? {
                    n @ ColType::Numeric(..) => return Ok(n),
                    ColType::BigInt => ty = ColType::BigInt,
                    _ => {}
                }
            }
            Ok(ty)
        }
        // v0.86: user-defined functions resolve their declared return
        // type here (Describe runs before eval_func).
        _ => {
            // v0.87: resolve overload by arity for type inference.
            if let Some(fdef) = eng
                .db
                .functions
                .get(name)
                .and_then(|ovs| ovs.iter().find(|f| f.arg_types.len() == args.len()))
            {
                // v1.38: resolve via the shared helper so named types,
                // shells, and the cstring pseudo-type type-check (the
                // old ad-hoc builtin/table checks 42883'd them).
                if let Ok(ct) =
                    resolve_func_type_name(eng, snap, own, session, &fdef.ret_type)
                {
                    return Ok(ct);
                }
            }
            Err(exec_err(
                "42883",
                format!("function {}() does not exist", name),
            ))
        }
    }
}

/// Compare two values for ORDER BY. Exact numerics (int2/int4/int8/
/// numeric) compare exactly; float4/float8 compare by `pg_float_ord`
/// (v0.94: PG19 float8_cmp_internal — NaN sorts last, -0.0 == 0.0);
/// mixed exact/float goes through f64 (a documented precision caveat).
/// Text compares byte-wise (no collation support yet), bools order
/// false < true, dates/times/bytea/uuid compare naturally. Mismatched
/// non-null types are an error, like PostgreSQL. NULL placement follows
/// PostgreSQL defaults unless overridden: NULLS LAST for ASC,
/// NULLS FIRST for DESC.
pub(crate) fn compare_values(
    a: &Value,
    b: &Value,
    desc: bool,
    nulls_first: Option<bool>,
) -> Result<Ordering, ExecError> {
    let nulls_first = nulls_first.unwrap_or(desc);
    let ord = match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            return Ok(if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        (_, Value::Null) => {
            return Ok(if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            });
        }
        (x, y) if is_exact_numeric(x) && is_exact_numeric(y) => {
            match (exact_as_i64(x), exact_as_i64(y)) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => exact_numeric(x).cmp(&exact_numeric(y)),
            }
        }
        (Value::Float4(x), Value::Float4(y)) => pg_float_ord(*x as f64, *y as f64),
        (Value::Float4(x), Value::Float(y)) => pg_float_ord(*x as f64, *y),
        (Value::Float(x), Value::Float4(y)) => pg_float_ord(*x, *y as f64),
        (Value::Float(x), Value::Float(y)) => pg_float_ord(*x, *y),
        (x, y @ (Value::Float4(_) | Value::Float(_))) if is_exact_numeric(x) => {
            pg_float_ord(exact_to_f64(x), float_val(y))
        }
        (x @ (Value::Float4(_) | Value::Float(_)), y) if is_exact_numeric(y) => {
            pg_float_ord(float_val(x), exact_to_f64(y))
        }
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        // v0.35: bpchar ordering ignores trailing spaces (like
        // cmp_ordering above).
        (Value::BpChar(x), Value::BpChar(y)) => {
            crate::storage::rtrim_spaces(x).cmp(crate::storage::rtrim_spaces(y))
        }
        (Value::BpChar(x), Value::Text(y)) => {
            crate::storage::rtrim_spaces(x).cmp(crate::storage::rtrim_spaces(y))
        }
        (Value::Text(x), Value::BpChar(y)) => {
            crate::storage::rtrim_spaces(x).cmp(crate::storage::rtrim_spaces(y))
        }
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x.cmp(y),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        // v0.36: PG's "char" ordering is a plain byte comparison.
        (Value::SingleChar(x), Value::SingleChar(y)) => x.cmp(y),
        // v0.82: PG19 record ordering for ORDER BY — lexicographic over
        // fields with NULL sorting larger (PG's default ASC NULLS LAST),
        // mirroring `cmp_records`' comparison semantics as a total order.
        (Value::Record(fa), Value::Record(fb)) => cmp_record_values(fa, fb)?,
        _ => {
            return Err(exec_err(
                "42804",
                format!(
                    "ORDER BY cannot compare {} with {}",
                    value_type_name(a),
                    value_type_name(b)
                ),
            ));
        }
    };
    Ok(if desc { ord.reverse() } else { ord })
}

/// v0.82: total lexicographic ordering over record fields for ORDER BY.
/// NULL fields sort larger (PG's default ASC NULLS LAST); nested records
/// recurse. Mismatched field counts are 42883, like `cmp_records`.
pub(crate) fn cmp_record_values(
    fa: &[(String, Value)],
    fb: &[(String, Value)],
) -> Result<Ordering, ExecError> {
    if fa.len() != fb.len() {
        return Err(exec_err(
            "42883",
            "cannot compare records with different field counts".to_string(),
        ));
    }
    for ((_, va), (_, vb)) in fa.iter().zip(fb.iter()) {
        let ord = match (va, vb) {
            (Value::Null, Value::Null) => continue,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            (Value::Record(rfa), Value::Record(rfb)) => cmp_record_values(rfa, rfb)?,
            _ => match cmp_ordering(va, vb, CmpOp::Lt)? {
                // None only arises for NULL operands, handled above.
                Some(o) => o,
                None => continue,
            },
        };
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// Exact numeric kinds: int2/int4/int8/numeric.
pub(crate) fn is_exact_numeric(v: &Value) -> bool {
    matches!(
        v,
        Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) | Value::Numeric(_)
    )
}

/// Lift an exact numeric to a canonical Numeric for comparison.
pub(crate) fn exact_numeric(v: &Value) -> Numeric {
    match v {
        Value::SmallInt(i) => Numeric::new(*i as i128, 0),
        Value::Int(i) => Numeric::new(*i as i128, 0),
        Value::BigInt(i) => Numeric::new(*i as i128, 0),
        Value::Numeric(n) => n.clone(),
        _ => Numeric::zero(),
    }
}

/// The i64 value of an exact numeric, when it is a plain integer type.
/// SmallInt/Int/BigInt all fit in i64, so same- and mixed-width integer
/// comparisons reduce to one integer compare instead of NUMERIC
/// normalization (which needs i128 checked arithmetic). Mirrors
/// `index::exact_as_i64`, the same fast path already shipped for B-tree
/// key comparison.
pub(crate) fn exact_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::SmallInt(i) => Some(*i as i64),
        Value::Int(i) => Some(*i as i64),
        Value::BigInt(i) => Some(*i),
        _ => None,
    }
}

pub(crate) fn exact_to_f64(v: &Value) -> f64 {
    match v {
        Value::SmallInt(i) => *i as f64,
        Value::Int(i) => *i as f64,
        Value::BigInt(i) => *i as f64,
        Value::Numeric(n) => n.to_f64(),
        _ => f64::NAN,
    }
}

pub(crate) fn float_val(v: &Value) -> f64 {
    match v {
        Value::Float4(f) => *f as f64,
        Value::Float(f) => *f,
        _ => f64::NAN,
    }
}

use std::borrow::Cow;

pub(crate) fn value_type_name(v: &Value) -> Cow<'static, str> {
    match v {
        Value::Tid(_, _) => Cow::Borrowed("tid"), // v1.40
        Value::SmallInt(_) => Cow::Borrowed("smallint"),
        Value::Int(_) => Cow::Borrowed("integer"),
        Value::BigInt(_) => Cow::Borrowed("bigint"),
        Value::Float4(_) => Cow::Borrowed("real"),
        Value::Float(_) => Cow::Borrowed("float"),
        Value::Numeric(_) => Cow::Borrowed("numeric"),
        Value::Text(_) => Cow::Borrowed("text"),
        Value::BpChar(_) => Cow::Borrowed("character"), // v0.35
        Value::SingleChar(_) => Cow::Borrowed("\"char\""), // v0.36
        Value::Bool(_) => Cow::Borrowed("boolean"),
        Value::Date(_) => Cow::Borrowed("date"),
        Value::Timestamp(_) => Cow::Borrowed("timestamp"),
        Value::Timestamptz(_) => Cow::Borrowed("timestamptz"),
        Value::Bytea(_) => Cow::Borrowed("bytea"),
        Value::BitString(_) => Cow::Borrowed("bit"), // v1.39
        Value::Uuid(_) => Cow::Borrowed("uuid"),
        Value::PgLsn(_) => Cow::Borrowed("pg_lsn"), // v0.64
        Value::Record(_) => Cow::Borrowed("record"), // v0.73
        // v0.79: arrays report their element type, like pg_typeof.
        Value::Array(a) => Cow::Owned(format!("{}[]", a.elem.sql_name())),
        Value::Null => Cow::Borrowed("null"),
    }
}
