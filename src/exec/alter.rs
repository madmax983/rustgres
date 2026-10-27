// v1.78 mechanical split: moved verbatim from src/exec.rs (64110-66048).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

/// v0.37: ALTER TABLE name ALTER COLUMN col SET STORAGE mode.
/// Follows PG's `ATExecSetStorage`: the mode name is case-insensitive;
/// invalid names are 22023; non-toastable types may only use PLAIN
/// (anything else is 0A000); `default` restores the type default.
pub(crate) fn alter_set_storage(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    column: &str,
    mode: &str,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let col_idx = t
        .columns
        .iter()
        .position(|(n, _)| n == column)
        .ok_or_else(|| {
            exec_err(
                "42703",
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    column, name
                ),
            )
        })?;
    let col_type = t.columns[col_idx].1.clone();
    let storage = toast_storage::parse(mode, col_type.default_toast_storage())
        .ok_or_else(|| exec_err("22023", format!("invalid storage type \"{}\"", mode)))?;
    if storage != toast_storage::PLAIN && !col_type.is_toastable() {
        return Err(exec_err(
            "0A000",
            // PG19 tablecmds.c GetAttributeStorage: "column data type %s
            // can only have storage PLAIN".
            format!(
                "column data type {} can only have storage PLAIN",
                col_type.sql_name(),
            ),
        ));
    }
    let mut next = t;
    next.col_storage[col_idx] = storage;
    // v0.37: creating the toast table itself is lazy (on first toast);
    // SET STORAGE only records the strategy, like PG.
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v0.41: validate one column's `COMPRESSION` option, following PG19's
/// `GetAttributeCompression`: absent or `default` = no explicit method
/// (`None`); an explicit method on a non-toastable type is 0A000 (PG
/// checks toastability *before* the method name); an unknown method is
/// 22023.
pub(crate) fn parse_column_compression(
    col_type: &ColType,
    mode: Option<&str>,
) -> Result<Option<crate::storage::ToastCompression>, ExecError> {
    let mode = match mode {
        None => return Ok(None),
        Some(m) => m,
    };
    if mode.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    if !col_type.is_toastable() {
        return Err(exec_err(
            "0A000",
            format!(
                "column data type {} does not support compression",
                col_type.sql_name(),
            ),
        ));
    }
    crate::storage::ToastCompression::from_name(mode)
        .map(Some)
        .ok_or_else(|| exec_err("22023", format!("invalid compression method \"{}\"", mode)))
}

/// v0.41: ALTER TABLE name ALTER COLUMN col SET COMPRESSION method.
/// Follows PG19's `ATExecSetCompression` / `GetAttributeCompression`:
/// `default` clears the explicit method (resolves to the session GUC at
/// write time); an explicit method on a non-toastable type is 0A000;
/// an unknown method is 22023. Like PG, this only records metadata for
/// future writes — existing values are not rewritten.
pub(crate) fn alter_set_compression(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    column: &str,
    mode: &str,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let col_idx = t
        .columns
        .iter()
        .position(|(n, _)| n == column)
        .ok_or_else(|| {
            exec_err(
                "42703",
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    column, name
                ),
            )
        })?;
    let col_type = t.columns[col_idx].1.clone();
    // PG's GetAttributeCompression: default = no explicit method,
    // 0A000 on non-toastable types, 22023 on unknown methods.
    let method = parse_column_compression(&col_type, Some(mode))?;
    let mut next = t;
    // `col_compression` parallels `columns`; grow defensively if an
    // older code path left it short.
    if next.col_compression.len() < next.columns.len() {
        next.col_compression.resize(next.columns.len(), None);
    }
    next.col_compression[col_idx] = method;
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v0.37: ALTER TABLE name SET (opt = val, ...). Only
/// `toast_tuple_target` is supported (range 128..=8160, like PG's
/// reloption); anything else is 22023 ("unrecognized parameter").
/// v0.64: `parallel_workers` is also accepted (PG19 reloption, >= 0);
/// it is a planner hint and a no-op here, but accepting it keeps
/// regression tests' transactions alive.
/// v1.96: PG19 relation reloption kinds (reloptions.c
/// reloption_name_table kinds): "bool" / "int" / "real". Covers the
/// autovacuum family, fillfactor, vacuum_truncate and friends; the
/// toast.* spellings fold to the same kinds.
pub(crate) fn pg_reloption_kind(opt: &str) -> Option<&'static str> {
    let base = opt
        .strip_prefix("toast.")
        .unwrap_or(opt)
        .to_ascii_lowercase();
    Some(match base.as_str() {
        "autovacuum_enabled" | "vacuum_truncate" | "user_catalog_table" => "bool",
        "autovacuum_vacuum_threshold"
        | "autovacuum_analyze_threshold"
        | "autovacuum_vacuum_cost_limit"
        | "autovacuum_freeze_min_age"
        | "autovacuum_freeze_max_age"
        | "autovacuum_freeze_table_age"
        | "autovacuum_multixact_freeze_min_age"
        | "autovacuum_multixact_freeze_max_age"
        | "autovacuum_multixact_freeze_table_age"
        | "fillfactor" => "int",
        "autovacuum_vacuum_scale_factor"
        | "autovacuum_analyze_scale_factor"
        | "autovacuum_vacuum_cost_delay"
        | "vacuum_max_eager_freeze_failure_rate" => "real",
        _ => return None,
    })
}

/// v1.96: PG19 boolean spellings for reloptions/GUCs.
pub(crate) fn parse_reloption_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

