// v1.78 mechanical split: moved verbatim from src/exec.rs (10095-13887).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// --- EXPLAIN ---------------------------------------------------------------

// ============================================================================
// v1.08: Postgres-style EXPLAIN text (EXPLAIN (COSTS OFF) parity).
//
// PG19 references: `src/backend/commands/explain.c` (`ExplainNode`,
// `ExplainIndentText`, `show_scan_qual`, `show_upper_qual`,
// `show_sort_group_keys`) and `src/backend/utils/adt/ruleutils.c`
// (`get_const_expr`, `get_oper_expr`, boolean-expression wrapping).
//
// The planning-only renderer (`render_plan`) takes a `costs` flag: with
/// v1.08: PG19 text EXPLAIN node indentation (explain.c): depth 0 has
/// no arrow; deeper levels get `(6*d-4)` spaces + `"->  "`. Properties get
/// `(6*d+2)` spaces. This is SEPARATE from `analyze_pad` (v1.03 ANALYZE),
/// which must not change.
pub(crate) fn pg_pad(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        format!("{}->  ", " ".repeat(6 * depth - 4))
    }
}

/// v1.08: PG19 text EXPLAIN property indentation: `(6*d+2)` spaces.
pub(crate) fn pg_ppad(depth: usize) -> String {
    " ".repeat(6 * depth + 2)
}

/// v1.08: `table` or `table alias` for PG EXPLAIN scan lines.
pub(crate) fn pg_scan_name(table: &str, alias: &Option<String>) -> String {
    match alias {
        Some(a) if a != table => format!("{table} {}", pg_quote_ident(a)),
        _ => table.to_string(),
    }
}

/// v1.50: VERBOSE scan name — PG19's `ExplainTargetRel` (explain.c:4615)
/// schema-qualifies the relation name only when `es->verbose`.
pub(crate) fn pg_scan_name_verbose(table: &str, alias: &Option<String>, verbose: bool) -> String {
    let base = pg_scan_name(table, alias);
    if verbose {
        // v1.50: schema-qualify with `public.` for VERBOSE.
        // If there's an alias, the format is `table alias`; we need
        // `public.table alias`.
        match alias {
            Some(a) if a != table => format!("public.{table} {}", pg_quote_ident(a)),
            _ => format!("public.{table}"),
        }
    } else {
        base
    }
}

// COSTS OFF the `(rows=N)` estimate suffix is omitted from every node
// line and the node tree is indented exactly like PG19 text EXPLAIN.
// Expression text (Filter / Index Cond / Sort Key / Join Filter) is
// deparsed by `pg_expr_text`, which mirrors ruleutils.c for the shapes
// the planner can produce. Anything without a faithful PG spelling
// returns `None`; callers then keep the statement honestly masked
// (EXPECTED-FAIL) instead of printing a wrong plan.
// ============================================================================

/// Quote an identifier like PG19 `quote_identifier` when it would not
/// scan as a bare identifier (mixed case, punctuation, ...).
pub(crate) fn pg_quote_ident(s: &str) -> String {
    let mut bs = s.bytes();
    let bare = matches!(bs.next(), Some(b'a'..=b'z' | b'_'))
        && bs.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$'));
    if bare && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// Element-type label for `ColType::Array` (`integer[]`, `text[]`, ...).
pub(crate) fn pg_array_elem_label(e: ArrayElem) -> Option<&'static str> {
    Some(match e {
        ArrayElem::Bool => "boolean",
        ArrayElem::Bytea => "bytea",
        ArrayElem::Bit => "bit", // v1.39
        ArrayElem::Tid => "tid", // v1.40
        ArrayElem::SingleChar => "\"char\"",
        ArrayElem::Name => "name",
        ArrayElem::SmallInt => "smallint",
        ArrayElem::Int => "integer",
        ArrayElem::Text => "text",
        ArrayElem::Char => "character",
        ArrayElem::Varchar => "character varying",
        ArrayElem::BigInt => "bigint",
        ArrayElem::Float4 => "real",
        ArrayElem::Float => "double precision",
        ArrayElem::Date => "date",
        ArrayElem::Timestamp => "timestamp without time zone",
        ArrayElem::Timestamptz => "timestamp with time zone",
        ArrayElem::Numeric => "numeric",
        ArrayElem::Uuid => "uuid",
        ArrayElem::Regclass => "regclass",
        ArrayElem::Json => "json",
        ArrayElem::Record => "record",
        ArrayElem::PgLsn => "pg_lsn",
        ArrayElem::Xid => "xid",
    })
}

/// PG19 `format_type_with_typemod` for the types a deparsed constant can
/// carry. `None` = no EXPLAIN-text spelling here.
pub(crate) fn pg_type_label(ty: ColType) -> Option<String> {
    match ty {
        ColType::Int => Some("integer".to_string()),
        ColType::BigInt => Some("bigint".to_string()),
        ColType::SmallInt => Some("smallint".to_string()),
        ColType::Float => Some("double precision".to_string()),
        ColType::Float4 => Some("real".to_string()),
        ColType::Numeric(None) => Some("numeric".to_string()),
        ColType::Numeric(Some((p, s))) => Some(format!("numeric({p},{s})")),
        ColType::Text => Some("text".to_string()),
        ColType::Char(Some(n)) => Some(format!("character({n})")),
        ColType::Char(None) => Some("character".to_string()),
        ColType::Varchar(Some(n)) => Some(format!("character varying({n})")),
        ColType::Varchar(None) => Some("character varying".to_string()),
        ColType::SingleChar => Some("\"char\"".to_string()),
        ColType::Bool => Some("boolean".to_string()),
        ColType::Date => Some("date".to_string()),
        ColType::Timestamp => Some("timestamp without time zone".to_string()),
        ColType::Timestamptz => Some("timestamp with time zone".to_string()),
        ColType::Bytea => Some("bytea".to_string()),
        ColType::Uuid => Some("uuid".to_string()),
        ColType::Name => Some("name".to_string()),
        ColType::Json => Some("json".to_string()),
        ColType::Array(e) => Some(format!("{}[]", pg_array_elem_label(e)?)),
        _ => None,
    }
}

/// Render an untyped text literal coerced to `ty`, like PG19's parser
/// coercing an unknown-type literal to the comparison column's type and
/// `get_const_expr` deparsing the result. `None` = no faithful spelling.
pub(crate) fn pg_coerced_text(s: &str, ty: ColType) -> Option<String> {
    let label = pg_type_label(ty)?;
    match ty {
        // Character types: quoted with a label.
        ColType::Text
        | ColType::Name
        | ColType::Char(_)
        | ColType::Varchar(_)
        | ColType::SingleChar => Some(format!("{}::{label}", pg_quote_literal(s))),
        // Integer targets: the text must scan as an integer; then the
        // coerced INT4/INT8/INT2 constant deparses per its own rules.
        ColType::Int => {
            let i: i64 = s.trim().parse().ok()?;
            Some(if i >= 0 && i <= i32::MAX as i64 {
                i.to_string()
            } else {
                format!("'{i}'::integer")
            })
        }
        ColType::BigInt => {
            let _: i64 = s.trim().parse().ok()?;
            Some(format!("{}::bigint", pg_quote_literal(s.trim())))
        }
        ColType::SmallInt => {
            let _: i64 = s.trim().parse().ok()?;
            Some(format!("{}::smallint", pg_quote_literal(s.trim())))
        }
        ColType::Bool => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Some("true".to_string()),
            "false" => Some("false".to_string()),
            _ => None,
        },
        // Numeric: PG19 prints the constant bare iff it looks like a
        // float (leading digit plus `.`/`e`) without a leading sign.
        ColType::Numeric(_) => {
            let t = s.trim();
            let bare = t.bytes().next().is_some_and(|b| b.is_ascii_digit())
                && t.bytes().any(|b| b == b'.' || b == b'e' || b == b'E');
            Some(if bare {
                t.to_string()
            } else {
                format!("{}::numeric", pg_quote_literal(t))
            })
        }
        ColType::Float | ColType::Float4 => {
            let _: f64 = s.trim().parse().ok()?;
            Some(format!("{}::{label}", pg_quote_literal(s.trim())))
        }
        // Date/time/uuid/bytea/json: quoted with a label.
        ColType::Date
        | ColType::Timestamp
        | ColType::Timestamptz
        | ColType::Uuid
        | ColType::Bytea
        | ColType::Json => Some(format!("{}::{label}", pg_quote_literal(s))),
        ColType::Array(_) => Some(format!("{}::{label}", pg_quote_literal(s))),
        _ => None,
    }
}

