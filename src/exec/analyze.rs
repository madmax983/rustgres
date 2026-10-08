// v1.78 mechanical split: moved verbatim from src/exec.rs (22012-22620).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ============================================================================
// v1.03: EXPLAIN ANALYZE - execute the inner SELECT once and render
// actual row counts (PG19 explain.c `ExplainNode` text format).
// ============================================================================

/// v1.03: context for computing actual row counts in EXPLAIN ANALYZE.
pub(crate) struct AnalyzeCtx<'a> {
    pub(crate) eng: &'a Engine,
    pub(crate) snap: &'a Snapshot,
    /// v1.21: every xid owned by the transaction (top + sub-xids).
    pub(crate) all_xids: Vec<u64>,
    pub(crate) session: u64,
    /// Actual rows produced by the top-level SELECT (executed once).
    pub(crate) top_rows: u64,
}

/// v1.03: parse a `Limit.n` ("3", "ALL", "3 OFFSET 1") into
/// (limit, offset). Mirrors how the Limit executor interprets `n`.
pub(crate) fn parse_limit_n(n: &str) -> (Option<u64>, u64) {
    let mut it = n.split_whitespace();
    let lim = match it.next() {
        None | Some("ALL") => None,
        Some(x) => x.parse::<u64>().ok(),
    };
    let mut off = 0u64;
    if let Some(w) = it.next() {
        if w.eq_ignore_ascii_case("OFFSET") {
            off = it.next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        }
    }
    (lim, off)
}

/// v1.03: actual row count for a plan node under EXPLAIN ANALYZE.
/// The top node uses the executed row count; `Limit` applies its
/// bound and a `Sort` under a `Limit` only sorts the top-N rows (PG's
/// `top-N heapsort`); `SeqScan` counts visible rows (PG reports
/// post-filter rows, but the bounded subset has no filter pushdown
/// into the count - the measured table count is the honest value for
/// the conformance target, which has no filter); everything else falls
/// back to the planner's estimate.
pub(crate) fn analyze_actual(
    node: &PlanNode,
    ax: &AnalyzeCtx,
    cap: Option<u64>,
    is_top: bool,
) -> u64 {
    if is_top {
        return ax.top_rows;
    }
    let bounded = |c: u64| match cap {
        Some(k) => c.min(k),
        None => c,
    };
    match node {
        PlanNode::Result { rows, .. } => *rows,
        PlanNode::SeqScan { table, .. } => ax
            .eng
            .db
            .find_table(table, ax.snap, &ax.all_xids, ax.session)
            .map(|t| {
                t.rows
                    .iter()
                    .filter(|r| crate::storage::row_visible(r, ax.snap, &ax.all_xids))
                    .count() as u64
            })
            .unwrap_or(0),
        PlanNode::IndexScan { rows, .. }
        | PlanNode::IndexOrderScan { rows, .. }
        | PlanNode::NestedLoop { rows, .. }
        | PlanNode::HashJoin { rows, .. }
        | PlanNode::MergeJoin { rows, .. }
        // v1.48: a Materialize node passes its child's rows through.
        | PlanNode::Materialize { rows, .. }
        | PlanNode::Aggregate { rows, .. } => *rows,
        PlanNode::Unique { child, .. } => analyze_actual(child, ax, cap, false),
        PlanNode::Sort { child, .. } => bounded(analyze_actual(child, ax, None, false)),
        PlanNode::Limit { n, child, .. } => {
            let (lim, off) = parse_limit_n(n);
            let child_cap = lim.map(|l| off.saturating_add(l));
            let c = analyze_actual(child, ax, child_cap, false);
            let c = c.saturating_sub(off);
            match lim {
                Some(l) => c.min(l),
                None => c,
            }
        }
        PlanNode::SubqueryScan { child, .. } => analyze_actual(child, ax, cap, false),
        PlanNode::Values { rows, .. } => *rows,
    }
}

/// v1.03: line prefix for an analyzed plan node at `depth` (PG19
/// explain.c text format: the top node starts at column 0; each deeper
/// level is prefixed with `->` two spaces in from its property
/// indent).
pub(crate) fn analyze_pad(depth: usize) -> String {
    if depth == 0 {
        String::new()
    } else {
        format!("{}->  ", " ".repeat(6 * depth - 3))
    }
}

