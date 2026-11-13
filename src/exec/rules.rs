//! v2.19: the CREATE RULE system (PG19 rewrite rules).
//!
//! Rules are stored in `Database::rules` keyed by event-relation
//! base name (PG19 pg_rewrite). DDL lives here; firing lives here
//! too: `exec_insert` / `exec_update` / `exec_delete` (dml.rs)
//! consult `try_fire_insert` / `try_fire_update` / `try_fire_delete`
//! before any other target handling, and fall through to their
//! `*_inner` bodies when no rule involvement applies.
//!
//! Semantics grounded in the local PG19 source
//! (backend/rewrite/rewriteHandler.c fireRules / RewriteQuery /
//! rewriteTargetView, backend/rewrite/rewriteDefine.c DefineRule):
//! rules fire in rulename order; a rule's qualification ANDs into
//! its actions; an INSTEAD rule suppresses the original action for
//! the rows its qualification matches (on a table the original
//! still runs for the remaining rows; on a view the original cannot
//! run at all, so a view needs an unconditional INSTEAD rule — or
//! INSTEAD OF triggers — per event). PG rewrites queries set-wise;
//! this engine fires per row, which is equivalent for the action
//! shapes the regression suites exercise (see the v2.19 scope doc
//! for the documented divergence boundary).
use super::*;

use crate::sql::{RuleAction, RuleDef, rule_event};

/// The relation key rules are stored under: the unqualified base
/// name (single-namespace engine precedent).
pub(crate) fn rule_rel_key(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_string()
}

/// The registry key for a relation in a statement context.
/// Rules on a TEMP table are scoped to the owning session
/// ("pg_temp:<sid>:<name>"): they can neither fire for nor be
/// seen by another session's same-named relation, and they die
/// with the session (Database::drop_session_temps purges them) —
/// the same same-name hazard the storage layer documents for
/// temp indexes.
pub(crate) fn rule_key_for(eng: &Engine, ctx: &StmtCtx, rel: &str) -> String {
    let base = rule_rel_key(rel);
    if eng.db.is_temp_table(ctx.session, &base) {
        format!("pg_temp:{}:{}", ctx.session, base)
    } else {
        base
    }
}

/// The display (base) name for a registry key.
pub(crate) fn rule_key_display(key: &str) -> &str {
    match key.strip_prefix("pg_temp:") {
        Some(rest) => rest.split(':').nth(1).unwrap_or(rest),
        None => key,
    }
}

/// Whether a registry key is visible to `session` (temp keys
/// only to their owning session).
fn rule_key_visible(key: &str, session: u64) -> bool {
    match key.strip_prefix("pg_temp:") {
        Some(rest) => rest.split(':').next().and_then(|s| s.parse::<u64>().ok()) == Some(session),
        None => true,
    }
}

/// All rules (any event, any enable state) under a registry key.
pub(crate) fn rules_on(eng: &Engine, key: &str) -> Vec<RuleDef> {
    eng.db.rules.get(key).cloned().unwrap_or_default()
}

