// v1.78 mechanical split: moved verbatim from src/exec.rs (36410-40782).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// v0.79: real array support (PG19 arrayfuncs.c / parse_expr.c semantics)
// ---------------------------------------------------------------------------

/// v0.79: `ARRAY[]::<elem>[]` — an empty array constructor takes its
/// element type from an enclosing array cast (PG's parse analysis
/// coerces the empty ArrayExpr to the cast target); a bare empty
/// constructor is still 42P08. Returns `Some` only for the empty
/// constructor under an array cast.
pub(crate) fn cast_empty_array_ctor(expr: &Expr, to: &ColType) -> Option<Value> {
    match (expr, to) {
        (Expr::ArrayCtor { elems, .. }, ColType::Array(elem)) if elems.is_empty() => {
            Some(Value::Array(Box::new(ArrayVal {
                elem: *elem,
                dims: Vec::new(),
                lower: Vec::new(),
                elems: Vec::new(),
            })))
        }
        _ => None,
    }
}

/// v0.79: evaluate `ARRAY[...]` to a real array value. The element type
/// is PG19's `select_common_type` over the elements (NULLs don't
/// constrain, all-unknown resolves to text); an empty list is 42P08.
/// The nested `ARRAY[[...],[...]]` form requires every element to be an
/// array with identical dims (PG's "multidimensional arrays must have
/// array expressions with matching dimensions", 22P02); dims stack.
pub(crate) fn eval_array_ctor(
    q: &mut Q,
    scopes: &[Scope],
    elems: &[Expr],
    nested: bool,
) -> Result<Value, ExecError> {
    if elems.is_empty() {
        return Err(exec_err("42P08", "cannot determine type of empty array"));
    }
    let mut vals: Vec<Value> = Vec::with_capacity(elems.len());
    for e in elems {
        vals.push(eval_expr(q, scopes, e)?);
    }
    array_ctor_from_vals(vals, nested, false)
}

/// v0.79: `ARRAY[...]` from already-evaluated element values — shared by
/// `eval_expr` and `eval_grouped` (which evaluate operands differently).
/// The flat form takes PG19's `select_common_type` over the elements
/// (NULLs don't constrain; all-unknown resolves to text). The nested
/// `ARRAY[[...],[...]]` form requires every element to be an array with
/// identical element type and dims (PG's "multidimensional arrays must
/// have array expressions with matching dimensions", 22P02); dims stack.
/// v0.92: shared `array_agg` finalization for the grouped and windowed
/// paths. PG semantics (PG19 docs §9.21; PG17 REL_17_STABLE arrayfuncs.c
/// `array_agg_transfn` / `accumArrayResultArr` — the PG19 tree on disk
/// lacks utils/adt, so the C grounding is PG17):
/// - zero input rows -> NULL;
/// - scalar input: NULLs are kept — "Collects all the input values,
///   including nulls, into an array";
/// - array input: each input array becomes one sub-array of the
///   (n+1)-dimensional result; a NULL input is 22004 "cannot
///   accumulate null arrays", an empty first input is 2202E "cannot
///   accumulate empty arrays" (a later empty input fails the
///   dimensionality check instead), and inputs whose dimension
///   count, dimension lengths, or lower bounds differ are 2202E
///   "cannot accumulate arrays of different dimensionality".
/// v0.92: static array-type probe for `array_agg` overload resolution.
/// PG picks `array_agg(anyarray)` vs `array_agg(anynonarray)` by the
/// argument's STATIC type. When every runtime value is NULL there is no
/// `Value::Array` to inspect, so consult the resolved expression:
/// array constructors and casts to an array type are statically arrays.
/// Column references need the query schema (grouped path passes it);
/// anything else falls back to the runtime-value heuristic.
pub(crate) fn expr_is_statically_array(e: &Expr, schema: Option<&[QCol]>) -> bool {
    match e {
        Expr::ArrayCtor { .. } => true,
        Expr::Cast { to, .. } => matches!(to, ColType::Array(_)),
        Expr::Column { table, name } => schema.is_some_and(|s| {
            s.iter().any(|c| {
                &c.name == name
                    && table.as_deref().is_none_or(|t| c.qual == t)
                    && matches!(c.ty, ColType::Array(_))
            })
        }),
        _ => false,
    }
}

pub(crate) fn array_agg_final(vals: Vec<Value>, is_array_input: bool) -> Result<Value, ExecError> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    // PG overload resolution is static: an array-typed argument selects
    // the array-input variant even when every runtime value is NULL (no
    // Value::Array to inspect). Otherwise, any array value in the input
    // selects it (the static argument type is uniform, so a leading NULL
    // must not hide it).
    let nested = is_array_input || vals.iter().any(|v| matches!(v, Value::Array(_)));
    array_ctor_from_vals(vals, nested, true)
}

pub(crate) fn array_ctor_from_vals(
    vals: Vec<Value>,
    nested: bool,
    // v0.92: true when called for array_agg (PG's "cannot accumulate
    // ..." errors); false for ARRAY[...] literals (PG's 22P02
    // "multidimensional arrays ..." errors).
    for_agg: bool,
) -> Result<Value, ExecError> {
    if nested {
        let mut rows: Vec<ArrayVal> = Vec::with_capacity(vals.len());
        for (idx, v) in vals.into_iter().enumerate() {
            match v {
                Value::Array(a) => {
                    // v0.92: PG17 accumArrayResultArr raises "cannot
                    // accumulate empty arrays" (2202E) only when the
                    // FIRST input is empty; a later empty input fails
                    // the dimensionality check below, like PG.
                    if for_agg && idx == 0 && a.ndim() == 0 {
                        return Err(exec_err("2202E", "cannot accumulate empty arrays"));
                    }
                    rows.push(*a)
                }
                Value::Null => {
                    return Err(if for_agg {
                        exec_err("22004", "cannot accumulate null arrays")
                    } else {
                        exec_err(
                            "22P02",
                            "multidimensional arrays must have array expressions with matching dimensions",
                        )
                    });
                }
                other => {
                    return Err(exec_err(
                        "22P02",
                        format!(
                            "multidimensional arrays must have array expressions with matching dimensions, not {}",
                            other.type_name()
                        ),
                    ));
                }
            }
        }
        let first = &rows[0];
        for r in &rows[1..] {
            // v0.92: array_agg requires the same dimensionality AND the
            // same dimension lengths and lower bounds (PG17
            // accumArrayResultArr); the ARRAY literal keeps its
            // historical element+dimension check.
            let dim_mismatch =
                r.elem != first.elem || r.dims != first.dims || (for_agg && r.lower != first.lower);
            if dim_mismatch {
                return Err(if for_agg {
                    exec_err(
                        "2202E",
                        "cannot accumulate arrays of different dimensionality",
                    )
                } else {
                    exec_err(
                        "22P02",
                        "multidimensional arrays must have array expressions with matching dimensions",
                    )
                });
            }
        }
        let mut flat: Vec<Value> = Vec::new();
        for r in &rows {
            flat.extend(r.elems.iter().cloned());
        }
        let mut dims = Vec::with_capacity(first.dims.len() + 1);
        dims.push(rows.len() as i32);
        dims.extend(first.dims.iter().cloned());
        let mut lower = Vec::with_capacity(first.lower.len() + 1);
        lower.push(1);
        lower.extend(first.lower.iter().cloned());
        return Ok(Value::Array(Box::new(ArrayVal {
            elem: first.elem,
            dims,
            lower,
            elems: flat,
        })));
    }
    // Common element type, skipping NULLs (PG's unknown literals don't
    // constrain select_common_type; all-unknown resolves to text).
    let mut acc: Option<ColType> = None;
    for v in &vals {
        if matches!(v, Value::Null) {
            continue;
        }
        let t = value_coltype(v);
        acc = Some(match acc {
            Some(a) => common_supertype("ARRAY", &a, &t)?,
            None => t,
        });
    }
    let elem_ty = acc.unwrap_or(ColType::Text);
    let elem = crate::storage::ArrayElem::of(&elem_ty);
    let mut out: Vec<Value> = Vec::with_capacity(vals.len());
    for v in vals {
        out.push(eval_cast(&v, elem_ty)?);
    }
    Ok(Value::Array(Box::new(ArrayVal {
        elem,
        dims: vec![out.len() as i32],
        lower: vec![1],
        elems: out,
    })))
}

/// v0.79: coerce an array subscript/slice-bound value to an integer, like
/// PG19's assignment coercion of `A_Indices` to int4. NULL is handled by
/// the caller (NULL bound => NULL result / default bound).
pub(crate) fn array_index_to_i64(v: &Value) -> Result<i64, ExecError> {
    match eval_cast(v, ColType::Int)? {
        Value::Int(n) => Ok(n),
        // SmallInt/BigInt fold into Int through the cast; anything else
        // the cast produced is an internal inconsistency.
        other => Err(exec_err(
            "42804",
            format!(
                "array subscript must have type integer, not {}",
                other.type_name()
            ),
        )),
    }
}

/// v0.79: `a[i, ...]` — PG19 `array_get_element` semantics for one
/// multidimensional subscript operation. A NULL array or NULL index
/// yields NULL; a non-array base is 42804 ("cannot subscript type
/// ..."); a non-integer index is 42804 ("array subscript must have
/// type integer", PG19 array_subscript_transform). PG19 returns
/// NULL unless the index count equals the array's dimensionality
/// (a partial subscript is NULL, not a subarray), and NULL for any
/// out-of-range index (PG never raises on fetch).
pub(crate) fn eval_subscript_vals(base: &Value, indices: &[Value]) -> Result<Value, ExecError> {
    if matches!(base, Value::Null) || indices.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let Value::Array(a) = base else {
        return Err(exec_err(
            "42804",
            format!("cannot subscript type {}", base.type_name()),
        ));
    };
    if a.ndim() == 0 || a.ndim() != indices.len() {
        return Ok(Value::Null);
    }
    let mut offset = 0usize;
    let mut stride = 1usize;
    for d in (0..a.ndim()).rev() {
        let i = array_index_to_i64(&indices[d])?;
        let pos = i - a.lower[d] as i64;
        if pos < 0 || pos >= a.dims[d] as i64 {
            return Ok(Value::Null);
        }
        offset += pos as usize * stride;
        stride *= a.dims[d] as usize;
    }
    Ok(a.elems[offset].clone())
}

/// v0.79: `a[l:u, ...]` — PG19 `array_get_slice` semantics. A NULL
/// array yields NULL; slicing a non-array is 42804. More slice dims
/// than array dims, or any empty dim range, yields PG's empty
/// (0-dimensional) array. Absent (or NULL-valued) bounds default to
/// the array's own bounds for that dimension; bounds clamp to the
/// array's range; dims beyond the slice list keep their full range.
/// The result's lower bounds are reset to 1 (PG19 array_get_slice:
/// "Lower bounds of the new array are set to 1").
pub(crate) fn eval_slice_vals(
    base: &Value,
    bounds: &[(Option<Value>, Option<Value>)],
) -> Result<Value, ExecError> {
    if matches!(base, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::Array(a) = base else {
        return Err(exec_err(
            "42804",
            format!("cannot subscript type {}", base.type_name()),
        ));
    };
    if a.ndim() == 0 {
        return Ok(base.clone());
    }
    // PG19 array_get_slice: more subscripts than dimensions is the
    // empty array, not an error.
    if bounds.len() > a.ndim() {
        return Ok(Value::Array(Box::new(ArrayVal {
            elem: a.elem,
            dims: Vec::new(),
            lower: Vec::new(),
            elems: Vec::new(),
        })));
    }
    let bound = |v: Option<&Value>, dflt: i64| -> Result<i64, ExecError> {
        match v {
            Some(x) if !matches!(x, Value::Null) => array_index_to_i64(x),
            _ => Ok(dflt),
        }
    };
    let ndim = a.ndim();
    // Per-dim (start offset into the source, element count).
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(ndim);
    for d in 0..ndim {
        let arr_lo = a.lower[d] as i64;
        let arr_hi = arr_lo + a.dims[d] as i64 - 1;
        let (lo, hi) = if d < bounds.len() {
            let (l, u) = &bounds[d];
            let lo = bound(l.as_ref(), arr_lo)?.max(arr_lo);
            let hi = bound(u.as_ref(), arr_hi)?.min(arr_hi);
            (lo, hi)
        } else {
            (arr_lo, arr_hi)
        };
        if hi < lo {
            return Ok(Value::Array(Box::new(ArrayVal {
                elem: a.elem,
                dims: Vec::new(),
                lower: Vec::new(),
                elems: Vec::new(),
            })));
        }
        ranges.push(((lo - arr_lo) as usize, (hi - lo + 1) as usize));
    }
    // Row-major strides of the source array.
    let mut strides = vec![0usize; ndim];
    let mut s = 1usize;
    for d in (0..ndim).rev() {
        strides[d] = s;
        s *= a.dims[d] as usize;
    }
    let total: usize = ranges.iter().map(|(_, c)| c).product();
    let mut elems = Vec::with_capacity(total);
    // Odometer over the selected ranges, innermost dim fastest.
    let mut pos = vec![0usize; ndim];
    for _ in 0..total {
        let mut off = 0usize;
        for d in 0..ndim {
            off += (ranges[d].0 + pos[d]) * strides[d];
        }
        elems.push(a.elems[off].clone());
        for d in (0..ndim).rev() {
            pos[d] += 1;
            if pos[d] < ranges[d].1 {
                break;
            }
            pos[d] = 0;
        }
    }
    Ok(Value::Array(Box::new(ArrayVal {
        elem: a.elem,
        dims: ranges.iter().map(|(_, c)| *c as i32).collect(),
        lower: vec![1i32; ndim],
        elems,
    })))
}

/// v0.79: `=` / `<>` on arrays — PG19 `array_eq` semantics. Dimensions
/// must match exactly (else false); elements compare with three-valued
/// logic: any false element comparison makes the result false, else any
/// NULL element makes it NULL. Element types are unified through the
/// common supertype first (so `int[] = bigint[]` works, like PG's
/// operator resolution). Ordering operators have no PG array support.
pub(crate) fn eval_array_cmp(op: CmpOp, a: &ArrayVal, b: &ArrayVal) -> Result<Value, ExecError> {
    if !matches!(op, CmpOp::Eq | CmpOp::Ne) {
        return Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                a.type_name(),
                op.sql(),
                b.type_name()
            ),
        ));
    }
    if a.ndim() != b.ndim() || a.dims != b.dims {
        return Ok(Value::Bool(matches!(op, CmpOp::Ne)));
    }
    let cty = common_supertype(
        "array comparison",
        &elem_scalar_type(a.elem),
        &elem_scalar_type(b.elem),
    )?;
    let mut null_seen = false;
    for (x, y) in a.elems.iter().zip(b.elems.iter()) {
        let x = eval_cast(x, cty)?;
        let y = eval_cast(y, cty)?;
        match eval_cmp_vals(CmpOp::Eq, &x, &y)? {
            Value::Bool(false) => return Ok(Value::Bool(matches!(op, CmpOp::Ne))),
            Value::Null => null_seen = true,
            Value::Bool(true) => {}
            other => {
                return Err(exec_err(
                    "XX000",
                    format!(
                        "internal error: array element comparison returned {}",
                        other.type_name()
                    ),
                ));
            }
        }
    }
    if null_seen {
        return Ok(Value::Null);
    }
    Ok(Value::Bool(matches!(op, CmpOp::Eq)))
}

/// v0.79: `||` with an array operand — PG19 `array_cat` (array||array),
/// `array_append` (array||element), `array_prepend` (element||array). A
/// NULL array yields NULL; an untyped NULL scalar is a NULL *element*
/// (PG resolves the unknown literal to the element type). Array||array
/// merges along the first dimension (multi-dim arrays need matching
/// inner dims, else 2202E like PG's "cannot concatenate incompatible
/// arrays"); append/prepend require a one-dimensional array (2202E).
pub(crate) fn eval_array_concat(a: &Value, b: &Value) -> Result<Value, ExecError> {
    match (a, b) {
        // NULL array (strict, like PG).
        (Value::Null, _) => Ok(Value::Null),
        (Value::Array(x), Value::Array(y)) => array_cat_vals(x, y),
        // Untyped NULL scalar: NULL element (PG's unknown-literal
        // resolution). A typed NULL array is indistinguishable at
        // runtime; the literal case is the common one.
        (Value::Array(x), Value::Null) => array_append_elem(x, &Value::Null),
        (Value::Array(x), scalar) => array_append_elem(x, scalar),
        (scalar, Value::Array(y)) => array_prepend_elem(y, scalar),
        _ => Err(exec_err(
            "XX000",
            "internal error: eval_array_concat without an array operand",
        )),
    }
}

/// Element type both arrays coerce to for `||` / comparison.
pub(crate) fn array_common_elem(
    x: &ArrayVal,
    y: &ArrayVal,
    op: &str,
) -> Result<crate::storage::ArrayElem, ExecError> {
    let cty = common_supertype(op, &elem_scalar_type(x.elem), &elem_scalar_type(y.elem))?;
    Ok(crate::storage::ArrayElem::of(&cty))
}

pub(crate) fn array_cat_vals(x: &ArrayVal, y: &ArrayVal) -> Result<Value, ExecError> {
    let elem = array_common_elem(x, y, "||")?;
    let cty = elem_scalar_type(elem);
    let cast_all = |a: &ArrayVal| -> Result<Vec<Value>, ExecError> {
        a.elems.iter().map(|v| eval_cast(v, cty)).collect()
    };
    // PG: concatenating with an empty (0-dim) array yields the other
    // side (retyped to the common element type).
    if x.ndim() == 0 {
        return Ok(Value::Array(Box::new(ArrayVal {
            elem,
            dims: y.dims.clone(),
            lower: y.lower.clone(),
            elems: cast_all(y)?,
        })));
    }
    if y.ndim() == 0 {
        return Ok(Value::Array(Box::new(ArrayVal {
            elem,
            dims: x.dims.clone(),
            lower: x.lower.clone(),
            elems: cast_all(x)?,
        })));
    }
    if x.ndim() != y.ndim() || x.dims[1..] != y.dims[1..] {
        return Err(exec_err("2202E", "cannot concatenate incompatible arrays"));
    }
    let mut elems = cast_all(x)?;
    elems.extend(cast_all(y)?);
    let mut dims = x.dims.clone();
    dims[0] += y.dims[0];
    Ok(Value::Array(Box::new(ArrayVal {
        elem,
        dims,
        lower: x.lower.clone(),
        elems,
    })))
}

