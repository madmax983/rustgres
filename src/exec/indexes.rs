// v1.78 mechanical split: moved verbatim from src/exec.rs (9074-10094).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// v0.8 indexes: DDL, unique enforcement, planner, EXPLAIN, ANALYZE
// ---------------------------------------------------------------------------

/// 23505 error constructor.
pub(crate) fn unique_violation_err(index: &str) -> ExecError {
    exec_err(
        "23505",
        format!(
            "duplicate key value violates unique constraint \"{}\"",
            index
        ),
    )
}

/// Statement-atomic UNIQUE check for INSERT: every candidate row is checked
/// against the indexes and against earlier rows of the same statement
/// before any version is pushed.
pub(crate) fn check_insert_unique(
    db: &Database,
    table: &str,
    rows: &[Row],
    snap: &Snapshot,
    owns: &[u64],
    session: u64,
) -> Result<(), ExecError> {
    // v0.22: temp tables have no backing indexes; their constraints are
    // checked by column positions with index key semantics.
    let uniques: Vec<(String, Vec<usize>)> = if db.is_temp_table(session, table) {
        temp_unique_cols(db, table, snap, owns, session)
    } else {
        db.visible_indexes_for(table, snap, owns, session)
            .into_iter()
            .filter(|ix| ix.def.unique && ix.def.planner_usable)
            .map(|ix| (ix.def.name.clone(), ix.def.cols.clone()))
            .collect()
    };
    for (i, values) in rows.iter().enumerate() {
        for (cname, cols) in &uniques {
            let key = temp_key(cols, values);
            if key.0.iter().any(|v| matches!(v, Value::Null)) {
                continue; // NULLs never conflict
            }
            if rows[..i].iter().any(|prev| temp_key(cols, prev) == key) {
                return Err(unique_violation_err(cname));
            }
        }
        if let Some(name) = db.unique_violation(table, values, None, snap, owns, session) {
            return Err(unique_violation_err(&name));
        }
    }
    Ok(())
}

/// v0.71: statement-atomic UNIQUE enforcement for INSERT into a
/// partitioned parent (PG19: the per-leaf backing indexes are the
/// enforced constraints; the parent's own indexes hold no rows). Each
/// candidate row is routed to its leaf and remapped to the leaf's column
/// order, then checked against that leaf's unique indexes and against
/// earlier rows of this statement bound for the same leaf.
pub(crate) fn check_partitioned_insert_unique(
    eng: &mut Engine,
    ctx: &StmtCtx,
    table: &str,
    rows: &[Row],
) -> Result<(), ExecError> {
    let parent_cols = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("target still visible; engine lock held throughout");
        t.columns.clone()
    };
    // Group candidate row indices by destination leaf (routing is
    // statement-atomic: no mutation happens here).
    let mut by_leaf: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let leaf = find_row_leaf(eng, ctx, table, row)?;
        match by_leaf.iter_mut().find(|(l, _)| *l == leaf) {
            Some((_, idxs)) => idxs.push(i),
            None => by_leaf.push((leaf, vec![i])),
        }
    }
    for (leaf, idxs) in &by_leaf {
        let leaf_cols = {
            let lt = eng
                .db
                .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("leaf still visible; engine lock held throughout");
            lt.columns.clone()
        };
        let uniques: Vec<(String, Vec<usize>)> = eng
            .db
            .visible_indexes_for(leaf, ctx.snap, &ctx.all_xids, ctx.session)
            .into_iter()
            .filter(|ix| ix.def.unique && ix.def.planner_usable)
            .map(|ix| (ix.def.name.clone(), ix.def.cols.clone()))
            .collect();
        let leaf_rows: Vec<Vec<Value>> = idxs
            .iter()
            .map(|&i| remap_parent_to_leaf(&parent_cols, &leaf_cols, &rows[i]))
            .collect();
        for (j, values) in leaf_rows.iter().enumerate() {
            for (iname, cols) in &uniques {
                let key = temp_key(cols, values);
                if key.0.iter().any(|v| matches!(v, Value::Null)) {
                    continue; // NULLs never conflict
                }
                if leaf_rows[..j]
                    .iter()
                    .any(|prev| temp_key(cols, prev) == key)
                {
                    return Err(unique_violation_err(&leaf_constraint_name(
                        eng, ctx, leaf, iname,
                    )));
                }
            }
            if let Some(name) =
                eng.db
                    .unique_violation(leaf, values, None, ctx.snap, &ctx.all_xids, ctx.session)
            {
                return Err(unique_violation_err(&leaf_constraint_name(
                    eng, ctx, leaf, &name,
                )));
            }
        }
    }
    Ok(())
}

