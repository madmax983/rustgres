// v1.78 mechanical split: moved verbatim from src/exec.rs (3621-4477).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ============================================================================
// v0.9: constraint enforcement (NOT NULL / CHECK / FOREIGN KEY / DEFAULT).
// ============================================================================

/// Owned copy of a table's constraint metadata, so checks can run while
/// `eng` is mutably borrowed for expression evaluation.
#[derive(Clone)]
pub(crate) struct TableMeta {
    pub(crate) columns: Vec<(String, ColType)>,
    /// v0.84: named composite type per column, parallel to `columns`
    /// (`Some(name)` iff the ColType is `Composite`).
    pub(crate) composite_types: Vec<Option<String>>,
    /// v0.85: domain name per column, parallel to `columns`
    /// (`Some(d)` iff the column was declared with domain type `d`).
    pub(crate) domain_types: Vec<Option<String>>,
    /// v0.85: iff `domain_types[i]` is `Some` and the column was
    /// declared as `d[]`: the domain applies per array element.
    pub(crate) domain_elem: Vec<bool>,
    pub(crate) not_null: Vec<bool>,
    pub(crate) defaults: Vec<Option<DefaultExpr>>,
    pub(crate) checks: Vec<CheckDef>,
    pub(crate) pkey: Option<UniqueDef>,
    pub(crate) fks: Vec<FkDef>,
}

impl TableMeta {
    pub(crate) fn of(t: &Table) -> Self {
        TableMeta {
            columns: t.columns.clone(),
            composite_types: t.composite_types.clone(),
            domain_types: t.domain_types.clone(),
            domain_elem: t.domain_elem.clone(),
            not_null: t.not_null.clone(),
            defaults: t.defaults.clone(),
            checks: t.checks.clone(),
            pkey: t.pkey.clone(),
            fks: t.fks.clone(),
        }
    }

    pub(crate) fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(n, _)| n == name)
    }
}

/// Evaluate a column DEFAULT to a value of the column's type.
pub(crate) fn eval_default(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    d: &DefaultExpr,
    ctype: &ColType,
    cname: &str,
) -> Result<Value, ExecError> {
    match d {
        DefaultExpr::Lit(lit) => coerce_literal(lit, ctype, cname),
        DefaultExpr::Nextval(seq) => {
            let v = seq_nextval(eng, snap, own, session, role, seq)?;
            coerce_value(Value::BigInt(v), ctype, cname)
        }
        DefaultExpr::Expr(e) => {
            let mut lock_ids = Vec::new();
            let mut q = Q {
                eng,
                snap,
                own,
                all_xids: vec![own],
                session,
                role,
                // v0.17: DML-only helper — INSERT/UPDATE/DELETE are
                // statement-blocked when read-only, so this is false.
                read_only: false,
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
                write: None,
            };
            let v = eval_expr(&mut q, &[], e)?;
            coerce_value(v, ctype, cname)
        }
    }
}

/// v0.85: enforce a domain's constraints on a finished value (PG19
/// `coerce_to_domain` / `domain_check`). The checks run only on the
/// fully reconstructed container value — never on partial subfield
/// assignments. TRUE and NULL pass; only FALSE violates (23514),
/// matching the table CHECK semantics. `is_elem` marks the
/// array-of-domain case: each array element is checked as a domain
/// value (a NULL array passes whole).
pub(crate) fn check_domain_value(
    q: &mut Q,
    dname: &str,
    value: &Value,
    is_elem: bool,
) -> Result<Value, ExecError> {
    let dom = q
        .eng
        .db
        .types
        .get(dname)
        .and_then(|st| st.domain.clone())
        .ok_or_else(|| exec_err("42704", format!("type \"{}\" does not exist", dname)))?;
    if is_elem {
        // Array of domain: check each element against the domain.
        match value {
            Value::Null => return Ok(Value::Null),
            Value::Array(a) => {
                for e in &a.elems {
                    check_domain_value(q, dname, e, false)?;
                }
                return Ok(value.clone());
            }
            _ => {}
        }
    }
    if matches!(value, Value::Null) {
        if dom.not_null {
            return Err(exec_err(
                "23502",
                format!("value for domain {} violates not-null constraint", dname),
            ));
        }
        // PG19 still enforces inner domains' NOT NULL on nulls.
        if let Some(inner) = &dom.base_domain {
            check_domain_value(q, inner, value, false)?;
        }
        return Ok(Value::Null);
    }
    // Domain over domain: the inner domain's constraints run first
    // (PG19 checks innermost first).
    if let Some(inner) = &dom.base_domain {
        check_domain_value(q, inner, value, false)?;
    }
    // This domain's CHECKs see a one-column scope named `value` (PG19's
    // VALUE pseudo-column).
    for c in &dom.checks {
        let schema = vec![QCol {
            qual: String::new(),
            name: "value".to_string(),
            ty: dom.base.clone(),
            hidden: false,
            src_ord: 0,
        }];
        let frame = Scope {
            schema: &schema,
            row: std::slice::from_ref(value),
            prov: None,
        };
        let result = eval_expr(q, &[frame], &c.expr)?;
        if matches!(result, Value::Bool(false)) {
            return Err(exec_err(
                "23514",
                format!(
                    "value for domain {} violates check constraint \"{}\"",
                    dname, c.name
                ),
            ));
        }
    }
    Ok(value.clone())
}

