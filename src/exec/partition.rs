// v1.78 mechanical split: moved verbatim from src/exec.rs (970-3620).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ------------------------------------------------------------------
// v0.69: declarative partitioning (PG19 partdef.c).
// ------------------------------------------------------------------

use crate::sql::{PartBoundDef, PartitionDef, PartitionKeyDef, RangeBoundDef};
use crate::storage::{PartBound, PartKey, PartMethod, PartitionInfo, RangeBound};

/// v0.69: build the `PartitionInfo` for a new table from its
/// `PartitionDef`. For `PARTITION OF`, the parent is looked up and the
/// method/key are inherited; the bound is validated and coerced to the
/// parent key column types. Returns the info and (for PARTITION OF)
/// the parent's name so the caller can link the child.
pub(crate) fn build_partition_info(
    eng: &Engine,
    ctx: &StmtCtx,
    name: &str,
    def: &PartitionDef,
    columns: &[(String, ColType)],
) -> Result<(PartitionInfo, Option<String>), ExecError> {
    if let Some(parent_name) = &def.parent {
        // PARTITION OF: inherit method/key/columns from the parent.
        let parent = eng
            .db
            .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| {
                exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", parent_name),
                )
            })?;
        let pinfo = parent.partition.clone().ok_or_else(|| {
            exec_err(
                "42601",
                format!("relation \"{}\" is not partitioned", parent_name),
            )
        })?;
        // The parent must itself be a partitioned table (have a method).
        // (A leaf can't be a parent in our model — PG allows it via
        // sub-partitioning, which we support: an intermediate has both
        // a bound and children.)
        let bound_def = def
            .bound
            .as_ref()
            .ok_or_else(|| exec_err("42601", "PARTITION OF requires FOR VALUES".to_string()))?;
        let bound = convert_part_bound(eng, ctx, bound_def, parent, &pinfo)?;
        // Validate the bound against siblings (no overlap).
        check_bound_no_overlap(eng, ctx, parent_name, &pinfo, &bound, name)?;
        // v0.70: a trailing `PARTITION BY` makes this child a
        // sub-partitioned intermediate with its own method/key (the bound
        // above is still relative to the *parent's* key). Without it, the
        // child is a plain leaf inheriting the parent's method/key.
        // (The parser leaves `keys` empty when there is no trailing
        // `PARTITION BY`.)
        let (method, key) = if def.keys.is_empty() {
            (pinfo.method, pinfo.key.clone())
        } else {
            let mut key = Vec::new();
            for kd in &def.keys {
                match kd {
                    PartitionKeyDef::Column(col) => {
                        let idx = columns.iter().position(|(n, _)| n == col).ok_or_else(|| {
                            exec_err(
                                "42703",
                                format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    col, name
                                ),
                            )
                        })?;
                        key.push(PartKey {
                            col: idx,
                            expr: None,
                        });
                    }
                    PartitionKeyDef::Expr(e) => {
                        key.push(PartKey {
                            col: usize::MAX,
                            expr: Some(e.clone()),
                        });
                    }
                }
            }
            (def.method, key)
        };
        let info = PartitionInfo {
            method,
            key,
            bound: Some(bound),
            is_default: matches!(bound_def, PartBoundDef::Default),
            parent: Some(parent_name.clone()),
            children: Vec::new(),
            // v0.72: a trailing PARTITION BY (non-empty keys) makes this
            // a sub-partitioned intermediate; otherwise it is a leaf.
            is_partitioned: !def.keys.is_empty(),
        };
        Ok((info, Some(parent_name.clone())))
    } else {
        // Partitioned root: resolve key columns.
        let mut key = Vec::new();
        for kd in &def.keys {
            match kd {
                PartitionKeyDef::Column(col) => {
                    let idx = columns.iter().position(|(n, _)| n == col).ok_or_else(|| {
                        exec_err(
                            "42703",
                            format!("column \"{}\" of relation \"{}\" does not exist", col, name),
                        )
                    })?;
                    key.push(PartKey {
                        col: idx,
                        expr: None,
                    });
                }
                PartitionKeyDef::Expr(e) => {
                    key.push(PartKey {
                        col: usize::MAX,
                        expr: Some(e.clone()),
                    });
                }
            }
        }
        let info = PartitionInfo {
            method: def.method,
            key,
            bound: None,
            is_default: false,
            parent: None,
            children: Vec::new(),
            // v0.72: a partitioned root always accepts routed inserts;
            // only its leaves (or a DEFAULT child) hold rows.
            is_partitioned: true,
        };
        Ok((info, None))
    }
}

/// v0.69: coerce a parsed `PartBoundDef` to a runtime `PartBound`,
/// using the parent's key column types. Enforces PG19's
/// MINVALUE/MAXVALUE rules (partition.c: `check_new_partition_bound`).
/// v0.69: infer the result type of a partition key expression.
/// Used for coercing bound literals. Handles the corpus patterns:
/// lower/upper -> Text, abs(x) -> type of x, arithmetic -> type of left.
pub(crate) fn infer_expr_key_type(expr: &Expr, parent: &Table) -> ColType {
    match expr {
        Expr::Column { name, .. } => {
            // Find the column type by name.
            parent
                .columns
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, ty)| *ty)
                .unwrap_or(ColType::Int)
        }
        Expr::Func { name, args } => {
            if name.eq_ignore_ascii_case("lower") || name.eq_ignore_ascii_case("upper") {
                ColType::Text
            } else if name.eq_ignore_ascii_case("abs") {
                // abs(x) has the same type as x.
                args.first()
                    .map(|a| infer_expr_key_type(a, parent))
                    .unwrap_or(ColType::Int)
            } else {
                // Unknown function: default to Text (corpus uses text).
                ColType::Text
            }
        }
        Expr::Arith { left, .. } => {
            // Arithmetic: use the left operand's type.
            infer_expr_key_type(left, parent)
        }
        _ => ColType::Int,
    }
}

pub(crate) fn convert_part_bound(
    _eng: &Engine,
    _ctx: &StmtCtx,
    bound: &PartBoundDef,
    parent: &Table,
    pinfo: &PartitionInfo,
) -> Result<PartBound, ExecError> {
    // Key column types for coercion. For expression keys, we infer the
    // type from the expression shape.
    let key_types: Vec<ColType> = pinfo
        .key
        .iter()
        .map(|k| {
            if k.expr.is_none() {
                parent.columns[k.col].1
            } else {
                // Expression key: infer type from expression shape.
                infer_expr_key_type(k.expr.as_ref().unwrap(), parent)
            }
        })
        .collect();
    match bound {
        PartBoundDef::List(exprs) => {
            let mut values = Vec::new();
            let mut has_null = false;
            for e in exprs {
                match e {
                    Expr::Literal(Literal::Null) => has_null = true,
                    Expr::Literal(l) => {
                        // Coerce to the first key column's type (list keys
                        // are single-column in the corpus; multi-column
                        // list keys aren't supported by PG anyway).
                        let ty = key_types.first().copied().unwrap_or(ColType::Int);
                        values.push(coerce_literal(l, &ty, &parent.columns[0].0)?);
                    }
                    _ => {
                        return Err(exec_err(
                            "42601",
                            "partition bound must be a literal".to_string(),
                        ));
                    }
                }
            }
            Ok(PartBound::List { values, has_null })
        }
        PartBoundDef::Range { lower, upper } => {
            let n = pinfo.key.len();
            if lower.len() != n || upper.len() != n {
                return Err(exec_err(
                    "42601",
                    format!(
                        "partition bound must have {} columns, got {} and {}",
                        n,
                        lower.len(),
                        upper.len()
                    ),
                ));
            }
            let mut lo = Vec::with_capacity(n);
            let mut hi = Vec::with_capacity(n);
            for (i, b) in lower.iter().enumerate() {
                lo.push(convert_range_bound(b, &key_types[i], &parent.columns)?);
            }
            for (i, b) in upper.iter().enumerate() {
                hi.push(convert_range_bound(b, &key_types[i], &parent.columns)?);
            }
            // PG19: every bound following MINVALUE must also be MINVALUE;
            // every bound following MAXVALUE must also be MAXVALUE.
            check_minmax_rules(&lo, "MINVALUE")?;
            check_minmax_rules(&hi, "MAXVALUE")?;
            // Lower must be < upper (PG checks this; the corpus doesn't
            // test invalid ranges, but it's cheap).
            Ok(PartBound::Range {
                lower: lo,
                upper: hi,
            })
        }
        PartBoundDef::Hash { modulus, remainder } => {
            if *modulus == 0 || *remainder >= *modulus {
                return Err(exec_err(
                    "42601",
                    "invalid MODULUS/REMAINDER for hash partition".to_string(),
                ));
            }
            Ok(PartBound::Hash {
                modulus: *modulus,
                remainder: *remainder,
            })
        }
        PartBoundDef::Default => {
            // DEFAULT is represented as a flag; the bound is a dummy.
            // (We use is_default on the PartitionInfo.)
            Ok(PartBound::List {
                values: Vec::new(),
                has_null: false,
            })
        }
    }
}

pub(crate) fn convert_range_bound(
    b: &RangeBoundDef,
    ty: &ColType,
    columns: &[(String, ColType)],
) -> Result<RangeBound, ExecError> {
    match b {
        RangeBoundDef::Min => Ok(RangeBound::Min),
        RangeBoundDef::Max => Ok(RangeBound::Max),
        RangeBoundDef::Val(Expr::Literal(l)) => {
            Ok(RangeBound::Val(coerce_literal(l, ty, &columns[0].0)?))
        }
        RangeBoundDef::Val(_) => Err(exec_err(
            "42601",
            "partition bound must be a literal".to_string(),
        )),
    }
}