pub(crate) fn array_append_elem(x: &ArrayVal, scalar: &Value) -> Result<Value, ExecError> {
    if x.ndim() > 1 {
        return Err(exec_err(
            "2202E",
            "array_append requires a one-dimensional array",
        ));
    }
    let cty = elem_scalar_type(x.elem);
    let v = eval_cast(scalar, cty)?;
    let mut elems: Vec<Value> = x
        .elems
        .iter()
        .map(|e| eval_cast(e, cty))
        .collect::<Result<_, _>>()?;
    elems.push(v);
    let (dims, lower) = if x.ndim() == 0 {
        (vec![1i32], vec![1i32])
    } else {
        (vec![x.dims[0] + 1], x.lower.clone())
    };
    Ok(Value::Array(Box::new(ArrayVal {
        elem: x.elem,
        dims,
        lower,
        elems,
    })))
}

pub(crate) fn array_prepend_elem(y: &ArrayVal, scalar: &Value) -> Result<Value, ExecError> {
    if y.ndim() > 1 {
        return Err(exec_err(
            "2202E",
            "array_prepend requires a one-dimensional array",
        ));
    }
    let cty = elem_scalar_type(y.elem);
    let v = eval_cast(scalar, cty)?;
    let mut elems = Vec::with_capacity(y.elems.len() + 1);
    elems.push(v);
    elems.extend(
        y.elems
            .iter()
            .map(|e| eval_cast(e, cty))
            .collect::<Result<Vec<_>, _>>()?,
    );
    let (dims, lower) = if y.ndim() == 0 {
        (vec![1i32], vec![1i32])
    } else {
        // PG19 array_prepend: the element is inserted below the
        // input's lower bound, then the result's lower bound is
        // readjusted to match the input's ("as expected for
        // prepend"), so `0 || '{1,2}'::int[]` is `{0,1,2}`.
        (vec![y.dims[0] + 1], vec![y.lower[0]])
    };
    Ok(Value::Array(Box::new(ArrayVal {
        elem: y.elem,
        dims,
        lower,
        elems,
    })))
}

/// v0.79: scalar array functions (PG19 arrayfuncs.c). NULL array (or
/// NULL dimension) yields NULL; an invalid dimension (out of 1..ndims)
/// yields NULL; an empty array yields NULL for length/lower/upper/dims
/// and 0 for ndims/cardinality.
pub(crate) fn eval_array_func(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    let arr = |idx: usize| -> Result<Option<&ArrayVal>, ExecError> {
        match &vals[idx] {
            Value::Null => Ok(None),
            Value::Array(a) => Ok(Some(a)),
            // PG coerces an unknown text literal to text[] for unnest;
            // other functions have no text signature (42883).
            other => Err(func_arg_err(name, other)),
        }
    };
    let dim = |idx: usize| -> Result<Option<i64>, ExecError> {
        match int_arg(name, &vals[idx])? {
            None => Ok(None),
            Some(n) => Ok(Some(n)),
        }
    };
    // Resolve the (array, dim) pair shared by array_length/lower/upper:
    // Ok(None) means "SQL NULL result".
    let len_lower_upper = |idx_arr: usize, idx_dim: usize| -> Result<Option<i64>, ExecError> {
        let a = match arr(idx_arr)? {
            None => return Ok(None),
            Some(a) => a,
        };
        let d = match dim(idx_dim)? {
            None => return Ok(None),
            Some(d) => d,
        };
        if a.ndim() == 0 || d < 1 || d > a.ndim() as i64 {
            return Ok(None);
        }
        let i = (d - 1) as usize;
        Ok(Some(match name {
            "array_length" => a.dims[i] as i64,
            "array_lower" => a.lower[i] as i64,
            _ => (a.lower[i] + a.dims[i] - 1) as i64, // array_upper
        }))
    };
    match name {
        "array_length" | "array_lower" | "array_upper" => Ok(match len_lower_upper(0, 1)? {
            None => Value::Null,
            Some(n) => Value::Int(n),
        }),
        "cardinality" => Ok(match arr(0)? {
            None => Value::Null,
            Some(a) => Value::Int(a.nitems() as i64),
        }),
        "array_ndims" => Ok(match arr(0)? {
            None => Value::Null,
            Some(a) => Value::Int(a.ndim() as i64),
        }),
        "array_dims" => Ok(match arr(0)? {
            None => Value::Null,
            Some(a) if a.ndim() == 0 => Value::Null,
            Some(a) => {
                let mut s = String::new();
                for (d, l) in a.dims.iter().zip(a.lower.iter()) {
                    s.push_str(&format!("[{}:{}]", l, l + d - 1));
                }
                Value::text(s.as_str())
            }
        }),
        _ => Err(exec_err(
            "42883",
            format!("function {name}() does not exist"),
        )),
    }
}

/// v0.79: `unnest(anyarray)` element rows — one output row per element
/// (NULL elements become NULL rows); a NULL array yields zero rows, like
/// PG. A text value is parsed as a text[] literal (PG's unknown-literal
/// coercion for `unnest('{1,2}')`); anything else is 42883.
pub(crate) fn unnest_rows(v: &Value) -> Result<Vec<Value>, ExecError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(a) => Ok(a.elems.clone()),
        Value::Text(s) | Value::BpChar(s) => {
            let a = parse_array_literal(s, crate::storage::ArrayElem::Text)?;
            Ok(a.elems)
        }
        other => Err(func_arg_err("unnest", other)),
    }
}

/// v0.79: PG19 `array_in` for one-dimensional (possibly nested-brace)
/// literals. Parses `{...}` text into an `ArrayVal`, running each
/// element through the element type's input function (via `eval_cast`
/// of its text form, so `int4in`-style 22P02 errors surface intact).
/// Multidimensional input must have matching dims (22P02); malformed
/// input is 22P02 `malformed array literal: "<string>"`.
pub(crate) fn parse_array_literal(
    s: &str,
    elem: crate::storage::ArrayElem,
) -> Result<ArrayVal, ExecError> {
    struct P<'a> {
        chars: &'a [u8],
        pos: usize,
        src: &'a str,
    }
    impl<'a> P<'a> {
        fn malformed(&self) -> ExecError {
            exec_err("22P02", format!("malformed array literal: {:?}", self.src))
        }
        fn ws(&mut self) {
            while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
        }
        // Parse one element (quoted or unquoted) at the current depth;
        // returns None for a nested `{...}` (handled by the caller).
        fn element(&mut self) -> Result<(String, bool), ExecError> {
            self.ws();
            if self.pos < self.chars.len() && self.chars[self.pos] == b'"' {
                self.pos += 1;
                let mut out = String::new();
                loop {
                    if self.pos >= self.chars.len() {
                        return Err(self.malformed());
                    }
                    let c = self.chars[self.pos];
                    if c == b'\\' {
                        self.pos += 1;
                        if self.pos >= self.chars.len() {
                            return Err(self.malformed());
                        }
                        out.push(self.chars[self.pos] as char);
                        self.pos += 1;
                    } else if c == b'"' {
                        self.pos += 1;
                        break;
                    } else {
                        out.push(c as char);
                        self.pos += 1;
                    }
                }
                Ok((out, true))
            } else {
                let start = self.pos;
                while self.pos < self.chars.len()
                    && !matches!(self.chars[self.pos], b',' | b'}' | b'{')
                {
                    self.pos += 1;
                }
                let raw = &self.src[start..self.pos];
                Ok((raw.trim().to_string(), false))
            }
        }
    }

    fn cast_elem(
        text: &str,
        is_null: bool,
        elem: crate::storage::ArrayElem,
    ) -> Result<Value, ExecError> {
        if is_null {
            return Ok(Value::Null);
        }
        let ty = elem_scalar_type(elem);
        eval_cast(&Value::text(text), ty)
    }

    // Recursive descent: parse one `{...}` level, returning the nested
    // values as a tree; the caller flattens and validates dims.
    #[derive(Debug)]
    enum Node {
        Elem(String, bool), // (text, is_null)
        Arr(Vec<Node>),
    }
    fn parse_level(p: &mut P) -> Result<Vec<Node>, ExecError> {
        // Caller consumed '{'.
        let mut items: Vec<Node> = Vec::new();
        loop {
            p.ws();
            if p.pos >= p.chars.len() {
                return Err(p.malformed());
            }
            if p.chars[p.pos] == b'}' {
                p.pos += 1;
                return Ok(items);
            }
            if p.chars[p.pos] == b'{' {
                p.pos += 1;
                let inner = parse_level(p)?;
                items.push(Node::Arr(inner));
            } else {
                let (t, quoted) = p.element()?;
                // Quoted "NULL" stays a string; unquoted NULL (any
                // case) is SQL NULL.
                let is_null = !quoted && t.eq_ignore_ascii_case("null");
                items.push(Node::Elem(t, is_null));
            }
            p.ws();
            if p.pos < p.chars.len() && p.chars[p.pos] == b',' {
                p.pos += 1;
                continue;
            }
            p.ws();
            if p.pos < p.chars.len() && p.chars[p.pos] == b'}' {
                continue; // loop head consumes it
            }
            return Err(p.malformed());
        }
    }

    let mut p = P {
        chars: s.as_bytes(),
        pos: 0,
        src: s,
    };
    // Optional `[l:u]...=` dimension prefix (array_out form), per
    // PG's ReadArrayDimensions: `[` int `:` int `]` groups, then `=`.
    let mut dim_decls: Vec<(i32, i32)> = Vec::new();
    // Copy the source out so the int parsing below doesn't hold `p`
    // borrowed while `p.malformed()` also needs it.
    let psrc: &str = p.src;
    p.ws();
    while p.pos < p.chars.len() && p.chars[p.pos] == b'[' {
        p.pos += 1; // consume '['
        p.ws();
        let lb_start = p.pos;
        if p.pos < p.chars.len() && p.chars[p.pos] == b'-' {
            p.pos += 1;
        }
        while p.pos < p.chars.len() && p.chars[p.pos].is_ascii_digit() {
            p.pos += 1;
        }
        let lb: i32 = psrc[lb_start..p.pos].parse().map_err(|_| p.malformed())?;
        p.ws();
        if p.pos >= p.chars.len() || p.chars[p.pos] != b':' {
            return Err(p.malformed());
        }
        p.pos += 1;
        p.ws();
        let ub_start = p.pos;
        if p.pos < p.chars.len() && p.chars[p.pos] == b'-' {
            p.pos += 1;
        }
        while p.pos < p.chars.len() && p.chars[p.pos].is_ascii_digit() {
            p.pos += 1;
        }
        let ub: i32 = psrc[ub_start..p.pos].parse().map_err(|_| p.malformed())?;
        p.ws();
        if p.pos >= p.chars.len() || p.chars[p.pos] != b']' {
            return Err(p.malformed());
        }
        p.pos += 1;
        if ub < lb {
            return Err(p.malformed());
        }
        dim_decls.push((lb, ub));
    }
    if !dim_decls.is_empty() {
        p.ws();
        if p.pos >= p.chars.len() || p.chars[p.pos] != b'=' {
            return Err(p.malformed());
        }
        p.pos += 1; // consume '='
    }
    p.ws();
    if p.pos >= p.chars.len() || p.chars[p.pos] != b'{' {
        return Err(p.malformed());
    }
    p.pos += 1;
    let top = parse_level(&mut p)?;
    p.ws();
    if p.pos != p.chars.len() {
        return Err(p.malformed());
    }

    // Validate the tree is a proper rectangle (PG's ReadArrayStr):
    // every level's children share one kind (all scalars or all
    // arrays), sibling arrays have equal lengths, and all leaves sit
    // at the same depth. Then flatten in row-major order.
    fn shape(
        nodes: &[Node],
        depth: usize,
        leaf_depth: &mut Option<usize>,
        dims: &mut Vec<i32>,
    ) -> Result<(), ExecError> {
        let mismatch = || {
            exec_err(
                "22P02",
                "multidimensional arrays must have array expressions with matching dimensions",
            )
        };
        if depth >= dims.len() {
            dims.push(nodes.len() as i32);
        } else if dims[depth] != nodes.len() as i32 {
            return Err(mismatch());
        }
        let mut child_is_arr: Option<bool> = None;
        for n in nodes {
            let is_arr = matches!(n, Node::Arr(_));
            match child_is_arr {
                Some(k) if k != is_arr => return Err(mismatch()),
                _ => child_is_arr = Some(is_arr),
            }
        }
        for n in nodes {
            match n {
                Node::Elem(..) => match leaf_depth {
                    Some(d) if *d != depth => return Err(mismatch()),
                    _ => *leaf_depth = Some(depth),
                },
                Node::Arr(inner) => shape(inner, depth + 1, leaf_depth, dims)?,
            }
        }
        Ok(())
    }
    fn emit(
        nodes: &[Node],
        elem: crate::storage::ArrayElem,
        out: &mut Vec<Value>,
    ) -> Result<(), ExecError> {
        for n in nodes {
            match n {
                Node::Elem(t, is_null) => out.push(cast_elem(t, *is_null, elem)?),
                Node::Arr(inner) => emit(inner, elem, out)?,
            }
        }
        Ok(())
    }

    if top.is_empty() {
        return Ok(ArrayVal {
            elem,
            dims: Vec::new(),
            lower: Vec::new(),
            elems: Vec::new(),
        });
    }
    let mut dims: Vec<i32> = Vec::new();
    let mut leaf_depth: Option<usize> = None;
    shape(&top, 0, &mut leaf_depth, &mut dims)?;
    let mut elems: Vec<Value> = Vec::with_capacity(top.len());
    emit(&top, elem, &mut elems)?;
    // Declared `[l:u]...=` lower bounds (parsed above) apply to the
    // result; the declared sizes must match the data (PG rejects
    // mismatched dimension declarations in array_in).
    let mut lower = vec![1i32; dims.len()];
    for (i, (lb, _ub)) in dim_decls.iter().enumerate() {
        if i < lower.len() {
            lower[i] = *lb;
        }
    }
    if !dim_decls.is_empty() {
        if dim_decls.len() != dims.len() {
            return Err(exec_err(
                "22P02",
                format!("malformed array literal: {:?}", s),
            ));
        }
        for (i, (_lb, ub)) in dim_decls.iter().enumerate() {
            let declared = ub - dim_decls[i].0 + 1;
            if declared != dims[i] {
                return Err(exec_err(
                    "22P02",
                    format!("malformed array literal: {:?}", s),
                ));
            }
        }
    }
    Ok(ArrayVal {
        elem,
        dims,
        lower,
        elems,
    })
}

