// v1.78 mechanical split: moved verbatim from src/exec.rs (60503-60885, 63193-63456).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ============================================================================
// v0.9: sequences.
// ============================================================================

/// Resolve CREATE/ALTER SEQUENCE options to concrete parameters.
/// Postgres defaults: start 1, increment 1, minvalue 1, maxvalue 2^63-1,
/// no cycle — except descending sequences (increment < 0), which default
/// to start -1, minvalue -(2^63), maxvalue -1.
/// v0.99: PG19 init_params semantics. `cur` is the live sequence for
/// ALTER (None for CREATE): unresolved options fall back to it, and
/// `AS` on ALTER resets min/max to the new type's bounds only when the
/// old bounds were the old type's defaults.
pub(crate) fn sequence_params(
    opts: &SequenceOpts,
    cur: Option<&crate::storage::Sequence>,
) -> Result<
    (
        crate::sql::SeqType,
        i64,
        i64,
        i64,
        i64,
        bool,
        i64,
        Option<i64>,
    ),
    ExecError,
> {
    use crate::sql::SeqType;
    let bad = |m: &str| exec_err("22023", m.to_string());
    let increment = opts.increment.or(cur.map(|s| s.increment)).unwrap_or(1);
    if increment == 0 {
        return Err(bad("INCREMENT must not be zero"));
    }
    let descending = increment < 0;
    // AS type: explicit wins; ALTER keeps the current type; CREATE
    // defaults to bigint (PG19 init_params).
    let seq_type = opts
        .seq_type
        .or(cur.map(|s| s.seq_type))
        .unwrap_or(SeqType::BigInt);
    // ALTER ... AS: reset a bound to the new type's bound only when the
    // old bound was the old type's default (PG19 init_params).
    let (reset_min, reset_max) = match (opts.seq_type, cur) {
        (Some(new_t), Some(s)) if new_t != s.seq_type => (
            s.min_value == s.seq_type.min_value(),
            s.max_value == s.seq_type.max_value(),
        ),
        _ => (false, false),
    };
    // v0.99: NO MAXVALUE / NO MINVALUE explicitly reset to the default.
    let reset_max = reset_max || opts.reset_max;
    let reset_min = reset_min || opts.reset_min;
    // MAXVALUE: explicit wins; ALTER keeps the old max unless the type
    // change reset it; otherwise the PG19 default (type max ascending,
    // -1 descending).
    let max_value = match (opts.max_value, cur) {
        (Some(v), _) => v,
        (None, Some(s)) if !reset_max => s.max_value,
        _ => {
            if descending {
                -1
            } else {
                seq_type.max_value()
            }
        }
    };
    // MINVALUE: explicit wins; ALTER keeps the old min unless the type
    // change reset it; otherwise the PG19 default (type min descending,
    // 1 ascending).
    let min_value = match (opts.min_value, cur) {
        (Some(v), _) => v,
        (None, Some(s)) if !reset_min => s.min_value,
        _ => {
            if descending {
                seq_type.min_value()
            } else {
                1
            }
        }
    };
    // PG19: explicit bounds outside the sequence type's range are 22023.
    if max_value < seq_type.min_value() || max_value > seq_type.max_value() {
        return Err(bad(&format!(
            "MAXVALUE ({}) is out of range for sequence data type {}",
            max_value,
            seq_type.pg_name()
        )));
    }
    if min_value < seq_type.min_value() || min_value > seq_type.max_value() {
        return Err(bad(&format!(
            "MINVALUE ({}) is out of range for sequence data type {}",
            min_value,
            seq_type.pg_name()
        )));
    }
    // START: explicit wins; ALTER keeps the old start; CREATE defaults
    // to min (ascending) or max (descending).
    let start = match (opts.start, cur) {
        (Some(v), _) => v,
        (None, Some(s)) => s.start,
        (None, None) => {
            if descending {
                max_value
            } else {
                min_value
            }
        }
    };
    let cycle = opts.cycle.or(cur.map(|s| s.cycle)).unwrap_or(false);
    // v0.98: CACHE (PG19 DefineSequence: 22023 when < 1).
    let cache = opts.cache.or(cur.map(|s| s.cache)).unwrap_or(1);
    if let Some(c) = opts.cache {
        if c < 1 {
            return Err(bad(&format!("CACHE value {} must be greater than zero", c)));
        }
    }
    if min_value >= max_value {
        return Err(bad(&format!(
            "MINVALUE ({}) must be less than MAXVALUE ({})",
            min_value, max_value
        )));
    }
    if start < min_value || start > max_value {
        return Err(bad(&format!(
            "START value ({}) cannot be less than MINVALUE ({}) or greater than MAXVALUE ({})",
            start, min_value, max_value
        )));
    }
    if cur.is_none() {
        if let Some(r) = opts.restart {
            // CREATE SEQUENCE ... RESTART is a Postgres syntax error.
            let _ = r;
            return Err(exec_err(
                "42601",
                "syntax error: RESTART is not allowed in CREATE SEQUENCE".to_string(),
            ));
        }
    }
    let restart = match opts.restart {
        None => None,
        Some(v) if v == SequenceOpts::RESTART_SENTINEL => Some(start),
        Some(v) => {
            if v < min_value || v > max_value {
                return Err(bad("RESTART value out of bounds"));
            }
            Some(v)
        }
    };
    Ok((
        seq_type, start, increment, min_value, max_value, cycle, cache, restart,
    ))
}