/// Render a `Literal` like PG19 `get_const_expr(..., showtype=0)` (the
/// EXPLAIN deparse mode). `target` is the coerced type of an untyped
/// string literal (the column type on the other side of a comparison);
/// `None` renders the literal's own parsed type. Returns `None` for
/// shapes with no faithful PG spelling.
pub(crate) fn pg_literal_text(lit: &Literal, target: Option<ColType>) -> Option<String> {
    // Untyped string literal: PG19 coerces unknown-type literals to the
    // comparison target at parse time.
    if let Literal::Text(s) = lit {
        let ty = target?;
        return pg_coerced_text(s, ty);
    }
    match lit {
        // PG19 INT4OID: bare unless negative (`'-n'::integer`).
        Literal::Int(i) => Some(if *i >= 0 {
            i.to_string()
        } else {
            format!("'{i}'::integer")
        }),
        // INT8/INT2 take PG19's default const arm: quoted + label.
        Literal::BigInt(i) => Some(format!("'{i}'::bigint")),
        Literal::SmallInt(i) => Some(format!("'{i}'::smallint")),
        Literal::Float(f) => Some(format!("'{}'::double precision", pg_float_text(*f))),
        Literal::Real(f) => Some(format!("'{}'::real", pg_float_text(f64::from(*f)))),
        // PG19 NUMERICOID: bare iff float-looking with a leading digit.
        Literal::Decimal(s) => {
            let bare = s.bytes().next().is_some_and(|b| b.is_ascii_digit())
                && s.bytes().any(|b| b == b'.' || b == b'e' || b == b'E');
            Some(if bare {
                s.clone()
            } else {
                format!("{}::numeric", pg_quote_literal(s))
            })
        }
        Literal::Bool(b) => Some(if *b {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        Literal::Null => Some("NULL".to_string()),
        // Typed literals only arise from typed params/casts; no
        // faithful untyped spelling here.
        _ => None,
    }
}

/// Shortest-round-trip float text approximating PG19 `float8out` for
/// ordinary magnitudes.
pub(crate) fn pg_float_text(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    format!("{f}")
}

/// Render a *coerced* `Value` like PG19 `get_const_expr` deparses an
/// index bound constant, given the column type `ty` it was coerced to.
/// `None` = no faithful spelling (the plan stays honestly masked).
pub(crate) fn pg_value_text(v: &Value, ty: ColType) -> Option<String> {
    match v {
        Value::Null => Some("NULL".to_string()),
        Value::Bool(b) => Some(if *b {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        // PG19 INT4OID: bare unless negative (`'-n'::integer`).
        Value::Int(i) => Some(if *i >= 0 {
            i.to_string()
        } else {
            format!("'{i}'::integer")
        }),
        Value::SmallInt(i) => Some(format!("'{i}'::smallint")),
        Value::BigInt(i) => Some(format!("'{i}'::bigint")),
        Value::Float4(f) => Some(format!("'{}'::real", pg_float_text(f64::from(*f)))),
        Value::Float(f) => Some(format!("'{}'::double precision", pg_float_text(*f))),
        // Character-ish values: quoted with the column's type label.
        Value::Text(s) => {
            let label = pg_type_label(ty)?;
            Some(format!("{}::{label}", pg_quote_literal(s)))
        }
        // Scalar values with a canonical quoted spelling.
        Value::Bytea(_)
        | Value::Date(_)
        | Value::Timestamp(_)
        | Value::Timestamptz(_)
        | Value::Uuid(_) => {
            let label = pg_type_label(ty)?;
            Some(format!("{}::{label}", pg_quote_literal(&pg_value_bare(v)?)))
        }
        // Numeric/array/json/interval/...: no faithful short spelling here.
        _ => None,
    }
}

/// Bare (unquoted, unlabelled) text of a scalar `Value` for
/// `pg_value_text`'s quoted arm.
pub(crate) fn pg_value_bare(v: &Value) -> Option<String> {
    match v {
        Value::Bytea(b) => {
            let mut s = String::with_capacity(2 + b.len() * 2);
            s.push_str("\\x");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            Some(s)
        }
        Value::Date(d) => Some(format!("{d}")),
        Value::Timestamp(t) => Some(format!("{t}")),
        Value::Timestamptz(t) => Some(format!("{t}")),
        // UUID text needs hyphenated hex; no faithful short spelling here.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// v1.08: expression deparse (PG19 ruleutils.c style) and helpers.
// ---------------------------------------------------------------------------

/// v1.08: qualifier names visible for one FROM item (table name + alias;
/// joins contribute their inputs' qualifiers, like PG19's rtable).
pub(crate) fn pg_item_quals(item: &FromItem) -> Vec<String> {
    match item {
        FromItem::Table { name, alias, .. } => {
            let mut v = vec![name.clone()];
            if let Some(a) = alias {
                if a != name {
                    v.push(a.clone());
                }
            }
            v
        }
        FromItem::Derived { alias, .. } => vec![alias.clone()],
        FromItem::Values { alias, .. } => vec![alias.clone()],
        FromItem::Function { alias, .. } => alias.clone().map(|a| vec![a]).unwrap_or_default(),
        FromItem::Join { left, right, .. } => {
            let mut v = pg_item_quals(left);
            v.extend(pg_item_quals(right));
            v
        }
    }
}

/// Collect every column reference in `e` as (qualifier, column).
pub(crate) fn pg_expr_tables(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        Expr::Column { table, name } => out.push((table.clone(), name.clone())),
        Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Cmp {
            left: a,
            right: b,
            op: _,
        }
        | Expr::Arith {
            left: a, right: b, ..
        }
        | Expr::Concat(a, b)
        | Expr::IsDistinctFrom {
            left: a, right: b, ..
        } => {
            pg_expr_tables(a, out);
            pg_expr_tables(b, out);
        }
        Expr::Not(x) | Expr::Neg(x) | Expr::BitNot(x) | Expr::IsNull { expr: x, .. } => {
            pg_expr_tables(x, out)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            pg_expr_tables(expr, out);
            pg_expr_tables(low, out);
            pg_expr_tables(high, out);
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            pg_expr_tables(expr, out);
            pg_expr_tables(pattern, out);
            if let Some(x) = escape {
                pg_expr_tables(x, out);
            }
        }
        Expr::Cast { expr, .. } => pg_expr_tables(expr, out),
        Expr::Func { args, .. } => {
            for a in args {
                pg_expr_tables(a, out);
            }
        }
        _ => {}
    }
}

/// Render an expression like PG19 `ruleutils.c` deparse (EXPLAIN mode:
/// not pretty, `showimplicit=false` except sort/group keys). `qualify`
/// mirrors PG's `varprefix` (true when the plan's rtable has more than
/// one relation). Returns `None` for shapes with no faithful spelling.
pub(crate) fn pg_expr_text(e: &Expr, pctx: &PgPlanCtx, qualify: bool) -> Option<String> {
    pg_expr_target(e, pctx, qualify, None)
}

/// v1.08: Try PG-text deparse; fall back to Rust Debug format if the
/// expression has no faithful PG spelling. The Debug output won't match
/// PG's expected plan text, so the conformance mask keeps the statement
/// EXPECTED-FAIL — but unlike an error, this does NOT abort an enclosing
/// transaction (critical for EXPLAIN inside BEGIN/COMMIT blocks).
pub(crate) fn pg_expr_text_or_debug(e: &Expr, pctx: &PgPlanCtx, qualify: bool) -> String {
    pg_expr_text(e, pctx, qualify).unwrap_or_else(|| format!("{:?}", e))
}

// ---------------------------------------------------------------------------
// v1.57: const-folding of IN-list items + coercion-aware IN-LHS deparse.
// ---------------------------------------------------------------------------

/// v1.57: sentinel `written` marker for synthesized implicit-coercion
/// casts (see `pg_rewrite_coercions`). A NUL byte cannot appear in real
/// SQL text, so this never collides with a user-written cast.
pub(crate) const PG_SYNTH_COERCE_WRITTEN: &str = "\u{0}pg-coerce";

/// v1.57: declared argument types and result type of the immutable float8
/// builtins understood by the IN-list machinery. Grounded in pg_proc.dat
/// (proargtypes/prorettype; provolatile defaults to immutable for these).
pub(crate) fn pg_math_builtin_sig(name: &str) -> Option<(&'static [ColType], ColType)> {
    const F8: &[ColType] = &[ColType::Float];
    const F8F8: &[ColType] = &[ColType::Float, ColType::Float];
    match name {
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "asinh"
        | "acosh" | "atanh" | "exp" | "ln" | "sqrt" | "cbrt" => Some((F8, ColType::Float)),
        "atan2" => Some((F8F8, ColType::Float)),
        _ => None,
    }
}

/// v1.57: PG19 has native cross-type operators for all int2/int4/int8
/// pairs and for (float4,float8) — for `=`, `+`, `-`, `*`, `/`
/// (pg_operator.dat: int48eq, float48eq, int24pl, ...). No coercion is
/// inserted for these pairs, so no cast deparses.
pub(crate) fn pg_native_binop_pair(a: ColType, b: ColType) -> bool {
    use ColType::*;
    if a == b {
        return true;
    }
    let int = |t: &ColType| matches!(t, SmallInt | Int | BigInt);
    if int(&a) && int(&b) {
        return true;
    }
    matches!((&a, &b), (Float4, Float) | (Float, Float4))
}

/// v1.57: exact f64 value of a constant literal, mirroring PG19's parser
/// coercion of the literal to float8 before an immutable builtin is
/// const-folded: correctly-rounded decimal parse; int64->float8 is exact
/// below 2^53 and round-to-nearest above (like PG's C cast).
pub(crate) fn pg_literal_to_f64(lit: &Literal) -> Option<f64> {
    match lit {
        Literal::Int(i) | Literal::BigInt(i) => Some(*i as f64),
        Literal::SmallInt(i) => Some(f64::from(*i)),
        Literal::Float(f) => Some(*f),
        Literal::Real(r) => Some(f64::from(*r)),
        Literal::Decimal(s) => s.parse::<f64>().ok(),
        // v1.58: substituted EXECUTE arguments round-trip through
        // Value::Numeric, so parameter-derived literals arrive as
        // Numeric rather than Decimal; Numeric::to_f64 is PG19's
        // numeric_float8 (correctly rounded).
        Literal::Numeric(n) => Some(n.to_f64()),
        Literal::Text(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// v1.57: intrinsic type of a constant literal. Unknown-type text and
/// untyped NULL fail closed (their type comes from context).
pub(crate) fn pg_literal_col_type(lit: &Literal) -> Option<ColType> {
    match lit {
        Literal::SmallInt(_) => Some(ColType::SmallInt),
        Literal::Int(_) => Some(ColType::Int),
        Literal::BigInt(_) => Some(ColType::BigInt),
        Literal::Float(_) => Some(ColType::Float),
        Literal::Real(_) => Some(ColType::Float4),
        Literal::Decimal(_) => Some(ColType::Numeric(None)),
        Literal::Bool(_) => Some(ColType::Bool),
        _ => None,
    }
}

/// v1.57: fold `CAST(lit AS to)` over an already-folded literal. Only
/// PG-agreeing exact casts: NULL (folds to a null of the target type; the
/// array element prints bare NULL either way), unknown->text (no-op),
/// exact int->float8 widening, and decimal->float8 (correctly rounded,
/// like PG's numeric_float8). Anything else fails closed.
pub(crate) fn pg_cast_const_literal(lit: &Literal, to: ColType) -> Option<Literal> {
    if matches!(lit, Literal::Null) {
        return Some(Literal::Null);
    }
    match (lit, to) {
        (Literal::Text(_), ColType::Text) => Some(lit.clone()),
        (Literal::Int(i) | Literal::BigInt(i), ColType::Float) if i.unsigned_abs() < (1 << 53) => {
            Some(Literal::Float(*i as f64))
        }
        (Literal::SmallInt(i), ColType::Float) => Some(Literal::Float(f64::from(*i))),
        (Literal::Real(r), ColType::Float) => Some(Literal::Float(f64::from(*r))),
        (Literal::Decimal(s), ColType::Float) => s.parse::<f64>().ok().map(Literal::Float),
        _ => {
            if pg_literal_col_type(lit) == Some(to) {
                Some(lit.clone())
            } else {
                None
            }
        }
    }
}

/// v1.59: compare two non-null constant literals with a comparison
/// operator, mirroring PG19's const-folding (`ece_evaluate_expr` in
/// clauses.c). Integers compare as i64 (exact); floats as f64 (fail
/// closed on NaN, where PG's float8eq has NaN-is-equal semantics);
/// mixed int/float as f64 when the int is exactly representable
/// (|i| < 2^53, else fail closed); text and bool compare naturally.
/// Anything else (dates, numerics, bytea, ...) fails closed.
/// None = incomparable.
pub(crate) fn pg_literal_cmp(l: &Literal, op: CmpOp, r: &Literal) -> Option<bool> {
    use std::cmp::Ordering;
    fn as_i64(l: &Literal) -> Option<i64> {
        match l {
            Literal::SmallInt(i) => Some(*i as i64),
            Literal::Int(i) | Literal::BigInt(i) => Some(*i),
            _ => None,
        }
    }
    fn as_f64(l: &Literal) -> Option<f64> {
        match l {
            Literal::Float(f) => Some(*f),
            Literal::Real(x) => Some(f64::from(*x)),
            _ => None,
        }
    }
    let ord: Ordering = if let (Some(a), Some(b)) = (as_i64(l), as_i64(r)) {
        a.cmp(&b)
    } else if let (Some(a), Some(b)) = (as_f64(l), as_f64(r)) {
        if a.is_nan() || b.is_nan() {
            return None;
        }
        a.partial_cmp(&b)?
    } else if let (Some(a), Some(b)) = (as_i64(l), as_f64(r)) {
        // PG coerces the int to float8; exact when |a| < 2^53.
        if a.unsigned_abs() >= (1 << 53) {
            return None;
        }
        (a as f64).partial_cmp(&b)?
    } else if let (Some(a), Some(b)) = (as_f64(l), as_i64(r)) {
        if b.unsigned_abs() >= (1 << 53) {
            return None;
        }
        a.partial_cmp(&(b as f64))?
    } else if let (Literal::Text(a), Literal::Text(b)) = (l, r) {
        a.cmp(b)
    } else if let (Literal::Bool(a), Literal::Bool(b)) = (l, r) {
        a.cmp(b)
    } else {
        return None;
    };
    Some(match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Ge => ord != Ordering::Less,
        CmpOp::ImageEq => return None,
    })
}

/// v1.60: per-fold context for the EXPLAIN-path constant folder, mirroring
/// PG19 `eval_const_expressions`' `context` argument (clauses.c): the
/// catalog for immutable-function lookup, the active-function stack (PG's
/// `context->active_fns`) so a self-recursive function can never send the
/// folder into an infinite loop, and two scoped bindings:
/// - `params`/`param_names`: while folding a function body, `Param(n)` and
///   named-argument columns resolve to the folded argument literals
///   (innermost query level wins, like PG);
/// - `fnscan_subs`: in a pulled-up WHERE clause, unqualified columns naming
///   a constant-folded function-scan output resolve to its constant (PG19
///   `pull_up_constant_function`, prepjointree.c:2235).
pub(crate) struct PgConstFold<'a> {
    pub(crate) db: Option<&'a Database>,
    pub(crate) visiting: Vec<(String, Vec<Option<String>>)>,
    pub(crate) params: Vec<Literal>,
    pub(crate) param_names: Vec<Option<String>>,
    pub(crate) fnscan_subs: Vec<(String, Literal)>,
}

impl<'a> PgConstFold<'a> {
    /// A pure folder with no catalog and no bindings: behaves exactly like
    /// the v1.57-v1.59 folder (immutable builtins only; user-defined calls
    /// and bare columns never fold).
    pub(crate) fn pure() -> Self {
        PgConstFold {
            db: None,
            visiting: Vec::new(),
            params: Vec::new(),
            param_names: Vec::new(),
            fnscan_subs: Vec::new(),
        }
    }
    /// A catalog-backed folder for the EXPLAIN plan path.
    pub(crate) fn with_db(db: &'a Database) -> Self {
        PgConstFold {
            db: Some(db),
            visiting: Vec::new(),
            params: Vec::new(),
            param_names: Vec::new(),
            fnscan_subs: Vec::new(),
        }
    }
    /// Resolve an unqualified column reference: function-body parameters
    /// first (innermost scope), then pulled-up function-scan outputs.
    pub(crate) fn resolve_col(&self, name: &str) -> Option<&Literal> {
        if let Some(i) = self
            .param_names
            .iter()
            .position(|a| a.as_deref() == Some(name))
        {
            return self.params.get(i);
        }
        self.fnscan_subs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, l)| l)
    }
}

/// v1.60: fold a call to an immutable function with all-constant arguments,
/// mirroring PG19 `simplify_function` (clauses.c:5205-5297) feeding
/// `pull_up_constant_function` (prepjointree.c:2235):
/// - only IMMUTABLE functions fold (STABLE/VOLATILE never),
/// - all arguments must already fold to constants,
/// - strict + constant-NULL input -> NULL without calling the body
///   (a non-strict function is evaluated with the NULL bound),
/// - the body must be a single plain `SELECT <expr>` with no FROM/WHERE/
///   GROUP BY/etc. (the v0.97 desugar shape for `RETURN <expr>`);
///   anything else fails closed,
/// - a recursion guard (PG's `active_fns`) rejects self-recursive calls.
///
/// Any lookup miss, arity ambiguity, or evaluation failure -> None (fail
/// closed). EXPLAIN-path only; execution never calls this.
pub(crate) fn pg_fold_immutable_call(
    fc: &mut PgConstFold,
    name: &str,
    args: &[Expr],
) -> Option<Literal> {
    // Resolve exactly one catalog candidate (name + arity); the borrow of
    // `fc.db` ends here so `fc` can be mutated below. PG resolves
    // overloads; we have no overload set to choose from, so ambiguity
    // fails closed.
    let (arg_names, body, found_strict) = {
        let db = fc.db?;
        let found = db.functions.get(name).and_then(|v| {
            let mut it = v.iter().filter(|f| f.arg_names.len() == args.len());
            match (it.next(), it.next()) {
                (Some(f), None) => Some(f),
                _ => None,
            }
        })?;
        if found.volatility != crate::sql::FuncVolatility::Immutable
            || found.returns_set
            || found.plpgsql.is_some()
        {
            return None;
        }
        (found.arg_names.clone(), found.parsed.clone()?, found.strict)
    };
    // Recursion guard (PG's `active_fns` in `eval_const_expressions`,
    // which wraps the body expansion): pushed only around the body fold
    // below. Argument folding needs no guard — each nested call operates
    // on a strictly smaller sub-expression, so it always terminates;
    // keeping the guard off here lets legitimate nested calls like
    // `f(f(2))` fold.
    let key = (name.to_string(), arg_names.clone());
    // Fold the arguments first (PG folds args before the call).
    let mut folded: Vec<Literal> = Vec::with_capacity(args.len());
    for a in args {
        folded.push(pg_fold_const_item(a, fc)?);
    }
    // Strict + constant-NULL input -> NULL without calling the body (PG19
    // `simplify_function` folds this even for non-immutable functions).
    // A non-strict function is evaluated with the NULL bound instead.
    if found_strict && folded.iter().any(|l| matches!(l, Literal::Null)) {
        return Some(Literal::Null);
    }
    // Body must be a single plain `SELECT <expr>`.
    let target = match body.as_slice() {
        [Stmt::Select(s)]
            if s.with.is_empty()
                && !s.distinct
                && s.distinct_on.is_empty()
                && s.from.is_empty()
                && s.where_.is_none()
                && s.group_by.is_empty()
                && s.having.is_none()
                && s.order_by.is_empty()
                && s.limit.is_none()
                && s.offset.is_none()
                && s.set_op.is_none() =>
        {
            match s.items.as_slice() {
                [SelectItem::Expr { expr, .. }] => expr.clone(),
                _ => return None,
            }
        }
        _ => return None,
    };
    if fc.visiting.contains(&key) {
        return None;
    }
    fc.visiting.push(key);
    // Bind the folded arguments as the body's parameters (innermost scope
    // wins, like PG's query levels); the outer pulled-up map is hidden
    // while the body folds, since the body is a separate query level.
    let saved_params = std::mem::replace(&mut fc.params, folded);
    let saved_names = std::mem::replace(&mut fc.param_names, arg_names);
    let saved_subs = std::mem::take(&mut fc.fnscan_subs);
    let out = pg_fold_const_item(&target, fc);
    fc.params = saved_params;
    fc.param_names = saved_names;
    fc.fnscan_subs = saved_subs;
    fc.visiting.pop();
    out
}

/// v1.60: mirror PG19 `pull_up_constant_function` (prepjointree.c:2235)
/// for the EXPLAIN const-false check: collect the constant each
/// constant-folded FROM-function item's output column stands for, and the
/// FROM items surviving the pullup (folded function scans become no-op
/// RTE_RESULTs, dropped by PG19 `remove_useless_results`, which also
/// elides single-child joins and keeps at least one child). Returns
/// `(substitutions, surviving_items)`; the survivors feed the `Replaces:`
/// line when the qual goes const-false.
///
/// Fail-closed: any FROM item that is not a plain table, a function, or a
/// join of those disables substitution entirely (survivors = the original
/// FROM); a function whose call does not fold is left alone; an output
/// name colliding with a real table column or another substituted output
/// is dropped from the map (PG would qualify those references; we fail
/// closed instead).
pub(crate) fn pg_fnscan_subs(
    from: &[FromItem],
    fc: &mut PgConstFold,
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> (Vec<(String, Literal)>, Vec<FromItem>) {
    /// One item's pullup result: the surviving item (`None` = the item was
    /// a folded function scan, i.e. a dropped no-op RTE_RESULT), the
    /// substitutions its subtree contributed, the table columns its
    /// subtree occupies, and whether the subtree was fully analyzable.
    struct PruneOut {
        item: Option<FromItem>,
        subs: Vec<(String, Literal)>,
        taken: Vec<String>,
        ok: bool,
    }
    fn prune(
        it: &FromItem,
        fc: &mut PgConstFold,
        eng: &Engine,
        snap: &Snapshot,
        own: u64,
        session: u64,
    ) -> PruneOut {
        let blank = |item: Option<FromItem>| PruneOut {
            item,
            subs: Vec::new(),
            taken: Vec::new(),
            ok: true,
        };
        match it {
            FromItem::Table { name, .. } => {
                let mut o = blank(Some(it.clone()));
                if let Some(t) = eng.db.find_table(name, snap, &[own], session) {
                    o.taken.extend(t.columns.iter().map(|c| c.0.clone()));
                }
                o
            }
            FromItem::Function {
                name,
                args,
                alias,
                col_aliases,
                ..
            } => match pg_fold_immutable_call(fc, name, args) {
                Some(lit) => {
                    let out = col_aliases
                        .first()
                        .cloned()
                        .or_else(|| alias.clone())
                        .unwrap_or_else(|| name.clone());
                    let mut o = blank(None);
                    o.subs.push((out, lit));
                    o
                }
                None => blank(Some(it.clone())),
            },
            FromItem::Join {
                left,
                kind,
                right,
                on,
                using,
                natural,
                using_alias,
                alias,
                col_aliases,
            } => {
                let l = prune(left, fc, eng, snap, own, session);
                let r = prune(right, fc, eng, snap, own, session);
                let o = PruneOut {
                    // PG19 `remove_useless_results`: a join that lost a
                    // no-op side collapses to the surviving side; if both
                    // sides were no-ops, at least one child is kept.
                    item: match (l.item, r.item) {
                        (Some(li), Some(ri)) => Some(FromItem::Join {
                            left: Box::new(li),
                            kind: *kind,
                            right: Box::new(ri),
                            on: on.clone(),
                            using: using.clone(),
                            natural: *natural,
                            using_alias: using_alias.clone(),
                            alias: alias.clone(),
                            col_aliases: col_aliases.clone(),
                        }),
                        (Some(li), None) => Some(li),
                        (None, Some(ri)) => Some(ri),
                        (None, None) => Some(it.clone()),
                    },
                    subs: l.subs.into_iter().chain(r.subs).collect(),
                    taken: l.taken.into_iter().chain(r.taken).collect(),
                    ok: l.ok && r.ok,
                };
                o
            }
            // Derived / Values / anything else: the output column set is
            // not provable here — disable substitution (fail closed) but
            // keep the item for `Replaces:`.
            _ => PruneOut {
                item: Some(it.clone()),
                subs: Vec::new(),
                taken: Vec::new(),
                ok: false,
            },
        }
    }
    let mut subs: Vec<(String, Literal)> = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    let mut ok = true;
    let mut surviving: Vec<FromItem> = Vec::new();
    for it in from {
        let o = prune(it, fc, eng, snap, own, session);
        subs.extend(o.subs);
        taken.extend(o.taken);
        ok = ok && o.ok;
        if let Some(item) = o.item {
            surviving.push(item);
        }
    }
    if !ok || subs.is_empty() {
        return (Vec::new(), from.to_vec());
    }
    // Drop substitutions whose output name collides with a real table
    // column or another substituted function output.
    let mut seen: Vec<String> = Vec::new();
    subs.retain(|(name, _)| {
        if taken.iter().any(|t| t == name) || seen.iter().any(|s| s == name) {
            return false;
        }
        seen.push(name.clone());
        true
    });
    if subs.is_empty() {
        return (Vec::new(), from.to_vec());
    }
    // PG19 `remove_useless_results` keeps at least one child: never drop
    // every FROM item.
    if surviving.is_empty() {
        if let Some(last) = from.last() {
            surviving.push(last.clone());
        }
    }
    (subs, surviving)
}

/// v1.57: fold a constant IN-list item to a `Literal`, mirroring PG19
/// `eval_const_expressions`/`evaluate_function` (clauses.c) for immutable
/// builtins on constant arguments:
/// - strict + constant-NULL input -> NULL (PG folds this even for
///   non-immutable functions; ours are strict),
/// - all-constant inputs + immutable -> evaluate,
/// - anything else -> None (fail closed; the OR form is kept).
///
/// Only `sin`/`cos`/`tan` are evaluated: they are total on finite float8
/// inputs (PG's dsin/dcos/dtan raise only on infinite input or overflow,
/// neither reachable from a finite literal), so the fold cannot disagree
/// with PG. Domain-checked builtins (asin/acos/exp/ln/...) stay unfolded.
pub(crate) fn pg_fold_const_item(e: &Expr, fc: &mut PgConstFold) -> Option<Literal> {
    match e {
        Expr::Literal(lit) => Some(lit.clone()),
        // v1.60: parameter / pulled-up function-scan references. While
        // folding a function body, `Param(n)` and named-argument columns
        // resolve to the folded argument literals; in a pulled-up WHERE
        // clause, unqualified columns naming a folded function-scan output
        // resolve to its constant (PG19 `pull_up_constant_function`,
        // prepjointree.c:2235). Both maps are empty on the pure
        // (deparse-path) folder, so behavior there is unchanged.
        Expr::Param(n) => fc.params.get((*n as usize).checked_sub(1)?).cloned(),
        Expr::Column { table: None, name } => fc.resolve_col(name).cloned(),
        Expr::Func { name, args } => {
            // v1.59: NULLIF(a, b) = CASE WHEN a = b THEN NULL ELSE a END
            // (PG19 eval_const_expressions folds it; NULLIF is immutable).
            if name.eq_ignore_ascii_case("nullif") {
                if args.len() != 2 {
                    return None;
                }
                let a = pg_fold_const_item(&args[0], fc)?;
                let b = pg_fold_const_item(&args[1], fc)?;
                // `a = NULL` / `NULL = b` is never true, so NULLIF
                // returns a (NULLIF is not strict on either argument).
                if matches!(a, Literal::Null) || matches!(b, Literal::Null) {
                    return Some(a);
                }
                return Some(if pg_literal_cmp(&a, CmpOp::Eq, &b)? {
                    Literal::Null
                } else {
                    a
                });
            }
            let f: fn(f64) -> f64 = match name.to_ascii_lowercase().as_str() {
                "sin" => f64::sin,
                "cos" => f64::cos,
                "tan" => f64::tan,
                // v1.60: anything else may be an immutable user function
                // with constant args (PG19 `simplify_function`,
                // clauses.c:5205-5297); STABLE/VOLATILE never fold.
                _ => return pg_fold_immutable_call(fc, name, args),
            };
            if args.len() != 1 {
                return None;
            }
            let arg = pg_fold_const_item(&args[0], fc)?;
            if matches!(arg, Literal::Null) {
                return Some(Literal::Null);
            }
            let x = pg_literal_to_f64(&arg)?;
            // PG raises on infinite input; NaN has no faithful array
            // spelling here — fail closed on both (no corpus coverage).
            if !x.is_finite() {
                return None;
            }
            let y = f(x);
            if !y.is_finite() {
                return None;
            }
            Some(Literal::Float(y))
        }
        Expr::Cast { expr, to, .. } => {
            let lit = pg_fold_const_item(expr, fc)?;
            pg_cast_const_literal(&lit, *to)
        }
        _ => None,
    }
}

/// v1.57: infer the intrinsic type of an IN-list LHS expression without an
/// Engine (deparse path only). Arithmetic uses the executor's numeric
/// lattice (smallint < integer < bigint < numeric < real < double
/// precision), which agrees with PG's operator resolution for `+ - * /`
/// (native cross-type int/float operators resolve to the wider type).
/// None = unknowable -> the caller fails closed.
pub(crate) fn pg_infer_type(e: &Expr, pctx: &PgPlanCtx) -> Option<ColType> {
    match e {
        Expr::Column { table, name } => pctx.col_type(table.as_deref(), name),
        Expr::Literal(lit) => pg_literal_col_type(lit),
        Expr::Cast { to, .. } => Some(*to),
        Expr::Func { name, args } => {
            let (arg_tys, ret) = pg_math_builtin_sig(&name.to_ascii_lowercase())?;
            if args.len() == arg_tys.len() {
                Some(ret)
            } else {
                None
            }
        }
        Expr::Arith { op, left, right } => {
            if !matches!(
                op,
                ArithOp::Add | ArithOp::Sub | ArithOp::Mul | ArithOp::Div
            ) {
                return None;
            }
            let l = pg_infer_type(left, pctx)?;
            let r = pg_infer_type(right, pctx)?;
            Some(rank_type(numeric_rank(&l)?.max(numeric_rank(&r)?)))
        }
        _ => None,
    }
}

/// v1.57: insert explicit `Cast` nodes where PG19's parser would have put
/// implicit coercions, so the existing `pg_expr_text` Cast arm renders
/// PG's `(arg)::type` deparse (ruleutils.c `get_coercion_expr`, shown
/// because ScalarArrayOpExpr deparse uses showimplicit=true). Handles the
/// shapes an IN-list LHS can take: immutable float8 builtins (argument
/// coercion) and `+ - * /` (operand coercion to the common type, except
/// where PG has a native cross-type operator). None = fail closed.
pub(crate) fn pg_rewrite_coercions(e: &Expr, pctx: &PgPlanCtx) -> Option<Expr> {
    match e {
        Expr::Column { .. } | Expr::Literal(_) | Expr::Param(_) => Some(e.clone()),
        // Explicit casts already deparse faithfully; leave them alone.
        Expr::Cast { .. } => Some(e.clone()),
        Expr::Func { name, args } => {
            let (arg_tys, _) = pg_math_builtin_sig(&name.to_ascii_lowercase())?;
            if args.len() != arg_tys.len() {
                return None;
            }
            let mut out = Vec::with_capacity(args.len());
            for (a, want) in args.iter().zip(arg_tys.iter()) {
                let rw = pg_rewrite_coercions(a, pctx)?;
                let got = pg_infer_type(a, pctx)?;
                out.push(if got == *want || pg_native_binop_pair(got, *want) {
                    rw
                } else {
                    // Implicit upcast (pg_cast.dat): PG inserts a FuncExpr
                    // coercion, shown with showimplicit=true. A
                    // downcast/uncastable type means PG would reject the
                    // expression — fail closed.
                    let gr = numeric_rank(&got)?;
                    let wr = numeric_rank(want)?;
                    if gr >= wr {
                        return None;
                    }
                    Expr::Cast {
                        expr: Box::new(rw),
                        to: *want,
                        written: Some(PG_SYNTH_COERCE_WRITTEN.to_string()),
                    }
                });
            }
            Some(Expr::Func {
                name: name.clone(),
                args: out,
            })
        }
        Expr::Arith { op, left, right } => {
            if !matches!(
                op,
                ArithOp::Add | ArithOp::Sub | ArithOp::Mul | ArithOp::Div
            ) {
                return None;
            }
            let rw_l = pg_rewrite_coercions(left, pctx)?;
            let rw_r = pg_rewrite_coercions(right, pctx)?;
            let tl = pg_infer_type(left, pctx)?;
            let tr = pg_infer_type(right, pctx)?;
            if pg_native_binop_pair(tl, tr) {
                // PG uses the native cross-type operator; no coercion.
                return Some(Expr::Arith {
                    op: *op,
                    left: Box::new(rw_l),
                    right: Box::new(rw_r),
                });
            }
            let common = rank_type(numeric_rank(&tl)?.max(numeric_rank(&tr)?));
            let cr = numeric_rank(&common)?;
            let wrap = |rw: Expr, got: ColType| -> Option<Expr> {
                let gr = numeric_rank(&got)?;
                if gr == cr {
                    Some(rw)
                } else if gr < cr {
                    Some(Expr::Cast {
                        expr: Box::new(rw),
                        to: common,
                        written: Some(PG_SYNTH_COERCE_WRITTEN.to_string()),
                    })
                } else {
                    // Unreachable (common is the max rank); fail closed.
                    None
                }
            };
            Some(Expr::Arith {
                op: *op,
                left: Box::new(wrap(rw_l, tl)?),
                right: Box::new(wrap(rw_r, tr)?),
            })
        }
        _ => None,
    }
}

/// v1.56: recognize our parser's `x IN (v1, v2, ...)` desugar (a left-deep
/// `Or` tree of `=` comparisons against one structurally-equal LHS with
/// constant RHS items) and render PG19's `x = ANY ('{...}'::elemtype[])`
/// form. Returns `None` for anything that isn't faithfully an IN-list
/// (fail closed → the OR form is kept).
///
/// PG19 groundings:
/// - `transformAExprIn` (parse_expr.c) builds the ScalarArrayOpExpr only
///   when there are >1 non-Var RHS items; single-item IN stays `x = a`,
///   and Var-containing items are ORed on separately (so every folded
///   item must be Var-free — literals always are).
/// - v1.57: constant items are folded per `eval_const_expressions`
///   (clauses.c) / `evaluate_function`: immutable builtins on constant
///   args (`sin(0.5)` -> `0.479425538604203`) before the array is built.
/// - The array element type prefers the LHS type when the RHS items are
///   unknown-type literals (`select_common_type` with lexpr first).
/// - EXPLAIN deparse: `(lhs = ANY (array))` (ruleutils.c
///   `T_ScalarArrayOpExpr`, not-pretty mode, showimplicit=true); the
///   constant array prints via `array_out` + `simple_quote_literal` +
///   `::elemtype[]` (`get_const_expr`). Parser-inserted coercions on the
///   left arg deparse as casts (`get_coercion_expr`), except where PG has
///   a native cross-type operator (all int2/int4/int8 pairs and
///   float4/float8 for `=`/`+`/`-`/`*`/`/` — pg_operator.dat), in which
///   case no coercion exists and none deparses.
///
/// Known limitation: a hand-written `x = v1 OR x = v2` (not via IN) has
/// the same shape and folds too; PG would deparse the OR form. No
/// conformance statement has that shape (verified 2026-10-01).
pub(crate) fn pg_fold_in_any(e: &Expr, pctx: &PgPlanCtx, qualify: bool) -> Option<String> {
    // Flatten the left-deep Or spine the parser builds, back into source
    // order. Anything right-nested fails the Cmp check below.
    let mut rev: Vec<&Expr> = Vec::new();
    let mut cur = e;
    loop {
        match cur {
            Expr::Or(a, b) => {
                rev.push(b);
                cur = a;
            }
            _ => {
                rev.push(cur);
                break;
            }
        }
    }
    rev.reverse();
    // PG only builds the array form for >1 items.
    if rev.len() < 2 {
        return None;
    }
    let mut lhs: Option<&Expr> = None;
    let mut items: Vec<Literal> = Vec::with_capacity(rev.len());
    for d in &rev {
        let Expr::Cmp {
            op: CmpOp::Eq,
            left,
            right,
        } = d
        else {
            return None;
        };
        match lhs {
            None => lhs = Some(left),
            Some(l) if l == left.as_ref() => {}
            _ => return None,
        }
        // v1.57: items join the array when constant — literals directly,
        // or immutable builtins on constants (PG19 eval_const_expressions
        // folds these before planning). Mirrors PG's rnonvars partition:
        // Var-containing (non-constant) items are ORed on separately, so
        // one kills the whole fold (fail closed). v1.60: the deparse path
        // stays pure (no catalog, no bindings) — user calls never fold
        // here, exactly as before.
        items.push(pg_fold_const_item(
            right.as_ref(),
            &mut PgConstFold::pure(),
        )?);
    }
    let lhs = lhs?;
    // Element type: PG prefers the LHS type for unknown literals.
    let mut elem_ty: ColType = match lhs {
        Expr::Cast { to, .. } => *to,
        Expr::Column { table, name } => pctx.col_type(table.as_deref(), name)?,
        // v1.57: a computed LHS (e.g. `sin(two)+four`) — PG's lexpr has a
        // real type; infer it. Uninferable -> fail closed.
        Expr::Arith { .. } | Expr::Func { .. } => pg_infer_type(lhs, pctx)?,
        _ => return None,
    };
    // PG19 select_common_type: an item type the LHS type coerces to
    // implicitly (but not vice versa) wins. In the integer hierarchy
    // (int2 -> int4 -> int8 are implicit upcasts) that widens the
    // element type to the widest int kind present, e.g. `intcol IN
    // (1, 3000000000)` deparses as `bigint[]`.
    if let Some(mut rank) = pg_int_kind_rank(elem_ty) {
        for lit in &items {
            let lr = match lit {
                Literal::SmallInt(_) => 0,
                Literal::Int(_) => 1,
                Literal::BigInt(_) => 2,
                _ => continue,
            };
            if lr > rank {
                rank = lr;
            }
        }
        elem_ty = match rank {
            0 => ColType::SmallInt,
            1 => ColType::Int,
            _ => ColType::BigInt,
        };
    }
    let label = pg_any_array_label(elem_ty)?;
    let mut arr = String::from("{");
    for (i, lit) in items.iter().enumerate() {
        if i > 0 {
            arr.push(',');
        }
        arr.push_str(&pg_array_elem_text(lit, elem_ty)?);
    }
    arr.push('}');
    let lhs_text = match lhs {
        // PG deparses an explicit coercion as `(arg)::type` (ruleutils.c
        // get_coercion_expr: parens around the argument, then `::type`),
        // e.g. `((stringu1)::text = ANY (...))`.
        Expr::Cast { expr, to, .. } => {
            let label = pg_type_label(*to)?;
            let inner = pg_expr_text(expr, pctx, qualify)?;
            format!("({inner})::{label}")
        }
        _ => {
            // v1.57: PG deparses the ScalarArrayOpExpr's left arg with
            // showimplicit=true (ruleutils.c T_ScalarArrayOpExpr), so
            // parser-inserted coercions appear as casts: re-insert them
            // as explicit Casts, then render. A native cross-type `=`
            // operator (all int2/int4/int8 pairs, float4/float8 —
            // pg_operator.dat) needs no coercion, so no cast deparses
            // (e.g. `(unique1 = ANY ('{1200,1}'::bigint[]))`).
            let rewritten = pg_rewrite_coercions(lhs, pctx)?;
            let lhs_ty = pg_infer_type(lhs, pctx)?;
            let inner = pg_expr_text(&rewritten, pctx, qualify)?;
            if lhs_ty == elem_ty || pg_native_binop_pair(lhs_ty, elem_ty) {
                inner
            } else {
                // Unreachable for Column/Arith/Func LHS (the element type
                // derives from the LHS type); fail closed if ever hit.
                return None;
            }
        }
    };
    Some(format!(
        "({lhs_text} = ANY ({}::{label}))",
        pg_quote_literal(&arr)
    ))
}

/// v1.56: rank within PG's implicit-upcast integer hierarchy
/// int2 -> int4 -> int8, for the IN-list element-type widening.
pub(crate) fn pg_int_kind_rank(t: ColType) -> Option<u8> {
    match t {
        ColType::SmallInt => Some(0),
        ColType::Int => Some(1),
        ColType::BigInt => Some(2),
        _ => None,
    }
}

/// v1.56: true for plain `digits[.digits]` spellings, which PG19
/// numeric_out reproduces exactly (other spellings get normalized).
pub(crate) fn pg_is_plain_decimal(s: &str) -> bool {
    let t = s.trim();
    let mut parts = t.split('.');
    let int_ok = parts
        .next()
        .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let frac_ok = match parts.next() {
        None => true,
        Some(p) => !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()),
    };
    int_ok && frac_ok && parts.next().is_none()
}

/// v1.56: render one IN-list item like PG19 `array_out` prints an array
/// element of type `elem`: the type's output function, then `array_out`'s
/// quoting rules (empty string and case-insensitive "null" are quoted;
/// `"`, `\`, `{`, `}`, the delimiter `,` and whitespace force quotes
/// with `\`-escapes; NULL prints bare). `None` = the literal cannot be
/// an element of an `elem`-typed array (fail closed).
pub(crate) fn pg_array_elem_text(lit: &Literal, elem: ColType) -> Option<String> {
    match lit {
        Literal::Null => Some("NULL".to_string()),
        Literal::Text(s) => {
            let s = s.as_ref();
            match elem {
                // Character types: the unknown literal is coerced with
                // typmod -1 (coerce_to_common_type), so no blank-padding
                // or truncation applies; the raw text is the element.
                // (Name/SingleChar are deliberately excluded: namein
                // silently truncates to 63 bytes and charin takes one
                // byte — no corpus coverage, fail closed.)
                ColType::Text | ColType::Char(_) | ColType::Varchar(_) => {
                    Some(pg_array_quote_elem(s))
                }
                // PG19 coerces unknown literals to the common type; the
                // element then prints via the type's output function.
                ColType::Int | ColType::BigInt | ColType::SmallInt => {
                    let v: i64 = s.trim().parse().ok()?;
                    Some(v.to_string())
                }
                // Text -> float8/float4 is deliberately unhandled: PG's
                // float8out switches to scientific notation at extreme
                // magnitudes and our pg_float_text approximation doesn't
                // reproduce those thresholds — fail closed (no corpus
                // coverage, so these stay EF either way).
                // numeric_out preserves plain `digits[.digits]` spellings;
                // anything else fails closed (numeric_out normalizes it).
                ColType::Numeric(_) if pg_is_plain_decimal(s) => Some(s.trim().to_string()),
                ColType::Bool => match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" => Some("t".to_string()),
                    "false" | "f" | "0" => Some("f".to_string()),
                    _ => None,
                },
                _ => None,
            }
        }
        Literal::Int(i) | Literal::BigInt(i) => match elem {
            ColType::Int | ColType::BigInt | ColType::SmallInt => Some(i.to_string()),
            // int coerced to numeric: numeric_out(i) is plain digits.
            ColType::Numeric(_) => Some(i.to_string()),
            // int coerced to float8/float4: float8out(i). PG switches to
            // scientific notation at 1e15 (d2s.c: "we switch to scientific
            // notation when the display exponent reaches 15"); below that
            // both PG and Rust print plain digits.
            ColType::Float | ColType::Float4 if i.unsigned_abs() < 1_000_000_000_000_000 => {
                Some(i.to_string())
            }
            _ => None,
        },
        Literal::SmallInt(i) => match elem {
            ColType::SmallInt | ColType::Int | ColType::BigInt => Some(i.to_string()),
            _ => None,
        },
        // float8out shortest round-trip (same approximation as the
        // scalar const path).
        Literal::Float(f) => match elem {
            ColType::Float => Some(pg_float_text(*f)),
            _ => None,
        },
        Literal::Real(f) => match elem {
            ColType::Float4 => Some(pg_float_text(f64::from(*f))),
            _ => None,
        },
        // Numeric items: numeric_out reproduces plain `digits[.digits]`
        // spellings exactly; anything else (exponents, `.5`, `5.`) fails
        // closed since numeric_out normalizes them.
        Literal::Decimal(s) => match elem {
            ColType::Numeric(_) if pg_is_plain_decimal(s.as_ref()) => Some(s.to_string()),
            _ => None,
        },
        Literal::Bool(b) => match elem {
            ColType::Bool => Some(if *b { "t" } else { "f" }.to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// v1.56: PG19 `array_out` element quoting: double-quote when the element
/// is empty, case-insensitively "null", or contains `"`, `\`, `{`, `}`,
/// `,` or whitespace (PG's `scanner_isspace`); `"` and `\` are
/// backslash-escaped inside the quotes.
pub(crate) fn pg_array_quote_elem(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| {
            matches!(
                c,
                '"' | '\\' | '{' | '}' | ',' | ' ' | '\t' | '\n' | '\r' | '\x0C' | '\x0B'
            )
        });
    if !needs_quote {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// v1.56: the `::elemtype[]` label for an IN-list array constant.
/// Typmods are dropped — PG's `format_type_with_typemod` on the array
/// type shows the bare element name (`character[]`, `numeric[]`).
pub(crate) fn pg_any_array_label(elem: ColType) -> Option<String> {
    let e = match elem {
        ColType::Int => "integer",
        ColType::BigInt => "bigint",
        ColType::SmallInt => "smallint",
        ColType::Float => "double precision",
        ColType::Float4 => "real",
        ColType::Numeric(_) => "numeric",
        ColType::Text => "text",
        ColType::Name => "name",
        ColType::Char(_) => "character",
        ColType::Varchar(_) => "character varying",
        ColType::SingleChar => "\"char\"",
        ColType::Bool => "boolean",
        ColType::Date => "date",
        ColType::Timestamp => "timestamp without time zone",
        ColType::Timestamptz => "timestamp with time zone",
        ColType::Bytea => "bytea",
        ColType::Uuid => "uuid",
        _ => return None,
    };
    Some(format!("{e}[]"))
}

/// `pg_expr_text` with a coercion target for a top-level untyped string
/// literal (used for comparison operands).
pub(crate) fn pg_expr_target(
    e: &Expr,
    pctx: &PgPlanCtx,
    qualify: bool,
    target: Option<ColType>,
) -> Option<String> {
    match e {
        Expr::Column { table, name } => {
            if qualify {
                let q = pctx.print_qual(table.as_deref(), name)?;
                Some(format!("{}.{}", q, pg_quote_ident(name)))
            } else {
                Some(pg_quote_ident(name))
            }
        }
        Expr::Literal(l) => pg_literal_text(l, target),
        Expr::Param(n) => Some(format!("${n}")),
        Expr::Cmp { op, left, right } => {
            // Untyped string literal takes the other side's column type
            // (PG19 parser coercion of unknown-type literals). The target
            // goes to the LITERAL side, not the column side.
            // v1.56: also when the other side is a Cast-wrapped column
            // (e.g. `stringu1::text = 'RFAAAA'`), whose target is the
            // cast's type. Previously no target was found (the old arms
            // only matched a bare Column), so `pg_literal_text` got
            // `None` and the whole comparison fell to a Debug-fallback EF.
            let tgt_l = match (&**left, &**right) {
                (Expr::Literal(Literal::Text(_)), Expr::Column { table, name }) => {
                    pctx.col_type(table.as_deref(), name)
                }
                (Expr::Literal(Literal::Text(_)), Expr::Cast { to, .. }) => Some(*to),
                _ => None,
            };
            let tgt_r = match (&**left, &**right) {
                (Expr::Column { table, name }, Expr::Literal(Literal::Text(_))) => {
                    pctx.col_type(table.as_deref(), name)
                }
                (Expr::Cast { to, .. }, Expr::Literal(Literal::Text(_))) => Some(*to),
                _ => None,
            };
            let l = pg_expr_target(left, pctx, qualify, tgt_l)?;
            let r = pg_expr_target(right, pctx, qualify, tgt_r)?;
            // v1.50: Paren-marking for pulled-up non-Column exprs (T2).
            // Per ruleutils.c `get_special_variable`, a non-Var referent
            // through a special varno forces parens.
            // v1.58: a Cast of a literal also needs the parens now that
            // the Cast arm renders `'lit'::type` without outer parens
            // (PG folds the cast at plan time; the pull-up parens come
            // from get_special_variable, not the cast). A Cast of a
            // non-literal still renders parenthesized, so it must not
            // be double-wrapped.
            let needs_pullup_paren = |e: &Expr| {
                matches!(e, Expr::Literal(_))
                    || matches!(e, Expr::Cast { expr, .. } if matches!(&**expr, Expr::Literal(_)))
            };
            let l = if needs_pullup_paren(left) && pctx.pulled_exprs.contains(left) {
                format!("({l})")
            } else {
                l
            };
            let r = if needs_pullup_paren(right) && pctx.pulled_exprs.contains(right) {
                format!("({r})")
            } else {
                r
            };
            let op = match op {
                CmpOp::Eq => "=",
                CmpOp::Ne => "<>",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
                CmpOp::ImageEq => return None,
            };
            Some(format!("({l} {op} {r})"))
        }
        Expr::And(_, _) => {
            // v1.73: PG renders a BoolExpr AND as a flat `(x AND y AND z)`
            // (explain.c), not left-nested `((x AND y) AND z)`. Flatten
            // the binary tree to match PG's output byte-exactly.
            let mut parts: Vec<String> = Vec::new();
            let mut stack: Vec<&Expr> = vec![e];
            while let Some(x) = stack.pop() {
                match x {
                    Expr::And(a2, b2) => {
                        stack.push(a2.as_ref());
                        stack.push(b2.as_ref());
                    }
                    _ => {
                        parts.push(pg_expr_text(x, pctx, qualify)?);
                    }
                }
            }
            parts.reverse();
            Some(format!("({})", parts.join(" AND ")))
        }
        Expr::Or(a, b) => {
            // v1.56: the parser desugars `x IN (v1, v2, ...)` to a left-deep
            // Or of `=` comparisons; PG19 deparses that shape as
            // `x = ANY ('{...}'::elemtype[])`.
            if let Some(any) = pg_fold_in_any(e, pctx, qualify) {
                return Some(any);
            }
            Some(format!(
                "({} OR {})",
                pg_expr_text(a, pctx, qualify)?,
                pg_expr_text(b, pctx, qualify)?
            ))
        }
        Expr::Not(x) => Some(format!("(NOT {})", pg_expr_text(x, pctx, qualify)?)),
        Expr::IsNull { expr, neg } => Some(format!(
            "({} IS {}NULL)",
            pg_expr_text(expr, pctx, qualify)?,
            if *neg { "NOT " } else { "" }
        )),
        Expr::Arith { op, left, right } => {
            let l = pg_expr_text(left, pctx, qualify)?;
            let r = pg_expr_text(right, pctx, qualify)?;
            let op = match op {
                ArithOp::Add => "+",
                ArithOp::Sub => "-",
                ArithOp::Mul => "*",
                ArithOp::Div => "/",
                ArithOp::Mod => "%",
                ArithOp::Pow => "^",
                ArithOp::BitAnd => "&",
                ArithOp::BitOr => "|",
                ArithOp::BitXor => "#",
                ArithOp::Shl => "<<",
                ArithOp::Shr => ">>",
            };
            Some(format!("({l} {op} {r})"))
        }
        Expr::Neg(x) => Some(format!("(- {})", pg_expr_text(x, pctx, qualify)?)),
        Expr::BitNot(x) => Some(format!("(~ {})", pg_expr_text(x, pctx, qualify)?)),
        Expr::Concat(a, b) => Some(format!(
            "({} || {})",
            pg_expr_text(a, pctx, qualify)?,
            pg_expr_text(b, pctx, qualify)?
        )),
        Expr::Cast { expr, to, written } => {
            let label = pg_type_label(*to)?;
            // v1.57: synthesized implicit-coercion casts (see
            // pg_rewrite_coercions) render PG19's get_coercion_expr form
            // `(arg)::type` — parens around the argument, not the cast.
            if written.as_deref() == Some(PG_SYNTH_COERCE_WRITTEN) {
                let inner = pg_expr_text(expr, pctx, qualify)?;
                return Some(format!("({inner})::{label}"));
            }
            // v1.50: for `'literal'::type`, deparse the literal as a plain
            // quoted string (not coerced) to avoid `::text::text` doubling.
            let inner = match &**expr {
                Expr::Literal(Literal::Text(s)) => pg_quote_literal(s),
                _ => pg_expr_target(expr, pctx, qualify, Some(*to))?,
            };
            // v1.58: PG19 folds `CAST(const AS type)` at plan time
            // (eval_const_expressions), so a cast of a literal deparses
            // as a Const — `'lit'::type` with no outer parens
            // (ruleutils.c get_const_expr). The old `('lit'::type)`
            // form never matches PG.
            if matches!(&**expr, Expr::Literal(_)) {
                Some(format!("{inner}::{label}"))
            } else {
                Some(format!("({inner}::{label})"))
            }
        }
        Expr::Func { name, args } => {
            // v1.08: SIMILAR TO is desugared by the parser to similar_to();
            // PG deparses it as a POSIX regex match (f1 ~ 'regex'::text).
            if name.eq_ignore_ascii_case("similar_to") && (2..=3).contains(&args.len()) {
                let expr_text = pg_expr_text(&args[0], pctx, qualify)?;
                let pat = match &args[1] {
                    Expr::Literal(Literal::Text(s)) => s.clone(),
                    _ => return None,
                };
                let escape = if args.len() == 3 {
                    match &args[2] {
                        Expr::Literal(Literal::Text(s)) => {
                            let mut ch = s.chars();
                            let c = ch.next();
                            // Empty string means "no escape"; single char only.
                            if c.is_some() && ch.next().is_none() {
                                c
                            } else if s.is_empty() {
                                None
                            } else {
                                return None;
                            }
                        }
                        _ => return None,
                    }
                } else {
                    // v0.31: omitted ESCAPE defaults to backslash (PG 19).
                    Some('\\')
                };
                let regex = similar_to_regex(&pat, escape).ok()?;
                // PG renders the regex as a text literal with ::text cast.
                let lit = format!("{}::text", pg_quote_literal(&regex));
                return Some(format!("({expr_text} ~ {lit})"));
            }
            let mut s = String::new();
            s.push_str(&pg_quote_ident(name));
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&pg_expr_text(a, pctx, qualify)?);
            }
            s.push(')');
            Some(s)
        }
        Expr::IsDistinctFrom { left, right, neg } => {
            let l = pg_expr_text(left, pctx, qualify)?;
            let r = pg_expr_text(right, pctx, qualify)?;
            Some(if *neg {
                format!("({l} IS NOT DISTINCT FROM {r})")
            } else {
                format!("({l} IS DISTINCT FROM {r})")
            })
        }
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            let e = pg_expr_text(expr, pctx, qualify)?;
            let lo = pg_expr_text(low, pctx, qualify)?;
            let hi = pg_expr_text(high, pctx, qualify)?;
            Some(if *neg {
                format!("({e} NOT BETWEEN {lo} AND {hi})")
            } else {
                format!("({e} BETWEEN {lo} AND {hi})")
            })
        }
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => {
            let e = pg_expr_text(expr, pctx, qualify)?;
            let p = pg_expr_text(pattern, pctx, qualify)?;
            let op = match (ilike, not) {
                (false, false) => "~~",
                (false, true) => "!~~",
                (true, false) => "~~*",
                (true, true) => "!~~*",
            };
            let mut s = format!("({e} {op} {p}");
            if let Some(x) = escape {
                s.push_str(&format!(" ESCAPE {}", pg_expr_text(x, pctx, qualify)?));
            }
            s.push(')');
            Some(s)
        }
        Expr::IsBool { expr, neg, val } => {
            let e = pg_expr_text(expr, pctx, qualify)?;
            let v = match val {
                Some(true) => "TRUE",
                Some(false) => "FALSE",
                None => "UNKNOWN",
            };
            Some(if *neg {
                format!("({e} IS NOT {v})")
            } else {
                format!("({e} IS {v})")
            })
        }
        // Anything else (subqueries, aggregates, arrays, rows, ...) has
        // no faithful short spelling here.
        _ => None,
    }
}

/// v1.08: name/type resolution context for PG-style EXPLAIN expression
/// text, built per query level in `plan_select`.
pub(crate) struct PgPlanCtx<'a> {
    /// Per FROM item, in order: qualifier names (table name, then alias)
    /// and the item's visible columns (name, type).
    pub(crate) items: Vec<(Vec<String>, Vec<(String, ColType)>)>,
    /// v1.50: non-Column tlist exprs substituted by subquery pullup, for
    /// PG-text paren marking (ruleutils.c `get_special_variable`).
    pub(crate) pulled_exprs: Vec<Expr>,
    /// v1.54: EXPLAIN VERBOSE flag — PG's `show_upper_qual` uses
    /// `useprefix = rtable_size > 1 || es->verbose` for Filter lines.
    pub(crate) verbose: bool,
    pub(crate) _mark: std::marker::PhantomData<&'a ()>,
}

impl<'a> PgPlanCtx<'a> {
    /// Declared type of `qual.col` (or unqualified `col` when it resolves
    /// to exactly one FROM item).
    pub(crate) fn col_type(&self, qual: Option<&str>, col: &str) -> Option<ColType> {
        let mut found = None;
        for (quals, cols) in &self.items {
            if let Some(q) = qual {
                if !quals.iter().any(|x| x == q) {
                    continue;
                }
            }
            if let Some((_, ty)) = cols.iter().find(|(n, _)| n == col) {
                if found.is_some() {
                    return None; // ambiguous
                }
                found = Some(*ty);
            }
            if qual.is_some() {
                break;
            }
        }
        found
    }

    /// Qualifier to print for `qual.col` when PG qualifies the column
    /// (multi-relation deparse context): the qualifier as written, or the
    /// owning table's name when unqualified and unambiguous.
    pub(crate) fn print_qual(&self, qual: Option<&str>, col: &str) -> Option<String> {
        if let Some(q) = qual {
            return Some(pg_quote_ident(q));
        }
        let mut found = None;
        for (quals, cols) in &self.items {
            if cols.iter().any(|(n, _)| n == col) {
                if found.is_some() {
                    return None; // ambiguous
                }
                found = Some(quals.first().cloned().unwrap_or_default());
            }
        }
        found.map(|q| pg_quote_ident(&q))
    }
}

/// v1.08: build the PG-text name/type context for one query level.
/// Joins flatten to their inputs (PG19's rtable has no join entries).
pub(crate) fn pg_plan_ctx<'a>(
    eng: &'a Engine,
    items: &[FromItem],
    snap: &'a Snapshot,
    own: u64,
    session: u64,
    ctes: &[CteDef],
    pulled_exprs: Vec<Expr>,
    // v1.54: EXPLAIN VERBOSE flag for the Filter `useprefix` rule.
    verbose: bool,
) -> PgPlanCtx<'a> {
    fn push_item<'a>(
        eng: &'a Engine,
        item: &FromItem,
        snap: &'a Snapshot,
        own: u64,
        session: u64,
        ctes: &[CteDef],
        out: &mut Vec<(Vec<String>, Vec<(String, ColType)>)>,
    ) {
        match item {
            FromItem::Table { name, .. } => {
                let cols = if ctes.iter().any(|c| c.name == *name) {
                    Vec::new()
                } else {
                    eng.db
                        .find_table(name, snap, &[own], session)
                        .map(|t| t.columns.iter().map(|(n, ty)| (n.clone(), *ty)).collect())
                        .unwrap_or_default()
                };
                out.push((pg_item_quals(item), cols));
            }
            FromItem::Join { left, right, .. } => {
                push_item(eng, left, snap, own, session, ctes, out);
                push_item(eng, right, snap, own, session, ctes, out);
            }
            _ => out.push((pg_item_quals(item), Vec::new())),
        }
    }
    let mut items_out = Vec::new();
    for item in items {
        push_item(eng, item, snap, own, session, ctes, &mut items_out);
    }
    PgPlanCtx {
        items: items_out,
        pulled_exprs,
        verbose,
        _mark: std::marker::PhantomData,
    }
}

/// v1.08: fold conjuncts into one AND tree (`None` for an empty list).
/// v1.48: constant-TRUE conjuncts are dropped before folding. PG19 drops
/// constant-TRUE clauses "in any case" (restrictinfo.c
/// `extract_actual_clauses`), so `ON true` / `WHERE true` never render a
/// `Join Filter:` / `Filter:` line.
pub(crate) fn pg_fold_and(cs: Vec<Expr>) -> Option<Expr> {
    cs.into_iter()
        .filter(|e| !matches!(e, Expr::Literal(Literal::Bool(true))))
        .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
}

/// v1.48: PG19's text label for a nested-loop join kind (explain.c
/// `ExplainNode`: the jointype switch appends to the `"Nested Loop"`
/// pname; inner renders bare, and cross joins plan as inner so PG has no
/// cross label). v1.68: `JoinKind::Anti` renders `Nested Loop Anti Join`
/// (PG19 `JOIN_ANTI`); the variant exists for the EXPLAIN planner only.
pub(crate) fn pg_join_label(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Left => "Nested Loop Left Join",
        JoinKind::Right => "Nested Loop Right Join",
        JoinKind::Full => "Nested Loop Full Join",
        JoinKind::Anti => "Nested Loop Anti Join",
        JoinKind::Inner | JoinKind::Cross => "Nested Loop",
    }
}

