// v2.32: plan-time partition pruning (PG19 partprune.c, reduced to the
// shapes this planner renders). Given a partitioned parent's
// PartitionInfo, its direct children (with bounds), and the FROM item's
// WHERE slice, decide which children can contain rows satisfying the
// WHERE. A child is dropped only when the clause set PROVES its bound
// disjoint from the predicate (three-valued: a comparison that cannot
// be evaluated keeps the child). The executor never consumes this —
// execution scans every leaf and applies the real predicate — so a
// wrong answer here can only misrender an EXPLAIN, never misexecute.
//
// Two predicates over (bound, expr), mirroring partprune's proof
// obligation:
//   may(bound, e)  — some row in the bound could make `e` TRUE
//   must(bound, e) — every row in the bound makes `e` TRUE
// with may(Not e) = !must(e) and must(Not e) = !may(e). Non-key or
// unanalyzable clauses are (may=true, must=false).
//
// DEFAULT partitions and HASH routing are decided by the caller-level
// drivers below (coverage / full-key equality), not by may/must: a
// default bound is a complement, not a set, and a hash bound needs all
// key parts pinned at once.

use super::*;
use crate::storage::{PartBound, PartMethod, PartitionInfo, RangeBound};

/// One extracted key-part clause: `part` indexes PartitionInfo.key.
#[derive(Clone, Debug)]
enum KeyClauseKind {
    Cmp(CmpOp, Value),
    InSet(Vec<Value>),
    NotInSet(Vec<Value>),
    /// `neg == false`: IS NULL; `neg == true`: IS NOT NULL.
    IsNull {
        neg: bool,
    },
    /// v2.32: `IS [NOT] TRUE/FALSE` — NOT the same as an equality:
    /// at a NULL key value, IS NOT TRUE/FALSE is TRUE while an
    /// equality is UNKNOWN (the boolpart family scores exactly
    /// this distinction).
    IsBool {
        neg: bool,
        val: bool,
    },
}

#[derive(Clone, Debug)]
struct KeyClause {
    part: usize,
    kind: KeyClauseKind,
}

/// Strip COLLATE wrappers (collation does not change the value set a
/// clause talks about for pruning; PG19 coll_pruning cases prune on
/// the underlying comparison, and bound comparison in this engine is
/// byte-order, matching the "C"/"POSIX" cases the suite exercises).
fn strip_collate(mut e: &Expr) -> &Expr {
    while let Expr::Collate { expr, .. } = e {
        e = expr;
    }
    e
}

/// Does `e` denote partition key part `part` of `parent`? Plain-column
/// parts match a Column with the key column's name and a qualifier
/// that is empty, the parent name, or the parent alias. Expression
/// parts match a structurally equal expression (COLLATE-stripped).
fn key_part_matches(
    e: &Expr,
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
    part: usize,
) -> bool {
    let pk = &pinfo.key[part];
    let e = strip_collate(e);
    match &pk.expr {
        Some(ke) => {
            let ke = strip_collate(ke);
            if ke == e {
                return true;
            }
            // Key is a cast of a plain column (`(a::text)` in
            // coercepart) while the clause names the bare column:
            // the comparison coerces to the key's domain, and the
            // stored bounds already live there. Faithful for the
            // text and numeric cast families.
            match ke {
                Expr::Cast {
                    expr: inner, to, ..
                } => {
                    matches!(
                        to,
                        ColType::Text
                            | ColType::Char(_)
                            | ColType::Varchar(_)
                            | ColType::Int
                            | ColType::BigInt
                            | ColType::SmallInt
                            | ColType::Float
                            | ColType::Float4
                            | ColType::Numeric(_)
                    ) && strip_collate(inner) == e
                }
                _ => false,
            }
        }
        None => {
            let col_name = &parent.columns[pk.col].0;
            match e {
                Expr::Column { table, name } => {
                    name == col_name
                        && match table {
                            None => true,
                            Some(q) => {
                                q == parent_name || parent_alias.map(|a| q == a).unwrap_or(false)
                            }
                        }
                }
                // A cast of the key column (e.g. `a::numeric = 1`):
                // match only for numeric-family widening casts, where
                // the comparison on the cast value is faithful.
                Expr::Cast { expr, to, .. } => {
                    matches!(
                        to,
                        ColType::Int
                            | ColType::BigInt
                            | ColType::SmallInt
                            | ColType::Float
                            | ColType::Float4
                            | ColType::Numeric(_)
                    ) && key_part_matches(expr, pinfo, parent, parent_name, parent_alias, part)
                }
                _ => false,
            }
        }
    }
}

/// Which key part (if any) does `e` reference?
fn key_part_of(
    e: &Expr,
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
) -> Option<usize> {
    (0..pinfo.key.len()).find(|&p| key_part_matches(e, pinfo, parent, parent_name, parent_alias, p))
}

/// Is this key part boolean-typed (plain bool column, or a
/// `NOT <bool column>` expression key)?
fn part_is_bool(pinfo: &PartitionInfo, parent: &Table, part: usize) -> bool {
    let pk = &pinfo.key[part];
    match &pk.expr {
        None => parent.columns[pk.col].1 == ColType::Bool,
        Some(Expr::Not(inner)) => match strip_collate(inner) {
            Expr::Column { name, .. } => parent
                .columns
                .iter()
                .any(|(n, t)| n == name && *t == ColType::Bool),
            _ => false,
        },
        _ => false,
    }
}

/// Key-part finder for COMPARISON clauses: additionally matches a
/// clause naming the inner expression of a `NOT (...)` key (the
/// iboolpart shape, key `(not a)`, clause on `a`). BooleanTest /
/// NullTest extraction deliberately does NOT use this: PG19's
/// match_clause_to_partition_key matches those structurally only.
fn key_part_of_cmp(
    e: &Expr,
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
) -> Option<usize> {
    if let Some(p) = key_part_of(e, pinfo, parent, parent_name, parent_alias) {
        return Some(p);
    }
    let e = strip_collate(e);
    (0..pinfo.key.len()).find(|&p| match &pinfo.key[p].expr {
        Some(Expr::Not(inner)) => strip_collate(inner) == e,
        _ => false,
    })
}

/// When a comparison clause matched the INNER of a `NOT (...)` key,
/// flip its bool constants into key domain (`a = false` on key
/// `(not a)` pins the key to TRUE). Non-bool shapes fail closed.
fn flip_for_not_key(pinfo: &PartitionInfo, kc: KeyClause) -> Option<KeyClause> {
    if !matches!(&pinfo.key[kc.part].expr, Some(Expr::Not(_))) {
        return Some(kc);
    }
    let kind = match kc.kind {
        KeyClauseKind::Cmp(op @ (CmpOp::Eq | CmpOp::Ne), Value::Bool(b)) => {
            KeyClauseKind::Cmp(op, Value::Bool(!b))
        }
        KeyClauseKind::InSet(vals) => {
            let mut out = Vec::new();
            for v in vals {
                match v {
                    Value::Bool(b) => out.push(Value::Bool(!b)),
                    _ => return None,
                }
            }
            KeyClauseKind::InSet(out)
        }
        KeyClauseKind::NotInSet(vals) => {
            let mut out = Vec::new();
            for v in vals {
                match v {
                    Value::Bool(b) => out.push(Value::Bool(!b)),
                    _ => return None,
                }
            }
            KeyClauseKind::NotInSet(out)
        }
        _ => return None,
    };
    Some(KeyClause { kind, ..kc })
}

/// PG19 simplifies boolean equality (clauses.c
/// simplify_boolean_equality) before pruning: `key = true` is the
/// bare bool qual, `key = false` its negation. Mirror that in the
/// extracted kind so pointwise/region semantics at NULL match the
/// steps PG generates (an IS NOT TRUE step admits NULL rows).
fn bool_normalize(pinfo: &PartitionInfo, parent: &Table, kc: KeyClause) -> KeyClause {
    // Deliberately NOT normalized to the IsBool kind: an OpExpr
    // bool equality is NULL-strict (`a = false` is UNKNOWN at NULL,
    // so the default holding NULLs is pruned), while a BooleanTest
    // `a IS NOT TRUE` is TRUE at NULL and keeps it — PG19's
    // boolpart outputs prove the distinction. The IsBool kind is
    // produced only from actual BooleanTest nodes.
    let _ = (pinfo, parent);
    kc
}

/// PG19 opfamily of a type, for the clause-family gate: a pruning
/// step exists only when the clause operator belongs to the key
/// part's opfamily (int2/int4/int8 share one; numeric, float, and
/// text are separate — `a = 1::numeric` on an int key prunes
/// NOTHING in PG19, it coerces the column instead).
fn type_family(t: &ColType) -> u8 {
    match t {
        ColType::SmallInt | ColType::Int | ColType::BigInt => 1,
        ColType::Numeric(_) => 2,
        ColType::Float | ColType::Float4 => 3,
        ColType::Text | ColType::Char(_) | ColType::Varchar(_) | ColType::Name => 4,
        ColType::Bool => 5,
        _ => 0,
    }
}

fn value_family(v: &Value) -> u8 {
    match v {
        Value::SmallInt(_) | Value::Int(_) | Value::BigInt(_) => 1,
        Value::Numeric(_) => 2,
        Value::Float(_) | Value::Float4(_) => 3,
        Value::Text(_) | Value::BpChar(_) => 4,
        Value::Bool(_) => 5,
        _ => 0,
    }
}

/// The key part's type (plain column, or inferred for expr keys).
fn part_type(pinfo: &PartitionInfo, parent: &Table, part: usize) -> ColType {
    let pk = &pinfo.key[part];
    match &pk.expr {
        None => parent.columns[pk.col].1,
        Some(e) => infer_expr_key_type(e, parent),
    }
}

/// May a clause carrying value `v` form a pruning step on `part`?
/// Unknown families on either side stay permissive.
fn family_ok(pinfo: &PartitionInfo, parent: &Table, part: usize, v: &Value) -> bool {
    let kf = type_family(&part_type(pinfo, parent, part));
    let vf = value_family(v);
    kf == 0 || vf == 0 || kf == vf
}

fn flip_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::ImageEq => CmpOp::ImageEq,
    }
}

