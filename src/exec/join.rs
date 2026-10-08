// v1.78 mechanical split: moved verbatim from src/exec.rs (29911-32838).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// v0.93: hash equi-join.
//
// A basic in-memory hash join for INNER joins whose ON predicate is a
// conjunction with at least one `left_col = right_col` equality between
// plain (resolved) column references. The build side is the right input;
// probing walks the left rows in order and emits matches with the right
// rows in their original order, so the output row order is identical to
// the nested loop's (left-major, right-minor).
//
// Correctness argument:
// * Only top-level AND conjuncts of the form `lcol = rcol` (one side in
//   the left frame, the other in the right frame) become hash keys. The
//   full ON predicate must be TRUE for a pair to be emitted, and a TRUE
//   equi conjunct requires equal keys — so any pair the hash lookup
//   skips could never have satisfied ON. Every candidate pair is
//   re-checked against the full resolved ON predicate with the same
//   two-frame scope the nested loop uses, so three-valued logic,
//   coercions, and residual (non-equi) conjuncts behave exactly as
//   before.
// * NULL key components never match: a `NULL = x` conjunct is never
//   TRUE, so build/probe both skip NULL keys (like PG's hash join,
//   which never inserts NULL keys).
// * The hash key canonicalization below is *consistent* with the
//   engine's `=` (eval_cmp_vals/cmp_ordering): whenever `a = b` is TRUE,
//   both sides produce equal key parts. Families are deliberately
//   narrow — mixed-type conjuncts (int = numeric is fine, int = text is
//   not) and exotic types decline, falling back to the nested loop.
//   The per-candidate full-predicate re-check additionally guards
//   against any false positive the hash might admit.
// * `RUSTGRES_NO_HASH_JOIN=1` disables the hash path (differential
//   testing against the nested loop).
// ---------------------------------------------------------------------------

/// v0.93: the hash family of one equi-join key column, from its static
/// `ColType`. Both sides of a conjunct must map to the same family;
/// anything else (mixed families, arrays, records, regclass, name,
/// json, composite) declines the hash join for that conjunct.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum HashFam {
    /// int2/int4/int8/numeric: normalized exact decimal
    /// (special, sign, magnitude, scale) — exactly `=` semantics, and
    /// mixed int/numeric conjuncts hash consistently.
    ExactNum,
    /// float4/float8: f64 bits (float4 widened, as in cmp_ordering).
    Float,
    /// text/varchar: raw bytes.
    Text,
    /// character(n): PG's bpcharcmp ignores trailing spaces.
    BpChar,
    Bool,
    Date,
    /// timestamp + timestamptz: the engine compares raw micros
    /// (UTC-only), so both hash the i64.
    Ts,
    Bytea,
    // v1.39
    Bit,
    Uuid,
    /// "char": the single byte.
    Char,
    PgLsn,
    Tid, // v1.40
}

pub(crate) fn hash_family(ty: &ColType) -> Option<HashFam> {
    Some(match ty {
        ColType::SmallInt | ColType::Int | ColType::BigInt | ColType::Numeric(_) => {
            HashFam::ExactNum
        }
        ColType::Float4 | ColType::Float => HashFam::Float,
        ColType::Text | ColType::Varchar(_) => HashFam::Text,
        ColType::Char(_) => HashFam::BpChar,
        ColType::Bool => HashFam::Bool,
        ColType::Date => HashFam::Date,
        ColType::Timestamp | ColType::Timestamptz => HashFam::Ts,
        ColType::Bytea => HashFam::Bytea,
        ColType::Bit => HashFam::Bit, // v1.39
        ColType::Uuid => HashFam::Uuid,
        ColType::SingleChar => HashFam::Char,
        ColType::PgLsn => HashFam::PgLsn,
        ColType::Tid => HashFam::Tid, // v1.40
        _ => return None,
    })
}

/// v0.93: one canonical hash-join key component. `Eq` on this enum
/// coincides with the engine's `=` returning TRUE within a family
/// (see the module docs above); the per-candidate re-check makes any
/// residual doubt moot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HashKeyPart {
    /// (special as u8, negative, magnitude, scale), canonicalized by
    /// `Numeric::hash_key`: trailing zeros stripped, so `5`, `5.0`,
    /// `5.00`, int4 `5`, and int8 `5` share one key — exactly `=`
    /// semantics, and mixed int/numeric conjuncts hash consistently
    /// (PG19's hashint8 is likewise "compatible with the values
    /// produced by hashint4 and hashint2 for logically equal
    /// inputs", src/backend/access/hash/hashfunc.c).
    ExactNum(u8, bool, crate::storage::NumKeyMag, i32),
    /// Canonicalized f64 bits: `-0.0` -> `0.0`, NaN -> standard NaN
    /// (see `canon_float_key`).
    Float(u64),
    Text(std::sync::Arc<str>),
    BpChar(std::sync::Arc<str>),
    Bool(bool),
    Date(i32),
    Ts(i64),
    Bytea(Vec<u8>),
    // v1.39: bit length matters (trailing bits are padding).
    BitString {
        bitlen: u32,
        bytes: Vec<u8>,
    },
    Uuid([u8; 16]),
    Char(u8),
    PgLsn(u64),
    Tid(u32, u32), // v1.40
}

/// v0.94: PG19 float hash canonicalization
/// (src/backend/access/hash/hashfunc.c, `hashfloat4`/`hashfloat8`):
/// "On IEEE-float machines, minus zero and zero have different bit
/// patterns but should compare as equal. We must ensure that they
/// have the same hash value" — so `-0.0` hashes as `0.0`; "NaNs can
/// have different bit patterns but they should all compare as equal.
/// For backwards-compatibility reasons we force them to have the
/// hash value of a standard NaN."
#[inline]
pub(crate) fn canon_float_key(f: f64) -> u64 {
    if f.is_nan() {
        f64::NAN.to_bits()
    } else if f == 0.0 {
        0
    } else {
        f.to_bits()
    }
}

pub(crate) fn hash_key_part(v: &Value, fam: HashFam) -> Option<HashKeyPart> {
    match fam {
        HashFam::ExactNum => {
            let n = match v {
                Value::SmallInt(i) => crate::storage::Numeric::new(*i as i128, 0),
                Value::Int(i) | Value::BigInt(i) => crate::storage::Numeric::new(*i as i128, 0),
                Value::Numeric(n) => n.clone(),
                _ => return None,
            };
            // v0.94: canonicalize defensively (trailing-zero strip),
            // so `5`, `5.0`, `5.00` share one key — see
            // `Numeric::hash_key`.
            let (special, neg, mag, scale) = n.hash_key();
            Some(HashKeyPart::ExactNum(special, neg, mag, scale))
        }
        HashFam::Float => match v {
            // v0.94: PG19 hashfloat4/hashfloat8 canonicalization
            // (src/backend/access/hash/hashfunc.c): float4 is widened
            // to float8 first, then `-0.0` hashes as `0` and every NaN
            // hashes as one standard NaN — matching `=` (float8_eq).
            Value::Float4(f) => Some(HashKeyPart::Float(canon_float_key(*f as f64))),
            Value::Float(f) => Some(HashKeyPart::Float(canon_float_key(*f))),
            _ => None,
        },
        HashFam::Text => match v {
            Value::Text(s) => Some(HashKeyPart::Text(s.clone())),
            _ => None,
        },
        HashFam::BpChar => match v {
            Value::BpChar(s) => Some(HashKeyPart::BpChar(crate::storage::rtrim_spaces(s).into())),
            _ => None,
        },
        HashFam::Bool => match v {
            Value::Bool(b) => Some(HashKeyPart::Bool(*b)),
            _ => None,
        },
        HashFam::Date => match v {
            Value::Date(d) => Some(HashKeyPart::Date(*d)),
            _ => None,
        },
        HashFam::Ts => match v {
            Value::Timestamp(t) | Value::Timestamptz(t) => Some(HashKeyPart::Ts(*t)),
            _ => None,
        },
        HashFam::Bytea => match v {
            Value::Bytea(b) => Some(HashKeyPart::Bytea(b.clone())),
            _ => None,
        },
        // v1.39
        HashFam::Bit => match v {
            Value::BitString(b) => Some(HashKeyPart::BitString {
                bitlen: b.bitlen,
                bytes: b.bytes.clone(),
            }),
            _ => None,
        },
        HashFam::Uuid => match v {
            Value::Uuid(u) => Some(HashKeyPart::Uuid(*u)),
            _ => None,
        },
        HashFam::Char => match v {
            Value::SingleChar(c) => Some(HashKeyPart::Char(*c)),
            _ => None,
        },
        HashFam::PgLsn => match v {
            Value::PgLsn(l) => Some(HashKeyPart::PgLsn(*l)),
            _ => None,
        },
        HashFam::Tid => match v {
            Value::Tid(b, o) => Some(HashKeyPart::Tid(*b, *o)),
            _ => None,
        },
    }
}

/// v0.93: extract the equi-join keys from a resolved ON predicate.
/// Returns the `(left_idx, right_idx, family)` key conjuncts, or `None`
/// when the hash join does not apply (no usable equi conjunct). `frame`
/// is the number of outer scopes; the left frame is `frame` and the
/// right frame is `frame + 1`.
pub(crate) fn hash_join_keys(
    pred: &Expr,
    n_outer: usize,
    lschema: &[QCol],
    rschema: &[QCol],
) -> Option<Vec<(usize, usize, HashFam)>> {
    if std::env::var("RUSTGRES_NO_HASH_JOIN").is_ok() {
        return None;
    }
    let (lf, rf) = (n_outer, n_outer + 1);
    let mut keys = Vec::new();
    for c in split_conjuncts(pred) {
        let Expr::Cmp { op, left, right } = c else {
            continue;
        };
        if !matches!(op, CmpOp::Eq) {
            continue;
        }
        let (Expr::ResolvedCol { frame: fl, idx: li }, Expr::ResolvedCol { frame: fr, idx: ri }) =
            (left.as_ref(), right.as_ref())
        else {
            continue;
        };
        // One side must be the left frame and the other the right frame
        // (either order); same-side or outer-frame equalities are not
        // join keys — they stay in the full predicate, re-checked per
        // candidate pair.
        let (li, ri) = match (*fl, *fr) {
            (a, b) if a == lf && b == rf => (*li, *ri),
            (a, b) if a == rf && b == lf => (*ri, *li),
            _ => continue,
        };
        let (Some(lt), Some(rt)) = (lschema.get(li), rschema.get(ri)) else {
            continue;
        };
        // An unsupported conjunct only disqualifies *itself*, not the
        // whole plan: another conjunct may still be a usable key, and the
        // full predicate (including this conjunct) is re-checked per
        // candidate pair.
        let (Some(lfam), Some(rfam)) = (hash_family(&lt.ty), hash_family(&rt.ty)) else {
            continue;
        };
        if lfam != rfam {
            continue;
        }
        keys.push((li, ri, lfam));
    }
    if keys.is_empty() {
        return None;
    }
    Some(keys)
}