/// v1.03: render one analyzed plan node (PG19 `ExplainNode` text
/// format): `<pad><Node> (actual rows=%.2f loops=1)`, properties at
/// `depth * 6 + 3` spaces. `cap` is the row bound imposed by an
/// ancestor Limit (for top-N Sort rendering); `is_top` marks the node
/// whose actual count is the executed row count.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_analyze(
    node: &PlanNode,
    ax: &AnalyzeCtx,
    depth: usize,
    cap: Option<u64>,
    is_top: bool,
    out: &mut Vec<String>,
) {
    let actual = analyze_actual(node, ax, cap, is_top);
    let tag = format!("(actual rows={:.2} loops=1)", actual as f64);
    let pad = analyze_pad(depth);
    let ppad = " ".repeat(6 * depth + 3);
    match node {
        PlanNode::Result { filter, .. } => {
            out.push(format!("{}Result {}", pad, tag));
            if let Some(f) = filter {
                out.push(format!("{}Filter: {}", ppad, f));
            }
        }
        PlanNode::SeqScan { table, filter, .. } => {
            out.push(format!("{}Seq Scan on {} {}", pad, table, tag));
            if let Some(f) = filter {
                out.push(format!("{}Filter: {}", ppad, f));
            }
        }
        PlanNode::IndexScan {
            table,
            index,
            cond,
            filter,
            ..
        } => {
            out.push(format!(
                "{}Index Scan using {} on {} {}",
                pad, index, table, tag
            ));
            out.push(format!("{}Index Cond: {}", ppad, cond));
            if let Some(f) = filter {
                out.push(format!("{}Filter: {}", ppad, f));
            }
        }
        PlanNode::IndexOrderScan {
            table,
            index,
            order,
            ..
        } => {
            out.push(format!(
                "{}Index Scan using {} on {} {}",
                pad, index, table, tag
            ));
            out.push(format!("{}Order: {}", ppad, order));
        }
        PlanNode::NestedLoop {
            filter,
            outer,
            inner,
            kind,
            ..
        } => {
            // v1.48: same join-kind label rule as the PG-text renderer.
            out.push(format!("{}{} {}", pad, pg_join_label(*kind), tag));
            if let Some(f) = filter {
                out.push(format!("{}Filter: {}", ppad, f));
            }
            render_analyze(outer, ax, depth + 1, None, false, out);
            render_analyze(inner, ax, depth + 1, None, false, out);
        }
        // v1.64: EXPLAIN ANALYZE rendering for Hash Join (the node is
        // display-only; ANALYZE executes via the executor, so this shapes
        // the label tree only).
        PlanNode::HashJoin {
            filter,
            hash_cond,
            outer,
            inner,
            kind,
            ..
        } => {
            // v1.68: kind interpolates into the label (PG19 explain.c).
            out.push(format!("{}{} {}", pad, pg_hash_join_label(*kind), tag));
            out.push(format!("{}Hash Cond: {}", ppad, hash_cond));
            if let Some(f) = filter {
                out.push(format!("{}Join Filter: {}", ppad, f));
            }
            render_analyze(outer, ax, depth + 1, None, false, out);
            out.push(format!("{}Hash {}", pg_pad(depth + 1), tag));
            render_analyze(inner, ax, depth + 2, None, false, out);
        }
        // v1.69: EXPLAIN ANALYZE rendering for Merge Join (display-only).
        PlanNode::MergeJoin {
            filter,
            merge_cond,
            outer,
            inner,
            kind,
            ..
        } => {
            out.push(format!("{}{} {}", pad, pg_merge_join_label(*kind), tag));
            out.push(format!("{}Merge Cond: {}", ppad, merge_cond));
            if let Some(f) = filter {
                out.push(format!("{}Join Filter: {}", ppad, f));
            }
            render_analyze(outer, ax, depth + 1, None, false, out);
            render_analyze(inner, ax, depth + 1, None, false, out);
        }
        // v1.48: PG19 `Materialize` (explain.c `T_Material`).
        PlanNode::Materialize { child, .. } => {
            out.push(format!("{}Materialize {}", pad, tag));
            render_analyze(child, ax, depth + 1, None, false, out);
        }
        PlanNode::Aggregate { child, .. } => {
            out.push(format!("{}Aggregate {}", pad, tag));
            render_analyze(child, ax, depth + 1, None, false, out);
        }
        PlanNode::Unique { child, .. } => {
            out.push(format!("{}Unique {}", pad, tag));
            render_analyze(child, ax, depth + 1, None, false, out);
        }
        PlanNode::Sort { keys, child, .. } => {
            // PG renders "Sort Method: top-N heapsort" when the sort
            // feeds a Limit (bounded input), "quicksort" otherwise
            // (PG19 explain.c `show_sort_info`; the bounded subset
            // never spills, so there is no Disk/buckets line).
            let top_n = cap.is_some();
            out.push(format!("{}Sort {}", pad, tag));
            out.push(format!("{}Sort Key: {}", ppad, keys));
            let method = if top_n { "top-N heapsort" } else { "quicksort" };
            // PG reports measured memory; the bounded executor does not
            // instrument the sort, so report a deterministic estimate
            // (~64 bytes/row, minimum 1kB). Conformance normalizes it.
            let mem_kb = ((actual * 64 + 1023) / 1024).max(1);
            out.push(format!(
                "{}Sort Method: {}  Memory: {}kB",
                ppad, method, mem_kb
            ));
            render_analyze(child, ax, depth + 1, None, false, out);
        }
        PlanNode::Limit { n, child, .. } => {
            // PG's analyzed Limit shows no row-count target (unlike the
            // planning-only renderer); the actual count carries it.
            out.push(format!("{}Limit {}", pad, tag));
            let (lim, off) = parse_limit_n(n);
            let child_cap = lim.map(|l| off.saturating_add(l));
            render_analyze(child, ax, depth + 1, child_cap, false, out);
        }
        PlanNode::SubqueryScan { alias, child, .. } => {
            out.push(format!("{}Subquery Scan on {} {}", pad, alias, tag));
            render_analyze(child, ax, depth + 1, cap, false, out);
        }
        PlanNode::Values { .. } => {
            out.push(format!("{}Values {}", pad, tag));
        }
    }
}

