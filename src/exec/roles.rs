// v1.78 mechanical split: moved verbatim from src/exec.rs (63457-64109).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

/// Dispatch for the nextval/currval/setval SQL functions (called from
/// eval_func with pre-evaluated args).

// ============================================================================
// v0.11: roles and privileges
// ============================================================================

/// v1.96 finishing pass: PG19 role-attribute snapshot for the
/// CREATE/ALTER/DROP ROLE permission rules (user.c).
pub(crate) struct RoleAttrs {
    pub superuser: bool,
    pub createrole: bool,
    pub createdb: bool,
    pub replication: bool,
    pub bypassrls: bool,
}

pub(crate) fn role_attrs(eng: &Engine, ctx: &StmtCtx, name: &str) -> RoleAttrs {
    match eng.db.find_role(name, ctx.snap, ctx.own) {
        Some(r) => RoleAttrs {
            superuser: r.superuser,
            createrole: r.createrole,
            createdb: r.createdb,
            replication: r.replication,
            bypassrls: r.bypassrls,
        },
        None => RoleAttrs {
            superuser: false,
            createrole: false,
            createdb: false,
            replication: false,
            bypassrls: false,
        },
    }
}

/// PG19 `is_admin_of_role`: a superuser is admin of every role;
/// otherwise an explicit membership edge with the ADMIN option.
pub(crate) fn is_admin_of_role(eng: &Engine, ctx: &StmtCtx, target: &str) -> bool {
    if crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return true;
    }
    eng.db
        .find_role(ctx.role, ctx.snap, ctx.own)
        .map(|r| r.memberships.iter().any(|m| m.role == target && m.admin))
        .unwrap_or(false)
}

/// PG19 `have_createrole_privilege`: CREATEROLE or superuser.
pub(crate) fn have_createrole(eng: &Engine, ctx: &StmtCtx) -> bool {
    crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        || role_attrs(eng, ctx, ctx.role).createrole
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    login: bool,
    superuser: bool,
    bypassrls: bool,
    inherit: bool,
    createrole: bool,
    createdb: bool,
    replication: bool,
    member_of: &[(String, bool)],
    members: &[(String, bool)],
    password: &Option<String>,
    connlimit: Option<i32>,
    valid_until: &Option<String>,
) -> Result<ExecResult, ExecError> {
    // v1.96 finishing pass: PG19 CreateRole permission rule — a
    // non-superuser needs CREATEROLE, and may only confer attributes
    // it itself holds (SUPERUSER requires superuser).
    {
        let me = role_attrs(eng, ctx, ctx.role);
        if !me.superuser {
            if !me.createrole {
                return Err(exec_err(
                    "42501",
                    "permission denied to create role".to_string(),
                ));
            }
            if superuser {
                return Err(exec_err(
                    "42501",
                    "permission denied to create role".to_string(),
                ));
            }
            if createdb && !me.createdb {
                return Err(exec_err(
                    "42501",
                    "permission denied to create role".to_string(),
                ));
            }
            if replication && !me.replication {
                return Err(exec_err(
                    "42501",
                    "permission denied to create role".to_string(),
                ));
            }
            if bypassrls && !me.bypassrls {
                return Err(exec_err(
                    "42501",
                    "permission denied to create role".to_string(),
                ));
            }
        }
    }
    if eng.db.find_role(name, ctx.snap, ctx.own).is_some() {
        return Err(exec_err(
            "42710",
            format!("role \"{}\" already exists", name),
        ));
    }
    if let Some(vu) = valid_until {
        crate::storage::check_valid_until(vu).map_err(|e| exec_err("22008", e))?;
    }
    let role = crate::storage::Role {
        name: name.to_string(),
        password: password
            .as_ref()
            .map(|pw| crate::crypto::verifier_for_password(pw)),
        can_login: login,
        superuser,
        bypassrls,
        inherit,
        createrole,
        createdb,
        replication,
        connlimit: connlimit.unwrap_or(-1),
        memberships: Vec::new(),
        valid_until: valid_until.clone(),
        created_xmin: ctx.write_xid,
        dropped_xmax: 0,
    };
    eng.db.roles.entry(name.to_string()).or_default().push(role);
    ctx.writes.push(WriteOp::CreateRole {
        name: name.to_string(),
    });
    // v1.96 finishing pass: PG19's implicit self-grant — a
    // non-superuser creator becomes a member of the new role with
    // ADMIN OPTION (grantor recorded as the bootstrap superuser);
    // the createrole_self_grant GUC additionally confers the INHERIT
    // and/or SET options named in its value.
    if !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        && ctx.role != name
        && eng.db.find_role(ctx.role, ctx.snap, ctx.own).is_some()
    {
        let guc = crate::exec::installed_guc("createrole_self_grant")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let inherit_opt = guc.split(',').any(|t| t.trim() == "inherit");
        let set_opt = guc.split(',').any(|t| t.trim() == "set");
        let creator = ctx.role.to_string();
        let new_role = name.to_string();
        alter_role_swap(eng, ctx, &creator, move |r| {
            r.memberships.push(crate::storage::RoleMembership {
                role: new_role,
                grantor: "postgres".to_string(),
                admin: true,
                inherit: inherit_opt,
                set: set_opt,
            });
        });
    }
    // v1.96: CREATE ROLE/GROUP membership clauses. IN ROLE adds the new
    // role to those groups; USER/ADMIN add members to the new group.
    for (group, admin) in member_of {
        if group == name {
            return Err(exec_err(
                "0A000",
                format!("role \"{}\" cannot be a member of itself", name),
            ));
        }
        if eng.db.find_role(group, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", group),
            ));
        }
        // v1.96 finishing pass: PG19 CreateRole adds the new role to
        // each IN ROLE group via check_role_membership_authorization —
        // the creator must hold ADMIN OPTION on the group (superusers
        // qualify implicitly). This is what bars CREATEROLE holders
        // from minting members of predefined or superuser roles.
        if !is_admin_of_role(eng, ctx, group) {
            return Err(exec_err(
                "42501",
                format!(
                    "permission denied: must have ADMIN OPTION on role \"{}\"",
                    group
                ),
            ));
        }
        let (g, a) = (group.clone(), *admin);
        let grantor = ctx.role.to_string();
        alter_role_swap(eng, ctx, name, move |r| {
            r.memberships.push(crate::storage::RoleMembership {
                role: g.clone(),
                grantor: grantor.clone(),
                admin: a,
                inherit: true,
                set: true,
            });
        });
    }
    for (member, admin) in members {
        if member == name {
            return Err(exec_err(
                "0A000",
                format!("role \"{}\" cannot be a member of itself", name),
            ));
        }
        if eng.db.find_role(member, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", member),
            ));
        }
        let group = name.to_string();
        let a = *admin;
        let grantor = ctx.role.to_string();
        alter_role_swap(eng, ctx, member, move |r| {
            r.memberships.push(crate::storage::RoleMembership {
                role: group.clone(),
                grantor: grantor.clone(),
                admin: a,
                inherit: true,
                set: true,
            });
        });
    }
    Ok(ExecResult::Command {
        tag: "CREATE ROLE".to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_alter_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    login: Option<bool>,
    superuser: Option<bool>,
    bypassrls: Option<bool>,
    inherit: Option<bool>,
    createrole: Option<bool>,
    createdb: Option<bool>,
    replication: Option<bool>,
    password: &Option<Option<String>>,
    connlimit: Option<i32>,
    valid_until: &Option<String>,
) -> Result<ExecResult, ExecError> {
    let live = eng
        .db
        .find_role(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42704", format!("role \"{}\" does not exist", name)))?
        .clone();
    // The bootstrap superuser cannot be de-superusered or renamed away
    // into uselessness: refuse to remove SUPERUSER from `postgres`.
    // (Documented deviation: real PostgreSQL lets you do this.)
    if live.name == "postgres" && superuser == Some(false) {
        return Err(exec_err(
            "42501",
            "permission denied: cannot remove superuser from role \"postgres\"".to_string(),
        ));
    }
    // v1.96 finishing pass: PG19 AlterRole permission rule. To alter a
    // superuser, or to touch the SUPERUSER attribute, you must be a
    // superuser. Most other changes require CREATEROLE plus ADMIN
    // OPTION on the target; without them only a password change to
    // one's own role is allowed. With them, changing CREATEDB /
    // REPLICATION / BYPASSRLS additionally requires holding that
    // attribute yourself.
    {
        let me = role_attrs(eng, ctx, ctx.role);
        if !me.superuser {
            if live.superuser || superuser.is_some() {
                return Err(exec_err(
                    "42501",
                    "permission denied to alter role".to_string(),
                ));
            }
            if !(have_createrole(eng, ctx) && is_admin_of_role(eng, ctx, name)) {
                let touches_attrs = inherit.is_some()
                    || createrole.is_some()
                    || createdb.is_some()
                    || login.is_some()
                    || connlimit.is_some()
                    || valid_until.is_some()
                    || replication.is_some()
                    || bypassrls.is_some();
                if touches_attrs {
                    return Err(exec_err(
                        "42501",
                        "permission denied to alter role".to_string(),
                    ));
                }
                if password.is_some() && ctx.role != name {
                    return Err(exec_err(
                        "42501",
                        "permission denied to alter role".to_string(),
                    ));
                }
            } else {
                if createdb.is_some() && !me.createdb {
                    return Err(exec_err(
                        "42501",
                        "permission denied to alter role".to_string(),
                    ));
                }
                if replication.is_some() && !me.replication {
                    return Err(exec_err(
                        "42501",
                        "permission denied to alter role".to_string(),
                    ));
                }
                if bypassrls.is_some() && !me.bypassrls {
                    return Err(exec_err(
                        "42501",
                        "permission denied to alter role".to_string(),
                    ));
                }
            }
        }
    }
    let prev = live.clone();
    let cur = eng
        .db
        .find_role_mut(name, ctx.snap, ctx.own)
        .expect("visible above");
    cur.dropped_xmax = ctx.own;
    let versions = eng.db.roles.get_mut(name).expect("visible above");
    let mut next = live;
    if let Some(l) = login {
        next.can_login = l;
    }
    if let Some(s) = superuser {
        next.superuser = s;
    }
    if let Some(b) = bypassrls {
        next.bypassrls = b;
    }
    if let Some(i) = inherit {
        next.inherit = i;
    }
    if let Some(c) = createrole {
        next.createrole = c;
    }
    if let Some(c) = createdb {
        next.createdb = c;
    }
    if let Some(r) = replication {
        next.replication = r;
    }
    if let Some(pw) = password {
        next.password = pw.as_ref().map(|p| crate::crypto::verifier_for_password(p));
    }
    if let Some(c) = connlimit {
        next.connlimit = c;
    }
    if let Some(vu) = valid_until {
        crate::storage::check_valid_until(vu).map_err(|e| exec_err("22008", e))?;
        next.valid_until = crate::storage::normalize_valid_until(vu);
    }
    next.created_xmin = ctx.own;
    next.dropped_xmax = 0;
    versions.push(next);
    ctx.writes.push(WriteOp::AlterRole {
        name: name.to_string(),
        prev,
    });
    Ok(ExecResult::Command {
        tag: "ALTER ROLE".to_string(),
    })
}

/// Swap the live version of role `name` for a mutated copy, WAL-logged
/// as AlterRole. The caller must have verified the role exists and is
/// visible under (snap, own). Returns the previous live version.
pub(crate) fn alter_role_swap(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    mutate: impl FnOnce(&mut crate::storage::Role),
) {
    let prev = eng
        .db
        .find_role(name, ctx.snap, ctx.own)
        .expect("role visible")
        .clone();
    let cur = eng
        .db
        .find_role_mut(name, ctx.snap, ctx.own)
        .expect("role visible");
    cur.dropped_xmax = ctx.own;
    let versions = eng.db.roles.get_mut(name).expect("role visible");
    let mut next = prev.clone();
    mutate(&mut next);
    next.created_xmin = ctx.own;
    next.dropped_xmax = 0;
    versions.push(next);
    ctx.writes.push(WriteOp::AlterRole {
        name: name.to_string(),
        prev,
    });
}

/// Would `GRANT role TO grantee` create a membership cycle? True when
/// `grantee` is already (transitively) a member of `role`.
pub(crate) fn membership_would_cycle(
    db: &crate::storage::Database,
    role: &str,
    grantee: &str,
    snap: &crate::storage::Snapshot,
    own: u64,
) -> bool {
    let mut seen = vec![role.to_string()];
    let mut i = 0;
    while i < seen.len() {
        let n = seen[i].clone();
        i += 1;
        if n == grantee {
            return true;
        }
        if let Some(r) = db.find_role(&n, snap, own) {
            for m in &r.memberships {
                if !seen.iter().any(|x| x == &m.role) {
                    seen.push(m.role.clone());
                }
            }
        }
    }
    false
}

/// v1.96: resolve a GRANTED BY specification to a role name.
pub(crate) fn resolve_grantor(ctx: &StmtCtx, granted_by: &Option<crate::sql::Grantor>) -> String {
    match granted_by {
        None => ctx.role.to_string(),
        Some(crate::sql::Grantor::Role(r)) => r.clone(),
        Some(crate::sql::Grantor::CurrentUser) => ctx.role.to_string(),
        Some(crate::sql::Grantor::CurrentRole) => ctx.role.to_string(),
        Some(crate::sql::Grantor::SessionUser) => crate::exec::session_user_name(ctx.role),
    }
}

/// v1.96: BFS over INHERIT-option membership edges (PG19
/// roles_is_member_of ROLERECURSE_PRIVS — the walk behind
/// has_privs_of_role and select_best_admin; a NOINHERIT edge does
/// not propagate privileges or admin standing, per the privileges
/// suite's REVOKE INHERIT OPTION sequence): the roles `name`
/// belongs to, itself first, then ancestors by hops (edge order).
pub(crate) fn membership_closure_any(eng: &Engine, ctx: &StmtCtx, name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i].clone();
        i += 1;
        if let Some(r) = eng.db.find_role(&cur, ctx.snap, ctx.own) {
            for m in &r.memberships {
                if m.inherit && !out.iter().any(|x| x == &m.role) {
                    out.push(m.role.clone());
                }
            }
        }
    }
    out
}

/// v1.96: PG19 has_privs_of_role — `member` is `role`, inherits it
/// through any membership chain, or is a superuser.
pub(crate) fn has_privs_of_role(eng: &Engine, ctx: &StmtCtx, member: &str, role: &str) -> bool {
    member == role
        || crate::storage::is_superuser_snap(&eng.db, member, ctx.snap, ctx.own)
        || membership_closure_any(eng, ctx, member)
            .iter()
            .any(|n| n == role)
}

/// v1.96: PG19 select_best_admin — the role through which `member`
/// holds ADMIN OPTION on `role`: `member` itself when it has a
/// direct admin edge, else the closest ancestor (fewest hops, edge
/// order) holding one. A role is never its own admin.
pub(crate) fn select_best_admin(
    eng: &Engine,
    ctx: &StmtCtx,
    member: &str,
    role: &str,
) -> Option<String> {
    if member == role {
        return None;
    }
    for cand in membership_closure_any(eng, ctx, member) {
        if let Some(r) = eng.db.find_role(&cand, ctx.snap, ctx.own) {
            if r.memberships.iter().any(|m| m.role == role && m.admin) {
                return Some(cand);
            }
        }
    }
    None
}

/// v1.96: does `member` hold ADMIN OPTION on `role` (PG19
/// is_admin_of_role: superuser, or an admin edge reachable through
/// any membership chain)?
pub(crate) fn has_admin_option(eng: &Engine, ctx: &StmtCtx, member: &str, role: &str) -> bool {
    crate::storage::is_superuser_snap(&eng.db, member, ctx.snap, ctx.own)
        || select_best_admin(eng, ctx, member, role).is_some()
}

/// v1.96: roles that hold a membership edge in `role` granted BY
/// `grantor` (dependent grants for REVOKE ... CASCADE checks).
pub(crate) fn membership_dependents(
    eng: &Engine,
    ctx: &StmtCtx,
    role: &str,
    grantor: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for (rn, vs) in &eng.db.roles {
        if vs.iter().any(|r| {
            crate::storage::role_visible(r, ctx.snap, ctx.own)
                && r.memberships
                    .iter()
                    .any(|m| m.role == role && m.grantor == grantor)
        }) {
            out.push(rn.clone());
        }
    }
    out
}

