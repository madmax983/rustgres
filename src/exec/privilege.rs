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

// ---------------------------------------------------------------------------
// v1.96: LOCK TABLE, ALTER DEFAULT PRIVILEGES, default-ACL application.
// ---------------------------------------------------------------------------

/// LOCK [TABLE] name [, ...] IN mode MODE: existence + privilege gate,
/// otherwise a no-op (single-node engine; PG19 locktag semantics are
/// not observable beyond errors in the regression corpus).
pub(crate) fn exec_lock_table(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    mode: &str,
    _nowait: bool,
) -> Result<ExecResult, ExecError> {
    // PG19 lockcmds.c LockTableAclCheck: any of MAINTAIN / UPDATE /
    // DELETE / TRUNCATE permits every mode; SELECT adds ACCESS
    // SHARE; INSERT adds modes up to ROW EXCLUSIVE.
    let strength = match mode {
        "ACCESS SHARE" => 1,
        "ROW SHARE" => 2,
        "ROW EXCLUSIVE" => 3,
        "SHARE UPDATE EXCLUSIVE" => 4,
        "SHARE" => 5,
        "SHARE ROW EXCLUSIVE" => 6,
        "EXCLUSIVE" => 7,
        _ => 8, // ACCESS EXCLUSIVE
    };
    let mut mask = 512u32
        | crate::storage::PRIV_UPDATE
        | crate::storage::PRIV_DELETE
        | crate::storage::PRIV_TRUNCATE;
    if strength <= 1 {
        mask |= crate::storage::PRIV_SELECT;
    }
    if strength <= 3 {
        mask |= crate::storage::PRIV_INSERT;
    }
    for name in names {
        lock_acl_check(eng, ctx, name, mask, 0)?;
    }
    Ok(ExecResult::Command {
        tag: "LOCK TABLE".to_string(),
    })
}

/// v1.96 finishing pass: the LockTableAclCheck for one named
/// relation. Tables check their ACL; views check the VIEW's ACL
/// (underlying relations lock implicitly under the view owner's
/// rights) — except security_invoker views, whose underlying
/// relations must ALSO pass the check as the invoking role
/// (recursively).
fn lock_acl_check(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    mask: u32,
    depth: usize,
) -> Result<(), ExecError> {
    if depth > 16 {
        return Ok(());
    }
    let t = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session);
    if let Some(t) = t {
        let have = crate::storage::table_privs(&eng.db, ctx.role, t, ctx.snap, ctx.own);
        let owner_like = t.owner == ctx.role
            || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own);
        if !owner_like && have & mask == 0 {
            return Err(exec_err(
                "42501",
                format!("permission denied for table \"{}\"", name),
            ));
        }
        return Ok(());
    }
    if let Some(v) = eng.db.find_view(name, ctx.snap, ctx.own).cloned() {
        let owner_like = v.owner == ctx.role
            || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own);
        if !owner_like {
            let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
            let mut have = 0u32;
            for e in &v.acl {
                if e.role == "public" || closure.iter().any(|n| n == &e.role) {
                    have |= e.privs;
                }
            }
            if have & mask == 0 {
                return Err(exec_err(
                    "42501",
                    format!("permission denied for view \"{}\"", name),
                ));
            }
        }
        if v.security_invoker {
            for dep in &v.deps {
                lock_acl_check(eng, ctx, dep, mask, depth + 1)?;
            }
        }
        return Ok(());
    }
    if eng.db.find_sequence(name, ctx.snap, ctx.own).is_some() {
        return Ok(());
    }
    Err(exec_err(
        "42P01",
        format!("relation \"{}\" does not exist", name),
    ))
}