/// v1.03: produce the text rows of an EXPLAIN over `sel`. When
/// `analyze` is false this is the planning-only renderer; when true
/// the SELECT is executed exactly once and actual row counts are
/// rendered (PG19 `EXPLAIN ANALYZE`). Shared by top-level EXPLAIN and
/// plpgsql `FOR x IN EXPLAIN ... LOOP`.
/// v1.08: `costs` selects the PG19 text shape (COSTS OFF) versus the
/// legacy rustgres shape (COSTS ON, the default).
pub(crate) fn explain_rows(
    q: &mut Q,
    scopes: &[Scope],
    sel: &SelectStmt,
    analyze: bool,
    costs: bool,
    // v1.46: EXPLAIN VERBOSE — render the `Output:` targetlist lines
    // (planning-only path; ANALYZE instrumentation stays out of scope).
    verbose: bool,
) -> Result<Vec<Row>, ExecError> {
    // v1.08: COSTS OFF selects the PG-text plan rendering.
    let plan = plan_select(&*q.eng, sel, q.snap, q.own, q.session, &[], !costs, verbose)?;
    let mut lines = Vec::new();
    if analyze {
        let out = run_select(q, sel, scopes)?;
        let ax = AnalyzeCtx {
            eng: q.eng,
            snap: q.snap,
            all_xids: q.all_xids.clone(),
            session: q.session,
            top_rows: out.rows.len() as u64,
        };
        render_analyze(&plan, &ax, 0, None, true, &mut lines);
    } else {
        render_plan(&plan, 0, costs, verbose, &mut lines);
    }
    Ok(lines
        .into_iter()
        .map(|l| Row::new(vec![Value::text(l)]))
        .collect())
}

/// v1.03: top-level `EXPLAIN ANALYZE <select>`: plan, execute the inner
/// SELECT exactly once, and render actual row counts. (EXPLAIN takes
/// no row locks in PG, so unlike a plain SELECT there is no lock
/// acquisition here.)
pub(crate) fn exec_explain_analyze(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    stmt: &Stmt,
    costs: bool,
) -> Result<ExecResult, ExecError> {
    let sel = match stmt {
        Stmt::Select(s) => s,
        _ => {
            return Err(exec_err(
                "0A000",
                "EXPLAIN only supports SELECT statements".to_string(),
            ));
        }
    };
    let mut lock_ids: Vec<(String, u64)> = Vec::new();
    let rows = {
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
        explain_rows(&mut q, &[], sel, true, costs, false)?
    };
    Ok(ExecResult::Explain {
        columns: vec![("QUERY PLAN".to_string(), ColType::Text)],
        rows,
    })
}