/// v1.96: PG19 check_role_membership_authorization (user.c): to
/// grant/revoke membership in a superuser role you must be a
/// superuser; otherwise you must be admin of the role
/// (is_admin_of_role — superusers qualify; a role is never admin
/// of itself).
pub(crate) fn check_role_grant_permission(
    eng: &Engine,
    ctx: &StmtCtx,
    role: &str,
    is_grant: bool,
) -> Result<(), ExecError> {
    let target_super = eng
        .db
        .find_role(role, ctx.snap, ctx.own)
        .map(|r| r.superuser)
        .unwrap_or(false);
    let cur_super = crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own);
    if target_super && !cur_super {
        return Err(exec_err(
            "42501",
            format!(
                "permission denied to {} role \"{}\"",
                if is_grant { "grant" } else { "revoke" },
                role
            ),
        ));
    }
    if cur_super || select_best_admin(eng, ctx, ctx.role, role).is_some() {
        return Ok(());
    }
    Err(exec_err(
        "42501",
        format!(
            "permission denied to {} role \"{}\"",
            if is_grant { "grant" } else { "revoke" },
            role
        ),
    ))
}

/// v1.96: PG19 check_role_grantor (user.c) — validate / infer the
/// grantor recorded for a role GRANT or REVOKE. Authorization
/// (check_role_grant_permission) must already have passed.
/// - Explicit GRANTED BY g: the current user must have privileges
///   of g; for GRANT, g must itself be its own best admin on the
///   role (or the bootstrap superuser).
/// - Implicit: a superuser executor records the bootstrap
///   superuser; anyone else records their best admin role.
pub(crate) fn resolve_role_grantor(
    eng: &Engine,
    ctx: &StmtCtx,
    role: &str,
    granted_by: &Option<crate::sql::Grantor>,
    is_grant: bool,
) -> Result<String, ExecError> {
    if granted_by.is_some() {
        let g = resolve_grantor(ctx, granted_by);
        if eng.db.find_role(&g, ctx.snap, ctx.own).is_none() {
            return Err(exec_err("42704", format!("role \"{}\" does not exist", g)));
        }
        if !has_privs_of_role(eng, ctx, ctx.role, &g) {
            return Err(exec_err(
                "42501",
                format!(
                    "permission denied to {} privileges as role \"{}\"",
                    if is_grant { "grant" } else { "revoke" },
                    g
                ),
            ));
        }
        if is_grant
            && g != "postgres"
            && select_best_admin(eng, ctx, &g, role).as_deref() != Some(g.as_str())
        {
            return Err(exec_err(
                "42501",
                format!("permission denied to grant privileges as role \"{}\"", g),
            ));
        }
        return Ok(g);
    }
    if crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Ok("postgres".to_string());
    }
    Ok(select_best_admin(eng, ctx, ctx.role, role).unwrap_or_else(|| ctx.role.to_string()))
}

/// GRANT role [, ...] TO role [, ...] [WITH options] [GRANTED BY ...]:
/// role membership with PG19 options (v1.96).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_grant_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    roles: &[String],
    grantees: &[String],
    admin: Option<bool>,
    inherit: Option<bool>,
    set: Option<bool>,
    granted_by: &Option<crate::sql::Grantor>,
) -> Result<ExecResult, ExecError> {
    for grantee in grantees {
        // v1.96: CURRENT_USER / CURRENT_ROLE / SESSION_USER grantees.
        let grantee = &resolve_grantee_name(ctx, grantee);
        if eng.db.find_role(grantee, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", grantee),
            ));
        }
        for role in roles {
            if eng.db.find_role(role, ctx.snap, ctx.own).is_none() {
                return Err(exec_err(
                    "42704",
                    format!("role \"{}\" does not exist", role),
                ));
            }
            check_role_grant_permission(eng, ctx, role, true)?;
            let grantor = resolve_role_grantor(eng, ctx, role, granted_by, true)?;
            // v1.96: pg_database_owner can neither have explicit
            // members nor be a member (PG19 user.c).
            if role == "pg_database_owner" {
                return Err(exec_err(
                    "42501",
                    "role \"pg_database_owner\" cannot have explicit members".to_string(),
                ));
            }
            if grantee == "pg_database_owner" {
                return Err(exec_err(
                    "42501",
                    "role \"pg_database_owner\" cannot be a member of any role".to_string(),
                ));
            }
            // v1.96: ADMIN option cannot be granted back to your own
            // grantor (user.c AddRoleMems circularity check): the
            // grantee must not be the grantor of the grantor's own
            // admin edge on this role.
            if admin == Some(true) {
                let grantor_edge_admin_by_grantee = eng
                    .db
                    .find_role(&grantor, ctx.snap, ctx.own)
                    .map(|r| {
                        r.memberships
                            .iter()
                            .any(|m| &m.role == role && m.admin && m.grantor == *grantee)
                    })
                    .unwrap_or(false);
                if grantor_edge_admin_by_grantee {
                    return Err(exec_err(
                        "42501",
                        "ADMIN option cannot be granted back to your own grantor".to_string(),
                    ));
                }
            }
            if role == grantee {
                return Err(exec_err(
                    "42501",
                    "a role cannot be a member of itself".to_string(),
                ));
            }
            if membership_would_cycle(&eng.db, role, grantee, ctx.snap, ctx.own) {
                return Err(exec_err(
                    "42501",
                    format!(
                        "granting \"{}\" to \"{}\" would create a membership cycle",
                        role, grantee
                    ),
                ));
            }
            // Defaults: ADMIN false, INHERIT = grantee's rolinherit,
            // SET true (PG19 GrantRoleStmt defaults).
            let def_inherit = eng
                .db
                .find_role(grantee, ctx.snap, ctx.own)
                .map(|r| r.inherit)
                .unwrap_or(true);
            let (a, i, s) = (
                admin.unwrap_or(false),
                inherit.unwrap_or(def_inherit),
                set.unwrap_or(true),
            );
            // v1.96: edges are keyed (role, grantor) like
            // pg_auth_members rows: the same pair granted by two
            // different grantors is two edges.
            let existing = eng.db.find_role(grantee, ctx.snap, ctx.own).and_then(|r| {
                r.memberships
                    .iter()
                    .find(|m| &m.role == role && m.grantor == grantor)
                    .cloned()
            });
            let role_name = role.clone();
            let grantor_c = grantor.clone();
            match existing {
                Some(_) => {
                    // Re-grant from the same grantor updates the
                    // specified options on that edge only.
                    alter_role_swap(eng, ctx, grantee, move |r| {
                        if let Some(m) = r
                            .memberships
                            .iter_mut()
                            .find(|m| m.role == role_name && m.grantor == grantor_c)
                        {
                            if admin.is_some() {
                                m.admin = a;
                            }
                            if inherit.is_some() {
                                m.inherit = i;
                            }
                            if set.is_some() {
                                m.set = s;
                            }
                        }
                    });
                }
                None => {
                    alter_role_swap(eng, ctx, grantee, move |r| {
                        r.memberships.push(crate::storage::RoleMembership {
                            role: role_name.clone(),
                            grantor: grantor_c.clone(),
                            admin: a,
                            inherit: i,
                            set: s,
                        });
                    });
                }
            }
        }
    }
    Ok(ExecResult::Command {
        tag: "GRANT".to_string(),
    })
}

/// v1.96: resolve a grantee name that may be a CURRENT_* keyword.
pub(crate) fn resolve_grantee_name(ctx: &StmtCtx, name: &str) -> String {
    match name {
        "current_user" | "current_role" => ctx.role.to_string(),
        "session_user" => crate::exec::session_user_name(ctx.role),
        _ => name.to_string(),
    }
}

/// REVOKE role [, ...] FROM role [, ...] [GRANTED BY ...]
/// [CASCADE|RESTRICT], or REVOKE x OPTION FOR (v1.96).
/// PG19 DelRoleMems semantics: only the resolved grantor's edge is
/// removed; losing membership (or ADMIN OPTION) orphans the grants
/// the member made as admin of the role — 2BP01 without CASCADE,
/// recursive removal with it.
pub(crate) fn exec_revoke_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    roles: &[String],
    grantees: &[String],
    option_for: Option<char>,
    granted_by: &Option<crate::sql::Grantor>,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    for grantee in grantees {
        let grantee = &resolve_grantee_name(ctx, grantee);
        if eng.db.find_role(grantee, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", grantee),
            ));
        }
        for role in roles {
            if eng.db.find_role(role, ctx.snap, ctx.own).is_none() {
                return Err(exec_err(
                    "42704",
                    format!("role \"{}\" does not exist", role),
                ));
            }
            check_role_grant_permission(eng, ctx, role, false)?;
            let grantor = resolve_role_grantor(eng, ctx, role, granted_by, false)?;
            let edge = eng.db.find_role(grantee, ctx.snap, ctx.own).and_then(|r| {
                r.memberships
                    .iter()
                    .find(|m| &m.role == role && m.grantor == grantor)
                    .cloned()
            });
            let Some(edge) = edge else {
                // No such grant by that grantor: warning only.
                continue;
            };
            // Does this revoke strip the member's admin standing on
            // the role (membership itself, or its ADMIN OPTION)?
            let strips_admin = match option_for {
                None => edge.admin,
                Some('a') | Some('g') => true,
                _ => false,
            };
            let other_admin = eng
                .db
                .find_role(grantee, ctx.snap, ctx.own)
                .map(|r| {
                    r.memberships
                        .iter()
                        .any(|m| &m.role == role && m.admin && m.grantor != grantor)
                })
                .unwrap_or(false);
            // Dependent grants: memberships in `role` granted BY
            // the member being stripped.
            let deps: Vec<String> = if strips_admin && !other_admin {
                membership_dependents(eng, ctx, role, grantee)
                    .into_iter()
                    .filter(|d| d != grantee)
                    .collect()
            } else {
                Vec::new()
            };
            if !deps.is_empty() && !cascade {
                return Err(exec_err("2BP01", "dependent privileges exist".to_string()));
            }
            let role_name = role.clone();
            let grantor_c = grantor.clone();
            match option_for {
                None => {
                    let (rn, gc) = (role_name.clone(), grantor_c.clone());
                    alter_role_swap(eng, ctx, grantee, move |r| {
                        r.memberships.retain(|m| !(m.role == rn && m.grantor == gc));
                    });
                }
                Some(which) => {
                    let (rn, gc) = (role_name.clone(), grantor_c.clone());
                    alter_role_swap(eng, ctx, grantee, move |r| {
                        if let Some(m) = r
                            .memberships
                            .iter_mut()
                            .find(|m| m.role == rn && m.grantor == gc)
                        {
                            match which {
                                'a' | 'g' => m.admin = false,
                                'i' => m.inherit = false,
                                's' => m.set = false,
                                _ => {}
                            }
                        }
                    });
                }
            }
            // CASCADE: dependent grants lose their backing admin and
            // are removed, recursively (their own dependents too).
            // Queue items are (member, grantor) edges to delete.
            if cascade && !deps.is_empty() {
                let mut queue: Vec<(String, String)> =
                    deps.iter().map(|d| (d.clone(), grantee.clone())).collect();
                let mut processed: Vec<(String, String)> = Vec::new();
                while let Some((member, g)) = queue.pop() {
                    if processed.contains(&(member.clone(), g.clone())) {
                        continue;
                    }
                    processed.push((member.clone(), g.clone()));
                    let sub = membership_dependents(eng, ctx, role, &member);
                    let rn = role_name.clone();
                    alter_role_swap(eng, ctx, &member, move |r| {
                        r.memberships.retain(|m| !(m.role == rn && m.grantor == g));
                    });
                    for s in sub {
                        if s != member {
                            queue.push((s, member.clone()));
                        }
                    }
                }
            }
        }
    }
    Ok(ExecResult::Command {
        tag: "REVOKE".to_string(),
    })
}

/// v1.96: ALTER GROUP name ADD USER ... / DROP USER ... / RENAME TO ...
pub(crate) fn exec_alter_group(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    add: &[String],
    drop: &[String],
    rename_to: &Option<String>,
) -> Result<ExecResult, ExecError> {
    // PG19: ALTER GROUP requires superuser, CREATEROLE, or ADMIN
    // OPTION on the group role.
    let cur = eng.db.find_role(ctx.role, ctx.snap, ctx.own);
    let allowed = crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        || cur.map(|r| r.createrole).unwrap_or(false)
        || has_admin_option(eng, ctx, ctx.role, name);
    if !allowed {
        return Err(exec_err(
            "42501",
            "permission denied to alter role".to_string(),
        ));
    }
    if eng.db.find_role(name, ctx.snap, ctx.own).is_none() {
        return Err(exec_err(
            "42704",
            format!("role \"{}\" does not exist", name),
        ));
    }
    if let Some(new_name) = rename_to {
        // v1.96 finishing pass: PG19 RenameRole — renaming requires
        // CREATEROLE *and* ADMIN OPTION on the target (the loose
        // ALTER GROUP gate above is not sufficient), and a superuser
        // target requires a superuser executor.
        let target_super = eng
            .db
            .find_role(name, ctx.snap, ctx.own)
            .map(|r| r.superuser)
            .unwrap_or(false);
        let iam_super = crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own);
        if !iam_super
            && (target_super || !(have_createrole(eng, ctx) && is_admin_of_role(eng, ctx, name)))
        {
            return Err(exec_err(
                "42501",
                "permission denied to rename role".to_string(),
            ));
        }
        if eng.db.find_role(new_name, ctx.snap, ctx.own).is_some() {
            return Err(exec_err(
                "42710",
                format!("role \"{}\" already exists", new_name),
            ));
        }
        // Rename: drop the old version, create the same role under the
        // new name (WAL: DropRole + CreateRole), and rewrite membership
        // edges in other roles that pointed at the old name.
        let live = eng
            .db
            .find_role(name, ctx.snap, ctx.own)
            .expect("visible above")
            .clone();
        let cur = eng
            .db
            .find_role_mut(name, ctx.snap, ctx.own)
            .expect("visible above");
        cur.dropped_xmax = ctx.own;
        ctx.writes.push(WriteOp::DropRole {
            name: name.to_string(),
            prev: live.clone(),
        });
        let mut next = live;
        next.name = new_name.clone();
        next.created_xmin = ctx.own;
        next.dropped_xmax = 0;
        eng.db.roles.entry(new_name.clone()).or_default().push(next);
        ctx.writes.push(WriteOp::CreateRole {
            name: new_name.clone(),
        });
        let affected: Vec<String> = eng
            .db
            .roles
            .iter()
            .filter(|(rn, vs)| {
                rn.as_str() != name
                    && vs.iter().any(|r| {
                        crate::storage::role_visible(r, ctx.snap, ctx.own)
                            && r.memberships.iter().any(|m| m.role == name)
                    })
            })
            .map(|(rn, _)| rn.clone())
            .collect();
        for rn in affected {
            let (old, newn) = (name.to_string(), new_name.clone());
            alter_role_swap(eng, ctx, &rn, move |r| {
                for m in &mut r.memberships {
                    if m.role == old {
                        m.role = newn.clone();
                    }
                }
            });
        }
        // v1.96: name-keyed owner maps follow the rename (PG keys
        // ownership by OID, so ALTER SCHEMA OWNER + RENAME keeps
        // pg_namespace.nspowner pointing at the role). Table/view/
        // sequence owner fields are NOT rewritten (documented
        // hold-back of the name-keyed model).
        if eng.db.schema_owners.values().any(|o| o == name) {
            let prev = eng.db.schemas.clone();
            let prev_owners = eng.db.schema_owners.clone();
            for o in eng.db.schema_owners.values_mut() {
                if o == name {
                    *o = new_name.clone();
                }
            }
            ctx.writes.push(WriteOp::SchemaSet {
                prev,
                prev_owners,
                prev_acls: eng.db.schema_acls.clone(),
            });
        }
        if eng.db.db_owner == name {
            let prev = eng.db.db_owner.clone();
            eng.db.db_owner = new_name.clone();
            ctx.writes.push(WriteOp::DbOwner { prev });
        }
        if eng.db.fn_owners.values().any(|o| o == name)
            || eng.db.type_owners.values().any(|o| o == name)
        {
            let prev_fn = eng.db.fn_acls.clone();
            let prev_type = eng.db.type_acls.clone();
            let prev_lang = eng.db.lang_acls.clone();
            let prev_fn_owners = eng.db.fn_owners.clone();
            let prev_type_owners = eng.db.type_owners.clone();
            for o in eng.db.fn_owners.values_mut() {
                if o == name {
                    *o = new_name.clone();
                }
            }
            for o in eng.db.type_owners.values_mut() {
                if o == name {
                    *o = new_name.clone();
                }
            }
            ctx.writes.push(WriteOp::FnTypeAcls {
                prev_fn,
                prev_type,
                prev_lang,
                prev_fn_owners,
                prev_type_owners,
            });
        }
        if eng.db.lo_owners.values().any(|o| o == name) {
            let prev = eng.db.large_objects.clone();
            let prev_owners = eng.db.lo_owners.clone();
            let prev_acls = eng.db.lo_acls.clone();
            for o in eng.db.lo_owners.values_mut() {
                if o == name {
                    *o = new_name.clone();
                }
            }
            ctx.writes.push(WriteOp::LargeObjects {
                prev,
                prev_owners,
                prev_acls,
            });
        }
        return Ok(ExecResult::Command {
            tag: "ALTER GROUP".to_string(),
        });
    }
    for user in add {
        if eng.db.find_role(user, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", user),
            ));
        }
        let has = eng
            .db
            .find_role(user, ctx.snap, ctx.own)
            .map(|r| r.memberships.iter().any(|m| m.role == name))
            .unwrap_or(false);
        if has {
            continue;
        }
        let g = name.to_string();
        let grantor = ctx.role.to_string();
        alter_role_swap(eng, ctx, user, move |r| {
            r.memberships.push(crate::storage::RoleMembership {
                role: g.clone(),
                grantor: grantor.clone(),
                admin: false,
                inherit: true,
                set: true,
            });
        });
    }
    for user in drop {
        if eng.db.find_role(user, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", user),
            ));
        }
        let g = name.to_string();
        alter_role_swap(eng, ctx, user, move |r| {
            r.memberships.retain(|m| m.role != g);
        });
    }
    Ok(ExecResult::Command {
        tag: "ALTER GROUP".to_string(),
    })
}

