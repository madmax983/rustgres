// v1.78 mechanical split: moved verbatim from src/exec.rs (66049-66203).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ============================================================================
// v0.9: views.
// ============================================================================

pub(crate) fn exec_create_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    query: &str,
    col_aliases: &[String],
    or_replace: bool,
) -> Result<ExecResult, ExecError> {
    // A table by this name blocks the view (Postgres shares the namespace).
    if eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .is_some()
    {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    let existing = eng.db.find_view(name, ctx.snap, ctx.own).is_some();
    if existing && !or_replace {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    if existing {
        // v0.11: OR REPLACE requires ownership of the existing view.
        require_view_owner(eng, ctx, name)?;
    }
    // The query must parse, and must not reference the view itself
    // (checked by the parser) or unknown relations.
    let stmt = parse_statement(query).map_err(sql_err)?;
    let select = match stmt {
        Stmt::Select(s) => s,
        _ => {
            return Err(exec_err(
                "42601",
                "CREATE VIEW query must be a SELECT".to_string(),
            ));
        }
    };
    let mut deps = Vec::new();
    collect_table_refs(&select, &mut deps);
    // Every referenced relation must exist (as a table or view).
    for d in &deps {
        let is_table = eng
            .db
            .find_table(d, ctx.snap, &ctx.all_xids, ctx.session)
            .is_some();
        let is_view = eng.db.find_view(d, ctx.snap, ctx.own).is_some();
        if !is_table && !is_view {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", d),
            ));
        }
    }
    if existing {
        // OR REPLACE: drop the old definition first (same statement).
        drop_view_internal(eng, ctx, name)?;
    }
    let def = ViewDef {
        query: query.to_string(),
        col_aliases: col_aliases.to_vec(),
        deps,
        created_xmin: ctx.write_xid,
        dropped_xmax: 0,
        // v0.11: the creating role owns the view.
        owner: ctx.role.to_string(),
    };
    eng.db.views.entry(name.to_string()).or_default().push(def);
    ctx.writes.push(WriteOp::CreateView {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: "CREATE VIEW".to_string(),
    })
}

pub(crate) fn exec_drop_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        // v0.11: only the owner (or a superuser) may drop a view.
        require_view_owner(eng, ctx, name)?;
        let exists = eng.db.find_view(name, ctx.snap, ctx.own).is_some();
        if !exists {
            if if_exists {
                continue;
            }
            return Err(exec_err(
                "42P01",
                format!("view \"{}\" does not exist", name),
            ));
        }
        // Other views depending on this one.
        let mut dependents: Vec<String> = Vec::new();
        for (vname, vs) in &eng.db.views {
            if vname == name {
                continue;
            }
            if vs.iter().any(|v| {
                crate::storage::view_visible(v, ctx.snap, ctx.own)
                    && v.deps.iter().any(|d| d == name)
            }) {
                dependents.push(vname.clone());
            }
        }
        if !dependents.is_empty() && !cascade {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop view {} because view {} depends on it",
                    name, dependents[0]
                ),
            ));
        }
        for dep in &dependents {
            drop_view_internal(eng, ctx, dep)?;
        }
        drop_view_internal(eng, ctx, name)?;
    }
    Ok(ExecResult::Command {
        tag: "DROP VIEW".to_string(),
    })
}

/// Drop a view by name, logging WriteOp::DropView. Used by DROP VIEW and
/// by CASCADE paths from DROP TABLE / DROP COLUMN.
pub(crate) fn drop_view_internal(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
) -> Result<(), ExecError> {
    let snapshot = eng
        .db
        .find_view(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("view \"{}\" does not exist", name)))?
        .clone();
    eng.db
        .find_view_mut(name, ctx.snap, ctx.own)
        .expect("view still present")
        .dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropView {
        name: name.to_string(),
        view: snapshot,
    });
    Ok(())
}