/// v0.22: (constraint name, key column positions) for a temp table's
/// PRIMARY KEY / UNIQUE constraints, which have no backing indexes.
/// Used wherever the permanent path reads unique indexes.
pub(crate) fn temp_unique_cols(
    db: &Database,
    table: &str,
    snap: &Snapshot,
    owns: &[u64],
    session: u64,
) -> Vec<(String, Vec<usize>)> {
    let Some(t) = db.find_table(table, snap, owns, session) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push = |u: &UniqueDef| {
        let cols: Vec<usize> = u.cols.iter().filter_map(|c| t.column_index(c)).collect();
        if cols.len() == u.cols.len() {
            out.push((u.name.clone(), cols));
        }
    };
    if let Some(pk) = &t.pkey {
        push(pk);
    }
    for u in &t.uniques {
        push(u);
    }
    out
}

/// v0.22: build an `IndexKey` over `cols` positions of `values`, so temp
/// constraint checks compare with index key semantics (`index_key_cmp`),
/// not raw `==` — exactly what `Index::key_for` does for real indexes.
pub(crate) fn temp_key(cols: &[usize], values: &[Value]) -> IndexKey {
    IndexKey(cols.iter().map(|&p| values[p].clone()).collect())
}

/// Pairwise UNIQUE check for UPDATE's planned rows: the index still holds
/// only the old entries at plan time, so new-vs-new conflicts need an
/// explicit pass.
/// v0.22: temp tables have no backing indexes; their constraints are
/// checked by column positions with index key semantics.
pub(crate) fn check_update_unique_pairs(
    db: &Database,
    table: &str,
    plan: &[(u64, u64, Row)],
    snap: &Snapshot,
    owns: &[u64],
    session: u64,
) -> Result<(), ExecError> {
    let uniques: Vec<(String, Vec<usize>)> = if db.is_temp_table(session, table) {
        temp_unique_cols(db, table, snap, owns, session)
    } else {
        db.visible_indexes_for(table, snap, owns, session)
            .into_iter()
            .filter(|ix| ix.def.unique && ix.def.planner_usable)
            .map(|ix| (ix.def.name.clone(), ix.def.cols.clone()))
            .collect()
    };
    for (i, (_, _, values)) in plan.iter().enumerate() {
        for (cname, cols) in &uniques {
            let key = temp_key(cols, values);
            if key.0.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            if plan[..i]
                .iter()
                .any(|(_, _, prev)| temp_key(cols, prev) == key)
            {
                return Err(unique_violation_err(cname));
            }
        }
    }
    Ok(())
}

