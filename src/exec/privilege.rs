// v1.78 mechanical split: moved verbatim from src/exec.rs (271-400).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// v0.11: privilege checks. Denials are SQLSTATE 42501
// (insufficient_privilege), like PostgreSQL.
// ---------------------------------------------------------------------------

/// Require superuser (for role management).
pub(crate) fn require_superuser(eng: &Engine, ctx: &StmtCtx) -> Result<(), ExecError> {
    if crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        Ok(())
    } else {
        Err(exec_err(
            "42501",
            "permission denied: must be superuser to manage roles".to_string(),
        ))
    }
}

/// Require `privs` on a table. Missing tables are left alone so the
/// caller still reports 42P01 (like PostgreSQL, existence is checked
/// before privileges).
pub(crate) fn require_table_priv(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    privs: u32,
    priv_name: &str,
) -> Result<(), ExecError> {
    if let Some(t) = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        let have = crate::storage::table_privs(&eng.db, ctx.role, t, ctx.snap, ctx.own);
        if have & privs != privs {
            return Err(exec_err(
                "42501",
                format!(
                    "permission denied for table \"{}\" (needs {})",
                    table, priv_name
                ),
            ));
        }
    }
    Ok(())
}

/// v0.11: require `privs` on each of `columns`, satisfied by a
/// table-level grant or a column-level grant. Unknown columns are
/// skipped — the statement's own validation reports them as 42703.
pub(crate) fn require_column_privs(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    columns: &[String],
    privs: u32,
    priv_name: &str,
) -> Result<(), ExecError> {
    if let Some(t) = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
        for c in columns {
            if !t.columns.iter().any(|(n, _)| n == c) {
                continue;
            }
            let have = crate::storage::column_privs_in(
                &eng.db, ctx.role, t, c, &closure, ctx.snap, ctx.own,
            );
            if have & privs != privs {
                return Err(exec_err(
                    "42501",
                    format!(
                        "permission denied for column \"{}\" of table \"{}\" (needs {})",
                        c, table, priv_name
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Require owner-or-superuser on a table (DDL).
pub(crate) fn require_table_owner(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
) -> Result<(), ExecError> {
    if let Some(t) = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        if t.owner != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!("permission denied: must be owner of table \"{}\"", table),
            ));
        }
    }
    Ok(())
}

/// Require owner-or-superuser on a sequence (DDL).
pub(crate) fn require_seq_owner(eng: &Engine, ctx: &StmtCtx, seq: &str) -> Result<(), ExecError> {
    if let Some(s) = eng.db.find_sequence(seq, ctx.snap, ctx.own) {
        if s.owner != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!("permission denied: must be owner of sequence \"{}\"", seq),
            ));
        }
    }
    Ok(())
}

/// Require owner-or-superuser on a view (DDL).
pub(crate) fn require_view_owner(eng: &Engine, ctx: &StmtCtx, view: &str) -> Result<(), ExecError> {
    if let Some(v) = eng.db.find_view(view, ctx.snap, ctx.own) {
        if v.owner != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!("permission denied: must be owner of view \"{}\"", view),
            ));
        }
    }
    Ok(())
}