/// Extract a key clause from a leaf (non-And/Or) expression, if it is
/// one. Constants are folded with the planner's const folder (literal
/// casts included). Returns None for anything else.
fn extract_key_clause(
    e: &Expr,
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
    db: &Database,
) -> Option<KeyClause> {
    let mut fc = PgConstFold::with_db(db);
    let fold_val = |x: &Expr, fc: &mut PgConstFold| -> Option<Value> {
        if matches!(x, Expr::Column { .. }) {
            return None;
        }
        let x = strip_collate(x);
        if let Some(lit) = pg_fold_const_item(x, fc) {
            if matches!(lit, Literal::Null) {
                return None;
            }
            return Some(lit.into_value());
        }
        // Cast-chain fallback: the planner's const folder only folds
        // a few exact casts (int->float8); a typed literal like
        // `1::bigint` / `1::numeric` needs the engine's general
        // literal coercion (the same one partition bounds use).
        let mut inner = x;
        let mut target: Option<ColType> = None;
        while let Expr::Cast { expr, to, .. } = inner {
            target = Some(*to);
            inner = strip_collate(expr);
        }
        if let (Some(to), Expr::Literal(l)) = (target, inner) {
            if matches!(l, Literal::Null) {
                return None;
            }
            return coerce_literal(l, &to, "").ok();
        }
        // Negative numeric literal (`a IN (-1, 20)`): the parser
        // keeps unary minus as an expression node.
        if let Expr::Neg(inner) = x {
            if let Expr::Literal(l) = strip_collate(inner) {
                return match l {
                    Literal::Int(i) => Some(Value::Int(-i)),
                    Literal::BigInt(i) => Some(Value::BigInt(-i)),
                    Literal::SmallInt(i) => Some(Value::SmallInt(-i)),
                    Literal::Float(f) => Some(Value::Float(-f)),
                    Literal::Decimal(d) => Some(Literal::Decimal(format!("-{d}")).into_value()),
                    _ => None,
                };
            }
        }
        None
    };
    // The explicit collation named on an expression, if any.
    fn side_collation(x: &Expr) -> Option<String> {
        match x {
            Expr::Collate { name, .. } => Some(name.clone()),
            _ => None,
        }
    }
    fn collation_ok(key_side: &Expr, pinfo: &PartitionInfo, parent: &Table, part: usize) -> bool {
        let clause_coll = match side_collation(key_side) {
            Some(c) => c,
            None => return true,
        };
        let pk = &pinfo.key[part];
        if pk.expr.is_some() {
            return true; // expression keys: no stored collation to check
        }
        let key_coll = parent
            .col_collation(pk.col)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "default".to_string());
        clause_coll == key_coll
    }
    match e {
        Expr::Cmp { op, left, right } => {
            if matches!(op, CmpOp::ImageEq) {
                return None;
            }
            if let Some(part) = key_part_of_cmp(left, pinfo, parent, parent_name, parent_alias) {
                if !collation_ok(left, pinfo, parent, part) {
                    return None;
                }
            }
            if let Some(part) = key_part_of_cmp(right, pinfo, parent, parent_name, parent_alias) {
                if !collation_ok(right, pinfo, parent, part) {
                    return None;
                }
            }
            if let Some(part) = key_part_of_cmp(left, pinfo, parent, parent_name, parent_alias) {
                let v = fold_val(right, &mut fc)?;
                if !family_ok(pinfo, parent, part, &v) {
                    return None;
                }
                let kc = KeyClause {
                    part,
                    kind: KeyClauseKind::Cmp(*op, v),
                };
                return Some(bool_normalize(pinfo, parent, flip_for_not_key(pinfo, kc)?));
            }
            if let Some(part) = key_part_of_cmp(right, pinfo, parent, parent_name, parent_alias) {
                let v = fold_val(left, &mut fc)?;
                if !family_ok(pinfo, parent, part, &v) {
                    return None;
                }
                let kc = KeyClause {
                    part,
                    kind: KeyClauseKind::Cmp(flip_op(*op), v),
                };
                return Some(bool_normalize(pinfo, parent, flip_for_not_key(pinfo, kc)?));
            }
            None
        }
        // A bare boolean key column as a qual is `key = true` after
        // PG19's boolean simplification; NOT of a bool-shaped clause
        // negates its IsBool form.
        Expr::Column { .. } => {
            let part = key_part_of(e, pinfo, parent, parent_name, parent_alias)?;
            if part_is_bool(pinfo, parent, part) {
                Some(KeyClause {
                    part,
                    kind: KeyClauseKind::IsBool {
                        neg: false,
                        val: true,
                    },
                })
            } else {
                None
            }
        }
        Expr::Not(inner) => {
            let kc = extract_key_clause(inner, pinfo, parent, parent_name, parent_alias, db)?;
            match kc.kind {
                KeyClauseKind::IsBool { neg, val } => Some(KeyClause {
                    part: kc.part,
                    kind: KeyClauseKind::IsBool { neg: !neg, val },
                }),
                // NOT (key = b) is key <> b, exactly, in 3VL
                // (both UNKNOWN at NULL).
                KeyClauseKind::Cmp(CmpOp::Eq, v @ Value::Bool(_)) => Some(KeyClause {
                    part: kc.part,
                    kind: KeyClauseKind::Cmp(CmpOp::Ne, v),
                }),
                KeyClauseKind::Cmp(CmpOp::Ne, v @ Value::Bool(_)) => Some(KeyClause {
                    part: kc.part,
                    kind: KeyClauseKind::Cmp(CmpOp::Eq, v),
                }),
                _ => None,
            }
        }
        Expr::IsNull { expr, neg } => {
            let part = key_part_of(expr, pinfo, parent, parent_name, parent_alias)?;
            Some(KeyClause {
                part,
                kind: KeyClauseKind::IsNull { neg: *neg },
            })
        }
        Expr::IsBool { expr, neg, val } => {
            let part = key_part_of(expr, pinfo, parent, parent_name, parent_alias)?;
            match val {
                Some(b) => Some(KeyClause {
                    part,
                    kind: KeyClauseKind::IsBool { neg: *neg, val: *b },
                }),
                None => Some(KeyClause {
                    part,
                    kind: KeyClauseKind::IsNull { neg: *neg },
                }),
            }
        }
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            // Only the positive form is extracted here; the caller
            // decomposes Between itself for the recursive predicates.
            let _ = (expr, low, high, neg);
            None
        }
        // `key = ANY(ARRAY[...])` / `key <> ALL(ARRAY[...])`: the
        // parser desugars quantified array comparisons to the hidden
        // `__any_all_array` builtin (parser.rs v0.91).
        Expr::Func { name, args } if name == "__any_all_array" && args.len() == 4 => {
            let part = key_part_of_cmp(&args[0], pinfo, parent, parent_name, parent_alias)?;
            let lit_text = |x: &Expr| -> Option<String> {
                match x {
                    Expr::Literal(Literal::Text(t)) => Some(t.to_string()),
                    _ => None,
                }
            };
            let op = lit_text(&args[1])?;
            let quant = lit_text(&args[2])?;
            let arr = match &args[3] {
                Expr::Cast { expr, .. } => expr.as_ref(),
                other => other,
            };
            // `= ANY (NULL::text[])`: a null array makes the
            // comparison never TRUE — the empty value set.
            if matches!(arr, Expr::Literal(Literal::Null)) {
                return match (op.as_str(), quant.as_str()) {
                    ("=", "any") | ("=", "all") => Some(KeyClause {
                        part,
                        kind: KeyClauseKind::InSet(Vec::new()),
                    }),
                    _ => None,
                };
            }
            let elems = match arr {
                Expr::ArrayCtor { elems, nested } if !nested => elems,
                // `'{ab,bc}'` text-literal array form.
                Expr::Literal(Literal::Text(t)) => {
                    let (vals, had_null) = parse_array_literal(t)?;
                    return match (op.as_str(), quant.as_str()) {
                        ("=", "any") => Some(KeyClause {
                            part,
                            kind: KeyClauseKind::InSet(vals),
                        }),
                        ("<>", "all") => Some(KeyClause {
                            part,
                            kind: KeyClauseKind::NotInSet(vals),
                        }),
                        ("=", "all") => {
                            // A NULL element makes `= ALL` never
                            // TRUE (the coercepart '{ab,null}' case).
                            if had_null {
                                return Some(KeyClause {
                                    part,
                                    kind: KeyClauseKind::InSet(Vec::new()),
                                });
                            }
                            let first = vals.first()?.clone();
                            if vals.iter().all(|v| val_eq(v, &first) == Some(true)) {
                                Some(KeyClause {
                                    part,
                                    kind: KeyClauseKind::Cmp(CmpOp::Eq, first),
                                })
                            } else {
                                Some(KeyClause {
                                    part,
                                    kind: KeyClauseKind::InSet(Vec::new()),
                                })
                            }
                        }
                        _ => None,
                    };
                }
                _ => return None,
            };
            let mut vals = Vec::new();
            for el in elems {
                vals.push(fold_val(el, &mut fc)?);
            }
            match (op.as_str(), quant.as_str()) {
                ("=", "any") => Some(KeyClause {
                    part,
                    kind: KeyClauseKind::InSet(vals),
                }),
                ("<>", "all") => Some(KeyClause {
                    part,
                    kind: KeyClauseKind::NotInSet(vals),
                }),
                // `= ALL`: every element equal — a single point, or
                // the empty set when they differ.
                ("=", "all") => {
                    let first = vals.first()?.clone();
                    if vals.iter().all(|v| val_eq(v, &first) == Some(true)) {
                        Some(KeyClause {
                            part,
                            kind: KeyClauseKind::Cmp(CmpOp::Eq, first),
                        })
                    } else {
                        Some(KeyClause {
                            part,
                            kind: KeyClauseKind::InSet(Vec::new()),
                        })
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Parse a PG array-literal string (`{ab,bc}`, `{"a,b",NULL}`) into
/// pruning values. NULL elements are dropped: they can never satisfy
/// an equality step (PG19's pruning steps ignore them the same way).
/// Elements are typed as text — bound comparison coerces across the
/// text family, and non-text keys fail closed via compare returning
/// None downstream. Returns None on malformed input.
fn parse_array_literal(t: &str) -> Option<(Vec<Value>, bool)> {
    let t = t.trim();
    let inner = t.strip_prefix('{')?.strip_suffix('}')?;
    if inner.is_empty() {
        return Some((Vec::new(), false));
    }
    let mut out = Vec::new();
    let mut had_null = false;
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = inner.chars().peekable();
    let flush = |cur: &mut String, quoted: bool, out: &mut Vec<Value>, had_null: &mut bool| {
        let raw = std::mem::take(cur);
        if !quoted && raw.eq_ignore_ascii_case("null") {
            *had_null = true;
            return;
        }
        out.push(Value::Text(raw.into()));
    };
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                '"' => quoted = false,
                _ => cur.push(c),
            }
        } else {
            match c {
                '"' => quoted = true,
                ',' => flush(&mut cur, false, &mut out, &mut had_null),
                _ => cur.push(c),
            }
        }
    }
    if quoted {
        return None;
    }
    flush(&mut cur, false, &mut out, &mut had_null);
    Some((out, had_null))
}

/// If `e` is `NOT (<in-set form>)` — an Or chain of Eq clauses on one
/// key part, or the `__any_all_array` Eq/Any builtin — return the
/// equivalent NotInSet clause. (The parser builds NOT IN as
/// Not(Or(Eq, Eq, ...)).)
fn not_in_clause(
    e: &Expr,
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
    db: &Database,
) -> Option<KeyClause> {
    let inner = match e {
        Expr::Not(inner) => inner.as_ref(),
        _ => return None,
    };
    // Direct builtin form: NOT (key = ANY(array)).
    if let Some(kc) = extract_key_clause(inner, pinfo, parent, parent_name, parent_alias, db) {
        if let KeyClauseKind::InSet(vals) = kc.kind {
            return Some(KeyClause {
                part: kc.part,
                kind: KeyClauseKind::NotInSet(vals),
            });
        }
        return None;
    }
    // Or chain of Eq on one part.
    fn arms<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        match e {
            Expr::Or(a, b) => {
                arms(a, out);
                arms(b, out);
            }
            _ => out.push(e),
        }
    }
    let mut parts = Vec::new();
    arms(inner, &mut parts);
    if parts.len() < 2 {
        return None;
    }
    let mut part_no: Option<usize> = None;
    let mut vals = Vec::new();
    for a in parts {
        let kc = extract_key_clause(a, pinfo, parent, parent_name, parent_alias, db)?;
        match kc.kind {
            KeyClauseKind::Cmp(CmpOp::Eq, v) => {
                match part_no {
                    None => part_no = Some(kc.part),
                    Some(p) if p != kc.part => return None,
                    _ => {}
                }
                vals.push(v);
            }
            _ => return None,
        }
    }
    Some(KeyClause {
        part: part_no?,
        kind: KeyClauseKind::NotInSet(vals),
    })
}