/// ALTER DEFAULT PRIVILEGES: store rules for tables/sequences; other
/// object kinds are validated and stored inertly (no ACL store exists
/// for functions/types/schemas in this engine).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_alter_default_privileges(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    target_role: Option<&str>,
    revoke: bool,
    privs: &[crate::sql::PrivSpec],
    kind: char,
    grantees: &[String],
    _grant_option_for: bool,
    schemas: &[String],
) -> Result<ExecResult, ExecError> {
    // v1.96: IN SCHEMA names must exist; LARGE OBJECTS reject
    // IN SCHEMA (PG19 0A000).
    if kind == 'l' && !schemas.is_empty() {
        return Err(exec_err(
            "0A000",
            "cannot use IN SCHEMA clause when using GRANT/REVOKE ON LARGE OBJECTS",
        ));
    }
    if kind == 'n' && !schemas.is_empty() {
        return Err(exec_err(
            "0A000",
            "cannot use IN SCHEMA clause when using GRANT/REVOKE ON SCHEMAS",
        ));
    }
    for s in schemas {
        if !eng.db.schemas.iter().any(|x| x == s) {
            return Err(exec_err(
                "3F000",
                format!("schema \"{}\" does not exist", s),
            ));
        }
    }
    let target = target_role.unwrap_or(ctx.role).to_string();
    if eng.db.find_role(&target, ctx.snap, ctx.own).is_none() {
        return Err(exec_err(
            "42704",
            format!("role \"{}\" does not exist", target),
        ));
    }
    if target != ctx.role
        && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
    {
        return Err(exec_err(
            "42501",
            "permission denied: must be superuser or the target role to alter default privileges"
                .to_string(),
        ));
    }
    let resolved: Vec<String> = grantees
        .iter()
        .map(|g| resolve_grantee_name(ctx, g))
        .collect();
    for g in &resolved {
        if g != "public" && eng.db.find_role(g, ctx.snap, ctx.own).is_none() {
            return Err(exec_err("42704", format!("role \"{}\" does not exist", g)));
        }
    }
    let mut bits = 0u32;
    for s in privs {
        if !s.columns.is_empty() {
            return Err(exec_err(
                "0A000",
                "column lists are not allowed in ALTER DEFAULT PRIVILEGES".to_string(),
            ));
        }
        bits |= match (s.priv_, kind) {
            (crate::sql::Privilege::All, 'r') => crate::storage::PRIV_ALL_TABLE,
            (crate::sql::Privilege::All, 'n') => {
                crate::storage::PRIV_USAGE | crate::storage::PRIV_REFERENCES
            }
            (crate::sql::Privilege::All, 'S') => {
                crate::storage::PRIV_USAGE
                    | crate::storage::PRIV_SELECT
                    | crate::storage::PRIV_UPDATE
            }
            (crate::sql::Privilege::All, _) => s.priv_.bits(),
            _ => s.priv_.bits(),
        };
    }
    let prev = eng.db.default_acls.clone();
    // v1.96: one rule per IN SCHEMA target ("" = global).
    let schema_keys: Vec<String> = if schemas.is_empty() {
        vec![String::new()]
    } else {
        schemas.to_vec()
    };
    for g in &resolved {
        for sk in &schema_keys {
            if let Some(e) = eng.db.default_acls.iter_mut().find(|d| {
                d.creator == target && d.kind == kind && &d.grantee == g && &d.schema == sk
            }) {
                if revoke {
                    e.privs &= !bits;
                } else {
                    e.privs |= bits;
                }
            } else if !revoke {
                eng.db.default_acls.push(crate::storage::DefaultAcl {
                    creator: target.clone(),
                    kind,
                    grantee: g.clone(),
                    privs: bits,
                    schema: sk.clone(),
                });
            } else if revoke && *g == target {
                // v1.99: REVOKE FROM the creator with no prior rule:
                // PG19 computes the new pg_default_acl entry from
                // acldefault, where the creator's own item holds ALL
                // rights for the kind — so the revoke must record
                // (all & !bits) or it is silently lost and every
                // later CREATE resurrects the revoked privilege.
                // (Exposed by select_into: ALTER DEFAULT PRIVILEGES
                // FOR ROLE u REVOKE INSERT ON TABLES FROM u, then a
                // CTAS by u must deny u INSERT.) Kinds 'r'/'S' have
                // unambiguous acldefault bit sets here; other kinds
                // keep the historical drop (no in-scope consumer).
                let all = match kind {
                    'r' => crate::storage::PRIV_ALL_TABLE | 512,
                    'S' => {
                        crate::storage::PRIV_USAGE
                            | crate::storage::PRIV_SELECT
                            | crate::storage::PRIV_UPDATE
                    }
                    _ => 0,
                };
                if all != 0 {
                    eng.db.default_acls.push(crate::storage::DefaultAcl {
                        creator: target.clone(),
                        kind,
                        grantee: g.clone(),
                        privs: all & !bits,
                        schema: sk.clone(),
                    });
                }
            } else if g == "public" {
                // v1.96: REVOKE FROM public with no prior rule still
                // records a rule: PG computes the new default ACL
                // from acldefault (PUBLIC holds EXECUTE on routines
                // and USAGE on types), so the revoke must persist or
                // later CREATEs would resurrect the default.
                let dflt = match kind {
                    'f' | 'T' => crate::storage::PRIV_USAGE,
                    _ => 0,
                };
                eng.db.default_acls.push(crate::storage::DefaultAcl {
                    creator: target.clone(),
                    kind,
                    grantee: g.clone(),
                    privs: dflt & !bits,
                    schema: sk.clone(),
                });
            }
        }
    }
    // Zero rules survive only where they encode a revoked PUBLIC
    // default (see above); other zero rules are dropped.
    eng.db
        .default_acls
        .retain(|d| d.privs != 0 || (d.grantee == "public" && matches!(d.kind, 'f' | 'T')));
    ctx.writes.push(WriteOp::DefaultAcls { prev });
    Ok(ExecResult::Command {
        tag: "ALTER DEFAULT PRIVILEGES".to_string(),
    })
}