/// v0.98: validate `OWNED BY table.col` / `OWNED BY NONE`. Returns the
/// temp-session tag to store in `Sequence.owned_by` (Some(session) when
/// the table resolves to a session temp table, else None — the same
/// shape `create_serial_sequence` writes, which `owned_seqs_of` matches
/// at DROP TABLE).
pub(crate) fn validate_sequence_owned_by(
    eng: &Engine,
    ctx: &StmtCtx,
    owned: &crate::sql::OwnedBySpec,
    seq_owner: &str,
) -> Result<Option<u64>, ExecError> {
    match owned {
        crate::sql::OwnedBySpec::None_ => Ok(None),
        crate::sql::OwnedBySpec::Table { table, column } => {
            let t = eng
                .db
                .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                .ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", table))
                })?;
            if !t.columns.iter().any(|(c, _)| c == column) {
                return Err(exec_err(
                    "42703",
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        column, table
                    ),
                ));
            }
            // v0.98: PG19 requires the owned-by table to have the same
            // owner as the sequence (same schema too — vacuous here: the
            // engine is single-schema `public`).
            if t.owner != seq_owner {
                return Err(exec_err(
                    "42832",
                    "sequence must have same owner and schema as table".to_string(),
                ));
            }
            let is_temp = eng
                .db
                .temp_tables
                .get(&ctx.session)
                .is_some_and(|m| m.contains_key(table.as_str()));
            Ok(if is_temp { Some(ctx.session) } else { None })
        }
    }
}