/// Three-valued evaluation of a comparison of concrete values:
/// Some(true/false), or None when the values are incomparable.
fn cmp3(a: &Value, op: CmpOp, b: &Value) -> Option<bool> {
    let ord = compare_partition_values(a, b)?;
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

fn val_eq(a: &Value, b: &Value) -> Option<bool> {
    cmp3(a, CmpOp::Eq, b)
}

/// Exact (can-be-TRUE, can-be-FALSE) of one key clause for one
/// concrete key value. A NULL value makes every comparison UNKNOWN:
/// (false, false) — never TRUE (so it cannot witness `may`) and
/// never FALSE either (so NOT of it is still never TRUE).
fn clause_point(kc: &KeyClause, v: &Value) -> (bool, bool) {
    if *v == Value::Null {
        return match &kc.kind {
            KeyClauseKind::IsNull { neg } => {
                if *neg {
                    (false, true)
                } else {
                    (true, false)
                }
            }
            // IS TRUE/FALSE at NULL is FALSE; IS NOT TRUE/FALSE at
            // NULL is TRUE (never UNKNOWN).
            KeyClauseKind::IsBool { neg, .. } => {
                if *neg {
                    (true, false)
                } else {
                    (false, true)
                }
            }
            _ => (false, false),
        };
    }
    match &kc.kind {
        KeyClauseKind::Cmp(op, cv) => match cmp3(v, *op, cv) {
            Some(b) => (b, !b),
            None => (true, true),
        },
        KeyClauseKind::InSet(vals) => {
            let mut unknown = false;
            for x in vals {
                match val_eq(v, x) {
                    Some(true) => return (true, false),
                    Some(false) => {}
                    None => unknown = true,
                }
            }
            if unknown { (true, true) } else { (false, true) }
        }
        KeyClauseKind::NotInSet(vals) => {
            let mut unknown = false;
            for x in vals {
                match val_eq(v, x) {
                    Some(true) => return (false, true),
                    Some(false) => {}
                    None => unknown = true,
                }
            }
            if unknown { (true, true) } else { (true, false) }
        }
        KeyClauseKind::IsNull { neg } => {
            if *neg {
                (true, false)
            } else {
                (false, true)
            }
        }
        KeyClauseKind::IsBool { neg, val } => match v {
            Value::Bool(x) => {
                let t = (*x == *val) != *neg;
                (t, !t)
            }
            _ => (true, true),
        },
    }
}

/// One end of an effective per-part interval for a RANGE bound.
#[derive(Clone, Debug, PartialEq)]
enum End {
    NegInf,
    Val(Value, bool), // value, inclusive
    PosInf,
}

/// Compare a bound vector's prefix against concrete values.
fn cmp_bound_prefix(bounds: &[RangeBound], prefix: &[Value]) -> Option<Ordering> {
    for (i, v) in prefix.iter().enumerate() {
        match &bounds[i] {
            RangeBound::Min => return Some(Ordering::Less),
            RangeBound::Max => return Some(Ordering::Greater),
            RangeBound::Val(bv) => match compare_partition_values(bv, v)? {
                Ordering::Equal => {}
                other => return Some(other),
            },
        }
    }
    Some(Ordering::Equal)
}

/// The effective interval of part `k` when the CLAUSE pins every
/// earlier part to a single value (prefix from `pins`): the bound's
/// prefix decides — below the pin the part is unbounded, above it
/// the leaf holds no tuple with this prefix at all (None).
fn eff_interval_pinned(
    lower: &[RangeBound],
    upper: &[RangeBound],
    k: usize,
    pins: &[Option<Value>],
) -> Option<(End, End)> {
    let prefix: Vec<Value> = pins[..k].iter().map(|p| p.clone().unwrap()).collect();
    let lo = match cmp_bound_prefix(lower, &prefix)? {
        Ordering::Less => End::NegInf,
        Ordering::Greater => return None,
        Ordering::Equal => match &lower[k] {
            RangeBound::Min => End::NegInf,
            RangeBound::Max => End::PosInf,
            RangeBound::Val(v) => End::Val(v.clone(), true),
        },
    };
    let hi = match cmp_bound_prefix(upper, &prefix)? {
        Ordering::Less => return None,
        Ordering::Greater => End::PosInf,
        Ordering::Equal => match &upper[k] {
            RangeBound::Max => End::PosInf,
            RangeBound::Min => End::NegInf,
            RangeBound::Val(v) => End::Val(v.clone(), upper_incl(upper, k)),
        },
    };
    Some((lo, hi))
}

/// Whether part `k` of an upper bound can be EQUALLED by a tuple in
/// the bound: yes when some later part can sit below its own upper
/// end (any later upper part other than MINVALUE).
fn upper_incl(upper: &[RangeBound], k: usize) -> bool {
    upper[k + 1..].iter().any(|b| !matches!(b, RangeBound::Min))
}

/// The effective [lo, hi] interval of key part `k` inside the range
/// bound [lower, upper): when every earlier part is pinned (lower ==
/// upper on that part), part k ranges over [lower[k], upper[k]);
/// otherwise earlier parts can sit strictly inside, so part k is
/// effectively unbounded.
fn eff_interval(lower: &[RangeBound], upper: &[RangeBound], k: usize) -> (End, End) {
    for i in 0..k {
        let pinned = match (&lower[i], &upper[i]) {
            (RangeBound::Min, RangeBound::Min) => true,
            (RangeBound::Max, RangeBound::Max) => true,
            (RangeBound::Val(a), RangeBound::Val(b)) => val_eq(a, b) == Some(true),
            _ => false,
        };
        if !pinned {
            return (End::NegInf, End::PosInf);
        }
    }
    let lo = match &lower[k] {
        RangeBound::Min => End::NegInf,
        RangeBound::Max => End::PosInf,
        RangeBound::Val(v) => End::Val(v.clone(), true),
    };
    let hi = match &upper[k] {
        RangeBound::Max => End::PosInf,
        RangeBound::Min => End::NegInf,
        RangeBound::Val(v) => End::Val(v.clone(), upper_incl(upper, k)),
    };
    (lo, hi)
}

/// Is `v` inside the effective interval? None = incomparable.
fn in_interval(v: &Value, lo: &End, hi: &End) -> Option<bool> {
    let above_lo = match lo {
        End::NegInf => true,
        End::PosInf => false,
        End::Val(lv, incl) => match compare_partition_values(v, lv)? {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => *incl,
        },
    };
    let below_hi = match hi {
        End::PosInf => true,
        End::NegInf => false,
        End::Val(hv, incl) => match compare_partition_values(v, hv)? {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => *incl,
        },
    };
    Some(above_lo && below_hi)
}

/// (may, must) of one key clause against a RANGE bound. Conservative
/// on `must` (it only feeds NOT handling): under-claiming must is
/// sound, over-claiming is not.
fn clause_range(kc: &KeyClause, lower: &[RangeBound], upper: &[RangeBound]) -> (bool, bool) {
    let (lo, hi) = eff_interval(lower, upper, kc.part);
    match &kc.kind {
        KeyClauseKind::IsNull { neg } => {
            // Range bounds never contain NULL key values.
            if *neg { (true, true) } else { (false, false) }
        }
        KeyClauseKind::Cmp(op, cv) => {
            // may: the clause's solution set intersects [lo, hi].
            let may = match op {
                CmpOp::Eq => in_interval(cv, &lo, &hi).unwrap_or(true),
                CmpOp::Ne => {
                    // False only when the interval is exactly {cv}.
                    let point = matches!(&lo, End::Val(lv, true) if val_eq(lv, cv) == Some(true))
                        && matches!(&hi, End::Val(hv, false) if val_eq(hv, cv) == Some(true));
                    !point
                }
                CmpOp::Lt => match &lo {
                    End::NegInf => true,
                    End::PosInf => false,
                    End::Val(lv, _) => cmp3(lv, CmpOp::Lt, cv).unwrap_or(true),
                },
                CmpOp::Le => match &lo {
                    End::NegInf => true,
                    End::PosInf => false,
                    End::Val(lv, _) => cmp3(lv, CmpOp::Le, cv).unwrap_or(true),
                },
                CmpOp::Gt => match &hi {
                    End::PosInf => true,
                    End::NegInf => false,
                    End::Val(hv, _) => cmp3(hv, CmpOp::Gt, cv).unwrap_or(true),
                },
                CmpOp::Ge => match &hi {
                    End::PosInf => true,
                    End::NegInf => false,
                    End::Val(hv, _) => cmp3(hv, CmpOp::Ge, cv).unwrap_or(true),
                },
                CmpOp::ImageEq => true,
            };
            // must: the whole interval lies inside the solution set.
            let must = match op {
                CmpOp::Eq => {
                    matches!(&lo, End::Val(lv, true) if val_eq(lv, cv) == Some(true))
                        && matches!(&hi, End::Val(hv, false) if val_eq(hv, cv) == Some(true))
                }
                CmpOp::Ne => in_interval(cv, &lo, &hi) == Some(false),
                CmpOp::Lt | CmpOp::Le => match (&hi, op) {
                    (End::Val(hv, _), CmpOp::Lt) => cmp3(hv, CmpOp::Le, cv).unwrap_or(false),
                    (End::Val(hv, _), CmpOp::Le) => cmp3(hv, CmpOp::Le, cv).unwrap_or(false),
                    _ => false,
                },
                CmpOp::Gt | CmpOp::Ge => match (&lo, op) {
                    (End::Val(lv, _), CmpOp::Gt) => cmp3(lv, CmpOp::Ge, cv).unwrap_or(false),
                    (End::Val(lv, _), CmpOp::Ge) => cmp3(lv, CmpOp::Ge, cv).unwrap_or(false),
                    _ => false,
                },
                CmpOp::ImageEq => false,
            };
            (may, must)
        }
        KeyClauseKind::InSet(vals) => {
            let may = vals
                .iter()
                .any(|v| in_interval(v, &lo, &hi).unwrap_or(true));
            let must = matches!(&lo, End::Val(lv, true)
                if matches!(&hi, End::Val(hv, false) if val_eq(lv, hv) == Some(true))
                    && vals.iter().any(|v| val_eq(lv, v) == Some(true)));
            (may, must)
        }
        KeyClauseKind::NotInSet(vals) => {
            let point = match (&lo, &hi) {
                (End::Val(lv, true), End::Val(hv, false)) if val_eq(lv, hv) == Some(true) => {
                    Some(lv.clone())
                }
                _ => None,
            };
            let may = match &point {
                Some(p) => !vals.iter().any(|v| val_eq(p, v) == Some(true)),
                None => true,
            };
            let must = vals.iter().all(|v| in_interval(v, &lo, &hi) == Some(false));
            (may, must)
        }
        KeyClauseKind::IsBool { .. } => {
            let r = clause_region(kc);
            (segs_overlap(&r.segs, &[(lo, hi)]), false)
        }
    }
}

/// (may, must) of one key clause against a child's bound, used for
/// the compositional `must` predicate (sound per-clause) and as the
/// LIST pointwise primitive. NOT used for `may` under And: two
/// clauses may be witnessed by different bound points, so And-may
/// needs the joint tests below.
fn clause_bound(kc: &KeyClause, method: &PartMethod, bound: &PartBound) -> (bool, bool) {
    match (method, bound) {
        (PartMethod::List, PartBound::List { values, has_null }) => {
            let mut may = false;
            let mut must = !values.is_empty() || *has_null;
            for v in values {
                let (t, f) = clause_point(kc, v);
                may |= t;
                must &= t && !f;
            }
            if *has_null {
                let (t, f) = clause_point(kc, &Value::Null);
                may |= t;
                must &= t && !f;
            }
            (may, must)
        }
        (PartMethod::Range, PartBound::Range { lower, upper }) => clause_range(kc, lower, upper),
        // Hash bounds and shape mismatches: no per-clause proof.
        _ => (true, false),
    }
}

struct PruneCtx<'a> {
    pinfo: &'a PartitionInfo,
    parent: &'a Table,
    parent_name: &'a str,
    parent_alias: Option<&'a str>,
    db: &'a Database,
    method: &'a PartMethod,
    bound: &'a PartBound,
}

