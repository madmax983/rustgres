// v2.27: the constraint-locality model (PG19 conislocal/coninhcount).
//
// Only ONE bit of locality is stored per constraint: `is_local`
// (CheckDef / UniqueDef / FkDef) resp. `Table::nn_local` for not-null
// columns. `coninhcount` / `attinhcount` are DERIVED from the live
// parent links: the number of DIRECT parents (Table::inherits entries
// plus the partition parent) that carry the same constraint / column.
// Deriving the count makes every parent-side mutation (ADD / DROP
// CONSTRAINT, SET / DROP NOT NULL, INHERIT / NO INHERIT, ATTACH /
// DETACH) adjust the children's counts automatically; the mutating
// paths only maintain the is_local bits and the presence of the
// constraint copies themselves.
//
// Matching rules (PG19): CHECK constraints match across a parent link
// by NAME (an identical local definition merges into the inherited
// one); NOT NULL constraints match by COLUMN; UNIQUE / PRIMARY KEY /
// FOREIGN KEY constraints are never inherited through plain INHERITS
// (only CHECK and NOT NULL are), so they match only across a
// partition link, where the child's copy may be renamed — by shape
// (columns, plus the referenced table for FKs).
use super::*;

/// The identity of a stored constraint for locality purposes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConRef {
    /// A CHECK-kind CheckDef, matched by name.
    Check(String),
    /// A NOT NULL constraint over this column (named NotNull CheckDefs
    /// and bare not-null flags alike), matched by column.
    NotNull(String),
    /// A UNIQUE constraint, matched by column shape.
    Unique(Vec<String>),
    /// The PRIMARY KEY, matched by column shape.
    PKey(Vec<String>),
    /// A FOREIGN KEY, matched by (columns, referenced table).
    Fk(Vec<String>, String),
}

impl ConRef {
    pub(crate) fn of_check(c: &CheckDef) -> ConRef {
        match c.kind {
            crate::sql::CheckKind::NotNull => {
                let mut refs = Vec::new();
                crate::sql::collect_col_refs(&c.expr, &mut refs);
                ConRef::NotNull(refs.into_iter().next().map(|(_, n)| n).unwrap_or_default())
            }
            crate::sql::CheckKind::Check => ConRef::Check(c.name.clone()),
        }
    }
}