/// v0.82: parse a composite text literal `'(f1,f2,...)'` (PG19
/// `record_in` format) against a named composite definition. Returns
/// the field values in order. Quoting mirrors PG: `"..."` quoted fields
/// with `\"`/`\\` escapes, backslash escapes outside quotes, an empty
/// *unquoted* field is NULL (a quoted `""` is an empty string, and the
/// word NULL unquoted is the literal text "NULL" — unlike array_in).
/// A field whose type is itself a composite must be a nested `(...)`
/// literal (quoted or bare) and is parsed recursively. 22P02 on any
/// malformed input, like PG's record_in.
pub(crate) fn parse_record_literal(
    s: &str,
    type_name: &str,
    fields: &[(String, ColType, Option<String>)],
    types: &std::collections::HashMap<
        String,
        crate::storage::ShellType,
        crate::fxhash::FxBuildHasher,
    >,
) -> Result<Vec<Value>, ExecError> {
    fn bad(s: &str, type_name: &str) -> ExecError {
        exec_err(
            "22P02",
            format!("invalid input syntax for type {}: {:?}", type_name, s),
        )
    }
    let chars: &[u8] = s.as_bytes();
    let mut pos = 0usize;
    let ws = |pos: &mut usize| {
        while *pos < chars.len() && chars[*pos].is_ascii_whitespace() {
            *pos += 1;
        }
    };
    // Parse one field: Ok((text, quoted)). Quoted-ness matters because
    // only an empty *unquoted* field is NULL.
    fn field(
        chars: &[u8],
        pos: &mut usize,
        s: &str,
        type_name: &str,
    ) -> Result<(String, bool), ExecError> {
        let bad = |s: &str| {
            exec_err(
                "22P02",
                format!("invalid input syntax for type {}: {:?}", type_name, s),
            )
        };
        if *pos < chars.len() && chars[*pos] == b'"' {
            *pos += 1;
            let mut out = String::new();
            loop {
                if *pos >= chars.len() {
                    return Err(bad(s));
                }
                let c = chars[*pos];
                if c == b'\\' {
                    *pos += 1;
                    if *pos >= chars.len() {
                        return Err(bad(s));
                    }
                    out.push(chars[*pos] as char);
                    *pos += 1;
                } else if c == b'"' {
                    *pos += 1;
                    break;
                } else {
                    out.push(c as char);
                    *pos += 1;
                }
            }
            Ok((out, true))
        } else {
            // Unquoted field: scan to `,` or `)` at depth 0, tracking
            // balanced parens (nested composites like `((5),hi)`) and
            // skipping quoted sections so their commas don't delimit.
            let start = *pos;
            let mut depth = 0;
            while *pos < chars.len() {
                let c = chars[*pos];
                if c == b'(' {
                    depth += 1;
                } else if c == b')' {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                } else if c == b',' && depth == 0 {
                    break;
                } else if c == b'"' {
                    *pos += 1;
                    while *pos < chars.len() && chars[*pos] != b'"' {
                        if chars[*pos] == b'\\' {
                            *pos += 1;
                            if *pos >= chars.len() {
                                return Err(bad(s));
                            }
                        }
                        *pos += 1;
                    }
                    if *pos >= chars.len() {
                        return Err(bad(s));
                    }
                } else if c == b'\\' {
                    *pos += 1;
                    if *pos >= chars.len() {
                        return Err(bad(s));
                    }
                }
                *pos += 1;
            }
            // Note: backslash escapes are resolved when the field text is
            // used below; here we just delimit.
            Ok((s[start..*pos].to_string(), false))
        }
    }
    // Resolve backslash escapes in an unquoted field body.
    fn unescape(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        let mut it = raw.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    ws(&mut pos);
    if pos >= chars.len() || chars[pos] != b'(' {
        return Err(bad(s, type_name));
    }
    pos += 1;
    let mut vals: Vec<Value> = Vec::with_capacity(fields.len());
    // `()` is the empty record; otherwise parse `field (, field)*`.
    // An empty unquoted field (e.g. the second field of `(1,)`) is NULL.
    ws(&mut pos);
    let empty_record = pos < chars.len() && chars[pos] == b')';
    if empty_record {
        pos += 1;
    }
    while !empty_record {
        if vals.len() >= fields.len() {
            return Err(exec_err(
                "22P02",
                format!(
                    "invalid input syntax for type {}: too many columns in {:?}",
                    type_name, s
                ),
            ));
        }
        let (raw, quoted) = field(chars, &mut pos, s, type_name)?;
        let (_, fty, nested) = &fields[vals.len()];
        let text = if quoted {
            raw
        } else {
            unescape(&raw).trim().to_string()
        };
        let val = if !quoted && text.is_empty() {
            Value::Null
        } else if *fty == ColType::Composite {
            let nested_name = nested.as_deref().unwrap_or(type_name);
            let nested_fields = types
                .get(nested_name)
                .and_then(|st| st.composite.clone())
                .ok_or_else(|| {
                    exec_err("42704", format!("type \"{}\" does not exist", nested_name))
                })?;
            let inner = parse_record_literal(&text, nested_name, &nested_fields, types)?;
            Value::Record(
                nested_fields
                    .iter()
                    .zip(inner)
                    .map(|((n, _, _), v)| (n.clone(), v))
                    .collect(),
            )
        } else {
            eval_cast(&Value::text(text.as_str()), *fty).map_err(|_| bad(s, type_name))?
        };
        vals.push(val);
        ws(&mut pos);
        if pos < chars.len() && chars[pos] == b',' {
            pos += 1;
            continue;
        }
        if pos < chars.len() && chars[pos] == b')' {
            pos += 1;
            break;
        }
        return Err(bad(s, type_name));
    }
    if vals.len() != fields.len() {
        return Err(exec_err(
            "22P02",
            format!(
                "invalid input syntax for type {}: too few columns in {:?}",
                type_name, s
            ),
        ));
    }
    ws(&mut pos);
    if pos != chars.len() {
        return Err(bad(s, type_name));
    }
    Ok(vals)
}

/// v0.79: scalar `ColType` for an `ArrayElem` (element input/output).
pub(crate) fn elem_scalar_type(elem: crate::storage::ArrayElem) -> ColType {
    use crate::storage::ArrayElem as E;
    match elem {
        E::Bool => ColType::Bool,
        E::Bytea => ColType::Bytea,
        E::Bit => ColType::Bit, // v1.39
        E::Tid => ColType::Tid, // v1.40
        E::SingleChar => ColType::SingleChar,
        E::Name => ColType::Name,
        E::SmallInt => ColType::SmallInt,
        E::Int => ColType::Int,
        E::Text => ColType::Text,
        E::Char => ColType::Char(None),
        E::Varchar => ColType::Varchar(None),
        E::BigInt => ColType::BigInt,
        E::Float4 => ColType::Float4,
        E::Float => ColType::Float,
        E::Date => ColType::Date,
        E::Timestamp => ColType::Timestamp,
        E::Timestamptz => ColType::Timestamptz,
        E::Numeric => ColType::Numeric(None),
        E::Uuid => ColType::Uuid,
        E::Regclass => ColType::Regclass,
        E::Json => ColType::Json,
        E::Record => ColType::Record,
        E::PgLsn => ColType::PgLsn,
        E::Xid => ColType::Xid,
    }
}

/// Evaluate one expression against the scope chain (params substituted).
/// v1.11: evaluate a subquery as a row value (PG19 row subquery), for
/// `ROW(...) = (SELECT ...)` comparisons. Returns Value::Record with
/// PG's f1, f2, ... field names, or Value::Null when the subquery
/// returns no rows. More than one row is 21000, like scalar subqueries.
pub(crate) fn eval_row_subquery(
    q: &mut Q,
    scopes: &[Scope],
    sub: &SelectStmt,
) -> Result<Value, ExecError> {
    let out = {
        let mut sub_q = Q {
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
            plan_fold_memo: q.plan_fold_memo.clone(),
            hashed_in: q.hashed_in.clone(),
            // v0.89: plain subqueries never see the UPDATE overlay.
            pending_updates: None,
            write: q.write.as_mut().map(QWrite::reborrow),
        };
        run_select(&mut sub_q, sub, scopes)?
    };
    if out.rows.len() > 1 {
        return Err(exec_err(
            "21000",
            "more than one row returned by a subquery used as an expression",
        ));
    }
    Ok(match out.rows.first() {
        Some(row) => Value::Record(
            row.iter()
                .enumerate()
                .map(|(i, v)| (format!("f{}", i + 1), v.clone()))
                .collect(),
        ),
        None => Value::Null,
    })
}

/// v1.13: evaluate the `tableoid` system column. The value is the OID of
/// the table (partition leaf) that holds the current row, taken from the
/// scope's row provenance. Returns an error if provenance is unavailable
/// (e.g. the row comes from a subquery rather than a base table scan).
pub(crate) fn eval_tableoid(
    q: &mut Q,
    scopes: &[Scope],
    qual: Option<&str>,
) -> Result<Value, ExecError> {
    // Find the target scope: the one matching the qualifier, or the
    // innermost scope for an unqualified reference. Scopes are ordered
    // outer-to-inner, and resolve_col searches innermost-first.
    let sc = match qual {
        Some(qual_name) => scopes
            .iter()
            .rev()
            .find(|sc| sc.schema.iter().any(|c| c.qual == qual_name))
            .ok_or_else(|| {
                exec_err(
                    "42703",
                    format!("missing FROM-clause entry for table \"{qual_name}\""),
                )
            })?,
        None => scopes
            .last()
            .ok_or_else(|| exec_err("42703", "column \"tableoid\" does not exist".to_string()))?,
    };
    // The provenance entry names the source table. For a partitioned
    // scan this is the leaf partition holding the row; for a plain
    // table it is the table itself. A qualifier picks its own range's
    // entry (v1.17: previously the first entry was always used, which
    // was wrong for `b.tableoid` when `b` was not the first range).
    let table_name = sc
        .prov
        .and_then(|p| {
            p.iter()
                .find(|e| qual.is_none_or(|qn| e.qual == qn))
                .map(|e| e.table.as_str())
        })
        .ok_or_else(|| exec_err("42703", "column \"tableoid\" does not exist".to_string()))?;
    let t = q
        .eng
        .db
        .find_table(table_name, q.snap, &q.all_xids, q.session)
        .ok_or_else(|| exec_err("42703", "column \"tableoid\" does not exist".to_string()))?;
    Ok(Value::Int(t.oid as i64))
}

/// v1.17: `xmin`/`xmax` system columns — the inserting (or
/// deleting/updating) transaction's id, read from the row's MVCC
/// version header (PG19's `HeapTupleHeaderData.t_xmin/t_xmax`). Like
/// `tableoid`, resolved from row provenance when no real column
/// matches; a user column named `xmin`/`xmax` wins (resolved above).
/// Values are carried as `Value::Int` — PG's `xidout` renders them as
/// unsigned decimal (what `value_text` produces for non-negative
/// integers) and PG's `xideq` is plain integer equality (what `=`
/// does). `xmax` is 0 for live rows, exactly like PG.
pub(crate) fn eval_xmin_xmax(
    q: &mut Q,
    scopes: &[Scope],
    qual: Option<&str>,
    name: &str,
) -> Result<Value, ExecError> {
    let missing = || exec_err("42703", format!("column \"{name}\" does not exist"));
    // Find the target scope: the one matching the qualifier, or the
    // innermost scope for an unqualified reference (same as tableoid).
    let sc = match qual {
        Some(qual_name) => scopes
            .iter()
            .rev()
            .find(|sc| sc.schema.iter().any(|c| c.qual == qual_name))
            .ok_or_else(|| {
                exec_err(
                    "42703",
                    format!("missing FROM-clause entry for table \"{qual_name}\""),
                )
            })?,
        None => scopes.last().ok_or_else(missing)?,
    };
    let prov = sc.prov.ok_or_else(missing)?;
    // Pick this range's provenance entry. A qualifier selects its own
    // range (v1.17 `RowProv.qual`); an unqualified reference with more
    // than one range is ambiguous in PG (42702).
    let entry = match qual {
        Some(qual_name) => prov
            .iter()
            .find(|e| e.qual == qual_name)
            .ok_or_else(missing)?,
        None => {
            let mut quals: Vec<&str> = Vec::new();
            for e in prov {
                if !quals.contains(&e.qual.as_str()) {
                    quals.push(e.qual.as_str());
                }
            }
            if quals.len() > 1 {
                return Err(exec_err(
                    "42702",
                    format!("column reference \"{name}\" is ambiguous"),
                ));
            }
            prov.first().ok_or_else(missing)?
        }
    };
    let rv = q
        .eng
        .db
        .find_row_version(entry.row_id)
        .ok_or_else(missing)?;
    let xid = if name == "xmax" { rv.xmax } else { rv.xmin };
    Ok(Value::Int(xid as i64))
}

/// v1.40: shared scope + provenance-entry selection for the system
/// columns added in v1.40 (`ctid`, `cmin`, `cmax`). Identical semantics
/// to `eval_xmin_xmax`'s selection: the qualifier picks its range's
/// entry; an unqualified reference with more than one range is
/// ambiguous in PG (42702). A user column wins (resolved before this
/// is called).
pub(crate) fn sys_prov_entry<'s>(
    scopes: &'s [Scope<'s>],
    qual: Option<&str>,
    name: &str,
) -> Result<&'s RowProv, ExecError> {
    let missing = || exec_err("42703", format!("column \"{name}\" does not exist"));
    let sc = match qual {
        Some(qual_name) => scopes
            .iter()
            .rev()
            .find(|sc| sc.schema.iter().any(|c| c.qual == qual_name))
            .ok_or_else(|| {
                exec_err(
                    "42703",
                    format!("missing FROM-clause entry for table \"{qual_name}\""),
                )
            })?,
        None => scopes.last().ok_or_else(missing)?,
    };
    let prov = sc.prov.ok_or_else(missing)?;
    match qual {
        Some(qual_name) => prov
            .iter()
            .find(|e| e.qual == qual_name)
            .ok_or_else(missing),
        None => {
            let mut quals: Vec<&str> = Vec::new();
            for e in prov {
                if !quals.contains(&e.qual.as_str()) {
                    quals.push(e.qual.as_str());
                }
            }
            if quals.len() > 1 {
                return Err(exec_err(
                    "42702",
                    format!("column reference \"{name}\" is ambiguous"),
                ));
            }
            prov.first().ok_or_else(missing)
        }
    }
}

/// v1.40: `ctid` system column — the row's tuple identifier, read from
/// row provenance like `xmin`/`xmax`. rustgres has no heap pages, so
/// the block is always 0 and the offset is the row version's position
/// in its table (PG19 `tidout` renders `(b,o)`). A user column named
/// `ctid` wins (resolved before this is called).
pub(crate) fn eval_ctid(
    q: &mut Q,
    scopes: &[Scope],
    qual: Option<&str>,
) -> Result<Value, ExecError> {
    let missing = || exec_err("42703", "column \"ctid\" does not exist".to_string());
    let entry = sys_prov_entry(scopes, qual, "ctid")
        .map_err(|e| if e.code == "42703" { missing() } else { e })?;
    // v1.40: the RETURNING path marks FROM/USING ranges (whose row ids
    // are not tracked) with row_id = u64::MAX; their system columns
    // stay 42703, but the qualifier still counts for ambiguity above.
    if entry.row_id == u64::MAX {
        return Err(missing());
    }
    let t = q
        .eng
        .db
        .find_table(&entry.table, q.snap, &q.all_xids, q.session)
        .ok_or_else(missing)?;
    let pos = t.row_pos(entry.row_id).ok_or_else(missing)?;
    Ok(Value::Tid(0, pos as u32))
}

/// v1.40: `cmin`/`cmax` system columns — PG19's per-row command ids
/// (`HeapTupleHeaderData.t_cmin/t_cmax`). rustgres does not track
/// per-row command ids, so both report 0 (documented honest gap).
/// Typed as `xid`: PG's `cid` displays as unsigned decimal exactly
/// like `xid` (the wire OID 28 vs PG's 29 is a documented fidelity
/// gap). A user column named `cmin`/`cmax` wins (resolved before this
/// is called).
pub(crate) fn eval_cmin_cmax(
    scopes: &[Scope],
    qual: Option<&str>,
    name: &str,
) -> Result<Value, ExecError> {
    let missing = || exec_err("42703", format!("column \"{name}\" does not exist"));
    let entry = sys_prov_entry(scopes, qual, name)
        .map_err(|e| if e.code == "42703" { missing() } else { e })?;
    if entry.row_id == u64::MAX {
        return Err(missing());
    }
    Ok(Value::Int(0))
}