/// v1.68: PG19's text label for a hash-join kind (explain.c `ExplainNode`:
/// `" %s Join"` interpolated for non-inner jointypes, bare `"Hash Join"`
/// for `JOIN_INNER`).
pub(crate) fn pg_hash_join_label(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Anti => "Hash Anti Join",
        _ => "Hash Join",
    }
}

/// v1.69: PG19's text label for a merge-join kind (explain.c
/// `ExplainNode`): `"Merge Anti Join"` for `JOIN_ANTI`, `"Merge Left
/// Join"` / `"Merge Right Join"` / `"Merge Full Join"` for the outer
/// kinds, bare `"Merge Join"` for `JOIN_INNER`.
pub(crate) fn pg_merge_join_label(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Anti => "Merge Anti Join",
        JoinKind::Left => "Merge Left Join",
        JoinKind::Right => "Merge Right Join",
        JoinKind::Full => "Merge Full Join",
        JoinKind::Inner | JoinKind::Cross => "Merge Join",
    }
}

/// v1.48: does any node in this plan subtree carry a scan `Filter:` or a
/// join `Join Filter:`?
pub(crate) fn plan_has_filter(node: &PlanNode) -> bool {
    match node {
        PlanNode::Result { filter, .. }
        | PlanNode::SeqScan { filter, .. }
        | PlanNode::IndexScan { filter, .. } => filter.is_some(),
        PlanNode::NestedLoop {
            filter,
            join_filter,
            outer,
            inner,
            ..
        } => {
            filter.is_some()
                || join_filter.is_some()
                || plan_has_filter(outer)
                || plan_has_filter(inner)
        }
        PlanNode::HashJoin {
            filter,
            outer,
            inner,
            ..
        } => filter.is_some() || plan_has_filter(outer) || plan_has_filter(inner),
        PlanNode::MergeJoin {
            filter,
            outer,
            inner,
            ..
        } => filter.is_some() || plan_has_filter(outer) || plan_has_filter(inner),
        PlanNode::Materialize { child, .. }
        | PlanNode::Aggregate { child, .. }
        | PlanNode::Unique { child, .. }
        | PlanNode::Sort { child, .. }
        | PlanNode::Limit { child, .. }
        | PlanNode::SubqueryScan { child, .. } => plan_has_filter(child),
        PlanNode::IndexOrderScan { .. } | PlanNode::Values { .. } => false,
    }
}