/// Direct parents of `t`: the INHERITS links in order, then the
/// partition parent (a table has one or the other in practice).
pub(crate) fn direct_parents(t: &Table) -> Vec<String> {
    let mut out = t.inherits.clone();
    if let Some(p) = t.partition.as_ref().and_then(|p| p.parent.clone()) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Is `t` a partition child (its parent link is the partition link)?
pub(crate) fn is_partition_child(t: &Table) -> bool {
    t.partition.as_ref().is_some_and(|p| p.parent.is_some())
}

/// Does parent table `pt` carry a constraint matching `r`?
pub(crate) fn parent_carries(pt: &Table, r: &ConRef, partition_link: bool) -> bool {
    match r {
        ConRef::Check(name) => pt
            .checks
            .iter()
            .any(|c| c.kind == crate::sql::CheckKind::Check && &c.name == name),
        ConRef::NotNull(col) => {
            // The parent's column must be not-null through an
            // inheritable source: a NO INHERIT not-null constraint on
            // the parent does not propagate (PG19 connoinherit).
            let Some(pi) = pt.column_index(col) else {
                return false;
            };
            if !pt.not_null.get(pi).copied().unwrap_or(false) {
                return false;
            }
            !pt.checks.iter().any(|c| {
                c.kind == crate::sql::CheckKind::NotNull
                    && c.no_inherit
                    && crate::sql::check_expr_column(&c.expr).as_deref() == Some(col.as_str())
            })
        }
        ConRef::Unique(cols) => partition_link && pt.uniques.iter().any(|u| &u.cols == cols),
        ConRef::PKey(cols) => partition_link && pt.pkey.as_ref().is_some_and(|p| &p.cols == cols),
        ConRef::Fk(cols, ref_table) => {
            partition_link
                && pt
                    .fks
                    .iter()
                    .any(|f| &f.cols == cols && &f.ref_table == ref_table)
        }
    }
}

/// Does table `t` itself carry a stored constraint matching `r`
/// (presence, ignoring parent links)?
pub(crate) fn carries(t: &Table, r: &ConRef) -> bool {
    parent_carries(t, r, true)
}

/// PG19 coninhcount for constraint `r` on table `t`: the number of
/// direct parents carrying a matching constraint.
pub(crate) fn con_inhcount(
    db: &Database,
    snap: &Snapshot,
    xids: &[u64],
    session: u64,
    t: &Table,
    r: &ConRef,
) -> i64 {
    let part_parent = t.partition.as_ref().and_then(|p| p.parent.clone());
    let mut n = 0;
    for pname in direct_parents(t) {
        let partition_link = part_parent.as_deref() == Some(pname.as_str());
        if let Some(pt) = db.find_table(&pname, snap, xids, session) {
            if parent_carries(pt, r, partition_link) {
                n += 1;
            }
        }
    }
    n
}

/// PG19 attinhcount for column `col` on table `t`.
pub(crate) fn att_inhcount(
    db: &Database,
    snap: &Snapshot,
    xids: &[u64],
    session: u64,
    t: &Table,
    col: &str,
) -> i64 {
    let mut n = 0;
    for pname in direct_parents(t) {
        if let Some(pt) = db.find_table(&pname, snap, xids, session) {
            if pt.column_index(col).is_some() {
                n += 1;
            }
        }
    }
    n
}

/// The stored local bit for constraint `r` on table `t` (PG19
/// conislocal). For NOT NULL this is the column's nn_local bit.
/// Partition CHECK/UNIQUE/PK/FK copies are never local while
/// inherited (the caller combines this with the derived count; see
/// `con_is_local`).
pub(crate) fn stored_is_local(t: &Table, r: &ConRef) -> bool {
    match r {
        ConRef::Check(name) => t
            .checks
            .iter()
            .find(|c| c.kind == crate::sql::CheckKind::Check && &c.name == name)
            .map(|c| c.is_local)
            .unwrap_or(true),
        ConRef::NotNull(col) => {
            // A named NotNull CheckDef carries its own bit; the
            // flag-only case reads the column's nn_local bit.
            if let Some(c) = t.checks.iter().find(|c| {
                c.kind == crate::sql::CheckKind::NotNull
                    && crate::sql::check_expr_column(&c.expr).as_deref() == Some(col.as_str())
            }) {
                return c.is_local;
            }
            t.column_index(col)
                .map(|i| t.nn_is_local(i))
                .unwrap_or(true)
        }
        ConRef::Unique(cols) => t
            .uniques
            .iter()
            .find(|u| &u.cols == cols)
            .map(|u| u.is_local)
            .unwrap_or(true),
        ConRef::PKey(cols) => t
            .pkey
            .as_ref()
            .filter(|p| &p.cols == cols)
            .map(|p| p.is_local)
            .unwrap_or(true),
        ConRef::Fk(cols, ref_table) => t
            .fks
            .iter()
            .find(|f| &f.cols == cols && &f.ref_table == ref_table)
            .map(|f| f.is_local)
            .unwrap_or(true),
    }
}

/// Effective PG19 conislocal for constraint `r` on table `t`: on a
/// partition child, an inherited CHECK/UNIQUE/PK/FK is never local
/// (heap.c MergeWithExistingConstraint forces conislocal = false for
/// relispartition); NOT NULL keeps its stored bit (the PARTITION OF
/// element case: part_b_b_not_null is t / 1). On plain tables the
/// stored bit stands.
pub(crate) fn con_is_local(
    db: &Database,
    snap: &Snapshot,
    xids: &[u64],
    session: u64,
    t: &Table,
    r: &ConRef,
) -> bool {
    if is_partition_child(t) && !matches!(r, ConRef::NotNull(_)) {
        if con_inhcount(db, snap, xids, session, t, r) > 0 {
            return false;
        }
    }
    stored_is_local(t, r)
}

/// The name under which constraint `r` is stored on table `t`, if
/// present (partition FK/UNIQUE/PK clones may be renamed per child).
pub(crate) fn con_stored_name(t: &Table, r: &ConRef) -> Option<String> {
    match r {
        ConRef::Check(name) => t
            .checks
            .iter()
            .find(|c| c.kind == crate::sql::CheckKind::Check && &c.name == name)
            .map(|c| c.name.clone()),
        ConRef::NotNull(col) => t
            .checks
            .iter()
            .find(|c| {
                c.kind == crate::sql::CheckKind::NotNull
                    && crate::sql::check_expr_column(&c.expr).as_deref() == Some(col.as_str())
            })
            .map(|c| c.name.clone()),
        ConRef::Unique(cols) => t
            .uniques
            .iter()
            .find(|u| &u.cols == cols)
            .map(|u| u.name.clone()),
        ConRef::PKey(cols) => t
            .pkey
            .as_ref()
            .filter(|p| &p.cols == cols)
            .map(|p| p.name.clone()),
        ConRef::Fk(cols, ref_table) => t
            .fks
            .iter()
            .find(|f| &f.cols == cols && &f.ref_table == ref_table)
            .map(|f| f.name.clone()),
    }
}
