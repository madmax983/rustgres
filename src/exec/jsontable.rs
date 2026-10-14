// ---------------------------------------------------------------------------
// v1.89: JSON_TABLE (SQL/JSON table function, PG19 gram.y JsonTable +
// utils/adt/jsonfuncs.c JsonTable* + executor/execExprInterp.c
// JsonCoercion). A FROM-clause table function over the v1.85 jsonpath
// engine: the root path's item sequence is the row source; each
// COLUMNS node expands its context item into rows — plain columns
// evaluate once per context item, NESTED PATH subtrees cross-product
// (an empty nested sequence yields one all-NULL sub-row). Column
// conversion reuses the v1.84 populate core (populate_typed), the
// named-type cast path (eval_cast_named: domains, composites, user
// ranges), and assignment coercion (coerce_value) for behavior
// expressions. Behavior kinds match exec/json.rs: 0 ERROR, 1 NULL,
// 2 DEFAULT, 3 EMPTY, 4 EMPTY ARRAY, 5 EMPTY OBJECT, 6 TRUE, 7 FALSE,
// 8 UNKNOWN.
// ---------------------------------------------------------------------------
use super::*;

pub(crate) const JT_UNKNOWN: i64 = 8;

/// Validate the spec's shared namespace (root path name, every column
/// name, every nested path name — PG19: "duplicate JSON_TABLE column
/// or path name: %s", 42P09) and resolve the static output schema.
/// Shared by the Describe path and execution so both agree.
pub(crate) fn jt_validate_names(spec: &JsonTableSpec) -> Result<(), ExecError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut dup = |n: &str| -> Result<(), ExecError> {
        if !seen.insert(n.to_string()) {
            return Err(exec_err(
                "42P09",
                format!("duplicate JSON_TABLE column or path name: {n}"),
            ));
        }
        Ok(())
    };
    if let Some(p) = &spec.path_name {
        dup(p)?;
    }
    fn walk(cols: &[JtColumn], seen: &mut HashSet<String>) -> Result<(), ExecError> {
        for c in cols {
            match c {
                JtColumn::Ordinality { name }
                | JtColumn::Regular { name, .. }
                | JtColumn::Exists { name, .. } => {
                    if !seen.insert(name.clone()) {
                        return Err(exec_err(
                            "42P09",
                            format!("duplicate JSON_TABLE column or path name: {name}"),
                        ));
                    }
                }
                JtColumn::Nested {
                    path_name, columns, ..
                } => {
                    if let Some(p) = path_name {
                        if !seen.insert(p.clone()) {
                            return Err(exec_err(
                                "42P09",
                                format!("duplicate JSON_TABLE column or path name: {p}"),
                            ));
                        }
                    }
                    walk(columns, seen)?;
                }
            }
        }
        Ok(())
    }
    walk(&spec.columns, &mut seen)
}

/// The static (plan-time) type of one leaf column: named types resolve
/// against the catalog (domain -> its base, composite -> Composite,
/// user range -> Range/Multirange); unknown names are 42704 like PG's
/// type lookup. Ordinality is bigint (PG19).
pub(crate) fn jt_static_leaf_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    ty: ColType,
    named: &Option<String>,
) -> Result<ColType, ExecError> {
    let _ = (snap, own, session);
    match (ty, named) {
        (_, Some(name)) => {
            if let Some(st) = eng.db.types.get(name) {
                if let Some(dom) = &st.domain {
                    return Ok(match &dom.base {
                        ColType::Composite => ColType::Composite,
                        b => *b,
                    });
                }
                if st.composite.is_some() {
                    return Ok(ColType::Composite);
                }
                if let Some((elem, multi)) = st.range {
                    return Ok(if multi {
                        ColType::Multirange(elem)
                    } else {
                        ColType::Range(elem)
                    });
                }
                // A table rowtype name (CREATE TYPE AS over a table is
                // not a thing, but a table's own name is a composite).
                return Ok(ColType::Composite);
            }
            if eng.db.find_table(name, snap, &[own], session).is_some() {
                return Ok(ColType::Composite);
            }
            Err(exec_err("42704", format!("type \"{name}\" does not exist")))
        }
        (t, None) => Ok(t),
    }
}

