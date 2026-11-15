// v2.17: extended statistics (CREATE STATISTICS family).
// Semantics grounded in PG19 commands/statscmds.c, statistics/
// (ndistinct / dependencies / MCV builders) and the pg_statistic_ext /
// pg_statistic_ext_data catalogs + pg_stats_ext views (system_views.sql).
use super::*;
use crate::sql::AlterStatisticsAction;
use crate::storage::{ExtExprStat, ExtStatData, ExtStatItem, ExtStatObj};

pub(crate) fn split_qname(q: &str) -> (Option<String>, String) {
    match q.split_once('.') {
        Some((s, n)) => (Some(s.to_string()), n.to_string()),
        None => (None, q.to_string()),
    }
}

pub(crate) fn stats_base_name(q: &str) -> &str {
    q.rsplit('.').next().unwrap_or(q)
}

/// Parse an expression from source text (for statistics items stored
/// as text at CREATE time).
pub(crate) fn parse_stat_expr(text: &str) -> Result<Expr, ExecError> {
    let mut toks = crate::sql::tokenize(text).map_err(|e| exec_err(e.code, e.message.clone()))?;
    toks.push(crate::sql::Token::EOF);
    let mut p = crate::sql::Parser {
        tokens: toks,
        pos: 0,
        unnamed_seq: 0,
        allow_similar_to: true,
        last_bit_typmod: None,
    };
    let e = p
        .parse_or()
        .map_err(|e| exec_err(e.code, e.message.clone()))?;
    Ok(e)
}

fn table_schema(eng: &Engine, table_q: &str) -> String {
    let (sch, _) = split_qname(table_q);
    if let Some(s) = sch {
        return s;
    }
    eng.db
        .obj_schema
        .get(table_q)
        .cloned()
        .unwrap_or_else(|| "public".to_string())
}

fn find_obj(db: &Database, schema: &str, name: &str) -> Option<usize> {
    db.ext_stats
        .iter()
        .position(|o| o.schema == schema && o.name == name)
}

/// Resolve an object by (optional schema, name); schema defaults to
/// any match (search-path approximation).
fn resolve_obj(db: &Database, schema: &Option<String>, name: &str) -> Option<usize> {
    match schema {
        Some(s) => find_obj(db, s, name),
        // PG19: an unqualified reference does not resolve objects in
        // a pg_temp namespace (the stats_ext_temp DROP case "shall
        // fail" while the qualified form succeeds).
        None => db
            .ext_stats
            .iter()
            .position(|o| o.name == name && !o.schema.starts_with("pg_temp")),
    }
}