/// v0.97: the domain name for a `pg_typeof(arg)` argument that is
/// statically domain-typed (PG19 resolves `pg_typeof` at parse
/// analysis and reports the domain, not the base type). Handles
/// `pg_typeof('x'::d)` (a `CastNamed` to a domain) and
/// `pg_typeof(col)` where the column resolves through the scope
/// chain to a table with a domain-typed column (best-effort: table
/// aliases do not resolve to the base table, so those fall back to
/// the base type). Returns None otherwise — the caller falls back
/// to the value's type name.
pub(crate) fn pg_typeof_domain_name(q: &Q, scopes: &[Scope], arg: &Expr) -> Option<String> {
    let is_domain = |n: &str| q.eng.db.types.get(n).is_some_and(|st| st.domain.is_some());
    match arg {
        Expr::CastNamed { name, .. } => is_domain(name).then(|| name.clone()),
        Expr::Column { table, name: col } => {
            let (si, ci) = resolve_col(scopes, table.as_deref(), col).ok()?;
            let qual = scopes[si].schema[ci].qual.clone();
            let t = q.eng.db.find_table(&qual, q.snap, &q.all_xids, q.session)?;
            let idx = t.columns.iter().position(|(n, _)| n == col)?;
            t.domain_types.get(idx).and_then(Clone::clone)
        }
        _ => None,
    }
}

/// v0.85: coerce a record value to a named composite by position
/// (PG19 assignment coercion): record field `i` goes to composite
/// field `i`, each coerced to the field's type; missing fields become
/// NULL and extras are dropped. Used for bare `ROW(...)` values
/// assigned to composite (or domain-over-composite) columns.
pub(crate) fn coerce_record_to_composite(
    eng: &Engine,
    rec: &[(String, Value)],
    tname: &str,
) -> Result<Value, ExecError> {
    let st = eng
        .db
        .types
        .get(tname)
        .ok_or_else(|| exec_err("42704", format!("type \"{}\" does not exist", tname)))?;
    let cdef = st.composite.clone().ok_or_else(|| {
        exec_err(
            "0A000",
            format!("type \"{}\" is not a composite type", tname),
        )
    })?;
    let mut out: Vec<(String, Value)> = Vec::with_capacity(cdef.len());
    for (i, (fname, fty, _nested)) in cdef.iter().enumerate() {
        let fv = rec
            .get(i)
            .map(|(_, val)| val.clone())
            .unwrap_or(Value::Null);
        let cv = eval_cast(&fv, fty.clone())?;
        out.push((fname.clone(), cv));
    }
    Ok(Value::Record(out))
}

/// v0.85: after the ordinary assignment coercion, a record value
/// assigned to a composite (or domain-over-composite) column is
/// coerced by position. Non-record values pass through unchanged.
pub(crate) fn coerce_assign_composite(
    eng: &Engine,
    v: Value,
    comp: Option<&str>,
) -> Result<Value, ExecError> {
    match (v, comp) {
        (Value::Record(rec), Some(tname)) => coerce_record_to_composite(eng, &rec, tname),
        (v, _) => Ok(v),
    }
}

