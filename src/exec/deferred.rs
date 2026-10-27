// v2.06: deferred foreign-key checking (PG19 trigger.c's
// after-trigger queue, simplified to a per-session queue in the
// Engine). A DEFERRABLE FK whose current mode is deferred (INITIALLY
// DEFERRED, or SET CONSTRAINTS ... DEFERRED) queues its checks
// instead of running them at the DML statement; the queue fires at
// COMMIT, at the end of an autocommit statement, and on
// SET CONSTRAINTS ... IMMEDIATE.
//
// Fire-time semantics mirror PG's trigger re-execution: the row is
// judged by its CURRENT image. Child entries re-validate by key
// (any visible child row still carrying the queued key must find it
// in the parent). Parent entries carry the referenced row's
// pre-change image; if the key is back in the parent, nothing
// happens, otherwise the referential action is planned then, with
// the parent row's current image as the update target.
use super::*;
use crate::storage::{DeferredFkState, PendingFk};

/// The session's deferred state, cloned out (queue operations never
/// hold the RefCell borrow across a recursive call).
fn state_of(eng: &Engine, session: u64) -> DeferredFkState {
    eng.deferred_fk
        .borrow()
        .get(&session)
        .cloned()
        .unwrap_or_default()
}

fn store_state(eng: &Engine, session: u64, st: DeferredFkState) {
    if st.queue.is_empty() && st.modes.is_empty() {
        eng.deferred_fk.borrow_mut().remove(&session);
    } else {
        eng.deferred_fk.borrow_mut().insert(session, st);
    }
}

/// Is this FK currently checked deferred for the session? A
/// non-deferrable or NOT ENFORCED FK is never deferred; an FK being
/// fired right now is forced immediate (its own cascade planning
/// must not re-queue itself).
pub(crate) fn fk_mode_deferred(eng: &Engine, session: u64, child: &str, fk: &FkDef) -> bool {
    if !fk.deferrable || !fk.enforced {
        return false;
    }
    let st = eng.deferred_fk.borrow();
    let Some(st) = st.get(&session) else {
        return fk.initially_deferred;
    };
    if st.firing.contains(&(child.to_string(), fk.name.clone())) {
        return false;
    }
    st.modes
        .get(&(child.to_string(), fk.name.clone()))
        .copied()
        .unwrap_or(fk.initially_deferred)
}

pub(crate) fn queue_child_fk(
    eng: &Engine,
    session: u64,
    child: &str,
    fk_name: &str,
    key: Vec<Value>,
) {
    let mut map = eng.deferred_fk.borrow_mut();
    map.entry(session)
        .or_default()
        .queue
        .push(PendingFk::Child {
            child: child.to_string(),
            fk_name: fk_name.to_string(),
            key,
        });
}

/// v2.07: the UNIQUE/PK definition named `con_name` on table `t`.
pub(crate) fn unique_def_of(t: &Table, con_name: &str) -> Option<UniqueDef> {
    if let Some(pk) = &t.pkey {
        if pk.name == con_name {
            return Some(pk.clone());
        }
    }
    t.uniques.iter().find(|u| u.name == con_name).cloned()
}

/// v2.07: the topmost partition ancestor of `table` that carries the
/// unique/PK constraint `con_name` (the table itself when it is not a
/// partition child, or when an ancestor does not carry the name).
/// Deferred revalidation is queued against — and scans from — this
/// root, mirroring PG's partitioned unique index spanning the tree.
pub(crate) fn unique_constraint_root(
    eng: &Engine,
    snap: &Snapshot,
    owns: &[u64],
    session: u64,
    table: &str,
    con_name: &str,
) -> String {
    let mut cur = table.to_string();
    loop {
        let Some(t) = eng.db.find_table(&cur, snap, owns, session) else {
            return cur;
        };
        let Some(parent) = t.partition.as_ref().and_then(|p| p.parent.clone()) else {
            return cur;
        };
        let Some(pt) = eng.db.find_table(&parent, snap, owns, session) else {
            return cur;
        };
        if unique_def_of(pt, con_name).is_none() {
            return cur;
        }
        cur = parent;
    }
}