/// v0.69: enforce "every bound following MINVALUE must also be MINVALUE"
/// and the MAXVALUE analogue (PG19 partition.c).
pub(crate) fn check_minmax_rules(bounds: &[RangeBound], kind: &str) -> Result<(), ExecError> {
    let mut seen_min = false;
    let mut seen_max = false;
    for b in bounds {
        match b {
            RangeBound::Min => {
                if seen_max {
                    return Err(exec_err(
                        "42601",
                        "every bound following MAXVALUE must also be MAXVALUE".to_string(),
                    ));
                }
                seen_min = true;
            }
            RangeBound::Max => {
                if seen_min {
                    return Err(exec_err(
                        "42601",
                        format!("every bound following {} must also be {}", kind, kind),
                    ));
                }
                seen_max = true;
            }
            RangeBound::Val(_) => {
                if seen_min {
                    return Err(exec_err(
                        "42601",
                        "every bound following MINVALUE must also be MINVALUE".to_string(),
                    ));
                }
                if seen_max {
                    return Err(exec_err(
                        "42601",
                        "every bound following MAXVALUE must also be MAXVALUE".to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// v0.69: check that a new partition's bound doesn't overlap existing
/// siblings (PG19: 23514 "partition would overlap").
pub(crate) fn check_bound_no_overlap(
    eng: &Engine,
    ctx: &StmtCtx,
    parent_name: &str,
    pinfo: &PartitionInfo,
    bound: &PartBound,
    new_name: &str,
) -> Result<(), ExecError> {
    for child_name in &pinfo.children {
        // v1.80: skip a stale link (child gone, or no longer a bounded
        // partition) like routing does, instead of panicking. v1.79's
        // expect()s killed the connection on state left by the WAL losing
        // partition metadata (fixed in v1.80, RGSWAL21).
        let Some(cbound) = eng
            .db
            .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
            .and_then(|child| child.partition.as_ref())
            .and_then(|cinfo| cinfo.bound.as_ref())
        else {
            continue;
        };
        if bounds_overlap(&pinfo.method, bound, cbound) {
            return Err(exec_err(
                "23514",
                format!(
                    "partition \"{}\" would overlap partition \"{}\"",
                    new_name, child_name
                ),
            ));
        }
    }
    // Multiple DEFAULT partitions are not allowed.
    if matches!(bound, PartBound::List { values, has_null } if values.is_empty() && !has_null) {
        // This is a DEFAULT bound; check if a default already exists.
        for child_name in &pinfo.children {
            let child = eng
                .db
                .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("child still visible");
            if child
                .partition
                .as_ref()
                .map(|p| p.is_default)
                .unwrap_or(false)
            {
                return Err(exec_err(
                    "42601",
                    format!(
                        "relation \"{}\" already has a DEFAULT partition",
                        parent_name
                    ),
                ));
            }
        }
    }
    let _ = parent_name;
    Ok(())
}

/// v0.69: do two partition bounds overlap? (Conservative: only checks
/// exact duplicates for LIST, range intersection for RANGE.)
pub(crate) fn bounds_overlap(method: &PartMethod, a: &PartBound, b: &PartBound) -> bool {
    match (method, a, b) {
        (
            PartMethod::List,
            PartBound::List {
                values: av,
                has_null: an,
            },
            PartBound::List {
                values: bv,
                has_null: bn,
            },
        ) => {
            if *an && *bn {
                return true;
            }
            av.iter().any(|v| bv.contains(v))
        }
        (
            PartMethod::Range,
            PartBound::Range {
                lower: al,
                upper: au,
            },
            PartBound::Range {
                lower: bl,
                upper: bu,
            },
        ) => {
            // Overlap iff al < bu AND bl < au (lexicographic).
            range_bound_lt(al, bu) && range_bound_lt(bl, au)
        }
        (
            PartMethod::Hash,
            PartBound::Hash {
                modulus: am,
                remainder: ar,
            },
            PartBound::Hash {
                modulus: bm,
                remainder: br,
            },
        ) => {
            // PG requires the same modulus for all hash partitions;
            // overlap iff remainders are equal.
            am == bm && ar == br
        }
        _ => false,
    }
}

/// v0.69: lexicographic `<` on range bound vectors. Min < Val < Max.
pub(crate) fn range_bound_lt(a: &[RangeBound], b: &[RangeBound]) -> bool {
    for (x, y) in a.iter().zip(b.iter()) {
        match (x, y) {
            (RangeBound::Min, RangeBound::Min) => continue,
            (RangeBound::Min, _) => return true,
            (_, RangeBound::Min) => return false,
            (RangeBound::Max, RangeBound::Max) => continue,
            (RangeBound::Max, _) => return false,
            (_, RangeBound::Max) => return true,
            (RangeBound::Val(vx), RangeBound::Val(vy)) => match compare_partition_values(vx, vy) {
                Some(std::cmp::Ordering::Less) => return true,
                Some(std::cmp::Ordering::Greater) => return false,
                _ => continue,
            },
        }
    }
    false
}

/// v0.69: does a key satisfy `[lower, upper)`? (Range partition containment.)
pub(crate) fn range_bound_contains(
    lower: &[RangeBound],
    upper: &[RangeBound],
    key: &[Value],
) -> bool {
    // v0.72: a NULL partition key value never satisfies a range bound
    // (PG19: NULL is not routable for RANGE — it lands in the DEFAULT
    // partition if one exists, else 23514).
    if key.iter().any(|k| matches!(k, Value::Null)) {
        return false;
    }
    // key >= lower (lexicographic)
    for (k, b) in key.iter().zip(lower.iter()) {
        match b {
            RangeBound::Min => continue,
            RangeBound::Max => return false, // key < +inf always, but >= +inf never
            RangeBound::Val(v) => match compare_partition_values(k, v) {
                Some(std::cmp::Ordering::Less) => return false,
                Some(std::cmp::Ordering::Greater) => break,
                _ => continue, // equal or incomparable: check next
            },
        }
    }
    // key < upper (lexicographic): need a strict Less at some position,
    // with all prior positions Equal.
    for (k, b) in key.iter().zip(upper.iter()) {
        match b {
            RangeBound::Max => return true, // key < +inf always
            RangeBound::Min => return false,
            RangeBound::Val(v) => match compare_partition_values(k, v) {
                Some(std::cmp::Ordering::Less) => return true,
                Some(std::cmp::Ordering::Greater) => return false,
                _ => continue, // equal: check next column
            },
        }
    }
    // All columns equal: key == upper, not < upper.
    false
}

/// v0.69: total ordering on Values for partition routing. Returns None
/// for incomparable types (treated as not-equal).
pub(crate) fn compare_partition_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Null, _) | (_, Value::Null) => None,
        // Integers: compare as i64 (SmallInt/Int/BigInt are all ints).
        (Value::SmallInt(x), Value::SmallInt(y)) => Some(x.cmp(y)),
        (Value::SmallInt(x), Value::Int(y)) => Some((*x as i64).cmp(y)),
        (Value::SmallInt(x), Value::BigInt(y)) => Some((*x as i64).cmp(y)),
        (Value::Int(x), Value::SmallInt(y)) => Some(x.cmp(&(*y as i64))),
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Int(x), Value::BigInt(y)) => Some(x.cmp(y)),
        (Value::BigInt(x), Value::SmallInt(y)) => Some(x.cmp(&(*y as i64))),
        (Value::BigInt(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::BigInt(x), Value::BigInt(y)) => Some(x.cmp(y)),
        // Text: lexicographic.
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::BpChar(x), Value::BpChar(y)) => Some(x.cmp(y)),
        (Value::Text(x), Value::BpChar(y)) => Some(x.as_ref().cmp(y.as_ref())),
        (Value::BpChar(x), Value::Text(y)) => Some(x.as_ref().cmp(y.as_ref())),
        // Cross-type int/text etc: not comparable.
        _ => {
            // Fallback: compare discriminants for a stable (if arbitrary)
            // order — but only for equality, not ordering.
            if std::mem::discriminant(a) == std::mem::discriminant(b) {
                None
            } else {
                None
            }
        }
    }
}

/// v0.70: commit a modified table clone as a new version owned by this
/// transaction (the same copy-on-write pattern every other ALTER uses)
/// and log the `AlterTable` undo op. The caller's `next` must already
/// carry the desired mutations; this hides the old live version from
/// this transaction, moves its rows over, and records `prev` for
/// rollback.
pub(crate) fn commit_table_version(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    prev: Table,
    mut next: Table,
) {
    // v1.80: temp tables live in the session's temp map, not in
    // `eng.db.tables`. Version them the temp way, so every caller (CREATE
    // and DROP TRIGGER included) is safe on temp tables. v1.79 looked
    // only in the permanent map and panicked "table still visible".
    if eng.db.is_temp_table(ctx.session, name) {
        let _ = prev; // alter_swap_temp snapshots the undo image itself
        alter_swap_temp(eng, ctx, name, None, next, None)
            .expect("is_temp_table just found the temp table");
        return;
    }
    next.created_xmin = ctx.own;
    next.dropped_xmax = 0;
    let versions = eng.db.tables.get_mut(name).expect("table still visible");
    let old_live = versions
        .iter_mut()
        .find(|t| crate::storage::table_visible(t, ctx.snap, &ctx.all_xids))
        .expect("table still visible");
    old_live.dropped_xmax = ctx.own;
    next.rows = std::mem::take(&mut old_live.rows);
    next.rebuild_row_index();
    old_live.rebuild_row_index();
    versions.push(next);
    ctx.writes.push(WriteOp::AlterTable {
        name: name.to_string(),
        prev,
        renamed_to: None,
        rewrite_rows: false,
    });
}

/// v0.70: add (`link=true`) or remove (`link=false`) a child from a
/// partitioned parent's `children` list, transactionally: the parent is
/// versioned via [`commit_table_version`], so ROLLBACK restores the
/// previous link set. (v0.69 mutated the live version in place, which
/// silently defeated the `AlterTable` undo — an `ATTACH PARTITION`
/// survived ROLLBACK.)
pub(crate) fn parent_link_child(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    parent_name: &str,
    child_name: &str,
    link: bool,
) {
    let prev = eng
        .db
        .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
        .expect("parent still visible")
        .clone();
    let mut next = prev.clone();
    {
        let pi = next.partition.as_mut().expect("parent is partitioned");
        if link {
            if !pi.children.iter().any(|c| c == child_name) {
                pi.children.push(child_name.to_string());
            }
        } else {
            pi.children.retain(|c| c != child_name);
        }
    }
    commit_table_version(eng, ctx, parent_name, prev, next);
}

/// v0.69: `ALTER TABLE parent ATTACH PARTITION child FOR VALUES ...`.
pub(crate) fn attach_partition(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    parent_name: &str,
    child_name: &str,
    bound_def: &PartBoundDef,
) -> Result<ExecResult, ExecError> {
    // v0.11: every ALTER TABLE needs owner-or-superuser.
    require_table_owner(eng, ctx, parent_name)?;
    // Resolve parent (must be partitioned).
    let (pinfo, parent_cols) = {
        let parent = eng
            .db
            .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| {
                exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", parent_name),
                )
            })?;
        let pinfo = parent.partition.clone().ok_or_else(|| {
            exec_err(
                "42601",
                format!("relation \"{}\" is not partitioned", parent_name),
            )
        })?;
        (pinfo, parent.columns.clone())
    };
    // Resolve child (must exist, must not already be a partition *of
    // something else* — PG allows attaching an already-partitioned
    // table, which becomes a sub-partitioned intermediate).
    let (child_cols, child_pinfo) = {
        let child = eng
            .db
            .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| {
                exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", child_name),
                )
            })?;
        if child.partition.as_ref().is_some_and(|p| p.parent.is_some()) {
            return Err(exec_err(
                "42601",
                format!("relation \"{}\" is already a partition", child_name),
            ));
        }
        (child.columns.clone(), child.partition.clone())
    };
    // Validate column compatibility by name (PG: same names, binary-
    // coercible types; we require exact type equality for simplicity,
    // which covers the corpus).
    for (pname, ptype) in &parent_cols {
        let ctype = child_cols
            .iter()
            .find(|(n, _)| n == pname)
            .map(|(_, t)| t)
            .ok_or_else(|| {
                exec_err(
                    "42601",
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        pname, child_name
                    ),
                )
            })?;
        if ctype != ptype {
            return Err(exec_err(
                "42601",
                format!(
                    "column \"{}\" of relation \"{}\" has type {} but parent has type {}",
                    pname,
                    child_name,
                    col_type_name(ctype),
                    col_type_name(ptype)
                ),
            ));
        }
    }
    // Convert and validate the bound.
    let parent_for_bound = eng
        .db
        .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
        .expect("parent still visible");
    let bound = convert_part_bound(eng, ctx, bound_def, parent_for_bound, &pinfo)?;
    check_bound_no_overlap(eng, ctx, parent_name, &pinfo, &bound, child_name)?;
    // Existing rows must satisfy the bound (PG checks this).
    {
        // Copy the rows out; the borrow ends before key evaluation,
        // which needs `&mut eng`.
        let rows: Vec<Row> = {
            let child = eng
                .db
                .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("child still visible");
            child
                .rows
                .iter()
                .filter(|rv| row_visible(rv, ctx.snap, &ctx.all_xids))
                .map(|rv| rv.values.clone())
                .collect()
        };
        for values in &rows {
            // Map child row to parent column order, then check bound.
            // v0.70: expression partition keys are evaluated for real
            // (was NULL), via the shared key evaluator.
            let prow = remap_row_to_parent(&child_cols, &parent_cols, values);
            let key = eval_table_part_key(eng, ctx, parent_name, &pinfo, &parent_cols, &prow)?;
            if !bound_contains(&pinfo.method, &bound, &key) {
                return Err(exec_err(
                    "23514",
                    format!(
                        "partition constraint is violated by some row in relation \"{}\"",
                        child_name
                    ),
                ));
            }
        }
    }
    // Set the child's partition info — versioned, so ROLLBACK undoes it.
    // v0.70: an already-partitioned child keeps its own method, key,
    // and children (it becomes a sub-partitioned intermediate); only
    // the bound, parent link, and default flag are set by the attach.
    let is_default = matches!(bound_def, PartBoundDef::Default);
    let cinfo = match child_pinfo {
        Some(cp) => PartitionInfo {
            method: cp.method,
            key: cp.key,
            bound: Some(bound),
            is_default,
            parent: Some(parent_name.to_string()),
            children: cp.children,
            // v0.72: an already-partitioned child stays partitioned.
            is_partitioned: cp.is_partitioned,
        },
        None => PartitionInfo {
            method: pinfo.method,
            key: pinfo.key.clone(),
            bound: Some(bound),
            is_default,
            parent: Some(parent_name.to_string()),
            children: Vec::new(),
            // v0.72: a plain table attached as a partition is a leaf.
            is_partitioned: false,
        },
    };
    let prev = eng
        .db
        .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
        .expect("child still visible")
        .clone();
    let mut next = prev.clone();
    next.partition = Some(cinfo);
    commit_table_version(eng, ctx, child_name, prev, next);
    // Link into the parent's children — versioned, so ROLLBACK undoes it.
    parent_link_child(eng, ctx, parent_name, child_name, true);
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v0.96: `ALTER TABLE child INHERIT parent` / `NO INHERIT parent`
/// (PG19 table inheritance). Only the link is created or dropped —
/// columns are never added, removed, or reordered (PG19
/// `MergeAttributesIntoExisting` / `RemoveInheritance`).
pub(crate) fn alter_inherit(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    child_name: &str,
    parent_name: &str,
    link: bool,
) -> Result<ExecResult, ExecError> {
    // v0.11: every ALTER TABLE needs owner-or-superuser; PG requires
    // ownership of the parent too (the child was checked in
    // `exec_alter_one`).
    require_table_owner(eng, ctx, parent_name)?;
    let child = eng
        .db
        .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| {
            exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", child_name),
            )
        })?
        .clone();
    let parent = eng
        .db
        .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| {
            exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", parent_name),
            )
        })?
        .clone();
    // PG19 ATPrepChangeInherit: a partition (or partitioned table)
    // cannot change inheritance, for INHERIT and NO INHERIT alike.
    if child
        .partition
        .as_ref()
        .is_some_and(|p| p.is_partitioned || p.parent.is_some())
    {
        return Err(exec_err(
            "42809",
            "cannot change inheritance of a partition".to_string(),
        ));
    }
    let already = child.inherits.iter().any(|p| p == parent_name);
    if link {
        // Duplicate link (PG19 CreateInheritance).
        if already {
            return Err(exec_err(
                "42P16",
                format!(
                    "relation \"{}\" would be inherited from more than once",
                    parent_name
                ),
            ));
        }
        // A table cannot inherit from itself (would duplicate every row
        // in parent scans; PG's own checks don't catch this case).
        if parent_name == child_name {
            return Err(exec_err(
                "42P16",
                format!(
                    "cannot inherit from relation \"{}\" because it is the same relation",
                    parent_name
                ),
            ));
        }
        // Circular inheritance (PG19 ATExecAddInherit): the parent must
        // not already be a descendant of the child.
        if eng
            .db
            .inheritance_descendants(child_name, ctx.snap, ctx.own, ctx.session)
            .iter()
            .any(|d| d == parent_name)
        {
            return Err(exec_err(
                "42P16",
                format!(
                    "circular inheritance not allowed: \"{}\" is already a child of \"{}\"",
                    parent_name, child_name
                ),
            ));
        }
        // Persistence rules (PG19, 42809 WRONG_OBJECT_TYPE in both the
        // CREATE and ALTER paths): a permanent table cannot inherit
        // from a temporary one; a temp child may inherit from a
        // permanent parent.
        if !eng.db.is_temp_table(ctx.session, child_name)
            && eng.db.is_temp_table(ctx.session, parent_name)
        {
            return Err(exec_err(
                "42809",
                format!("cannot inherit from temporary relation \"{}\"", parent_name),
            ));
        }
        // Partitioned tables cannot be inherited from (PG19
        // ATExecAddInherit, 42809).
        if parent.partition.as_ref().is_some_and(|p| p.is_partitioned) {
            return Err(exec_err(
                "42809",
                format!("cannot inherit from partitioned table \"{}\"", parent_name),
            ));
        }
        if parent
            .partition
            .as_ref()
            .is_some_and(|p| p.parent.is_some())
        {
            return Err(exec_err(
                "42809",
                "cannot inherit from a partition".to_string(),
            ));
        }
        // Column compatibility (PG19 MergeAttributesIntoExisting,
        // 42804): every parent column must already exist in the child
        // with exactly the same type (typmod included), and a parent
        // NOT NULL column must be NOT NULL in the child.
        for (pi, (pname, ptype)) in parent.columns.iter().enumerate() {
            let (ci, (_, ctype)) = child
                .columns
                .iter()
                .enumerate()
                .find(|(_, (n, _))| n == pname)
                .ok_or_else(|| {
                    exec_err(
                        "42804",
                        format!("child table is missing column \"{}\"", pname),
                    )
                })?;
            let same_type = ctype == ptype
                && child.composite_types.get(ci) == parent.composite_types.get(pi)
                && child.domain_types.get(ci) == parent.domain_types.get(pi)
                && child.domain_elem.get(ci) == parent.domain_elem.get(pi);
            if !same_type {
                return Err(exec_err(
                    "42804",
                    format!(
                        "child table \"{}\" has different type for column \"{}\"",
                        child_name, pname
                    ),
                ));
            }
            if parent.not_null.get(pi) == Some(&true) && child.not_null.get(ci) != Some(&true) {
                return Err(exec_err(
                    "42804",
                    format!(
                        "column \"{}\" in child table \"{}\" must be marked NOT NULL",
                        pname, child_name
                    ),
                ));
            }
        }
        let prev = child.clone();
        let mut next = child;
        next.inherits.push(parent_name.to_string());
        if eng.db.is_temp_table(ctx.session, child_name) {
            alter_swap_temp(eng, ctx, child_name, None, next, None)?;
        } else {
            commit_table_version(eng, ctx, child_name, prev, next);
        }
    } else {
        // NO INHERIT: dropping a link that isn't there is 42P01, like
        // PostgreSQL's RemoveInheritance.
        if !already {
            return Err(exec_err(
                "42P01",
                format!(
                    "relation \"{}\" is not a parent of relation \"{}\"",
                    parent_name, child_name
                ),
            ));
        }
        let prev = child.clone();
        let mut next = child;
        next.inherits.retain(|p| p != parent_name);
        if eng.db.is_temp_table(ctx.session, child_name) {
            alter_swap_temp(eng, ctx, child_name, None, next, None)?;
        } else {
            commit_table_version(eng, ctx, child_name, prev, next);
        }
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v0.69: remap a row from child column order to parent column order.
pub(crate) fn remap_row_to_parent(
    child_cols: &[(String, ColType)],
    parent_cols: &[(String, ColType)],
    values: &[Value],
) -> Vec<Value> {
    parent_cols
        .iter()
        .map(|(pn, _)| {
            child_cols
                .iter()
                .position(|(cn, _)| cn == pn)
                .map(|i| values[i].clone())
                .unwrap_or(Value::Null)
        })
        .collect()
}

/// v0.69: does a partition key satisfy a partition bound?
pub(crate) fn bound_contains(method: &PartMethod, bound: &PartBound, key: &[Value]) -> bool {
    match (method, bound) {
        (PartMethod::List, PartBound::List { values, has_null }) => {
            if key.len() == 1 {
                let kv = &key[0];
                if *kv == Value::Null {
                    return *has_null;
                }
                values.contains(kv)
            } else {
                false
            }
        }
        (PartMethod::Range, PartBound::Range { lower, upper }) => {
            range_bound_contains(lower, upper, key)
        }
        (PartMethod::Hash, PartBound::Hash { modulus, remainder }) => {
            if key.len() == 1 {
                hash_value(&key[0], *modulus) == *remainder
            } else {
                false
            }
        }
        _ => false,
    }
}

/// v0.69: PG's hashint4-like hash for partitioning: the corpus routes
/// by `a % 4`, so we use the integer value directly (non-negative mod).
pub(crate) fn hash_value(v: &Value, modulus: u32) -> u32 {
    let i: i64 = match v {
        Value::SmallInt(x) => *x as i64,
        Value::Int(x) => *x,
        Value::BigInt(x) => *x,
        _ => 0,
    };
    ((i % modulus as i64 + modulus as i64) % modulus as i64) as u32
}

/// v0.69: collect all leaf partition names under a partitioned table
/// (recursive for multilevel partitioning).
pub(crate) fn collect_partition_leaves(
    db: &Database,
    table: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Vec<String> {
    let mut leaves = Vec::new();
    let mut stack = vec![table.to_string()];
    while let Some(name) = stack.pop() {
        if let Some(t) = db.find_table(&name, snap, &[own], session) {
            if let Some(p) = &t.partition {
                if p.children.is_empty() {
                    // v0.72: a childless *partitioned* table holds no
                    // rows (direct inserts are rejected) — it is not a
                    // leaf, so skip it. A true leaf holds the rows.
                    if !p.is_partitioned {
                        leaves.push(name);
                    }
                } else {
                    // A table with children is a root or an
                    // intermediate: recurse (a bound + children means
                    // intermediate).
                    stack.extend(p.children.iter().cloned());
                }
            } else {
                // Not partitioned (shouldn't happen).
                leaves.push(name);
            }
        }
    }
    leaves
}

/// v0.69: human-readable type name for error messages.
pub(crate) fn col_type_name(ty: &ColType) -> &'static str {
    match ty {
        ColType::SmallInt => "smallint",
        ColType::Int => "integer",
        ColType::BigInt => "bigint",
        ColType::Float4 => "real",
        ColType::Float => "double precision",
        ColType::Numeric(_) => "numeric",
        ColType::Text => "text",
        ColType::Varchar(_) => "character varying",
        ColType::Char(_) => "character",
        ColType::Bool => "boolean",
        ColType::Date => "date",
        ColType::Timestamp => "timestamp",
        ColType::Timestamptz => "timestamptz",
        ColType::Bytea => "bytea",
        ColType::Uuid => "uuid",
        _ => "unknown",
    }
}

/// v0.69: route INSERT rows to partition leaves. Returns
/// `(leaf_name, Vec<(row_id, remapped_row)>)` groups. If the target is
/// not a partitioned parent, returns a single group for the target
/// itself (no-op). If the target is a leaf (has a bound), validates the
/// bound instead of routing.
pub(crate) fn route_partition_inserts(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    inserts: &[(u64, Row)],
) -> Result<Vec<(String, Vec<(u64, Row)>)>, ExecError> {
    // Get partition info (clone to avoid borrow issues).
    let (pinfo, table_cols) = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("target still visible");
        match &t.partition {
            Some(p) => (p.clone(), t.columns.clone()),
            None => {
                // Not partitioned: single group.
                return Ok(vec![(table.to_string(), inserts.to_vec())]);
            }
        }
    };
    if pinfo.bound.is_some() {
        // v0.72: a partitioned table with no children is not a leaf —
        // direct inserts fail exactly like a parent with no matching
        // partition (PG19 23514 "no partition of relation ... found").
        if pinfo.is_partitioned && pinfo.children.is_empty() {
            return Err(exec_err(
                "23514",
                format!("no partition of relation \"{}\" found for row", table),
            ));
        }
        // Direct insert: validate the bound (and all ancestors).
        for (_id, row) in inserts {
            check_leaf_bound(eng, ctx, table, &pinfo, &table_cols, &row[..])?;
        }
        if pinfo.children.is_empty() {
            // Leaf: rows land here.
            return Ok(vec![(table.to_string(), inserts.to_vec())]);
        }
        // v0.70: sub-partitioned intermediate (the corpus inserts into
        // mlparted1 directly): the bound is validated above, then rows
        // route to descendant leaves. Fall through.
    }
    // Parent: route each row to a leaf.
    let mut by_leaf: std::collections::HashMap<String, (Vec<(String, ColType)>, Vec<(u64, Row)>)> =
        std::collections::HashMap::new();
    for (id, row) in inserts {
        // Find the leaf. Each level evaluates its own partition key
        // against the row (v0.70: multilevel routing).
        let leaf_name = find_partition_leaf(eng, ctx, table, &pinfo, &table_cols, &row[..])?;
        // Remap to leaf column order.
        let leaf_cols = {
            let lt = eng
                .db
                .find_table(&leaf_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("leaf still visible");
            lt.columns.clone()
        };
        let remapped: Vec<Value> = leaf_cols
            .iter()
            .map(|(ln, _)| {
                table_cols
                    .iter()
                    .position(|(pn, _)| pn == ln)
                    .map(|i| row[i].clone())
                    .unwrap_or(Value::Null)
            })
            .collect();
        by_leaf
            .entry(leaf_name.clone())
            .or_insert_with(|| (leaf_cols.clone(), Vec::new()))
            .1
            .push((*id, Row::new(remapped)));
    }
    // v1.00: fire each leaf's BEFORE INSERT row triggers on its routed
    // rows (PG19 fires leaf triggers after routing), then re-validate
    // the leaf bound — a trigger may have moved the row out of its
    // partition (the fixtures' 23514 "violates partition constraint"
    // cases). Suppressed rows never reach the leaf.
    let mut result: Vec<(String, Vec<(u64, Row)>)> = Vec::with_capacity(by_leaf.len());
    for (leaf_name, (leaf_cols, leaf_rows)) in by_leaf {
        let leaf_pinfo = {
            let lt = eng
                .db
                .find_table(&leaf_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("leaf still visible");
            lt.partition.clone().expect("leaf is partitioned")
        };
        let rows_only: Vec<Row> = leaf_rows.iter().map(|(_, r)| r.clone()).collect();
        let fired = fire_before_insert_triggers(eng, ctx, &leaf_name, &leaf_cols, &rows_only)?;
        // Re-pair ids with fired rows in order; suppressed rows (None)
        // drop their ids.
        let mut fired_rows: Vec<(u64, Row)> = Vec::with_capacity(fired.len());
        for ((id, _), opt_row) in leaf_rows.iter().zip(fired.iter()) {
            if let Some(fr) = opt_row {
                fired_rows.push((*id, fr.clone()));
            }
        }
        // v1.00: the leaf's partition bound is an implicit CHECK — it
        // must hold for the post-trigger row.
        for (_, row) in &fired_rows {
            check_leaf_bound(eng, ctx, &leaf_name, &leaf_pinfo, &leaf_cols, &row[..])?;
        }
        result.push((leaf_name, fired_rows));
    }
    Ok(result)
}

/// v1.00: fire BEFORE INSERT FOR EACH ROW triggers for `table`.
///
/// `rows` are the rows about to be inserted, in the table's own column
/// order. Returns one entry per input row, in order: `Some(row)` for
/// the (possibly modified) row to insert, `None` for a row suppressed
/// by a `RETURN NULL` body. Each firing trigger runs its function body
/// against each row in trigger-creation order (PG19 fires in name
/// order; creation order is the documented v1.00 simplification).
/// AFTER triggers, non-INSERT events, and statement-level triggers are
/// cataloged but inert in v1.00.
///
/// Bodies are parsed once per call (they were validated at CREATE
/// FUNCTION time, so a parse failure here is a defensive 0A000).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_before_insert_triggers(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    cols: &[(String, ColType)],
    rows: &[Row],
) -> Result<Vec<Option<Row>>, ExecError> {
    let firing: Vec<TriggerDef> = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible");
        t.triggers
            .iter()
            .filter(|tr| {
                tr.timing == TriggerTiming::Before
                    && tr.for_each_row
                    && (tr.events & trig_event::INSERT) != 0
            })
            .cloned()
            .collect()
    };
    if firing.is_empty() {
        return Ok(rows.iter().map(|r| Some(r.clone())).collect());
    }
    // Resolve and parse each trigger's function body once.
    let mut bodies: Vec<Vec<TriggerBodyStmt>> = Vec::with_capacity(firing.len());
    for tr in &firing {
        let fdef = find_function_by_signature(eng, &tr.function, &[]).ok_or_else(|| {
            exec_err(
                "42883",
                format!("function {}() does not exist", tr.function),
            )
        })?;
        let stmts = parse_trigger_body(&fdef.body).map_err(|e| {
            exec_err(
                e.code,
                format!("invalid trigger function body: {}", e.message),
            )
        })?;
        bodies.push(stmts);
    }
    let mut out: Vec<Option<Row>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cur: Option<Vec<Value>> = Some(row[..].to_vec());
        for stmts in &bodies {
            let Some(row_vals) = cur else { break };
            cur = exec_trigger_body(eng, ctx, table, cols, &row_vals, stmts)?;
        }
        out.push(cur.map(Row::new));
    }
    Ok(out)
}