pub(crate) fn eval_expr(q: &mut Q, scopes: &[Scope], e: &Expr) -> Result<Value, ExecError> {
    match e {
        // v0.95: a NamedArg that reaches evaluation unwraps to its value
        // (function-call paths resolve named args first).
        Expr::NamedArg { expr, .. } => eval_expr(q, scopes, expr),
        Expr::Column { table, name } => {
            match resolve_col(scopes, table.as_deref(), name) {
                Ok((si, ci)) => Ok(scopes[si].row[ci].clone()),
                Err(e) => {
                    // v1.13: `tableoid` system column — the OID of the
                    // table (partition leaf) holding this row. Resolved
                    // from row provenance when no real column matches.
                    // A user column named `tableoid` wins (resolved above).
                    if e.code == "42703" && name == "tableoid" {
                        if let Ok(v) = eval_tableoid(q, scopes, table.as_deref()) {
                            return Ok(v);
                        }
                    }
                    // v1.17: `xmin`/`xmax` system columns — the row's
                    // MVCC version header. Same fallback position as
                    // `tableoid`: a user column wins (resolved above).
                    // A 42702 (ambiguous) or other error from the
                    // system-column path propagates; only a 42703
                    // (no provenance) falls back to the original error.
                    if e.code == "42703" && (name == "xmin" || name == "xmax") {
                        match eval_xmin_xmax(q, scopes, table.as_deref(), name) {
                            Ok(v) => return Ok(v),
                            Err(e2) if e2.code == "42703" => {}
                            Err(e2) => return Err(e2),
                        }
                    }
                    // v1.40: `ctid` system column — the row's tuple id.
                    // Same fallback position as `xmin`/`xmax`: a user
                    // column wins (resolved above). A 42702 (ambiguous)
                    // or other error propagates; only a 42703 (no
                    // provenance) falls back to the original error.
                    if e.code == "42703" && name == "ctid" {
                        match eval_ctid(q, scopes, table.as_deref()) {
                            Ok(v) => return Ok(v),
                            Err(e2) if e2.code == "42703" => {}
                            Err(e2) => return Err(e2),
                        }
                    }
                    // v1.40: `cmin`/`cmax` system columns — PG19's
                    // per-row command ids (always 0 here; see
                    // eval_cmin_cmax). Same fallback position.
                    if e.code == "42703" && (name == "cmin" || name == "cmax") {
                        match eval_cmin_cmax(scopes, table.as_deref(), name) {
                            Ok(v) => return Ok(v),
                            Err(e2) if e2.code == "42703" => {}
                            Err(e2) => return Err(e2),
                        }
                    }
                    // v0.73: PG19 whole-row fallback — a bare identifier
                    // that names no column but names a range is a
                    // whole-row Var (`SELECT view_a FROM view_a`,
                    // `RETURNING tbl`). Column references win over range
                    // names, and a genuinely unknown name keeps its 42703.
                    if table.is_none()
                        && e.code == "42703"
                        && scopes
                            .iter()
                            .any(|sc| sc.schema.iter().any(|c| c.qual == *name))
                    {
                        return eval_wholerow(scopes, name);
                    }
                    Err(e)
                }
            }
        }
        // v0.73: PG19 whole-row Var — `tbl` / `tbl.*` in expression
        // position evaluates to a composite record value.
        Expr::WholeRow { qual } => eval_wholerow(scopes, qual),
        // Pre-resolved by resolve_predicate_columns for a fixed scope
        // shape: direct positional fetch, no name lookup. The positions
        // were resolved against these exact schemas, so the indexes hold.
        Expr::ResolvedCol { frame, idx } => Ok(scopes[*frame].row[*idx].clone()),
        Expr::Literal(lit) => Ok(lit.clone().into_value()),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Arith { op, left, right } => {
            let va = eval_expr(q, scopes, left)?;
            let vb = eval_expr(q, scopes, right)?;
            eval_arith(*op, &va, &vb)
        }
        Expr::Cast { expr, to, .. } => {
            if let Some(v) = cast_empty_array_ctor(expr, to) {
                return Ok(v);
            }
            let v = eval_expr(q, scopes, expr)?;
            // v0.37: regclass cast needs catalog lookup (OID -> name).
            if *to == ColType::Regclass {
                return eval_regclass_cast(q, &v);
            }
            eval_cast(&v, *to)
        }
        // v0.81: `ROW(a, b, ...)` — evaluates to a composite record with
        // PG's `f1`, `f2`, ... field names.
        // v1.43: `ROW(qual.*, ...)` — PG19 expands the star into the
        // row's field list (flat, sequential f-names), not a nested
        // whole-row value. `wholerow_flat_values` reuses
        // `qual_star_order`, so the `JOIN ... USING ... AS alias` quirk
        // (alias exposes only merged keys) matches target-list `qual.*`
        // and `eval_wholerow` exactly.
        Expr::Row(elems) => {
            let mut fields = Vec::new();
            for e in elems {
                if let Expr::WholeRow { qual } = e {
                    for v in wholerow_flat_values(scopes, qual)? {
                        fields.push((format!("f{}", fields.len() + 1), v));
                    }
                } else {
                    let v = eval_expr(q, scopes, e)?;
                    fields.push((format!("f{}", fields.len() + 1), v));
                }
            }
            Ok(Value::Record(fields))
        }
        // v0.81: `(expr).field` — composite field access.
        Expr::FieldAccess { expr, field } => {
            let v = eval_expr(q, scopes, expr)?;
            match v {
                Value::Record(fields) => fields
                    .into_iter()
                    .find(|(n, _)| n == field)
                    .map(|(_, v)| v)
                    .ok_or_else(|| {
                        exec_err("42703", format!("column \"{}\" not found in record", field))
                    }),
                Value::Null => Ok(Value::Null),
                _ => Err(exec_err(
                    "42809",
                    "cannot access field of non-composite value".to_string(),
                )),
            }
        }
        // v0.81: `expr::named_composite` — resolves the type name against
        // the catalog (42704 if undefined), then coerces the record.
        Expr::CastNamed { expr, name } => {
            let v = eval_expr(q, scopes, expr)?;
            eval_cast_named(q, &v, name)
        }
        Expr::Concat(a, b) => {
            let va = eval_expr(q, scopes, a)?;
            let vb = eval_expr(q, scopes, b)?;
            eval_concat(&va, &vb)
        }
        // v0.79: real array expressions.
        Expr::ArrayCtor { elems, nested } => eval_array_ctor(q, scopes, elems, *nested),
        Expr::Subscript { array, indices } => {
            let va = eval_expr(q, scopes, array)?;
            let idx_vals = indices
                .iter()
                .map(|i| eval_expr(q, scopes, i))
                .collect::<Result<Vec<_>, _>>()?;
            eval_subscript_vals(&va, &idx_vals)
        }
        Expr::Slice { array, bounds } => {
            let va = eval_expr(q, scopes, array)?;
            let bound_vals = bounds
                .iter()
                .map(|(l, u)| {
                    Ok((
                        l.as_deref().map(|l| eval_expr(q, scopes, l)).transpose()?,
                        u.as_deref().map(|u| eval_expr(q, scopes, u)).transpose()?,
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            eval_slice_vals(&va, &bound_vals)
        }
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => {
            let va = eval_expr(q, scopes, expr)?;
            let vb = eval_expr(q, scopes, pattern)?;
            let ve = match escape {
                Some(e) => Some(eval_expr(q, scopes, e)?),
                None => None,
            };
            eval_like(&va, &vb, ve.as_ref(), *not, *ilike)
        }
        // v0.68: regex match operators.
        Expr::Regex {
            expr,
            pattern,
            not,
            case_insensitive,
        } => {
            let va = eval_expr(q, scopes, expr)?;
            let vb = eval_expr(q, scopes, pattern)?;
            eval_regex_match(&va, &vb, *not, *case_insensitive)
        }
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            let v = eval_expr(q, scopes, expr)?;
            let lo = eval_expr(q, scopes, low)?;
            let hi = eval_expr(q, scopes, high)?;
            eval_between(&v, &lo, &hi, *neg)
        }
        Expr::IsBool { expr, neg, val } => {
            let v = eval_expr(q, scopes, expr)?;
            eval_is_bool(&v, *neg, *val)
        }
        Expr::Func { name, args } => {
            // v1.28: PG19 ProjectSet on the plain path — an SRF call
            // collected for fan-out evaluates to its current row's value.
            // `q.srf_vals` is only non-empty while a fan-out is projecting
            // (plain or grouped); subqueries run with a fresh Q (empty
            // `srf_vals`), so this only fires for the item's own
            // expression tree. Mirrors `eval_grouped`'s v1.27 arm.
            if let Some((_, v)) = q.srf_vals.iter().find(|(call, _)| call == e) {
                return Ok(v.clone());
            }
            // v1.37: hand the call-site node to the plan-fold memo.
            eval_func(q, scopes, name, args, e as *const Expr)
        }
        Expr::Extract { field, from } => {
            let v = eval_expr(q, scopes, from)?;
            eval_extract(field, &v)
        }
        Expr::Cmp { op, left, right } => {
            // v1.11: row-valued subquery — `ROW(...) = (SELECT ...)` or
            // `(SELECT ...) = ROW(...)`. PG19 permits a multi-column
            // subquery as an operand of a row comparison; the subquery
            // evaluates to a record (f1, f2, ...). Detected
            // syntactically: one side is a ROW(...) constructor and the
            // other is a scalar subquery.
            let row_sub: Option<(&SelectStmt, bool)> = match (left.as_ref(), right.as_ref()) {
                (Expr::Row(_), Expr::ScalarSub(sub)) => Some((sub, true)),
                (Expr::ScalarSub(sub), Expr::Row(_)) => Some((sub, false)),
                _ => None,
            };
            if let Some((sub, row_on_left)) = row_sub {
                // NB: the subquery side is NOT evaluated via eval_expr
                // (that would raise 42601 for multi-column); it goes
                // through eval_row_subquery instead.
                let (va, vb) = if row_on_left {
                    let va = eval_expr(q, scopes, left)?;
                    (va, eval_row_subquery(q, scopes, sub)?)
                } else {
                    let vb = eval_expr(q, scopes, right)?;
                    (eval_row_subquery(q, scopes, sub)?, vb)
                };
                if is_exact_int_value(&va) && is_exact_int_value(&vb) {
                    return eval_cmp_vals(*op, &va, &vb);
                }
                let (va, vb) = coerce_regclass_cmp(q, scopes, left, right, va, vb)?;
                let (va, vb) = coerce_name_cmp(scopes, left, right, va, vb);
                return eval_cmp_vals(*op, &va, &vb);
            }
            let va = eval_expr(q, scopes, left)?;
            let vb = eval_expr(q, scopes, right)?;
            // v0.63 perf: an int-valued operand can never be regclass- or
            // name-typed (see `coerce_regclass_cmp`/`coerce_name_cmp`), so
            // when both sides are already int-valued (the common case for
            // join keys and id filters), both coercion cascades below are
            // provably no-ops — skip calling them, and the extra by-value
            // `Value` moves that come with each call, entirely.
            if is_exact_int_value(&va) && is_exact_int_value(&vb) {
                return eval_cmp_vals(*op, &va, &vb);
            }
            // v0.38: regclass/oid binary coercion (below).
            let (va, vb) = coerce_regclass_cmp(q, scopes, left, right, va, vb)?;
            // v0.57: name-vs-unknown-literal truncation (PG19 namein).
            let (va, vb) = coerce_name_cmp(scopes, left, right, va, vb);
            eval_cmp_vals(*op, &va, &vb)
        }
        Expr::And(a, b) => {
            let va = eval_expr(q, scopes, a)?;
            let vb = eval_expr(q, scopes, b)?;
            eval_and_vals(&va, &vb)
        }
        Expr::Or(a, b) => {
            let va = eval_expr(q, scopes, a)?;
            let vb = eval_expr(q, scopes, b)?;
            eval_or_vals(&va, &vb)
        }
        Expr::Not(x) => {
            let v = eval_expr(q, scopes, x)?;
            eval_not_val(&v)
        }
        Expr::BitNot(x) => {
            let v = eval_expr(q, scopes, x)?;
            eval_bitnot_val(&v)
        }
        Expr::Neg(x) => {
            let v = eval_expr(q, scopes, x)?;
            eval_neg_val(&v)
        }
        // v0.55: CASE (PG19). Arms short-circuit; the taken result is
        // coerced to the CASE's resolved result type.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            let schemas: Vec<&[QCol]> = scopes.iter().map(|s| s.schema).collect();
            let ty = case_eval_type(q.eng, q.snap, q.own, q.session, &schemas, whens, else_)?;
            eval_case(operand, whens, else_, ty, |e| eval_expr(q, scopes, e))
        }
        Expr::IsNull { expr: x, neg } => {
            let v = eval_expr(q, scopes, x)?;
            // v0.73: a whole-row value is null iff every field is null
            // (PG19 ExecEvalNullTest / IS NULL semantics).
            Ok(Value::Bool(crate::storage::value_is_null(&v) != *neg))
        }
        // v0.48: `IS [NOT] DISTINCT FROM` — the NULL-safe comparison
        // (PG19): NULLs compare equal and never produce unknown; NaN
        // compares equal to NaN (unlike `=`); otherwise it is the
        // negation of `=`.
        Expr::IsDistinctFrom { left, right, neg } => {
            let l = eval_expr(q, scopes, left)?;
            let r = eval_expr(q, scopes, right)?;
            eval_is_distinct_from(l, r, *neg)
        }
        // Aggregates only evaluate in the grouped path; reaching the
        // row-level evaluator is a validation bug (validate_select keeps
        // them out of WHERE/ON/GROUP BY, and is_agg_query routes select
        // lists with aggregates to exec_agg).
        Expr::Agg { .. } => Err(exec_err("42803", "aggregates not allowed in this context")),
        // v1.30: ordered-set aggregates likewise only evaluate in the
        // grouped path.
        Expr::WithinGroup { .. } => {
            Err(exec_err("42803", "aggregates not allowed in this context"))
        }
        Expr::ScalarSub(sub) => {
            let out = {
                let mut sub_q = Q {
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
                    plan_fold_memo: q.plan_fold_memo.clone(),
                    hashed_in: q.hashed_in.clone(),
                    // v0.89: plain subqueries never see the UPDATE overlay.
                    pending_updates: None,
                    write: q.write.as_mut().map(QWrite::reborrow),
                };
                run_select(&mut sub_q, sub, scopes)?
            };
            if out.columns.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            if out.rows.len() > 1 {
                return Err(exec_err(
                    "21000",
                    "more than one row returned by a subquery used as an expression",
                ));
            }
            Ok(out
                .rows
                .first()
                .map(|r| r[0].clone())
                .unwrap_or(Value::Null))
        }
        // v0.77: `array(SELECT ...)` — evaluate the subquery and format
        // the first column of each row as a PG array literal.
        Expr::ArraySubquery(sub) => {
            let out = {
                let mut sub_q = Q {
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
                    plan_fold_memo: q.plan_fold_memo.clone(),
                    hashed_in: q.hashed_in.clone(),
                    // v0.89: plain subqueries never see the UPDATE overlay.
                    pending_updates: None,
                    write: q.write.as_mut().map(QWrite::reborrow),
                };
                run_select(&mut sub_q, sub, scopes)?
            };
            if out.columns.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            // v0.79: a real typed array, not text (PG19's array subquery
            // builds the array value directly). The element type is the
            // subquery's column type, flattened like PG (`array(SELECT
            // array(...))` keeps the innermost element's array OID);
            // array-valued rows stack into extra dims (nested
            // `ARRAY[[...],[...]]` semantics), scalar rows cast to the
            // element type.
            let col_ty = out.columns[0].1;
            let vals: Vec<Value> = out.rows.iter().map(|r| r[0].clone()).collect();
            if matches!(col_ty, ColType::Array(_)) {
                if vals.is_empty() {
                    // Empty subquery over an array column: PG yields an
                    // empty array of the (flattened) element type.
                    let elem = crate::storage::ArrayElem::of(&col_ty);
                    return Ok(Value::Array(Box::new(ArrayVal {
                        elem,
                        dims: Vec::new(),
                        lower: Vec::new(),
                        elems: Vec::new(),
                    })));
                }
                array_ctor_from_vals(vals, true, false)
            } else {
                let elem = crate::storage::ArrayElem::of(&col_ty);
                let ty = elem_scalar_type(elem);
                let mut elems = Vec::with_capacity(vals.len());
                for v in vals {
                    elems.push(eval_cast(&v, ty)?);
                }
                Ok(Value::Array(Box::new(ArrayVal {
                    elem,
                    dims: vec![elems.len() as i32],
                    lower: vec![1],
                    elems,
                })))
            }
        }
        Expr::InSub { expr, sub, neg } => eval_in(q, scopes, expr, sub, *neg),
        // v0.87: quantified comparisons and user-defined operators.
        Expr::Quantified {
            left,
            op,
            quant,
            sub,
        } => eval_quantified(q, scopes, left, op, *quant, sub),
        Expr::UserOp { op, left, right } => eval_user_op(q, scopes, op, left, right),
        Expr::Exists { sub, neg } => {
            // v0.80: hashed correlated-EXISTS fast path — a simple
            // equality-correlated subquery builds its inner key set once
            // per statement instead of re-running per outer row.
            if let Some(b) = eval_hashed_exists(q, scopes, sub, *neg)? {
                return Ok(Value::Bool(b));
            }
            let out = {
                let mut sub_q = Q {
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
                    plan_fold_memo: q.plan_fold_memo.clone(),
                    hashed_in: q.hashed_in.clone(),
                    // v0.89: plain subqueries never see the UPDATE overlay.
                    pending_updates: None,
                    write: q.write.as_mut().map(QWrite::reborrow),
                };
                run_select(&mut sub_q, sub, scopes)?
            };
            let exists = !out.rows.is_empty();
            Ok(Value::Bool(if *neg { !exists } else { exists }))
        }
        // v0.10: window functions are precomputed per query level
        // (Q.wctx) before projection; here we just look the value up.
        Expr::Window { wid, .. } => {
            let wctx = q
                .wctx
                .as_ref()
                .ok_or_else(|| exec_err("XX000", "internal error: window without context"))?;
            wctx.values
                .get(*wid)
                .and_then(|v| v.get(wctx.row))
                .cloned()
                .ok_or_else(|| exec_err("XX000", "internal error: window value missing"))
        }
    }
}

/// `[NOT] IN (subquery)` with SQL three-valued logic: TRUE if any equal
/// value, else NULL if NULLs were involved, else FALSE.
/// v0.80: hashed correlated-EXISTS / uncorrelated-IN subplans.
///
/// A correlated `EXISTS (SELECT ... FROM t k WHERE k.c = <outer>)`
/// re-runs the inner query once per outer row. When the subquery has
/// the simple shape `SELECT ... FROM <single base table> [AS k] WHERE
/// k.c = <outer expr>` (no CTEs, grouping, limit, or set ops; no
/// aggregates/windows in the select list), the inner side is
/// row-independent: the set of `k.c` values is built once per
/// statement (one MVCC scan under the statement snapshot) and each
/// outer row probes it. Likewise an uncorrelated `IN (SELECT ...)`
/// runs its subquery once per statement instead of once per row.
///
/// Soundness rules — anything else falls back to the row-by-row
/// executor, which is always correct:
/// - the inner range resolves exactly as the executor's own FROM
///   resolution would: a shadowing CTE, view, or information_schema
///   name falls back; partitioned parents fall back;
/// - the hashed column's type is one whose equality is bytewise on
///   the stored form (int2/int4/int8, text, bool, date, timestamp,
///   timestamptz, bytea, uuid); a probe of another family (e.g.
///   numeric against int, date against timestamp) falls back so
///   `cmp_ordering` keeps its exact cross-type semantics;
/// - NULL inner values never match; a NULL probe never matches;
/// - the probe expression may not reference the inner range (it is
///   evaluated in the outer scopes, where such a reference would not
///   resolve) and may not contain a subquery;
/// - the SELECT privilege check the table scan would perform runs
///   when the set is built (same 42501).
/// The caches are shared across nested query levels by Rc (like CTE
/// bindings); the statement snapshot is fixed, so one build serves
/// every row. A volatile function inside a cached IN subquery
/// evaluates once per statement — Postgres materializes uncorrelated
/// subplans the same way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SubplanFamily {
    Int,
    Text,
    Bool,
    Date,
    Timestamp,
    Timestamptz,
    Bytea,
    // v1.39
    Bit,
    Uuid,
}

/// A hashable cell value. Only built for values whose family matches
/// the hashed column's family, so set membership agrees exactly with
/// `cmp_ordering`'s `Eq`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum SubplanKey {
    Int(i64),
    Text(String),
    Bool(bool),
    Date(i32),
    Timestamp(i64),
    Timestamptz(i64),
    Bytea(Vec<u8>),
    // v1.39: bit length matters (trailing bits are padding).
    BitString { bitlen: u32, bytes: Vec<u8> },
    Uuid([u8; 16]),
}

/// Materialized inner key set for one hashable EXISTS pattern.
pub(crate) struct HashedExists {
    pub(crate) family: SubplanFamily,
    pub(crate) set: HashSet<SubplanKey>,
}

/// Cached output of one uncorrelated IN subquery: the first-column
/// values, plus the probe set when every value was hash-safe.
pub(crate) struct HashedIn {
    pub(crate) values: Vec<Value>,
    pub(crate) saw_null: bool,
    pub(crate) set: Option<(SubplanFamily, HashSet<SubplanKey>)>,
}

pub(crate) fn subplan_key(v: &Value) -> Option<SubplanKey> {
    match v {
        Value::SmallInt(x) => Some(SubplanKey::Int(*x as i64)),
        Value::Int(x) => Some(SubplanKey::Int(*x)),
        Value::BigInt(x) => Some(SubplanKey::Int(*x)),
        Value::Text(x) => Some(SubplanKey::Text(x.to_string())),
        Value::Bool(x) => Some(SubplanKey::Bool(*x)),
        Value::Date(x) => Some(SubplanKey::Date(*x)),
        Value::Timestamp(x) => Some(SubplanKey::Timestamp(*x)),
        Value::Timestamptz(x) => Some(SubplanKey::Timestamptz(*x)),
        Value::Bytea(x) => Some(SubplanKey::Bytea(x.clone())),
        // v1.39
        Value::BitString(x) => Some(SubplanKey::BitString {
            bitlen: x.bitlen,
            bytes: x.bytes.clone(),
        }),
        Value::Uuid(x) => Some(SubplanKey::Uuid(*x)),
        _ => None,
    }
}

pub(crate) fn subplan_key_family(k: &SubplanKey) -> SubplanFamily {
    match k {
        SubplanKey::Int(_) => SubplanFamily::Int,
        SubplanKey::Text(_) => SubplanFamily::Text,
        SubplanKey::Bool(_) => SubplanFamily::Bool,
        SubplanKey::Date(_) => SubplanFamily::Date,
        SubplanKey::Timestamp(_) => SubplanFamily::Timestamp,
        SubplanKey::Timestamptz(_) => SubplanFamily::Timestamptz,
        SubplanKey::Bytea(_) => SubplanFamily::Bytea,
        SubplanKey::BitString { .. } => SubplanFamily::Bit, // v1.39
        SubplanKey::Uuid(_) => SubplanFamily::Uuid,
    }
}

/// Column types whose stored values always land in one hash family.
/// Char/varchar are excluded: their values may be `BpChar`, whose
/// trailing-space-insensitive equality is not bytewise.
pub(crate) fn subplan_col_family(ty: &ColType) -> Option<SubplanFamily> {
    match ty {
        ColType::SmallInt | ColType::Int | ColType::BigInt => Some(SubplanFamily::Int),
        ColType::Text => Some(SubplanFamily::Text),
        ColType::Bool => Some(SubplanFamily::Bool),
        ColType::Date => Some(SubplanFamily::Date),
        ColType::Timestamp => Some(SubplanFamily::Timestamp),
        ColType::Timestamptz => Some(SubplanFamily::Timestamptz),
        ColType::Bytea => Some(SubplanFamily::Bytea),
        ColType::Bit => Some(SubplanFamily::Bit), // v1.39
        ColType::Uuid => Some(SubplanFamily::Uuid),
        _ => None,
    }
}