/// v2.07: is this UNIQUE/PK currently checked deferred for the
/// session? Mirrors fk_mode_deferred; the SET CONSTRAINTS mode lookup
/// also walks partition ancestors, because the mode is recorded
/// against the table the user named (usually the root) while DML
/// lands on leaves.
pub(crate) fn unique_mode_deferred(
    eng: &Engine,
    snap: &Snapshot,
    owns: &[u64],
    session: u64,
    table: &str,
    u: &UniqueDef,
) -> bool {
    if !u.deferrable {
        return false;
    }
    let st = eng.deferred_fk.borrow();
    let Some(st) = st.get(&session) else {
        return u.initially_deferred;
    };
    if st.firing.contains(&(table.to_string(), u.name.clone())) {
        return false;
    }
    // Mode overrides: this table first, then each partition ancestor.
    let mut cur = table.to_string();
    loop {
        if let Some(m) = st.modes.get(&(cur.clone(), u.name.clone())) {
            return *m;
        }
        let parent = eng
            .db
            .find_table(&cur, snap, owns, session)
            .and_then(|t| t.partition.as_ref().and_then(|p| p.parent.clone()));
        match parent {
            Some(p) => cur = p,
            None => return u.initially_deferred,
        }
    }
}

/// v2.07: queue a deferred UNIQUE/PK revalidation (deduplicated: one
/// entry per (root, constraint) is enough — firing rescans the tree).
pub(crate) fn queue_unique(eng: &Engine, session: u64, root: &str, con_name: &str) {
    let mut map = eng.deferred_fk.borrow_mut();
    let st = map.entry(session).or_default();
    let exists = st.queue.iter().any(|e| {
        matches!(e, PendingFk::Unique { table, con_name: c }
            if table == root && c == con_name)
    });
    if !exists {
        st.queue.push(PendingFk::Unique {
            table: root.to_string(),
            con_name: con_name.to_string(),
        });
    }
}

/// v2.07: verdict for one violated unique index on `table`.
pub(crate) enum UniqueVerdict {
    /// The backing constraint is currently deferred: the violation
    /// was queued for commit-time revalidation.
    Queued,
    /// Immediate violation; the payload is the name PG names in the
    /// 23505 message (the constraint name, or the index name for a
    /// standalone unique index).
    Immediate(String),
}

/// v2.07: classify one violated unique index name on `table`: map
/// the backing index to its constraint definition and, when that
/// constraint is currently deferred, queue the revalidation.
pub(crate) fn unique_verdict(
    eng: &Engine,
    snap: &Snapshot,
    owns: &[u64],
    session: u64,
    table: &str,
    index_name: &str,
) -> UniqueVerdict {
    let Some(t) = eng.db.find_table(table, snap, owns, session) else {
        return UniqueVerdict::Immediate(index_name.to_string());
    };
    let is_child = t.partition.as_ref().is_some_and(|p| p.parent.is_some());
    let mut all: Vec<&UniqueDef> = t.uniques.iter().collect();
    if let Some(pk) = &t.pkey {
        all.push(pk);
    }
    let u = all.into_iter().find(|u| {
        u.name == index_name || constraint_index_name(table, &u.name, is_child) == index_name
    });
    match u {
        Some(u) if unique_mode_deferred(eng, snap, owns, session, table, u) => {
            let root = unique_constraint_root(eng, snap, owns, session, table, &u.name);
            queue_unique(eng, session, &root, &u.name);
            UniqueVerdict::Queued
        }
        Some(u) => UniqueVerdict::Immediate(u.name.clone()),
        None => UniqueVerdict::Immediate(index_name.to_string()),
    }
}

