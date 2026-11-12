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
        // v2.23: PG19 parse_utilcmd.c — a hash-partitioned table
        // may not have a default partition (42P16).
        if pinfo.method == crate::storage::PartMethod::Hash
            && matches!(bound_def, PartBoundDef::Default)
        {
            return Err(exec_err(
                "42P16",
                "a hash-partitioned table may not have a default partition".to_string(),
            ));
        }
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
                    PartitionKeyDef::Column(col, opclass) => {
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
                            opclass: opclass.clone(),
                        });
                    }
                    PartitionKeyDef::Expr(e, opclass) => {
                        key.push(PartKey {
                            col: usize::MAX,
                            expr: Some(e.clone()),
                            opclass: opclass.clone(),
                        });
                    }
                }
            }
            // v2.23: a HASH sub-partition key's opclasses resolve
            // like a root's (unknown class -> 42704).
            if def.method == crate::storage::PartMethod::Hash {
                for (kd, pk) in def.keys.iter().zip(key.iter()) {
                    let ty = if pk.expr.is_none() {
                        Some(columns[pk.col].1)
                    } else {
                        None
                    };
                    let _ = kd;
                    super::opclass::validate_key_opclass(eng, pk, ty)?;
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
                PartitionKeyDef::Column(col, opclass) => {
                    let idx = columns.iter().position(|(n, _)| n == col).ok_or_else(|| {
                        exec_err(
                            "42703",
                            format!("column \"{}\" of relation \"{}\" does not exist", col, name),
                        )
                    })?;
                    key.push(PartKey {
                        col: idx,
                        expr: None,
                        opclass: opclass.clone(),
                    });
                }
                PartitionKeyDef::Expr(e, opclass) => {
                    key.push(PartKey {
                        col: usize::MAX,
                        expr: Some(e.clone()),
                        opclass: opclass.clone(),
                    });
                }
            }
        }
        // v2.23: for a HASH root, every named opclass must resolve
        // (PG19 42704 for an unknown class; a class whose type does
        // not match the key column's type is 42P16-ish — PG reports
        // "operator class ... does not exist for access method" /
        // type mismatch at parse analysis; the census only needs
        // the unknown-name error). RANGE/LIST keys accept + record
        // any class (btree semantics are not wired).
        if def.method == crate::storage::PartMethod::Hash {
            for pk in &key {
                let ty = if pk.expr.is_none() {
                    Some(columns[pk.col].1)
                } else {
                    None
                };
                super::opclass::validate_key_opclass(eng, pk, ty)?;
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
        // v2.21: collation/cast wrappers are transparent for key typing
        // (PARTITION BY LIST (c COLLATE "C") keys coerce bounds as the
        // underlying column type).
        Expr::Collate { expr, .. } | Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => {
            infer_expr_key_type(expr, parent)
        }
        _ => ColType::Int,
    }
}