/// Rules that fire for (relation, event) in this session, in
/// rulename order (PG19 scans pg_rewrite in name order).
pub(crate) fn firing_rules(eng: &Engine, rel: &str, event: u8) -> Vec<RuleDef> {
    let replica = session_is_replica();
    let mut out: Vec<RuleDef> = rules_on(eng, rel)
        .into_iter()
        .filter(|r| r.event == event && r.fires(replica))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Any enabled non-SELECT rule on the relation (MERGE gate, PG19
/// rewriteHandler.c: the check runs over the relation's rule locks
/// after the session-replication-role filter).
pub(crate) fn has_enabled_nonselect_rule(eng: &Engine, ctx: &StmtCtx, rel: &str) -> bool {
    let replica = session_is_replica();
    rules_on(eng, &rule_key_for(eng, ctx, rel))
        .iter()
        .any(|r| r.event != rule_event::SELECT && r.fires(replica))
}

/// Replace the rule list under a registry key (undo-logged).
fn set_rules(ctx: &mut StmtCtx, eng: &mut Engine, key: &str, next: Vec<RuleDef>) {
    let prev = eng.db.rules.get(key).cloned();
    ctx.writes.push(WriteOp::RulesSet {
        rel: key.to_string(),
        prev,
    });
    if next.is_empty() {
        eng.db.rules.remove(key);
    } else {
        eng.db.rules.insert(key.to_string(), next);
    }
}

/// Drop all rules of a relation (DROP TABLE / DROP VIEW cleanup;
/// undo-logged like any rule mutation).
pub(crate) fn clear_rules(ctx: &mut StmtCtx, eng: &mut Engine, rel: &str) {
    let key = rule_key_for(eng, ctx, rel);
    if eng.db.rules.contains_key(&key) {
        set_rules(ctx, eng, &key, Vec::new());
    }
}

/// Columns of a table or view in declaration order.
pub(crate) fn rel_columns(
    eng: &Engine,
    ctx: &StmtCtx,
    rel: &str,
) -> Result<Vec<(String, ColType)>, ExecError> {
    if let Some(t) = eng.db.find_table(rel, ctx.snap, &ctx.all_xids, ctx.session) {
        return Ok(t.columns.clone());
    }
    if eng.db.find_view(rel, ctx.snap, ctx.own).is_some() {
        // view_columns_of needs &Engine/&StmtCtx only for describe.
        return view_columns_of(eng, ctx, rel);
    }
    Err(exec_err(
        "42P01",
        format!("relation \"{}\" does not exist", rule_rel_key(rel)),
    ))
}

pub(crate) fn rel_is_view(eng: &Engine, ctx: &StmtCtx, rel: &str) -> bool {
    eng.db
        .find_table(rel, ctx.snap, &ctx.all_xids, ctx.session)
        .is_none()
        && eng.db.find_view(rel, ctx.snap, ctx.own).is_some()
}

fn rel_owner(eng: &Engine, ctx: &StmtCtx, rel: &str) -> Option<String> {
    if let Some(t) = eng.db.find_table(rel, ctx.snap, &ctx.all_xids, ctx.session) {
        return Some(t.owner.clone());
    }
    eng.db
        .find_view(rel, ctx.snap, ctx.own)
        .map(|v| v.owner.clone())
}

/// Does this expression reference NEW./OLD. (or bare new/old
/// whole-row forms)? Used by CREATE RULE validation.
fn expr_refs_pseudo(e: &Expr, pseudo: &str) -> bool {
    let mut hit = false;
    walk_expr(e, &mut |x| match x {
        Expr::Column { table: Some(t), .. } if t.eq_ignore_ascii_case(pseudo) => hit = true,
        Expr::WholeRow { qual } if qual.eq_ignore_ascii_case(pseudo) => hit = true,
        _ => {}
    });
    hit
}

fn stmt_exprs(s: &Stmt, out: &mut Vec<Expr>) {
    // Collect the top-level expressions of an action statement for
    // NEW/OLD validation: SET/WHERE/RETURNING/VALUES positions.
    match s {
        Stmt::Insert {
            rows,
            returning,
            select,
            ..
        } => {
            for row in rows {
                for v in row {
                    if let InsertValue::Expr(e) = v {
                        out.push(e.clone());
                    }
                }
            }
            if let Some(sel) = select {
                collect_select_exprs(sel, out);
            }
            for item in returning {
                if let SelectItem::Expr { expr, .. } = item {
                    out.push(expr.clone());
                }
            }
        }
        Stmt::Update {
            sets,
            where_,
            returning,
            ..
        } => {
            for (_, e) in sets {
                out.push(e.clone());
            }
            if let Some(w) = where_ {
                out.push(w.clone());
            }
            for item in returning {
                if let SelectItem::Expr { expr, .. } = item {
                    out.push(expr.clone());
                }
            }
        }
        Stmt::Delete {
            where_, returning, ..
        } => {
            if let Some(w) = where_ {
                out.push(w.clone());
            }
            for item in returning {
                if let SelectItem::Expr { expr, .. } = item {
                    out.push(expr.clone());
                }
            }
        }
        Stmt::Select(sel) => collect_select_exprs(sel, out),
        _ => {}
    }
}

/// Does a CTE definition (or a select's item list within it)
/// reference OLD./NEW. — including the `OLD.*` / `NEW.*` star
/// forms, which are select items, not expressions?
fn cte_body_refs_pseudo(s: &Stmt, pseudo: &str) -> bool {
    fn select_refs(sel: &SelectStmt, pseudo: &str) -> bool {
        for item in &sel.items {
            if let SelectItem::AllOf(q) = item {
                if q.eq_ignore_ascii_case(pseudo) {
                    return true;
                }
            }
        }
        let mut exprs = Vec::new();
        collect_select_exprs(sel, &mut exprs);
        exprs.iter().any(|e| expr_refs_pseudo(e, pseudo))
    }
    let withs: &[CteDef] = match s {
        Stmt::Insert { with, .. } | Stmt::Update { with, .. } | Stmt::Delete { with, .. } => with,
        Stmt::Select(sel) => &sel.with,
        _ => &[],
    };
    for cte in withs {
        let hit = match &cte.body {
            CteBody::Simple(s) => select_refs(s, pseudo),
            CteBody::Union { left, right, .. } => {
                select_refs(left, pseudo) || select_refs(right, pseudo)
            }
            CteBody::Dml(s) => {
                let mut exprs = Vec::new();
                stmt_exprs(s, &mut exprs);
                exprs.iter().any(|e| expr_refs_pseudo(e, pseudo))
            }
        };
        if hit {
            return true;
        }
    }
    false
}

fn collect_select_exprs(sel: &SelectStmt, out: &mut Vec<Expr>) {
    walk_select(sel, &mut |e| out.push(e.clone()));
    if let Some(op) = &sel.set_op {
        collect_select_exprs(&op.left, out);
        for b in &op.chain {
            collect_select_exprs(&b.right, out);
        }
    }
    for cte in &sel.with {
        match &cte.body {
            CteBody::Simple(s) => collect_select_exprs(s, out),
            CteBody::Union { left, right, .. } => {
                collect_select_exprs(left, out);
                collect_select_exprs(right, out);
            }
            CteBody::Dml(s) => stmt_exprs(s, out),
        }
    }
}

/// CREATE [OR REPLACE] RULE (PG19 DefineRule).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_rule(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    or_replace: bool,
    relation: &str,
    event: u8,
    condition: &Option<Expr>,
    condition_text: &Option<String>,
    instead: bool,
    nothing: bool,
    actions: &[RuleAction],
) -> Result<ExecResult, ExecError> {
    // The event relation must exist (as a table or a view).
    let is_view = rel_is_view(eng, ctx, relation);
    let is_table = eng
        .db
        .find_table(relation, ctx.snap, &ctx.all_xids, ctx.session)
        .is_some();
    if !is_table && !is_view {
        return Err(exec_err(
            "42P01",
            format!("relation \"{}\" does not exist", rule_rel_key(relation)),
        ));
    }
    // PG19: only the relation owner may define rules on it
    // (object_ownercheck; membership of the owning role counts).
    if let Some(owner) = rel_owner(eng, ctx, relation) {
        if owner != ctx.role
            && !ctx.privs_suppressed
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            && !has_privs_of_role(eng, ctx, ctx.role, &owner)
        {
            return Err(exec_err(
                "42501",
                format!(
                    "must be owner of {} {}",
                    if is_view { "view" } else { "table" },
                    rule_rel_key(relation)
                ),
            ));
        }
    }
    // PG19 rewriteDefine.c: ON SELECT rules are view definitions.
    if event == rule_event::SELECT {
        if !is_view {
            return Err(exec_err(
                "42809",
                format!(
                    "relation \"{}\" cannot have ON SELECT rules",
                    rule_rel_key(relation)
                ),
            ));
        }
        // A view already carries its _RETURN rule; this engine does
        // not back views with the rule registry, so any ON SELECT
        // rule on a view meets PG's "already a view" refusal.
        return Err(exec_err(
            "55000",
            format!("\"{}\" is already a view", rule_rel_key(relation)),
        ));
    }
    // Action statement kinds: PG19 gram.y restricts rule actions to
    // INSERT / UPDATE / DELETE / SELECT / NOTIFY (VALUES rides as a
    // Select carrier here).
    for a in actions {
        match &a.stmt {
            Stmt::Insert { .. }
            | Stmt::Update { .. }
            | Stmt::Delete { .. }
            | Stmt::Select(_)
            | Stmt::Notify { .. } => {}
            _ => {
                return Err(exec_err(
                    "42601",
                    "syntax error: rule action must be INSERT, UPDATE, DELETE, SELECT, or NOTIFY"
                        .to_string(),
                ));
            }
        }
    }
    let cols = rel_columns(eng, ctx, relation)?;
    // NEW/OLD availability per event (PG19 parse analysis: the
    // unavailable pseudotable simply does not resolve).
    let mut exprs: Vec<Expr> = Vec::new();
    if let Some(c) = condition {
        exprs.push(c.clone());
    }
    for a in actions {
        stmt_exprs(&a.stmt, &mut exprs);
    }
    if event == rule_event::INSERT {
        for e in &exprs {
            if expr_refs_pseudo(e, "old") {
                return Err(exec_err(
                    "42P01",
                    "relation \"old\" does not exist".to_string(),
                ));
            }
        }
    }
    if event == rule_event::DELETE {
        for e in &exprs {
            if expr_refs_pseudo(e, "new") {
                return Err(exec_err(
                    "42P01",
                    "relation \"new\" does not exist".to_string(),
                ));
            }
        }
    }
    // Actions may not target OLD or NEW (rewriteDefine.c).
    for a in actions {
        let tgt = match &a.stmt {
            Stmt::Insert { table, .. }
            | Stmt::Update { table, .. }
            | Stmt::Delete { table, .. } => Some(rule_rel_key(table)),
            _ => None,
        };
        if let Some(t) = tgt {
            if t == "old" {
                return Err(exec_err(
                    "0A000",
                    "rule actions on OLD are not implemented".to_string(),
                ));
            }
            if t == "new" {
                return Err(exec_err(
                    "0A000",
                    "rule actions on NEW are not implemented".to_string(),
                ));
            }
        }
    }
    // PG19 parse_utilcmd.c: OLD/NEW may not be referenced within
    // a rule action's WITH query.
    for a in actions {
        if cte_body_refs_pseudo(&a.stmt, "old") {
            return Err(exec_err(
                "0A000",
                "cannot refer to OLD within WITH query".to_string(),
            ));
        }
        if cte_body_refs_pseudo(&a.stmt, "new") {
            return Err(exec_err(
                "0A000",
                "cannot refer to NEW within WITH query".to_string(),
            ));
        }
    }
    // FOR UPDATE OF targets in SELECT actions must name a FROM
    // relation of the action (PG19 transformLockingClause; the
    // rules_base "for update of old" case errors at CREATE time).
    for a in actions {
        if let Stmt::Select(sel) = &a.stmt {
            if !sel.for_update_of.is_empty() {
                fn from_names(item: &FromItem, out: &mut Vec<String>) {
                    match item {
                        FromItem::Table { name, alias, .. } => {
                            out.push(alias.clone().unwrap_or_else(|| rule_rel_key(name)));
                        }
                        FromItem::Derived { alias, .. } => {
                            out.push(alias.clone());
                        }
                        FromItem::Join { left, right, .. } => {
                            from_names(left, out);
                            from_names(right, out);
                        }
                        _ => {}
                    }
                }
                let mut names: Vec<String> = Vec::new();
                for item in &sel.from {
                    from_names(item, &mut names);
                }
                for n in &sel.for_update_of {
                    if !names.iter().any(|x| x.eq_ignore_ascii_case(n)) {
                        return Err(exec_err(
                            "42P01",
                            format!(
                                "relation \"{}\" in FOR UPDATE clause not found in FROM clause",
                                n
                            ),
                        ));
                    }
                }
            }
        }
    }
    // PG19 analyze.c: an INSERT ... ON CONFLICT DO SELECT action
    // must itself carry a RETURNING clause.
    for a in actions {
        if let Stmt::Insert {
            on_conflict,
            returning,
            ..
        } = &a.stmt
        {
            if returning.is_empty()
                && matches!(
                    on_conflict.as_ref().map(|oc| &oc.action),
                    Some(ConflictAction::DoSelect { .. })
                )
            {
                return Err(exec_err_detail(
                    "42601",
                    "ON CONFLICT DO SELECT requires a RETURNING clause",
                    "A rule action is INSERT ... ON CONFLICT DO SELECT, which requires a RETURNING clause.",
                ));
            }
        }
    }
    // RETURNING discipline (rewriteDefine.c): RETURNING may appear
    // in at most one action, only of an unconditional INSTEAD rule,
    // and its entry count must match the event relation's columns.
    {
        let mut with_ret: Vec<&RuleAction> = Vec::new();
        for a in actions {
            let has = match &a.stmt {
                Stmt::Insert { returning, .. }
                | Stmt::Update { returning, .. }
                | Stmt::Delete { returning, .. } => !returning.is_empty(),
                _ => false,
            };
            if has {
                with_ret.push(a);
            }
        }
        if with_ret.len() > 1 {
            return Err(exec_err(
                "0A000",
                "RETURNING lists in multiple actions of a rule are not supported".to_string(),
            ));
        }
        if let Some(a) = with_ret.first() {
            if !instead || condition.is_some() {
                return Err(exec_err(
                    "0A000",
                    "RETURNING is not supported in conditional or non-INSTEAD rules".to_string(),
                ));
            }
            // PG19 rewriteDefine.c: a star in the action's
            // RETURNING expands over the ACTION TARGET's columns;
            // the total must equal the EVENT relation's column
            // count (checked against `cols` below).
            let target_width = match &a.stmt {
                Stmt::Insert { table, .. }
                | Stmt::Update { table, .. }
                | Stmt::Delete { table, .. } => rel_columns(eng, ctx, table)
                    .map(|c| c.len())
                    .unwrap_or(cols.len()),
                _ => cols.len(),
            };
            let n = match &a.stmt {
                Stmt::Insert { returning, .. }
                | Stmt::Update { returning, .. }
                | Stmt::Delete { returning, .. } => returning
                    .iter()
                    .map(|i| match i {
                        SelectItem::All => target_width,
                        SelectItem::AllOf(_) => target_width,
                        SelectItem::Expr { .. } => 1,
                    })
                    .sum::<usize>(),
                _ => 0,
            };
            if n > cols.len() {
                return Err(exec_err(
                    "42601",
                    "RETURNING list has too many entries".to_string(),
                ));
            }
            if n < cols.len() {
                return Err(exec_err(
                    "42601",
                    "RETURNING list has too few entries".to_string(),
                ));
            }
        }
    }
    // Bare-reference validation for INSERT..VALUES actions
    // (rules_foo): an unqualified column reference in a VALUES list
    // has no scope to bind to — PG rejects it at CREATE time.
    for a in actions {
        if let Stmt::Insert { rows, .. } = &a.stmt {
            for row in rows {
                for v in row {
                    if let InsertValue::Expr(e) = v {
                        let mut bad: Option<String> = None;
                        walk_expr(e, &mut |x| {
                            if let Expr::Column { table: None, name } = x {
                                bad = Some(name.clone());
                            }
                        });
                        if let Some(col) = bad {
                            return Err(exec_err(
                                "42703",
                                format!("column \"{}\" does not exist", col),
                            ));
                        }
                    }
                }
            }
        }
    }
    let key = rule_key_for(eng, ctx, relation);
    let mut list = rules_on(eng, &key);
    if let Some(pos) = list.iter().position(|r| r.name == name) {
        if !or_replace {
            return Err(exec_err(
                "42710",
                format!(
                    "rule \"{}\" for relation \"{}\" already exists",
                    name,
                    rule_rel_key(relation)
                ),
            ));
        }
        if list[pos].event != event {
            return Err(exec_err(
                "42P01",
                format!(
                    "rule \"{}\" for relation \"{}\" is not an {} rule",
                    name,
                    rule_rel_key(relation),
                    rule_event::name(event)
                ),
            ));
        }
        let oid = list[pos].oid;
        let enabled = list[pos].enabled;
        list[pos] = RuleDef {
            name: name.to_string(),
            event,
            instead,
            nothing,
            condition: condition.clone(),
            condition_text: condition_text.clone(),
            actions: actions.to_vec(),
            enabled,
            oid,
        };
    } else {
        let oid = eng.db.next_oid;
        eng.db.next_oid += 1;
        list.push(RuleDef {
            name: name.to_string(),
            event,
            instead,
            nothing,
            condition: condition.clone(),
            condition_text: condition_text.clone(),
            actions: actions.to_vec(),
            enabled: b'O',
            oid,
        });
    }
    set_rules(ctx, eng, &key, list);
    Ok(ExecResult::Command {
        tag: "CREATE RULE".to_string(),
    })
}

/// DROP RULE (PG19 RemoveRule-ish / DropRuleStmt).
pub(crate) fn exec_drop_rule(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    relation: &str,
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    let is_view = rel_is_view(eng, ctx, relation);
    let exists = is_view
        || eng
            .db
            .find_table(relation, ctx.snap, &ctx.all_xids, ctx.session)
            .is_some();
    if !exists {
        if if_exists {
            ctx.notices.push(format!(
                "relation \"{}\" does not exist, skipping",
                rule_rel_key(relation)
            ));
            return Ok(ExecResult::Command {
                tag: "DROP RULE".to_string(),
            });
        }
        return Err(exec_err(
            "42P01",
            format!("relation \"{}\" does not exist", rule_rel_key(relation)),
        ));
    }
    // PG19 (rules.sql, bug #5072): a view's _RETURN rule cannot be
    // dropped directly.
    if name.eq_ignore_ascii_case("_RETURN") && is_view {
        return Err(exec_err(
            "55000",
            "cannot drop the \"_RETURN\" rule of a view".to_string(),
        ));
    }
    let key = rule_key_for(eng, ctx, relation);
    let mut list = rules_on(eng, &key);
    let pos = list.iter().position(|r| r.name == name);
    match pos {
        Some(p) => {
            list.remove(p);
            set_rules(ctx, eng, &key, list);
            Ok(ExecResult::Command {
                tag: "DROP RULE".to_string(),
            })
        }
        None => {
            if if_exists {
                ctx.notices.push(format!(
                    "rule \"{}\" for relation \"{}\" does not exist, skipping",
                    name,
                    rule_rel_key(relation)
                ));
                Ok(ExecResult::Command {
                    tag: "DROP RULE".to_string(),
                })
            } else {
                Err(exec_err(
                    "42704",
                    format!(
                        "rule \"{}\" for relation \"{}\" does not exist",
                        name,
                        rule_rel_key(relation)
                    ),
                ))
            }
        }
    }
}

/// ALTER RULE ... RENAME TO (PG19 RenameRuleStmt).
pub(crate) fn exec_alter_rule_rename(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    relation: &str,
    new_name: &str,
) -> Result<ExecResult, ExecError> {
    if rel_columns(eng, ctx, relation).is_err() {
        return Err(exec_err(
            "42P01",
            format!("relation \"{}\" does not exist", rule_rel_key(relation)),
        ));
    }
    // PG19: the _RETURN rule of a view cannot be renamed, and no
    // rule may be renamed TO _RETURN.
    if name.eq_ignore_ascii_case("_RETURN") || new_name.eq_ignore_ascii_case("_RETURN") {
        return Err(exec_err(
            "0A000",
            "renaming a view's \"_RETURN\" rule is not supported".to_string(),
        ));
    }
    let key = rule_key_for(eng, ctx, relation);
    let mut list = rules_on(eng, &key);
    if list.iter().any(|r| r.name == new_name) {
        return Err(exec_err(
            "42710",
            format!(
                "rule \"{}\" for relation \"{}\" already exists",
                new_name,
                rule_rel_key(relation)
            ),
        ));
    }
    match list.iter_mut().find(|r| r.name == name) {
        Some(r) => {
            r.name = new_name.to_string();
            set_rules(ctx, eng, &key, list);
            Ok(ExecResult::Command {
                tag: "ALTER RULE".to_string(),
            })
        }
        None => Err(exec_err(
            "42704",
            format!(
                "rule \"{}\" for relation \"{}\" does not exist",
                name,
                rule_rel_key(relation)
            ),
        )),
    }
}

/// ALTER TABLE ... {ENABLE|DISABLE} [ALWAYS|REPLICA] RULE.
pub(crate) fn exec_set_rule_enabled(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    name: &Option<String>,
    mode: u8,
) -> Result<(), ExecError> {
    let key = rule_key_for(eng, ctx, table);
    let mut list = rules_on(eng, &key);
    let mut hit = false;
    for r in list.iter_mut() {
        if name.as_deref().is_none_or(|n| n == r.name) {
            r.enabled = mode;
            hit = true;
        }
    }
    if !hit {
        return Err(exec_err(
            "42704",
            format!(
                "rule \"{}\" for relation \"{}\" does not exist",
                name.as_deref().unwrap_or(""),
                rule_rel_key(table)
            ),
        ));
    }
    set_rules(ctx, eng, &key, list);
    Ok(())
}

/// pg_get_ruledef reconstruction for one rule (PG19 ruleutils
/// shape, approximated from the stored source texts).
pub(crate) fn ruledef_text(rel: &str, rule: &RuleDef) -> String {
    let mut out = format!(
        "CREATE RULE {} AS ON {} TO {}",
        rule.name,
        rule_event::name(rule.event),
        rule_rel_key(rel)
    );
    if let Some(c) = &rule.condition_text {
        out.push_str(&format!(" WHERE {}", c));
    }
    out.push_str(" DO ");
    if rule.instead {
        out.push_str("INSTEAD ");
    }
    if rule.nothing {
        out.push_str("NOTHING");
    } else if rule.actions.len() == 1 {
        out.push_str(&rule.actions[0].text);
    } else {
        let parts: Vec<&str> = rule.actions.iter().map(|a| a.text.as_str()).collect();
        out.push_str(&format!("({})", parts.join("; ")));
    }
    out.push(';');
    out
}

/// pg_rules.definition: pg_get_ruledef minus the
/// "CREATE RULE <name> AS " prefix.
pub(crate) fn rule_definition_text(rel: &str, rule: &RuleDef) -> String {
    let full = ruledef_text(rel, rule);
    let prefix = format!("CREATE RULE {} AS ", rule.name);
    full.strip_prefix(&prefix).unwrap_or(&full).to_string()
}

// =====================================================================
// Firing: NEW/OLD substitution + per-row rule application.
// =====================================================================

/// The NEW/OLD row pair a rule fires with, in event-relation
/// column space.
pub(crate) struct Pseudo<'a> {
    pub cols: &'a [(String, ColType)],
    pub new_row: Option<&'a [Value]>,
    pub old_row: Option<&'a [Value]>,
}