/// v1.96: PG19 parse-time USAGE check for a user type or domain
/// (aclchk.c: every type named by a DDL statement — column types,
/// domain bases, function arg/return types, cast targets — needs
/// USAGE for the current role; PUBLIC holds it by default). Names
/// not present in the type catalog (builtins, or genuinely unknown
/// names that error elsewhere) pass through.
pub(crate) fn check_type_usage(eng: &Engine, ctx: &StmtCtx, name: &str) -> Result<(), ExecError> {
    let mut base = name
        .rsplit('.')
        .next()
        .unwrap_or(name)
        .trim()
        .to_ascii_lowercase();
    if let Some(p) = base.find('(') {
        base.truncate(p);
    }
    while let Some(s) = base.strip_suffix("[]") {
        base = s.to_string();
    }
    let base = base.trim().to_string();
    if !eng.db.types.contains_key(&base) {
        return Ok(());
    }
    if crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Ok(());
    }
    if eng.db.type_owners.get(&base).map(|o| o.as_str()) == Some(ctx.role) {
        return Ok(());
    }
    let privs = type_privs_for(&eng.db, ctx.role, &base, ctx.snap, ctx.own);
    if privs & crate::storage::PRIV_USAGE != crate::storage::PRIV_USAGE {
        return Err(exec_err(
            "42501",
            format!("permission denied for type {}", base),
        ));
    }
    Ok(())
}

/// v1.96: check USAGE on every named cast target in an expression.
pub(crate) fn check_expr_cast_usages(
    eng: &Engine,
    ctx: &StmtCtx,
    e: &crate::sql::Expr,
) -> Result<(), ExecError> {
    let mut names = Vec::new();
    crate::sql::collect_cast_type_names(e, &mut names);
    for n in &names {
        check_type_usage(eng, ctx, n)?;
    }
    Ok(())
}

/// v1.96: check USAGE on every named cast target in a SELECT
/// (items, WHERE, GROUP BY, HAVING, ORDER BY, DISTINCT ON).
pub(crate) fn check_select_cast_usages(
    eng: &Engine,
    ctx: &StmtCtx,
    stmt: &crate::sql::SelectStmt,
) -> Result<(), ExecError> {
    use crate::sql::SelectItem;
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            check_expr_cast_usages(eng, ctx, expr)?;
        }
    }
    if let Some(w) = &stmt.where_ {
        check_expr_cast_usages(eng, ctx, w)?;
    }
    for g in stmt.group_by.iter().flatten() {
        check_expr_cast_usages(eng, ctx, g)?;
    }
    if let Some(h) = &stmt.having {
        check_expr_cast_usages(eng, ctx, h)?;
    }
    for o in &stmt.order_by {
        check_expr_cast_usages(eng, ctx, &o.expr)?;
    }
    for d in &stmt.distinct_on {
        check_expr_cast_usages(eng, ctx, d)?;
    }
    Ok(())
}

/// v1.96: PG19 DML read checks (ExecCheckRTEPerms selectedCols for
/// the target relation): columns read by UPDATE SET expressions,
/// UPDATE/DELETE WHERE clauses, and ON CONFLICT DO UPDATE/DO SELECT
/// expressions need SELECT privilege (column-level or table-level)
/// on the target table. `EXCLUDED.col` references read the proposed
/// row's column, so they check `col` on the target too. Arbiter
/// columns are exempt (PG treats the conflict target as readable
/// for the operation — the suite's `RETURNING atest5.two` on the
/// arbiter succeeds without SELECT on it). Failures use PG's
/// table-level message.
pub(crate) fn check_dml_read_cols(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    alias: Option<&str>,
    exprs: &[&crate::sql::Expr],
    exempt: &[String],
) -> Result<(), ExecError> {
    if crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own) {
        return Ok(());
    }
    let t = match eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        Some(t) => t,
        None => return Ok(()),
    };
    if t.owner == ctx.role {
        return Ok(());
    }
    let base = table.rsplit('.').next().unwrap_or(table);
    let mut refs: Vec<(Option<String>, String)> = Vec::new();
    for e in exprs {
        crate::sql::collect_col_refs(e, &mut refs);
    }
    let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
    for (qual, col) in &refs {
        if col == "*" || exempt.iter().any(|c| c == col) {
            continue;
        }
        let mine = match qual.as_deref() {
            None => true,
            Some(qn) => qn == base || Some(qn) == alias || qn.eq_ignore_ascii_case("excluded"),
        };
        if !mine {
            continue;
        }
        // Only real columns of the target are checked (a name that
        // is not a target column resolves elsewhere or errors).
        if !t.columns.iter().any(|(n, _)| n == col) {
            continue;
        }
        let have =
            crate::storage::column_privs_in(&eng.db, ctx.role, t, col, &closure, ctx.snap, ctx.own);
        if have & crate::storage::PRIV_SELECT != crate::storage::PRIV_SELECT {
            return Err(exec_err(
                "42501",
                format!("permission denied for table \"{}\"", base),
            ));
        }
    }
    Ok(())
}