/// Compositional `must`: every row in the bound satisfies `e`.
/// Per-clause musts compose soundly under And/Or/Not.
fn bound_must(cx: &PruneCtx, e: &Expr) -> bool {
    if let Some(kc) = not_in_clause(
        e,
        cx.pinfo,
        cx.parent,
        cx.parent_name,
        cx.parent_alias,
        cx.db,
    ) {
        return clause_bound(&kc, cx.method, cx.bound).1;
    }
    match e {
        Expr::And(a, b) => bound_must(cx, a) && bound_must(cx, b),
        Expr::Or(a, b) => bound_must(cx, a) || bound_must(cx, b),
        Expr::Not(inner) => !bound_may(cx, inner),
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            let both = Expr::And(
                Box::new(Expr::Cmp {
                    op: CmpOp::Ge,
                    left: expr.clone(),
                    right: low.clone(),
                }),
                Box::new(Expr::Cmp {
                    op: CmpOp::Le,
                    left: expr.clone(),
                    right: high.clone(),
                }),
            );
            if *neg {
                !bound_may(cx, &both)
            } else {
                bound_must(cx, &both)
            }
        }
        Expr::Literal(Literal::Bool(b)) => *b,
        Expr::Literal(Literal::Null) => false,
        _ => {
            match extract_key_clause(
                e,
                cx.pinfo,
                cx.parent,
                cx.parent_name,
                cx.parent_alias,
                cx.db,
            ) {
                Some(kc) => clause_bound(&kc, cx.method, cx.bound).1,
                None => false,
            }
        }
    }
}

/// (can-be-TRUE, can-be-FALSE) of `e` for one concrete key point
/// (LIST bounds): clauses on part 0 evaluate exactly via
/// clause_point; anything else is (true, true) — it may go either
/// way depending on columns the bound does not pin.
fn eval_pair_point(cx: &PruneCtx, e: &Expr, point: &Value) -> (bool, bool) {
    match e {
        Expr::And(a, b) => {
            let (t1, f1) = eval_pair_point(cx, a, point);
            let (t2, f2) = eval_pair_point(cx, b, point);
            (t1 && t2, f1 || f2)
        }
        Expr::Or(a, b) => {
            let (t1, f1) = eval_pair_point(cx, a, point);
            let (t2, f2) = eval_pair_point(cx, b, point);
            (t1 || t2, f1 && f2)
        }
        Expr::Not(inner) => {
            let (t, f) = eval_pair_point(cx, inner, point);
            (f, t)
        }
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            let both = Expr::And(
                Box::new(Expr::Cmp {
                    op: CmpOp::Ge,
                    left: expr.clone(),
                    right: low.clone(),
                }),
                Box::new(Expr::Cmp {
                    op: CmpOp::Le,
                    left: expr.clone(),
                    right: high.clone(),
                }),
            );
            let (t, f) = eval_pair_point(cx, &both, point);
            if *neg { (f, t) } else { (t, f) }
        }
        Expr::Literal(Literal::Bool(b)) => (*b, !*b),
        Expr::Literal(Literal::Null) => (false, false),
        _ => {
            match extract_key_clause(
                e,
                cx.pinfo,
                cx.parent,
                cx.parent_name,
                cx.parent_alias,
                cx.db,
            ) {
                Some(kc) if kc.part == 0 => clause_point(&kc, point),
                _ => (true, true),
            }
        }
    }
}

/// A value-line region for one key part: disjoint segments plus a
/// NULL flag (RANGE joint-may machinery).
#[derive(Clone, Debug)]
struct Region {
    segs: Vec<(End, End)>,
    null: bool,
    /// v2.32: the union of EQ/IN point sets constraining this part,
    /// when any. PG19's pruning steps do NOT intersect two equality
    /// steps on the same part (its step results combine per step),
    /// so `(a = 1 AND a = 3)` keeps every partition containing 1 or
    /// 3; range-op clauses, by contrast, DO intersect. The points
    /// therefore accumulate by union under And and are intersected
    /// with `segs` only when the region is consumed.
    points: Option<Vec<Value>>,
}

impl Region {
    fn full() -> Region {
        Region {
            segs: vec![(End::NegInf, End::PosInf)],
            null: true,
            points: None,
        }
    }

    /// The segment list the region actually denotes: `segs`
    /// intersected with the EQ/IN point set, when one accumulated.
    fn materialized(&self) -> Vec<(End, End)> {
        match &self.points {
            None => self.segs.clone(),
            Some(pts) => {
                let psegs: Vec<(End, End)> = pts
                    .iter()
                    .map(|v| (End::Val(v.clone(), true), End::Val(v.clone(), true)))
                    .collect();
                let mut out = Vec::new();
                for s1 in &self.segs {
                    for s2 in &psegs {
                        if let Some(s) = seg_intersect(s1, s2) {
                            out.push(s);
                        }
                    }
                }
                out
            }
        }
    }

    /// The single value this region pins the part to, if it denotes
    /// exactly one point.
    fn pin(&self) -> Option<Value> {
        let m = self.materialized();
        match m.as_slice() {
            [(End::Val(a, true), End::Val(b, true))] if val_eq(a, b) == Some(true) => {
                Some(a.clone())
            }
            _ => None,
        }
    }
}

fn clause_region(kc: &KeyClause) -> Region {
    match &kc.kind {
        KeyClauseKind::Cmp(op, v) => {
            if matches!(op, CmpOp::Eq) {
                return Region {
                    segs: vec![(End::NegInf, End::PosInf)],
                    null: false,
                    points: Some(vec![v.clone()]),
                };
            }
            let segs = match op {
                CmpOp::Eq => vec![(End::Val(v.clone(), true), End::Val(v.clone(), true))],
                CmpOp::Ne => vec![
                    (End::NegInf, End::Val(v.clone(), false)),
                    (End::Val(v.clone(), false), End::PosInf),
                ],
                CmpOp::Lt => vec![(End::NegInf, End::Val(v.clone(), false))],
                CmpOp::Le => vec![(End::NegInf, End::Val(v.clone(), true))],
                CmpOp::Gt => vec![(End::Val(v.clone(), false), End::PosInf)],
                CmpOp::Ge => vec![(End::Val(v.clone(), true), End::PosInf)],
                CmpOp::ImageEq => return Region::full(),
            };
            Region {
                segs,
                null: false,
                points: None,
            }
        }
        KeyClauseKind::InSet(vals) => Region {
            segs: vec![(End::NegInf, End::PosInf)],
            null: vals.iter().any(|v| *v == Value::Null),
            points: Some(
                vals.iter()
                    .filter(|v| **v != Value::Null)
                    .cloned()
                    .collect(),
            ),
        },
        KeyClauseKind::NotInSet(vals) => {
            // Complement of the sorted distinct points.
            let mut pts: Vec<Value> = vals
                .iter()
                .filter(|v| **v != Value::Null)
                .cloned()
                .collect();
            pts.sort_by(|a, b| compare_partition_values(a, b).unwrap_or(Ordering::Equal));
            pts.dedup_by(|a, b| val_eq(a, b) == Some(true));
            let mut segs = Vec::new();
            let mut lo = End::NegInf;
            for p in &pts {
                segs.push((lo, End::Val(p.clone(), false)));
                lo = End::Val(p.clone(), false);
            }
            segs.push((lo, End::PosInf));
            Region {
                segs,
                null: false,
                points: None,
            }
        }
        KeyClauseKind::IsNull { neg } => {
            if *neg {
                Region {
                    segs: vec![(End::NegInf, End::PosInf)],
                    null: false,
                    points: None,
                }
            } else {
                Region {
                    segs: Vec::new(),
                    null: true,
                    points: None,
                }
            }
        }
        KeyClauseKind::IsBool { neg, val } => {
            let bv = Value::Bool(*val);
            if *neg {
                Region {
                    segs: vec![
                        (End::NegInf, End::Val(bv.clone(), false)),
                        (End::Val(bv, false), End::PosInf),
                    ],
                    null: true,
                    points: None,
                }
            } else {
                Region {
                    segs: vec![(End::Val(bv.clone(), true), End::Val(bv, true))],
                    null: false,
                    points: None,
                }
            }
        }
    }
}