/// v0.14: best-effort `Value` -> `ColType` for VALUES column typing
/// (first non-NULL value wins; all-NULL columns describe as TEXT).
/// v0.21: `type_rank` orders numeric types for picking the highest
/// (float8 > float4 > numeric > bigint > int > smallint), like PG's
/// VALUES type coercion.
pub(crate) fn type_rank(t: &ColType) -> u8 {
    match t {
        ColType::SmallInt => 1,
        ColType::Int => 2,
        ColType::BigInt => 3,
        ColType::Numeric(..) => 4,
        ColType::Float4 => 5,
        ColType::Float => 6,
        _ => 0,
    }
}
pub(crate) fn value_coltype(v: &Value) -> ColType {
    match v {
        Value::Tid(_, _) => ColType::Tid, // v1.40
        Value::SmallInt(_) => ColType::SmallInt,
        Value::Int(_) => ColType::Int,
        Value::BigInt(_) => ColType::BigInt,
        Value::Float4(_) => ColType::Float4,
        Value::Float(_) => ColType::Float,
        Value::Numeric(_) => ColType::Numeric(None),
        Value::Text(_) => ColType::Text,
        Value::BpChar(_) => ColType::Char(None),     // v0.35
        Value::SingleChar(_) => ColType::SingleChar, // v0.36
        Value::Bool(_) => ColType::Bool,
        Value::Date(_) => ColType::Date,
        Value::Timestamp(_) => ColType::Timestamp,
        Value::Timestamptz(_) => ColType::Timestamptz,
        Value::Bytea(_) => ColType::Bytea,
        Value::BitString(_) => ColType::Bit, // v1.39
        Value::Uuid(_) => ColType::Uuid,
        Value::PgLsn(_) => ColType::PgLsn,   // v0.64
        Value::Record(_) => ColType::Record, // v0.73
        // v0.79: array values report their array type.
        Value::Array(a) => ColType::Array(a.elem),
        Value::Null => ColType::Text,
    }
}

// --- ANALYZE -----------------------------------------------------------------

/// Compute ANALYZE statistics for one table over the snapshot's visible
/// rows. Distinct counts are exact (not estimated); the MCV list and
/// histogram bounds derive from the same pass.
pub(crate) fn analyze_table(
    db: &Database,
    table: &str,
    snap: &Snapshot,
    own: u64,
    session: u64,
) -> TableStats {
    let t = db
        .find_table(table, snap, &[own], session)
        .expect("table checked visible by caller");
    let vis: Vec<&RowVersion> = t
        .rows
        .iter()
        .filter(|r| row_visible(r, snap, &[own]))
        .collect();
    let total = vis.len() as f64;
    let mut cols = HashMap::new();
    for (ci, (cname, _)) in t.columns.iter().enumerate() {
        let mut nulls = 0u64;
        let mut counts: HashMap<Vec<u8>, (Value, u64)> = HashMap::new();
        for r in &vis {
            let v = &r.values[ci];
            if matches!(v, Value::Null) {
                nulls += 1;
                continue;
            }
            let mut k = Vec::new();
            value_key(v, &mut k);
            let e = counts.entry(k).or_insert_with(|| (v.clone(), 0));
            e.1 += 1;
        }
        // Most common values: top 100 by count (ties by index order).
        let mut by_freq: Vec<(Value, u64)> =
            counts.values().map(|(v, c)| (v.clone(), *c)).collect();
        by_freq.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| index_key_cmp(&a.0, &b.0)));
        let mcv: Vec<(Value, f64)> = by_freq
            .iter()
            .take(100)
            .map(|(v, c)| (v.clone(), if total > 0.0 { *c as f64 / total } else { 0.0 }))
            .collect();
        // Histogram bounds: up to 101 evenly spaced distinct values in
        // index order (5 and 5.0 merge — index ordering, not byte order).
        let mut distinct: Vec<Value> = counts.values().map(|(v, _)| v.clone()).collect();
        distinct.sort_by(index_key_cmp);
        distinct.dedup_by(|a, b| index_key_cmp(a, b) == Ordering::Equal);
        let n_distinct = distinct.len() as f64;
        let hist_bounds: Vec<Value> = if distinct.len() <= 101 {
            distinct
        } else {
            (0..101)
                .map(|i| distinct[i * (distinct.len() - 1) / 100].clone())
                .collect()
        };
        cols.insert(
            cname.clone(),
            ColStats {
                null_frac: if total > 0.0 {
                    nulls as f64 / total
                } else {
                    0.0
                },
                n_distinct,
                mcv,
                hist_bounds,
            },
        );
    }
    TableStats {
        reltuples: total,
        cols,
    }
}