impl<'a> Pseudo<'a> {
    fn row_of(&self, pseudo: &str) -> Option<&'a [Value]> {
        if pseudo.eq_ignore_ascii_case("new") {
            self.new_row
        } else if pseudo.eq_ignore_ascii_case("old") {
            self.old_row
        } else {
            None
        }
    }
    fn value(&self, pseudo: &str, col: &str) -> Option<Value> {
        let row = self.row_of(pseudo)?;
        let idx = self.cols.iter().position(|(n, _)| n == col)?;
        row.get(idx).cloned()
    }
    fn is_pseudo(qual: &str) -> bool {
        qual.eq_ignore_ascii_case("new") || qual.eq_ignore_ascii_case("old")
    }
}

/// Substitute NEW./OLD. references in an expression with literal
/// values from the firing row. When `bare` is set (rule
/// qualifications only), unqualified references to event-relation
/// columns also substitute, from the bare row (NEW for INSERT,
/// OLD for UPDATE/DELETE — PG19's result-relation substitution).
fn subst_expr(e: &mut Expr, p: &Pseudo, bare: Option<&[Value]>) -> Result<(), ExecError> {
    match e {
        Expr::Column { table, name } => {
            if let Some(q) = table {
                if Pseudo::is_pseudo(q) {
                    return match p.value(q, name) {
                        Some(v) => {
                            *e = value_literal_expr(&v);
                            Ok(())
                        }
                        None => Err(exec_err(
                            "42703",
                            format!("column {}.{} does not exist", q, name),
                        )),
                    };
                }
            } else if let Some(row) = bare {
                if let Some(idx) = p.cols.iter().position(|(n, _)| n == name) {
                    if let Some(v) = row.get(idx).cloned() {
                        *e = value_literal_expr(&v);
                        return Ok(());
                    }
                }
            }
            Ok(())
        }
        Expr::WholeRow { qual } => {
            if Pseudo::is_pseudo(qual) {
                if let Some(row) = p.row_of(qual) {
                    let lits: Vec<Expr> = row.iter().map(value_literal_expr).collect();
                    *e = Expr::Row(lits);
                }
            }
            Ok(())
        }
        Expr::NamedArg { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::CastNamed { expr, .. }
        | Expr::FieldAccess { expr, .. }
        | Expr::Not(expr)
        | Expr::BitNot(expr)
        | Expr::Neg(expr)
        | Expr::IsNull { expr, .. }
        | Expr::IsBool { expr, .. }
        | Expr::Extract { from: expr, .. } => subst_expr(expr, p, bare),
        Expr::Arith { left, right, .. }
        | Expr::Concat(left, right)
        | Expr::JsonOp { left, right, .. }
        | Expr::Cmp { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::IsDistinctFrom { left, right, .. }
        | Expr::UserOp { left, right, .. } => {
            subst_expr(left, p, bare)?;
            subst_expr(right, p, bare)
        }
        Expr::AtTimeZone { expr, zone } => {
            subst_expr(expr, p, bare)?;
            if let Some(z) = zone {
                subst_expr(z, p, bare)?;
            }
            Ok(())
        }
        Expr::Row(elems) | Expr::ArrayCtor { elems, .. } => {
            for el in elems {
                subst_expr(el, p, bare)?;
            }
            Ok(())
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            subst_expr(expr, p, bare)?;
            subst_expr(pattern, p, bare)?;
            if let Some(esc) = escape {
                subst_expr(esc, p, bare)?;
            }
            Ok(())
        }
        Expr::Regex { expr, pattern, .. } => {
            subst_expr(expr, p, bare)?;
            subst_expr(pattern, p, bare)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            subst_expr(expr, p, bare)?;
            subst_expr(low, p, bare)?;
            subst_expr(high, p, bare)
        }
        Expr::Func { args, .. } => {
            for a in args {
                subst_expr(a, p, bare)?;
            }
            Ok(())
        }
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            if let Some(o) = operand {
                subst_expr(o, p, bare)?;
            }
            for (k, r) in whens {
                subst_expr(k, p, bare)?;
                subst_expr(r, p, bare)?;
            }
            if let Some(el) = else_ {
                subst_expr(el, p, bare)?;
            }
            Ok(())
        }
        Expr::Agg { arg, arg2, .. } => {
            if let Some(a) = arg {
                subst_expr(a, p, bare)?;
            }
            if let Some(a) = arg2 {
                subst_expr(a, p, bare)?;
            }
            Ok(())
        }
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            for a in direct_args {
                subst_expr(a, p, bare)?;
            }
            for o in within_order_by {
                subst_expr(&mut o.expr, p, bare)?;
            }
            if let Some(x) = filter {
                subst_expr(x, p, bare)?;
            }
            Ok(())
        }
        Expr::Subscript { array, indices, .. } => {
            subst_expr(array, p, bare)?;
            for i in indices {
                subst_expr(i, p, bare)?;
            }
            Ok(())
        }
        Expr::Slice { array, bounds, .. } => {
            subst_expr(array, p, bare)?;
            for (lo, hi) in bounds {
                if let Some(b) = lo {
                    subst_expr(b, p, bare)?;
                }
                if let Some(b) = hi {
                    subst_expr(b, p, bare)?;
                }
            }
            Ok(())
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                subst_expr(a, p, bare)?;
            }
            for x in partition_by {
                subst_expr(x, p, bare)?;
            }
            for t in order_by {
                subst_expr(&mut t.expr, p, bare)?;
            }
            Ok(())
        }
        Expr::ScalarSub(sub) | Expr::ArraySubquery(sub) => subst_select(sub, p),
        Expr::JsonArraySubq { query, .. } => subst_select(query, p),
        Expr::InSub { expr, sub, .. } => {
            subst_expr(expr, p, bare)?;
            subst_select(sub, p)
        }
        Expr::Quantified { left, sub, .. } => {
            subst_expr(left, p, bare)?;
            subst_select(sub, p)
        }
        Expr::Exists { sub, .. } => subst_select(sub, p),
        _ => Ok(()),
    }
}

/// Expand a select/returning item list's NEW.*/OLD.* stars into
/// per-column literal items (in firing-row order positions).
fn subst_items(items: &mut Vec<SelectItem>, p: &Pseudo) -> Result<(), ExecError> {
    let mut out: Vec<SelectItem> = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        match item {
            SelectItem::AllOf(q) if Pseudo::is_pseudo(&q) => {
                if let Some(row) = p.row_of(&q) {
                    for (i, (cn, _)) in p.cols.iter().enumerate() {
                        out.push(SelectItem::Expr {
                            expr: value_literal_expr(&row[i]),
                            alias: Some(cn.clone()),
                        });
                    }
                }
            }
            SelectItem::Expr { mut expr, alias } => {
                subst_expr(&mut expr, p, None)?;
                out.push(SelectItem::Expr { expr, alias });
            }
            other => out.push(other),
        }
    }
    *items = out;
    Ok(())
}