/// Visit `e` and all its child expressions, depth-first.
pub(crate) fn walk_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match e {
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => walk_expr(expr, f),
        Expr::Column { .. }
        | Expr::ResolvedCol { .. }
        | Expr::Literal(_)
        | Expr::Param(_)
        | Expr::WholeRow { .. } => {}
        Expr::Arith { left, right, .. }
        | Expr::Concat(left, right)
        | Expr::Cmp { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::IsDistinctFrom { left, right, .. } => {
            walk_expr(left, f);
            walk_expr(right, f);
        }
        Expr::Cast { expr, .. }
        // v0.81: named casts and field accesses recurse into their
        // operand, just like the builtin cast.
        | Expr::CastNamed { expr, .. }
        | Expr::FieldAccess { expr, .. }
        | Expr::Not(expr)
        | Expr::BitNot(expr)
        | Expr::Neg(expr)
        | Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Extract { from: expr, .. } => walk_expr(expr, f),
        // v0.81: row constructors recurse into each element.
        Expr::Row(elems) => {
            for el in elems {
                walk_expr(el, f);
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            walk_expr(expr, f);
            walk_expr(pattern, f);
            if let Some(esc) = escape {
                walk_expr(esc, f);
            }
        }
        Expr::Regex { expr, pattern, .. } => {
            walk_expr(expr, f);
            walk_expr(pattern, f);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            walk_expr(expr, f);
            walk_expr(low, f);
            walk_expr(high, f);
        }
        Expr::Func { args, .. } => {
            for a in args {
                walk_expr(a, f);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            if let Some(o) = operand {
                walk_expr(o, f);
            }
            for (k, r) in whens {
                walk_expr(k, f);
                walk_expr(r, f);
            }
            if let Some(el) = else_ {
                walk_expr(el, f);
            }
        }
        Expr::Agg { arg, arg2, .. } => {
            if let Some(a) = arg {
                walk_expr(a, f);
            }
            if let Some(a) = arg2 {
                walk_expr(a, f);
            }
        }
        // v1.30: ordered-set aggregate — walk direct args, the WITHIN
        // GROUP sort keys, and the FILTER.
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            for a in direct_args {
                walk_expr(a, f);
            }
            for o in within_order_by {
                walk_expr(&o.expr, f);
            }
            if let Some(x) = filter {
                walk_expr(x, f);
            }
        }
        Expr::ArrayCtor { elems, .. } => {
            for el in elems {
                walk_expr(el, f);
            }
        }
        Expr::Subscript { array, indices, .. } => {
            walk_expr(array, f);
            for i in indices {
                walk_expr(i, f);
            }
        }
        Expr::Slice { array, bounds, .. } => {
            walk_expr(array, f);
            for (lo, hi) in bounds {
                if let Some(b) = lo {
                    walk_expr(b, f);
                }
                if let Some(b) = hi {
                    walk_expr(b, f);
                }
            }
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                walk_expr(a, f);
            }
            for p in partition_by {
                walk_expr(p, f);
            }
            for t in order_by {
                walk_expr(&t.expr, f);
            }
        }
        Expr::ScalarSub(sub) | Expr::ArraySubquery(sub) => {
            walk_select(sub, f);
        }
        Expr::InSub { expr, sub, .. } => {
            walk_expr(expr, f);
            walk_select(sub, f);
        }
        // v0.87: quantified comparison and user operator.
        Expr::Quantified { left, sub, .. } => {
            walk_expr(left, f);
            walk_select(sub, f);
        }
        Expr::UserOp { left, right, .. } => {
            walk_expr(left, f);
            walk_expr(right, f);
        }
        Expr::Exists { sub, .. } => walk_select(sub, f),
    }
}

/// Visit every expression in the statement positions that can carry a
/// column reference: select items, WHERE, GROUP BY, HAVING, ORDER BY.
pub(crate) fn walk_select(s: &SelectStmt, f: &mut impl FnMut(&Expr)) {
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk_expr(expr, f);
        }
    }
    if let Some(w) = &s.where_ {
        walk_expr(w, f);
    }
    for g in s.group_by.iter().flatten() {
        walk_expr(g, f);
    }
    if let Some(h) = &s.having {
        walk_expr(h, f);
    }
    for t in &s.order_by {
        walk_expr(&t.expr, f);
    }
}

// ============================================================================
// v1.22: plan-time CASE constant folding (PG19 `eval_const_expressions`,
// clauses.c). Postgres folds constant subexpressions while the plan is
// built and raises errors (e.g. 22012 division by zero) then — but it never
// folds unreachable CASE arms: a WHEN that folds to FALSE drops its arm
// without touching the result; a WHEN that folds to TRUE folds its result
// and drops every later arm and the ELSE. This pre-pass runs at the top of
// `run_select` (every query level funnels through it, including subqueries
// and CTE bodies). It only EVALUATES; it never rewrites the tree.
// A subexpression is folded only if syntactically pure — literals,
// arithmetic/comparison/logical operators, casts, and nested CASEs — with
// no functions, columns, subqueries, aggregates, or window calls, so there
// are no side effects and no row dependence. Anything else is left for
// execution, matching PG's "we do not suppress folding of potentially
// reachable subexpressions".
// ============================================================================

/// v1.22: is this expression free of side effects and row references, so
/// evaluating it at plan time is safe? Conservative: anything not
/// explicitly listed (notably Func, Column, subqueries, Agg, Window)
/// returns false.
pub(crate) fn is_pure_const(e: &Expr) -> bool {
    let mut pure = true;
    walk_expr(e, &mut |sub| {
        if !pure {
            return;
        }
        match sub {
            Expr::Literal(_)
            | Expr::Arith { .. }
            | Expr::Cmp { .. }
            | Expr::And(..)
            | Expr::Or(..)
            | Expr::Not(_)
            | Expr::Neg(_)
            | Expr::BitNot(_)
            | Expr::IsNull { .. }
            | Expr::IsBool { .. }
            | Expr::Case { .. }
            | Expr::Cast { .. }
            | Expr::Concat(..)
            | Expr::Between { .. } => {}
            _ => {
                pure = false;
            }
        }
    });
    pure
}

/// v1.22: add a whole expression subtree to the dead set.
pub(crate) fn mark_subtree_dead(e: &Expr, dead: &mut HashSet<*const Expr>) {
    walk_expr(e, &mut |sub| {
        dead.insert(sub as *const Expr);
    });
}

/// v1.22: phase 1 of plan-time folding — mark unreachable CASE arms dead.
/// A WHEN that folds to FALSE kills its arm; a WHEN that folds to TRUE
/// kills every later arm and the ELSE. Evaluating a constant WHEN can
/// itself raise (e.g. `WHEN 1/0`), which aborts the statement at plan
/// time, like PG19.
pub(crate) fn mark_dead_arms(
    q: &mut Q,
    operand: &Option<Box<Expr>>,
    whens: &[(Box<Expr>, Box<Expr>)],
    else_: &Option<Box<Expr>>,
    dead: &mut HashSet<*const Expr>,
) -> Result<(), ExecError> {
    // Simple CASE with a pure-constant operand folds `operand = key`.
    let op_val: Option<Value> = match operand {
        Some(o) if is_pure_const(o) => Some(eval_expr(q, &[], o)?),
        _ => None,
    };
    let mut done = false;
    for (key, result) in whens {
        if done {
            mark_subtree_dead(result, dead);
            continue;
        }
        let cond: Option<Value> = if operand.is_some() {
            match (&op_val, is_pure_const(key)) {
                (Some(ov), true) => {
                    let kv = eval_expr(q, &[], key)?;
                    Some(eval_cmp_vals(CmpOp::Eq, ov, &kv)?)
                }
                _ => None,
            }
        } else if is_pure_const(key) {
            Some(eval_expr(q, &[], key)?)
        } else {
            None
        };
        match cond {
            Some(Value::Bool(false)) => mark_subtree_dead(result, dead),
            Some(Value::Bool(true)) => done = true,
            _ => {}
        }
    }
    if done {
        if let Some(e) = else_ {
            mark_subtree_dead(e, dead);
        }
    }
    Ok(())
}

/// v1.22: does this query level contain a CASE expression? Cheap
/// pre-check so `const_fold_check` only runs when there's work to do.
pub(crate) fn contains_case(s: &SelectStmt) -> bool {
    let mut found = false;
    walk_select(s, &mut |e| {
        if !found {
            if let Expr::Case { .. } = e {
                found = true;
            }
        }
    });
    found
}

/// v1.22: plan-time constant folding for one query level. Phase 1 marks
/// unreachable CASE arms dead; phase 2 evaluates every reachable
/// pure-constant subexpression, propagating errors (e.g. 22012) as
/// plan-time statement errors.
pub(crate) fn const_fold_check(q: &mut Q, stmt: &SelectStmt) -> Result<(), ExecError> {
    let mut dead: HashSet<*const Expr> = HashSet::new();
    // Phase 1: mark unreachable CASE arms.
    let mut err: Result<(), ExecError> = Ok(());
    walk_select(stmt, &mut |e| {
        if err.is_err() {
            return;
        }
        // A CASE under an already-dead arm stays dead; walk_expr still
        // descends, but every node down there is in `dead` too.
        if dead.contains(&(e as *const Expr)) {
            return;
        }
        if let Expr::Case {
            operand,
            whens,
            else_,
        } = e
        {
            if let Err(x) = mark_dead_arms(q, operand, whens, else_, &mut dead) {
                err = Err(x);
            }
        }
    });
    if err.is_err() {
        return err;
    }
    // Phase 2: fold reachable pure constants.
    walk_select(stmt, &mut |e| {
        if err.is_err() {
            return;
        }
        if dead.contains(&(e as *const Expr)) {
            return;
        }
        if is_pure_const(e) {
            if let Err(x) = eval_expr(q, &[], e) {
                err = Err(x);
            }
        }
    });
    err
}

/// True iff every column reference in `e` is qualified with a qualifier
/// other than `inner_qual`. Fails closed (false) on anything that
/// could hide an inner-range reference — notably nested subqueries,
/// which may correlate to the inner range, and unqualified columns,
/// which would resolve to the inner range in the row-by-row executor.
pub(crate) fn expr_refs_only_outer(e: &Expr, inner_qual: &str) -> bool {
    let mut ok = true;
    walk_expr(e, &mut |x| {
        if !ok {
            return;
        }
        match x {
            Expr::Column { table, .. } => {
                if !matches!(table, Some(q) if q != inner_qual) {
                    ok = false;
                }
            }
            Expr::WholeRow { qual } => {
                if qual != inner_qual {
                    ok = false;
                }
            }
            Expr::ScalarSub(_)
            | Expr::ArraySubquery(_)
            | Expr::InSub { .. }
            | Expr::Exists { .. }
            | Expr::ResolvedCol { .. } => {
                ok = false;
            }
            _ => {}
        }
    });
    ok
}

/// The matched shape of a hashable EXISTS subquery.
pub(crate) struct HashableExists<'a> {
    pub(crate) table: &'a str,
    pub(crate) inner_col: &'a str,
    pub(crate) outer: &'a Expr,
}

/// Match `EXISTS (SELECT ... FROM <table> [AS q] WHERE q.c = <outer>)`.
/// Returns None for any other shape; the caller falls back to the
/// row-by-row executor.
pub(crate) fn match_hashable_exists(sub: &SelectStmt) -> Option<HashableExists<'_>> {
    if sub.set_op.is_some()
        || !sub.group_by.is_empty()
        || sub.having.is_some()
        || sub.limit.is_some()
        || sub.offset.is_some()
        || !sub.with.is_empty()
    {
        return None;
    }
    let [from] = sub.from.as_slice() else {
        return None;
    };
    let FromItem::Table {
        name,
        alias,
        col_aliases,
        ..
    } = from
    else {
        return None;
    };
    if !col_aliases.is_empty() {
        return None;
    }
    // No aggregates or window functions: they change what "existence"
    // means (e.g. an aggregate without GROUP BY always yields one row).
    for item in &sub.items {
        if let SelectItem::Expr { expr, .. } = item {
            if contains_agg(expr) || contains_window(expr) {
                return None;
            }
        }
    }
    let w = sub.where_.as_ref()?;
    let Expr::Cmp {
        op: CmpOp::Eq,
        left,
        right,
    } = w
    else {
        return None;
    };
    let inner_qual = alias.as_deref().unwrap_or(name.as_str());
    // Exactly one side is a column of the inner range; the other side is
    // the per-row probe.
    let (inner_col, outer) = match (left.as_ref(), right.as_ref()) {
        (
            Expr::Column {
                table: Some(t),
                name,
            },
            outer,
        ) if t == inner_qual => {
            expr_refs_only_outer(outer, inner_qual).then_some((name.as_str(), outer))?
        }
        (
            outer,
            Expr::Column {
                table: Some(t),
                name,
            },
        ) if t == inner_qual => {
            expr_refs_only_outer(outer, inner_qual).then_some((name.as_str(), outer))?
        }
        _ => return None,
    };
    Some(HashableExists {
        table: name.as_str(),
        inner_col,
        outer,
    })
}

/// The SELECT privilege check the inner table scan would perform,
/// mirrored for the hashed build (same 42501).
pub(crate) fn check_subplan_table_privs(q: &Q, table: &str) -> Result<(), ExecError> {
    let db = &q.eng.db;
    if let Some(t) = db.find_table(table, q.snap, &q.all_xids, q.session) {
        let have = crate::storage::table_privs(db, q.role, t, q.snap, q.own);
        let select_ok = have & crate::storage::PRIV_SELECT == crate::storage::PRIV_SELECT
            || crate::storage::has_col_priv(
                db,
                q.role,
                t,
                crate::storage::PRIV_SELECT,
                q.snap,
                q.own,
            );
        if !select_ok {
            return Err(exec_err(
                "42501",
                format!("permission denied for table \"{}\" (needs SELECT)", table),
            ));
        }
    }
    Ok(())
}

/// True iff `name` resolves to something other than a base table under
/// the executor's own FROM resolution order (CTE, then
/// information_schema, then view, then table).
pub(crate) fn subplan_inner_is_base_table(q: &Q, name: &str) -> bool {
    if q.ctes.iter().any(|b| b.name == name) {
        return false;
    }
    if name == "information_schema.tables"
        || name == "information_schema.columns"
        || name == "information_schema.sequences"
    {
        return false;
    }
    if q.eng.db.find_view(name, q.snap, q.own).is_some() {
        return false;
    }
    true
}

/// Probe a materialized EXISTS set with one outer-row probe value.
pub(crate) fn probe_hashed_exists(
    q: &mut Q,
    scopes: &[Scope],
    h: &HashedExists,
    outer: &Expr,
    neg: bool,
) -> Result<Option<bool>, ExecError> {
    let ov = eval_expr(q, scopes, outer)?;
    if ov == Value::Null {
        // A NULL probe never matches; NOT EXISTS over no match is true.
        return Ok(Some(neg));
    }
    let Some(k) = subplan_key(&ov) else {
        // Unhashable probe type: the slow path for this row, where
        // `cmp_ordering` applies its exact semantics.
        return Ok(None);
    };
    if subplan_key_family(&k) != h.family {
        // Cross-family probe (e.g. numeric against int, date against
        // timestamp): `cmp_ordering` may still call it equal — slow path.
        return Ok(None);
    }
    let exists = h.set.contains(&k);
    Ok(Some(if neg { !exists } else { exists }))
}

/// Try the hashed fast path for `EXISTS (sub)`. Returns `Ok(None)`
/// when the shape is not hashable (or this row's probe is not): the
/// caller runs the regular row-by-row subquery.
pub(crate) fn eval_hashed_exists(
    q: &mut Q,
    scopes: &[Scope],
    sub: &SelectStmt,
    neg: bool,
) -> Result<Option<bool>, ExecError> {
    let pat = match match_hashable_exists(sub) {
        Some(p) => p,
        None => return Ok(None),
    };
    if !subplan_inner_is_base_table(q, pat.table) {
        return Ok(None);
    }
    let key = (pat.table.to_string(), pat.inner_col.to_string());
    // The RefCell borrow is scoped: probing and building both recurse
    // into eval_expr, which must be free to use the cache.
    let hit = { q.hashed_exists.borrow().get(&key).cloned() };
    if let Some(h) = hit {
        return probe_hashed_exists(q, scopes, &h, pat.outer, neg);
    }
    check_subplan_table_privs(q, pat.table)?;
    // Build the set: one MVCC scan of the inner column under the
    // statement snapshot.
    let h = {
        let db = &q.eng.db;
        let t = match db.find_table(pat.table, q.snap, &q.all_xids, q.session) {
            Some(t) => t,
            // Gone between the base-table check and now: the slow path
            // reports it exactly as before (42P01).
            None => return Ok(None),
        };
        // Partitioned parents fan out to leaves; keep the slow path.
        let is_partitioned = t
            .partition
            .as_ref()
            .map(|p| !p.children.is_empty())
            .unwrap_or(false);
        if is_partitioned {
            return Ok(None);
        }
        let idx = match t.columns.iter().position(|(n, _)| n == pat.inner_col) {
            Some(i) => i,
            // 42703 surfaces via the slow path.
            None => return Ok(None),
        };
        let family = match subplan_col_family(&t.columns[idx].1) {
            Some(f) => f,
            None => return Ok(None),
        };
        let mut set = HashSet::new();
        for r in t
            .rows
            .iter()
            .filter(|r| row_visible(r, q.snap, &q.all_xids))
        {
            let v = &r.values[idx];
            if *v == Value::Null {
                continue;
            }
            let Some(k) = subplan_key(v) else {
                // A value outside the column's family should not happen;
                // fail closed to the slow path.
                return Ok(None);
            };
            if subplan_key_family(&k) != family {
                return Ok(None);
            }
            set.insert(k);
        }
        Rc::new(HashedExists { family, set })
    };
    q.hashed_exists.borrow_mut().insert(key, h.clone());
    probe_hashed_exists(q, scopes, &h, pat.outer, neg)
}