/// v0.85: the DEFAULT for a domain-typed column with no column-level
/// DEFAULT: the domain's own DEFAULT (PG19 falls back to it). Does not
/// apply to `d[]` columns (the array column's type is not the domain).
pub(crate) fn domain_default<'e>(
    eng: &'e Engine,
    meta: &TableMeta,
    ci: usize,
) -> Option<&'e DefaultExpr> {
    if meta.domain_elem.get(ci).copied().unwrap_or(false) {
        return None;
    }
    let dname = meta.domain_types.get(ci)?.as_deref()?;
    eng.db.types.get(dname)?.domain.as_ref()?.default.as_ref()
}

/// NOT NULL + CHECK validation for a fully-built row (INSERT / UPDATE /
/// cascades writes). SQLSTATEs 23502 / 23514, like Postgres.
/// v0.85: domain CHECKs are enforced here too, on the finished row
/// (PG19 checks domain constraints during value coercion, before the
/// table's own CHECK constraints).
pub(crate) fn check_row_constraints(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    meta: &TableMeta,
    table: &str,
    values: &[Value],
) -> Result<(), ExecError> {
    for (i, (name, _)) in meta.columns.iter().enumerate() {
        if meta.not_null[i] && matches!(values[i], Value::Null) {
            return Err(exec_err(
                "23502",
                format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    name, table
                ),
            ));
        }
    }
    // v0.85: domain CHECK constraints (PG19 coerce_to_domain) — on the
    // finished value, before the table's own CHECK constraints.
    for (i, dname) in meta.domain_types.iter().enumerate() {
        if let Some(dname) = dname {
            let is_elem = meta.domain_elem.get(i).copied().unwrap_or(false);
            let mut lock_ids = Vec::new();
            let mut q = Q {
                eng,
                snap,
                own,
                all_xids: vec![own],
                session,
                role,
                // v0.17: DML-only helper — INSERT/UPDATE/DELETE are
                // statement-blocked when read-only, so this is false.
                read_only: false,
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
                write: None,
            };
            check_domain_value(&mut q, dname, &values[i], is_elem)?;
        }
    }
    if meta.checks.is_empty() {
        return Ok(());
    }
    let schema: Vec<QCol> = meta
        .columns
        .iter()
        .map(|(n, ty)| QCol {
            qual: String::new(),
            name: n.clone(),
            ty: ty.clone(),

            hidden: false,
            src_ord: 0,
        })
        .collect();
    for check in &meta.checks {
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng,
            snap,
            own,
            all_xids: vec![own],
            session,
            role,
            // v0.17: DML-only helper — INSERT/UPDATE/DELETE are
            // statement-blocked when read-only, so this is false.
            read_only: false,
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
            write: None,
        };
        let frame = Scope {
            schema: &schema,
            row: values,
            prov: None,
        };
        let v = eval_expr(&mut q, &[frame], &check.expr)?;
        // Postgres CHECK passes on TRUE or NULL; only FALSE fails it.
        if matches!(v, Value::Bool(false)) {
            // v0.77: an ALTER-added NOT NULL constraint is stored as an
            // `IS NOT NULL` check; PG19 reports its violation as 23502
            // (not-null violation), not 23514 (check violation). The
            // `CheckKind` marker decides — not the expression shape — so
            // an ordinary user CHECK with an `IS NOT NULL` shape still
            // reports 23514.
            if check.kind == crate::sql::CheckKind::NotNull {
                if let crate::sql::Expr::IsNull { expr, neg: true } = &check.expr {
                    if let crate::sql::Expr::Column { table: None, name } = expr.as_ref() {
                        return Err(exec_err(
                            "23502",
                            format!(
                                "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                                name, table
                            ),
                        ));
                    }
                }
            }
            return Err(exec_err(
                "23514",
                format!(
                    "new row for relation \"{}\" violates check constraint \"{}\"",
                    table, check.name
                ),
            ));
        }
    }
    Ok(())
}

/// Resolve an FK's referenced column names (empty = parent's primary key).
pub(crate) fn fk_ref_cols(parent_meta: &TableMeta, fk: &FkDef) -> Result<Vec<String>, ExecError> {
    if fk.ref_cols.is_empty() {
        match &parent_meta.pkey {
            Some(pk) => Ok(pk.cols.clone()),
            None => Err(exec_err(
                "42830",
                format!(
                    "there is no primary key for referenced table \"{}\"",
                    fk.ref_table
                ),
            )),
        }
    } else {
        Ok(fk.ref_cols.clone())
    }
}