/// v2.21: peel collation wrappers off a partition bound expression
/// (PG folds `'a' COLLATE "C"` to the literal for bound purposes).
/// Cast wrappers around the literal fold the same way (the bound is
/// coerced to the key type afterwards regardless).
pub(crate) fn unwrap_bound_collate(e: &Expr) -> &Expr {
    match e {
        Expr::Collate { expr, .. } | Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => {
            unwrap_bound_collate(expr)
        }
        other => other,
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
                match unwrap_bound_collate(e) {
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
            // v2.23: PG19 parse_utilcmd.c messages (42P16).
            if *modulus == 0 {
                return Err(exec_err(
                    "42P16",
                    "modulus for hash partition must be an integer value greater than zero"
                        .to_string(),
                ));
            }
            if *remainder >= *modulus {
                return Err(exec_err(
                    "42P16",
                    "remainder for hash partition must be less than modulus".to_string(),
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
        RangeBoundDef::Val(e) => match unwrap_bound_collate(e) {
            Expr::Literal(l) => Ok(RangeBound::Val(coerce_literal(l, ty, &columns[0].0)?)),
            _ => Err(exec_err(
                "42601",
                "partition bound must be a literal".to_string(),
            )),
        },
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
    // v2.23: PG19's hash modulus factor rule (partbounds.c
    // check_new_partition_bound): every modulus must be a factor
    // of the next larger modulus — for any sibling pair, one
    // modulus must divide the other (42P17).
    if let (PartMethod::Hash, PartBound::Hash { modulus: nm, .. }) = (&pinfo.method, bound) {
        for child_name in &pinfo.children {
            if child_name == new_name {
                continue;
            }
            let Some(PartBound::Hash { modulus: sm, .. }) = eng
                .db
                .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
                .and_then(|child| child.partition.as_ref())
                .and_then(|cinfo| cinfo.bound.as_ref())
            else {
                continue;
            };
            if nm % sm != 0 && sm % nm != 0 {
                let detail = if nm < sm {
                    format!(
                        "The new modulus {} is not a factor of {}, the modulus of existing partition \"{}\".",
                        nm, sm, child_name
                    )
                } else {
                    format!(
                        "The new modulus {} is not divisible by {}, the modulus of existing partition \"{}\".",
                        nm, sm, child_name
                    )
                };
                return Err(exec_err_detail(
                    "42P17",
                    "every hash partition modulus must be a factor of the next larger modulus"
                        .to_string(),
                    detail,
                ));
            }
        }
    }
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
            // v2.23: the residue classes {x = ar (mod am)} and
            // {x = br (mod bm)} intersect iff ar = br modulo
            // gcd(am, bm) (PG19's indexes[] overlap test reduces
            // to this once the factor rule holds).
            fn gcd(mut a: u32, mut b: u32) -> u32 {
                while b != 0 {
                    let t = b;
                    b = a % b;
                    a = t;
                }
                a.max(1)
            }
            let g = gcd(*am, *bm);
            ar % g == br % g
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
///
/// v2.20: the v0.69 hand-rolled match only ordered integer and text
/// pairs; every other pair (Numeric, Date, Float, Bool, Timestamp,
/// Uuid, cross-width numeric mixes, ...) returned None, and
/// `range_bound_contains` reads None on its upper-bound scan as
/// "not less", so NO range bound ever contained a numeric/date/
/// float key — routing into any subtree keyed by such a column
/// failed 23514 (update.sql's part_b_10_b_20, keyed by
/// `c numeric`, was the first critical path to hit it; the column
/// permutation of that subtree was innocent — probe220 case B
/// routes the same shape keyed by int correctly). Delegate to the
/// engine's general btree ordering (`compare_values`: exact
/// int/numeric comparison, pg_float_ord, bpchar trailing-space
/// rules, date/time natural order), mapping its 42804
/// incomparable-types error back to None so exotic key types keep
/// the old not-equal fallback. NULLs never reach the delegate:
/// callers pre-handle NULL keys (a RANGE key with a NULL component
/// is unroutable; bound values are non-NULL by construction).
pub(crate) fn compare_partition_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Null, _) | (_, Value::Null) => None,
        _ => compare_values(a, b, false, None).ok(),
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
    // v2.16: PG19 ATExecAttachPartition preconditions.
    require_table_owner(eng, ctx, child_name)?;
    {
        let child = eng
            .db
            .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("child resolved above");
        if eng.db.typed_tables.contains_key(child_name) {
            return Err(exec_err(
                "42809",
                "cannot attach a typed table as partition".to_string(),
            ));
        }
        if !child.inherits.is_empty() {
            return Err(exec_err(
                "42809",
                "cannot attach inheritance child as partition".to_string(),
            ));
        }
        // Bound form must match the parent's strategy (PG19
        // transformPartitionBound, 42P16).
        let form_ok = match (&pinfo.method, bound_def) {
            (_, PartBoundDef::Default) => true,
            (crate::storage::PartMethod::List, PartBoundDef::List(_)) => true,
            (crate::storage::PartMethod::Range, PartBoundDef::Range { .. }) => true,
            (crate::storage::PartMethod::Hash, PartBoundDef::Hash { .. }) => true,
            _ => false,
        };
        if !form_ok {
            let kind = match pinfo.method {
                crate::storage::PartMethod::List => "list",
                crate::storage::PartMethod::Range => "range",
                crate::storage::PartMethod::Hash => "hash",
            };
            return Err(exec_err(
                "42P16",
                format!("invalid bound specification for a {} partition", kind),
            ));
        }
        // v2.23: PG19 — a hash-partitioned table may not have a
        // default partition (parse_utilcmd.c, 42P16).
        if pinfo.method == crate::storage::PartMethod::Hash
            && matches!(bound_def, PartBoundDef::Default)
        {
            return Err(exec_err(
                "42P16",
                "a hash-partitioned table may not have a default partition".to_string(),
            ));
        }
        // Extra child columns are rejected (42804); missing ones
        // are caught by the compatibility loop below.
        for (cname, _) in &child.columns {
            if !parent_cols.iter().any(|(n, _)| n == cname) {
                return Err(exec_err(
                    "42804",
                    format!(
                        "table \"{}\" contains column \"{}\" not found in parent \"{}\"",
                        child_name, cname, parent_name
                    ),
                ));
            }
        }
    }
    // v1.97: PG19 rejects circular partition attachment
    // ("circular inheritance not allowed", 42P16): the table being
    // attached must not be an ancestor of the parent in the
    // partition tree. (Such cycles previously formed silently and
    // recursive ALTER propagation over them overflowed the stack.)
    {
        let mut cur = Some(parent_name.to_string());
        let mut hops = 0usize;
        while let Some(c) = cur {
            if c == child_name {
                return Err(exec_err(
                    "42P16",
                    "circular inheritance not allowed".to_string(),
                ));
            }
            hops += 1;
            if hops > 100_000 {
                break;
            }
            cur = eng
                .db
                .find_table(&c, ctx.snap, &ctx.all_xids, ctx.session)
                .and_then(|t| t.partition.as_ref().and_then(|p| p.parent.clone()));
        }
    }
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
    // v1.97: PG19 MergeCheckConstraint on ATTACH — a parent's
    // validated NOT NULL constraint conflicts with a child's
    // NOT VALID not-null constraint on the same column (42P17).
    {
        let parent_t = eng
            .db
            .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("parent resolved above");
        let child_t = eng
            .db
            .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("child resolved above");
        for (pi, (pname, _)) in parent_t.columns.iter().enumerate() {
            if parent_t.not_null.get(pi) != Some(&true) {
                continue;
            }
            // v2.27: the conflict is directional — a VALID parent
            // not-null cannot merge into a NOT VALID child copy.
            // When the parent's own constraint is NOT VALID too,
            // the two merge without complaint (constraints.sql
            // notnull_tbl1_3 attach). A flag-only parent not-null
            // is inherently valid.
            let parent_nn_valid = !parent_t.checks.iter().any(|pc| {
                pc.kind == crate::sql::CheckKind::NotNull
                    && pc.not_valid
                    && crate::sql::check_expr_column(&pc.expr).as_deref() == Some(pname.as_str())
            });
            if !parent_nn_valid {
                continue;
            }
            for cc in &child_t.checks {
                if cc.kind == crate::sql::CheckKind::NotNull && cc.not_valid {
                    let mut refs: Vec<(Option<String>, String)> = Vec::new();
                    crate::sql::collect_col_refs(&cc.expr, &mut refs);
                    if refs.iter().any(|(_, r)| r == pname) {
                        let pname_con = parent_t
                            .checks
                            .iter()
                            .find(|pc| {
                                pc.kind == crate::sql::CheckKind::NotNull && {
                                    let mut pr: Vec<(Option<String>, String)> = Vec::new();
                                    crate::sql::collect_col_refs(&pc.expr, &mut pr);
                                    pr.iter().any(|(_, r)| r == pname)
                                }
                            })
                            .map(|pc| pc.name.clone())
                            .unwrap_or_else(|| cc.name.clone());
                        return Err(exec_err(
                            "42P17",
                            format!(
                                "constraint \"{}\" conflicts with NOT VALID constraint on child table \"{}\"",
                                pname_con, child_name
                            ),
                        ));
                    }
                }
            }
        }
        // v2.16: PG19 merge rule — a parent constraint cannot
        // merge into a child copy marked NO INHERIT (42P17,
        // "conflicts with non-inherited constraint on child").
        for cc in &child_t.checks {
            if !cc.no_inherit {
                continue;
            }
            let mut refs: Vec<(Option<String>, String)> = Vec::new();
            crate::sql::collect_col_refs(&cc.expr, &mut refs);
            let conflicts = match cc.kind {
                crate::sql::CheckKind::NotNull => refs.iter().any(|(_, r)| {
                    parent_t
                        .columns
                        .iter()
                        .position(|(n, _)| n == r)
                        .is_some_and(|pi| parent_t.not_null.get(pi) == Some(&true))
                }),
                crate::sql::CheckKind::Check => parent_t.checks.iter().any(|pc| pc.name == cc.name),
            };
            if conflicts {
                return Err(exec_err(
                    "42P17",
                    format!(
                        "constraint \"{}\" conflicts with non-inherited constraint on child table \"{}\"",
                        cc.name, child_name
                    ),
                ));
            }
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
        // v2.23: an intermediate (sub-partitioned) child's rows live
        // in its descendant leaves — PG scans the whole subtree
        // (the alter_table hpart_5 shape: the violating row sits in
        // hpart_5_a, not in hpart_5 itself). Each row travels with
        // its own table's column list for the remap to parent order.
        let mut rows: Vec<(Vec<(String, ColType)>, Row)> = Vec::new();
        {
            let child = eng
                .db
                .find_table(child_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("child still visible");
            for rv in child
                .rows
                .iter()
                .filter(|rv| row_visible(rv, ctx.snap, &ctx.all_xids))
            {
                rows.push((child_cols.clone(), rv.values.clone()));
            }
            let leaves =
                collect_partition_leaves(&eng.db, child_name, ctx.snap, ctx.own, ctx.session);
            for leaf in leaves {
                if leaf == child_name {
                    continue;
                }
                let Some(lt) = eng
                    .db
                    .find_table(&leaf, ctx.snap, &ctx.all_xids, ctx.session)
                else {
                    continue;
                };
                let lcols = lt.columns.clone();
                for rv in lt
                    .rows
                    .iter()
                    .filter(|rv| row_visible(rv, ctx.snap, &ctx.all_xids))
                {
                    rows.push((lcols.clone(), rv.values.clone()));
                }
            }
        }
        // v2.06: for a DEFAULT attach the converted bound is a dummy
        // that contains nothing; PG's rule is the inverse — every
        // existing row must fall outside every SIBLING bound (a row
        // that belongs in another partition violates the default
        // partition constraint). Collect the sibling bounds up front.
        let sibling_bounds: Vec<PartBound> = if matches!(bound_def, PartBoundDef::Default) {
            let parent = eng
                .db
                .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("parent still visible");
            let mut out = Vec::new();
            if let Some(pp) = &parent.partition {
                for sib in &pp.children {
                    if sib == child_name {
                        continue;
                    }
                    if let Some(st) = eng.db.find_table(sib, ctx.snap, &ctx.all_xids, ctx.session)
                        && let Some(sb) = st.partition.as_ref().and_then(|sp| sp.bound.clone())
                    {
                        out.push(sb);
                    }
                }
            }
            out
        } else {
            Vec::new()
        };
        for (row_cols, values) in &rows {
            // Map the row to parent column order, then check bound.
            // v0.70: expression partition keys are evaluated for real
            // (was NULL), via the shared key evaluator.
            let prow = remap_row_to_parent(row_cols, &parent_cols, values);
            let key = eval_table_part_key(eng, ctx, parent_name, &pinfo, &parent_cols, &prow)?;
            // v2.23: HASH attach validation uses the PG row hash.
            let row_hash = if pinfo.method == crate::storage::PartMethod::Hash {
                Some(partition_row_hash(eng, ctx, &pinfo, &parent_cols, &key)?)
            } else {
                None
            };
            let row_ok = if matches!(bound_def, PartBoundDef::Default) {
                sibling_bounds
                    .iter()
                    .all(|sb| !bound_contains_hash(&pinfo.method, sb, &key, row_hash))
            } else {
                bound_contains_hash(&pinfo.method, &bound, &key, row_hash)
            };
            if !row_ok {
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
    // v2.15: PG19 ATExecAttachPartition merges the parent's FK
    // constraints into the child tree. A matching NOT VALID clone
    // is validated against the child's existing rows (a violation
    // aborts the attach with 23503) and becomes validated; a
    // missing constraint is cloned onto the child (validated the
    // same way when the parent's constraint is validated). The
    // pre-fix code did neither, so a child carrying a violating
    // row attached silently and the FK stayed NOT VALID forever.
    let mut fk_flips: Vec<(String, String)> = Vec::new(); // (table, fk name)
    let mut fk_clones: Vec<(String, FkDef)> = Vec::new(); // (table, fk)
    {
        let parent_t = eng
            .db
            .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("parent resolved above")
            .clone();
        let mut tree: Vec<String> = vec![child_name.to_string()];
        tree.extend(crate::exec::alter::partition_descendants(
            eng, ctx, child_name,
        ));
        let same_fk = |a: &FkDef, b: &FkDef| {
            a.cols == b.cols
                && a.ref_table == b.ref_table
                && (a.ref_cols == b.ref_cols || a.ref_cols.is_empty() || b.ref_cols.is_empty())
        };
        for pfk in &parent_t.fks {
            for tname in &tree {
                let Some(tt) = eng
                    .db
                    .find_table(tname, ctx.snap, &ctx.all_xids, ctx.session)
                    .cloned()
                else {
                    continue;
                };
                match tt.fks.iter().find(|cf| same_fk(cf, pfk)) {
                    Some(cfk) => {
                        if cfk.not_valid && !pfk.not_valid {
                            // Validate this table's rows against
                            // the clone, then mark it validated.
                            let meta = {
                                let mut m = TableMeta::of(&tt);
                                m.fks = vec![cfk.clone()];
                                m
                            };
                            let rows: Vec<Row> = tt
                                .rows
                                .iter()
                                .filter(|rv| row_visible(rv, ctx.snap, &ctx.all_xids))
                                .map(|rv| rv.values.clone())
                                .collect();
                            for values in &rows {
                                check_fk_child_row(
                                    eng,
                                    ctx.snap,
                                    ctx.own,
                                    ctx.session,
                                    &meta,
                                    tname,
                                    values,
                                    &[],
                                    None,
                                )?;
                            }
                            fk_flips.push((tname.clone(), cfk.name.clone()));
                        }
                    }
                    None => {
                        let mut clone = pfk.clone();
                        clone.name = format!("{}_{}_fkey", tname, pfk.cols.join("_"));
                        let mut n = 1;
                        while tt.fks.iter().any(|f| f.name == clone.name) {
                            n += 1;
                            clone.name = format!("{}_{}_fkey{}", tname, pfk.cols.join("_"), n);
                        }
                        if !pfk.not_valid {
                            let meta = {
                                let mut m = TableMeta::of(&tt);
                                m.fks = vec![clone.clone()];
                                m
                            };
                            let rows: Vec<Row> = tt
                                .rows
                                .iter()
                                .filter(|rv| row_visible(rv, ctx.snap, &ctx.all_xids))
                                .map(|rv| rv.values.clone())
                                .collect();
                            for values in &rows {
                                check_fk_child_row(
                                    eng,
                                    ctx.snap,
                                    ctx.own,
                                    ctx.session,
                                    &meta,
                                    tname,
                                    values,
                                    &[],
                                    None,
                                )?;
                            }
                            clone.not_valid = false;
                        }
                        fk_clones.push((tname.clone(), clone));
                    }
                }
            }
        }
    }
    // v2.25: PG19 builds a child index on the attaching subtree's
    // leaves for every unique index on the new ancestors; duplicates
    // within a leaf abort the ATTACH with 23505.
    validate_attach_ancestor_uniques(eng, ctx, parent_name, child_name)?;
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
    // v2.27: ATTACH locality flips (PG19 MergeWithExistingConstraint
    // / AttachPartition): every child constraint the parent also
    // carries becomes non-local (coninhcount 1 by derivation),
    // not-null flags the parent carries become non-local, and all
    // columns become attribute-non-local. Child-only constraints
    // stay local.
    {
        let parent = eng
            .db
            .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
            .cloned();
        if let Some(parent) = parent {
            for c in next.checks.iter_mut() {
                let r = crate::exec::locality::ConRef::of_check(c);
                if crate::exec::locality::parent_carries(&parent, &r, true) {
                    c.is_local = false;
                }
            }
            for u in next.uniques.iter_mut() {
                if parent.uniques.iter().any(|pu| pu.cols == u.cols) {
                    u.is_local = false;
                }
            }
            if let Some(pk) = next.pkey.as_mut() {
                if parent.pkey.as_ref().is_some_and(|pp| pp.cols == pk.cols) {
                    pk.is_local = false;
                }
            }
            for f in next.fks.iter_mut() {
                if parent
                    .fks
                    .iter()
                    .any(|pf| pf.cols == f.cols && pf.ref_table == f.ref_table)
                {
                    f.is_local = false;
                }
            }
            for i in 0..next.columns.len() {
                let col = next.columns[i].0.clone();
                if let Some(pi) = parent.column_index(&col) {
                    if parent.not_null.get(pi).copied().unwrap_or(false)
                        && next.not_null.get(i).copied().unwrap_or(false)
                    {
                        next.set_nn_local(i, false);
                    }
                }
                next.set_att_local(i, false);
            }
        }
    }
    // v2.15: the child's own FK flips/clones from the merge above
    // ride its new version; descendants get their own versions.
    for (tname, fname) in &fk_flips {
        if tname == child_name {
            if let Some(f) = next.fks.iter_mut().find(|f| &f.name == fname) {
                f.not_valid = false;
            }
        }
    }
    for (tname, clone) in &fk_clones {
        if tname == child_name {
            next.fks.push(clone.clone());
        }
    }
    commit_table_version(eng, ctx, child_name, prev, next);
    for (tname, fname) in &fk_flips {
        if tname != child_name {
            let p = eng
                .db
                .find_table(tname, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("descendant still visible")
                .clone();
            let mut n = p.clone();
            if let Some(f) = n.fks.iter_mut().find(|f| &f.name == fname) {
                f.not_valid = false;
            }
            commit_table_version(eng, ctx, tname, p, n);
        }
    }
    for (tname, clone) in &fk_clones {
        if tname != child_name {
            let p = eng
                .db
                .find_table(tname, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("descendant still visible")
                .clone();
            let mut n = p.clone();
            n.fks.push(clone.clone());
            commit_table_version(eng, ctx, tname, p, n);
        }
    }
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
    // v1.96 finishing pass: PG19 forbids changing the inheritance
    // of a typed table.
    if eng.db.typed_tables.contains_key(child_name) {
        return Err(exec_err(
            "42P16",
            "cannot change inheritance of typed table".to_string(),
        ));
    }
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
            // v2.16: compare the VALUES of the type slots, not the
            // Option returned by Vec::get — ADD COLUMN does not grow
            // composite_types/domain_types/domain_elem, so a short
            // vec (missing slot = plain column) must compare equal
            // to an explicit None/false entry (the atacc3 INHERIT
            // failure after drop/re-add column).
            let same_type = ctype == ptype
                && child.composite_types.get(ci).cloned().flatten()
                    == parent.composite_types.get(pi).cloned().flatten()
                && child.domain_types.get(ci).cloned().flatten()
                    == parent.domain_types.get(pi).cloned().flatten()
                && child.domain_elem.get(ci).copied().unwrap_or(false)
                    == parent.domain_elem.get(pi).copied().unwrap_or(false);
            if !same_type {
                return Err(exec_err(
                    "42804",
                    format!(
                        "child table \"{}\" has different type for column \"{}\"",
                        child_name, pname
                    ),
                ));
            }
            // v2.16 finishing pass: PG19 MergeAttributesIntoExisting —
            // a column must be generated in the child if and only if it
            // is generated in the parent, and of the same kind (the
            // CREATE merge has enforced this since the run-2 fix round;
            // ALTER ... INHERIT silently accepted the mismatch, which
            // the generated_stored/virtual gtest_normal_child sections
            // expose once CREATE-side conflicts error correctly).
            let pgen = parent.gen_cols.get(pi).cloned().unwrap_or(None);
            let cgen = child.gen_cols.get(ci).cloned().unwrap_or(None);
            match (&cgen, &pgen) {
                (None, Some(_)) => {
                    return Err(exec_err(
                        "42804",
                        format!(
                            "column \"{}\" in child table must be a generated column",
                            pname
                        ),
                    ));
                }
                (Some(_), None) => {
                    return Err(exec_err(
                        "42804",
                        format!(
                            "column \"{}\" in child table must not be a generated column",
                            pname
                        ),
                    ));
                }
                (Some((_, cs)), Some((_, ps))) if cs != ps => {
                    return Err(exec_err(
                        "42804",
                        format!(
                            "column \"{}\" inherits from generated column of different kind",
                            pname
                        ),
                    ));
                }
                _ => {}
            }
            // v1.97: a parent NOT NULL backed by a NO INHERIT
            // constraint does not merge into the child (PG19), so
            // it does not force the child column NOT NULL.
            let parent_nn_no_inherit = parent.checks.iter().any(|c| {
                c.kind == crate::sql::CheckKind::NotNull && c.no_inherit && {
                    let mut refs: Vec<(Option<String>, String)> = Vec::new();
                    crate::sql::collect_col_refs(&c.expr, &mut refs);
                    refs.iter().any(|(_, r)| r == pname)
                }
            });
            if !parent_nn_no_inherit
                && parent.not_null.get(pi) == Some(&true)
                && child.not_null.get(ci) != Some(&true)
            {
                return Err(exec_err(
                    "42804",
                    format!(
                        "column \"{}\" in child table \"{}\" must be marked NOT NULL",
                        pname, child_name
                    ),
                ));
            }
        }
        // v1.97: PG19 MergeCheckConstraint — every parent CHECK
        // constraint must already exist in the child (matched by
        // definition), else INHERIT fails (42804).
        for pc in &parent.checks {
            if pc.kind != crate::sql::CheckKind::Check {
                continue;
            }
            let pexpr = format!("{:?}", pc.expr);
            let found = child.checks.iter().any(|cc| {
                cc.kind == crate::sql::CheckKind::Check && format!("{:?}", cc.expr) == pexpr
            });
            if !found {
                return Err(exec_err(
                    "42804",
                    format!("child table is missing constraint \"{}\"", pc.name),
                ));
            }
        }
        // v2.27: PG19 — a VALID parent constraint cannot merge
        // into a NOT VALID child copy at INHERIT time either
        // (constraints.sql: "constraint \"nn_parent\" conflicts
        // with NOT VALID constraint on child table ..."). Checks
        // match by definition, not-nulls by column; a parent
        // not-null backed by a NO INHERIT constraint never merges
        // and is exempt, as above.
        for pc in &parent.checks {
            if pc.not_valid {
                continue;
            }
            let child_conflict = match pc.kind {
                crate::sql::CheckKind::Check => {
                    let pexpr = format!("{:?}", pc.expr);
                    child.checks.iter().find(|cc| {
                        cc.kind == crate::sql::CheckKind::Check
                            && cc.not_valid
                            && format!("{:?}", cc.expr) == pexpr
                    })
                }
                crate::sql::CheckKind::NotNull => {
                    let pcol = crate::sql::check_expr_column(&pc.expr);
                    let Some(pcol) = pcol else { continue };
                    if pc.no_inherit {
                        continue;
                    }
                    child.checks.iter().find(|cc| {
                        cc.kind == crate::sql::CheckKind::NotNull
                            && cc.not_valid
                            && crate::sql::check_expr_column(&cc.expr).as_deref()
                                == Some(pcol.as_str())
                    })
                }
            };
            if let Some(cc) = child_conflict {
                return Err(exec_err(
                    "42P17",
                    format!(
                        "constraint \"{}\" conflicts with NOT VALID constraint on child table \"{}\"",
                        cc.name, child_name
                    ),
                ));
            }
        }
        // A flag-only parent not-null (no record on the parent) is
        // inherently VALID: same conflict against a NOT VALID
        // child record for the column.
        for (pi, (pname, _)) in parent.columns.iter().enumerate() {
            if parent.not_null.get(pi) != Some(&true) {
                continue;
            }
            if parent.checks.iter().any(|pc| {
                pc.kind == crate::sql::CheckKind::NotNull
                    && crate::sql::check_expr_column(&pc.expr).as_deref() == Some(pname.as_str())
            }) {
                continue; // record case handled above
            }
            if let Some(cc) = child.checks.iter().find(|cc| {
                cc.kind == crate::sql::CheckKind::NotNull
                    && cc.not_valid
                    && crate::sql::check_expr_column(&cc.expr).as_deref() == Some(pname.as_str())
            }) {
                return Err(exec_err(
                    "42P17",
                    format!(
                        "constraint \"{}\" conflicts with NOT VALID constraint on child table \"{}\"",
                        cc.name, child_name
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
        // v2.27: PG19 RemoveInheritance — the link's removal
        // decrements the child's inhcounts (derived here, so they
        // already reflect the unlink); copies whose count reaches 0
        // are marked LOCAL, never deleted. Columns likewise.
        {
            let refs: Vec<crate::exec::locality::ConRef> = next
                .checks
                .iter()
                .map(crate::exec::locality::ConRef::of_check)
                .collect();
            for r in &refs {
                if crate::exec::locality::con_inhcount(
                    &eng.db,
                    ctx.snap,
                    &ctx.all_xids,
                    ctx.session,
                    &next,
                    r,
                ) == 0
                {
                    match r {
                        crate::exec::locality::ConRef::Check(n) => {
                            for c in next.checks.iter_mut() {
                                if c.kind == crate::sql::CheckKind::Check && &c.name == n {
                                    c.is_local = true;
                                }
                            }
                        }
                        crate::exec::locality::ConRef::NotNull(col) => {
                            if let Some(i) = next.column_index(col) {
                                next.set_nn_local(i, true);
                            }
                            for c in next.checks.iter_mut() {
                                if c.kind == crate::sql::CheckKind::NotNull
                                    && crate::sql::check_expr_column(&c.expr).as_deref()
                                        == Some(col.as_str())
                                {
                                    c.is_local = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            for i in 0..next.columns.len() {
                let col = next.columns[i].0.clone();
                if crate::exec::locality::att_inhcount(
                    &eng.db,
                    ctx.snap,
                    &ctx.all_xids,
                    ctx.session,
                    &next,
                    &col,
                ) == 0
                {
                    next.set_att_local(i, true);
                }
            }
        }
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
                // v2.20: match by btree equality, not `Value`
                // PartialEq — the latter is scale-sensitive for
                // Numeric (1.0 != 1.00) and variant-sensitive for
                // cross-width ints, so a LIST bound could miss a
                // numerically equal key.
                values
                    .iter()
                    .any(|v| compare_partition_values(kv, v) == Some(std::cmp::Ordering::Equal))
            } else {
                false
            }
        }
        (PartMethod::Range, PartBound::Range { lower, upper }) => {
            range_bound_contains(lower, upper, key)
        }
        (PartMethod::Hash, PartBound::Hash { .. }) => {
            // v2.23: HASH membership needs the PG row hash, which
            // can require calling a user hash function — it cannot
            // be computed from the key alone. Callers with engine
            // access use `bound_contains_hash`; reaching this arm
            // means a pure call site judged a hash bound, which is
            // a programming error — fail closed (no match).
            false
        }
        _ => false,
    }
}

// ------------------------------------------------------------------
// v2.23: the PG hash-partition algorithm (partitioning/partbounds.c
// compute_partition_hash_value, include/catalog/partition.h,
// common/hashfn.c). rowHash starts at 0; each non-NULL key value is
// hashed with the key's opclass FUNCTION 2 (or the type's default
// extended hash) under HASH_PARTITION_SEED, and combined with
// hash_combine64. The bound matches iff rowHash % modulus ==
// remainder. An all-NULL key hashes to 0.
// ------------------------------------------------------------------

pub(crate) const HASH_PARTITION_SEED: u64 = 0x7A5B22367996DCFD;

pub(crate) fn hash_combine64(a: u64, b: u64) -> u64 {
    a ^ b
        .wrapping_add(0x49a0f4dd15e5a8e3)
        .wrapping_add(a << 54)
        .wrapping_add(a >> 7)
}

fn jenkins_mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

fn jenkins_final(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(14));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(11));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(25));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(16));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(4));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(14));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(24));
}

/// Jenkins `hash_bytes` (common/hashfn.c), little-endian path —
/// byte-identical to the C unaligned path for every input.
// The non-extended entry point is part of the faithful hashfn.c
// port; routing uses the extended forms, so this one is only
// exercised by the pinned-vector unit tests.
#[allow(dead_code)]
pub(crate) fn hash_bytes(k: &[u8]) -> u32 {
    let (mut a, mut b, mut c) = jenkins_hash_state(k, 0, false);
    jenkins_final(&mut a, &mut b, &mut c);
    c
}

/// Jenkins `hash_bytes_extended` (common/hashfn.c).
pub(crate) fn hash_bytes_extended(k: &[u8], seed: u64) -> u64 {
    let (mut a, mut b, mut c) = jenkins_hash_state(k, seed, true);
    jenkins_final(&mut a, &mut b, &mut c);
    ((b as u64) << 32) | (c as u64)
}

/// Shared body of hash_bytes / hash_bytes_extended: the initial
/// state, the optional seed mix, the 12-byte chunk loop and the
/// tail switch (little-endian, unaligned formulation — the C
/// aligned path produces identical results).
fn jenkins_hash_state(k: &[u8], seed: u64, extended: bool) -> (u32, u32, u32) {
    let mut a: u32 = 0x9e3779b9u32
        .wrapping_add(k.len() as u32)
        .wrapping_add(3923095);
    let mut b = a;
    let mut c = a;
    if extended && seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        jenkins_mix(&mut a, &mut b, &mut c);
    }
    let mut chunks = k.chunks_exact(12);
    for ch in &mut chunks {
        a = a.wrapping_add(u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]));
        b = b.wrapping_add(u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]));
        c = c.wrapping_add(u32::from_le_bytes([ch[8], ch[9], ch[10], ch[11]]));
        jenkins_mix(&mut a, &mut b, &mut c);
    }
    let t = chunks.remainder();
    // Tail (little-endian), the C switch's fallthrough written
    // out case by case:
    //   11: c+=t[10]<<24; 10: c+=t[9]<<16; 9: c+=t[8]<<8;
    //   8: b+=word(t[4..8]), a+=word(t[0..4])
    //   7: b+=t[6]<<16; 6: b+=t[5]<<8; 5: b+=t[4];
    //   4: a+=word(t[0..4])
    //   3: a+=t[2]<<16; 2: a+=t[1]<<8; 1: a+=t[0]
    match t.len() {
        11 => {
            c = c.wrapping_add((t[10] as u32) << 24);
            c = c.wrapping_add((t[9] as u32) << 16);
            c = c.wrapping_add((t[8] as u32) << 8);
            b = b.wrapping_add(u32::from_le_bytes([t[4], t[5], t[6], t[7]]));
            a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
        }
        10 => {
            c = c.wrapping_add((t[9] as u32) << 16);
            c = c.wrapping_add((t[8] as u32) << 8);
            b = b.wrapping_add(u32::from_le_bytes([t[4], t[5], t[6], t[7]]));
            a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
        }
        9 => {
            c = c.wrapping_add((t[8] as u32) << 8);
            b = b.wrapping_add(u32::from_le_bytes([t[4], t[5], t[6], t[7]]));
            a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
        }
        8 => {
            b = b.wrapping_add(u32::from_le_bytes([t[4], t[5], t[6], t[7]]));
            a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
        }
        7 => {
            b = b.wrapping_add((t[6] as u32) << 16);
            b = b.wrapping_add((t[5] as u32) << 8);
            b = b.wrapping_add(t[4] as u32);
            a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
        }
        6 => {
            b = b.wrapping_add((t[5] as u32) << 8);
            b = b.wrapping_add(t[4] as u32);
            a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
        }
        5 => {
            b = b.wrapping_add(t[4] as u32);
            a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
        }
        4 => {
            a = a.wrapping_add(u32::from_le_bytes([t[0], t[1], t[2], t[3]]));
        }
        3 => {
            a = a.wrapping_add((t[2] as u32) << 16);
            a = a.wrapping_add((t[1] as u32) << 8);
            a = a.wrapping_add(t[0] as u32);
        }
        2 => {
            a = a.wrapping_add((t[1] as u32) << 8);
            a = a.wrapping_add(t[0] as u32);
        }
        1 => {
            a = a.wrapping_add(t[0] as u32);
        }
        _ => {}
    }
    (a, b, c)
}

/// `hash_bytes_uint32_extended` (common/hashfn.c).
pub(crate) fn hash_bytes_uint32_extended(k: u32, seed: u64) -> u64 {
    let mut a: u32 = 0x9e3779b9u32.wrapping_add(4).wrapping_add(3923095);
    let mut b = a;
    let mut c = a;
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        jenkins_mix(&mut a, &mut b, &mut c);
    }
    a = a.wrapping_add(k);
    jenkins_final(&mut a, &mut b, &mut c);
    ((b as u64) << 32) | (c as u64)
}

/// hashint8's cross-type fold (access/hash/hashfunc.c): xor the
/// high half (or its complement for negatives) into the low half.
fn hash_int8_fold(val: i64) -> u32 {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    if val >= 0 { lo ^ hi } else { lo ^ !hi }
}

/// v2.23: the default extended hash for a value (the type's
/// default hash opclass FUNCTION 2), ported per type from
/// PG19's utils/adt and access/hash sources. `col_ty` refines
/// variants that share a Value representation (text vs bpchar vs
/// name; int widths); when it is `None` the Value variant decides.
pub(crate) fn default_extended_hash(v: &Value, col_ty: Option<&ColType>, seed: u64) -> u64 {
    match v {
        Value::SmallInt(x) => hash_bytes_uint32_extended(*x as i32 as u32, seed),
        Value::Int(x) => {
            // int4 by default; an int8 column carrying an Int
            // value (the engine widens freely) folds as int8.
            if matches!(col_ty, Some(ColType::BigInt)) {
                hash_bytes_uint32_extended(hash_int8_fold(*x), seed)
            } else {
                hash_bytes_uint32_extended(*x as i32 as u32, seed)
            }
        }
        Value::BigInt(x) => hash_bytes_uint32_extended(hash_int8_fold(*x), seed),
        Value::Bool(b) => hash_bytes_uint32_extended(if *b { 1 } else { 0 }, seed),
        Value::SingleChar(b) => hash_bytes_uint32_extended(*b as u32, seed),
        Value::Float4(f) => {
            if *f == 0.0 {
                return seed;
            }
            let key8 = if f.is_nan() { f64::NAN } else { *f as f64 };
            hash_bytes_extended(&key8.to_le_bytes(), seed)
        }
        Value::Float(f) => {
            if *f == 0.0 {
                return seed;
            }
            let key = if f.is_nan() { f64::NAN } else { *f };
            hash_bytes_extended(&key.to_le_bytes(), seed)
        }
        Value::Text(s) => {
            if matches!(col_ty, Some(ColType::Char(_))) {
                // hashbpcharextended: bcTruelen (trailing spaces
                // are not part of the value).
                let trimmed = s.trim_end_matches(' ');
                hash_bytes_extended(trimmed.as_bytes(), seed)
            } else {
                hash_bytes_extended(s.as_bytes(), seed)
            }
        }
        Value::BpChar(s) => {
            let trimmed = s.trim_end_matches(' ');
            hash_bytes_extended(trimmed.as_bytes(), seed)
        }
        Value::Date(d) => hash_bytes_uint32_extended(*d as u32, seed),
        Value::Timestamp(t) | Value::Timestamptz(t) | Value::Time(t) => {
            hash_bytes_uint32_extended(hash_int8_fold(*t), seed)
        }
        Value::TimeTz(t, zone) => {
            let th = hash_bytes_uint32_extended(hash_int8_fold(*t), seed);
            th ^ hash_bytes_uint32_extended(*zone as u32, seed)
        }
        Value::Interval(iv) => {
            // interval_cmp_value: ((months*30 + days) * USECS_PER_DAY
            // + micros), low 64 bits, hashed as int8.
            let span: i128 = ((iv.months as i128 * 30 + iv.days as i128) * 86_400_000_000i128)
                + iv.micros as i128;
            hash_bytes_uint32_extended(hash_int8_fold(span as i64), seed)
        }
        Value::Uuid(bytes) => hash_bytes_extended(bytes, seed),
        Value::Bytea(bytes) => hash_bytes_extended(bytes, seed),
        Value::PgLsn(x) => hash_bytes_uint32_extended(hash_int8_fold(*x as i64), seed),
        Value::Array(av) => {
            // hash_array_extended (arrayfuncs.c): result starts
            // at 1; each element is hashed with the ORIGINAL
            // seed (NULL elements hash to 0) and folded in as
            // result = result*31 + elthash. (The scope note's
            // "running seed / combine64" reading does not match
            // the C; the C is authoritative.)
            let mut result: u64 = 1;
            for e in &av.elems {
                let eh = if *e == Value::Null {
                    0
                } else {
                    default_extended_hash(e, None, seed)
                };
                result = result.wrapping_mul(31).wrapping_add(eh);
            }
            result
        }
        Value::Numeric(n) => {
            // Documented fallback: hash_any_extended over the
            // canonical text (PG hashes the base-10000 digit
            // array + weight; no schedule statement observes a
            // numeric default-hash placement).
            hash_bytes_extended(n.to_text().as_bytes(), seed)
        }
        other => {
            // Documented fallback for jsonb/json/record/bit and
            // every remaining variant: hash the canonical text.
            hash_bytes_extended(value_to_text_cast(other).as_bytes(), seed)
        }
    }
}

/// v2.23: the partition key type list for a table's columns —
/// plain keys take the column type; expression keys infer from
/// the expression shape (the `infer_expr_key_type` rules against
/// a column slice instead of a `Table`).
pub(crate) fn key_col_types(cols: &[(String, ColType)], pinfo: &PartitionInfo) -> Vec<ColType> {
    pinfo
        .key
        .iter()
        .map(|k| match &k.expr {
            None => cols[k.col].1,
            Some(e) => infer_key_type_from_cols(e, cols),
        })
        .collect()
}

fn infer_key_type_from_cols(expr: &Expr, cols: &[(String, ColType)]) -> ColType {
    match expr {
        Expr::Column { name, .. } => cols
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, ty)| *ty)
            .unwrap_or(ColType::Int),
        Expr::Func { name, args } => {
            if name.eq_ignore_ascii_case("lower") || name.eq_ignore_ascii_case("upper") {
                ColType::Text
            } else if name.eq_ignore_ascii_case("abs") {
                args.first()
                    .map(|a| infer_key_type_from_cols(a, cols))
                    .unwrap_or(ColType::Int)
            } else {
                ColType::Text
            }
        }
        Expr::Arith { left, .. } => infer_key_type_from_cols(left, cols),
        Expr::Collate { expr, .. } | Expr::Cast { expr, .. } | Expr::CastNamed { expr, .. } => {
            infer_key_type_from_cols(expr, cols)
        }
        _ => ColType::Int,
    }
}

/// v2.23: hash one partition key value under its opclass. A
/// custom class with a FUNCTION 2 calls that user function with
/// (value, seed) through the standard machinery (a NULL return
/// is 22023, like PG's "hash function returned null"); a class
/// without FUNCTION 2 (a btree-style equality-only class used on
/// a hash key) falls back to the default hash. No class -> the
/// default extended hash for the value's type.
pub(crate) fn partition_key_hash(
    eng: &mut Engine,
    ctx: &StmtCtx,
    pinfo: &PartitionInfo,
    idx: usize,
    value: &Value,
    col_ty: &ColType,
) -> Result<u64, ExecError> {
    let pk = &pinfo.key[idx];
    if let Some(ocname) = &pk.opclass
        && let Some(class) = super::opclass::find_opclass(&eng.db, ocname, "hash")
        && let Some(fname) = &class.hash_function
    {
        let base = fname.rsplit('.').next().unwrap_or(fname).to_string();
        let arg_types: Vec<String> = if class.hash_arg_types.is_empty() {
            vec![
                match col_ty {
                    ColType::SmallInt => "int2".to_string(),
                    ColType::Int => "int4".to_string(),
                    ColType::BigInt => "int8".to_string(),
                    ColType::Text | ColType::Varchar(_) | ColType::Char(_) => "text".to_string(),
                    _ => "int4".to_string(),
                },
                "int8".to_string(),
            ]
        } else {
            class.hash_arg_types.clone()
        };
        let fdef = find_function_by_signature(eng, &base, &arg_types)
            .ok_or_else(|| exec_err("42883", format!("function \"{}\" does not exist", fname)))?;
        // Build a Q over the statement context (the
        // eval_partition_key_expr pattern).
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            depth: 0,
            lock_ids: &mut lock_ids,
            ctes: Vec::new(),
            wctx: None,
            srf_vals: Vec::new(),
            role: ctx.role,
            read_only: false,
            priv_scopes: Vec::new(),
            priv_role_override: None,
            hashed_exists: Rc::new(RefCell::new(HashMap::new())),
            immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
            plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
            hashed_in: Rc::new(RefCell::new(Vec::new())),
            pending_updates: None,
            write: None,
        };
        let out = call_user_function(
            &mut q,
            &[],
            &fdef,
            &[value.clone(), Value::BigInt(HASH_PARTITION_SEED as i64)],
            &[None, None],
        )?;
        return match out {
            Value::BigInt(x) => Ok(x as u64),
            Value::Int(x) => Ok(x as u64),
            Value::SmallInt(x) => Ok(x as u64),
            Value::Null => Err(exec_err(
                "22023",
                format!("hash function \"{}\" returned null", fname),
            )),
            other => Err(exec_err(
                "42804",
                format!(
                    "hash function \"{}\" returned {}, expected bigint",
                    fname,
                    other.type_name()
                ),
            )),
        };
    }
    Ok(default_extended_hash(
        value,
        Some(col_ty),
        HASH_PARTITION_SEED,
    ))
}

/// v2.23: the PG row hash for a fully evaluated partition key
/// (compute_partition_hash_value): NULL values are skipped.
pub(crate) fn partition_row_hash(
    eng: &mut Engine,
    ctx: &StmtCtx,
    pinfo: &PartitionInfo,
    table_cols: &[(String, ColType)],
    key: &[Value],
) -> Result<u64, ExecError> {
    let types = key_col_types(table_cols, pinfo);
    let mut row_hash: u64 = 0;
    for (i, v) in key.iter().enumerate() {
        if *v == Value::Null {
            continue;
        }
        let h = partition_key_hash(eng, ctx, pinfo, i, v, &types[i])?;
        row_hash = hash_combine64(row_hash, h);
    }
    Ok(row_hash)
}

/// v2.23: `bound_contains` with a caller-computed row hash for
/// HASH bounds. `row_hash` is required iff the method is Hash
/// (callers pass `None` for other methods).
pub(crate) fn bound_contains_hash(
    method: &PartMethod,
    bound: &PartBound,
    key: &[Value],
    row_hash: Option<u64>,
) -> bool {
    match (method, bound) {
        (PartMethod::Hash, PartBound::Hash { modulus, remainder }) => match row_hash {
            Some(h) => h % (*modulus as u64) == (*remainder as u64),
            None => false,
        },
        _ => bound_contains(method, bound, key),
    }
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
/// v2.26: merge a `PARTITION OF` element list into the inherited
/// def's constraint fields (PG19 tablecmds.c MergeAttributes,
/// is_partition block; error shapes from parse_utilcmd.c /
/// MergeCheckConstraint). The caller has already cloned the parent's
/// rowtype and constraints into the def; `ov` is the element def built
/// by `build_partition_override_def` (its column types are
/// placeholders — only names and the parallel constraint fields are
/// read). Fields are passed split because the caller holds
/// `&def.partition` borrowed across the merge.
/// v2.27: a PARTITION OF child's not-null locality (PG19
/// conislocal for the column's not-null constraint): inherited
/// not-nulls are non-local, except a column whose NOT NULL was
/// restated by a PARTITION OF element definition — that element
/// records a local NotNull CheckDef (see apply_partition_overrides),
/// e.g. part_b's `b NOT NULL` yields part_b_b_not_null at t / 1.
pub(crate) fn fix_partition_child_nn_local(t: &mut Table) {
    for i in 0..t.columns.len() {
        if !t.not_null.get(i).copied().unwrap_or(false) {
            continue;
        }
        let col = t.columns[i].0.clone();
        let local = t.checks.iter().any(|c| {
            c.kind == crate::sql::CheckKind::NotNull
                && c.is_local
                && crate::sql::check_expr_column(&c.expr).as_deref() == Some(col.as_str())
        });
        t.set_nn_local(i, local);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_partition_overrides(
    child: &str,
    ov: TableDef,
    columns: &[(String, crate::storage::ColType)],
    gen_cols: &mut Vec<Option<(crate::sql::Expr, bool)>>,
    defaults: &mut Vec<Option<crate::sql::DefaultExpr>>,
    not_null: &mut Vec<bool>,
    checks: &mut Vec<crate::sql::CheckDef>,
    uniques: &mut Vec<crate::sql::UniqueDef>,
    pkey: &mut Option<crate::sql::UniqueDef>,
    fks: &mut Vec<crate::sql::FkDef>,
) -> Result<(), ExecError> {
    let bare = child.rsplit('.').next().unwrap_or(child);
    let col_pos = |col: &str| -> Result<usize, ExecError> {
        columns
            .iter()
            .position(|(n, _)| n == col)
            .ok_or_else(|| exec_err("42703", format!("column \"{}\" does not exist", col)))
    };
    // Per-column elements: existence, the generated-family rules,
    // DEFAULT replacement, NOT NULL accumulation.
    for (oi, (col, _ty)) in ov.columns.iter().enumerate() {
        let pos = col_pos(col)?;
        let pgen = gen_cols.get(pos).cloned().unwrap_or(None);
        let ogen = ov.gen_cols.get(oi).cloned().unwrap_or(None);
        match (&pgen, &ogen) {
            (Some(_), None) => {
                if ov.defaults.get(oi).and_then(|d| d.as_ref()).is_some() {
                    return Err(exec_err(
                        "42P16",
                        format!(
                            "column \"{}\" inherits from generated column but specifies default",
                            col
                        ),
                    ));
                }
            }
            (None, Some(_)) => {
                return Err(exec_err(
                    "42P16",
                    format!("child column \"{}\" specifies generation expression", col),
                ));
            }
            (Some((_, ps)), Some((_, os))) if ps != os => {
                return Err(exec_err(
                    "42P16",
                    format!(
                        "column \"{}\" inherits from generated column of different kind",
                        col
                    ),
                ));
            }
            (Some(_), Some((e, s))) => {
                if gen_cols.len() != columns.len() {
                    *gen_cols = vec![None; columns.len()];
                }
                gen_cols[pos] = Some((e.clone(), *s));
            }
            _ => {}
        }
        if pgen.is_none() {
            if let Some(d) = ov.defaults.get(oi).and_then(|d| d.clone()) {
                defaults[pos] = Some(d);
            }
        }
        // v2.27: the local record is pushed whenever the element
        // restates NOT NULL, even when the parent column is already
        // not-null — PG records part_b_b_not_null as a LOCAL
        // constraint (conislocal t, coninhcount 1) in exactly that
        // case; the pre-v2.27 gate on `!not_null[pos]` skipped it.
        if ov.not_null.get(oi).copied().unwrap_or(false) {
            not_null[pos] = true;
            // PG records a named not-null constraint on the child
            // (part_b_b_not_null in create_table's pg_constraint
            // expectations).
            let cname = format!("{}_{}_not_null", bare, col);
            if !checks.iter().any(|c| c.name == cname) {
                checks.push(crate::sql::CheckDef {
                    name: cname,
                    expr: crate::sql::Expr::IsNull {
                        expr: Box::new(crate::sql::Expr::Column {
                            table: None,
                            name: col.clone(),
                        }),
                        neg: true,
                    },
                    not_valid: false,
                    kind: crate::sql::CheckKind::NotNull,
                    no_inherit: false,
                    enforced: true,
                    is_local: true,
                });
            }
        }
    }
    // Table-constraint NOT NULL elements naming columns with no
    // column element of their own (deferred by the builder).
    for col in &ov.deferred_not_null {
        let pos = col_pos(col)?;
        not_null[pos] = true;
    }
    // CHECK elements (column- and table-level): an element whose name
    // and expression match an inherited constraint MERGES with it
    // (PG NOTICE "merging constraint ... with inherited definition");
    // the same name with a different expression is 42710.
    for c in &ov.checks {
        if c.kind == crate::sql::CheckKind::NotNull {
            // Not-null constraint records travel with the flags set
            // above; keep the record if its name is new.
            if !checks.iter().any(|x| x.name == c.name) {
                checks.push(c.clone());
            }
            continue;
        }
        let mut refs: Vec<(Option<String>, String)> = Vec::new();
        crate::sql::collect_col_refs(&c.expr, &mut refs);
        for (_, r) in &refs {
            col_pos(r)?;
        }
        if let Some(existing) = checks.iter().find(|x| x.name == c.name) {
            if existing.expr == c.expr && existing.kind == c.kind {
                continue;
            }
            return Err(exec_err(
                "42710",
                format!(
                    "check constraint name \"{}\" appears multiple times but with different expressions",
                    c.name
                ),
            ));
        }
        checks.push(c.clone());
    }
    // UNIQUE elements.
    for u in &ov.uniques {
        for c in &u.cols {
            col_pos(c)?;
        }
        if let Some(existing) = uniques.iter().find(|x| x.name == u.name) {
            if existing.cols == u.cols {
                continue;
            }
            return Err(exec_err(
                "42P16",
                format!(
                    "constraint \"{}\" for relation \"{}\" already exists",
                    u.name, bare
                ),
            ));
        }
        uniques.push(u.clone());
    }
    // PRIMARY KEY element.
    if let Some(pk) = &ov.pkey {
        for c in &pk.cols {
            col_pos(c)?;
        }
        match &*pkey {
            None => {
                for c in &pk.cols {
                    let pos = col_pos(c)?;
                    not_null[pos] = true;
                }
                *pkey = Some(pk.clone());
            }
            Some(existing) if existing.cols == pk.cols => {}
            Some(_) => {
                return Err(exec_err(
                    "42601",
                    "multiple primary keys for table".to_string(),
                ));
            }
        }
    }
    // FOREIGN KEY elements (validated by the caller's usual FK pass).
    for fk in &ov.fks {
        for c in &fk.cols {
            col_pos(c)?;
        }
        if fks.iter().any(|x| x.name == fk.name) {
            continue;
        }
        fks.push(fk.clone());
    }
    Ok(())
}

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
        let (leaf_pinfo, leaf_has_row_triggers) = {
            let lt = eng
                .db
                .find_table(&leaf_name, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("leaf still visible");
            (
                lt.partition.clone().expect("leaf is partitioned"),
                lt.triggers
                    .iter()
                    .any(is_eligible_before_insert_row_trigger),
            )
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
        // must hold for the post-trigger row. Skip the walk-up entirely
        // when the leaf has no eligible BEFORE INSERT FOR EACH ROW
        // trigger: `fire_before_insert_triggers` then returns every row
        // byte-for-byte unchanged (its own `firing.is_empty()` fast
        // path), so the row is exactly what `find_partition_leaf`
        // already proved satisfies this same bound chain while routing
        // it here, and the walk-up (one `eval_table_part_key`/
        // `partition_row_hash` per ancestor level, same primitives
        // `find_partition_leaf` already called) cannot find a
        // violation.
        if leaf_has_row_triggers {
            for (_, row) in &fired_rows {
                check_leaf_bound(eng, ctx, &leaf_name, &leaf_pinfo, &leaf_cols, &row[..])?;
            }
        }
        // v2.26: leaf-local write semantics (PG19 ExecInsert judges
        // the row by the LEAF's tuple descriptor): a partition whose
        // definitions differ from the target's — a generated-column
        // override, an element NOT NULL, or a leaf-local CHECK (all
        // from a PARTITION OF element list) — recomputes its generated
        // columns and re-validates the routed rows against its own
        // constraint set. Leaves whose definitions match the target's
        // (the overwhelmingly common plain child) skip everything.
        {
            let (leaf_meta, target_meta) = {
                let lt = eng
                    .db
                    .find_table(&leaf_name, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("leaf still visible");
                let tt = eng
                    .db
                    .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("target still visible");
                (TableMeta::of(lt), TableMeta::of(tt))
            };
            let by_name =
                |meta: &TableMeta, n: &str| meta.columns.iter().position(|(cn, _)| cn == n);
            let gen_differs = leaf_meta.columns.iter().enumerate().any(|(li, (ln, _))| {
                let lg = leaf_meta.gen_cols.get(li).cloned().unwrap_or(None);
                let tg = by_name(&target_meta, ln)
                    .and_then(|ti| target_meta.gen_cols.get(ti).cloned().unwrap_or(None));
                lg != tg
            });
            let nn_differs = leaf_meta.columns.iter().enumerate().any(|(li, (ln, _))| {
                let lv = leaf_meta.not_null.get(li).copied().unwrap_or(false);
                let tv = by_name(&target_meta, ln)
                    .and_then(|ti| target_meta.not_null.get(ti).copied())
                    .unwrap_or(false);
                lv != tv
            });
            let extra_checks = leaf_meta.checks.iter().any(|c| {
                !target_meta
                    .checks
                    .iter()
                    .any(|t| t.name == c.name && t.expr == c.expr)
            });
            if gen_differs || nn_differs || extra_checks {
                for (_, row) in &mut fired_rows {
                    let mut vals = row[..].to_vec();
                    if gen_differs {
                        compute_generated_columns(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            &leaf_meta,
                            &mut vals,
                        )?;
                    }
                    check_row_constraints(
                        eng,
                        ctx.snap,
                        ctx.own,
                        ctx.session,
                        ctx.role,
                        &leaf_meta,
                        &leaf_name,
                        &vals,
                    )?;
                    *row = Row::new(vals);
                }
                if gen_differs && !leaf_has_row_triggers {
                    for (_, row) in &fired_rows {
                        check_leaf_bound(eng, ctx, &leaf_name, &leaf_pinfo, &leaf_cols, &row[..])?;
                    }
                }
            }
        }
        result.push((leaf_name, fired_rows));
    }
    Ok(result)
}

/// Whether `tr` is a BEFORE INSERT FOR EACH ROW trigger that
/// [`fire_before_insert_triggers`] would actually fire. Shared with
/// [`route_partition_inserts`], which uses it to skip the post-trigger
/// bound re-check entirely when a leaf has no such trigger (the
/// re-check can only matter for a row a trigger might have changed).
fn is_eligible_before_insert_row_trigger(tr: &TriggerDef) -> bool {
    tr.timing == TriggerTiming::Before
        && tr.for_each_row
        && tr.fires()
        && !tr.has_transition
        && (tr.events & trig_event::INSERT) != 0
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
            .filter(|tr| is_eligible_before_insert_row_trigger(tr))
            .cloned()
            .collect()
    };
    if firing.is_empty() {
        return Ok(rows.iter().map(|r| Some(r.clone())).collect());
    }
    // Resolve and parse each trigger's function body once.
    let mut bodies: Vec<TriggerBody> = Vec::with_capacity(firing.len());
    for tr in &firing {
        let fdef = find_function_by_signature(eng, &tr.function, &[]).ok_or_else(|| {
            exec_err(
                "42883",
                format!("function {}() does not exist", tr.function),
            )
        })?;
        let body = parse_trigger_body(&fdef.body).map_err(|e| {
            exec_err(
                e.code,
                format!("invalid trigger function body: {}", e.message),
            )
        })?;
        bodies.push(body);
    }
    let mut out: Vec<Option<Row>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cur: Option<Vec<Value>> = Some(row[..].to_vec());
        for (tr, body) in firing.iter().zip(bodies.iter()) {
            let Some(row_vals) = cur.take() else { break };
            // v2.09: WHEN gates BEFORE row triggers too (PG19
            // ExecCallTriggerFunc evaluates the qual first).
            let tctx = TrigCtx {
                name: tr.name.clone(),
                table: table.to_string(),
                timing: tr.timing,
                event: trig_event::INSERT,
                args: tr.args.clone(),
                relid: eng
                    .db
                    .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                    .map(|t| t.oid as i64)
                    .unwrap_or(0),
                for_each_row: tr.for_each_row,
            };
            if !trigger_when_matches(
                eng,
                ctx,
                &tctx,
                cols,
                Some(&row_vals),
                None,
                tr.when_expr.as_ref(),
            )? {
                cur = Some(row_vals);
                continue;
            }
            cur = exec_trigger_body(eng, ctx, &tctx, cols, Some(&row_vals), None, body)?;
        }
        out.push(cur.map(Row::new));
    }
    Ok(out)
}

/// v2.09: the firing context of one trigger invocation (PG19's
/// TriggerData, reduced to what bodies can observe: TG_NAME,
/// TG_TABLE_NAME, TG_WHEN, TG_OP, TG_LEVEL).
#[derive(Clone)]
pub(crate) struct TrigCtx {
    pub name: String,
    pub table: String,
    pub timing: TriggerTiming,
    /// Single `trig_event::*` bit being fired.
    pub event: u8,
    /// v2.09: the trigger's EXECUTE arguments (TG_ARGV).
    pub args: Vec<String>,
    /// v2.09: OID of the firing table (NEW.tableoid / TG_RELID).
    pub relid: i64,
    /// v2.14: FOR EACH ROW (TG_LEVEL) — false for statement-level
    /// firings.
    pub for_each_row: bool,
}

impl TrigCtx {
    fn op_name(&self) -> &'static str {
        match self.event {
            trig_event::INSERT => "INSERT",
            trig_event::DELETE => "DELETE",
            trig_event::UPDATE => "UPDATE",
            _ => "TRUNCATE",
        }
    }
    fn when_name(&self) -> &'static str {
        match self.timing {
            TriggerTiming::Before => "BEFORE",
            TriggerTiming::After => "AFTER",
            TriggerTiming::InsteadOf => "INSTEAD OF",
        }
    }
    /// v2.14: TG_LEVEL text.
    pub(crate) fn level_name(&self) -> &'static str {
        if self.for_each_row {
            "ROW"
        } else {
            "STATEMENT"
        }
    }
}

/// v2.09: how a trigger-body statement list ended.
enum TrigFlow {
    /// Fell off the end of the list.
    End,
    /// `RETURN NEW` / `RETURN OLD` / `RETURN NULL` executed; the
    /// payload is which row the function returned (None = NULL).
    Return(Option<RowChoice>),
}

#[derive(Clone, Copy, PartialEq)]
enum RowChoice {
    New,
    Old,
}

/// v2.10: mutable per-invocation state of a trigger body: DECLAREd
/// variables, FOR-loop record bindings (variable name -> the current
/// row as (column name, value) pairs), and the FOUND flag (PG19
/// plpgsql's `found` variable, set by DML / SELECT INTO / PERFORM /
/// FOR).
#[derive(Default)]
pub(crate) struct TrigRun {
    pub vars: HashMap<String, Value>,
    pub recs: HashMap<String, Vec<(String, Value)>>,
    pub found: bool,
    /// v2.10: `ALIAS FOR NEW|OLD` bindings (true = NEW, false = OLD).
    pub aliases: HashMap<String, bool>,
}

impl TrigRun {
    /// Read a scalar variable: a record variable reads as its first
    /// column (bounded model of PG's composite read in scalar
    /// contexts the suites use).
    fn get_var(&self, name: &str) -> Option<Value> {
        if let Some(row) = self.recs.get(name) {
            return row.first().map(|(_, v)| v.clone());
        }
        self.vars.get(name).cloned()
    }
}

/// v1.00/v2.09/v2.10: execute one trigger-function body for a single
/// row pair. `new_row` / `old_row` are None when the event has no
/// such image (INSERT has no OLD; DELETE has no NEW — the missing
/// image reads as all-NULL, like PG). Returns the row the function
/// returned: Some(row) for RETURN NEW/OLD (with NEW assignments
/// applied), None for RETURN NULL. Falling off the end without
/// RETURN is 2F005, as in PG. v2.10: the invocation runs at
/// pg_trigger_depth()+1 (RAII guard) and DECLARE defaults are
/// evaluated in declaration order.
pub(crate) fn exec_trigger_body(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    tctx: &TrigCtx,
    cols: &[(String, ColType)],
    new_row: Option<&[Value]>,
    old_row: Option<&[Value]>,
    body: &TriggerBody,
) -> Result<Option<Vec<Value>>, ExecError> {
    let _depth = TriggerDepthGuard::enter()?;
    let nulls: Vec<Value> = vec![Value::Null; cols.len()];
    let mut new_vals: Vec<Value> = new_row.unwrap_or(&nulls).to_vec();
    let old_vals: Vec<Value> = old_row.unwrap_or(&nulls).to_vec();
    let mut run = TrigRun::default();
    for d in &body.decls {
        if let Some(target) = &d.alias_for {
            run.aliases.insert(d.name.clone(), target == "new");
            continue;
        }
        let v = match &d.default {
            Some(e) => eval_trigger_expr(eng, ctx, tctx, cols, &new_vals, &old_vals, &run, e)?,
            None => Value::Null,
        };
        run.vars.insert(d.name.clone(), v);
    }
    match exec_trigger_stmts(
        eng,
        ctx,
        tctx,
        cols,
        &mut new_vals,
        &old_vals,
        &mut run,
        &body.stmts,
    )? {
        TrigFlow::Return(Some(RowChoice::New)) => Ok(Some(new_vals)),
        TrigFlow::Return(Some(RowChoice::Old)) => Ok(Some(old_vals)),
        TrigFlow::Return(None) => Ok(None),
        TrigFlow::End => Err(exec_err(
            "2F005",
            "control reached end of trigger procedure without RETURN".to_string(),
        )),
    }
}

fn exec_trigger_stmts(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    tctx: &TrigCtx,
    cols: &[(String, ColType)],
    new_vals: &mut Vec<Value>,
    old_vals: &[Value],
    run: &mut TrigRun,
    stmts: &[TriggerBodyStmt],
) -> Result<TrigFlow, ExecError> {
    for stmt in stmts {
        match stmt {
            TriggerBodyStmt::Assign { col, expr } => {
                let idx = cols.iter().position(|(n, _)| n == col).ok_or_else(|| {
                    exec_err(
                        "42703",
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            col, tctx.table
                        ),
                    )
                })?;
                let val = eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, expr)?;
                new_vals[idx] = coerce_value(val, &cols[idx].1, col)?;
            }
            TriggerBodyStmt::Perform(expr) => {
                let _ = eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, expr)?;
            }
            TriggerBodyStmt::ReturnNew => return Ok(TrigFlow::Return(Some(RowChoice::New))),
            TriggerBodyStmt::ReturnOld => return Ok(TrigFlow::Return(Some(RowChoice::Old))),
            TriggerBodyStmt::ReturnNull => return Ok(TrigFlow::Return(None)),
            TriggerBodyStmt::Raise {
                level,
                format,
                args,
            } => {
                let vals: Vec<Value> = args
                    .iter()
                    .map(|e| eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, e))
                    .collect::<Result<Vec<_>, _>>()?;
                let msg = format_raise_message(format, &vals);
                match level {
                    RaiseLevel::Notice | RaiseLevel::Warning => ctx.notices.push(msg),
                    RaiseLevel::Exception => return Err(exec_err("P0001", msg)),
                }
            }
            TriggerBodyStmt::If {
                branches,
                else_body,
            } => {
                let mut taken = false;
                for (cond, body) in branches {
                    let v = match cond {
                        TrigCond::Expr(e) => {
                            eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, e)?
                        }
                        TrigCond::Query(q) => {
                            let mut bound = (**q).clone();
                            bind_trigger_stmt(
                                eng, ctx, tctx, cols, new_vals, old_vals, run, &mut bound,
                            )?;
                            let res = execute(eng, ctx, &bound)?;
                            result_rows(&res)
                                .first()
                                .and_then(|r| r.first().cloned())
                                .unwrap_or(Value::Null)
                        }
                    };
                    if matches!(v, Value::Bool(true)) {
                        let flow = exec_trigger_stmts(
                            eng, ctx, tctx, cols, new_vals, old_vals, run, body,
                        )?;
                        if !matches!(flow, TrigFlow::End) {
                            return Ok(flow);
                        }
                        taken = true;
                        break;
                    }
                }
                if !taken && !else_body.is_empty() {
                    let flow = exec_trigger_stmts(
                        eng, ctx, tctx, cols, new_vals, old_vals, run, else_body,
                    )?;
                    if !matches!(flow, TrigFlow::End) {
                        return Ok(flow);
                    }
                }
            }
            TriggerBodyStmt::Block { body, handlers } => {
                let write_mark = ctx.writes.len();
                match exec_trigger_stmts(eng, ctx, tctx, cols, new_vals, old_vals, run, body) {
                    Ok(flow) => {
                        if !matches!(flow, TrigFlow::End) {
                            return Ok(flow);
                        }
                    }
                    Err(e) => {
                        // PG19: an EXCEPTION handler matching the
                        // error's SQLSTATE (by condition name or by
                        // explicit SQLSTATE) runs instead; the error
                        // is then considered handled.
                        let handler = handlers.iter().find(|(keys, _)| {
                            keys.iter().any(|k| {
                                k == "others"
                                    || k.eq_ignore_ascii_case(e.code)
                                    || k == &format!("sqlstate:{}", e.code)
                                    || condition_name_for_sqlstate(e.code).is_some_and(|n| n == k)
                            })
                        });
                        match handler {
                            Some((_, hbody)) => {
                                // v2.10: an EXCEPTION handler is a
                                // subtransaction in PG — roll the
                                // failed block's writes back before
                                // the handler runs.
                                ctx.writes.truncate(write_mark);
                                let flow = exec_trigger_stmts(
                                    eng, ctx, tctx, cols, new_vals, old_vals, run, hbody,
                                )?;
                                if !matches!(flow, TrigFlow::End) {
                                    return Ok(flow);
                                }
                            }
                            None => return Err(e),
                        }
                    }
                }
            }
            TriggerBodyStmt::ReturnAlias(name) => {
                return match run.aliases.get(name) {
                    Some(true) => Ok(TrigFlow::Return(Some(RowChoice::New))),
                    Some(false) => Ok(TrigFlow::Return(Some(RowChoice::Old))),
                    None => Err(exec_err(
                        "42601",
                        format!(
                            "RETURN value in a trigger function must be NEW, OLD, NULL, or an alias for one of them (got {:?})",
                            name
                        ),
                    )),
                };
            }
            // ---- v2.10 arms: variables, DML, dynamic SQL, loops ----
            TriggerBodyStmt::AssignNewWhole(expr) => {
                let v = eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, expr)?;
                match v {
                    Value::Record(fields) => {
                        for (fname, fval) in &fields {
                            if let Some(idx) = cols.iter().position(|(n, _)| n == fname) {
                                new_vals[idx] = coerce_value(fval.clone(), &cols[idx].1, fname)?;
                            }
                        }
                    }
                    _ => {
                        return Err(exec_err(
                            "42804",
                            "assignment to NEW must be a record value".to_string(),
                        ));
                    }
                }
            }
            TriggerBodyStmt::VarAssign { var, expr } => {
                if !run.vars.contains_key(var) && !run.recs.contains_key(var) {
                    return Err(exec_err(
                        "42703",
                        format!("variable \"{}\" does not exist", var),
                    ));
                }
                let val = eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, expr)?;
                run.recs.remove(var);
                run.vars.insert(var.clone(), val);
            }
            TriggerBodyStmt::Dml(stmt) => {
                let mut bound = stmt.clone();
                bind_trigger_stmt(eng, ctx, tctx, cols, new_vals, old_vals, run, &mut bound)?;
                let res = execute(eng, ctx, &bound)?;
                run.found = result_affected(&res) > 0;
            }
            TriggerBodyStmt::SelectInto { targets, query } => {
                if targets.is_empty() {
                    return Err(exec_err(
                        "42601",
                        "query has no destination for result data".to_string(),
                    ));
                }
                let mut bound = (**query).clone();
                bind_trigger_stmt(eng, ctx, tctx, cols, new_vals, old_vals, run, &mut bound)?;
                let res = execute(eng, ctx, &bound)?;
                let rows = result_rows(&res);
                run.found = !rows.is_empty();
                // v2.10: a single variable target receiving a
                // multi-column row binds as a record (SELECT INTO
                // rec * FROM ...), readable as var.<col>.
                if targets.len() == 1 {
                    if let TrigIntoTarget::Var(name) = &targets[0] {
                        if let Some(first) = rows.first() {
                            if first.len() > 1 {
                                if let ExecResult::Select { columns, .. } = &res {
                                    let rec: Vec<(String, Value)> = columns
                                        .iter()
                                        .map(|(n, _)| n.clone())
                                        .zip(first[..].iter().cloned())
                                        .collect();
                                    run.recs.insert(name.clone(), rec);
                                    run.vars.insert(
                                        name.clone(),
                                        first.first().cloned().unwrap_or(Value::Null),
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                }
                assign_into_targets(tctx, cols, new_vals, run, targets, &rows)?;
            }
            TriggerBodyStmt::DmlInto { targets, stmt } => {
                let mut bound = (**stmt).clone();
                bind_trigger_stmt(eng, ctx, tctx, cols, new_vals, old_vals, run, &mut bound)?;
                let res = execute(eng, ctx, &bound)?;
                run.found = result_affected(&res) > 0;
                let rows = result_rows(&res);
                assign_into_targets(tctx, cols, new_vals, run, targets, &rows)?;
            }
            TriggerBodyStmt::PerformQuery(stmt) => {
                let mut bound = stmt.clone();
                bind_trigger_stmt(eng, ctx, tctx, cols, new_vals, old_vals, run, &mut bound)?;
                let res = execute(eng, ctx, &bound)?;
                run.found = result_affected(&res) > 0;
            }
            TriggerBodyStmt::Execute(expr) => {
                let v = eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, expr)?;
                let text = value_to_text_cast(&v);
                let mut stmt = crate::sql::parse_statement(&text).map_err(sql_err)?;
                bind_trigger_stmt(eng, ctx, tctx, cols, new_vals, old_vals, run, &mut stmt)?;
                let res = execute(eng, ctx, &stmt)?;
                run.found = result_affected(&res) > 0;
            }
            TriggerBodyStmt::RaiseSqlstate { code } => {
                // ExecError.code is &'static str; a user-chosen
                // SQLSTATE goes through the v2.11 intern table
                // (leaked once per distinct code, not per raise).
                let code: &'static str = intern_sqlstate(code);
                return Err(exec_err(code, String::new()));
            }
            TriggerBodyStmt::ForRange {
                var,
                low,
                high,
                reverse,
                body,
            } => {
                let lo = eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, low)?;
                let hi = eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, high)?;
                let (Some(lo), Some(hi)) = (value_as_i64(&lo), value_as_i64(&hi)) else {
                    return Err(exec_err(
                        "22P02",
                        "FOR loop bounds must be integers".to_string(),
                    ));
                };
                let mut vals: Vec<i64> = if *reverse {
                    (hi..=lo).rev().collect()
                } else {
                    (lo..=hi).collect()
                };
                if vals.len() > 1_000_000 {
                    vals.truncate(1_000_000);
                }
                run.found = !vals.is_empty();
                let saved = run.vars.get(var).cloned();
                for v in vals {
                    run.vars.insert(var.clone(), Value::BigInt(v));
                    let flow =
                        exec_trigger_stmts(eng, ctx, tctx, cols, new_vals, old_vals, run, body)?;
                    if !matches!(flow, TrigFlow::End) {
                        return Ok(flow);
                    }
                }
                match saved {
                    Some(v) => {
                        run.vars.insert(var.clone(), v);
                    }
                    None => {
                        run.vars.remove(var);
                    }
                }
            }
            TriggerBodyStmt::ForQuery { var, source, body } => {
                let stmt = match source {
                    TrigForSource::Query(s) => s.clone(),
                    TrigForSource::Dynamic(expr) => {
                        let v =
                            eval_trigger_expr(eng, ctx, tctx, cols, new_vals, old_vals, run, expr)?;
                        crate::sql::parse_statement(&value_to_text_cast(&v)).map_err(sql_err)?
                    }
                };
                let mut bound = stmt;
                bind_trigger_stmt(eng, ctx, tctx, cols, new_vals, old_vals, run, &mut bound)?;
                let res = execute(eng, ctx, &bound)?;
                let (colnames, rows): (Vec<String>, Vec<Row>) = match &res {
                    ExecResult::Select { columns, rows } => (
                        columns.iter().map(|(n, _)| n.clone()).collect(),
                        rows.clone(),
                    ),
                    ExecResult::Dml { columns, rows, .. } => (
                        columns.iter().map(|(n, _)| n.clone()).collect(),
                        rows.clone(),
                    ),
                    _ => (Vec::new(), Vec::new()),
                };
                run.found = !rows.is_empty();
                let saved_var = run.vars.get(var).cloned();
                let saved_rec = run.recs.get(var).cloned();
                for row in &rows {
                    let rec: Vec<(String, Value)> = colnames
                        .iter()
                        .cloned()
                        .zip(row[..].iter().cloned())
                        .collect();
                    run.recs.insert(var.clone(), rec);
                    let flow =
                        exec_trigger_stmts(eng, ctx, tctx, cols, new_vals, old_vals, run, body)?;
                    if !matches!(flow, TrigFlow::End) {
                        return Ok(flow);
                    }
                }
                match saved_rec {
                    Some(r) => {
                        run.recs.insert(var.clone(), r);
                    }
                    None => {
                        run.recs.remove(var);
                    }
                }
                match saved_var {
                    Some(v) => {
                        run.vars.insert(var.clone(), v);
                    }
                    None => {
                        run.vars.remove(var);
                    }
                }
            }
        }
    }
    Ok(TrigFlow::End)
}

