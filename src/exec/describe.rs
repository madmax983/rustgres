// v1.78 mechanical split: moved verbatim from src/exec.rs (51338-53163).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// Result-column typing (Describe + execution agree via describe_select)
// ---------------------------------------------------------------------------

/// Working schemas of the FROM clause: one entry per source, in order.
/// Unknown tables are an error here (42P01); parameter inference uses
/// `unwrap_or_default` instead, so a bad table doesn't break Bind.
pub(crate) fn from_schemas(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    from: &[FromItem],
    visible: &[CteDef],
    bindings: &[Rc<CteBinding>],
    // v0.99: enclosing query's schemas, for correlated references inside
    // derived tables (PG19: a FROM-subquery sees enclosing ranges, but
    // not same-level FROM siblings).
    outer_schemas: &[&[QCol]],
    // v1.10: schemas of textually-preceding FROM items (cross-nest
    // LATERAL, PG19). Only LATERAL items may see the prefix.
    prefix_schemas: &[&[QCol]],
) -> Result<Vec<Vec<QCol>>, ExecError> {
    let mut out = Vec::new();
    for item in from {
        from_schema_item(
            eng,
            snap,
            own,
            session,
            item,
            &mut out,
            visible,
            bindings,
            outer_schemas,
            prefix_schemas,
        )?;
    }
    Ok(out)
}