pub(crate) fn exec_create_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_not_exists: bool,
    opts: &SequenceOpts,
) -> Result<ExecResult, ExecError> {
    if eng.db.find_sequence(name, ctx.snap, ctx.own).is_some() {
        if if_not_exists {
            return Ok(ExecResult::Command {
                tag: "CREATE SEQUENCE".to_string(),
            });
        }
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    let (seq_type, start, increment, min_value, max_value, cycle, cache, _) =
        sequence_params(opts, None)?;
    let mut seq = Sequence::new(
        name.to_string(),
        seq_type,
        start,
        increment,
        min_value,
        max_value,
        cycle,
        cache,
        ctx.own,
    );
    // v0.11: the creating role owns the sequence.
    seq.owner = ctx.role.to_string();
    // v0.98: OWNED BY table.col / OWNED BY NONE. Validate BEFORE inserting
    // so a bad target errors without leaving a leaked sequence behind;
    // the link is set on the new version directly (one insert, one
    // WriteOp::CreateSequence).
    if let Some(owned) = &opts.owned_by {
        let temp_session = validate_sequence_owned_by(eng, ctx, owned, &ctx.role)?;
        seq.owned_by = match owned {
            crate::sql::OwnedBySpec::None_ => None,
            crate::sql::OwnedBySpec::Table { table, column } => {
                Some((table.clone(), column.clone(), temp_session))
            }
        };
    }
    eng.db
        .sequences
        .entry(name.to_string())
        .or_default()
        .push(seq);
    ctx.writes.push(WriteOp::CreateSequence {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: "CREATE SEQUENCE".to_string(),
    })
}

pub(crate) fn exec_alter_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_exists: bool,
    opts: &SequenceOpts,
) -> Result<ExecResult, ExecError> {
    // v0.11: only the owner (or a superuser) may alter a sequence.
    require_seq_owner(eng, ctx, name)?;
    let live = match eng.db.find_sequence(name, ctx.snap, ctx.own) {
        Some(s) => s,
        None if if_exists => {
            // v0.98: ALTER SEQUENCE IF EXISTS on a missing sequence
            // skips (PG19 emits NOTICE "relation ... does not exist,
            // skipping"; the executor has no notice channel, like the
            // CREATE ... IF NOT EXISTS path above).
            return Ok(ExecResult::Command {
                tag: "ALTER SEQUENCE".to_string(),
            });
        }
        None => {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", name),
            ));
        }
    };
    // v0.99: no pre-merge — sequence_params resolves each option
    // against the live sequence itself (needed for ALTER ... AS
    // bound-reset semantics).
    let (seq_type, start, increment, min_value, max_value, cycle, cache, restart) =
        sequence_params(opts, Some(live))?;
    // v0.98: OWNED BY is validated (table/column existence) before any
    // version is created.
    let owned_by_update: Option<Option<(String, String, Option<u64>)>> = match &opts.owned_by {
        None => None,
        Some(owned) => {
            let temp_session = validate_sequence_owned_by(eng, ctx, owned, &live.owner)?;
            Some(match owned {
                crate::sql::OwnedBySpec::None_ => None,
                crate::sql::OwnedBySpec::Table { table, column } => {
                    Some((table.clone(), column.clone(), temp_session))
                }
            })
        }
    };
    let prev = live.clone();
    let versions = eng.db.sequences.get_mut(name).expect("visible above");
    let cur = versions
        .iter_mut()
        .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
        .expect("visible above");
    cur.dropped_xmax = ctx.own;
    let mut next = Sequence::new(
        name.to_string(),
        seq_type,
        start,
        increment,
        min_value,
        max_value,
        cycle,
        cache,
        ctx.own,
    );
    // ALTER SEQUENCE changes parameters, not the position: carry the
    // current value and is_called forward (Postgres behavior).
    next.current = prev.current;
    next.is_called = prev.is_called;
    // v0.11: ALTER SEQUENCE preserves owner and grants.
    next.owner = prev.owner.clone();
    next.acl = prev.acl.clone();
    // v0.65: ALTER SEQUENCE preserves serial ownership; v0.98: an
    // explicit OWNED BY clause replaces it.
    next.owned_by = match owned_by_update {
        Some(link) => link,
        None => prev.owned_by.clone(),
    };
    if let Some(r) = restart {
        // v0.98: PostgreSQL validates RESTART against the (possibly new)
        // [min_value, max_value] with SQLSTATE 22023.
        if r < next.min_value {
            return Err(exec_err(
                "22023",
                format!(
                    "RESTART value ({}) cannot be less than MINVALUE ({})",
                    r, next.min_value
                ),
            ));
        }
        if r > next.max_value {
            return Err(exec_err(
                "22023",
                format!(
                    "RESTART value ({}) cannot be greater than MAXVALUE ({})",
                    r, next.max_value
                ),
            ));
        }
        // v0.66: RESTART is setval(r, false): the next nextval RETURNS r
        // (PG19 ALTER SEQUENCE docs: "equivalent to calling the setval
        // function with is_called = false").
        next.current = Some(r);
        next.is_called = false;
        // A RESTART counts as an advance for WAL purposes.
        if !eng.seq_advanced.contains(&name.to_string()) {
            eng.seq_advanced.push(name.to_string());
        }
    }
    eng.db
        .sequences
        .get_mut(name)
        .expect("visible above")
        .push(next);
    ctx.writes.push(WriteOp::AlterSequence {
        name: name.to_string(),
        prev,
    });
    Ok(ExecResult::Command {
        tag: "ALTER SEQUENCE".to_string(),
    })
}