pub(crate) fn exec_drop_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    // v1.96 finishing pass: PG19 DropRole permission rule — the
    // executor needs CREATEROLE; per target, a superuser target needs
    // a superuser executor, and the executor must hold ADMIN OPTION
    // on the target (superusers are admins of every role).
    if !have_createrole(eng, ctx) {
        return Err(exec_err(
            "42501",
            "permission denied to drop role".to_string(),
        ));
    }
    for name in names {
        let live = eng.db.find_role(name, ctx.snap, ctx.own).cloned();
        let Some(role) = live else {
            if if_exists {
                continue;
            }
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", name),
            ));
        };
        // v1.96: predefined roles cannot be dropped (PG19 user.c).
        if crate::storage::is_predefined_role(&role.name) {
            return Err(exec_err(
                "42501",
                format!(
                    "permission denied: cannot drop role \"{}\" because it is a predefined role",
                    role.name
                ),
            ));
        }
        // PG19: the current/session role cannot be dropped (55006).
        if role.name == ctx.role {
            return Err(exec_err(
                "55006",
                "current user cannot be dropped".to_string(),
            ));
        }
        // PG19 DropRole: superuser targets need a superuser executor;
        // the executor must be an admin of the target.
        if role.superuser
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                "permission denied to drop role".to_string(),
            ));
        }
        if !is_admin_of_role(eng, ctx, &role.name) {
            return Err(exec_err(
                "42501",
                "permission denied to drop role".to_string(),
            ));
        }
        // The bootstrap superuser is the safety net: it cannot be dropped.
        if role.name == "postgres" {
            return Err(exec_err(
                "42501",
                "permission denied: cannot drop role \"postgres\"".to_string(),
            ));
        }
        // Like PostgreSQL (2BP01): refuse while the role still owns objects.
        let mut owned = Vec::new();
        for (tn, vs) in &eng.db.tables {
            if vs.iter().any(|t| {
                crate::storage::table_visible(t, ctx.snap, &ctx.all_xids) && t.owner == role.name
            }) {
                owned.push(format!("table {}", tn));
            }
        }
        for (vn, vs) in &eng.db.views {
            if vs
                .iter()
                .any(|v| crate::storage::view_visible(v, ctx.snap, ctx.own) && v.owner == role.name)
            {
                owned.push(format!("view {}", vn));
            }
        }
        for (sn, vs) in &eng.db.sequences {
            if vs
                .iter()
                .any(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own) && s.owner == role.name)
            {
                owned.push(format!("sequence {}", sn));
            }
        }
        // v1.96: PG19 checkSharedDependencies — owning objects,
        // holding or having granted privileges, and being the grantor
        // of a membership all block DROP ROLE (2BP01).
        let mut blockers: Vec<String> = owned.clone();
        if blockers.is_empty() {
            let acl_has = |acl: &[crate::storage::AclEntry]| {
                acl.iter()
                    .any(|e| e.role == role.name || e.grantor == role.name)
            };
            for (tn, vs) in &eng.db.tables {
                if vs.iter().any(|t| {
                    crate::storage::table_visible(t, ctx.snap, &ctx.all_xids)
                        && (acl_has(&t.acl)
                            || t.col_acl
                                .iter()
                                .any(|e| e.role == role.name || e.grantor == role.name))
                }) {
                    blockers.push(format!("privileges on table {}", tn));
                }
            }
            for (sn, vs) in &eng.db.sequences {
                if vs
                    .iter()
                    .any(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own) && acl_has(&s.acl))
                {
                    blockers.push(format!("privileges on sequence {}", sn));
                }
            }
            if acl_has(&eng.db.db_acl) {
                blockers.push("privileges on database".to_string());
            }
            for (kn, a) in &eng.db.fn_acls {
                if acl_has(a) {
                    blockers.push(format!("privileges on function {}", kn));
                }
            }
            for (kn, a) in &eng.db.type_acls {
                if acl_has(a) {
                    blockers.push(format!("privileges on type {}", kn));
                }
            }
            for (kn, a) in &eng.db.lang_acls {
                if acl_has(a) {
                    blockers.push(format!("privileges on language {}", kn));
                }
            }
            for d in &eng.db.default_acls {
                // v1.96 finishing pass: PG19 DropRole silently
                // deletes the dropped role's OWN default-ACL rules
                // (pg_default_acl rows keyed by defaclroleid); only a
                // grantee mention in ANOTHER role's rule is a shared
                // dependency. (matview / select_into suites: users
                // hold FOR ROLE self GRANT ... TO self rules and must
                // still drop cleanly.)
                if d.grantee == role.name && d.creator != role.name {
                    blockers.push(format!(
                        "default privileges (creator {} grantee {})",
                        d.creator, d.grantee
                    ));
                }
            }
            for (kn, o) in &eng.db.fn_owners {
                if o == &role.name {
                    blockers.push(format!("owner of function {}", kn));
                }
            }
            for (kn, o) in &eng.db.type_owners {
                if o == &role.name {
                    blockers.push(format!("owner of type {}", kn));
                }
            }
            for (rn, vs) in &eng.db.roles {
                if vs.iter().any(|r| {
                    crate::storage::role_visible(r, ctx.snap, ctx.own)
                        && r.memberships.iter().any(|m| m.grantor == role.name)
                }) {
                    blockers.push(format!("grantor of membership in {}", rn));
                }
            }
            for (oid, o) in &eng.db.lo_owners {
                if o == &role.name {
                    blockers.push(format!("owner of large object {}", oid));
                }
            }
            for (sn, o) in &eng.db.schema_owners {
                if o == &role.name {
                    blockers.push(format!("owner of schema {}", sn));
                }
            }
        }
        if !blockers.is_empty() {
            return Err(ExecError {
                detail: Some(blockers.join("\n")),
                code: "2BP01",
                message: format!(
                    "role \"{}\" cannot be dropped because some objects depend on it",
                    role.name
                ),
            });
        }
        // Grants *to* the dropped role linger but are inert (documented
        // deviation from PostgreSQL, which drops them).
        let cur = eng
            .db
            .find_role_mut(name, ctx.snap, ctx.own)
            .expect("visible above");
        cur.dropped_xmax = ctx.own;
        let dropped_role_name = role.name.clone();
        ctx.writes.push(WriteOp::DropRole {
            name: name.to_string(),
            prev: role,
        });
        // v1.96 finishing pass: PG19 DropRole deletes the role's own
        // default-ACL rules (see the blocker scan above).
        if eng
            .db
            .default_acls
            .iter()
            .any(|d| d.creator == dropped_role_name)
        {
            let prev = eng.db.default_acls.clone();
            eng.db
                .default_acls
                .retain(|d| d.creator != dropped_role_name);
            ctx.writes.push(WriteOp::DefaultAcls { prev });
        }
        // Membership edges referencing the dropped role are removed from
        // every remaining role (PostgreSQL drops them from pg_auth_members).
        let affected: Vec<String> = eng
            .db
            .roles
            .iter()
            .filter(|(rn, vs)| {
                *rn != name
                    && vs.iter().any(|r| {
                        crate::storage::role_visible(r, ctx.snap, ctx.own)
                            && r.memberships.iter().any(|m| &m.role == name)
                    })
            })
            .map(|(rn, _)| rn.clone())
            .collect();
        for rn in affected {
            let dropped = name.clone();
            alter_role_swap(eng, ctx, &rn, move |r| {
                r.memberships.retain(|m| m.role != dropped);
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP ROLE".to_string(),
    })
}

/// v1.96: the role whose privileges a GRANT/REVOKE acts with: the
/// GRANTED BY role when specified (CURRENT_USER/CURRENT_ROLE are
/// the current role in this engine; SESSION_USER is approximated
/// by it too), else the current role.
pub(crate) fn grantor_actor_name(
    ctx: &StmtCtx,
    granted_by: &Option<crate::sql::Grantor>,
) -> String {
    match granted_by {
        Some(crate::sql::Grantor::Role(n)) => n.clone(),
        _ => ctx.role.to_string(),
    }
}

/// v1.96: PG19 aclchk.c restrict_and_check_grant hard error: when
/// the effective grantor holds NO grant option for any requested
/// bit (groups empty) AND holds no privileges at all on the object
/// (actor_any false), GRANT/REVOKE errors 42501 instead of warning.
/// `actor_any` is computed by the caller with the object's own
/// privs-for helper (defaults included, e.g. PUBLIC EXECUTE on a
/// function whose ACL is still NULL counts as holding a privilege).
pub(crate) fn check_stranger_grantor(
    groups: &[(String, u32)],
    actor_any: bool,
    kind: &str,
    name: &str,
) -> Result<(), ExecError> {
    if groups.is_empty() && !actor_any {
        return Err(exec_err(
            "42501",
            format!("permission denied for {} \"{}\"", kind, name),
        ));
    }
    Ok(())
}

/// v1.96: PG19 aclchk.c select_best_grantor, per privilege bit:
/// group the requested bits by the grantor that may pass them on.
/// Bits no candidate may grant are dropped (PG issues a WARNING and
/// carries on — the statement still succeeds). An explicit GRANTED
/// BY requires the current user to have that role's privileges
/// (42501 otherwise); the grantor is then used exactly, with only
/// the grant options it directly holds. Owner and superuser
/// executors act as (and record) the owner.
pub(crate) fn select_grantor_groups(
    eng: &Engine,
    ctx: &StmtCtx,
    owner: &str,
    acl: &[crate::storage::AclEntry],
    bits: u32,
    granted_by: &Option<crate::sql::Grantor>,
    revoke: bool,
) -> Result<Vec<(String, u32)>, ExecError> {
    let direct_go = |who: &str| -> u32 {
        if who == owner {
            return bits;
        }
        let mut go = 0u32;
        for e in acl {
            if e.role == who || e.role == "public" {
                go |= e.grant_option;
            }
        }
        go & bits
    };
    if granted_by.is_some() {
        let g = resolve_grantor(ctx, granted_by);
        if eng.db.find_role(&g, ctx.snap, ctx.own).is_none() {
            return Err(exec_err("42704", format!("role \"{}\" does not exist", g)));
        }
        if !has_privs_of_role(eng, ctx, ctx.role, &g) {
            return Err(exec_err(
                "42501",
                format!("must inherit privileges of role \"{}\"", g),
            ));
        }
        let go = direct_go(&g);
        return Ok(vec![(g, go)]);
    }
    if ctx.role == owner || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
    {
        // PG19 select_best_grantor: owner and superuser executors act
        // as the owner — exactly ONE grantor group. A revoke therefore
        // strips only entries granted by the owner (privileges suite:
        // an owner's REVOKE of a privilege another role granted "does
        // nothing"); dependent grants are handled by the backing
        // validation / CASCADE machinery, not by widening the strip.
        let _ = revoke;
        return Ok(vec![(owner.to_string(), bits)]);
    }
    // Candidates in roles_is_member_of order: the current user,
    // then the roles whose privileges it possesses (fewest hops).
    let candidates = membership_closure_any(eng, ctx, ctx.role);
    let mut groups: Vec<(String, u32)> = Vec::new();
    let mut remaining = bits;
    for cand in &candidates {
        if remaining == 0 {
            break;
        }
        let take = direct_go(cand) & remaining;
        if take != 0 {
            groups.push((cand.clone(), take));
            remaining &= !take;
        }
    }
    Ok(groups)
}

/// v1.96: grant-option bits `role` holds on an ACL (owner: all;
/// otherwise its own entries plus PUBLIC's).
pub(crate) fn acl_go_bits(acl: &[crate::storage::AclEntry], owner: &str, role: &str) -> u32 {
    if role == owner {
        return u32::MAX;
    }
    let mut go = 0u32;
    for e in acl {
        if e.role == role || e.role == "public" {
            go |= e.grant_option;
        }
    }
    go
}

/// v1.96: PG19 aclchk dependent validation — a grant is *dependent*
/// only when its grantor's own grant option flowed through a role
/// whose privileges this REVOKE just stripped. (The finishing pass
/// scoped the check to the revoked grantees: the original form —
/// every entry's grantor must hold grant option — misfired on
/// entries granted by a superuser or a previous owner, whose grants
/// PG treats as irrevocably backed; e.g. foreign_key's REVOKE after
/// ALTER TABLE ... OWNER TO.) `revoked` seeds the affected set;
/// CASCADE strips unbacked bits and extends the set with each
/// stripped entry's grantee, recursively. RESTRICT raises 2BP01.
pub(crate) fn validate_acl_backing(
    acl: &mut Vec<crate::storage::AclEntry>,
    owner: &str,
    cascade: bool,
    revoked: &[String],
) -> Result<(), ExecError> {
    let mut affected: Vec<String> = revoked.to_vec();
    loop {
        let mut bad: Option<(usize, u32)> = None;
        for (i, e) in acl.iter().enumerate() {
            if e.grantor.is_empty() || e.grantor == owner || e.privs == 0 {
                continue;
            }
            if !affected.iter().any(|a| a == &e.grantor) {
                continue;
            }
            let go = acl_go_bits(acl, owner, &e.grantor);
            let unbacked = e.privs & !go;
            if unbacked != 0 {
                bad = Some((i, unbacked));
                break;
            }
        }
        let Some((i, unbacked)) = bad else {
            return Ok(());
        };
        if !cascade {
            return Err(exec_err("2BP01", "dependent privileges exist".to_string()));
        }
        acl[i].privs &= !unbacked;
        acl[i].grant_option &= !unbacked;
        let grantee = acl[i].role.clone();
        if !affected.contains(&grantee) {
            affected.push(grantee);
        }
    }
}