pub(crate) fn from_schema_item(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    item: &FromItem,
    out: &mut Vec<Vec<QCol>>,
    visible: &[CteDef],
    bindings: &[Rc<CteBinding>],
    outer_schemas: &[&[QCol]],
    // v1.10: schemas of textually-preceding FROM items (cross-nest
    // LATERAL, PG19). Only LATERAL items may see the prefix.
    prefix_schemas: &[&[QCol]],
) -> Result<(), ExecError> {
    match item {
        FromItem::Table {
            name,
            alias,
            col_aliases,
            ..
        } => {
            // v0.23: `FROM tbl [AS] x (a, b, c)` — positional column
            // renames, applied on every describe sub-path (the executor
            // already applies them; the describe path ignored them).
            // More aliases than columns is 42601, like PostgreSQL.
            // `qual` is defined before each use below.
            let apply_aliases = |schema: Vec<QCol>| -> Result<Vec<QCol>, ExecError> {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                apply_col_aliases(schema, &qual, col_aliases)
            };
            // v0.10: materialized CTE bindings (e.g. the recursive CTE
            // currently being evaluated) shadow everything.
            if let Some(b) = bindings.iter().rev().find(|b| b.name == *name) {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                out.push(apply_aliases(
                    b.schema
                        .iter()
                        .map(|c| QCol {
                            qual: qual.clone(),
                            name: c.name.clone(),
                            ty: c.ty.clone(),

                            hidden: false,
                            src_ord: c.src_ord,
                        })
                        .collect(),
                )?);
                return Ok(());
            }
            // v0.10: CTEs shadow everything (like Postgres). `visible`
            // holds outer scopes' CTEs plus this query's own WITH list;
            // only CTEs before this one (outer + earlier siblings) are
            // visible to its body — a CTE never sees itself or later
            // siblings.
            if let Some(pos) = visible.iter().rposition(|c| c.name == *name) {
                let schema = describe_cte(eng, snap, own, session, &visible[pos], &visible[..pos])?;
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                out.push(
                    schema
                        .into_iter()
                        .map(|mut c| {
                            c.qual = qual.clone();
                            c
                        })
                        .collect(),
                );
                return Ok(());
            }
            // v0.9: information_schema virtual tables.
            if name == "information_schema.tables" {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = info_tables_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.13: pg_replication_slots is virtual too.
            if name.as_str() == "pg_replication_slots"
                && eng.db.find_table(name, snap, &[own], session).is_none()
            {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = pg_replication_slots_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            if name == "information_schema.columns" {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = info_columns_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.98: information_schema.sequences.
            if name == "information_schema.sequences" {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = info_sequences_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.9: views describe as their stored SELECT's schema.
            if let Some(view) = eng.db.find_view(name, snap, own) {
                let stmt = parse_statement(&view.query).map_err(sql_err)?;
                let select = match stmt {
                    Stmt::Select(s) => s,
                    _ => {
                        return Err(exec_err(
                            "0A000",
                            format!("view \"{}\" query is not a SELECT", name),
                        ));
                    }
                };
                let cols = describe_select(eng, snap, own, session, &select, &[], &[])?;
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = cols
                    .into_iter()
                    .enumerate()
                    .map(|(i, (cn, ty))| QCol {
                        qual: qual.clone(),
                        name: view.col_aliases.get(i).cloned().unwrap_or(cn),
                        ty,

                        hidden: false,
                        src_ord: i as u32,
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.8: the pg_stats system catalog is virtual — a real table
            // by that name takes precedence.
            if name == "pg_stats" && eng.db.find_table(name, snap, &[own], session).is_none() {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = pg_stats_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.37: pg_class is virtual too (TOAST introspection).
            if name == "pg_class" && eng.db.find_table(name, snap, &[own], session).is_none() {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = pg_class_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.88: pg_attribute is virtual too (bounded catalog subset).
            if name == "pg_attribute" && eng.db.find_table(name, snap, &[own], session).is_none() {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = pg_attribute_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.96: pg_inherits is virtual too (inheritance catalog).
            if name == "pg_inherits" && eng.db.find_table(name, snap, &[own], session).is_none() {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = pg_inherits_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.98: pg_sequences is virtual too (sequence catalog).
            if name == "pg_sequences" && eng.db.find_table(name, snap, &[own], session).is_none() {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = pg_sequences_schema()
                    .into_iter()
                    .map(|mut c| {
                        c.qual = qual.clone();
                        c
                    })
                    .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            // v0.11: the role catalogs are virtual too.
            if matches!(
                name.as_str(),
                "pg_authid" | "pg_roles" | "pg_user" | "pg_auth_members"
            ) && eng.db.find_table(name, snap, &[own], session).is_none()
            {
                let qual = alias.clone().unwrap_or_else(|| name.clone());
                let schema: Vec<QCol> = match name.as_str() {
                    "pg_roles" => pg_roles_schema(),
                    "pg_user" => pg_user_schema(),
                    "pg_auth_members" => pg_auth_members_schema(),
                    _ => pg_authid_schema(),
                }
                .into_iter()
                .map(|mut c| {
                    c.qual = qual.clone();
                    c
                })
                .collect();
                out.push(apply_aliases(schema)?);
                return Ok(());
            }
            let t = eng
                .db
                .find_table(name, snap, &[own], session)
                .ok_or_else(|| {
                    exec_err("42P01", format!("relation \"{}\" does not exist", name))
                })?;
            let qual = alias.clone().unwrap_or_else(|| name.clone());
            // v0.23: positional renames apply to plain tables too.
            let schema: Vec<QCol> = t
                .columns
                .iter()
                .enumerate()
                .map(|(i, (n, ty))| QCol {
                    qual: qual.clone(),
                    name: n.clone(),
                    ty: *ty,
                    hidden: false,
                    // v0.23: source ordinal for `qual.*` ordering.
                    src_ord: i as u32,
                })
                .collect();
            out.push(apply_aliases(schema)?);
            Ok(())
        }
        FromItem::Derived {
            sub,
            alias,
            col_aliases,
            ..
        } => {
            // v0.99: a FROM-subquery's correlated references resolve
            // against the enclosing query's schemas (PG19); same-level
            // FROM siblings stay invisible (they are not in
            // `outer_schemas`).
            let cols =
                describe_select_outer(eng, snap, own, session, sub, visible, &[], outer_schemas)?;
            // v0.23: more column aliases than output columns is 42601.
            check_col_alias_arity(alias, cols.len(), col_aliases)?;
            out.push(
                cols.into_iter()
                    .enumerate()
                    .map(|(i, (n, ty))| QCol {
                        qual: alias.clone(),
                        // v0.23: `(SELECT ...) AS s(x, y)` positional renames.
                        name: col_aliases.get(i).cloned().unwrap_or(n),
                        ty,

                        hidden: false,
                        src_ord: i as u32,
                    })
                    .collect(),
            );
            Ok(())
        }
        // v0.32: table functions describe as their output columns (all
        // text); unknown functions are 42883. v0.43: pg_input_error_info
        // describes as PG's four OUT columns. v0.46: shared
        // table_function_col_names (adds generate_series); the schema is
        // statically inferred from the argument types (same helper as
        // the executor), so wire OIDs match execution. Prior FROM items
        // (`out`) are visible to the arguments, as in the executor.
        FromItem::Function {
            name,
            args,
            alias,
            col_aliases,
            ..
        } => {
            let left: Vec<QCol> = out.iter().flatten().cloned().collect();
            let cols = lateral_function_schema(
                eng,
                snap,
                own,
                session,
                name,
                args,
                alias,
                col_aliases,
                &left,
            )?;
            out.push(cols);
            Ok(())
        }
        // v0.14: VALUES columns are `column1`, ... typed from the first
        // row's expressions (all-NULL columns describe as TEXT).
        // v0.21: explicit `AS t(a, b)` column aliases override the names.
        // v0.21: pick the highest type across all rows (float8 wins over
        // numeric), like PG's VALUES type coercion.
        FromItem::Values {
            rows,
            alias,
            col_aliases,
            ..
        } => {
            let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
            // v0.23: more column aliases than VALUES columns is 42601.
            check_col_alias_arity(alias, ncols, col_aliases)?;
            out.push(
                (0..ncols)
                    .map(|i| {
                        let ty = rows
                            .iter()
                            .filter_map(|r| r.get(i))
                            .filter_map(|e| hint_type(eng, snap, own, session, &[], e))
                            .max_by_key(|t| type_rank(t))
                            .unwrap_or(ColType::Text);
                        QCol {
                            qual: alias.clone(),
                            name: col_aliases
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| format!("column{}", i + 1)),
                            ty,

                            hidden: false,
                            src_ord: i as u32,
                        }
                    })
                    .collect(),
            );
            Ok(())
        }
        FromItem::Join {
            left,
            right,
            kind,
            using,
            natural,
            using_alias,
            alias,
            col_aliases,
            ..
        } => {
            // v0.23: a join contributes a single merged schema (same layout
            // as the executor builds), not two side-by-side schemas.
            let mut l: Vec<Vec<QCol>> = Vec::new();
            from_schema_item(
                eng,
                snap,
                own,
                session,
                left,
                &mut l,
                visible,
                bindings,
                outer_schemas,
                prefix_schemas,
            )?;
            let lflat: Vec<QCol> = l.into_iter().flatten().collect();
            // v1.10: LATERAL (explicit `LATERAL (SELECT ...)` /
            // `LATERAL (VALUES ...)` / `LATERAL func(...)`, plus the
            // v0.46 implicit-LATERAL comma case): the right schema comes
            // from the same static helper the executor uses, with the
            // left schema as the innermost enclosing scope — so Describe
            // and execution agree exactly.
            let is_comma = matches!(kind, JoinKind::Cross)
                && using.is_empty()
                && !natural
                && using_alias.is_none()
                && alias.is_none();
            let lateral_right: Option<LateralRight> = match right.as_ref() {
                FromItem::Function {
                    name,
                    args,
                    alias: falias,
                    col_aliases: fca,
                    lateral,
                } if *lateral || (is_comma && function_args_lateral(args, &lflat)) => {
                    Some(LateralRight::Func {
                        name: name.as_str(),
                        args: args.as_slice(),
                        alias: falias,
                        col_aliases: fca.as_slice(),
                    })
                }
                FromItem::Derived {
                    sub,
                    alias,
                    col_aliases,
                    lateral: true,
                } => Some(LateralRight::Derived {
                    sub,
                    alias: alias.as_str(),
                    col_aliases: col_aliases.as_slice(),
                }),
                FromItem::Values {
                    rows,
                    alias,
                    col_aliases,
                    lateral: true,
                } => Some(LateralRight::Values {
                    rows: rows.as_slice(),
                    alias: alias.as_str(),
                    col_aliases: col_aliases.as_slice(),
                }),
                _ => None,
            };
            // v1.10: PG19 forbids a CORRELATED lateral reference on
            // RIGHT/FULL joins (same rule as the executor, so EXPLAIN
            // agrees).
            if let Some(lr) = &lateral_right {
                if matches!(kind, JoinKind::Right | JoinKind::Full) {
                    let mut scope_schemas: Vec<&[QCol]> =
                        Vec::with_capacity(prefix_schemas.len() + 1);
                    scope_schemas.extend_from_slice(prefix_schemas);
                    scope_schemas.push(&lflat);
                    if lateral_is_correlated(
                        eng,
                        snap,
                        own,
                        session,
                        bindings,
                        outer_schemas,
                        &scope_schemas,
                        lr,
                    ) {
                        return Err(exec_err(
                            "42P10",
                            "invalid reference to FROM-clause entry: \
                             the combining JOIN type must be INNER or LEFT \
                             for a LATERAL reference",
                        ));
                    }
                }
            }
            let rflat: Vec<QCol> = match &lateral_right {
                Some(lr) => {
                    // v1.10: cross-nest LATERAL sees the prefix schemas
                    // between the outer query and the immediate left.
                    let mut scope_schemas: Vec<&[QCol]> =
                        Vec::with_capacity(prefix_schemas.len() + 1);
                    scope_schemas.extend_from_slice(prefix_schemas);
                    scope_schemas.push(&lflat);
                    lateral_right_schema(
                        eng,
                        snap,
                        own,
                        session,
                        bindings,
                        outer_schemas,
                        &scope_schemas,
                        lr,
                    )?
                }
                None => {
                    let mut r: Vec<Vec<QCol>> = Vec::new();
                    // v1.10: a right subtree containing a lateral item
                    // sees the left schema as preceding-sibling scope
                    // (cross-nest); a non-lateral right keeps the
                    // original prefix (siblings stay invisible).
                    let right_prefix: Vec<&[QCol]> = if from_item_has_lateral(right) {
                        let mut p: Vec<&[QCol]> = Vec::with_capacity(prefix_schemas.len() + 1);
                        p.extend_from_slice(prefix_schemas);
                        p.push(&lflat);
                        p
                    } else {
                        prefix_schemas.to_vec()
                    };
                    from_schema_item(
                        eng,
                        snap,
                        own,
                        session,
                        right,
                        &mut r,
                        visible,
                        bindings,
                        outer_schemas,
                        &right_prefix,
                    )?;
                    r.into_iter().flatten().collect()
                }
            };
            let layout = plan_join(
                &lflat,
                &rflat,
                using,
                *natural,
                using_alias.as_deref(),
                alias.as_deref(),
                col_aliases,
            )?;
            out.push(layout.schema);
            Ok(())
        }
    }
}

/// v0.10: output schema of one CTE for the Describe path. `earlier` holds
/// the CTEs visible to the body (outer scopes + earlier siblings — never
/// the CTE itself). A recursive CTE describes as its non-recursive term.
/// v1.39: describe the RETURNING columns of a data-modifying CTE body,
/// mirroring `describe_columns`' DML arms. An empty RETURNING list means
/// zero columns.
pub(crate) fn describe_cte_dml(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    stmt: &Stmt,
) -> Result<Vec<(String, ColType)>, ExecError> {
    match stmt {
        Stmt::Insert {
            table, returning, ..
        } => {
            if returning.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(describe_returning(eng, snap, own, session, table, table, &[], returning)?.0)
            }
        }
        Stmt::Update {
            table,
            alias,
            from,
            returning,
            with,
            ..
        } => {
            if returning.is_empty() {
                Ok(Vec::new())
            } else {
                let extra = from_schemas(eng, snap, own, session, from, with, &[], &[], &[])?;
                Ok(describe_returning(
                    eng,
                    snap,
                    own,
                    session,
                    table,
                    alias.as_deref().unwrap_or(table),
                    &extra,
                    returning,
                )?
                .0)
            }
        }
        Stmt::Delete {
            table,
            alias,
            using,
            returning,
            with,
            ..
        } => {
            if returning.is_empty() {
                Ok(Vec::new())
            } else {
                let extra = from_schemas(eng, snap, own, session, using, with, &[], &[], &[])?;
                Ok(describe_returning(
                    eng,
                    snap,
                    own,
                    session,
                    table,
                    alias.as_deref().unwrap_or(table),
                    &extra,
                    returning,
                )?
                .0)
            }
        }
        _ => Err(exec_err(
            "0A000",
            "data-modifying CTE body must be INSERT, UPDATE, or DELETE",
        )),
    }
}

pub(crate) fn describe_cte(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    cte: &CteDef,
    earlier: &[CteDef],
) -> Result<Vec<QCol>, ExecError> {
    // v1.39: a data-modifying CTE body describes its RETURNING list
    // (an empty RETURNING list means zero columns).
    let cols: Vec<(String, ColType)> = match &cte.body {
        CteBody::Simple(s) => describe_select_outer(eng, snap, own, session, s, earlier, &[], &[])?,
        CteBody::Union { left, .. } => {
            describe_select_outer(eng, snap, own, session, left, earlier, &[], &[])?
        }
        CteBody::Dml(stmt) => describe_cte_dml(eng, snap, own, session, stmt)?,
    };
    // v0.23: more column aliases than CTE output columns is 42601.
    check_col_alias_arity(&cte.name, cols.len(), &cte.col_aliases)?;
    Ok(cols
        .into_iter()
        .enumerate()
        .map(|(i, (name, ty))| QCol {
            qual: cte.name.clone(),
            name: cte.col_aliases.get(i).cloned().unwrap_or(name),
            ty,

            hidden: false,
            src_ord: 0,
        })
        .collect())
}

/// (name, type) of every output column. Used by Describe and by execution
/// itself, so the two can never disagree.
pub(crate) fn describe_select(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    stmt: &SelectStmt,
    bindings: &[Rc<CteBinding>],
    outer_schemas: &[&[QCol]],
) -> Result<Vec<(String, ColType)>, ExecError> {
    describe_select_outer(eng, snap, own, session, stmt, &[], bindings, outer_schemas)
}

/// v0.44: describe a set-operation root: describe every branch, require
/// equal column counts, resolve a common type per column. Names come
/// from the leftmost branch. `visible` already includes the carrier's
/// WITH.
pub(crate) fn describe_set_op(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    root: &SetOpRoot,
    visible: &[CteDef],
    bindings: &[Rc<CteBinding>],
    outer_schemas: &[&[QCol]],
) -> Result<Vec<(String, ColType)>, ExecError> {
    let mut out = describe_select_outer(
        eng,
        snap,
        own,
        session,
        &root.left,
        visible,
        bindings,
        outer_schemas,
    )?;
    for b in &root.chain {
        let op_name = match b.op {
            SetOpKind::Union => "UNION",
            SetOpKind::Intersect => "INTERSECT",
            SetOpKind::Except => "EXCEPT",
        };
        let r = describe_select_outer(
            eng,
            snap,
            own,
            session,
            &b.right,
            visible,
            bindings,
            outer_schemas,
        )?;
        if r.len() != out.len() {
            return Err(exec_err(
                "42804",
                format!("each {op_name} query must have the same number of columns"),
            ));
        }
        let mut resolved: Vec<ColType> = Vec::with_capacity(out.len());
        for ((_, lty), (_, rty)) in out.iter().zip(r.iter()) {
            resolved.push(common_supertype(op_name, lty, rty)?);
        }
        for (i, ty) in resolved.into_iter().enumerate() {
            out[i].1 = ty;
        }
    }
    Ok(out)
}

/// v0.10: `outer` holds the CTE definitions visible from enclosing query
/// levels (innermost last); the query's own WITH list is appended, so a
/// CTE body only sees outer CTEs and earlier siblings.
pub(crate) fn describe_select_outer(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    stmt: &SelectStmt,
    outer: &[CteDef],
    bindings: &[Rc<CteBinding>],
    outer_schemas: &[&[QCol]],
) -> Result<Vec<(String, ColType)>, ExecError> {
    let mut visible: Vec<CteDef> = outer.to_vec();
    visible.extend(stmt.with.iter().cloned());
    // v0.44: a set-operation carrier describes via its branches: names
    // from the leftmost branch, types resolved per column. The carrier's
    // WITH (now in `visible`) applies to every branch.
    if let Some(root) = &stmt.set_op {
        return describe_set_op(
            eng,
            snap,
            own,
            session,
            root,
            &visible,
            bindings,
            outer_schemas,
        );
    }
    let schemas = from_schemas(
        eng,
        snap,
        own,
        session,
        &stmt.from,
        &visible,
        bindings,
        outer_schemas,
        &[],
    )?;
    let refs: Vec<&[QCol]> = schemas.iter().map(|s| s.as_slice()).collect();
    let mut out = Vec::new();
    for item in &stmt.items {
        match item {
            SelectItem::All => {
                for s in &schemas {
                    // v0.23: hidden columns are skipped by `*`.
                    for c in s.iter().filter(|c| !c.hidden) {
                        out.push((c.name.clone(), c.ty.clone()));
                    }
                }
            }
            SelectItem::AllOf(qual) => {
                // v0.23: expand in the qualifier's source-column order.
                let mut any = false;
                for s in &schemas {
                    for i in qual_star_order(s, qual) {
                        let c = &s[i];
                        out.push((c.name.clone(), c.ty.clone()));
                        any = true;
                    }
                }
                if !any {
                    return Err(exec_err(
                        "42P01",
                        format!("missing FROM-clause entry for table \"{}\"", qual),
                    ));
                }
            }
            SelectItem::Expr { expr, alias } => {
                let ty = expr_type(
                    eng,
                    snap,
                    own,
                    session,
                    &refs,
                    outer_schemas,
                    &visible,
                    expr,
                )?;
                let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
                out.push((name, ty));
            }
        }
    }
    Ok(out)
}

/// Default output column name when there is no alias: the column name for
/// bare refs, the function name for aggregates, the type name for casts
/// (v0.14, like Postgres), `?column?` otherwise.
/// v0.37: PG19's `FigureColnameInternal` (parse_target.c) with its
/// confidence strength: 2 = good name (column, function), 1 =
/// second-best (a cast falling back to the type name), 0 = no
/// information (`?column?`). A cast keeps its argument's name only at
/// strength 2; otherwise the type name wins (`'x'::text` -> `text`,
/// `'x'::text::varchar` -> `varchar`).
pub(crate) fn expr_col_name_strength(e: &Expr) -> (String, u8) {
    match e {
        Expr::Column { name, .. } => (name.clone(), 2),
        // v0.73: PG19 names an unaliased whole-row Var after the range
        // (`SELECT tbl` -> `tbl`).
        Expr::WholeRow { qual } => (qual.clone(), 2),
        // v0.73: PG19 names a scalar subquery's output column after the
        // subquery's own output column (`SELECT (SELECT view_a)` -> `view_a`).
        Expr::ScalarSub(sub) => match sub.items.as_slice() {
            [crate::sql::SelectItem::Expr { expr, alias }] => {
                if let Some(a) = alias {
                    (a.clone(), 2)
                } else {
                    expr_col_name_strength(expr)
                }
            }
            _ => ("?column?".to_string(), 0),
        },
        Expr::Agg { func, .. } => (func.name().to_string(), 2),
        // v1.30: PG19 names the column after the ordered-set
        // aggregate (`FigureColName`: the FuncCall name).
        Expr::WithinGroup { func, .. } => (func.name().to_string(), 2),
        Expr::Cast { expr, to, written } => {
            // v0.93: a func-style cast (`float8(q1)`) is named after the
            // type name as written (PG19 treats the type-named call like
            // a function call for output naming).
            if let Some(w) = written {
                return (w.clone(), 2);
            }
            let (inner, s) = expr_col_name_strength(expr);
            if s <= 1 {
                (to.pg_typname(), 1)
            } else {
                (inner, s)
            }
        }
        // v0.18: function calls are named after the function (SELECT sqrt(2) -> "sqrt").
        Expr::Func { name, .. } => (name.clone(), 2),
        // v0.55: PG names an unaliased CASE output column "case".
        Expr::Case { .. } => ("case".to_string(), 2),
        // v1.43: PG19's `FigureColnameInternal` (parse_target.c,
        // `T_RowExpr`): "make ROW() act like a function" — the column
        // name is "row" at strength 2 (so a cast over ROW() keeps
        // "row", like PG).
        Expr::Row(..) => ("row".to_string(), 2),
        // v1.23: PG19's `FigureColnameInternal` for `T_A_Indirection`
        // (parse_target.c): a subscript/slice chain with no field name
        // in the indirection takes its column name from the operand, so
        // `(SELECT ARRAY[1,2,3])[1]` and `(array[1,2])[(SELECT 1)]` are
        // named `array`, while `arr[1]` keeps the column name `arr`.
        Expr::Subscript { array, .. } => expr_col_name_strength(array),
        Expr::Slice { array, .. } => expr_col_name_strength(array),
        // v1.23: PG19 names an `ARRAY[...]` constructor `array`
        // (`FigureColnameInternal`, `T_A_ArrayExpr` case).
        Expr::ArrayCtor { .. } => ("array".to_string(), 2),
        // v1.23: PG19 names `ARRAY(subselect)` `array`
        // (`FigureColnameInternal`, `T_SubLink`/`ARRAY_SUBLINK` case).
        Expr::ArraySubquery(_) => ("array".to_string(), 2),
        // v0.75: PG names an unaliased EXISTS output column "exists".
        Expr::Exists { .. } => ("exists".to_string(), 2),
        _ => ("?column?".to_string(), 0),
    }
}

pub(crate) fn expr_col_name(e: &Expr) -> String {
    expr_col_name_strength(e).0
}

pub(crate) fn expr_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    outer: &[&[QCol]],
    ctes: &[CteDef],
    e: &Expr,
) -> Result<ColType, ExecError> {
    match e {
        // v0.95: a named arg has its inner expression's type.
        Expr::NamedArg { expr, .. } => {
            expr_type(eng, snap, own, session, schemas, outer, ctes, expr)
        }
        Expr::Column { table, name } => {
            // v0.73: PG19 correlated references — a subquery's expressions
            // resolve against the enclosing query's ranges after its own
            // (ambiguity is reported per level, like PG).
            let scopes: Vec<Scope> = schemas
                .iter()
                .map(|s| Scope {
                    schema: s,
                    row: &[],
                    prov: None,
                })
                .collect();
            let oscopes: Vec<Scope> = outer
                .iter()
                .map(|s| Scope {
                    schema: s,
                    row: &[],
                    prov: None,
                })
                .collect();
            // v0.73: PG19 whole-row fallback (see eval_expr): a bare range
            // name has the `record` pseudo-type.
            let is_range = |ss: &[&[QCol]]| {
                table.is_none() && ss.iter().any(|s| s.iter().any(|c| c.qual == *name))
            };
            match resolve_col(&scopes, table.as_deref(), name) {
                Ok((si, ci)) => Ok(schemas[si][ci].ty.clone()),
                Err(e) if e.code == "42703" => {
                    // v1.13: `tableoid` system column — typed as integer
                    // (the OID; PG's `oid` displays as a number, like our
                    // pg_class.oid). A user column wins (resolved above).
                    if name == "tableoid" {
                        return Ok(ColType::Int);
                    }
                    // v1.17: `xmin`/`xmax` system columns — typed as xid
                    // (OID 28), like PG.
                    if name == "xmin" || name == "xmax" {
                        return Ok(ColType::Xid);
                    }
                    // v1.40: `ctid` system column — typed as tid (OID
                    // 27), like PG. `cmin`/`cmax` are typed as xid
                    // (see eval_cmin_cmax for why).
                    if name == "ctid" {
                        return Ok(ColType::Tid);
                    }
                    if name == "cmin" || name == "cmax" {
                        return Ok(ColType::Xid);
                    }
                    match resolve_col(&oscopes, table.as_deref(), name) {
                        Ok((si, ci)) => Ok(outer[si][ci].ty.clone()),
                        Err(e2) => {
                            if e2.code == "42703" && (is_range(schemas) || is_range(outer)) {
                                return Ok(ColType::Record);
                            }
                            Err(if e2.code == "42703" { e } else { e2 })
                        }
                    }
                }
                Err(e) => Err(e),
            }
        }
        // v0.73: a whole-row Var has PG's `record` pseudo-type (the
        // per-table rowtype OID is a documented gap; 2249 is genuine).
        // An unknown qualifier is PG19's 42703.
        Expr::WholeRow { qual } => {
            // v0.73: a correlated whole-row Var may name an outer range.
            if !schemas.iter().any(|s| s.iter().any(|c| c.qual == *qual))
                && !outer.iter().any(|s| s.iter().any(|c| c.qual == *qual))
            {
                return Err(exec_err(
                    "42703",
                    format!("missing FROM-clause entry for table \"{qual}\""),
                ));
            }
            Ok(ColType::Record)
        }
        // Pre-resolved: the type lives at the same position in the schemas.
        Expr::ResolvedCol { frame, idx } => schemas
            .get(*frame)
            .and_then(|s| s.get(*idx))
            .map(|c| c.ty.clone())
            .ok_or_else(|| exec_err("XX000", "internal error: resolved column out of range")),
        Expr::Literal(lit) => Ok(lit.col_type()),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Arith { op, left, right } => {
            let ta = arith_operand_type(eng, snap, own, session, schemas, outer, ctes, left, *op)?;
            let tb = arith_operand_type(eng, snap, own, session, schemas, outer, ctes, right, *op)?;
            combine_arith_types(*op, ta, tb)
        }
        Expr::Cast { to, .. } => Ok(*to),
        // v0.81: `ROW(a, b, ...)` evaluates to a composite record value.
        Expr::Row(elems) => {
            for e in elems {
                expr_type(eng, snap, own, session, schemas, outer, ctes, e)?;
            }
            Ok(ColType::Record)
        }
        // v0.81: `expr::named_composite` — the name must denote a defined
        // composite type (42704 otherwise, like the execution-time check
        // in eval_cast_named); the result is the named composite marker.
        // v0.85: a domain name is also accepted — the cast's result type
        // is the domain's base type.
        Expr::CastNamed { expr, name } => {
            expr_type(eng, snap, own, session, schemas, outer, ctes, expr)?;
            match eng.db.types.get(name) {
                Some(st) if st.composite.is_some() => Ok(ColType::Composite),
                Some(st) => match st.domain.as_ref() {
                    Some(dom) => Ok(dom.base.clone()),
                    // v1.38: LIKE types carry their base's physical
                    // type (a `::xfloat4` value is physically a
                    // float4); unknown shells still 42704.
                    None => match st.like_base.as_deref() {
                        Some(base) => crate::sql::coltype_by_name(base).map_err(|_| {
                            exec_err("42704", format!("type \"{}\" does not exist", name))
                        }),
                        None => Err(exec_err(
                            "42704",
                            format!("type \"{}\" does not exist", name),
                        )),
                    },
                },
                _ => {
                    // v0.86: table rowtypes (PG19: every table has a
                    // composite rowtype).
                    if eng.db.find_table(name, snap, &[own], session).is_some() {
                        return Ok(ColType::Composite);
                    }
                    Err(exec_err(
                        "42704",
                        format!("type \"{}\" does not exist", name),
                    ))
                }
            }
        }
        // v0.81: `(expr).field` — on a `ROW(...)` literal the field
        // resolves positionally to the element's type (42703 for an
        // unknown field, like execution); other composites carry PG's
        // record pseudo-type since the field type isn't tracked.
        Expr::FieldAccess { expr, field } => {
            if let Expr::Row(elems) = &**expr {
                for (i, e) in elems.iter().enumerate() {
                    if field == &format!("f{}", i + 1) {
                        return expr_type(eng, snap, own, session, schemas, outer, ctes, e);
                    }
                }
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" not found in record", field),
                ));
            }
            expr_type(eng, snap, own, session, schemas, outer, ctes, expr)?;
            Ok(ColType::Record)
        }
        Expr::BitNot(x) => {
            // v0.25: `~smallint`/`~int` -> int, `~bigint` -> bigint (PG
            // promotes smallint, having no int2not).
            // v0.53: `~ NULL` is NULL::int (PG resolves the unknown-type
            // NULL to int4 for `~`); eval_bitnot_val already NULL-propagates.
            if matches!(**x, Expr::Literal(Literal::Null)) {
                return Ok(ColType::Int);
            }
            match expr_type(eng, snap, own, session, schemas, outer, ctes, x)? {
                ColType::BigInt => Ok(ColType::BigInt),
                ColType::SmallInt | ColType::Int => Ok(ColType::Int),
                t => Err(exec_err(
                    "42883",
                    format!("operator does not exist: ~ {}", t.sql_name()),
                )),
            }
        }
        Expr::Neg(x) => {
            // v0.53: PG19 doNegate preserves the operand type (`-int2`
            // stays int2 — the old `0 - x` desugar widened it to int).
            // An unknown-type (text) literal resolves to integer, as
            // the old `0 - text` typing did.
            match expr_type(eng, snap, own, session, schemas, outer, ctes, x)? {
                t @ (ColType::SmallInt
                | ColType::Int
                | ColType::BigInt
                | ColType::Float4
                | ColType::Float
                | ColType::Numeric(..)) => Ok(t),
                ColType::Text => Ok(ColType::Int),
                t => Err(exec_err(
                    "42883",
                    format!("operator does not exist: - {}", t.sql_name()),
                )),
            }
        }
        // v0.55: PG19 CASE result type = common type of all result
        // arms (+ ELSE). For simple CASE, the operand and WHEN keys
        // must additionally share a comparable type (unknown/NULL
        // literals don't constrain, like PG's unknown-type coercion).
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(op) = operand {
                let mut cmp_acc: Option<ColType> = None;
                let mut keys: Vec<&Expr> = Vec::with_capacity(whens.len() + 1);
                keys.push(op);
                for (k, _) in whens.iter() {
                    keys.push(k);
                }
                for k in keys {
                    if matches!(
                        k,
                        Expr::Literal(Literal::Null) | Expr::Literal(Literal::Text(_))
                    ) {
                        continue;
                    }
                    let t = expr_type(eng, snap, own, session, schemas, outer, ctes, k)?;
                    cmp_acc = Some(match cmp_acc {
                        Some(a) => common_supertype("CASE", &a, &t)?,
                        None => t,
                    });
                }
            }
            case_result_type(eng, snap, own, session, schemas, ctes, whens, else_)
        }
        Expr::Concat(..) => Ok(ColType::Text),
        Expr::Cmp { .. }
        | Expr::And(_, _)
        | Expr::Or(_, _)
        | Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::IsBool { .. }
        | Expr::IsDistinctFrom { .. }
        | Expr::Like { .. }
        | Expr::Regex { .. }
        | Expr::Between { .. }
        | Expr::InSub { .. }
        | Expr::Quantified { .. }
        | Expr::Exists { .. } => Ok(ColType::Bool),
        // v0.87: user-defined operator — the return type comes from the
        // operator's procedure function.
        Expr::UserOp { op, left, right } => {
            let lt = expr_type(eng, snap, own, session, schemas, outer, ctes, left)?;
            let rt = expr_type(eng, snap, own, session, schemas, outer, ctes, right)?;
            let (_, fdef) =
                resolve_user_operator(eng, op, &coltype_op_name(&lt), &coltype_op_name(&rt))
                    .ok_or_else(|| exec_err("42883", format!("operator does not exist: {}", op)))?;
            resolve_func_type_name(eng, snap, own, session, &fdef.ret_type)
        }
        Expr::Func { name, args } => {
            func_result_type(name, args, eng, snap, own, session, schemas, outer, ctes)
        }
        Expr::Extract { .. } => Ok(ColType::Numeric(None)),
        Expr::Agg {
            func,
            arg,
            distinct: _,
            arg2,
            ..
        } => agg_result_type(
            eng,
            snap,
            own,
            session,
            schemas,
            outer,
            ctes,
            *func,
            arg.as_deref(),
            arg2.as_deref(),
        ),
        // v1.30: PG19 ordered-set aggregate (WITHIN GROUP).
        Expr::WithinGroup {
            func,
            direct_args,
            within_order_by,
            ..
        } => within_group_result_type(
            eng,
            snap,
            own,
            session,
            schemas,
            outer,
            ctes,
            *func,
            direct_args,
            within_order_by,
        ),
        Expr::ScalarSub(sub) => {
            // v0.73: the subquery sees this level's ranges (plus any
            // enclosing ones) as correlated outer scopes.
            let mut sub_outer: Vec<&[QCol]> = Vec::with_capacity(schemas.len() + outer.len());
            sub_outer.extend_from_slice(schemas);
            sub_outer.extend_from_slice(outer);
            let cols = describe_select_outer(eng, snap, own, session, sub, ctes, &[], &sub_outer)?;
            if cols.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            Ok(cols[0].clone().1)
        }
        // v0.78: `array(SELECT ...)` — a proper array type over the
        // subquery's (single) column type, with PG's array OID. Values
        // are still carried as the `{...}` literal text (exactly what
        // array_out puts on the wire); a Value::Array variant with
        // element-wise operations is future work.
        Expr::ArraySubquery(sub) => {
            let mut sub_outer: Vec<&[QCol]> = Vec::with_capacity(schemas.len() + outer.len());
            sub_outer.extend_from_slice(schemas);
            sub_outer.extend_from_slice(outer);
            let cols = describe_select_outer(eng, snap, own, session, sub, ctes, &[], &sub_outer)?;
            if cols.len() != 1 {
                return Err(exec_err("42601", "subquery must return only one column"));
            }
            let elem = cols[0].clone().1;
            // PG flattens nested arrays: `array(SELECT array(...))`
            // keeps the innermost element's array OID.
            Ok(ColType::Array(ArrayElem::of(&elem)))
        }
        // v0.79: real array expressions (mirror the runtime typing in
        // array_ctor_from_vals / eval_subscript_val / eval_slice_val so
        // RowDescription agrees with execution).
        Expr::ArrayCtor { elems, nested } => {
            let r = |e: &Expr| expr_type(eng, snap, own, session, schemas, outer, ctes, e);
            if elems.is_empty() {
                return Err(exec_err("42P08", "cannot determine type of empty array"));
            }
            if *nested {
                let mut elem_ty: Option<crate::storage::ArrayElem> = None;
                for e in elems {
                    match r(e)? {
                        ColType::Array(ae) => {
                            if let Some(prev) = elem_ty {
                                if prev != ae {
                                    return Err(exec_err(
                                        "22P02",
                                        "multidimensional arrays must have array expressions with matching dimensions",
                                    ));
                                }
                            } else {
                                elem_ty = Some(ae);
                            }
                        }
                        other => {
                            return Err(exec_err(
                                "22P02",
                                format!(
                                    "multidimensional arrays must have array expressions with matching dimensions, not {}",
                                    other.sql_name()
                                ),
                            ));
                        }
                    }
                }
                Ok(ColType::Array(
                    elem_ty.unwrap_or(crate::storage::ArrayElem::Text),
                ))
            } else {
                let mut acc: Option<ColType> = None;
                for e in elems {
                    // NULL literals don't constrain the common type
                    // (PG's unknown literals), mirroring
                    // array_ctor_from_vals.
                    if matches!(e, Expr::Literal(Literal::Null)) {
                        continue;
                    }
                    let t = r(e)?;
                    acc = Some(match acc {
                        Some(a) => common_supertype("ARRAY", &a, &t)?,
                        None => t,
                    });
                }
                let elem_ty = acc.unwrap_or(ColType::Text);
                Ok(ColType::Array(crate::storage::ArrayElem::of(&elem_ty)))
            }
        }
        Expr::Subscript { array, indices } => {
            match expr_type(eng, snap, own, session, schemas, outer, ctes, array)? {
                ColType::Array(e) => {
                    for i in indices {
                        check_array_index_type(eng, snap, own, session, schemas, outer, ctes, i)?;
                    }
                    Ok(elem_scalar_type(e))
                }
                other => Err(exec_err(
                    "42804",
                    format!("cannot subscript type {}", other.sql_name()),
                )),
            }
        }
        Expr::Slice { array, bounds } => {
            match expr_type(eng, snap, own, session, schemas, outer, ctes, array)? {
                ct @ ColType::Array(_) => {
                    for (l, u) in bounds {
                        if let Some(l) = l {
                            check_array_index_type(
                                eng, snap, own, session, schemas, outer, ctes, l,
                            )?;
                        }
                        if let Some(u) = u {
                            check_array_index_type(
                                eng, snap, own, session, schemas, outer, ctes, u,
                            )?;
                        }
                    }
                    Ok(ct)
                }
                other => Err(exec_err(
                    "42804",
                    format!("cannot subscript type {}", other.sql_name()),
                )),
            }
        }
        // v0.10: window function result types.
        Expr::Window { func, args, .. } => {
            window_result_type(eng, snap, own, session, schemas, outer, ctes, func, args)
        }
    }
}

/// v0.79: PG19 `array_subscript_transform` coerces every subscript
/// (and slice bound) to int4, raising 42804 "array subscript must
/// have type integer" when the index expression's type is not
/// integer. A NULL literal has unknown type (coercible; a NULL
/// index yields NULL at execution, like PG).
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_array_index_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    outer: &[&[QCol]],
    ctes: &[CteDef],
    e: &Expr,
) -> Result<(), ExecError> {
    if matches!(e, Expr::Literal(Literal::Null)) {
        return Ok(());
    }
    match expr_type(eng, snap, own, session, schemas, outer, ctes, e)? {
        ColType::SmallInt | ColType::Int | ColType::BigInt => Ok(()),
        other => Err(exec_err(
            "42804",
            format!(
                "array subscript must have type integer, not {}",
                other.sql_name()
            ),
        )),
    }
}

/// v0.10: result type of a window function.
pub(crate) fn window_result_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    outer: &[&[QCol]],
    ctes: &[CteDef],
    func: &WindowFunc,
    args: &[Expr],
) -> Result<ColType, ExecError> {
    match func {
        // Postgres returns bigint for the ranking functions, integer
        // for ntile.
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => Ok(ColType::BigInt),
        WindowFunc::Ntile => Ok(ColType::Int),
        // lag/lead/first_value/last_value/nth_value: type of the value
        // argument.
        WindowFunc::Lag
        | WindowFunc::Lead
        | WindowFunc::FirstValue
        | WindowFunc::LastValue
        | WindowFunc::NthValue => {
            let a = args
                .first()
                .ok_or_else(|| exec_err("42883", "window function requires an argument"))?;
            expr_type(eng, snap, own, session, schemas, outer, ctes, a)
        }
        WindowFunc::Agg(f) => {
            let arg = args.first();
            let arg2 = args.get(1);
            agg_result_type(eng, snap, own, session, schemas, outer, ctes, *f, arg, arg2)
        }
    }
}

/// Operand type of an arithmetic operator for the description pass;
/// NULL contributes nothing.
pub(crate) fn arith_operand_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    outer: &[&[QCol]],
    ctes: &[CteDef],
    e: &Expr,
    op: ArithOp,
) -> Result<Option<ColType>, ExecError> {
    match e {
        Expr::Literal(Literal::Null) => Ok(None),
        Expr::Literal(lit) => Ok(Some(lit.col_type())),
        Expr::Param(n) => Err(exec_err("42P02", format!("there is no parameter ${}", n))),
        Expr::Column { .. } | Expr::ResolvedCol { .. } => Ok(Some(expr_type(
            eng, snap, own, session, schemas, outer, ctes, e,
        )?)),
        Expr::Arith {
            op: inner,
            left,
            right,
        } => {
            let ta =
                arith_operand_type(eng, snap, own, session, schemas, outer, ctes, left, *inner)?;
            let tb =
                arith_operand_type(eng, snap, own, session, schemas, outer, ctes, right, *inner)?;
            Ok(Some(combine_arith_types(*inner, ta, tb)?))
        }
        Expr::Agg { .. } | Expr::WithinGroup { .. } | Expr::ScalarSub(_) => Ok(Some(expr_type(
            eng, snap, own, session, schemas, outer, ctes, e,
        )?)),
        Expr::Cast { to, .. } => Ok(Some(*to)),
        Expr::Func { .. } => Ok(Some(expr_type(
            eng, snap, own, session, schemas, outer, ctes, e,
        )?)),
        Expr::Concat(..) => Ok(Some(ColType::Text)),
        // v0.25: `~x` result type via the shared expr_type rule.
        Expr::BitNot(_) => Ok(Some(expr_type(
            eng, snap, own, session, schemas, outer, ctes, e,
        )?)),
        // v0.53: `-x` result type via the shared expr_type rule.
        Expr::Neg(_) => Ok(Some(expr_type(
            eng, snap, own, session, schemas, outer, ctes, e,
        )?)),
        // v0.55: best-effort CASE hint = common type of the result
        // arms' hints (unknown literals skipped, conflicts ignored).
        Expr::Case { whens, else_, .. } => {
            let mut acc: Option<ColType> = None;
            let mut arms: Vec<&Expr> = Vec::with_capacity(whens.len() + 1);
            for (_, r) in whens.iter() {
                arms.push(r);
            }
            if let Some(e) = else_.as_deref() {
                arms.push(e);
            }
            for a in arms {
                if matches!(
                    a,
                    Expr::Literal(Literal::Null) | Expr::Literal(Literal::Text(_))
                ) {
                    continue;
                }
                if let Some(t) = hint_type(eng, snap, own, session, schemas, a) {
                    acc = Some(match acc {
                        Some(prev) => common_supertype("CASE", &prev, &t).unwrap_or(prev),
                        None => t,
                    });
                }
            }
            Ok(acc)
        }
        // Boolean / predicate expressions can't be arithmetic operands.
        _ => Err(exec_err(
            "42883",
            format!("operator does not exist: boolean {} integer", op.sql()),
        )),
    }
}

/// Rank in the numeric promotion lattice (v0.7):
/// smallint < integer < bigint < real < double precision < numeric.
/// v0.21: Postgres numeric promotion order — smallint < integer <
/// bigint < numeric < real < double precision — matching `NumCat`,
/// so the static result type agrees with runtime promotion
/// (float8 beats numeric).
pub(crate) fn numeric_rank(t: &ColType) -> Option<u8> {
    match t {
        ColType::SmallInt => Some(0),
        ColType::Int => Some(1),
        ColType::BigInt => Some(2),
        ColType::Numeric(..) => Some(3),
        ColType::Float4 => Some(4),
        ColType::Float => Some(5),
        _ => None,
    }
}

pub(crate) fn rank_type(rank: u8) -> ColType {
    match rank {
        0 => ColType::SmallInt,
        1 => ColType::Int,
        2 => ColType::BigInt,
        3 => ColType::Numeric(None),
        4 => ColType::Float4,
        _ => ColType::Float,
    }
}

/// Result type of `a <op> b` given operand types (None = NULL/unknown
/// side). Date arithmetic: date +/- integer-kind -> date, date - date
/// -> integer. Anything else mismatched is 42883.
pub(crate) fn combine_arith_types(
    op: ArithOp,
    a: Option<ColType>,
    b: Option<ColType>,
) -> Result<ColType, ExecError> {
    let op_err = |x: &ColType, y: &ColType| {
        exec_err(
            "42883",
            format!(
                "operator does not exist: {} {} {}",
                x.sql_name(),
                op.sql(),
                y.sql_name()
            ),
        )
    };
    match (a, b) {
        (None, None) => Ok(ColType::Int),
        (None, Some(t)) | (Some(t), None) => Ok(t),
        (Some(x), Some(y)) => {
            // Date arithmetic (v0.7): date +/- int-kind -> date,
            // date - date -> integer (days). No intervals in v0.7.
            let is_int_kind =
                |t: &ColType| matches!(t, ColType::SmallInt | ColType::Int | ColType::BigInt);
            match (&x, &y) {
                (ColType::Date, y) if is_int_kind(y) => match op {
                    ArithOp::Add | ArithOp::Sub => Ok(ColType::Date),
                    _ => Err(op_err(&x, &y)),
                },
                // `int + date` commutes; `int - date` is undefined.
                (x, ColType::Date) if is_int_kind(x) => match op {
                    ArithOp::Add => Ok(ColType::Date),
                    _ => Err(op_err(&x, &y)),
                },
                (ColType::Date, ColType::Date) => match op {
                    ArithOp::Sub => Ok(ColType::Int),
                    _ => Err(op_err(&x, &y)),
                },
                // v0.21: an unknown-type (text) literal against a numeric
                // operand resolves to the numeric type, like Postgres;
                // the runtime parses the text in coerce_text_numeric.
                (x, ColType::Text) if numeric_rank(x).is_some() => Ok(*x),
                (ColType::Text, y) if numeric_rank(y).is_some() => Ok(*y),
                (x, y)
                    if matches!(
                        op,
                        ArithOp::BitAnd
                            | ArithOp::BitOr
                            | ArithOp::BitXor
                            | ArithOp::Shl
                            | ArithOp::Shr
                    ) =>
                {
                    // v0.51: bitwise operators only accept the integer
                    // kinds; the result type is the operand max kind —
                    // int2 pairs stay int2 (PG19 pg_operator.dat
                    // L4398-4410; the v0.25 int2->int4 promotion was
                    // wrong, like the arithmetic one fixed in v0.50).
                    match (numeric_rank(x), numeric_rank(y)) {
                        (Some(rx), Some(ry)) if rx <= 2 && ry <= 2 => Ok(rank_type(rx.max(ry))),
                        _ => Err(op_err(x, y)),
                    }
                }
                (x, y) => match (numeric_rank(x), numeric_rank(y)) {
                    (Some(rx), Some(ry)) => {
                        if op == ArithOp::Pow {
                            // Postgres `^`: numeric for exact inputs,
                            // float8 when either side is floating.
                            return Ok(if rx.max(ry) >= 4 {
                                ColType::Float
                            } else {
                                ColType::Numeric(None)
                            });
                        }
                        if op == ArithOp::Mod && matches!(rx.max(ry), 4 | 5) {
                            // Postgres defines % only for the exact numeric
                            // types (not real/double).
                            return Err(op_err(x, y));
                        }
                        // v0.50: int2 <op> int2 -> int2 (Postgres int2pl etc.
                        // return smallint; there is no promotion to int4).
                        Ok(rank_type(rx.max(ry)))
                    }
                    _ => Err(op_err(x, y)),
                },
            }
        }
    }
}

pub(crate) fn numeric_agg_arg(func: &str, t: &ColType) -> Result<(), ExecError> {
    match t {
        ColType::SmallInt
        | ColType::Int
        | ColType::BigInt
        | ColType::Float4
        | ColType::Float
        | ColType::Numeric(..) => Ok(()),
        _ => Err(exec_err(
            "42883",
            format!("function {}({}) does not exist", func, t.sql_name()),
        )),
    }
}

pub(crate) fn agg_result_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    outer: &[&[QCol]],
    ctes: &[CteDef],
    func: AggFunc,
    arg: Option<&Expr>,
    arg2: Option<&Expr>,
) -> Result<ColType, ExecError> {
    match func {
        // Postgres count() returns bigint.
        AggFunc::Count => Ok(ColType::BigInt),
        AggFunc::Avg => {
            if let Some(a) = arg {
                numeric_agg_arg(
                    "avg",
                    &expr_type(eng, snap, own, session, schemas, outer, ctes, a)?,
                )?;
            }
            Ok(ColType::Float)
        }
        AggFunc::Sum => match arg {
            // Only COUNT takes `*`; the parser guarantees it.
            None => Ok(ColType::Int),
            Some(a) => {
                let t = expr_type(eng, snap, own, session, schemas, outer, ctes, a)?;
                numeric_agg_arg("sum", &t)?;
                // v0.76: PG19 widening — sum(smallint/int) -> bigint,
                // sum(bigint) -> numeric.
                match t {
                    ColType::SmallInt | ColType::Int => Ok(ColType::BigInt),
                    ColType::BigInt => Ok(ColType::Numeric(None)),
                    _ => Ok(t),
                }
            }
        },
        AggFunc::Min | AggFunc::Max => {
            let a = arg.expect("min/max always take an argument");
            expr_type(eng, snap, own, session, schemas, outer, ctes, a)
        }
        AggFunc::StringAgg => {
            // Delimiter should be text-ish; be permissive here (the
            // executor coerces via casts) and just require an argument.
            let a = arg.expect("string_agg always takes arguments");
            let t = expr_type(eng, snap, own, session, schemas, outer, ctes, a)?;
            if !matches!(t, ColType::Text) {
                return Err(exec_err(
                    "42883",
                    format!("function string_agg({}) does not exist", t.sql_name()),
                ));
            }
            let _ = arg2;
            Ok(ColType::Text)
        }
        AggFunc::BoolAnd => {
            let a = arg.expect("bool_and always takes an argument");
            let t = expr_type(eng, snap, own, session, schemas, outer, ctes, a)?;
            if !matches!(t, ColType::Bool) {
                return Err(exec_err(
                    "42883",
                    format!("function bool_and({}) does not exist", t.sql_name()),
                ));
            }
            Ok(ColType::Bool)
        }
        // v0.91: array_agg(anyelement) -> anyarray; the element type is
        // the argument's scalar element (arrays flatten, like PG).
        AggFunc::ArrayAgg => {
            let a = arg.expect("array_agg always takes an argument");
            let t = expr_type(eng, snap, own, session, schemas, outer, ctes, a)?;
            Ok(ColType::Array(crate::storage::ArrayElem::of(&t)))
        }
        AggFunc::VarianceSamp | AggFunc::VariancePop | AggFunc::StddevSamp | AggFunc::StddevPop => {
            let a = arg.expect("variance/stddev always take an argument");
            let t = expr_type(eng, snap, own, session, schemas, outer, ctes, a)?;
            // PG: variance(int-kind) -> numeric; variance(float) ->
            // double precision; variance(numeric) -> numeric.
            match t {
                ColType::SmallInt | ColType::Int | ColType::BigInt | ColType::Numeric(_) => {
                    Ok(ColType::Numeric(None))
                }
                ColType::Float4 | ColType::Float => Ok(ColType::Float),
                _ => Err(exec_err(
                    "42883",
                    format!("function {}({}) does not exist", func.name(), t.sql_name()),
                )),
            }
        }
    }
}

/// v1.30: static result type of a PG19 ordered-set aggregate
/// (WITHIN GROUP), mirroring `agg_result_type`. PG19 pg_proc
/// signatures:
/// - `percentile_cont(float8)` -> float8, `percentile_cont(float8[])`
///   -> float8[] (the interval variant is out of scope: rustgres has
///   no Interval value type)
/// - `percentile_disc(float8)` -> the sort expression's type,
///   `percentile_disc(float8[])` -> array of it
/// - `mode()` -> the sort expression's type
/// - `rank` / `dense_rank` -> bigint, `percent_rank` / `cume_dist` ->
///   double precision.
#[allow(clippy::too_many_arguments)]
pub(crate) fn within_group_result_type(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    schemas: &[&[QCol]],
    outer: &[&[QCol]],
    ctes: &[CteDef],
    func: OrderedSetAgg,
    direct_args: &[Expr],
    within_order_by: &[OrderTerm],
) -> Result<ColType, ExecError> {
    // Hypothetical-set aggregates have exactly one sort term per
    // direct arg (enforced at parse time); the sort term's type is
    // the representative's type for percentile_disc / mode.
    let sort_ty = |i: usize| {
        expr_type(
            eng,
            snap,
            own,
            session,
            schemas,
            outer,
            ctes,
            &within_order_by[i].expr,
        )
    };
    match func {
        OrderedSetAgg::PercentileCont => {
            let a = direct_args
                .first()
                .expect("percentile_cont takes one direct arg");
            let t = expr_type(eng, snap, own, session, schemas, outer, ctes, a)?;
            // PG19 coerces the fraction to float8 implicitly
            // (numeric/int literals work); the sort column must be
            // float8-able too (PG19's float8 variant; the interval
            // variant is out of scope — rustgres has no Interval).
            // Text is exempt: untyped NULL literals type as Text, and
            // all-NULL inputs must yield NULL; a real text sort value
            // fails at execution with 42883 (see percentile_cont_final).
            let st = sort_ty(0)?;
            if st != ColType::Text && !within_group_numeric(&st) {
                return Err(exec_err(
                    "42883",
                    "function percentile_cont(float8) does not exist".to_string(),
                ));
            }
            // PG19: an untyped NULL literal coerces to float8 and
            // yields NULL; any other non-numeric fraction has no such
            // signature (42883).
            let null_lit = matches!(a, Expr::Literal(Literal::Null));
            match t {
                _ if within_group_numeric(&t) || null_lit => Ok(ColType::Float),
                ColType::Array(_) => Ok(ColType::Array(crate::storage::ArrayElem::Float)),
                _ => Err(exec_err(
                    "42883",
                    format!("function percentile_cont({}) does not exist", t.sql_name()),
                )),
            }
        }
        OrderedSetAgg::PercentileDisc => {
            let a = direct_args
                .first()
                .expect("percentile_disc takes one direct arg");
            let t = expr_type(eng, snap, own, session, schemas, outer, ctes, a)?;
            let elem = sort_ty(0)?;
            // PG19: an untyped NULL literal coerces to float8 and
            // yields NULL; any other non-numeric fraction has no such
            // signature (42883).
            let null_lit = matches!(a, Expr::Literal(Literal::Null));
            match t {
                _ if within_group_numeric(&t) || null_lit => Ok(elem),
                ColType::Array(_) => Ok(ColType::Array(crate::storage::ArrayElem::of(&elem))),
                _ => Err(exec_err(
                    "42883",
                    format!("function percentile_disc({}) does not exist", t.sql_name()),
                )),
            }
        }
        OrderedSetAgg::Mode => Ok(sort_ty(0)?),
        OrderedSetAgg::Rank | OrderedSetAgg::DenseRank => Ok(ColType::BigInt),
        OrderedSetAgg::PercentRank | OrderedSetAgg::CumeDist => Ok(ColType::Float),
    }
}