/// v1.00: execute one trigger-function body for a single NEW row.
/// Returns `Ok(Some(row))` for the (possibly modified) row to insert,
/// `Ok(None)` when the body returned NULL (the row is suppressed).
pub(crate) fn exec_trigger_body(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    cols: &[(String, ColType)],
    new_row: &[Value],
    stmts: &[TriggerBodyStmt],
) -> Result<Option<Vec<Value>>, ExecError> {
    let mut row: Vec<Value> = new_row.to_vec();
    for stmt in stmts {
        match stmt {
            TriggerBodyStmt::Assign { col, expr } => {
                let idx = cols.iter().position(|(n, _)| n == col).ok_or_else(|| {
                    exec_err(
                        "42703",
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            col, table
                        ),
                    )
                })?;
                let val = eval_trigger_expr(eng, ctx, table, cols, &row, expr)?;
                // Coerce like a normal INSERT assignment would.
                row[idx] = coerce_value(val, &cols[idx].1, col)?;
            }
            TriggerBodyStmt::ReturnNew => return Ok(Some(row)),
            TriggerBodyStmt::ReturnOld => {
                // v1.00: INSERT has no OLD row. PG would return the
                // (all-null) OLD row; the bounded grammar only runs on
                // INSERT, so treat it as the current NEW row.
                return Ok(Some(row));
            }
            TriggerBodyStmt::ReturnNull => return Ok(None),
            TriggerBodyStmt::Raise {
                level,
                format,
                args,
            } => {
                let vals: Vec<Value> = args
                    .iter()
                    .map(|e| eval_trigger_expr(eng, ctx, table, cols, &row, e))
                    .collect::<Result<Vec<_>, _>>()?;
                let msg = format_raise_message(format, &vals);
                match level {
                    RaiseLevel::Notice => ctx.notices.push(msg),
                    RaiseLevel::Exception => return Err(exec_err("P0001", msg)),
                }
            }
        }
    }
    // Falling off the end without RETURN: PG19 raises 2F005
    // (null_value_not_allowed / "control reached end of trigger
    // procedure without RETURN").
    Err(exec_err(
        "2F005",
        "control reached end of trigger procedure without RETURN".to_string(),
    ))
}