const SYSTEM_COLS: [&str; 6] = ["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_statistics(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    schema: &Option<String>,
    name: &str,
    auto_name: bool,
    if_not_exists: bool,
    kinds: &[char],
    items: &[String],
    table: &str,
) -> Result<ExecResult, ExecError> {
    // Kind validation first (PG gram/statscmds: unknown kind is 42601).
    if kinds.contains(&'?') {
        return Err(exec_err(
            "42601",
            "unrecognized statistics kind".to_string(),
        ));
    }
    let tbl = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
        .cloned();
    let tbl = match tbl {
        Some(t) => t,
        None => {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", stats_base_name(table)),
            ));
        }
    };
    // Ownership: PG requires owning the table (42501).
    if tbl.owner != ctx.role
        && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
    {
        return Err(exec_err(
            "42501",
            format!("must be owner of table {}", stats_base_name(table)),
        ));
    }
    let obj_schema = schema.clone().unwrap_or_else(|| table_schema(eng, table));
    // v2.17 adjudication: explicit cross-schema placement requires
    // CREATE privilege on the target schema (PG19 aclchk namespace
    // check; the table owner creating in the table's own schema is
    // always allowed, per the stats_ext privilege section).
    if let Some(explicit) = schema {
        let tschema = table_schema(eng, table);
        if explicit != &tschema
            && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            && eng.db.schema_owners.get(explicit).map(|o| o.as_str()) != Some(ctx.role)
        {
            let has_create = eng
                .db
                .schema_acls
                .get(explicit)
                .map(|es| {
                    es.iter().any(|e| {
                        (e.role == ctx.role || e.role == "PUBLIC")
                            && e.privs & crate::storage::PRIV_REFERENCES != 0
                    })
                })
                .unwrap_or(false);
            if !has_create {
                return Err(exec_err(
                    "42501",
                    format!("permission denied for schema {}", explicit),
                ));
            }
        }
    }
    // PG19 statscmds: at most 8 attributes in one statistics object.
    if items.len() > 8 {
        return Err(exec_err(
            "54011",
            "cannot have more than 8 columns in statistics".to_string(),
        ));
    }
    // PG: auto-generated names get the label incremented on collision
    // (ChooseExtendedStatisticName: "stat" -> "stat1" -> "stat2",
    // appended directly with no separator).
    let name_owned: String = if auto_name && find_obj(&eng.db, &obj_schema, name).is_some() {
        let mut idx = 1u32;
        loop {
            let candidate = format!("{}{}", name, idx);
            if find_obj(&eng.db, &obj_schema, &candidate).is_none() {
                break candidate;
            }
            idx += 1;
        }
    } else {
        name.to_string()
    };
    let name: &str = &name_owned;
    if let Some(pos) = find_obj(&eng.db, &obj_schema, name) {
        let _ = pos;
        if if_not_exists {
            ctx.notices.push(format!(
                "statistics object \"{}\" already exists, skipping",
                name
            ));
            return Ok(ExecResult::Command {
                tag: "CREATE STATISTICS".to_string(),
            });
        }
        return Err(exec_err(
            "42P07",
            format!("statistics object \"{}\" already exists", name),
        ));
    }
    // Resolve items against the table.
    let mut resolved: Vec<ExtStatItem> = Vec::new();
    let mut seen_cols: Vec<u16> = Vec::new();
    for item in items {
        let trimmed = item.trim();
        let lower = trimmed.to_lowercase();
        // Plain column reference?
        let colpos = tbl
            .columns
            .iter()
            .position(|(cn, _)| cn.eq_ignore_ascii_case(trimmed));
        if let Some(ci) = colpos {
            let attnum = (ci + 1) as u16;
            if seen_cols.contains(&attnum) {
                return Err(exec_err(
                    "42701",
                    "duplicate column name in statistics definition".to_string(),
                ));
            }
            seen_cols.push(attnum);
            resolved.push(ExtStatItem::Column(attnum));
            continue;
        }
        if SYSTEM_COLS.contains(&lower.as_str()) {
            return Err(exec_err(
                "0A000",
                "statistics creation on system columns is not supported".to_string(),
            ));
        }
        // Expression path: a bare identifier that is not a column is a
        // missing column (42703), anything else parses as an expression.
        let is_bare_ident = !trimmed.is_empty()
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '"');
        if is_bare_ident {
            return Err(exec_err(
                "42703",
                format!("column \"{}\" does not exist", trimmed.trim_matches('"')),
            ));
        }
        // PG19: the same expression twice is an error (statscmds.c:
        // "duplicate expression in statistics definition", 42701).
        if resolved
            .iter()
            .any(|r| matches!(r, ExtStatItem::Expr(t, _) if t == trimmed))
        {
            return Err(exec_err(
                "42701",
                "duplicate expression in statistics definition".to_string(),
            ));
        }
        let expr = parse_stat_expr(trimmed)?;
        // v2.30: PG19 statscmds.c — an expression referencing a
        // system column is rejected like a plain system-column item.
        {
            let mut refs: Vec<(Option<String>, String)> = Vec::new();
            crate::sql::collect_col_refs(&expr, &mut refs);
            if refs
                .iter()
                .any(|(_, r)| SYSTEM_COLS.contains(&r.to_lowercase().as_str()))
            {
                return Err(exec_err(
                    "0A000",
                    "statistics creation on system columns is not supported".to_string(),
                ));
            }
        }
        // A parenthesized row of columns is not a valid single item.
        if matches!(expr, Expr::Row(_)) {
            return Err(exec_err(
                "42601",
                "statistics expressions must not be row-valued".to_string(),
            ));
        }
        // v2.30: PG19 statscmds.c — a parenthesised bare column
        // reference (`ON (y)`) is classified as a plain column, not
        // an expression; it then hits the single-column gate below.
        if let Expr::Column { name: cn, .. } = &expr {
            let ci = tbl
                .columns
                .iter()
                .position(|(n, _)| n.eq_ignore_ascii_case(cn))
                .ok_or_else(|| exec_err("42703", format!("column \"{}\" does not exist", cn)))?;
            let attnum = (ci + 1) as u16;
            if seen_cols.contains(&attnum) {
                return Err(exec_err(
                    "42701",
                    "duplicate column name in statistics definition".to_string(),
                ));
            }
            seen_cols.push(attnum);
            resolved.push(ExtStatItem::Column(attnum));
            continue;
        }
        resolved.push(ExtStatItem::Expr(trimmed.to_string(), expr));
    }
    // v2.30: PG19 statscmds.c — with more than one item, every
    // plain column (and every column referenced by an expression
    // item) must belong to a type with a less-than operator;
    // otherwise the type has no default btree operator class and
    // multivariate statistics cannot be built on it (0A000).
    if resolved.len() > 1 {
        let type_name = |ty: &ColType| -> String {
            // PG prints the type's pg_type name (format_type_be).
            match ty {
                ColType::Xid => "xid".to_string(),
                other => format!("{:?}", other).to_lowercase(),
            }
        };
        let check_col = |ci: usize| -> Result<(), ExecError> {
            let (cname, ty) = &tbl.columns[ci];
            if matches!(ty, ColType::Xid) {
                return Err(crate::exec::exec_err_detail(
                    "0A000",
                    format!(
                        "cannot create multivariate statistics on column \"{}\"",
                        cname
                    ),
                    format!(
                        "The type {} has no default btree operator class.",
                        type_name(ty)
                    ),
                ));
            }
            Ok(())
        };
        for item in &resolved {
            match item {
                ExtStatItem::Column(a) => check_col((*a as usize) - 1)?,
                ExtStatItem::Expr(_, e) => {
                    let mut refs: Vec<(Option<String>, String)> = Vec::new();
                    crate::sql::collect_col_refs(e, &mut refs);
                    for (_, rn) in &refs {
                        if let Some(ci) = tbl
                            .columns
                            .iter()
                            .position(|(n, _)| n.eq_ignore_ascii_case(rn))
                        {
                            check_col(ci)?;
                        }
                    }
                }
            }
        }
    }
    // v2.30: PG19 statscmds.c gates, in PG's order. A single item is
    // only legal when it is an expression (a virtual generated column
    // counts as an expression; a stored generated column is a plain
    // column). Gate 1: single non-virtual column -> 42P17. Gate 2:
    // kinds specified for a univariate (single-expression) object
    // -> 0A000.
    let is_virtual_gen = |a: u16| {
        tbl.gen_cols
            .get((a as usize).saturating_sub(1))
            .is_some_and(|g| matches!(g, Some((_, false))))
    };
    let single_is_expr = resolved.len() == 1
        && match &resolved[0] {
            ExtStatItem::Expr(_, _) => true,
            ExtStatItem::Column(a) => is_virtual_gen(*a),
        };
    if resolved.len() < 2 && !single_is_expr {
        return Err(exec_err(
            "42P17",
            "cannot create extended statistics on a single non-virtual column".to_string(),
        ));
    }
    if resolved.len() == 1 && single_is_expr && !kinds.is_empty() {
        return Err(exec_err(
            "0A000",
            "cannot specify statistics kinds when building univariate statistics".to_string(),
        ));
    }
    let mut kinds_final: Vec<char> = if kinds.is_empty() {
        if resolved.len() >= 2 {
            vec!['d', 'f', 'm']
        } else {
            // Single expression: expression statistics only (stxkind {e}).
            Vec::new()
        }
    } else {
        let mut k = kinds.to_vec();
        k.sort_unstable();
        k.dedup();
        k
    };
    // v2.30: PG19 appends kind 'e' (STATS_EXT_EXPRESSIONS) whenever the
    // object has expression items (a virtual generated column counts),
    // always in d, f, m, e order. Stored in `kinds` so stxkind renders
    // verbatim.
    let has_expr_item = resolved.iter().any(|i| match i {
        ExtStatItem::Expr(_, _) => true,
        ExtStatItem::Column(a) => is_virtual_gen(*a),
    });
    if has_expr_item {
        kinds_final.push('e');
    }
    let oid = eng.db.alloc_oid();
    eng.db.ext_stats.push(ExtStatObj {
        oid,
        name: name.to_string(),
        schema: obj_schema,
        owner: ctx.role.to_string(),
        table: table.to_string(),
        items: resolved,
        item_texts: items.to_vec(),
        kinds: kinds_final,
        stattarget: None,
        data: ExtStatData::default(),
        data_inh: None,
    });
    Ok(ExecResult::Command {
        tag: "CREATE STATISTICS".to_string(),
    })
}

pub(crate) fn exec_drop_statistics(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[(Option<String>, String)],
    if_exists: bool,
    // CASCADE/RESTRICT: statistics objects have no dependents that
    // block a drop in this engine, so both behave identically (PG:
    // RESTRICT is the default and also succeeds for a bare object).
    _cascade: bool,
) -> Result<ExecResult, ExecError> {
    // v2.30: PG19 ownership check (aclchk OBJECT_STATISTIC_EXT):
    // dropping requires owning the object. All names are checked
    // before any is dropped (statement atomicity; the registry is
    // mutated directly, not through WriteOps).
    for (schema, name) in names {
        if let Some(pos) = resolve_obj(&eng.db, schema, name) {
            let o = &eng.db.ext_stats[pos];
            let owns = o.owner == ctx.role
                || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own);
            if !owns {
                return Err(exec_err(
                    "42501",
                    format!("must be owner of statistics object {}", name),
                ));
            }
        }
    }
    for (schema, name) in names {
        match resolve_obj(&eng.db, schema, name) {
            Some(pos) => {
                eng.db.ext_stats.remove(pos);
            }
            None => {
                if if_exists {
                    ctx.notices.push(format!(
                        "statistics object \"{}\" does not exist, skipping",
                        name
                    ));
                } else {
                    return Err(exec_err(
                        "42P01",
                        format!("statistics object \"{}\" does not exist", name),
                    ));
                }
            }
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP STATISTICS".to_string(),
    })
}