/// v2.10: assign a result set's first row (or all-NULL when empty)
/// to SELECT INTO / RETURNING INTO targets.
fn assign_into_targets(
    tctx: &TrigCtx,
    cols: &[(String, ColType)],
    new_vals: &mut Vec<Value>,
    run: &mut TrigRun,
    targets: &[TrigIntoTarget],
    rows: &[Row],
) -> Result<(), ExecError> {
    let first: Vec<Value> = rows
        .first()
        .map(|r| r[..].to_vec())
        .unwrap_or_else(|| vec![Value::Null; targets.len()]);
    for (i, t) in targets.iter().enumerate() {
        let v = first.get(i).cloned().unwrap_or(Value::Null);
        match t {
            TrigIntoTarget::Var(name) => {
                run.vars.insert(name.clone(), v);
            }
            TrigIntoTarget::NewCol(col) => {
                let idx = cols.iter().position(|(n, _)| n == col).ok_or_else(|| {
                    exec_err(
                        "42703",
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            col, tctx.table
                        ),
                    )
                })?;
                new_vals[idx] = coerce_value(v, &cols[idx].1, col)?;
            }
        }
    }
    Ok(())
}

/// v2.10: rows carried by an ExecResult (empty for plain commands).
fn result_rows(res: &ExecResult) -> Vec<Row> {
    match res {
        ExecResult::Select { rows, .. }
        | ExecResult::Explain { rows, .. }
        | ExecResult::Dml { rows, .. } => rows.clone(),
        ExecResult::Command { .. } => Vec::new(),
    }
}