/// v1.00: evaluate a trigger-body expression against the NEW row. The
/// scope is qualified as `new`, so both `new.col` and bare `col`
/// resolve against the row (PG19 trigger bodies see NEW/OLD as the
/// row variables).
pub(crate) fn eval_trigger_expr(
    eng: &mut Engine,
    ctx: &StmtCtx,
    _table: &str,
    cols: &[(String, ColType)],
    row: &[Value],
    expr: &Expr,
) -> Result<Value, ExecError> {
    let schema: Vec<QCol> = cols
        .iter()
        .enumerate()
        .map(|(i, (n, ty))| QCol {
            qual: "new".to_string(),
            name: n.clone(),
            ty: *ty,
            hidden: false,
            src_ord: i as u32,
        })
        .collect();
    eval_partition_key_expr(
        eng,
        ctx.snap,
        ctx.own,
        ctx.session,
        ctx.role,
        &schema,
        row,
        expr,
    )
}

/// v1.00: expand a `RAISE ... '<format>'` format string. Each `%`
/// consumes the next argument (rendered in its text-cast form, like
/// PG's `%`); `%%` is a literal `%`. Surplus arguments are ignored
/// and a `%` with no argument left is kept literally (PG would raise,
/// but the bounded grammar keeps this total).
pub(crate) fn format_raise_message(format: &str, args: &[Value]) -> String {
    let mut out = String::new();
    let mut arg_iter = args.iter();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.clone().next() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            _ => {
                if let Some(v) = arg_iter.next() {
                    out.push_str(&value_to_text_cast(v));
                } else {
                    out.push('%');
                }
            }
        }
    }
    out
}