/// v0.93: run the hash equi-join. Builds the hash table over the right
/// rows, then probes in left-row order; every candidate pair is
/// re-checked against the full ON predicate `pred` (the same two-frame
/// evaluation the nested loop performs), so the emitted rows are
/// exactly the nested loop's, in the same order.
///
/// Returns `Ok(None)` when a row carries a runtime value outside its
/// key family's variant set (should not happen for well-typed rows);
/// the caller then falls back to the nested loop, so correctness never
/// depends on the type system's promises.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_hash_join(
    q: &mut Q,
    outer: &[Scope],
    lschema: &[QCol],
    lrows: &[QRow],
    rschema: &[QCol],
    rrows: &[QRow],
    layout: &JoinLayout,
    kind: JoinKind,
    pred: &Expr,
    keys: &[(usize, usize, HashFam)],
) -> Result<Option<Vec<QRow>>, ExecError> {
    type JoinMap =
        std::collections::HashMap<Vec<HashKeyPart>, Vec<usize>, crate::fxhash::FxBuildHasher>;
    let mut table: JoinMap =
        std::collections::HashMap::with_capacity_and_hasher(rrows.len(), Default::default());
    // Build over the right side. Rows with a NULL key component can
    // never satisfy the equi conjunct (`NULL = x` is never TRUE) and
    // are skipped, exactly like PG's hash join.
    for (ri, r) in rrows.iter().enumerate() {
        let mut key = Vec::with_capacity(keys.len());
        let mut null_key = false;
        for (_, rj, fam) in keys {
            match &r.cells[*rj] {
                Value::Null => {
                    null_key = true;
                    break;
                }
                v => match hash_key_part(v, *fam) {
                    Some(p) => key.push(p),
                    None => return Ok(None),
                },
            }
        }
        if null_key {
            continue;
        }
        table.entry(key).or_default().push(ri);
    }
    let mut rows = Vec::new();
    // Probe in left-row order; matches emit right rows in original
    // order — identical output order to the nested loop.
    if outer.is_empty() {
        // Hot path: the two frames live in a stack array; Scope is Copy.
        for l in lrows {
            let mut key = Vec::with_capacity(keys.len());
            let mut ok = true;
            let mut null_key = false;
            for (li, _, fam) in keys {
                match &l.cells[*li] {
                    Value::Null => {
                        null_key = true;
                        break;
                    }
                    v => match hash_key_part(v, *fam) {
                        Some(p) => key.push(p),
                        None => {
                            ok = false;
                            break;
                        }
                    },
                }
            }
            if !ok {
                return Ok(None);
            }
            if null_key {
                continue;
            }
            let Some(bucket) = table.get(&key) else {
                continue;
            };
            let fl = Scope {
                schema: lschema,
                row: &l.cells,
                prov: None,
            };
            for &ri in bucket {
                let r = &rrows[ri];
                let fr = Scope {
                    schema: rschema,
                    row: &r.cells,
                    prov: None,
                };
                let scopes = [fl, fr];
                if check_bool(eval_expr(q, &scopes, pred)?, "join condition")? {
                    rows.push(combine_join_pair(l, r, layout, kind));
                }
            }
        }
    } else {
        // Correlated join (rare): one small scope Vec per candidate.
        for l in lrows {
            let mut key = Vec::with_capacity(keys.len());
            let mut ok = true;
            let mut null_key = false;
            for (li, _, fam) in keys {
                match &l.cells[*li] {
                    Value::Null => {
                        null_key = true;
                        break;
                    }
                    v => match hash_key_part(v, *fam) {
                        Some(p) => key.push(p),
                        None => {
                            ok = false;
                            break;
                        }
                    },
                }
            }
            if !ok {
                return Ok(None);
            }
            if null_key {
                continue;
            }
            let Some(bucket) = table.get(&key) else {
                continue;
            };
            for &ri in bucket {
                let r = &rrows[ri];
                let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 2);
                scopes.extend_from_slice(outer);
                scopes.push(Scope {
                    schema: lschema,
                    row: &l.cells,
                    prov: None,
                });
                scopes.push(Scope {
                    schema: rschema,
                    row: &r.cells,
                    prov: None,
                });
                if check_bool(eval_expr(q, &scopes, pred)?, "join condition")? {
                    rows.push(combine_join_pair(l, r, layout, kind));
                }
            }
        }
    }
    Ok(Some(rows))
}

/// v0.23: a positional column alias list may be shorter than the column
/// list (the rest keep their names) but never longer — PostgreSQL raises
/// 42601 (`table "x" has 3 columns available but 4 specified`).
pub(crate) fn check_col_alias_arity(
    table_alias: &str,
    available: usize,
    col_aliases: &[String],
) -> Result<(), ExecError> {
    if col_aliases.len() > available {
        return Err(exec_err(
            "42601",
            format!(
                "table \"{table_alias}\" has {available} columns available but {} specified",
                col_aliases.len()
            ),
        ));
    }
    Ok(())
}

/// v0.23: apply a positional column alias list to a schema
/// (`FROM tbl AS t(a, b)`), with PostgreSQL's arity rule.
pub(crate) fn apply_col_aliases(
    mut schema: Vec<QCol>,
    table_alias: &str,
    col_aliases: &[String],
) -> Result<Vec<QCol>, ExecError> {
    check_col_alias_arity(table_alias, schema.len(), col_aliases)?;
    for (i, a) in col_aliases.iter().enumerate() {
        if i < schema.len() {
            schema[i].name = a.clone();
        }
    }
    Ok(schema)
}

/// v0.23: resolved layout of a `JOIN ... USING` / `NATURAL JOIN` output.
///
/// PostgreSQL merges each USING/NATURAL key into a single visible column
/// (merged keys first in USING order, then remaining left columns, then
/// remaining right columns) while keeping the original side columns
/// reachable through qualified references. The originals are preserved as
/// *hidden* columns: invisible to `*` and unqualified lookup, reachable
/// via `qual.*` and `qual.col`.
pub(crate) struct JoinLayout {
    /// `(left_idx, right_idx)` per merged column, in output order; also
    /// drives the implicit USING/NATURAL equality predicate.
    pub(crate) keys: Vec<(usize, usize)>,
    /// Per-output-cell source plan; `row_plan.len() == schema.len()` by
    /// construction so the combiner and the schema can never disagree
    /// (e.g. when a whole-join alias drops hidden cells).
    pub(crate) row_plan: Vec<CellSrc>,
    /// Final output schema: merged keys, left rest, right rest,
    /// propagated hidden, new hidden originals, USING-alias copies —
    /// after whole-join alias rewriting.
    pub(crate) schema: Vec<QCol>,
}

/// v0.23: where one output cell of a merged join comes from.
#[derive(Clone, Copy)]
pub(crate) enum CellSrc {
    /// Merged key: (left index, right index) — resolved per join kind.
    MergedKey(usize, usize),
    /// Pass-through cell from the left (index) or right row.
    Left(usize),
    Right(usize),
}

/// v0.23: resolve the merged columns of a USING/NATURAL join and plan the
/// output schema. Shared by the executor and the describe path so both
/// agree exactly.
pub(crate) fn plan_join(
    lschema: &[QCol],
    rschema: &[QCol],
    using: &[String],
    natural: bool,
    using_alias: Option<&str>,
    alias: Option<&str>,
    col_aliases: &[String],
) -> Result<JoinLayout, ExecError> {
    // The merged column names: NATURAL merges the common *visible* column
    // names in left-schema order; USING uses the clause order (deduped).
    let mut seen = HashSet::new();
    let names: Vec<String> = if natural {
        lschema
            .iter()
            .filter(|c| !c.hidden && seen.insert(c.name.clone()))
            .map(|c| c.name.clone())
            .filter(|n| rschema.iter().any(|c| !c.hidden && c.name == *n))
            .collect()
    } else {
        using
            .iter()
            .filter(|n| seen.insert((*n).clone()))
            .cloned()
            .collect()
    };
    // Each merged column must appear exactly once among its side's
    // *visible* columns (PostgreSQL 42703 / 42701).
    let mut keys = Vec::with_capacity(names.len());
    for col in &names {
        let find = |schema: &[QCol], side: &str| -> Result<usize, ExecError> {
            let mut found = None;
            for (i, c) in schema.iter().enumerate() {
                if !c.hidden && c.name == *col {
                    if found.is_some() {
                        return Err(exec_err(
                            "42701",
                            format!(
                                "common column name \"{col}\" appears more than once in {side} table"
                            ),
                        ));
                    }
                    found = Some(i);
                }
            }
            found.ok_or_else(|| {
                exec_err(
                    "42703",
                    format!(
                        "column \"{col}\" specified in USING clause does not exist in {side} table"
                    ),
                )
            })
        };
        let li = find(lschema, "left")?;
        let ri = find(rschema, "right")?;
        keys.push((li, ri));
    }
    let lkey: HashSet<usize> = keys.iter().map(|(l, _)| *l).collect();
    let rkey: HashSet<usize> = keys.iter().map(|(_, r)| *r).collect();
    let mut lpass = Vec::new();
    let mut lhid = Vec::new();
    for (i, c) in lschema.iter().enumerate() {
        if c.hidden {
            lhid.push(i);
        } else if !lkey.contains(&i) {
            lpass.push(i);
        }
    }
    let mut rpass = Vec::new();
    let mut rhid = Vec::new();
    for (i, c) in rschema.iter().enumerate() {
        if c.hidden {
            rhid.push(i);
        } else if !rkey.contains(&i) {
            rpass.push(i);
        }
    }
    // v0.23: `USING (...) AS x` exposes only the merged columns under `x`;
    // like a table alias, it must not collide with a sibling qualifier.
    if let Some(x) = using_alias {
        let clash = lschema
            .iter()
            .chain(rschema.iter())
            .filter(|c| !c.hidden)
            .any(|c| c.qual == x);
        if clash {
            return Err(exec_err(
                "42712",
                format!("table name \"{x}\" specified more than once"),
            ));
        }
    }
    // (QCol, CellSrc) pairs in output order; split into schema/row_plan
    // at the end so the two can never disagree (a whole-join alias drops
    // hidden cells from both together).
    let mut cells: Vec<(QCol, CellSrc)> =
        Vec::with_capacity(names.len() + lschema.len() + rschema.len() + keys.len());
    // Merged keys first, in USING/NATURAL order. The merged column belongs
    // to no single table (empty qualifier), like PostgreSQL's common
    // column; the originals stay reachable as hidden qualified columns.
    for (k, ((li, ri), col)) in keys.iter().zip(names.iter()).enumerate() {
        cells.push((
            QCol {
                qual: String::new(),
                name: col.clone(),
                ty: lschema[*li].ty.clone(),
                hidden: false,
                // Merged keys have no source qualifier; ordinal is the
                // key position (only USING-alias copies reuse it).
                src_ord: k as u32,
            },
            CellSrc::MergedKey(*li, *ri),
        ));
    }
    let push_visible =
        |cells: &mut Vec<(QCol, CellSrc)>, side: &[QCol], pass: &[usize], left: bool| {
            for &i in pass {
                let c = &side[i];
                cells.push((
                    QCol {
                        qual: c.qual.clone(),
                        name: c.name.clone(),
                        ty: c.ty.clone(),
                        hidden: false,
                        // Preserve the side's source order so `qual.*`
                        // expands in the side's own column order.
                        src_ord: c.src_ord,
                    },
                    if left {
                        CellSrc::Left(i)
                    } else {
                        CellSrc::Right(i)
                    },
                ));
            }
        };
    push_visible(&mut cells, lschema, &lpass, true);
    push_visible(&mut cells, rschema, &rpass, false);
    // Propagated hidden columns (nested joins): keep qualifier + value.
    for &i in &lhid {
        cells.push((lschema[i].clone(), CellSrc::Left(i)));
    }
    for &i in &rhid {
        cells.push((rschema[i].clone(), CellSrc::Right(i)));
    }
    // Preserved originals of this level's merged keys, for qualified refs
    // and `qual.*` — hidden from `*` and unqualified lookup. They keep
    // the side's source ordinal so `qual.*` emits the side's columns in
    // the side's own order (PostgreSQL), not in merged output order.
    for (li, _) in &keys {
        let c = &lschema[*li];
        cells.push((
            QCol {
                qual: c.qual.clone(),
                name: c.name.clone(),
                ty: c.ty.clone(),
                hidden: true,
                src_ord: c.src_ord,
            },
            CellSrc::Left(*li),
        ));
    }
    for (_, ri) in &keys {
        let c = &rschema[*ri];
        cells.push((
            QCol {
                qual: c.qual.clone(),
                name: c.name.clone(),
                ty: c.ty.clone(),
                hidden: true,
                src_ord: c.src_ord,
            },
            CellSrc::Right(*ri),
        ));
    }
    // USING-alias exposure copies (merged values, hidden).
    if let Some(x) = using_alias {
        for (k, ((li, ri), col)) in keys.iter().zip(names.iter()).enumerate() {
            cells.push((
                QCol {
                    qual: x.to_string(),
                    name: col.clone(),
                    ty: lschema[*li].ty.clone(),
                    hidden: true,
                    src_ord: k as u32,
                },
                CellSrc::MergedKey(*li, *ri),
            ));
        }
    }
    // v0.23: `(a JOIN b ...) AS x [(cols)]` — the alias hides the inner
    // table names: visible columns are requalified to `x` and the hidden
    // originals are dropped (PostgreSQL: the inner names are no longer
    // referenceable from outside).
    if let Some(x) = alias {
        cells.retain(|(c, _)| !c.hidden);
        // Reassign source ordinals in output order: `x.*` must expand in
        // the join's output order (PostgreSQL), not the sides' orders.
        for (i, (c, _)) in cells.iter_mut().enumerate() {
            c.qual = x.to_string();
            c.src_ord = i as u32;
        }
    }
    // v0.23: positional renames for a whole-join alias
    // (`(a JOIN b ...) AS x (cols)`). More aliases than visible output
    // columns is 42601 in PostgreSQL; fewer is fine.
    let visible = cells.iter().filter(|(c, _)| !c.hidden).count();
    check_col_alias_arity(alias.unwrap_or(""), visible, col_aliases)?;
    let mut vi = 0;
    for (c, _) in cells.iter_mut() {
        if c.hidden {
            continue;
        }
        if let Some(a) = col_aliases.get(vi) {
            c.name = a.clone();
        }
        vi += 1;
    }
    let mut schema = Vec::with_capacity(cells.len());
    let mut row_plan = Vec::with_capacity(cells.len());
    for (c, s) in cells {
        schema.push(c);
        row_plan.push(s);
    }
    Ok(JoinLayout {
        keys,
        row_plan,
        schema,
    })
}