/// Static output schema of a JSON_TABLE FROM item (Describe path and
/// the executor agree through this one function).
pub(crate) fn jt_schema(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    spec: &JsonTableSpec,
    alias: &Option<String>,
    col_aliases: &[String],
) -> Result<Vec<QCol>, ExecError> {
    jt_validate_names(spec)?;
    let mut flat: Vec<&JtColumn> = Vec::new();
    spec.flat_columns(&mut flat);
    let qual = alias.clone().unwrap_or_else(|| "json_table".to_string());
    // PG19 alias arity: "JSON_TABLE function has %d columns available
    // but %d columns specified" (42601).
    if col_aliases.len() > flat.len() {
        return Err(exec_err(
            "42601",
            format!(
                "JSON_TABLE function has {} columns available but {} columns specified",
                flat.len(),
                col_aliases.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(flat.len());
    for (i, c) in flat.iter().enumerate() {
        let (name, ty) = match c {
            JtColumn::Ordinality { name } => (name.clone(), ColType::BigInt),
            JtColumn::Regular {
                name, ty, named, ..
            }
            | JtColumn::Exists {
                name, ty, named, ..
            } => (
                name.clone(),
                jt_static_leaf_type(eng, snap, own, session, *ty, named)?,
            ),
            JtColumn::Nested { .. } => unreachable!(),
        };
        out.push(QCol {
            qual: qual.clone(),
            name: col_aliases.get(i).cloned().unwrap_or(name),
            ty,
            hidden: false,
            src_ord: i as u32,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Behavior values
// ---------------------------------------------------------------------

/// Coerce an already-evaluated SQL value (a DEFAULT behavior
/// expression, or a literal behavior constant) to a column's type with
/// PG's JsonExpr assignment semantics. Domain columns coerce to the
/// base then run the domain checks (eval_cast_named); a bit(n) column
/// checks the value's bit length against the typmod.
#[allow(clippy::too_many_arguments)]
pub(crate) fn jt_coerce_sql_value(
    q: &mut Q,
    v: Value,
    ty: ColType,
    named: &Option<String>,
    bit_len: Option<i32>,
) -> Result<Value, ExecError> {
    if v == Value::Null {
        // A NULL behavior value still runs a domain's NOT NULL check.
        if let Some(name) = named {
            if q.eng
                .db
                .types
                .get(name)
                .is_some_and(|st| st.domain.is_some())
            {
                return check_domain_value(q, name, &v, false);
            }
        }
        return Ok(v);
    }
    let out = if let Some(name) = named {
        eval_cast_named(q, &v, name)?
    } else {
        coerce_value(v, &ty, "")?
    };
    if let (Some(n), Value::BitString(bs)) = (bit_len, &out) {
        if bs.bitlen as i32 != n {
            return Err(exec_err(
                "22026",
                format!(
                    "bit string length {} does not match type bit({})",
                    bs.bitlen, n
                ),
            ));
        }
    }
    Ok(out)
}

/// Apply a behavior clause after an ON EMPTY / ON ERROR trigger.
/// `clause` is "ON EMPTY" or "ON ERROR" (for PG's error text when a
/// DEFAULT coercion fails: 42804 "could not coerce ... to the
/// RETURNING type").
#[allow(clippy::too_many_arguments)]
pub(crate) fn jt_apply_behavior(
    q: &mut Q,
    scopes: &[Scope],
    b: &JtBehavior,
    clause: &str,
    ty: ColType,
    named: &Option<String>,
    bit_len: Option<i32>,
) -> Result<Value, ExecError> {
    match b.kind {
        1 => Ok(Value::Null),
        2 => {
            let v = eval_expr(q, scopes, &b.expr)?;
            jt_coerce_sql_value(q, v, ty, named, bit_len).map_err(|e| {
                exec_err(
                    "42804",
                    format!(
                        "could not coerce {clause} expression (DEFAULT) to the RETURNING type: {}",
                        e.message
                    ),
                )
            })
        }
        3 => Ok(Value::Null),
        4 => jt_convert_item(
            q,
            &JsonbVal::Array(Vec::new()),
            ty,
            named,
            bit_len,
            false,
            None,
        )
        .map_err(|e| {
            exec_err(
                "42804",
                format!(
                    "could not coerce {clause} expression (EMPTY ARRAY) to the RETURNING type: {}",
                    e.message
                ),
            )
        }),
        5 => jt_convert_item(
            q,
            &JsonbVal::Object(Vec::new()),
            ty,
            named,
            bit_len,
            false,
            None,
        )
        .map_err(|e| {
            exec_err(
                "42804",
                format!(
                    "could not coerce {clause} expression (EMPTY OBJECT) to the RETURNING type: {}",
                    e.message
                ),
            )
        }),
        _ => Err(exec_err(
            "XX000",
            format!("JSON_TABLE behavior kind {} not applicable here", b.kind),
        )),
    }
}

// ---------------------------------------------------------------------
// Item conversion (regular columns)
// ---------------------------------------------------------------------

/// Convert one jsonpath item to a regular column's value. PG19
/// jsonfuncs.c JsonTableColumnOutput + the populate_record conversion
/// core: FORMAT JSON re-renders first; json/jsonb targets take the
/// item as-is (modulo QUOTES on string items); arrays/composites go
/// through populate_typed; scalars convert through their text form
/// with type INPUT semantics (populate_typed's scalar arm).
#[allow(clippy::too_many_arguments)]
pub(crate) fn jt_convert_item(
    q: &mut Q,
    item: &JsonbVal,
    ty: ColType,
    named: &Option<String>,
    bit_len: Option<i32>,
    format_json: bool,
    quotes: Option<bool>,
) -> Result<Value, ExecError> {
    // Domain columns: convert to the base type, then check. (The
    // composite-base case rides base_named.) A boolean item is first
    // materialized as its "true"/"false" string: PG coerces a bool
    // item to a domain through IO conversion, unlike a plain string
    // column, which receives boolout's 't'/'f' (see the scalar arm).
    if let Some(name) = named {
        if let Some(dom) = q.eng.db.types.get(name).and_then(|st| st.domain.clone()) {
            let (bty, bnamed) = match &dom.base {
                ColType::Composite => (ColType::Composite, dom.base_named.clone()),
                b => (*b, dom.base_domain.clone()),
            };
            if let JsonbVal::Bool(b) = item {
                let s = JsonbVal::Str(if *b { "true" } else { "false" }.to_string());
                let v = jt_convert_item(q, &s, bty, &bnamed, bit_len, format_json, quotes)?;
                return check_domain_value(q, name, &v, false);
            }
            let v = jt_convert_item(q, item, bty, &bnamed, bit_len, format_json, quotes)?;
            return check_domain_value(q, name, &v, false);
        }
    }
    if format_json {
        // The item as JSON text: a string item renders WITH quotes
        // unless OMIT QUOTES (then its raw content is the text).
        let rendered = match (item, quotes) {
            (JsonbVal::Str(s), Some(false)) => s.clone(),
            _ => item.render(),
        };
        return match ty {
            ColType::Json => Ok(Value::text(rendered)),
            ColType::Jsonb => Ok(Value::jsonb(parse_jsonb(&rendered)?.render())),
            ColType::Text => Ok(Value::text(rendered)),
            ColType::Char(_) | ColType::Varchar(_) => {
                // PG converts the formatted text through the type's
                // assignment semantics (overlength is an error).
                let f = PopField {
                    name: String::new(),
                    ty,
                    named: named.clone(),
                    sub: None,
                };
                populate_typed(q, &f, None, &JsonbVal::Str(rendered))
            }
            _ => {
                // Re-parse the formatted text and convert normally.
                let reparsed = parse_jsonb(&rendered)?;
                jt_convert_item(q, &reparsed, ty, named, bit_len, false, Some(true))
            }
        };
    }
    match ty {
        ColType::Json => match (item, quotes) {
            (JsonbVal::Str(s), Some(false)) => {
                // OMIT QUOTES: the string content must itself be JSON.
                let v = parse_jsonb(s)?;
                Ok(Value::text(v.render()))
            }
            _ => Ok(Value::text(item.render())),
        },
        ColType::Jsonb => match (item, quotes) {
            (JsonbVal::Str(s), Some(false)) => {
                let v = parse_jsonb(s)?;
                Ok(Value::jsonb(v.render()))
            }
            _ => Ok(Value::jsonb(item.render())),
        },
        ColType::Array(_) | ColType::Composite => {
            let f = PopField {
                name: String::new(),
                ty,
                named: named.clone(),
                sub: None,
            };
            populate_typed(q, &f, None, item)
        }
        _ => {
            // Scalar target: a JSON array/object item is an error
            // (PG19: "SQL/JSON scalar required", 2203F) — it must NOT
            // fall through to populate_typed's render-as-text path.
            if matches!(item, JsonbVal::Array(_) | JsonbVal::Object(_)) {
                return Err(exec_err("2203F", "SQL/JSON scalar required".to_string()));
            }
            // A boolean item landing in a plain string column renders
            // as boolout's 't'/'f' (PG19 JsonTable quirk, proven by
            // the json_table_test expected output: text/char columns
            // show f/t while a text domain shows false/true — the
            // domain path above materializes the full string first).
            if let JsonbVal::Bool(b) = item {
                if matches!(ty, ColType::Text | ColType::Char(_) | ColType::Varchar(_)) {
                    let f = PopField {
                        name: String::new(),
                        ty,
                        named: named.clone(),
                        sub: None,
                    };
                    return populate_typed(
                        q,
                        &f,
                        None,
                        &JsonbVal::Str(if *b { "t" } else { "f" }.to_string()),
                    );
                }
            }
            // Scalar: text form per the QUOTES clause (KEEP QUOTES on
            // a string item yields its rendered, quoted form; the
            // default for scalar columns is the raw string), then the
            // populate scalar conversion (input semantics; char /
            // varchar via assignment semantics).
            if let JsonbVal::Str(s) = item {
                if quotes == Some(true) {
                    let rendered = item.render();
                    let f = PopField {
                        name: String::new(),
                        ty,
                        named: named.clone(),
                        sub: None,
                    };
                    return populate_typed(q, &f, None, &JsonbVal::Str(rendered));
                }
                let _ = s;
            }
            let f = PopField {
                name: String::new(),
                ty,
                named: named.clone(),
                sub: None,
            };
            let v = populate_typed(q, &f, None, item)?;
            if let (Some(n), Value::BitString(bs)) = (bit_len, &v) {
                if bs.bitlen as i32 != n {
                    return Err(exec_err(
                        "22026",
                        format!(
                            "bit string length {} does not match type bit({})",
                            bs.bitlen, n
                        ),
                    ));
                }
            }
            Ok(v)
        }
    }
}

// ---------------------------------------------------------------------
// Column evaluation
// ---------------------------------------------------------------------

struct JtCtx<'a> {
    vars: Option<&'a JsonbVal>,
}

/// Evaluate one regular column against a context item.
#[allow(clippy::too_many_arguments)]
fn jt_eval_regular(
    q: &mut Q,
    scopes: &[Scope],
    ctx: &JtCtx,
    name: &str,
    ty: ColType,
    named: &Option<String>,
    bit_len: Option<i32>,
    format_json: bool,
    path_str: &str,
    wrapper: i64,
    quotes: Option<bool>,
    on_empty: &JtBehavior,
    on_error: &JtBehavior,
    item_ctx: &JsonbVal,
) -> Result<Value, ExecError> {
    let path = match super::jsonpath::parse_jsonpath(path_str) {
        Ok(p) => p,
        Err(e) => {
            return match on_error.kind {
                0 => Err(e),
                _ => jt_apply_behavior(q, scopes, on_error, "ON ERROR", ty, named, bit_len),
            };
        }
    };
    let seq = match super::jsonpath::jp_query_seq(item_ctx, &path, ctx.vars) {
        Ok(s) => s,
        Err(e) => {
            return match on_error.kind {
                0 => Err(e),
                _ => jt_apply_behavior(q, scopes, on_error, "ON ERROR", ty, named, bit_len),
            };
        }
    };
    if seq.is_empty() {
        return match on_empty.kind {
            0 => Err(exec_err(
                "22035",
                format!("no SQL/JSON item found for specified path of column \"{name}\""),
            )),
            _ => jt_apply_behavior(q, scopes, on_empty, "ON EMPTY", ty, named, bit_len),
        };
    }
    // Wrapper handling: WITHOUT requires a single item; WITH
    // UNCONDITIONAL wraps the whole sequence; WITH CONDITIONAL wraps
    // unless the single item is already an array.
    let item: JsonbVal = match wrapper {
        1 => JsonbVal::Array(seq),
        2 => {
            if seq.len() == 1 && matches!(seq[0], JsonbVal::Array(_)) {
                seq.into_iter().next().unwrap()
            } else {
                JsonbVal::Array(seq)
            }
        }
        _ => {
            if seq.len() > 1 {
                let e = exec_err(
                    "22023",
                    format!(
                        "JSON path expression for column \"{name}\" must return single item when no wrapper is requested"
                    ),
                );
                return match on_error.kind {
                    0 => Err(e),
                    _ => jt_apply_behavior(q, scopes, on_error, "ON ERROR", ty, named, bit_len),
                };
            }
            seq.into_iter().next().unwrap()
        }
    };
    // A JSON null item is SQL NULL for non-json targets (it is a
    // present item — ON EMPTY does not apply), the JSON null itself
    // for json/jsonb targets, and the string "null" under FORMAT
    // JSON for text targets.
    if matches!(item, JsonbVal::Null) && !matches!(ty, ColType::Json | ColType::Jsonb) {
        if format_json && matches!(ty, ColType::Text | ColType::Char(_) | ColType::Varchar(_)) {
            // Falls through to conversion, which renders "null".
        } else {
            return Ok(Value::Null);
        }
    }
    match jt_convert_item(q, &item, ty, named, bit_len, format_json, quotes) {
        Ok(v) => Ok(v),
        Err(e) => match on_error.kind {
            0 => Err(e),
            _ => jt_apply_behavior(q, scopes, on_error, "ON ERROR", ty, named, bit_len),
        },
    }
}

/// Convert a boolean EXISTS result to the column type (PG19's
/// JsonExpr coercion: bool casts exist only to int4/text/varchar/
/// bpchar; json/jsonb keep the JSON boolean; domains ride their base;
/// everything else goes through IO conversion of "true"/"false",
/// which fails for int2/int8/float4/... exactly as PG's does).
fn jt_convert_bool(
    q: &mut Q,
    b: bool,
    ty: ColType,
    named: &Option<String>,
    bit_len: Option<i32>,
) -> Result<Value, ExecError> {
    if let Some(name) = named {
        if let Some(dom) = q.eng.db.types.get(name).and_then(|st| st.domain.clone()) {
            let (bty, bnamed) = match &dom.base {
                ColType::Composite => (ColType::Composite, dom.base_named.clone()),
                b => (*b, dom.base_domain.clone()),
            };
            let v = jt_convert_bool(q, b, bty, &bnamed, bit_len)?;
            return check_domain_value(q, name, &v, false);
        }
        if q.eng
            .db
            .types
            .get(name)
            .is_some_and(|st| st.composite.is_some())
        {
            return Err(exec_err(
                "42804",
                "cannot coerce boolean to a composite type".to_string(),
            ));
        }
    }
    match ty {
        ColType::Bool => Ok(Value::Bool(b)),
        ColType::Int => Ok(Value::Int(b as i64)),
        ColType::Text => Ok(Value::text(if b { "true" } else { "false" })),
        ColType::Varchar(n) => eval_char_assign(if b { "true" } else { "false" }, n, false),
        ColType::Char(n) => eval_char_assign(if b { "true" } else { "false" }, n, true),
        ColType::Json => Ok(Value::text(if b { "true" } else { "false" })),
        ColType::Jsonb => Ok(Value::jsonb(if b { "true" } else { "false" })),
        _ => {
            // IO conversion fallback: boolout text into the target's
            // input function (the populate scalar path).
            let f = PopField {
                name: String::new(),
                ty,
                named: named.clone(),
                sub: None,
            };
            populate_typed(q, &f, None, &JsonbVal::Bool(b))
        }
    }
}

/// Evaluate one EXISTS column against a context item.
#[allow(clippy::too_many_arguments)]
fn jt_eval_exists(
    q: &mut Q,
    scopes: &[Scope],
    ctx: &JtCtx,
    ty: ColType,
    named: &Option<String>,
    bit_len: Option<i32>,
    path_str: &str,
    on_error: &JtBehavior,
    item_ctx: &JsonbVal,
) -> Result<Value, ExecError> {
    let parsed = super::jsonpath::parse_jsonpath(path_str);
    let result = match parsed {
        Ok(p) => super::jsonpath::jp_exists(item_ctx, &p, ctx.vars),
        Err(e) => Err(e),
    };
    match result {
        Ok(b) => match jt_convert_bool(q, b, ty, named, bit_len) {
            Ok(v) => Ok(v),
            // A result-conversion failure is an ON ERROR trigger in PG.
            Err(e) => jt_exists_on_error(q, scopes, on_error, ty, named, bit_len, e),
        },
        Err(e) => jt_exists_on_error(q, scopes, on_error, ty, named, bit_len, e),
    }
}

/// The ON ERROR behavior of an EXISTS column after a path or
/// conversion failure. TRUE/FALSE convert through the same bool
/// coercion (their coercibility was pre-checked); UNKNOWN is NULL;
/// DEFAULT evaluates and coerces like a regular behavior value.
fn jt_exists_on_error(
    q: &mut Q,
    scopes: &[Scope],
    on_error: &JtBehavior,
    ty: ColType,
    named: &Option<String>,
    bit_len: Option<i32>,
    orig: ExecError,
) -> Result<Value, ExecError> {
    match on_error.kind {
        0 => Err(orig),
        6 => jt_convert_bool(q, true, ty, named, bit_len),
        7 => jt_convert_bool(q, false, ty, named, bit_len),
        JT_UNKNOWN => Ok(Value::Null),
        2 => {
            let v = eval_expr(q, scopes, &on_error.expr)?;
            jt_coerce_sql_value(q, v, ty, named, bit_len)
        }
        _ => Err(orig),
    }
}

/// Pre-coerce an EXISTS column's ON ERROR behavior value (PG coerces
/// behavior expressions when the plan is built — a failure is a hard
/// 42804 even if no row ever triggers the behavior).
fn jt_precheck_exists(
    q: &mut Q,
    scopes: &[Scope],
    ty: ColType,
    named: &Option<String>,
    bit_len: Option<i32>,
    on_error: &JtBehavior,
) -> Result<(), ExecError> {
    match on_error.kind {
        6 => {
            jt_convert_bool(q, true, ty, named, bit_len).map_err(|e| {
                exec_err(
                    "42804",
                    format!(
                        "could not coerce ON ERROR expression (TRUE) to the RETURNING type: {}",
                        e.message
                    ),
                )
            })?;
        }
        7 => {
            jt_convert_bool(q, false, ty, named, bit_len).map_err(|e| {
                exec_err(
                    "42804",
                    format!(
                        "could not coerce ON ERROR expression (FALSE) to the RETURNING type: {}",
                        e.message
                    ),
                )
            })?;
        }
        2 => {
            let v = eval_expr(q, scopes, &on_error.expr)?;
            jt_coerce_sql_value(q, v, ty, named, bit_len).map_err(|e| {
                exec_err(
                    "42804",
                    format!(
                        "could not coerce ON ERROR expression (DEFAULT) to the RETURNING type: {}",
                        e.message
                    ),
                )
            })?;
        }
        _ => {}
    }
    Ok(())
}

fn jt_precheck_columns(q: &mut Q, scopes: &[Scope], cols: &[JtColumn]) -> Result<(), ExecError> {
    for c in cols {
        match c {
            JtColumn::Exists {
                ty,
                named,
                bit_len,
                on_error,
                ..
            } => jt_precheck_exists(q, scopes, *ty, named, *bit_len, on_error)?,
            JtColumn::Nested { columns, .. } => jt_precheck_columns(q, scopes, columns)?,
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Row expansion
// ---------------------------------------------------------------------

/// Expand one COLUMNS node against one context item. `ordinal` is the
/// item's 1-based position in the sequence that produced it (the
/// value of this node's FOR ORDINALITY column). Returns rows of leaf
/// cells in flattened output order.
fn jt_expand_node(
    q: &mut Q,
    scopes: &[Scope],
    ctx: &JtCtx,
    cols: &[JtColumn],
    item: &JsonbVal,
    ordinal: i64,
) -> Result<Vec<Vec<Value>>, ExecError> {
    // Leaf layout for this node: plain columns first (written order),
    // then each nested subtree's leaves (written order) — the same
    // flattening as JsonTableSpec::flat_columns.
    let plain_count = cols
        .iter()
        .filter(|c| !matches!(c, JtColumn::Nested { .. }))
        .count();
    let total = plain_count
        + cols
            .iter()
            .map(|c| match c {
                JtColumn::Nested { columns, .. } => count_leaves(columns),
                _ => 0,
            })
            .sum::<usize>();
    // Evaluate plain columns once for this context item.
    let mut plain: Vec<Value> = Vec::with_capacity(plain_count);
    for c in cols {
        match c {
            JtColumn::Ordinality { .. } => plain.push(Value::BigInt(ordinal)),
            JtColumn::Regular {
                name,
                ty,
                named,
                bit_len,
                format_json,
                path,
                wrapper,
                quotes,
                on_empty,
                on_error,
            } => plain.push(jt_eval_regular(
                q,
                scopes,
                ctx,
                name,
                *ty,
                named,
                *bit_len,
                *format_json,
                path,
                *wrapper,
                *quotes,
                on_empty,
                on_error,
                item,
            )?),
            JtColumn::Exists {
                ty,
                named,
                bit_len,
                path,
                on_error,
                ..
            } => plain.push(jt_eval_exists(
                q, scopes, ctx, *ty, named, *bit_len, path, on_error, item,
            )?),
            JtColumn::Nested { .. } => {}
        }
    }
    // Expand each nested subtree into its sub-row set. Sibling
    // NESTED paths UNION their rows (PG19 JsonTable semantics — the
    // plan-execution regress case: b=[1,2,3] and c=[10,null,20] under
    // one parent yield 3 + 3 rows, not a 9-row cross product); each
    // union row carries the plain cells and NULLs for the sibling
    // subtrees. When every nested sequence is empty the node yields
    // exactly one plain row with all nested leaves NULL.
    let mut nested_rows: Vec<Vec<Vec<Value>>> = Vec::new();
    for c in cols {
        if let JtColumn::Nested {
            path, columns: sub, ..
        } = c
        {
            let seq = match super::jsonpath::parse_jsonpath(path) {
                Ok(p) => super::jsonpath::jp_query_seq(item, &p, ctx.vars).unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let mut rows: Vec<Vec<Value>> = Vec::new();
            for (i, sub_item) in seq.iter().enumerate() {
                let mut sub_rows = jt_expand_node(q, scopes, ctx, sub, sub_item, (i + 1) as i64)?;
                rows.append(&mut sub_rows);
            }
            nested_rows.push(rows);
        }
    }
    if nested_rows.is_empty() {
        return Ok(vec![plain]);
    }
    // Leaf offsets of the nested subtrees, in written order.
    let mut offsets: Vec<usize> = Vec::with_capacity(nested_rows.len());
    {
        let mut off = plain_count;
        for c in cols {
            if let JtColumn::Nested { columns, .. } = c {
                offsets.push(off);
                off += count_leaves(columns);
            }
        }
    }
    let mut out: Vec<Vec<Value>> = Vec::new();
    for (sub_rows, slot) in nested_rows.iter().zip(offsets.iter()) {
        for sr in sub_rows {
            let mut r = Vec::with_capacity(total);
            r.extend(plain.iter().cloned());
            r.resize(total, Value::Null);
            for (j, v) in sr.iter().enumerate() {
                r[slot + j] = v.clone();
            }
            out.push(r);
        }
    }
    if out.is_empty() {
        let mut r = Vec::with_capacity(total);
        r.extend(plain.iter().cloned());
        r.resize(total, Value::Null);
        out.push(r);
    }
    Ok(out)
}

fn count_leaves(cols: &[JtColumn]) -> usize {
    let mut n = 0;
    for c in cols {
        match c {
            JtColumn::Nested { columns, .. } => n += count_leaves(columns),
            _ => n += 1,
        }
    }
    n
}

// ---------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------

/// Evaluate a JSON_TABLE FROM item: `args` are the evaluated
/// [doc, path, vars] values. Returns (schema, rows) in the item's own
/// layout (aliasing applied by the caller-visible schema helper —
/// this function returns the canonical schema from jt_schema so
/// execution and Describe agree cell-for-cell).
#[allow(clippy::too_many_arguments)]
pub(crate) fn eval_json_table(
    q: &mut Q,
    scopes: &[Scope],
    spec: &JsonTableSpec,
    args: &[Expr],
    arg_vals: &[Value],
    alias: &Option<String>,
    col_aliases: &[String],
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    let schema = jt_schema(&*q.eng, q.snap, q.own, q.session, spec, alias, col_aliases)?;
    // EXISTS behavior pre-coercion (plan-time in PG).
    jt_precheck_columns(q, scopes, &spec.columns)?;
    let _ = args;
    let doc_val = &arg_vals[0];
    if *doc_val == Value::Null {
        return Ok((schema, Vec::new()));
    }
    let doc = match jsonb_val_arg("json_table", doc_val)? {
        Some(d) => d,
        None => return Ok((schema, Vec::new())),
    };
    let path_text = match &arg_vals[1] {
        Value::Text(t) | Value::BpChar(t) => t.to_string(),
        other => return Err(func_arg_err("json_table", other)),
    };
    let vars = if arg_vals[2] == Value::Null {
        None
    } else {
        jsonb_val_arg("json_table", &arg_vals[2])?
    };
    let ctx = JtCtx {
        vars: vars.as_ref(),
    };
    // Root path evaluation, with the table-level ON ERROR behavior:
    // ERROR propagates; EMPTY yields zero rows; EMPTY ARRAY / EMPTY
    // OBJECT substitute that container as the sole context item.
    let root_items: Vec<JsonbVal> = {
        let parsed = super::jsonpath::parse_jsonpath(&path_text)
            .and_then(|p| super::jsonpath::jp_query_seq(&doc, &p, ctx.vars));
        match parsed {
            Ok(s) => s,
            Err(e) => match spec.on_error.kind {
                3 => Vec::new(),
                4 => vec![JsonbVal::Array(Vec::new())],
                5 => vec![JsonbVal::Object(Vec::new())],
                _ => return Err(e),
            },
        }
    };
    let mut rows: Vec<QRow> = Vec::new();
    for (i, item) in root_items.iter().enumerate() {
        let node_rows = jt_expand_node(q, scopes, &ctx, &spec.columns, item, (i + 1) as i64)?;
        for cells in node_rows {
            rows.push(QRow {
                cells: Row::new(cells),
                prov: Vec::new(),
            });
        }
    }
    Ok((schema, rows))
}