pub(crate) fn exec_alter_statistics(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    schema: &Option<String>,
    name: &str,
    if_exists: bool,
    action: &AlterStatisticsAction,
) -> Result<ExecResult, ExecError> {
    let pos = match resolve_obj(&eng.db, schema, name) {
        Some(p) => p,
        None => {
            if if_exists {
                ctx.notices.push(format!(
                    "statistics object \"{}\" does not exist, skipping",
                    name
                ));
                return Ok(ExecResult::Command {
                    tag: "ALTER STATISTICS".to_string(),
                });
            }
            return Err(exec_err(
                "42P01",
                format!("statistics object \"{}\" does not exist", name),
            ));
        }
    };
    // PG19: ALTER STATISTICS requires owning the object (42501).
    // PG's object_ownercheck passes for a role that is a MEMBER of
    // the owning role (has_privs_of_role) - the alter_generic case:
    // user1 transfers alt_stat2 to user3 (a role user1 belongs to)
    // and may still SET SCHEMA on it. OWNER TO additionally requires
    // being able to SET ROLE to the new owner.
    {
        let o = &eng.db.ext_stats[pos];
        let owns = o.owner == ctx.role
            || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
            || eng
                .db
                .find_role(ctx.role, ctx.snap, ctx.own)
                .map(|r| r.memberships.iter().any(|m| m.role == o.owner))
                .unwrap_or(false);
        if !owns {
            return Err(exec_err(
                "42501",
                format!("must be owner of statistics object {}", name),
            ));
        }
        if let AlterStatisticsAction::OwnerTo { owner } = action {
            let is_super = crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own);
            if !is_super && owner != ctx.role {
                let member = eng
                    .db
                    .find_role(ctx.role, ctx.snap, ctx.own)
                    .map(|r| r.memberships.iter().any(|m| &m.role == owner))
                    .unwrap_or(false);
                if !member {
                    return Err(exec_err(
                        "42501",
                        format!("must be able to SET ROLE \"{}\"", owner),
                    ));
                }
            }
        }
    }
    match action {
        AlterStatisticsAction::RenameTo { new_name } => {
            let sc = eng.db.ext_stats[pos].schema.clone();
            if find_obj(&eng.db, &sc, new_name).is_some() {
                return Err(exec_err(
                    "42P07",
                    format!("statistics object \"{}\" already exists", new_name),
                ));
            }
            eng.db.ext_stats[pos].name = new_name.clone();
        }
        AlterStatisticsAction::OwnerTo { owner } => {
            eng.db.ext_stats[pos].owner = owner.clone();
        }
        AlterStatisticsAction::SetSchema { schema: ns } => {
            let nm = eng.db.ext_stats[pos].name.clone();
            if find_obj(&eng.db, ns, &nm).is_some() {
                return Err(exec_err(
                    "42P07",
                    format!("statistics object \"{}\" already exists", nm),
                ));
            }
            eng.db.ext_stats[pos].schema = ns.clone();
        }
        AlterStatisticsAction::SetStatistics { target } => {
            if let Some(t) = target {
                if *t < -1 || *t > 10000 {
                    return Err(exec_err(
                        "22023",
                        "statistics target must be in range -1 to 10000".to_string(),
                    ));
                }
            }
            eng.db.ext_stats[pos].stattarget = *target;
        }
    }
    Ok(ExecResult::Command {
        tag: "ALTER STATISTICS".to_string(),
    })
}

/// Drop statistics objects bound to a table (DROP TABLE hook).
pub(crate) fn drop_stats_for_table(db: &mut Database, table: &str) {
    let base = stats_base_name(table).to_string();
    db.ext_stats.retain(|o| stats_base_name(&o.table) != base);
}

// ======================================================================
// ANALYZE build
// ======================================================================

/// v2.30: the planner-facing attnum of every item of `obj`, in item
/// order (columns: their stored attnum; expressions: -1, -2, ...).
pub(crate) fn item_attnums(obj: &ExtStatObj) -> Vec<i32> {
    (0..obj.items.len()).map(|i| attnum_of(obj, i)).collect()
}

fn attnum_of(obj: &ExtStatObj, item_idx: usize) -> i32 {
    match &obj.items[item_idx] {
        ExtStatItem::Column(a) => *a as i32,
        ExtStatItem::Expr(_, _) => {
            // Expressions are numbered -1, -2, ... in item order among
            // expressions (PG pg_statistic_ext expression attnums).
            let mut n = 0i32;
            for (i, it) in obj.items.iter().enumerate() {
                if matches!(it, ExtStatItem::Expr(_, _)) {
                    n += 1;
                    if i == item_idx {
                        return -n;
                    }
                }
            }
            0
        }
    }
}

fn fmt_float(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{:.1}", x)
    } else {
        format!("{}", x)
    }
}

