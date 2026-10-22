// v1.99: EXPLAIN of DML — generalize the v1.98 EXPLAIN analysis pass
// beyond SELECT (scope: hidden_files/v199/scope.md; PG19 gram.y
// ExplainableStmt = Select | Insert | Update | Delete | DeclareCursor |
// CreateAs | CreateMatView | RefreshMatView | Execute).
// v2.01: MergeStmt joins the set (vx_merge + Merge ModifyTable plans).
//
// Three layers, mirroring the SELECT path:
//  1. `validate_explain_stmt` — static analysis in PG19's shape: target
//     relation existence (42P01), the executors' own privilege helpers
//     (42501), target-column existence/duplicates (42703/42701), VALUES
//     arity (42601), ON CONFLICT arbiter inference via `plan_upsert`
//     (42P10), and expression resolution through the v1.98 `vx_*`
//     machinery over synthesized scopes. EXPLAIN ANALYZE runs the same
//     validation before executing, exactly like the SELECT path.
//  2. Planning render — a ModifyTable header (`Insert on t` /
//     `Update on t` / `Delete on t`, plus the ON CONFLICT property
//     lines) over the child plan `plan_select` builds for the
//     statement's scan shape; CTAS / CREATE MATERIALIZED VIEW /
//     DECLARE / REFRESH render the inner query's plan directly (PG19
//     ExplainOneUtility has no wrapper node for them).
//  3. EXPLAIN ANALYZE — the statement is executed through the real
//     executor (PG semantics: the change happens) and the plan is
//     rendered with actual row counts, reusing `render_analyze` for
//     the child tree.
use super::*;

use crate::sql::empty_select;

/// Build the throwaway `Q` the validation pass runs on (same shape as
/// `exec_explain`'s; no write context — validation never executes).
fn explain_q<'a>(
    eng: &'a mut Engine,
    ctx: &StmtCtx<'a>,
    lock_ids: &'a mut Vec<(String, u64)>,
) -> Q<'a, 'a> {
    Q {
        eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: ctx.read_only,
        depth: 0,
        lock_ids,
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
    }
}

/// v1.99: EXPLAIN analysis for every explainable statement kind.
/// `Stmt::Select` keeps the v1.98 entry point; DML kinds dispatch to
/// the validators below. Called from `exec_explain` (planning) and
/// `exec_explain_analyze` (before execution).
pub(crate) fn validate_explain_stmt(
    q: &mut Q,
    ctx: &StmtCtx,
    stmt: &Stmt,
) -> Result<(), ExecError> {
    match stmt {
        Stmt::Select(sel) => validate_explain_select(q, sel),
        Stmt::Insert {
            table,
            columns,
            rows,
            select,
            with,
            on_conflict,
            returning,
        } => vx_insert(
            q,
            ctx,
            table,
            columns.as_ref(),
            rows,
            select.as_ref(),
            with,
            on_conflict.as_ref(),
            returning,
        ),
        Stmt::Update {
            table,
            alias,
            sets,
            from,
            where_,
            with,
            returning,
        } => vx_update(
            q,
            ctx,
            table,
            alias.as_deref(),
            sets,
            from,
            where_.as_ref(),
            with,
            returning,
        ),
        Stmt::Delete {
            table,
            alias,
            using,
            where_,
            with,
            returning,
        } => vx_delete(
            q,
            ctx,
            table,
            alias.as_deref(),
            using,
            where_.as_ref(),
            with,
            returning,
        ),
        Stmt::Merge {
            table,
            alias,
            source,
            on,
            whens,
            returning,
            with,
        } => vx_merge(
            q,
            ctx,
            table,
            alias.as_deref(),
            source,
            on,
            whens,
            returning,
            with,
        ),
        Stmt::CreateTableAs { select, .. } => validate_explain_select(q, select),
        Stmt::Declare { query, .. } => validate_explain_select(q, query),
        Stmt::CreateView { query, .. } => match parse_statement(query) {
            Ok(Stmt::Select(sel)) => validate_explain_select(q, &sel),
            _ => Ok(()),
        },
        Stmt::RefreshMatView { name, .. } => {
            let view = q.eng.db.find_view(name, q.snap, q.own).cloned();
            match view {
                Some(v) => match parse_statement(&v.query) {
                    Ok(Stmt::Select(sel)) => validate_explain_select(q, &sel),
                    _ => Ok(()),
                },
                None => Err(exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", name),
                )),
            }
        }
        _ => Ok(()),
    }
}

/// The target table of a DML statement, or PG's 42P01. The engine's
/// DML executors resolve targets through `find_table` only (views are
/// NOT DML targets anywhere in this engine — plain DML on a view fails
/// 42P01 identically), so EXPLAIN mirrors that exactly.
fn dml_target<'a>(q: &'a Q, table: &str) -> Result<&'a Table, ExecError> {
    q.eng
        .db
        .find_table(table, q.snap, &q.all_xids, q.session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))
}

/// Validate the CTE definitions a DML statement carries (PG19 analyzes
/// WITH bodies at their definition level; data-modifying bodies are
/// validated by the DML validators recursively).
fn vx_cte_defs(q: &mut Q, ctx: &StmtCtx, defs: &[CteDef]) -> Result<(), ExecError> {
    for (i, def) in defs.iter().enumerate() {
        let outer: Vec<CteDef> = defs.iter().take(i).cloned().collect();
        match &def.body {
            CteBody::Simple(s) => {
                let mut visiting = Vec::new();
                vx_select(q, s, &outer, &[], 1, &mut visiting)?;
            }
            CteBody::Union { left, right, .. } => {
                let mut visiting = Vec::new();
                vx_select(q, left, &outer, &[], 1, &mut visiting)?;
                vx_select(q, right, &outer, &[], 1, &mut visiting)?;
            }
            CteBody::Dml(inner) => {
                validate_explain_stmt(q, ctx, inner)?;
            }
        }
    }
    Ok(())
}