/// v2.10: the affected/returned row count of an ExecResult, for
/// FOUND: SELECT/PERFORM count result rows; DML counts the tag's
/// trailing count (or RETURNING rows when present).
fn result_affected(res: &ExecResult) -> usize {
    match res {
        ExecResult::Select { rows, .. } | ExecResult::Explain { rows, .. } => rows.len(),
        ExecResult::Dml { tag, rows, .. } => {
            if !rows.is_empty() {
                rows.len()
            } else {
                tag.rsplit(' ')
                    .next()
                    .and_then(|t| t.parse::<usize>().ok())
                    .unwrap_or(0)
            }
        }
        ExecResult::Command { tag } => tag
            .rsplit(' ')
            .next()
            .and_then(|t| t.parse::<usize>().ok())
            .unwrap_or(0),
    }
}

fn value_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::SmallInt(i) => Some(*i as i64),
        Value::Int(i) => Some(*i),
        Value::BigInt(i) => Some(*i),
        _ => None,
    }
}

/// v2.10: bind a statement about to run from a trigger body: rewrite
/// NEW.col / OLD.col / variable / record-field / FOUND references to
/// $N parameters, then fold them to literals with the existing
/// subst_params walker (the plpgsql binding strategy). References
/// the bounded rewriter does not reach stay as column refs and fail
/// or resolve against the statement's own tables, as before.
fn bind_trigger_stmt(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    tctx: &TrigCtx,
    cols: &[(String, ColType)],
    new_vals: &[Value],
    old_vals: &[Value],
    run: &TrigRun,
    stmt: &mut Stmt,
) -> Result<(), ExecError> {
    let _ = (eng, ctx);
    let mut slots: Vec<Option<Value>> = Vec::new();
    // v2.10: `VALUES (NEW.*)` / `(OLD.*)` — a whole-row reference in
    // an INSERT row expands to one parameter per column, in the
    // firing table's column order (PG's NEW.* expansion).
    if let Stmt::Insert { rows, .. } = stmt {
        for row in rows.iter_mut() {
            let mut out: Vec<InsertValue> = Vec::with_capacity(row.len());
            for v in row.drain(..) {
                let whole = match &v {
                    InsertValue::Expr(Expr::WholeRow { qual })
                        if qual.eq_ignore_ascii_case("new") || qual.eq_ignore_ascii_case("old") =>
                    {
                        Some(qual.eq_ignore_ascii_case("new"))
                    }
                    _ => None,
                };
                match whole {
                    Some(is_new) => {
                        let vals = if is_new { new_vals } else { old_vals };
                        for val in vals {
                            slots.push(Some(val.clone()));
                            out.push(InsertValue::Expr(Expr::Param(slots.len() as u32)));
                        }
                    }
                    None => out.push(v),
                }
            }
            *row = out;
        }
    }
    rewrite_stmt_refs(stmt, &mut |e| {
        rewrite_ref_expr(e, tctx, cols, new_vals, old_vals, run, &mut slots)
    });
    if !slots.is_empty() {
        subst_params(stmt, &slots)?;
    }
    Ok(())
}