pub(crate) fn exec_create_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    table: &str,
    columns: &[IndexColSpec],
    unique: bool,
    if_not_exists: bool,
    predicate: Option<&str>,
) -> Result<ExecResult, ExecError> {
    // v0.11: indexing a table needs its owner (or a superuser).
    require_table_owner(eng, ctx, table)?;
    // v0.87: temp tables use the session-local temp index map.
    let is_temp = eng.db.is_temp_table(ctx.session, table);
    let exists = if is_temp {
        eng.db
            .temp_indexes
            .get(&ctx.session)
            .and_then(|m| m.get(name))
            .filter(|ix| index_visible(&ix.def, ctx.snap, &ctx.all_xids))
            .is_some()
    } else {
        eng.db.find_index(name, ctx.snap, &ctx.all_xids).is_some()
    };
    if exists {
        if if_not_exists {
            return Ok(ExecResult::Command {
                tag: "CREATE INDEX".to_string(),
            });
        }
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    // v0.88: expression / partial indexes are catalog-only for now: the
    // v0.88 planner has no expression evaluation or predicate
    // implication, so they are stored but never built, maintained, or
    // consulted by a scan path.
    let planner_usable = predicate.is_none() && columns.iter().all(|c| c.expr.is_none());
    // Resolve the table and plain columns (immutable borrows only).
    // Expression key columns carry usize::MAX (no single position).
    let (cols, col_names, descs, nulls_firsts, exprs) = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let mut cols = Vec::with_capacity(columns.len());
        let mut col_names = Vec::with_capacity(columns.len());
        let mut descs = Vec::with_capacity(columns.len());
        let mut nulls_firsts = Vec::with_capacity(columns.len());
        let mut exprs = Vec::with_capacity(columns.len());
        let mut seen = Vec::with_capacity(columns.len());
        for c in columns {
            match (&c.name, &c.expr) {
                (Some(n), None) => {
                    let pos = t.column_index(n).ok_or_else(|| {
                        exec_err(
                            "42703",
                            format!("column \"{}\" of relation \"{}\" does not exist", n, table),
                        )
                    })?;
                    if seen.contains(&pos) {
                        return Err(exec_err(
                            "42701",
                            format!("column \"{}\" specified more than once", n),
                        ));
                    }
                    seen.push(pos);
                    cols.push(pos);
                    col_names.push(n.clone());
                }
                (None, Some(e)) => {
                    cols.push(usize::MAX);
                    col_names.push(e.clone());
                }
                _ => {
                    return Err(exec_err(
                        "42601",
                        "syntax error: malformed index key".to_string(),
                    ));
                }
            }
            descs.push(c.desc);
            nulls_firsts.push(c.nulls_first);
            exprs.push(c.expr.clone());
        }
        if cols.is_empty() {
            return Err(exec_err(
                "42601",
                "syntax error: index requires at least one column".to_string(),
            ));
        }
        (cols, col_names, descs, nulls_firsts, exprs)
    };
    let mut ix = Index::new(IndexDef {
        name: name.to_string(),
        table: table.to_string(),
        cols,
        col_names,
        unique,
        internal: false,
        created_xmin: ctx.write_xid,
        dropped_xmax: 0,
        desc: descs,
        nulls_first: nulls_firsts,
        exprs,
        predicate: predicate.map(|s| s.to_string()),
        planner_usable,
    });
    if planner_usable {
        // Build + backfill from every version of the visible table
        // version; visibility is resolved at scan time, so uncommitted
        // and dead versions get entries too (like Postgres' heap/index
        // split). v0.88: the tree is always built in canonical ascending
        // order (NULLs high), even for DESC keys — the DESC flags are
        // catalog fidelity; the ORDER BY fast path only trusts
        // all-ascending indexes.
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible; engine lock held throughout");
        for r in &t.rows {
            let key = ix.key_for(&r.values);
            ix.insert(key, r.id);
        }
        if unique {
            // A duplicate among versions that could still become visible
            // fails the CREATE, like Postgres. Conservative: versions deleted
            // only by still-active transactions (or by us) count as live.
            for (key, ids) in &ix.tree {
                if key.0.iter().any(|v| matches!(v, Value::Null)) {
                    continue;
                }
                let mut live = 0u32;
                for &id in ids {
                    let dead = match t.row_pos(id) {
                        Some(pos) => {
                            let r = &t.rows[pos];
                            r.xmax != 0 && r.xmax != ctx.own && eng.xid_committed(r.xmax)
                        }
                        None => true, // vacuumed away: cannot conflict
                    };
                    if !dead {
                        live += 1;
                        if live >= 2 {
                            return Err(unique_violation_err(name));
                        }
                    }
                }
            }
        }
    }
    // `t`'s borrow ends at its last use above; the insert below needs
    // `eng.db` mutably.
    if is_temp {
        eng.db
            .temp_indexes
            .entry(ctx.session)
            .or_default()
            .insert(name.to_string(), ix);
        ctx.writes.push(WriteOp::CreateTempIndex {
            session: ctx.session,
            name: name.to_string(),
        });
    } else {
        eng.db.indexes.insert(name.to_string(), ix);
        ctx.writes.push(WriteOp::CreateIndex {
            name: name.to_string(),
        });
    }
    Ok(ExecResult::Command {
        tag: "CREATE INDEX".to_string(),
    })
}

pub(crate) fn exec_drop_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    // v0.89: like PostgreSQL, resolve every name before dropping any: a
    // missing name anywhere in the list aborts the whole statement with
    // 42P01 and no index is dropped (the old sequential loop left
    // earlier names dropped when a later name was missing).
    for name in names {
        let temp_hit = eng
            .db
            .temp_indexes
            .get(&ctx.session)
            .and_then(|m| m.get(name))
            .filter(|ix| index_visible(&ix.def, ctx.snap, &ctx.all_xids))
            .is_some();
        if !temp_hit && eng.db.find_index(name, ctx.snap, &ctx.all_xids).is_none() && !if_exists {
            return Err(exec_err(
                "42P01",
                format!("index \"{}\" does not exist", name),
            ));
        }
    }
    for name in names {
        exec_drop_one_index(eng, ctx, name, if_exists)?;
    }
    Ok(ExecResult::Command {
        tag: "DROP INDEX".to_string(),
    })
}