/// Child-side foreign-key check for one new row (INSERT / UPDATE /
/// cascaded SET NULL / SET DEFAULT). `self_new` holds the statement's own
/// new rows on the child table (self-references); `ignore_id` excludes the
/// row version being replaced (self-referencing UPDATE).
///
/// MATCH SIMPLE semantics: a NULL in any referencing column skips the
/// check. Violation is SQLSTATE 23503.
pub(crate) fn check_fk_child_row(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    child_meta: &TableMeta,
    child_table: &str,
    values: &[Value],
    self_new: &[Row],
    ignore_id: Option<u64>,
) -> Result<(), ExecError> {
    for fk in &child_meta.fks {
        let child_pos: Vec<usize> = fk
            .cols
            .iter()
            .map(|c| {
                child_meta
                    .column_index(c)
                    .expect("fk columns validated at DDL")
            })
            .collect();
        let key: Vec<Value> = child_pos.iter().map(|&i| values[i].clone()).collect();
        if key.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        let parent = eng
            .db
            .find_table(&fk.ref_table, snap, &[own], session)
            .expect("fk parent validated at DDL time");
        let parent_meta = TableMeta::of(parent);
        let ref_cols = fk_ref_cols(&parent_meta, fk)?;
        let parent_pos: Vec<usize> = ref_cols
            .iter()
            .map(|c| {
                parent
                    .column_index(c)
                    .expect("fk ref cols validated at DDL")
            })
            .collect();
        let matches_key = |vals: &[Value]| {
            parent_pos
                .iter()
                .zip(key.iter())
                .all(|(&pi, kv)| vals[pi] == *kv)
        };
        let self_ref = fk.ref_table == child_table;
        let found = parent
            .rows
            .iter()
            .filter(|r| row_visible(r, snap, &[own]))
            .filter(|r| !(self_ref && Some(r.id) == ignore_id))
            .any(|r| matches_key(&r.values))
            || (self_ref && self_new.iter().any(|v| matches_key(v)));
        if !found {
            return Err(exec_err(
                "23503",
                format!(
                    "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                    child_table, fk.name
                ),
            ));
        }
    }
    Ok(())
}

/// Every (child table, FK) pair in the database whose referenced table is
/// `parent`, using the versions visible to (`snap`, `own`).
pub(crate) fn fks_referencing(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    parent: &str,
) -> Vec<(String, FkDef)> {
    let mut out = Vec::new();
    for (name, versions) in &eng.db.tables {
        if let Some(t) = versions
            .iter()
            .find(|t| crate::storage::table_visible(t, snap, &[own]))
        {
            for fk in &t.fks {
                if fk.ref_table == parent {
                    out.push((name.clone(), fk.clone()));
                }
            }
        }
    }
    // v0.22: the session's temp tables can also reference the parent.
    if let Some(tmps) = eng.db.temp_tables.get(&session) {
        for (name, t) in tmps {
            for fk in &t.fks {
                if fk.ref_table == parent {
                    out.push((name.clone(), fk.clone()));
                }
            }
        }
    }
    out
}

/// Cascaded writes collected while planning parent-side FK actions.
#[derive(Default)]
pub(crate) struct FkCascade {
    /// (table, row id, prev xmax) to delete.
    pub(crate) deletes: Vec<(String, u64, u64)>,
    /// (table, row id, prev xmax, new values) to update.
    pub(crate) updates: Vec<(String, u64, u64, Row)>,
}

impl FkCascade {
    pub(crate) fn contains(&self, table: &str, id: u64) -> bool {
        self.deletes.iter().any(|(t, i, _)| t == table && *i == id)
            || self
                .updates
                .iter()
                .any(|(t, i, _, _)| t == table && *i == id)
    }
}