pub(crate) fn exec_grant_revoke(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    privs: &[crate::sql::PrivSpec],
    object: &crate::sql::GrantObject,
    grantees_in: &[String],
    revoke: bool,
    granted_by: &Option<crate::sql::Grantor>,
    grant_option: bool,
    grant_option_for: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    use crate::sql::{GrantObject, PrivSpec, Privilege};
    // v1.96: resolve CURRENT_* grantees.
    let resolved: Vec<String> = grantees_in
        .iter()
        .map(|g| resolve_grantee_name(ctx, g))
        .collect();
    let grantees: &[String] = &resolved;
    // All grantees must be existing roles — except PUBLIC, PG's
    // pseudo-role (acl.h ACL_ID_PUBLIC), which needs no catalog row.
    for g in grantees {
        if g != "public" && eng.db.find_role(g, ctx.snap, ctx.own).is_none() {
            return Err(exec_err("42704", format!("role \"{}\" does not exist", g)));
        }
    }
    // v1.96: ALL TABLES / ALL SEQUENCES IN SCHEMA expands to the live
    // object list (PG19 GrantStmt with is_all_seqs/objects).
    match object {
        GrantObject::AllTables => {
            let names: Vec<String> = eng
                .db
                .tables
                .keys()
                .filter(|n| {
                    eng.db
                        .find_table(n, ctx.snap, &ctx.all_xids, ctx.session)
                        .is_some()
                })
                .cloned()
                .collect();
            let expanded = GrantObject::Table(names);
            return exec_grant_revoke(
                eng,
                ctx,
                privs,
                &expanded,
                grantees_in,
                revoke,
                granted_by,
                grant_option,
                grant_option_for,
                cascade,
            );
        }
        GrantObject::AllSequences => {
            let names: Vec<String> = eng.db.sequences.keys().cloned().collect();
            for n in names {
                let one = GrantObject::Sequence(n);
                exec_grant_revoke(
                    eng,
                    ctx,
                    privs,
                    &one,
                    grantees_in,
                    revoke,
                    granted_by,
                    grant_option,
                    grant_option_for,
                    cascade,
                )?;
            }
            return Ok(ExecResult::Command {
                tag: if revoke { "REVOKE" } else { "GRANT" }.to_string(),
            });
        }
        // v1.96: ALL FUNCTIONS / PROCEDURES / ROUTINES IN SCHEMA —
        // expand to the schema's routines (names stored qualified;
        // unqualified names live in the default schema "public").
        GrantObject::AllFunctionsInSchema(schema)
        | GrantObject::AllProceduresInSchema(schema)
        | GrantObject::AllRoutinesInSchema(schema) => {
            let want_proc = matches!(object, GrantObject::AllProceduresInSchema(_));
            let want_fn = !matches!(object, GrantObject::AllProceduresInSchema(_));
            let in_schema = |n: &str| {
                if schema == "public" {
                    !n.contains('.')
                } else {
                    n.starts_with(&format!("{}.", schema))
                }
            };
            let mut fn_names: Vec<String> = Vec::new();
            let mut proc_names: Vec<String> = Vec::new();
            for (n, vs) in &eng.db.functions {
                if !in_schema(n) {
                    continue;
                }
                if vs.iter().any(|f| f.is_procedure) {
                    proc_names.push(n.clone());
                }
                if vs.iter().any(|f| !f.is_procedure) {
                    fn_names.push(n.clone());
                }
            }
            if want_fn {
                for n in eng.db.aggregates.keys() {
                    if in_schema(n) {
                        fn_names.push(n.clone());
                    }
                }
            }
            if want_fn && !fn_names.is_empty() {
                let expanded = GrantObject::Function(fn_names);
                exec_grant_revoke(
                    eng,
                    ctx,
                    privs,
                    &expanded,
                    grantees_in,
                    revoke,
                    granted_by,
                    grant_option,
                    grant_option_for,
                    cascade,
                )?;
            }
            if (want_proc || matches!(object, GrantObject::AllRoutinesInSchema(_)))
                && !proc_names.is_empty()
            {
                let expanded = GrantObject::Procedure(proc_names);
                exec_grant_revoke(
                    eng,
                    ctx,
                    privs,
                    &expanded,
                    grantees_in,
                    revoke,
                    granted_by,
                    grant_option,
                    grant_option_for,
                    cascade,
                )?;
            }
            return Ok(ExecResult::Command {
                tag: if revoke { "REVOKE" } else { "GRANT" }.to_string(),
            });
        }
        // v1.96: GRANT/REVOKE ON LARGE OBJECT — per-LO ACL entries
        // (SELECT/UPDATE only; INSERT et al. are 0A000 in PG19).
        GrantObject::LargeObject(oids) => {
            for p in privs {
                if !matches!(
                    p.priv_,
                    Privilege::Select | Privilege::Update | Privilege::All
                ) {
                    return Err(exec_err(
                        "0A000",
                        format!(
                            "invalid privilege type {} for large object",
                            privilege_name(p.priv_)
                        ),
                    ));
                }
            }
            let bits = if privs.iter().any(|s| matches!(s.priv_, Privilege::All)) {
                crate::storage::PRIV_SELECT | crate::storage::PRIV_UPDATE
            } else {
                privs.iter().fold(0u32, |b, s| b | s.priv_.bits())
            };
            for oid in oids {
                if !eng.db.large_objects.contains_key(oid) {
                    return Err(exec_err(
                        "42704",
                        format!("large object {} does not exist", oid),
                    ));
                }
                let owner = eng
                    .db
                    .lo_owners
                    .get(oid)
                    .cloned()
                    .unwrap_or_else(|| "postgres".to_string());
                let prev = eng.db.large_objects.clone();
                let prev_owners = eng.db.lo_owners.clone();
                let prev_acls = eng.db.lo_acls.clone();
                let mut entries = eng.db.lo_acls.get(oid).cloned().unwrap_or_default();
                let groups =
                    select_grantor_groups(eng, ctx, &owner, &entries, bits, granted_by, revoke)?;
                apply_acl_delta(
                    &mut entries,
                    grantees_in,
                    &groups,
                    revoke,
                    false,
                    grant_option,
                    grant_option_for,
                    cascade,
                    &owner,
                )?;
                eng.db.lo_acls.insert(*oid, entries);
                ctx.writes.push(WriteOp::LargeObjects {
                    prev,
                    prev_owners,
                    prev_acls,
                });
            }
            return Ok(ExecResult::Command {
                tag: if revoke { "REVOKE" } else { "GRANT" }.to_string(),
            });
        }
        // v1.96: GRANT ON LANGUAGE — USAGE entries in lang_acls
        // (trusted languages default PUBLIC USAGE when no entry).
        GrantObject::Language(lname) => {
            let lname = lname.to_ascii_lowercase();
            let lname_disp = lname.clone();
            let bits = if privs.iter().any(|s| matches!(s.priv_, Privilege::All)) {
                crate::storage::PRIV_USAGE
            } else {
                privs.iter().fold(0u32, |b, s| b | s.priv_.bits())
            };
            let prev_fn = eng.db.fn_acls.clone();
            let prev_type = eng.db.type_acls.clone();
            let prev_lang = eng.db.lang_acls.clone();
            let prev_fn_owners = eng.db.fn_owners.clone();
            let prev_type_owners = eng.db.type_owners.clone();
            // v1.96: materialize the implicit PUBLIC usage default
            // ONLY when the language has never had an ACL (key
            // absent). Seeding whenever the public entry is missing
            // resurrects a revoked default on the next GRANT (the
            // priv_testfunc3 root). Zero entries are kept below so
            // a revoked public entry stays visible as privs=0.
            let had_key = eng.db.lang_acls.contains_key(&lname);
            let mut entries = eng.db.lang_acls.get(&lname).cloned().unwrap_or_default();
            if !had_key {
                entries.push(crate::storage::AclEntry::new(
                    "public",
                    crate::storage::PRIV_USAGE,
                ));
            }
            let groups =
                select_grantor_groups(eng, ctx, "postgres", &entries, bits, granted_by, revoke)?;
            apply_acl_delta(
                &mut entries,
                grantees_in,
                &groups,
                revoke,
                true,
                grant_option,
                grant_option_for,
                cascade,
                "postgres",
            )?;
            eng.db.lang_acls.insert(lname, entries);
            ctx.writes.push(WriteOp::FnTypeAcls {
                prev_fn,
                prev_type,
                prev_lang,
                prev_fn_owners,
                prev_type_owners,
            });
            let _ = lname_disp;
            return Ok(ExecResult::Command {
                tag: if revoke { "REVOKE" } else { "GRANT" }.to_string(),
            });
        }
        // v1.96: GRANT ON SCHEMA — validated no-op (single namespace;
        // USAGE/CREATE are not enforced per-schema).
        GrantObject::Schema(sname) => {
            if !eng.db.schemas.iter().any(|s| s == sname) {
                return Err(exec_err(
                    "3F000",
                    format!("schema \"{}\" does not exist", sname),
                ));
            }
            // v1.96: real schema ACLs (were a validated no-op).
            // Bits: USAGE = PRIV_USAGE, CREATE = PRIV_REFERENCES.
            let mut bits = 0u32;
            for s in privs {
                bits |= match s.priv_ {
                    crate::sql::Privilege::All => {
                        crate::storage::PRIV_USAGE | crate::storage::PRIV_REFERENCES
                    }
                    crate::sql::Privilege::Usage => crate::storage::PRIV_USAGE,
                    crate::sql::Privilege::Create => crate::storage::PRIV_REFERENCES,
                    _ => {
                        return Err(exec_err(
                            "0A000",
                            format!("invalid privilege {:?} for schema", s.priv_),
                        ));
                    }
                };
            }
            let owner = eng
                .db
                .schema_owners
                .get(sname)
                .cloned()
                .unwrap_or_else(|| eng.db.db_owner.clone());
            let prev = eng.db.schemas.clone();
            let prev_owners = eng.db.schema_owners.clone();
            let prev_acls = eng.db.schema_acls.clone();
            let mut entries = eng.db.schema_acls.get(sname).cloned().unwrap_or_default();
            let groups =
                select_grantor_groups(eng, ctx, &owner, &entries, bits, granted_by, revoke)?;
            {
                let actor = grantor_actor_name(ctx, granted_by);
                let closure = crate::storage::role_closure(&eng.db, &actor, ctx.snap, ctx.own);
                let mut actor_any = actor == owner;
                if let Some(es) = eng.db.schema_acls.get(sname) {
                    for e in es {
                        if e.role == "public" || closure.iter().any(|n| n == &e.role) {
                            actor_any = true;
                        }
                    }
                }
                // pg_read/write_all_data hold USAGE on every schema.
                if bits & crate::storage::PRIV_USAGE != 0
                    && closure
                        .iter()
                        .any(|n| n == "pg_read_all_data" || n == "pg_write_all_data")
                {
                    actor_any = true;
                }
                check_stranger_grantor(&groups, actor_any, "schema", sname)?;
            }
            apply_acl_delta(
                &mut entries,
                grantees_in,
                &groups,
                revoke,
                false,
                grant_option,
                grant_option_for,
                cascade,
                &owner,
            )?;
            eng.db.schema_acls.insert(sname.clone(), entries);
            ctx.writes.push(WriteOp::SchemaSet {
                prev,
                prev_owners,
                prev_acls,
            });
            return Ok(ExecResult::Command {
                tag: if revoke { "REVOKE" } else { "GRANT" }.to_string(),
            });
        }
        _ => {}
    }
    // v1.96: GRANTED BY existence validation (the privileges-of
    // check itself happens per object in select_grantor_groups,
    // PG19 aclchk.c order: object lookup first, then grantor).
    if granted_by.is_some() {
        let grantor = resolve_grantor(ctx, granted_by);
        if eng.db.find_role(&grantor, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", grantor),
            ));
        }
    }
    // Column lists are only meaningful on tables.
    if !matches!(object, GrantObject::Table(_)) && privs.iter().any(|s| !s.columns.is_empty()) {
        return Err(exec_err(
            "0A000",
            "column lists are only allowed on tables".to_string(),
        ));
    }
    // Collapse the whole-object privileges to a bitmask, validating
    // applicability; column-restricted specs are handled separately.
    let mut bits = 0u32;
    let mut col_specs: Vec<&PrivSpec> = Vec::new();
    for s in privs {
        let p = s.priv_;
        if !s.columns.is_empty() {
            col_specs.push(s);
            continue;
        }
        match (p, object) {
            (Privilege::All, GrantObject::Table(_)) => bits |= crate::storage::PRIV_ALL_TABLE,
            // v1.96 finishing pass: PG19 sequence privileges are
            // SELECT|UPDATE|USAGE — ALL means all three, and SELECT /
            // UPDATE are individually grantable (previously ALL
            // collapsed to USAGE and UPDATE was rejected outright).
            (Privilege::All, GrantObject::Sequence(_)) => {
                bits |= crate::storage::PRIV_USAGE
                    | crate::storage::PRIV_SELECT
                    | crate::storage::PRIV_UPDATE
            }
            (Privilege::Select | Privilege::Update, GrantObject::Sequence(_)) => bits |= p.bits(),
            (Privilege::All, GrantObject::Database) => bits |= crate::storage::PRIV_CONNECT,
            (Privilege::Usage, GrantObject::Sequence(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::Connect, GrantObject::Database) => bits |= crate::storage::PRIV_CONNECT,
            (Privilege::Connect, GrantObject::Table(_)) => bits |= crate::storage::PRIV_CONNECT,
            (Privilege::Create, GrantObject::Database) => bits |= crate::storage::PRIV_REFERENCES,
            (Privilege::Temporary, GrantObject::Database) => bits |= crate::storage::PRIV_TRUNCATE,
            (Privilege::All, GrantObject::Function(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::Execute, GrantObject::Function(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::All, GrantObject::Procedure(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::Execute, GrantObject::Procedure(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::All, GrantObject::Type(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::Usage, GrantObject::Type(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::All, GrantObject::Domain(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::Usage, GrantObject::Domain(_)) => bits |= crate::storage::PRIV_USAGE,
            (
                Privilege::Select
                | Privilege::Insert
                | Privilege::Update
                | Privilege::Delete
                | Privilege::Truncate
                | Privilege::References
                | Privilege::Trigger
                | Privilege::Maintain,
                GrantObject::Table(_),
            ) => bits |= p.bits(),
            (Privilege::Usage, GrantObject::Table(_)) => bits |= crate::storage::PRIV_USAGE,
            _ => {
                return Err(exec_err(
                    "0A000",
                    format!("invalid privilege {:?} for {:?}", p, object),
                ));
            }
        }
    }
    let verb = if revoke { "REVOKE" } else { "GRANT" };
    match object {
        // Handled (early-return) in the pre-match above.
        GrantObject::Language(_) | GrantObject::Schema(_) | GrantObject::LargeObject(_) => {
            unreachable!("language/schema/large-object grants return earlier")
        }
        GrantObject::Table(names) => {
            for name in names {
                // v1.96: GRANT/REVOKE on a view mutates the view ACL.
                if eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .is_none()
                {
                    if let Some(v) = eng.db.find_view(name, ctx.snap, ctx.own).cloned() {
                        let groups = select_grantor_groups(
                            eng, ctx, &v.owner, &v.acl, bits, granted_by, revoke,
                        )?;
                        let prev_acl = v.acl.clone();
                        let prev_populated = v.populated;
                        let live = eng
                            .db
                            .find_view_mut(name, ctx.snap, ctx.own)
                            .expect("visible above");
                        apply_acl_delta(
                            &mut live.acl,
                            grantees,
                            &groups,
                            revoke,
                            false,
                            grant_option,
                            grant_option_for,
                            cascade,
                            &v.owner,
                        )?;
                        ctx.writes.push(WriteOp::ViewState {
                            name: name.clone(),
                            prev_acl,
                            prev_populated,
                            prev_owner: String::new(),
                        });
                        continue;
                    }
                }
                // v1.96: a bare GRANT/REVOKE name that is not a
                // table or view resolves by object kind like PG —
                // a sequence of that name takes the sequence path
                // (GRANT USAGE on x_seq with no SEQUENCE keyword).
                if eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .is_none()
                    && eng.db.find_sequence(name, ctx.snap, ctx.own).is_some()
                {
                    let one = GrantObject::Sequence(name.clone());
                    return exec_grant_revoke(
                        eng,
                        ctx,
                        privs,
                        &one,
                        grantees_in,
                        revoke,
                        granted_by,
                        grant_option,
                        grant_option_for,
                        cascade,
                    );
                }
                let t = eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .ok_or_else(|| {
                        exec_err("42P01", format!("relation \"{}\" does not exist", name))
                    })?;
                let groups =
                    select_grantor_groups(eng, ctx, &t.owner, &t.acl, bits, granted_by, revoke)?;
                {
                    let actor = grantor_actor_name(ctx, granted_by);
                    let closure = crate::storage::role_closure(&eng.db, &actor, ctx.snap, ctx.own);
                    let actor_any = crate::storage::table_privs_in(
                        &eng.db, &actor, &t, &closure, ctx.snap, ctx.own,
                    ) != 0;
                    check_stranger_grantor(&groups, actor_any, "table", name)?;
                }
                let mut next = t.clone();
                // v1.96: materialize the owner's own ACL item on
                // first GRANT/REVOKE (PG acldefault for relations),
                // so REVOKE ... FROM <owner> can subtract from it
                // (datdba_only: DELETE revoked from pg_database_owner
                // must stick for owner-members).
                if !next
                    .acl
                    .iter()
                    .any(|e| e.role == t.owner && (e.grantor.is_empty() || e.grantor == t.owner))
                {
                    next.acl.insert(
                        0,
                        crate::storage::AclEntry::new(
                            &t.owner,
                            crate::storage::PRIV_ALL_TABLE | 512,
                        ),
                    );
                }
                apply_acl_delta(
                    &mut next.acl,
                    grantees,
                    &groups,
                    revoke,
                    false,
                    grant_option,
                    grant_option_for,
                    cascade,
                    &t.owner,
                )?;
                // v1.96 (PG19 aclchk.c ExecGrant_Relation): revoking
                // relation privileges that are also column privileges
                // implicitly revokes them from every column too, per
                // SQL spec. (GRANT needs no mirror: permission checks
                // consult both relation and column ACLs.)
                if revoke {
                    let colbits = bits
                        & (crate::storage::PRIV_SELECT
                            | crate::storage::PRIV_INSERT
                            | crate::storage::PRIV_UPDATE
                            | crate::storage::PRIV_REFERENCES);
                    if colbits != 0 && !next.col_acl.is_empty() {
                        let all_cols: Vec<String> =
                            next.columns.iter().map(|(n, _)| n.clone()).collect();
                        if !all_cols.is_empty() {
                            let col_acl_view: Vec<crate::storage::AclEntry> = next
                                .col_acl
                                .iter()
                                .map(|e| crate::storage::AclEntry {
                                    role: e.role.clone(),
                                    privs: e.privs,
                                    grant_option: e.grant_option,
                                    grantor: e.grantor.clone(),
                                })
                                .collect();
                            let col_groups = select_grantor_groups(
                                eng,
                                ctx,
                                &t.owner,
                                &col_acl_view,
                                colbits,
                                granted_by,
                                revoke,
                            )?;
                            apply_col_acl_delta(
                                &mut next.col_acl,
                                &next.acl.clone(),
                                grantees,
                                &col_groups,
                                &all_cols,
                                true,
                                grant_option,
                                grant_option_for,
                                cascade,
                                &t.owner,
                            )?;
                        }
                    }
                }
                // v0.11: column-level grants.
                if !col_specs.is_empty() {
                    let cols: Vec<String> = next.columns.iter().map(|(n, _)| n.clone()).collect();
                    for s in &col_specs {
                        // v1.96: ALL (cols) expands to the four
                        // column-grantable privileges (PG19).
                        let spec_bits = match (s.priv_, object) {
                            (
                                Privilege::Select
                                | Privilege::Insert
                                | Privilege::Update
                                | Privilege::References,
                                GrantObject::Table(_),
                            ) => s.priv_.bits(),
                            (Privilege::All, GrantObject::Table(_)) => {
                                crate::storage::PRIV_SELECT
                                    | crate::storage::PRIV_INSERT
                                    | crate::storage::PRIV_UPDATE
                                    | crate::storage::PRIV_REFERENCES
                            }
                            _ => {
                                return Err(exec_err(
                                    "0A000",
                                    format!("invalid privilege {:?} for {:?}", s.priv_, object),
                                ));
                            }
                        };
                        for c in &s.columns {
                            // v1.96: system columns (tableoid et al.)
                            // are grantable in PG (atestp2 shape) and
                            // recorded in col_acl like real columns.
                            if ["tableoid", "ctid", "xmin", "xmax", "cmin", "cmax"]
                                .contains(&c.as_str())
                            {
                                continue;
                            }
                            if !cols.iter().any(|n| n == c) {
                                return Err(exec_err(
                                    "42703",
                                    format!(
                                        "column \"{}\" of relation \"{}\" does not exist",
                                        c, name
                                    ),
                                ));
                            }
                        }
                        let col_acl_view: Vec<crate::storage::AclEntry> = next
                            .col_acl
                            .iter()
                            .filter(|e| e.columns.iter().any(|c| s.columns.contains(c)))
                            .map(|e| crate::storage::AclEntry {
                                role: e.role.clone(),
                                privs: e.privs,
                                grant_option: e.grant_option,
                                grantor: e.grantor.clone(),
                            })
                            .collect();
                        let col_groups = select_grantor_groups(
                            eng,
                            ctx,
                            &t.owner,
                            &col_acl_view,
                            spec_bits,
                            granted_by,
                            revoke,
                        )?;
                        apply_col_acl_delta(
                            &mut next.col_acl,
                            &next.acl.clone(),
                            grantees,
                            &col_groups,
                            &s.columns,
                            revoke,
                            grant_option,
                            grant_option_for,
                            cascade,
                            &t.owner,
                        )?;
                    }
                }
                alter_swap(eng, ctx, name, None, next, None)?;
            }
        }
        GrantObject::Sequence(name) => {
            let s = eng
                .db
                .find_sequence(name, ctx.snap, ctx.own)
                .ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", name))
                })?;
            let seq_owner = s.owner.clone();
            let groups =
                select_grantor_groups(eng, ctx, &seq_owner, &s.acl, bits, granted_by, revoke)?;
            // Version-swap like ALTER SEQUENCE so the grant is transactional.
            let prev = s.clone();
            let versions = eng.db.sequences.get_mut(name).expect("visible above");
            let cur = versions
                .iter_mut()
                .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
                .expect("visible above");
            cur.dropped_xmax = ctx.own;
            let mut next = prev.clone();
            // v1.96 finishing pass: a GRANT/REVOKE naming the owner
            // materializes the owner's acldefault item first (PG:
            // the item exists in the ACL from that moment and is
            // authoritative for sequence_privs).
            if grantees.iter().any(|g| g == &seq_owner)
                && !next.acl.iter().any(|e| e.role == seq_owner)
            {
                next.acl.push(crate::storage::AclEntry {
                    role: seq_owner.clone(),
                    privs: crate::storage::PRIV_USAGE
                        | crate::storage::PRIV_SELECT
                        | crate::storage::PRIV_UPDATE,
                    grant_option: 0,
                    grantor: seq_owner.clone(),
                });
            }
            apply_acl_delta(
                &mut next.acl,
                grantees,
                &groups,
                revoke,
                // v1.96 finishing pass: keep_zero — a fully revoked
                // entry (notably the owner's) must persist as an
                // explicit zero item, or sequence_privs cannot tell
                // "revoked owner" from "never touched".
                true,
                grant_option,
                grant_option_for,
                cascade,
                &seq_owner,
            )?;
            next.created_xmin = ctx.own;
            next.dropped_xmax = 0;
            versions.push(next);
            ctx.writes.push(WriteOp::AlterSequence {
                name: name.to_string(),
                prev,
            });
        }
        GrantObject::Function(names) => {
            // v1.96: only EXECUTE is a valid function privilege.
            for p in privs {
                if !matches!(p.priv_, Privilege::Execute | Privilege::All) {
                    return Err(exec_err(
                        "0A000",
                        format!(
                            "invalid privilege type {} for function",
                            privilege_name(p.priv_)
                        ),
                    ));
                }
            }
            for name in names {
                let is_user_fn = eng
                .db
                .functions
                .get(name)
                .map(|vs| vs.iter().any(|f| !f.is_procedure))
                .unwrap_or(false)
                // v1.96: aggregates live in the FUNCTION grant
                // namespace too (PG19 pg_proc).
                || eng.db.aggregates.contains_key(name);
                // v1.96: builtin CAST functions are named after their
                // target type (int8(integer), text(int), ...) and live in
                // pg_proc even though is_builtin_fn doesn't list them.
                let is_cast_fn = matches!(
                    name.as_str(),
                    "int"
                        | "integer"
                        | "int2"
                        | "int4"
                        | "int8"
                        | "bigint"
                        | "smallint"
                        | "text"
                        | "varchar"
                        | "bpchar"
                        | "char"
                        | "bool"
                        | "boolean"
                        | "float4"
                        | "float8"
                        | "real"
                        | "double precision"
                        | "numeric"
                        | "decimal"
                        | "date"
                        | "time"
                        | "timestamp"
                        | "timestamptz"
                        | "interval"
                        | "uuid"
                        | "json"
                        | "jsonb"
                        | "bytea"
                        | "name"
                        | "oid"
                        | "regclass"
                        | "money"
                        | "inet"
                        | "cidr"
                        | "macaddr"
                        | "xml"
                );
                if !is_user_fn && !is_cast_fn && !crate::sql::is_builtin_fn(name) {
                    return Err(exec_err(
                        "42883",
                        format!("function \"{}\" does not exist", name),
                    ));
                }
                let prev_fn = eng.db.fn_acls.clone();
                let prev_type = eng.db.type_acls.clone();
                let prev_lang = eng.db.lang_acls.clone();
                let prev_fn_owners = eng.db.fn_owners.clone();
                let prev_type_owners = eng.db.type_owners.clone();
                let owner = eng
                    .db
                    .fn_owners
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "postgres".to_string());
                let groups = select_grantor_groups(
                    eng,
                    ctx,
                    &owner,
                    eng.db
                        .fn_acls
                        .get(name)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]),
                    bits,
                    granted_by,
                    revoke,
                )?;
                {
                    let actor = grantor_actor_name(ctx, granted_by);
                    let actor_any =
                        function_privs_for(&eng.db, &actor, name, ctx.snap, ctx.own) != 0;
                    check_stranger_grantor(&groups, actor_any, "function", name)?;
                }
                let entries = eng.db.fn_acls.entry(name.clone()).or_default();
                if entries.is_empty() {
                    // Materialize the PG default (PUBLIC EXECUTE/USAGE)
                    // so a REVOKE FROM PUBLIC has something to subtract
                    // from (v1.96 fix: this must happen for REVOKE too —
                    // an empty entry list reads back as the default).
                    entries.push(crate::storage::AclEntry::new(
                        "public",
                        crate::storage::PRIV_USAGE,
                    ));
                }
                apply_acl_delta(
                    entries,
                    grantees,
                    &groups,
                    revoke,
                    true,
                    grant_option,
                    grant_option_for,
                    cascade,
                    &owner,
                )?;
                ctx.writes.push(WriteOp::FnTypeAcls {
                    prev_fn,
                    prev_type,
                    prev_lang,
                    prev_fn_owners,
                    prev_type_owners,
                });
            }
        }
        GrantObject::Procedure(names) => {
            // v1.96: only EXECUTE is a valid function privilege.
            for p in privs {
                if !matches!(p.priv_, Privilege::Execute | Privilege::All) {
                    return Err(exec_err(
                        "0A000",
                        format!(
                            "invalid privilege type {} for procedure",
                            privilege_name(p.priv_)
                        ),
                    ));
                }
            }
            for name in names {
                let is_user_fn = eng
                    .db
                    .functions
                    .get(name)
                    .map(|vs| vs.iter().any(|f| f.is_procedure))
                    .unwrap_or(false);
                if !is_user_fn && !crate::sql::is_builtin_fn(name) {
                    return Err(exec_err(
                        "42883",
                        format!("procedure \"{}\" does not exist", name),
                    ));
                }
                let prev_fn = eng.db.fn_acls.clone();
                let prev_type = eng.db.type_acls.clone();
                let prev_lang = eng.db.lang_acls.clone();
                let prev_fn_owners = eng.db.fn_owners.clone();
                let prev_type_owners = eng.db.type_owners.clone();
                let owner = eng
                    .db
                    .fn_owners
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "postgres".to_string());
                let groups = select_grantor_groups(
                    eng,
                    ctx,
                    &owner,
                    eng.db
                        .fn_acls
                        .get(name)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]),
                    bits,
                    granted_by,
                    revoke,
                )?;
                {
                    let actor = grantor_actor_name(ctx, granted_by);
                    let actor_any =
                        function_privs_for(&eng.db, &actor, name, ctx.snap, ctx.own) != 0;
                    check_stranger_grantor(&groups, actor_any, "function", name)?;
                }
                let entries = eng.db.fn_acls.entry(name.clone()).or_default();
                if entries.is_empty() {
                    // Materialize the PG default (PUBLIC EXECUTE/USAGE)
                    // so a REVOKE FROM PUBLIC has something to subtract
                    // from (v1.96 fix: this must happen for REVOKE too —
                    // an empty entry list reads back as the default).
                    entries.push(crate::storage::AclEntry::new(
                        "public",
                        crate::storage::PRIV_USAGE,
                    ));
                }
                apply_acl_delta(
                    entries,
                    grantees,
                    &groups,
                    revoke,
                    true,
                    grant_option,
                    grant_option_for,
                    cascade,
                    &owner,
                )?;
                ctx.writes.push(WriteOp::FnTypeAcls {
                    prev_fn,
                    prev_type,
                    prev_lang,
                    prev_fn_owners,
                    prev_type_owners,
                });
            }
        }
        GrantObject::Type(name) | GrantObject::Domain(name) => {
            // v1.96 finishing pass: PG19 forbids setting privileges
            // on a multirange type directly (aclchk.c) — its range
            // type carries them.
            if let GrantObject::Type(_) = object {
                if eng
                    .db
                    .types
                    .get(name)
                    .map(|st| matches!(st.range, Some((_, true))))
                    .unwrap_or(false)
                {
                    return Err(exec_err(
                        "0A000",
                        "cannot set privileges of multirange types".to_string(),
                    ));
                }
            }
            // v1.96: ON DOMAIN requires the name to be a domain
            // (PG19 42809 wrong object type).
            if matches!(object, GrantObject::Domain(_)) {
                if let Some(st) = eng.db.types.get(name) {
                    if st.domain.is_none() {
                        return Err(exec_err(
                            "42809",
                            format!("type \"{}\" is not a domain", name),
                        ));
                    }
                }
            }
            if !eng.db.types.contains_key(name)
                && !matches!(
                    name.as_str(),
                    "int"
                        | "integer"
                        | "int2"
                        | "int4"
                        | "int8"
                        | "bigint"
                        | "smallint"
                        | "text"
                        | "varchar"
                        | "bool"
                        | "boolean"
                        | "float4"
                        | "float8"
                        | "numeric"
                        | "date"
                        | "timestamp"
                        | "timestamptz"
                        | "uuid"
                )
            {
                return Err(exec_err(
                    "42704",
                    format!("type \"{}\" does not exist", name),
                ));
            }
            let prev_fn = eng.db.fn_acls.clone();
            let prev_type = eng.db.type_acls.clone();
            let prev_lang = eng.db.lang_acls.clone();
            let prev_fn_owners = eng.db.fn_owners.clone();
            let prev_type_owners = eng.db.type_owners.clone();
            let owner = eng
                .db
                .type_owners
                .get(name)
                .cloned()
                .unwrap_or_else(|| "postgres".to_string());
            let groups = select_grantor_groups(
                eng,
                ctx,
                &owner,
                eng.db
                    .type_acls
                    .get(name)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]),
                bits,
                granted_by,
                revoke,
            )?;
            {
                let actor = grantor_actor_name(ctx, granted_by);
                let actor_any = type_privs_for(&eng.db, &actor, name, ctx.snap, ctx.own) != 0;
                check_stranger_grantor(&groups, actor_any, "type", name)?;
            }
            let entries = eng.db.type_acls.entry(name.clone()).or_default();
            if entries.is_empty() {
                // Materialize for REVOKE too (see the FUNCTION arm).
                entries.push(crate::storage::AclEntry::new(
                    "public",
                    crate::storage::PRIV_USAGE,
                ));
            }
            apply_acl_delta(
                entries,
                grantees,
                &groups,
                revoke,
                true,
                grant_option,
                grant_option_for,
                cascade,
                &owner,
            )?;
            ctx.writes.push(WriteOp::FnTypeAcls {
                prev_fn,
                prev_type,
                prev_lang,
                prev_fn_owners,
                prev_type_owners,
            });
        }
        GrantObject::AllTables
        | GrantObject::AllSequences
        | GrantObject::AllFunctionsInSchema(_)
        | GrantObject::AllProceduresInSchema(_)
        | GrantObject::AllRoutinesInSchema(_) => {
            unreachable!("expanded above")
        }
        GrantObject::Database => {
            require_superuser(eng, ctx)?;
            let prev = eng.db.db_acl.clone();
            let groups = select_grantor_groups(
                eng,
                ctx,
                "postgres",
                &eng.db.db_acl.clone(),
                bits,
                granted_by,
                revoke,
            )?;
            apply_acl_delta(
                &mut eng.db.db_acl,
                grantees,
                &groups,
                revoke,
                true,
                grant_option,
                grant_option_for,
                cascade,
                "postgres",
            )?;
            ctx.writes.push(WriteOp::DbAcl { prev });
        }
    }
    Ok(ExecResult::Command {
        tag: verb.to_string(),
    })
}