/// v2.10: apply an expression rewriter to every expression of the
/// statement kinds a trigger body can hold (INSERT/UPDATE/DELETE/
/// SELECT, incl. WITH wrappers handled by subst_params' own CTE
/// walk — here we rewrite the top-level shapes subst_params would
/// visit, using the same traversal discipline: rewrite first into
/// Param placeholders, then let subst_params do the full walk).
fn rewrite_stmt_refs(stmt: &mut Stmt, f: &mut impl FnMut(&Expr) -> Expr) {
    match stmt {
        Stmt::Insert {
            rows,
            select,
            with,
            on_conflict,
            returning,
            ..
        } => {
            let _ = (with, on_conflict, returning);
            for row in rows {
                for v in row {
                    if let InsertValue::Expr(e) = v {
                        *e = f(e);
                    }
                }
            }
            if let Some(sel) = select {
                rewrite_select_refs(sel, f);
            }
        }
        Stmt::Select(sel) => rewrite_select_refs(sel, f),
        Stmt::Update { sets, where_, .. } => {
            for (_, e) in sets {
                *e = f(e);
            }
            if let Some(w) = where_ {
                *w = f(w);
            }
        }
        Stmt::Delete { where_, .. } => {
            if let Some(w) = where_ {
                *w = f(w);
            }
        }
        _ => {}
    }
}

fn rewrite_select_refs(sel: &mut SelectStmt, f: &mut impl FnMut(&Expr) -> Expr) {
    for item in &mut sel.items {
        if let SelectItem::Expr { expr, .. } = item {
            *expr = f(expr);
        }
    }
    if let Some(w) = &mut sel.where_ {
        *w = f(w);
    }
    if let Some(h) = &mut sel.having {
        *h = f(h);
    }
}

/// v2.10: rewrite one expression's NEW/OLD/var/record/FOUND/TG
/// references into $N parameter placeholders, appending each bound
/// value to `slots`. Mirrors subst_tg_vars' variant coverage;
/// anything unhandled is cloned unchanged.
fn rewrite_ref_expr(
    e: &Expr,
    tctx: &TrigCtx,
    cols: &[(String, ColType)],
    new_vals: &[Value],
    old_vals: &[Value],
    run: &TrigRun,
    slots: &mut Vec<Option<Value>>,
) -> Expr {
    let push = |v: Value, slots: &mut Vec<Option<Value>>| -> Expr {
        slots.push(Some(v));
        Expr::Param(slots.len() as u32)
    };
    match e {
        Expr::Column {
            table: Some(q),
            name,
        } if q.eq_ignore_ascii_case("new") || q.eq_ignore_ascii_case("old") => {
            if name == "tableoid" {
                return push(Value::Int(tctx.relid), slots);
            }
            let row = if q.eq_ignore_ascii_case("new") {
                new_vals
            } else {
                old_vals
            };
            match cols.iter().position(|(n, _)| n == name) {
                Some(i) => push(row.get(i).cloned().unwrap_or(Value::Null), slots),
                None => e.clone(),
            }
        }
        Expr::Column {
            table: Some(q),
            name,
        } if run.aliases.contains_key(q.as_str()) => {
            let is_new = run.aliases[q.as_str()];
            let row = if is_new { new_vals } else { old_vals };
            match cols.iter().position(|(n, _)| n == name) {
                Some(i) => push(row.get(i).cloned().unwrap_or(Value::Null), slots),
                None => e.clone(),
            }
        }
        Expr::Column {
            table: Some(q),
            name,
        } => match run.recs.get(q.as_str()) {
            Some(rec) => match rec.iter().find(|(n, _)| n == name) {
                Some((_, v)) => push(v.clone(), slots),
                None => e.clone(),
            },
            None => e.clone(),
        },
        Expr::Column { table: None, name } => {
            if name == "found" {
                return push(Value::Bool(run.found), slots);
            }
            match run.get_var(name) {
                Some(v) => push(v, slots),
                None => match name.as_str() {
                    "tg_name" => push(Value::text(tctx.name.clone()), slots),
                    "tg_table_name" => push(Value::text(tctx.table.clone()), slots),
                    "tg_relname" => push(Value::text(tctx.table.clone()), slots),
                    "tg_table_schema" => push(Value::text("public".to_string()), slots),
                    "tg_when" => push(Value::text(tctx.when_name().to_string()), slots),
                    "tg_op" => push(Value::text(tctx.op_name().to_string()), slots),
                    "tg_level" => push(Value::text(tctx.level_name().to_string()), slots),
                    "tg_nargs" => push(Value::Int(tctx.args.len() as i64), slots),
                    "tg_relid" => push(Value::Int(tctx.relid), slots),
                    _ => e.clone(),
                },
            }
        }
        Expr::Subscript { array, indices } => {
            // TG_ARGV[i] folds to the i-th trigger argument (PG:
            // out-of-range subscripts yield NULL).
            if let Expr::Column { table: None, name } = &**array {
                if name == "tg_argv" && indices.len() == 1 {
                    let idx =
                        rewrite_ref_expr(&indices[0], tctx, cols, new_vals, old_vals, run, slots);
                    let i = match &idx {
                        Expr::Literal(crate::sql::Literal::Int(i)) => Some(*i),
                        Expr::Param(k) => match slots.get((*k as usize).saturating_sub(1)) {
                            Some(Some(Value::Int(i))) => Some(*i),
                            Some(Some(Value::BigInt(i))) => Some(*i),
                            Some(Some(Value::SmallInt(i))) => Some(*i as i64),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(i) = i {
                        return match tctx.args.get(i as usize) {
                            Some(a) => push(Value::text(a.clone()), slots),
                            None => push(Value::Null, slots),
                        };
                    }
                }
            }
            Expr::Subscript {
                array: Box::new(rewrite_ref_expr(
                    array, tctx, cols, new_vals, old_vals, run, slots,
                )),
                indices: indices
                    .iter()
                    .map(|x| rewrite_ref_expr(x, tctx, cols, new_vals, old_vals, run, slots))
                    .collect(),
            }
        }
        Expr::Arith { op, left, right } => Expr::Arith {
            op: *op,
            left: Box::new(rewrite_ref_expr(
                left, tctx, cols, new_vals, old_vals, run, slots,
            )),
            right: Box::new(rewrite_ref_expr(
                right, tctx, cols, new_vals, old_vals, run, slots,
            )),
        },
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: Box::new(rewrite_ref_expr(
                left, tctx, cols, new_vals, old_vals, run, slots,
            )),
            right: Box::new(rewrite_ref_expr(
                right, tctx, cols, new_vals, old_vals, run, slots,
            )),
        },
        Expr::And(a, b) => Expr::And(
            Box::new(rewrite_ref_expr(
                a, tctx, cols, new_vals, old_vals, run, slots,
            )),
            Box::new(rewrite_ref_expr(
                b, tctx, cols, new_vals, old_vals, run, slots,
            )),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(rewrite_ref_expr(
                a, tctx, cols, new_vals, old_vals, run, slots,
            )),
            Box::new(rewrite_ref_expr(
                b, tctx, cols, new_vals, old_vals, run, slots,
            )),
        ),
        Expr::Not(a) => Expr::Not(Box::new(rewrite_ref_expr(
            a, tctx, cols, new_vals, old_vals, run, slots,
        ))),
        Expr::Neg(a) => Expr::Neg(Box::new(rewrite_ref_expr(
            a, tctx, cols, new_vals, old_vals, run, slots,
        ))),
        Expr::BitNot(a) => Expr::BitNot(Box::new(rewrite_ref_expr(
            a, tctx, cols, new_vals, old_vals, run, slots,
        ))),
        Expr::Cast { expr, to, written } => Expr::Cast {
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
            to: *to,
            written: written.clone(),
        },
        Expr::IsNull { expr, neg } => Expr::IsNull {
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
            neg: *neg,
        },
        Expr::IsBool { expr, neg, val } => Expr::IsBool {
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
            neg: *neg,
            val: *val,
        },
        Expr::IsDistinctFrom { left, right, neg } => Expr::IsDistinctFrom {
            left: Box::new(rewrite_ref_expr(
                left, tctx, cols, new_vals, old_vals, run, slots,
            )),
            right: Box::new(rewrite_ref_expr(
                right, tctx, cols, new_vals, old_vals, run, slots,
            )),
            neg: *neg,
        },
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => Expr::Between {
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
            low: Box::new(rewrite_ref_expr(
                low, tctx, cols, new_vals, old_vals, run, slots,
            )),
            high: Box::new(rewrite_ref_expr(
                high, tctx, cols, new_vals, old_vals, run, slots,
            )),
            neg: *neg,
        },
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => Expr::Like {
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
            pattern: Box::new(rewrite_ref_expr(
                pattern, tctx, cols, new_vals, old_vals, run, slots,
            )),
            not: *not,
            ilike: *ilike,
            escape: escape.as_ref().map(|x| {
                Box::new(rewrite_ref_expr(
                    x, tctx, cols, new_vals, old_vals, run, slots,
                ))
            }),
        },
        Expr::Concat(a, b) => Expr::Concat(
            Box::new(rewrite_ref_expr(
                a, tctx, cols, new_vals, old_vals, run, slots,
            )),
            Box::new(rewrite_ref_expr(
                b, tctx, cols, new_vals, old_vals, run, slots,
            )),
        ),
        Expr::Row(items) => Expr::Row(
            items
                .iter()
                .map(|a| rewrite_ref_expr(a, tctx, cols, new_vals, old_vals, run, slots))
                .collect(),
        ),
        Expr::FieldAccess { expr, field } => Expr::FieldAccess {
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
            field: field.clone(),
        },
        Expr::JsonOp { op, left, right } => Expr::JsonOp {
            op: *op,
            left: Box::new(rewrite_ref_expr(
                left, tctx, cols, new_vals, old_vals, run, slots,
            )),
            right: Box::new(rewrite_ref_expr(
                right, tctx, cols, new_vals, old_vals, run, slots,
            )),
        },
        Expr::Regex {
            expr,
            pattern,
            not,
            case_insensitive,
        } => Expr::Regex {
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
            pattern: Box::new(rewrite_ref_expr(
                pattern, tctx, cols, new_vals, old_vals, run, slots,
            )),
            not: *not,
            case_insensitive: *case_insensitive,
        },
        Expr::Extract { field, from } => Expr::Extract {
            field: field.clone(),
            from: Box::new(rewrite_ref_expr(
                from, tctx, cols, new_vals, old_vals, run, slots,
            )),
        },
        Expr::AtTimeZone { expr, zone } => Expr::AtTimeZone {
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
            zone: zone.as_ref().map(|z| {
                Box::new(rewrite_ref_expr(
                    z, tctx, cols, new_vals, old_vals, run, slots,
                ))
            }),
        },
        Expr::ArrayCtor { elems, nested } => Expr::ArrayCtor {
            elems: elems
                .iter()
                .map(|a| rewrite_ref_expr(a, tctx, cols, new_vals, old_vals, run, slots))
                .collect(),
            nested: *nested,
        },
        Expr::UserOp { op, left, right } => Expr::UserOp {
            op: op.clone(),
            left: Box::new(rewrite_ref_expr(
                left, tctx, cols, new_vals, old_vals, run, slots,
            )),
            right: Box::new(rewrite_ref_expr(
                right, tctx, cols, new_vals, old_vals, run, slots,
            )),
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| rewrite_ref_expr(a, tctx, cols, new_vals, old_vals, run, slots))
                .collect(),
        },
        Expr::NamedArg { name, expr } => Expr::NamedArg {
            name: name.clone(),
            expr: Box::new(rewrite_ref_expr(
                expr, tctx, cols, new_vals, old_vals, run, slots,
            )),
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: operand.as_ref().map(|x| {
                Box::new(rewrite_ref_expr(
                    x, tctx, cols, new_vals, old_vals, run, slots,
                ))
            }),
            whens: whens
                .iter()
                .map(|(a, b)| {
                    (
                        Box::new(rewrite_ref_expr(
                            a, tctx, cols, new_vals, old_vals, run, slots,
                        )),
                        Box::new(rewrite_ref_expr(
                            b, tctx, cols, new_vals, old_vals, run, slots,
                        )),
                    )
                })
                .collect(),
            else_: else_.as_ref().map(|x| {
                Box::new(rewrite_ref_expr(
                    x, tctx, cols, new_vals, old_vals, run, slots,
                ))
            }),
        },
        other => other.clone(),
    }
}

/// v2.09: the plpgsql condition name for a SQLSTATE (the subset the
/// suite's EXCEPTION blocks name), for handler matching.
fn condition_name_for_sqlstate(code: &str) -> Option<&'static str> {
    Some(match code {
        "22012" => "division_by_zero",
        "23505" => "unique_violation",
        "23503" => "foreign_key_violation",
        "23514" => "check_violation",
        "23502" => "not_null_violation",
        "42P01" => "undefined_table",
        "42703" => "undefined_column",
        "42883" => "undefined_function",
        "42601" => "syntax_error",
        "22P02" => "invalid_text_representation",
        "22003" => "numeric_value_out_of_range",
        "P0001" => "raise_exception",
        _ => return None,
    })
}

/// v1.00/v2.09: evaluate a trigger-body expression with NEW and OLD
/// in scope (qualified `new` / `old`; bare columns resolve against
/// NEW first, the v1.00 behavior) plus the TG_* variables.
pub(crate) fn eval_trigger_expr(
    eng: &mut Engine,
    ctx: &StmtCtx,
    tctx: &TrigCtx,
    cols: &[(String, ColType)],
    new_row: &[Value],
    old_row: &[Value],
    run: &TrigRun,
    expr: &Expr,
) -> Result<Value, ExecError> {
    // v2.09: TG_ARGV[i] — the trigger's EXECUTE arguments as text.
    if let Expr::Subscript { array, indices } = expr {
        if let Expr::Column { table: None, name } = &**array {
            if name == "tg_argv" && indices.len() == 1 {
                let iv =
                    eval_trigger_expr(eng, ctx, tctx, cols, new_row, old_row, run, &indices[0])?;
                let idx = match iv {
                    Value::Int(i) => i,
                    Value::SmallInt(i) => i as i64,
                    Value::BigInt(i) => i,
                    _ => {
                        return Err(exec_err(
                            "22023",
                            "TG_ARGV index must be an integer".to_string(),
                        ));
                    }
                };
                return Ok(tctx
                    .args
                    .get(idx as usize)
                    .map(|a| Value::text(a.clone()))
                    .unwrap_or(Value::Null));
            }
        }
    }
    // v2.10: bare NEW / OLD read as whole-row records (whole-row
    // assignment and record-valued function arguments).
    if let Expr::Column { table: None, name } = expr {
        if name == "new" || name == "old" {
            let row = if name == "new" { new_row } else { old_row };
            return Ok(Value::Record(
                cols.iter()
                    .map(|(n, _)| n.clone())
                    .zip(row.iter().cloned())
                    .collect(),
            ));
        }
    }
    // TG_* variables are expression leaves, not columns.
    if let Expr::Column { table: None, name } = expr {
        let v = match name.as_str() {
            "tg_name" => Some(Value::text(tctx.name.clone())),
            "tg_table_name" => Some(Value::text(tctx.table.clone())),
            "tg_relname" => Some(Value::text(tctx.table.clone())),
            "tg_table_schema" => Some(Value::text("public".to_string())),
            "tg_when" => Some(Value::text(tctx.when_name().to_string())),
            "tg_op" => Some(Value::text(tctx.op_name().to_string())),
            "tg_level" => Some(Value::text(tctx.level_name().to_string())),
            "tg_nargs" => Some(Value::Int(tctx.args.len() as i64)),
            "tg_relid" => Some(Value::Int(tctx.relid)),
            _ => None,
        };
        if let Some(v) = v {
            return Ok(v);
        }
    }
    // v2.10: `(ps).pfname`-style field access on NEW/OLD/alias/
    // record bases parses as FieldAccess over a bare column.
    if let Expr::FieldAccess { expr, field } = expr {
        if let Expr::Column { table: None, name } = &**expr {
            let row: Option<&[Value]> = if name == "new" {
                Some(new_row)
            } else if name == "old" {
                Some(old_row)
            } else if let Some(is_new) = run.aliases.get(name.as_str()) {
                Some(if *is_new { new_row } else { old_row })
            } else {
                None
            };
            if let Some(row) = row {
                return Ok(cols
                    .iter()
                    .position(|(n, _)| n == field)
                    .map(|i| row.get(i).cloned().unwrap_or(Value::Null))
                    .unwrap_or(Value::Null));
            }
            if let Some(rec) = run.recs.get(name.as_str()) {
                return Ok(rec
                    .iter()
                    .find(|(n, _)| n == field)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(Value::Null));
            }
            if let Some(Value::Record(fields)) = run.vars.get(name.as_str()) {
                return Ok(fields
                    .iter()
                    .find(|(n, _)| n == field)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(Value::Null));
            }
        }
    }
    // v2.10: FOUND, declared variables, and record fields are
    // expression leaves too.
    if let Expr::Column { table: None, name } = expr {
        if name == "found" {
            return Ok(Value::Bool(run.found));
        }
        if let Some(v) = run.get_var(name) {
            return Ok(v);
        }
    }
    if let Expr::Column {
        table: Some(q),
        name,
    } = expr
    {
        if let Some(is_new) = run.aliases.get(q.as_str()) {
            let row = if *is_new { new_row } else { old_row };
            return Ok(cols
                .iter()
                .position(|(n, _)| n == name)
                .map(|i| row.get(i).cloned().unwrap_or(Value::Null))
                .unwrap_or(Value::Null));
        }
        if let Some(rec) = run.recs.get(q.as_str()) {
            return Ok(rec
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or(Value::Null));
        }
    }
    // v2.10: fold variable/record/FOUND references inside compound
    // expressions to literals (via the $N rewrite + subst_expr),
    // since the row evaluator only knows columns.
    let owned_v;
    let expr = if expr_has_run_ref(expr, run) {
        let mut slots: Vec<Option<Value>> = Vec::new();
        let mut rw = rewrite_ref_expr(expr, tctx, cols, new_row, old_row, run, &mut slots);
        subst_expr(&mut rw, &slots)?;
        owned_v = rw;
        &owned_v
    } else {
        expr
    };
    // TG_* variables can appear anywhere inside a compound
    // expression (e.g. `TG_LEVEL = 'ROW'`); fold them to literals
    // first, since the row evaluator only knows columns.
    let owned;
    let expr = if expr_has_tg_var(expr) {
        owned = subst_tg_vars(expr, tctx);
        &owned
    } else {
        expr
    };
    let mk_schema = |qual: &str| -> Vec<QCol> {
        cols.iter()
            .enumerate()
            .map(|(i, (n, ty))| QCol {
                qual: qual.to_string(),
                name: n.clone(),
                ty: *ty,
                hidden: false,
                src_ord: i as u32,
            })
            .collect()
    };
    let schema_new = mk_schema("new");
    let schema_old = mk_schema("old");
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        role: ctx.role,
        read_only: false,
        priv_scopes: Vec::new(),
        priv_role_override: None,
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: None,
        write: None,
    };
    let scope_new = Scope {
        schema: &schema_new,
        row: new_row,
        prov: None,
    };
    let scope_old = Scope {
        schema: &schema_old,
        row: old_row,
        prov: None,
    };
    eval_expr(&mut q, &[scope_new, scope_old], expr)
}