fn subst_from(item: &mut FromItem, p: &Pseudo) -> Result<(), ExecError> {
    match item {
        FromItem::Table { .. } => Ok(()),
        FromItem::Derived { sub, .. } => subst_select(sub, p),
        FromItem::Values { rows, .. } => {
            for row in rows {
                for e in row {
                    subst_expr(e, p, None)?;
                }
            }
            Ok(())
        }
        FromItem::Function { args, .. } => {
            for a in args {
                subst_expr(a, p, None)?;
            }
            Ok(())
        }
        FromItem::Join {
            left, right, on, ..
        } => {
            subst_from(left, p)?;
            subst_from(right, p)?;
            if let Some(on) = on {
                subst_expr(on, p, None)?;
            }
            Ok(())
        }
    }
}

fn subst_select(sel: &mut SelectStmt, p: &Pseudo) -> Result<(), ExecError> {
    for cte in &mut sel.with {
        match &mut cte.body {
            CteBody::Simple(s) => subst_select(s, p)?,
            CteBody::Union { left, right, .. } => {
                subst_select(left, p)?;
                subst_select(right, p)?;
            }
            CteBody::Dml(s) => subst_stmt(s, p)?,
        }
    }
    subst_items(&mut sel.items, p)?;
    for item in &mut sel.from {
        subst_from(item, p)?;
    }
    if let Some(w) = &mut sel.where_ {
        subst_expr(w, p, None)?;
    }
    for set in &mut sel.group_by {
        for e in set {
            subst_expr(e, p, None)?;
        }
    }
    if let Some(h) = &mut sel.having {
        subst_expr(h, p, None)?;
    }
    for t in &mut sel.order_by {
        subst_expr(&mut t.expr, p, None)?;
    }
    for e in &mut sel.distinct_on {
        subst_expr(e, p, None)?;
    }
    if let Some(op) = &mut sel.set_op {
        subst_select(&mut op.left, p)?;
        for b in &mut op.chain {
            subst_select(&mut b.right, p)?;
        }
        for t in &mut op.order_by {
            subst_expr(&mut t.expr, p, None)?;
        }
    }
    Ok(())
}

fn subst_on_conflict(oc: &mut OnConflict, p: &Pseudo) -> Result<(), ExecError> {
    match &mut oc.action {
        ConflictAction::DoUpdate { sets, where_ } => {
            for (_, e) in sets {
                subst_expr(e, p, None)?;
            }
            if let Some(w) = where_ {
                subst_expr(w, p, None)?;
            }
        }
        ConflictAction::DoSelect { where_, .. } => {
            if let Some(w) = where_ {
                subst_expr(w, p, None)?;
            }
        }
        ConflictAction::DoNothing => {}
    }
    Ok(())
}

/// Substitute NEW/OLD throughout one rule action statement
/// (cloned from the stored RuleDef before execution).
fn subst_stmt(stmt: &mut Stmt, p: &Pseudo) -> Result<(), ExecError> {
    match stmt {
        Stmt::Insert {
            rows,
            select,
            with,
            on_conflict,
            returning,
            ..
        } => {
            for cte in with.iter_mut() {
                match &mut cte.body {
                    CteBody::Simple(s) => subst_select(s, p)?,
                    CteBody::Union { left, right, .. } => {
                        subst_select(left, p)?;
                        subst_select(right, p)?;
                    }
                    CteBody::Dml(s) => subst_stmt(s, p)?,
                }
            }
            // VALUES rows: NEW.*/OLD.* whole-row values expand into
            // one literal cell per event-relation column.
            for row in rows.iter_mut() {
                let mut out: Vec<InsertValue> = Vec::with_capacity(row.len());
                for v in row.drain(..) {
                    match v {
                        InsertValue::Expr(Expr::WholeRow { qual }) if Pseudo::is_pseudo(&qual) => {
                            if let Some(r) = p.row_of(&qual) {
                                for val in r {
                                    out.push(InsertValue::Expr(value_literal_expr(val)));
                                }
                            }
                        }
                        InsertValue::Expr(mut e) => {
                            subst_expr(&mut e, p, None)?;
                            out.push(InsertValue::Expr(e));
                        }
                        other => out.push(other),
                    }
                }
                *row = out;
            }
            if let Some(sel) = select {
                subst_select(sel, p)?;
            }
            if let Some(oc) = on_conflict {
                subst_on_conflict(oc, p)?;
            }
            subst_items(returning, p)?;
            Ok(())
        }
        Stmt::Update {
            sets,
            from,
            where_,
            with,
            returning,
            ..
        } => {
            for cte in with.iter_mut() {
                match &mut cte.body {
                    CteBody::Simple(s) => subst_select(s, p)?,
                    CteBody::Union { left, right, .. } => {
                        subst_select(left, p)?;
                        subst_select(right, p)?;
                    }
                    CteBody::Dml(s) => subst_stmt(s, p)?,
                }
            }
            for (_, e) in sets {
                subst_expr(e, p, None)?;
            }
            for item in from {
                subst_from(item, p)?;
            }
            if let Some(w) = where_ {
                subst_expr(w, p, None)?;
            }
            subst_items(returning, p)?;
            Ok(())
        }
        Stmt::Delete {
            using,
            where_,
            with,
            returning,
            ..
        } => {
            for cte in with.iter_mut() {
                match &mut cte.body {
                    CteBody::Simple(s) => subst_select(s, p)?,
                    CteBody::Union { left, right, .. } => {
                        subst_select(left, p)?;
                        subst_select(right, p)?;
                    }
                    CteBody::Dml(s) => subst_stmt(s, p)?,
                }
            }
            for item in using {
                subst_from(item, p)?;
            }
            if let Some(w) = where_ {
                subst_expr(w, p, None)?;
            }
            subst_items(returning, p)?;
            Ok(())
        }
        Stmt::Select(sel) => subst_select(sel, p),
        _ => Ok(()),
    }
}

/// Evaluate an expression that substitution has made constant
/// (literals / functions of literals / uncorrelated subqueries).
fn eval_const(eng: &mut Engine, ctx: &StmtCtx, e: &Expr) -> Result<Value, ExecError> {
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
        priv_role_override: None,
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: None,
        write: None,
    };
    eval_expr(&mut q, &[], e)
}

/// Evaluate an expression over a single row scope (qual = `qual`,
/// columns = `cols`, values = `row`).
fn eval_over_row(
    eng: &mut Engine,
    ctx: &StmtCtx,
    qual: &str,
    cols: &[(String, ColType)],
    row: &[Value],
    e: &Expr,
) -> Result<Value, ExecError> {
    let schema: Vec<QCol> = cols
        .iter()
        .map(|(n, ty)| QCol {
            qual: qual.to_string(),
            name: n.clone(),
            ty: *ty,
            hidden: false,
            src_ord: 0,
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
        read_only: ctx.read_only,
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
    let scope = Scope {
        schema: &schema,
        row,
        prov: None,
    };
    eval_expr(&mut q, &[scope], e)
}

/// Does the rule's qualification hold for this firing row?
/// (No qualification = unconditional = always true.)
fn rule_qual_ok(
    eng: &mut Engine,
    ctx: &StmtCtx,
    rule: &RuleDef,
    p: &Pseudo,
    bare: Option<&[Value]>,
) -> Result<bool, ExecError> {
    match &rule.condition {
        None => Ok(true),
        Some(cond) => {
            let mut e = cond.clone();
            subst_expr(&mut e, p, bare)?;
            match eval_const(eng, ctx, &e)? {
                Value::Bool(b) => Ok(b),
                Value::Null => Ok(false),
                _ => Err(exec_err(
                    "42804",
                    "rule qualification must evaluate to boolean".to_string(),
                )),
            }
        }
    }
}

/// Execute one rule's actions for one firing row. Returns the
/// DML results of the actions (for tag/RETURNING accounting).
fn fire_rule_actions(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    rule: &RuleDef,
    p: &Pseudo,
) -> Result<Vec<ExecResult>, ExecError> {
    let mut out = Vec::new();
    for action in &rule.actions {
        let mut stmt = action.stmt.clone();
        subst_stmt(&mut stmt, p)?;
        let r = execute(eng, ctx, &stmt)?;
        out.push(r);
    }
    Ok(out)
}

/// Count carried by a DML result's tag ("INSERT 0 3" / "UPDATE 3").
fn dml_tag_count(r: &ExecResult) -> u64 {
    let tag = match r {
        ExecResult::Dml { tag, .. } => tag.as_str(),
        ExecResult::Command { tag } => tag.as_str(),
        _ => return 0,
    };
    tag.rsplit(' ')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn dml_result(
    tag_word: &str,
    count: u64,
    cols: Vec<(String, ColType)>,
    rows: Vec<Row>,
) -> ExecResult {
    let tag = if tag_word == "INSERT" {
        format!("INSERT 0 {}", count)
    } else {
        format!("{} {}", tag_word, count)
    };
    ExecResult::Dml {
        tag,
        columns: cols,
        rows,
    }
}

/// Recursion guard (PG19 rewrite_events): entering the firing
/// path for a (relation, event) already on the firing stack is
/// 42P17. The frame is pushed for the body's duration and always
/// popped, including on error.
fn with_rule_frame<R>(
    eng: &mut Engine,
    rel: &str,
    event: u8,
    body: impl FnOnce(&mut Engine) -> Result<R, ExecError>,
) -> Result<R, ExecError> {
    let key = (rule_rel_key(rel), event);
    if eng.rule_stack.contains(&key) {
        return Err(exec_err(
            "42P17",
            format!(
                "infinite recursion detected in rules for relation \"{}\"",
                key.0
            ),
        ));
    }
    eng.rule_stack.push(key);
    let r = body(eng);
    eng.rule_stack.pop();
    r
}

/// Returning-column descriptors for a composed result when the
/// original statement did not run (all rows consumed by INSTEAD
/// actions): expand `*`/stars against the event relation's
/// columns; expression types are best-effort (simple-protocol
/// runners read values as text).
fn returning_columns(cols: &[(String, ColType)], items: &[SelectItem]) -> Vec<(String, ColType)> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::All | SelectItem::AllOf(_) => out.extend(cols.iter().cloned()),
            SelectItem::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
                let ty = match expr {
                    Expr::Column { name, .. } => cols
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, t)| *t)
                        .unwrap_or(ColType::Text),
                    _ => ColType::Text,
                };
                out.push((name, ty));
            }
        }
    }
    out
}