/// Compare a lower end against an upper end: Less/Equal means the
/// segment intersection is nonempty at this pair. None = incomparable
/// (caller treats as overlapping, conservatively).
fn end_le(lo: &End, hi: &End) -> Option<bool> {
    match (lo, hi) {
        (End::NegInf, _) | (_, End::PosInf) => Some(true),
        (End::PosInf, _) | (_, End::NegInf) => Some(false),
        (End::Val(a, ai), End::Val(b, bi)) => match compare_partition_values(a, b)? {
            Ordering::Less => Some(true),
            Ordering::Greater => Some(false),
            Ordering::Equal => Some(*ai && *bi),
        },
    }
}

fn segs_overlap(a: &[(End, End)], b: &[(End, End)]) -> bool {
    for (l1, h1) in a {
        for (l2, h2) in b {
            let ok1 = end_le(l1, h2).unwrap_or(true);
            let ok2 = end_le(l2, h1).unwrap_or(true);
            if ok1 && ok2 {
                return true;
            }
        }
    }
    false
}

fn seg_intersect(a: &(End, End), b: &(End, End)) -> Option<(End, End)> {
    // max of lowers, min of uppers, under end ordering where a Val
    // lower with inclusive=false is greater than the same Val incl.
    fn max_lo(x: &End, y: &End) -> End {
        match (x, y) {
            (End::NegInf, _) => y.clone(),
            (_, End::NegInf) => x.clone(),
            (End::PosInf, _) | (_, End::PosInf) => End::PosInf,
            (End::Val(a, ai), End::Val(b, bi)) => match compare_partition_values(a, b) {
                Some(Ordering::Greater) => x.clone(),
                Some(Ordering::Less) => y.clone(),
                _ => End::Val(a.clone(), *ai && *bi),
            },
        }
    }
    fn min_hi(x: &End, y: &End) -> End {
        match (x, y) {
            (End::PosInf, _) => y.clone(),
            (_, End::PosInf) => x.clone(),
            (End::NegInf, _) | (_, End::NegInf) => End::NegInf,
            (End::Val(a, ai), End::Val(b, bi)) => match compare_partition_values(a, b) {
                Some(Ordering::Less) => x.clone(),
                Some(Ordering::Greater) => y.clone(),
                _ => End::Val(a.clone(), *ai && *bi),
            },
        }
    }
    let lo = max_lo(&a.0, &b.0);
    let hi = min_hi(&a.1, &b.1);
    if end_le(&lo, &hi).unwrap_or(true) {
        Some((lo, hi))
    } else {
        None
    }
}

fn region_intersect(a: &Region, b: &Region) -> Region {
    let mut segs = Vec::new();
    for s1 in &a.segs {
        for s2 in &b.segs {
            if let Some(s) = seg_intersect(s1, s2) {
                segs.push(s);
            }
        }
    }
    // EQ/IN point sets UNION under And (see Region::points).
    let points = match (&a.points, &b.points) {
        (Some(x), Some(y)) => {
            let mut u = x.clone();
            for v in y {
                if !u.iter().any(|w| val_eq(w, v) == Some(true)) {
                    u.push(v.clone());
                }
            }
            Some(u)
        }
        (Some(x), None) => Some(x.clone()),
        (None, Some(y)) => Some(y.clone()),
        (None, None) => None,
    };
    Region {
        segs,
        null: a.null && b.null,
        points,
    }
}

fn region_union(a: &Region, b: &Region) -> Region {
    let mut segs = a.materialized();
    segs.extend(b.materialized());
    Region {
        segs,
        null: a.null || b.null,
        points: None,
    }
}

/// Per-part regions implied by `e`, when `e` is separable (And of
/// single-part constraints; Or only over arms constraining one common
/// part). Returns None for unseparable shapes (cross-part Or, Not) —
/// the caller then falls back to the compositional may (over-keep).
fn conjunct_regions(cx: &PruneCtx, e: &Expr) -> Option<Vec<Option<Region>>> {
    let n = cx.pinfo.key.len();
    if let Some(kc) = not_in_clause(
        e,
        cx.pinfo,
        cx.parent,
        cx.parent_name,
        cx.parent_alias,
        cx.db,
    ) {
        let mut out: Vec<Option<Region>> = vec![None; n];
        out[kc.part] = Some(clause_region(&kc));
        return Some(out);
    }
    match e {
        Expr::And(a, b) => {
            let ra = conjunct_regions(cx, a)?;
            let rb = conjunct_regions(cx, b)?;
            let mut out: Vec<Option<Region>> = vec![None; n];
            for i in 0..n {
                out[i] = match (ra[i].clone(), rb[i].clone()) {
                    (Some(x), Some(y)) => Some(region_intersect(&x, &y)),
                    (Some(x), None) => Some(x),
                    (None, y) => y,
                };
            }
            Some(out)
        }
        Expr::Or(a, b) => {
            // Both arms must constrain the same single part (other
            // parts unconstrained in both arms).
            fn single(cx: &PruneCtx, e: &Expr) -> Option<(usize, Region)> {
                let regs = conjunct_regions(cx, e)?;
                let mut found: Option<(usize, Region)> = None;
                for (i, r) in regs.into_iter().enumerate() {
                    if let Some(r) = r {
                        if found.is_some() {
                            return None;
                        }
                        found = Some((i, r));
                    }
                }
                found
            }
            let (p1, r1) = single(cx, a)?;
            let (p2, r2) = single(cx, b)?;
            if p1 != p2 {
                return None;
            }
            let mut out: Vec<Option<Region>> = vec![None; n];
            out[p1] = Some(region_union(&r1, &r2));
            Some(out)
        }
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            if *neg {
                return None;
            }
            let both = Expr::And(
                Box::new(Expr::Cmp {
                    op: CmpOp::Ge,
                    left: expr.clone(),
                    right: low.clone(),
                }),
                Box::new(Expr::Cmp {
                    op: CmpOp::Le,
                    left: expr.clone(),
                    right: high.clone(),
                }),
            );
            conjunct_regions(cx, &both)
        }
        Expr::Literal(Literal::Bool(true)) => Some(vec![None; n]),
        Expr::Literal(Literal::Bool(false)) => {
            let mut out: Vec<Option<Region>> = vec![None; n];
            if n > 0 {
                out[0] = Some(Region {
                    segs: Vec::new(),
                    null: false,
                    points: None,
                });
            }
            Some(out)
        }
        _ => {
            match extract_key_clause(
                e,
                cx.pinfo,
                cx.parent,
                cx.parent_name,
                cx.parent_alias,
                cx.db,
            ) {
                Some(kc) => {
                    let mut out: Vec<Option<Region>> = vec![None; n];
                    out[kc.part] = Some(clause_region(&kc));
                    Some(out)
                }
                // A non-key conjunct constrains nothing we model.
                None => Some(vec![None; n]),
            }
        }
    }
}

/// Compositional `may` fallback (per-clause, over-keeping under And —
/// used only when the joint region test cannot separate the shape).
fn bound_may(cx: &PruneCtx, e: &Expr) -> bool {
    if let Some(kc) = not_in_clause(
        e,
        cx.pinfo,
        cx.parent,
        cx.parent_name,
        cx.parent_alias,
        cx.db,
    ) {
        return clause_bound(&kc, cx.method, cx.bound).0;
    }
    match e {
        Expr::And(a, b) => bound_may(cx, a) && bound_may(cx, b),
        Expr::Or(a, b) => bound_may(cx, a) || bound_may(cx, b),
        Expr::Not(inner) => !bound_must(cx, inner),
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            let both = Expr::And(
                Box::new(Expr::Cmp {
                    op: CmpOp::Ge,
                    left: expr.clone(),
                    right: low.clone(),
                }),
                Box::new(Expr::Cmp {
                    op: CmpOp::Le,
                    left: expr.clone(),
                    right: high.clone(),
                }),
            );
            if *neg {
                !bound_must(cx, &both)
            } else {
                bound_may(cx, &both)
            }
        }
        Expr::Literal(Literal::Bool(b)) => *b,
        Expr::Literal(Literal::Null) => false,
        _ => {
            match extract_key_clause(
                e,
                cx.pinfo,
                cx.parent,
                cx.parent_name,
                cx.parent_alias,
                cx.db,
            ) {
                Some(kc) => clause_bound(&kc, cx.method, cx.bound).0,
                None => true,
            }
        }
    }
}

/// The joint (may, must) of `where` against one child's bound.
fn bound_may_must(cx: &PruneCtx, e: &Expr) -> (bool, bool) {
    let must = bound_must(cx, e);
    let may = match (cx.method, cx.bound) {
        (PartMethod::List, PartBound::List { values, has_null }) => {
            let mut may = false;
            for v in values {
                if eval_pair_point(cx, e, v).0 {
                    may = true;
                }
            }
            if *has_null && eval_pair_point(cx, e, &Value::Null).0 {
                may = true;
            }
            may
        }
        (PartMethod::Range, PartBound::Range { lower, upper }) => {
            // Per OR group: per-part clause regions (EQ/IN points
            // unioned, range ops intersected), tested against the
            // bound's effective interval for the part — the
            // clause-pinned interval when the group pins every
            // earlier part (PG19's prefix-step discipline: a step on
            // part k only separates when the earlier parts are
            // pinned, by the clause or by the bound itself).
            let mut any = false;
            for group in or_groups(e) {
                // Combine the group's conjuncts into per-part regions.
                let mut regions: Vec<Option<Region>> = vec![None; cx.pinfo.key.len()];
                let mut separable = true;
                for conjunct in &group {
                    match conjunct_regions(cx, conjunct) {
                        Some(regs) => {
                            for (i, r) in regs.into_iter().enumerate() {
                                if let Some(r) = r {
                                    regions[i] = Some(match regions[i].take() {
                                        Some(prev) => region_intersect(&prev, &r),
                                        None => r,
                                    });
                                }
                            }
                        }
                        None => {
                            separable = false;
                            break;
                        }
                    }
                }
                if !separable {
                    // Unseparable group shape: the compositional
                    // per-clause may decides (over-keeps).
                    if bound_may(cx, e) {
                        any = true;
                    }
                    continue;
                }
                let pins: Vec<Option<Value>> = regions
                    .iter()
                    .map(|r| r.as_ref().and_then(|r| if r.null { None } else { r.pin() }))
                    .collect();
                let mut group_may = true;
                for (part, region) in regions.iter().enumerate() {
                    let region = match region {
                        Some(r) => r,
                        None => continue,
                    };
                    let domain = if part > 0 && pins[..part].iter().all(|p| p.is_some()) {
                        match eff_interval_pinned(lower, upper, part, &pins) {
                            Some(iv) => vec![iv],
                            None => {
                                group_may = false;
                                continue;
                            }
                        }
                    } else {
                        let (lo, hi) = eff_interval(lower, upper, part);
                        vec![(lo, hi)]
                    };
                    // Range domains never contain NULL, so the
                    // region's null flag cannot rescue it.
                    if !segs_overlap(&region.materialized(), &domain) {
                        group_may = false;
                    }
                }
                if group_may {
                    any = true;
                }
            }
            any
        }
        _ => bound_may(cx, e),
    };
    (may, must)
}

