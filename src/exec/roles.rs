// v1.78 mechanical split: moved verbatim from src/exec.rs (63457-64109).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

/// Dispatch for the nextval/currval/setval SQL functions (called from
/// eval_func with pre-evaluated args).

// ============================================================================
// v0.11: roles and privileges
// ============================================================================

pub(crate) fn exec_create_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    login: bool,
    superuser: bool,
    password: &Option<String>,
    connlimit: Option<i32>,
    valid_until: &Option<String>,
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
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
    Ok(ExecResult::Command {
        tag: "CREATE ROLE".to_string(),
    })
}

pub(crate) fn exec_alter_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    login: Option<bool>,
    superuser: Option<bool>,
    password: &Option<Option<String>>,
    connlimit: Option<i32>,
    valid_until: &Option<String>,
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
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

/// GRANT role [, ...] TO role [, ...]: role membership. Members inherit
/// the group's GRANTed privileges. Superuser-only (PostgreSQL also
/// allows ADMIN OPTION holders; rustgres has no CREATEROLE yet).
pub(crate) fn exec_grant_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    roles: &[String],
    grantees: &[String],
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    for grantee in grantees {
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
            let already = eng
                .db
                .find_role(grantee, ctx.snap, ctx.own)
                .map(|r| r.memberships.iter().any(|m| &m.role == role))
                .unwrap_or(false);
            if already {
                continue;
            }
            let grantor = ctx.role.to_string();
            let role_name = role.clone();
            alter_role_swap(eng, ctx, grantee, move |r| {
                r.memberships.push(crate::storage::RoleMembership {
                    role: role_name.clone(),
                    grantor: grantor.clone(),
                });
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "GRANT".to_string(),
    })
}

/// REVOKE role [, ...] FROM role [, ...]: remove membership.
pub(crate) fn exec_revoke_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    roles: &[String],
    grantees: &[String],
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
    for grantee in grantees {
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
            let has = eng
                .db
                .find_role(grantee, ctx.snap, ctx.own)
                .map(|r| r.memberships.iter().any(|m| &m.role == role))
                .unwrap_or(false);
            if !has {
                continue;
            }
            let role_name = role.clone();
            alter_role_swap(eng, ctx, grantee, move |r| {
                r.memberships.retain(|m| m.role != role_name);
            });
        }
    }
    Ok(ExecResult::Command {
        tag: "REVOKE".to_string(),
    })
}

pub(crate) fn exec_drop_role(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    require_superuser(eng, ctx)?;
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
        if !owned.is_empty() {
            return Err(exec_err(
                "2BP01",
                format!(
                    "role \"{}\" cannot be dropped because it owns {}",
                    role.name,
                    owned.join(", ")
                ),
            ));
        }
        // Grants *to* the dropped role linger but are inert (documented
        // deviation from PostgreSQL, which drops them).
        let cur = eng
            .db
            .find_role_mut(name, ctx.snap, ctx.own)
            .expect("visible above");
        cur.dropped_xmax = ctx.own;
        ctx.writes.push(WriteOp::DropRole {
            name: name.to_string(),
            prev: role,
        });
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

/// Apply a GRANT or REVOKE (revoke = true). Only superusers and object
/// owners may grant; the grantee roles must exist.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_grant_revoke(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    privs: &[crate::sql::PrivSpec],
    object: &crate::sql::GrantObject,
    grantees: &[String],
    revoke: bool,
) -> Result<ExecResult, ExecError> {
    use crate::sql::{GrantObject, PrivSpec, Privilege};
    // All grantees must be existing roles.
    for g in grantees {
        if eng.db.find_role(g, ctx.snap, ctx.own).is_none() {
            return Err(exec_err("42704", format!("role \"{}\" does not exist", g)));
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
            (Privilege::All, GrantObject::Sequence(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::All, GrantObject::Database) => bits |= crate::storage::PRIV_CONNECT,
            (Privilege::Usage, GrantObject::Sequence(_)) => bits |= crate::storage::PRIV_USAGE,
            (Privilege::Connect, GrantObject::Database) => bits |= crate::storage::PRIV_CONNECT,
            (
                Privilege::Select
                | Privilege::Insert
                | Privilege::Update
                | Privilege::Delete
                | Privilege::Truncate
                | Privilege::References
                | Privilege::Trigger,
                GrantObject::Table(_),
            ) => bits |= p.bits(),
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
        GrantObject::Table(name) => {
            let t = eng
                .db
                .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                .ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", name))
                })?;
            if t.owner != ctx.role
                && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            {
                return Err(exec_err(
                    "42501",
                    format!(
                        "permission denied: must be owner of table \"{}\" to {}",
                        name,
                        verb.to_lowercase()
                    ),
                ));
            }
            let mut next = t.clone();
            apply_acl_delta(&mut next.acl, grantees, bits, revoke, false);
            // v0.11: column-level grants.
            if !col_specs.is_empty() {
                let cols: Vec<String> = next.columns.iter().map(|(n, _)| n.clone()).collect();
                for s in &col_specs {
                    match (s.priv_, object) {
                        (
                            Privilege::Select
                            | Privilege::Insert
                            | Privilege::Update
                            | Privilege::References,
                            GrantObject::Table(_),
                        ) => {}
                        _ => {
                            return Err(exec_err(
                                "0A000",
                                format!("invalid privilege {:?} for {:?}", s.priv_, object),
                            ));
                        }
                    }
                    for c in &s.columns {
                        if !cols.iter().any(|n| n == c) {
                            return Err(exec_err(
                                "42703",
                                format!("column \"{}\" of relation \"{}\" does not exist", c, name),
                            ));
                        }
                    }
                    apply_col_acl_delta(
                        &mut next.col_acl,
                        grantees,
                        s.priv_.bits(),
                        &s.columns,
                        revoke,
                    );
                }
            }
            alter_swap(eng, ctx, name, None, next, None)?;
        }
        GrantObject::Sequence(name) => {
            let s = eng
                .db
                .find_sequence(name, ctx.snap, ctx.own)
                .ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", name))
                })?;
            if s.owner != ctx.role
                && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            {
                return Err(exec_err(
                    "42501",
                    format!(
                        "permission denied: must be owner of sequence \"{}\" to {}",
                        name,
                        verb.to_lowercase()
                    ),
                ));
            }
            // Version-swap like ALTER SEQUENCE so the grant is transactional.
            let prev = s.clone();
            let versions = eng.db.sequences.get_mut(name).expect("visible above");
            let cur = versions
                .iter_mut()
                .find(|s| crate::storage::seq_visible(s, ctx.snap, ctx.own))
                .expect("visible above");
            cur.dropped_xmax = ctx.own;
            let mut next = prev.clone();
            apply_acl_delta(&mut next.acl, grantees, bits, revoke, false);
            next.created_xmin = ctx.own;
            next.dropped_xmax = 0;
            versions.push(next);
            ctx.writes.push(WriteOp::AlterSequence {
                name: name.to_string(),
                prev,
            });
        }
        GrantObject::Database => {
            require_superuser(eng, ctx)?;
            let prev = eng.db.db_acl.clone();
            apply_acl_delta(&mut eng.db.db_acl, grantees, bits, revoke, true);
            ctx.writes.push(WriteOp::DbAcl { prev });
        }
    }
    Ok(ExecResult::Command {
        tag: verb.to_string(),
    })
}