/// Project the user's RETURNING list over an INSTEAD action's
/// result row (event-relation space): NEW = the action row,
/// OLD = the source row the rule fired for.
fn project_user_returning(
    eng: &mut Engine,
    ctx: &StmtCtx,
    rel: &str,
    cols: &[(String, ColType)],
    new_row: &[Value],
    old_row: Option<&[Value]>,
    items: &[SelectItem],
) -> Result<Vec<Value>, ExecError> {
    let mk_schema = |qual: &str| -> Vec<QCol> {
        cols.iter()
            .map(|(n, ty)| QCol {
                qual: qual.to_string(),
                name: n.clone(),
                ty: *ty,
                hidden: false,
                src_ord: 0,
            })
            .collect()
    };
    let schema_new = mk_schema("new");
    let schema_old = mk_schema("old");
    let schema_rel = mk_schema(rel);
    let empty: &[Value] = &[];
    let old_or_empty = old_row.unwrap_or(empty);
    let scopes = [
        Scope {
            schema: &schema_old,
            row: old_or_empty,
            prov: None,
        },
        Scope {
            schema: &schema_new,
            row: new_row,
            prov: None,
        },
        Scope {
            schema: &schema_rel,
            row: new_row,
            prov: None,
        },
    ];
    let mut lock_ids = Vec::new();
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::All => out.extend(new_row.iter().cloned()),
            SelectItem::AllOf(q) => {
                if q.eq_ignore_ascii_case("old") {
                    out.extend(old_or_empty.iter().cloned());
                } else {
                    out.extend(new_row.iter().cloned());
                }
            }
            SelectItem::Expr { expr, .. } => {
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
                    priv_role_override: None,
                    hashed_exists: Rc::new(RefCell::new(HashMap::new())),
                    immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
                    plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
                    hashed_in: Rc::new(RefCell::new(Vec::new())),
                    pending_updates: None,
                    write: None,
                };
                out.push(eval_expr(&mut q, &scopes, expr)?);
            }
        }
    }
    Ok(out)
}

// =====================================================================
// Source-row materialization (single evaluation of the original
// statement's candidate rows, in event-relation column space).
// =====================================================================

/// All currently-visible rows of a table (partitioned targets scan
/// every leaf, remapped to parent column order), unfiltered.
fn scan_table_rows(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    cols: &[(String, ColType)],
) -> Vec<Vec<Value>> {
    let t = match eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        Some(t) => t,
        None => return Vec::new(),
    };
    let partitioned = t.partition.is_some();
    let targets: Vec<(String, Vec<(String, ColType)>)> = if partitioned {
        let mut ls = partition_leaves(eng, ctx, table);
        if ls.is_empty() {
            ls.push((rule_rel_key(table), cols.to_vec()));
        }
        ls
    } else {
        vec![(rule_rel_key(table), cols.to_vec())]
    };
    let mut out = Vec::new();
    for (leaf, leaf_cols) in &targets {
        let lt = match eng
            .db
            .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
        {
            Some(t) => t,
            None => continue,
        };
        for r in lt
            .rows
            .iter()
            .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
        {
            if leaf == &rule_rel_key(table) {
                out.push(r.values.to_vec());
            } else {
                out.push(reorder_row(leaf_cols, cols, &r.values));
            }
        }
    }
    out
}

/// Rows of a view (base name key), via the v2.14 scan helper.
fn scan_view_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let rows = view_scan_all(eng, ctx, table)?;
    Ok(rows.into_iter().map(|r| r.to_vec()).collect())
}

enum InsertSource {
    Rows(Vec<Vec<Value>>),
    /// The source form can't be materialized safely (indirection
    /// targets, parameters): delegate when only ALSO rules exist,
    /// error when INSTEAD semantics are required.
    Fallback,
}

fn materialize_insert_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    cols: &[(String, ColType)],
    columns: &Option<Vec<InsertTarget>>,
    rows: &[Vec<InsertValue>],
    select: &Option<SelectStmt>,
    with: &[CteDef],
) -> Result<InsertSource, ExecError> {
    let base = rule_rel_key(table);
    let is_view = eng.db.views.contains_key(&base);
    let meta: Option<TableMeta> = if is_view {
        None
    } else {
        eng.db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .map(TableMeta::of)
    };
    let gen_flags: Vec<bool> = match &meta {
        Some(m) => m.gen_cols.iter().map(|g| g.is_some()).collect(),
        None => vec![false; cols.len()],
    };
    let target_cols: Vec<usize> = match columns {
        Some(ts) => {
            let mut v = Vec::with_capacity(ts.len());
            for t in ts {
                if !t.indirection.is_empty() {
                    return Ok(InsertSource::Fallback);
                }
                match cols.iter().position(|(n, _)| *n == t.name) {
                    Some(i) => v.push(i),
                    None => {
                        return Err(exec_err(
                            "42703",
                            format!(
                                "column \"{}\" of relation \"{}\" does not exist",
                                t.name, base
                            ),
                        ));
                    }
                }
            }
            v
        }
        None => (0..cols.len()).filter(|i| !gen_flags[*i]).collect(),
    };
    let ctes = materialize_dml_ctes(eng, ctx, with)?;
    // View-space defaults: the view's own ALTER VIEW defaults,
    // else — for a simply-mapped column — the underlying base
    // column's default (PG: a view column inherits the default of
    // the base column it maps to; NEW in a rule action sees the
    // filled value).
    let view_defaults: Vec<Option<DefaultExpr>> = if is_view {
        let mut d: Vec<Option<DefaultExpr>> = eng
            .db
            .find_view(table, ctx.snap, ctx.own)
            .map(|v| v.col_defaults.clone())
            .unwrap_or_default();
        d.resize(cols.len(), None);
        if let Ok(ViewClass::Auto(av)) = classify_view(eng, ctx, table) {
            if let Some(bt) = eng
                .db
                .find_table(&av.base, ctx.snap, &ctx.all_xids, ctx.session)
            {
                let bmeta = TableMeta::of(bt);
                for (i, vc) in av.cols.iter().enumerate() {
                    if i >= d.len() || d[i].is_some() {
                        continue;
                    }
                    if let VCol::Base(bname) = vc {
                        if let Some(bi) = bmeta.column_index(bname) {
                            let dd = domain_default(eng, &bmeta, bi).cloned();
                            if let Some(def) = bmeta.defaults[bi].as_ref().or(dd.as_ref()) {
                                d[i] = Some(def.clone());
                            }
                        }
                    }
                }
            }
        }
        d
    } else {
        Vec::new()
    };
    let mut filled: Vec<(Vec<Value>, Vec<bool>)> = Vec::new();
    {
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: ctx.read_only,
            depth: 0,
            lock_ids: &mut Vec::new(),
            ctes: ctes.clone(),
            wctx: None,
            srf_vals: Vec::new(),
            priv_scopes: Vec::new(),
            priv_role_override: None,
            hashed_exists: Rc::new(RefCell::new(HashMap::new())),
            immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
            plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
            hashed_in: Rc::new(RefCell::new(Vec::new())),
            pending_updates: None,
            write: Some(qwrite_from_ctx(
                &mut ctx.writes,
                ctx.write_xid,
                ctx.level,
                ctx.default_toast_compression,
            )),
        };
        if let Some(sel) = select {
            let srows = run_select(&mut q, sel, &[])?.rows;
            for srow in &srows {
                if srow.len() != target_cols.len() {
                    return Err(exec_err(
                        "42601",
                        if srow.len() > target_cols.len() {
                            "INSERT has more expressions than target columns".to_string()
                        } else {
                            "INSERT has more target columns than expressions".to_string()
                        },
                    ));
                }
                let mut values = vec![Value::Null; cols.len()];
                let mut explicit = vec![false; cols.len()];
                for (k, &ci) in target_cols.iter().enumerate() {
                    let (cname, ctype) = &cols[ci];
                    values[ci] = coerce_value(srow[k].clone(), ctype, cname)?;
                    explicit[ci] = true;
                }
                filled.push((values, explicit));
            }
        } else {
            let expanded = expand_insert_srf_rows(&mut q, rows)?;
            {
                for erow in &expanded {
                    let mut values = vec![Value::Null; cols.len()];
                    let mut explicit = vec![false; cols.len()];
                    for (k, v) in erow.iter().enumerate() {
                        let ci = target_cols[k];
                        let (cname, ctype) = &cols[ci];
                        values[ci] = match v {
                            InsertValue::Lit(l) => coerce_literal(l, ctype, cname)?,
                            InsertValue::Param(_) => return Ok(InsertSource::Fallback),
                            InsertValue::Expr(e) => {
                                let val = eval_expr(&mut q, &[], e)?;
                                coerce_value(val, ctype, cname)?
                            }
                            InsertValue::Default => match &meta {
                                Some(m) => {
                                    let dd = domain_default(&*q.eng, m, ci).cloned();
                                    match m.defaults[ci].as_ref().or(dd.as_ref()) {
                                        Some(d) => eval_default(
                                            &mut *q.eng,
                                            ctx.snap,
                                            ctx.own,
                                            ctx.session,
                                            ctx.role,
                                            d,
                                            ctype,
                                            cname,
                                        )?,
                                        None => Value::Null,
                                    }
                                }
                                None => match view_defaults.get(ci) {
                                    Some(Some(d)) => {
                                        let d = d.clone();
                                        eval_default(
                                            &mut *q.eng,
                                            ctx.snap,
                                            ctx.own,
                                            ctx.session,
                                            ctx.role,
                                            &d,
                                            ctype,
                                            cname,
                                        )?
                                    }
                                    _ => Value::Null,
                                },
                            },
                        };
                        explicit[ci] = true;
                    }
                    filled.push((values, explicit));
                }
            }
        }
    }
    let mut out = Vec::with_capacity(filled.len());
    for (mut values, explicit) in filled {
        if let Some(m) = &meta {
            for i in 0..cols.len() {
                if !explicit[i] {
                    let dd = domain_default(eng, m, i).cloned();
                    if let Some(d) = m.defaults[i].as_ref().or(dd.as_ref()) {
                        values[i] = eval_default(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            d,
                            &cols[i].1,
                            &cols[i].0,
                        )?;
                    }
                }
            }
            compute_generated_columns(
                eng,
                ctx.snap,
                ctx.own,
                ctx.session,
                ctx.role,
                m,
                &mut values,
            )?;
        } else if is_view {
            for i in 0..cols.len() {
                if !explicit[i] {
                    if let Some(Some(d)) = view_defaults.get(i) {
                        values[i] = eval_default(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            d,
                            &cols[i].1,
                            &cols[i].0,
                        )?;
                    }
                }
            }
        }
        out.push(values);
    }
    Ok(InsertSource::Rows(out))
}

/// Column positions used when re-issuing kept INSERT rows as a
/// literal VALUES insert: every non-generated column (generated
/// columns are recomputed by the inner path; identity-always
/// columns accept the materialized value only through the inner
/// path's own default handling, so they are included and the
/// inner path's OVERRIDING rules apply as for any explicit list —
/// suites' rule cases carry no identity columns).
fn insert_recon_cols(meta: &Option<TableMeta>, ncols: usize) -> Vec<usize> {
    match meta {
        Some(m) => (0..ncols).filter(|&i| m.gen_cols[i].is_none()).collect(),
        None => (0..ncols).collect(),
    }
}

/// OLD rows matched by an UPDATE/DELETE (views: view rows; tables:
/// visible storage rows), filtered by the statement WHERE.
fn materialize_old_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    where_: &Option<Expr>,
    with: &[CteDef],
) -> Result<Vec<Vec<Value>>, ExecError> {
    let base = rule_rel_key(table);
    let cols = rel_columns(eng, ctx, table)?;
    let qual = alias.clone().unwrap_or_else(|| base.clone());
    let cands: Vec<Vec<Value>> = if eng.db.views.contains_key(&base) {
        scan_view_rows(eng, ctx, table)?
    } else {
        scan_table_rows(eng, ctx, table, &cols)
    };
    let _ = with;
    let mut out = Vec::new();
    for row in cands {
        let keep = match where_ {
            None => true,
            Some(w) => {
                matches!(
                    eval_over_row(eng, ctx, &qual, &cols, &row, w)?,
                    Value::Bool(true)
                )
            }
        };
        if keep {
            out.push(row);
        }
    }
    Ok(out)
}