/// The AND-conjuncts of `where` (one group), or each arm of a
/// top-level OR chain as its own group.
fn or_groups(where_: &Expr) -> Vec<Vec<&Expr>> {
    fn arms<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        match e {
            Expr::Or(a, b) => {
                arms(a, out);
                arms(b, out);
            }
            _ => out.push(e),
        }
    }
    let mut tops = Vec::new();
    arms(where_, &mut tops);
    tops.iter().map(|t| split_conjuncts(t)).collect()
}

/// Key clauses of one AND group, by part.
fn group_clauses(
    group: &[&Expr],
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
    db: &Database,
) -> Vec<KeyClause> {
    group
        .iter()
        .filter_map(|e| extract_key_clause(e, pinfo, parent, parent_name, parent_alias, db))
        .collect()
}

/// RANGE default-coverage: is the key region implied by `where`
/// contained in the union of the sibling range bounds? The region is
/// built per OR group as a lexicographic interval; every group must be
/// covered. Any IS NULL on a key part makes the region include NULL,
/// which range siblings never cover.
fn range_default_covered(
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
    db: &Database,
    siblings: &[&PartBound],
    where_: &Expr,
    own_clip: Option<&(Vec<RangeBound>, Vec<RangeBound>)>,
) -> bool {
    let n = pinfo.key.len();
    // Sibling intervals, as (lower, upper) bound vectors.
    let mut sib: Vec<(&Vec<RangeBound>, &Vec<RangeBound>)> = Vec::new();
    for b in siblings {
        if let PartBound::Range { lower, upper } = b {
            sib.push((lower, upper));
        } else {
            return false;
        }
    }
    for group in or_groups(where_) {
        let clauses = group_clauses(&group, pinfo, parent, parent_name, parent_alias, db);
        // PG19 prunes a RANGE default only when the group's clauses
        // constrain EVERY key part (its pruning steps must form a
        // complete key prefix); a clause set silent on a later part
        // leaves the default in place (mc3p `a = 1`, rlp `a <= 31`).
        if (0..n).any(|k| !clauses.iter().any(|kc| kc.part == k)) {
            return false;
        }
        // Build the group's interval: per part, eq pins, else the
        // tightest range ends; IN on part 0 expands to point intervals
        // (handled by requiring an eq pin per value below).
        let mut lo: Vec<RangeBound> = vec![RangeBound::Min; n];
        let mut hi: Vec<RangeBound> = vec![RangeBound::Max; n];
        let mut in_sets: Vec<Option<Vec<Value>>> = vec![None; n];
        let mut empty = false;
        for kc in &clauses {
            match &kc.kind {
                KeyClauseKind::IsNull { neg: false } => return false,
                KeyClauseKind::IsNull { .. } => {}
                KeyClauseKind::InSet(vals) => {
                    in_sets[kc.part] = Some(vals.clone());
                }
                KeyClauseKind::NotInSet(_) => {}
                KeyClauseKind::IsBool { neg, val } => {
                    if *neg {
                        return false;
                    }
                    in_sets[kc.part] = Some(vec![Value::Bool(*val)]);
                }
                KeyClauseKind::Cmp(op, v) => match op {
                    CmpOp::Eq => {
                        in_sets[kc.part] = Some(vec![v.clone()]);
                    }
                    CmpOp::Gt | CmpOp::Ge => {
                        // Clause lower end: (v, exclusive for Gt).
                        // RangeBound has no exclusivity; a Gt end is
                        // recorded as Val(v) and checked with the
                        // strict flag kept alongside.
                        let cur = &lo[kc.part];
                        let tighter = match cur {
                            RangeBound::Min => true,
                            RangeBound::Val(cv) => {
                                compare_partition_values(v, cv) == Some(Ordering::Greater)
                            }
                            RangeBound::Max => false,
                        };
                        if tighter {
                            lo[kc.part] = RangeBound::Val(v.clone());
                        }
                        if *op == CmpOp::Gt {
                            // Represent (v, +inf) coverage need: the
                            // region starts strictly after v; sibling
                            // coverage of [v, ...) implies coverage of
                            // (v, ...) only when v itself is covered —
                            // it is, since [v,...) ⊇ (v,...). So the
                            // inclusive relaxation is sound here.
                        }
                    }
                    CmpOp::Lt | CmpOp::Le => {
                        let cur = &hi[kc.part];
                        let tighter = match cur {
                            RangeBound::Max => true,
                            RangeBound::Val(cv) => {
                                compare_partition_values(v, cv) == Some(Ordering::Less)
                            }
                            RangeBound::Min => false,
                        };
                        if tighter {
                            hi[kc.part] = RangeBound::Val(v.clone());
                        }
                    }
                    _ => {}
                },
            }
        }
        // Clip the group's region by this table's own bound: the
        // region only matters inside the bound.
        let mut hi_excl = false;
        if let Some((clo, chi)) = own_clip {
            if bound_tuple_cmp(&lo, clo) == Some(Ordering::Less) {
                lo = clo.clone();
            }
            match bound_tuple_cmp(&hi, chi) {
                Some(Ordering::Greater) => {
                    hi = chi.clone();
                    hi_excl = true;
                }
                Some(Ordering::Equal) => {
                    hi_excl = true;
                }
                _ => {}
            }
        }
        // Expand IN/EQ pins on part 0 into point checks; pins on later
        // parts only matter when all earlier parts are pinned — the
        // interval built above already pins them via lo/hi when the
        // pin came from Eq; InSet pins on later parts are ignored
        // (conservative: region wider, coverage harder to prove).
        let mut intervals: Vec<(Vec<RangeBound>, Vec<RangeBound>, bool)> = Vec::new();
        match &in_sets[0] {
            Some(vals) => {
                for v in vals {
                    let mut l = lo.clone();
                    let mut h = hi.clone();
                    l[0] = RangeBound::Val(v.clone());
                    h[0] = RangeBound::Val(v.clone());
                    intervals.push((l, h, hi_excl));
                }
                if vals.is_empty() {
                    empty = true;
                }
            }
            None => intervals.push((lo, hi, hi_excl)),
        }
        if empty {
            continue;
        }
        for (l, h, hx) in &intervals {
            // Point-interval [v, v] in bound terms is [v, next) — for
            // coverage testing treat the region as the closed tuple
            // range [l, h] and test: every sibling chain from the one
            // containing l reaches at least h. Simpler sufficient
            // test: the region's lower tuple is inside some sibling,
            // and that sibling's upper exceeds the region's upper
            // tuple (siblings are disjoint, so a region spanning a
            // gap between siblings is uncovered; a region spanning
            // ADJACENT siblings [a,b),[b,c) is covered — handle by
            // walking the chain).
            if !tuple_interval_covered(&sib, l, h, *hx) {
                return false;
            }
        }
    }
    true
}

/// Lexicographic compare of two bound tuples (Min < Val < Max).
fn bound_tuple_cmp(a: &[RangeBound], b: &[RangeBound]) -> Option<Ordering> {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = match (x, y) {
            (RangeBound::Min, RangeBound::Min) => Ordering::Equal,
            (RangeBound::Max, RangeBound::Max) => Ordering::Equal,
            (RangeBound::Min, _) => Ordering::Less,
            (_, RangeBound::Min) => Ordering::Greater,
            (RangeBound::Max, _) => Ordering::Greater,
            (_, RangeBound::Max) => Ordering::Less,
            (RangeBound::Val(v1), RangeBound::Val(v2)) => compare_partition_values(v1, v2)?,
        };
        if ord != Ordering::Equal {
            return Some(ord);
        }
    }
    Some(Ordering::Equal)
}