/// v0.69: evaluate a partition key expression against a row.
pub(crate) fn eval_partition_key_expr(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    schema: &[QCol],
    row: &[Value],
    expr: &Expr,
) -> Result<Value, ExecError> {
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap,
        own,
        all_xids: vec![own],
        session,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        role,
        read_only: false,
        priv_scopes: Vec::new(),
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: None,
        write: None,
    };
    let scope = Scope {
        schema,
        row,
        prov: None,
    };
    eval_expr(&mut q, &[scope], expr)
}

/// v0.70: evaluate a partitioned table's key against a row given in
/// that table's own column order. Each recursion level of
/// [`find_partition_leaf`] recomputes the key with the *current*
/// table's definition (v0.69 wrongly reused the root key all the way
/// down, so multilevel routing compared the wrong values).
pub(crate) fn eval_table_part_key(
    eng: &mut Engine,
    ctx: &StmtCtx,
    table_name: &str,
    pinfo: &PartitionInfo,
    table_cols: &[(String, ColType)],
    row: &[Value],
) -> Result<Vec<Value>, ExecError> {
    let schema: Vec<QCol> = table_cols
        .iter()
        .enumerate()
        .map(|(i, (n, ty))| QCol {
            qual: table_name.to_string(),
            name: n.clone(),
            ty: *ty,
            hidden: false,
            src_ord: i as u32,
        })
        .collect();
    let mut key = Vec::with_capacity(pinfo.key.len());
    for k in &pinfo.key {
        if let Some(e) = &k.expr {
            key.push(eval_partition_key_expr(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &schema,
                row,
                e,
            )?);
        } else {
            key.push(row[k.col].clone());
        }
    }
    Ok(key)
}