/// v2.09: evaluate a trigger's WHEN clause for one row pair (PG19
/// ExecCallTriggerFunc: the qual is evaluated with OLD/NEW in scope;
/// NULL counts as false). No WHEN = always fire.
pub(crate) fn trigger_when_matches(
    eng: &mut Engine,
    ctx: &StmtCtx,
    tctx: &TrigCtx,
    cols: &[(String, ColType)],
    new_row: Option<&[Value]>,
    old_row: Option<&[Value]>,
    when: Option<&Expr>,
) -> Result<bool, ExecError> {
    let Some(w) = when else { return Ok(true) };
    let nulls: Vec<Value> = vec![Value::Null; cols.len()];
    let v = eval_trigger_expr(
        eng,
        ctx,
        tctx,
        cols,
        new_row.unwrap_or(&nulls),
        old_row.unwrap_or(&nulls),
        &TrigRun::default(),
        w,
    )?;
    Ok(matches!(v, Value::Bool(true)))
}

/// v2.09: every trigger definition that applies to rows landing in
/// `leaf`: the leaf's own triggers plus the triggers of every
/// partitioned ancestor (PG19 clones a parent's triggers onto its
/// partitions). Returns (defining table, definition) pairs, the
/// leaf's own first, in creation order per table.
pub(crate) fn trigger_defs_for_leaf(
    eng: &Engine,
    ctx: &StmtCtx,
    leaf: &str,
) -> Vec<(String, TriggerDef)> {
    let mut out = Vec::new();
    if let Some(t) = eng
        .db
        .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
    {
        for tr in &t.triggers {
            out.push((leaf.to_string(), tr.clone()));
        }
    }
    let mut names: Vec<&String> = eng.db.tables.keys().collect();
    names.sort();
    for name in names {
        if name == leaf {
            continue;
        }
        let Some(t) = eng
            .db
            .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        else {
            continue;
        };
        if t.partition.is_none() || t.triggers.is_empty() {
            continue;
        }
        if partition_tree_contains(eng, ctx, name, leaf, 0) {
            for tr in &t.triggers {
                out.push((name.clone(), tr.clone()));
            }
        }
    }
    out
}

fn partition_tree_contains(
    eng: &Engine,
    ctx: &StmtCtx,
    root: &str,
    leaf: &str,
    depth: usize,
) -> bool {
    if depth > 32 {
        return false;
    }
    let Some(t) = eng
        .db
        .find_table(root, ctx.snap, &ctx.all_xids, ctx.session)
    else {
        return false;
    };
    let Some(pi) = &t.partition else { return false };
    for c in &pi.children {
        if c == leaf || partition_tree_contains(eng, ctx, c, leaf, depth + 1) {
            return true;
        }
    }
    false
}

/// v2.09: fire (or queue, for deferred constraint triggers) the
/// AFTER FOR EACH ROW triggers applying to `leaf` for one event.
/// `pairs` are (OLD, NEW) images in `cols` order; either side is
/// None for INSERT/DELETE respectively. `set_cols` is the UPDATE
/// statement's SET column list (for UPDATE OF matching; None uses
/// value-change detection). Deferred constraint triggers queue into
/// the v2.06 deferred state and fire from flush_deferred_fk.
pub(crate) fn fire_after_row_triggers(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    leaf: &str,
    cols: &[(String, ColType)],
    event: u8,
    pairs: &[(Option<Row>, Option<Row>)],
    set_cols: Option<&[String]>,
) -> Result<(), ExecError> {
    stmt_images_push(leaf, cols, pairs);
    let defs = trigger_defs_for_leaf(eng, ctx, leaf);
    for (def_table, tr) in &defs {
        if tr.timing != TriggerTiming::After || !tr.for_each_row || !tr.enforced || !tr.fires() {
            continue;
        }
        if (tr.events & event) == 0 {
            continue;
        }
        if event == trig_event::UPDATE && !tr.update_cols.is_empty() {
            let hit = match set_cols {
                Some(cols_set) => tr.update_cols.iter().any(|c| cols_set.contains(c)),
                None => pairs.iter().any(|(o, n)| {
                    tr.update_cols.iter().any(|c| {
                        let idx = cols.iter().position(|(cn, _)| cn == c);
                        match (idx, o, n) {
                            (Some(i), Some(o), Some(n)) => o[i] != n[i],
                            _ => true,
                        }
                    })
                }),
            };
            if !hit {
                continue;
            }
        }
        let tctx = TrigCtx {
            name: tr.name.clone(),
            table: leaf.to_string(),
            timing: tr.timing,
            event,
            args: tr.args.clone(),
            relid: eng
                .db
                .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .map(|t| t.oid as i64)
                .unwrap_or(0),
            for_each_row: tr.for_each_row,
        };
        // v2.14: transition tables are materialized once per
        // (trigger, statement) and visible to every row firing.
        let installed = if tr.has_transition {
            install_transition_tables(eng, ctx, cols, pairs, tr)?
        } else {
            Vec::new()
        };
        let mut fire_res: Result<(), ExecError> = Ok(());
        for (old, new) in pairs {
            let old_ref = old.as_ref().map(|r| &r[..]);
            let new_ref = new.as_ref().map(|r| &r[..]);
            let m = match trigger_when_matches(
                eng,
                ctx,
                &tctx,
                cols,
                new_ref,
                old_ref,
                tr.when_expr.as_ref(),
            ) {
                Ok(m) => m,
                Err(e) => {
                    fire_res = Err(e);
                    break;
                }
            };
            if !m {
                continue;
            }
            if tr.is_constraint && trig_mode_deferred(eng, ctx.session, def_table, tr) {
                queue_trig(
                    eng,
                    ctx.session,
                    def_table,
                    leaf,
                    &tr.name,
                    event,
                    old.clone(),
                    new.clone(),
                );
                continue;
            }
            if let Err(e) = fire_one_trigger(eng, ctx, &tctx, tr, cols, new_ref, old_ref) {
                fire_res = Err(e);
                break;
            }
        }
        drop_transition_tables(eng, ctx, installed);
        fire_res?;
    }
    Ok(())
}

/// v2.10: cheap per-leaf check used by the UPDATE/DELETE plan loops
/// to skip the BEFORE-trigger hook entirely when no BEFORE row
/// trigger for the event exists on the leaf or its ancestors.
pub(crate) fn leaf_has_before_triggers(eng: &Engine, ctx: &StmtCtx, leaf: &str, event: u8) -> bool {
    trigger_defs_for_leaf(eng, ctx, leaf).iter().any(|(_, tr)| {
        tr.timing == TriggerTiming::Before
            && tr.for_each_row
            && tr.enforced
            && tr.fires()
            && !tr.has_transition
            && (tr.events & event) != 0
    })
}