/// v2.07: decide a set of violated unique index names (from
/// unique_violations_all) for one DML row on `table`: violations on
/// currently-deferred constraints are QUEUED for commit-time
/// revalidation; the first violation on an immediate constraint (or
/// on a standalone unique index, which has no constraint to defer)
/// errors 23505 naming the constraint, like PG.
pub(crate) fn resolve_unique_violations(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    violated: Vec<String>,
) -> Result<(), ExecError> {
    for index_name in violated {
        if let UniqueVerdict::Immediate(name) = unique_verdict(
            eng,
            ctx.snap,
            &ctx.all_xids,
            ctx.session,
            table,
            &index_name,
        ) {
            return Err(unique_violation_err(&name));
        }
    }
    Ok(())
}

/// v2.07: fire one queued deferred UNIQUE/PK revalidation: scan the
/// root table and every partition descendant, group visible rows by
/// the constraint key (IndexKey semantics, so NUMERIC 1 == INT 1),
/// and fail 23505 on any surviving duplicate. NULL keys never
/// conflict unless the constraint is NULLS NOT DISTINCT (v2.08).
/// Rows are remapped to the root's column order by name.
fn fire_unique(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    root: &str,
    con_name: &str,
) -> Result<(), ExecError> {
    let Some(rt) = eng
        .db
        .find_table(root, ctx.snap, &ctx.all_xids, ctx.session)
        .cloned()
    else {
        return Ok(()); // table dropped inside the transaction
    };
    let Some(u) = unique_def_of(&rt, con_name) else {
        return Ok(()); // constraint dropped inside the transaction
    };
    let root_cols: Vec<(String, ColType)> = rt.columns.clone();
    let key_pos: Vec<usize> = u
        .cols
        .iter()
        .map(|c| {
            root_cols
                .iter()
                .position(|(n, _)| n == c)
                .expect("unique columns validated at DDL")
        })
        .collect();
    // Root + all descendants (collect_partition_leaves includes the
    // root itself when it is not partitioned).
    let mut tables = collect_partition_leaves(&eng.db, root, ctx.snap, ctx.own, ctx.session);
    if !tables.iter().any(|t| t == root) {
        tables.push(root.to_string());
    }
    let mut seen: std::collections::BTreeSet<IndexKey> = std::collections::BTreeSet::new();
    for tname in &tables {
        let Some(t) = eng
            .db
            .find_table(tname, ctx.snap, &ctx.all_xids, ctx.session)
            .cloned()
        else {
            continue;
        };
        for r in t
            .rows
            .iter()
            .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
        {
            let vals = remap_parent_to_leaf(&t.columns, &root_cols, &r.values);
            let key = temp_key(&key_pos, &vals);
            if !u.nulls_not_distinct && key.0.iter().any(|v| matches!(v, Value::Null)) {
                continue; // NULLs never conflict (NULLS DISTINCT)
            }
            if !seen.insert(key) {
                return Err(unique_violation_err(con_name));
            }
        }
    }
    Ok(())
}

pub(crate) fn queue_parent_fk(
    eng: &Engine,
    session: u64,
    parent: &str,
    child: &str,
    fk_name: &str,
    row_id: u64,
    old_values: Row,
) {
    let mut map = eng.deferred_fk.borrow_mut();
    map.entry(session)
        .or_default()
        .queue
        .push(PendingFk::Parent {
            parent: parent.to_string(),
            child: child.to_string(),
            fk_name: fk_name.to_string(),
            row_id,
            old_values,
        });
}

/// Server savepoint support: (queue length, modes) snapshot.
pub(crate) fn deferred_mark(
    eng: &Engine,
    session: u64,
) -> (usize, HashMap<(String, String), bool>) {
    let st = state_of(eng, session);
    (st.queue.len(), st.modes)
}

/// Server savepoint support: truncate the queue and restore modes.
pub(crate) fn deferred_restore(
    eng: &Engine,
    session: u64,
    mark: &(usize, HashMap<(String, String), bool>),
) {
    let mut st = state_of(eng, session);
    st.queue.truncate(mark.0);
    st.modes = mark.1.clone();
    store_state(eng, session, st);
}