/// v0.23: build one merged join output row from a kept `(l, r)` pair.
/// `l`/`r` are in side-schema layout (a null-extended side is all-NULL in
/// its own layout); the merged key value follows the join kind —
/// `COALESCE(left, right)` for FULL, the preserved side for RIGHT, the
/// left (equal to the right on a match) otherwise.
pub(crate) fn combine_join_pair(l: &QRow, r: &QRow, layout: &JoinLayout, kind: JoinKind) -> QRow {
    let merged = |li: usize, ri: usize| -> Value {
        let lv = &l.cells[li];
        match kind {
            // FULL: the merged key is COALESCE(left, right).
            JoinKind::Full if matches!(lv, Value::Null) => r.cells[ri].clone(),
            JoinKind::Right => r.cells[ri].clone(),
            _ => lv.clone(),
        }
    };
    // v0.23: follow the layout's row_plan so cells always match the
    // schema exactly (a whole-join alias drops hidden cells from both).
    let mut cells = Vec::with_capacity(layout.row_plan.len());
    for src in &layout.row_plan {
        cells.push(match *src {
            CellSrc::MergedKey(li, ri) => merged(li, ri),
            CellSrc::Left(i) => l.cells[i].clone(),
            CellSrc::Right(i) => r.cells[i].clone(),
        });
    }
    let mut prov = Vec::with_capacity(l.prov.len() + r.prov.len());
    prov.extend(l.prov.iter().cloned());
    prov.extend(r.prov.iter().cloned());
    QRow {
        cells: Row::new(cells),
        prov,
    }
}

/// v0.46: output column names of a table function, without evaluating
/// its arguments. Every current table function names its columns
/// independently of the argument values (`generate_series` and
/// `regexp_split_to_table` use the function name; `pg_input_error_info`
/// uses PG19's OUT parameter names). Used for the empty-input side of
/// an implicit-LATERAL join, where PG still reports the function's
/// columns with zero rows.
pub(crate) fn table_function_col_names(name: &str) -> Result<Vec<String>, ExecError> {
    match name {
        "regexp_split_to_table" | "generate_series" => Ok(vec![name.to_string()]),
        // v0.79: unnest's single output column is named for the function.
        "unnest" => Ok(vec![name.to_string()]),
        // v0.99: parse_ident as a scalar function in FROM yields one row
        // with the text[] value (PG19: a scalar function in FROM is a
        // one-row table).
        "parse_ident" => Ok(vec![name.to_string()]),
        "pg_input_error_info" => Ok(["message", "detail", "hint", "sql_error_code"]
            .iter()
            .map(|s| s.to_string())
            .collect()),
        _ => Err(exec_err(
            "42883",
            format!("function {name}() does not exist"),
        )),
    }
}

/// v0.46: output column types of a table function, resolved from the
/// evaluated argument values. `generate_series` reports PG's real
/// signatures (int4/int8/numeric, so the wire RowDescription carries
/// the right OIDs); the other table functions return text.
pub(crate) fn table_function_col_types(
    name: &str,
    vals: &[Value],
) -> Result<Vec<ColType>, ExecError> {
    match name {
        "regexp_split_to_table" => Ok(vec![ColType::Text]),
        "pg_input_error_info" => Ok(vec![ColType::Text; 4]),
        // v0.99: parse_ident returns text[] (see eval_str_func).
        "parse_ident" => Ok(vec![ColType::Array(crate::storage::ArrayElem::Text)]),
        "generate_series" => {
            // PG resolves mixed int/numeric to the numeric signature and
            // int4/int8 mixes to int8 (implicit casts); all-NULL (strict,
            // zero rows) defaults to int4, like PG's unknown literals.
            let ty = if vals.iter().any(|v| matches!(v, Value::Numeric(_))) {
                ColType::Numeric(None)
            } else if vals.iter().any(|v| matches!(v, Value::BigInt(_))) {
                ColType::BigInt
            } else {
                ColType::Int
            };
            Ok(vec![ty])
        }
        _ => Err(exec_err(
            "42883",
            format!("function {name}() does not exist"),
        )),
    }
}

/// v0.46: the output schema of a function FROM item: the PG 19 column
/// alias arity rule (more aliases than columns is 42601), then the
/// v0.32/v0.43 naming (single-column functions take the table alias
/// when present, else the function name; explicit column aliases win;
/// multi-column functions keep their OUT names unless aliased
/// positionally).
pub(crate) fn function_item_schema(
    name: &str,
    col_names: &[String],
    col_types: &[ColType],
    alias: &Option<String>,
    col_aliases: &[String],
) -> Result<Vec<QCol>, ExecError> {
    let qual = alias.clone().unwrap_or_else(|| name.to_string());
    // PG 19 arity rule for column aliases (more aliases than
    // columns is 42601), via check_col_alias_arity.
    check_col_alias_arity(&qual, col_names.len(), col_aliases)?;
    debug_assert_eq!(
        col_names.len(),
        col_types.len(),
        "table function names and types run in parallel"
    );
    let ncols = col_names.len();
    Ok(col_names
        .iter()
        .enumerate()
        .map(|(i, cn)| {
            let col_name = col_aliases
                .get(i)
                .cloned()
                .or_else(|| if ncols == 1 { alias.clone() } else { None })
                .unwrap_or_else(|| cn.clone());
            QCol {
                qual: qual.clone(),
                name: col_name,
                ty: col_types.get(i).copied().unwrap_or(ColType::Text),
                hidden: false,
                src_ord: i as u32,
            }
        })
        .collect())
}

/// v0.46: evaluate one function FROM item against `scopes`: argument
/// expressions, then the table function, then schema and rows. The
/// caller chooses the scope chain — the outer query's scopes for the
/// uncorrelated case, one accumulated input row per call for implicit
/// LATERAL.
pub(crate) fn eval_function_item(
    q: &mut Q,
    scopes: &[Scope],
    name: &str,
    args: &[Expr],
    alias: &Option<String>,
    col_aliases: &[String],
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    let mut arg_vals = Vec::with_capacity(args.len());
    for a in args {
        arg_vals.push(eval_expr(q, scopes, a)?);
    }
    // v0.86: user-defined functions in FROM (scalar composite-returning
    // functions expand to their fields; SETOF returns rows).
    // v0.87: resolve the best overload by (name, arg count, arg types).
    if let Some(fdef) = resolve_function_overload(q.eng, name, &arg_vals) {
        let (col_names, col_types, rows) = call_table_function(q, scopes, &fdef, &arg_vals)?;
        return finish_function_item(name, alias, col_aliases, &col_names, &col_types, rows);
    }
    let (col_names, row_vals) = eval_table_function(name, &arg_vals)?;
    // v0.46: PG's real output types (generate_series is int4/int8/
    // numeric, so the wire RowDescription carries the right OIDs and
    // the harness canonicalizes numerics by value, not by text).
    // v0.79: unnest returns setof the array's element type — from the
    // value when available, else statically (so `unnest(NULL::int[])`
    // still reports int4 with zero rows, like PG's declared return
    // type; a text *literal* coerces to text[], like PG's unknown
    // literal, while a typed text value has no unnest signature).
    let col_types = if name == "unnest" {
        if args.len() != 1 {
            return Err(exec_err(
                "42883",
                "function unnest() does not exist".to_string(),
            ));
        }
        let elem_ty = match &arg_vals[0] {
            Value::Array(a) => elem_scalar_type(a.elem),
            Value::Text(_) | Value::BpChar(_) => ColType::Text,
            Value::Null => {
                let schemas: Vec<&[QCol]> = scopes.iter().map(|s| s.schema).collect();
                match expr_type(
                    &mut *q.eng,
                    q.snap,
                    q.own,
                    q.session,
                    &schemas,
                    &[],
                    &[],
                    &args[0],
                ) {
                    Ok(ColType::Array(e)) => elem_scalar_type(e),
                    // Typing failed (e.g. a CTE reference, which the
                    // Describe path types properly): zero rows either
                    // way; fall back to text for the column type.
                    _ => ColType::Text,
                }
            }
            other => return Err(func_arg_err(name, other)),
        };
        vec![elem_ty]
    } else {
        table_function_col_types(name, &arg_vals)?
    };
    let schema = function_item_schema(name, &col_names, &col_types, alias, col_aliases)?;
    let rows = row_vals
        .into_iter()
        .map(|cells| QRow {
            cells: Row::new(cells),
            prov: Vec::new(),
        })
        .collect();
    Ok((schema, rows))
}

/// v0.86: finish a user-defined function FROM item: build the output
/// schema (PG 19 column-alias arity rules) and wrap the rows.
pub(crate) fn finish_function_item(
    name: &str,
    alias: &Option<String>,
    col_aliases: &[String],
    col_names: &[String],
    col_types: &[ColType],
    rows: Vec<Row>,
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    let schema = function_item_schema(name, col_names, col_types, alias, col_aliases)?;
    let rows = rows
        .into_iter()
        .map(|cells| QRow {
            cells,
            prov: Vec::new(),
        })
        .collect();
    Ok((schema, rows))
}

/// v0.46: true when a function FROM item's argument expressions reference
/// columns of the FROM items already accumulated to its left. PG treats
/// `FROM t, f(t.x)` as implicit LATERAL; such functions are re-evaluated
/// once per input row instead of once per query.
pub(crate) fn function_args_lateral(args: &[Expr], acc_schema: &[QCol]) -> bool {
    let mut refs = Vec::new();
    for a in args {
        collect_column_refs(a, &mut refs);
    }
    refs.iter().any(|(qt, nm)| match qt {
        Some(t) => acc_schema.iter().any(|c| c.qual == *t),
        None => acc_schema.iter().any(|c| c.name == *nm),
    })
}

/// v1.10: does this FROM item contain an explicit-LATERAL Derived,
/// VALUES, or Function at the current query level? Used for PG19
/// cross-nest LATERAL visibility (`FROM a, b JOIN LATERAL (SELECT a.x)
/// ...`): when the right subtree of a join contains a lateral item, the
/// subtree is rebuilt once per left row with the left row's scope
/// appended to the prefix. Does not descend into Derived subqueries
/// (separate query levels with their own prefix).
pub(crate) fn from_item_has_lateral(item: &FromItem) -> bool {
    match item {
        FromItem::Derived { lateral, .. }
        | FromItem::Values { lateral, .. }
        | FromItem::Function { lateral, .. } => *lateral,
        FromItem::Join { left, right, .. } => {
            from_item_has_lateral(left) || from_item_has_lateral(right)
        }
        _ => false,
    }
}

