// v2.01: MERGE (PG19 MergeStmt) — parser + AST in src/sql, execution
// here. Plan-then-apply like UPDATE/DELETE: the join between the target
// and the USING source is evaluated against the statement snapshot, all
// actions are planned and validated (privileges, cardinality, unique /
// CHECK / FK), and only then applied through the same WriteOps, toast
// and index maintenance the plain DML executors use, so WAL/durability
// come for free.
//
// Semantics grounded in PG19 nodeModifyTable.c (ExecMerge) and
// analyze.c: WHEN clauses fire in written order, first true condition
// wins; MATCHED conditions/actions see target+source, NOT MATCHED
// (BY TARGET) conditions and INSERT values see the source only, NOT
// MATCHED BY SOURCE sees the target only (out-of-scope references fail
// 42703 at evaluation, as PG's analysis rejects them); a target row
// updated/deleted by more than one source row is 21000.
use super::*;

use crate::sql::{MergeAction, MergeMatchKind, MergeWhen};

/// One planned MERGE action, in processing order (for RETURNING).
enum MAct {
    Update {
        leaf: String,
        id: u64,
        xmax: u64,
        old: Row,
        new: Row,
        src: Option<Row>,
    },
    Delete {
        leaf: String,
        id: u64,
        xmax: u64,
        old: Row,
        src: Option<Row>,
    },
    Insert {
        dst: String,
        new: Row,
        src: Row,
    },
}

fn eval_m(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    schema: &[QCol],
    values: &[Value],
    e: &Expr,
    ctes: &[Rc<CteBinding>],
) -> Result<Value, ExecError> {
    eval_update_expr(
        eng,
        ctx.snap,
        ctx.own,
        ctx.session,
        ctx.role,
        schema,
        values,
        e,
        ctes,
        None,
        Some(qwrite_from_ctx(
            &mut *ctx.writes,
            ctx.write_xid,
            ctx.level,
            ctx.default_toast_compression,
        )),
    )
}

fn truthy(v: &Value) -> bool {
    *v == Value::Bool(true)
}