fn array_quote(s: &str) -> String {
    if s.is_empty()
        || s.chars()
            .any(|c| c == ',' || c == '{' || c == '}' || c == '"' || c == '\\' || c.is_whitespace())
        || s.eq_ignore_ascii_case("NULL")
    {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Build extended statistics for every object on `table` (called from
/// ANALYZE). Errors are swallowed into "not built" — ANALYZE in PG
/// would fail, but expression evaluation gaps must not break ANALYZE
/// for the whole table in this bounded model.
pub(crate) fn build_ext_stats_for_table(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    table: &str,
) {
    let indices: Vec<usize> = eng
        .db
        .ext_stats
        .iter()
        .enumerate()
        .filter(|(_, o)| stats_base_name(&o.table) == stats_base_name(table))
        .map(|(i, _)| i)
        .collect();
    if indices.is_empty() {
        return;
    }
    let Some(tbl) = eng.db.find_table(table, snap, &[own], session).cloned() else {
        return;
    };
    let vis: Vec<&RowVersion> = tbl
        .rows
        .iter()
        .filter(|r| row_visible(r, snap, &[own]))
        .collect();
    let total = vis.len() as f64;
    let schema: Vec<QCol> = tbl
        .columns
        .iter()
        .enumerate()
        .map(|(i, (cn, ty))| QCol {
            qual: String::new(),
            name: cn.clone(),
            ty: ty.clone(),
            hidden: false,
            src_ord: i as u32,
        })
        .collect();
    let mut built: Vec<(usize, ExtStatData, Option<ExtStatData>)> = Vec::new();
    // v2.30: the inheritance/partition family of `table` (itself
    // first), for the inherited (stxdinherit) build. PG computes the
    // inherited payload from the union of the family's rows when
    // ANALYZE runs on a table with descendants.
    let family = family_tables(&eng.db, table, snap, own, session);
    for pos in indices {
        let obj = eng.db.ext_stats[pos].clone();
        // v2.30: PG19 build skips (extended_stats.c
        // BuildRelationExtStatistics): an object-level statistics
        // target of 0 leaves existing values untouched; a column at
        // SET STATISTICS 0 makes the object uncomputable
        // (lookup_var_attr_stats returns NULL -> no store at all).
        if obj.stattarget == Some(0) {
            continue;
        }
        let col_target_zero = obj.items.iter().any(|it| match it {
            ExtStatItem::Column(a) => tbl.col_stattarget_at((*a as usize).saturating_sub(1)) == 0,
            ExtStatItem::Expr(_, _) => false,
        });
        if col_target_zero {
            continue;
        }
        // v2.30: a partitioned parent has no rows of its own; PG
        // still builds the inherited payload for its objects, so
        // only the own-data build is skipped when `vis` is empty.
        let own_data = if vis.is_empty() {
            None
        } else {
            let Some(item_vals) = eval_items(eng, snap, own, session, &obj, &schema, &vis) else {
                continue;
            };
            Some(compute_ext_data(&obj, &item_vals, total))
        };
        // Inherited build over the family union (only when the table
        // actually has descendants).
        let inh = if family.len() > 1 {
            let mut fam_vals: Vec<Vec<Value>> = vec![Vec::new(); obj.items.len()];
            let mut fam_total = 0usize;
            let mut fam_ok = true;
            for ft_name in &family {
                let Some(ft) = eng.db.find_table(ft_name, snap, &[own], session).cloned() else {
                    continue;
                };
                let fvis: Vec<&RowVersion> = ft
                    .rows
                    .iter()
                    .filter(|r| row_visible(r, snap, &[own]))
                    .collect();
                // Map this family member's columns onto the object's
                // items by column NAME (children may append columns).
                let fschema: Vec<QCol> = ft
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, (cn, ty))| QCol {
                        qual: String::new(),
                        name: cn.clone(),
                        ty: ty.clone(),
                        hidden: false,
                        src_ord: i as u32,
                    })
                    .collect();
                let Some(vals) = eval_items(eng, snap, own, session, &obj, &fschema, &fvis) else {
                    fam_ok = false;
                    break;
                };
                fam_total += fvis.len();
                for (ii, col) in vals.into_iter().enumerate() {
                    fam_vals[ii].extend(col);
                }
            }
            if fam_ok && fam_total > 0 {
                Some(compute_ext_data(&obj, &fam_vals, fam_total as f64))
            } else {
                None
            }
        } else {
            None
        };
        // With no own rows there is no own-data payload to store;
        // keep whatever was there (PG leaves the non-inherited row
        // absent for partitioned tables) and record only the
        // inherited build below.
        if let Some(data) = own_data {
            built.push((pos, data, inh));
        } else if inh.is_some() {
            if let Some(slot) = eng.db.ext_stats.get_mut(pos) {
                slot.data_inh = inh;
            }
        }
    }
    for (pos, data, inh) in built {
        if let Some(slot) = eng.db.ext_stats.get_mut(pos) {
            slot.data = data;
            if inh.is_some() {
                slot.data_inh = inh;
            }
        }
    }
}

/// v2.30: all tables in `table`'s family: itself plus transitive
/// inheritance descendants and partition descendants (by name).
pub(crate) fn family_tables(
    db: &Database,
    table: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> Vec<String> {
    let mut out: Vec<String> = vec![table.to_string()];
    let mut frontier: Vec<String> = vec![table.to_string()];
    while let Some(cur) = frontier.pop() {
        // Inheritance children: tables whose inherits list names cur.
        for (k, vs) in db.tables.iter() {
            let is_child = vs.last().is_some_and(|t| {
                t.inherits
                    .iter()
                    .any(|p| stats_base_name(p) == stats_base_name(&cur))
            });
            if is_child && !out.contains(k) {
                out.push(k.clone());
                frontier.push(k.clone());
            }
        }
        // Partition children of cur.
        if let Some(t) = db.find_table(&cur, snap, &[own], session) {
            if let Some(p) = &t.partition {
                for c in &p.children {
                    if !out.iter().any(|x| stats_base_name(x) == stats_base_name(c)) {
                        out.push(c.clone());
                        frontier.push(c.clone());
                    }
                }
            }
        }
    }
    out
}

/// v2.30: evaluate every item of `obj` over `rows` of table `tbl`
/// (whose QCol schema is `schema`). None on any expression error.
fn eval_items(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    obj: &ExtStatObj,
    schema: &[QCol],
    rows: &[&RowVersion],
) -> Option<Vec<Vec<Value>>> {
    let mut lock_ids: Vec<(String, u64)> = Vec::new();
    let mut q = Q {
        eng,
        snap,
        own,
        all_xids: vec![own],
        session,
        role: "postgres",
        read_only: true,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        priv_role_override: None,
        hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
        hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        pending_updates: None,
        write: None,
    };
    let mut item_vals: Vec<Vec<Value>> = vec![Vec::new(); obj.items.len()];
    let mut ok = true;
    for r in rows {
        let frame = Scope {
            schema,
            row: &r.values,
            prov: None,
        };
        for (ii, item) in obj.items.iter().enumerate() {
            let v = match item {
                ExtStatItem::Column(a) => r
                    .values
                    .get((*a as usize).saturating_sub(1))
                    .cloned()
                    .unwrap_or(Value::Null),
                ExtStatItem::Expr(_, e) => match eval_expr(&mut q, std::slice::from_ref(&frame), e)
                {
                    Ok(v) => v,
                    Err(_) => {
                        ok = false;
                        Value::Null
                    }
                },
            };
            item_vals[ii].push(v);
        }
    }
    drop(q);
    if ok { Some(item_vals) } else { None }
}

/// v2.30: compute one ExtStatData payload from evaluated item values
/// (PG19 statext_*_build ports; full-census sample, so PG's sampling
/// estimators reduce to exact counts).
fn compute_ext_data(obj: &ExtStatObj, item_vals: &[Vec<Value>], total: f64) -> ExtStatData {
    let nrows = item_vals.first().map(|v| v.len()).unwrap_or(0);
    let mut data = ExtStatData {
        built: true,
        ..Default::default()
    };
    {
        let n = obj.items.len();
        let key_of = |vals: &[Value]| -> Vec<u8> {
            let mut k = Vec::new();
            for v in vals {
                value_key(v, &mut k);
                k.push(0xff);
            }
            k
        };
        // Combinations of item indices of size k, lexicographic
        // (PG's CombinationGenerator order).
        fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
            let mut out: Vec<Vec<usize>> = Vec::new();
            if k == 0 || k > n {
                return out;
            }
            let mut idx: Vec<usize> = (0..k).collect();
            loop {
                out.push(idx.clone());
                let mut i = k;
                loop {
                    if i == 0 {
                        return out;
                    }
                    i -= 1;
                    if idx[i] < n - k + i {
                        idx[i] += 1;
                        for j in (i + 1)..k {
                            idx[j] = idx[j - 1] + 1;
                        }
                        break;
                    }
                }
            }
        }
        // ndistinct (mvdistinct.c): items for every subset of size
        // >= 2; the value is the distinct-combination count (PG's
        // estimate_ndistinct with a full-table sample is exact).
        // Text form is pg_ndistinct_out's JSON array.
        if obj.kinds.contains(&'d') {
            let mut parts: Vec<String> = Vec::new();
            for k in 2..=n {
                for combo in combinations(n, k) {
                    let mut set: std::collections::HashSet<Vec<u8>> =
                        std::collections::HashSet::new();
                    for ri in 0..nrows {
                        let vals: Vec<Value> =
                            combo.iter().map(|&i| item_vals[i][ri].clone()).collect();
                        set.insert(key_of(&vals));
                    }
                    let atts: Vec<i32> = combo.iter().map(|&i| attnum_of(obj, i)).collect();
                    let att_txt: Vec<String> = atts.iter().map(|a| a.to_string()).collect();
                    parts.push(format!(
                        "{{\"attributes\": [{}], \"ndistinct\": {}}}",
                        att_txt.join(", "),
                        set.len()
                    ));
                    data.ndistinct_items.push((atts, set.len() as f64));
                }
            }
            data.ndistinct = Some(format!("[{}]", parts.join(", ")));
        }
        // dependencies (dependencies.c): for each candidate
        // (antecedent set, implied item), the degree is the fraction
        // of rows in FULLY consistent antecedent groups (a group
        // whose implied values are not all equal contributes 0).
        // Stored iff degree > 0.
        if obj.kinds.contains(&'f') && n >= 2 {
            let mut deps: Vec<String> = Vec::new();
            // PG19 DependencyGenerator order: for each size k, the
            // antecedent is an ascending (k-1)-combination in
            // lexicographic order, and the implied item runs over
            // all remaining items in index order.
            for k in 2..=n {
                for subset in combinations(n, k - 1) {
                    for dep in 0..n {
                        if subset.contains(&dep) {
                            continue;
                        }
                        let mut groups: HashMap<Vec<u8>, (Vec<u8>, bool, u64)> = HashMap::default();
                        for ri in 0..nrows {
                            let gk = key_of(
                                &subset
                                    .iter()
                                    .map(|&i| item_vals[i][ri].clone())
                                    .collect::<Vec<_>>(),
                            );
                            let dk = key_of(&[item_vals[dep][ri].clone()]);
                            let e = groups.entry(gk).or_insert_with(|| (dk.clone(), true, 0));
                            if e.0 != dk {
                                e.1 = false;
                            }
                            e.2 += 1;
                        }
                        let supporting: u64 = groups
                            .values()
                            .filter(|(_, consistent, _)| *consistent)
                            .map(|(_, _, c)| *c)
                            .sum();
                        let degree = if total > 0.0 {
                            supporting as f64 / total
                        } else {
                            0.0
                        };
                        if degree > 0.0 {
                            let atts: Vec<i32> =
                                subset.iter().map(|&i| attnum_of(obj, i)).collect();
                            let att_txt: Vec<String> = atts.iter().map(|a| a.to_string()).collect();
                            deps.push(format!(
                                "{{\"attributes\": [{}], \"dependency\": {}, \"degree\": {:.6}}}",
                                att_txt.join(", "),
                                attnum_of(obj, dep),
                                degree
                            ));
                            data.deps_items.push((atts, attnum_of(obj, dep), degree));
                        }
                    }
                }
            }
            data.dependencies = Some(format!("[{}]", deps.join(", ")));
        }
        // MCV over all items (mcv.c): frequency = count/totalrows;
        // base_frequency = product over dimensions of that
        // dimension's own frequency for the item's value (NULL
        // dimensions use the NULL fraction).
        if obj.kinds.contains(&'m') {
            // Per-dimension value frequencies.
            let mut dim_counts: Vec<HashMap<Vec<u8>, u64>> = vec![HashMap::default(); n];
            for d in 0..n {
                for ri in 0..nrows {
                    let k = key_of(&[item_vals[d][ri].clone()]);
                    *dim_counts[d].entry(k).or_insert(0) += 1;
                }
            }
            let mut counts: HashMap<Vec<u8>, (Vec<Option<Value>>, u64)> = HashMap::default();
            for ri in 0..nrows {
                let vals: Vec<Value> = (0..n).map(|i| item_vals[i][ri].clone()).collect();
                let k = key_of(&vals);
                let e = counts.entry(k).or_insert_with(|| {
                    (
                        vals.iter()
                            .map(|v| {
                                if matches!(v, Value::Null) {
                                    None
                                } else {
                                    Some(v.clone())
                                }
                            })
                            .collect(),
                        0,
                    )
                });
                e.1 += 1;
            }
            let mut items: Vec<(Vec<Option<Value>>, u64)> =
                counts.values().map(|(v, c)| (v.clone(), *c)).collect();
            // v2.19 adjudication: frequency ties must be
            // deterministic — the v2.17 builder emitted ties in
            // HashMap iteration order, so equal-frequency MCVs
            // surfaced in arbitrary order (the stats_ext_range
            // pg_stats_ext mismatch). PG's stats builder is
            // order-stable for ties; use the rendered key ascending
            // as the tie-break.
            let render_key = |vals: &Vec<Option<Value>>| -> String {
                vals.iter()
                    .map(|v| match v {
                        Some(v) => value_to_text_cast(v),
                        None => "NULL".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            };
            items.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| render_key(&a.0).cmp(&render_key(&b.0)))
            });
            data.mcv = items
                .into_iter()
                .take(100)
                .map(|(v, c)| {
                    let f = if total > 0.0 { c as f64 / total } else { 0.0 };
                    let mut base = 1.0f64;
                    for d in 0..n {
                        let dv = match &v[d] {
                            Some(x) => x.clone(),
                            None => Value::Null,
                        };
                        let dk = key_of(&[dv]);
                        let dc = dim_counts[d].get(&dk).copied().unwrap_or(0);
                        base *= if total > 0.0 { dc as f64 / total } else { 0.0 };
                    }
                    (v, f, base)
                })
                .collect();
        }
        // Per-expression stats (cont.)
        for (ii, item) in obj.items.iter().enumerate() {
            if matches!(item, ExtStatItem::Expr(_, _)) {
                let vals = &item_vals[ii];
                let nulls = vals.iter().filter(|v| matches!(v, Value::Null)).count();
                let mut counts: HashMap<Vec<u8>, (Value, u64)> = HashMap::default();
                for v in vals {
                    if matches!(v, Value::Null) {
                        continue;
                    }
                    let mut k = Vec::new();
                    value_key(v, &mut k);
                    let e = counts.entry(k).or_insert_with(|| (v.clone(), 0));
                    e.1 += 1;
                }
                let mut by_freq: Vec<(Value, u64)> =
                    counts.values().map(|(v, c)| (v.clone(), *c)).collect();
                // v2.19: same deterministic tie-break as the MCV
                // list above (rendered key ascending).
                by_freq.sort_by(|a, b| {
                    b.1.cmp(&a.1)
                        .then_with(|| value_to_text_cast(&a.0).cmp(&value_to_text_cast(&b.0)))
                });
                data.expr_stats.push(ExtExprStat {
                    null_frac: if total > 0.0 {
                        nulls as f64 / total
                    } else {
                        0.0
                    },
                    avg_width: 4,
                    n_distinct: counts.len() as f64,
                    // v2.30: expression MCVs keep values seen at
                    // least twice (see scope.md archaeology note).
                    mcv: by_freq
                        .iter()
                        .filter(|(_, c)| *c >= 2)
                        .take(100)
                        .map(|(v, c)| {
                            (v.clone(), if total > 0.0 { *c as f64 / total } else { 0.0 })
                        })
                        .collect(),
                });
            }
        }
    }
    data
}