/// v1.10: PG19 forbids a CORRELATED lateral reference on RIGHT/FULL
/// joins (`ERROR: invalid reference to FROM-clause entry for table "a"`,
/// `DETAIL: The combining JOIN type must be INNER or LEFT for a LATERAL
/// reference`). Returns true when the lateral item references the
/// lateral scopes (prefix + immediate left). Uncorrelated LATERAL on
/// RIGHT/FULL is legal (the keyword is then a noise word).
pub(crate) fn lateral_is_correlated(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    bindings: &[Rc<CteBinding>],
    outer_schemas: &[&[QCol]],
    // v1.10: lateral-visible schemas, innermost last (prefix then left).
    scope_schemas: &[&[QCol]],
    right: &LateralRight,
) -> bool {
    // Does (qual, name) resolve to one of the lateral-visible schemas?
    // (Conservative: a qualifier matching a lateral schema, or an
    // unqualified name matching a visible column, counts as a lateral
    // reference — PG resolves such refs to the innermost scope.)
    let hits_lateral = |refs: &[(Option<String>, String)]| {
        refs.iter().any(|(qt, nm)| {
            scope_schemas.iter().any(|s| match qt {
                Some(t) => s.iter().any(|c| c.qual == *t),
                None => s.iter().any(|c| !c.hidden && c.name == *nm),
            })
        })
    };
    match right {
        LateralRight::Func { args, .. } => {
            let mut refs = Vec::new();
            for a in *args {
                collect_column_refs(a, &mut refs);
            }
            hits_lateral(&refs)
        }
        LateralRight::Values { rows, .. } => {
            let mut refs = Vec::new();
            for row in *rows {
                for e in row {
                    collect_column_refs(e, &mut refs);
                }
            }
            hits_lateral(&refs)
        }
        LateralRight::Derived { sub, .. } => {
            // The subquery is correlated iff it resolves only with the
            // lateral scopes in the chain.
            let mut with: Vec<&[QCol]> =
                Vec::with_capacity(outer_schemas.len() + scope_schemas.len());
            with.extend_from_slice(outer_schemas);
            with.extend_from_slice(scope_schemas);
            let without_ok =
                describe_select(eng, snap, own, session, sub, bindings, outer_schemas).is_ok();
            let with_ok = describe_select(eng, snap, own, session, sub, bindings, &with).is_ok();
            !without_ok && with_ok
        }
    }
}

/// v1.10: a right-hand FROM item that must be evaluated once per left
/// row (PG19's LATERAL rule: the left row's columns are visible as the
/// innermost scope during evaluation). Explicit `LATERAL (SELECT ...)`
/// / `LATERAL (VALUES ...)` / `LATERAL func(...)` all arrive here; the
/// v0.46 implicit-LATERAL case (`FROM t, f(t.x)`) is the `Func` variant
/// with `explicit` unset in spirit.
#[derive(Clone, Copy)]
pub(crate) enum LateralRight<'a> {
    Func {
        name: &'a str,
        args: &'a [Expr],
        alias: &'a Option<String>,
        col_aliases: &'a [String],
    },
    Derived {
        sub: &'a SelectStmt,
        alias: &'a str,
        col_aliases: &'a [String],
    },
    Values {
        rows: &'a [Vec<Expr>],
        alias: &'a str,
        col_aliases: &'a [String],
    },
}

/// v1.10: static output schema of a LATERAL right-hand item, inferred
/// from types only (PG19 fixes the item's rowtype at plan time). Shared
/// by the executor and the Describe path so wire OIDs agree exactly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lateral_right_schema(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    bindings: &[Rc<CteBinding>],
    outer_schemas: &[&[QCol]],
    // v1.10: schemas visible to correlated references, innermost last:
    // the textually-preceding FROM-item schemas (cross-nest LATERAL)
    // followed by the immediate left input's schema.
    scope_schemas: &[&[QCol]],
    right: &LateralRight,
) -> Result<Vec<QCol>, ExecError> {
    match right {
        LateralRight::Func {
            name,
            args,
            alias,
            col_aliases,
        } => lateral_function_schema(
            eng,
            snap,
            own,
            session,
            name,
            args,
            alias,
            col_aliases,
            // v1.10: arg type inference sees the innermost scope (the
            // immediate left input), as before.
            scope_schemas.last().copied().unwrap_or(&[]),
        ),
        LateralRight::Derived {
            sub,
            alias,
            col_aliases,
        } => {
            // Correlated refs resolve against the enclosing schemas
            // with the left input innermost (PG19); textually-preceding
            // FROM-item schemas sit between the outer query and the
            // immediate left (cross-nest LATERAL).
            let mut schemas: Vec<&[QCol]> =
                Vec::with_capacity(outer_schemas.len() + scope_schemas.len());
            schemas.extend_from_slice(outer_schemas);
            schemas.extend_from_slice(scope_schemas);
            let cols = describe_select(eng, snap, own, session, sub, bindings, &schemas)?;
            // v0.23: more column aliases than output columns is 42601.
            check_col_alias_arity(alias, cols.len(), col_aliases)?;
            Ok(cols
                .into_iter()
                .enumerate()
                .map(|(i, (n, ty))| QCol {
                    qual: (*alias).to_string(),
                    name: col_aliases.get(i).cloned().unwrap_or(n),
                    ty,
                    hidden: false,
                    src_ord: i as u32,
                })
                .collect())
        }
        LateralRight::Values {
            rows,
            alias,
            col_aliases,
        } => {
            let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
            check_col_alias_arity(alias, ncols, col_aliases)?;
            let mut schemas: Vec<&[QCol]> =
                Vec::with_capacity(outer_schemas.len() + scope_schemas.len());
            schemas.extend_from_slice(outer_schemas);
            schemas.extend_from_slice(scope_schemas);
            Ok((0..ncols)
                .map(|i| {
                    // Static type inference (the Describe path shares
                    // this helper, so both agree); a column no row can
                    // hint is TEXT, like the non-LATERAL VALUES path.
                    let ty = rows
                        .iter()
                        .filter_map(|r| r.get(i))
                        .filter_map(|e| {
                            lateral_values_coltype(eng, snap, own, session, bindings, &schemas, e)
                        })
                        .max_by_key(type_rank)
                        .unwrap_or(ColType::Text);
                    QCol {
                        qual: (*alias).to_string(),
                        name: col_aliases
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| format!("column{}", i + 1)),
                        ty,
                        hidden: false,
                        src_ord: i as u32,
                    }
                })
                .collect())
        }
    }
}

/// v1.10: static type of one LATERAL VALUES cell. `expr_type` needs
/// parsed CTE defs (unavailable here), so use the Describe-path helper
/// `hint_type` like `from_schema_item` does — except scalar subqueries,
/// which are described with the real enclosing scopes so a correlated
/// `VALUES ((SELECT s.i))` types as its true type, not TEXT.
pub(crate) fn lateral_values_coltype(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    bindings: &[Rc<CteBinding>],
    schemas: &[&[QCol]],
    e: &Expr,
) -> Option<ColType> {
    if let Expr::ScalarSub(sub) = e {
        let cols = describe_select(eng, snap, own, session, sub, bindings, schemas).ok()?;
        return if cols.len() == 1 {
            Some(cols[0].1.clone())
        } else {
            None
        };
    }
    hint_type(eng, snap, own, session, schemas, e)
}

/// v1.10: execute a join whose right side is LATERAL: the right item
/// is evaluated once per left row with the left row as the innermost
/// scope (PG19's LATERAL rule). The right schema is computed statically
/// up front, so an empty left input still yields the right columns.
/// `on`/`using` apply per pair like the normal path; RIGHT/FULL
/// preserve per-left-row unmatched right rows (each lateral
/// evaluation's rows are the inner set for their left row).
#[allow(clippy::too_many_arguments)]
pub(crate) fn eval_lateral_join(
    q: &mut Q,
    outer: &[Scope],
    // v1.10: scopes of textually-preceding FROM-item rows (cross-nest
    // LATERAL, PG19): visible to the right item between the outer query
    // and the immediate left row.
    prefix: &[Scope],
    lschema: &[QCol],
    lrows: &[QRow],
    right: &LateralRight,
    kind: JoinKind,
    on: &Option<Expr>,
    using: &[String],
    natural: bool,
    using_alias: Option<&str>,
    alias: Option<&str>,
    col_aliases: &[String],
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    // v1.26: LATERAL namespace marker (PG19 `scanNameSpaceForRefname` /
    // `check_agglevels_and_constraints`). Pushed once for the whole
    // join: every left row rebuilds the same scope shape
    // (`outer + prefix + [left row]`), and the marker pops on every
    // return path via the guard.
    let _lateral_ns = push_lateral_ns(q.depth, outer.len(), prefix.len());
    let outer_schemas: Vec<&[QCol]> = outer.iter().map(|s| s.schema).collect();
    // Static schemas, innermost last: preceding FROM items, then the
    // immediate left input.
    let scope_schemas: Vec<&[QCol]> = prefix
        .iter()
        .map(|s| s.schema)
        .chain(std::iter::once(lschema))
        .collect();
    let rschema = lateral_right_schema(
        &*q.eng,
        q.snap,
        q.own,
        q.session,
        &q.ctes,
        &outer_schemas,
        &scope_schemas,
        right,
    )?;
    // Same merged layout as a regular join, so the describe path and
    // execution agree exactly.
    let layout = plan_join(
        lschema,
        &rschema,
        using,
        natural,
        using_alias,
        alias,
        col_aliases,
    )?;
    let schema = layout.schema.clone();
    // USING keys become a merged-key equijoin predicate, exactly like
    // the normal path; otherwise the explicit ON clause.
    let on_expr: Option<Expr> = if !layout.keys.is_empty() {
        let mut cond: Option<Expr> = None;
        for (li, ri) in &layout.keys {
            let lc = &lschema[*li];
            let rc = &rschema[*ri];
            let eq = Expr::Cmp {
                op: CmpOp::Eq,
                left: Box::new(Expr::Column {
                    table: Some(lc.qual.clone()),
                    name: lc.name.clone(),
                }),
                right: Box::new(Expr::Column {
                    table: Some(rc.qual.clone()),
                    name: rc.name.clone(),
                }),
            };
            cond = Some(match cond {
                None => eq,
                Some(c) => Expr::And(Box::new(c), Box::new(eq)),
            });
        }
        cond
    } else {
        on.clone()
    };
    let preserve_left = matches!(kind, JoinKind::Left | JoinKind::Full);
    let preserve_right = matches!(kind, JoinKind::Right | JoinKind::Full);
    let null_l = QRow {
        cells: Row::new(vec![Value::Null; lschema.len()]),
        prov: Vec::new(),
    };
    let null_r = QRow {
        cells: Row::new(vec![Value::Null; rschema.len()]),
        prov: Vec::new(),
    };
    let mut rows = Vec::new();
    for l in lrows {
        // The left row is the innermost scope when the right side is
        // evaluated; textually-preceding FROM rows (cross-nest LATERAL)
        // sit between the enclosing query scopes and the left row.
        let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + prefix.len() + 1);
        scopes.extend_from_slice(outer);
        scopes.extend_from_slice(prefix);
        scopes.push(Scope {
            schema: lschema,
            row: &l.cells,
            prov: None,
        });
        let rrows = eval_lateral_right(q, &scopes, &rschema, right)?;
        let mut matched_right = vec![false; rrows.len()];
        let mut matched = false;
        for (ri, r) in rrows.iter().enumerate() {
            // The ON clause sees the merged pair, like the normal
            // path's slow path (ambiguity raises 42702 identically).
            let pair = combine_join_pair(l, r, &layout, kind);
            let keep = match &on_expr {
                None => true,
                Some(p) => {
                    let frame = Scope {
                        schema: &schema,
                        row: &pair.cells,
                        prov: None,
                    };
                    let mut pscopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                    pscopes.extend_from_slice(outer);
                    pscopes.push(frame);
                    check_bool(eval_expr(q, &pscopes, p)?, "join condition")?
                }
            };
            if keep {
                matched = true;
                matched_right[ri] = true;
                rows.push(pair);
            }
        }
        if !matched && preserve_left {
            rows.push(combine_join_pair(l, &null_r, &layout, kind));
        }
        // v0.20 semantics: right rows that never matched are emitted
        // with NULLs for the left columns — per left row, since each
        // lateral evaluation's rows are that row's inner set.
        if preserve_right {
            for (ri, r) in rrows.iter().enumerate() {
                if !matched_right[ri] {
                    rows.push(combine_join_pair(&null_l, r, &layout, kind));
                }
            }
        }
    }
    Ok((schema, rows))
}