/// v1.19: collect (table_name, qualifier) for every base table in a FROM
/// clause, recursing through JOINs. Returns None if the FROM contains
/// anything other than plain tables and joins (derived tables, VALUES,
/// etc.) — those fail closed to the slow path.
pub(crate) fn collect_hashable_tables(from: &[FromItem]) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    fn walk(item: &FromItem, out: &mut Vec<(String, String)>) -> bool {
        match item {
            FromItem::Table {
                name,
                alias,
                col_aliases,
                ..
            } => {
                if !col_aliases.is_empty() {
                    return false;
                }
                out.push((name.clone(), alias.clone().unwrap_or_else(|| name.clone())));
                true
            }
            FromItem::Join { left, right, .. } => walk(left, out) && walk(right, out),
            _ => false,
        }
    }
    for item in from {
        if !walk(item, &mut out) {
            return None;
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Match `IN (SELECT <single expr> FROM <tables..> ...)` — the
/// uncorrelated shape whose output can be materialized once. Correlation
/// is checked separately. v1.19: accepts JOINs, not just a single table.
pub(crate) fn match_hashable_in(sub: &SelectStmt) -> Option<Vec<(String, String)>> {
    if sub.set_op.is_some() || sub.limit.is_some() || sub.offset.is_some() || !sub.with.is_empty() {
        return None;
    }
    let tables = collect_hashable_tables(&sub.from)?;
    let [item] = sub.items.as_slice() else {
        return None;
    };
    if !matches!(item, SelectItem::Expr { .. }) {
        return None;
    }
    Some(tables)
}

/// True iff every column reference in the IN subquery resolves inside
/// the subquery's own single range: qualified refs must name the inner
/// qualifier, and unqualified refs must name one of the inner table's
/// own columns (the innermost scope wins, so they resolve inside).
/// Nested subqueries fail closed — they could correlate past the inner
/// range to an outer scope.
pub(crate) fn subquery_refs_only_inner(
    q: &Q,
    sub: &SelectStmt,
    tables: &[(String, String)],
) -> bool {
    // v1.19: union of all inner tables' columns, for JOIN support.
    let mut cols: Vec<String> = Vec::new();
    for (table, _) in tables {
        match q.eng.db.find_table(table, q.snap, &q.all_xids, q.session) {
            Some(t) => cols.extend(t.columns.iter().map(|(n, _)| n.clone())),
            None => return false,
        }
    }
    let mut ok = true;
    walk_select(sub, &mut |x| {
        if !ok {
            return;
        }
        match x {
            Expr::Column { table: t, name } => match t {
                Some(qq) => {
                    if !tables.iter().any(|(_, qual)| qual == qq) {
                        ok = false;
                    }
                }
                None => {
                    if !cols.iter().any(|c| c == name) {
                        ok = false;
                    }
                }
            },
            Expr::WholeRow { qual } => {
                if !tables.iter().any(|(_, q2)| q2 == qual) {
                    ok = false;
                }
            }
            Expr::ScalarSub(_)
            | Expr::ArraySubquery(_)
            | Expr::InSub { .. }
            | Expr::Exists { .. }
            | Expr::ResolvedCol { .. } => {
                ok = false;
            }
            _ => {}
        }
    });
    ok
}

/// Materialize one uncorrelated IN subquery's first-column values.
pub(crate) fn build_hashed_in(rows: &[Row]) -> HashedIn {
    let mut values = Vec::with_capacity(rows.len());
    let mut saw_null = false;
    let mut set: Option<HashSet<SubplanKey>> = Some(HashSet::new());
    let mut family: Option<SubplanFamily> = None;
    for row in rows {
        let v = row[0].clone();
        if v == Value::Null {
            saw_null = true;
        } else if let Some(s) = set.as_mut() {
            match subplan_key(&v) {
                Some(k) => {
                    let f = subplan_key_family(&k);
                    if family.map_or(true, |ff| ff == f) {
                        family = Some(f);
                        s.insert(k);
                    } else {
                        set = None;
                    }
                }
                None => {
                    set = None;
                }
            }
        }
        values.push(v);
    }
    let set = match (set, family) {
        (Some(s), Some(f)) => Some((f, s)),
        _ => None,
    };
    HashedIn {
        values,
        saw_null,
        set,
    }
}

/// The row-by-row three-valued scan, over cached rows instead of a
/// re-executed subquery. Used when the probe is not hash-safe.
pub(crate) fn scan_in_values(values: &[Value], v: &Value) -> Result<Option<bool>, ExecError> {
    let mut saw_null = false;
    let mut found = false;
    for rv in values {
        match cmp_ordering(v, rv, CmpOp::Eq)? {
            Some(Ordering::Equal) => {
                found = true;
                break;
            }
            Some(_) => {}
            None => saw_null = true,
        }
    }
    Ok(if found {
        Some(true)
    } else if saw_null {
        None
    } else {
        Some(false)
    })
}

/// Probe a cached IN-subquery result with one LHS value, preserving
/// the exact three-valued logic of the row-by-row executor.
pub(crate) fn probe_hashed_in(cached: &HashedIn, v: &Value, neg: bool) -> Result<Value, ExecError> {
    let result: Option<bool> = if *v == Value::Null {
        // NULL probe: unknown only when the set holds a NULL that could
        // match; over an empty or NULL-free set it is determinately
        // false (IN) / true (NOT IN).
        if cached.saw_null { None } else { Some(false) }
    } else if let Some((family, set)) = &cached.set {
        match subplan_key(v) {
            Some(k) if subplan_key_family(&k) == *family => {
                if set.contains(&k) {
                    Some(true)
                } else if cached.saw_null {
                    None
                } else {
                    Some(false)
                }
            }
            // Cross-family probe: `cmp_ordering` may still call it
            // equal (int vs numeric) — scan with exact semantics.
            _ => scan_in_values(&cached.values, v)?,
        }
    } else {
        scan_in_values(&cached.values, v)?
    };
    let result = if neg { not3(result) } else { result };
    Ok(match result {
        Some(b) => Value::Bool(b),
        None => Value::Null,
    })
}

/// Try the cached fast path for `IN (subquery)`: run an uncorrelated
/// subquery once per statement instead of once per row. Returns
/// `Ok(None)` when the shape is not cacheable; the caller runs the
/// regular per-row subquery.
pub(crate) fn eval_hashed_in(
    q: &mut Q,
    scopes: &[Scope],
    v: &Value,
    sub: &SelectStmt,
    neg: bool,
) -> Result<Option<Value>, ExecError> {
    // Bolt (2026-09-29): check the cache — keyed by `sub`'s address, see
    // the `hashed_in` field doc — before re-deriving hashability. Once a
    // `sub` has been validated and built for this statement, its shape
    // and correlation safety cannot change (fixed snapshot, immutable
    // AST), so a hit skips `match_hashable_in`/`subplan_inner_is_base_table`/
    // `subquery_refs_only_inner` entirely instead of re-running the whole
    // validation chain on every probed row.
    let key = sub as *const SelectStmt;
    let hit = {
        q.hashed_in
            .borrow()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, c)| c.clone())
    };
    let cached = match hit {
        Some(c) => c,
        None => {
            let tables = match match_hashable_in(sub) {
                Some(p) => p,
                None => return Ok(None),
            };
            // v1.19: all inner tables must be base tables (JOINs allowed).
            for (table, _) in &tables {
                if !subplan_inner_is_base_table(q, table) {
                    return Ok(None);
                }
            }
            if !subquery_refs_only_inner(q, sub, &tables) {
                return Ok(None);
            }
            // The RefCell borrow is scoped: building runs the subquery,
            // which recurses into eval_expr and must be free to use the
            // cache.
            let out = {
                let mut sub_q = Q {
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
                    plan_fold_memo: q.plan_fold_memo.clone(),
                    hashed_in: q.hashed_in.clone(),
                    // v0.89: plain subqueries never see the UPDATE overlay.
                    pending_updates: None,
                    write: q.write.as_mut().map(QWrite::reborrow),
                };
                run_select(&mut sub_q, sub, scopes)?
            };
            if out.columns.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            let cached = Rc::new(build_hashed_in(&out.rows));
            q.hashed_in.borrow_mut().push((key, cached.clone()));
            cached
        }
    };
    Ok(Some(probe_hashed_in(&cached, v, neg)?))
}

/// v0.87: three-valued row-wise comparison of two row values with a
/// builtin `CmpOp`, following PG19's row-wise comparison rules:
/// - `=`: true iff every pair is equal; false iff any pair is definitely
///   unequal; else NULL.
/// - `<>`: true iff any pair is definitely unequal; false iff every pair
///   is equal; else NULL.
/// - `<`, `<=`, `>`, `>=`: lexicographic — pairs compared left to right,
///   stopping at the first unequal or NULL pair (a NULL pair yields NULL
///   unless an earlier pair already decided).
/// Returns `Ok(None)` for NULL (unknown). `*=` on rows is 0A000.
pub(crate) fn eval_row_cmp(op: CmpOp, l: &[Value], r: &[Value]) -> Result<Option<bool>, ExecError> {
    match op {
        CmpOp::Eq => {
            let mut saw_null = false;
            for (a, b) in l.iter().zip(r.iter()) {
                match eval_cmp_vals(CmpOp::Eq, a, b)? {
                    Value::Bool(true) => {}
                    Value::Bool(false) => return Ok(Some(false)),
                    Value::Null => saw_null = true,
                    _ => {
                        return Err(exec_err(
                            "XX000",
                            "internal error: non-boolean row comparison",
                        ));
                    }
                }
            }
            Ok(if saw_null { None } else { Some(true) })
        }
        CmpOp::Ne => {
            let mut saw_null = false;
            for (a, b) in l.iter().zip(r.iter()) {
                match eval_cmp_vals(CmpOp::Eq, a, b)? {
                    Value::Bool(true) => {}
                    Value::Bool(false) => return Ok(Some(true)),
                    Value::Null => saw_null = true,
                    _ => {
                        return Err(exec_err(
                            "XX000",
                            "internal error: non-boolean row comparison",
                        ));
                    }
                }
            }
            Ok(if saw_null { None } else { Some(false) })
        }
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            for (a, b) in l.iter().zip(r.iter()) {
                if matches!(a, Value::Null) || matches!(b, Value::Null) {
                    return Ok(None);
                }
                match cmp_ordering(a, b, CmpOp::Eq)? {
                    Some(std::cmp::Ordering::Equal) => continue,
                    Some(ord) => {
                        let result = match op {
                            CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                            CmpOp::Ge => ord != std::cmp::Ordering::Less,
                            CmpOp::Lt => ord == std::cmp::Ordering::Less,
                            CmpOp::Le => ord != std::cmp::Ordering::Greater,
                            _ => unreachable!(),
                        };
                        return Ok(Some(result));
                    }
                    None => return Ok(None),
                }
            }
            // All pairs equal.
            Ok(Some(matches!(op, CmpOp::Le | CmpOp::Ge)))
        }
        CmpOp::ImageEq => Err(exec_err(
            "0A000",
            "row-wise *= comparison is not supported".to_string(),
        )),
    }
}

/// v0.87: resolve a user-defined function call to the best overload.
/// Matches by arity, preferring overloads where each argument's runtime
/// type name canonically equals the declared parameter type. Falls back
/// to the first arity match (the call site coerces). Returns None if no
/// overload has the right arity.
pub(crate) fn resolve_function_overload(
    eng: &Engine,
    name: &str,
    arg_vals: &[Value],
) -> Option<crate::storage::FuncDef> {
    let overloads = eng.db.functions.get(name)?;
    let mut arity_match: Option<&crate::storage::FuncDef> = None;
    for fdef in overloads {
        if fdef.arg_types.len() != arg_vals.len() {
            continue;
        }
        if arity_match.is_none() {
            arity_match = Some(fdef);
        }
        // Prefer exact type matches.
        let exact = fdef
            .arg_types
            .iter()
            .zip(arg_vals.iter())
            .all(|(decl, val)| match value_op_type_name(val) {
                Some(actual) => canon_func_type_name(decl) == canon_func_type_name(actual),
                None => false,
            });
        if exact {
            return Some(fdef.clone());
        }
    }
    arity_match.cloned()
}

/// v0.87: resolve a function by (name, declared argument type names) for
/// DROP FUNCTION and CREATE OR REPLACE. Matches by arity and canonical
/// type name, like PG19.
pub(crate) fn find_function_by_signature(
    eng: &Engine,
    name: &str,
    arg_types: &[String],
) -> Option<crate::storage::FuncDef> {
    let overloads = eng.db.functions.get(name)?;
    for fdef in overloads {
        if fdef.arg_types.len() == arg_types.len()
            && fdef
                .arg_types
                .iter()
                .zip(arg_types.iter())
                .all(|(a, b)| canon_func_type_name(a) == canon_func_type_name(b))
        {
            return Some(fdef.clone());
        }
    }
    None
}

/// v0.86: user-defined `=` operator lookup for IN-subquery comparison.
/// Returns the (operator, function) pair when a user-defined `=`
/// matches the (outer, inner) type pair by canonical type name.
pub(crate) fn find_user_equality_op(
    eng: &Engine,
    outer_ty: &str,
    inner_ty: &str,
) -> Option<(crate::storage::OperDef, crate::storage::FuncDef)> {
    let defs = eng.db.operators.get("=")?;
    for op in defs {
        let l = op.leftarg.as_deref().unwrap_or("");
        let r = op.rightarg.as_deref().unwrap_or("");
        if canon_func_type_name(l) == canon_func_type_name(outer_ty)
            && canon_func_type_name(r) == canon_func_type_name(inner_ty)
        {
            // v0.87: the procedure may be overloaded; match its signature
            // to the operator's argument types.
            let f =
                find_function_by_signature(eng, &op.procedure, &[l.to_string(), r.to_string()])?;
            return Some((op.clone(), f));
        }
    }
    None
}

/// v0.87: resolve a user-defined binary operator by (name, left type,
/// right type) names, like PG19's oper() lookup. Returns the operator
/// definition and its procedure's function definition.
pub(crate) fn resolve_user_operator(
    eng: &Engine,
    name: &str,
    left_ty: &str,
    right_ty: &str,
) -> Option<(crate::storage::OperDef, crate::storage::FuncDef)> {
    let defs = eng.db.operators.get(name)?;
    for op in defs {
        let l = op.leftarg.as_deref().unwrap_or("");
        let r = op.rightarg.as_deref().unwrap_or("");
        if canon_func_type_name(l) == canon_func_type_name(left_ty)
            && canon_func_type_name(r) == canon_func_type_name(right_ty)
        {
            // v0.87: the procedure may be overloaded; match its signature
            // to the operator's argument types.
            let f =
                find_function_by_signature(eng, &op.procedure, &[l.to_string(), r.to_string()])?;
            return Some((op.clone(), f));
        }
    }
    None
}

/// v0.86: canonical type name of a runtime value, for operator lookup.
pub(crate) fn value_op_type_name(v: &Value) -> Option<&'static str> {
    Some(match v {
        Value::SmallInt(_) => "smallint",
        Value::Int(_) => "integer",
        Value::BigInt(_) => "bigint",
        Value::Float4(_) => "real",
        Value::Float(_) => "double precision",
        Value::Numeric(_) => "numeric",
        Value::Text(_) | Value::BpChar(_) => "text",
        Value::Bool(_) => "boolean",
        Value::Date(_) => "date",
        Value::Timestamp(_) => "timestamp",
        Value::Timestamptz(_) => "timestamptz",
        Value::Bytea(_) => "bytea",
        Value::Uuid(_) => "uuid",
        _ => return None,
    })
}

/// v0.86: canonical type name of a column type, for operator lookup.
pub(crate) fn coltype_op_name(ct: &ColType) -> String {
    match ct {
        ColType::SmallInt => "smallint".to_string(),
        ColType::Int => "integer".to_string(),
        ColType::BigInt => "bigint".to_string(),
        ColType::Float4 => "real".to_string(),
        ColType::Float => "double precision".to_string(),
        ColType::Numeric(_) => "numeric".to_string(),
        ColType::Text | ColType::Varchar(_) | ColType::Char(_) => "text".to_string(),
        ColType::Bool => "boolean".to_string(),
        ColType::Date => "date".to_string(),
        ColType::Timestamp => "timestamp".to_string(),
        ColType::Timestamptz => "timestamptz".to_string(),
        ColType::Bytea => "bytea".to_string(),
        ColType::Uuid => "uuid".to_string(),
        _ => format!("{:?}", ct).to_lowercase(),
    }
}

/// v0.86: probe one IN pair through a user-defined `=` operator.
/// Returns None for NULL (unknown), honoring STRICT.
pub(crate) fn probe_user_eq(
    q: &mut Q,
    scopes: &[Scope],
    op: &crate::storage::OperDef,
    fdef: &crate::storage::FuncDef,
    outer: &Value,
    inner: &Value,
) -> Result<Option<bool>, ExecError> {
    if outer == &Value::Null || inner == &Value::Null {
        // PG: a STRICT operator/function yields NULL on NULL input;
        // a non-strict one still runs — but SQL function bodies with
        // NULL args produce NULL here in every corpus case, and the
        // generic path below handles it. Take the strict shortcut.
        if fdef.strict {
            return Ok(None);
        }
    }
    let r = call_user_function(q, scopes, fdef, &[outer.clone(), inner.clone()])?;
    Ok(match r {
        Value::Bool(b) => Some(b),
        Value::Null => None,
        other => {
            return Err(exec_err(
                "42804",
                format!(
                    "operator {} must return boolean, got {}",
                    op.name,
                    other.type_name()
                ),
            ));
        }
    })
}

pub(crate) fn eval_in(
    q: &mut Q,
    scopes: &[Scope],
    e: &Expr,
    sub: &SelectStmt,
    neg: bool,
) -> Result<Value, ExecError> {
    let v = eval_expr(q, scopes, e)?;
    eval_in_value(q, scopes, v, sub, neg)
}

/// v1.22: `IN (subquery)` against a precomputed left value (used by
/// `eval_grouped` when the left side holds aggregates at that level).
pub(crate) fn eval_in_value(
    q: &mut Q,
    scopes: &[Scope],
    v: Value,
    sub: &SelectStmt,
    neg: bool,
) -> Result<Value, ExecError> {
    // v0.86: a user-defined `=` operator disables the hashed IN path
    // (builtin hashing can't apply custom equality); the nested loop
    // below probes through the operator instead.
    let user_eq_defined = q.eng.db.operators.contains_key("=");
    if !user_eq_defined {
        if let Some(r) = eval_hashed_in(q, scopes, &v, sub, neg)? {
            return Ok(r);
        }
    }
    let out = {
        let mut sub_q = Q {
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
            plan_fold_memo: q.plan_fold_memo.clone(),
            hashed_in: q.hashed_in.clone(),
            // v0.89: plain subqueries never see the UPDATE overlay.
            pending_updates: None,
            write: q.write.as_mut().map(QWrite::reborrow),
        };
        run_select(&mut sub_q, sub, scopes)?
    };
    // v0.87: row-wise IN — a row constructor on the left requires the
    // subquery to return the same number of columns.
    let left_vals: Vec<Value> = match &v {
        Value::Record(fields) => fields.iter().map(|(_, x)| x.clone()).collect(),
        x => vec![x.clone()],
    };
    let is_row = left_vals.len() > 1 || matches!(&v, Value::Record(_));
    if is_row {
        if out.columns.len() != left_vals.len() {
            return Err(exec_err(
                "42601",
                format!(
                    "subquery must return {} columns for row-wise IN",
                    left_vals.len()
                ),
            ));
        }
    } else if out.columns.len() != 1 {
        return Err(exec_err("42601", "subquery must return only one column"));
    }
    // v0.86: user-defined `=` operator for this (outer, inner) type
    // pair (e.g. the conformance corpus's `?=` / int8=text equality).
    let user_op = match (
        value_op_type_name(&v),
        out.columns.first().map(|(_, t)| coltype_op_name(t)),
    ) {
        (Some(o), Some(i)) => find_user_equality_op(q.eng, o, &i).map(|(op, f)| (op, f)),
        _ => None,
    };
    let mut saw_null = false;
    let mut found = false;
    for row in &out.rows {
        // v0.87: row-wise IN compares row values with three-valued logic.
        if is_row {
            match eval_row_cmp(CmpOp::Eq, &left_vals, row)? {
                Some(true) => {
                    found = true;
                    break;
                }
                Some(false) => {}
                None => saw_null = true,
            }
            continue;
        }
        if let Some((op, fdef)) = &user_op {
            match probe_user_eq(q, scopes, op, fdef, &v, &row[0])? {
                Some(true) => {
                    found = true;
                    break;
                }
                Some(false) => {}
                None => saw_null = true,
            }
            continue;
        }
        match cmp_ordering(&v, &row[0], CmpOp::Eq)? {
            Some(Ordering::Equal) => {
                found = true;
                break;
            }
            Some(_) => {}
            None => saw_null = true,
        }
    }
    let result = if found {
        Some(true)
    } else if v == Value::Null || saw_null {
        None
    } else {
        Some(false)
    };
    let result = if neg { not3(result) } else { result };
    Ok(match result {
        Some(b) => Value::Bool(b),
        None => Value::Null,
    })
}