pub(crate) fn deferred_clear(eng: &Engine, session: u64) {
    eng.deferred_fk.borrow_mut().remove(&session);
}

/// Does a visible parent row carry `key` in the FK's ref columns?
/// Delegates to the shared scan, which also searches the referenced
/// table's partition leaves (a partitioned parent's own row store is
/// always empty — the pre-fix copy here missed every leaf key and
/// failed deferred COMMITs against partitioned parents, fkpart9).
fn parent_has_key(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    fk: &FkDef,
    key: &[Value],
) -> Result<bool, ExecError> {
    fk_parent_key_exists(eng, snap, own, session, fk, key, None)
}

fn fire_child(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    child: &str,
    fk_name: &str,
    key: &[Value],
) -> Result<(), ExecError> {
    let Some(t) = eng
        .db
        .find_table(child, ctx.snap, &ctx.all_xids, ctx.session)
        .cloned()
    else {
        return Ok(()); // child dropped inside the transaction
    };
    let Some(fk) = t.fks.iter().find(|f| f.name == fk_name).cloned() else {
        return Ok(()); // constraint dropped inside the transaction
    };
    if !fk.enforced {
        return Ok(());
    }
    let meta = TableMeta::of(&t);
    let child_pos: Vec<usize> = fk
        .cols
        .iter()
        .map(|c| meta.column_index(c).expect("fk columns validated at DDL"))
        .collect();
    // Any visible child row still carrying the queued key?
    let present = t
        .rows
        .iter()
        .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
        .any(|r| {
            child_pos
                .iter()
                .zip(key.iter())
                .all(|(&ci, kv)| &r.values[ci] == kv || fk_key_eq(&r.values[ci], kv))
        });
    if !present {
        return Ok(());
    }
    if key.iter().any(|v| matches!(v, Value::Null)) {
        // MATCH FULL: a partially-NULL key violates; SIMPLE/PARTIAL
        // keys with any NULL never violate (they are not queued).
        if fk.match_full && !key.iter().all(|v| matches!(v, Value::Null)) {
            return Err(exec_err(
                "23503",
                format!(
                    "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                    child, fk.name
                ),
            ));
        }
        return Ok(());
    }
    if !parent_has_key(eng, ctx.snap, ctx.own, ctx.session, &fk, key)? {
        return Err(exec_err(
            "23503",
            format!(
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                child, fk.name
            ),
        ));
    }
    Ok(())
}

fn fire_parent(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    parent: &str,
    child: &str,
    fk_name: &str,
    row_id: u64,
    old_values: &Row,
) -> Result<(), ExecError> {
    let Some(pt) = eng
        .db
        .find_table(parent, ctx.snap, &ctx.all_xids, ctx.session)
        .cloned()
    else {
        return Ok(());
    };
    let Some(ct) = eng
        .db
        .find_table(child, ctx.snap, &ctx.all_xids, ctx.session)
        .cloned()
    else {
        return Ok(());
    };
    let Some(fk) = ct.fks.iter().find(|f| f.name == fk_name).cloned() else {
        return Ok(());
    };
    if !fk.enforced {
        return Ok(());
    }
    let parent_meta = TableMeta::of(&pt);
    let ref_cols = fk_ref_cols(&parent_meta, &fk)?;
    let parent_pos: Vec<usize> = ref_cols
        .iter()
        .map(|c| pt.column_index(c).expect("fk ref cols validated at DDL"))
        .collect();
    let old_key: Vec<Value> = parent_pos.iter().map(|&i| old_values[i].clone()).collect();
    // Key restored (or never actually removed)? Nothing to do. The
    // search goes through the referenced table's whole partition
    // tree (parent_has_key), not just this leaf's rows.
    if parent_has_key(eng, ctx.snap, ctx.own, ctx.session, &fk, &old_key)? {
        return Ok(());
    }
    // The referenced row's current image: still present (an UPDATE
    // moved the key) or gone (a DELETE).
    let new_values: Option<Row> = pt
        .rows
        .iter()
        .find(|r| r.id == row_id && row_visible(r, ctx.snap, &ctx.all_xids))
        .map(|r| r.values.clone());
    // Force this FK immediate while its action is planned, so the
    // planner does not re-queue the very entry being fired.
    {
        let mut map = eng.deferred_fk.borrow_mut();
        map.entry(ctx.session)
            .or_default()
            .firing
            .insert((child.to_string(), fk_name.to_string()));
    }
    let mut out = FkCascade::default();
    let planned = plan_fk_cascade(
        eng,
        ctx.snap,
        ctx.own,
        ctx.session,
        ctx.role,
        ctx.level,
        parent,
        &parent_meta,
        &[(row_id, old_values.clone(), new_values)],
        0,
        &mut out,
    );
    {
        let mut map = eng.deferred_fk.borrow_mut();
        if let Some(st) = map.get_mut(&ctx.session) {
            st.firing.remove(&(child.to_string(), fk_name.to_string()));
        }
    }
    planned?;
    apply_fk_cascade(eng, ctx, out)
}