/// Merge grant/revoke bits into an ACL vector.
///
/// When `keep_zero` is false, entries left with no privileges are pruned
/// (fine for tables/sequences, whose default is deny). When true, zero-priv
/// entries are kept as explicit deny markers (needed for the database ACL,
/// whose default is allow: `db_connect_allowed` treats an entry without the
/// CONNECT bit as a revoked role).
pub(crate) fn apply_acl_delta(
    acl: &mut Vec<crate::storage::AclEntry>,
    grantees: &[String],
    bits: u32,
    revoke: bool,
    keep_zero: bool,
) {
    for g in grantees {
        if let Some(e) = acl.iter_mut().find(|e| e.role == *g) {
            if revoke {
                e.privs &= !bits;
            } else {
                e.privs |= bits;
            }
        } else {
            // No entry yet. A grant creates one; a revoke creates an
            // explicit deny marker only when keep_zero (database ACL).
            // Otherwise revoking a never-granted privilege is a no-op,
            // like PostgreSQL's warning (we stay silent).
            if !revoke || keep_zero {
                acl.push(crate::storage::AclEntry {
                    role: g.clone(),
                    privs: if revoke { 0 } else { bits },
                });
            }
        }
    }
    if !keep_zero {
        acl.retain(|e| e.privs != 0);
    }
}

/// Merge column grant/revoke bits into a table's `col_acl` vector.
/// Entries are keyed by (role, column-set): a grant unions the column
/// list, a revoke removes the columns (dropping the entry when its
/// column list empties out).
pub(crate) fn apply_col_acl_delta(
    col_acl: &mut Vec<crate::storage::ColAclEntry>,
    grantees: &[String],
    bits: u32,
    columns: &[String],
    revoke: bool,
) {
    for g in grantees {
        if revoke {
            // Remove the columns from every entry for this role whose
            // privs overlap; drop emptied entries.
            for e in col_acl.iter_mut().filter(|e| e.role == *g) {
                if e.privs & bits != 0 {
                    e.columns.retain(|c| !columns.contains(c));
                    if e.columns.is_empty() {
                        e.privs = 0;
                    } else {
                        // Partial column revoke: keep the entry but drop
                        // the revoked bits only when no columns remain
                        // for them. Simpler and safe: clear the bits —
                        // remaining columns keep other granted bits.
                        // (We track one privs mask per entry; revoking
                        // one privilege's columns while another's remain
                        // is approximated by keeping the entry.)
                    }
                }
            }
            col_acl.retain(|e| e.privs != 0 && !e.columns.is_empty());
        } else if let Some(e) = col_acl.iter_mut().find(|e| e.role == *g && e.privs == bits) {
            for c in columns {
                if !e.columns.contains(c) {
                    e.columns.push(c.clone());
                }
            }
        } else {
            col_acl.push(crate::storage::ColAclEntry {
                role: g.clone(),
                privs: bits,
                columns: columns.to_vec(),
            });
        }
    }
}

/// ALTER TABLE name OWNER TO new_owner.
pub(crate) fn alter_owner_to(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    new_owner: &str,
) -> Result<ExecResult, ExecError> {
    require_table_owner(eng, ctx, name)?;
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
    next.owner = new_owner.to_string();
    alter_swap(eng, ctx, name, None, next, None)?;
    Ok(ExecResult::Command {
        tag: "ALTER TABLE".to_string(),
    })
}