/// The target relation's column schema as a `vx_expr` scope, qualified
/// by the name (or alias) expressions use.
fn target_schema(tcols: &[(String, ColType)], qual: &str) -> Vec<QCol> {
    tcols
        .iter()
        .enumerate()
        .map(|(i, (n, ty))| QCol {
            qual: qual.to_string(),
            name: n.clone(),
            ty: *ty,
            hidden: false,
            src_ord: i as u32,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn vx_insert(
    q: &mut Q,
    ctx: &StmtCtx,
    table: &str,
    columns: Option<&Vec<InsertTarget>>,
    rows: &[Vec<InsertValue>],
    select: Option<&SelectStmt>,
    with: &[CteDef],
    on_conflict: Option<&OnConflict>,
    returning: &[SelectItem],
) -> Result<(), ExecError> {
    // Existence first (PG19 parse analysis opens the target relation
    // before any ACL check runs). The Table is cloned out as owned
    // metadata: the vx pass below needs `&mut q`.
    let meta = TableMeta::of(dml_target(q, table)?);
    let tcols = meta.columns.clone();
    // Privileges, in exec_insert's exact order and shape.
    if let Some(targets) = columns {
        let names: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
        require_column_privs(
            &*q.eng,
            ctx,
            table,
            &names,
            crate::storage::PRIV_INSERT,
            "INSERT",
        )?;
    } else {
        require_table_priv(&*q.eng, ctx, table, crate::storage::PRIV_INSERT, "INSERT")?;
    }
    vx_cte_defs(q, ctx, with)?;
    // Duplicate whole-column targets are 42701 (mirrors exec_insert).
    if let Some(targets) = columns {
        let mut whole = std::collections::HashSet::new();
        let mut partial = std::collections::HashSet::new();
        for t in targets {
            if t.indirection.is_empty() {
                if !whole.insert(&t.name) || partial.contains(&t.name) {
                    return Err(exec_err(
                        "42701",
                        format!("column \"{}\" specified more than once", t.name),
                    ));
                }
            } else if whole.contains(&t.name) {
                return Err(exec_err(
                    "42701",
                    format!("column \"{}\" specified more than once", t.name),
                ));
            } else {
                partial.insert(&t.name);
            }
        }
        // Target columns must exist (PG19 transformInsertStatement:
        // "column \"x\" of relation \"t\" does not exist").
        for t in targets {
            if meta.column_index(&t.name).is_none() {
                return Err(exec_err(
                    "42703",
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        t.name, table
                    ),
                ));
            }
        }
    }
    // VALUES arity (PG19: checked during parse analysis).
    let n_targets = match columns {
        Some(targets) => targets.len(),
        None => meta.columns.len(),
    };
    for row in rows {
        if row.len() > n_targets {
            return Err(exec_err(
                "42601",
                "INSERT has more expressions than target columns".to_string(),
            ));
        }
        if row.len() < n_targets {
            return Err(exec_err(
                "42601",
                "INSERT has more target columns than expressions".to_string(),
            ));
        }
    }
    // Source: VALUES expressions resolve in an empty scope (their
    // subqueries carry their own levels via vx_expr); an INSERT..SELECT
    // is a full query level.
    let no_refs: Vec<&[QCol]> = Vec::new();
    for row in rows {
        for v in row {
            if let InsertValue::Expr(e) = v {
                let mut visiting = Vec::new();
                vx_expr(q, e, &no_refs, &[], with, 1, &mut visiting)?;
            }
        }
    }
    if let Some(targets) = columns {
        for t in targets {
            for step in &t.indirection {
                if let InsertIndirection::Index(exprs) = step {
                    for e in exprs {
                        let mut visiting = Vec::new();
                        vx_expr(q, e, &no_refs, &[], with, 1, &mut visiting)?;
                    }
                }
            }
        }
    }
    if let Some(sel) = select {
        // The statement's WITH list is visible to the source query.
        let mut owned = sel.clone();
        if !with.is_empty() {
            let mut w = with.to_vec();
            w.extend(owned.with.iter().cloned());
            owned.with = w;
        }
        validate_explain_select(q, &owned)?;
        // Source/target arity (PG19 parse analysis).
        let cols = describe_select_outer(
            &*q.eng,
            q.snap,
            q.own,
            q.session,
            &owned,
            &owned.with.clone(),
            &[],
            &[],
        )?;
        if cols.len() > n_targets {
            return Err(exec_err(
                "42601",
                "INSERT has more expressions than target columns".to_string(),
            ));
        }
        if cols.len() < n_targets {
            return Err(exec_err(
                "42601",
                "INSERT has more target columns than expressions".to_string(),
            ));
        }
    }
    // ON CONFLICT: arbiter inference and action analysis.
    if let Some(oc) = on_conflict {
        plan_upsert(&*q.eng, ctx, table, oc, &meta)?;
        // Read-privilege shape mirrors exec_insert (DO UPDATE SET
        // columns need UPDATE; reads need SELECT).
        let returning_exprs: Vec<&Expr> = returning
            .iter()
            .filter_map(|i| match i {
                SelectItem::Expr { expr, .. } => Some(expr),
                _ => None,
            })
            .collect();
        match &oc.action {
            ConflictAction::DoUpdate { sets, where_ } => {
                require_column_privs(
                    &*q.eng,
                    ctx,
                    table,
                    &sets.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                    crate::storage::PRIV_UPDATE,
                    "UPDATE",
                )?;
                let mut exprs: Vec<&Expr> = sets.iter().map(|(_, e)| e).collect();
                if let Some(w) = where_ {
                    exprs.push(w);
                }
                exprs.extend(returning_exprs.iter().copied());
                if !exprs.is_empty() {
                    check_dml_read_cols(&*q.eng, ctx, table, None, &exprs, &[])?;
                }
                // Expression resolution over target + EXCLUDED scopes.
                let tschema = target_schema(&tcols, table.rsplit('.').next().unwrap_or(table));
                let eschema = target_schema(&tcols, "excluded");
                let refs: Vec<&[QCol]> = vec![tschema.as_slice(), eschema.as_slice()];
                for (_, e) in sets {
                    let mut visiting = Vec::new();
                    vx_expr(q, e, &refs, &[], with, 1, &mut visiting)?;
                }
                if let Some(w) = where_ {
                    let mut visiting = Vec::new();
                    vx_expr(q, w, &refs, &[], with, 1, &mut visiting)?;
                    vx_bool_clause(q, w, &refs, &[], with, "WHERE")?;
                }
            }
            ConflictAction::DoSelect {
                where_, for_update, ..
            } => {
                if returning.is_empty() {
                    return Err(exec_err(
                        "42601",
                        "ON CONFLICT DO SELECT requires a RETURNING clause".to_string(),
                    ));
                }
                if *for_update {
                    let t = dml_target(q, table)?;
                    let closure =
                        crate::storage::role_closure(&q.eng.db, ctx.role, ctx.snap, ctx.own);
                    let ok = crate::storage::table_privs_in(
                        &q.eng.db, ctx.role, t, &closure, ctx.snap, ctx.own,
                    ) & crate::storage::PRIV_UPDATE
                        == crate::storage::PRIV_UPDATE
                        || t.columns.iter().any(|(cn, _)| {
                            crate::storage::column_privs_in(
                                &q.eng.db, ctx.role, t, cn, &closure, ctx.snap, ctx.own,
                            ) & crate::storage::PRIV_UPDATE
                                == crate::storage::PRIV_UPDATE
                        });
                    if !ok {
                        return Err(exec_err(
                            "42501",
                            format!(
                                "permission denied for table \"{}\"",
                                table.rsplit('.').next().unwrap_or(table)
                            ),
                        ));
                    }
                }
                let mut exprs: Vec<&Expr> = Vec::new();
                if let Some(w) = where_ {
                    exprs.push(w);
                }
                exprs.extend(returning_exprs.iter().copied());
                if !exprs.is_empty() {
                    check_dml_read_cols(&*q.eng, ctx, table, None, &exprs, &[])?;
                }
                if let Some(w) = where_ {
                    let tschema = target_schema(&tcols, table.rsplit('.').next().unwrap_or(table));
                    let eschema = target_schema(&tcols, "excluded");
                    let refs: Vec<&[QCol]> = vec![tschema.as_slice(), eschema.as_slice()];
                    let mut visiting = Vec::new();
                    vx_expr(q, w, &refs, &[], with, 1, &mut visiting)?;
                }
            }
            ConflictAction::DoNothing => {}
        }
    }
    // RETURNING expressions resolve against the target row.
    if !returning.is_empty() {
        let tschema = target_schema(&tcols, table.rsplit('.').next().unwrap_or(table));
        let refs: Vec<&[QCol]> = vec![tschema.as_slice()];
        for item in returning {
            if let SelectItem::Expr { expr, .. } = item {
                let mut visiting = Vec::new();
                vx_expr(q, expr, &refs, &[], with, 1, &mut visiting)?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn vx_update(
    q: &mut Q,
    ctx: &StmtCtx,
    table: &str,
    alias: Option<&str>,
    sets: &[(String, Expr)],
    from: &[FromItem],
    where_: Option<&Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
) -> Result<(), ExecError> {
    let meta = TableMeta::of(dml_target(q, table)?);
    // Privileges in exec_update's order: UPDATE on the SET columns,
    // then SELECT on every column the SET/WHERE/RETURNING expressions
    // read on the target.
    require_column_privs(
        &*q.eng,
        ctx,
        table,
        &sets.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        crate::storage::PRIV_UPDATE,
        "UPDATE",
    )?;
    {
        let mut exprs: Vec<&Expr> = sets.iter().map(|(_, e)| e).collect();
        if let Some(w) = where_ {
            exprs.push(w);
        }
        exprs.extend(returning.iter().filter_map(|i| match i {
            SelectItem::Expr { expr, .. } => Some(expr),
            _ => None,
        }));
        check_dml_read_cols(&*q.eng, ctx, table, alias, &exprs, &[])?;
    }
    // Duplicate SET targets are 42701; unknown SET targets 42703
    // (mirrors exec_update / PG19 parse analysis).
    {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in sets {
            if !seen.insert(name) {
                return Err(exec_err(
                    "42701",
                    format!("multiple assignments to same column \"{}\"", name),
                ));
            }
            if meta.column_index(name).is_none() {
                return Err(exec_err(
                    "42703",
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        name, table
                    ),
                ));
            }
        }
    }
    // The target relation may not reappear in FROM (42712, mirrors
    // exec_update).
    if !from.is_empty() {
        fn collect_table_names(item: &FromItem, out: &mut Vec<String>) {
            match item {
                FromItem::Table { name, .. } => {
                    out.push(name.rsplit('.').next().unwrap_or(name).to_string());
                }
                FromItem::Join { left, right, .. } => {
                    collect_table_names(left, out);
                    collect_table_names(right, out);
                }
                _ => {}
            }
        }
        let target = table.rsplit('.').next().unwrap_or(table);
        let mut names = Vec::new();
        for item in from {
            collect_table_names(item, &mut names);
        }
        for from_name in names {
            if from_name.eq_ignore_ascii_case(target) {
                return Err(exec_err(
                    "42712",
                    format!("table name \"{}\" specified more than once", from_name),
                ));
            }
        }
    }
    vx_cte_defs(q, ctx, with)?;
    // Expression resolution over a synthesized SELECT level (target +
    // FROM items in scope; SET and RETURNING expressions as targets).
    let mut synth = scan_select(table, alias, from, where_, with);
    synth.items = sets
        .iter()
        .map(|(_, e)| SelectItem::Expr {
            expr: e.clone(),
            alias: None,
        })
        .chain(returning.iter().cloned())
        .collect();
    validate_explain_select(q, &synth)?;
    // The WHERE clause is boolean-typed; validate_explain_select has
    // already applied that check to the synthesized level's WHERE.
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn vx_delete(
    q: &mut Q,
    ctx: &StmtCtx,
    table: &str,
    alias: Option<&str>,
    using: &[FromItem],
    where_: Option<&Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
) -> Result<(), ExecError> {
    dml_target(q, table)?;
    // Privileges in exec_delete's order: DELETE on the table, then
    // SELECT on the columns the WHERE/RETURNING expressions read.
    require_table_priv(&*q.eng, ctx, table, crate::storage::PRIV_DELETE, "DELETE")?;
    {
        let mut exprs: Vec<&Expr> = Vec::new();
        if let Some(w) = where_ {
            exprs.push(w);
        }
        exprs.extend(returning.iter().filter_map(|i| match i {
            SelectItem::Expr { expr, .. } => Some(expr),
            _ => None,
        }));
        if !exprs.is_empty() {
            check_dml_read_cols(&*q.eng, ctx, table, alias, &exprs, &[])?;
        }
    }
    vx_cte_defs(q, ctx, with)?;
    let mut synth = scan_select(table, alias, using, where_, with);
    synth.items = returning.to_vec();
    validate_explain_select(q, &synth)?;
    Ok(())
}

/// v2.01: EXPLAIN analysis for MERGE — mirrors exec_merge's own checks
/// (privileges in executor order, SET/INSERT column existence and
/// duplicates, target-in-source), then resolves every expression
/// position over a synthesized SELECT level (target + source ranges,
/// ON as the level's WHERE). RETURNING items that use the PG17-only
/// merge_action()/old/new forms are skipped here (their resolution is
/// the executor's describe path, which EXPLAIN ANALYZE exercises by
/// executing).
#[allow(clippy::too_many_arguments)]
fn vx_merge(
    q: &mut Q,
    ctx: &StmtCtx,
    table: &str,
    alias: Option<&str>,
    source: &[FromItem],
    on: &Expr,
    whens: &[MergeWhen],
    returning: &[SelectItem],
    with: &[CteDef],
) -> Result<(), ExecError> {
    let meta = TableMeta::of(dml_target(q, table)?);
    if whens
        .iter()
        .any(|w| matches!(w.action, MergeAction::Delete))
    {
        require_table_priv(&*q.eng, ctx, table, crate::storage::PRIV_DELETE, "DELETE")?;
    }
    for w in whens {
        match &w.action {
            MergeAction::Update { sets } => {
                require_column_privs(
                    &*q.eng,
                    ctx,
                    table,
                    &sets.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                    crate::storage::PRIV_UPDATE,
                    "UPDATE",
                )?;
                let mut seen = std::collections::HashSet::new();
                for (name, _) in sets {
                    if !seen.insert(name) {
                        return Err(exec_err(
                            "42701",
                            format!("multiple assignments to same column \"{}\"", name),
                        ));
                    }
                    if meta.column_index(name).is_none() {
                        return Err(exec_err(
                            "42703",
                            format!(
                                "column \"{}\" of relation \"{}\" does not exist",
                                name, table
                            ),
                        ));
                    }
                }
            }
            MergeAction::Insert { columns, .. } => {
                if let Some(cols) = columns {
                    require_column_privs(
                        &*q.eng,
                        ctx,
                        table,
                        cols,
                        crate::storage::PRIV_INSERT,
                        "INSERT",
                    )?;
                    for c in cols {
                        if meta.column_index(c).is_none() {
                            return Err(exec_err(
                                "42703",
                                format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    c, table
                                ),
                            ));
                        }
                    }
                } else {
                    require_table_priv(&*q.eng, ctx, table, crate::storage::PRIV_INSERT, "INSERT")?;
                }
            }
            _ => {}
        }
    }
    {
        let mut exprs: Vec<&Expr> = vec![on];
        for w in whens {
            if let Some(c) = &w.condition {
                exprs.push(c);
            }
            if let MergeAction::Update { sets } = &w.action {
                exprs.extend(sets.iter().map(|(_, e)| e));
            }
        }
        exprs.extend(returning.iter().filter_map(|i| match i {
            SelectItem::Expr { expr, .. } => Some(expr),
            _ => None,
        }));
        check_dml_read_cols(&*q.eng, ctx, table, alias, &exprs, &[])?;
    }
    // Target in source (mirrors exec_merge).
    {
        fn collect_table_names(item: &FromItem, out: &mut Vec<String>) {
            match item {
                FromItem::Table { name, .. } => {
                    out.push(name.rsplit('.').next().unwrap_or(name).to_string());
                }
                FromItem::Join { left, right, .. } => {
                    collect_table_names(left, out);
                    collect_table_names(right, out);
                }
                _ => {}
            }
        }
        let target = table.rsplit('.').next().unwrap_or(table);
        let mut names = Vec::new();
        for item in source {
            collect_table_names(item, &mut names);
        }
        for n in names {
            if n.eq_ignore_ascii_case(target) {
                return Err(exec_err(
                    "42712",
                    format!("name \"{}\" specified more than once", target),
                ));
            }
        }
    }
    vx_cte_defs(q, ctx, with)?;
    let mut synth = scan_select(table, alias, source, Some(on), with);
    let mut items: Vec<SelectItem> = Vec::new();
    for w in whens {
        if let MergeAction::Update { sets } = &w.action {
            items.extend(sets.iter().map(|(_, e)| SelectItem::Expr {
                expr: e.clone(),
                alias: None,
            }));
        }
        if let Some(c) = &w.condition {
            items.push(SelectItem::Expr {
                expr: c.clone(),
                alias: None,
            });
        }
        if let MergeAction::Insert { values, .. } = &w.action {
            if let Some(vs) = values {
                items.extend(vs.iter().map(|e| SelectItem::Expr {
                    expr: e.clone(),
                    alias: None,
                }));
            }
        }
    }
    for item in returning {
        if let SelectItem::Expr { expr, .. } = item {
            // Skip PG17 RETURNING-only forms (merge_action / old / new).
            let mut refs: Vec<(Option<String>, String)> = Vec::new();
            crate::sql::collect_col_refs(expr, &mut refs);
            let special = refs.iter().any(|(q, n)| {
                q.as_deref() == Some("old")
                    || q.as_deref() == Some("new")
                    || (q.is_none() && (n == "old" || n == "new"))
            }) || matches!(expr, Expr::Func { name, .. } if name.eq_ignore_ascii_case("merge_action"));
            if !special {
                items.push(item.clone());
            }
        }
    }
    synth.items = items;
    validate_explain_select(q, &synth)?;
    Ok(())
}

/// The scan shape of an UPDATE/DELETE as a SELECT level: the target
/// relation (under its alias) followed by the FROM/USING items, with
/// the statement's WHERE and WITH. Used both for validation scopes
/// and for the child plan under the ModifyTable header.
pub(crate) fn scan_select(
    table: &str,
    alias: Option<&str>,
    extra_from: &[FromItem],
    where_: Option<&Expr>,
    with: &[CteDef],
) -> SelectStmt {
    let mut synth = empty_select();
    synth.with = with.to_vec();
    let mut from = vec![FromItem::Table {
        name: table.to_string(),
        alias: alias.map(|a| a.to_string()),
        col_aliases: Vec::new(),
        only: false,
    }];
    from.extend(extra_from.iter().cloned());
    synth.from = from;
    synth.where_ = where_.cloned();
    synth
}

// ===========================================================================
// Plan rendering
// ===========================================================================

/// PG19's `Conflict Resolution:` text for an ON CONFLICT action
/// (explain.c `show_modifytable_info`).
fn conflict_resolution(oc: &OnConflict) -> &'static str {
    match &oc.action {
        ConflictAction::DoNothing => "NOTHING",
        ConflictAction::DoUpdate { .. } => "UPDATE",
        ConflictAction::DoSelect { strength, .. } => match strength {
            1 => "SELECT FOR KEY SHARE",
            2 => "SELECT FOR SHARE",
            3 => "SELECT FOR NO KEY UPDATE",
            4 => "SELECT FOR UPDATE",
            _ => "SELECT",
        },
    }
}

/// The `Conflict Arbiter Indexes:` list for the plan display (PG19
/// `infer_arbiter_indexes`): for an explicit inference specification,
/// EVERY unique index the satisfaction rule accepts (opclass /
/// collation variants are distinct indexes over the same columns),
/// in catalog (creation) order; for ON CONSTRAINT, the backing
/// index. Inference errors themselves are `plan_upsert`'s job (run
/// in validation); an empty result here renders no line.
fn explain_arbiter_indexes(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    oc: &OnConflict,
    meta: &TableMeta,
) -> Vec<String> {
    match &oc.arbiter {
        ConflictArbiter::None => Vec::new(),
        ConflictArbiter::Constraint(_) => match plan_upsert(eng, ctx, table, oc, meta) {
            Ok(plan) => plan.indexes.iter().map(|(n, _)| n.clone()).collect(),
            Err(_) => Vec::new(),
        },
        ConflictArbiter::Columns(cols) => {
            if eng.db.is_temp_table(ctx.session, table) {
                // Temp tables arbitrate on constraint defs (no Index
                // objects); plan_upsert resolved the single match.
                return match plan_upsert(eng, ctx, table, oc, meta) {
                    Ok(plan) => plan.indexes.iter().map(|(n, _)| n.clone()).collect(),
                    Err(_) => Vec::new(),
                };
            }
            let mut matched: Vec<(u64, String)> = eng
                .db
                .visible_indexes_for(table, ctx.snap, &ctx.all_xids, ctx.session)
                .into_iter()
                .filter(|ix| ix.def.unique)
                .filter(|ix| {
                    arbiter_elems_match(
                        &ix.def.col_names,
                        &ix.def.collates,
                        &ix.def.opclasses,
                        cols,
                    )
                })
                .map(|ix| (ix.def.created_xmin, ix.def.name.clone()))
                .collect();
            // PG19 lists arbiters in catalog order (creation order);
            // created_xmin is this engine's creation sequence proxy.
            matched.sort();
            matched.into_iter().map(|(_, n)| n).collect()
        }
    }
}

/// Header + property lines of a ModifyTable node (planning form).
/// `depth` is the node's depth (0 at top level); properties sit at
/// the node's property indent, the child subtree renders at depth+1
/// through the normal renderers.
fn modify_header(
    label: &str,
    table: &str,
    alias: Option<&str>,
    oc: Option<&OnConflict>,
    eng: &Engine,
    ctx: &StmtCtx,
    meta: Option<&TableMeta>,
    depth: usize,
    out: &mut Vec<String>,
) {
    let pad = pg_pad(depth);
    let name = match alias {
        Some(a) if a != table => format!("{} {}", table, pg_quote_ident(a)),
        _ => table.to_string(),
    };
    out.push(format!("{pad}{label} on {name}"));
    if let (Some(oc), Some(meta)) = (oc, meta) {
        let ppad = pg_ppad(depth);
        out.push(format!(
            "{ppad}Conflict Resolution: {}",
            conflict_resolution(oc)
        ));
        let arbiters = explain_arbiter_indexes(eng, ctx, table, oc, meta);
        if !arbiters.is_empty() {
            out.push(format!(
                "{ppad}Conflict Arbiter Indexes: {}",
                arbiters.join(", ")
            ));
        }
    }
}

/// Plan one INSERT: header (+ conflict lines) over a Result (single
/// VALUES row), a Values Scan (multi-row VALUES), or the source
/// query's plan (INSERT..SELECT).
#[allow(clippy::too_many_arguments)]
fn insert_plan(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    rows: &[Vec<InsertValue>],
    select: Option<&SelectStmt>,
    with: &[CteDef],
    on_conflict: Option<&OnConflict>,
    costs: bool,
    verbose: bool,
    out: &mut Vec<String>,
) -> Result<(), ExecError> {
    let meta = {
        let t = eng
            .db
            .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        TableMeta::of(t)
    };
    if costs {
        // Legacy (COSTS ON) shape, consistent with render_plan_costs_on.
        let n = if select.is_some() {
            0
        } else {
            rows.len() as u64
        };
        out.push(format!("Insert on {table} (rows={n})"));
        if let Some(oc) = on_conflict {
            out.push(format!(
                "  Conflict Resolution: {}",
                conflict_resolution(oc)
            ));
        }
    } else {
        modify_header(
            "Insert",
            table,
            None,
            on_conflict,
            eng,
            ctx,
            Some(&meta),
            0,
            out,
        );
    }
    match select {
        Some(sel) => {
            let mut owned = sel.clone();
            if !with.is_empty() {
                let mut w = with.to_vec();
                w.extend(owned.with.iter().cloned());
                owned.with = w;
            }
            let plan = plan_select(
                eng,
                &owned,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                verbose,
            )?;
            render_plan(&plan, 1, costs, verbose, out);
        }
        None => {
            if rows.len() > 1 && !costs {
                out.push(format!("{}Values Scan on \"*VALUES*\"", pg_pad(1)));
            } else {
                let node = PlanNode::Result {
                    rows: rows.len() as u64,
                    filter: None,
                    output: Vec::new(),
                    one_time_filter: false,
                    replaces: None,
                };
                render_plan(&node, 1, costs, verbose, out);
            }
        }
    }
    Ok(())
}

/// Plan an UPDATE or DELETE: header over the scan plan of the
/// statement's synthesized SELECT shape.
#[allow(clippy::too_many_arguments)]
fn scan_dml_plan(
    eng: &Engine,
    ctx: &StmtCtx,
    label: &str,
    table: &str,
    alias: Option<&str>,
    extra_from: &[FromItem],
    where_: Option<&Expr>,
    with: &[CteDef],
    costs: bool,
    verbose: bool,
    out: &mut Vec<String>,
) -> Result<(), ExecError> {
    if costs {
        out.push(format!("{label} on {table} (rows=0)"));
    } else {
        modify_header(label, table, alias, None, eng, ctx, None, 0, out);
    }
    let mut synth = scan_select(table, alias, extra_from, where_, with);
    synth.items = vec![SelectItem::All];
    let plan = plan_select(
        eng,
        &synth,
        ctx.snap,
        ctx.own,
        ctx.session,
        &[],
        !costs,
        verbose,
    )?;
    render_plan(&plan, 1, costs, verbose, out);
    Ok(())
}

/// CTAS-family existence gate (PG19 createas.c
/// CreateTableAsRelExists, reached from ExplainOneUtility for both
/// planning and ANALYZE): an existing target errors 42P07, or — with
/// IF NOT EXISTS — produces a dummy (empty) plan. Returns true when
/// the caller should emit the empty plan.
fn ctas_exists_gate(
    eng: &Engine,
    ctx: &StmtCtx,
    name: &str,
    if_not_exists: bool,
) -> Result<bool, ExecError> {
    let exists = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .is_some()
        || eng.db.find_view(name, ctx.snap, ctx.own).is_some();
    if !exists {
        return Ok(false);
    }
    if if_not_exists {
        return Ok(true);
    }
    Err(exec_err(
        "42P07",
        format!("relation \"{}\" already exists", name),
    ))
}

/// The inner SELECT of a CREATE MATERIALIZED VIEW / REFRESH target:
/// the view's stored query text, parsed.
fn matview_select(eng: &Engine, ctx: &StmtCtx, name: &str) -> Result<SelectStmt, ExecError> {
    let view = eng
        .db
        .find_view(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?;
    match parse_statement(&view.query) {
        Ok(Stmt::Select(sel)) => Ok(sel),
        _ => Err(exec_err(
            "0A000",
            "materialized view query is not a SELECT".to_string(),
        )),
    }
}

/// v1.99: planning-only EXPLAIN lines for a non-SELECT statement
/// (validation has already run in `exec_explain`).
pub(crate) fn explain_dml_plan(
    eng: &Engine,
    ctx: &StmtCtx,
    stmt: &Stmt,
    costs: bool,
    verbose: bool,
) -> Result<Vec<String>, ExecError> {
    let mut out: Vec<String> = Vec::new();
    match stmt {
        Stmt::Insert {
            table,
            rows,
            select,
            with,
            on_conflict,
            ..
        } => insert_plan(
            eng,
            ctx,
            table,
            rows,
            select.as_ref(),
            with,
            on_conflict.as_ref(),
            costs,
            verbose,
            &mut out,
        )?,
        Stmt::Update {
            table,
            alias,
            from,
            where_,
            with,
            ..
        } => scan_dml_plan(
            eng,
            ctx,
            "Update",
            table,
            alias.as_deref(),
            from,
            where_.as_ref(),
            with,
            costs,
            verbose,
            &mut out,
        )?,
        Stmt::Delete {
            table,
            alias,
            using,
            where_,
            with,
            ..
        } => scan_dml_plan(
            eng,
            ctx,
            "Delete",
            table,
            alias.as_deref(),
            using,
            where_.as_ref(),
            with,
            costs,
            verbose,
            &mut out,
        )?,
        Stmt::Merge {
            table,
            alias,
            source,
            on,
            with,
            ..
        } => scan_dml_plan(
            eng,
            ctx,
            "Merge",
            table,
            alias.as_deref(),
            source,
            Some(on),
            with,
            costs,
            verbose,
            &mut out,
        )?,
        Stmt::CreateTableAs {
            name,
            select,
            if_not_exists,
            ..
        } => {
            if ctas_exists_gate(eng, ctx, name, *if_not_exists)? {
                return Ok(out);
            }
            let plan = plan_select(
                eng,
                select,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                verbose,
            )?;
            render_plan(&plan, 0, costs, verbose, &mut out);
        }
        Stmt::CreateView {
            name,
            query,
            matview,
            ..
        } => {
            // A matview whose name is taken: PG19 errors 42P07 from
            // parse analysis of the CreateAsStmt (no OR REPLACE for
            // materialized views in PG19).
            if *matview
                && (eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .is_some()
                    || eng.db.find_view(name, ctx.snap, ctx.own).is_some())
            {
                return Err(exec_err(
                    "42P07",
                    format!("relation \"{}\" already exists", name),
                ));
            }
            let sel = match parse_statement(query) {
                Ok(Stmt::Select(sel)) => sel,
                _ => {
                    return Err(exec_err(
                        "0A000",
                        "materialized view query is not a SELECT".to_string(),
                    ));
                }
            };
            let plan = plan_select(
                eng,
                &sel,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                verbose,
            )?;
            render_plan(&plan, 0, costs, verbose, &mut out);
        }
        Stmt::RefreshMatView { name, .. } => {
            let sel = matview_select(eng, ctx, name)?;
            let plan = plan_select(
                eng,
                &sel,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                verbose,
            )?;
            render_plan(&plan, 0, costs, verbose, &mut out);
        }
        Stmt::Declare { query, .. } => {
            let plan = plan_select(
                eng,
                query,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                verbose,
            )?;
            render_plan(&plan, 0, costs, verbose, &mut out);
        }
        _ => {
            return Err(exec_err(
                "0A000",
                "EXPLAIN of this statement is not supported".to_string(),
            ));
        }
    }
    Ok(out)
}

// ===========================================================================
// Entry points used by exec_explain / exec_explain_analyze
// ===========================================================================

fn explain_result(lines: Vec<String>) -> ExecResult {
    ExecResult::Explain {
        columns: vec![("QUERY PLAN".to_string(), ColType::Text)],
        rows: lines
            .into_iter()
            .map(|l| Row::new(vec![Value::text(l)]))
            .collect(),
    }
}

/// v1.99: planning-only EXPLAIN of a non-SELECT statement: validate
/// (the v1.98 pass, generalized), then render the plan.
pub(crate) fn exec_explain_dml(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    stmt: &Stmt,
    costs: bool,
    verbose: bool,
) -> Result<ExecResult, ExecError> {
    {
        let mut lock_ids: Vec<(String, u64)> = Vec::new();
        let mut q = explain_q(eng, ctx, &mut lock_ids);
        validate_explain_stmt(&mut q, ctx, stmt)?;
    }
    let lines = explain_dml_plan(eng, ctx, stmt, costs, verbose)?;
    Ok(explain_result(lines))
}

/// Rows a DML/CTAS execution reports: the command tag's count, or the
/// RETURNING row count.
fn affected_of(res: &ExecResult) -> u64 {
    match res {
        ExecResult::Command { tag } | ExecResult::Dml { tag, .. } => tag
            .rsplit(' ')
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0),
        ExecResult::Select { rows, .. } => rows.len() as u64,
        _ => 0,
    }
}

/// Render a plan tree as PG19 renders a query that was planned but
/// never run (EXPLAIN ANALYZE ... WITH NO DATA): every node line gets
/// `(never executed)`, property lines are unchanged.
fn render_never_executed(node: &PlanNode, depth: usize, out: &mut Vec<String>) {
    let mut tmp = Vec::new();
    render_plan(node, depth, false, false, &mut tmp);
    for line in tmp {
        if line.contains("->  ") || !line.starts_with(' ') {
            out.push(format!("{line} (never executed)"));
        } else {
            out.push(line);
        }
    }
}

/// The AnalyzeCtx for a DML child tree whose top node produced
/// `top_rows` rows.
fn dml_ax<'a>(eng: &'a Engine, ctx: &StmtCtx<'a>, top_rows: u64) -> AnalyzeCtx<'a> {
    AnalyzeCtx {
        eng,
        snap: ctx.snap,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        top_rows,
    }
}

/// v1.99: EXPLAIN ANALYZE of a non-SELECT statement: validate, plan,
/// execute through the real executor (PG semantics — the change
/// happens), and render actual row counts.
pub(crate) fn exec_explain_dml_analyze(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    stmt: &Stmt,
    costs: bool,
) -> Result<ExecResult, ExecError> {
    {
        let mut lock_ids: Vec<(String, u64)> = Vec::new();
        let mut q = explain_q(eng, ctx, &mut lock_ids);
        validate_explain_stmt(&mut q, ctx, stmt)?;
    }
    let mut out: Vec<String> = Vec::new();
    match stmt {
        Stmt::Insert {
            table,
            columns,
            rows,
            select,
            with,
            on_conflict,
            returning,
        } => {
            // Plan the child first (PG plans before executing).
            let child: Option<PlanNode> = match select {
                Some(sel) => {
                    let mut owned = sel.clone();
                    if !with.is_empty() {
                        let mut w = with.clone();
                        w.extend(owned.with.iter().cloned());
                        owned.with = w;
                    }
                    Some(plan_select(
                        eng,
                        &owned,
                        ctx.snap,
                        ctx.own,
                        ctx.session,
                        &[],
                        !costs,
                        false,
                    )?)
                }
                None => None,
            };
            let meta = {
                let t = eng
                    .db
                    .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
                    .ok_or_else(|| {
                        exec_err("42P01", format!("relation \"{}\" does not exist", table))
                    })?;
                TableMeta::of(t)
            };
            let res = exec_insert(
                eng,
                ctx,
                table,
                columns,
                rows,
                select,
                with,
                on_conflict,
                returning,
            )?;
            let affected = affected_of(&res);
            out.push(format!(
                "Insert on {table} (actual rows={:.2} loops=1)",
                affected as f64
            ));
            if let Some(oc) = on_conflict {
                out.push(format!(
                    "   Conflict Resolution: {}",
                    conflict_resolution(oc)
                ));
                let arbiters = explain_arbiter_indexes(eng, ctx, table, oc, &meta);
                if !arbiters.is_empty() {
                    out.push(format!(
                        "   Conflict Arbiter Indexes: {}",
                        arbiters.join(", ")
                    ));
                }
            }
            match child {
                Some(plan) => {
                    let ax = dml_ax(eng, ctx, affected);
                    render_analyze(&plan, &ax, 1, None, true, &mut out);
                }
                None => {
                    if rows.len() > 1 {
                        out.push(format!(
                            "{}Values Scan on \"*VALUES*\" (actual rows={:.2} loops=1)",
                            analyze_pad(1),
                            rows.len() as f64
                        ));
                    } else {
                        let node = PlanNode::Result {
                            rows: rows.len() as u64,
                            filter: None,
                            output: Vec::new(),
                            one_time_filter: false,
                            replaces: None,
                        };
                        let ax = dml_ax(eng, ctx, rows.len() as u64);
                        render_analyze(&node, &ax, 1, None, true, &mut out);
                    }
                }
            }
        }
        Stmt::Update {
            table,
            alias,
            sets,
            from,
            where_,
            with,
            returning,
        } => {
            let mut synth = scan_select(table, alias.as_deref(), from, where_.as_ref(), with);
            synth.items = vec![SelectItem::All];
            let plan = plan_select(
                eng,
                &synth,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                false,
            )?;
            let res = exec_update(eng, ctx, table, alias, sets, from, where_, with, returning)?;
            let affected = affected_of(&res);
            out.push(format!(
                "Update on {table} (actual rows={:.2} loops=1)",
                affected as f64
            ));
            let ax = dml_ax(eng, ctx, affected);
            render_analyze(&plan, &ax, 1, None, true, &mut out);
        }
        Stmt::Delete {
            table,
            alias,
            using,
            where_,
            with,
            returning,
        } => {
            let mut synth = scan_select(table, alias.as_deref(), using, where_.as_ref(), with);
            synth.items = vec![SelectItem::All];
            let plan = plan_select(
                eng,
                &synth,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                false,
            )?;
            let res = exec_delete(eng, ctx, table, alias, using, where_, with, returning)?;
            let affected = affected_of(&res);
            out.push(format!(
                "Delete on {table} (actual rows={:.2} loops=1)",
                affected as f64
            ));
            let ax = dml_ax(eng, ctx, affected);
            render_analyze(&plan, &ax, 1, None, true, &mut out);
        }
        Stmt::Merge {
            table,
            alias,
            source,
            on,
            whens,
            returning,
            with,
        } => {
            let mut synth = scan_select(table, alias.as_deref(), source, Some(on), with);
            synth.items = vec![SelectItem::All];
            let plan = plan_select(
                eng,
                &synth,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                false,
            )?;
            let res = exec_merge(eng, ctx, table, alias, source, on, whens, returning, with)?;
            let affected = affected_of(&res);
            out.push(format!(
                "Merge on {table} (actual rows={:.2} loops=1)",
                affected as f64
            ));
            let ax = dml_ax(eng, ctx, affected);
            render_analyze(&plan, &ax, 1, None, true, &mut out);
        }
        Stmt::CreateTableAs {
            name,
            col_aliases,
            select,
            temp,
            if_not_exists,
            with_data,
            ..
        } => {
            if ctas_exists_gate(eng, ctx, name, *if_not_exists)? {
                return Ok(explain_result(out));
            }
            let plan = plan_select(
                eng,
                select,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                false,
            )?;
            let res = exec_create_table_as(
                eng,
                ctx,
                name,
                col_aliases,
                select,
                *temp,
                *if_not_exists,
                *with_data,
            )?;
            if *with_data {
                let n = affected_of(&res);
                let ax = dml_ax(eng, ctx, n);
                render_analyze(&plan, &ax, 0, None, true, &mut out);
            } else {
                render_never_executed(&plan, 0, &mut out);
            }
        }
        Stmt::CreateView {
            name,
            query,
            col_aliases,
            or_replace,
            security_invoker,
            check_option,
            matview,
            with_data,
        } => {
            if *matview
                && (eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .is_some()
                    || eng.db.find_view(name, ctx.snap, ctx.own).is_some())
            {
                return Err(exec_err(
                    "42P07",
                    format!("relation \"{}\" already exists", name),
                ));
            }
            let sel = match parse_statement(query) {
                Ok(Stmt::Select(sel)) => sel,
                _ => {
                    return Err(exec_err(
                        "0A000",
                        "materialized view query is not a SELECT".to_string(),
                    ));
                }
            };
            let plan = plan_select(
                eng,
                &sel,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                false,
            )?;
            let res = exec_create_view(
                eng,
                ctx,
                name,
                query,
                col_aliases,
                *or_replace,
                *security_invoker,
                *check_option,
                *matview,
                *with_data,
            )?;
            if *with_data {
                let n = affected_of(&res);
                let ax = dml_ax(eng, ctx, n);
                render_analyze(&plan, &ax, 0, None, true, &mut out);
            } else {
                render_never_executed(&plan, 0, &mut out);
            }
        }
        Stmt::RefreshMatView {
            name,
            concurrently,
            with_data,
        } => {
            let sel = matview_select(eng, ctx, name)?;
            let plan = plan_select(
                eng,
                &sel,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                false,
            )?;
            exec_refresh_matview(eng, ctx, name, *with_data, *concurrently)?;
            if *with_data {
                let n = eng
                    .db
                    .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                    .map(|t| {
                        t.rows
                            .iter()
                            .filter(|r| crate::storage::row_visible(r, ctx.snap, &ctx.all_xids))
                            .count() as u64
                    })
                    .unwrap_or(0);
                let ax = dml_ax(eng, ctx, n);
                render_analyze(&plan, &ax, 0, None, true, &mut out);
            } else {
                render_never_executed(&plan, 0, &mut out);
            }
        }
        Stmt::Declare { query, .. } => {
            // PG19 runs the query under EXPLAIN ANALYZE DECLARE (no
            // cursor is created); this engine renders the planning
            // form — no census statement exercises ANALYZE DECLARE.
            let plan = plan_select(
                eng,
                query,
                ctx.snap,
                ctx.own,
                ctx.session,
                &[],
                !costs,
                false,
            )?;
            render_plan(&plan, 0, costs, false, &mut out);
        }
        _ => {
            return Err(exec_err(
                "0A000",
                "EXPLAIN ANALYZE of this statement is not supported".to_string(),
            ));
        }
    }
    Ok(explain_result(out))
}