pub(crate) fn exec_drop_one_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_exists: bool,
) -> Result<(), ExecError> {
    // v0.87: check session-local temp indexes first.
    let temp_snapshot = eng
        .db
        .temp_indexes
        .get(&ctx.session)
        .and_then(|m| m.get(name))
        .filter(|ix| index_visible(&ix.def, ctx.snap, &ctx.all_xids))
        .cloned();
    if let Some(snapshot) = temp_snapshot {
        // v0.11: dropping an index needs its table's owner (or a superuser).
        require_table_owner(eng, ctx, &snapshot.def.table.clone())?;
        if let Some(m) = eng.db.temp_indexes.get_mut(&ctx.session) {
            if let Some(ix) = m.get_mut(name) {
                ix.def.dropped_xmax = ctx.own;
            }
        }
        ctx.writes.push(WriteOp::DropTempIndex {
            session: ctx.session,
            name: name.to_string(),
            index: snapshot,
        });
        return Ok(());
    }
    // Snapshot the definition first: the write log's undo restores it on
    // ROLLBACK, and the WAL replays the drop on commit.
    // v0.11: dropping an index needs its table's owner (or a superuser).
    if let Some(ix) = eng.db.find_index(name, ctx.snap, &ctx.all_xids) {
        require_table_owner(eng, ctx, &ix.def.table.clone())?;
    }
    let snapshot = match eng.db.find_index(name, ctx.snap, &ctx.all_xids) {
        Some(ix) => ix.clone(),
        None => {
            if if_exists {
                return Ok(());
            }
            return Err(exec_err(
                "42P01",
                format!("index \"{}\" does not exist", name),
            ));
        }
    };
    eng.db
        .find_index_mut(name, ctx.snap, &ctx.all_xids)
        .expect("index still present; engine lock held throughout")
        .def
        .dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropIndex {
        name: name.to_string(),
        index: snapshot,
    });
    Ok(())
}

// --- v0.8 planner ----------------------------------------------------------

/// One usable index bound extracted from a WHERE conjunct.
#[derive(Clone)]
pub(crate) enum IndexBoundKind {
    Eq,
    /// Greater-than; bool = inclusive.
    Gt(bool),
    /// Less-than; bool = inclusive.
    Lt(bool),
}

/// Try to read one WHERE conjunct as bounds on this table's columns.
/// `None` when the conjunct is not indexable — never an error, the
/// residual filter handles it.
pub(crate) fn conjunct_bounds(
    e: &Expr,
    qual: &str,
    table_name: &str,
    columns: &[(String, ColType)],
) -> Option<Vec<(usize, IndexBoundKind, Value)>> {
    // Resolve `Column op Literal` in either order, normalizing the
    // comparison direction.
    let from_cmp =
        |op: &CmpOp, left: &Expr, right: &Expr| -> Option<(usize, IndexBoundKind, Value)> {
            let (col_side, lit_side, flip) = match (left, right) {
                (Expr::Column { .. }, Expr::Literal(_)) => (left, right, false),
                (Expr::Literal(_), Expr::Column { .. }) => (right, left, true),
                _ => return None,
            };
            let (cq, cname) = match col_side {
                Expr::Column { table, name } => (table, name),
                _ => return None,
            };
            if let Some(cq) = cq {
                if cq != qual && cq != table_name {
                    return None;
                }
            }
            let lit = match lit_side {
                Expr::Literal(l) => l,
                _ => return None,
            };
            let pos = columns.iter().position(|(n, _)| n == cname)?;
            let (_, ctype) = &columns[pos];
            // The literal is coerced to the column type, exactly like an
            // INSERT value; anything uncoercible stays a sequential scan.
            let v = coerce_literal(lit, ctype, cname).ok()?;
            let kind = match (op, flip) {
                (CmpOp::Eq, _) => IndexBoundKind::Eq,
                (CmpOp::Gt, false) | (CmpOp::Lt, true) => IndexBoundKind::Gt(false),
                (CmpOp::Ge, false) | (CmpOp::Le, true) => IndexBoundKind::Gt(true),
                (CmpOp::Lt, false) | (CmpOp::Gt, true) => IndexBoundKind::Lt(false),
                (CmpOp::Le, false) | (CmpOp::Ge, true) => IndexBoundKind::Lt(true),
                _ => return None, // <> is never indexable
            };
            Some((pos, kind, v))
        };
    match e {
        Expr::Cmp { op, left, right } => from_cmp(op, left, right).map(|b| vec![b]),
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            if *neg {
                return None;
            }
            let (cq, cname) = match &**expr {
                Expr::Column { table, name } => (table, name),
                _ => return None,
            };
            if let Some(cq) = cq {
                if cq != qual && cq != table_name {
                    return None;
                }
            }
            let lo = match &**low {
                Expr::Literal(l) => l,
                _ => return None,
            };
            let hi = match &**high {
                Expr::Literal(l) => l,
                _ => return None,
            };
            let pos = columns.iter().position(|(n, _)| n == cname)?;
            let (_, ctype) = &columns[pos];
            let lo_v = coerce_literal(lo, ctype, cname).ok()?;
            let hi_v = coerce_literal(hi, ctype, cname).ok()?;
            Some(vec![
                (pos, IndexBoundKind::Gt(true), lo_v),
                (pos, IndexBoundKind::Lt(true), hi_v),
            ])
        }
        _ => None,
    }
}