pub(crate) fn exec_drop_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        // v0.11: only the owner (or a superuser) may drop a sequence.
        require_seq_owner(eng, ctx, name)?;
        let live = eng.db.find_sequence(name, ctx.snap, ctx.own);
        match live {
            None if if_exists => continue,
            None => {
                return Err(exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", name),
                ));
            }
            Some(_) => {}
        }
        // Postgres refuses to drop a sequence owned by a column default
        // (dependent). v0.9 tracks the dependency: DEFAULT nextval('s').
        let mut dependent: Option<(String, String)> = None;
        for (tname, vs) in &eng.db.tables {
            let Some(t) = vs
                .iter()
                .find(|t| crate::storage::table_visible(t, ctx.snap, &ctx.all_xids))
            else {
                continue;
            };
            for (i, d) in t.defaults.iter().enumerate() {
                if matches!(d, Some(DefaultExpr::Nextval(s)) if s == name) {
                    dependent = Some((tname.clone(), t.columns[i].0.clone()));
                    break;
                }
            }
            if dependent.is_some() {
                break;
            }
        }
        if let Some((t, c)) = dependent {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop sequence {} because column {}.{} has a default depending on it",
                    name, t, c
                ),
            ));
        }
        let versions = eng.db.sequences.get_mut(name).expect("visible above");
        let cur = versions
            .iter_mut()
            .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
            .expect("visible above");
        let prev = cur.clone();
        cur.dropped_xmax = ctx.own;
        ctx.writes.push(WriteOp::DropSequence {
            name: name.clone(),
            seq: prev,
        });
    }
    Ok(ExecResult::Command {
        tag: "DROP SEQUENCE".to_string(),
    })
}