/// Merge grant/revoke bits into an ACL vector.
///
/// v1.96 rewrite (PG19 aclchk.c): ACL entries are keyed
/// (grantee, grantor) like pg_class.relacl items. `groups` carries
/// (grantor, bits) pairs from select_grantor_groups — a GRANT
/// merges each group's bits into that grantor's entry for the
/// grantee; a REVOKE strips each group's bits from that grantor's
/// entry only, after which validate_acl_backing enforces the
/// dependent-privileges rule (2BP01 / CASCADE). Entries with an
/// empty grantor are implicit defaults administered by the owner.
///
/// When `keep_zero` is false, entries left with no privileges are
/// pruned (fine for tables/sequences, whose default is deny). When
/// true, zero-priv entries are kept as explicit deny markers
/// (needed for the database ACL, whose default is allow:
/// `db_connect_allowed` treats an entry without the CONNECT bit as
/// a revoked role).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_acl_delta(
    acl: &mut Vec<crate::storage::AclEntry>,
    grantees: &[String],
    groups: &[(String, u32)],
    revoke: bool,
    keep_zero: bool,
    grant_option: bool,
    grant_option_for: bool,
    cascade: bool,
    owner: &str,
) -> Result<(), ExecError> {
    for g in grantees {
        for (grantor, gbits) in groups {
            if *gbits == 0 {
                continue;
            }
            let idx = acl.iter().position(|e| {
                e.role == *g
                    && (e.grantor == *grantor || (e.grantor.is_empty() && grantor == owner))
            });
            if !revoke {
                let e = match idx {
                    Some(i) => &mut acl[i],
                    None => {
                        acl.push(crate::storage::AclEntry {
                            role: g.clone(),
                            privs: 0,
                            grant_option: 0,
                            grantor: grantor.clone(),
                        });
                        acl.last_mut().unwrap()
                    }
                };
                if e.grantor.is_empty() {
                    e.grantor = grantor.clone();
                }
                e.privs |= gbits;
                if grant_option {
                    e.grant_option |= gbits;
                }
            } else if let Some(i) = idx {
                if grant_option_for {
                    acl[i].grant_option &= !gbits;
                } else {
                    acl[i].privs &= !gbits;
                    acl[i].grant_option &= !gbits;
                }
            } else if keep_zero {
                // v1.96 rescue: no entry for this (grantee, grantor)
                // yet — a revoke under keep_zero still records an
                // explicit zero-priv deny marker, restoring the
                // pre-v1.96 apply_acl_delta contract the database
                // ACL depends on (db_connect_allowed treats an entry
                // without CONNECT as a revoked role; without the
                // marker REVOKE CONNECT on a fresh database was a
                // silent no-op).
                acl.push(crate::storage::AclEntry {
                    role: g.clone(),
                    privs: 0,
                    grant_option: 0,
                    grantor: grantor.clone(),
                });
            }
        }
    }
    if revoke {
        validate_acl_backing(acl, owner, cascade, grantees)?;
    }
    if !keep_zero {
        acl.retain(|e| e.privs != 0 || e.grant_option != 0);
    }
    Ok(())
}