/// v1.62: estimated heap page count for the scan-type choice — PG19's
/// own heap-page accounting (`pg_heap_page_count`, the v1.41 port of
/// hio.c placement) over every row version in the table. PG's planner
/// costs `baserel->pages` (the physical page count, dead tuples
/// included — rustgres has no VACUUM), so this deliberately ignores
/// snapshot visibility, mirroring `relpages`.
pub(crate) fn est_heap_pages(t: &Table) -> u64 {
    // A page holds at most 291 minimum-size tuples
    // (`MAX_HEAP_TUPLES_PER_PAGE` inside `pg_heap_page_count`); past
    // that the table provably exceeds one page without measuring
    // every tuple.
    const MAX_TUPLES_PER_PAGE: usize = 291;
    let mut lens: Vec<usize> = Vec::new();
    for rv in &t.rows {
        if lens.len() >= MAX_TUPLES_PER_PAGE {
            return 2;
        }
        lens.push(pg_heap_tuple_len(t, rv));
    }
    // `pg_heap_page_count` returns bytes; one heap page is 8192.
    pg_heap_page_count(t, &lens) / 8192
}

/// Planned access path for one base-table scan.
#[derive(Clone, Debug)]
pub(crate) enum AccessPath {
    SeqScan,
    IndexScan {
        index: String,
        /// Equality values for the leading index columns.
        prefix: Vec<Value>,
        /// Optional range on the next index column: (value, inclusive).
        lo: Option<(Value, bool)>,
        hi: Option<(Value, bool)>,
        /// Human-readable condition for EXPLAIN.
        cond: String,
        /// v1.08: AND-conjunct indices consumed by `cond` (the rest stay
        /// residual Filters).
        used: Vec<usize>,
    },
}