// ======================================================================
// Catalogs
// ======================================================================

fn qc(name: &str, ty: ColType, qual: &str) -> QCol {
    QCol {
        qual: qual.to_string(),
        name: name.to_string(),
        ty,
        hidden: false,
        src_ord: 0,
    }
}

fn row(cells: Vec<Value>) -> QRow {
    QRow {
        cells: Row::new(cells),
        prov: Vec::new(),
    }
}

fn kinds_text(kinds: &[char]) -> String {
    format!(
        "{{{}}}",
        kinds
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn obj_visible_to(
    db: &Database,
    role: &str,
    obj: &ExtStatObj,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> bool {
    if crate::storage::is_superuser_snap(db, role, snap, own) || obj.owner == role {
        return true;
    }
    if let Some(t) = db.find_table(&obj.table, snap, &[own], session) {
        if t.owner == role {
            return true;
        }
        return crate::storage::table_privs(db, role, t, snap, own) != 0;
    }
    false
}

pub(crate) fn pg_statistic_ext_schema() -> Vec<QCol> {
    let q = "pg_statistic_ext";
    vec![
        qc("oid", ColType::Int, q),
        qc("stxrelid", ColType::Int, q),
        qc("stxname", ColType::Text, q),
        qc("stxnamespace", ColType::Int, q),
        qc("stxowner", ColType::Int, q),
        qc("stxstattarget", ColType::Int, q),
        qc("stxkeys", ColType::Text, q),
        qc("stxkind", ColType::Text, q),
        qc("stxexprs", ColType::Text, q),
    ]
}

pub(crate) fn pg_statistic_ext_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_statistic_ext_schema();
    let mut rows = Vec::new();
    for o in &db.ext_stats {
        let t = db.find_table(&o.table, snap, &[own], session);
        let relid = t.map(|t| t.oid as i64).unwrap_or(0);
        let keys: Vec<String> = o
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| match it {
                ExtStatItem::Column(a) => Some(a.to_string()),
                ExtStatItem::Expr(_, _) => {
                    let _ = i;
                    None
                }
            })
            .collect();
        let exprs: Vec<String> = o
            .items
            .iter()
            .filter_map(|it| match it {
                ExtStatItem::Expr(txt, _) => Some(txt.clone()),
                _ => None,
            })
            .collect();
        rows.push(row(vec![
            Value::Int(o.oid as i64),
            Value::Int(relid),
            Value::text(o.name.as_str()),
            Value::Int(if o.schema == "public" { 2200 } else { 0 }),
            // v2.30: the owner's real role OID (was hardcoded 10, so
            // stxowner::regrole rendered "postgres" for every owner).
            Value::Int(crate::exec::role_oid(&o.owner) as i64),
            match o.stattarget {
                Some(t) => Value::Int(t as i64),
                None => Value::Null,
            },
            Value::text(keys.join(" ").as_str()),
            Value::text(kinds_text(&o.kinds).as_str()),
            Value::text(exprs.join(", ").as_str()),
        ]));
    }
    (schema, rows)
}

/// v2.30: stxdmcv payload. PG stores an opaque pg_mcv_list bytea
/// whose text form is `\x<hex>`; the only expansion path is
/// pg_mcv_list_items(). We serialize the items as JSON inside the
/// hex so the SRF can decode exactly what ANALYZE built:
/// {"items":[{"values":["1",null],"nulls":[false,true],
/// "frequency":0.5,"base_frequency":0.25}, ...]}
pub(crate) fn mcv_serialize(data: &ExtStatData) -> Option<String> {
    if data.mcv.is_empty() {
        return None;
    }
    fn esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }
    let mut items = Vec::new();
    for (vals, freq, base) in &data.mcv {
        let vs: Vec<String> = vals
            .iter()
            .map(|v| match v {
                Some(x) => format!("\"{}\"", esc(&value_to_text_cast(x))),
                None => "null".to_string(),
            })
            .collect();
        let ns: Vec<String> = vals
            .iter()
            .map(|v| if v.is_none() { "true" } else { "false" }.to_string())
            .collect();
        items.push(format!(
            "{{\"values\":[{}],\"nulls\":[{}],\"frequency\":{},\"base_frequency\":{}}}",
            vs.join(","),
            ns.join(","),
            fmt_float_full(*freq),
            fmt_float_full(*base)
        ));
    }
    let json = format!("{{\"items\":[{}]}}", items.join(","));
    let hex: String = json.bytes().map(|b| format!("{:02x}", b)).collect();
    Some(format!("\\x{}", hex))
}