/// v1.96: column-ACL backing check, mirroring validate_acl_backing
/// at column granularity: a column entry's grantor must hold grant
/// option for its bits on every listed column — through column
/// entries (its own or PUBLIC's) or table-level grant options.
pub(crate) fn validate_col_acl_backing(
    col_acl: &mut Vec<crate::storage::ColAclEntry>,
    table_acl: &[crate::storage::AclEntry],
    owner: &str,
    cascade: bool,
    revoked: &[String],
) -> Result<(), ExecError> {
    fn col_go(
        col_acl: &[crate::storage::ColAclEntry],
        table_acl: &[crate::storage::AclEntry],
        owner: &str,
        who: &str,
        bit: u32,
        col: &str,
    ) -> bool {
        if who == owner {
            return true;
        }
        if acl_go_bits(table_acl, owner, who) & bit == bit {
            return true;
        }
        col_acl.iter().any(|e| {
            (e.role == who || e.role == "public")
                && e.grant_option & bit == bit
                && e.columns.iter().any(|c| c == col)
        })
    }
    // v1.96 finishing pass: same scoping as validate_acl_backing —
    // only grants flowing through the revoked grantees can be
    // dependent (CASCADE extends the affected set as it strips).
    let mut affected: Vec<String> = revoked.to_vec();
    loop {
        let mut bad: Option<(usize, u32, Vec<String>)> = None;
        'outer: for (i, e) in col_acl.iter().enumerate() {
            if e.grantor.is_empty() || e.grantor == owner || e.privs == 0 {
                continue;
            }
            if !affected.iter().any(|a| a == &e.grantor) {
                continue;
            }
            let mut bits = e.privs;
            while bits != 0 {
                let bit = bits & bits.wrapping_neg();
                bits &= bits - 1;
                let unbacked_cols: Vec<String> = e
                    .columns
                    .iter()
                    .filter(|c| !col_go(col_acl, table_acl, owner, &e.grantor, bit, c))
                    .cloned()
                    .collect();
                if !unbacked_cols.is_empty() {
                    bad = Some((i, bit, unbacked_cols));
                    break 'outer;
                }
            }
        }
        let Some((i, bit, cols)) = bad else {
            return Ok(());
        };
        if !cascade {
            return Err(exec_err("2BP01", "dependent privileges exist".to_string()));
        }
        // CASCADE: drop the unbacked columns for that bit; when the
        // bit is unbacked on every column, drop the bit entirely.
        let stripped_grantee = col_acl[i].role.clone();
        if !affected.contains(&stripped_grantee) {
            affected.push(stripped_grantee);
        }
        let e = &mut col_acl[i];
        if cols.len() == e.columns.len() {
            e.privs &= !bit;
            e.grant_option &= !bit;
        } else {
            e.columns.retain(|c| !cols.contains(c));
        }
        col_acl.retain(|e| e.privs != 0 && !e.columns.is_empty());
    }
}

/// Merge column grant/revoke bits into a table's `col_acl` vector
/// (v1.96: (grantee, grantor)-keyed entries, PG19 grantor groups,
/// dependent validation against `table_acl` for backing).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_col_acl_delta(
    col_acl: &mut Vec<crate::storage::ColAclEntry>,
    table_acl: &[crate::storage::AclEntry],
    grantees: &[String],
    groups: &[(String, u32)],
    columns: &[String],
    revoke: bool,
    grant_option: bool,
    grant_option_for: bool,
    cascade: bool,
    owner: &str,
) -> Result<(), ExecError> {
    for g in grantees {
        for (grantor, gbits) in groups {
            if *gbits == 0 {
                continue;
            }
            if !revoke {
                if let Some(e) = col_acl
                    .iter_mut()
                    .find(|e| e.role == *g && e.grantor == *grantor && e.privs == *gbits)
                {
                    for c in columns {
                        if !e.columns.contains(c) {
                            e.columns.push(c.clone());
                        }
                    }
                    if grant_option {
                        e.grant_option |= gbits;
                    }
                } else {
                    col_acl.push(crate::storage::ColAclEntry {
                        role: g.clone(),
                        privs: *gbits,
                        columns: columns.to_vec(),
                        grant_option: if grant_option { *gbits } else { 0 },
                        grantor: grantor.clone(),
                    });
                }
            } else {
                // v1.96: revoke from EVERY (grantee, grantor) entry —
                // a grantee can hold several entries (one per privs
                // set) for the same grantor, and the revoked columns
                // may be a subset of an entry's columns, in which
                // case the entry splits: unaffected columns keep the
                // old privs, affected columns keep privs & !gbits.
                let mut extra: Vec<crate::storage::ColAclEntry> = Vec::new();
                for e in col_acl.iter_mut() {
                    if !(e.role == *g
                        && (e.grantor == *grantor || (e.grantor.is_empty() && grantor == owner)))
                    {
                        continue;
                    }
                    let overlap: Vec<String> = e
                        .columns
                        .iter()
                        .filter(|c| columns.contains(c))
                        .cloned()
                        .collect();
                    if overlap.is_empty() {
                        continue;
                    }
                    if grant_option_for {
                        if overlap.len() == e.columns.len() {
                            e.grant_option &= !gbits;
                        } else {
                            let mut split = e.clone();
                            split.columns = overlap;
                            split.grant_option &= !gbits;
                            e.columns.retain(|c| !columns.contains(c));
                            extra.push(split);
                        }
                    } else if e.privs & gbits != 0 {
                        let rest = e.privs & !gbits;
                        if overlap.len() == e.columns.len() {
                            e.privs = rest;
                            e.grant_option &= rest;
                        } else {
                            let mut split = e.clone();
                            split.columns = overlap;
                            split.privs = rest;
                            split.grant_option &= rest;
                            e.columns.retain(|c| !columns.contains(c));
                            extra.push(split);
                        }
                    }
                }
                col_acl.extend(extra);
            }
        }
    }
    if revoke {
        validate_col_acl_backing(col_acl, table_acl, owner, cascade, grantees)?;
    }
    col_acl.retain(|e| e.privs != 0 && !e.columns.is_empty());
    Ok(())
}

/// ALTER TABLE name OWNER TO new_owner.
pub(crate) fn alter_owner_to(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    new_owner: &str,
) -> Result<ExecResult, ExecError> {
    // v1.96 (PG19 ATExecChangeOwner): the executor must hold the
    // current owner's privileges (inheritance counts — an INHERIT
    // membership in the owning role suffices) and must be able to
    // SET ROLE to the new owner (a SET FALSE membership does not).
    {
        let t = eng
            .db
            .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
        let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
        let superuser = crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own);
        if !superuser && !closure.iter().any(|r| r == &t.owner) {
            return Err(exec_err(
                "42501",
                format!("must be owner of table {}", name),
            ));
        }
        if !superuser
            && ctx.role != new_owner
            && !crate::storage::member_can_set_role(&eng.db, ctx.role, new_owner, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!(
                    "permission denied to change owner of table {}: must be able to SET ROLE to the new owner",
                    name
                ),
            ));
        }
    }
    if eng.db.find_role(new_owner, ctx.snap, ctx.own).is_none() {
        return Err(exec_err(
            "42704",
            format!("role \"{}\" does not exist", new_owner),
        ));
    }
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    let mut next = t;
    let old_owner = next.owner.clone();
    next.owner = new_owner.to_string();
    // v1.96 finishing pass: PG19 aclnewowner — on an owner change,
    // the old owner is replaced by the new owner wherever it appears
    // as grantor OR grantee, and any resulting duplicate
    // (grantee, grantor) entries merge. Without the grantor half, a
    // later REVOKE sees a grantor who holds no grant option on the
    // object and the dependent-privileges check misfires
    // (foreign_key suite). Empty grantors mean "the owner" already
    // and follow automatically.
    if old_owner != new_owner {
        for e in next.acl.iter_mut() {
            if e.grantor == old_owner {
                e.grantor = new_owner.to_string();
            }
            if e.role == old_owner {
                e.role = new_owner.to_string();
            }
        }
        let mut merged: Vec<crate::storage::AclEntry> = Vec::new();
        for e in next.acl.drain(..) {
            if let Some(m) = merged
                .iter_mut()
                .find(|m| m.role == e.role && m.grantor == e.grantor)
            {
                m.privs |= e.privs;
                m.grant_option |= e.grant_option;
            } else {
                merged.push(e);
            }
        }
        next.acl = merged;
        for e in next.col_acl.iter_mut() {
            if e.grantor == old_owner {
                e.grantor = new_owner.to_string();
            }
            if e.role == old_owner {
                e.role = new_owner.to_string();
            }
        }
        let mut merged: Vec<crate::storage::ColAclEntry> = Vec::new();
        for e in next.col_acl.drain(..) {
            if let Some(m) = merged
                .iter_mut()
                .find(|m| m.role == e.role && m.grantor == e.grantor && m.columns == e.columns)
            {
                m.privs |= e.privs;
                m.grant_option |= e.grant_option;
            } else {
                merged.push(e);
            }
        }
        next.col_acl = merged;
    }
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}

/// v1.96: CREATE SCHEMA — the engine is single-namespace; the name is
/// registered (existence gates DROP SCHEMA / IN SCHEMA clauses).
pub(crate) fn exec_create_schema(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_not_exists: bool,
    authorization: &Option<String>,
) -> Result<ExecResult, ExecError> {
    if let Some(auth) = authorization {
        if eng.db.find_role(auth, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", auth),
            ));
        }
        // v1.96 finishing pass: PG19 CreateSchema — creating a schema
        // owned by another role requires being able to SET ROLE to
        // that role (or superuser).
        if auth != ctx.role
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            && !crate::storage::member_can_set_role(&eng.db, ctx.role, auth, ctx.snap, ctx.own)
        {
            return Err(exec_err(
                "42501",
                format!(
                    "permission denied: must be able to SET ROLE to \"{}\" to create a schema for it",
                    auth
                ),
            ));
        }
    }
    if eng.db.schemas.iter().any(|s| s == name) {
        if if_not_exists {
            return Ok(ExecResult::Command {
                tag: "CREATE SCHEMA".to_string(),
            });
        }
        return Err(exec_err(
            "42P06",
            format!("schema \"{}\" already exists", name),
        ));
    }
    let prev = eng.db.schemas.clone();
    let prev_owners = eng.db.schema_owners.clone();
    let prev_acls = eng.db.schema_acls.clone();
    eng.db.schemas.push(name.to_string());
    // v1.96: the schema owner is the AUTHORIZATION role or the creator.
    let owner = authorization
        .clone()
        .unwrap_or_else(|| ctx.role.to_string());
    eng.db.schema_owners.insert(name.to_string(), owner.clone());
    // v1.96: ALTER DEFAULT PRIVILEGES ... ON SCHEMAS rules for the
    // owning role seed the new schema's ACL (global rules only — an
    // IN SCHEMA rule cannot target schemas). Bits are masked to the
    // schema family (USAGE | CREATE).
    let seeded: Vec<crate::storage::AclEntry> = eng
        .db
        .default_acls
        .iter()
        .filter(|d| {
            d.kind == 'n'
                && d.creator == owner
                && d.schema.is_empty()
                && d.privs & (crate::storage::PRIV_USAGE | crate::storage::PRIV_REFERENCES) != 0
        })
        .map(|d| crate::storage::AclEntry {
            role: d.grantee.clone(),
            privs: d.privs & (crate::storage::PRIV_USAGE | crate::storage::PRIV_REFERENCES),
            grant_option: 0,
            grantor: d.creator.clone(),
        })
        .collect();
    if !seeded.is_empty() {
        eng.db.schema_acls.insert(name.to_string(), seeded);
    }
    ctx.writes.push(WriteOp::SchemaSet {
        prev,
        prev_owners,
        prev_acls,
    });
    Ok(ExecResult::Command {
        tag: "CREATE SCHEMA".to_string(),
    })
}