/// Choose an access path for one base-table source. The index scan's row
/// set is always a *superset* of the true matches (same-type bounds under
/// the index's own ordering); the executor still applies the full residual
/// predicate and MVCC visibility, so a wrong choice costs speed, never
/// correctness.
pub(crate) fn plan_access_path(
    db: &Database,
    t: &Table,
    table_name: &str,
    qual: &str,
    where_: Option<&Expr>,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> AccessPath {
    let w = match where_ {
        Some(w) => w,
        None => return AccessPath::SeqScan,
    };
    // Gather per-column bounds from the AND-conjuncts, remembering which
    // conjunct each bound came from (v1.08: the winner's used set becomes
    // the Index Cond; the rest stay residual Filters).
    let mut col_bounds: HashMap<usize, Vec<(IndexBoundKind, Value, usize)>> = HashMap::new();
    for (ci, c) in split_conjuncts(w).iter().enumerate() {
        if let Some(bs) = conjunct_bounds(c, qual, table_name, &t.columns) {
            for (pos, kind, v) in bs {
                col_bounds.entry(pos).or_default().push((kind, v, ci));
            }
        }
    }
    if col_bounds.is_empty() {
        return AccessPath::SeqScan;
    }
    // Prefer the longest equality prefix; a point lookup beats a range,
    // and more bound columns beat fewer.
    let mut best: Option<AccessPath> = None;
    let mut best_score = (0usize, 0usize, false);
    for ix in db.visible_indexes_for(table_name, snap, &[own], session) {
        // v0.88: expression / partial indexes are catalog-only (never
        // built); no scan path may consult them.
        if !ix.def.planner_usable {
            continue;
        }
        let mut prefix: Vec<Value> = Vec::new();
        // v1.08: (conjunct index, text); sorted by source order for PG.
        let mut cond_parts: Vec<(usize, String)> = Vec::new();
        // v1.08: conjunct indices consumed by this candidate's cond.
        let mut used: Vec<usize> = Vec::new();
        // v1.08: render one bound like PG19's indexqual deparse
        // `(col op const)`. Values with no faithful PG spelling fall back
        // to the legacy bare text (wrong, EXPECTED-FAIL via mask) — the
        // access path itself (prefix/lo/hi) is never affected by display.
        let bound_text = |col_name: &str, pos: usize, op: &str, v: &Value| -> String {
            let (_, ctype) = &t.columns[pos];
            match pg_value_text(v, *ctype) {
                Some(val) => format!("({col_name} {op} {val})"),
                None => format!("({col_name} {op} {})", value_to_text_cast(v)),
            }
        };
        let mut i = 0;
        while i < ix.def.cols.len() {
            let cp = ix.def.cols[i];
            let eq_val = col_bounds
                .get(&cp)
                .and_then(|bs| bs.iter().find(|(k, _, _)| matches!(k, IndexBoundKind::Eq)));
            match eq_val {
                Some((_, v, ci)) => {
                    let t = bound_text(&ix.def.col_names[i], cp, "=", v);
                    cond_parts.push((*ci, t));
                    used.push(*ci);
                    prefix.push(v.clone());
                    i += 1;
                }
                None => break,
            }
        }
        // Optional range on the next column — or on the leading column
        // when there is no equality prefix at all. When several bounds
        // exist for one side the first wins — a wider range is still a
        // correct superset.
        let (mut lo, mut hi): (Option<(Value, bool)>, Option<(Value, bool)>) = (None, None);
        if i < ix.def.cols.len() {
            if let Some(bs) = col_bounds.get(&ix.def.cols[i]) {
                for (k, v, ci) in bs {
                    match k {
                        IndexBoundKind::Gt(incl) if lo.is_none() => {
                            let op = if *incl { ">=" } else { ">" };
                            let t = bound_text(&ix.def.col_names[i], ix.def.cols[i], op, v);
                            cond_parts.push((*ci, t));
                            used.push(*ci);
                            lo = Some((v.clone(), *incl));
                        }
                        IndexBoundKind::Lt(incl) if hi.is_none() => {
                            let op = if *incl { "<=" } else { "<" };
                            let t = bound_text(&ix.def.col_names[i], ix.def.cols[i], op, v);
                            cond_parts.push((*ci, t));
                            used.push(*ci);
                            hi = Some((v.clone(), *incl));
                        }
                        _ => {}
                    }
                }
            }
        }
        if i == 0 && lo.is_none() && hi.is_none() {
            continue; // leading column unbound: index not usable
        }
        let is_point = i == ix.def.cols.len() && lo.is_none() && hi.is_none();
        let bound_cols = i + usize::from(lo.is_some() || hi.is_some());
        let score = (i, bound_cols, is_point);
        if score > best_score {
            best_score = score;
            used.sort_unstable();
            used.dedup();
            best = Some(AccessPath::IndexScan {
                index: ix.def.name.clone(),
                prefix,
                lo,
                hi,
                // PG parenthesizes each index qual; a multi-qual cond
                // wraps the AND list in one more pair, in source order.
                cond: {
                    // v1.08: `split_conjuncts` returns reversed source order,
                    // so sort `ci` descending to restore source order.
                    cond_parts.sort_by_key(|(ci, _)| std::cmp::Reverse(*ci));
                    let texts: Vec<&str> = cond_parts.iter().map(|(_, t)| t.as_str()).collect();
                    if texts.len() == 1 {
                        texts[0].to_string()
                    } else {
                        format!("({})", texts.join(" AND "))
                    }
                },
                used,
            });
        }
    }
    // v1.62: PG19 cost-model parity for tiny tables. PG19's `cost_index`
    // (costsize.c) can never beat `cost_seqscan` on a single-page
    // relation: the index path's I/O floor is one `random_page_cost`
    // (4x `seq_page_cost`) plus the index descent, while the seq scan
    // reads the one page at `seq_page_cost` — and `index_pages_fetched`
    // returns >= 1 for any positive fetch count on a 1-page table, so
    // the inequality holds for every selectivity and correlation.
    // PG therefore always Seq-Scans 1-page relations even when a usable
    // index exists (e.g. join.out's `sj`, 4 rows on 1 page). The scan
    // choice never changes results — both scans return the same rows
    // and the full predicate is always re-applied — only the plan.
    match best {
        Some(AccessPath::IndexScan { .. }) if est_heap_pages(t) <= 1 => AccessPath::SeqScan,
        Some(path) => path,
        None => AccessPath::SeqScan,
    }
}

/// Row-version ids from `ix` matching an equality `prefix` plus an
/// optional range on the next key component, in ascending key order.
///
/// The scan starts at the short prefix key: every key with this prefix
/// sorts after it and before any key with a greater prefix (lexicographic
/// order), so the matching keys form the contiguous run up to the first
/// prefix break. Within the run the range component is monotonic, so the
/// high bound stops the scan and the low bound only skips the head.
pub(crate) fn index_scan_ids(
    ix: &Index,
    prefix: &[Value],
    lo: Option<(&Value, bool)>,
    hi: Option<(&Value, bool)>,
) -> Vec<u64> {
    let start = IndexKey(
        prefix
            .iter()
            .cloned()
            .chain(lo.map(|(v, _)| (*v).clone()))
            .collect(),
    );
    let mut out = Vec::new();
    let p = prefix.len();
    let has_range = lo.is_some() || hi.is_some();
    for (k, ids) in ix.tree.range((Bound::Included(&start), Bound::Unbounded)) {
        if k.0.len() < p
            || k.0
                .iter()
                .zip(prefix.iter())
                // Prefix equality in *index* ordering (Numeric 5 = 5.0).
                .any(|(a, b)| index_key_cmp(a, b) != Ordering::Equal)
        {
            break;
        }
        if has_range {
            // A range always sits on column p < ncols here (a full-prefix
            // point lookup never takes this path).
            let c = &k.0[p];
            if let Some((lv, incl)) = lo {
                match index_key_cmp(c, lv) {
                    Ordering::Less => continue,
                    Ordering::Equal if !incl => continue,
                    _ => {}
                }
            }
            if let Some((hv, incl)) = hi {
                match index_key_cmp(c, hv) {
                    Ordering::Greater => break,
                    Ordering::Equal if !incl => break,
                    _ => {}
                }
            }
        }
        out.extend(ids.iter().copied());
    }
    out
}

/// Hint for an index-ordered scan: `SELECT ... FROM t ORDER BY <index cols>`
/// with no WHERE — rows stream out of the index in ORDER BY order and the
/// sort step is skipped.
#[derive(Clone, Debug)]
pub(crate) struct OrderHint {
    pub(crate) index: String,
    pub(crate) desc: bool,
}

/// ORDER BY term rendering for EXPLAIN.
/// v1.03: format a sort key as PG does (`table.column`). When the
/// expression is an unqualified column and `qual` gives the single
/// source table name, qualify it (PG resolves ORDER BY columns to
/// their table).
pub(crate) fn order_term_text_qualified(t: &OrderTerm, qual: Option<&str>) -> String {
    let e = match &t.expr {
        Expr::Column { table, name } => match table {
            Some(tbl) => format!("{}.{}", tbl, name),
            None => match qual {
                Some(q) => format!("{}.{}", q, name),
                None => name.clone(),
            },
        },
        other => format!("{:?}", other),
    };
    if t.desc { format!("{} DESC", e) } else { e }
}

pub(crate) fn plan_order_scan(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    stmt: &SelectStmt,
) -> Option<OrderHint> {
    // Only the simplest shape: single base table, no filter, no
    // aggregation / DISTINCT / grouping. v0.52: DISTINCT ON bails too —
    // its first-row-per-group filter removes rows, which would break the
    // early-LIMIT truncation below (same reason as plain DISTINCT).
    if stmt.from.len() != 1
        || stmt.where_.is_some()
        || stmt.distinct
        || !stmt.distinct_on.is_empty()
    {
        return None;
    }
    if is_agg_query(stmt) || !stmt.group_by.is_empty() || stmt.having.is_some() {
        return None;
    }
    if stmt.order_by.is_empty() {
        return None;
    }
    let (table_name, qual) = match &stmt.from[0] {
        FromItem::Table { name, alias, .. } => {
            (name.as_str(), alias.clone().unwrap_or_else(|| name.clone()))
        }
        FromItem::Derived { .. }
        | FromItem::Join { .. }
        | FromItem::Values { .. }
        | FromItem::Function { .. } => return None,
    };
    let t = eng.db.find_table(table_name, snap, &[own], session)?;
    // ORDER BY alias safety: an unqualified ORDER BY column that matches a
    // select-list output name resolves to the *output* value (which may be
    // an expression), not the raw column — unless the select item is the
    // identical expression. Qualified terms always read the column.
    for term in &stmt.order_by {
        if let Expr::Column { table: None, name } = &term.expr {
            for item in &stmt.items {
                match item {
                    SelectItem::Expr { expr: e, alias } => {
                        let out_name = alias.clone().unwrap_or_else(|| expr_col_name(e));
                        if out_name == *name && e != &term.expr {
                            return None;
                        }
                    }
                    SelectItem::All | SelectItem::AllOf(_) => {}
                }
            }
        }
    }
    // Every term must be a plain column of this table, all in the same
    // direction, with default NULL placement (NULLs sort high in the
    // index: NULLS LAST for ASC, NULLS FIRST for DESC — Postgres'
    // defaults, matching compare_values).
    let desc = stmt.order_by[0].desc;
    let mut cols: Vec<String> = Vec::new();
    for term in &stmt.order_by {
        if term.desc != desc {
            return None;
        }
        if term.nulls_first.unwrap_or(desc) != desc {
            return None;
        }
        match &term.expr {
            Expr::Column { table, name } => {
                if let Some(q) = table {
                    if q != &qual && q != table_name {
                        return None;
                    }
                }
                if !t.columns.iter().any(|(n, _)| n == name) {
                    return None;
                }
                cols.push(name.clone());
            }
            _ => return None,
        }
    }
    // An index whose columns are exactly this sequence (indexes are
    // ascending; a DESC query scans it backwards).
    // An index whose leading columns are exactly this sequence satisfies
    // the ordering (indexes are ascending; a DESC query scans backwards).
    // A composite index also satisfies a prefix: (a,b) order implies
    // a-order.
    // v0.88: only all-ascending, default-null-placement, planner-usable
    // indexes are trusted here. A DESC (or otherwise directed) index is
    // built in canonical ascending order with the direction stored as
    // catalog metadata, so streaming it as if it were ordered would
    // produce wrong row order; those queries fall back to Sort (which is
    // what happened before v0.88 accepted the DDL at all).
    for ix in eng
        .db
        .visible_indexes_for(table_name, snap, &[own], session)
    {
        if !ix.def.planner_usable
            || ix.def.desc.iter().any(|d| *d)
            || ix.def.nulls_first.iter().any(|n| *n)
        {
            continue;
        }
        if ix.def.col_names.len() >= cols.len() && ix.def.col_names[..cols.len()] == cols[..] {
            return Some(OrderHint {
                index: ix.def.name.clone(),
                desc,
            });
        }
    }
    None
}

/// Rows of one base table in index key order (for ORDER BY ... LIMIT with
/// no WHERE). NULL placement matches the ORDER BY defaults: ascending
/// scans yield NULLS LAST, descending scans NULLS FIRST. Stops after
/// `limit` *visible* rows when set — the caller guarantees no residual
/// filter, DISTINCT, or aggregation can remove rows afterwards.
pub(crate) fn index_order_rows(
    ix: &Index,
    t: &Table,
    table_name: &str,
    // v1.17: range qualifier (alias) for RowProv, so `a.xmin` resolves.
    qual: &str,
    desc: bool,
    snap: &Snapshot,
    own: u64,
    need_prov: bool,
    limit: Option<usize>,
) -> Vec<QRow> {
    let mut rows = Vec::new();
    // Walk the buckets in index order, resolving each id to its visible
    // heap version, and stop as soon as `limit` visible rows exist.
    let mut iter: Box<dyn Iterator<Item = (&IndexKey, &Vec<u64>)>> = if desc {
        Box::new(ix.tree.iter().rev())
    } else {
        Box::new(ix.tree.iter())
    };
    'scan: for (_, ids) in iter.by_ref() {
        for &id in ids {
            if let Some(n) = limit {
                if rows.len() >= n {
                    break 'scan;
                }
            }
            if let Some(pos) = t.row_pos(id) {
                let r = &t.rows[pos];
                if row_visible(r, snap, &[own]) {
                    rows.push(QRow {
                        cells: r.values.clone(),
                        prov: if need_prov {
                            vec![RowProv {
                                qual: qual.to_string(),
                                table: table_name.to_string(),
                                row_id: r.id,
                            }]
                        } else {
                            Vec::new()
                        },
                    });
                }
            }
        }
    }
    rows
}