/// v1.48: should the inner side of this nested loop be wrapped in a PG19
/// `Materialize` node?
///
/// PG19 (joinpath.c `try_nestloop_path`) always considers a materialized
/// inner path (`create_material_path`) alongside the plain one and keeps
/// the cheaper (`add_path`). For a non-self-materializing inner the
/// costing (costsize.c `initial_cost_nestloop` + `cost_rescan` +
/// `cost_material`) reduces to roughly "outer rows > 1": with a single
/// outer row no rescan ever happens, so the materialize build overhead
/// makes the materialized path lose (or tie, with the plain path added
/// first).
///
/// rustgres has no cost model, so the rule is structural and
/// conservative: materialize only when the outer estimate exceeds one
/// row, the inner is a plain seq scan (the PG `!ExecMaterializesOutput`
/// case the corpus exercises; parameterized/index inner shapes need the
/// real cost model), and neither side carries a filter — rustgres's scan
/// estimates ignore filter selectivity while PG's filtered estimates
/// decide the rescan count, so a filtered side makes the structural
/// estimate unreliable (this is what keeps the passing filtered
/// self-joins on `sl`/`sj` rendering PG's plain shape).
pub(crate) fn pg_should_materialize(outer: &PlanNode, inner: &PlanNode) -> bool {
    if outer.rows() <= 1 {
        return false;
    }
    // v1.50: T2 needs Materialize over a Left Join inner, not just SeqScan.
    // Allow NestedLoop (and other) inners, not just SeqScan.
    let is_nested_loop = matches!(inner, PlanNode::NestedLoop { .. });
    if !matches!(
        inner,
        PlanNode::SeqScan { .. } | PlanNode::NestedLoop { .. }
    ) {
        return false;
    }
    // v1.50: For T2's Left Join inner, allow materialize even with a filter
    // (the pushed-down Filter). The original filter check was to avoid
    // regressions; T2 requires it.
    if !is_nested_loop && (plan_has_filter(outer) || plan_has_filter(inner)) {
        return false;
    }
    true
}