/// v1.10: cross-nest LATERAL (`FROM a, b JOIN LATERAL (SELECT a.x) ...`,
/// PG19). The right subtree contains a lateral item that may reference
/// the left row, so the subtree is rebuilt once per left row with the
/// left row's scope appended to the prefix. The merged output schema is
/// computed statically up front (via the describe path, which shares
/// `lateral_right_schema`/`plan_join` with execution); combination uses
/// the same per-pair ON/USING semantics as `eval_lateral_join`, without
/// the hash-join/pushdown fast paths. `right` is always a `Join` here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn eval_cross_nest_join(
    q: &mut Q,
    outer: &[Scope],
    prefix: &[Scope],
    lschema: &[QCol],
    lrows: &[QRow],
    right: &FromItem,
    kind: JoinKind,
    on: &Option<Expr>,
    using: &[String],
    natural: bool,
    using_alias: Option<&str>,
    alias: Option<&str>,
    col_aliases: &[String],
    need_prov: bool,
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    // Static right schema (describe path) with the left schema as
    // preceding-sibling scope, so the merged layout never depends on
    // row values.
    let outer_schemas: Vec<&[QCol]> = outer.iter().map(|s| s.schema).collect();
    let prefix_schemas: Vec<&[QCol]> = prefix.iter().map(|s| s.schema).collect();
    let mut rdesc: Vec<Vec<QCol>> = Vec::new();
    let mut right_prefix: Vec<&[QCol]> = Vec::with_capacity(prefix_schemas.len() + 1);
    right_prefix.extend_from_slice(&prefix_schemas);
    right_prefix.push(lschema);
    from_schema_item(
        &*q.eng,
        q.snap,
        q.own,
        q.session,
        right,
        &mut rdesc,
        &[],
        &q.ctes,
        &outer_schemas,
        &right_prefix,
    )?;
    let rschema: Vec<QCol> = rdesc.into_iter().flatten().collect();
    // Same merged layout as a regular join, so the describe path and
    // execution agree exactly.
    let layout = plan_join(
        lschema,
        &rschema,
        &using,
        natural,
        using_alias,
        alias,
        col_aliases,
    )?;
    let schema = layout.schema.clone();
    // USING keys become a merged-key equijoin predicate, like the
    // normal path; otherwise the explicit ON clause.
    let on_expr: Option<Expr> = if !layout.keys.is_empty() {
        let mut cond: Option<Expr> = None;
        for (li, ri) in &layout.keys {
            let lc = &lschema[*li];
            let rc = &rschema[*ri];
            let eq = Expr::Cmp {
                op: CmpOp::Eq,
                left: Box::new(Expr::Column {
                    table: Some(lc.qual.clone()),
                    name: lc.name.clone(),
                }),
                right: Box::new(Expr::Column {
                    table: Some(rc.qual.clone()),
                    name: rc.name.clone(),
                }),
            };
            cond = Some(match cond {
                None => eq,
                Some(c) => Expr::And(Box::new(c), Box::new(eq)),
            });
        }
        cond
    } else {
        on.clone()
    };
    let is_cross = kind == JoinKind::Cross;
    let on_pred: Option<&Expr> = if is_cross { None } else { on_expr.as_ref() };
    let preserve_left = matches!(kind, JoinKind::Left | JoinKind::Full);
    let preserve_right = matches!(kind, JoinKind::Right | JoinKind::Full);
    let null_l = QRow {
        cells: Row::new(vec![Value::Null; lschema.len()]),
        prov: Vec::new(),
    };
    let null_r = QRow {
        cells: Row::new(vec![Value::Null; rschema.len()]),
        prov: Vec::new(),
    };
    let mut rows = Vec::new();
    // v1.26: cross-nest LATERAL inheritance. The right subtree is rebuilt
    // once per left row with `prefix + [left row]` appended to `outer`;
    // a lateral item nested in that subtree (at the same `q.depth`)
    // extends its namespace marker over those trailing scopes. Published
    // for the whole row loop and restored afterwards; sibling lateral
    // items each read the same value. Non-lateral references in the
    // right subtree never consult it.
    let _lateral_inherit = LateralInheritGuard {
        prev: LATERAL_INHERIT.with(|s| s.borrow_mut().replace((prefix.len() + 1, q.depth))),
    };
    for l in lrows {
        let mut prefix2: Vec<Scope> = Vec::with_capacity(prefix.len() + 1);
        prefix2.extend_from_slice(prefix);
        prefix2.push(Scope {
            schema: lschema,
            row: &l.cells,
            prov: None,
        });
        let (_, rrows) = build_source(q, outer, right, None, need_prov, None, None, &prefix2)?;
        let mut matched_right = vec![false; rrows.len()];
        let mut matched = false;
        for (ri, r) in rrows.iter().enumerate() {
            // The ON clause sees the merged pair, like the normal
            // path's slow path (ambiguity raises 42702 identically).
            let pair = combine_join_pair(l, r, &layout, kind);
            let keep = match &on_pred {
                None => true,
                Some(p) => {
                    let frame = Scope {
                        schema: &schema,
                        row: &pair.cells,
                        prov: None,
                    };
                    let mut pscopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                    pscopes.extend_from_slice(outer);
                    pscopes.push(frame);
                    check_bool(eval_expr(q, &pscopes, p)?, "join condition")?
                }
            };
            if keep {
                matched = true;
                matched_right[ri] = true;
                rows.push(pair);
            }
        }
        if !matched && preserve_left {
            rows.push(combine_join_pair(l, &null_r, &layout, kind));
        }
        // v0.20 semantics: right rows that never matched are emitted
        // with NULLs for the left columns — per left row, since each
        // rebuild's rows are that row's inner set.
        if preserve_right {
            for (ri, r) in rrows.iter().enumerate() {
                if !matched_right[ri] {
                    rows.push(combine_join_pair(&null_l, r, &layout, kind));
                }
            }
        }
    }
    Ok((schema, rows))
}

/// v1.10: evaluate a LATERAL right-hand item for one left row.
/// `scopes` is the enclosing scopes plus the left row as the innermost
/// frame (PG19's LATERAL rule). Returned rows are in `rschema` layout;
/// the schema itself was fixed up front.
pub(crate) fn eval_lateral_right(
    q: &mut Q,
    scopes: &[Scope],
    rschema: &[QCol],
    right: &LateralRight,
) -> Result<Vec<QRow>, ExecError> {
    match right {
        LateralRight::Func {
            name,
            args,
            alias,
            col_aliases,
        } => {
            let (_, frows) = eval_function_item(q, scopes, name, args, alias, col_aliases)?;
            Ok(frows)
        }
        LateralRight::Derived { sub, .. } => {
            // Same evaluation as a plain derived table, but the left
            // row's scope is visible (PG19 LATERAL). Rows are already
            // in the statically-computed layout.
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
            Ok(out
                .rows
                .into_iter()
                .map(|cells| QRow {
                    cells,
                    prov: Vec::new(),
                })
                .collect())
        }
        LateralRight::Values { rows, .. } => {
            let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
            let mut eval_rows: Vec<Vec<Value>> = Vec::new();
            for row in *rows {
                // v0.54: ragged VALUES is a syntax error (42601), like
                // the non-LATERAL path.
                if row.len() != ncols {
                    return Err(exec_err(
                        "42601",
                        format!(
                            "VALUES lists must all be the same length ({} vs {})",
                            ncols,
                            row.len()
                        ),
                    ));
                }
                // v1.10: row expressions see the left row (LATERAL).
                eval_rows.extend(expand_values_srf_row(q, scopes, row)?);
            }
            // Coerce each cell to the statically-inferred column type,
            // like the non-LATERAL VALUES arm.
            let mut out = Vec::with_capacity(eval_rows.len());
            for vals in eval_rows {
                let mut cells: Vec<Value> = Vec::with_capacity(vals.len());
                for (i, v) in vals.into_iter().enumerate() {
                    let col = &rschema[i];
                    cells.push(coerce_value(v, &col.ty, &col.name)?);
                }
                out.push(QRow {
                    cells: Row::new(cells),
                    prov: Vec::new(),
                });
            }
            Ok(out)
        }
    }
}

/// v0.46: output schema of a table function in an implicit-LATERAL
/// position, inferred statically from the argument *types* against the
/// left input's schema. Row values aren't available (and must not
/// matter): like a declared return type, the schema is fixed per query,
/// so an empty left input still yields the right schema. Only
/// `generate_series` has a type-dependent schema — the same
/// int4/int8/numeric resolution as `table_function_col_types`; every
/// other table function is all-text. Shared by the executor and the
/// Describe path so wire OIDs agree with execution.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lateral_function_schema(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    name: &str,
    args: &[Expr],
    alias: &Option<String>,
    col_aliases: &[String],
    left: &[QCol],
) -> Result<Vec<QCol>, ExecError> {
    // v0.86: user-defined functions in FROM (Describe path). A scalar
    // function returning a table rowtype expands to the table's columns.
    // v0.87: resolve overload by arity for the describe path.
    if let Some(fdef) = eng
        .db
        .functions
        .get(name)
        .and_then(|ovs| ovs.iter().find(|f| f.arg_types.len() == args.len()))
    {
        if let Some(t) = eng.db.find_table(&fdef.ret_type, snap, &[own], session) {
            let mut cols = Vec::with_capacity(t.columns.len());
            for (idx, (cn, ct)) in t.columns.iter().enumerate() {
                cols.push(QCol {
                    qual: alias.clone().unwrap_or_else(|| name.to_string()),
                    name: cn.clone(),
                    ty: ct.clone(),
                    hidden: false,
                    src_ord: idx as u32,
                });
            }
            return Ok(cols);
        }
        // v1.03: RETURNS SETOF <scalar> — a single column named for the
        // function (PG names the output column after the function).
        if fdef.returns_set {
            if let Ok(ct) = crate::sql::coltype_by_name(&fdef.ret_type) {
                return function_item_schema(name, &[name.to_string()], &[ct], alias, col_aliases);
            }
        }
    }
    let out_names = table_function_col_names(name)?;
    let col_types: Vec<ColType> = if name == "generate_series" {
        let schemas = [left];
        let mut tys = Vec::with_capacity(args.len());
        for a in args {
            tys.push(expr_type(eng, snap, own, session, &schemas, &[], &[], a)?);
        }
        let ty = if tys.iter().any(|t| matches!(t, ColType::Numeric(..))) {
            ColType::Numeric(None)
        } else if tys.iter().any(|t| matches!(t, ColType::BigInt)) {
            ColType::BigInt
        } else {
            ColType::Int
        };
        vec![ty]
    } else if name == "unnest" {
        // v0.79: setof the array's element type (a text literal coerces
        // to text[], like PG's unknown literal).
        if args.len() != 1 {
            return Err(exec_err(
                "42883",
                "function unnest() does not exist".to_string(),
            ));
        }
        let schemas = [left];
        let ty = match expr_type(eng, snap, own, session, &schemas, &[], &[], &args[0])? {
            ColType::Array(e) => elem_scalar_type(e),
            ColType::Text if matches!(args[0], Expr::Literal(_)) => ColType::Text,
            other => {
                return Err(exec_err(
                    "42883",
                    format!("function unnest({}) does not exist", other.sql_name()),
                ));
            }
        };
        vec![ty]
    } else {
        vec![ColType::Text; out_names.len()]
    };
    function_item_schema(name, &out_names, &col_types, alias, col_aliases)
}

/// v0.89: apply the statement-local UPDATE overlay to base-table scan
/// rows. Only volatile SQL function bodies carry a pending overlay;
/// every other scan gets `None` and returns `rows` untouched. Rows
/// already planned by the in-flight UPDATE are replaced by their new
/// cell values, matched by (table, row-version id) from the row
/// provenance (which the caller forces on whenever an overlay is
/// present). Nothing here touches storage — the overlay is a plan.
pub(crate) fn apply_pending_overlay(
    q: &Q<'_, '_>,
    scan_name: &str,
    schema: &[QCol],
    rows: Vec<QRow>,
) -> Vec<QRow> {
    let Some(pending) = q.pending_updates.as_ref() else {
        return rows;
    };
    let pending = pending.borrow();
    if pending.is_empty() {
        return rows;
    }
    // (table, row id) -> new cells in SCAN column order. Entries for a
    // partitioned leaf are remapped from leaf order to parent order;
    // non-partitioned entries already match the scan order.
    let mut map: std::collections::HashMap<(&str, u64), Row> = std::collections::HashMap::new();
    for (t, id, cells) in pending.iter() {
        let ordered = if t == scan_name {
            cells.clone()
        } else if let Some(lt) = q.eng.db.find_table(t, q.snap, &q.all_xids, q.session) {
            Row::new(
                schema
                    .iter()
                    .map(|c| {
                        lt.columns
                            .iter()
                            .position(|(ln, _)| ln == &c.name)
                            .map(|i| cells[i].clone())
                            .unwrap_or(Value::Null)
                    })
                    .collect(),
            )
        } else {
            continue;
        };
        map.insert((t.as_str(), *id), ordered);
    }
    if map.is_empty() {
        return rows;
    }
    rows.into_iter()
        .map(|mut qr| {
            let hit = qr
                .prov
                .iter()
                .find_map(|e| map.get(&(e.table.as_str(), e.row_id)));
            if let Some(cells) = hit {
                qr.cells = cells.clone();
            }
            qr
        })
        .collect()
}