/// v2.10 (secondary): fire the BEFORE UPDATE FOR EACH ROW triggers
/// for one planned row pair (PG19 ExecBRUpdateTriggers). Returns the
/// (possibly trigger-modified) NEW image, or None when a body
/// returned NULL (the row is skipped). UPDATE OF matching uses the
/// statement's SET column list, as for AFTER triggers.
pub(crate) fn fire_before_update_triggers(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    leaf: &str,
    cols: &[(String, ColType)],
    old_row: &[Value],
    new_row: &[Value],
    set_cols: &[String],
) -> Result<Option<Vec<Value>>, ExecError> {
    let defs = trigger_defs_for_leaf(eng, ctx, leaf);
    let mut cur: Vec<Value> = new_row.to_vec();
    for (def_table, tr) in &defs {
        let _ = def_table;
        if tr.timing != TriggerTiming::Before
            || !tr.for_each_row
            || !tr.enforced
            || !tr.fires()
            || tr.has_transition
            || (tr.events & trig_event::UPDATE) == 0
        {
            continue;
        }
        if !tr.update_cols.is_empty() && !tr.update_cols.iter().any(|c| set_cols.contains(c)) {
            continue;
        }
        // v2.14: PG's built-in suppress_redundant_updates_trigger():
        // return NULL (skip the row) when NEW equals OLD in every
        // column, else NEW unchanged (trigger.c builtin body).
        if tr.function == "suppress_redundant_updates_trigger"
            && find_function_by_signature(eng, &tr.function, &[]).is_none()
        {
            if old_row.len() == cur.len() && old_row.iter().zip(cur.iter()).all(|(a, b)| a == b) {
                return Ok(None);
            }
            continue;
        }
        let tctx = TrigCtx {
            name: tr.name.clone(),
            table: leaf.to_string(),
            timing: tr.timing,
            event: trig_event::UPDATE,
            args: tr.args.clone(),
            relid: eng
                .db
                .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .map(|t| t.oid as i64)
                .unwrap_or(0),
            for_each_row: tr.for_each_row,
        };
        if !trigger_when_matches(
            eng,
            ctx,
            &tctx,
            cols,
            Some(&cur),
            Some(old_row),
            tr.when_expr.as_ref(),
        )? {
            continue;
        }
        let fdef = find_function_by_signature(eng, &tr.function, &[]).ok_or_else(|| {
            exec_err(
                "42883",
                format!("function {}() does not exist", tr.function),
            )
        })?;
        let body = parse_trigger_body(&fdef.body).map_err(|e| {
            exec_err(
                e.code,
                format!("invalid trigger function body: {}", e.message),
            )
        })?;
        match exec_trigger_body(eng, ctx, &tctx, cols, Some(&cur), Some(old_row), &body)? {
            Some(nv) => cur = nv,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// v2.10 (secondary): fire the BEFORE DELETE FOR EACH ROW triggers
/// for one planned row (PG19 ExecBRDeleteTriggers). Returns false
/// when a body returned NULL (the row is not deleted).
pub(crate) fn fire_before_delete_triggers(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    leaf: &str,
    cols: &[(String, ColType)],
    old_row: &[Value],
) -> Result<bool, ExecError> {
    let defs = trigger_defs_for_leaf(eng, ctx, leaf);
    for (_def_table, tr) in &defs {
        if tr.timing != TriggerTiming::Before
            || !tr.for_each_row
            || !tr.enforced
            || !tr.fires()
            || tr.has_transition
            || (tr.events & trig_event::DELETE) == 0
        {
            continue;
        }
        let tctx = TrigCtx {
            name: tr.name.clone(),
            table: leaf.to_string(),
            timing: tr.timing,
            event: trig_event::DELETE,
            args: tr.args.clone(),
            relid: eng
                .db
                .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .map(|t| t.oid as i64)
                .unwrap_or(0),
            for_each_row: tr.for_each_row,
        };
        if !trigger_when_matches(
            eng,
            ctx,
            &tctx,
            cols,
            None,
            Some(old_row),
            tr.when_expr.as_ref(),
        )? {
            continue;
        }
        let fdef = find_function_by_signature(eng, &tr.function, &[]).ok_or_else(|| {
            exec_err(
                "42883",
                format!("function {}() does not exist", tr.function),
            )
        })?;
        let body = parse_trigger_body(&fdef.body).map_err(|e| {
            exec_err(
                e.code,
                format!("invalid trigger function body: {}", e.message),
            )
        })?;
        if exec_trigger_body(eng, ctx, &tctx, cols, None, Some(old_row), &body)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// v2.09: run one AFTER trigger body for one row pair (the return
/// value is ignored for AFTER triggers, per PG).
pub(crate) fn fire_one_trigger(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    tctx: &TrigCtx,
    tr: &TriggerDef,
    cols: &[(String, ColType)],
    new_row: Option<&[Value]>,
    old_row: Option<&[Value]>,
) -> Result<(), ExecError> {
    let fdef = find_function_by_signature(eng, &tr.function, &[]).ok_or_else(|| {
        exec_err(
            "42883",
            format!("function {}() does not exist", tr.function),
        )
    })?;
    let body = parse_trigger_body(&fdef.body).map_err(|e| {
        exec_err(
            e.code,
            format!("invalid trigger function body: {}", e.message),
        )
    })?;
    let _ = exec_trigger_body(eng, ctx, tctx, cols, new_row, old_row, &body)?;
    Ok(())
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
        priv_role_override: None,
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
    // v2.23: the key's PG row hash when the method is Hash
    // (computed by the caller, which owns the &mut Engine the
    // hash may need for user-function calls).
    row_hash: Option<u64>,
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
        if bound_contains_hash(&pinfo.method, cbound, key, row_hash) {
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
    // v2.23: for HASH routing, the PG row hash is computed once
    // per level (it may call user hash functions).
    let row_hash = if pinfo.method == crate::storage::PartMethod::Hash {
        Some(partition_row_hash(eng, ctx, pinfo, table_cols, &key)?)
    } else {
        None
    };
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
                !explicit_sibling_accepts(eng, ctx, pinfo, child_name, &key, row_hash)?
            } else {
                let cbound = cinfo_ref.bound.as_ref().expect("child has bound");
                bound_contains_hash(&pinfo.method, cbound, &key, row_hash)
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

/// v2.20: destination leaf for a DML statement's new row image whose
/// target is `table` (UPDATE / MERGE paths). Three target shapes:
/// - a leaf partition (bound, no children, not is_partitioned): the
///   row cannot move — it must satisfy the leaf's whole bound chain
///   ([`check_leaf_bound`], 23514 on violation) and stays in place;
/// - a sub-partitioned intermediate (bound + children): the image
///   must still satisfy the intermediate's own ancestor bound
///   chain, then routes within its subtree;
/// - a partitioned root (no bound): routes as before.
/// Before v2.20 the callers handed every target to
/// [`find_partition_leaf`] as if it were a root, so direct DML on
/// any leaf with a matched row failed 23514 "no partition of
/// relation \"<leaf>\" found for row".
pub(crate) fn route_dml_destination(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    pinfo: &PartitionInfo,
    table_cols: &[(String, ColType)],
    row: &[Value],
) -> Result<String, ExecError> {
    let is_leaf_target =
        pinfo.bound.is_some() && pinfo.children.is_empty() && !pinfo.is_partitioned;
    if is_leaf_target {
        check_leaf_bound(eng, ctx, table, pinfo, table_cols, row)?;
        return Ok(table.to_string());
    }
    if pinfo.bound.is_some() {
        check_leaf_bound(eng, ctx, table, pinfo, table_cols, row)?;
    }
    find_partition_leaf(eng, ctx, table, pinfo, table_cols, row)
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
        // v2.23: HASH levels judge by the PG row hash.
        let row_hash = if parent_pinfo.method == crate::storage::PartMethod::Hash {
            Some(partition_row_hash(
                eng,
                ctx,
                &parent_pinfo,
                &parent_cols,
                &key,
            )?)
        } else {
            None
        };
        let bound = cur_pinfo.bound.as_ref().expect("partition has bound");
        let ok = if cur_pinfo.is_default {
            !explicit_sibling_accepts(eng, ctx, &parent_pinfo, &cur_name, &key, row_hash)?
        } else {
            bound_contains_hash(&parent_pinfo.method, bound, &key, row_hash)
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
            // v1.93: the declared bit typmod rides along too.
            def.bit_typmods
                .push(src.bit_typmods.get(i).copied().unwrap_or(None));
            def.domain_elem
                .push(src.domain_elem.get(i).copied().unwrap_or(false));
            // v2.21: the column's collation rides along (part of
            // the column's type identity in PG, not an INCLUDING
            // option).
            if def.collations.len() < def.columns.len() - 1 {
                def.collations.resize(def.columns.len() - 1, None);
            }
            def.collations
                .push(src.col_collation(i).map(|s| s.to_string()));
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
                    no_inherit: ck.no_inherit,
                    enforced: true,
                    is_local: true,
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
                    deferrable: u.deferrable,
                    initially_deferred: u.initially_deferred,
                    nulls_not_distinct: u.nulls_not_distinct,
                    is_local: true,
                });
            }
            if def.pkey.is_none() {
                if let Some(pk) = &src.pkey {
                    def.pkey = Some(UniqueDef {
                        name: pk.name.clone(),
                        cols: pk.cols.clone(),
                        deferrable: pk.deferrable,
                        initially_deferred: pk.initially_deferred,
                        nulls_not_distinct: pk.nulls_not_distinct,
                        is_local: true,
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
    // v2.21: column collation names must resolve (42704).
    for c in like_def.collations.iter().flatten() {
        crate::exec::collate::validate_collation_name(&eng.db, c)?;
    }
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
                // v1.88: a user-defined range type column stores as the
                // range/multirange ColType; the type name stays in
                // composite_types (like composites) so casts and
                // typeof can find it.
                Some(st) if st.range.is_some() => {
                    let (elem, multi) = st.range.expect("range branch has a def");
                    like_def.columns[i].1 = if multi {
                        ColType::Multirange(elem)
                    } else {
                        ColType::Range(elem)
                    };
                }
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
        // v1.97: PG19 — a column that is both locally defined (with
        // a NO INHERIT not-null constraint) and inherited cannot be
        // defined: the NO INHERIT declaration would be overridden
        // by the inherited definition.
        {
            let mut parent_cols: Vec<String> = Vec::new();
            for pname in &like_def.inherits {
                if let Some(pt) = eng
                    .db
                    .find_table(pname, ctx.snap, &ctx.all_xids, ctx.session)
                {
                    parent_cols.extend(pt.columns.iter().map(|(n, _)| n.clone()));
                }
            }
            for c in &like_def.checks {
                if c.kind == crate::sql::CheckKind::NotNull && c.no_inherit {
                    let mut refs: Vec<(Option<String>, String)> = Vec::new();
                    crate::sql::collect_col_refs(&c.expr, &mut refs);
                    for (_, col) in &refs {
                        if parent_cols.contains(col)
                            && like_def.columns.iter().any(|(n, _)| n == col)
                        {
                            return Err(exec_err(
                                "42P16",
                                format!(
                                    "cannot define not-null constraint with NO INHERIT on column \"{}\"",
                                    col
                                ),
                            ));
                        }
                    }
                }
            }
        }
        let mut merged = TableDef::empty();
        // v2.16: generation metadata carried alongside the merge
        // (TableDef merging does not track gen_cols): a column
        // inherited with differing generation KINDS is a 42804
        // conflict (PG19 MergeChildAttribute); differing generation
        // EXPRESSIONS are a 42P16 conflict unless the child's own
        // column definition resolves them (PG19's bogus-marker
        // pass in DefineRelation).
        let mut merged_gen: Vec<Option<(crate::sql::Expr, bool)>> = Vec::new();
        // v2.27: not-null locality parallel to the merged columns
        // (false = the not-null exists only by inheritance).
        let mut merged_nn: Vec<bool> = Vec::new();
        // v2.27: attribute locality parallel to the merged columns
        // (false = inherited-only; a local redeclaration merges and
        // stays local, PG19 MergeChildAttribute).
        let mut merged_att: Vec<bool> = Vec::new();
        // v2.16 finishing pass: effective per-column storage carried
        // alongside the merge for the same reason (PG19
        // MergeChildAttribute storage conflicts, 42804).
        let mut merged_storage: Vec<u8> = Vec::new();
        let mut gen_expr_conflict: Option<String> = None;
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
                        && merged.bit_typmods[pos]
                            == parent.bit_typmods.get(i).copied().unwrap_or(None)
                        && merged.domain_elem[pos]
                            == parent.domain_elem.get(i).copied().unwrap_or(false);
                    if !same {
                        return Err(exec_err(
                            "42804",
                            format!("column \"{}\" has a type conflict", col),
                        ));
                    }
                    // v2.16: generation conflicts across parents
                    // (PG19 MergeChildAttribute / DefineRelation).
                    let pgen = parent.gen_cols.get(i).cloned().unwrap_or(None);
                    match (&merged_gen[pos], &pgen) {
                        (a, b) if a.is_some() != b.is_some() => {
                            return Err(exec_err(
                                "42804",
                                format!("inherited column \"{}\" has a generation conflict", col),
                            ));
                        }
                        (Some((ea, sa)), Some((eb, sb))) => {
                            if sa != sb {
                                return Err(exec_err(
                                    "42804",
                                    format!(
                                        "inherited column \"{}\" has a generation conflict",
                                        col
                                    ),
                                ));
                            }
                            if ea != eb && gen_expr_conflict.is_none() {
                                gen_expr_conflict = Some(col.clone());
                            }
                        }
                        _ => {}
                    }
                    // v2.16 finishing pass: storage conflicts across
                    // parents (PG19 MergeChildAttribute, 42804) — the
                    // create_table_like inh_error1 case (ctlt1.a MAIN
                    // versus ctlt4.a EXTENDED).
                    if let Some(ps) = parent.col_storage.get(i).copied() {
                        if merged_storage[pos] != ps {
                            return Err(exec_err(
                                "42804",
                                format!(
                                    "inherited column \"{}\" has a storage parameter conflict",
                                    col
                                ),
                            ));
                        }
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
                // v1.93: inherit the declared bit typmod too.
                merged
                    .bit_typmods
                    .push(parent.bit_typmods.get(i).copied().unwrap_or(None));
                merged
                    .domain_elem
                    .push(parent.domain_elem.get(i).copied().unwrap_or(false));
                merged
                    .not_null
                    .push(parent.not_null.get(i).copied().unwrap_or(false));
                // v2.27: an inherited not-null is non-local.
                merged_nn.push(!parent.not_null.get(i).copied().unwrap_or(false));
                merged_att.push(false);
                merged
                    .defaults
                    .push(parent.defaults.get(i).cloned().unwrap_or(None));
                merged.serial.push(None);
                merged.compression.push(None);
                merged.storage.push(None);
                merged_gen.push(parent.gen_cols.get(i).cloned().unwrap_or(None));
                merged_storage.push(
                    parent
                        .col_storage
                        .get(i)
                        .copied()
                        .unwrap_or_else(|| ty.default_toast_storage()),
                );
            }
            // v2.16: NO INHERIT constraints do not propagate to
            // children (PG19 connoinherit) — neither CHECKs nor
            // not-null constraint entries.
            // v2.27: inherited copies are non-local; the same
            // constraint arriving from two parents merges into ONE
            // entry (its derived coninhcount counts both parents).
            for pc in parent.checks.iter().filter(|c| !c.no_inherit) {
                let dup = merged.checks.iter().any(|c| {
                    c.name == pc.name
                        && c.kind == pc.kind
                        && format!("{:?}", c.expr) == format!("{:?}", pc.expr)
                });
                if !dup {
                    let mut copy = pc.clone();
                    copy.is_local = false;
                    merged.checks.push(copy);
                }
            }
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
                && merged.bit_typmods[pos] == like_def.bit_typmods.get(i).copied().unwrap_or(None)
                && merged.domain_elem[pos] == like_def.domain_elem.get(i).copied().unwrap_or(false);
            if !same {
                return Err(exec_err(
                    "42804",
                    format!("column \"{}\" has a type conflict", col),
                ));
            }
            // v2.16: the child's own column vs the inherited
            // generation kind (PG19 MergeChildAttribute, 42804).
            {
                let lgen = like_def.gen_cols.get(i).cloned().unwrap_or(None);
                let mgen = &merged_gen[pos];
                let kind_diff = match (mgen, &lgen) {
                    (None, None) => false,
                    (Some((_, s1)), Some((_, s2))) => s1 != s2,
                    _ => true,
                };
                if kind_diff {
                    return Err(exec_err(
                        "42804",
                        format!("inherited column \"{}\" has a generation conflict", col),
                    ));
                }
            }
            // v2.16 finishing pass: the child's own specified storage
            // (own STORAGE clause or LIKE ... INCLUDING STORAGE) must
            // match the inherited storage (PG19 MergeChildAttribute,
            // 42804) — the create_table_like inh_error2 case.
            if let Some(s) = like_def.storage.get(i).copied().flatten() {
                if merged_storage[pos] != s {
                    return Err(exec_err(
                        "42804",
                        format!("column \"{}\" has a storage parameter conflict", col),
                    ));
                }
            }
            // The local column refines the inherited slot: NOT NULL
            // accumulates, and local default/serial/storage settings win.
            // v2.27: a locally (re)declared column is local.
            if let Some(l) = merged_att.get_mut(pos) {
                *l = true;
            }
            if like_def.not_null.get(i).copied().unwrap_or(false) {
                merged.not_null[pos] = true;
                // v2.27: a locally declared not-null on a merged
                // column makes the constraint local too.
                if let Some(l) = merged_nn.get_mut(pos) {
                    *l = true;
                }
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
            // v2.16: the child's own generation expression resolves
            // an inherited generation-expression conflict (PG19).
            if like_def.gen_cols.get(i).is_some_and(|g| g.is_some())
                && gen_expr_conflict.as_deref() == Some(col.as_str())
            {
                gen_expr_conflict = None;
            }
            local_merged[i] = true;
        }
        // v2.16: unresolved inherited generation-expression
        // conflict (PG19 DefineRelation, 42P16).
        if let Some(col) = &gen_expr_conflict {
            return Err(exec_err(
                "42P16",
                format!(
                    "column \"{}\" inherits conflicting generation expressions",
                    col
                ),
            ));
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
            // v1.93: declared bit typmod for the new column.
            merged
                .bit_typmods
                .push(like_def.bit_typmods.get(i).copied().unwrap_or(None));
            merged
                .domain_elem
                .push(like_def.domain_elem.get(i).copied().unwrap_or(false));
            merged
                .not_null
                .push(like_def.not_null.get(i).copied().unwrap_or(false));
            merged_nn.push(true);
            merged_att.push(true);
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
        // v2.27: a local CHECK identical (name + definition) to an
        // inherited copy merges into it and makes it local (PG19
        // MergeWithExistingConstraint); differing definitions with
        // the same name were already rejected in exec_create.
        for lc in like_def.checks.iter() {
            if let Some(ex) = merged.checks.iter_mut().find(|c| {
                c.kind == lc.kind
                    && c.name == lc.name
                    && format!("{:?}", c.expr) == format!("{:?}", lc.expr)
            }) {
                ex.is_local = true;
                continue;
            }
            merged.checks.push(lc.clone());
        }
        merged.nn_local = merged_nn.clone();
        merged.att_local = merged_att.clone();
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
                // v2.27: partition children are never attribute-local.
                def.att_local = vec![false; parent.columns.len()];
                def.composite_types = parent.composite_types.clone();
                def.domain_types = parent.domain_types.clone();
                def.bit_typmods = parent.bit_typmods.clone();
                def.domain_elem = parent.domain_elem.clone();
                def.not_null = parent.not_null.clone();
                def.defaults = parent.defaults.clone();
                def.checks = parent
                    .checks
                    .iter()
                    .map(|c| {
                        let mut c = c.clone();
                        // v2.27: partition copies are never local.
                        c.is_local = false;
                        c
                    })
                    .collect();
                // v0.70: inherit the parent's constraints too (PG copies
                // them to every partition). Backing unique indexes get
                // per-table names via `constraint_index_name`.
                def.uniques = parent
                    .uniques
                    .iter()
                    .map(|u| {
                        let mut u = u.clone();
                        u.is_local = false;
                        u
                    })
                    .collect();
                def.pkey = parent.pkey.clone().map(|mut p| {
                    p.is_local = false;
                    p
                });
                def.fks = parent
                    .fks
                    .iter()
                    .map(|f| {
                        let mut f = f.clone();
                        f.is_local = false;
                        f
                    })
                    .collect();
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
                // v2.26: generated expressions and collations are part
                // of the rowtype too (the clone above never carried
                // them, so children silently lost both).
                def.gen_cols = parent.gen_cols.clone();
                def.collations = parent.col_collations.clone();
                // v2.26: PARTITION OF element list (PG19
                // MergeAttributes) merged into the inherited def.
                if let Some(ov) = def.partition_overrides.take().map(|b| *b) {
                    apply_partition_overrides(
                        name,
                        ov,
                        &def.columns,
                        &mut def.gen_cols,
                        &mut def.defaults,
                        &mut def.not_null,
                        &mut def.checks,
                        &mut def.uniques,
                        &mut def.pkey,
                        &mut def.fks,
                    )?;
                    validate_generated_columns(eng, &def)?;
                }
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
        // v2.27: a temp PARTITION OF child's not-null locality.
        if temp_parent.is_some() {
            fix_partition_child_nn_local(&mut t);
        }
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
            // v2.27: partition children are never attribute-local.
            def.att_local = vec![false; parent.columns.len()];
            def.composite_types = parent.composite_types.clone();
            def.domain_types = parent.domain_types.clone();
            def.bit_typmods = parent.bit_typmods.clone();
            def.domain_elem = parent.domain_elem.clone();
            def.not_null = parent.not_null.clone();
            def.defaults = parent.defaults.clone();
            def.checks = parent
                .checks
                .iter()
                .map(|c| {
                    let mut c = c.clone();
                    // v2.27: partition copies are never local.
                    c.is_local = false;
                    c
                })
                .collect();
            // v0.70: inherit the parent's constraints too (PG copies
            // them to every partition). Backing unique indexes get
            // per-table names via `constraint_index_name`.
            def.uniques = parent
                .uniques
                .iter()
                .map(|u| {
                    let mut u = u.clone();
                    u.is_local = false;
                    u
                })
                .collect();
            def.pkey = parent.pkey.clone().map(|mut p| {
                p.is_local = false;
                p
            });
            def.fks = parent
                .fks
                .iter()
                .map(|f| {
                    let mut f = f.clone();
                    f.is_local = false;
                    f
                })
                .collect();
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
            // v2.26: generated expressions and collations are part of
            // the rowtype too (the clone above never carried them, so
            // children silently lost both).
            def.gen_cols = parent.gen_cols.clone();
            def.collations = parent.col_collations.clone();
            // v2.26: PARTITION OF element list (PG19 MergeAttributes)
            // merged into the inherited def.
            if let Some(ov) = def.partition_overrides.take().map(|b| *b) {
                apply_partition_overrides(
                    name,
                    ov,
                    &def.columns,
                    &mut def.gen_cols,
                    &mut def.defaults,
                    &mut def.not_null,
                    &mut def.checks,
                    &mut def.uniques,
                    &mut def.pkey,
                    &mut def.fks,
                )?;
                validate_generated_columns(eng, &def)?;
            }
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
            // v2.27: a PARTITION OF child's not-null locality.
            fix_partition_child_nn_local(&mut t);
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
            note_obj_schema(eng, ctx, name, name);
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
    note_obj_schema(eng, ctx, name, name);
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
/// v2.15: PG19's FK type-compatibility predicate (tablecmds.c
/// ATAddForeignKeyConstraint): every referencing/referenced column
/// pair needs an equality operator — the PK column's btree
/// opfamily must carry one for the (domain-stripped) FK type, or
/// both types must implicitly coerce to the opclass input type.
/// The scalar rules encode PG's opfamily memberships and implicit
/// promotions for the builtin families the engine models; a named
/// (composite) FK type instead needs a registered IMPLICIT cast
/// to the referenced type (PG's can_coerce_type path).
pub(crate) fn fk_types_compatible(eng: &Engine, child: &Table, fk: &FkDef, parent: &Table) -> bool {
    let parent_meta = TableMeta::of(parent);
    let Ok(ref_cols) = fk_ref_cols(&parent_meta, fk) else {
        return true; // resolution errors are reported elsewhere
    };
    use crate::storage::ColType as CT;
    // Effective type of one column: (scalar type, named type).
    // Domains resolve to their base type (PG getBaseType).
    let eff = |t: &Table, idx: usize| -> (CT, Option<String>) {
        if let Some(Some(d)) = t.domain_types.get(idx) {
            if let Some(st) = eng.db.types.get(d) {
                if let Some(dom) = &st.domain {
                    return (dom.base.clone(), None);
                }
            }
        }
        if let Some(Some(n)) = t.composite_types.get(idx) {
            return (CT::Composite, Some(n.clone()));
        }
        (t.columns[idx].1.clone(), None)
    };
    let int_fam = |t: &CT| matches!(t, CT::SmallInt | CT::Int | CT::BigInt);
    let text_fam = |t: &CT| matches!(t, CT::Text | CT::Varchar(_) | CT::Char(_) | CT::Name);
    let float_fam = |t: &CT| matches!(t, CT::Float | CT::Float4);
    let compatible = |ct: &CT, cn: &Option<String>, pt: &CT, pn: &Option<String>| -> bool {
        if let (Some(a), Some(b)) = (cn, pn) {
            return a == b;
        }
        if let Some(a) = cn {
            // Named FK type: needs an implicit cast to the
            // referenced column's type (PG can_coerce_type).
            // Cast keys keep the spelling used at CREATE CAST,
            // so resolve the target name back to a ColType.
            return eng.db.casts.iter().any(|((src, dst), c)| {
                src == a
                    && c.context == crate::sql::CastContext::Implicit
                    && crate::sql::coltype_by_name(dst).ok().as_ref() == Some(pt)
            });
        }
        if pn.is_some() {
            return false;
        }
        if ct == pt {
            return true;
        }
        if int_fam(ct) && int_fam(pt) {
            return true;
        }
        // Promotions on the FK side (implicit coercion to the
        // referenced type): int-family into numeric or float,
        // numeric into float — never the reverse.
        if int_fam(ct) && matches!(pt, CT::Numeric(_)) {
            return true;
        }
        if (int_fam(ct) || matches!(ct, CT::Numeric(_))) && float_fam(pt) {
            return true;
        }
        if text_fam(ct) && text_fam(pt) {
            return true;
        }
        // (No text<->integer cross-family allowance: the
        // comment_test_child case in alter_table is text->text at
        // CREATE time; the 42804 surfaces when the parent column
        // is rebuilt to int, exactly like fk_1_2's int->text
        // rejection at ADD time.)
        // oid and the integer family are binary-coercible both
        // ways (pg_cast.dat: castfunc 0, context 'i').
        if (matches!(ct, CT::Oid) && int_fam(pt)) || (int_fam(ct) && matches!(pt, CT::Oid)) {
            return true;
        }
        if float_fam(ct) && float_fam(pt) {
            return true;
        }
        false
    };
    for (cc, rc) in fk.cols.iter().zip(ref_cols.iter()) {
        let Some(ci) = child.column_index(cc) else {
            return true; // missing columns are reported elsewhere
        };
        let Some(pi) = parent.column_index(rc) else {
            return true;
        };
        let (ct, cn) = eff(child, ci);
        let (pt, pn) = eff(parent, pi);
        if !compatible(&ct, &cn, &pt, &pn) {
            return false;
        }
    }
    true
}

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
    // v2.15: SET NULL/DEFAULT column lists must name FK columns
    // (PG19 tablecmds.c validateFkOnDeleteSetColumns: 42703).
    for (verb, list) in [
        ("ON DELETE", &fk.on_delete_cols),
        ("ON UPDATE", &fk.on_update_cols),
    ] {
        if let Some(names) = list {
            for n in names {
                if !fk.cols.contains(n) {
                    return Err(exec_err(
                        "42703",
                        format!(
                            "column \"{}\" referenced in {} SET action must be part of foreign key",
                            n, verb
                        ),
                    ));
                }
            }
        }
    }
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
    // v2.15: FK column types must be compatible (42804).
    if !fk_types_compatible(eng, child, fk, parent) {
        return Err(exec_err(
            "42804",
            format!(
                "foreign key constraint \"{}\" cannot be implemented",
                fk.name
            ),
        ));
    }
    // v2.07: PG19 (tablecmds.c) refuses an FK whose referenced key is
    // backed ONLY by a deferrable (INITIALLY DEFERRED) unique/PK
    // constraint — 55000. A DEFERRABLE INITIALLY IMMEDIATE constraint
    // is immediate (indimmediate) and can be referenced, as can any
    // other immediate unique constraint/index over the same columns.
    let deferred_only = |u: &UniqueDef| u.deferrable && u.initially_deferred;
    // v2.15: PG matches the referenced columns against a unique
    // constraint by column SET, not by list order (the suite's
    // "index and FK order differ" case: PRIMARY KEY (ptest2, ptest1)
    // referenced as (ptest1, ptest2)). The FK's own pairing of child
    // to referenced columns stays positional as written.
    let same_key =
        |cols: &[String]| cols.len() == ref_cols.len() && cols.iter().all(|c| ref_cols.contains(c));
    let pk_match = parent.pkey.as_ref().filter(|pk| same_key(&pk.cols));
    let uq_match = parent.uniques.iter().find(|u| same_key(&u.cols));
    // A backing index of a deferred-only constraint does not count
    // (PG's indimmediate is false for it); other indexes do.
    let parent_is_child = parent
        .partition
        .as_ref()
        .is_some_and(|p| p.parent.is_some());
    let deferred_backing: Vec<String> = parent
        .uniques
        .iter()
        .chain(parent.pkey.iter())
        .filter(|u| deferred_only(u))
        .map(|u| constraint_index_name(&fk.ref_table, &u.name, parent_is_child))
        .collect();
    // v2.15: scan only indexes VISIBLE to this statement — the raw
    // global map also holds dropped indexes (kept for rollback),
    // and a dropped table's PK index leaking into a same-named new
    // table's FK validation silently accepted bad FKs.
    let idx_match = eng
        .db
        .visible_indexes_for(&fk.ref_table, ctx.snap, &ctx.all_xids, ctx.session)
        .iter()
        .any(|idx| {
            idx.def.unique
                && same_key(&idx.def.col_names)
                && !deferred_backing.contains(&idx.def.name)
        });
    let is_key = pk_match.is_some_and(|pk| !deferred_only(pk))
        || uq_match.is_some_and(|u| !deferred_only(u))
        // v0.75: a standalone unique index also satisfies the FK
        // uniqueness requirement (PG allows FKs to reference unique
        // indexes, not just constraints).
        || idx_match;
    if !is_key {
        if pk_match.is_some_and(|pk| deferred_only(pk)) {
            return Err(exec_err(
                "55000",
                format!(
                    "cannot use a deferrable primary key for referenced table \"{}\"",
                    fk.ref_table
                ),
            ));
        }
        if uq_match.is_some_and(|u| deferred_only(u)) {
            return Err(exec_err(
                "55000",
                format!(
                    "cannot use a deferrable unique constraint for referenced table \"{}\"",
                    fk.ref_table
                ),
            ));
        }
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
    // v2.05: a constraint created directly on the child already
    // carries the child's name (`{table}_pkey` from
    // ChooseConstraintName), so prefixing again produced
    // `{table}_{table}_pkey` and REPLICA IDENTITY USING INDEX
    // could never find the backing index (42704). PG names the
    // backing index after the constraint verbatim; the prefix is
    // only needed when a parent-named constraint propagates.
    if is_partition_child && !con.starts_with(&format!("{table}_")) {
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
    // v2.08: NULLS NOT DISTINCT of the owning constraint.
    nulls_not_distinct: bool,
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
    let mut def = IndexDef::plain(
        ix_name.clone(),
        table.to_string(),
        seen,
        cols.to_vec(),
        true,
        internal,
        ctx.own,
    );
    def.nulls_not_distinct = nulls_not_distinct;
    let mut ix = Index::new(def);
    for r in &t.rows {
        let key = ix.key_for(&r.values);
        ix.insert(key, r.id);
    }
    // Duplicate check among versions that could still become visible
    // (mirrors CREATE UNIQUE INDEX).
    for (key, ids) in &ix.tree {
        if !nulls_not_distinct && key.0.iter().any(|v| matches!(v, Value::Null)) {
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

/// v2.06: `ALTER TABLE parent DETACH PARTITION child
/// [CONCURRENTLY|FINALIZE]` (PG19 ATExecDetachPartition +
/// RemoveInheritance): the child becomes a standalone table keeping
/// its rows; an intermediate (itself partitioned) child keeps its
/// own partitioning and children. PG's CONCURRENTLY two-phase detach
/// (pending-detach state) has no analogue here — CONCURRENTLY and
/// FINALIZE perform the same single-phase detach. The 25001
/// transaction-block restriction for CONCURRENTLY is enforced by
/// PG's utility hook; this engine's executor cannot see the block
/// state, so it is not reproduced (see v206 scope.md).
pub(crate) fn detach_partition(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    parent_name: &str,
    child_name: &str,
    _mode: u8,
) -> Result<ExecResult, ExecError> {
    require_table_owner(eng, ctx, parent_name)?;
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
            "42809",
            format!("relation \"{}\" is not partitioned", parent_name),
        )
    })?;
    if !pinfo.children.iter().any(|c| c == child_name) {
        return Err(exec_err(
            "42P01",
            format!(
                "relation \"{}\" is not a partition of relation \"{}\"",
                child_name, parent_name
            ),
        ));
    }
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
    // Unlink on the parent side (transactional version swap).
    parent_link_child(eng, ctx, parent_name, child_name, false);
    // Child side: clear the parent link and bound. A leaf becomes a
    // plain table; an intermediate keeps method/key/children.
    let mut next = child.clone();
    // v2.27: DETACH locality flips (PG19 RemoveInheritance): with the
    // parent link gone every surviving constraint/column is local
    // (attislocal/conislocal true, inhcounts 0 by derivation).
    for c in next.checks.iter_mut() {
        c.is_local = true;
    }
    for u in next.uniques.iter_mut() {
        u.is_local = true;
    }
    if let Some(pk) = next.pkey.as_mut() {
        pk.is_local = true;
    }
    for f in next.fks.iter_mut() {
        f.is_local = true;
    }
    for i in 0..next.columns.len() {
        next.set_nn_local(i, true);
        next.set_att_local(i, true);
    }
    match &mut next.partition {
        Some(pi) => {
            pi.parent = None;
            pi.bound = None;
            pi.is_default = false;
            if !pi.is_partitioned {
                next.partition = None;
            }
        }
        None => {
            return Err(exec_err(
                "42P01",
                format!(
                    "relation \"{}\" is not a partition of relation \"{}\"",
                    child_name, parent_name
                ),
            ));
        }
    }
    commit_table_version(eng, ctx, child_name, child, next);
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v2.09: does this expression reference a TG_* variable?
fn expr_has_tg_var(e: &Expr) -> bool {
    let mut found = false;
    eval::walk_expr(e, &mut |x| {
        if let Expr::Column { table: None, name } = x {
            if name.starts_with("tg_") {
                found = true;
            }
        }
        if let Expr::Column {
            table: Some(_),
            name,
        } = x
        {
            if name == "tableoid" {
                found = true;
            }
        }
    });
    found
}

/// v2.10: does an expression reference a declared variable, a
/// record variable/field, or FOUND (all folded before row eval)?
fn expr_has_run_ref(e: &Expr, run: &TrigRun) -> bool {
    let mut hit = false;
    walk_expr(e, &mut |x| match x {
        Expr::Column { table: None, name } => {
            if name == "found" || run.vars.contains_key(name) || run.recs.contains_key(name) {
                hit = true;
            }
        }
        Expr::Column { table: Some(q), .. } => {
            if run.recs.contains_key(q.as_str()) || run.aliases.contains_key(q.as_str()) {
                hit = true;
            }
        }
        _ => {}
    });
    hit
}

/// v2.09: fold TG_* column leaves to literals (mirrors
/// merge.rs's subst_tableoid container set, extended).
fn subst_tg_vars(e: &Expr, tctx: &TrigCtx) -> Expr {
    let lit = |v: &str| Expr::Literal(crate::sql::Literal::Text(v.into()));
    match e {
        Expr::Column {
            table: Some(q),
            name,
        } if name == "tableoid"
            && (q.eq_ignore_ascii_case("new") || q.eq_ignore_ascii_case("old")) =>
        {
            Expr::Literal(crate::sql::Literal::Int(tctx.relid))
        }
        Expr::Column { table: None, name } => match name.as_str() {
            "tg_name" => lit(&tctx.name),
            "tg_table_name" => lit(&tctx.table.clone()),
            "tg_relname" => lit(&tctx.table.clone()),
            "tg_table_schema" => lit("public"),
            "tg_when" => lit(tctx.when_name()),
            "tg_op" => lit(tctx.op_name()),
            "tg_level" => lit(tctx.level_name()),
            "tg_nargs" => Expr::Literal(crate::sql::Literal::Int(tctx.args.len() as i64)),
            "tg_relid" => Expr::Literal(crate::sql::Literal::Int(tctx.relid)),
            _ => e.clone(),
        },
        Expr::Arith { op, left, right } => Expr::Arith {
            op: *op,
            left: Box::new(subst_tg_vars(left, tctx)),
            right: Box::new(subst_tg_vars(right, tctx)),
        },
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: Box::new(subst_tg_vars(left, tctx)),
            right: Box::new(subst_tg_vars(right, tctx)),
        },
        Expr::And(a, b) => Expr::And(
            Box::new(subst_tg_vars(a, tctx)),
            Box::new(subst_tg_vars(b, tctx)),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(subst_tg_vars(a, tctx)),
            Box::new(subst_tg_vars(b, tctx)),
        ),
        Expr::Not(a) => Expr::Not(Box::new(subst_tg_vars(a, tctx))),
        Expr::Neg(a) => Expr::Neg(Box::new(subst_tg_vars(a, tctx))),
        Expr::BitNot(a) => Expr::BitNot(Box::new(subst_tg_vars(a, tctx))),
        Expr::Cast { expr, to, written } => Expr::Cast {
            expr: Box::new(subst_tg_vars(expr, tctx)),
            to: *to,
            written: written.clone(),
        },
        Expr::IsNull { expr, neg } => Expr::IsNull {
            expr: Box::new(subst_tg_vars(expr, tctx)),
            neg: *neg,
        },
        Expr::IsBool { expr, neg, val } => Expr::IsBool {
            expr: Box::new(subst_tg_vars(expr, tctx)),
            neg: *neg,
            val: *val,
        },
        Expr::IsDistinctFrom { left, right, neg } => Expr::IsDistinctFrom {
            left: Box::new(subst_tg_vars(left, tctx)),
            right: Box::new(subst_tg_vars(right, tctx)),
            neg: *neg,
        },
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => Expr::Between {
            expr: Box::new(subst_tg_vars(expr, tctx)),
            low: Box::new(subst_tg_vars(low, tctx)),
            high: Box::new(subst_tg_vars(high, tctx)),
            neg: *neg,
        },
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => Expr::Like {
            expr: Box::new(subst_tg_vars(expr, tctx)),
            pattern: Box::new(subst_tg_vars(pattern, tctx)),
            not: *not,
            ilike: *ilike,
            escape: escape.as_ref().map(|x| Box::new(subst_tg_vars(x, tctx))),
        },
        // TG_ARGV[<int literal>] folds to the argument text.
        Expr::Subscript { array, indices }
            if matches!(&**array, Expr::Column { table: None, name } if name == "tg_argv")
                && indices.len() == 1 =>
        {
            match &indices[0] {
                Expr::Literal(crate::sql::Literal::Int(i)) => tctx
                    .args
                    .get(*i as usize)
                    .map(|a| Expr::Literal(crate::sql::Literal::Text(a.clone().into())))
                    .unwrap_or(Expr::Literal(crate::sql::Literal::Null)),
                _ => e.clone(),
            }
        }
        Expr::Subscript { array, indices } => Expr::Subscript {
            array: Box::new(subst_tg_vars(array, tctx)),
            indices: indices.iter().map(|i| subst_tg_vars(i, tctx)).collect(),
        },
        Expr::Concat(a, b) => Expr::Concat(
            Box::new(subst_tg_vars(a, tctx)),
            Box::new(subst_tg_vars(b, tctx)),
        ),
        Expr::Row(items) => Expr::Row(items.iter().map(|a| subst_tg_vars(a, tctx)).collect()),
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args.iter().map(|a| subst_tg_vars(a, tctx)).collect(),
        },
        Expr::NamedArg { name, expr } => Expr::NamedArg {
            name: name.clone(),
            expr: Box::new(subst_tg_vars(expr, tctx)),
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: operand.as_ref().map(|x| Box::new(subst_tg_vars(x, tctx))),
            whens: whens
                .iter()
                .map(|(a, b)| {
                    (
                        Box::new(subst_tg_vars(a, tctx)),
                        Box::new(subst_tg_vars(b, tctx)),
                    )
                })
                .collect(),
            else_: else_.as_ref().map(|x| Box::new(subst_tg_vars(x, tctx))),
        },
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// v2.14: statement-level triggers, transition tables, INSTEAD OF support.
// ---------------------------------------------------------------------------

thread_local! {
    /// v2.14: row images of the DML statement currently executing
    /// (accumulated by fire_after_row_triggers per leaf), consumed by
    /// the AFTER STATEMENT hook to materialize transition tables.
    /// One (leaf table, cols, pairs) entry per touched leaf.
    static STMT_IMAGES: std::cell::RefCell<
        Vec<(String, Vec<(String, ColType)>, Vec<(Option<Row>, Option<Row>)>)>,
    > = const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {
    /// v2.14: relations with a DML statement currently executing on
    /// this thread (PG's "being used by active queries in this
    /// session" for ALTER TABLE ... ATTACH/DETACH PARTITION issued
    /// from inside a trigger body).
    static ACTIVE_DML: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) fn active_dml_push(rel: &str) {
    ACTIVE_DML.with(|a| a.borrow_mut().push(rel.to_string()));
}

pub(crate) fn active_dml_pop() {
    ACTIVE_DML.with(|a| {
        a.borrow_mut().pop();
    });
}

pub(crate) fn active_dml_contains(rel: &str) -> bool {
    ACTIVE_DML.with(|a| a.borrow().iter().any(|r| r == rel))
}

/// v2.14: reset the statement-image capture (called by the dispatch
/// hook before a DML statement runs).
pub(crate) fn stmt_images_reset() {
    STMT_IMAGES.with(|s| s.borrow_mut().clear());
}

/// v2.14: accumulate one leaf's row images into the capture.
fn stmt_images_push(leaf: &str, cols: &[(String, ColType)], pairs: &[(Option<Row>, Option<Row>)]) {
    if pairs.is_empty() {
        return;
    }
    STMT_IMAGES.with(|s| {
        let mut b = s.borrow_mut();
        if let Some(e) = b.iter_mut().find(|e| e.0 == leaf) {
            e.2.extend(pairs.iter().cloned());
        } else {
            b.push((leaf.to_string(), cols.to_vec(), pairs.to_vec()));
        }
    });
}

/// v2.14: take the captured images for the statement that targeted
/// `rel`, unioned across every touched leaf (PG fires a partitioned
/// parent's statement trigger once with the full transition set).
/// Pairs are re-projected onto `root_cols` by column name (leaves of
/// one statement share the root's columns; inheritance children with
/// a different order are aligned by name, missing columns read as
/// NULL). None when nothing was captured.
pub(crate) fn stmt_images_take_pub(
    eng: &Engine,
    ctx: &StmtCtx,
    rel: &str,
    root_cols: &[(String, ColType)],
) -> Option<(Vec<(String, ColType)>, Vec<(Option<Row>, Option<Row>)>)> {
    let all = STMT_IMAGES.with(|s| std::mem::take(&mut *s.borrow_mut()));
    // Only the target relation's own subtree counts: a data-
    // modifying CTE's inner statement captures its own images (PG
    // fires its statement triggers separately), so entries from
    // unrelated relations in the same outer statement are dropped.
    let entries: Vec<_> = all
        .into_iter()
        .filter(|(leaf, _, _)| leaf_belongs_to(eng, ctx, leaf, rel))
        .collect();
    if entries.is_empty() {
        return None;
    }
    let out_cols: Vec<(String, ColType)> = if root_cols.is_empty() {
        entries[0].1.clone()
    } else {
        root_cols.to_vec()
    };
    let mut pairs: Vec<(Option<Row>, Option<Row>)> = Vec::new();
    for (_, cols, ps) in &entries {
        if *cols == out_cols {
            pairs.extend(ps.iter().cloned());
            continue;
        }
        let idx: Vec<Option<usize>> = out_cols
            .iter()
            .map(|(n, _)| cols.iter().position(|(cn, _)| cn == n))
            .collect();
        for (o, n) in ps {
            let proj = |r: &Option<Row>| -> Option<Row> {
                r.as_ref().map(|row| {
                    Row::new(
                        idx.iter()
                            .map(|i| match i {
                                Some(j) => row[*j].clone(),
                                None => Value::Null,
                            })
                            .collect(),
                    )
                })
            };
            pairs.push((proj(o), proj(n)));
        }
    }
    Some((out_cols, pairs))
}

/// v2.14: is `leaf` the relation `rel` itself, a partition
/// descendant of it, or an inheritance descendant of it?
fn leaf_belongs_to(eng: &Engine, ctx: &StmtCtx, leaf: &str, rel: &str) -> bool {
    if leaf == rel {
        return true;
    }
    let mut cur = leaf.to_string();
    for _ in 0..64 {
        let Some(t) = eng
            .db
            .find_table(&cur, ctx.snap, &ctx.all_xids, ctx.session)
        else {
            return false;
        };
        if let Some(pi) = &t.partition {
            if let Some(p) = &pi.parent {
                if p == rel {
                    return true;
                }
                cur = p.clone();
                continue;
            }
        }
        if t.inherits.iter().any(|p| p == rel) {
            return true;
        }
        match t.inherits.first() {
            Some(p) => {
                cur = p.clone();
            }
            None => return false,
        }
    }
    false
}

/// v2.14: every trigger definition stored on a relation itself plus
/// its partitioned ancestors (walking PartitionInfo.parent links),
/// plus view triggers when the relation is a view. Returns
/// (defining relation, definition) pairs, own first.
pub(crate) fn relation_trigger_defs(
    eng: &Engine,
    ctx: &StmtCtx,
    rel: &str,
) -> Vec<(String, TriggerDef)> {
    let mut out = Vec::new();
    if let Some(t) = eng.db.find_table(rel, ctx.snap, &ctx.all_xids, ctx.session) {
        for tr in &t.triggers {
            out.push((rel.to_string(), tr.clone()));
        }
        let mut parent = t.partition.as_ref().and_then(|p| p.parent.clone());
        let mut guard = 0;
        while let Some(pname) = parent {
            guard += 1;
            if guard > 64 {
                break;
            }
            let Some(pt) = eng
                .db
                .find_table(&pname, ctx.snap, &ctx.all_xids, ctx.session)
            else {
                break;
            };
            for tr in &pt.triggers {
                out.push((pname.clone(), tr.clone()));
            }
            parent = pt.partition.as_ref().and_then(|p| p.parent.clone());
        }
        return out;
    }
    for tr in crate::exec::views::view_triggers(eng, ctx, rel) {
        out.push((rel.to_string(), tr));
    }
    out
}

/// v2.14: does the relation have any statement-level trigger for
/// (timing, event)? Cheap gate for the dispatch hook.
pub(crate) fn has_statement_triggers(
    eng: &Engine,
    ctx: &StmtCtx,
    rel: &str,
    timing: TriggerTiming,
    event: u8,
) -> bool {
    relation_trigger_defs(eng, ctx, rel).iter().any(|(_, tr)| {
        tr.timing == timing
            && !tr.for_each_row
            && tr.fires()
            && tr.enforced
            && (tr.events & event) != 0
    })
}

/// v2.14: materialize a trigger's REFERENCING transition tables as
/// session temp tables populated with the statement's row images.
/// Returns the installed names (drop with drop_transition_tables).
/// A name colliding with an existing temp table shadows it for the
/// duration (the previous table is restored on drop).
pub(crate) fn install_transition_tables(
    eng: &mut Engine,
    ctx: &StmtCtx,
    cols: &[(String, ColType)],
    pairs: &[(Option<Row>, Option<Row>)],
    tr: &TriggerDef,
) -> Result<Vec<(String, Option<crate::storage::Table>)>, ExecError> {
    let mut installed = Vec::new();
    let mk =
        |eng: &mut Engine, name: &str, rows: Vec<Row>| -> (String, Option<crate::storage::Table>) {
            let mut t = crate::storage::Table::new(cols.to_vec(), ctx.own);
            t.owner = ctx.role.to_string();
            for r in rows {
                let id = eng.alloc_row_id();
                t.rows.push(crate::storage::RowVersion {
                    id,
                    values: r,
                    xmin: ctx.own,
                    xmax: 0,
                    toast: Vec::new(),
                });
            }
            let prev = eng
                .db
                .temp_tables
                .entry(ctx.session)
                .or_default()
                .insert(name.to_string(), t);
            (name.to_string(), prev)
        };
    if let Some(n) = &tr.old_table {
        let rows: Vec<Row> = pairs.iter().filter_map(|(o, _)| o.clone()).collect();
        installed.push(mk(eng, n, rows));
    }
    if let Some(n) = &tr.new_table {
        let rows: Vec<Row> = pairs.iter().filter_map(|(_, n)| n.clone()).collect();
        installed.push(mk(eng, n, rows));
    }
    Ok(installed)
}

/// v2.14: remove transition tables installed by
/// install_transition_tables, restoring any shadowed temp table.
pub(crate) fn drop_transition_tables(
    eng: &mut Engine,
    ctx: &StmtCtx,
    installed: Vec<(String, Option<crate::storage::Table>)>,
) {
    for (name, prev) in installed {
        if let Some(tmps) = eng.db.temp_tables.get_mut(&ctx.session) {
            match prev {
                Some(t) => {
                    tmps.insert(name, t);
                }
                None => {
                    tmps.remove(&name);
                }
            }
        }
    }
}

/// v2.14: fire the BEFORE/AFTER STATEMENT triggers of a relation for
/// one event (PG19 ExecBS/ExecAS trigger queues: once per statement,
/// including zero-row statements). `set_cols` is the UPDATE
/// statement's SET list (UPDATE OF matching); `capture` carries the
/// statement's row images for AFTER transition-table triggers.
pub(crate) fn fire_statement_triggers(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    rel: &str,
    event: u8,
    timing: TriggerTiming,
    set_cols: Option<&[String]>,
    capture: Option<&(Vec<(String, ColType)>, Vec<(Option<Row>, Option<Row>)>)>,
) -> Result<(), ExecError> {
    let defs = relation_trigger_defs(eng, ctx, rel);
    // Relation columns (for WHEN evaluation and body scope). For
    // views, the view's output columns (NEW/OLD are all-NULL for
    // statement triggers, but their fields must resolve).
    let cols: Vec<(String, ColType)> =
        match eng.db.find_table(rel, ctx.snap, &ctx.all_xids, ctx.session) {
            Some(t) => t.columns.clone(),
            None => crate::exec::dml::view_columns_of(eng, ctx, rel)
                .unwrap_or_else(|_| capture.map(|(c, _)| c.clone()).unwrap_or_default()),
        };
    let relid = eng
        .db
        .find_table(rel, ctx.snap, &ctx.all_xids, ctx.session)
        .map(|t| t.oid as i64)
        .unwrap_or(0);
    for (def_table, tr) in &defs {
        let _ = def_table;
        if tr.timing != timing
            || tr.for_each_row
            || !tr.fires()
            || !tr.enforced
            || (tr.events & event) == 0
        {
            continue;
        }
        if event == trig_event::UPDATE && !tr.update_cols.is_empty() {
            match set_cols {
                Some(sc) => {
                    if !tr.update_cols.iter().any(|c| sc.contains(c)) {
                        continue;
                    }
                }
                None => continue,
            }
        }
        let tctx = TrigCtx {
            name: tr.name.clone(),
            table: rel.to_string(),
            timing: tr.timing,
            event,
            args: tr.args.clone(),
            relid,
            for_each_row: false,
        };
        if !trigger_when_matches(eng, ctx, &tctx, &cols, None, None, tr.when_expr.as_ref())? {
            continue;
        }
        // Deferred constraint triggers are ROW-only in PG; statement
        // triggers fire immediately.
        let installed = if tr.has_transition {
            match capture {
                Some((ccols, pairs)) => install_transition_tables(eng, ctx, ccols, pairs, tr)?,
                // PG always materializes the transition relation for
                // a statement trigger that names one — empty when the
                // statement touched no rows (or its images were not
                // captured, e.g. ON CONFLICT / CTE inner DML paths).
                None => install_transition_tables(eng, ctx, &cols, &[], tr)?,
            }
        } else {
            Vec::new()
        };
        let r = fire_one_trigger(eng, ctx, &tctx, tr, &cols, None, None);
        drop_transition_tables(eng, ctx, installed);
        r?;
    }
    Ok(())
}