fn fmt_float_full(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{:.1}", x)
    } else {
        format!("{}", x)
    }
}

/// Minimal JSON value for the mcv payload round-trip.
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

fn parse_json(s: &str) -> Option<Json> {
    let b = s.as_bytes();
    let mut i = 0usize;
    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && (b[*i] as char).is_whitespace() {
            *i += 1;
        }
    }
    fn val(b: &[u8], i: &mut usize) -> Option<Json> {
        ws(b, i);
        match *b.get(*i)? {
            b'{' => {
                *i += 1;
                let mut kv = Vec::new();
                ws(b, i);
                if b.get(*i) == Some(&b'}') {
                    *i += 1;
                    return Some(Json::Obj(kv));
                }
                loop {
                    let k = match val(b, i)? {
                        Json::Str(s) => s,
                        _ => return None,
                    };
                    ws(b, i);
                    if b.get(*i) != Some(&b':') {
                        return None;
                    }
                    *i += 1;
                    let v = val(b, i)?;
                    kv.push((k, v));
                    ws(b, i);
                    match b.get(*i) {
                        Some(b',') => *i += 1,
                        Some(b'}') => {
                            *i += 1;
                            return Some(Json::Obj(kv));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                *i += 1;
                let mut vs = Vec::new();
                ws(b, i);
                if b.get(*i) == Some(&b']') {
                    *i += 1;
                    return Some(Json::Arr(vs));
                }
                loop {
                    vs.push(val(b, i)?);
                    ws(b, i);
                    match b.get(*i) {
                        Some(b',') => *i += 1,
                        Some(b']') => {
                            *i += 1;
                            return Some(Json::Arr(vs));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => {
                *i += 1;
                let mut out = String::new();
                loop {
                    let c = *b.get(*i)?;
                    *i += 1;
                    match c {
                        b'"' => return Some(Json::Str(out)),
                        b'\\' => {
                            let e = *b.get(*i)?;
                            *i += 1;
                            match e {
                                b'"' => out.push('"'),
                                b'\\' => out.push('\\'),
                                b'/' => out.push('/'),
                                b'n' => out.push('\n'),
                                b't' => out.push('\t'),
                                b'r' => out.push('\r'),
                                b'u' => {
                                    let h = std::str::from_utf8(b.get(*i..*i + 4)?).ok()?;
                                    let cp = u32::from_str_radix(h, 16).ok()?;
                                    *i += 4;
                                    out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                                }
                                _ => return None,
                            }
                        }
                        _ => {
                            let start = *i - 1;
                            let len = if c < 0x80 {
                                1
                            } else if c >= 0xf0 {
                                4
                            } else if c >= 0xe0 {
                                3
                            } else {
                                2
                            };
                            let end = start + len;
                            out.push_str(std::str::from_utf8(b.get(start..end)?).ok()?);
                            *i = end;
                        }
                    }
                }
            }
            b't' => {
                if b[*i..].starts_with(b"true") {
                    *i += 4;
                    Some(Json::Bool(true))
                } else {
                    None
                }
            }
            b'f' => {
                if b[*i..].starts_with(b"false") {
                    *i += 5;
                    Some(Json::Bool(false))
                } else {
                    None
                }
            }
            b'n' => {
                if b[*i..].starts_with(b"null") {
                    *i += 4;
                    Some(Json::Null)
                } else {
                    None
                }
            }
            _ => {
                let start = *i;
                while *i < b.len()
                    && matches!(b[*i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                {
                    *i += 1;
                }
                std::str::from_utf8(b.get(start..*i)?)
                    .ok()?
                    .parse::<f64>()
                    .ok()
                    .map(Json::Num)
            }
        }
    }
    let v = val(b, &mut i)?;
    ws(b, &mut i);
    if i == b.len() { Some(v) } else { None }
}

/// v2.30: pg_mcv_list_items(stxdmcv) expansion — rows of
/// (index int4, values text[], nulls bool[], frequency float8,
/// base_frequency float8), 1-based index like PG.
pub(crate) fn mcv_list_items_rows(text: &str) -> Vec<Vec<Value>> {
    let Some(hex) = text.strip_prefix("\\x") else {
        return Vec::new();
    };
    let mut bytes: Vec<u8> = Vec::new();
    let hb = hex.as_bytes();
    let mut j = 0usize;
    while j + 2 <= hb.len() {
        let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&hb[j..j + 2]).unwrap_or(""), 16)
        else {
            return Vec::new();
        };
        bytes.push(byte);
        j += 2;
    }
    let Ok(json) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    let Some(Json::Obj(top)) = parse_json(&json) else {
        return Vec::new();
    };
    let Some(Json::Arr(items)) = top.into_iter().find(|(k, _)| k == "items").map(|(_, v)| v) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for (idx, item) in items.into_iter().enumerate() {
        let Json::Obj(fields) = item else {
            continue;
        };
        let get = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);
        let values = match get("values") {
            Some(Json::Arr(vs)) => {
                let elems: Vec<Value> = vs
                    .iter()
                    .map(|v| match v {
                        Json::Str(s) => Value::text(s.as_str()),
                        _ => Value::Null,
                    })
                    .collect();
                let n = elems.len() as i32;
                Value::Array(Box::new(crate::storage::ArrayVal {
                    elem: crate::storage::ArrayElem::Text,
                    dims: vec![n],
                    lower: vec![1],
                    elems,
                }))
            }
            _ => Value::Null,
        };
        let nulls = match get("nulls") {
            Some(Json::Arr(ns)) => {
                let elems: Vec<Value> = ns
                    .iter()
                    .map(|v| match v {
                        Json::Bool(b) => Value::Bool(*b),
                        _ => Value::Null,
                    })
                    .collect();
                let n = elems.len() as i32;
                Value::Array(Box::new(crate::storage::ArrayVal {
                    elem: crate::storage::ArrayElem::Bool,
                    dims: vec![n],
                    lower: vec![1],
                    elems,
                }))
            }
            _ => Value::Null,
        };
        let freq = match get("frequency") {
            Some(Json::Num(f)) => Value::Float(*f),
            _ => Value::Null,
        };
        let base = match get("base_frequency") {
            Some(Json::Num(f)) => Value::Float(*f),
            _ => Value::Null,
        };
        rows.push(vec![Value::Int(idx as i64), values, nulls, freq, base]);
    }
    rows
}

pub(crate) fn pg_statistic_ext_data_schema() -> Vec<QCol> {
    let q = "pg_statistic_ext_data";
    vec![
        qc("stxoid", ColType::Int, q),
        qc("stxdinherit", ColType::Bool, q),
        qc("stxdndistinct", ColType::Text, q),
        qc("stxddependencies", ColType::Text, q),
        qc("stxdmcv", ColType::Text, q),
    ]
}

pub(crate) fn pg_statistic_ext_data_scan(db: &Database) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_statistic_ext_data_schema();
    let mut rows = Vec::new();
    for o in &db.ext_stats {
        if o.data.built {
            rows.push(row(vec![
                Value::Int(o.oid as i64),
                Value::Bool(false),
                match &o.data.ndistinct {
                    Some(s) => Value::text(s.as_str()),
                    None => Value::Null,
                },
                match &o.data.dependencies {
                    Some(s) => Value::text(s.as_str()),
                    None => Value::Null,
                },
                // v2.30: stxdmcv renders as the \x-hex pg_mcv_list
                // payload once an MCV build exists (was always NULL).
                match mcv_serialize(&o.data) {
                    Some(s) => Value::text(s),
                    None => Value::Null,
                },
            ]));
        }
        // v2.30: the inherited (family) build row.
        if let Some(inh) = &o.data_inh {
            if inh.built {
                rows.push(row(vec![
                    Value::Int(o.oid as i64),
                    Value::Bool(true),
                    match &inh.ndistinct {
                        Some(s) => Value::text(s.as_str()),
                        None => Value::Null,
                    },
                    match &inh.dependencies {
                        Some(s) => Value::text(s.as_str()),
                        None => Value::Null,
                    },
                    match mcv_serialize(inh) {
                        Some(s) => Value::text(s),
                        None => Value::Null,
                    },
                ]));
            }
        }
    }
    (schema, rows)
}

fn mcv_render(o: &ExtStatObj) -> (String, String, String, String) {
    if o.data.mcv.is_empty() {
        return ("NULL".into(), "NULL".into(), "NULL".into(), "NULL".into());
    }
    let vals: Vec<String> = o
        .data
        .mcv
        .iter()
        .map(|(vals, _, _)| {
            let inner: Vec<String> = vals
                .iter()
                .map(|v| match v {
                    Some(v) => array_quote(&value_to_text_cast(v)),
                    None => "NULL".to_string(),
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        })
        .collect();
    let nulls: Vec<String> = o
        .data
        .mcv
        .iter()
        .map(|(vals, _, _)| {
            let inner: Vec<String> = vals
                .iter()
                .map(|v| if v.is_none() { "t" } else { "f" }.to_string())
                .collect();
            format!("{{{}}}", inner.join(","))
        })
        .collect();
    let freqs: Vec<String> = o.data.mcv.iter().map(|(_, f, _)| fmt_float(*f)).collect();
    let base: Vec<String> = o.data.mcv.iter().map(|(_, _, b)| fmt_float(*b)).collect();
    (
        format!("{{{}}}", vals.join(",")),
        format!("{{{}}}", nulls.join(",")),
        format!("{{{}}}", freqs.join(",")),
        format!("{{{}}}", base.join(",")),
    )
}

pub(crate) fn pg_stats_ext_schema() -> Vec<QCol> {
    let q = "pg_stats_ext";
    vec![
        qc("schemaname", ColType::Text, q),
        qc("tablename", ColType::Text, q),
        qc("tableid", ColType::Int, q),
        qc("statistics_schemaname", ColType::Text, q),
        qc("statistics_name", ColType::Text, q),
        qc("statistics_id", ColType::Int, q),
        qc("statistics_owner", ColType::Text, q),
        qc("attnames", ColType::Text, q),
        qc("exprs", ColType::Text, q),
        qc("kinds", ColType::Text, q),
        qc("inherited", ColType::Bool, q),
        qc("n_distinct", ColType::Text, q),
        qc("dependencies", ColType::Text, q),
        qc("most_common_vals", ColType::Text, q),
        qc("most_common_val_nulls", ColType::Text, q),
        qc("most_common_freqs", ColType::Text, q),
        qc("most_common_base_freqs", ColType::Text, q),
    ]
}

pub(crate) fn pg_stats_ext_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_stats_ext_schema();
    let mut rows = Vec::new();
    for o in &db.ext_stats {
        if !obj_visible_to(db, role, o, snap, own, session) {
            continue;
        }
        let t = db.find_table(&o.table, snap, &[own], session);
        let (relid, tcols) = match t {
            Some(t) => (t.oid as i64, t.columns.clone()),
            None => (0, Vec::new()),
        };
        let attnames: Vec<String> = o
            .items
            .iter()
            .filter_map(|it| match it {
                ExtStatItem::Column(a) => tcols
                    .get((*a as usize).saturating_sub(1))
                    .map(|(cn, _)| array_quote(cn)),
                _ => None,
            })
            .collect();
        let exprs: Vec<String> = o
            .items
            .iter()
            .filter_map(|it| match it {
                ExtStatItem::Expr(txt, _) => Some(txt.clone()),
                _ => None,
            })
            .collect();
        let (mcv, nulls, freqs, base) = mcv_render(o);
        let txt = |s: String| {
            if s == "NULL" {
                Value::Null
            } else {
                Value::text(s.as_str())
            }
        };
        rows.push(row(vec![
            Value::text(table_schema_for(db, o).as_str()),
            Value::text(stats_base_name(&o.table)),
            Value::Int(relid),
            Value::text(o.schema.as_str()),
            Value::text(o.name.as_str()),
            Value::Int(o.oid as i64),
            Value::text(o.owner.as_str()),
            if attnames.is_empty() {
                Value::Null
            } else {
                Value::text(format!("{{{}}}", attnames.join(",").as_str()))
            },
            if exprs.is_empty() {
                Value::Null
            } else {
                Value::text(exprs.join(", ").as_str())
            },
            Value::text(kinds_text(&o.kinds).as_str()),
            Value::Bool(false),
            match &o.data.ndistinct {
                Some(s) => Value::text(s.as_str()),
                None => Value::Null,
            },
            match &o.data.dependencies {
                Some(s) => Value::text(s.as_str()),
                None => Value::Null,
            },
            txt(mcv),
            txt(nulls),
            txt(freqs),
            txt(base),
        ]));
    }
    (schema, rows)
}

fn table_schema_for(db: &Database, o: &ExtStatObj) -> String {
    let (sch, _) = split_qname(&o.table);
    if let Some(s) = sch {
        return s;
    }
    db.obj_schema
        .get(&o.table)
        .cloned()
        .unwrap_or_else(|| "public".to_string())
}

pub(crate) fn pg_stats_ext_exprs_schema() -> Vec<QCol> {
    let q = "pg_stats_ext_exprs";
    vec![
        qc("schemaname", ColType::Text, q),
        qc("tablename", ColType::Text, q),
        qc("tableid", ColType::Int, q),
        qc("statistics_schemaname", ColType::Text, q),
        qc("statistics_name", ColType::Text, q),
        qc("statistics_id", ColType::Int, q),
        qc("statistics_owner", ColType::Text, q),
        qc("expr", ColType::Text, q),
        qc("inherited", ColType::Bool, q),
        qc("null_frac", ColType::Float, q),
        qc("avg_width", ColType::Int, q),
        qc("n_distinct", ColType::Float, q),
        qc("most_common_vals", ColType::Text, q),
        qc("most_common_freqs", ColType::Text, q),
        qc("histogram_bounds", ColType::Text, q),
        qc("range_length_histogram", ColType::Text, q),
        qc("range_empty_frac", ColType::Float, q),
        qc("range_bounds_histogram", ColType::Text, q),
    ]
}

pub(crate) fn pg_stats_ext_exprs_scan(
    db: &Database,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_stats_ext_exprs_schema();
    let mut rows = Vec::new();
    for o in &db.ext_stats {
        if !obj_visible_to(db, role, o, snap, own, session) {
            continue;
        }
        let t = db.find_table(&o.table, snap, &[own], session);
        let relid = t.map(|t| t.oid as i64).unwrap_or(0);
        let mut ei = 0usize;
        for item in &o.items {
            let ExtStatItem::Expr(txt, _) = item else {
                continue;
            };
            let st = o.data.expr_stats.get(ei);
            ei += 1;
            let (nf, aw, nd, mcv, freqs) = match st {
                Some(s) => {
                    let mv: Vec<String> = s
                        .mcv
                        .iter()
                        .map(|(v, _)| array_quote(&value_to_text_cast(v)))
                        .collect();
                    let fq: Vec<String> = s.mcv.iter().map(|(_, f)| fmt_float(*f)).collect();
                    (
                        s.null_frac,
                        s.avg_width as i64,
                        s.n_distinct,
                        if mv.is_empty() {
                            Value::Null
                        } else {
                            Value::text(format!("{{{}}}", mv.join(",").as_str()))
                        },
                        if fq.is_empty() {
                            Value::Null
                        } else {
                            Value::text(format!("{{{}}}", fq.join(",").as_str()))
                        },
                    )
                }
                None => (0.0, 0, 0.0, Value::Null, Value::Null),
            };
            rows.push(row(vec![
                Value::text(table_schema_for(db, o).as_str()),
                Value::text(stats_base_name(&o.table)),
                Value::Int(relid),
                Value::text(o.schema.as_str()),
                Value::text(o.name.as_str()),
                Value::Int(o.oid as i64),
                Value::text(o.owner.as_str()),
                Value::text(txt.as_str()),
                Value::Bool(false),
                Value::Float(nf),
                Value::Int(aw),
                Value::Float(nd),
                mcv,
                freqs,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]));
        }
    }
    (schema, rows)
}

/// pg_get_statisticsobjdef(oid) rendering (ruleutils.c format).
pub(crate) fn statisticsobjdef(db: &Database, oid: i64) -> Option<String> {
    let o = db.ext_stats.iter().find(|o| o.oid as i64 == oid)?;
    let mut def = format!("CREATE STATISTICS {}.{}", o.schema, o.name);
    // v2.30: ruleutils prints the kinds clause only when the
    // multivariate set differs from the default {d, f, m} (the 'e'
    // expression kind is never spelled in the clause).
    let mv: Vec<char> = o.kinds.iter().copied().filter(|k| *k != 'e').collect();
    if !mv.is_empty() && mv != ['d', 'f', 'm'] {
        let names: Vec<String> = mv
            .iter()
            .map(|k| match k {
                'd' => "ndistinct".to_string(),
                'f' => "dependencies".to_string(),
                _ => "mcv".to_string(),
            })
            .collect();
        def.push_str(&format!(" ({})", names.join(", ")));
    }
    let items: Vec<String> = o.item_texts.clone();
    def.push_str(&format!(
        " ON {} FROM {}",
        items.join(", "),
        stats_base_name(&o.table)
    ));
    Some(def)
}