/// (old, new) pairs for an UPDATE: FROM rows come from a
/// synthesized join SELECT (PG's multi-match cardinality error
/// surfaces from the inner path for kept rows; firing uses the
/// first pair per distinct old row).
fn materialize_update_pairs(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    sets: &[(String, Expr)],
    from: &[FromItem],
    where_: &Option<Expr>,
    with: &[CteDef],
) -> Result<Vec<(Vec<Value>, Vec<Value>)>, ExecError> {
    let base = rule_rel_key(table);
    let cols = rel_columns(eng, ctx, table)?;
    let qual = alias.clone().unwrap_or_else(|| base.clone());
    if from.is_empty() {
        let olds = materialize_old_rows(eng, ctx, table, alias, where_, with)?;
        let mut out = Vec::with_capacity(olds.len());
        for old in olds {
            let mut new = old.clone();
            for (cname, e) in sets {
                let ci = cols.iter().position(|(n, _)| n == cname).ok_or_else(|| {
                    exec_err(
                        "42703",
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            cname, base
                        ),
                    )
                })?;
                let v = eval_over_row(eng, ctx, &qual, &cols, &old, e)?;
                new[ci] = coerce_value(v, &cols[ci].1, cname)?;
            }
            out.push((old, new));
        }
        return Ok(out);
    }
    // The synthesized SELECT scans the target through the ordinary
    // SELECT path, which handles views as well as tables.
    // Synthesized SELECT: target columns + each distinct SET
    // expression, over target x FROM with the statement WHERE.
    let mut set_exprs: Vec<Expr> = Vec::new();
    for (_, e) in sets {
        if !set_exprs.contains(e) {
            set_exprs.push(e.clone());
        }
    }
    let mut items: Vec<SelectItem> = cols
        .iter()
        .map(|(n, _)| SelectItem::Expr {
            expr: Expr::Column {
                table: Some(qual.clone()),
                name: n.clone(),
            },
            alias: None,
        })
        .collect();
    for e in &set_exprs {
        items.push(SelectItem::Expr {
            expr: e.clone(),
            alias: None,
        });
    }
    let mut from_items = vec![FromItem::Table {
        name: table.to_string(),
        alias: alias.clone(),
        col_aliases: vec![],
        only: false,
    }];
    from_items.extend(from.iter().cloned());
    let sel = SelectStmt {
        with: vec![],
        distinct: false,
        distinct_on: vec![],
        items,
        from: from_items,
        where_: where_.clone(),
        group_by: vec![],
        group_by_sets: false,
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
        for_update: false,
        lock_clause: false,
        for_update_of: vec![],
        set_op: None,
        dead_rtes: 0,
    };
    let ctes = materialize_dml_ctes(eng, ctx, with)?;
    let srows = {
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: ctx.read_only,
            depth: 0,
            lock_ids: &mut Vec::new(),
            ctes: ctes.clone(),
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
        run_select(&mut q, &sel, &[])?.rows
    };
    let ncols = cols.len();
    let mut seen: Vec<Vec<Value>> = Vec::new();
    let mut out = Vec::new();
    for srow in &srows {
        let old: Vec<Value> = srow[..ncols].to_vec();
        if seen.contains(&old) {
            continue;
        }
        seen.push(old.clone());
        let mut new = old.clone();
        for (cname, e) in sets {
            let ci = cols.iter().position(|(n, _)| n == cname).ok_or_else(|| {
                exec_err(
                    "42703",
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        cname, base
                    ),
                )
            })?;
            let k = set_exprs.iter().position(|x| x == e).unwrap();
            new[ci] = coerce_value(srow[ncols + k].clone(), &cols[ci].1, cname)?;
        }
        out.push((old, new));
    }
    Ok(out)
}

/// An equality WHERE pinning exactly one row (primary key columns
/// when the table has a PK, else every column), for applying a
/// kept UPDATE/DELETE row through the inner path after partial
/// INSTEAD suppression.
fn row_equality_where(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    alias: &Option<String>,
    cols: &[(String, ColType)],
    row: &[Value],
) -> Expr {
    let base = rule_rel_key(table);
    let qual = alias.clone().unwrap_or(base);
    let pk_cols: Vec<usize> = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
        .and_then(|t| {
            t.pkey.as_ref().map(|pk| {
                pk.cols
                    .iter()
                    .filter_map(|cn| cols.iter().position(|(n, _)| n == cn))
                    .collect::<Vec<usize>>()
            })
        })
        .unwrap_or_default();
    let use_cols: Vec<usize> = if pk_cols.is_empty() {
        (0..cols.len()).collect()
    } else {
        pk_cols
    };
    let mut cond: Option<Expr> = None;
    for &ci in &use_cols {
        let cell = match &row[ci] {
            Value::Null => Expr::IsNull {
                expr: Box::new(Expr::Column {
                    table: Some(qual.clone()),
                    name: cols[ci].0.clone(),
                }),
                neg: false,
            },
            v => Expr::Cmp {
                left: Box::new(Expr::Column {
                    table: Some(qual.clone()),
                    name: cols[ci].0.clone(),
                }),
                op: CmpOp::Eq,
                right: Box::new(value_literal_expr(v)),
            },
        };
        cond = Some(match cond {
            None => cell,
            Some(prev) => Expr::And(Box::new(prev), Box::new(cell)),
        });
    }
    cond.unwrap_or_else(|| value_literal_expr(&Value::Bool(true)))
}

// =====================================================================
// Firing entry points. Each returns None when no rule applies (the
// caller runs the statement untouched), else the statement's
// result under rule semantics.
//
// PG19 execution order (RewriteQuery): for INSERT the original
// query runs FIRST and rule actions follow; for UPDATE/DELETE the
// rule actions run FIRST and the (NOT-qual-restricted) original
// runs last. Suppression is per-row: a row is consumed iff an
// enabled INSTEAD rule's qualification holds for it (or an
// unconditional INSTEAD rule exists for the event).
// =====================================================================

/// Index of the rule's (single, CREATE-validated) action that
/// carries a RETURNING list, if any.
fn action_returning_idx(rule: &RuleDef) -> Option<usize> {
    rule.actions.iter().position(|a| match &a.stmt {
        Stmt::Insert { returning, .. }
        | Stmt::Update { returning, .. }
        | Stmt::Delete { returning, .. } => !returning.is_empty(),
        _ => false,
    })
}

fn split_dml(r: &ExecResult) -> (u64, Vec<(String, ColType)>, Vec<Row>) {
    match r {
        ExecResult::Dml { columns, rows, .. } => (dml_tag_count(r), columns.clone(), rows.clone()),
        ExecResult::Command { .. } => (dml_tag_count(r), Vec::new(), Vec::new()),
        _ => (0, Vec::new(), Vec::new()),
    }
}

/// Compose the final result from the original statement's result
/// (kept rows, in source order) and the INSTEAD actions' projected
/// RETURNING rows (suppressed rows, in source order).
#[allow(clippy::too_many_arguments)]
fn compose_dml_result(
    verb: &str,
    n: usize,
    cols: &[(String, ColType)],
    returning: &[SelectItem],
    inner: Option<ExecResult>,
    kept: &[usize],
    instead_counts: u64,
    action_ret: &[(usize, Vec<Value>)],
) -> ExecResult {
    let (inner_count, inner_cols, inner_rows) = match &inner {
        Some(r) => split_dml(r),
        None => (0, Vec::new(), Vec::new()),
    };
    let count = if inner.is_some() {
        inner_count
    } else {
        instead_counts
    };
    let mut out_rows: Vec<Row> = Vec::new();
    if !returning.is_empty() {
        let mut queue = inner_rows.into_iter();
        for i in 0..n {
            if kept.contains(&i) {
                if let Some(r) = queue.next() {
                    out_rows.push(r);
                }
            } else {
                for (ai, prow) in action_ret {
                    if *ai == i {
                        out_rows.push(Row::new(prow.clone()));
                    }
                }
            }
        }
    }
    let out_cols = if inner.is_some() && !returning.is_empty() {
        inner_cols
    } else {
        returning_columns(cols, returning)
    };
    dml_result(verb, count, out_cols, out_rows)
}

fn conditional_view_err(verb: &str, base: &str) -> ExecError {
    exec_err_detail(
        "55000",
        format!("cannot {} view \"{}\"", verb, base),
        "Views with conditional DO INSTEAD rules are not automatically updatable.",
    )
}

fn with_rules_err() -> ExecError {
    exec_err(
        "0A000",
        "WITH cannot be used in a query that is rewritten by rules into multiple queries",
    )
}

// ---------------------------------------------------------------- INSERT

#[allow(clippy::too_many_arguments)]
pub(crate) fn try_fire_insert(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    // v2.28: OVERRIDING clause of the original statement.
    overriding: Option<u8>,
    columns: &Option<Vec<InsertTarget>>,
    rows: &[Vec<InsertValue>],
    select: &Option<SelectStmt>,
    with: &[CteDef],
    on_conflict: &Option<OnConflict>,
    returning: &[SelectItem],
) -> Option<Result<ExecResult, ExecError>> {
    let rules = firing_rules(eng, &rule_key_for(eng, ctx, table), rule_event::INSERT);
    if rules.is_empty() {
        return None;
    }
    let base = rule_rel_key(table);
    let is_view = rel_is_view(eng, ctx, table);
    let instead: Vec<&RuleDef> = rules.iter().filter(|r| r.instead).collect();
    if is_view
        && !instead.is_empty()
        && instead.iter().all(|r| r.condition.is_some())
        && instead_triggers_for(eng, ctx, table, trig_event::INSERT).is_empty()
    {
        return Some(Err(conditional_view_err("insert into", &base)));
    }
    if !with.is_empty() && !instead.iter().any(|r| r.condition.is_none()) {
        return Some(Err(with_rules_err()));
    }
    if on_conflict.is_some() && !(is_view && instead.is_empty()) {
        let do_update = matches!(
            on_conflict.as_ref().map(|oc| &oc.action),
            Some(ConflictAction::DoUpdate { .. })
        );
        let upd_rules = firing_rules(eng, &rule_key_for(eng, ctx, table), rule_event::UPDATE);
        if !rules.is_empty() || (do_update && !upd_rules.is_empty()) {
            return Some(Err(exec_err(
                "0A000",
                "INSERT with ON CONFLICT clause cannot be used with table that has INSERT or UPDATE rules",
            )));
        }
    }
    if !returning.is_empty()
        && !instead.is_empty()
        && !instead.iter().any(|r| r.action_returning().is_some())
    {
        return Some(Err(exec_err(
            "0A000",
            format!("cannot perform INSERT RETURNING on relation \"{}\"", base),
        )));
    }
    // PG19 rewriteHandler.c: an INSTEAD rule whose action is
    // INSERT ... ON CONFLICT DO SELECT requires the triggering
    // query to carry a RETURNING clause (42601).
    if returning.is_empty() {
        let do_select_action = instead.iter().any(|r| {
            r.actions.iter().any(|a| match &a.stmt {
                Stmt::Insert { on_conflict, .. } => matches!(
                    on_conflict.as_ref().map(|oc| &oc.action),
                    Some(ConflictAction::DoSelect { .. })
                ),
                _ => false,
            })
        });
        if do_select_action {
            return Some(Err(exec_err_detail(
                "42601",
                "ON CONFLICT DO SELECT requires a RETURNING clause",
                "A rule action is INSERT ... ON CONFLICT DO SELECT, which requires a RETURNING clause.",
            )));
        }
    }
    Some(with_rule_frame(eng, table, rule_event::INSERT, |eng| {
        fire_insert_impl(
            eng,
            ctx,
            table,
            overriding,
            columns,
            rows,
            select,
            with,
            on_conflict,
            returning,
            &rules,
        )
    }))
}