/// Is the closed tuple region [l, h] (with l/hi ends as bound vectors,
/// region = tuples t with l <= t <= h under bound semantics where a
/// Val end at position i with all-equal prefix means exactly that
/// value) covered by the union of sibling [lower, upper) intervals?
/// Sufficient-and-usual test: walk siblings in lower order, advancing
/// a coverage cursor; the region is covered when the cursor passes h
/// without a gap. The cursor starts at l; a sibling [sl, su) extends
/// coverage when sl <= cursor (tuple compare, where cursor as a lower
/// end) and su > cursor; coverage completes when su > h as tuples, or
/// su == h-as-upper (su is exclusive: su reaching the first tuple
/// above h — approximated by su > h, or su == h with h being a
/// point-pin end... conservative: require su > h, or the region is a
/// single point and su > that point).
fn tuple_interval_covered(
    sib: &[(&Vec<RangeBound>, &Vec<RangeBound>)],
    l: &[RangeBound],
    h: &[RangeBound],
    h_excl: bool,
) -> bool {
    // Region empty (l > h)? trivially covered.
    if bound_tuple_cmp(l, h) == Some(Ordering::Greater) {
        return true;
    }
    let mut order: Vec<usize> = (0..sib.len()).collect();
    order.sort_by(|&a, &b| bound_tuple_cmp(sib[a].0, sib[b].0).unwrap_or(Ordering::Equal));
    // cursor = current covered frontier as an upper-bound vector.
    let mut cursor: Vec<RangeBound> = l.to_vec();
    let mut started = false;
    for &i in &order {
        let (sl, su) = sib[i];
        // Sibling must start at or before the cursor.
        if bound_tuple_cmp(sl, &cursor) == Some(Ordering::Greater) {
            if started {
                break; // gap
            }
            continue;
        }
        started = true;
        if bound_tuple_cmp(su, &cursor) == Some(Ordering::Greater) {
            cursor = su.clone();
        }
        // Covered when the frontier passes the region's high end:
        // frontier is exclusive, region high end is a closed tuple;
        // frontier > h suffices; frontier == h suffices only when h
        // came from an Lt end (region excludes h)... our h is Val for
        // both Le (includes) and Lt (excludes); treating == as
        // covered is exact for Lt and for point pins (frontier is the
        // sibling's exclusive upper = first tuple above the pin when
        // siblings are adjacent at the pin's successor — not in
        // general). Stay conservative: require frontier > h, except
        // when the region is exactly the frontier's predecessor point
        // is unknowable — accept == only for single-point regions
        // whose point is < frontier... that IS frontier > point.
        match bound_tuple_cmp(&cursor, h) {
            Some(Ordering::Greater) => return true,
            // Frontier == h also completes coverage when h is an
            // exclusive end (clipped by the table's own bound) or
            // h's last component is MAXVALUE: no real tuple equals
            // h then, and every tuple in the region is < the
            // exclusive frontier.
            Some(Ordering::Equal) if h_excl || matches!(h.last(), Some(RangeBound::Max)) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// LIST default-coverage: every OR group must pin the key to a finite
/// value set (EQ / IN, narrowed by other clauses pointwise) that is
/// contained in the union of sibling list values; an IS NULL group is
/// covered only when a sibling carries NULL.
fn list_default_covered(
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
    db: &Database,
    siblings: &[&PartBound],
    where_: &Expr,
) -> bool {
    let mut union: Vec<Value> = Vec::new();
    let mut sib_has_null = false;
    for b in siblings {
        if let PartBound::List { values, has_null } = b {
            union.extend(values.iter().cloned());
            sib_has_null |= *has_null;
        } else {
            return false;
        }
    }
    for group in or_groups(where_) {
        let clauses = group_clauses(&group, pinfo, parent, parent_name, parent_alias, db);
        // Finite candidate set for the (single) list key part.
        let mut cand: Option<Vec<Value>> = None;
        let mut null_in_region = false;
        for kc in &clauses {
            if kc.part != 0 {
                return false;
            }
            match &kc.kind {
                KeyClauseKind::Cmp(CmpOp::Eq, v) => {
                    cand = Some(match cand {
                        None => vec![v.clone()],
                        Some(c) => c
                            .into_iter()
                            .filter(|x| val_eq(x, v) == Some(true))
                            .collect(),
                    });
                }
                KeyClauseKind::InSet(vals) => {
                    cand = Some(match cand {
                        None => vals.clone(),
                        Some(c) => c
                            .into_iter()
                            .filter(|x| vals.iter().any(|v| val_eq(x, v) == Some(true)))
                            .collect(),
                    });
                }
                // `key <> false` / NOT (key = false): the finite
                // complement {true}, NULL-strict (no null region).
                KeyClauseKind::Cmp(CmpOp::Ne, Value::Bool(b)) => {
                    let bv = Value::Bool(!*b);
                    cand = Some(match cand {
                        None => vec![bv.clone()],
                        Some(c) => c
                            .into_iter()
                            .filter(|x| val_eq(x, &bv) == Some(true))
                            .collect(),
                    });
                }
                KeyClauseKind::IsNull { neg: false } => {
                    null_in_region = true;
                    cand = Some(Vec::new());
                }
                // IS NOT TRUE/FALSE is satisfied by NULL key values
                // (region includes NULL) and by the complement bool
                // value (a finite candidate).
                KeyClauseKind::IsBool { neg: true, val } => {
                    null_in_region = true;
                    let bv = Value::Bool(!*val);
                    cand = Some(match cand {
                        None => vec![bv.clone()],
                        Some(c) => c
                            .into_iter()
                            .filter(|x| val_eq(x, &bv) == Some(true))
                            .collect(),
                    });
                }
                KeyClauseKind::IsBool { neg: false, val } => {
                    let bv = Value::Bool(*val);
                    cand = Some(match cand {
                        None => vec![bv.clone()],
                        Some(c) => c
                            .into_iter()
                            .filter(|x| val_eq(x, &bv) == Some(true))
                            .collect(),
                    });
                }
                _ => {}
            }
        }
        if null_in_region && !sib_has_null {
            return false;
        }
        let cand = match cand {
            Some(c) => c,
            None => return false, // unbounded region: not covered
        };
        // Narrow by the remaining clauses pointwise, then test
        // containment in the sibling union.
        let others: Vec<&KeyClause> = clauses
            .iter()
            .filter(|kc| {
                !matches!(
                    kc.kind,
                    KeyClauseKind::Cmp(CmpOp::Eq, _) | KeyClauseKind::InSet(_)
                )
            })
            .collect();
        for v in &cand {
            let survives = others.iter().all(|kc| clause_point(kc, v).0);
            if !survives {
                continue;
            }
            if !union.iter().any(|u| val_eq(u, v) == Some(true)) {
                return false;
            }
        }
    }
    true
}

/// HASH driver: when every key part is pinned to exactly one value by
/// the AND conjuncts, the row hash decides the single surviving child.
/// Returns Some(survivor child name), or None when not decidable.
/// Plan-time hash of one key value, mirroring the executor's
/// partition_key_hash: the type's default extended hash under
/// HASH_PARTITION_SEED, or the opclass's FUNCTION 2 evaluated
/// through the planner's immutable-call folder (the suite's
/// hand-rolled hash functions are immutable SQL functions over
/// constants, so they fold exactly). None = cannot prove.
fn prune_part_hash(
    db: &Database,
    pinfo: &PartitionInfo,
    types: &[ColType],
    idx: usize,
    v: &Value,
) -> Option<u64> {
    let default = || default_extended_hash(v, Some(&types[idx]), HASH_PARTITION_SEED);
    match &pinfo.key[idx].opclass {
        None => Some(default()),
        Some(ocname) => {
            let class = find_opclass(db, ocname, "hash")?;
            match &class.hash_function {
                None => Some(default()),
                Some(fname) => {
                    let base = fname.rsplit('.').next().unwrap_or(fname.as_str());
                    let lit = match v {
                        Value::SmallInt(x) => Literal::SmallInt(*x),
                        Value::Int(x) => Literal::Int(*x),
                        Value::BigInt(x) => Literal::BigInt(*x),
                        Value::Text(t) => Literal::Text(t.clone()),
                        Value::Bool(b) => Literal::Bool(*b),
                        _ => return None,
                    };
                    let args = [
                        Expr::Literal(lit),
                        Expr::Literal(Literal::BigInt(HASH_PARTITION_SEED as i64)),
                    ];
                    let mut fc = PgConstFold::with_db(db);
                    match pg_fold_immutable_call(&mut fc, base, &args)? {
                        Literal::BigInt(x) => Some(x as u64),
                        Literal::Int(x) => Some(x as u64),
                        Literal::SmallInt(x) => Some(x as u64),
                        _ => None,
                    }
                }
            }
        }
    }
}

fn hash_survivor(
    db: &Database,
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
    snap: &Snapshot,
    own: u64,
    session: u64,
    where_: &Expr,
) -> Option<Vec<String>> {
    let types = key_col_types(&parent.columns, pinfo);
    let mut routed: Vec<String> = Vec::new();
    for group in or_groups(where_) {
        let clauses = group_clauses(&group, pinfo, parent, parent_name, parent_alias, db);
        let mut key: Vec<Option<Value>> = vec![None; pinfo.key.len()];
        for kc in &clauses {
            let pin = match &kc.kind {
                KeyClauseKind::Cmp(CmpOp::Eq, v) => Some(v.clone()),
                // PG19 compute_partition_hash_value skips NULL key
                // parts entirely, so an IS NULL part still routes.
                KeyClauseKind::IsNull { neg: false } => Some(Value::Null),
                KeyClauseKind::IsBool { neg: false, val } => Some(Value::Bool(*val)),
                _ => None,
            };
            if let Some(v) = pin {
                match &key[kc.part] {
                    Some(prev) if val_eq(prev, &v) != Some(true) => return None,
                    _ => key[kc.part] = Some(v),
                }
            }
        }
        let key: Vec<Value> = key.into_iter().collect::<Option<Vec<_>>>()?;
        let mut row_hash: u64 = 0;
        for (i, v) in key.iter().enumerate() {
            if *v == Value::Null {
                continue;
            }
            let h = prune_part_hash(db, pinfo, &types, i, v)?;
            row_hash = hash_combine64(row_hash, h);
        }
        let mut hit = None;
        for child_name in &pinfo.children {
            let child = db.find_table(child_name, snap, &[own], session)?;
            let cinfo = child.partition.as_ref()?;
            if cinfo.is_default {
                continue;
            }
            if let Some(PartBound::Hash { modulus, remainder }) = &cinfo.bound {
                if row_hash % (*modulus as u64) == *remainder as u64 {
                    hit = Some(child_name.clone());
                }
            }
        }
        routed.push(hit?);
    }
    Some(routed)
}

/// Children of a partitioned parent in PG19 Append order (see
/// pruned_children). Falls back to creation order when a bound cannot
/// be read.
fn ordered_children(
    db: &Database,
    parent: &Table,
    pinfo: &PartitionInfo,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Vec<String> {
    let mut items: Vec<(String, bool, Option<PartBound>)> = Vec::new();
    for child_name in &pinfo.children {
        match db.find_table(child_name, snap, &[own], session) {
            Some(child) => {
                let cinfo = child.partition.as_ref();
                items.push((
                    child_name.clone(),
                    cinfo.map(|c| c.is_default).unwrap_or(false),
                    cinfo.and_then(|c| c.bound.clone()),
                ));
            }
            None => return pinfo.children.clone(),
        }
    }
    let _ = parent;
    items.sort_by(|a, b| {
        // DEFAULT last.
        match (a.1, b.1) {
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            _ => {}
        }
        match (&a.2, &b.2, &pinfo.method) {
            (
                Some(PartBound::List {
                    values: v1,
                    has_null: n1,
                }),
                Some(PartBound::List {
                    values: v2,
                    has_null: n2,
                }),
                PartMethod::List,
            ) => {
                // A values-only partition sorts by its smallest value;
                // a NULL-only partition sorts after all valued ones.
                let k1 = (
                    v1.is_empty() && *n1,
                    v1.iter()
                        .min_by(|x, y| compare_partition_values(x, y).unwrap_or(Ordering::Equal))
                        .cloned(),
                );
                let k2 = (
                    v2.is_empty() && *n2,
                    v2.iter()
                        .min_by(|x, y| compare_partition_values(x, y).unwrap_or(Ordering::Equal))
                        .cloned(),
                );
                match (k1, k2) {
                    ((true, _), (false, _)) => Ordering::Greater,
                    ((false, _), (true, _)) => Ordering::Less,
                    ((_, Some(x)), (_, Some(y))) => {
                        compare_partition_values(&x, &y).unwrap_or(Ordering::Equal)
                    }
                    _ => Ordering::Equal,
                }
            }
            (
                Some(PartBound::Range { lower: l1, .. }),
                Some(PartBound::Range { lower: l2, .. }),
                PartMethod::Range,
            ) => bound_tuple_cmp(l1, l2).unwrap_or(Ordering::Equal),
            (
                Some(PartBound::Hash { remainder: r1, .. }),
                Some(PartBound::Hash { remainder: r2, .. }),
                PartMethod::Hash,
            ) => r1.cmp(r2),
            _ => Ordering::Equal,
        }
    });
    items.into_iter().map(|(n, _, _)| n).collect()
}

/// v2.32: the surviving direct children of a partitioned parent under
/// `where` (None = no predicate), in PartitionInfo.children order.
/// `pruning` false (enable_partition_pruning=off) keeps every child.
/// v2.32: is a single-group WHERE refuted by itself (PG19's
/// predicate_refuted_by on the scan qual: `a = 1 AND a = 3`, or an
/// equality point outside the group's own range region, or IS NULL
/// combined with any value clause on the same part)? When the whole
/// qual is refuted, PG19 plans the dummy Result before partition
/// pruning is even consulted. Note the asymmetry with the leaf
/// tests: inside one OR arm, equality steps do NOT intersect (the
/// rlp `(a = 1 AND a = 3) OR ...` case keeps rlp2) — refutation is
/// a whole-qual proof only.
pub(crate) fn where_refuted(
    db: &Database,
    pinfo: &PartitionInfo,
    parent: &Table,
    parent_name: &str,
    parent_alias: Option<&str>,
    where_: &Expr,
) -> bool {
    let groups = or_groups(where_);
    if groups.len() != 1 {
        return false;
    }
    let clauses = group_clauses(&groups[0], pinfo, parent, parent_name, parent_alias, db);
    let n = pinfo.key.len();
    let mut pts: Vec<Option<Vec<Value>>> = vec![None; n];
    let mut segs: Vec<Vec<(End, End)>> = vec![vec![(End::NegInf, End::PosInf)]; n];
    let mut has_value_clause = vec![false; n];
    let mut has_null_clause = vec![false; n];
    let intersect_pts = |cur: &mut Option<Vec<Value>>, new: &[Value]| {
        let set: Vec<Value> = match cur.take() {
            None => new.to_vec(),
            Some(existing) => existing
                .into_iter()
                .filter(|x| new.iter().any(|v| val_eq(x, v) == Some(true)))
                .collect(),
        };
        *cur = Some(set);
    };
    for kc in &clauses {
        let k = kc.part;
        match &kc.kind {
            KeyClauseKind::Cmp(CmpOp::Eq, v) => {
                has_value_clause[k] = true;
                intersect_pts(&mut pts[k], std::slice::from_ref(v));
            }
            KeyClauseKind::InSet(vals) => {
                has_value_clause[k] = true;
                intersect_pts(&mut pts[k], vals);
            }
            KeyClauseKind::IsBool { neg: false, val } => {
                has_value_clause[k] = true;
                intersect_pts(&mut pts[k], &[Value::Bool(*val)]);
            }
            KeyClauseKind::IsNull { neg: false } => {
                has_null_clause[k] = true;
            }
            KeyClauseKind::IsNull { neg: true } => {}
            _ => {
                has_value_clause[k] = true;
                let r = clause_region(kc);
                segs[k] = region_intersect(
                    &Region {
                        segs: segs[k].clone(),
                        null: false,
                        points: None,
                    },
                    &r,
                )
                .segs;
            }
        }
    }
    for k in 0..n {
        if has_null_clause[k] && has_value_clause[k] {
            return true;
        }
        if let Some(set) = &pts[k] {
            if set.is_empty() {
                return true;
            }
            let any_in = set.iter().any(|v| {
                segs_overlap(
                    &[(End::Val(v.clone(), true), End::Val(v.clone(), true))],
                    &segs[k],
                )
            });
            if !any_in {
                return true;
            }
        } else if segs[k].is_empty() {
            return true;
        }
    }
    false
}

/// v2.32: can `where_` be TRUE for some row inside `bound` (a bound
/// expressed in `parent_name`'s key)? Used to fold a scanned
/// partition's own bound into its plan (PG19 constraint exclusion on
/// the partition itself). None when the parent cannot be read.
#[allow(clippy::too_many_arguments)]
pub(crate) fn where_may_match_bound(
    db: &Database,
    parent_name: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
    bound: &PartBound,
    where_: &Expr,
) -> Option<bool> {
    let parent = db.find_table(parent_name, snap, &[own], session)?;
    let pinfo = parent.partition.as_ref()?;
    let cx = PruneCtx {
        pinfo,
        parent,
        parent_name,
        parent_alias: None,
        db,
        method: &pinfo.method,
        bound,
    };
    Some(bound_may_must(&cx, where_).0)
}

pub(crate) fn pruned_children(
    db: &Database,
    parent_name: &str,
    parent_alias: Option<&str>,
    snap: &Snapshot,
    own: u64,
    session: u64,
    where_: Option<&Expr>,
    pruning: bool,
) -> Option<Vec<String>> {
    let parent = db.find_table(parent_name, snap, &[own], session)?;
    let pinfo = parent.partition.as_ref()?;
    if pinfo.children.is_empty() || !pinfo.is_partitioned {
        return None;
    }
    // PG19 lists Append children in bound order (partition.c's
    // bound-sorted partition descriptor), not creation order:
    // LIST by bound values (NULL-valued partition after all valued
    // ones), RANGE by lower bound, HASH by remainder; the DEFAULT
    // partition always sorts last.
    let ordered = ordered_children(db, parent, pinfo, snap, own, session);
    if !pruning {
        return Some(ordered);
    }
    let where_ = match where_ {
        Some(w) => w,
        None => return Some(ordered),
    };
    // v2.32: when this table is itself a partition, OR groups that
    // contradict the table's OWN bound can never reach its subtree
    // (PG19 plans the whole tree with the bound folded in), so they
    // must not keep the DEFAULT alive: `rlp WHERE a = 20 OR a = 40`
    // prunes rlp4_default because the a = 40 group cannot enter
    // rlp4 [20, 31) at all.
    let mut eff_where_owned: Option<Expr> = None;
    if let (Some(gp), Some(own_bound)) = (
        parent.partition.as_ref().and_then(|p| p.parent.clone()),
        parent.partition.as_ref().and_then(|p| p.bound.clone()),
    ) {
        let groups = or_groups(where_);
        let mut kept: Vec<Expr> = Vec::new();
        let mut dropped = false;
        for group in &groups {
            let gexpr = group
                .iter()
                .cloned()
                .cloned()
                .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
                .unwrap_or(Expr::Literal(Literal::Bool(true)));
            if where_may_match_bound(db, &gp, snap, own, session, &own_bound, &gexpr) == Some(false)
            {
                dropped = true;
            } else {
                kept.push(gexpr);
            }
        }
        if dropped {
            if kept.is_empty() {
                return Some(Vec::new());
            }
            eff_where_owned = Some(
                kept.into_iter()
                    .reduce(|a, b| Expr::Or(Box::new(a), Box::new(b)))
                    .unwrap(),
            );
        }
    }
    let eff_where: &Expr = eff_where_owned.as_ref().unwrap_or(where_);
    // HASH: full-key equality routes each OR group to exactly one
    // child; the Append lists the routed children in bound order.
    if pinfo.method == PartMethod::Hash {
        if let Some(routed) = hash_survivor(
            db,
            pinfo,
            parent,
            parent_name,
            parent_alias,
            snap,
            own,
            session,
            eff_where,
        ) {
            return Some(ordered.into_iter().filter(|c| routed.contains(c)).collect());
        }
        return Some(ordered);
    }
    // Sibling bounds for the default-coverage test.
    let mut child_tables: Vec<(String, bool, Option<PartBound>)> = Vec::new();
    for child_name in &ordered {
        let child = db.find_table(child_name, snap, &[own], session)?;
        let cinfo = child.partition.as_ref()?;
        child_tables.push((child_name.clone(), cinfo.is_default, cinfo.bound.clone()));
    }
    let sib_bounds: Vec<&PartBound> = child_tables
        .iter()
        .filter(|(_, is_def, _)| !is_def)
        .filter_map(|(_, _, b)| b.as_ref())
        .collect();
    // v2.32: this table's own RANGE bound, re-expressed in this
    // table's key-part order (plain columns only), used to clip the
    // default-coverage region (rlp5's `a <= 31`: inside rlp5 the
    // region is just {31}, which rlp5_1 covers).
    let own_clip: Option<(Vec<RangeBound>, Vec<RangeBound>)> = (|| {
        let gp_name = parent.partition.as_ref()?.parent.as_ref()?;
        let own_bound = parent.partition.as_ref()?.bound.as_ref()?;
        let (lo, hi) = match own_bound {
            PartBound::Range { lower, upper } => (lower, upper),
            _ => return None,
        };
        let gp = db.find_table(gp_name, snap, &[own], session)?;
        let gpinfo = gp.partition.as_ref()?;
        let mut clo = Vec::with_capacity(pinfo.key.len());
        let mut chi = Vec::with_capacity(pinfo.key.len());
        for pk in &pinfo.key {
            if pk.expr.is_some() {
                return None;
            }
            let gi = gpinfo
                .key
                .iter()
                .position(|g| g.expr.is_none() && g.col == pk.col)?;
            clo.push(lo[gi].clone());
            chi.push(hi[gi].clone());
        }
        Some((clo, chi))
    })();
    let default_covered = if child_tables.iter().any(|(_, d, _)| *d) {
        match pinfo.method {
            PartMethod::List => list_default_covered(
                pinfo,
                parent,
                parent_name,
                parent_alias,
                db,
                &sib_bounds,
                eff_where,
            ),
            PartMethod::Range => range_default_covered(
                pinfo,
                parent,
                parent_name,
                parent_alias,
                db,
                &sib_bounds,
                eff_where,
                own_clip.as_ref(),
            ),
            PartMethod::Hash => false,
        }
    } else {
        false
    };
    let mut out = Vec::new();
    for (child_name, is_default, bound) in &child_tables {
        if *is_default {
            if !default_covered {
                out.push(child_name.clone());
            }
            continue;
        }
        let bound = match bound {
            Some(b) => b,
            None => {
                out.push(child_name.clone());
                continue;
            }
        };
        let cx = PruneCtx {
            pinfo,
            parent,
            parent_name,
            parent_alias,
            db,
            method: &pinfo.method,
            bound,
        };
        if bound_may_must(&cx, eff_where).0 {
            out.push(child_name.clone());
        }
    }
    Some(out)
}