/// v0.87: evaluate a user-defined binary operator `left op right`
/// (e.g. `?=` from the conformance corpus). The operator is resolved by
/// (name, left type, right type) like PG19's oper() lookup; NULL handling
/// follows STRICT (via probe_user_eq).
pub(crate) fn eval_user_op(
    q: &mut Q,
    scopes: &[Scope],
    name: &str,
    left: &Expr,
    right: &Expr,
) -> Result<Value, ExecError> {
    let lv = eval_expr(q, scopes, left)?;
    let rv = eval_expr(q, scopes, right)?;
    let lty = value_op_type_name(&lv).unwrap_or("");
    let rty = value_op_type_name(&rv).unwrap_or("");
    let (_odef, fdef) = resolve_user_operator(q.eng, name, lty, rty)
        .ok_or_else(|| exec_err("42883", format!("operator does not exist: {}", name)))?;
    if lv == Value::Null || rv == Value::Null {
        if fdef.strict {
            return Ok(Value::Null);
        }
    }
    Ok(call_user_function(q, scopes, &fdef, &[lv, rv])?)
}

/// v0.87: evaluate a quantified comparison `left op ANY/ALL (subquery)`
/// with PG19 three-valued logic. `left` may be a scalar or a row
/// constructor; the subquery must return one column (scalar) or the same
/// number of columns (row-wise). `op` is a builtin `CmpOp` or a
/// user-defined operator name.
pub(crate) fn eval_quantified(
    q: &mut Q,
    scopes: &[Scope],
    left: &Expr,
    op: &QuantOp,
    quant: QuantKind,
    sub: &SelectStmt,
) -> Result<Value, ExecError> {
    let lv = eval_expr(q, scopes, left)?;
    eval_quantified_value(q, scopes, lv, op, quant, sub)
}

/// v1.22: quantified comparison against a precomputed left value (used
/// by `eval_grouped` when the left side holds aggregates at that level).
pub(crate) fn eval_quantified_value(
    q: &mut Q,
    scopes: &[Scope],
    lv: Value,
    op: &QuantOp,
    quant: QuantKind,
    sub: &SelectStmt,
) -> Result<Value, ExecError> {
    let left_vals: Vec<Value> = match &lv {
        Value::Record(fields) => fields.iter().map(|(_, v)| v.clone()).collect(),
        v => vec![v.clone()],
    };
    let is_row = left_vals.len() > 1 || matches!(&lv, Value::Record(_));
    let out = {
        let mut sub_q = Q {
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
            plan_fold_memo: q.plan_fold_memo.clone(),
            hashed_in: q.hashed_in.clone(),
            // v0.89: plain subqueries never see the UPDATE overlay.
            pending_updates: None,
            write: q.write.as_mut().map(QWrite::reborrow),
        };
        run_select(&mut sub_q, sub, scopes)?
    };
    let arity = out.columns.len();
    if is_row {
        if arity != left_vals.len() {
            return Err(exec_err(
                "42601",
                format!(
                    "subquery must return {} columns for row-wise comparison",
                    left_vals.len()
                ),
            ));
        }
    } else if arity != 1 {
        return Err(exec_err(
            "42601",
            "subquery must return only one column".to_string(),
        ));
    }
    // Resolve a user-defined operator once, if needed.
    let user_op: Option<(crate::storage::OperDef, crate::storage::FuncDef)> = match op {
        QuantOp::User(name) => {
            let (lty, rty) = if is_row {
                // Row-wise user operators are not supported (0A000).
                return Err(exec_err(
                    "0A000",
                    "row-wise user-defined quantified operators are not supported".to_string(),
                ));
            } else {
                let inner_ty = out
                    .columns
                    .first()
                    .map(|(_, t)| coltype_op_name(t))
                    .unwrap_or_default();
                (value_op_type_name(&left_vals[0]).unwrap_or(""), inner_ty)
            };
            Some(
                resolve_user_operator(q.eng, name, lty, &rty).ok_or_else(|| {
                    exec_err("42883", format!("operator does not exist: {}", name))
                })?,
            )
        }
        QuantOp::Cmp(_) => None,
    };
    // PG19: ANY is true if any comparison is true, else NULL if any is
    // NULL, else false. ALL is false if any comparison is false, else
    // NULL if any is NULL, else true.
    let mut saw_null = false;
    for row in &out.rows {
        let cmp: Option<bool> = match (&user_op, op) {
            (Some((odef, fdef)), _) => {
                probe_user_eq(q, scopes, odef, fdef, &left_vals[0], &row[0])?
            }
            (None, QuantOp::Cmp(cop)) => {
                if is_row {
                    eval_row_cmp(*cop, &left_vals, row)?
                } else {
                    match eval_cmp_vals(*cop, &left_vals[0], &row[0])? {
                        Value::Bool(b) => Some(b),
                        Value::Null => None,
                        _ => {
                            return Err(exec_err(
                                "XX000",
                                "internal error: non-boolean quantified comparison",
                            ));
                        }
                    }
                }
            }
            (None, QuantOp::User(_)) => unreachable!(),
        };
        match (quant, cmp) {
            (QuantKind::Any, Some(true)) => return Ok(Value::Bool(true)),
            (QuantKind::All, Some(false)) => return Ok(Value::Bool(false)),
            (_, None) => saw_null = true,
            _ => {}
        }
    }
    if saw_null {
        return Ok(Value::Null);
    }
    Ok(Value::Bool(matches!(quant, QuantKind::All)))
}

/// v0.91: `expr op ANY|ALL|SOME (array_expr)` — the hidden
/// `__any_all_array` builtin the parser desugars the array form of
/// quantified comparison into. `vals` is
/// `[left, op_text, quant_text, arr]` where `op_text` is a `CmpOp::sql`
/// string or `"user:<name>"` and `quant_text` is `"any"`/`"all"`.
/// PG19's ScalarArrayOp semantics: iterate the (flattened) array
/// elements with three-valued logic — ANY is true if any comparison
/// is true (else NULL if any is NULL, else false); ALL is false if
/// any is false (else NULL if any is NULL, else true). A NULL array
/// yields NULL; an empty array yields false (ANY) / true (ALL).
pub(crate) fn eval_any_all_array(
    q: &mut Q,
    scopes: &[Scope],
    vals: &[Value],
) -> Result<Value, ExecError> {
    if vals.len() != 4 {
        return Err(exec_err(
            "42883",
            "function __any_all_array() does not exist".to_string(),
        ));
    }
    let left = &vals[0];
    let op_txt = match &vals[1] {
        Value::Text(s) => s.as_ref(),
        _ => {
            return Err(exec_err(
                "XX000",
                "internal error: __any_all_array op must be text".to_string(),
            ));
        }
    };
    let is_all = match &vals[2] {
        Value::Text(s) if s.as_ref() == "all" => true,
        Value::Text(s) if s.as_ref() == "any" => false,
        _ => {
            return Err(exec_err(
                "XX000",
                "internal error: __any_all_array quant must be any/all".to_string(),
            ));
        }
    };
    let arr = match &vals[3] {
        Value::Null => return Ok(Value::Null),
        Value::Array(a) => a,
        other => {
            return Err(exec_err(
                "42821",
                format!(
                    "op ANY/ALL (array) requires array on right side, not {}",
                    other.type_name()
                ),
            ));
        }
    };
    // Resolve the comparison operator.
    enum ArrOp {
        Cmp(CmpOp),
        User(crate::storage::OperDef, crate::storage::FuncDef),
    }
    let cmp_op = if let Some(name) = op_txt.strip_prefix("user:") {
        let first_elem = arr.elems.first();
        let rty = first_elem.and_then(value_op_type_name).unwrap_or_default();
        let (odef, fdef) =
            resolve_user_operator(q.eng, name, value_op_type_name(left).unwrap_or(""), rty)
                .ok_or_else(|| exec_err("42883", format!("operator does not exist: {}", name)))?;
        ArrOp::User(odef, fdef)
    } else {
        let cop = match op_txt {
            "=" => CmpOp::Eq,
            "<>" => CmpOp::Ne,
            "<" => CmpOp::Lt,
            "<=" => CmpOp::Le,
            ">" => CmpOp::Gt,
            ">=" => CmpOp::Ge,
            "*=" => CmpOp::ImageEq,
            _ => {
                return Err(exec_err(
                    "XX000",
                    "internal error: __any_all_array unknown operator".to_string(),
                ));
            }
        };
        ArrOp::Cmp(cop)
    };
    let is_row = matches!(left, Value::Record(_));
    if is_row && matches!(cmp_op, ArrOp::User(..)) {
        return Err(exec_err(
            "0A000",
            "row-wise user-defined quantified operators are not supported".to_string(),
        ));
    }
    let left_fields: Vec<Value> = match left {
        Value::Record(fields) => fields.iter().map(|(_, v)| v.clone()).collect(),
        v => vec![v.clone()],
    };
    let mut saw_null = false;
    for elem in &arr.elems {
        let cmp: Option<bool> = match &cmp_op {
            ArrOp::User(odef, fdef) => probe_user_eq(q, scopes, odef, fdef, left, elem)?,
            ArrOp::Cmp(cop) => {
                if is_row {
                    let right_fields: Vec<Value> = match elem {
                        Value::Record(fields) => fields.iter().map(|(_, v)| v.clone()).collect(),
                        Value::Null => {
                            saw_null = true;
                            continue;
                        }
                        other => {
                            return Err(exec_err(
                                "42883",
                                format!(
                                    "operator does not exist: record {} {}",
                                    cop.sql(),
                                    other.type_name()
                                ),
                            ));
                        }
                    };
                    eval_row_cmp(*cop, &left_fields, &right_fields)?
                } else {
                    match eval_cmp_vals(*cop, &left_fields[0], elem)? {
                        Value::Bool(b) => Some(b),
                        Value::Null => None,
                        _ => {
                            return Err(exec_err(
                                "XX000",
                                "internal error: non-boolean quantified comparison",
                            ));
                        }
                    }
                }
            }
        };
        match (is_all, cmp) {
            (false, Some(true)) => return Ok(Value::Bool(true)),
            (true, Some(false)) => return Ok(Value::Bool(false)),
            (_, None) => saw_null = true,
            _ => {}
        }
    }
    if saw_null {
        return Ok(Value::Null);
    }
    Ok(Value::Bool(is_all))
}

pub(crate) fn not3(v: Option<bool>) -> Option<bool> {
    v.map(|b| !b)
}

/// Compare two values: None when either side is NULL (SQL semantics).
/// Exact numerics (int2/int4/int8/numeric) compare exactly; float
/// kinds compare by `pg_float_ord` (v0.94: PG19 float8_cmp_internal —
/// NaN sorts last, NaN = NaN, -0.0 == 0.0); mixed exact/float goes
/// through f64 (a documented precision caveat). Text is byte-wise (no
/// collations yet), bools false < true, dates/times/bytea/uuid compare
/// naturally. Mismatched non-null types are 42883, like Postgres.
/// Date as a timestamp (midnight) for mixed date/timestamp comparisons.
pub(crate) fn date_as_ts(d: i32) -> i64 {
    d as i64 * 86_400_000_000
}

/// v0.81: PG19 record comparison (`record_eq`, `record_lt`, ...).
/// Returns `Ok(None)` for NULL (unknown), `Ok(Some(ordering))` otherwise.
/// `=` semantics: true iff all field pairs are equal; NULL iff some pair
/// is NULL and none is definitively unequal; false otherwise. Ordering
/// is lexicographic with NULL sorting larger (PG's ASC NULLS LAST).
pub(crate) fn cmp_records(
    fa: &[(String, Value)],
    fb: &[(String, Value)],
    op: CmpOp,
) -> Result<Option<Ordering>, ExecError> {
    // `*=` is image equality, not semantic comparison.
    if op == CmpOp::ImageEq {
        return Ok(Some(if record_image_eq(fa, fb) {
            Ordering::Equal
        } else {
            Ordering::Less
        }));
    }
    if fa.len() != fb.len() {
        return Err(exec_err(
            "42883",
            "cannot compare records with different field counts".to_string(),
        ));
    }
    let mut saw_null = false;
    for ((_, va), (_, vb)) in fa.iter().zip(fb.iter()) {
        // NULL handling: for `=`/`<>`, a NULL field makes the result
        // NULL (unless a definitive inequality is found); for ordering,
        // NULL sorts larger (PG's default ASC NULLS LAST).
        let va_null = matches!(va, Value::Null);
        let vb_null = matches!(vb, Value::Null);
        if va_null || vb_null {
            match op {
                CmpOp::Eq | CmpOp::Ne => {
                    if va_null != vb_null || !va_null {
                        // One side NULL, other not: for `=` this is not
                        // definitive (result NULL); continue checking.
                    }
                    saw_null = true;
                    continue;
                }
                _ => match (va_null, vb_null) {
                    (true, true) => continue,
                    (true, false) => return Ok(Some(Ordering::Greater)),
                    (false, true) => return Ok(Some(Ordering::Less)),
                    (false, false) => unreachable!(),
                },
            }
        }
        match cmp_ordering(va, vb, CmpOp::Eq)? {
            None => saw_null = true,
            Some(Ordering::Equal) => {}
            Some(ord) => {
                // For `=`/`<>`, a definitive inequality decides; for
                // ordering, the first non-equal field decides.
                match op {
                    CmpOp::Eq => return Ok(Some(Ordering::Less)),
                    CmpOp::Ne => return Ok(Some(Ordering::Greater)),
                    _ => return Ok(Some(ord)),
                }
            }
        }
    }
    match op {
        CmpOp::Eq | CmpOp::Ne => {
            if saw_null {
                Ok(None)
            } else {
                Ok(Some(Ordering::Equal))
            }
        }
        _ => Ok(Some(Ordering::Equal)),
    }
}

/// v0.81: PG19 `record_image_eq` — byte-oriented record identity for
/// `*=`. NULL/NULL is identical; non-NULL fields use datum image
/// equality (for numeric, the full image including display scale, so
/// `1.00` is NOT `*=` `1.0`).
pub(crate) fn record_image_eq(fa: &[(String, Value)], fb: &[(String, Value)]) -> bool {
    if fa.len() != fb.len() {
        return false;
    }
    fa.iter().zip(fb.iter()).all(|((_, va), (_, vb))| {
        match (va, vb) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            (Value::Numeric(a), Value::Numeric(b)) => {
                a.unscaled == b.unscaled && a.scale == b.scale && a.dscale == b.dscale
            }
            // For other types, semantic equality is the image equality
            // (no separate binary representation is tracked).
            _ => va == vb,
        }
    })
}

/// v0.94: PG19 float comparison ordering (the `float8_lt`/`float8_eq`
/// family in src/include/utils/float.h): NaN is greater than
/// everything (sorts last), NaN = NaN, and `-0.0 == 0.0` (IEEE `==`,
/// unlike `total_cmp` which distinguishes the signed zeros). Used for
/// both comparison operators and ORDER BY.
pub(crate) fn pg_float_ord(a: f64, b: f64) -> Ordering {
    if a.is_nan() {
        if b.is_nan() {
            Ordering::Equal
        } else {
            Ordering::Greater
        }
    } else if b.is_nan() {
        Ordering::Less
    } else if a == b {
        // Covers -0.0 == 0.0.
        Ordering::Equal
    } else if a < b {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

pub(crate) fn cmp_ordering(a: &Value, b: &Value, op: CmpOp) -> Result<Option<Ordering>, ExecError> {
    // v0.82: `*=` (ImageEq) exists only for record operands. PG has no
    // such operator for scalars, so `1 *= 2` is 42883 "operator does
    // not exist" — checked before the NULL arm, since PG resolves the
    // operator at analysis time regardless of nullness.
    if op == CmpOp::ImageEq && !matches!((a, b), (Value::Record(_), Value::Record(_))) {
        return Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                a.type_name(),
                op.sql(),
                b.type_name()
            ),
        ));
    }
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Ok(None),
        (x, y) if is_exact_numeric(x) && is_exact_numeric(y) => {
            Ok(Some(match (exact_as_i64(x), exact_as_i64(y)) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => exact_numeric(x).cmp(&exact_numeric(y)),
            }))
        }
        (Value::Float4(x), Value::Float4(y)) => Ok(Some(pg_float_ord(*x as f64, *y as f64))),
        (Value::Float4(x), Value::Float(y)) => Ok(Some(pg_float_ord(*x as f64, *y))),
        (Value::Float(x), Value::Float4(y)) => Ok(Some(pg_float_ord(*x, *y as f64))),
        (Value::Float(x), Value::Float(y)) => Ok(Some(pg_float_ord(*x, *y))),
        (x, y @ (Value::Float4(_) | Value::Float(_))) if is_exact_numeric(x) => {
            Ok(Some(pg_float_ord(exact_to_f64(x), float_val(y))))
        }
        (x @ (Value::Float4(_) | Value::Float(_)), y) if is_exact_numeric(y) => {
            Ok(Some(pg_float_ord(float_val(x), exact_to_f64(y))))
        }
        (Value::Text(x), Value::Text(y)) => Ok(Some(x.cmp(y))),
        // v0.35: PG's bpcharcmp ignores trailing spaces (bcTruelen):
        // when either side is a blank-padded char, compare the
        // significant (rtrimmed) contents.
        (Value::BpChar(x), Value::BpChar(y)) => Ok(Some(
            crate::storage::rtrim_spaces(x).cmp(crate::storage::rtrim_spaces(y)),
        )),
        (Value::BpChar(x), Value::Text(y)) => Ok(Some(
            crate::storage::rtrim_spaces(x).cmp(crate::storage::rtrim_spaces(y)),
        )),
        (Value::Text(x), Value::BpChar(y)) => Ok(Some(
            crate::storage::rtrim_spaces(x).cmp(crate::storage::rtrim_spaces(y)),
        )),
        (Value::Bool(x), Value::Bool(y)) => Ok(Some(x.cmp(y))),
        (Value::Date(x), Value::Date(y)) => Ok(Some(x.cmp(y))),
        (Value::Timestamp(x), Value::Timestamp(y)) => Ok(Some(x.cmp(y))),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => Ok(Some(x.cmp(y))),
        // Postgres casts date up to timestamp/timestamptz for mixed
        // comparisons (date is midnight).
        (Value::Date(d), Value::Timestamp(t)) => Ok(Some(date_as_ts(*d).cmp(t))),
        (Value::Timestamp(t), Value::Date(d)) => Ok(Some(t.cmp(&date_as_ts(*d)))),
        (Value::Date(d), Value::Timestamptz(t)) => Ok(Some(date_as_ts(*d).cmp(t))),
        (Value::Timestamptz(t), Value::Date(d)) => Ok(Some(t.cmp(&date_as_ts(*d)))),
        // Postgres casts timestamp up to timestamptz (session zone; v0.7
        // is UTC-only so this is exact).
        (Value::Timestamp(t), Value::Timestamptz(z)) => Ok(Some(t.cmp(z))),
        (Value::Timestamptz(z), Value::Timestamp(t)) => Ok(Some(z.cmp(t))),
        (Value::Bytea(x), Value::Bytea(y)) => Ok(Some(x.cmp(y))),
        (Value::Uuid(x), Value::Uuid(y)) => Ok(Some(x.cmp(y))),
        // v0.36: PG's "char" comparison is a plain byte comparison
        // (chareq/charlt compare the single byte).
        (Value::SingleChar(x), Value::SingleChar(y)) => Ok(Some(x.cmp(y))),
        // v0.81: PG19 record comparison (`record_eq`, `record_lt`, ...).
        // NULL fields: `=` is NULL (not false) if any field pair is
        // NULL and all others equal; ordering treats NULL as larger
        // (PG's default NULLS LAST for ASC). Field counts must match.
        (Value::Record(fa), Value::Record(fb)) => cmp_records(fa, fb, op),
        _ => Err(exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                a.type_name(),
                op.sql(),
                b.type_name()
            ),
        )),
    }
}