pub(crate) fn alter_set_reloptions(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    options: &[(String, String)],
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    // v1.96 finishing pass: PG19 forbids storage parameters on a
    // partitioned table (its leaf partitions carry their own).
    if t.partition
        .as_ref()
        .map(|p| p.is_partitioned)
        .unwrap_or(false)
    {
        return Err(exec_err(
            "0A000",
            "cannot specify storage parameters for a partitioned table".to_string(),
        ));
    }
    let mut next = t;
    for (opt, val) in options {
        if opt.eq_ignore_ascii_case("toast_tuple_target") {
            let n: i64 = val.parse().map_err(|_| {
                exec_err(
                    "22023",
                    format!("invalid value for \"toast_tuple_target\": \"{}\"", val),
                )
            })?;
            if n < toast_consts::TOAST_TUPLE_TARGET_MIN as i64
                || n > toast_consts::TOAST_TUPLE_TARGET_MAIN as i64
            {
                return Err(exec_err(
                    "22023",
                    format!(
                        "value {} out of bounds for \"toast_tuple_target\" (128..={})",
                        n,
                        toast_consts::TOAST_TUPLE_TARGET_MAIN,
                    ),
                ));
            }
            next.toast_target = n as u32;
        } else if let Some(kind) = pg_reloption_kind(opt) {
            // v1.96: the standard PG19 relation reloptions
            // (reloptions.c): accepted and validated; planner/storage
            // hints only in this engine, so values are not stored.
            match kind {
                "bool" => {
                    if parse_reloption_bool(val).is_none() {
                        return Err(exec_err(
                            "22023",
                            format!(
                                "invalid value for boolean reloption \"{}\": \"{}\"",
                                opt, val
                            ),
                        ));
                    }
                }
                "int" => {
                    if val.parse::<i64>().is_err() {
                        return Err(exec_err(
                            "22023",
                            format!(
                                "invalid value for integer reloption \"{}\": \"{}\"",
                                opt, val
                            ),
                        ));
                    }
                }
                _ => {
                    if val.parse::<f64>().is_err() {
                        return Err(exec_err(
                            "22023",
                            format!("invalid value for real reloption \"{}\": \"{}\"", opt, val),
                        ));
                    }
                }
            }
        } else if opt.eq_ignore_ascii_case("parallel_workers") {
            let n: i64 = val.parse().map_err(|_| {
                exec_err(
                    "22023",
                    format!("invalid value for \"parallel_workers\": \"{}\"", val),
                )
            })?;
            if n < 0 {
                return Err(exec_err(
                    "22023",
                    format!("value {} out of bounds for \"parallel_workers\" (>= 0)", n),
                ));
            }
            // Planner hint only; no parallel scan to tune. Accepted
            // and stored nowhere (like PG, it does not affect results).
        } else {
            // PG19 reloptions.c parseRelOptionsInternal: unrecognized
            // parameters are 22023, not 0A000.
            return Err(exec_err(
                "22023",
                format!("unrecognized parameter \"{}\"", opt),
            ));
        }
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

pub(crate) fn eval_sequence_func(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    read_only: bool,
    name: &str,
    vals: &[Value],
) -> Result<Value, ExecError> {
    let seq_name = |v: &Value| -> Result<String, ExecError> {
        match v {
            Value::Text(s) => Ok(s.to_string()),
            // regclass input: Postgres accepts nextval('seq').
            _ => Err(exec_err(
                "42883",
                format!("function {}: sequence name must be text", name),
            )),
        }
    };
    match name {
        "nextval" => {
            // v0.17: advancing a sequence is a write — blocked read-only.
            if read_only {
                return Err(exec_err(
                    "25006",
                    "cannot execute nextval() in a read-only transaction".to_string(),
                ));
            }
            let s = seq_name(&vals[0])?;
            Ok(Value::BigInt(seq_nextval(
                eng, snap, own, session, role, &s,
            )?))
        }
        "currval" => {
            let s = seq_name(&vals[0])?;
            Ok(Value::BigInt(seq_currval(
                eng, snap, own, session, role, &s,
            )?))
        }
        "lastval" => Ok(Value::BigInt(seq_lastval(eng, session)?)),
        "setval" => {
            // v0.17: like nextval, setting a sequence is a write.
            if read_only {
                return Err(exec_err(
                    "25006",
                    "cannot execute setval() in a read-only transaction".to_string(),
                ));
            }
            let s = seq_name(&vals[0])?;
            let v = match vals[1] {
                Value::Int(i) => i as i64,
                Value::BigInt(i) => i,
                Value::SmallInt(i) => i as i64,
                _ => {
                    return Err(exec_err(
                        "42883",
                        "setval: value must be an integer".to_string(),
                    ));
                }
            };
            let is_called = match vals.get(2) {
                None => true,
                Some(Value::Bool(b)) => *b,
                _ => {
                    return Err(exec_err(
                        "42883",
                        "setval: is_called must be boolean".to_string(),
                    ));
                }
            };
            Ok(Value::BigInt(seq_setval(
                eng, snap, own, session, role, &s, v, is_called,
            )?))
        }
        _ => unreachable!(),
    }
}

// ============================================================================
// v0.9: ALTER TABLE.
// ============================================================================

/// Swap in an altered table version: mark the live version dropped by us,
/// push the new one, and log WriteOp::AlterTable for undo/WAL.
/// `new_rows`: Some for ADD/DROP COLUMN (rows rewritten with new ids, WAL-
/// logged as InsertRow); None for pure-metadata alters (rows move over).
/// v0.77: temp-table counterpart of [`alter_swap`]. Temp tables are
/// single-version and session-local, so there is no catalog version to
/// retire: the previous `Table` is cloned for the undo op and the new
/// one takes its place in `temp_tables[session]` (under the new name
/// when renamed). Row-rewriting ALTERs log `InsertRow` ops for the
/// fresh row ids exactly like the permanent path; their undos run
/// before this op's (newest-first) and are temp-aware
/// (`remove_own_version` searches temp tables too).
pub(crate) fn alter_swap_temp(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    renamed_to: Option<String>,
    mut next: Table,
    new_rows: Option<Vec<RowVersion>>,
) -> Result<(), ExecError> {
    let prev = eng
        .db
        .temp_tables
        .get(&ctx.session)
        .and_then(|m| m.get(name))
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    // v0.37/v0.41 (mirrored): the per-column TOAST storage and
    // COMPRESSION metadata must stay parallel to `columns`.
    debug_assert_eq!(
        next.col_storage.len(),
        next.columns.len(),
        "col_storage out of sync with columns in ALTER TABLE {}",
        name
    );
    debug_assert_eq!(
        next.col_compression.len(),
        next.columns.len(),
        "col_compression out of sync with columns in ALTER TABLE {}",
        name
    );
    let mut log_ids = Vec::new();
    if let Some(rows) = new_rows {
        log_ids.extend(rows.iter().map(|r| r.id));
        next.rows = rows;
        // v0.9: the row ids changed — rebuild the id->position index.
        next.rebuild_row_index();
    }
    let target = renamed_to.clone().unwrap_or_else(|| name.to_string());
    let tmps = eng.db.temp_tables.entry(ctx.session).or_default();
    if renamed_to.is_some() {
        tmps.remove(name);
    }
    tmps.insert(target.clone(), next);
    ctx.writes.push(WriteOp::AlterTempTable {
        session: ctx.session,
        name: name.to_string(),
        prev,
        renamed_to,
    });
    for id in log_ids {
        ctx.writes.push(WriteOp::InsertRow {
            table: target.clone(),
            row_id: id,
        });
    }
    Ok(())
}

pub(crate) fn alter_swap(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    renamed_to: Option<String>,
    mut next: Table,
    new_rows: Option<Vec<RowVersion>>,
) -> Result<(), ExecError> {
    // v0.77: temp tables live in `Database::temp_tables[session]`, not
    // the versioned catalog — swap them in place with a temp undo op
    // instead of minting a catalog version.
    if eng.db.is_temp_table(ctx.session, name) {
        return alter_swap_temp(eng, ctx, name, renamed_to, next, new_rows);
    }
    let prev = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    // v0.37: the per-column TOAST storage must stay parallel to
    // `columns` across every ALTER shape change.
    debug_assert_eq!(
        next.col_storage.len(),
        next.columns.len(),
        "col_storage out of sync with columns in ALTER TABLE {}",
        name
    );
    // v0.41: same for the per-column COMPRESSION metadata.
    debug_assert_eq!(
        next.col_compression.len(),
        next.columns.len(),
        "col_compression out of sync with columns in ALTER TABLE {}",
        name
    );
    next.created_xmin = ctx.own;
    next.dropped_xmax = 0;
    let rewrite = new_rows.is_some();
    let mut log_ids = Vec::new();
    if let Some(rows) = new_rows {
        log_ids.extend(rows.iter().map(|r| r.id));
        next.rows = rows;
        // v0.9: the row ids changed — rebuild the id->position index.
        next.rebuild_row_index();
    }
    let target = renamed_to.clone().unwrap_or_else(|| name.to_string());
    if renamed_to.is_some() {
        let mut versions = eng.db.tables.remove(name).unwrap_or_default();
        {
            let old_live = versions
                .iter_mut()
                .find(|t| crate::storage::table_visible(t, ctx.snap, &ctx.all_xids))
                .expect("visible above");
            old_live.dropped_xmax = ctx.own;
            if !rewrite {
                next.rows = std::mem::take(&mut old_live.rows);
                // v0.9: the old version's rows were moved; its index is stale.
                old_live.rebuild_row_index();
                // v0.9: next got the old rows (same ids/order); its cloned
                // index is still valid, but rebuild to be safe.
                next.rebuild_row_index();
            }
        }
        versions.push(next);
        eng.db.tables.insert(target.clone(), versions);
    } else {
        let versions = eng.db.tables.get_mut(name).expect("visible above");
        let old_live = versions
            .iter_mut()
            .find(|t| crate::storage::table_visible(t, ctx.snap, &ctx.all_xids))
            .expect("visible above");
        old_live.dropped_xmax = ctx.own;
        if !rewrite {
            next.rows = std::mem::take(&mut old_live.rows);
            // v0.9: the old version's rows were moved; its index is stale.
            old_live.rebuild_row_index();
            // v0.9: next got the old rows (same ids/order); its cloned
            // index is still valid, but rebuild to be safe.
            next.rebuild_row_index();
        }
        versions.push(next);
    }
    ctx.writes.push(WriteOp::AlterTable {
        name: name.to_string(),
        prev,
        renamed_to,
        rewrite_rows: rewrite,
    });
    for id in log_ids {
        ctx.writes.push(WriteOp::InsertRow {
            table: target.clone(),
            row_id: id,
        });
    }
    Ok(())
}

/// Rename column references inside a CHECK/DEFAULT expression (for
/// ALTER TABLE ... RENAME COLUMN). Constraint expressions are simple
/// (no subqueries/aggregates), so a shallow walk suffices.
pub(crate) fn rename_col_in_expr(e: &mut Expr, old: &str, new: &str) {
    match e {
        // v0.95: named args are transparent to inspection.
        Expr::NamedArg { expr, .. } => rename_col_in_expr(expr, old, new),
        Expr::Column { table, name } => {
            if table.is_none() && name == old {
                *name = new.to_string();
            }
        }
        // v0.73: a whole-row qualifier names a range, not a column.
        Expr::WholeRow { .. } => {}
        Expr::Arith { left, right, .. } => {
            rename_col_in_expr(left, old, new);
            rename_col_in_expr(right, old, new);
        }
        Expr::Cast { expr, .. } => rename_col_in_expr(expr, old, new),
        // v0.81: named casts, row constructors and field accesses
        // recurse into their operand(s), mutating in place.
        Expr::CastNamed { expr, .. } | Expr::FieldAccess { expr, .. } => {
            rename_col_in_expr(expr, old, new)
        }
        // v1.86: AT TIME ZONE.
        Expr::AtTimeZone { expr, zone } => {
            rename_col_in_expr(expr, old, new);
            if let Some(z) = zone {
                rename_col_in_expr(z, old, new);
            }
        }
        Expr::Row(elems) => {
            for e in elems {
                rename_col_in_expr(e, old, new);
            }
        }
        Expr::Concat(a, b)
        | Expr::JsonOp {
            left: a, right: b, ..
        }
        | Expr::And(a, b)
        | Expr::Or(a, b) => {
            rename_col_in_expr(a, old, new);
            rename_col_in_expr(b, old, new);
        }
        // v0.79: array expressions recurse into their operands.
        Expr::ArrayCtor { elems, .. } => {
            for e in elems {
                rename_col_in_expr(e, old, new);
            }
        }
        Expr::Subscript { array, indices } => {
            rename_col_in_expr(array, old, new);
            for i in indices {
                rename_col_in_expr(i, old, new);
            }
        }
        Expr::Slice { array, bounds } => {
            rename_col_in_expr(array, old, new);
            for (l, u) in bounds {
                if let Some(l) = l {
                    rename_col_in_expr(l, old, new);
                }
                if let Some(u) = u {
                    rename_col_in_expr(u, old, new);
                }
            }
        }
        Expr::Not(a) => rename_col_in_expr(a, old, new),
        Expr::BitNot(a) => rename_col_in_expr(a, old, new),
        Expr::Neg(a) => rename_col_in_expr(a, old, new),
        // v0.55: rename inside every CASE arm.
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                rename_col_in_expr(o, old, new);
            }
            for (k, r) in whens {
                rename_col_in_expr(k, old, new);
                rename_col_in_expr(r, old, new);
            }
            if let Some(e) = else_ {
                rename_col_in_expr(e, old, new);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            rename_col_in_expr(expr, old, new);
            rename_col_in_expr(pattern, old, new);
        }
        // v0.68: regex match, like LIKE.
        Expr::Regex { expr, pattern, .. } => {
            rename_col_in_expr(expr, old, new);
            rename_col_in_expr(pattern, old, new);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            rename_col_in_expr(expr, old, new);
            rename_col_in_expr(low, old, new);
            rename_col_in_expr(high, old, new);
        }
        Expr::IsBool { expr, .. } | Expr::IsNull { expr, .. } => rename_col_in_expr(expr, old, new),
        Expr::IsDistinctFrom { left, right, .. } => {
            rename_col_in_expr(left, old, new);
            rename_col_in_expr(right, old, new);
        }
        Expr::Extract { from, .. } => rename_col_in_expr(from, old, new),
        Expr::Cmp { left, right, .. } => {
            rename_col_in_expr(left, old, new);
            rename_col_in_expr(right, old, new);
        }
        Expr::Func { args, .. } => {
            for a in args {
                rename_col_in_expr(a, old, new);
            }
        }
        Expr::Literal(_) | Expr::Param(_) | Expr::ResolvedCol { .. } | Expr::Agg { .. } => {}
        // v1.30: ordered-set aggregates resolve like plain aggregates.
        Expr::WithinGroup { .. } => {}
        Expr::ScalarSub(_)
        | Expr::JsonArraySubq { .. }
        | Expr::ArraySubquery(_)
        | Expr::InSub { .. }
        | Expr::Quantified { .. }
        | Expr::UserOp { .. }
        | Expr::Exists { .. } => {}
        // v0.10: windows cannot appear in constraints; nothing to rename.
        Expr::Window { .. } => {}
    }
}

/// v0.72: run a comma-separated ALTER TABLE action list left to right
/// in one statement (PG19). The per-action checks (ownership, temp
/// rejection) apply to each action; the statement stays atomic through
/// the shared write log.
pub(crate) fn exec_alter(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    actions: &[AlterAction],
) -> Result<ExecResult, ExecError> {
    exec_alter_flags(eng, ctx, name, actions, false)
}

/// v1.97: ALTER TABLE statement entry (dispatch): honors
/// IF EXISTS (missing relation -> quiet success) and routes
/// RENAME COLUMN addressed at a view/matview to the view rename
/// path (PG19 accepts ALTER TABLE on views).
pub(crate) fn exec_alter_stmt(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    actions: &[AlterAction],
    if_exists: bool,
    only: bool,
) -> Result<ExecResult, ExecError> {
    let is_table = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .is_some();
    if !is_table {
        let is_view = eng.db.find_view(name, ctx.snap, ctx.own).is_some();
        if is_view
            && !actions.is_empty()
            && actions
                .iter()
                .all(|a| matches!(a, AlterAction::RenameColumn { .. }))
        {
            for action in actions {
                if let AlterAction::RenameColumn { old, new } = action {
                    exec_alter_view_rename_column_tag(eng, ctx, name, old, new, "ALTER TABLE")?;
                }
            }
            return Ok(ExecResult::Command {
                tag: "ALTER TABLE".to_string(),
            });
        }
        if if_exists {
            return Ok(ExecResult::Command {
                tag: "ALTER TABLE".to_string(),
            });
        }
    }
    exec_alter_flags(eng, ctx, name, actions, only)
}

pub(crate) fn exec_alter_flags(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    actions: &[AlterAction],
    only: bool,
) -> Result<ExecResult, ExecError> {
    for action in actions {
        exec_alter_one(eng, ctx, name, action, only)?;
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

pub(crate) fn exec_alter_one(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    action: &AlterAction,
    only: bool,
) -> Result<ExecResult, ExecError> {
    // v0.11: every ALTER TABLE needs owner-or-superuser.
    // v1.96: OWNER TO has its own PG19 rule (inheritance-aware
    // current-owner check + SET ROLE ability for the new owner),
    // enforced inside alter_owner_to.
    if !matches!(action, AlterAction::OwnerTo { .. }) {
        require_table_owner(eng, ctx, name)?;
    }
    // v0.77: ALTER TABLE on temp tables is supported — `alter_swap`
    // branches to a temp-safe path (`alter_swap_temp`) since the
    // versioned-catalog machinery only knows permanent tables.
    // ATTACH PARTITION still needs the versioned catalog
    // (`commit_table_version` / `parent_link_child`), so it stays
    // 0A000 on temp tables (either side).
    if let AlterAction::AttachPartition { child, .. } = action {
        if eng.db.is_temp_table(ctx.session, name) || eng.db.is_temp_table(ctx.session, child) {
            return Err(exec_err(
                "0A000",
                "ALTER TABLE ... ATTACH PARTITION on temporary tables is not supported in this version",
            ));
        }
    }
    match action {
        AlterAction::AddColumn {
            name: col,
            col_type,
            named_type,
            bit_typmod,
            not_null,
            default,
            serial,
            compression,
            checks,
            uniques,
            pkey,
            fks,
        } => {
            // v1.97: PG19 ATExecAddColumn — ONLY with inheritance /
            // partition children would put them out of step.
            if only && !rename_descendants(eng, ctx, name).is_empty() {
                return Err(exec_err(
                    "42P16",
                    "column must be added to child tables too".to_string(),
                ));
            }
            // v1.96: PG19 type-USAGE check for the new column's type
            // (and cast targets in its CHECK/DEFAULT clauses).
            if let Some(n) = named_type {
                check_type_usage(eng, ctx, n)?;
            }
            for c in checks {
                check_expr_cast_usages(eng, ctx, &c.expr)?;
            }
            if let Some(crate::sql::DefaultExpr::Expr(e)) = default {
                check_expr_cast_usages(eng, ctx, e)?;
            }
            let res = alter_add_column(
                eng,
                ctx,
                name,
                col,
                col_type,
                *bit_typmod,
                *not_null,
                default,
                *serial,
                compression,
                checks,
                uniques,
                pkey,
                fks,
            );
            // v0.69: propagate ADD COLUMN to partitions (PG does this).
            if res.is_ok() {
                let children: Vec<String> = eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .and_then(|t| t.partition.as_ref())
                    .map(|p| p.children.clone())
                    .unwrap_or_default();
                for child in &children {
                    // v0.77: a temp table may shadow a partition child's
                    // name in this session — never propagate into it
                    // (PG resolves children by OID; we resolve by name,
                    // so skip rather than alter the wrong table).
                    if eng.db.is_temp_table(ctx.session, child) {
                        continue;
                    }
                    // Recurse via exec_alter to handle nested partitions.
                    // Construct the action anew (we have the parts).
                    let child_action = AlterAction::AddColumn {
                        name: col.clone(),
                        col_type: *col_type,
                        named_type: named_type.clone(),
                        bit_typmod: *bit_typmod,
                        not_null: *not_null,
                        default: default.clone(),
                        serial: *serial,
                        compression: compression.clone(),
                        checks: checks.clone(),
                        uniques: uniques.clone(),
                        pkey: pkey.clone(),
                        fks: fks.clone(),
                    };
                    exec_alter(eng, ctx, child, std::slice::from_ref(&child_action))?;
                }
            }
            res
        }
        AlterAction::AlterColumnType {
            name: col,
            col_type,
            named_type,
            using_expr,
        } => {
            let res = alter_column_type(
                eng,
                ctx,
                name,
                col,
                col_type,
                named_type.as_deref(),
                using_expr.as_ref(),
            );
            // v1.96 finishing pass: propagate to partitions like
            // ADD COLUMN — a partition whose key uses the column
            // must fail the whole statement (PG19 ATExec on each
            // child; the partition-key guard lives in
            // alter_column_type).
            if res.is_ok() {
                let children: Vec<String> = eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .and_then(|t| t.partition.as_ref())
                    .map(|p| p.children.clone())
                    .unwrap_or_default();
                for child in &children {
                    if eng.db.is_temp_table(ctx.session, child) {
                        continue;
                    }
                    // v1.97: parent-driven propagation bypasses the
                    // inherited-column guard via the tree entry
                    // (which recurses the partition tree itself).
                    alter_column_type_tree(
                        eng,
                        ctx,
                        child,
                        col,
                        col_type,
                        named_type.as_deref(),
                        using_expr.as_ref(),
                    )?;
                }
            }
            res
        }
        AlterAction::OfType { type_name } => alter_of_type(eng, ctx, name, type_name),
        AlterAction::DropColumn { name: col, cascade } => {
            let res = alter_drop_column(eng, ctx, name, col, *cascade);
            // v0.69: propagate DROP COLUMN to partitions.
            if res.is_ok() {
                let children: Vec<String> = eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .and_then(|t| t.partition.as_ref())
                    .map(|p| p.children.clone())
                    .unwrap_or_default();
                for child in &children {
                    // v0.77: skip temp-shadowed partition children (see
                    // ADD COLUMN propagation above).
                    if eng.db.is_temp_table(ctx.session, child) {
                        continue;
                    }
                    // v1.97: propagation uses the bypass entry —
                    // the drop was authorized at the parent, so the
                    // child's inherited-column guard (PG's recursing
                    // case) must not fire.
                    drop_column_partition_tree(eng, ctx, child, col, *cascade)?;
                }
            }
            res
        }
        AlterAction::AddConstraint {
            check,
            unique,
            pkey,
            fk,
            notnull,
        } => {
            // v1.97: PG19 ATExecAddConstraint (FK) — ONLY cannot
            // create a foreign key on a partitioned table, since
            // the constraint must exist on every partition.
            if only {
                if let Some(f) = fk {
                    let is_partitioned = eng
                        .db
                        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                        .and_then(|t| t.partition.as_ref())
                        .is_some_and(|p| p.is_partitioned);
                    if is_partitioned {
                        return Err(exec_err(
                            "42809",
                            format!(
                                "cannot use ONLY for foreign key on partitioned table \"{}\" referencing relation \"{}\"",
                                name, f.ref_table
                            ),
                        ));
                    }
                }
                // v1.97: PG19 — a CHECK or NOT NULL constraint cannot
                // be added to ONLY a partitioned parent that has
                // partitions (42P16 "constraint must be added to
                // child tables too"). For plain inheritance parents
                // PG treats ONLY as NO INHERIT (allowed).
                if check.is_some() || notnull.is_some() {
                    let t = eng
                        .db
                        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session);
                    let partitioned_with_children = t.is_some_and(|t| {
                        t.partition
                            .as_ref()
                            .is_some_and(|p| p.is_partitioned && !p.children.is_empty())
                    });
                    if partitioned_with_children {
                        return Err(exec_err(
                            "42P16",
                            "constraint must be added to child tables too".to_string(),
                        ));
                    }
                }
            }
            let res = alter_add_constraint(eng, ctx, name, check, unique, pkey, fk, notnull);
            // v0.70: propagate ADD CONSTRAINT to partitions (PG does
            // this). Per-child backing indexes get per-table names via
            // `constraint_index_name`, so siblings never collide.
            if res.is_ok() {
                let con_name: Option<&str> = check
                    .as_ref()
                    .map(|c| c.name.as_str())
                    .or(unique.as_ref().map(|u| u.name.as_str()))
                    .or(pkey.as_ref().map(|p| p.name.as_str()))
                    .or(fk.as_ref().map(|f| f.name.as_str()))
                    .or(notnull.as_ref().map(|n| n.name.as_str()));
                let children: Vec<String> = eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .and_then(|t| t.partition.as_ref())
                    .map(|p| p.children.clone())
                    .unwrap_or_default();
                for child in &children {
                    // v0.77: skip temp-shadowed partition children (see
                    // ADD COLUMN propagation above).
                    if eng.db.is_temp_table(ctx.session, child) {
                        continue;
                    }
                    // Skip children that already carry this constraint
                    // (e.g. inherited at CREATE PARTITION OF time).
                    let already = con_name.is_some_and(|n| {
                        eng.db
                            .find_table(child, ctx.snap, &ctx.all_xids, ctx.session)
                            .is_some_and(|t| table_has_constraint(t, n))
                    });
                    if already {
                        continue;
                    }
                    let child_action = AlterAction::AddConstraint {
                        check: check.clone(),
                        unique: unique.clone(),
                        pkey: pkey.clone(),
                        fk: fk.clone(),
                        notnull: notnull.clone(),
                    };
                    exec_alter(eng, ctx, child, std::slice::from_ref(&child_action))?;
                }
            }
            res
        }
        AlterAction::ValidateConstraint { name: con } => {
            // v1.97: PG19 ATExecValidateConstraint — validate the
            // existing rows against a NOT VALID constraint, then
            // mark it valid.
            let t = eng
                .db
                .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
                .clone();
            let Some(cd) = t.checks.iter().find(|c| &c.name == con).cloned() else {
                return Err(exec_err(
                    "42704",
                    format!(
                        "constraint \"{}\" for relation \"{}\" does not exist",
                        con, name
                    ),
                ));
            };
            if cd.not_valid {
                let rows: Vec<Row> = t
                    .rows
                    .iter()
                    .filter(|rv| row_visible(rv, ctx.snap, &ctx.all_xids))
                    .map(|rv| rv.values.clone())
                    .collect();
                let qcols: Vec<QCol> = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, (cn, ct))| QCol {
                        qual: name.to_string(),
                        name: cn.clone(),
                        ty: ct.clone(),
                        hidden: false,
                        src_ord: i as u32,
                    })
                    .collect();
                let mut lock_ids = Vec::new();
                let mut q = Q {
                    eng: &mut *eng,
                    snap: ctx.snap,
                    own: ctx.own,
                    all_xids: ctx.all_xids.clone(),
                    session: ctx.session,
                    role: ctx.role,
                    read_only: true,
                    depth: 0,
                    lock_ids: &mut lock_ids,
                    ctes: Vec::new(),
                    wctx: None,
                    srf_vals: Vec::new(),
                    priv_scopes: Vec::new(),
                    priv_role_override: None,
                    hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                    pending_updates: None,
                    write: None,
                };
                for values in &rows {
                    let frame = Scope {
                        schema: &qcols,
                        row: values,
                        prov: None,
                    };
                    let v = eval_expr(&mut q, &[frame], &cd.expr)?;
                    if matches!(v, Value::Bool(false)) {
                        return Err(match cd.kind {
                            crate::sql::CheckKind::NotNull => {
                                let mut refs: Vec<(Option<String>, String)> = Vec::new();
                                crate::sql::collect_col_refs(&cd.expr, &mut refs);
                                let col = refs.first().map(|(_, c)| c.clone()).unwrap_or_default();
                                exec_err(
                                    "23502",
                                    format!(
                                        "column \"{}\" of relation \"{}\" contains null values",
                                        col, name
                                    ),
                                )
                            }
                            crate::sql::CheckKind::Check => exec_err(
                                "23514",
                                format!(
                                    "check constraint \"{}\" of relation \"{}\" is violated by some row",
                                    con, name
                                ),
                            ),
                        });
                    }
                }
                let mut next = t.clone();
                for c in next.checks.iter_mut() {
                    if &c.name == con {
                        c.not_valid = false;
                    }
                }
                alter_swap(eng, ctx, name, None, next, None)?;
            }
            Ok(ExecResult::Command {
                tag: "ALTER TABLE".to_string(),
            })
        }
        AlterAction::DropConstraint { name: con, cascade } => {
            let res = alter_drop_constraint(eng, ctx, name, con, *cascade);
            // v0.70: propagate DROP CONSTRAINT to partitions.
            if res.is_ok() {
                let children: Vec<String> = eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .and_then(|t| t.partition.as_ref())
                    .map(|p| p.children.clone())
                    .unwrap_or_default();
                for child in &children {
                    // Skip children that don't carry this constraint.
                    let missing = eng
                        .db
                        .find_table(child, ctx.snap, &ctx.all_xids, ctx.session)
                        .is_some_and(|t| !table_has_constraint(t, con));
                    if missing {
                        continue;
                    }
                    let child_action = AlterAction::DropConstraint {
                        name: con.clone(),
                        cascade: *cascade,
                    };
                    exec_alter(eng, ctx, child, std::slice::from_ref(&child_action))?;
                }
            }
            res
        }
        AlterAction::AlterColumnSetDefault { name: col, default } => {
            alter_set_default(eng, ctx, name, col, Some(default.clone()))
        }
        AlterAction::AlterColumnDropDefault { name: col } => {
            alter_set_default(eng, ctx, name, col, None)
        }
        AlterAction::AlterColumnSetNotNull { name: col } => {
            alter_set_not_null(eng, ctx, name, col, true)
        }
        AlterAction::AlterColumnDropNotNull { name: col } => {
            alter_set_not_null(eng, ctx, name, col, false)
        }
        // v2.05: SET EXPRESSION / DROP EXPRESSION (PG19).
        AlterAction::AlterColumnSetExpression { name: col, expr } => {
            alter_column_expression(eng, ctx, name, col, Some(expr.clone()), false)
        }
        AlterAction::AlterColumnDropExpression {
            name: col,
            if_exists,
        } => alter_column_expression(eng, ctx, name, col, None, *if_exists),
        AlterAction::RenameColumn { old, new } => {
            alter_rename_column_ext(eng, ctx, name, old, new, only, true)
        }
        AlterAction::RenameTo { new_name } => alter_rename_to(eng, ctx, name, new_name),
        // v0.11
        AlterAction::OwnerTo { new_owner } => alter_owner_to(eng, ctx, name, new_owner),
        // v1.95: ROW LEVEL SECURITY toggles (PG19 ATExecSetRowSecurity /
        // ATExecForceNoForceRowSecurity via the shared alter_swap).
        AlterAction::EnableRowSecurity => alter_set_rls(eng, ctx, name, Some(true), None),
        AlterAction::DisableRowSecurity => alter_set_rls(eng, ctx, name, Some(false), None),
        AlterAction::ForceRowSecurity => alter_set_rls(eng, ctx, name, None, Some(true)),
        AlterAction::NoForceRowSecurity => alter_set_rls(eng, ctx, name, None, Some(false)),
        // v2.03: REPLICA IDENTITY (PG19 ATExecReplicaIdentity).
        AlterAction::ReplicaIdentity { kind, index } => {
            alter_replica_identity(eng, ctx, name, *kind, index.as_deref())
        }
        // v0.37
        AlterAction::SetStorage { column, mode } => alter_set_storage(eng, ctx, name, column, mode),
        AlterAction::SetCompression { column, mode } => {
            alter_set_compression(eng, ctx, name, column, mode)
        }
        AlterAction::SetRelOptions { options } => alter_set_reloptions(eng, ctx, name, options),
        // v0.69: implemented below (attach_partition).
        AlterAction::AttachPartition { child, bound } => {
            attach_partition(eng, ctx, name, child, bound)
        }
        // v2.06: DETACH PARTITION (PG19 ATExecDetachPartition).
        AlterAction::DetachPartition { child, mode } => {
            detach_partition(eng, ctx, name, child, *mode)
        }
        // v2.06: ALTER CONSTRAINT (PG19 ATExecAlterConstraint). On a
        // partitioned table the constraint also lives on every
        // partition (PG alters the children too), so propagate like
        // ADD CONSTRAINT does.
        AlterAction::AlterConstraint {
            name: con,
            deferrable,
            initially_deferred,
            enforced,
        } => {
            let res = alter_alter_constraint(
                eng,
                ctx,
                name,
                con,
                *deferrable,
                *initially_deferred,
                *enforced,
            );
            if res.is_ok() {
                let children: Vec<String> = eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .and_then(|t| t.partition.as_ref())
                    .map(|p| p.children.clone())
                    .unwrap_or_default();
                for child in &children {
                    if eng.db.is_temp_table(ctx.session, child) {
                        continue;
                    }
                    let has = eng
                        .db
                        .find_table(child, ctx.snap, &ctx.all_xids, ctx.session)
                        .is_some_and(|t| table_has_constraint(t, con));
                    if has {
                        let child_action = AlterAction::AlterConstraint {
                            name: con.clone(),
                            deferrable: *deferrable,
                            initially_deferred: *initially_deferred,
                            enforced: *enforced,
                        };
                        exec_alter(eng, ctx, child, std::slice::from_ref(&child_action))?;
                    }
                }
            }
            res
        }
        // v2.06: ADD PRIMARY KEY USING INDEX.
        AlterAction::AddPrimaryKeyUsingIndex { index } => {
            alter_add_primary_key_using_index(eng, ctx, name, index)
        }
        // v2.07: ADD UNIQUE USING INDEX.
        AlterAction::AddUniqueUsingIndex { index } => {
            alter_add_unique_using_index(eng, ctx, name, index)
        }
        // v0.96: table inheritance (PG19).
        AlterAction::Inherit { parent } => alter_inherit(eng, ctx, name, parent, true),
        AlterAction::NoInherit { parent } => alter_inherit(eng, ctx, name, parent, false),
    }
}