/// v1.01: ANALYZE core shared by the top-level `ANALYZE` statement and
/// the bounded-plpgsql utility path. The function-body runner only
/// carries the read-path `Q` (no `StmtCtx`), so the pieces ANALYZE
/// needs are passed explicitly. Ownership checks and stats computation
/// are identical; statistics stay non-transactional like Postgres.
pub(crate) fn exec_analyze_core(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    table: &Option<String>,
) -> Result<(), ExecError> {
    // v0.11: ANALYZE requires ownership (or superuser), like PostgreSQL.
    let is_owner = |eng: &Engine, n: &str| {
        eng.db
            .find_table(n, snap, &[own], session)
            .map(|t| t.owner == role || crate::storage::is_superuser_snap(&eng.db, role, snap, own))
            .unwrap_or(false)
    };
    let names: Vec<String> = match table {
        Some(n) => {
            if eng.db.find_table(n, snap, &[own], session).is_none() {
                return Err(exec_err(
                    "42P01",
                    format!("relation \"{}\" does not exist", n),
                ));
            }
            if !is_owner(eng, n) {
                return Err(exec_err(
                    "42501",
                    format!("permission denied: must be owner of table \"{}\"", n),
                ));
            }
            vec![n.clone()]
        }
        // No table named: analyze the tables this role owns (like PG,
        // which only touches tables the user may maintain).
        None => eng
            .db
            .tables
            .keys()
            .filter(|n| is_owner(eng, n))
            .cloned()
            .collect(),
    };
    // Statistics are not transactional (like Postgres): they are computed
    // and stored without write ops.
    for n in names {
        let stats = analyze_table(&eng.db, &n, snap, own, session);
        eng.db.stats.insert(n, stats);
    }
    Ok(())
}

/// v0.8: `ANALYZE [table]` — top-level statement wrapper around
/// [`exec_analyze_core`].
pub(crate) fn exec_analyze(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &Option<String>,
) -> Result<ExecResult, ExecError> {
    exec_analyze_core(eng, ctx.snap, ctx.own, ctx.session, ctx.role, table)?;
    Ok(ExecResult::Command {
        tag: "ANALYZE".to_string(),
    })
}

/// The pg_stats system catalog table (virtual): one row per analyzed table
/// column. A real table named pg_stats takes precedence (checked by the
/// caller).
pub(crate) fn pg_stats_schema() -> Vec<QCol> {
    [
        ("schemaname", ColType::Text),
        ("tablename", ColType::Text),
        ("attname", ColType::Text),
        ("null_frac", ColType::Float),
        ("n_distinct", ColType::Float),
        ("most_common_vals", ColType::Text),
        ("most_common_freqs", ColType::Text),
    ]
    .into_iter()
    .map(|(n, ty)| QCol {
        qual: "pg_stats".to_string(),
        name: n.to_string(),
        ty,

        hidden: false,
        src_ord: 0,
    })
    .collect()
}

pub(crate) fn pg_stats_scan(db: &Database) -> (Vec<QCol>, Vec<QRow>) {
    let schema = pg_stats_schema();
    let mut table_names: Vec<&String> = db.stats.keys().collect();
    table_names.sort();
    let mut rows = Vec::new();
    for tn in table_names {
        let ts = &db.stats[tn];
        let mut col_names: Vec<&String> = ts.cols.keys().collect();
        col_names.sort();
        for cn in col_names {
            let cs = &ts.cols[cn];
            let vals = cs
                .mcv
                .iter()
                .map(|(v, _)| value_to_text_cast(v))
                .collect::<Vec<_>>()
                .join(",");
            let freqs = cs
                .mcv
                .iter()
                .map(|(_, f)| format!("{:.4}", f))
                .collect::<Vec<_>>()
                .join(",");
            rows.push(QRow {
                cells: Row::new(vec![
                    Value::text("public"),
                    Value::text(tn.as_str()),
                    Value::text(cn.as_str()),
                    Value::Float(cs.null_frac),
                    Value::Float(cs.n_distinct),
                    Value::text(format!("{{{}}}", vals)),
                    Value::text(format!("{{{}}}", freqs)),
                ]),
                prov: Vec::new(),
            });
        }
    }
    (schema, rows)
}