/// Advance a sequence, returning the new value. Non-transactional, like
/// Postgres: the advance survives abort (undo is a no-op) and is staged
/// for commit-time WAL logging via `eng.seq_advanced`.
pub fn seq_nextval(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    name: &str,
) -> Result<i64, ExecError> {
    // v0.11: nextval needs USAGE on the sequence.
    if let Some(s) = eng.db.find_sequence(name, snap, own) {
        let have = crate::storage::sequence_privs(&eng.db, role, s, snap, own);
        if have & crate::storage::PRIV_USAGE == 0 {
            return Err(exec_err(
                "42501",
                format!("permission denied for sequence \"{}\"", name),
            ));
        }
    }
    let cur = eng
        .db
        .find_sequence_mut(name, snap, own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    // Postgres semantics via is_called: a fresh sequence (or one reset by
    // setval(v,false)) returns its current value without advancing; once
    // called, each nextval advances by the increment.
    let next = match cur.current {
        None => cur.start,
        Some(v) if !cur.is_called => v,
        Some(v) => {
            let n = v.checked_add(cur.increment).ok_or_else(|| {
                exec_err(
                    "22000",
                    format!(
                        "nextval: reached {} value of sequence \"{}\"",
                        if cur.increment > 0 {
                            "maximum"
                        } else {
                            "minimum"
                        },
                        name
                    ),
                )
            })?;
            let over = if cur.increment > 0 {
                n > cur.max_value
            } else {
                n < cur.min_value
            };
            if over {
                if cur.cycle {
                    if cur.increment > 0 {
                        cur.min_value
                    } else {
                        cur.max_value
                    }
                } else {
                    return Err(exec_err(
                        "22000",
                        format!(
                            "nextval: reached {} value of sequence \"{}\" ({})",
                            if cur.increment > 0 {
                                "maximum"
                            } else {
                                "minimum"
                            },
                            name,
                            if cur.increment > 0 {
                                cur.max_value
                            } else {
                                cur.min_value
                            },
                        ),
                    ));
                }
            } else {
                n
            }
        }
    };
    cur.current = Some(next);
    cur.is_called = true;
    eng.seq_currval.insert((session, name.to_string()), next);
    // v0.98: lastval() tracks the most recent nextval of any sequence.
    eng.seq_lastval.insert(session, next);
    if !eng.seq_advanced.contains(&name.to_string()) {
        eng.seq_advanced.push(name.to_string());
    }
    Ok(next)
}

pub fn seq_currval(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    name: &str,
) -> Result<i64, ExecError> {
    // v0.11: currval needs USAGE on the sequence.
    match eng.db.find_sequence(name, snap, own) {
        None => {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", name),
            ));
        }
        Some(s) => {
            let have = crate::storage::sequence_privs(&eng.db, role, s, snap, own);
            if have & crate::storage::PRIV_USAGE == 0 {
                return Err(exec_err(
                    "42501",
                    format!("permission denied for sequence \"{}\"", name),
                ));
            }
        }
    }
    eng.seq_currval
        .get(&(session, name.to_string()))
        .copied()
        .ok_or_else(|| {
            exec_err(
                "55000",
                format!(
                    "currval of sequence \"{}\" is not yet defined in this session",
                    name
                ),
            )
        })
}

/// v0.98: `lastval()` — the value most recently returned by `nextval`
/// for any sequence in this session (PG19 sequence_functions). Unlike
/// currval it needs no sequence name; PG checks no privilege on it.
pub fn seq_lastval(eng: &Engine, session: u64) -> Result<i64, ExecError> {
    eng.seq_lastval.get(&session).copied().ok_or_else(|| {
        exec_err(
            "55000",
            "lastval is not yet defined in this session".to_string(),
        )
    })
}

pub fn seq_setval(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    name: &str,
    value: i64,
    is_called: bool,
) -> Result<i64, ExecError> {
    // v0.11: setval needs USAGE on the sequence.
    if let Some(s) = eng.db.find_sequence(name, snap, own) {
        let have = crate::storage::sequence_privs(&eng.db, role, s, snap, own);
        if have & crate::storage::PRIV_USAGE == 0 {
            return Err(exec_err(
                "42501",
                format!("permission denied for sequence \"{}\"", name),
            ));
        }
    }
    let cur = eng
        .db
        .find_sequence_mut(name, snap, own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    // v0.98: PostgreSQL validates setval against [min_value, max_value]
    // (SQLSTATE 22003, "setval: value n is out of bounds for sequence
    // \"s\" (min..max)").
    if value < cur.min_value || value > cur.max_value {
        return Err(exec_err(
            "22003",
            format!(
                "setval: value {} is out of bounds for sequence \"{}\" ({}..{})",
                value, name, cur.min_value, cur.max_value
            ),
        ));
    }
    cur.current = Some(value);
    // setval(v, true): is_called=true, next nextval returns v + increment.
    // setval(v, false): is_called=false, next nextval returns v itself.
    cur.is_called = is_called;
    if is_called {
        eng.seq_currval.insert((session, name.to_string()), value);
    }
    // v0.98: PG19 — setval with is_called=false does NOT change the value
    // reported by currval ("the value reported by currval is not changed in
    // this case", functions-sequence). A defined currval keeps its old
    // value; an undefined one stays undefined (55000).
    if !eng.seq_advanced.contains(&name.to_string()) {
        eng.seq_advanced.push(name.to_string());
    }
    Ok(value)
}