/// Merge the default-privilege ACL entries for `creator`/`kind` into a
/// fresh object's ACL (CREATE TABLE / CREATE SEQUENCE hook).
/// v1.96: schema-aware variant — global rules ("") always apply;
/// IN SCHEMA rules apply to objects in that schema (the qualifier of
/// `obj_name`, defaulting to "public" when unqualified).
pub(crate) fn default_acl_entries_for(
    db: &crate::storage::Database,
    creator: &str,
    kind: char,
    obj_name: &str,
) -> Vec<crate::storage::AclEntry> {
    let obj_schema = if obj_name.contains('.') {
        obj_name.split('.').next().unwrap_or("public")
    } else {
        "public"
    };
    // Schema-specific rows shadow global rows per grantee (PG19).
    let mut out: Vec<crate::storage::AclEntry> = db
        .default_acls
        .iter()
        .filter(|d| d.creator == creator && d.kind == kind && d.schema == obj_schema)
        .map(|d| crate::storage::AclEntry::new(&d.grantee, d.privs))
        .collect();
    let covered: std::collections::HashSet<String> = out.iter().map(|e| e.role.clone()).collect();
    out.extend(
        db.default_acls
            .iter()
            .filter(|d| {
                d.creator == creator
                    && d.kind == kind
                    && d.schema.is_empty()
                    && !covered.contains(&d.grantee)
            })
            .map(|d| crate::storage::AclEntry::new(&d.grantee, d.privs)),
    );
    out
}

/// v1.96: the initial ACL for a routine/type when ALTER DEFAULT
/// PRIVILEGES rules exist for (creator, kind): PG builds the new
/// object's ACL from acldefault with the rules applied, so a rule
/// for PUBLIC *replaces* the built-in PUBLIC default (including a
/// revoke down to zero). Returns None when no rule matches (the
/// caller then leaves the ACL unset = pure acldefault semantics).
/// `default_public` is acldefault's PUBLIC bit for the family;
/// `mask` clips rule bits to the family.
pub(crate) fn adp_initial_acl(
    db: &crate::storage::Database,
    creator: &str,
    kind: char,
    obj_name: &str,
    default_public: u32,
    mask: u32,
) -> Option<Vec<crate::storage::AclEntry>> {
    let obj_schema = if obj_name.contains('.') {
        obj_name.split('.').next().unwrap_or("public")
    } else {
        "public"
    };
    // PG19: a schema-specific pg_default_acl row shadows the global
    // row per grantee (AlterDefaultPrivileges lookup checks the
    // schema row first, then the global one).
    let mut rules: Vec<&crate::storage::DefaultAcl> = db
        .default_acls
        .iter()
        .filter(|d| d.creator == creator && d.kind == kind && d.schema == obj_schema)
        .collect();
    let schema_grantees: std::collections::HashSet<&str> =
        rules.iter().map(|d| d.grantee.as_str()).collect();
    rules.extend(db.default_acls.iter().filter(|d| {
        d.creator == creator
            && d.kind == kind
            && d.schema.is_empty()
            && !schema_grantees.contains(d.grantee.as_str())
    }));
    if rules.is_empty() {
        return None;
    }
    let mut out: Vec<crate::storage::AclEntry> = rules
        .iter()
        .map(|d| {
            let mut e = crate::storage::AclEntry::new(&d.grantee, d.privs & mask);
            e.grantor = creator.to_string();
            e
        })
        .collect();
    if default_public != 0 && !rules.iter().any(|d| d.grantee == "public") {
        out.push(crate::storage::AclEntry::new(
            "public",
            default_public & mask,
        ));
    }
    Some(out)
}
