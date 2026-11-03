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

fn base_name(q: &str) -> &str {
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
                format!("relation \"{}\" does not exist", base_name(table)),
            ));
        }
    };
    // Ownership: PG requires owning the table (42501).
    if tbl.owner != ctx.role
        && !crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
    {
        return Err(exec_err(
            "42501",
            format!("must be owner of table {}", base_name(table)),
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
    // PG: auto-generated names get a numeric suffix on collision.
    let name_owned: String = if auto_name && find_obj(&eng.db, &obj_schema, name).is_some() {
        let mut idx = 1u32;
        loop {
            let candidate = format!("{}_{}", name, idx);
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
                    format!("column \"{}\" specified more than once", trimmed),
                ));
            }
            seen_cols.push(attnum);
            resolved.push(ExtStatItem::Column(attnum));
            continue;
        }
        if SYSTEM_COLS.contains(&lower.as_str()) {
            return Err(exec_err(
                "42601",
                format!("cannot create statistics on system column \"{}\"", lower),
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
        // "duplicate expression in statistics definition").
        if resolved
            .iter()
            .any(|r| matches!(r, ExtStatItem::Expr(t, _) if t == trimmed))
        {
            return Err(exec_err(
                "42601",
                "duplicate expression in statistics definition".to_string(),
            ));
        }
        let expr = parse_stat_expr(trimmed)?;
        // A parenthesized row of columns is not a valid single item.
        if matches!(expr, Expr::Row(_)) {
            return Err(exec_err(
                "42601",
                "statistics expressions must not be row-valued".to_string(),
            ));
        }
        resolved.push(ExtStatItem::Expr(trimmed.to_string(), expr));
    }
    // PG: at least 2 plain columns for a column-only object; a single
    // expression is allowed (per-expression statistics).
    if resolved.len() < 2 && resolved.iter().all(|i| matches!(i, ExtStatItem::Column(_))) {
        // PG19: a single GENERATED column counts as an expression and
        // is allowed (the virtual_gen_stats_single cases).
        let generated_single = match &resolved[0] {
            ExtStatItem::Column(a) => tbl
                .gen_cols
                .get((*a as usize).saturating_sub(1))
                .is_some_and(|g| g.is_some()),
            _ => false,
        };
        if !generated_single {
            return Err(exec_err(
                "42601",
                "statistics require at least 2 columns".to_string(),
            ));
        }
    }
    let kinds_final: Vec<char> = if kinds.is_empty() {
        vec!['d', 'f', 'm']
    } else {
        let mut k = kinds.to_vec();
        k.sort_unstable();
        k.dedup();
        k
    };
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
    let base = base_name(table).to_string();
    db.ext_stats.retain(|o| base_name(&o.table) != base);
}

// ======================================================================
// ANALYZE build
// ======================================================================

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
        .filter(|(_, o)| base_name(&o.table) == base_name(table))
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
    let mut built: Vec<(usize, ExtStatData)> = Vec::new();
    for pos in indices {
        let obj = eng.db.ext_stats[pos].clone();
        // Evaluate item values for every row.
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
        for r in &vis {
            let frame = Scope {
                schema: &schema,
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
                    ExtStatItem::Expr(_, e) => {
                        match eval_expr(&mut q, std::slice::from_ref(&frame), e) {
                            Ok(v) => v,
                            Err(_) => {
                                ok = false;
                                Value::Null
                            }
                        }
                    }
                };
                item_vals[ii].push(v);
            }
        }
        drop(q);
        if !ok || vis.is_empty() {
            continue;
        }
        let mut data = ExtStatData {
            built: true,
            ..Default::default()
        };
        let n = obj.items.len();
        let key_of = |vals: &[Value]| -> Vec<u8> {
            let mut k = Vec::new();
            for v in vals {
                value_key(v, &mut k);
                k.push(0xff);
            }
            k
        };
        // ndistinct: every non-empty subset.
        if obj.kinds.contains(&'d') {
            let mut parts: Vec<String> = Vec::new();
            for mask in 1usize..(1 << n) {
                let idxs: Vec<usize> = (0..n).filter(|i| mask & (1 << i) != 0).collect();
                let mut set: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
                for ri in 0..vis.len() {
                    let vals: Vec<Value> = idxs.iter().map(|&i| item_vals[i][ri].clone()).collect();
                    set.insert(key_of(&vals));
                }
                let atts: Vec<String> = idxs
                    .iter()
                    .map(|&i| attnum_of(&obj, i).to_string())
                    .collect();
                parts.push(format!("\"{}\": {}", atts.join(", "), set.len()));
            }
            data.ndistinct = Some(format!("{{{}}}", parts.join(", ")));
        }
        // dependencies: ordered (subset -> item) with degree 1.0 kept
        // plus partial degrees >= 0.5 rendered like PG (degree field).
        if obj.kinds.contains(&'f') && n >= 2 {
            let mut deps: Vec<String> = Vec::new();
            for mask in 1usize..(1 << n) {
                let subset: Vec<usize> = (0..n).filter(|i| mask & (1 << i) != 0).collect();
                if subset.len() >= n {
                    continue;
                }
                for dep in 0..n {
                    if subset.contains(&dep) {
                        continue;
                    }
                    // group by subset values; degree = sum of max
                    // per-group frequency of dep / total.
                    let mut groups: HashMap<Vec<u8>, HashMap<Vec<u8>, u64>> = HashMap::default();
                    for ri in 0..vis.len() {
                        let gk = key_of(
                            &subset
                                .iter()
                                .map(|&i| item_vals[i][ri].clone())
                                .collect::<Vec<_>>(),
                        );
                        let dk = key_of(&[item_vals[dep][ri].clone()]);
                        *groups.entry(gk).or_default().entry(dk).or_insert(0) += 1;
                    }
                    let consistent: u64 = groups
                        .values()
                        .map(|m| m.values().max().copied().unwrap_or(0))
                        .sum();
                    let degree = if total > 0.0 {
                        consistent as f64 / total
                    } else {
                        0.0
                    };
                    if degree >= 0.999999 {
                        let atts: Vec<String> = subset
                            .iter()
                            .map(|&i| attnum_of(&obj, i).to_string())
                            .collect();
                        deps.push(format!(
                            "{{\"attributes\": [{}], \"dependency\": {}, \"degree\": {:.6}}}",
                            atts.join(", "),
                            attnum_of(&obj, dep),
                            degree
                        ));
                    }
                }
            }
            data.dependencies = Some(format!("[{}]", deps.join(", ")));
        }
        // MCV over all items.
        if obj.kinds.contains(&'m') {
            let mut counts: HashMap<Vec<u8>, (Vec<Option<Value>>, u64)> = HashMap::default();
            for ri in 0..vis.len() {
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
            let distinct = counts.len() as f64;
            let base = if distinct > 0.0 { 1.0 / distinct } else { 0.0 };
            let mut items: Vec<(Vec<Option<Value>>, u64)> =
                counts.values().map(|(v, c)| (v.clone(), *c)).collect();
            items.sort_by(|a, b| b.1.cmp(&a.1));
            data.mcv = items
                .into_iter()
                .take(100)
                .map(|(v, c)| {
                    let f = if total > 0.0 { c as f64 / total } else { 0.0 };
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
                by_freq.sort_by(|a, b| b.1.cmp(&a.1));
                data.expr_stats.push(ExtExprStat {
                    null_frac: if total > 0.0 {
                        nulls as f64 / total
                    } else {
                        0.0
                    },
                    avg_width: 4,
                    n_distinct: counts.len() as f64,
                    mcv: by_freq
                        .iter()
                        .take(100)
                        .map(|(v, c)| {
                            (v.clone(), if total > 0.0 { *c as f64 / total } else { 0.0 })
                        })
                        .collect(),
                });
            }
        }
        built.push((pos, data));
    }
    for (pos, data) in built {
        if let Some(slot) = eng.db.ext_stats.get_mut(pos) {
            slot.data = data;
        }
    }
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
            Value::Int(10),
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
        if !o.data.built {
            continue;
        }
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
            Value::Null,
        ]));
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
            Value::text(base_name(&o.table)),
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
                Value::text(base_name(&o.table)),
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
    if o.kinds.len() < 3 {
        let names: Vec<String> = o
            .kinds
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
        base_name(&o.table)
    ));
    Some(def)
}