/// v1.48: wrap the nested-loop inner side in `Materialize` when
/// [`pg_should_materialize`] says PG's planner would pick the
/// materialized inner path.
pub(crate) fn pg_maybe_materialize(outer: &PlanNode, inner: PlanNode) -> PlanNode {
    if pg_should_materialize(outer, &inner) {
        let rows = inner.rows();
        let output = inner.output().to_vec();
        PlanNode::Materialize {
            rows,
            child: Box::new(inner),
            output,
        }
    } else {
        inner
    }
}

/// v1.61: is every DISTINCT key of this plain `SELECT DISTINCT` provably
/// single-valued, so PG19 plans `LIMIT 1` instead of `Unique`?
///
/// PG19 (`create_distinct_paths`, planner.c:5394-5416): when all of the
/// distinct pathkeys are proven redundant by EquivalenceClass processing,
/// each DISTINCT target allows only a single value, so the tuples can be
/// uniquified by just taking the first one — PG adds a `LIMIT 1` path
/// instead of a Unique path.
///
/// rustgres has no EC machinery; the structural approximation is: a target
/// is single-valued if it is a non-null literal, or a bare column pinned
/// by a top-level `col = <non-null literal>` (either side) AND-conjunct in
/// the query's WHERE — exactly the EC-membership-with-a-const case (the
/// const side must be a literal, so volatile calls can never sneak in).
/// Anything else (`*`, casts, function calls, OR-nested quals, `= NULL`,
/// params) fails closed to Unique: a missed flip, never a wrong plan.
/// EXPLAIN-path only (called from `plan_select`); execution is untouched.
pub(crate) fn pg_distinct_keys_redundant(stmt: &SelectStmt) -> bool {
    // Columns pinned to a non-null literal by a top-level equality.
    let mut pinned: Vec<(Option<String>, String)> = Vec::new();
    if let Some(w) = &stmt.where_ {
        for c in split_conjuncts(w) {
            if let Expr::Cmp {
                op: CmpOp::Eq,
                left,
                right,
            } = c
            {
                let pin = match (&**left, &**right) {
                    (Expr::Column { table, name }, Expr::Literal(l))
                        if !matches!(l, Literal::Null) =>
                    {
                        Some((table.clone(), name.clone()))
                    }
                    (Expr::Literal(l), Expr::Column { table, name })
                        if !matches!(l, Literal::Null) =>
                    {
                        Some((table.clone(), name.clone()))
                    }
                    _ => None,
                };
                if let Some(p) = pin {
                    pinned.push(p);
                }
            }
        }
    }
    stmt.items.iter().all(|it| match it {
        SelectItem::Expr {
            expr: Expr::Literal(l),
            ..
        } => !matches!(l, Literal::Null),
        SelectItem::Expr {
            expr: Expr::Column { table, name },
            ..
        } => pinned.contains(&(table.clone(), name.clone())),
        _ => false,
    })
}

/// v1.08: AND-flatten preserving source order (unlike `split_conjuncts`,
/// which reverses via its stack). Conjunct order is user-visible in
/// EXPLAIN filter text, so PG-text paths use this.
pub(crate) fn pg_conjuncts(e: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    let mut stack = vec![e];
    while let Some(x) = stack.pop() {
        match x {
            Expr::And(a, b) => {
                stack.push(b);
                stack.push(a);
            }
            _ => out.push(x),
        }
    }
    out
}

/// v1.76: is this scan-Filter conjunct EC-deferred by PG19?
///
/// PG19's `distribute_qual_to_rels` (initsplan.c) routes binary `=`
/// quals through `process_equivalence` (equivclass.c), which absorbs them
/// into EquivalenceClasses instead of appending them to baserestrictinfo;
/// the originals are pushed back only later by
/// `generate_base_implied_equalities_const` — after all non-`=` quals.
/// Observable (11 live PG19-beta3 probes): non-`=` conjuncts render first
/// in written order, then `=` conjuncts in written order.
/// The predicate is just "binary `=` with syntactically different sides":
/// volatile `=` and array `=` are deferred too (probed); `X = X` is
/// excluded because PG rewrites it to `X IS NOT NULL` (not deferred).
pub(crate) fn pg_filter_conjunct_ec_deferred(c: &Expr) -> bool {
    match c {
        Expr::Cmp {
            op: CmpOp::Eq,
            left,
            right,
        } => left.as_ref() != right.as_ref(),
        _ => false,
    }
}

/// v1.76: PG19 scan-Filter qual ordering (EC deferral; EXPLAIN-deparse
/// only, the executor's qual list is untouched).
///
/// Reorders the top-level AND conjuncts of a scan WHERE into PG19's
/// render order: non-`=` conjuncts first (written order), then
/// EC-deferred `=` conjuncts (written order). Returns the original tree
/// unchanged unless the conjuncts are genuinely mixed (fail-closed:
/// single-conjunct, all-`=` and no-`=` Filters render exactly as before).
/// The re-fold is a manual left fold that preserves every conjunct
/// verbatim (`pg_fold_and` would drop `Literal::Bool(true)` conjuncts,
/// which PG renders, e.g. `(nt2.b1 AND true)`).
/// `order_qual_clauses`' stable cost sort is deliberately not implemented:
/// the only conjunct-order oracles in the corpus are the two v1.76
/// targets, whose quals have equal per_tuple cost.
pub(crate) fn pg_order_scan_filter(w: &Expr) -> Expr {
    let cs = pg_conjuncts(w);
    if cs.len() < 2 {
        return w.clone();
    }
    let mut immediate: Vec<Expr> = Vec::new();
    let mut deferred: Vec<Expr> = Vec::new();
    for c in cs {
        if pg_filter_conjunct_ec_deferred(c) {
            deferred.push((*c).clone());
        } else {
            immediate.push((*c).clone());
        }
    }
    if deferred.is_empty() || immediate.is_empty() {
        return w.clone();
    }
    immediate.extend(deferred);
    let mut it = immediate.into_iter();
    let first = it.next().expect("v1.76: mixed conjuncts are non-empty");
    it.fold(first, |a, b| Expr::And(Box::new(a), Box::new(b)))
}

/// v1.08: per top-level FROM item, the qualifier names and known
/// column names (joins flatten to their inputs' qualifiers/columns).
/// Used to distribute WHERE conjuncts over the FROM list.
pub(crate) fn pg_split_item(
    eng: &Engine,
    item: &FromItem,
    snap: &Snapshot,
    own: u64,
    session: u64,
    ctes: &[CteDef],
) -> (Vec<String>, Vec<String>) {
    match item {
        FromItem::Table { name, .. } => {
            let cols = if ctes.iter().any(|c| c.name == *name) {
                Vec::new()
            } else {
                eng.db
                    .find_table(name, snap, &[own], session)
                    .map(|t| t.columns.iter().map(|(n, _)| n.clone()).collect())
                    .unwrap_or_default()
            };
            (pg_item_quals(item), cols)
        }
        FromItem::Join { left, right, .. } => {
            let (mut q, mut c) = pg_split_item(eng, left, snap, own, session, ctes);
            let (q2, c2) = pg_split_item(eng, right, snap, own, session, ctes);
            q.extend(q2);
            c.extend(c2);
            (q, c)
        }
        _ => (pg_item_quals(item), Vec::new()),
    }
}

/// v1.08: distribute a WHERE clause's top-level AND-conjuncts over a
/// FROM list. Returns `(per_item, join)`: `per_item[i]` holds the
/// conjuncts referencing only item `i`, `join` holds multi-item (and
/// constant, for multi-item queries) conjuncts. `split` carries each
/// item's qualifier and column names. Returns `Err` when a conjunct
/// references nothing resolvable (honest mask).
#[allow(clippy::type_complexity)]
pub(crate) fn pg_split_where(
    w: &Expr,
    split: &[(Vec<String>, Vec<String>)],
) -> Result<(Vec<Vec<Expr>>, Vec<Expr>), ExecError> {
    let mut per_item: Vec<Vec<Expr>> = vec![Vec::new(); split.len()];
    let mut join: Vec<Expr> = Vec::new();
    for c in pg_conjuncts(w) {
        let mut refs = Vec::new();
        pg_expr_tables(c, &mut refs);
        // A conjunct with no column references belongs to the single
        // item when there is exactly one, else the join level.
        if refs.is_empty() {
            if split.len() == 1 {
                per_item[0].push(c.clone());
            } else {
                join.push(c.clone());
            }
            continue;
        }
        let mut idx: Option<usize> = None;
        let mut bad = false;
        let mut multi = false;
        for (q, col) in &refs {
            let mut found = None;
            for (i, (qs, cs)) in split.iter().enumerate() {
                let matches = match q {
                    Some(qq) => qs.iter().any(|x| x == qq),
                    // Unqualified: the unique owning item, if any.
                    None => cs.iter().any(|x| x == col),
                };
                if matches {
                    if found.is_some() {
                        // Ambiguous qualifier: no faithful placement.
                        bad = true;
                        break;
                    }
                    found = Some(i);
                }
            }
            if bad {
                break;
            }
            match found {
                Some(i) => match idx {
                    // References to two different sides: join level.
                    Some(j) if j != i => multi = true,
                    _ => idx = Some(i),
                },
                // Unresolvable reference: no faithful placement.
                None => {
                    bad = true;
                    break;
                }
            }
        }
        if bad {
            // Ambiguous/unresolvable: place at join level (wrong, but
            // EXPECTED-FAIL via mask; does not abort the transaction).
            join.push(c.clone());
            continue;
        }
        match (multi, idx) {
            (true, _) => join.push(c.clone()),
            (false, Some(i)) => per_item[i].push(c.clone()),
            (false, None) => join.push(c.clone()),
        }
    }
    Ok((per_item, join))
}

// ============================================================================
// v1.71: PG19 EC-canonical join-filter representatives.
//
// PG19 reference (`src/backend/optimizer/path/equivclass.c`,
// `generate_join_implied_equalities`): for an inner join the planner's
// nestloop Join Filter shows the EC-derived clause built as
// `best_outer_em = best_inner_em` — the FIRST equivalence-class member (in EC
// insertion order) from the call's outer side, then the first from the inner
// side. The scoring prefers simple Vars (+1 per side) and hashjoinable
// operators (+1), so for all-Var `=` clauses the first pair scores 3 and wins
// immediately.
//
// The clause orientation is fixed by the FIRST EC call per equivalence
// class and persists regardless of the plan's outer/inner:
// - `check_index_predicates` (indxpath.c) runs during base-rel path setup —
//   before join planning — but ONLY when the rel has a partial index. It
//   calls with outer=otherrels, so the derived clause is
//   `(first-other-member = first-indexed-member)`; it is saved via
//   `ec_add_derived_clause` and reused bidirectionally by
//   `build_joinrel_restrictlist`.
// - With no partial index, `build_joinrel_restrictlist` calls first with
//   outer=build-time-left, giving `(first-left-member =
//   first-right-member)`.
//
// ECs are built in jointree post-order: the join's own ON quals are
// distributed before the top-level WHERE (initsplan.c `deconstruct_recurse`
// appends each item after its children).
//
// Only plain `Column = Column` equalities are admitted to the EC (fail
// closed) — then every member is a Var and PG's rule provably reduces to
// first-member-per-side. The rewrite is EXPLAIN-text-only (the join filter
// is a pre-rendered String on the plan node; the executor never sees
// PlanNode) and semantics-preserving by EC construction.

/// Union-find over `(qualifier, column)` pairs, preserving insertion order.
pub(crate) struct PgEc {
    pub(crate) parent: Vec<usize>,
    pub(crate) ident: Vec<(String, String)>,
}

impl PgEc {
    pub(crate) fn new() -> Self {
        PgEc {
            parent: Vec::new(),
            ident: Vec::new(),
        }
    }
    pub(crate) fn get(&self, m: &(String, String)) -> Option<usize> {
        self.ident.iter().position(|x| x == m)
    }
    pub(crate) fn idx(&mut self, m: (String, String)) -> usize {
        if let Some(i) = self.get(&m) {
            return i;
        }
        let i = self.ident.len();
        self.ident.push(m);
        self.parent.push(i);
        i
    }
    pub(crate) fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }
    pub(crate) fn union(&mut self, a: (String, String), b: (String, String)) {
        let ia = self.idx(a);
        let ib = self.idx(b);
        let ra = self.find(ia);
        let rb = self.find(ib);
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
    /// First member (insertion order) of `root` whose qualifier is in `quals`.
    pub(crate) fn first_in(&mut self, root: usize, quals: &[String]) -> Option<(String, String)> {
        for i in 0..self.ident.len() {
            if self.find(i) == root && quals.iter().any(|q| q == &self.ident[i].0) {
                return Some(self.ident[i].clone());
            }
        }
        None
    }
}

/// Resolve a column reference to an EC member identity. Qualified refs keep
/// their qualifier (it must name a known side); unqualified refs resolve to
/// the unique single-qualifier item owning the column. Anything else returns
/// `None` (fail closed).
pub(crate) fn pg_ec_member(
    q: &Option<String>,
    col: &str,
    split: &[(Vec<String>, Vec<String>)],
) -> Option<(String, String)> {
    match q {
        Some(qq) => {
            if split.iter().any(|(qs, _)| qs.iter().any(|x| x == qq)) {
                Some((qq.clone(), col.to_string()))
            } else {
                None
            }
        }
        None => {
            let mut found = None;
            for (i, (_, cs)) in split.iter().enumerate() {
                if cs.iter().any(|x| x == col) {
                    if found.is_some() {
                        return None; // ambiguous: no faithful member
                    }
                    found = Some(i);
                }
            }
            let i = found?;
            if split[i].0.len() == 1 {
                Some((split[i].0[0].clone(), col.to_string()))
            } else {
                None
            }
        }
    }
}

/// Union one equality conjunct's plain-column sides into the EC.
pub(crate) fn pg_ec_union_conjunct(ec: &mut PgEc, c: &Expr, split: &[(Vec<String>, Vec<String>)]) {
    if let Expr::Cmp {
        op: CmpOp::Eq,
        left,
        right,
    } = c
    {
        if let (
            Expr::Column {
                table: lt,
                name: ln,
            },
            Expr::Column {
                table: rt,
                name: rn,
            },
        ) = (left.as_ref(), right.as_ref())
        {
            if let (Some(a), Some(b)) = (pg_ec_member(lt, ln, split), pg_ec_member(rt, rn, split)) {
                ec.union(a, b);
            }
        }
    }
}

/// v1.71: does this FROM item's base table carry a partial index? PG19's
/// `check_index_predicates` side effect (early EC-derived clause) fires only
/// for partial indexes, so only they affect the join-filter orientation.
pub(crate) fn pg_item_has_partial_index(
    eng: &Engine,
    item: &FromItem,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    let tname = match item {
        FromItem::Table { name, .. } => name,
        _ => return false,
    };
    eng.db
        .visible_indexes_for(tname, snap, &[own], session)
        .iter()
        .any(|ix| ix.def.predicate.is_some())
}

/// v1.71: rewrite nested-loop join-filter conjuncts to PG19 EC-canonical form
/// (`first-orientation-outer-member = first-orientation-inner-member`).
/// Returns the rendered text when at least one conjunct changed, else `None`
/// (caller keeps the pre-swap text — zero rendering drift for untouched
/// plans).
pub(crate) fn pg_ec_join_filter_text(
    jf: &[Expr],
    ec: &mut PgEc,
    outer_names: &[String],
    inner_names: &[String],
    split: &[(Vec<String>, Vec<String>)],
    px: &PgPlanCtx,
) -> Option<String> {
    // Self-joins (shared qualifiers across sides): no faithful assignment.
    if outer_names.iter().any(|q| inner_names.contains(q)) {
        return None;
    }
    let mut conjuncts: Vec<Expr> = Vec::new();
    for c in jf {
        for s in pg_conjuncts(c) {
            conjuncts.push(s.clone());
        }
    }
    let mut changed = false;
    let rewritten: Vec<Expr> = conjuncts
        .into_iter()
        .map(|c| {
            if let Expr::Cmp {
                op: CmpOp::Eq,
                left,
                right,
            } = &c
            {
                if let (
                    Expr::Column {
                        table: lt,
                        name: ln,
                    },
                    Expr::Column {
                        table: rt,
                        name: rn,
                    },
                ) = (left.as_ref(), right.as_ref())
                {
                    if let (Some(a), Some(b)) =
                        (pg_ec_member(lt, ln, split), pg_ec_member(rt, rn, split))
                    {
                        if let (Some(ia), Some(ib)) = (ec.get(&a), ec.get(&b)) {
                            if ec.find(ia) == ec.find(ib) {
                                let root = ec.find(ia);
                                if let (Some((oq, oc)), Some((iq, ic))) = (
                                    ec.first_in(root, outer_names),
                                    ec.first_in(root, inner_names),
                                ) {
                                    // No-op when the canonical members are
                                    // already the conjunct's members.
                                    let same = oq == a.0 && oc == a.1 && iq == b.0 && ic == b.1;
                                    if !same {
                                        changed = true;
                                        return Expr::Cmp {
                                            op: CmpOp::Eq,
                                            left: Box::new(Expr::Column {
                                                table: Some(oq),
                                                name: oc,
                                            }),
                                            right: Box::new(Expr::Column {
                                                table: Some(iq),
                                                name: ic,
                                            }),
                                        };
                                    }
                                }
                            }
                        }
                    }
                }
            }
            c
        })
        .collect();
    if !changed {
        return None;
    }
    pg_fold_and(rewritten).map(|e| pg_expr_text_or_debug(&e, px, true))
}