/// v1.96 finishing pass: the schema a CREATE places `name` in — the
/// explicit qualifier when the name is written qualified, else the
/// first search_path entry naming an existing schema ($user resolves
/// to the current role), else public.
pub(crate) fn create_schema_of(eng: &Engine, ctx: &StmtCtx, name: &str) -> String {
    if let Some((sch, _)) = name.split_once('.') {
        return sch.to_string();
    }
    let sp = crate::exec::installed_guc("search_path").unwrap_or_default();
    for raw in sp.split(',') {
        let e = raw.trim().trim_matches('"');
        if e.is_empty() {
            continue;
        }
        let cand = if e == "$user" {
            ctx.role.to_string()
        } else {
            e.to_string()
        };
        if eng.db.schemas.iter().any(|s| s == &cand) {
            return cand;
        }
    }
    "public".to_string()
}

/// Record object -> schema attribution for a freshly created object
/// (transactional via WriteOp::ObjSchemaSet).
pub(crate) fn note_obj_schema(eng: &mut Engine, ctx: &mut StmtCtx, key: &str, written_name: &str) {
    let sch = create_schema_of(eng, ctx, written_name);
    if eng.db.obj_schema.get(key) != Some(&sch) {
        let prev = eng.db.obj_schema.clone();
        eng.db.obj_schema.insert(key.to_string(), sch);
        ctx.writes.push(WriteOp::ObjSchemaSet { prev });
    }
}

/// Drop object -> schema attribution (object dropped or renamed).
pub(crate) fn forget_obj_schema(eng: &mut Engine, ctx: &mut StmtCtx, key: &str) {
    if eng.db.obj_schema.contains_key(key) {
        let prev = eng.db.obj_schema.clone();
        eng.db.obj_schema.remove(key);
        ctx.writes.push(WriteOp::ObjSchemaSet { prev });
    }
}

/// v1.96: DROP SCHEMA. Objects named `<schema>.` block RESTRICT;
/// CASCADE drops them first.
pub(crate) fn exec_drop_schema(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        if !eng.db.schemas.iter().any(|s| s == name) {
            if if_exists {
                continue;
            }
            return Err(exec_err(
                "3F000",
                format!("schema \"{}\" does not exist", name),
            ));
        }
        let prefix = format!("{}.", name);
        // v1.96 finishing pass: contents of the schema are (a) objects
        // whose storage key is schema-qualified, and (b) objects whose
        // CREATE-time attribution (obj_schema) names this schema —
        // the common suite pattern is `SET search_path = s` followed
        // by unqualified CREATEs, which the flat key space cannot
        // otherwise associate with `s`.
        let attributed = |k: &String| eng.db.obj_schema.get(k) == Some(name);
        let tables: Vec<String> = eng
            .db
            .tables
            .keys()
            .filter(|n| n.starts_with(&prefix) || attributed(n))
            .filter(|n| {
                eng.db
                    .find_table(n, ctx.snap, &ctx.all_xids, ctx.session)
                    .is_some()
            })
            .cloned()
            .collect();
        let views: Vec<String> = eng
            .db
            .views
            .keys()
            .filter(|n| n.starts_with(&prefix) || attributed(n))
            .filter(|n| eng.db.find_view(n, ctx.snap, ctx.own).is_some())
            .cloned()
            .collect();
        let seqs: Vec<String> = eng
            .db
            .sequences
            .keys()
            .filter(|n| n.starts_with(&prefix) || attributed(n))
            .filter(|n| eng.db.find_sequence(n, ctx.snap, ctx.own).is_some())
            .cloned()
            .collect();
        let fns: Vec<String> = eng
            .db
            .functions
            .keys()
            .filter(|n| n.starts_with(&prefix) || attributed(n))
            .cloned()
            .collect();
        if (!tables.is_empty() || !views.is_empty() || !seqs.is_empty() || !fns.is_empty())
            && !cascade
        {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop schema \"{}\" because other objects depend on it",
                    name
                ),
            ));
        }
        if !views.is_empty() {
            exec_drop_view(eng, ctx, &views, true, true)?;
        }
        if !tables.is_empty() {
            exec_drop(eng, ctx, &tables, true, true)?;
        }
        // Sequences after tables (serial-owned sequences are dropped
        // with their tables; only standalone ones remain here).
        let seqs: Vec<String> = seqs
            .into_iter()
            .filter(|n| eng.db.find_sequence(n, ctx.snap, ctx.own).is_some())
            .collect();
        if !seqs.is_empty() {
            exec_drop_sequence(eng, ctx, &seqs, true)?;
        }
        // Functions last (nothing in this engine hard-depends on them
        // at catalog level except operators, which CASCADE handles).
        for fname in &fns {
            let overloads: Vec<(Vec<String>, bool)> = eng
                .db
                .functions
                .get(fname)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|f| (f.arg_types.clone(), f.is_procedure))
                .collect();
            for (atypes, is_proc) in overloads {
                exec_drop_function(eng, ctx, fname, &atypes, true, true, is_proc, false, false)?;
            }
        }
        let prev = eng.db.schemas.clone();
        let prev_owners = eng.db.schema_owners.clone();
        let prev_acls = eng.db.schema_acls.clone();
        eng.db.schemas.retain(|s| s != name);
        eng.db.schema_owners.remove(name);
        eng.db.schema_acls.remove(name);
        ctx.writes.push(WriteOp::SchemaSet {
            prev,
            prev_owners,
            prev_acls,
        });
        // v1.96 finishing pass: attribution entries for objects that
        // no longer exist (or were just cascade-dropped through a
        // path that does not forget) are pruned with the schema.
        {
            let prev = eng.db.obj_schema.clone();
            eng.db.obj_schema.retain(|k, v| {
                v != name
                    || eng.db.tables.contains_key(k)
                    || eng.db.views.contains_key(k)
                    || eng.db.sequences.contains_key(k)
                    || eng.db.functions.contains_key(k)
            });
            if eng.db.obj_schema != prev {
                ctx.writes.push(WriteOp::ObjSchemaSet { prev });
            }
        }
        // v1.96: default-ACL rules scoped to the dropped schema go
        // away with it (PG19 RemoveDefaultAclInSchema-ish behavior
        // via DROP SCHEMA CASCADE on pg_default_acl rows).
        if eng.db.default_acls.iter().any(|d| &d.schema == name) {
            let prev = eng.db.default_acls.clone();
            eng.db.default_acls.retain(|d| &d.schema != name);
            ctx.writes.push(WriteOp::DefaultAcls { prev });
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP SCHEMA".to_string(),
    })
}

/// v1.96: REINDEX — indexes are logical in this engine; validate and
/// check MAINTAIN-level permission, then succeed.
pub(crate) fn exec_reindex(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    kind: char,
    name: &Option<String>,
    _concurrently: bool,
) -> Result<ExecResult, ExecError> {
    let maintain_ok = |t: &crate::storage::Table| {
        t.owner == ctx.role
            || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            || crate::storage::table_privs(&eng.db, ctx.role, t, ctx.snap, ctx.own) & 512 != 0
    };
    match kind {
        't' => {
            let n = name.as_deref().unwrap_or("");
            let t = eng
                .db
                .find_table(n, ctx.snap, &ctx.all_xids, ctx.session)
                .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", n)))?;
            if !maintain_ok(t) {
                return Err(exec_err(
                    "42501",
                    format!("permission denied: must be owner of table \"{}\"", n),
                ));
            }
        }
        'i' => {
            let n = name.as_deref().unwrap_or("");
            let idx = eng
                .db
                .indexes
                .get(n)
                .filter(|i| crate::storage::index_visible(&i.def, ctx.snap, &[ctx.own]));
            let Some(idx) = idx else {
                return Err(exec_err("42704", format!("index \"{}\" does not exist", n)));
            };
            let tname = idx.def.table.clone();
            if let Some(t) = eng
                .db
                .find_table(&tname, ctx.snap, &ctx.all_xids, ctx.session)
            {
                if !maintain_ok(t) {
                    return Err(exec_err(
                        "42501",
                        format!("permission denied: must be owner of table \"{}\"", tname),
                    ));
                }
            }
        }
        's' => {
            let n = name.as_deref().unwrap_or("");
            if !eng.db.schemas.iter().any(|s| s == n) {
                return Err(exec_err(
                    "3F000",
                    format!("schema \"{}\" does not exist", n),
                ));
            }
            // v1.96: REINDEX SCHEMA requires schema ownership or
            // pg_maintain membership (PG19: plain MAINTAIN grantees
            // fail with "must be owner of schema"; pg_maintain
            // members succeed).
            let owner = eng
                .db
                .schema_owners
                .get(n)
                .cloned()
                .unwrap_or_else(|| eng.db.db_owner.clone());
            let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
            if ctx.role != owner
                && !closure.iter().any(|r| r == "pg_maintain")
                && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            {
                return Err(exec_err("42501", format!("must be owner of schema {}", n)));
            }
        }
        'd' => {
            // v1.96 finishing pass: PG19 indexcmds.c — REINDEX
            // DATABASE may only name the currently open database
            // (0A000 otherwise). This server hosts one database,
            // "postgres".
            if let Some(n) = name.as_deref() {
                if n != "postgres" {
                    return Err(exec_err(
                        "0A000",
                        "can only reindex the currently open database",
                    ));
                }
            }
        }
        _ => {}
    }
    Ok(ExecResult::Command {
        tag: "REINDEX".to_string(),
    })
}