/// Fire queued deferred checks. `only == None` fires everything;
/// otherwise only the named (child, constraint) pairs. Firing can
/// queue fresh entries (a deferred cascade child), so loop until
/// the selected set is drained; the round cap mirrors the cascade
/// depth guard's spirit.
pub(crate) fn flush_deferred_fk(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    only: Option<&HashSet<(String, String)>>,
) -> Result<(), ExecError> {
    for _round in 0..64 {
        let batch: Vec<PendingFk> = {
            let mut map = eng.deferred_fk.borrow_mut();
            let Some(st) = map.get_mut(&ctx.session) else {
                return Ok(());
            };
            let mut taken = Vec::new();
            let mut kept = Vec::new();
            for e in std::mem::take(&mut st.queue) {
                let hit = match (&e, only) {
                    (_, None) => true,
                    (PendingFk::Child { child, fk_name, .. }, Some(set)) => {
                        set.contains(&(child.clone(), fk_name.clone()))
                    }
                    (PendingFk::Parent { child, fk_name, .. }, Some(set)) => {
                        set.contains(&(child.clone(), fk_name.clone()))
                    }
                    (PendingFk::Unique { table, con_name }, Some(set)) => {
                        set.contains(&(table.clone(), con_name.clone()))
                    }
                };
                if hit {
                    taken.push(e);
                } else {
                    kept.push(e);
                }
            }
            st.queue = kept;
            taken
        };
        if batch.is_empty() {
            return Ok(());
        }
        for e in batch {
            match e {
                PendingFk::Child {
                    child,
                    fk_name,
                    key,
                } => {
                    fire_child(eng, ctx, &child, &fk_name, &key)?;
                }
                PendingFk::Parent {
                    parent,
                    child,
                    fk_name,
                    row_id,
                    old_values,
                } => {
                    fire_parent(eng, ctx, &parent, &child, &fk_name, row_id, &old_values)?;
                }
                PendingFk::Unique { table, con_name } => {
                    fire_unique(eng, ctx, &table, &con_name)?;
                }
            }
        }
    }
    Err(exec_err(
        "54000",
        "deferred foreign key queue did not drain".to_string(),
    ))
}

/// v2.07: one resolved constraint in the SET CONSTRAINTS universe.
enum ConRes {
    Fk(FkDef),
    Unique(UniqueDef),
    /// CHECK / NOT NULL constraints: they resolve (so an unknown-name
    /// 42704 is not raised) but can never be deferrable (55000).
    Other,
}