/// v1.96: ALTER TABLE name OF TYPE type_name — validate USAGE on
/// the type and that the table's columns match the composite
/// type's fields exactly (PG19 check_of_type).
pub(crate) fn alter_of_type(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    type_name: &str,
) -> Result<ExecResult, ExecError> {
    check_type_usage(eng, ctx, type_name)?;
    let st = match eng.db.types.get(type_name) {
        Some(st) => st,
        None => {
            return Err(exec_err(
                "42704",
                format!("type \"{}\" does not exist", type_name),
            ));
        }
    };
    let fields = match &st.composite {
        Some(f) => f.clone(),
        None => {
            return Err(exec_err(
                "42809",
                format!("type \"{}\" is not a composite type", type_name),
            ));
        }
    };
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let matches = t.columns.len() == fields.len()
        && t.columns
            .iter()
            .zip(fields.iter())
            .all(|((cn, ct), (fn_, ft, _))| cn == fn_ && ct == ft);
    if !matches {
        return Err(exec_err(
            "42804",
            format!("table \"{}\" does not match type \"{}\"", name, type_name),
        ));
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v1.96: ALTER COLUMN c TYPE t — change the column's type
/// (USAGE on a named type is required); stored values are widened
/// in place for the int family, other values ride the dynamic
/// value representation unchanged.
#[allow(clippy::too_many_arguments)]
/// v1.97: ALTER COLUMN TYPE partition-tree propagation entry —
/// the child change is parent-driven, so the inherited-column
/// guard is bypassed (PG19 recursion into partitions).
#[allow(clippy::too_many_arguments)]
pub(crate) fn alter_column_type_tree(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    col_type: &ColType,
    named_type: Option<&str>,
    using_expr: Option<&crate::sql::Expr>,
) -> Result<ExecResult, ExecError> {
    let res = alter_column_type_ext(
        eng, ctx, name, col, col_type, named_type, using_expr, false, true,
    )?;
    let children: Vec<String> = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .and_then(|t| t.partition.as_ref())
        .map(|p| p.children.clone())
        .unwrap_or_default();
    for child in &children {
        if eng.db.is_temp_table(ctx.session, child) {
            continue;
        }
        alter_column_type_tree(eng, ctx, child, col, col_type, named_type, using_expr)?;
    }
    Ok(res)
}

pub(crate) fn alter_column_type(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    col_type: &ColType,
    named_type: Option<&str>,
    using_expr: Option<&crate::sql::Expr>,
) -> Result<ExecResult, ExecError> {
    alter_column_type_ext(
        eng, ctx, name, col, col_type, named_type, using_expr, false, false,
    )
}

/// v1.97: `bypass_typed` serves ALTER TYPE ... CASCADE propagation
/// into typed tables (the OF type itself ordered the change).
#[allow(clippy::too_many_arguments)]
pub(crate) fn alter_column_type_ext(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    col_type: &ColType,
    named_type: Option<&str>,
    using_expr: Option<&crate::sql::Expr>,
    bypass_typed: bool,
    bypass_inherited: bool,
) -> Result<ExecResult, ExecError> {
    if let Some(n) = named_type {
        check_type_usage(eng, ctx, n)?;
    }
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let idx = t
        .columns
        .iter()
        .position(|(n, _)| n == col)
        .ok_or_else(|| {
            exec_err(
                "42703",
                format!("column \"{}\" of relation \"{}\" does not exist", col, name),
            )
        })?;
    // v1.96 finishing pass: typed tables cannot change column
    // types (the OF type owns the column definitions).
    if !bypass_typed && eng.db.typed_tables.contains_key(name) {
        return Err(exec_err(
            "42P16",
            "cannot alter column type of typed table".to_string(),
        ));
    }
    // v1.97: PG19 ATExecAlterColumnType — a column inherited from
    // a parent (inheritance or partition tree) cannot be altered
    // directly in the child (42P16). Parent-driven propagation
    // passes bypass_inherited.
    if !bypass_inherited && ancestor_has_column(eng, ctx, &t, col) {
        return Err(exec_err(
            "42P16",
            format!("cannot alter inherited column \"{}\"", col),
        ));
    }
    // v1.96 finishing pass: PG19 ATExecAlterColumnType guards.
    // The partition-key check runs only on partition ROOTS:
    // attached partitions store the key in PARENT column
    // coordinates, so a positional check on a child whose columns
    // are reordered would false-fire (at_partitioned/at_part_2).
    // (Alter on an intermediate sub-partitioned table named
    // directly is a documented narrowing.)
    {
        if let Some(p) = &t.partition {
            if p.parent.is_none() && p.key.iter().any(|k| k.col == idx && k.expr.is_none()) {
                return Err(exec_err(
                    "0A000",
                    format!(
                        "cannot alter column \"{}\" because it is part of the partition key of relation \"{}\"",
                        col, name
                    ),
                ));
            }
        }
    }
    // v1.97: PG19 — the column may not be a partition-key column
    // of any descendant partitioned table either (e.g. altering
    // list_parted2.b fails because part_5 is partitioned on b).
    {
        for dname in partition_descendants(eng, ctx, name) {
            if let Some(dt) = eng
                .db
                .find_table(&dname, ctx.snap, &ctx.all_xids, ctx.session)
            {
                if let Some(p) = &dt.partition {
                    // Key column indexes are in the coordinates of
                    // the table that DEFINES the key: the descendant
                    // itself when it is partitioned, otherwise its
                    // partition parent (whose column order may
                    // differ — at_part_2 stores (b, a)).
                    let is_key = p.key.iter().any(|k| {
                        if k.expr.is_some() {
                            return false;
                        }
                        let key_col_name = if p.is_partitioned {
                            dt.columns.get(k.col).map(|(cn, _)| cn.clone())
                        } else if let Some(pname) = &p.parent {
                            eng.db
                                .find_table(pname, ctx.snap, &ctx.all_xids, ctx.session)
                                .and_then(|pt| pt.columns.get(k.col).map(|(cn, _)| cn.clone()))
                        } else {
                            dt.columns.get(k.col).map(|(cn, _)| cn.clone())
                        };
                        key_col_name.as_deref() == Some(col)
                    });
                    if is_key {
                        return Err(exec_err(
                            "42P16",
                            format!(
                                "cannot alter column \"{}\" because it is part of the partition key of relation \"{}\"",
                                col, dname
                            ),
                        ));
                    }
                }
            }
        }
    }
    if let Some(u) = using_expr {
        let mut refs = Vec::new();
        crate::sql::collect_col_refs(u, &mut refs);
        if refs.iter().any(|(_, n)| n == "*") {
            return Err(exec_err(
                "0A000",
                "whole-row references are disallowed in ALTER TABLE ALTER COLUMN TYPE USING"
                    .to_string(),
            ));
        }
    }
    if let Some(n) = named_type {
        if crate::exec::ddl_types::composite_type_contains(&eng.db, n, name, 0) {
            return Err(exec_err(
                "42P16",
                format!("composite type {} cannot be made a member of itself", name),
            ));
        }
    }
    {
        // Inheritance: a child inheriting this column from another
        // parent whose definition would diverge must fail
        // (PG19: "cannot alter inherited column").
        let mut tnames: Vec<String> = eng.db.tables.keys().cloned().collect();
        if let Some(tm) = eng.db.temp_tables.get(&ctx.session) {
            tnames.extend(tm.keys().cloned());
        }
        for cname in &tnames {
            if let Some(c) = eng
                .db
                .find_table(cname, ctx.snap, &ctx.all_xids, ctx.session)
            {
                if c.inherits.iter().any(|p| p == name) && c.inherits.len() > 1 {
                    if let Some((_, cty)) = c.columns.iter().find(|(cn, _)| cn == col) {
                        let _ = cty;
                        for p2 in &c.inherits {
                            if p2 == name {
                                continue;
                            }
                            if let Some(pt) =
                                eng.db.find_table(p2, ctx.snap, &ctx.all_xids, ctx.session)
                            {
                                if let Some((_, pty)) = pt.columns.iter().find(|(cn, _)| cn == col)
                                {
                                    if pty != col_type {
                                        return Err(exec_err(
                                            "42P16",
                                            format!(
                                                "cannot alter inherited column \"{}\" of relation \"{}\"",
                                                col, cname
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let mut next = t.clone();
    next.columns[idx].1 = *col_type;
    if idx < next.composite_types.len() {
        next.composite_types[idx] = None;
    }
    if idx < next.domain_types.len() {
        next.domain_types[idx] = None;
    }
    let named_is_domain = match named_type {
        Some(n) => match eng.db.types.get(n) {
            Some(st) if st.domain.is_some() => {
                if idx < next.domain_types.len() {
                    next.domain_types[idx] = Some(n.to_string());
                }
                true
            }
            _ => {
                if idx < next.composite_types.len() {
                    next.composite_types[idx] = Some(n.to_string());
                }
                false
            }
        },
        None => false,
    };
    // v1.96 finishing pass: PG19 rewrites the table — the USING
    // expression (or the assignment cast) is applied to every live
    // row and the rewritten rows are revalidated against the new
    // definition (NOT NULL / CHECK / domain / UNIQUE). The previous
    // implementation only widened int-family values in place, so
    // statements PG rejects (e.g. a USING that collapses a UNIQUE
    // column to a constant) silently succeeded.
    let schema: Vec<QCol> = t
        .columns
        .iter()
        .map(|(n, ty)| QCol {
            qual: name.to_string(),
            name: n.clone(),
            ty: ty.clone(),
            hidden: false,
            src_ord: 0,
        })
        .collect();
    let meta_new = TableMeta::of(&next);
    let mut new_rows: Vec<crate::storage::RowVersion> = Vec::with_capacity(t.rows.len());
    let mut live_new: Vec<Vec<Value>> = Vec::new();
    for rv in &t.rows {
        let mut cells = rv.values.to_vec();
        if !row_visible(rv, ctx.snap, &ctx.all_xids) {
            // Dead versions keep the historical widening-only map.
            if let Some(v) = cells.get_mut(idx) {
                *v = match (&*v, col_type) {
                    (Value::Int(i), ColType::BigInt) => Value::BigInt(*i),
                    (Value::SmallInt(i), ColType::Int) => Value::Int(*i as i64),
                    (Value::SmallInt(i), ColType::BigInt) => Value::BigInt(*i as i64),
                    (other, _) => other.clone(),
                };
            }
            new_rows.push(crate::storage::RowVersion {
                values: crate::storage::Row::new(cells),
                ..rv.clone()
            });
            continue;
        }
        if let Some(cell) = cells.get_mut(idx) {
            let old = cell.clone();
            let converted = if let Some(u) = using_expr {
                let mut lock_ids = Vec::new();
                let mut q = Q {
                    eng: &mut *eng,
                    snap: ctx.snap,
                    own: ctx.own,
                    all_xids: ctx.all_xids.clone(),
                    session: ctx.session,
                    role: ctx.role,
                    read_only: false,
                    depth: 0,
                    lock_ids: &mut lock_ids,
                    ctes: Vec::new(),
                    wctx: None,
                    srf_vals: Vec::new(),
                    priv_scopes: Vec::new(),
                    priv_role_override: None,
                    hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(
                        std::collections::HashMap::new(),
                    )),
                    hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                    pending_updates: None,
                    write: None,
                };
                let frame = Scope {
                    schema: &schema,
                    row: &rv.values,
                    prov: None,
                };
                let v = eval_expr(&mut q, &[frame], u)?;
                if matches!(v, Value::Null) {
                    v
                } else if named_is_domain {
                    eval_cast_named(&mut q, &v, named_type.unwrap())?
                } else if named_type.is_some() {
                    v
                } else {
                    eval_cast(&v, *col_type)?
                }
            } else if matches!(old, Value::Null) || named_type.is_some() {
                old
            } else {
                eval_cast(&old, *col_type)?
            };
            *cell = converted;
        }
        live_new.push(cells.clone());
        // v1.97: stamp the rewritten live version with the current
        // write xid (like ADD/DROP COLUMN's RowVersion::plain) — the
        // commit-time WAL conversion (own_row) only logs row versions
        // whose xmin is the committing transaction, so a plain clone
        // (old xmin) silently lost the row on WAL replay.
        new_rows.push(crate::storage::RowVersion {
            values: crate::storage::Row::new(cells),
            xmin: ctx.write_xid,
            xmax: 0,
            ..rv.clone()
        });
    }
    // Revalidate the rewritten live rows.
    for values in &live_new {
        check_row_constraints(
            eng,
            ctx.snap,
            ctx.own,
            ctx.session,
            ctx.role,
            &meta_new,
            name,
            values,
        )?;
    }
    for def in next.uniques.iter().chain(next.pkey.iter()) {
        let positions: Vec<usize> = def
            .cols
            .iter()
            .map(|c| next.column_index(c).unwrap_or(usize::MAX))
            .collect();
        if positions.contains(&usize::MAX) {
            continue;
        }
        let mut seen: Vec<IndexKey> = Vec::new();
        for values in &live_new {
            let key = IndexKey(
                positions
                    .iter()
                    .map(|&p| values.get(p).cloned().unwrap_or(Value::Null))
                    .collect(),
            );
            // PG: NULLs are distinct in unique constraints, unless
            // NULLS NOT DISTINCT (v2.08).
            if !def.nulls_not_distinct && key.0.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            if seen.contains(&key) {
                return Err(exec_err(
                    "23505",
                    format!(
                        "duplicate key value violates unique constraint \"{}\"",
                        def.name
                    ),
                ));
            }
            seen.push(key);
        }
    }
    alter_swap(eng, ctx, name, None, next, Some(new_rows))?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn alter_add_column(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    col_type: &ColType,
    // v1.93: declared bit typmod for the new column, if any.
    bit_typmod: Option<(i32, bool)>,
    not_null: bool,
    default: &Option<DefaultExpr>,
    serial: Option<SerialKind>,
    compression: &Option<String>,
    checks: &[CheckDef],
    uniques: &[UniqueDef],
    pkey: &Option<UniqueDef>,
    fks: &[FkDef],
) -> Result<ExecResult, ExecError> {
    alter_add_column_ext(
        eng,
        ctx,
        name,
        col,
        col_type,
        bit_typmod,
        not_null,
        default,
        serial,
        compression,
        checks,
        uniques,
        pkey,
        fks,
        false,
    )
}

/// v1.97: `bypass_typed` serves ALTER TYPE ... CASCADE propagation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn alter_add_column_ext(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    col_type: &ColType,
    bit_typmod: Option<(i32, bool)>,
    not_null: bool,
    default: &Option<DefaultExpr>,
    serial: Option<SerialKind>,
    compression: &Option<String>,
    checks: &[CheckDef],
    uniques: &[UniqueDef],
    pkey: &Option<UniqueDef>,
    fks: &[FkDef],
    bypass_typed: bool,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    // v1.96 finishing pass: PG19 forbids this on typed tables
    // (the column set is owned by the OF type).
    if !bypass_typed && eng.db.typed_tables.contains_key(name) {
        return Err(exec_err(
            "42P16",
            "cannot add column to typed table".to_string(),
        ));
    }
    if t.column_index(col).is_some() {
        return Err(exec_err(
            "42701",
            format!("column \"{}\" of relation \"{}\" already exists", col, name),
        ));
    }
    // v0.77: temp tables have no global indexes; a shadowed permanent
    // table's index entries must never be touched by the row rewrite
    // below (row ids are globally unique so it would be a no-op, but
    // say what we mean).
    let is_temp = eng.db.is_temp_table(ctx.session, name);
    // Validate the default and check expressions up front.
    if let Some(d) = default {
        if let DefaultExpr::Expr(e) = d {
            validate_constraint_expr(e, "DEFAULT").map_err(sql_err)?;
        }
    }
    for c in checks {
        validate_constraint_expr(&c.expr, "CHECK").map_err(sql_err)?;
    }
    let has_rows = t
        .rows
        .iter()
        .any(|r| row_visible(r, ctx.snap, &ctx.all_xids));
    if not_null && default.is_none() && has_rows {
        return Err(exec_err(
            "23502",
            format!("column \"{}\" contains null values", col),
        ));
    }
    let mut next = t.clone();
    next.columns.push((col.to_string(), col_type.clone()));
    // v2.03: keep generated-column metadata parallel to `columns`.
    if next.gen_cols.len() < next.columns.len() {
        next.gen_cols.push(None);
    }
    // v1.93: keep the declared bit typmods parallel to `columns`
    // (padding a short vec like col_compression below).
    if next.bit_typmods.len() < next.columns.len() - 1 {
        next.bit_typmods.resize(next.columns.len() - 1, None);
    }
    next.bit_typmods.push(bit_typmod);
    // v1.41: the new column gets the next never-reused attnum (PG19
    // ATExecAddColumn: max(all attnums, incl. dropped)+1).
    next.attnums.push(next.next_attnum);
    next.next_attnum += 1;
    // v0.37: keep the per-column TOAST storage parallel to `columns`.
    next.col_storage.push(col_type.default_toast_storage());
    // v0.41: validate the COMPRESSION option like CREATE TABLE does.
    let method = parse_column_compression(col_type, compression.as_deref())?;
    if next.col_compression.len() < next.columns.len() - 1 {
        next.col_compression.resize(next.columns.len() - 1, None);
    }
    next.col_compression.push(method);
    next.not_null.push(not_null);
    // v0.65: ALTER TABLE ... ADD COLUMN <serial> creates the backing
    // sequence (PG19 supports this via generateSerialExtraStmts). An
    // explicit DEFAULT is 42601 ("multiple default values specified"),
    // like CREATE TABLE. The create runs after `t`'s last use below
    // (it needs `&mut eng`).
    if serial.is_some() && default.is_some() {
        return Err(exec_err(
            "42601",
            format!(
                "multiple default values specified for column \"{}\" of table \"{}\"",
                col, name
            ),
        ));
    }
    let default = default.clone();
    let serial_kind = serial.filter(|_| default.is_none());
    next.defaults.push(default);
    next.checks.extend(checks.iter().cloned());
    next.uniques.extend(uniques.iter().cloned());
    if let Some(pk) = pkey {
        if next.pkey.is_some() {
            return Err(exec_err(
                "42P16",
                "multiple primary keys for table".to_string(),
            ));
        }
        next.pkey = Some(pk.clone());
    }
    // Validate FKs against the new table shape.
    let next_meta = TableMeta::of(&next);
    for fk in fks {
        validate_fk_def(eng, ctx, name, fk)?;
    }
    next.fks.extend(fks.iter().cloned());
    // Rewrite every row with the new column value (default or NULL).
    // Reuse the old row ids so existing indexes stay valid; the WAL
    // replays them as InsertRows. Copy the visible rows out first; the
    // borrow of `t` must end before eval_default.
    let old_cols = t.columns.len();
    let old_rows: Vec<(u64, Row, Vec<u32>)> = t
        .rows
        .iter()
        .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
        .map(|r| (r.id, r.values.clone(), r.toast.clone()))
        .collect();
    // v0.65: create the serial backing sequence now that `t`'s borrow
    // has ended, and point the new column's default at it.
    if let Some(kind) = serial_kind {
        let seq_name = create_serial_sequence(eng, ctx, name, col, kind, None)?;
        *next
            .defaults
            .last_mut()
            .expect("new column default just pushed") = Some(DefaultExpr::Nextval(seq_name));
    }
    // v0.42: a rewrite must mint fresh row ids. Reusing the old ids
    // violates the "globally unique, never reused" invariant: the old
    // table version still holds rows with those ids, so UPDATE/DELETE
    // misread the new version as a concurrent update (40001). PG's own
    // rewrite is a DELETE+INSERT with new ctids; this is the same shape.
    let new_ids: Vec<u64> = (0..old_rows.len()).map(|_| eng.alloc_row_id()).collect();
    let mut new_rows = Vec::with_capacity(old_rows.len());
    for ((old_id, values, old_toast), new_id) in old_rows.into_iter().zip(new_ids) {
        // v0.65: the new column's default is the one just pushed.
        let dv = match next.defaults.last() {
            Some(Some(d)) => eval_default(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                d,
                col_type,
                col,
            )?,
            _ => Value::Null,
        };
        let mut nv = Vec::with_capacity(old_cols + 1);
        nv.extend_from_slice(&values);
        // Defensive: tolerate rows narrower than the old schema (shouldn't happen).
        while nv.len() < old_cols {
            nv.push(Value::Null);
        }
        nv.push(dv);
        // New CHECK constraints must hold for existing rows.
        check_row_constraints(
            eng,
            ctx.snap,
            ctx.own,
            ctx.session,
            ctx.role,
            &next_meta,
            name,
            &nv,
        )?;
        new_rows.push({
            let mut rv = RowVersion::plain(new_id, Row::new(nv), ctx.write_xid);
            // v0.41: ADD COLUMN must not orphan TOAST metadata — carry
            // the per-cell value ids forward (PG keeps the toast
            // pointers across a rewrite); the new column's flag is 0.
            let mut flags = old_toast;
            flags.resize(old_cols, 0);
            flags.push(0);
            rv.toast = flags;
            // v0.42: the row id changed, so migrate surviving index
            // entries to the new id. The indexed key columns are
            // untouched (the new column is appended), so the keys are
            // identical; only the row id moves. v0.77: temp tables
            // have no global indexes — skip (index_insert_row below
            // already no-ops for temp tables via its session guard).
            if !is_temp {
                eng.db.index_remove_row(name, old_id, &values);
            }
            eng.db
                .index_insert_row(name, new_id, &rv.values, ctx.session);
            rv
        });
    }
    // Backing indexes for new unique/pkey constraints.
    let mut new_indexes: Vec<(String, Vec<String>, bool)> = Vec::new();
    for u in uniques {
        new_indexes.push((u.name.clone(), u.cols.clone(), u.nulls_not_distinct));
    }
    if let Some(pk) = pkey {
        new_indexes.push((pk.name.clone(), pk.cols.clone(), false));
    }
    // v0.37: adding the first toastable column earns the table a toast
    // table (like PG), so reltoastrelid is set even before any row is
    // toasted. The toast table's CREATE is its own write op, so
    // ROLLBACK removes it while alter_swap's AlterTable op restores the
    // old version (whose toast_relid is still 0).
    ensure_toast_table_eager(eng, ctx, name, &mut next);
    alter_swap(eng, ctx, name, None, next, Some(new_rows))?;
    for (ix_name, cols, nnd) in new_indexes {
        if eng.db.is_temp_table(ctx.session, name) {
            // v0.77: temp tables have no backing global indexes (v0.22);
            // duplicates are checked by scanning the rows.
            temp_unique_dup_check(eng, ctx, name, &ix_name, &cols, nnd)?;
        } else {
            create_constraint_index(eng, ctx, name, &ix_name, &cols, true, nnd)?;
        }
    }
    Ok(ExecResult::Command {
        tag: format!("ALTER TABLE"),
    })
}

pub(crate) fn alter_drop_column(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    alter_drop_column_ext(eng, ctx, name, col, cascade, false, false)
}

/// v1.97: partition-child propagation entry for DROP COLUMN: the
/// drop was already authorized at the partitioned parent, so the
/// inherited-column guard (PG's `recursing` case) is bypassed.
/// v1.97: DROP COLUMN propagation into a partition subtree (see
/// alter_drop_column_propagate).
fn drop_column_partition_tree(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    cascade: bool,
) -> Result<(), ExecError> {
    alter_drop_column_propagate(eng, ctx, name, col, cascade)?;
    let children: Vec<String> = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .and_then(|t| t.partition.as_ref())
        .map(|p| p.children.clone())
        .unwrap_or_default();
    for child in &children {
        if eng.db.is_temp_table(ctx.session, child) {
            continue;
        }
        drop_column_partition_tree(eng, ctx, child, col, cascade)?;
    }
    Ok(())
}

pub(crate) fn alter_drop_column_propagate(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    alter_drop_column_ext(eng, ctx, name, col, cascade, false, true)
}

/// v1.97: `bypass_typed` serves ALTER TYPE ... CASCADE propagation.
pub(crate) fn alter_drop_column_ext(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    cascade: bool,
    bypass_typed: bool,
    bypass_inherited: bool,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    // v1.96 finishing pass: PG19 forbids this on typed tables
    // (the column set is owned by the OF type).
    if !bypass_typed && eng.db.typed_tables.contains_key(name) {
        return Err(exec_err(
            "42P16",
            "cannot drop column from typed table".to_string(),
        ));
    }
    // v1.97: PG19 ATExecDropColumn — an inherited column cannot be
    // dropped in the child (attinhcount > 0, not recursing).
    if !bypass_inherited && ancestor_has_column(eng, ctx, t, col) {
        return Err(exec_err(
            "42P16",
            format!("cannot drop inherited column \"{}\"", col),
        ));
    }
    let ci = t.column_index(col).ok_or_else(|| {
        exec_err(
            "42703",
            format!("column \"{}\" of relation \"{}\" does not exist", col, name),
        )
    })?;
    // v1.80: a partitioned table's key column cannot be dropped (PG19
    // ATExecDropColumn), whether the key names it directly or through an
    // expression. v1.79 accepted it and left the key pointing at a column
    // that no longer existed.
    if let Some(p) = t.partition.as_ref().filter(|p| p.is_partitioned) {
        let in_key = p.key.iter().any(|k| match &k.expr {
            None => k.col == ci,
            Some(e) => {
                let mut refs = Vec::new();
                collect_col_refs(e, &mut refs);
                refs.iter().any(|(_, r)| r == col)
            }
        });
        if in_key {
            return Err(exec_err(
                "42P16",
                format!(
                    "cannot drop column \"{}\" because it is part of the partition key of relation \"{}\"",
                    col, name
                ),
            ));
        }
    }
    // v0.77: temp tables have no global indexes, no views can depend on
    // them, and no other catalog table's FKs reference them — a
    // same-named permanent table's catalog objects must not be touched
    // when the temp table shadows it.
    let is_temp = eng.db.is_temp_table(ctx.session, name);
    // Dependency scan.
    let mut dep_constraints: Vec<String> = Vec::new();
    for c in &t.checks {
        let mut refs = Vec::new();
        collect_col_refs(&c.expr, &mut refs);
        if refs.iter().any(|(_, r)| r == col) {
            dep_constraints.push(format!("constraint \"{}\"", c.name));
        }
    }
    for u in &t.uniques {
        if u.cols.iter().any(|c| c == col) {
            dep_constraints.push(format!("constraint \"{}\"", u.name));
        }
    }
    if let Some(pk) = &t.pkey {
        if pk.cols.iter().any(|c| c == col) {
            dep_constraints.push(format!("constraint \"{}\"", pk.name));
        }
    }
    for fk in &t.fks {
        if fk.cols.iter().any(|c| c == col) {
            dep_constraints.push(format!("constraint \"{}\"", fk.name));
        }
    }
    // Other tables' FKs referencing this column.
    let mut dep_fks: Vec<(String, String)> = Vec::new();
    if !is_temp {
        for (tname, vs) in &eng.db.tables {
            if tname == name {
                continue;
            }
            let Some(ot) = vs
                .iter()
                .find(|t| crate::storage::table_visible(t, ctx.snap, &ctx.all_xids))
            else {
                continue;
            };
            for fk in &ot.fks {
                if fk.ref_table != name {
                    continue;
                }
                // Empty ref_cols = references our pkey.
                let ref_cols: Vec<String> = if fk.ref_cols.is_empty() {
                    t.pkey.as_ref().map(|p| p.cols.clone()).unwrap_or_default()
                } else {
                    fk.ref_cols.clone()
                };
                if ref_cols.iter().any(|c| c == col) {
                    dep_fks.push((tname.clone(), fk.name.clone()));
                }
            }
        }
    }
    // Indexes on the column.
    let mut dep_indexes: Vec<String> = Vec::new();
    if !is_temp {
        for (ix_name, ix) in &eng.db.indexes {
            if ix.def.table != name {
                continue;
            }
            // v2.05: skip already-dropped index versions — the map
            // retains them for MVCC, and a same-named index dropped
            // with an earlier incarnation of this table otherwise
            // gets "dropped" again here and fails 42P01 in
            // drop_index_internal (indexing.sql idxpart_col1_idx).
            if !crate::storage::index_visible(&ix.def, ctx.snap, &ctx.all_xids) {
                continue;
            }
            if ix.def.col_names.iter().any(|c| c == col) {
                dep_indexes.push(ix_name.clone());
            }
        }
    }
    // Views reading the table.
    let mut dep_views: Vec<String> = Vec::new();
    if !is_temp {
        for (vname, vs) in &eng.db.views {
            if vs.iter().any(|v| {
                crate::storage::view_visible(v, ctx.snap, ctx.own)
                    && v.deps.iter().any(|d| d == name)
            }) {
                dep_views.push(vname.clone());
            }
        }
    }
    // v2.05: capture the RI index name now — `t`'s borrow must end
    // before the CASCADE mutations below.
    let ri_index_name: Option<String> = match &t.replica_identity {
        crate::storage::ReplicaIdentity::Index(n) => Some(n.clone()),
        _ => None,
    };
    // v2.05: generated columns whose generation expression references
    // the dropped column are NORMAL dependents (PG19 generated_*.out
    // gtest10: 2BP01). The dropped column's own generation expression
    // is not a dependent.
    let mut dep_gen: Vec<String> = Vec::new();
    for (i, g) in t.gen_cols.iter().enumerate() {
        if i == ci {
            continue;
        }
        if let Some((expr, _stored)) = g {
            let mut refs = Vec::new();
            collect_col_refs(expr, &mut refs);
            if refs.iter().any(|(_, r)| r == col) {
                dep_gen.push(t.columns[i].0.clone());
            }
        }
    }
    // v2.05: publications whose column list or row filter for this
    // table references the column are NORMAL dependents (PG19
    // publication.out testpub_tbl5: 2BP01 with DETAIL naming the
    // publication). (pub_name, in_column_list, in_row_filter)
    let tbl_key = crate::exec::publication::base_name(name).to_lowercase();
    let mut dep_pubs: Vec<(String, bool, bool)> = Vec::new();
    if !is_temp {
        for (pname, p) in &eng.db.publications {
            if let Some(spec) = p.table_specs.get(&tbl_key) {
                let in_cols = spec
                    .cols
                    .as_ref()
                    .is_some_and(|cs| cs.iter().any(|c| c.eq_ignore_ascii_case(col)));
                let in_filter = spec.filter.as_ref().is_some_and(|f| {
                    let mut refs = Vec::new();
                    collect_col_refs(f, &mut refs);
                    refs.iter().any(|(_, r)| r.eq_ignore_ascii_case(col))
                });
                if in_cols || in_filter {
                    dep_pubs.push((pname.clone(), in_cols, in_filter));
                }
            }
        }
    }
    // v2.05: PG19 dependency classes — this table's own CHECK /
    // UNIQUE / PK constraints and plain indexes on the column are
    // AUTO/INTERNAL dependents: they are dropped WITH the column
    // even under RESTRICT (alter_table.out atacc1 value / old_system
    // _table othercol), so dep_constraints / dep_indexes no longer
    // block; the unconditional drop code below removes them either
    // way. Only NORMAL dependents block: other tables' FKs, views,
    // generated expressions, publications.
    if !cascade
        && (!dep_fks.is_empty()
            || !dep_views.is_empty()
            || !dep_gen.is_empty()
            || !dep_pubs.is_empty())
    {
        if let Some((pname, _, _)) = dep_pubs.first() {
            let mut e = exec_err(
                "2BP01",
                format!(
                    "cannot drop column {} of table {} because other objects depend on it",
                    col,
                    crate::exec::publication::base_name(name)
                ),
            );
            e.detail = Some(format!(
                "publication of table {} in publication {} depends on column {} of table {}",
                crate::exec::publication::base_name(name),
                pname,
                col,
                crate::exec::publication::base_name(name)
            ));
            return Err(e);
        }
        let first = dep_fks
            .first()
            .map(|(_, f)| f.as_str())
            .or(dep_views.first().map(|s| s.as_str()))
            .or(dep_gen.first().map(|s| s.as_str()))
            .unwrap_or("?");
        return Err(exec_err(
            "2BP01",
            format!(
                "cannot drop column {} of table {} because {} depends on it",
                col, name, first
            ),
        ));
    }
    // Snapshot the table state and end `t`'s borrow before CASCADE
    // mutations (they need `eng` mutably).
    let mut next = t.clone();
    let old_rows: Vec<(u64, Row, Vec<u32>)> = t
        .rows
        .iter()
        .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
        .map(|r| (r.id, r.values.clone(), r.toast.clone()))
        .collect();
    let _ = t;
    // CASCADE: drop dependent objects.
    // v0.98: sequences OWNED BY this column are dropped with it (PG19:
    // the OWNED BY link is an AUTO dependency — the sequence goes away
    // with the column, with or without CASCADE).
    let own_sess = if is_temp { Some(ctx.session) } else { None };
    let owned_here: Vec<String> = eng
        .db
        .sequences
        .iter()
        .filter_map(|(sname, vs)| {
            vs.iter()
                .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
                .filter(|s| {
                    s.owned_by
                        .as_ref()
                        .is_some_and(|(t, c, ss)| t == name && c == col && *ss == own_sess)
                })
                .map(|_| sname.clone())
        })
        .collect();
    for sname in &owned_here {
        drop_serial_sequence(eng, ctx, sname)?;
    }
    // v2.05 CASCADE: publication dependents — strip the column from
    // the publication's column list; a row filter referencing the
    // column (or a list that becomes empty) removes the table entry
    // from the publication entirely (the pg_publication_rel row is
    // the dependent object). Under RESTRICT we already returned.
    if cascade {
        for (pname, _in_cols, in_filter) in &dep_pubs {
            let prev = eng.db.publications.get(pname).cloned();
            if let Some(p) = eng.db.publications.get_mut(pname) {
                if *in_filter {
                    p.tables.retain(|tn| {
                        crate::exec::publication::base_name(tn).to_lowercase() != tbl_key
                    });
                    p.table_specs.remove(&tbl_key);
                } else if let Some(spec) = p.table_specs.get_mut(&tbl_key) {
                    if let Some(cs) = spec.cols.as_mut() {
                        cs.retain(|c| !c.eq_ignore_ascii_case(col));
                        if cs.is_empty() {
                            // handled below via removal flag
                        }
                    }
                }
                let emptied = p
                    .table_specs
                    .get(&tbl_key)
                    .is_some_and(|s| s.cols.as_ref().is_some_and(|cs| cs.is_empty()));
                if emptied {
                    p.tables.retain(|tn| {
                        crate::exec::publication::base_name(tn).to_lowercase() != tbl_key
                    });
                    p.table_specs.remove(&tbl_key);
                }
            }
            ctx.writes.push(crate::storage::WriteOp::AlterPublication {
                name: pname.clone(),
                prev,
            });
        }
        // CASCADE: dependent generated columns lose their generation
        // expression (they keep their stored values and become plain
        // columns; PG drops the dependent — named divergence).
        for gname in &dep_gen {
            if let Some(pos) = next.columns.iter().position(|(n, _)| n == gname) {
                if let Some(slot) = next.gen_cols.get_mut(pos) {
                    *slot = None;
                }
            }
        }
    }
    // v2.05: when the column's AUTO drop removes the replica
    // identity index, the table's replica identity reverts to
    // DEFAULT (PG19: dropping the RI index resets relreplident).
    if let Some(ixname) = &ri_index_name {
        if dep_indexes.iter().any(|n| n == ixname) {
            next.replica_identity = crate::storage::ReplicaIdentity::Default;
        }
    }
    // Views first (they only read).
    for v in &dep_views {
        drop_view_internal(eng, ctx, v)?;
    }
    // This table's dependent constraints and their backing indexes.
    let drop_idx_for: Vec<String> = next
        .uniques
        .iter()
        .filter(|u| u.cols.iter().any(|c| c == col))
        .map(|u| u.name.clone())
        .chain(
            next.pkey
                .iter()
                .filter(|p| p.cols.iter().any(|c| c == col))
                .map(|p| p.name.clone()),
        )
        .collect();
    next.checks.retain(|c| {
        let mut refs = Vec::new();
        collect_col_refs(&c.expr, &mut refs);
        !refs.iter().any(|(_, r)| r == col)
    });
    next.uniques.retain(|u| !u.cols.iter().any(|c| c == col));
    if next
        .pkey
        .as_ref()
        .is_some_and(|p| p.cols.iter().any(|c| c == col))
    {
        next.pkey = None;
    }
    next.fks.retain(|fk| !fk.cols.iter().any(|c| c == col));
    // v0.77: temp tables have no backing global indexes — never touch
    // the global index map for them.
    if !is_temp {
        for ix in drop_idx_for {
            // v0.9: DROP the backing index for the dropped UNIQUE/PK constraint.
            // Use direct map removal (bypasses MVCC visibility which may miss
            // the index due to snapshot timing).
            if let Some(index) = eng.db.indexes.remove(&ix) {
                ctx.writes.push(WriteOp::DropIndex {
                    name: ix.clone(),
                    index,
                });
            }
        }
    }
    // Other tables' FKs referencing the column.
    for (tname, fk_name) in &dep_fks {
        alter_drop_constraint_internal(eng, ctx, tname, fk_name)?;
    }
    // Indexes on the column (non-constraint ones; constraint ones already dropped).
    for ix in &dep_indexes {
        if eng.db.indexes.contains_key(ix) {
            drop_index_internal(eng, ctx, ix)?;
        }
    }
    // Remaining indexes: shift positions after the dropped column.
    // v0.77: temp tables have no global indexes, so there is nothing
    // to shift.
    let mut index_defs: Vec<(String, Vec<usize>, Vec<String>)> = Vec::new();
    if !is_temp {
        for (ix_name, ix) in &eng.db.indexes {
            if ix.def.table != name {
                continue;
            }
            // v2.05: indexes already dropped above as dependents of
            // the column (marked dropped_xmax by this statement)
            // must not enter the shift/re-create pass — re-dropping
            // them failed 42P01 (a latent bug the old RESTRICT block
            // masked: it errored before reaching this code).
            if !crate::storage::index_visible(&ix.def, ctx.snap, &ctx.all_xids) {
                continue;
            }
            index_defs.push((
                ix_name.clone(),
                ix.def.cols.clone(),
                ix.def.col_names.clone(),
            ));
        }
    }
    // Now mutate the table shape.
    next.columns.remove(ci);
    // v1.41: remove the dropped column's attnum slot too — but do NOT
    // decrement next_attnum (PG never reuses dropped attnums).
    next.attnums.remove(ci);
    next.not_null.remove(ci);
    next.defaults.remove(ci);
    // v1.93: keep the declared bit typmods parallel too.
    if next.bit_typmods.len() > ci {
        next.bit_typmods.remove(ci);
    }
    // v0.37: keep the per-column TOAST storage parallel to `columns`.
    next.col_storage.remove(ci);
    // v2.03: keep generated-column metadata parallel too — leaving
    // it long made later writes index columns[] out of bounds (the
    // testpub_tbl5 crash in the v2.03 schedule run).
    if next.gen_cols.len() > ci {
        next.gen_cols.remove(ci);
    }
    // v0.41: keep the per-column COMPRESSION metadata parallel too.
    if next.col_compression.len() > ci {
        next.col_compression.remove(ci);
    }
    // v1.80: shift partition key positions like the index positions.
    // A partitioned table's own key is in its own column space. A leaf's
    // key is a copy of its parent's (parent column space, never consulted
    // for routing): DROP COLUMN propagation alters the parent first, so
    // re-copy the parent's now-shifted key. v1.79 shifted neither, and
    // the next routed INSERT or PARTITION OF read past the row.
    if let Some(p) = next.partition.as_mut() {
        if p.is_partitioned {
            for k in &mut p.key {
                if k.expr.is_none() && k.col > ci {
                    k.col -= 1;
                }
            }
        } else if let Some(parent_key) = p.parent.as_ref().and_then(|pn| {
            eng.db
                .find_table(pn, ctx.snap, &ctx.all_xids, ctx.session)
                .and_then(|pt| pt.partition.as_ref())
                .map(|pp| pp.key.clone())
        }) {
            p.key = parent_key;
        }
    }
    // CHECK expressions reference columns by name; nothing to shift.
    // Rewrite rows without the column. v0.42: mint fresh row ids (see
    // ADD COLUMN above) — the surviving indexes are dropped and
    // re-created from the rewritten rows below, so they pick the new
    // ids up automatically.
    let new_ids: Vec<u64> = (0..old_rows.len()).map(|_| eng.alloc_row_id()).collect();
    let mut new_rows = Vec::with_capacity(old_rows.len());
    for ((_, values, old_toast), new_id) in old_rows.into_iter().zip(new_ids) {
        let mut values = values.to_vec();
        let mut flags = old_toast;
        if values.len() > ci {
            values.remove(ci);
            // v0.41: DROP COLUMN keeps the surviving cells' TOAST value
            // ids (only the dropped column's flag goes away with it).
            if flags.len() > ci {
                flags.remove(ci);
            }
        }
        let mut rv = RowVersion::plain(new_id, Row::new(values), ctx.write_xid);
        flags.resize(rv.toast.len(), 0);
        rv.toast = flags;
        new_rows.push(rv);
    }
    // Shift index positions: drop each surviving index, rewrite the rows,
    // then re-create the index with positions adjusted, so undo (the
    // DropIndex+CreateIndex pair) and WAL (new def logged) stay correct.
    // v1.80: the drops must be logged BEFORE the row rewrite. The WAL
    // batch then reads DropIndex -> AlterTable/InsertRows -> CreateIndex,
    // and replay never indexes the rewritten, shorter rows through a stale
    // definition. v1.79 dropped after the rewrite; recovery panicked in
    // index_insert_row and the data directory never reopened.
    struct ShiftedIndex {
        name: String,
        cols: Vec<usize>,
        col_names: Vec<String>,
        unique: bool,
        internal: bool,
        // v2.08: NULLS NOT DISTINCT survives the rebuild.
        nulls_not_distinct: bool,
    }
    let mut shifted: Vec<ShiftedIndex> = Vec::with_capacity(index_defs.len());
    for (ix_name, cols, col_names) in index_defs {
        let snapshot = eng.db.indexes.get(&ix_name).cloned().expect("found above");
        drop_index_internal(eng, ctx, &ix_name)?;
        shifted.push(ShiftedIndex {
            name: ix_name,
            cols,
            col_names,
            unique: snapshot.def.unique,
            internal: snapshot.def.internal,
            nulls_not_distinct: snapshot.def.nulls_not_distinct,
        });
    }
    alter_swap(eng, ctx, name, None, next, Some(new_rows))?;
    for ShiftedIndex {
        name: ix_name,
        cols,
        col_names,
        unique,
        internal,
        nulls_not_distinct,
    } in shifted
    {
        let new_cols: Vec<usize> = cols
            .into_iter()
            .map(|p| if p > ci { p - 1 } else { p })
            .collect();
        let mut def = IndexDef::plain(
            ix_name.clone(),
            name.to_string(),
            new_cols,
            col_names,
            unique,
            internal,
            ctx.own,
        );
        def.nulls_not_distinct = nulls_not_distinct;
        let mut ix = Index::new(def);
        let t2 = eng
            .db
            .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("just altered");
        for r in &t2.rows {
            let key = ix.key_for(&r.values);
            ix.insert(key, r.id);
        }
        eng.db.indexes.insert(ix_name.clone(), ix);
        ctx.writes.push(WriteOp::CreateIndex { name: ix_name });
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// Drop an index by name, logging WriteOp::DropIndex. Used by ALTER TABLE
/// CASCADE paths.
pub(crate) fn drop_index_internal(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
) -> Result<(), ExecError> {
    let snapshot = eng
        .db
        .find_index(name, ctx.snap, &ctx.all_xids)
        .ok_or_else(|| exec_err("42P01", format!("index \"{}\" does not exist", name)))?
        .clone();
    eng.db
        .find_index_mut(name, ctx.snap, &ctx.all_xids)
        .expect("index still present")
        .def
        .dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropIndex {
        name: name.to_string(),
        index: snapshot,
    });
    Ok(())
}
/// True if a table already has a constraint with this name.
pub(crate) fn table_has_constraint(t: &Table, con: &str) -> bool {
    t.checks.iter().any(|c| c.name == con)
        || t.uniques.iter().any(|u| u.name == con)
        || t.pkey.as_ref().is_some_and(|p| p.name == con)
        || t.fks.iter().any(|f| f.name == con)
}

/// v0.77: duplicate check for ADD CONSTRAINT UNIQUE/PRIMARY KEY (or ADD
/// COLUMN with an inline unique/pkey constraint) on a temp table, which
/// has no backing global index (v0.22). Scans visible rows instead;
/// NULL keys are distinct, mirroring `create_constraint_index`.
pub(crate) fn temp_unique_dup_check(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    cname: &str,
    cols: &[String],
    // v2.08: NULLS NOT DISTINCT of the constraint being added.
    nulls_not_distinct: bool,
) -> Result<(), ExecError> {
    let t = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
    let positions: Vec<usize> = cols
        .iter()
        .map(|c| {
            t.column_index(c).ok_or_else(|| {
                exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", c, table),
                )
            })
        })
        .collect::<Result<_, _>>()?;
    let mut seen: Vec<IndexKey> = Vec::new();
    for r in t
        .rows
        .iter()
        .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
    {
        let key = temp_key(&positions, &r.values);
        // PG: NULLs are distinct in unique constraints, unless the
        // constraint is NULLS NOT DISTINCT (v2.08).
        if !nulls_not_distinct && key.0.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        if seen.contains(&key) {
            return Err(exec_err(
                "23505",
                format!(
                    "duplicate key value violates unique constraint \"{}\"",
                    cname
                ),
            ));
        }
        seen.push(key);
    }
    Ok(())
}

pub(crate) fn alter_add_constraint(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    check: &Option<CheckDef>,
    unique: &Option<UniqueDef>,
    pkey: &Option<UniqueDef>,
    fk: &Option<FkDef>,
    notnull: &Option<crate::sql::NotNullDef>,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    // v1.97: PG19 verifyNotNullPKCompatible — a NOT VALID
    // not-null constraint on a primary-key column, on the table
    // or any of its partitions, blocks ADD PRIMARY KEY (55000).
    if let Some(pk) = pkey {
        let mut candidates = vec![name.to_string()];
        candidates.extend(partition_descendants(eng, ctx, name));
        for cand in &candidates {
            let Some(ct) = eng
                .db
                .find_table(cand, ctx.snap, &ctx.all_xids, ctx.session)
            else {
                continue;
            };
            for col in &pk.cols {
                for c in &ct.checks {
                    if c.kind == crate::sql::CheckKind::NotNull && (c.not_valid || c.no_inherit) {
                        let mut refs = Vec::new();
                        crate::sql::collect_col_refs(&c.expr, &mut refs);
                        if refs.iter().any(|(_, r)| r == col) {
                            return Err(exec_err(
                                "55000",
                                format!("cannot create primary key on column \"{}\"", col),
                            ));
                        }
                    }
                }
            }
        }
    }
    // v1.96 finishing pass: PG19 ChooseConstraintName for unnamed
    // constraints ({table}_{col}_key / _fkey / _check, suffixed
    // until free on this table). Previously unnamed constraints
    // were stored with an empty name, so a later explicitly-named
    // add could not collide the way PG requires (tt9_c_key).
    let mut check = check.clone();
    let mut unique = unique.clone();
    let mut pkey = pkey.clone();
    let mut fk = fk.clone();
    {
        let taken = |n: &str| table_has_constraint(t, n);
        if let Some(u) = unique.as_mut() {
            if u.name.is_empty() {
                let base = format!(
                    "{}_{}_key",
                    name,
                    u.cols.first().cloned().unwrap_or_default()
                );
                let mut cand = base.clone();
                let mut k = 1;
                while taken(&cand) {
                    cand = format!("{}{}", base, k);
                    k += 1;
                }
                u.name = cand;
            }
        }
        if let Some(u) = pkey.as_mut() {
            if u.name.is_empty() {
                let base = format!("{}_pkey", name);
                let mut cand = base.clone();
                let mut k = 1;
                while taken(&cand) {
                    cand = format!("{}{}", base, k);
                    k += 1;
                }
                u.name = cand;
            }
        }
        if let Some(f) = fk.as_mut() {
            if f.name.is_empty() {
                // v2.05: PG19 FK naming (all columns, suffix on
                // "fkey", 63-byte truncation) — shared with CREATE.
                f.name = crate::sql::pg_fk_auto_name(name, &f.cols, taken);
            }
        }
        if let Some(c) = check.as_mut() {
            if c.name.is_empty() {
                let base = format!("{}_check", name);
                let mut cand = base.clone();
                let mut k = 1;
                while taken(&cand) {
                    cand = format!("{}{}", base, k);
                    k += 1;
                }
                c.name = cand;
            }
        }
    }
    // v1.96 finishing pass: PG19 — a table can have only one
    // primary key (also covers an inherited pkey on the child).
    if pkey.is_some() && t.pkey.is_some() {
        return Err(exec_err(
            "42P16",
            format!(
                "multiple primary keys for table \"{}\" are not allowed",
                name
            ),
        ));
    }
    let check = &check;
    let unique = &unique;
    let pkey = &pkey;
    let fk = &fk;
    // v1.96 finishing pass: PG19 constraint names are unique per
    // relation across all kinds (tablecmds.c ConstraintNameExists):
    // adding a CHECK/UNIQUE/PK/FK whose name the table already
    // carries is an error — except an identical re-add of an
    // inherited definition, which merges. (The earlier global-scan
    // version false-fired on other tables' and dropped versions'
    // constraints.)
    {
        let cname = check
            .as_ref()
            .map(|c| c.name.clone())
            .or_else(|| unique.as_ref().map(|u| u.name.clone()))
            .or_else(|| pkey.as_ref().map(|u| u.name.clone()))
            .or_else(|| fk.as_ref().map(|f| f.name.clone()));
        if let Some(cname) = cname {
            let same_check = |c: &CheckDef| {
                check
                    .as_ref()
                    .map(|n| n.name == c.name && format!("{:?}", n.expr) == format!("{:?}", c.expr))
                    .unwrap_or(false)
            };
            let same_unique = |u: &UniqueDef| {
                unique
                    .as_ref()
                    .map(|n| n.name == u.name && n.cols == u.cols)
                    .unwrap_or(false)
                    || pkey
                        .as_ref()
                        .map(|n| n.name == u.name && n.cols == u.cols)
                        .unwrap_or(false)
            };
            let same_fk = |f: &FkDef| {
                fk.as_ref()
                    .map(|n| n.name == f.name && n.cols == f.cols && n.ref_table == f.ref_table)
                    .unwrap_or(false)
            };
            let dup = t.checks.iter().any(|c| c.name == cname && !same_check(c))
                || t.uniques.iter().any(|u| u.name == cname && !same_unique(u))
                || t.fks.iter().any(|f| f.name == cname && !same_fk(f))
                || t.pkey
                    .as_ref()
                    .map(|u| u.name == cname && !same_unique(u))
                    .unwrap_or(false);
            if dup {
                return Err(exec_err(
                    "42P16",
                    format!(
                        "constraint \"{}\" for relation \"{}\" already exists",
                        cname, name
                    ),
                ));
            }
        }
    }
    let mut next = t.clone();
    let meta = TableMeta::of(&next);
    // v0.76: `ADD CONSTRAINT name NOT NULL col [NOT VALID]`. With NOT
    // VALID, existing rows are not validated (PG19). Without it, verify
    // no existing visible row has a NULL in the column. (Done here, before
    // the CHECK validation below, so the `t` borrow ends before
    // `check_row_constraints` needs `&mut eng`.)
    let notnull_to_add: Option<CheckDef> = if let Some(n) = notnull {
        if table_has_constraint(&next, &n.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", n.name),
            ));
        }
        // v1.97: PG19 — NO INHERIT not-null constraints are not
        // allowed on partitioned tables.
        if n.no_inherit && t.partition.as_ref().is_some_and(|p| p.is_partitioned) {
            return Err(exec_err(
                "42P16",
                "not-null constraints on partitioned tables cannot be NO INHERIT".to_string(),
            ));
        }
        // v1.97: PG19 — adding an inheritable not-null constraint
        // over a column that already has a NO INHERIT one fails
        // ("cannot change NO INHERIT status ...").
        if !n.no_inherit {
            for c in &t.checks {
                if c.kind == crate::sql::CheckKind::NotNull && c.no_inherit {
                    let mut refs: Vec<(Option<String>, String)> = Vec::new();
                    crate::sql::collect_col_refs(&c.expr, &mut refs);
                    if refs.iter().any(|(_, r)| r == &n.col) {
                        return Err(exec_err(
                            "42P16",
                            format!(
                                "cannot change NO INHERIT status of NOT NULL constraint \"{}\" on relation \"{}\"",
                                c.name, name
                            ),
                        ));
                    }
                }
            }
        }
        let col_idx = meta.column_index(&n.col).ok_or_else(|| {
            exec_err(
                "42703",
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    n.col, name
                ),
            )
        })?;
        if !n.not_valid {
            for r in t
                .rows
                .iter()
                .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
            {
                if r.values.get(col_idx) == Some(&crate::storage::Value::Null) {
                    return Err(exec_err(
                        "23502",
                        format!("column \"{}\" contains null values", n.col),
                    ));
                }
            }
        }
        // Record as a check-like constraint for DROP CONSTRAINT
        // visibility. check_row_constraints evaluates it on every future
        // write, so NOT NULL is enforced going forward (like PG19);
        // NOT VALID only skipped the existing-row scan above.
        Some(CheckDef {
            name: n.name.clone(),
            expr: crate::sql::Expr::IsNull {
                expr: Box::new(crate::sql::Expr::Column {
                    table: None,
                    name: n.col.clone(),
                }),
                neg: true,
            },
            not_valid: n.not_valid,
            kind: crate::sql::CheckKind::NotNull,
            no_inherit: n.no_inherit,
            enforced: true,
        })
    } else {
        None
    };
    if let Some(c) = check {
        validate_constraint_expr(&c.expr, "CHECK").map_err(sql_err)?;
        if table_has_constraint(&next, &c.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", c.name),
            ));
        }
        let mut refs = Vec::new();
        collect_col_refs(&c.expr, &mut refs);
        for (_, r) in &refs {
            if meta.column_index(r).is_none() {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" does not exist", r),
                ));
            }
        }
        // Existing rows must satisfy the new CHECK.
        let mut probe = next.clone();
        probe.checks.push(c.clone());
        let probe_meta = TableMeta::of(&probe);
        let old_rows: Vec<Row> = t
            .rows
            .iter()
            .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
            .map(|r| r.values.clone())
            .collect();
        for values in &old_rows {
            check_row_constraints(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                &probe_meta,
                name,
                values,
            )?;
        }
        next.checks.push(c.clone());
    }
    // v0.76: push the NOT NULL check-like constraint (validated above).
    if let Some(nn) = notnull_to_add {
        next.checks.push(nn);
    }
    if let Some(u) = unique {
        if table_has_constraint(&next, &u.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", u.name),
            ));
        }
        for c in &u.cols {
            if next.column_index(c).is_none() {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", c, name),
                ));
            }
        }
        next.uniques.push(u.clone());
        alter_swap(eng, ctx, name, None, next, None)?;
        if eng.db.is_temp_table(ctx.session, name) {
            // v0.77: temp tables have no backing global indexes (v0.22);
            // duplicates are checked by scanning the rows.
            temp_unique_dup_check(eng, ctx, name, &u.name, &u.cols, u.nulls_not_distinct)?;
        } else {
            create_constraint_index(eng, ctx, name, &u.name, &u.cols, true, u.nulls_not_distinct)?;
        }
        return Ok(ExecResult::Command {
            tag: "ALTER TABLE".to_string(),
        });
    }
    if let Some(pk) = pkey {
        if table_has_constraint(&next, &pk.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", pk.name),
            ));
        }
        if next.pkey.is_some() {
            return Err(exec_err(
                "42P16",
                "multiple primary keys for table".to_string(),
            ));
        }
        for c in &pk.cols {
            if next.column_index(c).is_none() {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", c, name),
                ));
            }
        }
        // v2.05: PG19 — ADD PRIMARY KEY implies NOT NULL on the key
        // columns (and fails 23502 when existing rows hold NULL).
        // Without this, REPLICA IDENTITY USING INDEX on an
        // ALTER-added PK always failed 42809 "column is nullable".
        for c in &pk.cols {
            let ci = next.column_index(c).expect("checked above");
            for r in next
                .rows
                .iter()
                .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
            {
                if r.values.get(ci) == Some(&crate::storage::Value::Null) {
                    return Err(exec_err(
                        "23502",
                        format!(
                            "column \"{}\" of relation \"{}\" contains null values",
                            c, name
                        ),
                    ));
                }
            }
            if let Some(nn) = next.not_null.get_mut(ci) {
                *nn = true;
            }
        }
        next.pkey = Some(pk.clone());
        alter_swap(eng, ctx, name, None, next, None)?;
        if eng.db.is_temp_table(ctx.session, name) {
            // v0.77: temp tables have no backing global indexes (v0.22).
            temp_unique_dup_check(eng, ctx, name, &pk.name, &pk.cols, false)?;
        } else {
            create_constraint_index(eng, ctx, name, &pk.name, &pk.cols, true, false)?;
        }
        return Ok(ExecResult::Command {
            tag: "ALTER TABLE".to_string(),
        });
    }
    if let Some(f) = fk {
        if table_has_constraint(&next, &f.name) {
            return Err(exec_err(
                "42710",
                format!("constraint \"{}\" already exists", f.name),
            ));
        }
        validate_fk_def(eng, ctx, name, f)?;
        next.fks.push(f.clone());
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// Drop a single constraint by name on a table (used by DROP CONSTRAINT and
/// by CASCADE paths). Returns the dropped constraint's backing index name,
/// if any.
pub(crate) fn alter_drop_constraint_internal(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    con: &str,
) -> Result<(), ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let mut next = t.clone();
    let mut backing_index: Option<String> = None;
    let mut found = false;
    // v0.70: partition children carry per-table backing index names
    // (see `constraint_index_name`).
    let is_child = next.partition.as_ref().is_some_and(|p| p.parent.is_some());
    if let Some(pos) = next.checks.iter().position(|c| c.name == con) {
        next.checks.remove(pos);
        found = true;
    }
    if !found {
        if let Some(pos) = next.uniques.iter().position(|u| u.name == con) {
            backing_index = Some(constraint_index_name(name, con, is_child));
            next.uniques.remove(pos);
            found = true;
        }
    }
    if !found {
        if next.pkey.as_ref().is_some_and(|p| p.name == con) {
            backing_index = Some(constraint_index_name(name, con, is_child));
            next.pkey = None;
            found = true;
        }
    }
    if !found {
        if let Some(pos) = next.fks.iter().position(|f| f.name == con) {
            next.fks.remove(pos);
            found = true;
        }
    }
    if !found {
        return Err(exec_err(
            "42704",
            format!(
                "constraint \"{}\" of relation \"{}\" does not exist",
                con, name
            ),
        ));
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    // v0.77: temp tables have no backing global indexes — never drop a
    // same-named permanent table's index when the temp table shadows it.
    if !eng.db.is_temp_table(ctx.session, name) {
        if let Some(ix) = backing_index {
            if eng.db.find_index(&ix, ctx.snap, &ctx.all_xids).is_some() {
                drop_index_internal(eng, ctx, &ix)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn alter_drop_constraint(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    con: &str,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    // RESTRICT: refuse if other tables' FKs depend on this constraint
    // (unique/pkey backing an FK). CASCADE: drop those FKs too.
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let is_uniqueish =
        t.uniques.iter().any(|u| u.name == con) || t.pkey.as_ref().is_some_and(|p| p.name == con);
    // v0.77: temp tables are session-local — no other catalog table's
    // FK can reference one's constraints; skip the global scan so a
    // shadowed permanent table's FKs are never touched.
    if is_uniqueish && !eng.db.is_temp_table(ctx.session, name) {
        let mut dep_fks: Vec<(String, String)> = Vec::new();
        for (tname, vs) in &eng.db.tables {
            if tname == name {
                continue;
            }
            let Some(ot) = vs
                .iter()
                .find(|tt| crate::storage::table_visible(tt, ctx.snap, &ctx.all_xids))
            else {
                continue;
            };
            for fk in &ot.fks {
                if fk.ref_table == name {
                    dep_fks.push((tname.clone(), fk.name.clone()));
                }
            }
        }
        if !dep_fks.is_empty() && !cascade {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop constraint {} because other objects depend on it",
                    con
                ),
            ));
        }
        for (tname, fk_name) in &dep_fks {
            alter_drop_constraint_internal(eng, ctx, tname, fk_name)?;
        }
    }
    alter_drop_constraint_internal(eng, ctx, name, con)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v2.03: ALTER COLUMN SET/DROP NOT NULL (PG19 ATExecSetNotNull /
/// ATExecDropNotNull). SET validates existing visible rows (23502
/// when a NULL is present); a primary-key column cannot DROP its
/// NOT NULL (PG keeps PK columns not-null).
pub(crate) fn alter_set_not_null(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    set: bool,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let ci = t.column_index(col).ok_or_else(|| {
        exec_err(
            "42703",
            format!("column \"{}\" of relation \"{}\" does not exist", col, name),
        )
    })?;
    if set {
        for r in t
            .rows
            .iter()
            .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
        {
            if r.values.get(ci) == Some(&crate::storage::Value::Null) {
                return Err(exec_err(
                    "23502",
                    format!(
                        "column \"{}\" of relation \"{}\" contains null values",
                        col, name
                    ),
                ));
            }
        }
    } else if t
        .pkey
        .as_ref()
        .is_some_and(|p| p.cols.iter().any(|c| c == col))
    {
        return Err(exec_err(
            "0A000",
            format!(
                "column \"{}\" is in a primary key and cannot be dropped NOT NULL",
                col
            ),
        ));
    }
    let mut next = t.clone();
    if next.not_null.len() <= ci {
        next.not_null.resize(next.columns.len(), false);
    }
    next.not_null[ci] = set;
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v2.05: ALTER COLUMN c SET EXPRESSION AS (expr) / DROP EXPRESSION
/// [IF EXISTS] (PG19 tablecmds.c AT_SetExpression /
/// AT_DropExpression). A non-generated column is 55000
/// (ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE, "is not a generated
/// column"); DROP EXPRESSION IF EXISTS on one is a no-op success
/// (PG emits a NOTICE). SET on a STORED column rewrites live rows
/// with the new expression; DROP on a VIRTUAL column materializes
/// the computed values into the rows before clearing the marker.
pub(crate) fn alter_column_expression(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    new_expr: Option<crate::sql::Expr>,
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let ci = t
        .columns
        .iter()
        .position(|(n, _)| n == col)
        .ok_or_else(|| {
            exec_err(
                "42703",
                format!("column \"{}\" of relation \"{}\" does not exist", col, name),
            )
        })?;
    let cur = t.gen_cols.get(ci).cloned().flatten();
    let Some((_, stored)) = cur else {
        if new_expr.is_none() && if_exists {
            return Ok(ExecResult::Command {
                tag: "ALTER TABLE".to_string(),
            });
        }
        return Err(exec_err(
            "55000",
            format!(
                "column \"{}\" of relation \"{}\" is not a generated column",
                col, name
            ),
        ));
    };
    let mut next = t.clone();
    while next.gen_cols.len() < next.columns.len() {
        next.gen_cols.push(None);
    }
    match &new_expr {
        Some(e) => next.gen_cols[ci] = Some((e.clone(), stored)),
        None => next.gen_cols[ci] = None,
    }
    // Partition descendants: the leaves hold the rows. Their defs
    // do not carry gen_cols in this engine (the root's meta drives
    // generated computation for routed rows), so leaf row work
    // borrows the root's gen list and leaf catalogs stay untouched.
    let mut descendants: Vec<(String, crate::storage::Table)> = Vec::new();
    {
        let mut stack: Vec<String> = t
            .partition
            .as_ref()
            .map(|p| p.children.clone())
            .unwrap_or_default();
        while let Some(cname) = stack.pop() {
            if let Some(ct) = eng
                .db
                .find_table(&cname, ctx.snap, &ctx.all_xids, ctx.session)
                .cloned()
            {
                if let Some(p) = &ct.partition {
                    stack.extend(p.children.iter().cloned());
                }
                descendants.push((cname, ct));
            }
        }
    }
    // The rewrite revalidates each row: a SET EXPRESSION that
    // would produce NULL in a NOT NULL generated column fails
    // 23502 (generated_stored gtestnn_parent), as does a DROP
    // EXPRESSION materializing NULLs into a NOT NULL column.
    fn rewrite_rows(
        eng: &mut Engine,
        ctx: &mut StmtCtx,
        tname: &str,
        tdef: &crate::storage::Table,
        compute_meta: &TableMeta,
        validate_meta: &TableMeta,
        swap_def: crate::storage::Table,
    ) -> Result<(), ExecError> {
        let mut new_rows: Vec<crate::storage::RowVersion> = Vec::with_capacity(tdef.rows.len());
        for rv in &tdef.rows {
            if row_visible(rv, ctx.snap, &ctx.all_xids) {
                let mut cells = rv.values.to_vec();
                compute_generated_columns(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    compute_meta,
                    &mut cells,
                )?;
                check_row_constraints(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    validate_meta,
                    tname,
                    &cells,
                )?;
                new_rows.push(crate::storage::RowVersion {
                    values: crate::storage::Row::new(cells),
                    xmin: ctx.write_xid,
                    xmax: 0,
                    ..rv.clone()
                });
            } else {
                new_rows.push(rv.clone());
            }
        }
        alter_swap(eng, ctx, tname, None, swap_def, Some(new_rows))
    }
    if new_expr.is_some() && stored {
        // SET on STORED: recompute with the NEW expression.
        let meta = TableMeta::of(&next);
        rewrite_rows(eng, ctx, name, &t, &meta, &meta, next.clone())?;
        for (lname, ldef) in &descendants {
            let mut cdef = ldef.clone();
            cdef.gen_cols = next.gen_cols.clone();
            let meta = TableMeta::of(&cdef);
            rewrite_rows(eng, ctx, lname, ldef, &meta, &meta, ldef.clone())?;
        }
    } else if new_expr.is_none() && !stored {
        // DROP on VIRTUAL: materialize with the OLD expression.
        let meta_old = TableMeta::of(&t);
        let meta_new = TableMeta::of(&next);
        rewrite_rows(eng, ctx, name, &t, &meta_old, &meta_new, next.clone())?;
        for (lname, ldef) in &descendants {
            let mut cdef = ldef.clone();
            cdef.gen_cols = t.gen_cols.clone();
            let meta_old = TableMeta::of(&cdef);
            let meta_new = TableMeta::of(ldef);
            rewrite_rows(eng, ctx, lname, ldef, &meta_old, &meta_new, ldef.clone())?;
        }
    } else if new_expr.is_some() {
        // SET on VIRTUAL stores no values, but PG19 still
        // verifies the new expression against existing rows when
        // the column is NOT NULL (verify_new_notnull) — over the
        // named table and every partition leaf.
        if next.not_null.get(ci).copied().unwrap_or(false) {
            let mut cdef_owned: Vec<crate::storage::Table> = vec![next.clone()];
            for (_, ldef) in &descendants {
                let mut cdef = ldef.clone();
                cdef.gen_cols = next.gen_cols.clone();
                cdef_owned.push(cdef);
            }
            let mut defs: Vec<&crate::storage::Table> = vec![&t];
            defs.extend(descendants.iter().map(|(_, ldef)| ldef));
            for (tdef, cdef) in defs.iter().zip(cdef_owned.iter()) {
                let meta_new = TableMeta::of(cdef);
                for rv in &tdef.rows {
                    if row_visible(rv, ctx.snap, &ctx.all_xids) {
                        let mut cells = rv.values.to_vec();
                        compute_generated_columns(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            &meta_new,
                            &mut cells,
                        )?;
                        if matches!(cells.get(ci), Some(Value::Null)) {
                            return Err(exec_err(
                                "23502",
                                format!(
                                    "column \"{}\" of relation \"{}\" contains null values",
                                    col, name
                                ),
                            ));
                        }
                    }
                }
            }
        }
        alter_swap(eng, ctx, name, None, next, None)?;
    } else {
        alter_swap(eng, ctx, name, None, next, None)?;
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

pub(crate) fn alter_set_default(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    default: Option<DefaultExpr>,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    let ci = t.column_index(col).ok_or_else(|| {
        exec_err(
            "42703",
            format!("column \"{}\" of relation \"{}\" does not exist", col, name),
        )
    })?;
    if let Some(DefaultExpr::Expr(e)) = &default {
        validate_constraint_expr(e, "DEFAULT").map_err(sql_err)?;
        // v1.96: a NEW default expression needs USAGE on the types
        // it names in casts (PG19: new stored expressions require
        // USAGE; rebuilds of existing expressions do not).
        check_expr_cast_usages(eng, ctx, e)?;
    }
    let mut next = t.clone();
    next.defaults[ci] = default;
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v1.97: all tables that inherit from (or are partitions of)
/// `name`, transitively — PG19 renameatt recurses to every
/// inheritor. Names are catalog keys of permanent tables.
/// v1.97: all partition descendants (recursive partition
/// children, not inheritance children) of a table.
pub(crate) fn partition_descendants(eng: &Engine, ctx: &StmtCtx, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<String> = vec![name.to_string()];
    while let Some(cur) = stack.pop() {
        if let Some(t) = eng
            .db
            .find_table(&cur, ctx.snap, &ctx.all_xids, ctx.session)
        {
            if let Some(p) = &t.partition {
                for c in &p.children {
                    if !out.contains(c) {
                        out.push(c.clone());
                        stack.push(c.clone());
                    }
                }
            }
        }
    }
    out
}

pub(crate) fn rename_descendants(eng: &Engine, ctx: &StmtCtx, name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut frontier: Vec<String> = vec![name.to_string()];
    while let Some(cur) = frontier.pop() {
        let keys: Vec<String> = eng.db.tables.keys().cloned().collect();
        for k in keys {
            if k == name || out.contains(&k) {
                continue;
            }
            let is_child = eng
                .db
                .find_table(&k, ctx.snap, &ctx.all_xids, ctx.session)
                .is_some_and(|t| {
                    t.inherits.iter().any(|p| p == &cur)
                        || t.partition.as_ref().and_then(|p| p.parent.as_deref())
                            == Some(cur.as_str())
                });
            if is_child {
                out.push(k.clone());
                frontier.push(k);
            }
        }
    }
    out
}

/// Whether any ancestor (inherits / partition parent, transitively)
/// of table `t0` has a column named `col`.
fn ancestor_has_column(eng: &Engine, ctx: &StmtCtx, t0: &Table, col: &str) -> bool {
    let mut stack: Vec<String> = t0.inherits.clone();
    if let Some(p) = t0.partition.as_ref().and_then(|p| p.parent.clone()) {
        stack.push(p);
    }
    let mut seen: Vec<String> = Vec::new();
    while let Some(p) = stack.pop() {
        if seen.contains(&p) {
            continue;
        }
        seen.push(p.clone());
        if let Some(pt) = eng.db.find_table(&p, ctx.snap, &ctx.all_xids, ctx.session) {
            if pt.columns.iter().any(|(cn, _)| cn == col) {
                return true;
            }
            stack.extend(pt.inherits.iter().cloned());
            if let Some(pp) = pt.partition.as_ref().and_then(|x| x.parent.clone()) {
                stack.push(pp);
            }
        }
    }
    false
}

/// v1.97: rename a column in ONE table (no guards, no recursion):
/// dependent views are rewritten first (their stored query text
/// follows the new name, PG19 ruleutils semantics), then the
/// table's own checks/defaults/unique/pkey/fk lists, other tables'
/// FK reference lists, and index column names follow.
pub(crate) fn rename_column_one_table(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    old: &str,
    new: &str,
) -> Result<(), ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let is_temp = eng.db.is_temp_table(ctx.session, name);
    // v1.97: dependent views/matviews follow the rename by having
    // their stored query text rewritten (was: blanket 2BP01 refusal
    // since v0.77). Temp tables have no global views (v0.77 note).
    if !is_temp {
        rewrite_views_for_rename(eng, ctx, name, old, new)?;
    }
    let mut next = t;
    let ci = next.column_index(old).expect("checked by caller");
    next.columns[ci].0 = new.to_string();
    // Rename inside CHECK/DEFAULT expressions.
    for c in &mut next.checks {
        rename_col_in_expr(&mut c.expr, old, new);
    }
    for d in &mut next.defaults {
        if let Some(DefaultExpr::Expr(e)) = d {
            rename_col_in_expr(e, old, new);
        }
    }
    // Rename inside unique/pkey/fk column lists.
    for u in &mut next.uniques {
        for c in &mut u.cols {
            if c == old {
                *c = new.to_string();
            }
        }
    }
    if let Some(pk) = &mut next.pkey {
        for c in &mut pk.cols {
            if c == old {
                *c = new.to_string();
            }
        }
    }
    for fk in &mut next.fks {
        for c in &mut fk.cols {
            if c == old {
                *c = new.to_string();
            }
        }
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    if !is_temp {
        // Other tables' FKs that REFERENCE this column follow too
        // (PG19 renameatt updates the constraint's referenced side).
        let referrers: Vec<String> = eng
            .db
            .tables
            .keys()
            .filter(|k| k.as_str() != name)
            .cloned()
            .collect();
        for rname in referrers {
            let needs = eng
                .db
                .find_table(&rname, ctx.snap, &ctx.all_xids, ctx.session)
                .is_some_and(|ot| {
                    ot.fks
                        .iter()
                        .any(|fk| fk.ref_table == name && fk.ref_cols.iter().any(|c| c == old))
                });
            if !needs {
                continue;
            }
            let mut ot = eng
                .db
                .find_table(&rname, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("checked")
                .clone();
            for fk in &mut ot.fks {
                if fk.ref_table == name {
                    for c in &mut fk.ref_cols {
                        if c == old {
                            *c = new.to_string();
                        }
                    }
                }
            }
            alter_swap(eng, ctx, &rname, None, ot, None)?;
        }
        // Update index col_names. v0.77: temp tables have no global
        // indexes — skipped above via is_temp.
        let ix_names: Vec<String> = eng
            .db
            .indexes
            .iter()
            .filter(|(_, ix)| ix.def.table == name)
            .map(|(n, _)| n.clone())
            .collect();
        for ix_name in ix_names {
            if let Some(ix) = eng.db.indexes.get_mut(&ix_name) {
                for c in &mut ix.def.col_names {
                    if c == old {
                        *c = new.to_string();
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn alter_rename_column_ext(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    old: &str,
    new: &str,
    only: bool,
    top: bool,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    // v1.96 finishing pass: PG19 forbids this on typed tables
    // (the column set is owned by the OF type).
    if eng.db.typed_tables.contains_key(name) {
        return Err(exec_err(
            "42P16",
            "cannot rename column of typed table".to_string(),
        ));
    }
    if t.column_index(old).is_none() {
        return Err(exec_err(
            "42703",
            format!("column \"{}\" of relation \"{}\" does not exist", old, name),
        ));
    }
    if t.column_index(new).is_some() {
        return Err(exec_err(
            "42701",
            format!("column \"{}\" of relation \"{}\" already exists", new, name),
        ));
    }
    if top {
        // v1.97: PG19 renameatt_internal guards. ONLY with any
        // inheritance children fails first (the recurse=false
        // branch); a non-ONLY rename of a column the table itself
        // inherited fails as an inherited-column rename.
        let descendants = rename_descendants(eng, ctx, name);
        if only && !descendants.is_empty() {
            return Err(exec_err(
                "42P16",
                format!(
                    "inherited column \"{}\" must be renamed in child tables too",
                    old
                ),
            ));
        }
        if ancestor_has_column(eng, ctx, &t, old) {
            return Err(exec_err(
                "42P16",
                format!("cannot rename inherited column \"{}\"", old),
            ));
        }
        // v1.97 (PG19 renameatt_internal): a descendant that
        // inherits the column from MORE THAN ONE parent cannot
        // have it renamed through just this parent (inherit.sql
        // inht1/inhts: b arrives via both inht1 and inhs1).
        if !only {
            for child in &descendants {
                let Some(ct) = eng
                    .db
                    .find_table(child, ctx.snap, &ctx.all_xids, ctx.session)
                else {
                    continue;
                };
                let parents_with_col = ct
                    .inherits
                    .iter()
                    .filter(|p| {
                        eng.db
                            .find_table(p, ctx.snap, &ctx.all_xids, ctx.session)
                            .is_some_and(|pt| pt.column_index(old).is_some())
                    })
                    .count();
                if parents_with_col > 1 {
                    return Err(exec_err(
                        "42P16",
                        format!("cannot rename inherited column \"{}\"", old),
                    ));
                }
            }
        }
        rename_column_one_table(eng, ctx, name, old, new)?;
        if !only {
            for child in descendants {
                // A descendant reached through several paths may
                // already carry the new name; skip those without
                // the old column.
                let has_old = eng
                    .db
                    .find_table(&child, ctx.snap, &ctx.all_xids, ctx.session)
                    .is_some_and(|ct| ct.column_index(old).is_some());
                if has_old {
                    rename_column_one_table(eng, ctx, &child, old, new)?;
                }
            }
        }
        return Ok(ExecResult::Command {
            tag: "ALTER TABLE".to_string(),
        });
    }
    rename_column_one_table(eng, ctx, name, old, new)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

pub(crate) fn alter_rename_to(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    new_name: &str,
) -> Result<ExecResult, ExecError> {
    if eng
        .db
        .find_table(new_name, ctx.snap, &ctx.all_xids, ctx.session)
        .is_some()
        || eng.db.views.get(new_name).is_some_and(|vs| {
            vs.iter()
                .any(|v| crate::storage::view_visible(v, ctx.snap, ctx.own))
        })
    {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", new_name),
        ));
    }
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    // v0.77: a temp-table rename is session-local — never rewrite other
    // tables' FKs, view deps, or global indexes, which belong to the
    // permanent catalog (a shadowed same-named permanent table's
    // objects must not be touched).
    let is_temp = eng.db.is_temp_table(ctx.session, name);
    // Update FK ref_table in other tables that point at the old name.
    if !is_temp {
        let mut ref_tables: Vec<String> = Vec::new();
        for (tname, vs) in &eng.db.tables {
            if tname == name {
                continue;
            }
            if let Some(ot) = vs
                .iter()
                .find(|tt| crate::storage::table_visible(tt, ctx.snap, &ctx.all_xids))
            {
                if ot.fks.iter().any(|fk| fk.ref_table == name) {
                    ref_tables.push(tname.clone());
                }
            }
        }
        for tname in ref_tables {
            let ot = eng
                .db
                .find_table(&tname, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("found above")
                .clone();
            let mut onext = ot.clone();
            for fk in &mut onext.fks {
                if fk.ref_table == name {
                    fk.ref_table = new_name.to_string();
                }
            }
            alter_swap(eng, ctx, &tname, None, onext, None)?;
        }
        // Update view dependencies.
        let mut view_names: Vec<String> = Vec::new();
        for (vname, vs) in &eng.db.views {
            if vs.iter().any(|v| {
                crate::storage::view_visible(v, ctx.snap, ctx.own)
                    && v.deps.iter().any(|d| d == name)
            }) {
                view_names.push(vname.clone());
            }
        }
        for vname in view_names {
            if let Some(vs) = eng.db.views.get_mut(&vname) {
                if let Some(v) = vs
                    .iter_mut()
                    .find(|v| crate::storage::view_visible(v, ctx.snap, ctx.own))
                {
                    for d in &mut v.deps {
                        if d == name {
                            *d = new_name.to_string();
                        }
                    }
                }
            }
        }
    }
    alter_swap(eng, ctx, name, Some(new_name.to_string()), t, None)?;
    // Indexes were updated inside alter_swap; fix their table field via def.
    // v0.77: temp tables have no global indexes — skip.
    if !is_temp {
        for ix in eng.db.indexes.values_mut() {
            if ix.def.table == name {
                ix.def.table = new_name.to_string();
            }
        }
    }
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

// ---------------------------------------------------------------------------
// v2.06: ALTER TABLE ... ALTER CONSTRAINT (PG19 ATExecAlterConstraint)
// ---------------------------------------------------------------------------

/// Validate every visible row of `t` against FK `fk` (used when a
/// constraint becomes ENFORCED: PG19 validates existing rows and the
/// alter fails 23503 on the first violation).
fn validate_fk_rows(
    eng: &mut Engine,
    ctx: &StmtCtx,
    t: &Table,
    table: &str,
    fk: &FkDef,
) -> Result<(), ExecError> {
    let meta = TableMeta::of(t);
    let rows: Vec<Row> = t
        .rows
        .iter()
        .filter(|rv| row_visible(rv, ctx.snap, &ctx.all_xids))
        .map(|rv| rv.values.clone())
        .collect();
    for values in &rows {
        // Single-FK check with the flag forced on: build a one-FK
        // meta so check_fk_child_row judges only this constraint.
        let mut one = meta.clone();
        one.fks = vec![fk.clone()];
        check_fk_child_row(
            eng,
            ctx.snap,
            ctx.own,
            ctx.session,
            &one,
            table,
            values,
            &[],
            None,
        )?;
    }
    Ok(())
}

/// Validate every visible row of `t` against CHECK constraint `cd`
/// (PG19: 23514 "check constraint ... is violated by some row").
fn validate_check_rows(
    eng: &mut Engine,
    ctx: &StmtCtx,
    t: &Table,
    table: &str,
    cd: &CheckDef,
) -> Result<(), ExecError> {
    let rows: Vec<Row> = t
        .rows
        .iter()
        .filter(|rv| row_visible(rv, ctx.snap, &ctx.all_xids))
        .map(|rv| rv.values.clone())
        .collect();
    let qcols: Vec<QCol> = t
        .columns
        .iter()
        .enumerate()
        .map(|(i, (cn, ct))| QCol {
            qual: table.to_string(),
            name: cn.clone(),
            ty: ct.clone(),
            hidden: false,
            src_ord: i as u32,
        })
        .collect();
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: true,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        priv_role_override: None,
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: None,
        write: None,
    };
    for values in &rows {
        let frame = Scope {
            schema: &qcols,
            row: values,
            prov: None,
        };
        let v = eval_expr(&mut q, &[frame], &cd.expr)?;
        if matches!(v, Value::Bool(false)) {
            return Err(exec_err(
                "23514",
                format!(
                    "check constraint \"{}\" of relation \"{}\" is violated by some row",
                    cd.name, table
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn alter_alter_constraint(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    con: &str,
    deferrable: Option<bool>,
    initially: Option<bool>,
    enforced: Option<bool>,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let mut next = t.clone();
    if let Some(fk) = next.fks.iter_mut().find(|f| f.name == con) {
        if let Some(d) = deferrable {
            fk.deferrable = d;
            if !d {
                fk.initially_deferred = false;
            }
        }
        if let Some(i) = initially {
            fk.initially_deferred = i;
            if i {
                fk.deferrable = true;
            }
        }
        if let Some(e) = enforced {
            if e && !fk.enforced {
                // PG19: becoming ENFORCED validates existing rows.
                let mut fk_now = fk.clone();
                fk_now.enforced = true;
                validate_fk_rows(eng, ctx, &t, name, &fk_now)?;
                fk.not_valid = false;
            }
            fk.enforced = e;
        }
    } else if let Some(cd) = next.checks.iter_mut().find(|c| c.name == con) {
        if deferrable.is_some() || initially.is_some() {
            return Err(exec_err(
                "42809",
                format!(
                    "constraint \"{}\" of relation \"{}\" is not a foreign key constraint",
                    con, name
                ),
            ));
        }
        if let Some(e) = enforced {
            if e && (!cd.enforced || cd.not_valid) {
                let cd_now = cd.clone();
                if cd_now.kind == crate::sql::CheckKind::Check {
                    validate_check_rows(eng, ctx, &t, name, &cd_now)?;
                }
                cd.not_valid = false;
            }
            cd.enforced = e;
        }
    } else if t.uniques.iter().any(|u| u.name == con)
        || t.pkey.as_ref().is_some_and(|p| p.name == con)
    {
        // v2.07: PG19 ATExecAlterConstraint — deferrability can only
        // be altered on FOREIGN KEY constraints, and enforceability
        // only on FK/CHECK (tablecmds.c). Both clause families error
        // on a UNIQUE/PK constraint; there is no catalog update.
        if deferrable.is_some() || initially.is_some() {
            return Err(exec_err(
                "42809",
                format!(
                    "constraint \"{}\" of relation \"{}\" is not a foreign key constraint",
                    con, name
                ),
            ));
        }
        if enforced.is_some() {
            return Err(exec_err(
                "42809",
                format!(
                    "cannot alter enforceability of constraint \"{}\" of relation \"{}\"",
                    con, name
                ),
            ));
        }
    } else {
        return Err(exec_err(
            "42704",
            format!(
                "constraint \"{}\" of relation \"{}\" does not exist",
                con, name
            ),
        ));
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v2.07: PG19 parse_utilcmd.c transformIndexConstraints — an index
/// adopted by a PRIMARY KEY / UNIQUE constraint must have default
/// sorting behavior on every key column: no DESC / NULLS FIRST
/// options (pg_index.indoption == 0), the column's own collation, and
/// the type's default opclass. Anything else is 42809 with the
/// per-column message. Column collations are not otherwise modeled
/// on Table, so an explicitly named index collation other than
/// "default" counts as non-default here.
pub(crate) fn check_using_index_sorting(
    t: &Table,
    def: &crate::index::IndexDef,
) -> Result<(), ExecError> {
    use crate::storage::ColType;
    fn default_opclass(ct: &ColType) -> Option<&'static str> {
        Some(match ct {
            ColType::SmallInt => "int2_ops",
            ColType::Int | ColType::Regclass => "int4_ops",
            ColType::BigInt => "int8_ops",
            ColType::Float => "float8_ops",
            ColType::Float4 => "float4_ops",
            ColType::Numeric(_) => "numeric_ops",
            ColType::Text | ColType::Varchar(_) => "text_ops",
            ColType::Char(_) => "bpchar_ops",
            ColType::Bool => "bool_ops",
            ColType::Date => "date_ops",
            ColType::Timestamp => "timestamp_ops",
            ColType::Timestamptz => "timestamptz_ops",
            ColType::Time => "time_ops",
            ColType::TimeTz => "timetz_ops",
            ColType::Interval => "interval_ops",
            ColType::Uuid => "uuid_ops",
            ColType::Bytea => "bytea_ops",
            _ => return None,
        })
    }
    for (i, &ci) in def.cols.iter().enumerate() {
        let bad_sort = def.desc.get(i).copied().unwrap_or(false)
            || def.nulls_first.get(i).copied().unwrap_or(false);
        let bad_collate = matches!(
            def.collates.get(i),
            Some(Some(c)) if !c.eq_ignore_ascii_case("default")
        );
        let bad_opclass = match (
            def.opclasses.get(i),
            t.columns.get(ci).map(|(_, ct)| default_opclass(ct)),
        ) {
            (Some(Some(op)), Some(Some(dflt))) => !op.eq_ignore_ascii_case(dflt),
            _ => false,
        };
        if bad_sort || bad_collate || bad_opclass {
            return Err(exec_err(
                "42809",
                format!(
                    "index \"{}\" column number {} does not have default sorting behavior",
                    def.name,
                    i + 1
                ),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// v2.06: ALTER TABLE ... ADD PRIMARY KEY USING INDEX (PG19
// transformIndexConstraints / ATExecAddConstraint over an index)
// ---------------------------------------------------------------------------

pub(crate) fn alter_add_primary_key_using_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    index_name: &str,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    if t.pkey.is_some() {
        return Err(exec_err(
            "42P16",
            format!(
                "multiple primary keys for table \"{}\" are not allowed",
                name
            ),
        ));
    }
    let def = eng
        .db
        .visible_indexes_for(name, ctx.snap, &ctx.all_xids, ctx.session)
        .into_iter()
        .find(|ix| ix.def.name.eq_ignore_ascii_case(index_name))
        .map(|ix| ix.def.clone());
    let def = match def {
        Some(d) => d,
        None => {
            // PG19: an index by that name on ANOTHER table is a
            // different error than no such index at all.
            let elsewhere = eng
                .db
                .tables
                .keys()
                .filter(|tn| tn.as_str() != name)
                .any(|tn| {
                    eng.db
                        .visible_indexes_for(tn, ctx.snap, &ctx.all_xids, ctx.session)
                        .iter()
                        .any(|ix| ix.def.name.eq_ignore_ascii_case(index_name))
                });
            if elsewhere {
                return Err(exec_err(
                    "42809",
                    format!(
                        "index \"{}\" does not belong to table \"{}\"",
                        index_name, name
                    ),
                ));
            }
            return Err(exec_err(
                "42704",
                format!("index \"{}\" does not exist", index_name),
            ));
        }
    };
    if !def.unique {
        return Err(exec_err(
            "42809",
            format!("index \"{}\" is not unique", index_name),
        ));
    }
    // v2.08: PG19 catalog/index.c — an NND index cannot back a
    // PRIMARY KEY (42P16).
    if def.nulls_not_distinct {
        return Err(exec_err(
            "42P16",
            format!(
                "primary keys cannot use NULLS NOT DISTINCT indexes (index \"{}\")",
                index_name
            ),
        ));
    }
    check_using_index_sorting(&t, &def)?;
    if def
        .cols
        .iter()
        .any(|&ci| !t.not_null.get(ci).copied().unwrap_or(false))
    {
        // PG19: USING INDEX requires the columns to be NOT NULL
        // (it does not set them, unlike ADD PRIMARY KEY (cols)).
        return Err(exec_err(
            "42809",
            format!(
                "columns of index \"{}\" are not marked NOT NULL",
                index_name
            ),
        ));
    }
    let mut next = t.clone();
    next.pkey = Some(UniqueDef::plain(
        format!("{}_pkey", base_name(name)),
        def.col_names.clone(),
    ));
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

// ---------------------------------------------------------------------------
// v2.07: ALTER TABLE ... ADD UNIQUE USING INDEX (PG19
// transformIndexConstraints): the unique index becomes a UNIQUE
// constraint named after the index. Unlike PRIMARY KEY, the index
// columns need not be NOT NULL.
// ---------------------------------------------------------------------------

pub(crate) fn alter_add_unique_using_index(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    index_name: &str,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let def = eng
        .db
        .visible_indexes_for(name, ctx.snap, &ctx.all_xids, ctx.session)
        .into_iter()
        .find(|ix| ix.def.name.eq_ignore_ascii_case(index_name))
        .map(|ix| ix.def.clone());
    let def = match def {
        Some(d) => d,
        None => {
            let elsewhere = eng
                .db
                .tables
                .keys()
                .filter(|tn| tn.as_str() != name)
                .any(|tn| {
                    eng.db
                        .visible_indexes_for(tn, ctx.snap, &ctx.all_xids, ctx.session)
                        .iter()
                        .any(|ix| ix.def.name.eq_ignore_ascii_case(index_name))
                });
            if elsewhere {
                return Err(exec_err(
                    "42809",
                    format!(
                        "index \"{}\" does not belong to table \"{}\"",
                        index_name, name
                    ),
                ));
            }
            return Err(exec_err(
                "42704",
                format!("index \"{}\" does not exist", index_name),
            ));
        }
    };
    if !def.unique {
        return Err(exec_err(
            "42809",
            format!("index \"{}\" is not unique", index_name),
        ));
    }
    check_using_index_sorting(&t, &def)?;
    // PG19: the constraint takes the index's name; a name collision
    // with an existing constraint is 42710.
    if t.uniques.iter().any(|u| u.name == def.name)
        || t.pkey.as_ref().is_some_and(|p| p.name == def.name)
        || t.checks.iter().any(|c| c.name == def.name)
        || t.fks.iter().any(|f| f.name == def.name)
    {
        return Err(exec_err(
            "42710",
            format!(
                "constraint \"{}\" for relation \"{}\" already exists",
                def.name, name
            ),
        ));
    }
    let mut next = t.clone();
    // v2.08: the new constraint adopts the index's null treatment
    // (PG19: the constraint's index IS this index).
    let mut udef = UniqueDef::plain(def.name.clone(), def.col_names.clone());
    udef.nulls_not_distinct = def.nulls_not_distinct;
    next.uniques.push(udef);
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}