/// v0.38: is this expression statically typed `regclass`? A
/// `::regclass` cast, or a regclass-typed column (e.g. flowing through
/// a view or CTE). PG treats regclass as binary-coercible to oid, so a
/// comparison against an integer compares OIDs, not display text.
pub(crate) fn expr_is_regclass(scopes: &[Scope], e: &Expr) -> bool {
    match e {
        Expr::Cast { to, .. } => *to == ColType::Regclass,
        Expr::Column { table, name } => resolve_col(scopes, table.as_deref(), name)
            .map(|(si, ci)| scopes[si].schema[ci].ty == ColType::Regclass)
            .unwrap_or(false),
        Expr::ResolvedCol { frame, idx } => scopes
            .get(*frame)
            .and_then(|s| s.schema.get(*idx))
            .map(|c| c.ty == ColType::Regclass)
            .unwrap_or(false),
        _ => false,
    }
}

/// v0.57: whether an expression is `name`-typed (PG19 OID 19), for
/// comparison coercion. Mirrors `expr_is_regclass`.
pub(crate) fn expr_is_name(scopes: &[Scope], e: &Expr) -> bool {
    match e {
        Expr::Cast { to, .. } => *to == ColType::Name,
        Expr::Column { table, name } => resolve_col(scopes, table.as_deref(), name)
            .map(|(si, ci)| scopes[si].schema[ci].ty == ColType::Name)
            .unwrap_or(false),
        Expr::ResolvedCol { frame, idx } => scopes
            .get(*frame)
            .and_then(|s| s.schema.get(*idx))
            .map(|c| c.ty == ColType::Name)
            .unwrap_or(false),
        _ => false,
    }
}

/// v0.57: PG19 `name` comparison coercion (name.c). A `name` value is
/// always stored truncated at 63 bytes (see `truncate_name`), but an
/// unknown-type (text) literal on the other side of the comparison is
/// not — PG coerces it through namein, truncating it too, before the
/// strncmp comparison. So when exactly one side of `=`/`<>`/`<`/etc.
/// is name-typed and the other side is a text/bpchar value, truncate
/// the text side to 63 bytes first.
///
/// Perf: the scope/schema walk is skipped unless at least one side is
/// text-like — int-vs-int join keys (the hot path) never pay for it.
pub(crate) fn coerce_name_cmp(
    scopes: &[Scope],
    left: &Expr,
    right: &Expr,
    va: Value,
    vb: Value,
) -> (Value, Value) {
    fn is_textual(v: &Value) -> bool {
        matches!(v, Value::Text(_) | Value::BpChar(_))
    }
    if matches!(va, Value::Null) || matches!(vb, Value::Null) {
        return (va, vb);
    }
    let a_text = is_textual(&va);
    let b_text = is_textual(&vb);
    if !a_text && !b_text {
        return (va, vb);
    }
    // Only the non-name side gets truncated; a name-typed side is
    // already truncated at input. Truncation is idempotent anyway.
    let l_name = b_text && expr_is_name(scopes, left);
    let r_name = a_text && expr_is_name(scopes, right);
    let trunc = |v: Value| -> Value {
        match v {
            Value::Text(s) => Value::text(crate::storage::truncate_name(&s)),
            Value::BpChar(s) => Value::text(crate::storage::truncate_name(&s)),
            other => other,
        }
    };
    match (l_name, r_name) {
        (true, false) => (va, trunc(vb)),
        (false, true) => (trunc(va), vb),
        _ => (va, vb),
    }
}

/// v0.38: resolve a regclass-typed value to its OID. `Value::Text` here
/// is the display form (`regclass_display`): `"-"` is OID 0, an
/// all-digit string is the OID itself, anything else must name a
/// visible relation (42P01, like the cast).
pub(crate) fn regclass_value_oid(q: &mut Q, v: &Value) -> Result<u32, ExecError> {
    match v {
        Value::SmallInt(i) => Ok(*i as u32),
        Value::Int(i) | Value::BigInt(i) => Ok(*i as u32),
        Value::Text(s) => {
            if s.as_ref() == "-" {
                Ok(0)
            } else if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                Ok(s.parse::<u32>().unwrap_or(0))
            } else {
                let t = q
                    .eng
                    .db
                    .find_table(s, q.snap, &q.all_xids, q.session)
                    .ok_or_else(|| {
                        exec_err("42P01", format!("relation \"{}\" does not exist", s))
                    })?;
                Ok(t.oid)
            }
        }
        _ => Err(exec_err(
            "42846",
            format!(
                "cannot resolve regclass value of type {} to an OID",
                v.type_name()
            ),
        )),
    }
}

/// v0.63 perf: true for the three exact-integer `Value` variants. An
/// int-valued operand can never be regclass- or name-typed (see
/// `coerce_regclass_cmp`/`coerce_name_cmp`'s doc comments), so
/// `Expr::Cmp`'s evaluator uses this to skip both coercion cascades
/// entirely when both operands are already int-valued, without calling
/// into either function.
#[inline]
pub(crate) fn is_exact_int_value(v: &Value) -> bool {
    matches!(v, Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_))
}

/// v0.38: PG's binary-coercible regclass/oid comparison. When one side
/// of a comparison is statically typed regclass and the other side is
/// integer-typed (or both sides are regclass), resolve the regclass
/// display text back to its OID and compare as integers — PG19 applies
/// `oideq` directly. Any other type combination keeps the existing
/// value-level coercion, so `intcol = 'sometext'` still raises 22P02
/// instead of resolving the text as a relation name.
pub(crate) fn coerce_regclass_cmp(
    q: &mut Q,
    scopes: &[Scope],
    left: &Expr,
    right: &Expr,
    va: Value,
    vb: Value,
) -> Result<(Value, Value), ExecError> {
    if matches!(va, Value::Null) || matches!(vb, Value::Null) {
        return Ok((va, vb));
    }
    let a_int = is_exact_int_value(&va);
    let b_int = is_exact_int_value(&vb);
    // A regclass-typed expression only ever evaluates to `Value::Text`:
    // `eval_regclass_cast` (the only producer of a regclass value) always
    // returns `Value::text(..)` (see its doc comment), and column
    // assignment into a regclass target goes through the same Text-only
    // path. So an int-valued operand can never itself be regclass-typed,
    // and `expr_is_regclass`'s scope/schema walk is skippable for that
    // side — this is the common case (`eval_expr`'s comparisons are
    // overwhelmingly int-vs-int: join keys, id filters), where both
    // lookups below are now skipped entirely instead of running on every
    // single comparison.
    let l_rc = !a_int && expr_is_regclass(scopes, left);
    let r_rc = !b_int && expr_is_regclass(scopes, right);
    match (l_rc, r_rc, a_int, b_int) {
        (true, _, _, true) => {
            let oid = regclass_value_oid(q, &va)?;
            Ok((Value::Int(oid as i64), vb))
        }
        (_, true, true, _) => {
            let oid = regclass_value_oid(q, &vb)?;
            Ok((va, Value::Int(oid as i64)))
        }
        (true, true, _, _) => {
            let a = regclass_value_oid(q, &va)?;
            let b = regclass_value_oid(q, &vb)?;
            Ok((Value::Int(a as i64), Value::Int(b as i64)))
        }
        _ => Ok((va, vb)),
    }
}

/// v0.48: NaN test across the float/numeric value kinds, for
/// `IS DISTINCT FROM` (PG19 treats NaN as equal to NaN there, unlike
/// the `=` operator).
pub(crate) fn is_nan_val(v: &Value) -> bool {
    match v {
        Value::Numeric(n) => n.is_nan(),
        Value::Float(f) => f.is_nan(),
        Value::Float4(f) => f.is_nan(),
        _ => false,
    }
}

/// v0.48: shared `IS [NOT] DISTINCT FROM` value logic for the row
/// and grouped evaluators (PG19): NULLs compare equal and never
/// produce unknown; NaN compares equal to NaN (like `=` since v0.94 —
/// PG19 `float8_eq`/`cmp_numerics` treat NaN as equal to NaN);
/// otherwise the negation of `=`.
pub(crate) fn eval_is_distinct_from(l: Value, r: Value, neg: bool) -> Result<Value, ExecError> {
    let distinct = match (&l, &r) {
        (Value::Null, Value::Null) => false,
        (Value::Null, _) | (_, Value::Null) => true,
        _ if is_nan_val(&l) && is_nan_val(&r) => false,
        _ => match eval_cmp_vals(CmpOp::Eq, &l, &r)? {
            Value::Bool(eq) => !eq,
            // Unreachable for non-null inputs (`=` never returns NULL
            // there); stay total, never panic.
            _ => true,
        },
    };
    Ok(Value::Bool(distinct != neg))
}

pub(crate) fn eval_cmp_vals(op: CmpOp, a: &Value, b: &Value) -> Result<Value, ExecError> {
    // v0.79: array comparisons — `=`/`<>` are PG19 `array_eq`
    // (three-valued element-wise); anything else has no PG array
    // operator (42883); array-vs-NULL is NULL.
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => return eval_array_cmp(op, x, y),
        (Value::Array(_), Value::Null) | (Value::Null, Value::Array(_)) => return Ok(Value::Null),
        (Value::Array(_), _other) | (_other, Value::Array(_)) => {
            return Err(exec_err(
                "42883",
                format!(
                    "operator does not exist: {} {} {}",
                    a.type_name(),
                    op.sql(),
                    b.type_name()
                ),
            ));
        }
        _ => {}
    }
    // v0.21: text-vs-numeric coercion (unknown-literal resolution) runs
    // before the NaN special-case, so 'nan' = x behaves like NaN = x
    // and 1.5 = '1.5' is true.
    let coerced = coerce_text_numeric(a, b)?;
    let (a, b): (&Value, &Value) = match &coerced {
        Some((ac, bc)) => (ac, bc),
        None => (a, b),
    };
    // v0.94: PG19 NaN equality. The old comment here ("NaN != NaN")
    // was wrong: `src/include/utils/float.h` defines
    // `float8_eq(a,b)` as `isnan(a) ? isnan(b) : ...` (NaN = NaN is
    // TRUE, NaN <> NaN is FALSE), and numeric.c `cmp_numerics`
    // says "We consider all NANs to be equal". This covers float
    // and numeric NaNs alike (mixed pairs coerce as usual); ORDER BY
    // still sorts NaN last via cmp_ordering.
    let a_is_nan = is_nan_val(a);
    let b_is_nan = is_nan_val(b);
    if a_is_nan || b_is_nan {
        if matches!(op, CmpOp::Eq | CmpOp::Ne) {
            let eq = a_is_nan && b_is_nan;
            return Ok(Value::Bool(if op == CmpOp::Eq { eq } else { !eq }));
        }
        // For ordering ops with NaN, fall through to cmp (NaN sorts last).
        return match cmp_ordering(a, b, op)? {
            None => Ok(Value::Null),
            Some(ord) => Ok(Value::Bool(match op {
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Le => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                CmpOp::Ge => ord != Ordering::Less,
                // Eq/Ne are handled above; any other operator is an
                // internal error, never a panic.
                _ => {
                    return Err(exec_err(
                        "XX000",
                        "internal error: unexpected comparison operator",
                    ));
                }
            })),
        };
    }
    match cmp_ordering(a, b, op)? {
        None => Ok(Value::Null),
        Some(ord) => Ok(Value::Bool(match op {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            // v0.81: `*=` is true iff image-equal (Equal from
            // record_image_eq); never NULL (NULL/NULL is identical).
            CmpOp::ImageEq => ord == Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
        })),
    }
}

/// v0.55: shared CASE evaluation (PG19 semantics). `ev` evaluates one
/// sub-expression in the caller's scope; `result_ty` is the CASE's
/// statically resolved result type (None = unknown at eval time), to
/// which the taken arm is coerced.
///
/// * The simple-CASE operand is evaluated exactly once.
/// * Arms short-circuit: neither the conditions/results of later arms
///   nor the untaken arms' errors are evaluated (`CASE WHEN 1=0 THEN
///   1/0 ...` does not raise).
/// * Simple-CASE uses regular `=` semantics: NULL never matches a WHEN
///   key (it is not `IS NOT DISTINCT FROM`).
/// * A searched-CASE WHEN must be boolean-or-NULL (NULL counts as not
///   true); anything else is 42804, like WHERE.
pub(crate) fn eval_case<E>(
    operand: &Option<Box<Expr>>,
    whens: &[(Box<Expr>, Box<Expr>)],
    else_: &Option<Box<Expr>>,
    result_ty: Option<ColType>,
    mut ev: E,
) -> Result<Value, ExecError>
where
    E: FnMut(&Expr) -> Result<Value, ExecError>,
{
    let op_val = match operand {
        Some(o) => Some(ev(o)?),
        None => None,
    };
    for (cond, result) in whens {
        let take = match &op_val {
            Some(ov) => {
                let kv = ev(cond)?;
                matches!(eval_cmp_vals(CmpOp::Eq, ov, &kv)?, Value::Bool(true))
            }
            None => check_bool(ev(cond)?, "WHEN")?,
        };
        if take {
            let v = ev(result)?;
            return match &result_ty {
                Some(t) => coerce_value(v, t, "CASE"),
                None => Ok(v),
            };
        }
    }
    let v = match else_ {
        Some(e) => ev(e)?,
        None => Value::Null,
    };
    match &result_ty {
        Some(t) => coerce_value(v, t, "CASE"),
        None => Ok(v),
    }
}

/// v0.55: eval-time CASE result type. Best-effort: CTE references inside
/// arms cannot be resolved here (the evaluator holds materialized CTE
/// bindings, not `CteDef`s), so 42P01 falls back to `None` — no
/// coercion, which is safe because the plan-time `expr_type` check
/// (with real CTEs) already validated the arms and fixed the output
/// column type. Any other error propagates.
#[allow(clippy::too_many_arguments)]
pub(crate) fn case_eval_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    whens: &[(Box<Expr>, Box<Expr>)],
    else_: &Option<Box<Expr>>,
) -> Result<Option<ColType>, ExecError> {
    match case_result_type(eng, snap, own, session, schemas, &[], whens, else_) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.code == "42P01" => Ok(None),
        Err(e) => Err(e),
    }
}

/// AND/OR/NOT operands must be boolean (or NULL); anything else is 42804.
pub(crate) fn as_bool3(v: &Value, what: &str) -> Result<Option<bool>, ExecError> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(exec_err(
            "42804",
            format!(
                "{} requires boolean operands, not {}",
                what,
                other.type_name()
            ),
        )),
    }
}

pub(crate) fn bool3(v: Option<bool>) -> Value {
    match v {
        Some(b) => Value::Bool(b),
        None => Value::Null,
    }
}

pub(crate) fn eval_and_vals(a: &Value, b: &Value) -> Result<Value, ExecError> {
    let (a, b) = (as_bool3(a, "AND")?, as_bool3(b, "AND")?);
    Ok(bool3(match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }))
}

pub(crate) fn eval_or_vals(a: &Value, b: &Value) -> Result<Value, ExecError> {
    let (a, b) = (as_bool3(a, "OR")?, as_bool3(b, "OR")?);
    Ok(bool3(match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }))
}

pub(crate) fn eval_not_val(v: &Value) -> Result<Value, ExecError> {
    Ok(bool3(not3(as_bool3(v, "NOT")?)))
}

/// Evaluate a SET expression for UPDATE: the row being updated is the
/// correlation scope, so subqueries in SET can reference it.
pub(crate) fn eval_update_expr(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    schema: &[QCol],
    values: &[Value],
    e: &Expr,
    ctes: &[Rc<CteBinding>],
    // v0.89: statement-local UPDATE overlay (volatile functions only).
    pending: Option<Rc<RefCell<Vec<(String, u64, Row)>>>>,
    // v1.33: statement write context (see `eval_dml_expr`).
    write: Option<QWrite<'_>>,
) -> Result<Value, ExecError> {
    eval_dml_expr(
        eng,
        snap,
        own,
        session,
        role,
        &[(schema, values)],
        None,
        e,
        ctes,
        pending,
        write,
    )
}

/// v0.10: evaluate a DML expression (UPDATE SET, RETURNING, ON CONFLICT
/// DO UPDATE) against one or more explicit frames. Frames are ordered
/// outermost-first: unqualified column references resolve to the LAST
/// frame (like nested scopes), so callers put the target table last.
pub(crate) fn eval_dml_expr(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    frames: &[(&[QCol], &[Value])],
    // v1.40: per-row provenance for RETURNING system columns
    // (`tableoid`/`xmin`/`xmax`/`ctid`/`cmin`/`cmax`). None for SET/WHERE
    // expressions (system columns are not visible there).
    prov: Option<&[RowProv]>,
    e: &Expr,
    ctes: &[Rc<CteBinding>],
    // v0.89: statement-local UPDATE overlay (volatile functions only).
    pending: Option<Rc<RefCell<Vec<(String, u64, Row)>>>>,
    // v1.33: statement write context, so DML inside SQL function bodies
    // called from DML expressions (e.g. `INSERT INTO t SELECT f()`
    // where `f` performs DML) reaches the statement's write log.
    write: Option<QWrite<'_>>,
) -> Result<Value, ExecError> {
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap,
        own,
        all_xids: vec![own],
        session,
        role,
        // v0.17: DML-only helper — INSERT/UPDATE/DELETE are
        // statement-blocked when read-only, so this is false.
        read_only: false,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: ctes.to_vec(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: pending,
        write,
    };
    let scopes: Vec<Scope> = frames
        .iter()
        .enumerate()
        .map(|(i, (schema, row))| Scope {
            schema,
            row,
            // v1.40: the provenance attaches to the last frame — the
            // target range (RETURNING combines FROM/USING + target
            // with the target last). Other frames keep prov: None.
            prov: if i + 1 == frames.len() { prov } else { None },
        })
        .collect();
    let v = eval_expr(&mut q, &scopes, e)?;
    // FOR UPDATE inside an UPDATE's SET subquery locks nothing: UPDATE is
    // not SELECT, so there is no statement-level lock flow to hand the
    // collected ids to (documented).
    Ok(v)
}