impl ConRes {
    fn deferrable(&self) -> bool {
        match self {
            ConRes::Fk(f) => f.deferrable,
            ConRes::Unique(u) => u.deferrable,
            ConRes::Other => false,
        }
    }
    fn name(&self) -> &str {
        match self {
            ConRes::Fk(f) => &f.name,
            ConRes::Unique(u) => &u.name,
            ConRes::Other => "",
        }
    }
}

/// Every constraint visible to the session, permanent and temp —
/// the SET CONSTRAINTS resolution universe. FKs and (v2.07) UNIQUE /
/// PK constraints carry their real deferrable flags; other kinds
/// resolve so the not-deferrable error is PG's 55000, not 42704.
fn all_constraints(eng: &Engine, ctx: &StmtCtx) -> Vec<(String, String, ConRes)> {
    let mut out = Vec::new();
    let mut push_table = |name: &str, t: &Table| {
        for fk in &t.fks {
            out.push((name.to_string(), fk.name.clone(), ConRes::Fk(fk.clone())));
        }
        for c in &t.checks {
            out.push((name.to_string(), c.name.clone(), ConRes::Other));
        }
        for u in &t.uniques {
            out.push((name.to_string(), u.name.clone(), ConRes::Unique(u.clone())));
        }
        if let Some(p) = &t.pkey {
            out.push((name.to_string(), p.name.clone(), ConRes::Unique(p.clone())));
        }
    };
    for (name, versions) in &eng.db.tables {
        if let Some(t) = versions
            .iter()
            .find(|t| crate::storage::table_visible(t, ctx.snap, &ctx.all_xids))
        {
            push_table(name, t);
        }
    }
    if let Some(tmps) = eng.db.temp_tables.get(&ctx.session) {
        for (name, t) in tmps {
            push_table(name, t);
        }
    }
    out
}

/// SET CONSTRAINTS {ALL | name [, ...]} {DEFERRED | IMMEDIATE}
/// (PG19 trigger.c AfterTriggerSetState): unknown name 42704,
/// non-deferrable 55000. IMMEDIATE fires the selected queue entries
/// now; DEFERRED only flips modes for the rest of the transaction.
pub(crate) fn exec_set_constraints(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &Option<Vec<String>>,
    deferred: bool,
) -> Result<ExecResult, ExecError> {
    let universe = all_constraints(eng, ctx);
    let mut targets: Vec<(String, String)> = Vec::new();
    match names {
        None => {
            for (t, _, res) in &universe {
                if res.deferrable() {
                    targets.push((t.clone(), res.name().to_string()));
                }
            }
        }
        Some(list) => {
            for raw in list {
                let (qual, base) = match raw.rsplit_once('.') {
                    Some((q, b)) => (Some(q), b),
                    None => (None, raw.as_str()),
                };
                let matches: Vec<&(String, String, ConRes)> = universe
                    .iter()
                    .filter(|(t, cname, _)| {
                        cname == base
                            && qual.is_none_or(|q| {
                                t.rsplit_once('.').is_some_and(|(s, _)| s == q) || t == q
                            })
                    })
                    .collect();
                if matches.is_empty() {
                    return Err(exec_err(
                        "42704",
                        format!("constraint \"{}\" does not exist", base),
                    ));
                }
                if !matches.iter().any(|(_, _, res)| res.deferrable()) {
                    return Err(exec_err(
                        "55000",
                        format!("constraint \"{}\" is not deferrable", base),
                    ));
                }
                for (t, _, res) in matches {
                    if res.deferrable() {
                        targets.push((t.clone(), res.name().to_string()));
                    }
                }
            }
        }
    }
    {
        let mut map = eng.deferred_fk.borrow_mut();
        let st = map.entry(ctx.session).or_default();
        for k in &targets {
            st.modes.insert(k.clone(), deferred);
        }
    }
    if !deferred {
        let set: HashSet<(String, String)> = targets.into_iter().collect();
        flush_deferred_fk(eng, ctx, Some(&set))?;
    }
    Ok(ExecResult::Command {
        tag: "SET CONSTRAINTS".to_string(),
    })
}
