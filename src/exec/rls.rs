// v1.95: row-level security (PG19 rewrite/rowsecurity.c,
// utils/misc/rls.c `check_enable_rls`, commands/policy.c).
//
// Policies live on `storage::Table` (`policies`, `rls_enabled`,
// `rls_forced`) and are evaluated here:
//
// - Visibility (SELECT / UPDATE / DELETE target rows, and every
//   non-target table scan): the applicable policies for the command
//   form one security-qual group — permissive USING exprs OR together,
//   restrictive USING exprs AND on top; no applicable permissive
//   policy means default-deny (no row is visible). UPDATE/DELETE
//   targets whose statement requires SELECT perms (a WHERE clause or
//   RETURNING) AND a second group built from the SELECT policies
//   (rowsecurity.c). SELECT ... FOR UPDATE (need_prov scans) ANDs an
//   UPDATE group the same way.
// - New-row checks (INSERT / UPDATE / COPY FROM): WITH CHECK exprs
//   (falling back to USING) combine identically; a failure is 42501
//   "new row violates row-level security policy ...".
//
// Exemptions (check_enable_rls): BYPASSRLS roles and superusers
// always bypass; the table owner bypasses unless the table is FORCE
// ROW LEVEL SECURITY. When RLS would apply but the session's
// `row_security` GUC is off, the statement errors 42501 instead
// ("query would be affected by row-level security policy ..."). The
// GUC reaches exec through a statement-scoped thread-local installed
// by the server (the v1.94 extra_float_digits pattern), defaulting
// to on.
use super::*;
use crate::sql::{PolicyCmd, PolicyDef};
use std::cell::{Cell, RefCell};

thread_local! {
    /// Session `row_security` GUC for the statement being executed.
    static ROW_SECURITY: Cell<bool> = const { Cell::new(true) };
    /// The connection's session user (login role), installed by the
    /// server per statement; `current_user`/`current_role` read
    /// Q.role, `session_user` reads this (falling back to Q.role when
    /// unset, e.g. in unit tests).
    static SESSION_USER: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn set_session_user(name: &str) {
    SESSION_USER.with(|s| *s.borrow_mut() = Some(name.to_string()));
}

pub(crate) fn session_user_name(fallback: &str) -> String {
    SESSION_USER.with(|s| s.borrow().clone().unwrap_or_else(|| fallback.to_string()))
}

/// Install the session's `row_security` GUC for one statement
/// (server.rs calls this next to the extra_float_digits install).
pub fn set_row_security(on: bool) {
    ROW_SECURITY.with(|c| c.set(on));
}

pub(crate) fn row_security_on() -> bool {
    ROW_SECURITY.with(|c| c.get())
}

/// Whether RLS applies to `t` for `role` under this snapshot. When it
/// would apply but `row_security` is off, this raises PG19's 42501
/// instead of returning true (rls.c `check_enable_rls`).
pub(crate) fn rls_applies(
    eng: &Engine,
    ctx_snap: &Snapshot,
    ctx_own: u64,
    t: &Table,
    table_name: &str,
    role: &str,
) -> Result<bool, ExecError> {
    if !t.rls_enabled {
        return Ok(false);
    }
    // BYPASSRLS users (superusers included) always bypass.
    if crate::storage::role_bypassrls_snap(&eng.db, role, ctx_snap, ctx_own) {
        return Ok(false);
    }
    // Owners bypass unless FORCE ROW LEVEL SECURITY.
    if t.owner == role && !t.rls_forced {
        return Ok(false);
    }
    if !row_security_on() {
        return Err(exec_err(
            "42501",
            format!(
                "query would be affected by row-level security policy for table \"{}\"",
                table_name
            ),
        ));
    }
    Ok(true)
}

/// The policies of `t` applicable to `role` for command `cmd`
/// (policy.c / rowsecurity.c `get_policies_for_relation`): the policy's
/// command is ALL or `cmd`, and its TO list is PUBLIC (empty) or names
/// the role or one of its groups (role_closure is transitive).
fn applicable_policies<'t>(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    t: &'t Table,
    cmd: PolicyCmd,
    role: &str,
) -> Vec<&'t PolicyDef> {
    let closure = crate::storage::role_closure(&eng.db, role, snap, own);
    t.policies
        .iter()
        .filter(|p| {
            (p.cmd == PolicyCmd::All || p.cmd == cmd)
                && (p.roles.is_empty() || p.roles.iter().any(|r| closure.iter().any(|c| c == r)))
        })
        .collect()
}