pub(crate) fn build_source(
    q: &mut Q,
    outer: &[Scope],
    item: &FromItem,
    where_: Option<&Expr>,
    // True when this statement is FOR UPDATE: base-table rows must carry
    // their (table, row-version id) provenance so the outer FOR UPDATE
    // pass can lock them. Otherwise provenance stays empty and costs
    // nothing per row.
    need_prov: bool,
    // v0.8: when the whole query is a plain single-table SELECT whose
    // ORDER BY matches an index, the table streams out of the index in
    // ORDER BY order and run_select skips the sort. Only ever set on the
    // fast path (single FROM item).
    order_hint: Option<&OrderHint>,
    // v0.8: row budget for early termination of the index-order scan.
    early_limit: Option<usize>,
    // v1.10: scopes of textually-preceding FROM-item rows at the same
    // query level (cross-nest LATERAL, PG19). Only LATERAL items may see
    // the prefix; every other FROM item ignores it, so non-LATERAL
    // sibling visibility is unchanged.
    prefix: &[Scope],
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    match item {
        FromItem::Table {
            name,
            alias,
            col_aliases,
            only,
        } => {
            let qual = alias.clone().unwrap_or_else(|| name.clone());
            // v0.23: positional column aliases, with PostgreSQL's arity
            // rule (more aliases than columns is 42601).
            let apply_aliases = |schema: Vec<QCol>| -> Result<Vec<QCol>, ExecError> {
                apply_col_aliases(schema, &qual, col_aliases)
            };
            // v0.10: CTEs shadow everything (like Postgres).
            if let Some(b) = q.ctes.iter().rev().find(|b| b.name == *name) {
                let schema: Vec<QCol> = b
                    .schema
                    .iter()
                    .map(|c| QCol {
                        qual: qual.clone(),
                        name: c.name.clone(),
                        ty: c.ty.clone(),

                        hidden: false,
                        src_ord: c.src_ord,
                    })
                    .collect();
                return Ok((apply_aliases(schema)?, b.rows.clone()));
            }
            // v0.9: information_schema virtual tables.
            if name == "information_schema.tables" {
                let (schema, rows) = info_tables_scan(&q.eng.db, q.snap, q.own);
                let schema: Vec<QCol> = schema
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                return Ok((apply_aliases(schema)?, rows));
            }
            if name == "information_schema.columns" {
                let (schema, rows) = info_columns_scan(&q.eng.db, q.snap, q.own);
                let schema: Vec<QCol> = schema
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.98: information_schema.sequences.
            if name == "information_schema.sequences" {
                let (schema, rows) = info_sequences_scan(&q.eng.db, q.snap, q.own);
                let schema: Vec<QCol> = schema
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.9: a view name expands to its stored SELECT. Views are
            // checked before tables (a table would have blocked CREATE VIEW).
            if let Some(view) = q.eng.db.find_view(name, q.snap, q.own).cloned() {
                let stmt = parse_statement(&view.query).map_err(sql_err)?;
                let select = match stmt {
                    Stmt::Select(s) => s,
                    _ => {
                        return Err(exec_err(
                            "0A000",
                            format!("view \"{}\" query is not a SELECT", name),
                        ));
                    }
                };
                // Guard against runaway recursion (e.g. a view recreated
                // over itself via OR REPLACE races).
                if q.depth > 16 {
                    return Err(exec_err(
                        "54001",
                        "view recursion limit exceeded".to_string(),
                    ));
                }
                q.depth += 1;
                let out = run_select(q, &select, outer);
                q.depth -= 1;
                let out = out?;
                let schema: Vec<QCol> = out
                    .columns
                    .into_iter()
                    .enumerate()
                    .map(|(i, (cn, ty))| QCol {
                        qual: qual.clone(),
                        name: view.col_aliases.get(i).cloned().unwrap_or(cn),
                        ty,

                        hidden: false,
                        src_ord: i as u32,
                    })
                    .collect();
                let rows: Vec<QRow> = out
                    .rows
                    .into_iter()
                    .map(|cells| QRow {
                        cells,
                        prov: Vec::new(),
                    })
                    .collect();
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.8: the pg_stats system catalog is virtual — a real table
            // by that name takes precedence.
            if name == "pg_stats"
                && q.eng
                    .db
                    .find_table(name, q.snap, &q.all_xids, q.session)
                    .is_none()
            {
                let schema: Vec<QCol> = pg_stats_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                let (_, rows) = pg_stats_scan(&q.eng.db);
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.37: pg_class is virtual too (TOAST introspection).
            if name == "pg_class"
                && q.eng
                    .db
                    .find_table(name, q.snap, &q.all_xids, q.session)
                    .is_none()
            {
                let schema: Vec<QCol> = pg_class_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                let (_, rows) = pg_class_scan(&q.eng.db, q.snap, q.own, q.session);
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.88: pg_attribute is virtual too (bounded catalog subset).
            if name == "pg_attribute"
                && q.eng
                    .db
                    .find_table(name, q.snap, &q.all_xids, q.session)
                    .is_none()
            {
                let schema: Vec<QCol> = pg_attribute_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                let (_, rows) = pg_attribute_scan(&q.eng.db, q.snap, q.own, q.session);
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.96: pg_inherits is virtual too (inheritance catalog).
            if name == "pg_inherits"
                && q.eng
                    .db
                    .find_table(name, q.snap, &q.all_xids, q.session)
                    .is_none()
            {
                let schema: Vec<QCol> = pg_inherits_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                let (_, rows) = pg_inherits_scan(&q.eng.db, q.snap, q.own, q.session);
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.98: pg_sequences is virtual too (sequence catalog).
            if name == "pg_sequences"
                && q.eng
                    .db
                    .find_table(name, q.snap, &q.all_xids, q.session)
                    .is_none()
            {
                let schema: Vec<QCol> = pg_sequences_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                let (_, rows) = pg_sequences_scan(&q.eng.db, q.snap, q.own);
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.11: the role catalogs are virtual too.
            if matches!(
                name.as_str(),
                "pg_authid" | "pg_roles" | "pg_user" | "pg_auth_members"
            ) && q
                .eng
                .db
                .find_table(name, q.snap, &q.all_xids, q.session)
                .is_none()
            {
                let (schema, rows) = pg_auth_scan(&q.eng.db, q.snap, q.own, q.role, name);
                let schema: Vec<QCol> = schema
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.13: pg_replication_slots is virtual too (cluster-global
            // slot map, not a table).
            if name.as_str() == "pg_replication_slots"
                && q.eng
                    .db
                    .find_table(name, q.snap, &q.all_xids, q.session)
                    .is_none()
            {
                let schema: Vec<QCol> = pg_replication_slots_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                let rows = pg_replication_slots_rows(q.eng);
                return Ok((apply_aliases(schema)?, rows));
            }
            // v0.11: scanning a real table needs SELECT (and UPDATE when
            // the statement is FOR UPDATE, like PostgreSQL). Existence is
            // still reported as 42P01 when the table is missing.
            {
                let (eng, snap, own, role) = (&q.eng, q.snap, q.own, q.role);
                if let Some(t) = eng.db.find_table(name, snap, &[own], q.session) {
                    let have = crate::storage::table_privs(&eng.db, role, t, snap, own);
                    // v0.11: table-level SELECT, or any column-level
                    // SELECT grant — the run_select pre-pass enforces
                    // the precise per-column rule.
                    let select_ok = have & crate::storage::PRIV_SELECT
                        == crate::storage::PRIV_SELECT
                        || crate::storage::has_col_priv(
                            &eng.db,
                            role,
                            t,
                            crate::storage::PRIV_SELECT,
                            snap,
                            own,
                        );
                    let update_ok = !need_prov
                        || have & crate::storage::PRIV_UPDATE == crate::storage::PRIV_UPDATE;
                    if !select_ok || !update_ok {
                        return Err(exec_err(
                            "42501",
                            format!(
                                "permission denied for table \"{}\" (needs {})",
                                name,
                                if need_prov {
                                    "SELECT, UPDATE"
                                } else {
                                    "SELECT"
                                },
                            ),
                        ));
                    }
                }
            }
            // Borrow ends before any recursive call below: everything is
            // cloned out of the table.
            // v0.89: a volatile function body's scan must carry row
            // provenance so the UPDATE overlay can match rows by
            // (table, row-version id), even when not FOR UPDATE.
            let prov_for_overlay = q.pending_updates.is_some();
            let (schema, rows) = {
                let t = q
                    .eng
                    .db
                    .find_table(name, q.snap, &q.all_xids, q.session)
                    .ok_or_else(|| {
                        exec_err("42P01", format!("relation \"{}\" does not exist", name))
                    })?;
                // v0.69: if this is a partitioned parent, scan all
                // descendant leaves instead (PG's Append plan). The schema
                // stays the parent's; rows are remapped to parent order.
                let is_partitioned_parent = t
                    .partition
                    .as_ref()
                    .map(|p| !p.children.is_empty())
                    .unwrap_or(false);
                // v0.96: table inheritance — a plain `FROM t` scans t plus
                // all recursive inheritance descendants (PG's Append
                // plan); `FROM ONLY t` scans just t. Inheritance and
                // partitioning are mutually exclusive, so at most one of
                // these expansions applies.
                let inherit_children: Vec<String> = if *only || is_partitioned_parent {
                    Vec::new()
                } else {
                    q.eng
                        .db
                        .inheritance_descendants(name, q.snap, q.own, q.session)
                };
                let parent_cols: Vec<(String, ColType)> = t.columns.clone();
                let schema: Vec<QCol> = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, (n, ty))| QCol {
                        qual: qual.clone(),
                        name: n.clone(),
                        ty: *ty,

                        hidden: false,
                        // v0.23: source ordinal for `qual.*` ordering.
                        src_ord: i as u32,
                    })
                    .collect();
                // v0.8: the planner may replace the sequential scan with
                // an index scan (WHERE bounds on a leading index prefix)
                // or an index-order scan (ORDER BY). Either way the full
                // residual predicate and MVCC visibility are still
                // applied afterwards, so a plan only ever costs speed.
                // v0.69: partitioned parents never use the index path;
                // they scan all leaves.
                let db = &q.eng.db;
                let snap = q.snap;
                let own = q.own;
                // v1.21: clone the xid family for visibility (small vec).
                let all_xids = q.all_xids.clone();
                let rows: Vec<QRow> = if is_partitioned_parent {
                    // Collect all leaves recursively.
                    let leaves = collect_partition_leaves(db, name, snap, own, q.session);
                    let mut all_rows = Vec::new();
                    for leaf_name in leaves {
                        let lt = db
                            .find_table(&leaf_name, snap, &[own], q.session)
                            .expect("leaf still visible");
                        let leaf_cols = lt.columns.clone();
                        for r in lt.rows.iter().filter(|r| row_visible(r, snap, &all_xids)) {
                            // Remap to parent column order.
                            let cells: Vec<Value> = parent_cols
                                .iter()
                                .map(|(pn, _)| {
                                    leaf_cols
                                        .iter()
                                        .position(|(ln, _)| ln == pn)
                                        .map(|i| r.values[i].clone())
                                        .unwrap_or(Value::Null)
                                })
                                .collect();
                            all_rows.push(QRow {
                                cells: Row::new(cells),
                                prov: if need_prov || prov_for_overlay {
                                    vec![RowProv {
                                        qual: qual.clone(),
                                        table: leaf_name.clone(),
                                        row_id: r.id,
                                    }]
                                } else {
                                    Vec::new()
                                },
                            });
                        }
                    }
                    all_rows
                } else if !inherit_children.is_empty() {
                    // v0.96: inheritance expansion — the parent's own rows
                    // first, then each descendant in DFS order, remapped
                    // to the parent's column order. Provenance keeps the
                    // actual child table (like partitions keep the leaf).
                    // Index fast paths are disabled: the scan is an
                    // append across tables.
                    let mut all_rows = Vec::new();
                    let mut tables: Vec<&str> = vec![name.as_str()];
                    tables.extend(inherit_children.iter().map(|s| s.as_str()));
                    for tname in tables {
                        let ct = db
                            .find_table(tname, snap, &[own], q.session)
                            .expect("inheritance child still visible");
                        let child_cols = ct.columns.clone();
                        for r in ct.rows.iter().filter(|r| row_visible(r, snap, &all_xids)) {
                            let cells: Vec<Value> = if tname == name.as_str() {
                                r.values.iter().cloned().collect()
                            } else {
                                parent_cols
                                    .iter()
                                    .map(|(pn, _)| {
                                        child_cols
                                            .iter()
                                            .position(|(ln, _)| ln == pn)
                                            .map(|i| r.values[i].clone())
                                            .unwrap_or(Value::Null)
                                    })
                                    .collect()
                            };
                            all_rows.push(QRow {
                                cells: Row::new(cells),
                                prov: if need_prov || prov_for_overlay {
                                    vec![RowProv {
                                        qual: qual.clone(),
                                        table: tname.to_string(),
                                        row_id: r.id,
                                    }]
                                } else {
                                    Vec::new()
                                },
                            });
                        }
                    }
                    all_rows
                } else if let Some(hint) = order_hint {
                    // v1.18: temp indexes live in the session-local map
                    // (same pattern as the IndexScan arm below); the global
                    // lookup alone panicked on temp tables, killing the
                    // connection for `SELECT ... ORDER BY` on a temp index.
                    let ix = db
                        .temp_indexes
                        .get(&q.session)
                        .and_then(|m| m.get(&hint.index))
                        .or_else(|| db.indexes.get(&hint.index))
                        .expect("planned index still present; engine lock held throughout");
                    index_order_rows(
                        ix,
                        t,
                        name,
                        &qual,
                        hint.desc,
                        snap,
                        own,
                        need_prov || prov_for_overlay,
                        early_limit,
                    )
                } else {
                    match plan_access_path(db, t, name, &qual, where_, snap, own, q.session) {
                        AccessPath::SeqScan => t
                            .rows
                            .iter()
                            .filter(|r| row_visible(r, snap, &all_xids))
                            .map(|r| QRow {
                                cells: r.values.clone(),
                                prov: if need_prov || prov_for_overlay {
                                    vec![RowProv {
                                        qual: qual.clone(),
                                        table: name.clone(),
                                        row_id: r.id,
                                    }]
                                } else {
                                    Vec::new()
                                },
                            })
                            .collect(),
                        AccessPath::IndexScan {
                            index,
                            prefix,
                            lo,
                            hi,
                            ..
                        } => {
                            // v0.87: temp indexes live in the session-local map.
                            let ix = db
                                .temp_indexes
                                .get(&q.session)
                                .and_then(|m| m.get(&index))
                                .or_else(|| db.indexes.get(&index))
                                .expect("planned index still present; engine lock held throughout");
                            let ids = index_scan_ids(
                                ix,
                                &prefix,
                                lo.as_ref().map(|(v, b)| (v, *b)),
                                hi.as_ref().map(|(v, b)| (v, *b)),
                            );
                            let mut rows = Vec::with_capacity(ids.len());
                            for id in ids {
                                if let Some(pos) = t.row_pos(id) {
                                    let r = &t.rows[pos];
                                    if row_visible(r, snap, &all_xids) {
                                        rows.push(QRow {
                                            cells: r.values.clone(),
                                            prov: if need_prov || prov_for_overlay {
                                                vec![RowProv {
                                                    qual: qual.clone(),
                                                    table: name.clone(),
                                                    row_id: r.id,
                                                }]
                                            } else {
                                                Vec::new()
                                            },
                                        });
                                    }
                                }
                            }
                            rows
                        }
                    }
                };
                (schema, rows)
            };
            // v0.89: volatile function bodies see the in-flight UPDATE's
            // already-planned rows instead of the statement snapshot.
            let rows = apply_pending_overlay(q, name, &schema, rows);
            Ok((apply_aliases(schema)?, rows))
        }
        // v0.32: set-returning table function (`regexp_split_to_table`).
        // v0.46: args may reference preceding FROM items (implicit
        // LATERAL); the caller selects the scope chain, so this arm just
        // evaluates against whatever scopes it is given, once per call.
        FromItem::Function {
            name,
            args,
            alias,
            col_aliases,
            ..
        } => eval_function_item(q, outer, name, args, alias, col_aliases),
        FromItem::Derived {
            sub,
            alias,
            col_aliases,
            ..
        } => {
            // v0.95: non-LATERAL derived tables see enclosing query
            // scopes (PG19), but not same-level FROM siblings (outer
            // excludes them; LATERAL sibling correlation remains
            // unsupported).
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
                run_select(&mut sub_q, sub, outer)?
            };
            // v0.23: more column aliases than output columns is 42601.
            check_col_alias_arity(alias, out.columns.len(), col_aliases)?;
            let schema: Vec<QCol> = out
                .columns
                .into_iter()
                .enumerate()
                .map(|(i, (n, ty))| QCol {
                    qual: alias.clone(),
                    // v0.23: `(SELECT ...) AS s(x, y)` positional renames.
                    name: col_aliases.get(i).cloned().unwrap_or(n),
                    ty,

                    hidden: false,
                    src_ord: 0,
                })
                .collect();
            let rows: Vec<QRow> = out
                .rows
                .into_iter()
                .map(|cells| QRow {
                    cells,
                    prov: Vec::new(),
                })
                .collect();
            Ok((schema, rows))
        }
        // v0.14: `(VALUES (e, ...) [, ...]) [AS] alias`. VALUES is
        // uncorrelated, so every row evaluates with no scopes. Columns
        // are named `column1`, `column2`, ... like PostgreSQL.
        FromItem::Values {
            rows,
            alias,
            col_aliases,
            ..
        } => {
            let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
            // v0.23: more column aliases than VALUES columns is 42601.
            check_col_alias_arity(alias, ncols, col_aliases)?;
            let mut eval_rows: Vec<Vec<Value>> = Vec::new();
            for row in rows {
                if row.len() != ncols {
                    return Err(exec_err(
                        "42601",
                        format!(
                            "VALUES lists must all be the same length ({} vs {})",
                            ncols,
                            row.len()
                        ),
                    ));
                }
                // v0.72: expand top-level set-returning calls (PG19 ROWS
                // FROM: `VALUES (generate_series(1,3))` is three rows).
                eval_rows.extend(expand_values_srf_row(q, &[], row)?);
            }
            let schema: Vec<QCol> = (0..ncols)
                .map(|i| {
                    // v0.21: pick the highest type among all non-NULL values
                    // (e.g. float8 wins over numeric when a float8 appears
                    // in the VALUES list, like PG's type coercion).
                    let ty = eval_rows
                        .iter()
                        .map(|r| &r[i])
                        .filter(|v| !matches!(v, Value::Null))
                        .map(value_coltype)
                        .max_by_key(|t| type_rank(t))
                        .unwrap_or(ColType::Text);
                    QCol {
                        qual: alias.clone(),
                        // v0.21: explicit column aliases override column1, ...
                        name: col_aliases
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| format!("column{}", i + 1)),
                        ty,
                        hidden: false,
                        src_ord: i as u32,
                    }
                })
                .collect();
            let rows = eval_rows
                .into_iter()
                .map(|cells| {
                    // v0.59: coerce every cell to the resolved column
                    // type, like PG's VALUES type resolution: unknown
                    // (text) literals take the resolved type ('1' ->
                    // numeric when another row is numeric), and genuinely
                    // incompatible values raise instead of flowing
                    // through untyped.
                    let cells = cells
                        .into_iter()
                        .enumerate()
                        .map(|(i, c)| coerce_value(c, &schema[i].ty, &schema[i].name))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(QRow {
                        cells: Row::new(cells),
                        prov: Vec::new(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((schema, rows))
        }
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
            let (lschema0, lrows0) =
                build_source(q, outer, left, where_, need_prov, None, None, prefix)?;
            // v1.10: LATERAL evaluation. PG19 (`LATERAL_P
            // select_with_parens` / `LATERAL_P func_table`) evaluates a
            // LATERAL right-hand item once per left row, with the left
            // row's columns visible as the innermost scope; the item's
            // output rowtype is fixed at plan time. Covers:
            //  - explicit `LATERAL (SELECT ...)` / `LATERAL (VALUES ...)`
            //    on any join kind (comma, CROSS/INNER/LEFT/RIGHT/FULL
            //    JOIN ... ON/USING);
            //  - explicit `LATERAL func(...)` (a noise word in PG, legal
            //    on any join kind);
            //  - v0.46 implicit LATERAL: comma-separated FROM items
            //    parse into CROSS JOINs, so `FROM t, f(t.x)` arrives here
            //    with the function on the right; when its arguments
            //    reference the left side's columns PG re-evaluates once
            //    per left row. Only plain comma joins qualify for the
            //    implicit case: an explicit ON/USING/NATURAL (or a
            //    join-level alias) is a true join, not a comma.
            //  - cross-nest LATERAL (PG19): a LATERAL item may reference
            //    any FROM item that precedes it textually, even outside
            //    its own join nest (`FROM a, b JOIN LATERAL (SELECT a.x)
            //    ...`). When the right subtree contains a lateral item,
            //    it is rebuilt once per left row with the left row
            //    appended to the prefix.
            let is_comma = matches!((kind, on, using_alias), (JoinKind::Cross, None, None))
                && using.is_empty()
                && !natural
                && alias.is_none();
            let lateral_right: Option<LateralRight> = match right.as_ref() {
                FromItem::Function {
                    name,
                    args,
                    alias: falias,
                    col_aliases,
                    lateral,
                } if *lateral || (is_comma && function_args_lateral(args, &lschema0)) => {
                    Some(LateralRight::Func {
                        name: name.as_str(),
                        args: args.as_slice(),
                        alias: falias,
                        col_aliases: col_aliases.as_slice(),
                    })
                }
                FromItem::Derived {
                    sub,
                    alias,
                    col_aliases,
                    lateral: true,
                } => Some(LateralRight::Derived {
                    sub,
                    alias: alias.as_str(),
                    col_aliases: col_aliases.as_slice(),
                }),
                FromItem::Values {
                    rows,
                    alias,
                    col_aliases,
                    lateral: true,
                } => Some(LateralRight::Values {
                    rows: rows.as_slice(),
                    alias: alias.as_str(),
                    col_aliases: col_aliases.as_slice(),
                }),
                _ => None,
            };
            if let Some(lr) = lateral_right {
                // v1.10: PG19 forbids a CORRELATED lateral reference on
                // RIGHT/FULL joins ("The combining JOIN type must be
                // INNER or LEFT for a LATERAL reference").
                if matches!(kind, JoinKind::Right | JoinKind::Full) {
                    let outer_schemas: Vec<&[QCol]> = outer.iter().map(|s| s.schema).collect();
                    let scope_schemas: Vec<&[QCol]> = prefix
                        .iter()
                        .map(|s| s.schema)
                        .chain(std::iter::once(lschema0.as_slice()))
                        .collect();
                    if lateral_is_correlated(
                        &*q.eng,
                        q.snap,
                        q.own,
                        q.session,
                        &q.ctes,
                        &outer_schemas,
                        &scope_schemas,
                        &lr,
                    ) {
                        return Err(exec_err(
                            "42P10",
                            "invalid reference to FROM-clause entry: \
                             the combining JOIN type must be INNER or LEFT \
                             for a LATERAL reference",
                        ));
                    }
                }
                return eval_lateral_join(
                    q,
                    outer,
                    prefix,
                    &lschema0,
                    &lrows0,
                    &lr,
                    *kind,
                    on,
                    using,
                    *natural,
                    using_alias.as_deref(),
                    alias.as_deref(),
                    col_aliases,
                );
            }
            // v1.10: cross-nest LATERAL — the right subtree contains a
            // lateral item (but is not itself a direct lateral right),
            // so it may reference the left row: rebuild per left row.
            if from_item_has_lateral(right) {
                return eval_cross_nest_join(
                    q,
                    outer,
                    prefix,
                    &lschema0,
                    &lrows0,
                    right,
                    *kind,
                    on,
                    using,
                    *natural,
                    using_alias.as_deref(),
                    alias.as_deref(),
                    col_aliases,
                    need_prov,
                );
            }
            let (rschema0, rrows0) =
                build_source(q, outer, right, where_, need_prov, None, None, prefix)?;
            // v0.23: resolve merged columns and the output layout (shared
            // with the describe path so both agree exactly).
            let layout = plan_join(
                &lschema0,
                &rschema0,
                using,
                *natural,
                using_alias.as_deref(),
                alias.as_deref(),
                col_aliases,
            )?;
            // v0.20.1: native RIGHT/FULL support — no side swapping. Each
            // side keeps its original schema/rows/order; preservation is
            // driven directly by the join kind below. (The v0.20 swap
            // trick inverted preserve_left/preserve_right and broke
            // predicate pushdown for RIGHT.)
            let (lschema, lrows) = (lschema0.clone(), lrows0);
            let (rschema, rrows) = (rschema0.clone(), rrows0);
            // v0.20: USING (cols) — build `left.col = right.col AND ...`
            // from the resolved key positions (visible columns only, so a
            // nested join's hidden originals can never be picked).
            let on_expr: Option<Expr> = if !layout.keys.is_empty() {
                let mut cond: Option<Expr> = None;
                for (li, ri) in &layout.keys {
                    let lc = &lschema[*li];
                    let rc = &rschema[*ri];
                    let eq = Expr::Cmp {
                        op: CmpOp::Eq,
                        left: Box::new(Expr::Column {
                            table: Some(lc.qual.clone()),
                            name: lc.name.clone(),
                        }),
                        right: Box::new(Expr::Column {
                            table: Some(rc.qual.clone()),
                            name: rc.name.clone(),
                        }),
                    };
                    cond = Some(match cond {
                        None => eq,
                        Some(c) => Expr::And(Box::new(c), Box::new(eq)),
                    });
                }
                cond
            } else {
                on.clone()
            };
            // v0.23: output schema is the merged join layout (merged keys
            // first, then remaining left/right columns, then hidden
            // originals); the predicate scopes below still use the
            // pre-merge side schemas.
            let schema = layout.schema.clone();
            // Predicate pushdown, inner joins only below nested joins:
            // each side is filtered here only when it is a leaf source
            // (table or derived table), so a conjunct is never pushed at
            // two levels. Pushing a WHERE conjunct to the non-preserved
            // side of an outer join is unsound (e.g. `WHERE b.y IS NULL`
            // on a LEFT JOIN would turn non-matches into matches), so:
            // INNER/CROSS push both sides; LEFT pushes left only; RIGHT
            // pushes right only; FULL pushes neither. The full WHERE
            // still applies after the join, so this is purely an
            // optimization.
            let push_left = matches!(kind, JoinKind::Inner | JoinKind::Cross | JoinKind::Left);
            let push_right = matches!(kind, JoinKind::Inner | JoinKind::Cross | JoinKind::Right);
            let lquals: HashSet<String> = lschema.iter().map(|c| c.qual.clone()).collect();
            let rquals: HashSet<String> = rschema.iter().map(|c| c.qual.clone()).collect();
            let lrows = if push_left
                && matches!(
                    left.as_ref(),
                    FromItem::Table { .. } | FromItem::Derived { .. }
                ) {
                let push = pushdown_for(&q.eng.db, where_, &lquals, &rquals, &lschema, &rschema);
                filter_rows(q, outer, &lschema, lrows, &push)?
            } else {
                lrows
            };
            let rrows = if push_right
                && matches!(
                    right.as_ref(),
                    FromItem::Table { .. } | FromItem::Derived { .. }
                ) {
                let push = pushdown_for(&q.eng.db, where_, &rquals, &lquals, &rschema, &lschema);
                filter_rows(q, outer, &rschema, rrows, &push)?
            } else {
                rrows
            };
            // Three execution paths for the nested loop. Fast path: no ON
            // column ref is ambiguous across the two sides, so two
            // separate frames resolve exactly like the old single
            // combined frame (which could never raise 42702 here) — and
            // no combined row is built per pair. The slow path is the old
            // combined-frame code, unchanged; it only triggers for
            // ambiguous ON predicates (which raise 42702 on the first
            // pair) — correctness is never at risk.
            let mut rows = Vec::new();
            let is_cross = *kind == JoinKind::Cross;
            let on_pred: Option<&Expr> = if is_cross { None } else { on_expr.as_ref() };
            // v0.20.1: native preservation flags — LEFT keeps unmatched
            // left rows, RIGHT keeps unmatched right rows, FULL keeps
            // both, INNER/CROSS keep neither.
            let preserve_left = matches!(kind, JoinKind::Left | JoinKind::Full);
            let preserve_right = matches!(kind, JoinKind::Right | JoinKind::Full);
            // v0.20: RIGHT/FULL JOIN track which right rows matched.
            let mut matched_right = vec![false; rrows.len()];
            let fast = is_cross || on_pred.map_or(false, |p| join_fast_path(p, &lschema, &rschema));
            // Pre-resolve the ON predicate's column references once: the
            // scope shape is fixed for the whole loop, so per-pair name
            // resolution is pure overhead (~20% of join instructions per
            // Callgrind). The prototype schema order matches the runtime
            // scope order below (outer frames, then left, then right).
            // Only resolve when the loop would actually evaluate the
            // predicate (both sides non-empty): with an empty input the
            // loop body never runs, and per-row evaluation would never
            // have raised a resolution error either.
            let mut proto_schemas: Vec<&[QCol]> = Vec::with_capacity(outer.len() + 2);
            for s in outer {
                proto_schemas.push(s.schema);
            }
            proto_schemas.push(&lschema);
            proto_schemas.push(&rschema);
            let on_resolved: Option<Expr> = match on_pred {
                Some(p) if fast && !lrows.is_empty() && !rrows.is_empty() => {
                    Some(resolve_predicate_columns(p, &proto_schemas)?)
                }
                _ => None,
            };
            let on_fast: Option<&Expr> = on_resolved.as_ref().or(on_pred);
            // v0.93: hash equi-join for INNER joins. `fast` guarantees
            // the two-frame evaluation the hash path relies on, and
            // `on_fast` is the resolved predicate the key extractor
            // reads. A runtime `None` (unexpected value variant) falls
            // back to the nested loop below — correctness never depends
            // on the hash path succeeding.
            let hash_keys: Option<Vec<(usize, usize, HashFam)>> =
                if *kind == JoinKind::Inner && fast && !is_cross {
                    on_fast.and_then(|p| hash_join_keys(p, outer.len(), &lschema, &rschema))
                } else {
                    None
                };
            if let (Some(pred), Some(keys)) = (on_fast, hash_keys) {
                if let Some(joined) = exec_hash_join(
                    q, outer, &lschema, &lrows, &rschema, &rrows, &layout, *kind, pred, &keys,
                )? {
                    return Ok((schema, joined));
                }
            }
            // v0.23: null-extended sides, in side-schema layout; the
            // combiner projects them into the merged layout (COALESCE for
            // FULL, preserved side otherwise).
            let null_l = QRow {
                cells: Row::new(vec![Value::Null; lschema.len()]),
                prov: Vec::new(),
            };
            let null_r = QRow {
                cells: Row::new(vec![Value::Null; rschema.len()]),
                prov: Vec::new(),
            };
            if fast && outer.is_empty() {
                // Hot path: zero allocation per row pair. The two frames
                // live in a stack array; Scope is Copy.
                for l in &lrows {
                    let fl = Scope {
                        schema: &lschema,
                        row: &l.cells,
                        prov: None,
                    };
                    let mut matched = false;
                    for (ri, r) in rrows.iter().enumerate() {
                        let fr = Scope {
                            schema: &rschema,
                            row: &r.cells,
                            prov: None,
                        };
                        let scopes = [fl, fr];
                        let keep = match on_fast {
                            None => true,
                            Some(p) => check_bool(eval_expr(q, &scopes, p)?, "join condition")?,
                        };
                        if keep {
                            matched = true;
                            if preserve_right {
                                matched_right[ri] = true;
                            }
                            rows.push(combine_join_pair(l, r, &layout, *kind));
                        }
                    }
                    if !matched && preserve_left {
                        rows.push(combine_join_pair(l, &null_r, &layout, *kind));
                    }
                }
            } else if fast {
                // Correlated join (rare): one small scope Vec per pair.
                for l in &lrows {
                    let mut matched = false;
                    for (ri, r) in rrows.iter().enumerate() {
                        let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 2);
                        scopes.extend_from_slice(outer);
                        scopes.push(Scope {
                            schema: &lschema,
                            row: &l.cells,
                            prov: None,
                        });
                        scopes.push(Scope {
                            schema: &rschema,
                            row: &r.cells,
                            prov: None,
                        });
                        let keep = match on_fast {
                            None => true,
                            Some(p) => check_bool(eval_expr(q, &scopes, p)?, "join condition")?,
                        };
                        if keep {
                            matched = true;
                            if preserve_right {
                                matched_right[ri] = true;
                            }
                            rows.push(combine_join_pair(l, r, &layout, *kind));
                        }
                    }
                    if !matched && preserve_left {
                        rows.push(combine_join_pair(l, &null_r, &layout, *kind));
                    }
                }
            } else {
                // Slow path: original combined-frame evaluation.
                for l in &lrows {
                    let mut matched = false;
                    for (ri, r) in rrows.iter().enumerate() {
                        // v0.23: the pair is built in merged layout up
                        // front, so the combined frame matches `schema`.
                        let pair = combine_join_pair(l, r, &layout, *kind);
                        let keep = match on_pred {
                            None => true,
                            Some(p) => {
                                let frame = Scope {
                                    schema: &schema,
                                    row: &pair.cells,
                                    prov: None,
                                };
                                let mut scopes: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
                                scopes.extend_from_slice(outer);
                                scopes.push(frame);
                                check_bool(eval_expr(q, &scopes, p)?, "join condition")?
                            }
                        };
                        if keep {
                            matched = true;
                            if preserve_right {
                                matched_right[ri] = true;
                            }
                            rows.push(pair);
                        }
                    }
                    if !matched && preserve_left {
                        rows.push(combine_join_pair(l, &null_r, &layout, *kind));
                    }
                }
            }
            // v0.20: RIGHT/FULL JOIN — add right rows that never matched,
            // with NULLs for the left columns. Rows are already in
            // (left, right) order; no un-swapping needed.
            if preserve_right {
                for (ri, r) in rrows.iter().enumerate() {
                    if !matched_right[ri] {
                        rows.push(combine_join_pair(&null_l, r, &layout, *kind));
                    }
                }
            }
            Ok((schema, rows))
        }
    }
}

pub(crate) fn apply_where(
    q: &mut Q,
    outer: &[Scope],
    schema: &[QCol],
    rows: Vec<QRow>,
    pred: Option<&Expr>,
) -> Result<Vec<QRow>, ExecError> {
    let Some(pred) = pred else {
        return Ok(rows);
    };
    let mut out = Vec::new();
    for row in rows {
        let frame = Scope {
            schema,
            row: &row.cells,
            prov: Some(&row.prov),
        };
        let scopes_storage: Vec<Scope>;
        let scopes: &[Scope] = if outer.is_empty() {
            std::slice::from_ref(&frame)
        } else {
            let mut buf: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
            buf.extend_from_slice(outer);
            buf.push(frame);
            scopes_storage = buf;
            &scopes_storage
        };
        let keep = check_bool(eval_expr(q, scopes, pred)?, "WHERE")?;
        if keep {
            out.push(row);
        }
    }
    Ok(out)
}

/// A predicate result keeps the row iff it is TRUE (NULL counts as false,
/// SQL three-valued logic); a non-boolean is 42804, like Postgres.
pub(crate) fn check_bool(v: Value, what: &str) -> Result<bool, ExecError> {
    match v {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        other => Err(exec_err(
            "42804",
            format!(
                "argument of {} must be type boolean, not type {}",
                what,
                other.type_name()
            ),
        )),
    }
}