/// PG19 target-relation gate for MERGE: a missing name is 42P01; a
/// materialized view is a dedicated error; a plain view is held back
/// (view DML rewriting is its own layer — plain UPDATE/DELETE on views
/// fail in this engine too).
fn merge_target_table<'a>(
    eng: &'a Engine,
    ctx: &StmtCtx,
    table: &str,
) -> Result<&'a Table, ExecError> {
    if let Some(t) = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        return Ok(t);
    }
    if let Some(v) = eng.db.find_view(table, ctx.snap, ctx.own) {
        let base = table.rsplit('.').next().unwrap_or(table);
        if v.matview {
            return Err(exec_err(
                "0A000",
                format!("cannot execute MERGE on relation \"{}\"", base),
            ));
        }
        return Err(exec_err(
            "0A000",
            format!("cannot execute MERGE on relation \"{}\"", base),
        ));
    }
    Err(exec_err(
        "42P01",
        format!("relation \"{}\" does not exist", table),
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_merge(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    alias: &Option<String>,
    source: &[FromItem],
    on: &Expr,
    whens: &[MergeWhen],
    returning: &[SelectItem],
    with: &[CteDef],
) -> Result<ExecResult, ExecError> {
    let qual = alias.as_deref().unwrap_or(table).to_string();
    // --- static validation: target, clause shapes ----------------------
    let (meta, partitioned, pinfo, unique_defs, target_oid, tdef) = {
        let t = merge_target_table(eng, ctx, table)?;
        let mut defs: Vec<UniqueDef> = t.uniques.clone();
        if let Some(pk) = &t.pkey {
            defs.push(pk.clone());
        }
        (
            TableMeta::of(t),
            t.partition.is_some(),
            t.partition.clone(),
            defs,
            t.oid,
            t.clone(),
        )
    };
    let columns = meta.columns.clone();
    // Target appearing in the source is PG's duplicate-name error.
    {
        fn walk(item: &FromItem, out: &mut Vec<String>) {
            match item {
                FromItem::Table { name, .. } => {
                    out.push(name.rsplit('.').next().unwrap_or(name).to_string())
                }
                FromItem::Join { left, right, .. } => {
                    walk(left, out);
                    walk(right, out);
                }
                _ => {}
            }
        }
        let mut names = Vec::new();
        for it in source {
            walk(it, &mut names);
        }
        let tbase = table.rsplit('.').next().unwrap_or(table);
        if names.iter().any(|n| n.eq_ignore_ascii_case(tbase)) {
            return Err(exec_err(
                "42712",
                format!("name \"{}\" specified more than once", tbase),
            ));
        }
    }
    // PG19 parse analysis: the source query cannot reference the
    // target relation at any depth (42P01 "invalid reference to
    // FROM-clause entry for table \"t\""), even when the source turns
    // out empty — reference resolution there is static, while the
    // executor's is per-row. An inner FROM entry of the same name
    // shadows the target and is legal.
    {
        fn item_quals(item: &FromItem, out: &mut HashSet<String>) {
            match item {
                FromItem::Table { name, alias, .. } => {
                    out.insert(alias.clone().unwrap_or_else(|| name.clone()));
                    out.insert(name.clone());
                }
                FromItem::Derived { alias, .. } => {
                    out.insert(alias.clone());
                }
                FromItem::Function { name, alias, .. } => {
                    out.insert(alias.clone().unwrap_or_else(|| name.clone()));
                }
                FromItem::Join { left, right, .. } => {
                    item_quals(left, out);
                    item_quals(right, out);
                }
                FromItem::Values { alias, .. } => {
                    out.insert(alias.clone());
                }
            }
        }
        fn check_refs(e: &Expr, tgt: &[String], shadow: &HashSet<String>) -> Result<(), ExecError> {
            let mut refs: Vec<(Option<String>, String)> = Vec::new();
            collect_col_refs(e, &mut refs);
            for (q, _) in &refs {
                if let Some(q) = q {
                    if tgt.iter().any(|t| t.eq_ignore_ascii_case(q))
                        && !shadow.iter().any(|s| s.eq_ignore_ascii_case(q))
                    {
                        return Err(exec_err(
                            "42P01",
                            format!("invalid reference to FROM-clause entry for table \"{}\"", q),
                        ));
                    }
                }
            }
            Ok(())
        }
        fn walk_items(
            items: &[FromItem],
            tgt: &[String],
            shadow: &HashSet<String>,
        ) -> Result<(), ExecError> {
            for item in items {
                match item {
                    FromItem::Derived { sub, .. } => {
                        walk_select(sub, tgt, shadow)?;
                    }
                    FromItem::Join {
                        left, right, on, ..
                    } => {
                        let mut inner = shadow.clone();
                        item_quals(left, &mut inner);
                        item_quals(right, &mut inner);
                        walk_items(std::slice::from_ref(&**left), tgt, &inner)?;
                        walk_items(std::slice::from_ref(&**right), tgt, &inner)?;
                        if let Some(on) = on {
                            check_refs(on, tgt, &inner)?;
                        }
                    }
                    FromItem::Values { rows, .. } => {
                        for row in rows {
                            for e in row {
                                check_refs(e, tgt, shadow)?;
                            }
                        }
                    }
                    FromItem::Function { args, .. } => {
                        for e in args {
                            check_refs(e, tgt, shadow)?;
                        }
                    }
                    FromItem::Table { .. } => {}
                }
            }
            Ok(())
        }
        fn walk_select(
            sel: &SelectStmt,
            tgt: &[String],
            shadow: &HashSet<String>,
        ) -> Result<(), ExecError> {
            let mut inner = shadow.clone();
            for it in &sel.from {
                item_quals(it, &mut inner);
            }
            for item in &sel.items {
                if let SelectItem::Expr { expr, .. } = item {
                    check_refs(expr, tgt, &inner)?;
                }
            }
            if let Some(w) = &sel.where_ {
                check_refs(w, tgt, &inner)?;
            }
            if let Some(h) = &sel.having {
                check_refs(h, tgt, &inner)?;
            }
            for set in &sel.group_by {
                for e in set {
                    check_refs(e, tgt, &inner)?;
                }
            }
            for t in &sel.order_by {
                check_refs(&t.expr, tgt, &inner)?;
            }
            for e in &sel.distinct_on {
                check_refs(e, tgt, &inner)?;
            }
            walk_items(&sel.from, tgt, &inner)
        }
        let tgt_names: Vec<String> = vec![
            qual.clone(),
            table.to_string(),
            table.rsplit('.').next().unwrap_or(table).to_string(),
        ];
        let empty = HashSet::new();
        walk_items(source, &tgt_names, &empty)?;
    }
    // SET / INSERT column existence + duplicate SET targets (42701).
    for w in whens {
        match &w.action {
            MergeAction::Update { sets } => {
                let mut seen = HashSet::new();
                for (name, _) in sets {
                    if !seen.insert(name.clone()) {
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
            MergeAction::Insert { columns, values } => {
                if let Some(cols) = columns {
                    let mut seen = HashSet::new();
                    for c in cols {
                        if !seen.insert(c.clone()) {
                            return Err(exec_err(
                                "42701",
                                format!("column \"{}\" specified more than once", c),
                            ));
                        }
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
                    if let Some(vs) = values {
                        if vs.len() > cols.len() {
                            return Err(exec_err(
                                "42601",
                                "INSERT has more expressions than target columns".to_string(),
                            ));
                        }
                        if vs.len() < cols.len() {
                            return Err(exec_err(
                                "42601",
                                "INSERT has more target columns than expressions".to_string(),
                            ));
                        }
                    }
                } else if let Some(vs) = values {
                    if vs.len() > meta.columns.len() {
                        return Err(exec_err(
                            "42601",
                            "INSERT has more expressions than target columns".to_string(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    // --- privileges (PG19 aclchk for MERGE, mirroring the DML executors)
    let has_delete = whens
        .iter()
        .any(|w| matches!(w.action, MergeAction::Delete));
    if has_delete {
        require_table_priv(eng, ctx, table, crate::storage::PRIV_DELETE, "DELETE")?;
    }
    for w in whens {
        match &w.action {
            MergeAction::Update { sets } => {
                require_column_privs(
                    eng,
                    ctx,
                    table,
                    &sets.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                    crate::storage::PRIV_UPDATE,
                    "UPDATE",
                )?;
            }
            MergeAction::Insert { columns, .. } => {
                if let Some(cols) = columns {
                    require_column_privs(
                        eng,
                        ctx,
                        table,
                        cols,
                        crate::storage::PRIV_INSERT,
                        "INSERT",
                    )?;
                } else {
                    require_table_priv(eng, ctx, table, crate::storage::PRIV_INSERT, "INSERT")?;
                }
            }
            _ => {}
        }
    }
    // Columns of the target READ by ON / conditions / SET exprs /
    // RETURNING need SELECT; when nothing is read by name the join
    // itself still needs table-level SELECT (PG: a DO NOTHING merge by
    // a role with no privileges fails on the target).
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
        let before = exprs.len();
        let ret_exprs: Vec<&Expr> = returning
            .iter()
            .filter_map(|i| match i {
                SelectItem::Expr { expr, .. } => Some(expr),
                _ => None,
            })
            .collect();
        exprs.extend(ret_exprs.iter().copied());
        let _ = before;
        check_dml_read_cols(eng, ctx, table, alias.as_deref(), &exprs, &[])?;
        // If no target column is referenced anywhere, fall back to the
        // table-level SELECT requirement.
        let mut refs: Vec<(Option<String>, String)> = Vec::new();
        for e in &exprs {
            collect_col_refs(e, &mut refs);
        }
        let base = table.rsplit('.').next().unwrap_or(table);
        let any_target_ref = refs.iter().any(|(q, n)| {
            if n == "*" {
                return q.as_deref() == Some(qual.as_str())
                    || q.as_deref() == Some(base)
                    || q.as_deref() == Some(table);
            }
            match q {
                Some(q) => {
                    (q == &qual || q == base || q == table) && meta.column_index(n).is_some()
                }
                None => meta.column_index(n).is_some(),
            }
        });
        if !any_target_ref {
            require_table_priv(eng, ctx, table, crate::storage::PRIV_SELECT, "SELECT")?;
        }
    }
    // v2.02: replica-identity guard, per action in written order
    // (PG19 CheckValidResultRel iterates mergeActions; INSERT and DO
    // NOTHING are unrestricted). Partitioned roots are skipped inside
    // the guard; their touched leaves are checked after planning.
    for w in whens {
        match &w.action {
            MergeAction::Update { .. } => {
                check_replica_identity_for(eng, ctx, table, true)?;
            }
            MergeAction::Delete => {
                check_replica_identity_for(eng, ctx, table, false)?;
            }
            _ => {}
        }
    }
    let ctes = materialize_dml_ctes(eng, &mut *ctx, with)?;
    // --- source rows -----------------------------------------------------
    let (src_schema, src_rows): (Vec<QCol>, Vec<Row>) = {
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: ctx.read_only,
            depth: 0,
            lock_ids: &mut lock_ids,
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
                &mut *ctx.writes,
                ctx.write_xid,
                ctx.level,
                ctx.default_toast_compression,
            )),
        };
        let (s, rows) = build_from(&mut q, &[], source, None, false, false, false, None, None)?;
        (s, rows.into_iter().map(|r| r.cells).collect())
    };
    // --- target rows (visible, over leaves for partitioned targets) -----
    let tgt_schema: Vec<QCol> = columns
        .iter()
        .map(|(n, ty)| QCol {
            qual: qual.clone(),
            name: n.clone(),
            ty: *ty,
            hidden: false,
            src_ord: 0,
        })
        .collect();
    // (leaf, leaf_cols, id, xmax, values in parent order, values in leaf
    // order, toast value ids)
    let mut vis: Vec<(String, Vec<(String, ColType)>, u64, u64, Row, Row, Vec<u32>)> = Vec::new();
    {
        let leaves: Vec<(String, Vec<(String, ColType)>)> = if partitioned {
            let mut ls = partition_leaves(eng, ctx, table);
            if ls.is_empty() {
                ls.push((table.to_string(), columns.clone()));
            }
            ls
        } else {
            vec![(table.to_string(), columns.clone())]
        };
        for (leaf, leaf_cols) in &leaves {
            let lt = eng
                .db
                .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("leaf still visible; engine lock held throughout");
            for r in lt
                .rows
                .iter()
                .filter(|r| row_visible(r, ctx.snap, &ctx.all_xids))
            {
                let pv = if leaf == table {
                    r.values.clone()
                } else {
                    Row::new(reorder_row(leaf_cols, &columns, &r.values))
                };
                vis.push((
                    leaf.clone(),
                    leaf_cols.clone(),
                    r.id,
                    r.xmax,
                    pv,
                    r.values.clone(),
                    r.toast.clone(),
                ));
            }
        }
    }
    // v2.02: row-level security — the target's SELECT policy group
    // filters the target rows before any matching (PG19: SELECT
    // security quals on the target RTE). Rows the SELECT group cannot
    // see are simply not there for ON/MATCHED/BY SOURCE evaluation;
    // a source row that would have matched one falls to NOT MATCHED.
    if tdef.rls_enabled {
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: ctx.read_only,
            depth: 0,
            lock_ids: &mut lock_ids,
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
                &mut *ctx.writes,
                ctx.write_xid,
                ctx.level,
                ctx.default_toast_compression,
            )),
        };
        if let Some(groups) =
            rls_visibility_groups(&mut q, &tdef, table, &[crate::sql::PolicyCmd::Select])?
        {
            let mut kept = Vec::with_capacity(vis.len());
            for entry in vis {
                if rls_row_visible(&mut q, &tgt_schema, &entry.4, &groups)? {
                    kept.push(entry);
                }
            }
            vis = kept;
        }
    }
    // Static scope checks (PG19 parse analysis for MERGE): NOT MATCHED
    // clauses see only the source, NOT MATCHED BY SOURCE only the
    // target; a reference to the out-of-scope relation is 42P01
    // "invalid reference to FROM-clause entry". System columns other
    // than tableoid are banned in WHEN conditions (0A000).
    {
        let src_quals: HashSet<String> = src_schema.iter().map(|c| c.qual.clone()).collect();
        let src_names: HashSet<&str> = src_schema.iter().map(|c| c.name.as_str()).collect();
        let base = table.rsplit('.').next().unwrap_or(table);
        for w in whens {
            if let Some(c) = &w.condition {
                let mut refs: Vec<(Option<String>, String)> = Vec::new();
                collect_col_refs(c, &mut refs);
                for (_, n) in &refs {
                    if matches!(n.as_str(), "xmin" | "xmax" | "cmin" | "cmax" | "ctid") {
                        return Err(exec_err(
                            "0A000",
                            format!("cannot use system column \"{}\" in MERGE WHEN condition", n),
                        ));
                    }
                }
            }
            // The WHEN condition is scope-checked for every clause of
            // the kind; action expressions join it where they exist.
            let mut scoped: Vec<&Expr> = Vec::new();
            if let Some(c) = &w.condition {
                scoped.push(c);
            }
            match (&w.kind, &w.action) {
                (MergeMatchKind::NotMatchedByTarget, MergeAction::Insert { values, .. }) => {
                    if let Some(vs) = values {
                        scoped.extend(vs.iter());
                    }
                    for e in &scoped {
                        let mut refs: Vec<(Option<String>, String)> = Vec::new();
                        collect_col_refs(e, &mut refs);
                        for (q, n) in &refs {
                            match q {
                                Some(q) if q == &qual || q == table || q == base => {
                                    return Err(exec_err(
                                        "42P01",
                                        format!(
                                            "invalid reference to FROM-clause entry for table \"{}\"",
                                            q
                                        ),
                                    ));
                                }
                                None if n != "*"
                                    && meta.column_index(n).is_some()
                                    && !src_names.contains(n.as_str()) =>
                                {
                                    return Err(exec_err(
                                        "42703",
                                        format!("column \"{}\" does not exist", n),
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                (MergeMatchKind::NotMatchedByTarget, _) => {
                    for e in &scoped {
                        let mut refs: Vec<(Option<String>, String)> = Vec::new();
                        collect_col_refs(e, &mut refs);
                        for (q, n) in &refs {
                            match q {
                                Some(q) if q == &qual || q == table || q == base => {
                                    return Err(exec_err(
                                        "42P01",
                                        format!(
                                            "invalid reference to FROM-clause entry for table \"{}\"",
                                            q
                                        ),
                                    ));
                                }
                                None if n != "*"
                                    && meta.column_index(n).is_some()
                                    && !src_names.contains(n.as_str()) =>
                                {
                                    return Err(exec_err(
                                        "42703",
                                        format!("column \"{}\" does not exist", n),
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                (MergeMatchKind::NotMatchedBySource, act) => {
                    if let MergeAction::Update { sets } = act {
                        scoped.extend(sets.iter().map(|(_, e)| e));
                    }
                    for e in &scoped {
                        let mut refs: Vec<(Option<String>, String)> = Vec::new();
                        collect_col_refs(e, &mut refs);
                        for (q, n) in &refs {
                            match q {
                                Some(q) if src_quals.contains(q) => {
                                    return Err(exec_err(
                                        "42P01",
                                        format!(
                                            "invalid reference to FROM-clause entry for table \"{}\"",
                                            q
                                        ),
                                    ));
                                }
                                None if n != "*"
                                    && src_names.contains(n.as_str())
                                    && meta.column_index(n).is_none() =>
                                {
                                    return Err(exec_err(
                                        "42703",
                                        format!("column \"{}\" does not exist", n),
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // tableoid folding for ON / WHEN conditions (target, plus a bare
    // single-table source). PG keeps tableoid legal in MERGE
    // conditions; the row evaluator has no provenance channel.
    let mut oid_subs: Vec<(String, u32)> = vec![(qual.clone(), target_oid)];
    if source.len() == 1 {
        if let FromItem::Table { name, alias, .. } = &source[0] {
            if let Some(st) = eng
                .db
                .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
            {
                oid_subs.push((alias.clone().unwrap_or_else(|| name.clone()), st.oid));
            }
        }
    }
    let on_owned = subst_tableoid(on, &oid_subs);
    let on = &on_owned;
    let whens_owned: Vec<MergeWhen> = whens
        .iter()
        .map(|w| MergeWhen {
            kind: w.kind,
            condition: w.condition.as_ref().map(|c| subst_tableoid(c, &oid_subs)),
            action: w.action.clone(),
        })
        .collect();
    let whens = &whens_owned[..];
    // Combined schema for ON / MATCHED evaluation: target first, then
    // source (mirrors UPDATE...FROM's single combined scope).
    let mut comb_schema = tgt_schema.clone();
    comb_schema.extend(src_schema.iter().cloned());
    let null_src: Vec<Value> = vec![Value::Null; src_schema.len()];
    // --- plan -------------------------------------------------------------
    let mut acts: Vec<MAct> = Vec::new();
    let mut matched: Vec<bool> = vec![false; vis.len()];
    let mut acted: Vec<bool> = vec![false; vis.len()];
    // Build one UPDATE's new row (parent order) from an evaluation scope.
    // (Inlined at each call site via this closure-like block pattern.)
    for srow in &src_rows {
        let mut any_match = false;
        for vi in 0..vis.len() {
            let (leaf, _, id, xmax, tvals, _, _) = &vis[vi];
            let mut cvals: Vec<Value> = tvals.iter().cloned().collect();
            cvals.extend(srow.iter().cloned());
            let on_v = eval_m(eng, ctx, &comb_schema, &cvals, on, &ctes)?;
            if !truthy(&on_v) {
                continue;
            }
            any_match = true;
            matched[vi] = true;
            check_write_conflict(eng, *xmax, ctx.level)?;
            for w in whens.iter().filter(|w| w.kind == MergeMatchKind::Matched) {
                let fire = match &w.condition {
                    None => true,
                    Some(c) => truthy(&eval_m(eng, ctx, &comb_schema, &cvals, c, &ctes)?),
                };
                if !fire {
                    continue;
                }
                match &w.action {
                    MergeAction::DoNothing => {}
                    MergeAction::Delete => {
                        if acted[vi] {
                            return Err(exec_err(
                                "21000",
                                "MERGE command cannot affect row a second time".to_string(),
                            ));
                        }
                        // v2.02: DELETE USING policies are a check,
                        // not a filter, for MERGE (42501 on failure).
                        if tdef.rls_enabled {
                            rls_check_merge_using_ctx(
                                eng,
                                ctx,
                                &tdef,
                                table,
                                crate::sql::PolicyCmd::Delete,
                                &tgt_schema,
                                tvals,
                            )?;
                        }
                        check_row_lock(eng, leaf, *id, ctx.own)?;
                        acted[vi] = true;
                        acts.push(MAct::Delete {
                            leaf: leaf.clone(),
                            id: *id,
                            xmax: *xmax,
                            old: tvals.clone(),
                            src: Some(srow.clone()),
                        });
                    }
                    MergeAction::Update { sets } => {
                        if acted[vi] {
                            return Err(exec_err(
                                "21000",
                                "MERGE command cannot affect row a second time".to_string(),
                            ));
                        }
                        // v2.02: UPDATE USING policies are a check,
                        // not a filter, for MERGE (42501 on failure).
                        if tdef.rls_enabled {
                            rls_check_merge_using_ctx(
                                eng,
                                ctx,
                                &tdef,
                                table,
                                crate::sql::PolicyCmd::Update,
                                &tgt_schema,
                                tvals,
                            )?;
                        }
                        check_row_lock(eng, leaf, *id, ctx.own)?;
                        let mut new_values = tvals.to_vec();
                        for (name, expr) in sets {
                            let ci = meta.column_index(name).expect("validated above");
                            let v = eval_m(eng, ctx, &comb_schema, &cvals, expr, &ctes)?;
                            let (cname, ctype) = &columns[ci];
                            let mut nv = coerce_value(v, ctype, cname)?;
                            let comp = meta.composite_types.get(ci).and_then(|o| o.as_deref());
                            nv = coerce_assign_composite(eng, nv, comp)?;
                            new_values[ci] = nv;
                        }
                        // v2.04: generated columns materialize on
                        // the MERGE apply path too (mirrors the
                        // v2.03 DML wiring in dml.rs).
                        compute_generated_columns(
                            eng,
                            ctx.snap,
                            ctx.own,
                            ctx.session,
                            ctx.role,
                            &meta,
                            &mut new_values,
                        )?;
                        // v2.02: WITH CHECK on the new row; the
                        // SELECT group joins as a check because MERGE
                        // always requires SELECT perms on the target
                        // (rowsecurity.c CMD_MERGE branch).
                        if tdef.rls_enabled {
                            rls_check_new_row_ctx(
                                eng,
                                ctx,
                                &tdef,
                                table,
                                crate::sql::PolicyCmd::Update,
                                true,
                                false,
                                &tgt_schema,
                                &new_values,
                            )?;
                        }
                        acted[vi] = true;
                        acts.push(MAct::Update {
                            leaf: leaf.clone(),
                            id: *id,
                            xmax: *xmax,
                            old: tvals.clone(),
                            new: Row::new(new_values),
                            src: Some(srow.clone()),
                        });
                    }
                    MergeAction::Insert { .. } => {
                        unreachable!("parser enforces kind/action pairing")
                    }
                }
                break;
            }
        }
        if !any_match {
            for w in whens
                .iter()
                .filter(|w| w.kind == MergeMatchKind::NotMatchedByTarget)
            {
                let fire = match &w.condition {
                    None => true,
                    Some(c) => truthy(&eval_m(eng, ctx, &src_schema, srow, c, &ctes)?),
                };
                if !fire {
                    continue;
                }
                if let MergeAction::Insert {
                    columns: icols,
                    values,
                } = &w.action
                {
                    let row = build_merge_insert(
                        eng,
                        ctx,
                        &meta,
                        table,
                        icols,
                        values,
                        &src_schema,
                        srow,
                        &ctes,
                    )?;
                    // v2.02: INSERT WITH CHECK on the new row; the
                    // SELECT group joins only when RETURNING is
                    // present (rowsecurity.c CMD_MERGE branch).
                    if tdef.rls_enabled {
                        rls_check_new_row_ctx(
                            eng,
                            ctx,
                            &tdef,
                            table,
                            crate::sql::PolicyCmd::Insert,
                            !returning.is_empty(),
                            false,
                            &tgt_schema,
                            &row[..],
                        )?;
                    }
                    acts.push(MAct::Insert {
                        dst: table.to_string(),
                        new: row,
                        src: srow.clone(),
                    });
                }
                break;
            }
        }
    }
    // NOT MATCHED BY SOURCE: target rows no source row joined to.
    for vi in 0..vis.len() {
        if matched[vi] {
            continue;
        }
        let (leaf, _, id, xmax, tvals, _, _) = &vis[vi];
        for w in whens
            .iter()
            .filter(|w| w.kind == MergeMatchKind::NotMatchedBySource)
        {
            let fire = match &w.condition {
                None => true,
                Some(c) => truthy(&eval_m(eng, ctx, &tgt_schema, tvals, c, &ctes)?),
            };
            if !fire {
                continue;
            }
            match &w.action {
                MergeAction::DoNothing => {}
                MergeAction::Delete => {
                    // v2.02: DELETE USING check (BY SOURCE rows are
                    // target rows too; same WCO as MATCHED deletes).
                    if tdef.rls_enabled {
                        rls_check_merge_using_ctx(
                            eng,
                            ctx,
                            &tdef,
                            table,
                            crate::sql::PolicyCmd::Delete,
                            &tgt_schema,
                            tvals,
                        )?;
                    }
                    check_row_lock(eng, leaf, *id, ctx.own)?;
                    acts.push(MAct::Delete {
                        leaf: leaf.clone(),
                        id: *id,
                        xmax: *xmax,
                        old: tvals.clone(),
                        src: None,
                    });
                }
                MergeAction::Update { sets } => {
                    // v2.02: UPDATE USING check on the existing row,
                    // then WITH CHECK (+ SELECT visibility) on the new.
                    if tdef.rls_enabled {
                        rls_check_merge_using_ctx(
                            eng,
                            ctx,
                            &tdef,
                            table,
                            crate::sql::PolicyCmd::Update,
                            &tgt_schema,
                            tvals,
                        )?;
                    }
                    check_row_lock(eng, leaf, *id, ctx.own)?;
                    let mut new_values = tvals.to_vec();
                    for (name, expr) in sets {
                        let ci = meta.column_index(name).expect("validated above");
                        let v = eval_m(eng, ctx, &tgt_schema, tvals, expr, &ctes)?;
                        let (cname, ctype) = &columns[ci];
                        let mut nv = coerce_value(v, ctype, cname)?;
                        let comp = meta.composite_types.get(ci).and_then(|o| o.as_deref());
                        nv = coerce_assign_composite(eng, nv, comp)?;
                        new_values[ci] = nv;
                    }
                    if tdef.rls_enabled {
                        rls_check_new_row_ctx(
                            eng,
                            ctx,
                            &tdef,
                            table,
                            crate::sql::PolicyCmd::Update,
                            true,
                            false,
                            &tgt_schema,
                            &new_values,
                        )?;
                    }
                    acts.push(MAct::Update {
                        leaf: leaf.clone(),
                        id: *id,
                        xmax: *xmax,
                        old: tvals.clone(),
                        new: Row::new(new_values),
                        src: None,
                    });
                }
                MergeAction::Insert { .. } => {
                    unreachable!("parser enforces kind/action pairing")
                }
            }
            break;
        }
    }
    // v2.02: replica-identity guard for partitioned targets — check
    // each leaf an UPDATE/DELETE action actually touches.
    {
        let mut seen: HashSet<(String, bool)> = HashSet::new();
        for a in &acts {
            let (leaf, is_update) = match a {
                MAct::Update { leaf, .. } => (leaf, true),
                MAct::Delete { leaf, .. } => (leaf, false),
                _ => continue,
            };
            if seen.insert((leaf.clone(), is_update)) {
                check_replica_identity_for(eng, ctx, leaf, is_update)?;
            }
        }
    }
    // --- route inserts / partition-key moves, fire INSERT triggers ------
    // Resolve each insert's destination leaf (parent order kept in `new`).
    let mut routed: Vec<MAct> = Vec::with_capacity(acts.len());
    for a in acts {
        match a {
            MAct::Insert { new, src, .. } if partitioned => {
                let p = pinfo.as_ref().expect("partitioned target keeps its info");
                let dst = find_partition_leaf(eng, ctx, table, p, &columns, &new.to_vec())?;
                routed.push(MAct::Insert { dst, new, src });
            }
            other => routed.push(other),
        }
    }
    let acts = routed;
    // BEFORE INSERT row triggers, per destination (rows in parent order;
    // leaves share the parent's column names — the trigger machinery
    // works off names). A trigger returning NULL drops the row.
    let acts: Vec<MAct> = {
        let mut out: Vec<MAct> = Vec::with_capacity(acts.len());
        let mut pending_ins: Vec<(usize, String, Row)> = Vec::new();
        for (i, a) in acts.iter().enumerate() {
            if let MAct::Insert { dst, new, .. } = a {
                pending_ins.push((i, dst.clone(), new.clone()));
            }
        }
        let mut dropped: HashSet<usize> = HashSet::new();
        let mut replaced: HashMap<usize, Row> = HashMap::new();
        if !pending_ins.is_empty() {
            // Group by destination, preserving order.
            let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
            for (pos, (i, dst, _)) in pending_ins.iter().enumerate() {
                match groups.iter_mut().find(|(d, _)| d == dst) {
                    Some((_, v)) => v.push(pos),
                    None => groups.push((dst.clone(), vec![pos])),
                }
                let _ = i;
            }
            for (dst, positions) in &groups {
                let rows: Vec<Row> = positions
                    .iter()
                    .map(|&p| pending_ins[p].2.clone())
                    .collect();
                let fired = fire_before_insert_triggers(eng, ctx, dst, &columns, &rows)?;
                for (k, &p) in positions.iter().enumerate() {
                    match &fired[k] {
                        None => {
                            dropped.insert(pending_ins[p].0);
                        }
                        Some(r) => {
                            replaced.insert(pending_ins[p].0, r.clone());
                        }
                    }
                }
            }
        }
        for (i, a) in acts.into_iter().enumerate() {
            if dropped.contains(&i) {
                continue;
            }
            match a {
                MAct::Insert { dst, new, src } => out.push(MAct::Insert {
                    dst,
                    new: replaced.get(&i).cloned().unwrap_or(new),
                    src,
                }),
                other => out.push(other),
            }
        }
        out
    };
    // --- validate planned writes -----------------------------------------
    // Destination leaf metadata + column order per leaf.
    let leaf_cols = |eng: &Engine, ctx: &StmtCtx, leaf: &str| -> Vec<(String, ColType)> {
        if leaf == table {
            columns.clone()
        } else {
            eng.db
                .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .map(|t| t.columns.clone())
                .unwrap_or_else(|| columns.clone())
        }
    };
    // New rows per destination (parent order), for FK self-reference and
    // pairwise unique checks.
    let mut new_by_dst: HashMap<String, Vec<Row>> = HashMap::new();
    for a in &acts {
        match a {
            MAct::Insert { dst, new, .. } => {
                new_by_dst.entry(dst.clone()).or_default().push(new.clone())
            }
            MAct::Update { leaf, new, .. } => new_by_dst
                .entry(leaf.clone())
                .or_default()
                .push(new.clone()),
            _ => {}
        }
    }
    for a in &acts {
        match a {
            MAct::Insert { dst, new, .. } => {
                let lc = leaf_cols(eng, ctx, dst);
                let dst_meta = {
                    let t = eng
                        .db
                        .find_table(dst, ctx.snap, &ctx.all_xids, ctx.session)
                        .expect("destination visible");
                    TableMeta::of(t)
                };
                let leaf_row = reorder_row(&columns, &lc, &new.to_vec());
                check_row_constraints(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    &dst_meta,
                    dst,
                    &leaf_row,
                )?;
                if let Some(vname) = eng.db.unique_violation(
                    dst,
                    &leaf_row,
                    None,
                    ctx.snap,
                    &ctx.all_xids,
                    ctx.session,
                ) {
                    return Err(exec_err(
                        "23505",
                        format!(
                            "duplicate key value violates unique constraint \"{}\"",
                            vname
                        ),
                    ));
                }
            }
            MAct::Update { leaf, id, new, .. } => {
                let lc = leaf_cols(eng, ctx, leaf);
                let dst_meta = {
                    let t = eng
                        .db
                        .find_table(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                        .expect("destination visible");
                    TableMeta::of(t)
                };
                let leaf_row = reorder_row(&columns, &lc, &new.to_vec());
                check_row_constraints(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    &dst_meta,
                    leaf,
                    &leaf_row,
                )?;
                if let Some(vname) = eng.db.unique_violation(
                    leaf,
                    &leaf_row,
                    Some(*id),
                    ctx.snap,
                    &ctx.all_xids,
                    ctx.session,
                ) {
                    return Err(exec_err(
                        "23505",
                        format!(
                            "duplicate key value violates unique constraint \"{}\"",
                            vname
                        ),
                    ));
                }
            }
            _ => {}
        }
    }
    // Pairwise unique conflicts among the planned new rows themselves
    // (the table scan above cannot see them yet). Parent column order.
    {
        let mut all_new: Vec<&Row> = Vec::new();
        for a in &acts {
            match a {
                MAct::Insert { new, .. } | MAct::Update { new, .. } => all_new.push(new),
                _ => {}
            }
        }
        for def in &unique_defs {
            let idxs: Vec<usize> = def
                .cols
                .iter()
                .filter_map(|c| meta.column_index(c))
                .collect();
            if idxs.len() != def.cols.len() || idxs.is_empty() {
                continue;
            }
            for i in 0..all_new.len() {
                for j in (i + 1)..all_new.len() {
                    let (a, b) = (&all_new[i], &all_new[j]);
                    if idxs.iter().all(|&k| a[k] != Value::Null && a[k] == b[k]) {
                        return Err(exec_err(
                            "23505",
                            format!(
                                "duplicate key value violates unique constraint \"{}\"",
                                def.name
                            ),
                        ));
                    }
                }
            }
        }
    }
    // FK child checks for inserted/updated rows. `self_new` (every new
    // row headed to the same destination, reordered into leaf column
    // order) depends only on `dst`, not on which action is being
    // checked — reorder each destination's rows once here rather than
    // once per action that happens to share the destination (was
    // O(actions^2) reorder_row calls for a destination with many
    // planned actions; a plain upsert MERGE sends every row to the
    // same destination).
    let self_new_by_dst: HashMap<&str, Vec<Row>> = new_by_dst
        .iter()
        .map(|(dst, rows)| {
            let lc = leaf_cols(eng, ctx, dst);
            let reordered: Vec<Row> = rows
                .iter()
                .map(|r| Row::new(reorder_row(&columns, &lc, &r.to_vec())))
                .collect();
            (dst.as_str(), reordered)
        })
        .collect();
    let no_self_new: Vec<Row> = Vec::new();
    for a in &acts {
        let (dst, nv, excl): (&str, &Row, Option<u64>) = match a {
            MAct::Insert { dst, new, .. } => (dst, new, None),
            MAct::Update { leaf, id, new, .. } => (leaf, new, Some(*id)),
            _ => continue,
        };
        let lc = leaf_cols(eng, ctx, dst);
        let dst_meta = {
            let t = eng
                .db
                .find_table(dst, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("destination visible");
            TableMeta::of(t)
        };
        let leaf_row = Row::new(reorder_row(&columns, &lc, &nv.to_vec()));
        let self_new = self_new_by_dst.get(dst).unwrap_or(&no_self_new);
        check_fk_child_row(
            eng,
            ctx.snap,
            ctx.own,
            ctx.session,
            &dst_meta,
            dst,
            &leaf_row,
            self_new,
            excl,
        )?;
    }
    // FK parent cascades for deleted / updated rows (old values in leaf
    // order), planned before any mutation.
    {
        let mut by_src: HashMap<&str, Vec<(u64, Row, Option<Row>)>> = HashMap::new();
        for a in &acts {
            match a {
                MAct::Delete { leaf, id, .. } => {
                    let v = vis
                        .iter()
                        .find(|v| v.0 == *leaf && v.2 == *id)
                        .expect("vis row");
                    by_src
                        .entry(leaf.as_str())
                        .or_default()
                        .push((*id, v.5.clone(), None));
                }
                MAct::Update { leaf, id, new, .. } => {
                    let v = vis
                        .iter()
                        .find(|v| v.0 == *leaf && v.2 == *id)
                        .expect("vis row");
                    let lc = leaf_cols(eng, ctx, leaf);
                    let leaf_new = Row::new(reorder_row(&columns, &lc, &new.to_vec()));
                    by_src.entry(leaf.as_str()).or_default().push((
                        *id,
                        v.5.clone(),
                        Some(leaf_new),
                    ));
                }
                _ => {}
            }
        }
        if !by_src.is_empty() {
            let mut cascade = FkCascade::default();
            for (src, changed) in &by_src {
                let src_meta = {
                    let t = eng
                        .db
                        .find_table(src, ctx.snap, &ctx.all_xids, ctx.session)
                        .expect("source leaf visible");
                    TableMeta::of(t)
                };
                plan_fk_cascade(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    ctx.level,
                    src,
                    &src_meta,
                    changed,
                    0,
                    &mut cascade,
                )?;
            }
            apply_fk_cascade(eng, ctx, cascade)?;
        }
    }
    // --- apply -------------------------------------------------------------
    let total = acts.len() as u64;
    // RETURNING provenance: new row ids for updates/inserts.
    let mut new_id_of: HashMap<u64, u64> = HashMap::new();
    // Deletes first (per leaf), then updates, then inserts — matching
    // rows are already validated; order among disjoint rows is not
    // observable except through RETURNING, which uses `acts` order.
    {
        let mut by_leaf: HashMap<String, Vec<(u64, u64)>> = HashMap::new();
        let mut toast_by_leaf: HashMap<String, Vec<u32>> = HashMap::new();
        for a in &acts {
            if let MAct::Delete { leaf, id, xmax, .. } = a {
                by_leaf.entry(leaf.clone()).or_default().push((*id, *xmax));
                let v = vis
                    .iter()
                    .find(|v| v.0 == *leaf && v.2 == *id)
                    .expect("vis row");
                toast_by_leaf
                    .entry(leaf.clone())
                    .or_default()
                    .extend(v.6.iter().copied().filter(|x| *x != 0));
            }
        }
        let mut leaves: Vec<String> = by_leaf.keys().cloned().collect();
        leaves.sort_unstable();
        for leaf in &leaves {
            let t = eng
                .db
                .find_table_mut(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                .expect("table still visible");
            for (id, prev_xmax) in &by_leaf[leaf.as_str()] {
                let pos = t.row_pos(*id).expect("row version still present");
                t.rows[pos].xmax = ctx.write_xid;
                ctx.writes.push(WriteOp::DeleteRow {
                    table: leaf.clone(),
                    row_id: *id,
                    prev_xmax: *prev_xmax,
                });
            }
        }
        for leaf in &leaves {
            if let Some(vids) = toast_by_leaf.get(leaf.as_str()) {
                if !vids.is_empty() {
                    toast_delete_chunks(eng, ctx, leaf, vids)?;
                }
            }
        }
    }
    {
        // In-place updates (destination == source leaf, or unpartitioned).
        let mut by_leaf: HashMap<String, Vec<(u64, u64, Row, Row, Vec<u32>)>> = HashMap::new();
        let mut moves: Vec<(String, u64, u64, String, Row, Row)> = Vec::new();
        for a in &acts {
            if let MAct::Update {
                leaf,
                id,
                xmax,
                old,
                new,
                ..
            } = a
            {
                let dst = if partitioned {
                    let p = pinfo.as_ref().expect("partitioned info");
                    find_partition_leaf(eng, ctx, table, p, &columns, &new.to_vec())?
                } else {
                    leaf.clone()
                };
                let v = vis
                    .iter()
                    .find(|v| v.0 == *leaf && v.2 == *id)
                    .expect("vis row");
                if dst == *leaf {
                    let lc = leaf_cols(eng, ctx, leaf);
                    let leaf_new = Row::new(reorder_row(&columns, &lc, &new.to_vec()));
                    by_leaf.entry(leaf.clone()).or_default().push((
                        *id,
                        *xmax,
                        leaf_new,
                        old.clone(),
                        v.6.clone(),
                    ));
                } else {
                    moves.push((leaf.clone(), *id, *xmax, dst, new.clone(), old.clone()));
                }
            }
        }
        let mut leaves: Vec<String> = by_leaf.keys().cloned().collect();
        leaves.sort_unstable();
        for leaf in &leaves {
            let rows = &by_leaf[leaf.as_str()];
            let mut new_ids = Vec::with_capacity(rows.len());
            for _ in 0..rows.len() {
                new_ids.push(eng.alloc_row_id());
            }
            let mut indexed: Vec<(u64, Row, Row, Vec<u32>)> = Vec::new();
            {
                let t = eng
                    .db
                    .find_table_mut(leaf, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("table still visible");
                for ((old_id, prev_xmax, new_values, _, _), new_id) in
                    rows.iter().cloned().zip(new_ids.iter())
                {
                    new_id_of.insert(old_id, *new_id);
                    let pos = t.row_pos(old_id).expect("row version still present");
                    let old_values = t.rows[pos].values.clone();
                    let old_toast = t.rows[pos].toast.clone();
                    t.rows[pos].xmax = ctx.write_xid;
                    t.push_version(RowVersion::plain(
                        *new_id,
                        new_values.clone(),
                        ctx.write_xid,
                    ));
                    ctx.writes.push(WriteOp::UpdateRow {
                        table: leaf.clone(),
                        old_id,
                        new_id: *new_id,
                        prev_xmax,
                        old_values: old_values.clone(),
                    });
                    indexed.push((*new_id, new_values, old_values, old_toast));
                }
            }
            for (new_id, new_values, old_values, old_toast) in &indexed {
                toast_update_row(eng, ctx, leaf, *new_id, old_values, old_toast, new_values)?;
            }
            for (new_id, new_values, _, _) in &indexed {
                eng.db
                    .index_insert_row(leaf, *new_id, new_values, ctx.session);
            }
        }
        // Partition-key moves: delete from source leaf, insert into dst.
        if !moves.is_empty() {
            let mut ins_by_dst: HashMap<String, Vec<(u64, Row)>> = HashMap::new();
            for (src, old_id, prev_xmax, dst, new_parent, _) in &moves {
                let new_id = eng.alloc_row_id();
                new_id_of.insert(*old_id, new_id);
                let t = eng
                    .db
                    .find_table_mut(src, ctx.snap, &ctx.all_xids, ctx.session)
                    .expect("source leaf visible");
                let pos = t.row_pos(*old_id).expect("row version still present");
                t.rows[pos].xmax = ctx.write_xid;
                ctx.writes.push(WriteOp::DeleteRow {
                    table: src.clone(),
                    row_id: *old_id,
                    prev_xmax: *prev_xmax,
                });
                let lc = leaf_cols(eng, ctx, dst);
                let leaf_new = Row::new(reorder_row(&columns, &lc, &new_parent.to_vec()));
                ins_by_dst
                    .entry(dst.clone())
                    .or_default()
                    .push((new_id, leaf_new));
            }
            let mut dsts: Vec<String> = ins_by_dst.keys().cloned().collect();
            dsts.sort_unstable();
            for dst in &dsts {
                apply_row_inserts(eng, ctx, dst, &ins_by_dst[dst.as_str()])?;
            }
        }
    }
    {
        let mut ins_by_dst: HashMap<String, Vec<(u64, Row)>> = HashMap::new();
        for a in &acts {
            if let MAct::Insert { dst, new, .. } = a {
                let new_id = eng.alloc_row_id();
                let lc = leaf_cols(eng, ctx, dst);
                let leaf_row = Row::new(reorder_row(&columns, &lc, &new.to_vec()));
                ins_by_dst
                    .entry(dst.clone())
                    .or_default()
                    .push((new_id, leaf_row));
            }
        }
        let mut dsts: Vec<String> = ins_by_dst.keys().cloned().collect();
        dsts.sort_unstable();
        for dst in &dsts {
            apply_row_inserts(eng, ctx, dst, &ins_by_dst[dst.as_str()])?;
        }
    }
    // --- RETURNING -----------------------------------------------------------
    let (ret_cols, ret_rows) = if returning.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        project_merge_returning(
            eng,
            ctx,
            table,
            &qual,
            &meta,
            &tgt_schema,
            &src_schema,
            &null_src,
            &acts,
            returning,
            &ctes,
        )?
    };
    Ok(ExecResult::Dml {
        tag: format!("MERGE {}", total),
        columns: ret_cols,
        rows: ret_rows,
    })
}

/// Build one MERGE INSERT row (parent column order) from the VALUES
/// expressions evaluated against the source row, with defaults for the
/// columns not listed (PG19 merge_insert path, mirroring exec_insert's
/// VALUES builder: column default, else domain default, else NULL).
#[allow(clippy::too_many_arguments)]
fn build_merge_insert(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    meta: &TableMeta,
    table: &str,
    icols: &Option<Vec<String>>,
    values: &Option<Vec<Expr>>,
    src_schema: &[QCol],
    srow: &Row,
    ctes: &[Rc<CteBinding>],
) -> Result<Row, ExecError> {
    let ncols = meta.columns.len();
    let mut vals = vec![Value::Null; ncols];
    let mut explicit = vec![false; ncols];
    if let Some(exprs) = values {
        let targets: Vec<usize> = match icols {
            Some(cols) => cols
                .iter()
                .map(|c| meta.column_index(c).expect("validated above"))
                .collect(),
            None => (0..exprs.len()).collect(),
        };
        for (expr, &ci) in exprs.iter().zip(targets.iter()) {
            let v = eval_m(eng, ctx, src_schema, srow, expr, ctes)?;
            let (cname, ctype) = &meta.columns[ci];
            let mut nv = coerce_value(v, ctype, cname)?;
            let comp = meta.composite_types.get(ci).and_then(|o| o.as_deref());
            nv = coerce_assign_composite(eng, nv, comp)?;
            vals[ci] = nv;
            explicit[ci] = true;
        }
    } else if let Some(cols) = icols {
        // INSERT (cols) DEFAULT VALUES is not in the PG grammar (the
        // parser only produces values=None without a column list), but
        // stay total: listed columns take defaults like the rest.
        let _ = cols;
    }
    let _ = table;
    for i in 0..ncols {
        if !explicit[i] {
            let dd = domain_default(eng, meta, i).cloned();
            if let Some(d) = meta.defaults[i].as_ref().or(dd.as_ref()) {
                let (cname, ctype) = &meta.columns[i];
                vals[i] = eval_default(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    d,
                    ctype,
                    cname,
                )?;
            }
        }
    }
    // Stamp user range/composite type names like the INSERT paths.
    let mut vals = vals;
    for ci in 0..vals.len() {
        if let Some(tname) = meta.composite_types.get(ci).and_then(|o| o.as_deref()) {
            let v = std::mem::replace(&mut vals[ci], Value::Null);
            vals[ci] = coerce_assign_composite(eng, v, Some(tname))?;
        }
    }
    // v2.04: generated columns materialize on the MERGE insert path.
    compute_generated_columns(
        eng,
        ctx.snap,
        ctx.own,
        ctx.session,
        ctx.role,
        meta,
        &mut vals,
    )?;
    Ok(Row::new(vals))
}

/// v2.01: rewrite a MERGE RETURNING expression for evaluation/typing:
/// `merge_action()` becomes a text literal (the action name when
/// `action` is Some, an empty placeholder for type inference), and the
/// bare identifiers `old` / `new` become whole-row references. Other
/// containers are rewritten recursively; anything else is cloned.
fn rewrite_returning_expr(e: &Expr, action: Option<&str>) -> Expr {
    match e {
        Expr::Func { name, args }
            if name.eq_ignore_ascii_case("merge_action") && args.is_empty() =>
        {
            Expr::Literal(crate::sql::Literal::Text(
                action.unwrap_or("").to_string().into(),
            ))
        }
        Expr::Column { table: None, name } if name == "old" || name == "new" => {
            Expr::WholeRow { qual: name.clone() }
        }
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| rewrite_returning_expr(a, action))
                .collect(),
        },
        Expr::Arith { op, left, right } => Expr::Arith {
            op: *op,
            left: Box::new(rewrite_returning_expr(left, action)),
            right: Box::new(rewrite_returning_expr(right, action)),
        },
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: Box::new(rewrite_returning_expr(left, action)),
            right: Box::new(rewrite_returning_expr(right, action)),
        },
        Expr::And(a, b) => Expr::And(
            Box::new(rewrite_returning_expr(a, action)),
            Box::new(rewrite_returning_expr(b, action)),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(rewrite_returning_expr(a, action)),
            Box::new(rewrite_returning_expr(b, action)),
        ),
        Expr::Not(a) => Expr::Not(Box::new(rewrite_returning_expr(a, action))),
        Expr::Neg(a) => Expr::Neg(Box::new(rewrite_returning_expr(a, action))),
        Expr::Cast { expr, to, written } => Expr::Cast {
            expr: Box::new(rewrite_returning_expr(expr, action)),
            to: *to,
            written: written.clone(),
        },
        Expr::FieldAccess { expr, field } => Expr::FieldAccess {
            expr: Box::new(rewrite_returning_expr(expr, action)),
            field: field.clone(),
        },
        Expr::Row(elems) => Expr::Row(
            elems
                .iter()
                .map(|x| rewrite_returning_expr(x, action))
                .collect(),
        ),
        Expr::Concat(a, b) => Expr::Concat(
            Box::new(rewrite_returning_expr(a, action)),
            Box::new(rewrite_returning_expr(b, action)),
        ),
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| Box::new(rewrite_returning_expr(o, action))),
            whens: whens
                .iter()
                .map(|(k, v)| {
                    (
                        Box::new(rewrite_returning_expr(k, action)),
                        Box::new(rewrite_returning_expr(v, action)),
                    )
                })
                .collect(),
            else_: else_
                .as_ref()
                .map(|o| Box::new(rewrite_returning_expr(o, action))),
        },
        other => other.clone(),
    }
}

/// v2.01: substitute `<qual>.tableoid` references with the relation's
/// OID literal. PG allows tableoid in MERGE ON / WHEN conditions (the
/// other system columns are rejected statically), but the combined-row
/// evaluator has no provenance channel, so the constant is folded in
/// before planning. Mirrors rewrite_returning_expr's container set.
fn subst_tableoid(e: &Expr, subs: &[(String, u32)]) -> Expr {
    match e {
        Expr::Column {
            table: Some(q),
            name,
            ..
        } if name.eq_ignore_ascii_case("tableoid") => {
            match subs.iter().find(|(sq, _)| sq.eq_ignore_ascii_case(q)) {
                Some((_, oid)) => Expr::Literal(crate::sql::Literal::Int(i64::from(*oid))),
                None => e.clone(),
            }
        }
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args.iter().map(|a| subst_tableoid(a, subs)).collect(),
        },
        Expr::Arith { op, left, right } => Expr::Arith {
            op: *op,
            left: Box::new(subst_tableoid(left, subs)),
            right: Box::new(subst_tableoid(right, subs)),
        },
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: Box::new(subst_tableoid(left, subs)),
            right: Box::new(subst_tableoid(right, subs)),
        },
        Expr::And(a, b) => Expr::And(
            Box::new(subst_tableoid(a, subs)),
            Box::new(subst_tableoid(b, subs)),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(subst_tableoid(a, subs)),
            Box::new(subst_tableoid(b, subs)),
        ),
        Expr::Not(a) => Expr::Not(Box::new(subst_tableoid(a, subs))),
        Expr::Neg(a) => Expr::Neg(Box::new(subst_tableoid(a, subs))),
        Expr::Cast { expr, to, written } => Expr::Cast {
            expr: Box::new(subst_tableoid(expr, subs)),
            to: *to,
            written: written.clone(),
        },
        Expr::IsNull { expr, neg } => Expr::IsNull {
            expr: Box::new(subst_tableoid(expr, subs)),
            neg: *neg,
        },
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => Expr::Between {
            expr: Box::new(subst_tableoid(expr, subs)),
            low: Box::new(subst_tableoid(low, subs)),
            high: Box::new(subst_tableoid(high, subs)),
            neg: *neg,
        },
        other => other.clone(),
    }
}

/// v2.01: MERGE RETURNING descriptors + expanded items, for execution,
/// Describe, and CTE-body description. Mirrors `describe_returning`
/// with the source schema as extra ranges plus the old/new ranges.
#[allow(clippy::too_many_arguments)]
pub(crate) fn describe_merge_returning(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    table: &str,
    alias: Option<&str>,
    source: &[FromItem],
    with: &[CteDef],
    returning: &[SelectItem],
) -> Result<(Vec<(String, ColType)>, Vec<SelectItem>), ExecError> {
    let qual = alias.unwrap_or(table);
    let t = eng
        .db
        .find_table(table, snap, &[own], session)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
    let tgt_cols: Vec<(String, ColType)> = t.columns.clone();
    let src_schemas = from_schemas(eng, snap, own, session, source, with, &[], &[], &[])?;
    let src_flat: Vec<QCol> = src_schemas.iter().flatten().cloned().collect();
    let tgt_schema: Vec<QCol> = tgt_cols
        .iter()
        .map(|(n, ty)| QCol {
            qual: qual.to_string(),
            name: n.clone(),
            ty: *ty,
            hidden: false,
            src_ord: 0,
        })
        .collect();
    let old_schema: Vec<QCol> = tgt_schema
        .iter()
        .map(|c| QCol {
            qual: "old".to_string(),
            ..c.clone()
        })
        .collect();
    let new_schema: Vec<QCol> = tgt_schema
        .iter()
        .map(|c| QCol {
            qual: "new".to_string(),
            ..c.clone()
        })
        .collect();
    // Expand star items.
    let mut expanded: Vec<SelectItem> = Vec::new();
    for item in returning {
        match item {
            SelectItem::All => {
                for (n, _) in &tgt_cols {
                    expanded.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(qual.to_string()),
                            name: n.clone(),
                        },
                        alias: None,
                    });
                }
            }
            SelectItem::AllOf(q) if q == qual || q == "old" || q == "new" => {
                for (n, _) in &tgt_cols {
                    expanded.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(q.clone()),
                            name: n.clone(),
                        },
                        alias: None,
                    });
                }
            }
            SelectItem::AllOf(q) => {
                let range: Vec<&QCol> = src_flat.iter().filter(|c| &c.qual == q).collect();
                if range.is_empty() {
                    return Err(exec_err(
                        "42601",
                        "RETURNING qual.* for a non-target table is not supported".to_string(),
                    ));
                }
                for c in range {
                    expanded.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(q.clone()),
                            name: c.name.clone(),
                        },
                        alias: None,
                    });
                }
            }
            SelectItem::Expr { .. } => expanded.push(item.clone()),
        }
    }
    let mut comb = tgt_schema.clone();
    comb.extend(src_flat.iter().cloned());
    let refs: Vec<&[QCol]> = vec![&comb, &old_schema, &new_schema];
    let mut out = Vec::new();
    for item in &expanded {
        if let SelectItem::Expr { expr, alias } = item {
            let rexpr = rewrite_returning_expr(expr, None);
            let ty = expr_type(eng, snap, own, session, &refs, &[], &[], &rexpr)?;
            let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
            out.push((name, ty));
        }
    }
    Ok((out, expanded))
}

/// v2.01: project MERGE RETURNING for every planned action, in action
/// order. Target references see the affected row (new for INSERT /
/// UPDATE, old for DELETE); `old` / `new` see the pre / post images
/// (all-NULL on the inapplicable side); source references see the
/// joined source row (all-NULL for BY SOURCE actions).
#[allow(clippy::too_many_arguments)]
fn project_merge_returning(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    qual: &str,
    meta: &TableMeta,
    tgt_schema: &[QCol],
    src_schema: &[QCol],
    null_src: &[Value],
    acts: &[MAct],
    returning: &[SelectItem],
    ctes: &[Rc<CteBinding>],
) -> Result<(Vec<(String, ColType)>, Vec<Row>), ExecError> {
    let _ = meta;
    let (cols, expanded) = describe_merge_returning_with_schemas(
        eng, ctx, table, qual, tgt_schema, src_schema, returning,
    )?;
    let old_schema: Vec<QCol> = tgt_schema
        .iter()
        .map(|c| QCol {
            qual: "old".to_string(),
            ..c.clone()
        })
        .collect();
    let new_schema: Vec<QCol> = tgt_schema
        .iter()
        .map(|c| QCol {
            qual: "new".to_string(),
            ..c.clone()
        })
        .collect();
    let null_tgt: Vec<Value> = vec![Value::Null; tgt_schema.len()];
    let mut out_rows: Vec<Row> = Vec::new();
    for a in acts {
        let (action_name, old_v, new_v, disp_v, src_v, leaf_name, row_id): (
            &str,
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
            Vec<Value>,
            String,
            u64,
        ) = match a {
            MAct::Insert { dst, new, src } => (
                "INSERT",
                null_tgt.clone(),
                new.to_vec(),
                new.to_vec(),
                src.to_vec(),
                dst.clone(),
                u64::MAX,
            ),
            MAct::Update {
                leaf,
                id,
                old,
                new,
                src,
                ..
            } => (
                "UPDATE",
                old.to_vec(),
                new.to_vec(),
                new.to_vec(),
                src.as_ref()
                    .map(|s| s.to_vec())
                    .unwrap_or_else(|| null_src.to_vec()),
                leaf.clone(),
                *id,
            ),
            MAct::Delete {
                leaf, id, old, src, ..
            } => (
                "DELETE",
                old.to_vec(),
                null_tgt.clone(),
                old.to_vec(),
                src.as_ref()
                    .map(|s| s.to_vec())
                    .unwrap_or_else(|| null_src.to_vec()),
                leaf.clone(),
                *id,
            ),
        };
        let mut comb_schema = tgt_schema.to_vec();
        comb_schema.extend(src_schema.iter().cloned());
        let mut comb_values = disp_v.clone();
        comb_values.extend(src_v.iter().cloned());
        let scopes: Vec<(&[QCol], &[Value])> = vec![
            (&comb_schema, &comb_values),
            (&old_schema, &old_v),
            (&new_schema, &new_v),
        ];
        let prov: Vec<RowProv> = vec![
            RowProv {
                qual: qual.to_string(),
                table: leaf_name.clone(),
                row_id,
            },
            RowProv {
                qual: "old".to_string(),
                table: leaf_name.clone(),
                row_id,
            },
            RowProv {
                qual: "new".to_string(),
                table: leaf_name,
                row_id,
            },
        ];
        let mut vals = Vec::with_capacity(expanded.len());
        for item in &expanded {
            if let SelectItem::Expr { expr, .. } = item {
                let rexpr = rewrite_returning_expr(expr, Some(action_name));
                let w = Some(qwrite_from_ctx(
                    &mut *ctx.writes,
                    ctx.write_xid,
                    ctx.level,
                    ctx.default_toast_compression,
                ));
                vals.push(eval_dml_expr(
                    eng,
                    ctx.snap,
                    ctx.own,
                    ctx.session,
                    ctx.role,
                    &scopes,
                    Some(&prov),
                    &rexpr,
                    ctes,
                    None,
                    w,
                )?);
            }
        }
        out_rows.push(Row::new(vals));
    }
    Ok((cols, out_rows))
}

/// Describe MERGE RETURNING against already-built target/source
/// schemas (execution path — the source FromItems are not needed once
/// the schemas exist).
fn describe_merge_returning_with_schemas(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    qual: &str,
    tgt_schema: &[QCol],
    src_schema: &[QCol],
    returning: &[SelectItem],
) -> Result<(Vec<(String, ColType)>, Vec<SelectItem>), ExecError> {
    let _ = table;
    let old_schema: Vec<QCol> = tgt_schema
        .iter()
        .map(|c| QCol {
            qual: "old".to_string(),
            ..c.clone()
        })
        .collect();
    let new_schema: Vec<QCol> = tgt_schema
        .iter()
        .map(|c| QCol {
            qual: "new".to_string(),
            ..c.clone()
        })
        .collect();
    let mut expanded: Vec<SelectItem> = Vec::new();
    for item in returning {
        match item {
            SelectItem::All => {
                for c in tgt_schema {
                    expanded.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(qual.to_string()),
                            name: c.name.clone(),
                        },
                        alias: None,
                    });
                }
            }
            SelectItem::AllOf(q) if q == qual || q == "old" || q == "new" => {
                for c in tgt_schema {
                    expanded.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(q.clone()),
                            name: c.name.clone(),
                        },
                        alias: None,
                    });
                }
            }
            SelectItem::AllOf(q) => {
                let range: Vec<&QCol> = src_schema.iter().filter(|c| &c.qual == q).collect();
                if range.is_empty() {
                    return Err(exec_err(
                        "42601",
                        "RETURNING qual.* for a non-target table is not supported".to_string(),
                    ));
                }
                for c in range {
                    expanded.push(SelectItem::Expr {
                        expr: Expr::Column {
                            table: Some(q.clone()),
                            name: c.name.clone(),
                        },
                        alias: None,
                    });
                }
            }
            SelectItem::Expr { .. } => expanded.push(item.clone()),
        }
    }
    let mut comb = tgt_schema.to_vec();
    comb.extend(src_schema.iter().cloned());
    let refs: Vec<&[QCol]> = vec![&comb, &old_schema, &new_schema];
    let mut out = Vec::new();
    for item in &expanded {
        if let SelectItem::Expr { expr, alias } = item {
            let rexpr = rewrite_returning_expr(expr, None);
            let ty = expr_type(eng, ctx.snap, ctx.own, ctx.session, &refs, &[], &[], &rexpr)?;
            let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
            out.push((name, ty));
        }
    }
    Ok((out, expanded))
}