/// Evaluate one policy-expr group against a row: (OR of permissive)
/// AND (AND of restrictive). `expr_of` picks USING or WITH CHECK per
/// policy. An empty permissive set is default-deny (false). A policy
/// whose picked expr is absent counts as `true` (PG19: a NULL qual is
/// unrestricted).
fn eval_group(
    q: &mut Q,
    schema: &[QCol],
    cells: &[Value],
    policies: &[&PolicyDef],
    expr_of: impl Fn(&PolicyDef) -> Option<&Expr>,
) -> Result<bool, ExecError> {
    let frame = Scope {
        schema,
        row: cells,
        prov: None,
    };
    let scopes = [frame];
    let mut any_permissive = false;
    let mut permissive_ok = false;
    for p in policies.iter().filter(|p| p.permissive) {
        any_permissive = true;
        let v = match expr_of(p) {
            Some(e) => check_bool(eval_expr(q, &scopes, e)?, "policy")?,
            None => true,
        };
        if v {
            permissive_ok = true;
        }
    }
    if !any_permissive || !permissive_ok {
        return Ok(false);
    }
    for p in policies.iter().filter(|p| !p.permissive) {
        let v = match expr_of(p) {
            Some(e) => check_bool(eval_expr(q, &scopes, e)?, "policy")?,
            None => true,
        };
        if !v {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Filter `rows` (already MVCC-visible) through the USING groups for
/// `cmds` on table `t`. Each command contributes one ANDed group.
/// `t` is the table the rows came from (for a partitioned parent, the
/// parent — PG19 applies the parent's policies to its partitions).
pub(crate) fn rls_filter_qrows(
    q: &mut Q,
    t: &Table,
    table_name: &str,
    cmds: &[PolicyCmd],
    schema: &[QCol],
    rows: Vec<QRow>,
) -> Result<Vec<QRow>, ExecError> {
    if !rls_applies(q.eng, q.snap, q.own, t, table_name, q.role)? {
        return Ok(rows);
    }
    let groups: Vec<Vec<&PolicyDef>> = cmds
        .iter()
        .map(|c| applicable_policies(q.eng, q.snap, q.own, t, *c, q.role))
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let mut keep = true;
        for g in &groups {
            if !eval_group(q, schema, &row.cells, g, |p| p.using_expr.as_ref())? {
                keep = false;
                break;
            }
        }
        if keep {
            out.push(row);
        }
    }
    Ok(out)
}

/// Row-visibility test for one row against the USING groups of
/// `cmds` (used by UPDATE/DELETE target collection in dml.rs, which
/// builds its own Q). Returns true when RLS does not apply.
pub(crate) fn rls_visible_q(
    q: &mut Q,
    t: &Table,
    table_name: &str,
    cmds: &[PolicyCmd],
    schema: &[QCol],
    cells: &[Value],
) -> Result<bool, ExecError> {
    if !rls_applies(q.eng, q.snap, q.own, t, table_name, q.role)? {
        return Ok(true);
    }
    for c in cmds {
        let group = applicable_policies(q.eng, q.snap, q.own, t, *c, q.role);
        if !eval_group(q, schema, cells, &group, |p| p.using_expr.as_ref())? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Evaluate the USING group of `cmd` as a WITH CHECK against `cells`
/// (ON CONFLICT's existing-row test, PG19 WCO_RLS_CONFLICT_CHECK):
/// an invisible existing row is an error, not a silent skip.
pub(crate) fn rls_check_using_as_check(
    q: &mut Q,
    t: &Table,
    table_name: &str,
    cmd: PolicyCmd,
    schema: &[QCol],
    cells: &[Value],
) -> Result<(), ExecError> {
    if !rls_applies(q.eng, q.snap, q.own, t, table_name, q.role)? {
        return Ok(());
    }
    let group = applicable_policies(q.eng, q.snap, q.own, t, cmd, q.role);
    check_one_group(q, t, table_name, &group, schema, cells, true, |p| {
        p.using_expr.as_ref()
    })
}

/// WITH CHECK validation for one new row (INSERT/UPDATE/COPY).
/// `cmd` is Insert or Update. When `also_select` (RETURNING present),
/// the SELECT policies' USING exprs form an additional check group
/// (rowsecurity.c). `conflict_flavor` selects the ON CONFLICT error
/// text ("(USING expression)").
pub(crate) fn rls_check_new_row(
    q: &mut Q,
    t: &Table,
    table_name: &str,
    cmd: PolicyCmd,
    also_select: bool,
    conflict_flavor: bool,
    schema: &[QCol],
    cells: &[Value],
) -> Result<(), ExecError> {
    if !rls_applies(q.eng, q.snap, q.own, t, table_name, q.role)? {
        return Ok(());
    }
    // Group 1: the command's own policies, WITH CHECK (fallback USING).
    let group = applicable_policies(q.eng, q.snap, q.own, t, cmd, q.role);
    check_one_group(
        q,
        t,
        table_name,
        &group,
        schema,
        cells,
        conflict_flavor,
        |p| p.check_expr.as_ref().or(p.using_expr.as_ref()),
    )?;
    // Group 2: SELECT policies as checks (RETURNING requires the new
    // row to remain visible).
    if also_select {
        let sel = applicable_policies(q.eng, q.snap, q.own, t, PolicyCmd::Select, q.role);
        check_one_group(
            q,
            t,
            table_name,
            &sel,
            schema,
            cells,
            conflict_flavor,
            |p| p.using_expr.as_ref(),
        )?;
    }
    Ok(())
}

fn check_one_group(
    q: &mut Q,
    _t: &Table,
    table_name: &str,
    policies: &[&PolicyDef],
    schema: &[QCol],
    cells: &[Value],
    conflict_flavor: bool,
    expr_of: impl Fn(&PolicyDef) -> Option<&Expr>,
) -> Result<(), ExecError> {
    let frame = Scope {
        schema,
        row: cells,
        prov: None,
    };
    let scopes = [frame];
    let permissives: Vec<&&PolicyDef> = policies.iter().filter(|p| p.permissive).collect();
    let fail = |name: Option<&str>| -> ExecError {
        let msg = match (name, conflict_flavor) {
            (Some(n), true) => format!(
                "new row violates row-level security policy \"{}\" (USING expression) for table \"{}\"",
                n, table_name
            ),
            (Some(n), false) => format!(
                "new row violates row-level security policy \"{}\" for table \"{}\"",
                n, table_name
            ),
            (None, true) => format!(
                "new row violates row-level security policy (USING expression) for table \"{}\"",
                table_name
            ),
            (None, false) => format!(
                "new row violates row-level security policy for table \"{}\"",
                table_name
            ),
        };
        exec_err("42501", msg)
    };
    if permissives.is_empty() {
        return Err(fail(None));
    }
    let mut ok = false;
    for p in &permissives {
        let v = match expr_of(p) {
            Some(e) => check_bool(eval_expr(q, &scopes, e)?, "policy")?,
            None => true,
        };
        if v {
            ok = true;
            break;
        }
    }
    if !ok {
        // Name the policy when exactly one permissive policy was in
        // play (PG19 names the failing WCO's policy).
        let name = if permissives.len() == 1 {
            Some(permissives[0].name.as_str())
        } else {
            None
        };
        return Err(fail(name));
    }
    for p in policies.iter().filter(|p| !p.permissive) {
        let v = match expr_of(p) {
            Some(e) => check_bool(eval_expr(q, &scopes, e)?, "policy")?,
            None => true,
        };
        if !v {
            return Err(fail(Some(p.name.as_str())));
        }
    }
    Ok(())
}

/// rls_check_using_as_check for callers holding a StmtCtx.
pub(crate) fn rls_check_using_as_check_ctx(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    t: &Table,
    table_name: &str,
    cmd: PolicyCmd,
    schema: &[QCol],
    cells: &[Value],
) -> Result<(), ExecError> {
    if !rls_applies(eng, ctx.snap, ctx.own, t, table_name, ctx.role)? {
        return Ok(());
    }
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: ctx.read_only,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: None,
        write: Some(qwrite_from_ctx(
            &mut *ctx.writes,
            ctx.write_xid,
            ctx.level,
            ctx.default_toast_compression,
        )),
    };
    rls_check_using_as_check(&mut q, t, table_name, cmd, schema, cells)
}

/// rls_check_new_row for callers holding a StmtCtx (INSERT/COPY in
/// dml.rs): builds a Q from the statement context.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rls_check_new_row_ctx(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    t: &Table,
    table_name: &str,
    cmd: PolicyCmd,
    also_select: bool,
    conflict_flavor: bool,
    schema: &[QCol],
    cells: &[Value],
) -> Result<(), ExecError> {
    if !rls_applies(eng, ctx.snap, ctx.own, t, table_name, ctx.role)? {
        return Ok(());
    }
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: ctx.read_only,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: None,
        write: Some(qwrite_from_ctx(
            &mut *ctx.writes,
            ctx.write_xid,
            ctx.level,
            ctx.default_toast_compression,
        )),
    };
    rls_check_new_row(
        &mut q,
        t,
        table_name,
        cmd,
        also_select,
        conflict_flavor,
        schema,
        cells,
    )
}

// ---------------------------------------------------------------------------
// Policy DDL (commands/policy.c). Mutations stage through
// commit_table_version, exactly like CREATE/DROP TRIGGER.
// ---------------------------------------------------------------------------

/// Resolve the target table for policy DDL: 42P01 when missing, 42809
/// when the name is a view (policy.c: "\"%s\" is not a table").
fn policy_target_table(eng: &Engine, ctx: &StmtCtx, table: &str) -> Result<Table, ExecError> {
    if let Some(t) = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        return Ok(t.clone());
    }
    if eng.db.find_view(table, ctx.snap, ctx.own).is_some() {
        return Err(exec_err("42809", format!("\"{}\" is not a table", table)));
    }
    Err(exec_err(
        "42P01",
        format!("relation \"{}\" does not exist", table),
    ))
}