/// v1.96: CLUSTER — no physical reordering in this engine; validate
/// + MAINTAIN-level permission, then succeed.
pub(crate) fn exec_cluster(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &Option<String>,
    index: &Option<String>,
) -> Result<ExecResult, ExecError> {
    if let Some(n) = name {
        let t = eng
            .db
            .find_table(n, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", n)))?
            .clone();
        let ok = t.owner == ctx.role
            || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            || crate::storage::table_privs(&eng.db, ctx.role, &t, ctx.snap, ctx.own) & 512 != 0;
        if !ok {
            return Err(exec_err(
                "42501",
                format!("permission denied: must be owner of table \"{}\"", n),
            ));
        }
        // v1.96 finishing pass: PG19 cluster.c guards.
        let is_partitioned_parent = t
            .partition
            .as_ref()
            .map(|p| p.is_partitioned)
            .unwrap_or(false);
        if let Some(ix) = index {
            let idef = eng
                .db
                .indexes
                .get(ix)
                .filter(|i| crate::storage::index_visible(&i.def, ctx.snap, &[ctx.own]))
                .map(|i| i.def.clone());
            let idef = match idef {
                Some(d) => d,
                None => {
                    return Err(exec_err(
                        "42704",
                        format!("index \"{}\" does not exist", ix),
                    ));
                }
            };
            if idef.table != *n {
                return Err(exec_err(
                    "42704",
                    format!("\"{}\" is not an index for table \"{}\"", ix, n),
                ));
            }
            if idef.predicate.is_some() {
                return Err(exec_err(
                    "0A000",
                    format!("cannot cluster on partial index \"{}\"", ix),
                ));
            }
            // A partitioned parent itself is never marked clustered
            // (its partitions are); PG leaves the parent unmarked,
            // so a later bare CLUSTER on it still errors.
            if !is_partitioned_parent {
                eng.db.cluster_marks.insert(n.clone(), ix.clone());
            }
        } else if !eng.db.cluster_marks.contains_key(n) {
            return Err(exec_err(
                "55000",
                format!("there is no previously clustered index for table \"{}\"", n),
            ));
        }
    }
    Ok(ExecResult::Command {
        tag: "CLUSTER".to_string(),
    })
}

/// v1.96: REASSIGN OWNED BY ... TO ... — move ownership of tables,
/// sequences, views, functions and types.
pub(crate) fn exec_reassign_owned(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    from: &[String],
    to: &str,
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    for f in from {
        if eng.db.find_role(f, ctx.snap, ctx.own).is_none() {
            return Err(exec_err("42704", format!("role \"{}\" does not exist", f)));
        }
    }
    if eng.db.find_role(to, ctx.snap, ctx.own).is_none() {
        return Err(exec_err("42704", format!("role \"{}\" does not exist", to)));
    }
    let from_set: Vec<&str> = from.iter().map(|s| s.as_str()).collect();
    let tables: Vec<String> = eng
        .db
        .tables
        .keys()
        .filter(|n| {
            eng.db
                .find_table(n, ctx.snap, &ctx.all_xids, ctx.session)
                .map(|t| from_set.contains(&t.owner.as_str()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for tn in tables {
        alter_owner_to(eng, ctx, &tn, to)?;
    }
    // Sequences: version-swap owner change.
    let seqs: Vec<String> = eng
        .db
        .sequences
        .keys()
        .filter(|n| {
            eng.db
                .find_sequence(n, ctx.snap, ctx.own)
                .map(|s| from_set.contains(&s.owner.as_str()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for sn in seqs {
        let prev = eng
            .db
            .find_sequence(&sn, ctx.snap, ctx.own)
            .expect("visible above")
            .clone();
        let versions = eng.db.sequences.get_mut(&sn).expect("present");
        let cur = versions
            .iter_mut()
            .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
            .expect("visible above");
        cur.dropped_xmax = ctx.own;
        let mut next = prev.clone();
        next.owner = to.to_string();
        next.created_xmin = ctx.own;
        next.dropped_xmax = 0;
        versions.push(next);
        ctx.writes.push(WriteOp::AlterSequence {
            name: sn.clone(),
            prev,
        });
    }
    // Views + function/type owners ride the FnTypeAcls op maps.
    {
        let prev_fn = eng.db.fn_acls.clone();
        let prev_type = eng.db.type_acls.clone();
        let prev_lang = eng.db.lang_acls.clone();
        let prev_fn_owners = eng.db.fn_owners.clone();
        let prev_type_owners = eng.db.type_owners.clone();
        let mut changed = false;
        for v in eng.db.fn_owners.values_mut() {
            if from_set.contains(&v.as_str()) {
                *v = to.to_string();
                changed = true;
            }
        }
        for v in eng.db.type_owners.values_mut() {
            if from_set.contains(&v.as_str()) {
                *v = to.to_string();
                changed = true;
            }
        }
        if changed {
            ctx.writes.push(WriteOp::FnTypeAcls {
                prev_fn,
                prev_type,
                prev_lang,
                prev_fn_owners,
                prev_type_owners,
            });
        }
    }
    let views: Vec<String> = eng
        .db
        .views
        .keys()
        .filter(|n| {
            eng.db
                .find_view(n, ctx.snap, ctx.own)
                .map(|v| from_set.contains(&v.owner.as_str()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for vn in views {
        let v = eng
            .db
            .find_view(&vn, ctx.snap, ctx.own)
            .expect("visible")
            .clone();
        let live = eng
            .db
            .find_view_mut(&vn, ctx.snap, ctx.own)
            .expect("visible");
        live.owner = to.to_string();
        ctx.writes.push(WriteOp::ViewState {
            name: vn.clone(),
            prev_acl: v.acl.clone(),
            prev_populated: v.populated,
            prev_owner: v.owner.clone(),
        });
    }
    Ok(ExecResult::Command {
        tag: "REASSIGN OWNED".to_string(),
    })
}

/// v1.96: DROP OWNED BY ... — drop the roles' objects and revoke
/// every privilege they hold or granted (PG19 DropOwnedStmt).
pub(crate) fn exec_drop_owned(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    roles: &[String],
    _cascade: bool,
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    for role in roles {
        if eng.db.find_role(role, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", role),
            ));
        }
    }
    for role in roles {
        // Drop owned tables/views/sequences/functions/types.
        let tables: Vec<String> = eng
            .db
            .tables
            .keys()
            .filter(|n| {
                eng.db
                    .find_table(n, ctx.snap, &ctx.all_xids, ctx.session)
                    .map(|t| &t.owner == role)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !tables.is_empty() {
            exec_drop(eng, ctx, &tables, true, true)?;
        }
        let views: Vec<String> = eng
            .db
            .views
            .keys()
            .filter(|n| {
                eng.db
                    .find_view(n, ctx.snap, ctx.own)
                    .map(|v| &v.owner == role)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !views.is_empty() {
            exec_drop_view(eng, ctx, &views, true, true)?;
        }
        let seqs: Vec<String> = eng
            .db
            .sequences
            .keys()
            .filter(|n| {
                eng.db
                    .find_sequence(n, ctx.snap, ctx.own)
                    .map(|s| &s.owner == role)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if !seqs.is_empty() {
            exec_drop_sequence(eng, ctx, &seqs, true)?;
        }
        // v1.96: DROP OWNED also drops owned functions, aggregates,
        // types, large objects and schemas (PG19 DropOwnedObjects) —
        // without this the final DROP ROLE still sees fn_owners /
        // type_owners dependencies and fails 2BP01.
        let owned_fns: Vec<String> = eng
            .db
            .fn_owners
            .iter()
            .filter(|(_, o)| *o == role)
            .map(|(n, _)| n.clone())
            .collect();
        for fname in owned_fns {
            if eng.db.aggregates.contains_key(&fname) {
                super::ddl_func::exec_drop_aggregate(eng, ctx, &fname, &[], true, true)?;
            } else if let Some(overloads) = eng.db.functions.get(&fname).cloned() {
                for f in overloads {
                    super::ddl_func::exec_drop_function(
                        eng,
                        ctx,
                        &fname,
                        &f.arg_types,
                        true,
                        true,
                        f.is_procedure,
                        false,
                        false,
                    )?;
                }
            }
        }
        let owned_types: Vec<String> = eng
            .db
            .type_owners
            .iter()
            .filter(|(_, o)| *o == role)
            .map(|(n, _)| n.clone())
            .collect();
        if !owned_types.is_empty() {
            super::ddl_types::exec_drop_type(eng, ctx, &owned_types, true, false)?;
        }
        let owned_los: Vec<i64> = eng
            .db
            .lo_owners
            .iter()
            .filter(|(_, o)| *o == role)
            .map(|(oid, _)| *oid)
            .collect();
        if !owned_los.is_empty() {
            let prev = eng.db.large_objects.clone();
            let prev_owners = eng.db.lo_owners.clone();
            let prev_acls = eng.db.lo_acls.clone();
            for oid in owned_los {
                eng.db.large_objects.remove(&oid);
                eng.db.lo_owners.remove(&oid);
                eng.db.lo_acls.remove(&oid);
            }
            ctx.writes.push(WriteOp::LargeObjects {
                prev,
                prev_owners,
                prev_acls,
            });
        }
        let owned_schemas: Vec<String> = eng
            .db
            .schema_owners
            .iter()
            .filter(|(_, o)| *o == role)
            .map(|(n, _)| n.clone())
            .collect();
        if !owned_schemas.is_empty() {
            let prev = eng.db.schemas.clone();
            let prev_owners = eng.db.schema_owners.clone();
            for sname in owned_schemas {
                eng.db.schemas.retain(|n| n != &sname);
                eng.db.schema_owners.remove(&sname);
            }
            ctx.writes.push(WriteOp::SchemaSet {
                prev,
                prev_owners,
                prev_acls: eng.db.schema_acls.clone(),
            });
        }
        // Revoke privileges held or granted on every remaining table.
        let tnames: Vec<String> = eng
            .db
            .tables
            .keys()
            .filter(|n| {
                eng.db
                    .find_table(n, ctx.snap, &ctx.all_xids, ctx.session)
                    .is_some()
            })
            .cloned()
            .collect();
        for tn in tnames {
            let t = eng
                .db
                .find_table(&tn, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("visible")
                .clone();
            if t.acl.iter().any(|e| &e.role == role || &e.grantor == role)
                || t.col_acl
                    .iter()
                    .any(|e| &e.role == role || &e.grantor == role)
            {
                let mut next = t.clone();
                next.acl.retain(|e| &e.role != role && &e.grantor != role);
                next.col_acl
                    .retain(|e| &e.role != role && &e.grantor != role);
                alter_swap(eng, ctx, &tn, None, next, None)?;
            }
        }
        // v1.96: strip privileges held/granted on views too.
        let vnames: Vec<String> = eng.db.views.keys().cloned().collect();
        for vn in vnames {
            let v = eng.db.find_view(&vn, ctx.snap, ctx.own).cloned();
            if let Some(v) = v {
                if v.acl.iter().any(|e| &e.role == role || &e.grantor == role) {
                    let prev_acl = v.acl.clone();
                    let prev_populated = v.populated;
                    let live = eng
                        .db
                        .find_view_mut(&vn, ctx.snap, ctx.own)
                        .expect("visible above");
                    live.acl.retain(|e| &e.role != role && &e.grantor != role);
                    ctx.writes.push(WriteOp::ViewState {
                        name: vn.clone(),
                        prev_acl,
                        prev_populated,
                        prev_owner: String::new(),
                    });
                }
            }
        }
        // Database ACL + fn/type/lang ACLs + default ACLs.
        eng.db
            .db_acl
            .retain(|e| &e.role != role && &e.grantor != role);
        {
            let prev_fn = eng.db.fn_acls.clone();
            let prev_type = eng.db.type_acls.clone();
            let prev_lang = eng.db.lang_acls.clone();
            let prev_fn_owners = eng.db.fn_owners.clone();
            let prev_type_owners = eng.db.type_owners.clone();
            for m in [
                &mut eng.db.fn_acls,
                &mut eng.db.type_acls,
                &mut eng.db.lang_acls,
            ] {
                for entries in m.values_mut() {
                    entries.retain(|e| &e.role != role && &e.grantor != role);
                }
            }
            ctx.writes.push(WriteOp::FnTypeAcls {
                prev_fn,
                prev_type,
                prev_lang,
                prev_fn_owners,
                prev_type_owners,
            });
        }
        if eng
            .db
            .default_acls
            .iter()
            .any(|d| &d.creator == role || &d.grantee == role)
        {
            let prev = eng.db.default_acls.clone();
            eng.db
                .default_acls
                .retain(|d| &d.creator != role && &d.grantee != role);
            ctx.writes.push(WriteOp::DefaultAcls { prev });
        }
        // v1.96: strip privileges held/granted on sequences, large
        // objects, and schemas (the fn/type/table strips above did
        // not cover these stores).
        let seqnames: Vec<String> = eng.db.sequences.keys().cloned().collect();
        for sn in seqnames {
            let s = eng.db.find_sequence(&sn, ctx.snap, ctx.own).cloned();
            if let Some(s) = s {
                if s.acl.iter().any(|e| &e.role == role || &e.grantor == role) {
                    let prev = s.clone();
                    let versions = eng.db.sequences.get_mut(&sn).expect("sequence exists");
                    let cur = versions
                        .iter_mut()
                        .find(|x| crate::storage::seq_visible(x, ctx.snap, ctx.own))
                        .expect("visible above");
                    cur.dropped_xmax = ctx.own;
                    let mut next = prev.clone();
                    next.acl.retain(|e| &e.role != role && &e.grantor != role);
                    next.created_xmin = ctx.own;
                    next.dropped_xmax = 0;
                    versions.push(next);
                    ctx.writes.push(WriteOp::AlterSequence {
                        name: sn.clone(),
                        prev,
                    });
                }
            }
        }
        if eng.db.lo_acls.values().any(|entries| {
            entries
                .iter()
                .any(|e| &e.role == role || &e.grantor == role)
        }) || eng.db.schema_acls.values().any(|entries| {
            entries
                .iter()
                .any(|e| &e.role == role || &e.grantor == role)
        }) {
            let prev = eng.db.large_objects.clone();
            let prev_owners = eng.db.lo_owners.clone();
            let prev_acls = eng.db.lo_acls.clone();
            for entries in eng.db.lo_acls.values_mut() {
                entries.retain(|e| &e.role != role && &e.grantor != role);
            }
            ctx.writes.push(WriteOp::LargeObjects {
                prev,
                prev_owners,
                prev_acls,
            });
            let prev = eng.db.schemas.clone();
            let prev_owners = eng.db.schema_owners.clone();
            let prev_acls = eng.db.schema_acls.clone();
            for entries in eng.db.schema_acls.values_mut() {
                entries.retain(|e| &e.role != role && &e.grantor != role);
            }
            ctx.writes.push(WriteOp::SchemaSet {
                prev,
                prev_owners,
                prev_acls,
            });
        }
        // v1.96: sweep owner records whose object no longer exists
        // (e.g. a CREATE that failed after the owner was recorded,
        // or a drop path that bypassed the cleanup hooks).
        {
            let stale_fns: Vec<String> = eng
                .db
                .fn_owners
                .iter()
                .filter(|(n, o)| {
                    *o == role
                        && !eng.db.functions.contains_key(*n)
                        && !eng.db.aggregates.contains_key(*n)
                })
                .map(|(n, _)| n.clone())
                .collect();
            let stale_types: Vec<String> = eng
                .db
                .type_owners
                .iter()
                .filter(|(n, o)| *o == role && !eng.db.types.contains_key(*n))
                .map(|(n, _)| n.clone())
                .collect();
            if !stale_fns.is_empty() || !stale_types.is_empty() {
                let prev_fn = eng.db.fn_acls.clone();
                let prev_type = eng.db.type_acls.clone();
                let prev_lang = eng.db.lang_acls.clone();
                let prev_fn_owners = eng.db.fn_owners.clone();
                let prev_type_owners = eng.db.type_owners.clone();
                for n in stale_fns {
                    eng.db.fn_owners.remove(&n);
                    eng.db.fn_acls.remove(&n);
                }
                for n in stale_types {
                    eng.db.type_owners.remove(&n);
                    eng.db.type_acls.remove(&n);
                }
                ctx.writes.push(WriteOp::FnTypeAcls {
                    prev_fn,
                    prev_type,
                    prev_lang,
                    prev_fn_owners,
                    prev_type_owners,
                });
            }
        }
        // Memberships this role granted onward are removed.
        let members = membership_dependents_multi(eng, ctx, role);
        for m in members {
            let rn = role.clone();
            alter_role_swap(eng, ctx, &m, move |r| {
                r.memberships.retain(|mm| mm.grantor != rn);
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP OWNED".to_string(),
    })
}

/// v1.96: every role holding a membership edge granted by `grantor`
/// (any group), for DROP OWNED.
pub(crate) fn membership_dependents_multi(
    eng: &Engine,
    ctx: &StmtCtx,
    grantor: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for (rn, vs) in &eng.db.roles {
        if vs.iter().any(|r| {
            crate::storage::role_visible(r, ctx.snap, ctx.own)
                && r.memberships.iter().any(|m| m.grantor == grantor)
        }) {
            out.push(rn.clone());
        }
    }
    out
}

/// v1.96: display name of a privilege keyword (PG19 aclchk messages).
pub(crate) fn privilege_name(p: crate::sql::Privilege) -> &'static str {
    use crate::sql::Privilege::*;
    match p {
        Select => "SELECT",
        Insert => "INSERT",
        Update => "UPDATE",
        Delete => "DELETE",
        Truncate => "TRUNCATE",
        References => "REFERENCES",
        Trigger => "TRIGGER",
        Usage => "USAGE",
        Create => "CREATE",
        Connect => "CONNECT",
        Temporary => "TEMPORARY",
        Execute => "EXECUTE",
        Set => "SET",
        AlterSystem => "ALTER SYSTEM",
        Maintain => "MAINTAIN",
        All => "ALL",
    }
}

/// v1.96: ALTER SCHEMA ... OWNER TO — superuser or current owner.
/// v1.96: ALTER DATABASE name OWNER TO role — single-database
/// engine: validate the database name and the target role, require
/// superuser (the database owner is postgres here), accept.
pub(crate) fn exec_alter_database_owner(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    new_owner: &str,
) -> Result<ExecResult, ExecError> {
    if name != "postgres" && name != "rustgres" {
        return Err(exec_err(
            "3D000",
            format!("database \"{}\" does not exist", name),
        ));
    }
    if eng.db.find_role(new_owner, ctx.snap, ctx.own).is_none() {
        return Err(exec_err(
            "42704",
            format!("role \"{}\" does not exist", new_owner),
        ));
    }
    if !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Err(exec_err(
            "42501",
            format!("must be owner of database \"{}\"", name),
        ));
    }
    // v1.96: store the new owner (transactional via WriteOp::DbOwner)
    // — pg_database_owner membership follows it.
    if eng.db.db_owner != new_owner {
        let prev = eng.db.db_owner.clone();
        eng.db.db_owner = new_owner.to_string();
        ctx.writes.push(WriteOp::DbOwner { prev });
    }
    Ok(ExecResult::Command {
        tag: "ALTER DATABASE".to_string(),
    })
}

/// v1.96 finishing pass: ALTER SCHEMA name RENAME TO new_name
/// (PG19 RenameSchema): owner or superuser; the new name must be
/// free. Renames the catalog entry and rewrites schema-keyed
/// side state (owners, ACLs, object attribution, default ACLs).
pub(crate) fn exec_alter_schema_rename(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    new_name: &str,
) -> Result<ExecResult, ExecError> {
    if !eng.db.schemas.iter().any(|s| s == name) {
        return Err(exec_err(
            "3F000",
            format!("schema \"{}\" does not exist", name),
        ));
    }
    if eng.db.schemas.iter().any(|s| s == new_name) {
        return Err(exec_err(
            "42P06",
            format!("schema \"{}\" already exists", new_name),
        ));
    }
    let cur = eng
        .db
        .schema_owners
        .get(name)
        .cloned()
        .unwrap_or_else(|| "postgres".to_string());
    if cur != ctx.role && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Err(exec_err(
            "42501",
            format!("must be owner of schema \"{}\"", name),
        ));
    }
    let prev = eng.db.schemas.clone();
    let prev_owners = eng.db.schema_owners.clone();
    let prev_acls = eng.db.schema_acls.clone();
    let prev_obj = eng.db.obj_schema.clone();
    let prev_defacls = eng.db.default_acls.clone();
    for s in eng.db.schemas.iter_mut() {
        if s == name {
            *s = new_name.to_string();
        }
    }
    if let Some(o) = eng.db.schema_owners.remove(name) {
        eng.db.schema_owners.insert(new_name.to_string(), o);
    }
    if let Some(a) = eng.db.schema_acls.remove(name) {
        eng.db.schema_acls.insert(new_name.to_string(), a);
    }
    for v in eng.db.obj_schema.values_mut() {
        if v == name {
            *v = new_name.to_string();
        }
    }
    for d in eng.db.default_acls.iter_mut() {
        if d.schema == name {
            d.schema = new_name.to_string();
        }
    }
    ctx.writes.push(WriteOp::SchemaSet {
        prev,
        prev_owners,
        prev_acls,
    });
    ctx.writes.push(WriteOp::ObjSchemaSet { prev: prev_obj });
    ctx.writes.push(WriteOp::DefaultAcls { prev: prev_defacls });
    Ok(ExecResult::Command {
        tag: "ALTER SCHEMA".to_string(),
    })
}

pub(crate) fn exec_alter_schema_owner(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    new_owner: &str,
) -> Result<ExecResult, ExecError> {
    if !eng.db.schemas.iter().any(|s| s == name) {
        return Err(exec_err(
            "3F000",
            format!("schema \"{}\" does not exist", name),
        ));
    }
    if eng.db.find_role(new_owner, ctx.snap, ctx.own).is_none() {
        return Err(exec_err(
            "42704",
            format!("role \"{}\" does not exist", new_owner),
        ));
    }
    let cur = eng
        .db
        .schema_owners
        .get(name)
        .cloned()
        .unwrap_or_else(|| "postgres".to_string());
    if cur != ctx.role && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Err(exec_err(
            "42501",
            format!("must be owner of schema \"{}\"", name),
        ));
    }
    let prev = eng.db.schemas.clone();
    let prev_owners = eng.db.schema_owners.clone();
    eng.db
        .schema_owners
        .insert(name.to_string(), new_owner.to_string());
    ctx.writes.push(WriteOp::SchemaSet {
        prev,
        prev_owners,
        prev_acls: eng.db.schema_acls.clone(),
    });
    Ok(ExecResult::Command {
        tag: "ALTER SCHEMA".to_string(),
    })
}