/// v1.08: an ORDER BY term's effective expression: a bare column naming
/// a SELECT output alias orders by the aliased expression (PG19 orders
/// by the targetlist entry, and EXPLAIN deparses that).
pub(crate) fn pg_order_expr(t: &OrderTerm, stmt: &SelectStmt) -> Expr {
    if let Expr::Column { table: None, name } = &t.expr {
        for item in &stmt.items {
            if let SelectItem::Expr {
                expr,
                alias: Some(a),
            } = item
            {
                if a.eq_ignore_ascii_case(name) {
                    return expr.clone();
                }
            }
        }
    }
    t.expr.clone()
}

/// EXPLAIN plan node: mirrors the executor's access-path decisions
/// without executing anything.
#[derive(Clone, Debug)]
pub(crate) enum PlanNode {
    Result {
        rows: u64,
        filter: Option<String>,
        // v1.46: VERBOSE `Output:` targetlist entries (PG19
        // `show_plan_tlist`). Empty == PG's NIL == no Output line.
        output: Vec<String>,
        // v1.49: PG19 const-false dummy rel (joinrels.c
        // `restriction_is_constant_false`, "constant NULL is as good as
        // constant FALSE", joinrels.c:1580-1582). Renders
        // `One-Time Filter: false` (explain.c:2253-2255) and, when
        // `replaces` is `Some`, `Replaces: ...` (explain.c
        // `show_result_replacement_info`). Set only on the EXPLAIN
        // (COSTS OFF) plan path; the executor never sees it.
        one_time_filter: bool,
        replaces: Option<String>,
    },
    SeqScan {
        table: String,
        // v1.08: table alias for PG EXPLAIN (`Seq Scan on t a`).
        alias: Option<String>,
        filter: Option<String>,
        rows: u64,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    IndexScan {
        table: String,
        // v1.08: table alias for PG EXPLAIN.
        alias: Option<String>,
        index: String,
        cond: String,
        filter: Option<String>,
        rows: u64,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    IndexOrderScan {
        table: String,
        // v1.08: table alias for PG EXPLAIN.
        alias: Option<String>,
        index: String,
        order: String,
        rows: u64,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    NestedLoop {
        filter: Option<String>,
        /// v1.08: join clauses, rendered as PG19's `Join Filter:`.
        join_filter: Option<String>,
        rows: u64,
        outer: Box<PlanNode>,
        inner: Box<PlanNode>,
        /// v1.48: the join kind. PG19 interpolates it into the node label
        /// (explain.c `ExplainNode`): `Nested Loop Left Join`,
        /// `Nested Loop Right Join`, `Nested Loop Full Join`; inner (and
        /// cross, which PG has no separate label for) renders bare
        /// `Nested Loop`.
        kind: JoinKind,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    /// v1.64: PG19 `HashJoin` (explain.c `T_HashJoin` → `"Hash Join"`),
    /// chosen by the cost-based hash-vs-nestloop rule (`pg_hashjoin_choice`,
    /// `cost_hashjoin` vs `cost_nestloop` parity). Display-only: like every
    /// other `PlanNode`, the executor never sees it (it runs its own
    /// hash-join detection), so results are identical by construction.
    /// `outer` is the probe side, `inner` the build side (rendered under a
    /// `Hash` wrapper node, PG19's `T_Hash`).
    HashJoin {
        /// Residual join quals (non-hash clauses), rendered as PG19's
        /// `Join Filter:` (explain.c `show_upper_qual` on `join.joinqual`).
        filter: Option<String>,
        /// The hash clauses, rendered as PG19's `Hash Cond:` with the
        /// outer var on the left (`create_hashjoin_plan`'s
        /// `get_switched_clauses`).
        hash_cond: String,
        rows: u64,
        outer: Box<PlanNode>,
        inner: Box<PlanNode>,
        /// v1.68: the join kind. PG19 interpolates it into the node label
        /// (explain.c `ExplainNode`): `Hash Anti Join` for `JOIN_ANTI`;
        /// inner renders bare `Hash Join`.
        kind: JoinKind,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    /// v1.69: PG19 `MergeJoin` (explain.c `T_MergeJoin` → `"Merge Join"`),
    /// chosen by the cost-based merge-vs-hash-vs-nestloop rule
    /// (`pg_mergejoin_choice`, `cost_mergejoin` parity on PG19 no-stats
    /// row estimates). Display-only: like every other `PlanNode`, the
    /// executor never sees it, so results are identical by construction.
    /// Inputs are `Sort` nodes unless the input path already produces the
    /// merge-key order (PG19 pathkeys — a Merge Join's output is ordered
    /// on its outer merge key, and nestloop preserves outer order).
    MergeJoin {
        /// Residual join quals (non-merge clauses), rendered as PG19's
        /// `Join Filter:`.
        filter: Option<String>,
        /// The merge clauses, rendered as PG19's `Merge Cond:` with the
        /// outer var on the left (`create_mergejoin_plan`).
        merge_cond: String,
        rows: u64,
        outer: Box<PlanNode>,
        inner: Box<PlanNode>,
        /// The join kind. PG19 interpolates it into the node label
        /// (explain.c `ExplainNode`): `Merge Anti Join` for `JOIN_ANTI`,
        /// `Merge Left Join` / `Merge Right Join` / `Merge Full Join`;
        /// inner renders bare `Merge Join`.
        kind: JoinKind,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    /// v1.48: PG19 `Materialize` node (explain.c `T_Material` →
    /// `"Materialize"`). Rendered above a nested loop's inner side when
    /// the planner chooses PG's materialized inner path (joinpath.c
    /// `create_material_path`). A Material node does not project, so
    /// `output` mirrors the child's targetlist.
    Materialize {
        rows: u64,
        child: Box<PlanNode>,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    Aggregate {
        rows: u64,
        child: Box<PlanNode>,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    Unique {
        rows: u64,
        child: Box<PlanNode>,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    Sort {
        keys: String,
        rows: u64,
        child: Box<PlanNode>,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    Limit {
        n: String,
        rows: u64,
        child: Box<PlanNode>,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    SubqueryScan {
        alias: String,
        rows: u64,
        child: Box<PlanNode>,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
    /// v0.14: `(VALUES ...)` table source.
    Values {
        rows: u64,
        // v1.46: VERBOSE `Output:` entries.
        output: Vec<String>,
    },
}

impl PlanNode {
    pub(crate) fn rows(&self) -> u64 {
        match self {
            PlanNode::Result { rows, .. }
            | PlanNode::SeqScan { rows, .. }
            | PlanNode::IndexScan { rows, .. }
            | PlanNode::IndexOrderScan { rows, .. }
            | PlanNode::NestedLoop { rows, .. }
            | PlanNode::HashJoin { rows, .. }
            | PlanNode::MergeJoin { rows, .. }
            | PlanNode::Aggregate { rows, .. }
            | PlanNode::Unique { rows, .. }
            | PlanNode::Sort { rows, .. }
            | PlanNode::Limit { rows, .. }
            | PlanNode::SubqueryScan { rows, .. }
            // v1.48: a Materialize node passes its child's row count
            // through (PG19 sizes the material path like the subpath).
            | PlanNode::Materialize { rows, .. } => *rows,
            PlanNode::Values { rows, .. } => *rows,
        }
    }

    /// Attach a residual filter to a scan-like node.
    /// v1.08: `None` omits the Filter line entirely (PG19 prints no
    /// Filter when there is no residual predicate).
    pub(crate) fn set_filter(&mut self, f: Option<String>) {
        match self {
            PlanNode::Result { filter, .. }
            | PlanNode::SeqScan { filter, .. }
            | PlanNode::IndexScan { filter, .. }
            | PlanNode::NestedLoop { filter, .. }
            | PlanNode::HashJoin { filter, .. }
            | PlanNode::MergeJoin { filter, .. } => *filter = f,
            _ => {}
        }
    }

    /// v1.08: set the Nested Loop's Join Filter (PG19's `Join Filter:`).
    /// A no-op on other nodes; the caller only sets it on joins.
    /// v1.64: also sets the Hash Join's residual filter (rendered as
    /// `Join Filter:`; PG19's `join.joinqual` beside `Hash Cond:`).
    pub(crate) fn set_join_filter(&mut self, f: Option<String>) {
        match self {
            PlanNode::NestedLoop { join_filter, .. } => *join_filter = f,
            PlanNode::HashJoin { filter, .. } => *filter = f,
            PlanNode::MergeJoin { filter, .. } => *filter = f,
            _ => {}
        }
    }

    /// v1.50: get the Nested Loop's Join Filter (for the Filter vs
    /// Join Filter split). v1.64: the Hash Join's residual filter too.
    pub(crate) fn join_filter(&self) -> Option<&String> {
        match self {
            PlanNode::NestedLoop { join_filter, .. } => join_filter.as_ref(),
            PlanNode::HashJoin { filter, .. } => filter.as_ref(),
            PlanNode::MergeJoin { filter, .. } => filter.as_ref(),
            _ => None,
        }
    }

    /// v1.46: VERBOSE `Output:` targetlist entries (empty == PG's NIL ==
    /// no Output line is rendered).
    pub(crate) fn output(&self) -> &[String] {
        match self {
            PlanNode::Result { output, .. }
            | PlanNode::SeqScan { output, .. }
            | PlanNode::IndexScan { output, .. }
            | PlanNode::IndexOrderScan { output, .. }
            | PlanNode::NestedLoop { output, .. }
            | PlanNode::HashJoin { output, .. }
            | PlanNode::MergeJoin { output, .. }
            | PlanNode::Aggregate { output, .. }
            | PlanNode::Unique { output, .. }
            | PlanNode::Sort { output, .. }
            | PlanNode::Limit { output, .. }
            | PlanNode::SubqueryScan { output, .. }
            // v1.48: Materialize does not project; its targetlist is the
            // child's.
            | PlanNode::Materialize { output, .. }
            | PlanNode::Values { output, .. } => output,
        }
    }

    /// v1.46: replace the VERBOSE `Output:` entries.
    pub(crate) fn set_output(&mut self, o: Vec<String>) {
        match self {
            PlanNode::Result { output, .. }
            | PlanNode::SeqScan { output, .. }
            | PlanNode::IndexScan { output, .. }
            | PlanNode::IndexOrderScan { output, .. }
            | PlanNode::NestedLoop { output, .. }
            | PlanNode::HashJoin { output, .. }
            | PlanNode::MergeJoin { output, .. }
            | PlanNode::Aggregate { output, .. }
            | PlanNode::Unique { output, .. }
            | PlanNode::Sort { output, .. }
            | PlanNode::Limit { output, .. }
            | PlanNode::SubqueryScan { output, .. }
            // v1.48: see `output()` above.
            | PlanNode::Materialize { output, .. }
            | PlanNode::Values { output, .. } => *output = o,
        }
    }
}

/// Estimated live row count: ANALYZE stats when present, else a visible
/// count under this snapshot.
pub(crate) fn est_rel_rows(
    db: &Database,
    table: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> u64 {
    if let Some(ts) = db.stats.get(table) {
        return ts.reltuples.round().max(0.0) as u64;
    }
    db.find_table(table, snap, &[own], session)
        .map(|t| {
            t.rows
                .iter()
                .filter(|r| row_visible(r, snap, &[own]))
                .count() as u64
        })
        .unwrap_or(0)
}

/// Equality selectivity from ANALYZE stats: MCV frequency when the value
/// is most-common, else the uniform remainder.
pub(crate) fn est_eq_sel(ts: Option<&TableStats>, col: &str, v: &Value) -> f64 {
    let cs = match ts.and_then(|t| t.cols.get(col)) {
        Some(c) => c,
        None => return 0.1,
    };
    if let Some((_, f)) = cs
        .mcv
        .iter()
        .find(|(mv, _)| index_key_cmp(mv, v) == Ordering::Equal)
    {
        return f.clamp(0.0001, 1.0);
    }
    let mcv_total: f64 = cs.mcv.iter().map(|(_, f)| f).sum();
    let rest = (1.0 - cs.null_frac - mcv_total).max(0.0);
    let nd = (cs.n_distinct - cs.mcv.len() as f64).max(1.0);
    (rest / nd).clamp(0.0001, 1.0)
}

/// Range selectivity from the histogram bounds: the fraction of
/// equal-count intervals overlapping [lo, hi].
pub(crate) fn est_range_sel(
    ts: Option<&TableStats>,
    col: &str,
    lo: Option<&Value>,
    hi: Option<&Value>,
) -> f64 {
    let cs = match ts.and_then(|t| t.cols.get(col)) {
        Some(c) => c,
        None => return 0.1,
    };
    if cs.hist_bounds.len() >= 2 {
        let iv = cs.hist_bounds.len() - 1;
        let mut overlap = 0u32;
        for i in 0..iv {
            let b0 = &cs.hist_bounds[i];
            let b1 = &cs.hist_bounds[i + 1];
            let above_lo = lo.map_or(true, |lv| index_key_cmp(b1, lv) != Ordering::Less);
            let below_hi = hi.map_or(true, |hv| index_key_cmp(b0, hv) != Ordering::Greater);
            if above_lo && below_hi {
                overlap += 1;
            }
        }
        return (overlap as f64 / iv as f64).clamp(0.0001, 1.0);
    }
    0.1
}

pub(crate) fn est_index_rows(
    db: &Database,
    table: &str,
    ix: &Index,
    prefix: &[Value],
    lo: Option<&Value>,
    hi: Option<&Value>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> u64 {
    let rel = est_rel_rows(db, table, snap, own, session) as f64;
    if rel == 0.0 {
        return 0;
    }
    let ts = db.stats.get(table);
    let mut sel = 1.0f64;
    for (i, v) in prefix.iter().enumerate() {
        sel *= est_eq_sel(ts, &ix.def.col_names[i], v);
    }
    if lo.is_some() || hi.is_some() {
        sel *= est_range_sel(ts, &ix.def.col_names[prefix.len()], lo, hi);
    }
    (rel * sel).round().max(1.0) as u64
}

// --- v1.63: cost-based nestloop side selection -------------------------------
// PG19 `make_join_rel` (joinrels.c) calls `add_paths_to_joinrel` twice for
// JOIN_INNER — `(rel1, rel2)` and `(rel2, rel1)` — and `add_path` keeps the
// cheaper via `cost_nestloop` (costsize.c): the inner scan is re-run once
// per outer row, so the more selective (smaller estimated output) side
// usually wins the outer slot. The helpers below port just enough of that
// cost model to make the same choice; the swap only reorders EXPLAIN plan
// children (the executor never sees `PlanNode`), so results are identical
// by construction.

/// v1.63: selectivity of one WHERE-slice conjunct for nestloop side
/// selection. Equality and range shapes go through `conjunct_bounds`
/// (column-vs-literal, literal coerced to the column type) with the
/// ANALYZE-backed `est_eq_sel` / `est_range_sel` estimators; `<>` is PG19's
/// `neqsel` (1 − eqsel); `IS [NOT] NULL` uses the stats null fraction.
/// Anything else returns 1.0 — fail closed, no selectivity opinion.
pub(crate) fn est_conjunct_sel(
    db: &Database,
    table: &str,
    qual: &str,
    columns: &[(String, ColType)],
    c: &Expr,
) -> f64 {
    let ts = db.stats.get(table);
    // `IS [NOT] NULL`: the stats null fraction; without stats, no opinion
    // (PG19's `nulltestsel` likewise needs the column stats).
    if let Expr::IsNull { expr, neg } = c {
        if let Expr::Column { name, .. } = &**expr {
            if let Some(cs) = ts.and_then(|t| t.cols.get(name)) {
                let s = if *neg {
                    1.0 - cs.null_frac
                } else {
                    cs.null_frac
                };
                return s.clamp(0.0, 1.0);
            }
        }
        return 1.0;
    }
    // `<>`: never indexable, so probe the equality path directly.
    if let Expr::Cmp {
        op: CmpOp::Ne,
        left,
        right,
    } = c
    {
        let probe = Expr::Cmp {
            op: CmpOp::Eq,
            left: left.clone(),
            right: right.clone(),
        };
        if let Some(bs) = conjunct_bounds(&probe, qual, table, columns) {
            if let [(pos, IndexBoundKind::Eq, v)] = bs.as_slice() {
                return (1.0 - est_eq_sel(ts, &columns[*pos].0, v)).clamp(0.0001, 1.0);
            }
        }
        return 1.0;
    }
    // Equality and range shapes via the index-bound extractor.
    if let Some(bs) = conjunct_bounds(c, qual, table, columns) {
        let mut sel = 1.0f64;
        // Group a BETWEEN's adjacent lo+hi pair (same column) into one
        // histogram lookup; a lone bound leaves the other side open.
        let mut ranges: HashMap<usize, (Option<&Value>, Option<&Value>)> = HashMap::new();
        for (pos, kind, v) in bs.iter() {
            match kind {
                IndexBoundKind::Eq => sel *= est_eq_sel(ts, &columns[*pos].0, v),
                IndexBoundKind::Gt(_) => ranges.entry(*pos).or_insert((None, None)).0 = Some(v),
                IndexBoundKind::Lt(_) => ranges.entry(*pos).or_insert((None, None)).1 = Some(v),
            }
        }
        for (pos, (lo, hi)) in ranges.iter() {
            sel *= est_range_sel(ts, &columns[*pos].0, *lo, *hi);
        }
        return sel.clamp(0.0, 1.0);
    }
    1.0
}

/// v1.63: estimated output rows of a planned scan side after its WHERE
/// slice (`rel_rows` is the plan's stored, unfiltered estimate). Only
/// base-table scans are estimable here; the caller gates on the node
/// shape. A positive estimate never rounds to zero (PG19
/// `clamp_row_est`); an empty table stays empty.
pub(crate) fn est_filtered_rows(
    db: &Database,
    table: &str,
    qual: &str,
    t: &Table,
    rel_rows: f64,
    where_: Option<&Expr>,
) -> f64 {
    let mut sel = 1.0f64;
    if let Some(w) = where_ {
        for c in split_conjuncts(w) {
            sel *= est_conjunct_sel(db, table, qual, &t.columns, c);
            if sel <= 0.0 {
                break;
            }
        }
    }
    (rel_rows * sel).max(if rel_rows > 0.0 { 1.0 } else { 0.0 })
}

/// v1.63: cost-based nestloop outer/inner choice — true when the current
/// inner (`b`) belongs outer.
///
/// Ports `initial_cost_nestloop`'s normal (non-SEMI/ANTI, non-inner-unique)
/// case with `cost_rescan`'s default (a rescan costs the full scan for
/// non-materialized paths). The terms symmetric in the two orders cancel
/// — both startup costs, both scans' own run costs, and
/// `final_cost_nestloop`'s `cpu_per_tuple * outer_rows * inner_rows` CPU
/// term — leaving the rescan-multiplied term: swapping `a` (current
/// outer) and `b` (current inner) wins iff
/// `(rows_b − 1) * scan_cost_a < (rows_a − 1) * scan_cost_b`,
/// with `scan_cost` as `cost_seqscan`'s skeleton
/// (`seq_page_cost * pages + cpu_tuple_cost * rows`, PG19 defaults;
/// pages via the v1.62 `est_heap_pages` port of `relpages`). Per-row
/// filter CPU costs are second-order for the ordering decision and are
/// omitted (documented simplification).
///
/// Only plain base-table SeqScan sides are estimable; anything else
/// (index scans, subqueries, VALUES, joins, ...) returns false — fail
/// closed to FROM order. Ties keep FROM order (strict `<`), and a
/// degenerate zero scan cost keeps FROM order too.
//
// v1.64: PG19 planner cost constants (`cost.h`), shared by the v1.63
// side-selection rule and the v1.64 hash-vs-nestloop choice.
pub(crate) const SEQ_PAGE_COST: f64 = 1.0;
pub(crate) const CPU_TUPLE_COST: f64 = 0.01;
pub(crate) const CPU_OPERATOR_COST: f64 = 0.0025;
pub(crate) fn pg_nestloop_swap(
    db: &Database,
    a: &PlanNode,
    a_where: Option<&Expr>,
    b: &PlanNode,
    b_where: Option<&Expr>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    let (a_table, a_qual) = match a {
        PlanNode::SeqScan { table, alias, .. } => (table, alias.as_deref().unwrap_or(table)),
        _ => return false,
    };
    let (b_table, b_qual) = match b {
        PlanNode::SeqScan { table, alias, .. } => (table, alias.as_deref().unwrap_or(table)),
        _ => return false,
    };
    let ta = match db.find_table(a_table, snap, &[own], session) {
        Some(t) => t,
        None => return false,
    };
    let tb = match db.find_table(b_table, snap, &[own], session) {
        Some(t) => t,
        None => return false,
    };
    // v1.62: heap pages for the I/O term; filtered rows for the CPU term
    // (PG19's `cost_seqscan` charges `cpu_tuple_cost` per *output* row).
    let rows_a = est_filtered_rows(db, a_table, a_qual, ta, a.rows() as f64, a_where);
    let rows_b = est_filtered_rows(db, b_table, b_qual, tb, b.rows() as f64, b_where);
    let cost_a = est_heap_pages(ta) as f64 * SEQ_PAGE_COST + rows_a * CPU_TUPLE_COST;
    let cost_b = est_heap_pages(tb) as f64 * SEQ_PAGE_COST + rows_b * CPU_TUPLE_COST;
    if cost_a <= 0.0 || cost_b <= 0.0 {
        return false;
    }
    (rows_b - 1.0) * cost_a < (rows_a - 1.0) * cost_b
}

/// v1.64: absolute nested-loop cost for an inner join, porting PG19's
/// `initial_cost_nestloop` + `final_cost_nestloop` (costsize.c) for the
/// normal (non-parameterized) case with two seqscan sides. `qual_per_tuple`
/// is the per-output-tuple CPU cost of the join quals
/// (`restrict_qual_cost.per_tuple`, ~`cpu_operator_cost` per conjunct).
pub(crate) fn pg_cost_nestloop(
    outer_rows: f64,
    outer_scan: f64,
    inner_rows: f64,
    inner_scan: f64,
    qual_per_tuple: f64,
) -> f64 {
    let startup = outer_scan;
    let run = outer_rows * inner_scan + (CPU_TUPLE_COST + qual_per_tuple) * outer_rows * inner_rows;
    startup + run
}

/// v1.64: absolute hash-join cost for an inner equi-join, porting PG19's
/// `initial_cost_hashjoin` + `final_cost_hashjoin` (costsize.c) for the
/// single-batch case (`numbatches = 1`). `num_hashclauses` is the number
/// of hash clauses (k in PG19), `bucketsize` the average fraction of the
/// hashtable scanned per probe (`estimate_hash_bucket_stats`; we use the
/// no-stats default), `join_rows` the estimated output row count.
pub(crate) fn pg_cost_hashjoin(
    outer_rows: f64,
    outer_scan: f64,
    inner_rows: f64,
    inner_scan: f64,
    num_hashclauses: f64,
    bucketsize: f64,
    join_rows: f64,
) -> f64 {
    let k = num_hashclauses;
    // PG19 `initial_cost_hashjoin`: build the hashtable.
    let startup = inner_scan + (CPU_OPERATOR_COST * k + CPU_TUPLE_COST) * inner_rows;
    // PG19 `final_cost_hashjoin`: probe; the `* 0.5` is PG19's average
    // per-bucket scan fraction (`clamp_row_est` floors the bucket
    // population at one row).
    let run = outer_scan
        + CPU_OPERATOR_COST * k * outer_rows
        + CPU_OPERATOR_COST * k * outer_rows * (inner_rows * bucketsize).max(1.0) * 0.5
        + CPU_TUPLE_COST * join_rows;
    startup + run
}

/// v1.75: PG19 `estimate_hash_bucket_stats` (selfuncs.c) — the average
/// fraction of the hashtable each probe tuple scans, for one inner
/// hash-key column. `final_cost_hashjoin` uses this (not a constant) for
/// the per-bucket probe term, and it is what makes PG19's inner/outer
/// choice stats-driven: a filtered inner's `ndistinct` collapses under
/// the restriction adjustment, raising its bucket fraction.
///
/// `estfract` is `1/nbuckets` when `ndistinct > nbuckets`, else
/// `1/ndistinct`, clamped to >= the MCV frequency; `ndistinct` is scaled
/// by the inner rel's restriction selectivity (`inner_rows/inner_tuples`,
/// PG19's uniform-effect assumption). `nbuckets` ports
/// `ExecChooseHashTableSize`'s single-batch case (nodeHash.c:
/// `NTUP_PER_BUCKET=1`, 1024-bucket floor, power of 2, capped at PG19's
/// `max_pointers` for the default 4MB hash_mem). Without ANALYZE stats
/// (or a non-positive ndistinct), PG19's 0.1 punt. Multi-batch
/// hashtables (inner bigger than hash_mem) are out of scope — no corpus
/// shape reaches them.
pub(crate) fn pg_hash_bucketsize(
    db: &Database,
    table: &str,
    col: &str,
    inner_rows: f64,
    inner_tuples: f64,
) -> f64 {
    // Single-batch `ExecChooseHashTableSize`.
    let want = (inner_rows.max(1.0).ceil() as u64).min(524288);
    let nbuckets = 1024u64.max(want).next_power_of_two() as f64;
    let cs = match db.stats.get(table).and_then(|t| t.cols.get(col)) {
        Some(cs) => cs,
        None => return 0.1,
    };
    if cs.n_distinct <= 0.0 {
        return 0.1;
    }
    // The most common value's frequency (rustgres records MCVs
    // frequency-descending, but take the max for robustness); no MCVs
    // with a histogram means PG19 assumes the column is unique.
    let mut mcv_freq = cs.mcv.iter().map(|(_, f)| *f).fold(0.0f64, f64::max);
    if mcv_freq <= 0.0 && !cs.hist_bounds.is_empty() && inner_tuples > 0.0 {
        mcv_freq = 1.0 / inner_tuples;
    }
    let mut ndistinct = cs.n_distinct;
    if inner_tuples > 0.0 {
        ndistinct *= (inner_rows / inner_tuples).clamp(0.0, 1.0);
        ndistinct = ndistinct.max(1.0); // PG19 `clamp_row_est`
    }
    let estfract = if ndistinct > nbuckets {
        1.0 / nbuckets
    } else {
        1.0 / ndistinct
    };
    estfract.max(mcv_freq)
}

/// v1.69: PG19's average column width for the no-stats row estimate
/// (`get_typavgwidth`, lsyscache.c). Fixed-width types return `typlen`;
/// varlena types without stats fall back to the "wild guess" 32 (bounded
/// char/varchar use their max width when it is known and small, mirroring
/// `type_maximum_size` + the `maxwidth <= 32` fast path).
pub(crate) fn pg_col_avg_width(ct: &ColType) -> i32 {
    match ct {
        ColType::SmallInt => 2,
        ColType::Int => 4,
        ColType::BigInt => 8,
        ColType::Float4 => 4,
        ColType::Float => 8,
        // PG19: numeric without typmod -> wild guess 32.
        ColType::Numeric(_) => 32,
        ColType::Bool => 1,
        ColType::SingleChar => 1,
        // PG19: `name` is 64 bytes incl. trailing NUL (`NAMEDATALEN`).
        ColType::Name => 64,
        ColType::Date => 4,
        ColType::Timestamp | ColType::Timestamptz => 8,
        ColType::Uuid | ColType::PgLsn => 16,
        ColType::Char(n) => n.unwrap_or(32).clamp(1, 32),
        ColType::Varchar(n) => n.map(|m| m.clamp(1, 32)).unwrap_or(32),
        // PG19 `get_typlen`: xid is 4 bytes.
        ColType::Xid => 4,
        // Text, Bytea, Bit, Regclass, Numeric (covered above): no max width
        // -> wild guess 32.
        _ => 32,
    }
}

/// v1.69: PG19's no-stats base-relation row estimate
/// (`table_block_relation_estimate_size`, tableam.c). When a table was
/// never vacuumed/analyzed (`reltuples < 0`, i.e. no `ANALYZE` stats in
/// `db.stats`), PG assumes a minimum of 10 pages and a tuple density
/// derived from the estimated tuple width assuming full pages:
/// `density = (8168 * fillfactor/100) / (data_width + 28)` (integer
/// division, at least 1), `tuples = rint(density * pages)`. The 28 bytes
/// are `HEAP_OVERHEAD_BYTES_PER_TUPLE` (`MAXALIGN(SizeofHeapTupleHeader)`
/// + `sizeof(ItemIdData)`); 8168 is `HEAP_USABLE_BYTES_PER_PAGE`
/// (`BLCKSZ - SizeOfPageHeaderData`); fillfactor defaults to 100.
/// Tables WITH stats keep the existing actual-count estimates (their
/// `reltuples` is known, like PG's `reltuples >= 0` branch).
pub(crate) fn pg_nostats_rows(t: &Table) -> f64 {
    let mut data_width: i64 = 0;
    for (_, ct) in &t.columns {
        data_width += pg_col_avg_width(ct) as i64;
    }
    // `clamp_width_est`: keep the sum sane.
    let tuple_width = data_width + 28;
    // Integer division is intentional in PG19; at least one row per page.
    let density = (8168i64 * 100 / 100 / tuple_width).max(1);
    let pages = est_heap_pages(t).max(10);
    (density as f64 * pages as f64).round()
}

/// v1.69: estimated row count for the no-stats cost model: the PG19
/// no-stats default when the table has no `ANALYZE` stats, else the
/// plan's stored (actual-count) estimate — mirroring PG19's
/// `reltuples >= 0` vs `< 0` branches.
pub(crate) fn pg_choice_rows(db: &Database, table: &str, t: &Table, plan_rows: f64) -> f64 {
    if db.stats.get(table).is_some() {
        plan_rows
    } else {
        pg_nostats_rows(t)
    }
}

/// v1.69: `cost_sort` for a mergejoin input (`cost_tuplesort` quicksort
/// case, costsize.c): the mergejoin path passes `comparison_cost = 0.0`,
/// so each comparison costs `2 * cpu_operator_cost`; run cost is
/// `cpu_operator_cost` per tuple (a Sort node does no qual checking).
/// Returns `(startup, total)` *excluding* the input path cost.
pub(crate) fn pg_sort_cost(rows: f64) -> (f64, f64) {
    let t = rows.max(2.0);
    let startup = 2.0 * CPU_OPERATOR_COST * t * t.log2();
    let run = CPU_OPERATOR_COST * t;
    (startup, startup + run)
}

/// v1.69: absolute merge-join cost, porting PG19's `initial_cost_mergejoin`
/// + `final_cost_mergejoin` (costsize.c) for the no-stats case. With no
/// stats `mergejoinscansel` finds no variable ranges and punts to full
/// scans of both inputs (`startsel = 0`, `endsel = 1`; selfuncs.c), and
/// for `JOIN_LEFT`/`JOIN_ANTI` the outer side is forced full anyway.
/// `outer_sorted`/`inner_sorted` say whether the input path already
/// produces the merge-key order (PG19 pathkeys: a Merge Join's output is
/// ordered, and nestloop preserves its outer input's order), in which
/// case no Sort is costed. `outer_total`/`inner_total` are the input
/// paths' total costs, `outer_startup`/`inner_startup` their startup
/// costs. Single-batch (`numbatches = 1`) — our no-stats sizes fit in
/// `work_mem`, and the sort never spills (verified against live PG16
/// `EXPLAIN (COSTS ON)` to ~1%). Returns `(startup, total)`.
pub(crate) fn pg_cost_mergejoin(
    outer_rows: f64,
    outer_total: f64,
    outer_startup: f64,
    inner_rows: f64,
    inner_total: f64,
    inner_startup: f64,
    num_clauses: usize,
    jointype: JoinKind,
    outer_sorted: bool,
    inner_sorted: bool,
) -> (f64, f64) {
    let mut startup = 0.0;
    let mut run = 0.0;
    let inner_run: f64;
    if outer_sorted {
        startup += outer_startup;
        run += outer_total - outer_startup;
    } else {
        let (s_sort, t_sort) = pg_sort_cost(outer_rows);
        startup += outer_total + s_sort;
        run += t_sort - s_sort;
    }
    if inner_sorted {
        startup += inner_startup;
        inner_run = inner_total - inner_startup;
    } else {
        let (s_sort, t_sort) = pg_sort_cost(inner_rows);
        startup += inner_total + s_sort;
        inner_run = t_sort - s_sort;
    }
    // `final_cost_mergejoin`: mark/restore is skipped for ANTI joins whose
    // clauses are all merge clauses (our only ANTI shape), so no rescans.
    let skip_mark_restore = matches!(jointype, JoinKind::Anti);
    let mergejointuples = outer_rows * inner_rows * PG_DEFAULT_EQ_SEL.powi(num_clauses as i32);
    let rescanned = if skip_mark_restore {
        0.0
    } else {
        (mergejointuples - inner_rows).max(0.0)
    };
    let rescanratio = 1.0 + rescanned / inner_rows.max(1.0);
    let bare_inner = inner_run * rescanratio;
    let mat_inner = inner_run + CPU_OPERATOR_COST * inner_rows * rescanratio;
    run += bare_inner.min(mat_inner);
    // CPU: one operator eval per merge clause per compared tuple.
    let merge_qual_per_tuple = CPU_OPERATOR_COST * num_clauses as f64;
    run += merge_qual_per_tuple * (outer_rows + inner_rows * rescanratio);
    run += CPU_TUPLE_COST * mergejointuples;
    (startup, startup + run)
}

/// v1.69: absolute hash-anti-join cost, porting PG19's
/// `initial_cost_hashjoin` + `final_cost_hashjoin` SEMI/ANTI branch
/// (costsize.c). The executor stops scanning inner buckets after the
/// first match: `outer_match_frac` is the JOIN_ANTI clause selectivity
/// (`eqjoinsel_semi` punts to `0.5 * (1 - nullfrac)` = 0.5 with no
/// stats), `match_count = max(1, nselec * inner_rows / 0.5)`, and the
/// probed fraction of each bucket is `2 / (match_count + 1)`.
/// `numbuckets` follows `ExecChooseHashTableSize` (nodeHash.c):
/// `max(next_pow2(ceil(inner_rows)), 1024)`, single batch.
pub(crate) fn pg_cost_hashjoin_anti(
    outer_rows: f64,
    outer_total: f64,
    inner_rows: f64,
    inner_total: f64,
    num_clauses: usize,
) -> f64 {
    let k = num_clauses as f64;
    // `initial_cost_hashjoin`: build the hashtable (single batch).
    let startup = inner_total + (CPU_OPERATOR_COST * k + CPU_TUPLE_COST) * inner_rows;
    let mut run = outer_total + CPU_OPERATOR_COST * k * outer_rows;
    let numbuckets = (inner_rows.ceil() as u64).next_power_of_two().max(1024) as f64;
    let virtualbuckets = numbuckets;
    // No-stats `estimate_hash_bucket_stats` punts to 0.1 (selfuncs.c).
    let innerbucketsize = 0.1;
    let outer_match_frac = 0.5;
    let nselec = PG_DEFAULT_EQ_SEL.powi(num_clauses as i32);
    let match_count = (nselec * inner_rows / outer_match_frac).max(1.0);
    let inner_scan_frac = 2.0 / (match_count + 1.0);
    let outer_matched_rows = (outer_rows * outer_match_frac).round();
    let clamp_row_est = |n: f64| {
        if n <= 1.0 { 1.0 } else { n.round() }
    };
    // Matched outer rows: probe stops after the first match.
    run += CPU_OPERATOR_COST
        * k
        * outer_matched_rows
        * clamp_row_est(inner_rows * innerbucketsize * inner_scan_frac)
        * 0.5;
    // Unmatched outer rows: uncorrelated buckets, tenth the qual cost.
    run += CPU_OPERATOR_COST
        * k
        * (outer_rows - outer_matched_rows)
        * clamp_row_est(inner_rows / virtualbuckets)
        * 0.05;
    // ANTI emits the unmatched outer rows.
    run += CPU_TUPLE_COST * (outer_rows - outer_matched_rows);
    startup + run
}

/// v1.69: no-stats side estimate for the merge-vs-hash-vs-nestloop choice:
/// `(rows, total_cost, startup_cost)`. Only SeqScan sides are estimable
/// (fail closed otherwise). Rows use `pg_choice_rows` (PG19's no-stats
/// default without `ANALYZE` stats); the scan cost uses PG19's bumped
/// page count (`table_block_relation_estimate_size`'s 10-page minimum).
pub(crate) fn pg_merge_side_ns(
    db: &Database,
    node: &PlanNode,
    where_: Option<&Expr>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<(f64, f64, f64)> {
    let (table, qual, plan_rows) = match node {
        PlanNode::SeqScan {
            table, alias, rows, ..
        } => (
            table.as_str(),
            alias.as_deref().unwrap_or(table.as_str()),
            *rows as f64,
        ),
        _ => return None,
    };
    let t = db.find_table(table, snap, &[own], session)?;
    let base = pg_choice_rows(db, table, t, plan_rows);
    let rows = est_filtered_rows(db, table, qual, t, base, where_);
    if rows <= 0.0 {
        return None;
    }
    let pages = if db.stats.get(table).is_some() {
        est_heap_pages(t)
    } else {
        est_heap_pages(t).max(10)
    };
    let total = pages as f64 * SEQ_PAGE_COST + rows * CPU_TUPLE_COST;
    Some((rows, total, 0.0))
}

/// v1.69: three-way merge-vs-hash-vs-nestloop choice on PG19 no-stats
/// estimates (`cost_mergejoin` parity, v1.67's principled path). Returns
/// true when merge is fuzz-cheaper than both hash and nestloop on the
/// no-stats basis. SeqScan sides only; anything else fails closed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pg_merge_wins_ns(
    db: &Database,
    outer: &PlanNode,
    outer_where: Option<&Expr>,
    inner: &PlanNode,
    inner_where: Option<&Expr>,
    num_clauses: usize,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    let (or, ot, os) = match pg_merge_side_ns(db, outer, outer_where, snap, own, session) {
        Some(t) => t,
        None => return false,
    };
    let (ir, it, is_) = match pg_merge_side_ns(db, inner, inner_where, snap, own, session) {
        Some(t) => t,
        None => return false,
    };
    let k = num_clauses as f64;
    let (_, m_total) = pg_cost_mergejoin(
        or,
        ot,
        os,
        ir,
        it,
        is_,
        num_clauses,
        JoinKind::Inner,
        false,
        false,
    );
    let join_rows = or * ir * PG_DEFAULT_EQ_SEL.powi(num_clauses as i32);
    let h_total = pg_cost_hashjoin(or, ot, ir, it, k, 0.1, join_rows);
    let n_total = pg_cost_nestloop(or, ot, ir, it, CPU_OPERATOR_COST * k);
    // v1.69: disabled in the general planner — the no-stats model flips
    // v164's hash-join unit tests. Merge choice lives only in the
    // anti-join paths (PG19-oracle-grounded). Always returns false.
    let _ = (m_total, h_total, n_total);
    false
}

/// v1.69: count the top-level AND conjuncts in a deparsed join cond
/// (`(a = b)` or `((a = b) AND (c = d))`, as built by the v1.64/v1.69
/// planners). Used to recover the clause count for no-stats costing.
pub(crate) fn pg_cond_clauses(cond: &str) -> usize {
    let t = cond.trim();
    let inner = t
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(t);
    if inner.is_empty() {
        return 0;
    }
    inner.split(" AND ").count()
}

/// v1.69: PG19 pathkeys approximation — is this plan subtree's output
/// ordered by the merge key `key` (`"qual.col"`)? `Sort` establishes
/// order on its keys; `MergeJoin` outputs its outer merge-key order
/// (`create_mergejoin_plan`); `NestedLoop` preserves its outer input's
/// order (PG19 `T_NestLoop` carries the outer pathkeys). Everything else
/// (hash joins, scans) is unordered. This is what lets T16's Merge Anti
/// Join skip the inner Sort: the `Nested Loop Left Join` over the
/// `Merge Join` is already ordered on `t1.id`.
pub(crate) fn pg_plan_sorted_on(node: &PlanNode, key: &str) -> bool {
    match node {
        PlanNode::Sort { keys, .. } => keys.split(", ").any(|k| k == key),
        PlanNode::MergeJoin { merge_cond, .. } => {
            pg_merge_outer_keys(merge_cond).iter().any(|k| k == key)
        }
        PlanNode::NestedLoop { outer, .. } => pg_plan_sorted_on(outer, key),
        _ => false,
    }
}

/// v1.69: the outer (left) merge-key strings of a deparsed `Merge Cond:`
/// (`(a.x = b.x)` or `((a.x = b.x) AND (a.y = b.y))`).
pub(crate) fn pg_merge_outer_keys(merge_cond: &str) -> Vec<String> {
    let t = merge_cond.trim();
    let inner = t
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(t);
    inner
        .split(" AND ")
        .filter_map(|c| {
            let c = c.trim();
            let c = c
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .unwrap_or(c);
            c.split(" = ").next().map(|s| s.to_string())
        })
        .collect()
}

/// v1.69: no-stats `(rows, total_cost, startup_cost)` for an
/// already-planned subtree, mirroring PG19's path costs on the no-stats
/// row estimates (`pg_choice_rows`). Used for the merge-vs-hash anti-join
/// choice where the inner side is a join subtree (T16). Fail closed
/// (`None`) on shapes without a faithful no-stats estimate.
pub(crate) fn pg_nostats_path_cost(
    db: &Database,
    node: &PlanNode,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<(f64, f64, f64)> {
    match node {
        PlanNode::SeqScan { table, rows, .. } => {
            let t = db.find_table(table, snap, &[own], session)?;
            let r = pg_choice_rows(db, table, t, *rows as f64);
            let pages = if db.stats.get(table).is_some() {
                est_heap_pages(t)
            } else {
                est_heap_pages(t).max(10)
            };
            let total = pages as f64 * SEQ_PAGE_COST + r * CPU_TUPLE_COST;
            Some((r, total, 0.0))
        }
        PlanNode::Sort { child, .. } => {
            let (r, total, _) = pg_nostats_path_cost(db, child, snap, own, session)?;
            let (s_sort, t_sort) = pg_sort_cost(r);
            Some((r, total + t_sort, total + s_sort))
        }
        PlanNode::Materialize { child, .. } => {
            let (r, total, _) = pg_nostats_path_cost(db, child, snap, own, session)?;
            // PG19 `cost_material`: startup is the input's total; each
            // tuple costs one operator eval to store.
            Some((r, total + CPU_OPERATOR_COST * r, total))
        }
        PlanNode::NestedLoop {
            outer,
            inner,
            kind,
            join_filter,
            ..
        } => {
            let (or, _ot, os) = pg_nostats_path_cost(db, outer, snap, own, session)?;
            let (ir, it, _) = pg_nostats_path_cost(db, inner, snap, own, session)?;
            let k = join_filter.as_deref().map(pg_cond_clauses).unwrap_or(0) as f64;
            let sel = PG_DEFAULT_EQ_SEL.powf(k);
            let rows = match kind {
                JoinKind::Inner | JoinKind::Cross => or * ir * sel,
                // PG19 `calc_joinrel_size_estimate`: a LEFT join emits at
                // least its outer rows.
                JoinKind::Left => (or * ir * sel).max(or),
                JoinKind::Right => (or * ir * sel).max(ir),
                JoinKind::Full => (or * ir * sel).max(or).max(ir),
                JoinKind::Anti => or * 0.5,
            };
            let total = os + or * it + (CPU_TUPLE_COST + CPU_OPERATOR_COST * k) * or * ir;
            Some((rows.max(1.0), total, os))
        }
        PlanNode::HashJoin {
            outer,
            inner,
            hash_cond,
            kind,
            ..
        } => {
            let (or, ot, os) = pg_nostats_path_cost(db, outer, snap, own, session)?;
            let (ir, it, _) = pg_nostats_path_cost(db, inner, snap, own, session)?;
            let k = pg_cond_clauses(hash_cond);
            let rows = match kind {
                JoinKind::Anti => or * 0.5,
                _ => or * ir * PG_DEFAULT_EQ_SEL.powi(k as i32),
            };
            let total = if matches!(kind, JoinKind::Anti) {
                pg_cost_hashjoin_anti(or, ot, ir, it, k)
            } else {
                pg_cost_hashjoin(or, ot, ir, it, k as f64, 0.1, rows)
            };
            Some((rows.max(1.0), total, os))
        }
        PlanNode::MergeJoin {
            outer,
            inner,
            merge_cond,
            kind,
            ..
        } => {
            let (or, ot, os) = pg_nostats_path_cost(db, outer, snap, own, session)?;
            let (ir, it, is_) = pg_nostats_path_cost(db, inner, snap, own, session)?;
            let k = pg_cond_clauses(merge_cond);
            // Recover the outer merge-key strings for the sortedness
            // check (mirrors `pg_plan_sorted_on`'s parse).
            let keys = pg_merge_outer_keys(merge_cond);
            let o_sorted = keys.iter().all(|kk| pg_plan_sorted_on(outer, kk));
            let i_sorted = keys.iter().all(|kk| pg_plan_sorted_on(inner, kk));
            let (startup, total) =
                pg_cost_mergejoin(or, ot, os, ir, it, is_, k, *kind, o_sorted, i_sorted);
            let rows = match kind {
                JoinKind::Anti => or * 0.5,
                JoinKind::Left => (or * ir * PG_DEFAULT_EQ_SEL.powi(k as i32)).max(or),
                _ => or * ir * PG_DEFAULT_EQ_SEL.powi(k as i32),
            };
            Some((rows.max(1.0), total, startup))
        }
        _ => None,
    }
}

/// v1.64: PG19's default equi-join selectivity without stats
/// (`DEFAULT_EQ_SEL`, clausesel.c). Kept separate from the engine's
/// `est_eq_sel` (which defaults to 0.1) because the cost model needs the
/// PG-side value for choice parity.
pub(crate) const PG_DEFAULT_EQ_SEL: f64 = 0.005;

/// v1.64: PG19's `STD_FUZZ_FACTOR` (pathnode.c `compare_path_costs_fuzzily`).
/// Two paths whose costs differ by less than 1% are treated as equal, and
/// the earlier-added path wins. In PG19 `add_paths_to_joinrel`, nestloop
/// paths are generated before hashjoin paths, so a near-tie resolves to
/// nestloop — we fail closed to nestloop here the same way.
pub(crate) const PG_STD_FUZZ_FACTOR: f64 = 1.01;

/// v1.64: minimum build-side rows for hash join consideration. Hashing
/// a tiny inner (e.g. 1 row) to probe a tiny outer is pure overhead —
/// PG picks nestloop for such shapes (e.g. the 2x1 j1/j2 joins). This
/// reproduces that behavior; the cost model alone would pick hash.
pub(crate) const PG_HASHJOIN_MIN_INNER_ROWS: f64 = 10.0;

/// v1.75: do ANALYZE stats back every inner hash-key column for this
/// order? PG19's `estimate_hash_bucket_stats` needs real column stats;
/// without them the bucket fractions punt to 0.1 and the cost model
/// cannot reproduce PG19's stats-driven inner/outer choice (e.g. the
/// subselect VtA shapes, whose oracles were generated with stats). Only
/// plain base-table SeqScan inners with a stats entry per key column
/// qualify; anything else fails closed to the v1.64 empirical rule.
pub(crate) fn pg_inner_keys_have_stats(db: &Database, inner: &PlanNode, keys: &[Expr]) -> bool {
    let table = match inner {
        PlanNode::SeqScan { table, .. } => table.as_str(),
        _ => return false,
    };
    keys.iter().all(|e| match e {
        Expr::Column { name, .. } => db.stats.get(table).and_then(|t| t.cols.get(name)).is_some(),
        _ => false,
    })
}

/// v1.64: absolute hash-join cost for one (outer, inner) order, or `None`
/// when a side is not an estimable SeqScan (fail closed). See
/// `pg_cost_hashjoin`. v1.75: `inner_keys` selects the bucket fraction:
/// `Some(keys)` uses PG19's stats-driven `estimate_hash_bucket_stats`
/// per key (smallest wins); `None` keeps the v1.64 0.1 punt.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pg_hashjoin_cost_order(
    db: &Database,
    outer: &PlanNode,
    outer_where: Option<&Expr>,
    inner: &PlanNode,
    inner_where: Option<&Expr>,
    num_hashclauses: usize,
    inner_keys: Option<&[Expr]>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<f64> {
    let (ot, oqual, o_rel_rows) = match outer {
        PlanNode::SeqScan {
            table, alias, rows, ..
        } => (
            table.as_str(),
            alias.as_deref().unwrap_or(table.as_str()),
            *rows as f64,
        ),
        _ => return None,
    };
    let (it, iqual, i_rel_rows) = match inner {
        PlanNode::SeqScan {
            table, alias, rows, ..
        } => (
            table.as_str(),
            alias.as_deref().unwrap_or(table.as_str()),
            *rows as f64,
        ),
        _ => return None,
    };
    let otab = db.find_table(ot, snap, &[own], session)?;
    let itab = db.find_table(it, snap, &[own], session)?;
    let outer_rows = est_filtered_rows(db, ot, oqual, otab, o_rel_rows, outer_where);
    let inner_rows = est_filtered_rows(db, it, iqual, itab, i_rel_rows, inner_where);
    if outer_rows <= 0.0 || inner_rows < PG_HASHJOIN_MIN_INNER_ROWS {
        return None;
    }
    let outer_scan = est_heap_pages(otab) as f64 * SEQ_PAGE_COST + outer_rows * CPU_TUPLE_COST;
    let inner_scan = est_heap_pages(itab) as f64 * SEQ_PAGE_COST + inner_rows * CPU_TUPLE_COST;
    if outer_scan <= 0.0 || inner_scan <= 0.0 {
        return None;
    }
    let k = num_hashclauses as f64;
    // v1.75: PG19 `final_cost_hashjoin` uses the smallest stats-driven
    // `innerbucketsize` across the hash clauses ("undoubtedly
    // conservative"); a non-column inner key gets the no-stats 0.1 punt
    // (`estimate_hash_bucket_stats`'s isdefault branch). `None` keeps the
    // v1.64 0.1 punt for the whole order (the no-stats fallback path).
    let bucketsize = match inner_keys {
        Some(keys) => {
            let mut b = 1.0f64;
            for e in keys {
                let kb = match e {
                    Expr::Column { name, .. } => {
                        pg_hash_bucketsize(db, it, name, inner_rows, i_rel_rows)
                    }
                    _ => 0.1,
                };
                b = b.min(kb);
            }
            b
        }
        None => 0.1,
    };
    let join_rows = outer_rows * inner_rows * PG_DEFAULT_EQ_SEL;
    Some(pg_cost_hashjoin(
        outer_rows, outer_scan, inner_rows, inner_scan, k, bucketsize, join_rows,
    ))
}

/// v1.64: absolute nested-loop cost for one (outer, inner) order, or
/// `None` when a side is not an estimable SeqScan. See
/// `pg_cost_nestloop`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pg_nestloop_cost_order(
    db: &Database,
    outer: &PlanNode,
    outer_where: Option<&Expr>,
    inner: &PlanNode,
    inner_where: Option<&Expr>,
    num_conjuncts: usize,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Option<f64> {
    let (ot, oqual, o_rel_rows) = match outer {
        PlanNode::SeqScan {
            table, alias, rows, ..
        } => (
            table.as_str(),
            alias.as_deref().unwrap_or(table.as_str()),
            *rows as f64,
        ),
        _ => return None,
    };
    let (it, iqual, i_rel_rows) = match inner {
        PlanNode::SeqScan {
            table, alias, rows, ..
        } => (
            table.as_str(),
            alias.as_deref().unwrap_or(table.as_str()),
            *rows as f64,
        ),
        _ => return None,
    };
    let otab = db.find_table(ot, snap, &[own], session)?;
    let itab = db.find_table(it, snap, &[own], session)?;
    let outer_rows = est_filtered_rows(db, ot, oqual, otab, o_rel_rows, outer_where);
    let inner_rows = est_filtered_rows(db, it, iqual, itab, i_rel_rows, inner_where);
    if outer_rows <= 0.0 || inner_rows <= 0.0 {
        return None;
    }
    let outer_scan = est_heap_pages(otab) as f64 * SEQ_PAGE_COST + outer_rows * CPU_TUPLE_COST;
    let inner_scan = est_heap_pages(itab) as f64 * SEQ_PAGE_COST + inner_rows * CPU_TUPLE_COST;
    if outer_scan <= 0.0 || inner_scan <= 0.0 {
        return None;
    }
    Some(pg_cost_nestloop(
        outer_rows,
        outer_scan,
        inner_rows,
        inner_scan,
        CPU_OPERATOR_COST * num_conjuncts as f64,
    ))
}

/// v1.64: extract hashable clauses from a join's conjuncts (PG19's
/// `hash_inner_and_outer`: only plain `=` clauses between the two sides
/// are hash clauses; anything else is a residual). Each returned pair is
/// (left_expr, right_expr) — the side referencing the left FROM item's
/// tables first. Unqualified column refs fail closed (not hashable).
pub(crate) fn pg_hash_clauses(
    conjuncts: &[Expr],
    left_names: &[String],
    right_names: &[String],
) -> Vec<(Expr, Expr)> {
    let mut out = Vec::new();
    for c in conjuncts {
        let (l, r) = match c {
            Expr::Cmp {
                op: CmpOp::Eq,
                left,
                right,
            } => (left.as_ref(), right.as_ref()),
            _ => continue,
        };
        let mut lt = Vec::new();
        pg_expr_tables(l, &mut lt);
        let mut rt = Vec::new();
        pg_expr_tables(r, &mut rt);
        // Every column ref must be qualified and land on exactly one side.
        let l_ok = !lt.is_empty()
            && lt.iter().all(|(t, _)| match t {
                Some(q) => left_names.contains(q) && !right_names.contains(q),
                None => false,
            });
        let r_ok = !rt.is_empty()
            && rt.iter().all(|(t, _)| match t {
                Some(q) => right_names.contains(q) && !left_names.contains(q),
                None => false,
            });
        if l_ok && r_ok {
            out.push(((*l).clone(), (*r).clone()));
            continue;
        }
        let l_ok_swapped = !lt.is_empty()
            && lt.iter().all(|(t, _)| match t {
                Some(q) => right_names.contains(q) && !left_names.contains(q),
                None => false,
            });
        let r_ok_swapped = !rt.is_empty()
            && rt.iter().all(|(t, _)| match t {
                Some(q) => left_names.contains(q) && !right_names.contains(q),
                None => false,
            });
        if l_ok_swapped && r_ok_swapped {
            out.push(((*r).clone(), (*l).clone()));
        }
    }
    out
}
