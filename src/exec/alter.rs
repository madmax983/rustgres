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
        Expr::Row(elems) => {
            for e in elems {
                rename_col_in_expr(e, old, new);
            }
        }
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
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
    for action in actions {
        exec_alter_one(eng, ctx, name, action)?;
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
) -> Result<ExecResult, ExecError> {
    // v0.11: every ALTER TABLE needs owner-or-superuser.
    require_table_owner(eng, ctx, name)?;
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
            not_null,
            default,
            serial,
            compression,
            checks,
            uniques,
            pkey,
            fks,
        } => {
            let res = alter_add_column(
                eng,
                ctx,
                name,
                col,
                col_type,
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
                    let child_action = AlterAction::DropColumn {
                        name: col.clone(),
                        cascade: *cascade,
                    };
                    exec_alter(eng, ctx, child, std::slice::from_ref(&child_action))?;
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
        AlterAction::RenameColumn { old, new } => alter_rename_column(eng, ctx, name, old, new),
        AlterAction::RenameTo { new_name } => alter_rename_to(eng, ctx, name, new_name),
        // v0.11
        AlterAction::OwnerTo { new_owner } => alter_owner_to(eng, ctx, name, new_owner),
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
        // v0.96: table inheritance (PG19).
        AlterAction::Inherit { parent } => alter_inherit(eng, ctx, name, parent, true),
        AlterAction::NoInherit { parent } => alter_inherit(eng, ctx, name, parent, false),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn alter_add_column(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    col: &str,
    col_type: &ColType,
    not_null: bool,
    default: &Option<DefaultExpr>,
    serial: Option<SerialKind>,
    compression: &Option<String>,
    checks: &[CheckDef],
    uniques: &[UniqueDef],
    pkey: &Option<UniqueDef>,
    fks: &[FkDef],
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
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
    let mut new_indexes: Vec<(String, Vec<String>)> = Vec::new();
    for u in uniques {
        new_indexes.push((u.name.clone(), u.cols.clone()));
    }
    if let Some(pk) = pkey {
        new_indexes.push((pk.name.clone(), pk.cols.clone()));
    }
    // v0.37: adding the first toastable column earns the table a toast
    // table (like PG), so reltoastrelid is set even before any row is
    // toasted. The toast table's CREATE is its own write op, so
    // ROLLBACK removes it while alter_swap's AlterTable op restores the
    // old version (whose toast_relid is still 0).
    ensure_toast_table_eager(eng, ctx, name, &mut next);
    alter_swap(eng, ctx, name, None, next, Some(new_rows))?;
    for (ix_name, cols) in new_indexes {
        if eng.db.is_temp_table(ctx.session, name) {
            // v0.77: temp tables have no backing global indexes (v0.22);
            // duplicates are checked by scanning the rows.
            temp_unique_dup_check(eng, ctx, name, &ix_name, &cols)?;
        } else {
            create_constraint_index(eng, ctx, name, &ix_name, &cols, true)?;
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
    if !cascade
        && (!dep_constraints.is_empty()
            || !dep_fks.is_empty()
            || !dep_indexes.is_empty()
            || !dep_views.is_empty())
    {
        let first = dep_constraints
            .first()
            .map(|s| s.as_str())
            .or(dep_fks.first().map(|(_, f)| f.as_str()))
            .or(dep_indexes.first().map(|s| s.as_str()))
            .or(dep_views.first().map(|s| s.as_str()))
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
    // v0.37: keep the per-column TOAST storage parallel to `columns`.
    next.col_storage.remove(ci);
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
        });
    }
    alter_swap(eng, ctx, name, None, next, Some(new_rows))?;
    for ShiftedIndex {
        name: ix_name,
        cols,
        col_names,
        unique,
        internal,
    } in shifted
    {
        let new_cols: Vec<usize> = cols
            .into_iter()
            .map(|p| if p > ci { p - 1 } else { p })
            .collect();
        let mut ix = Index::new(IndexDef::plain(
            ix_name.clone(),
            name.to_string(),
            new_cols,
            col_names,
            unique,
            internal,
            ctx.own,
        ));
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
        // PG: NULLs are distinct in unique constraints.
        if key.0.iter().any(|v| matches!(v, Value::Null)) {
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
            temp_unique_dup_check(eng, ctx, name, &u.name, &u.cols)?;
        } else {
            create_constraint_index(eng, ctx, name, &u.name, &u.cols, true)?;
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
        next.pkey = Some(pk.clone());
        alter_swap(eng, ctx, name, None, next, None)?;
        if eng.db.is_temp_table(ctx.session, name) {
            // v0.77: temp tables have no backing global indexes (v0.22).
            temp_unique_dup_check(eng, ctx, name, &pk.name, &pk.cols)?;
        } else {
            create_constraint_index(eng, ctx, name, &pk.name, &pk.cols, true)?;
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
    }
    let mut next = t.clone();
    next.defaults[ci] = default;
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

pub(crate) fn alter_rename_column(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    old: &str,
    new: &str,
) -> Result<ExecResult, ExecError> {
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
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
    // v0.77: temp tables are session-local — no global view can depend
    // on one, and a same-named permanent table's views/indexes must not
    // be touched.
    let is_temp = eng.db.is_temp_table(ctx.session, name);
    // Views reference columns by name in stored SQL; refuse rather than
    // silently breaking them.
    if !is_temp {
        for (vname, vs) in &eng.db.views {
            if vs.iter().any(|v| {
                crate::storage::view_visible(v, ctx.snap, ctx.own)
                    && v.deps.iter().any(|d| d == name)
            }) {
                return Err(exec_err(
                    "2BP01",
                    format!(
                        "cannot rename column {} because view {} depends on table {}",
                        old, vname, name
                    ),
                ));
            }
        }
    }
    let mut next = t.clone();
    let ci = next.column_index(old).expect("checked");
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
    // Update index col_names. v0.77: temp tables have no global
    // indexes — skip so a shadowed permanent table's indexes are
    // never touched.
    if !is_temp {
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