/// Validate a policy's TO roles exist (policy.c: 42704) and the
/// command/expression-shape rules (42601).
fn validate_policy_shape(
    eng: &Engine,
    ctx: &StmtCtx,
    cmd: PolicyCmd,
    roles: &[String],
    using_expr: &Option<Expr>,
    check_expr: &Option<Expr>,
    alter: bool,
) -> Result<(), ExecError> {
    for r in roles {
        if eng.db.find_role(r, ctx.snap, ctx.own).is_none() {
            return Err(exec_err("42704", format!("role \"{}\" does not exist", r)));
        }
    }
    match cmd {
        PolicyCmd::Insert => {
            if using_expr.is_some() {
                return Err(exec_err(
                    "42601",
                    "only WITH CHECK expression allowed for INSERT".to_string(),
                ));
            }
        }
        PolicyCmd::Select | PolicyCmd::Delete => {
            if check_expr.is_some() {
                return Err(if alter {
                    exec_err(
                        "42601",
                        "only USING expression allowed for SELECT, DELETE".to_string(),
                    )
                } else {
                    exec_err(
                        "42601",
                        "WITH CHECK cannot be applied to SELECT or DELETE".to_string(),
                    )
                });
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn exec_create_policy(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    table: &str,
    permissive: bool,
    cmd: PolicyCmd,
    roles: &[String],
    using_expr: &Option<Expr>,
    check_expr: &Option<Expr>,
    using_sql: &Option<String>,
    check_sql: &Option<String>,
) -> Result<ExecResult, ExecError> {
    require_table_owner(eng, ctx, table)?;
    let t = policy_target_table(eng, ctx, table)?;
    validate_policy_shape(eng, ctx, cmd, roles, using_expr, check_expr, false)?;
    if t.policies.iter().any(|p| p.name == name) {
        return Err(exec_err(
            "42710",
            format!("policy \"{}\" for table \"{}\" already exists", name, table),
        ));
    }
    let prev = t.clone();
    let mut next = t;
    next.policies.push(PolicyDef {
        name: name.to_string(),
        permissive,
        cmd,
        roles: roles.to_vec(),
        using_expr: using_expr.clone(),
        check_expr: check_expr.clone(),
        using_sql: using_sql.clone(),
        check_sql: check_sql.clone(),
    });
    commit_table_version(eng, ctx, table, prev, next);
    Ok(ExecResult::Command {
        tag: "CREATE POLICY".to_string(),
    })
}

pub(crate) fn exec_alter_policy(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    table: &str,
    rename_to: &Option<String>,
    roles: &Option<Vec<String>>,
    using_expr: &Option<Expr>,
    check_expr: &Option<Expr>,
    using_sql: &Option<String>,
    check_sql: &Option<String>,
    has_using: bool,
    has_check: bool,
) -> Result<ExecResult, ExecError> {
    require_table_owner(eng, ctx, table)?;
    let t = policy_target_table(eng, ctx, table)?;
    let idx = t
        .policies
        .iter()
        .position(|p| p.name == name)
        .ok_or_else(|| {
            exec_err(
                "42704",
                format!("policy \"{}\" for table \"{}\" does not exist", name, table),
            )
        })?;
    let prev = t.clone();
    let mut next = t;
    if let Some(new_name) = rename_to {
        if next.policies.iter().any(|p| p.name == *new_name) {
            return Err(exec_err(
                "42710",
                format!(
                    "policy \"{}\" for table \"{}\" already exists",
                    new_name, table
                ),
            ));
        }
        next.policies[idx].name = new_name.clone();
    } else {
        let pol = &next.policies[idx];
        let eff_using = if has_using {
            using_expr.clone()
        } else {
            pol.using_expr.clone()
        };
        let eff_check = if has_check {
            check_expr.clone()
        } else {
            pol.check_expr.clone()
        };
        let eff_roles = roles.clone().unwrap_or_else(|| pol.roles.clone());
        validate_policy_shape(eng, ctx, pol.cmd, &eff_roles, &eff_using, &eff_check, true)?;
        let pol = &mut next.policies[idx];
        pol.roles = eff_roles;
        pol.using_expr = eff_using;
        pol.check_expr = eff_check;
        if has_using {
            pol.using_sql = using_sql.clone();
        }
        if has_check {
            pol.check_sql = check_sql.clone();
        }
    }
    commit_table_version(eng, ctx, table, prev, next);
    Ok(ExecResult::Command {
        tag: "ALTER POLICY".to_string(),
    })
}

pub(crate) fn exec_drop_policy(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    table: &str,
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    require_table_owner(eng, ctx, table)?;
    // PG19: DROP POLICY on a missing table is 42P01 even with IF EXISTS
    // (IF EXISTS covers the policy, not the relation).
    let t = policy_target_table(eng, ctx, table)?;
    if !t.policies.iter().any(|p| p.name == name) {
        if if_exists {
            ctx.notices.push(format!(
                "policy \"{}\" for table \"{}\" does not exist, skipping",
                name, table
            ));
            return Ok(ExecResult::Command {
                tag: "DROP POLICY".to_string(),
            });
        }
        return Err(exec_err(
            "42704",
            format!("policy \"{}\" for table \"{}\" does not exist", name, table),
        ));
    }
    let prev = t.clone();
    let mut next = t;
    next.policies.retain(|p| p.name != name);
    commit_table_version(eng, ctx, table, prev, next);
    Ok(ExecResult::Command {
        tag: "DROP POLICY".to_string(),
    })
}

/// v1.95: ALTER TABLE {ENABLE|DISABLE|FORCE|NO FORCE} ROW LEVEL
/// SECURITY. Exactly one of `enabled` / `forced` is Some.
pub(crate) fn alter_set_rls(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    enabled: Option<bool>,
    forced: Option<bool>,
) -> Result<ExecResult, ExecError> {
    let t = policy_target_table(eng, ctx, name)?;
    let mut next = t;
    if let Some(e) = enabled {
        next.rls_enabled = e;
    }
    if let Some(f) = forced {
        next.rls_forced = f;
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}