/// Plan the child-side effects of deleting/updating parent rows.
///
/// `changed` holds (row id, old values, new values or None for delete) for
/// the parent table. RESTRICT violations fail with 23503; CASCADE / SET
/// NULL / SET DEFAULT collect into `out` (recursively, depth-limited).
pub(crate) fn plan_fk_cascade(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    level: IsolationLevel,
    parent_table: &str,
    parent_meta: &TableMeta,
    changed: &[(u64, Row, Option<Row>)],
    depth: u8,
    out: &mut FkCascade,
) -> Result<(), ExecError> {
    if depth > 16 {
        return Err(exec_err(
            "54000",
            "foreign key cascade depth exceeded".to_string(),
        ));
    }
    for (child_table, fk) in fks_referencing(eng, snap, own, session, parent_table) {
        let ref_cols = fk_ref_cols(parent_meta, &fk)?;
        let parent_pos: Vec<usize> = ref_cols
            .iter()
            .map(|c| {
                parent_meta
                    .column_index(c)
                    .expect("fk ref cols validated at DDL")
            })
            .collect();
        // Child metadata (owned; the borrow ends before recursion).
        let child_meta = {
            let t = eng
                .db
                .find_table(&child_table, snap, &[own], session)
                .expect("child table visible; engine lock held");
            TableMeta::of(t)
        };
        let child_pos: Vec<usize> = fk
            .cols
            .iter()
            .map(|c| {
                child_meta
                    .column_index(c)
                    .expect("fk cols validated at DDL")
            })
            .collect();
        for (pid, old_values, new_values) in changed {
            let _ = pid;
            let old_key: Vec<Value> = parent_pos.iter().map(|&i| old_values[i].clone()).collect();
            let new_key: Option<Vec<Value>> = new_values
                .as_ref()
                .map(|nv| parent_pos.iter().map(|&i| nv[i].clone()).collect());
            if let Some(nk) = &new_key {
                if nk == &old_key {
                    continue; // key unchanged: nothing to do
                }
            }
            let act = if new_values.is_some() {
                fk.on_update
            } else {
                fk.on_delete
            };
            // Visible child rows referencing the old key.
            let refs: Vec<(u64, u64, Row)> = {
                let t = eng
                    .db
                    .find_table(&child_table, snap, &[own], session)
                    .expect("child table visible; engine lock held");
                t.rows
                    .iter()
                    .filter(|r| row_visible(r, snap, &[own]))
                    .filter(|r| {
                        child_pos
                            .iter()
                            .zip(old_key.iter())
                            .all(|(&ci, kv)| r.values[ci] == *kv)
                    })
                    .map(|r| (r.id, r.xmax, r.values.clone()))
                    .collect()
            };
            for (cid, cxmax, cvalues) in refs {
                if out.contains(&child_table, cid) {
                    continue; // already handled by this cascade
                }
                match act {
                    FkAction::Restrict => {
                        return Err(exec_err(
                            "23503",
                            format!(
                                "{} on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                                if new_values.is_some() {
                                    "update"
                                } else {
                                    "delete"
                                },
                                parent_table,
                                fk.name,
                                child_table,
                            ),
                        ));
                    }
                    FkAction::Cascade => {
                        check_write_conflict(eng, cxmax, level)?;
                        check_row_lock(eng, &child_table, cid, own)?;
                        if let Some(nk) = &new_key {
                            // ON UPDATE CASCADE: move the child key.
                            let mut nv = cvalues.to_vec();
                            for (&ci, kv) in child_pos.iter().zip(nk.iter()) {
                                nv[ci] = kv.clone();
                            }
                            check_row_constraints(
                                eng,
                                snap,
                                own,
                                session,
                                role,
                                &child_meta,
                                &child_table,
                                &nv,
                            )?;
                            if let Some(vname) = eng.db.unique_violation(
                                &child_table,
                                &nv,
                                Some(cid),
                                snap,
                                &[own],
                                session,
                            ) {
                                return Err(exec_err(
                                    "23505",
                                    format!(
                                        "duplicate key value violates unique constraint \"{}\"",
                                        vname
                                    ),
                                ));
                            }
                            // The child's own FKs must still hold.
                            check_fk_child_row(
                                eng,
                                snap,
                                own,
                                session,
                                &child_meta,
                                &child_table,
                                &nv,
                                &[],
                                Some(cid),
                            )?;
                            let nv = Row::new(nv);
                            out.updates
                                .push((child_table.clone(), cid, cxmax, nv.clone()));
                            let child_meta2 = child_meta.clone();
                            plan_fk_cascade(
                                eng,
                                snap,
                                own,
                                session,
                                role,
                                level,
                                &child_table,
                                &child_meta2,
                                &[(cid, cvalues.clone(), Some(nv))],
                                depth + 1,
                                out,
                            )?;
                        } else {
                            // ON DELETE CASCADE.
                            out.deletes.push((child_table.clone(), cid, cxmax));
                            let child_meta2 = child_meta.clone();
                            plan_fk_cascade(
                                eng,
                                snap,
                                own,
                                session,
                                role,
                                level,
                                &child_table,
                                &child_meta2,
                                &[(cid, cvalues.clone(), None)],
                                depth + 1,
                                out,
                            )?;
                        }
                    }
                    FkAction::SetNull | FkAction::SetDefault => {
                        check_write_conflict(eng, cxmax, level)?;
                        check_row_lock(eng, &child_table, cid, own)?;
                        let mut nv = cvalues.to_vec();
                        for &ci in &child_pos {
                            nv[ci] = match act {
                                FkAction::SetNull => Value::Null,
                                _ => {
                                    let (cname, ctype) = &child_meta.columns[ci];
                                    match &child_meta.defaults[ci] {
                                        Some(d) => eval_default(
                                            eng, snap, own, session, role, d, ctype, cname,
                                        )?,
                                        None => Value::Null,
                                    }
                                }
                            };
                        }
                        check_row_constraints(
                            eng,
                            snap,
                            own,
                            session,
                            role,
                            &child_meta,
                            &child_table,
                            &nv,
                        )?;
                        if let Some(vname) = eng.db.unique_violation(
                            &child_table,
                            &nv,
                            Some(cid),
                            snap,
                            &[own],
                            session,
                        ) {
                            return Err(exec_err(
                                "23505",
                                format!(
                                    "duplicate key value violates unique constraint \"{}\"",
                                    vname
                                ),
                            ));
                        }
                        check_fk_child_row(
                            eng,
                            snap,
                            own,
                            session,
                            &child_meta,
                            &child_table,
                            &nv,
                            &[],
                            Some(cid),
                        )?;
                        let nv = Row::new(nv);
                        out.updates
                            .push((child_table.clone(), cid, cxmax, nv.clone()));
                        let child_meta2 = child_meta.clone();
                        plan_fk_cascade(
                            eng,
                            snap,
                            own,
                            session,
                            role,
                            level,
                            &child_table,
                            &child_meta2,
                            &[(cid, cvalues.clone(), Some(nv))],
                            depth + 1,
                            out,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Apply a planned FK cascade: deletes then updates, with index
/// maintenance and write logging, like exec_update/exec_delete do.
pub(crate) fn apply_fk_cascade(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    out: FkCascade,
) -> Result<(), ExecError> {
    // Deletes.
    for (table, id, prev_xmax) in &out.deletes {
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible; engine lock held throughout");
        let pos = t
            .row_pos(*id)
            .expect("row version still present; engine lock held throughout");
        t.rows[pos].xmax = ctx.write_xid;
        ctx.writes.push(WriteOp::DeleteRow {
            table: table.clone(),
            row_id: *id,
            prev_xmax: *prev_xmax,
        });
    }
    // Updates = delete old version + insert new version.
    let mut new_ids = Vec::with_capacity(out.updates.len());
    for _ in 0..out.updates.len() {
        new_ids.push(eng.alloc_row_id());
    }
    let mut indexed: Vec<(String, u64, Row)> = Vec::with_capacity(out.updates.len());
    for ((table, old_id, prev_xmax, new_values), new_id) in out.updates.iter().zip(new_ids) {
        let t = eng
            .db
            .find_table_mut(table, ctx.snap, &ctx.all_xids, ctx.session)
            .expect("table still visible; engine lock held throughout");
        let pos = t
            .row_pos(*old_id)
            .expect("row version still present; engine lock held throughout");
        // v0.13: capture the old values for the UpdateRow op (the
        // logical decoder needs them); values never change in place.
        let old_values = t.rows[pos].values.clone();
        t.rows[pos].xmax = ctx.write_xid;
        let new_values = new_values.clone();
        t.push_version(RowVersion::plain(new_id, new_values.clone(), ctx.write_xid));
        ctx.writes.push(WriteOp::UpdateRow {
            table: table.clone(),
            old_id: *old_id,
            new_id,
            prev_xmax: *prev_xmax,
            old_values,
        });
        indexed.push((table.clone(), new_id, new_values));
    }
    for (table, new_id, new_values) in &indexed {
        eng.db
            .index_insert_row(table, *new_id, new_values, ctx.session);
    }
    Ok(())
}