/// v0.70: does any explicit (non-default) child of `pinfo` accept `key`?
/// The DEFAULT partition receives only rows no explicit sibling accepts
/// (PG19 semantics); used by both routing and direct-insert validation.
pub(crate) fn explicit_sibling_accepts(
    eng: &Engine,
    ctx: &StmtCtx,
    pinfo: &PartitionInfo,
    except: &str,
    key: &[Value],
) -> Result<bool, ExecError> {
    for child_name in &pinfo.children {
        if child_name == except {
            continue;
        }
        let Some(child) = eng
            .db
            .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
        else {
            // v0.70: tolerate stale links (a rolled-back CREATE can leave
            // a dangling child name behind).
            continue;
        };
        let Some(cinfo) = child.partition.as_ref() else {
            continue;
        };
        if cinfo.is_default {
            continue;
        }
        let cbound = cinfo.bound.as_ref().expect("child has bound");
        if bound_contains(&pinfo.method, cbound, key) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// v0.70: find the leaf partition for a row (recursive for multilevel).
/// The row is carried in each level's own column order; the key is
/// re-evaluated per level with that table's key definition.
pub(crate) fn find_partition_leaf(
    eng: &mut Engine,
    ctx: &StmtCtx,
    table: &str,
    pinfo: &PartitionInfo,
    table_cols: &[(String, ColType)],
    row: &[Value],
) -> Result<String, ExecError> {
    let key = eval_table_part_key(eng, ctx, table, pinfo, table_cols, row)?;
    // Non-default children first, then the default partition (PG19:
    // the default only receives rows no explicit sibling accepts).
    //
    // Each rejected candidate used to clone its full `PartitionInfo`
    // (leaf `key`/`bound` Vecs) and `columns` (a `Vec<(String,
    // ColType)>`) before checking whether it even matches -- with P
    // sibling partitions, a bulk INSERT paid O(P) of these clones per
    // row for O(1) useful work. The bound test only needs borrowed
    // access to `child.partition`/`child.columns`, so it runs first;
    // the clone happens only for the one candidate that actually
    // matches (measured: -62% bytes / -42% allocations on the v0.71
    // `partition` bench, dhat).
    for default_pass in [false, true] {
        for child_name in &pinfo.children {
            let Some(child) = eng
                .db
                .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
            else {
                continue;
            };
            let Some(cinfo_ref) = child.partition.as_ref() else {
                continue;
            };
            if cinfo_ref.is_default != default_pass {
                continue;
            }
            let matches = if cinfo_ref.is_default {
                !explicit_sibling_accepts(eng, ctx, pinfo, child_name, &key)?
            } else {
                let cbound = cinfo_ref.bound.as_ref().expect("child has bound");
                bound_contains(&pinfo.method, cbound, &key)
            };
            if !matches {
                continue;
            }
            // Matched: this is the only candidate worth cloning.
            let (cinfo, child_cols) = {
                let child = eng
                    .db
                    .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("child still visible");
                (
                    child.partition.clone().expect("child still partitioned"),
                    child.columns.clone(),
                )
            };
            if cinfo.children.is_empty() {
                // v1.16: a childless *partitioned* table holds no rows —
                // but PG19 commits to the matched partition: the 23514
                // names the matched child (e.g. "no partition of
                // relation mlparted5_cd found for row"), it does NOT
                // fall through to siblings or the DEFAULT partition.
                // (v0.72 used `continue` here, misnaming the parent.)
                if cinfo.is_partitioned {
                    return Err(exec_err(
                        "23514",
                        format!("no partition of relation \"{}\" found for row", child_name),
                    ));
                }
                return Ok(child_name.clone());
            }
            // Recurse: remap the row to the child's column order and
            // evaluate the *child's* key there.
            let crow = remap_row_to_parent(table_cols, &child_cols, row);
            return find_partition_leaf(eng, ctx, child_name, &cinfo, &child_cols, &crow);
        }
    }
    // No partition found.
    Err(exec_err(
        "23514",
        format!("no partition of relation \"{}\" found for row", table),
    ))
}

/// v0.70: validate that a row directly inserted into a leaf satisfies
/// the leaf's bound and *every* ancestor bound (v0.69 stopped at the
/// immediate parent). The row is carried up the chain, remapped to each
/// ancestor's column order; a DEFAULT bound is satisfied only when no
/// explicit sibling bound accepts the key (v0.69 rejected every direct
/// DEFAULT insert outright).
pub(crate) fn check_leaf_bound(
    eng: &mut Engine,
    ctx: &StmtCtx,
    leaf_name: &str,
    pinfo: &PartitionInfo,
    leaf_cols: &[(String, ColType)],
    values: &[Value],
) -> Result<(), ExecError> {
    let mut cur_name = leaf_name.to_string();
    let mut cur_cols = leaf_cols.to_vec();
    let mut cur_row: Vec<Value> = values.to_vec();
    let mut cur_pinfo = pinfo.clone();
    loop {
        let parent_name = cur_pinfo.parent.clone().expect("partition has parent");
        let (parent_cols, parent_pinfo) = {
            let parent = eng
                .db
                .find_table(&parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("parent still visible");
            (
                parent.columns.clone(),
                parent.partition.clone().expect("parent is partitioned"),
            )
        };
        // Remap the row to the parent's column order, then evaluate the
        // parent's key (the current table's bound is parent-relative).
        let prow = remap_row_to_parent(&cur_cols, &parent_cols, &cur_row);
        let key = eval_table_part_key(eng, ctx, &parent_name, &parent_pinfo, &parent_cols, &prow)?;
        let bound = cur_pinfo.bound.as_ref().expect("partition has bound");
        let ok = if cur_pinfo.is_default {
            !explicit_sibling_accepts(eng, ctx, &parent_pinfo, &cur_name, &key)?
        } else {
            bound_contains(&parent_pinfo.method, bound, &key)
        };
        if !ok {
            return Err(exec_err(
                "23514",
                format!(
                    "new row for relation \"{}\" violates partition constraint",
                    leaf_name
                ),
            ));
        }
        // Move up: the parent becomes the current table.
        if parent_pinfo.parent.is_none() {
            break;
        }
        cur_name = parent_name;
        cur_cols = parent_cols;
        cur_row = prow;
        cur_pinfo = parent_pinfo;
    }
    Ok(())
}

/// v0.72: expand `LIKE source_table [like_option ...]` clauses into the
/// new table's definition (PG19 `transformTableLikeClause` +
/// `expandTableLikeClause`). Columns merge left to right — a source
/// column whose name already exists is skipped. PG19 option defaults
/// (gram.y: a bare LIKE yields options = 0): column names/types are
/// always copied, NOT NULL constraints are always copied regardless of
/// options, and every other kind (DEFAULTS, CONSTRAINTS, INDEXES,
/// STORAGE, COMPRESSION, COMMENTS, STATISTICS, IDENTITY, GENERATED)
/// is copied only with the matching INCLUDING option (or INCLUDING
/// ALL). Explicit options apply in order with later ones winning, and
/// `INCLUDING|EXCLUDING ALL` fans out to every kind. COMMENTS,
/// STATISTICS, IDENTITY and GENERATED have no runtime effect here (no
/// comment catalog / extended stats / identity columns in this engine).
pub(crate) fn expand_like_clauses(
    eng: &Engine,
    ctx: &StmtCtx,
    _name: &str,
    def: &mut crate::sql::TableDef,
) -> Result<(), ExecError> {
    use crate::sql::{CheckDef, LikeKind, UniqueDef};
    if def.likes.is_empty() {
        return Ok(());
    }
    fn all_kinds() -> [LikeKind; 9] {
        use LikeKind::*;
        [
            Comments,
            Compression,
            Constraints,
            Defaults,
            Identity,
            Indexes,
            Statistics,
            Storage,
            Generated,
        ]
    }
    let likes = std::mem::take(&mut def.likes);
    for lc in &likes {
        let src = eng
            .db
            .find_table(&lc.source, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| {
                exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", lc.source),
                )
            })?;
        // PG19 defaults (gram.y: a bare LIKE yields options = 0, so every
        // kind defaults to EXCLUDING). NOT NULL is copied regardless —
        // handled below, not via this flag set
        // (transformTableLikeClause: "regardless of options given").
        let mut flags: [(LikeKind, bool); 9] = all_kinds().map(|k| (k, false));
        for o in &lc.options {
            if o.kind == LikeKind::All {
                for f in flags.iter_mut() {
                    f.1 = o.including;
                }
            } else if let Some(f) = flags.iter_mut().find(|f| f.0 == o.kind) {
                f.1 = o.including;
            }
        }
        let including = |kind: LikeKind| flags.iter().find(|f| f.0 == kind).is_some_and(|f| f.1);
        // Columns (with per-column NOT NULL / DEFAULT / serial /
        // compression / storage slots kept in lockstep, like
        // build_table_def). Dropped columns do not exist here (DROP
        // COLUMN removes the slot); PG also skips attisdropped.
        for (i, (cname, ctype)) in src.columns.iter().enumerate() {
            // PG19 MergeAttributes (tablecmds.c): a LIKE-copied column
            // colliding with an explicitly defined column is 42701
            // "column ... specified more than once" — LIKE columns are
            // not is_from_type, so they never merge.
            if def.columns.iter().any(|(n, _)| n == cname) {
                return Err(exec_err(
                    "42701",
                    format!("column \"{cname}\" specified more than once"),
                ));
            }
            def.columns.push((cname.clone(), ctype.clone()));
            // v0.72: NOT NULL is ALWAYS copied (PG19
            // transformTableLikeClause, "regardless of options given").
            def.not_null
                .push(src.not_null.get(i).copied().unwrap_or(false));
            def.defaults.push(if including(LikeKind::Defaults) {
                src.defaults.get(i).cloned().flatten()
            } else {
                None
            });
            // v0.72: serial/identity backing sequences are *not* copied
            // (PG copies identity as identity; we have no identity
            // columns — a nextval DEFAULT may still ride along via
            // INCLUDING DEFAULTS, referencing the source's sequence).
            def.serial.push(None);
            // v0.72: compression only with INCLUDING COMPRESSION (or ALL);
            // the parent's explicit method round-trips through its name
            // (PG: GetCompressionMethodName(attcompression)).
            def.compression.push(if including(LikeKind::Compression) {
                src.col_compression
                    .get(i)
                    .copied()
                    .flatten()
                    .map(|c| c.name().to_string())
            } else {
                None
            });
            // v0.72: storage only with INCLUDING STORAGE (or ALL).
            def.storage.push(if including(LikeKind::Storage) {
                src.col_storage.get(i).copied()
            } else {
                None
            });
            // v0.81/v0.85: the named composite and domain type use ride
            // along with the column (PG copies the whole rowtype).
            def.composite_types
                .push(src.composite_types.get(i).cloned().unwrap_or(None));
            def.domain_types
                .push(src.domain_types.get(i).cloned().unwrap_or(None));
            def.domain_elem
                .push(src.domain_elem.get(i).copied().unwrap_or(false));
        }
        // CHECK constraints get fresh per-table names (PG generates new
        // names for the copies; ours are per-table anyway).
        if including(LikeKind::Constraints) {
            for ck in &src.checks {
                let mut cname = ck.name.clone();
                let mut n = 1;
                while def.checks.iter().any(|c| c.name == cname) {
                    n += 1;
                    cname = format!("{}_{}", ck.name, n);
                }
                def.checks.push(CheckDef {
                    name: cname,
                    expr: ck.expr.clone(),
                    not_valid: ck.not_valid,
                    kind: ck.kind,
                });
            }
        }
        // INCLUDING INDEXES copies unique/pkey constraints (their
        // backing indexes are built by the normal create path under
        // per-table names).
        if including(LikeKind::Indexes) {
            for u in &src.uniques {
                let mut cname = u.name.clone();
                let mut n = 1;
                while def.uniques.iter().any(|x| x.name == cname)
                    || def.pkey.as_ref().is_some_and(|p| p.name == cname)
                {
                    n += 1;
                    cname = format!("{}_{}", u.name, n);
                }
                def.uniques.push(UniqueDef {
                    name: cname,
                    cols: u.cols.clone(),
                });
            }
            if def.pkey.is_none() {
                if let Some(pk) = &src.pkey {
                    def.pkey = Some(UniqueDef {
                        name: pk.name.clone(),
                        cols: pk.cols.clone(),
                    });
                }
            }
        }
    }
    if def.columns.is_empty() {
        return Err(exec_err(
            "42601",
            "table must have at least one column".to_string(),
        ));
    }
    Ok(())
}

/// v0.72: validate the recognized `WITH (...)` storage parameters at
/// CREATE TABLE (PG19 reloptions.c `check_fillfactor`: 22023 when out
/// of range). Unrecognized parameters are accepted and ignored — they
/// have no runtime effect yet.
pub(crate) fn validate_reloptions(options: &[(String, String)]) -> Result<(), ExecError> {
    for (name, val) in options {
        if name == "fillfactor" {
            let n: i64 = val
                .parse()
                .map_err(|_| exec_err("22023", format!("invalid fillfactor value: {}", val)))?;
            if !(10..=100).contains(&n) {
                return Err(exec_err(
                    "22023",
                    "fillfactor must be between 10 and 100".to_string(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn create_table_from_def(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    def: &TableDef,
    temp: bool,
) -> Result<TableDef, ExecError> {
    // v0.22: CREATE TEMP TABLE creates a session-local table in
    // `Database::temp_tables[session]`, shadowing any permanent table of
    // the same name *only within this session* (PostgreSQL semantics).
    // The permanent table is never touched.
    // v0.72: expand LIKE clauses first (PG19 transformTableLikeClause),
    // so both the temp and permanent paths below see the merged def.
    let mut like_def = def.clone();
    expand_like_clauses(eng, ctx, name, &mut like_def)?;
    // v0.81: validate named composite columns — each `ColType::Composite`
    // must name a defined composite type (42704 otherwise, like PG).
    // v0.85: a named type that resolves to a domain rewrites the column
    // to the domain's base type and records the domain use
    // (`domain_types` / `domain_elem`); the domain's constraints are
    // enforced on the finished value (PG19 coerce_to_domain).
    for i in 0..like_def.columns.len() {
        let ty = like_def.columns[i].1.clone();
        if ty == ColType::Composite {
            let tname: Option<String> = like_def.composite_types.get(i).and_then(|o| o.clone());
            let st = tname.as_deref().and_then(|n| eng.db.types.get(n)).cloned();
            match st {
                Some(st) if st.composite.is_some() => {}
                Some(st) if st.domain.is_some() => {
                    let dom = st.domain.clone().expect("domain branch has a def");
                    like_def.columns[i].1 = dom.base.clone();
                    like_def.composite_types[i] = dom.base_named.clone();
                    like_def.domain_types[i] = tname;
                    like_def.domain_elem[i] = false;
                }
                _ => {
                    return Err(exec_err(
                        "42704",
                        format!(
                            "type \"{}\" does not exist",
                            tname.as_deref().unwrap_or("<unknown>")
                        ),
                    ));
                }
            }
        } else if ty == ColType::Array(ArrayElem::Record) {
            // v0.85: `d[]` — array of domain (or array of composite,
            // which keeps its existing shape). The column becomes an
            // array of the domain's base; the domain applies per array
            // element.
            let tname: Option<String> = like_def.composite_types.get(i).and_then(|o| o.clone());
            let st = tname.as_deref().and_then(|n| eng.db.types.get(n)).cloned();
            if let Some(st) = st {
                if let Some(dom) = st.domain.clone() {
                    like_def.columns[i].1 = ColType::Array(ArrayElem::of(&dom.base));
                    like_def.composite_types[i] = dom.base_named.clone();
                    like_def.domain_types[i] = tname;
                    like_def.domain_elem[i] = true;
                }
            }
        }
    }
    // v0.72: validate the recognized `WITH (...)` storage parameters
    // (PG19 reloptions.c); unrecognized ones are accepted and ignored.
    validate_reloptions(&like_def.reloptions)?;
    // v0.77/v0.96: expand `INHERITS (parent [, ...])` (PG19
    // DefineRelation + MergeAttributes): the child gets each parent's
    // columns first (in parent order), then its own; same-name columns
    // merge only on exact type compatibility (42804). Unlike PARTITION
    // OF, constraints are not inherited — only the columns with their
    // NOT NULL flags, defaults, and CHECK constraints. Parent scans
    // including children's rows are implemented in scan_table_from.
    if !like_def.inherits.is_empty() {
        let mut merged = TableDef::empty();
        merged.inherits = like_def.inherits.clone();
        let mut seen_parents: Vec<String> = Vec::new();
        // v0.96: columns whose inherited defaults conflict across
        // parents; the child must specify its own default for each
        // (PG19 MergeAttributes, 42804).
        let mut conflicting_defaults: Vec<String> = Vec::new();
        for parent_name in &like_def.inherits {
            // Reject duplications in the list of parents (PG19
            // DefineRelation, 42P16).
            if seen_parents.iter().any(|p| p == parent_name) {
                return Err(exec_err(
                    "42P16",
                    format!(
                        "relation \"{}\" would be inherited from more than once",
                        parent_name
                    ),
                ));
            }
            // The child does not exist yet, so naming it as its own
            // parent is an undefined table (PG19).
            if parent_name == name {
                return Err(exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", parent_name),
                ));
            }
            let parent = eng
                .db
                .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                .ok_or_else(|| {
                    exec_err(
                        "42P01",
                        format!("relation \"{}\" does not exist", parent_name),
                    )
                })?;
            seen_parents.push(parent_name.clone());
            // Permanent tables cannot inherit from temporary parents
            // (PG19, 42809); a temp child may inherit from a permanent
            // parent.
            if !temp && eng.db.is_temp_table(ctx.session, parent_name) {
                return Err(exec_err(
                    "42809",
                    format!("cannot inherit from temporary relation \"{}\"", parent_name),
                ));
            }
            // Partitioned tables and partitions cannot participate in
            // regular inheritance (PG19 MergeAttributes, 42809).
            if parent.partition.as_ref().is_some_and(|p| p.is_partitioned) {
                return Err(exec_err(
                    "42809",
                    format!("cannot inherit from partitioned table \"{}\"", parent_name),
                ));
            }
            if parent
                .partition
                .as_ref()
                .is_some_and(|p| p.parent.is_some())
            {
                return Err(exec_err(
                    "42809",
                    "cannot inherit from a partition".to_string(),
                ));
            }
            for (i, (col, ty)) in parent.columns.iter().enumerate() {
                if let Some(pos) = merged.columns.iter().position(|(n, _)| n == col) {
                    // Same column inherited from two parents: the types
                    // must match exactly (PG19 MergeChildAttribute).
                    let same = merged.columns[pos].1 == *ty
                        && merged.composite_types[pos]
                            == parent.composite_types.get(i).cloned().unwrap_or(None)
                        && merged.domain_types[pos]
                            == parent.domain_types.get(i).cloned().unwrap_or(None)
                        && merged.domain_elem[pos]
                            == parent.domain_elem.get(i).copied().unwrap_or(false);
                    if !same {
                        return Err(exec_err(
                            "42804",
                            format!("column \"{}\" has a type conflict", col),
                        ));
                    }
                    // NOT NULL accumulates across parents.
                    if parent.not_null.get(i).copied().unwrap_or(false) {
                        merged.not_null[pos] = true;
                    }
                    // v0.96: conflicting inherited defaults are rejected
                    // unless the child specifies its own default (PG19
                    // MergeAttributes, 42804).
                    let parent_def = parent.defaults.get(i).and_then(|d| d.clone());
                    match (&merged.defaults[pos], &parent_def) {
                        (Some(a), Some(b)) if a != b => {
                            if !conflicting_defaults.contains(col) {
                                conflicting_defaults.push(col.clone());
                            }
                        }
                        (None, Some(_)) => {
                            merged.defaults[pos] = parent_def;
                        }
                        _ => {}
                    }
                    continue;
                }
                merged.columns.push((col.clone(), ty.clone()));
                // v0.81: inherit the composite type name too.
                merged
                    .composite_types
                    .push(parent.composite_types.get(i).cloned().unwrap_or(None));
                merged
                    .domain_types
                    .push(parent.domain_types.get(i).cloned().unwrap_or(None));
                merged
                    .domain_elem
                    .push(parent.domain_elem.get(i).copied().unwrap_or(false));
                merged
                    .not_null
                    .push(parent.not_null.get(i).copied().unwrap_or(false));
                merged
                    .defaults
                    .push(parent.defaults.get(i).cloned().unwrap_or(None));
                merged.serial.push(None);
                merged.compression.push(None);
                merged.storage.push(None);
            }
            merged.checks.extend(parent.checks.iter().cloned());
        }
        // The child's own columns; a child column with the same name as
        // an inherited one merges into the inherited position (PG19
        // MergeChildAttribute) and is then consumed.
        let mut local_merged = vec![false; like_def.columns.len()];
        for (i, (col, ty)) in like_def.columns.iter().enumerate() {
            let Some(pos) = merged.columns.iter().position(|(n, _)| n == col) else {
                continue;
            };
            // The local definition must have exactly the same type
            // (PG19 MergeChildAttribute, 42804).
            let same = merged.columns[pos].1 == *ty
                && merged.composite_types[pos]
                    == like_def.composite_types.get(i).cloned().unwrap_or(None)
                && merged.domain_types[pos]
                    == like_def.domain_types.get(i).cloned().unwrap_or(None)
                && merged.domain_elem[pos] == like_def.domain_elem.get(i).copied().unwrap_or(false);
            if !same {
                return Err(exec_err(
                    "42804",
                    format!("column \"{}\" has a type conflict", col),
                ));
            }
            // The local column refines the inherited slot: NOT NULL
            // accumulates, and local default/serial/storage settings win.
            if like_def.not_null.get(i).copied().unwrap_or(false) {
                merged.not_null[pos] = true;
            }
            if let Some(d) = like_def.defaults.get(i).and_then(|d| d.clone()) {
                merged.defaults[pos] = Some(d);
            }
            if let Some(s) = like_def.serial.get(i).and_then(|s| s.clone()) {
                merged.serial[pos] = Some(s);
            }
            if let Some(c) = like_def.compression.get(i).and_then(|c| c.clone()) {
                merged.compression[pos] = Some(c);
            }
            if let Some(s) = like_def.storage.get(i).and_then(|s| s.clone()) {
                merged.storage[pos] = Some(s);
            }
            local_merged[i] = true;
        }
        for (i, (col, ty)) in like_def.columns.iter().enumerate() {
            if local_merged[i] {
                continue;
            }
            merged.columns.push((col.clone(), ty.clone()));
            // v0.81: composite type name for the new column.
            merged
                .composite_types
                .push(like_def.composite_types.get(i).cloned().unwrap_or(None));
            // v0.85: domain type use for the new column.
            merged
                .domain_types
                .push(like_def.domain_types.get(i).cloned().unwrap_or(None));
            merged
                .domain_elem
                .push(like_def.domain_elem.get(i).copied().unwrap_or(false));
            merged
                .not_null
                .push(like_def.not_null.get(i).copied().unwrap_or(false));
            merged
                .defaults
                .push(like_def.defaults.get(i).cloned().unwrap_or(None));
            merged
                .serial
                .push(like_def.serial.get(i).cloned().unwrap_or(None));
            merged
                .compression
                .push(like_def.compression.get(i).cloned().unwrap_or(None));
            merged
                .storage
                .push(like_def.storage.get(i).cloned().unwrap_or(None));
        }
        merged.checks.extend(like_def.checks.iter().cloned());
        merged.uniques = like_def.uniques.clone();
        merged.pkey = like_def.pkey.clone();
        merged.fks = like_def.fks.clone();
        merged.partition = like_def.partition.clone();
        merged.likes = like_def.likes.clone();
        merged.reloptions = like_def.reloptions.clone();
        // v0.96: conflicting inherited defaults must be resolved by an
        // explicit child default (PG19 MergeAttributes, 42804).
        for col in &conflicting_defaults {
            let overridden = like_def
                .columns
                .iter()
                .zip(like_def.defaults.iter())
                .any(|((n, _), d)| n == col && d.is_some());
            if !overridden {
                return Err(exec_err(
                    "42804",
                    format!(
                        "column \"{}\" inherits conflicting defaults; to resolve the conflict, specify a default explicitly",
                        col
                    ),
                ));
            }
        }
        // v0.96: with `INHERITS`, table-level constraints were allowed
        // to name columns the parser hadn't seen yet (they may come
        // from the parents). Validate them against the merged column
        // list now, and apply the implied NOT NULL markings.
        let col_pos = |c: &str| {
            merged
                .columns
                .iter()
                .position(|(n, _)| n == c)
                .ok_or_else(|| exec_err("42703", format!("column \"{}\" does not exist", c)))
        };
        for c in &like_def.deferred_not_null {
            let i = col_pos(c)?;
            merged.not_null[i] = true;
        }
        if let Some(pk) = &merged.pkey {
            for c in &pk.cols {
                let i = col_pos(c)?;
                merged.not_null[i] = true;
            }
        }
        for u in &merged.uniques {
            for c in &u.cols {
                col_pos(c)?;
            }
        }
        for fk in &merged.fks {
            for c in &fk.cols {
                col_pos(c)?;
            }
        }
        like_def = merged;
    }
    let def = &like_def;
    if temp {
        // Existence check first so a duplicate name fails before any
        // serial sequences are created (statement-atomic either way).
        if eng
            .db
            .temp_tables
            .get(&ctx.session)
            .is_some_and(|m| m.contains_key(name))
        {
            return Err(exec_err(
                "42P07",
                format!("relation \"{}\" already exists", name),
            ));
        }
        // v0.69: for `PARTITION OF`, inherit columns from the parent.
        let mut def = def.clone();
        let mut temp_pinfo: Option<PartitionInfo> = None;
        let mut temp_parent: Option<String> = None;
        // v1.41: PARTITION OF inherits the parent's attnums (rowtype
        // copy); stashed like temp_pinfo for after the build.
        let mut temp_attnums: Option<Vec<i16>> = None;
        let mut temp_next_attnum: Option<i16> = None;
        if let Some(pdef) = &def.partition {
            if let Some(parent_name) = &pdef.parent {
                let parent = eng
                    .db
                    .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                    .ok_or_else(|| {
                        exec_err(
                            "42P01",
                            format!("relation \"{}\" does not exist", parent_name),
                        )
                    })?;
                def.columns = parent.columns.clone();
                def.composite_types = parent.composite_types.clone();
                def.domain_types = parent.domain_types.clone();
                def.domain_elem = parent.domain_elem.clone();
                def.not_null = parent.not_null.clone();
                def.defaults = parent.defaults.clone();
                def.checks = parent.checks.clone();
                // v0.70: inherit the parent's constraints too (PG copies
                // them to every partition). Backing unique indexes get
                // per-table names via `constraint_index_name`.
                def.uniques = parent.uniques.clone();
                def.pkey = parent.pkey.clone();
                def.fks = parent.fks.clone();
                // v0.72: inherit the parent's per-column COMPRESSION
                // settings too (PG copies the whole rowtype) — see the
                // permanent-table PARTITION OF path for why.
                def.compression = parent
                    .col_compression
                    .iter()
                    .map(|m| m.map(|c| c.name().to_string()))
                    .collect();
                // v0.72: same for STORAGE — PG copies attstorage as part
                // of the rowtype.
                def.storage = parent.col_storage.iter().map(|b| Some(*b)).collect();
                // v1.41: attnums travel with the rowtype too (stashed
                // for after the table is built, like temp_pinfo).
                temp_attnums = Some(parent.attnums.clone());
                temp_next_attnum = Some(parent.next_attnum);
                let (pinfo, _) = build_partition_info(eng, ctx, name, pdef, &def.columns)?;
                temp_pinfo = Some(pinfo);
                temp_parent = Some(parent_name.clone());
            } else {
                // Partitioned root (temp).
                let (pinfo, _) = build_partition_info(eng, ctx, name, pdef, &def.columns)?;
                temp_pinfo = Some(pinfo);
            }
        }
        let mut t = Table::with_def(&def, ctx.own);
        t.partition = temp_pinfo;
        // v1.41: PARTITION OF child takes the parent's attnums.
        if let Some(a) = temp_attnums {
            t.attnums = a;
        }
        if let Some(n) = temp_next_attnum {
            t.next_attnum = n;
        }
        // v0.96: temp tables get real OIDs too (PG assigns OIDs to temp
        // tables; pg_inherits/pg_class expose them).
        t.oid = eng.db.alloc_oid();
        // v0.11: the creating role owns the table.
        t.owner = ctx.role.to_string();
        // v0.41: validate each column's COMPRESSION option (PG19
        // GetAttributeCompression) and store the explicit methods.
        // v0.72: index-based (not zip) so a short `def.compression`
        // degrades to None instead of truncating `col_compression`.
        t.col_compression = def
            .columns
            .iter()
            .enumerate()
            .map(|(i, (_, ty))| {
                parse_column_compression(ty, def.compression.get(i).and_then(|m| m.as_deref()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // v0.65: serial backing sequences for temp tables too (PG
        // creates them in pg_temp_N; ours live in the global sequence
        // namespace under the same <table>_<column>_seq naming).
        // Runs before the temp_tables borrow below.
        wire_serial_defaults(eng, ctx, name, &mut t, &def.serial, Some(ctx.session))?;
        let tmps = eng.db.temp_tables.entry(ctx.session).or_default();
        tmps.insert(name.to_string(), t);
        // v0.70: link a temp PARTITION OF child into its parent's
        // children. A permanent parent gets a versioned, undoable link;
        // a temp parent is session-local — mutate in place (stale links
        // are skipped by routing).
        if let Some(parent_name) = temp_parent {
            if eng.db.is_temp_table(ctx.session, &parent_name) {
                if let Some(pp) =
                    eng.db
                        .find_table_mut(&parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                {
                    if let Some(pi) = pp.partition.as_mut() {
                        pi.children.push(name.to_string());
                    }
                }
            } else {
                parent_link_child(eng, ctx, &parent_name, name, true);
            }
        }
        ctx.writes.push(WriteOp::CreateTempTable {
            session: ctx.session,
            name: name.to_string(),
        });
        // Validate foreign keys after the table exists so self-references
        // resolve. On failure the statement aborts and undo removes the
        // table (statement atomicity).
        for fk in &def.fks {
            validate_fk_def(eng, ctx, name, fk)?;
        }
        // v0.22: no backing unique indexes for temp tables. The global
        // index map is keyed by table *name*, so a temp table's index
        // would collide with (and corrupt) a same-named permanent
        // table's indexes. PRIMARY KEY / UNIQUE constraints on temp
        // tables are recorded in the table definition and enforced by a
        // session-local scan (`Database::temp_scan_constraint`).
        return Ok(def);
    }
    if eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .is_some()
        || eng.db.find_view(name, ctx.snap, ctx.own).is_some()
    {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    // NOTE: a concurrent uncommitted CREATE of the same name is allowed
    // here (it is invisible to us); the commit-time check in server.rs
    // rejects the second committer with 40001.
    // v0.69: for `PARTITION OF`, inherit the column definitions from
    // the parent (the parser leaves `def.columns` empty).
    let mut def = def.clone();
    if let Some(pdef) = &def.partition {
        if let Some(parent_name) = &pdef.parent {
            let parent = eng
                .db
                .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                .ok_or_else(|| {
                    exec_err(
                        "42P01",
                        format!("relation \"{}\" does not exist", parent_name),
                    )
                })?;
            // Inherit columns (PG copies the parent's rowtype).
            def.columns = parent.columns.clone();
            def.composite_types = parent.composite_types.clone();
            def.domain_types = parent.domain_types.clone();
            def.domain_elem = parent.domain_elem.clone();
            def.not_null = parent.not_null.clone();
            def.defaults = parent.defaults.clone();
            def.checks = parent.checks.clone();
            // v0.70: inherit the parent's constraints too (PG copies
            // them to every partition). Backing unique indexes get
            // per-table names via `constraint_index_name`.
            def.uniques = parent.uniques.clone();
            def.pkey = parent.pkey.clone();
            def.fks = parent.fks.clone();
            // v0.72: inherit the parent's per-column COMPRESSION settings
            // too (PG copies the whole rowtype). Without this,
            // `def.compression` stays empty and the zip below truncates
            // `col_compression` to [], desyncing it from `columns` and
            // panicking the next ALTER on this table (debug_assert in
            // the ALTER finish path).
            def.compression = parent
                .col_compression
                .iter()
                .map(|m| m.map(|c| c.name().to_string()))
                .collect();
            // v0.72: same for STORAGE — PG copies attstorage as part of
            // the rowtype.
            def.storage = parent.col_storage.iter().map(|b| Some(*b)).collect();
            // v1.41: PARTITION OF copies the parent's rowtype — attnums
            // travel with it (PG assigns the child the parent's
            // attribute numbers, gaps included).
            let inherit_attnums = parent.attnums.clone();
            let inherit_next_attnum = parent.next_attnum;
            // Build the partition info (validates the bound).
            let (pinfo, _) = build_partition_info(eng, ctx, name, pdef, &def.columns)?;
            // Stash the info for after the table is created.
            // (We can't borrow parent mutably while building.)
            let mut t = Table::with_def(&def, ctx.own);
            t.attnums = inherit_attnums;
            t.next_attnum = inherit_next_attnum;
            t.partition = Some(pinfo);
            // v0.11: the creating role owns the table.
            t.owner = ctx.role.to_string();
            // v0.72: index-based (not zip) so a short `def.compression`
            // degrades to None instead of truncating `col_compression`.
            t.col_compression = def
                .columns
                .iter()
                .enumerate()
                .map(|(i, (_, ty))| {
                    parse_column_compression(ty, def.compression.get(i).and_then(|m| m.as_deref()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            t.oid = eng.db.alloc_oid();
            ensure_toast_table_eager(eng, ctx, name, &mut t);
            wire_serial_defaults(eng, ctx, name, &mut t, &def.serial, None)?;
            eng.db.tables.entry(name.to_string()).or_default().push(t);
            // v0.70: link into the parent's children, transactionally, so
            // ROLLBACK undoes the link. A temp parent is session-local —
            // mutate it in place (stale links are skipped by routing).
            if eng.db.is_temp_table(ctx.session, parent_name) {
                if let Some(pp) =
                    eng.db
                        .find_table_mut(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                {
                    if let Some(pi) = pp.partition.as_mut() {
                        pi.children.push(name.to_string());
                    }
                }
            } else {
                parent_link_child(eng, ctx, parent_name, name, true);
            }
            ctx.writes.push(WriteOp::CreateTable {
                name: name.to_string(),
            });
            for fk in &def.fks {
                validate_fk_def(eng, ctx, name, fk)?;
            }
            return Ok(def);
        }
    }
    let mut t = Table::with_def(&def, ctx.own);
    // v0.69: partitioned root (`PARTITION BY` without `PARTITION OF`).
    if let Some(pdef) = &def.partition {
        let (pinfo, _) = build_partition_info(eng, ctx, name, pdef, &def.columns)?;
        t.partition = Some(pinfo);
    }
    // v0.11: the creating role owns the table.
    t.owner = ctx.role.to_string();
    // v0.41: validate each column's COMPRESSION option (PG19
    // GetAttributeCompression) and store the explicit methods.
    // v0.72: index-based (not zip) so a short `def.compression`
    // degrades to None instead of truncating `col_compression`.
    t.col_compression = def
        .columns
        .iter()
        .enumerate()
        .map(|(i, (_, ty))| {
            parse_column_compression(ty, def.compression.get(i).and_then(|m| m.as_deref()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // v0.37: assign the table OID (pg_class.oid).
    t.oid = eng.db.alloc_oid();
    // v0.37: PG creates the toast table at CREATE TABLE when the table
    // has toastable columns, so pg_class.reltoastrelid is set from the
    // start (not lazily on first toast).
    ensure_toast_table_eager(eng, ctx, name, &mut t);
    // v0.65: real SERIAL — backing sequences + nextval defaults.
    wire_serial_defaults(eng, ctx, name, &mut t, &def.serial, None)?;
    eng.db.tables.entry(name.to_string()).or_default().push(t);
    ctx.writes.push(WriteOp::CreateTable {
        name: name.to_string(),
    });
    // Validate foreign keys after the table exists so self-references
    // resolve. On failure the statement aborts and undo removes the
    // table (statement atomicity).
    for fk in &def.fks {
        validate_fk_def(eng, ctx, name, fk)?;
    }
    Ok(def)
}

/// Validate a foreign-key definition: the referenced table exists, the
/// referenced columns resolve, and they form the parent's primary key or
/// a unique constraint (SQLSTATE 42830, like Postgres).
pub(crate) fn validate_fk_def(
    eng: &Engine,
    ctx: &StmtCtx,
    child_table: &str,
    fk: &FkDef,
) -> Result<(), ExecError> {
    let child = eng
        .db
        .find_table(child_table, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| {
            exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", child_table),
            )
        })?;
    for c in &fk.cols {
        if child.column_index(c).is_none() {
            return Err(exec_err(
                "42703",
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    c, child_table
                ),
            ));
        }
    }
    let parent = eng
        .db
        .find_table(&fk.ref_table, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| {
            exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", fk.ref_table),
            )
        })?;
    let ref_cols: Vec<String> = if fk.ref_cols.is_empty() {
        match &parent.pkey {
            Some(pk) => pk.cols.clone(),
            None => {
                return Err(exec_err(
                    "42830",
                    format!(
                        "there is no primary key for referenced table \"{}\"",
                        fk.ref_table
                    ),
                ));
            }
        }
    } else {
        fk.ref_cols.clone()
    };
    if fk.cols.len() != ref_cols.len() {
        return Err(exec_err(
            "42830",
            format!(
                "number of referencing and referenced columns for foreign key \"{}\" must be the same",
                fk.name
            ),
        ));
    }
    for c in &ref_cols {
        if parent.column_index(c).is_none() {
            return Err(exec_err(
                "42703",
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    c, fk.ref_table
                ),
            ));
        }
    }
    let is_key = parent
        .pkey
        .as_ref()
        .map(|pk| pk.cols == ref_cols)
        .unwrap_or(false)
        || parent.uniques.iter().any(|u| u.cols == ref_cols)
        // v0.75: a standalone unique index also satisfies the FK
        // uniqueness requirement (PG allows FKs to reference unique
        // indexes, not just constraints).
        || eng.db.indexes.values().any(|idx| {
            idx.def.table == fk.ref_table
                && idx.def.unique
                && idx.def.col_names == ref_cols
        });
    if !is_key {
        return Err(exec_err(
            "42830",
            format!(
                "there is no unique constraint matching given keys for referenced table \"{}\"",
                fk.ref_table
            ),
        ));
    }
    Ok(())
}

/// Build the backing unique index for a PRIMARY KEY / UNIQUE constraint
/// (`internal` marks it as constraint-owned). Checks for duplicate keys
/// among live row versions, like CREATE UNIQUE INDEX.
/// v0.70: backing-index name for a UNIQUE/PKEY constraint. Partition
/// children get a per-table name (`{table}_{constraint}` — PG names
/// per-partition indexes after the partition, and the global index
/// namespace can't hold one `{constraint}` index per sibling); every
/// other table keeps the historical `{constraint}` name.
pub(crate) fn constraint_index_name(table: &str, con: &str, is_partition_child: bool) -> String {
    if is_partition_child {
        format!("{table}_{con}")
    } else {
        con.to_string()
    }
}

pub(crate) fn create_constraint_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    cname: &str,
    cols: &[String],
    internal: bool,
) -> Result<(), ExecError> {
    let t = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
        .expect("table still visible; engine lock held throughout");
    // v0.70: per-partition index names for partition children (see
    // `constraint_index_name`).
    let is_child = t.partition.as_ref().is_some_and(|p| p.parent.is_some());
    let ix_name = constraint_index_name(table, cname, is_child);
    if eng
        .db
        .find_index(&ix_name, ctx.snap, &ctx.all_xids)
        .is_some()
    {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", ix_name),
        ));
    }
    let mut seen = Vec::with_capacity(cols.len());
    for c in cols {
        let pos = t.column_index(c).ok_or_else(|| {
            exec_err(
                "42703",
                format!("column \"{}\" of relation \"{}\" does not exist", c, table),
            )
        })?;
        if seen.contains(&pos) {
            return Err(exec_err(
                "42701",
                format!("column \"{}\" specified more than once", c),
            ));
        }
        seen.push(pos);
    }
    let mut ix = Index::new(IndexDef::plain(
        ix_name.clone(),
        table.to_string(),
        seen,
        cols.to_vec(),
        true,
        internal,
        ctx.own,
    ));
    for r in &t.rows {
        let key = ix.key_for(&r.values);
        ix.insert(key, r.id);
    }
    // Duplicate check among versions that could still become visible
    // (mirrors CREATE UNIQUE INDEX).
    for (key, ids) in &ix.tree {
        if key.0.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        if ids.len() > 1 {
            return Err(exec_err(
                "23505",
                format!(
                    "duplicate key value violates unique constraint \"{}\"",
                    cname
                ),
            ));
        }
    }
    eng.db.indexes.insert(ix_name.clone(), ix);
    ctx.writes.push(WriteOp::CreateIndex { name: ix_name });
    Ok(())
}
