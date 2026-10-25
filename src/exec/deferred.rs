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
            }
        }
    }
    Err(exec_err(
        "54000",
        "deferred foreign key queue did not drain".to_string(),
    ))
}

/// Every constraint visible to the session, permanent and temp —
/// the SET CONSTRAINTS resolution universe. The bool is "is an FK"
/// (only FKs can be deferrable in this engine; other kinds resolve
/// so the not-deferrable error is PG's 55000, not 42704).
fn all_constraints(eng: &Engine, ctx: &StmtCtx) -> Vec<(String, String, Option<FkDef>)> {
    let mut out = Vec::new();
    let mut push_table = |name: &str, t: &Table| {
        for fk in &t.fks {
            out.push((name.to_string(), fk.name.clone(), Some(fk.clone())));
        }
        for c in &t.checks {
            out.push((name.to_string(), c.name.clone(), None));
        }
        for u in &t.uniques {
            out.push((name.to_string(), u.name.clone(), None));
        }
        if let Some(p) = &t.pkey {
            out.push((name.to_string(), p.name.clone(), None));
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
            for (t, _, fk) in &universe {
                if fk.as_ref().is_some_and(|f| f.deferrable) {
                    targets.push((t.clone(), fk.as_ref().unwrap().name.clone()));
                }
            }
        }
        Some(list) => {
            for raw in list {
                let (qual, base) = match raw.rsplit_once('.') {
                    Some((q, b)) => (Some(q), b),
                    None => (None, raw.as_str()),
                };
                let matches: Vec<&(String, String, Option<FkDef>)> = universe
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
                if !matches
                    .iter()
                    .any(|(_, _, fk)| fk.as_ref().is_some_and(|f| f.deferrable))
                {
                    return Err(exec_err(
                        "55000",
                        format!("constraint \"{}\" is not deferrable", base),
                    ));
                }
                for (t, _, fk) in matches {
                    if fk.as_ref().is_some_and(|f| f.deferrable) {
                        targets.push((t.clone(), fk.as_ref().unwrap().name.clone()));
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