#[allow(clippy::too_many_arguments)]
fn fire_insert_impl(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    // v2.28: OVERRIDING of the original statement (fallback path).
    overriding: Option<u8>,
    columns: &Option<Vec<InsertTarget>>,
    rows: &[Vec<InsertValue>],
    select: &Option<SelectStmt>,
    with: &[CteDef],
    on_conflict: &Option<OnConflict>,
    returning: &[SelectItem],
    rules: &[RuleDef],
) -> Result<ExecResult, ExecError> {
    let base = rule_rel_key(table);
    let cols = rel_columns(eng, ctx, table)?;
    let instead_exist = rules.iter().any(|r| r.instead);
    let src_rows =
        match materialize_insert_rows(eng, ctx, table, &cols, columns, rows, select, with)? {
            InsertSource::Rows(r) => r,
            InsertSource::Fallback => {
                if instead_exist {
                    return Err(exec_err(
                        "0A000",
                        "rules on this INSERT form are not supported",
                    ));
                }
                return exec_insert_inner(
                    eng,
                    ctx,
                    table,
                    overriding,
                    columns,
                    rows,
                    select,
                    with,
                    on_conflict,
                    returning,
                );
            }
        };
    let n = src_rows.len();
    // INSTEAD qualifications evaluate against pre-statement state
    // (they become NOT quals on the original, which runs first).
    let mut instead_ok: Vec<Vec<bool>> = vec![Vec::new(); rules.len()];
    let mut suppressed = vec![false; n];
    for (ri, rule) in rules.iter().enumerate() {
        if !rule.instead {
            continue;
        }
        let mut oks = Vec::with_capacity(n);
        for row in &src_rows {
            let p = Pseudo {
                cols: &cols,
                new_row: Some(row),
                old_row: None,
            };
            oks.push(rule_qual_ok(eng, ctx, rule, &p, Some(row))?);
        }
        for (i, ok) in oks.iter().enumerate() {
            if *ok {
                suppressed[i] = true;
            }
        }
        instead_ok[ri] = oks;
    }
    // The original runs FIRST (kept rows only), as a literal
    // VALUES reconstruction over the materialized rows.
    let kept: Vec<usize> = (0..n).filter(|i| !suppressed[*i]).collect();
    let inner = if kept.is_empty() {
        None
    } else {
        let meta: Option<TableMeta> = if eng.db.views.contains_key(&base) {
            None
        } else {
            eng.db
                .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                .map(TableMeta::of)
        };
        // For a view, the reconstruction may only target
        // base-mapped columns (expression columns are not
        // updatable — PG's original query never targets them).
        let recon_cols = if meta.is_none() {
            match classify_view(eng, ctx, table) {
                Ok(ViewClass::Auto(av)) => av
                    .cols
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| matches!(c, VCol::Base(_)))
                    .map(|(i, _)| i)
                    .collect::<Vec<usize>>(),
                _ => insert_recon_cols(&meta, cols.len()),
            }
        } else {
            insert_recon_cols(&meta, cols.len())
        };
        let recon_columns: Vec<InsertTarget> = recon_cols
            .iter()
            .map(|&ci| InsertTarget {
                name: cols[ci].0.clone(),
                indirection: Vec::new(),
            })
            .collect();
        let recon_rows: Vec<Vec<InsertValue>> = kept
            .iter()
            .map(|&i| {
                recon_cols
                    .iter()
                    .map(|&ci| InsertValue::Expr(value_literal_expr(&src_rows[i][ci])))
                    .collect()
            })
            .collect();
        Some(exec_insert_inner(
            eng,
            ctx,
            table,
            None,
            &Some(recon_columns),
            &recon_rows,
            &None,
            &[],
            on_conflict,
            returning,
        )?)
    };
    // Rule actions follow, in rule-name order.
    let mut instead_counts: u64 = 0;
    let mut action_ret: Vec<(usize, Vec<Value>)> = Vec::new();
    for (ri, rule) in rules.iter().enumerate() {
        for i in 0..n {
            let ok = if rule.instead {
                instead_ok[ri].get(i).copied().unwrap_or(false)
            } else {
                let p = Pseudo {
                    cols: &cols,
                    new_row: Some(&src_rows[i]),
                    old_row: None,
                };
                rule_qual_ok(eng, ctx, rule, &p, Some(&src_rows[i]))?
            };
            if !ok {
                continue;
            }
            let p = Pseudo {
                cols: &cols,
                new_row: Some(&src_rows[i]),
                old_row: None,
            };
            let results = fire_rule_actions(eng, ctx, rule, &p)?;
            if rule.instead {
                for (a, r) in rule.actions.iter().zip(results.iter()) {
                    if matches!(a.stmt, Stmt::Insert { .. }) {
                        instead_counts += dml_tag_count(r);
                    }
                }
                if !returning.is_empty() {
                    if let Some(ai) = action_returning_idx(rule) {
                        if let Some(ExecResult::Dml { rows: arows, .. }) = results.get(ai) {
                            for ar in arows {
                                let proj = project_user_returning(
                                    eng,
                                    ctx,
                                    table,
                                    &cols,
                                    &ar[..],
                                    None,
                                    returning,
                                )?;
                                action_ret.push((i, proj));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(compose_dml_result(
        "INSERT",
        n,
        &cols,
        returning,
        inner,
        &kept,
        instead_counts,
        &action_ret,
    ))
}

// ---------------------------------------------------------------- UPDATE

#[allow(clippy::too_many_arguments)]
pub(crate) fn try_fire_update(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    sets: &[(String, Expr)],
    from: &[FromItem],
    where_: &Option<Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
) -> Option<Result<ExecResult, ExecError>> {
    let rules = firing_rules(eng, &rule_key_for(eng, ctx, table), rule_event::UPDATE);
    if rules.is_empty() {
        return None;
    }
    let base = rule_rel_key(table);
    let is_view = rel_is_view(eng, ctx, table);
    let instead: Vec<&RuleDef> = rules.iter().filter(|r| r.instead).collect();
    if is_view
        && !instead.is_empty()
        && instead.iter().all(|r| r.condition.is_some())
        && instead_triggers_for(eng, ctx, table, trig_event::UPDATE).is_empty()
    {
        return Some(Err(conditional_view_err("update", &base)));
    }
    if !with.is_empty() && !instead.iter().any(|r| r.condition.is_none()) {
        return Some(Err(with_rules_err()));
    }
    if !returning.is_empty()
        && !instead.is_empty()
        && !instead.iter().any(|r| r.action_returning().is_some())
    {
        return Some(Err(exec_err(
            "0A000",
            format!("cannot perform UPDATE RETURNING on relation \"{}\"", base),
        )));
    }
    Some(with_rule_frame(eng, table, rule_event::UPDATE, |eng| {
        fire_update_impl(
            eng, ctx, table, alias, sets, from, where_, with, returning, &rules,
        )
    }))
}

#[allow(clippy::too_many_arguments)]
fn fire_update_impl(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    sets: &[(String, Expr)],
    from: &[FromItem],
    where_: &Option<Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
    rules: &[RuleDef],
) -> Result<ExecResult, ExecError> {
    let cols = rel_columns(eng, ctx, table)?;
    let pairs = materialize_update_pairs(eng, ctx, table, alias, sets, from, where_, with)?;
    let n = pairs.len();
    let mut suppressed = vec![false; n];
    let mut instead_counts: u64 = 0;
    let mut action_ret: Vec<(usize, Vec<Value>)> = Vec::new();
    // Rule actions run FIRST for UPDATE (rule-name order).
    for rule in rules {
        for (i, (old, new)) in pairs.iter().enumerate() {
            let p = Pseudo {
                cols: &cols,
                new_row: Some(new),
                old_row: Some(old),
            };
            if !rule_qual_ok(eng, ctx, rule, &p, Some(old))? {
                continue;
            }
            if rule.instead {
                suppressed[i] = true;
            }
            let results = fire_rule_actions(eng, ctx, rule, &p)?;
            if rule.instead {
                for (a, r) in rule.actions.iter().zip(results.iter()) {
                    if matches!(a.stmt, Stmt::Update { .. }) {
                        instead_counts += dml_tag_count(r);
                    }
                }
                if !returning.is_empty() {
                    if let Some(ai) = action_returning_idx(rule) {
                        if let Some(ExecResult::Dml { rows: arows, .. }) = results.get(ai) {
                            for ar in arows {
                                let proj = project_user_returning(
                                    eng,
                                    ctx,
                                    table,
                                    &cols,
                                    &ar[..],
                                    Some(old),
                                    returning,
                                )?;
                                action_ret.push((i, proj));
                            }
                        }
                    }
                }
            }
        }
    }
    // The original runs LAST, over kept rows only.
    let kept: Vec<usize> = (0..n).filter(|i| !suppressed[*i]).collect();
    let inner = if kept.is_empty() {
        None
    } else if kept.len() == n {
        Some(exec_update_inner(
            eng, ctx, table, alias, sets, from, where_, with, returning,
        )?)
    } else {
        let mut cnt: u64 = 0;
        let mut ocols: Vec<(String, ColType)> = Vec::new();
        let mut orows: Vec<Row> = Vec::new();
        for &i in &kept {
            let w = row_equality_where(eng, ctx, table, alias, &cols, &pairs[i].0);
            let r = exec_update_inner(eng, ctx, table, alias, sets, &[], &Some(w), &[], returning)?;
            let (c, cc, rr) = split_dml(&r);
            cnt += c;
            if ocols.is_empty() {
                ocols = cc;
            }
            orows.extend(rr);
        }
        Some(dml_result("UPDATE", cnt, ocols, orows))
    };
    Ok(compose_dml_result(
        "UPDATE",
        n,
        &cols,
        returning,
        inner,
        &kept,
        instead_counts,
        &action_ret,
    ))
}

// ---------------------------------------------------------------- DELETE

#[allow(clippy::too_many_arguments)]
pub(crate) fn try_fire_delete(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    using: &[FromItem],
    where_: &Option<Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
) -> Option<Result<ExecResult, ExecError>> {
    let rules = firing_rules(eng, &rule_key_for(eng, ctx, table), rule_event::DELETE);
    if rules.is_empty() {
        return None;
    }
    let base = rule_rel_key(table);
    let is_view = rel_is_view(eng, ctx, table);
    let instead: Vec<&RuleDef> = rules.iter().filter(|r| r.instead).collect();
    if is_view
        && !instead.is_empty()
        && instead.iter().all(|r| r.condition.is_some())
        && instead_triggers_for(eng, ctx, table, trig_event::DELETE).is_empty()
    {
        return Some(Err(conditional_view_err("delete from", &base)));
    }
    if !with.is_empty() && !instead.iter().any(|r| r.condition.is_none()) {
        return Some(Err(with_rules_err()));
    }
    if !returning.is_empty()
        && !instead.is_empty()
        && !instead.iter().any(|r| r.action_returning().is_some())
    {
        return Some(Err(exec_err(
            "0A000",
            format!("cannot perform DELETE RETURNING on relation \"{}\"", base),
        )));
    }
    Some(with_rule_frame(eng, table, rule_event::DELETE, |eng| {
        fire_delete_impl(
            eng, ctx, table, alias, using, where_, with, returning, &rules,
        )
    }))
}

#[allow(clippy::too_many_arguments)]
fn fire_delete_impl(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    using: &[FromItem],
    where_: &Option<Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
    rules: &[RuleDef],
) -> Result<ExecResult, ExecError> {
    let base = rule_rel_key(table);
    let cols = rel_columns(eng, ctx, table)?;
    let olds: Vec<Vec<Value>> = if using.is_empty() {
        materialize_old_rows(eng, ctx, table, alias, where_, with)?
    } else {
        // Fold USING into a synthesized join SELECT of the target
        // columns (dedupe: first pair per distinct old row).
        let qual = alias.clone().unwrap_or_else(|| base.clone());
        let mut items: Vec<SelectItem> = cols
            .iter()
            .map(|(cn, _)| SelectItem::Expr {
                expr: Expr::Column {
                    table: Some(qual.clone()),
                    name: cn.clone(),
                },
                alias: None,
            })
            .collect();
        items.dedup_by(|a, b| format!("{:?}", a) == format!("{:?}", b));
        let mut from_items = vec![FromItem::Table {
            name: table.to_string(),
            alias: alias.clone(),
            col_aliases: vec![],
            only: false,
        }];
        from_items.extend(using.iter().cloned());
        let sel = SelectStmt {
            with: vec![],
            distinct: false,
            distinct_on: vec![],
            items,
            from: from_items,
            where_: where_.clone(),
            group_by: vec![],
            group_by_sets: false,
            having: None,
            order_by: vec![],
            limit: None,
            offset: None,
            for_update: false,
            lock_clause: false,
            for_update_of: vec![],
            set_op: None,
            dead_rtes: 0,
        };
        let ctes = materialize_dml_ctes(eng, ctx, with)?;
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: ctx.read_only,
            depth: 0,
            lock_ids: &mut Vec::new(),
            ctes: ctes.clone(),
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
        let srows = run_select(&mut q, &sel, &[])?.rows;
        let mut seen: Vec<Vec<Value>> = Vec::new();
        let mut out = Vec::new();
        for srow in &srows {
            let old: Vec<Value> = srow[..cols.len()].to_vec();
            if !seen.contains(&old) {
                seen.push(old.clone());
                out.push(old);
            }
        }
        out
    };
    let n = olds.len();
    let mut suppressed = vec![false; n];
    let mut instead_counts: u64 = 0;
    let mut action_ret: Vec<(usize, Vec<Value>)> = Vec::new();
    // Rule actions run FIRST for DELETE (rule-name order).
    for rule in rules {
        for (i, old) in olds.iter().enumerate() {
            let p = Pseudo {
                cols: &cols,
                new_row: None,
                old_row: Some(old),
            };
            if !rule_qual_ok(eng, ctx, rule, &p, Some(old))? {
                continue;
            }
            if rule.instead {
                suppressed[i] = true;
            }
            let results = fire_rule_actions(eng, ctx, rule, &p)?;
            if rule.instead {
                for (a, r) in rule.actions.iter().zip(results.iter()) {
                    if matches!(a.stmt, Stmt::Delete { .. }) {
                        instead_counts += dml_tag_count(r);
                    }
                }
                if !returning.is_empty() {
                    if let Some(ai) = action_returning_idx(rule) {
                        if let Some(ExecResult::Dml { rows: arows, .. }) = results.get(ai) {
                            for ar in arows {
                                let proj = project_user_returning(
                                    eng,
                                    ctx,
                                    table,
                                    &cols,
                                    &ar[..],
                                    Some(old),
                                    returning,
                                )?;
                                action_ret.push((i, proj));
                            }
                        }
                    }
                }
            }
        }
    }
    // The original runs LAST, over kept rows only.
    let kept: Vec<usize> = (0..n).filter(|i| !suppressed[*i]).collect();
    let inner = if kept.is_empty() {
        None
    } else if kept.len() == n {
        Some(exec_delete_inner(
            eng, ctx, table, alias, using, where_, with, returning,
        )?)
    } else {
        let mut cnt: u64 = 0;
        let mut ocols: Vec<(String, ColType)> = Vec::new();
        let mut orows: Vec<Row> = Vec::new();
        for &i in &kept {
            let w = row_equality_where(eng, ctx, table, alias, &cols, &olds[i]);
            let r = exec_delete_inner(eng, ctx, table, alias, &[], &Some(w), &[], returning)?;
            let (c, cc, rr) = split_dml(&r);
            cnt += c;
            if ocols.is_empty() {
                ocols = cc;
            }
            orows.extend(rr);
        }
        Some(dml_result("DELETE", cnt, ocols, orows))
    };
    Ok(compose_dml_result(
        "DELETE",
        n,
        &cols,
        returning,
        inner,
        &kept,
        instead_counts,
        &action_ret,
    ))
}

// =====================================================================
// Catalogs: pg_rewrite + pg_rules (virtual), pg_get_ruledef.
// Rows cover every registered rule plus a synthesized _RETURN row
// per live view (PG models a view's SELECT rule in pg_rewrite).
// =====================================================================

fn qcol(qual: &str, name: &str, ty: ColType) -> QCol {
    QCol {
        qual: qual.to_string(),
        name: name.to_string(),
        ty,
        hidden: false,
        src_ord: 0,
    }
}

/// Deterministic synthetic OID for a view's _RETURN rule.
pub(crate) fn return_rule_oid(view: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in view.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    2_000_000_000 + (h % 100_000_000) as i64
}

fn return_ruledef_text(view: &str, query: &str) -> String {
    format!(
        "CREATE RULE _RETURN AS ON SELECT TO {} DO INSTEAD {};",
        view,
        query.trim().trim_end_matches(';')
    )
}

pub(crate) fn pg_rewrite_schema() -> Vec<QCol> {
    let q = "pg_rewrite";
    vec![
        qcol(q, "oid", ColType::Int),
        qcol(q, "rulename", ColType::Text),
        qcol(q, "ev_class", ColType::Int),
        qcol(q, "ev_namespace", ColType::Int),
        qcol(q, "ev_type", ColType::Text),
        qcol(q, "ev_enabled", ColType::Text),
        qcol(q, "is_instead", ColType::Bool),
        qcol(q, "ev_qual", ColType::Text),
        qcol(q, "ev_action", ColType::Text),
    ]
}

pub(crate) fn pg_rewrite_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_rewrite_schema();
    let mut rows: Vec<QRow> = Vec::new();
    let mut rels: Vec<&String> = db
        .rules
        .keys()
        .filter(|k| rule_key_visible(k, session))
        .collect();
    rels.sort();
    for rel in rels {
        let ev_class = db
            .find_table(rule_key_display(rel), snap, &[own], session)
            .map(|t| t.oid as i64)
            .unwrap_or(0);
        let mut rs: Vec<&RuleDef> = db.rules[rel].iter().collect();
        rs.sort_by(|a, b| a.name.cmp(&b.name));
        for r in rs {
            let action_text = r
                .actions
                .iter()
                .map(|a| a.text.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::Int(r.oid as i64),
                    Value::text(r.name.as_str()),
                    Value::Int(ev_class),
                    Value::Int(2200),
                    Value::SingleChar(r.event),
                    Value::SingleChar(r.enabled),
                    Value::Bool(r.instead),
                    Value::text(r.condition_text.as_deref().unwrap_or("")),
                    Value::text(action_text.as_str()),
                ]),
                prov: Vec::new(),
            });
        }
    }
    // Synthesized _RETURN rows, one per live view.
    let mut views: Vec<&String> = db.views.keys().collect();
    views.sort();
    for vname in views {
        let Some(v) = db.find_view(vname, snap, own) else {
            continue;
        };
        rows.push(QRow {
            cells: Row::new(vec![
                Value::Int(return_rule_oid(vname)),
                Value::text("_RETURN"),
                Value::Int(0),
                Value::Int(2200),
                Value::SingleChar(b'1'),
                Value::SingleChar(b'O'),
                Value::Bool(true),
                Value::text(""),
                Value::text(v.query.trim().trim_end_matches(';')),
            ]),
            prov: Vec::new(),
        });
    }
    (schema, rows)
}

pub(crate) fn pg_rules_schema() -> Vec<QCol> {
    let q = "pg_rules";
    vec![
        qcol(q, "schemaname", ColType::Text),
        qcol(q, "tablename", ColType::Text),
        qcol(q, "rulename", ColType::Text),
        qcol(q, "definition", ColType::Text),
    ]
}

pub(crate) fn pg_rules_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_rules_schema();
    let mut rows: Vec<QRow> = Vec::new();
    let mut rels: Vec<&String> = db
        .rules
        .keys()
        .filter(|k| rule_key_visible(k, session))
        .collect();
    rels.sort();
    for rel in rels {
        let display = rule_key_display(rel);
        let mut rs: Vec<&RuleDef> = db.rules[rel].iter().collect();
        rs.sort_by(|a, b| a.name.cmp(&b.name));
        for r in rs {
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::text("public"),
                    Value::text(display),
                    Value::text(r.name.as_str()),
                    Value::text(rule_definition_text(display, r).as_str()),
                ]),
                prov: Vec::new(),
            });
        }
    }
    let mut views: Vec<&String> = db.views.keys().collect();
    views.sort();
    for vname in views {
        let Some(v) = db.find_view(vname, snap, own) else {
            continue;
        };
        rows.push(QRow {
            cells: Row::new(vec![
                Value::text("public"),
                Value::text(vname.rsplit('.').next().unwrap_or(vname.as_str())),
                Value::text("_RETURN"),
                Value::text(
                    format!(
                        "ON SELECT TO {} DO INSTEAD {};",
                        vname.rsplit('.').next().unwrap_or(vname.as_str()),
                        v.query.trim().trim_end_matches(';')
                    )
                    .as_str(),
                ),
            ]),
            prov: Vec::new(),
        });
    }
    (schema, rows)
}

/// pg_get_ruledef(oid): the reconstructed rule text, the _RETURN
/// text for a view's synthetic OID, or NULL for an unknown OID
/// (PG19 ruleutils behavior for a missing rewrite row).
pub(crate) fn ruledef_by_oid(db: &Database, snap: &Snapshot, own: u64, oid: i64) -> Value {
    for (rel, rs) in &db.rules {
        for r in rs {
            if r.oid as i64 == oid {
                return Value::text(ruledef_text(rule_key_display(rel), r).as_str());
            }
        }
    }
    for vname in db.views.keys() {
        if return_rule_oid(vname) == oid {
            if let Some(v) = db.find_view(vname, snap, own) {
                return Value::text(
                    return_ruledef_text(
                        vname.rsplit('.').next().unwrap_or(vname.as_str()),
                        &v.query,
                    )
                    .as_str(),
                );
            }
        }
    }
    Value::Null
}
