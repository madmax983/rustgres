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
    /// v1.93: declared bit typmod per column, parallel to `columns`.
    pub(crate) bit_typmods: Vec<Option<(i32, bool)>>,
    /// v0.85: iff `domain_types[i]` is `Some` and the column was
    /// declared as `d[]`: the domain applies per array element.
    pub(crate) domain_elem: Vec<bool>,
    pub(crate) not_null: Vec<bool>,
    pub(crate) defaults: Vec<Option<DefaultExpr>>,
    pub(crate) checks: Vec<CheckDef>,
    pub(crate) pkey: Option<UniqueDef>,
    pub(crate) fks: Vec<FkDef>,
    /// v2.03: generated columns, parallel to `columns`.
    pub(crate) gen_cols: Vec<Option<(crate::sql::Expr, bool)>>,
}

impl TableMeta {
    pub(crate) fn of(t: &Table) -> Self {
        TableMeta {
            columns: t.columns.clone(),
            composite_types: t.composite_types.clone(),
            domain_types: t.domain_types.clone(),
            bit_typmods: t.bit_typmods.clone(),
            domain_elem: t.domain_elem.clone(),
            not_null: t.not_null.clone(),
            defaults: t.defaults.clone(),
            checks: t.checks.clone(),
            pkey: t.pkey.clone(),
            fks: t.fks.clone(),
            gen_cols: t.gen_cols.clone(),
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
                priv_role_override: None,
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
    // v1.92: a bit(n) / bit varying(n) domain typmod — ColType::Bit
    // is typmod-free in this engine, so the domain carries the
    // typmod and it is enforced here, before the CHECKs (PG coerces
    // to the domain type ahead of running its checks).
    if let (Some((n, varying)), Value::BitString(bs)) = (dom.bit_typmod, value) {
        super::json::sj_check_bit_len(bs.bitlen, n, varying)?;
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
        // v1.88: stamp the user range type's name onto values stored
        // in a column of that type (so pg_typeof / output naming
        // matches PG).
        (Value::Range(mut r), Some(tname)) => {
            if eng.db.types.get(tname).and_then(|st| st.range) == Some((r.elem, false)) {
                r.name = Some(tname.to_string());
            }
            Ok(Value::Range(r))
        }
        (Value::Multirange(mut m), Some(tname)) => {
            if eng.db.types.get(tname).and_then(|st| st.range) == Some((m.elem, true)) {
                m.name = Some(tname.to_string());
            }
            Ok(Value::Multirange(m))
        }
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

/// v2.03: materialize generated columns into `values` (PG19
/// ExecComputeStoredGenerated; virtual columns are materialized the
/// same way in this engine). Each generated expression evaluates
/// against the row's other columns; generated columns may reference
/// earlier generated columns, so evaluate in column order and write
/// back as we go.
#[allow(clippy::too_many_arguments)]

/// v2.03: PG19 generation-expression validity (heap.c cookDefault +
/// parse_agg/parse_expr EXPR_KIND_GENERATED_COLUMN), enforced at
/// CREATE TABLE: only immutable expressions over the table's own
/// non-generated columns. Without this, an expression the s-expr
/// codec cannot represent (e.g. an aggregate) would be stored and
/// then fail WAL replay on every restart.
pub(crate) fn validate_generated_columns(
    eng: &Engine,
    def: &crate::sql::TableDef,
) -> Result<(), ExecError> {
    for g in def.gen_cols.iter() {
        let Some((expr, _)) = g else { continue };
        gen_expr_walk(eng, def, expr)?;
    }
    Ok(())
}

const GEN_SYSTEM_COLS: [&str; 6] = ["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

fn gen_expr_walk(eng: &Engine, def: &crate::sql::TableDef, e: &Expr) -> Result<(), ExecError> {
    match e {
        Expr::Column { table: qual, name } => {
            let lname = name.to_lowercase();
            if GEN_SYSTEM_COLS.contains(&lname.as_str()) {
                return Err(exec_err(
                    "0A000",
                    format!(
                        "cannot use system column \"{}\" in column generation expression",
                        name
                    ),
                ));
            }
            if qual.is_some() {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" does not exist", name),
                ));
            }
            match def
                .columns
                .iter()
                .position(|(cn, _)| cn.eq_ignore_ascii_case(name))
            {
                None => Err(exec_err(
                    "42703",
                    format!("column \"{}\" does not exist", name),
                )),
                Some(p) if def.gen_cols.get(p).is_some_and(|g| g.is_some()) => Err(exec_err(
                    "42P10",
                    format!(
                        "cannot use generated column \"{}\" in column generation expression",
                        name
                    ),
                )),
                _ => Ok(()),
            }
        }
        Expr::WholeRow { .. } => Err(exec_err(
            "0A000",
            "cannot use whole-row variable in column generation expression".to_string(),
        )),
        Expr::Agg { .. } | Expr::WithinGroup { .. } => Err(exec_err(
            "42803",
            "aggregate functions are not allowed in column generation expressions".to_string(),
        )),
        Expr::Window { .. } => Err(exec_err(
            "42P20",
            "window functions are not allowed in column generation expressions".to_string(),
        )),
        Expr::ScalarSub(_) | Expr::InSub { .. } | Expr::Exists { .. } | Expr::Quantified { .. } => {
            Err(exec_err(
                "0A000",
                "cannot use subquery in column generation expression".to_string(),
            ))
        }
        Expr::Func { name, args, .. } => {
            let lname = name.to_lowercase();
            if lname == "generate_series"
                || lname == "generate_subscripts"
                || eng.db.functions.contains_key(&lname)
                || !super::publication::IMMUTABLE_FUNCS.contains(&lname.as_str())
            {
                return Err(exec_err(
                    "42P17",
                    "generation expression is not immutable".to_string(),
                ));
            }
            for a in args {
                gen_expr_walk(eng, def, a)?;
            }
            Ok(())
        }
        Expr::UserOp { .. } => Err(exec_err(
            "42P17",
            "generation expression is not immutable".to_string(),
        )),
        Expr::Arith { left, right, .. } | Expr::Cmp { left, right, .. } => {
            gen_expr_walk(eng, def, left)?;
            gen_expr_walk(eng, def, right)
        }
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            gen_expr_walk(eng, def, a)?;
            gen_expr_walk(eng, def, b)
        }
        Expr::JsonOp { left, right, .. } | Expr::IsDistinctFrom { left, right, .. } => {
            gen_expr_walk(eng, def, left)?;
            gen_expr_walk(eng, def, right)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            gen_expr_walk(eng, def, expr)?;
            gen_expr_walk(eng, def, pattern)?;
            if let Some(x) = escape {
                gen_expr_walk(eng, def, x)?;
            }
            Ok(())
        }
        Expr::Regex { expr, pattern, .. } => {
            gen_expr_walk(eng, def, expr)?;
            gen_expr_walk(eng, def, pattern)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            gen_expr_walk(eng, def, expr)?;
            gen_expr_walk(eng, def, low)?;
            gen_expr_walk(eng, def, high)
        }
        Expr::IsBool { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Not(expr)
        | Expr::BitNot(expr)
        | Expr::Neg(expr)
        | Expr::Cast { expr, .. }
        | Expr::CastNamed { expr, .. }
        | Expr::NamedArg { expr, .. }
        | Expr::FieldAccess { expr, .. }
        | Expr::Extract { from: expr, .. } => gen_expr_walk(eng, def, expr),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                gen_expr_walk(eng, def, o)?;
            }
            for (k, r) in whens {
                gen_expr_walk(eng, def, k)?;
                gen_expr_walk(eng, def, r)?;
            }
            if let Some(x) = else_ {
                gen_expr_walk(eng, def, x)?;
            }
            Ok(())
        }
        Expr::Row(elems) | Expr::ArrayCtor { elems, .. } => {
            for x in elems {
                gen_expr_walk(eng, def, x)?;
            }
            Ok(())
        }
        Expr::AtTimeZone { expr, zone } => {
            gen_expr_walk(eng, def, expr)?;
            if let Some(z) = zone {
                gen_expr_walk(eng, def, z)?;
            }
            Ok(())
        }
        Expr::Subscript { array, indices } => {
            gen_expr_walk(eng, def, array)?;
            for x in indices {
                gen_expr_walk(eng, def, x)?;
            }
            Ok(())
        }
        Expr::Slice { array, bounds } => {
            gen_expr_walk(eng, def, array)?;
            for (l, u) in bounds {
                if let Some(l) = l {
                    gen_expr_walk(eng, def, l)?;
                }
                if let Some(u) = u {
                    gen_expr_walk(eng, def, u)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub(crate) fn compute_generated_columns(
    eng: &mut Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    role: &str,
    meta: &TableMeta,
    values: &mut [Value],
) -> Result<(), ExecError> {
    if meta.gen_cols.iter().all(|g| g.is_none()) {
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
    let n = meta
        .gen_cols
        .len()
        .min(meta.columns.len())
        .min(values.len());
    for i in 0..n {
        let Some((expr, _stored)) = meta.gen_cols.get(i).cloned().flatten() else {
            continue;
        };
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng,
            snap,
            own,
            all_xids: vec![own],
            session,
            role,
            read_only: false,
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
        let frame = Scope {
            schema: &schema,
            row: values,
            prov: None,
        };
        let v = eval_expr(&mut q, &[frame], &expr)?;
        let (_, ctype) = &meta.columns[i];
        values[i] = coerce_value(v, ctype, &meta.columns[i].0)?;
    }
    Ok(())
}

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
    // v1.93: declared bit typmods (PG19 varbit.c assignment rules):
    // fixed bit requires the exact length, varying only bounds it
    // (22026). This is the shared row-write choke point — INSERT
    // VALUES / INSERT..SELECT / COPY (which reuses exec_insert) and
    // UPDATE all materialize their final rows through here. The check
    // runs on the finished BitString value, so text-literal input
    // (COPY hex, casts) is judged post-coercion, like PG.
    for (i, tm) in meta.bit_typmods.iter().enumerate() {
        if let (Some((n, varying)), Some(Value::BitString(bs))) = (tm, values.get(i)) {
            super::json::sj_check_bit_len(bs.bitlen, *n, *varying)?;
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
                priv_role_override: None,
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
        // v2.06: a NOT ENFORCED constraint is catalogued, not
        // evaluated (PG19 conenforced).
        if !check.enforced {
            continue;
        }
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
            priv_role_override: None,
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

/// v2.06: does a visible row carrying `key` in the FK's referenced
/// columns exist in the referenced table — OR IN ANY OF ITS PARTITION
/// DESCENDANTS? A partitioned parent's own row store is always empty
/// (rows live in the leaves), so the pre-v2.06 root-only scan
/// reported every key missing for partitioned parents; the deferred
/// flush shared the same blindness via its own copy. `ignore` skips
/// one row id on one named table (self-referencing UPDATE).
pub(crate) fn fk_parent_key_exists(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    fk: &FkDef,
    key: &[Value],
    ignore: Option<(&str, u64)>,
) -> Result<bool, ExecError> {
    // Collect the referenced table plus its partition tree. Borrowed,
    // not cloned: `eng` is only ever borrowed immutably here, so there
    // is no need to deep-copy every row of every table just to check
    // one key's presence (this runs once per child row checked).
    let mut tables: Vec<(String, &Table)> = Vec::new();
    let mut stack = vec![fk.ref_table.clone()];
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(t) = eng.db.find_table(&name, snap, &[own], session) else {
            continue;
        };
        if let Some(p) = &t.partition {
            for c in &p.children {
                stack.push(c.clone());
            }
        }
        tables.push((name, t));
    }
    for (name, t) in &tables {
        let meta = TableMeta::of(t);
        let ref_cols = fk_ref_cols(&meta, fk)?;
        let pos: Vec<usize> = ref_cols
            .iter()
            .map(|c| t.column_index(c).expect("fk ref cols validated at DDL"))
            .collect();
        let hit = t
            .rows
            .iter()
            .filter(|r| row_visible(r, snap, &[own]))
            .filter(|r| !matches!(ignore, Some((tn, id)) if tn == name && r.id == id))
            .any(|r| {
                pos.iter()
                    .zip(key.iter())
                    .all(|(&pi, kv)| fk_key_eq(&r.values[pi], kv))
            });
        if hit {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Child-side foreign-key check for one new row (INSERT / UPDATE /
/// cascaded SET NULL / SET DEFAULT). `self_new` holds the statement's own
/// new rows on the child table (self-references); `ignore_id` excludes the
/// row version being replaced (self-referencing UPDATE).
///
/// MATCH SIMPLE semantics: a NULL in any referencing column skips the
/// v1.96 finishing pass: FK key equality with PG semantics —
/// cross-type numeric representations compare by value (an
/// ALTER COLUMN TYPE int->bigint on either side must not unlink
/// the constraint), where raw `Value` equality would miss.
pub(crate) fn fk_key_eq(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return false;
    }
    compare_values(a, b, false, None)
        .map(|o| o == std::cmp::Ordering::Equal)
        .unwrap_or(false)
}

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
    check_fk_child_row_pv(
        eng,
        snap,
        own,
        session,
        child_meta,
        child_table,
        values,
        self_new,
        ignore_id,
        None,
    )
}

/// As `check_fk_child_row`, plus a prevalidated (referenced table,
/// key) pair: a key the caller guarantees exists in that table's
/// FINAL image. The ON UPDATE CASCADE planner uses this — the
/// cascaded child row's new key is carried by the parent row being
/// updated, whose new version is not visible in the table yet at
/// plan time (the pre-fix planner therefore reported every cascaded
/// key missing and failed plain ON UPDATE CASCADE 23503). Matching
/// by referenced table (not constraint name) also covers several
/// FKs from one child to the same parent acting on the same row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_fk_child_row_pv(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    child_meta: &TableMeta,
    child_table: &str,
    values: &[Value],
    self_new: &[Row],
    ignore_id: Option<u64>,
    prevalidated: Option<(&str, &[Value])>,
) -> Result<(), ExecError> {
    for fk in &child_meta.fks {
        // v2.06: NOT ENFORCED FKs are never checked.
        if !fk.enforced {
            continue;
        }
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
            // v2.06: MATCH SIMPLE/PARTIAL skip on any NULL; MATCH
            // FULL fails on a partially-NULL key (deferred FKs queue
            // the key and fail at fire time instead).
            if fk.match_full && !key.iter().all(|v| matches!(v, Value::Null)) {
                if fk_mode_deferred(eng, session, child_table, fk) {
                    queue_child_fk(eng, session, child_table, &fk.name, key);
                    continue;
                }
                return Err(exec_err(
                    "23503",
                    format!(
                        "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                        child_table, fk.name
                    ),
                ));
            }
            continue;
        }
        // v2.06: a currently-deferred FK queues its check for
        // COMMIT / SET CONSTRAINTS IMMEDIATE instead of running it.
        if fk_mode_deferred(eng, session, child_table, fk) {
            queue_child_fk(eng, session, child_table, &fk.name, key);
            continue;
        }
        // Prevalidated key (ON UPDATE CASCADE plan): the parent's
        // pending row carries exactly this key, so it exists in the
        // final image by construction.
        if let Some((href, hkey)) = prevalidated
            && href == fk.ref_table
            && hkey.len() == key.len()
            && hkey.iter().zip(key.iter()).all(|(a, b)| fk_key_eq(a, b))
        {
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
                .all(|(&pi, kv)| fk_key_eq(&vals[pi], kv))
        };
        let self_ref = fk.ref_table == child_table;
        // v2.06: the key may live in any partition leaf of the
        // referenced table, not only in its (always empty) root.
        let found = fk_parent_key_exists(
            eng,
            snap,
            own,
            session,
            fk,
            &key,
            if self_ref {
                ignore_id.map(|id| (child_table, id))
            } else {
                None
            },
        )? || (self_ref && self_new.iter().any(|v| matches_key(v)));
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
    pub(crate) fn is_planned_delete(&self, table: &str, id: u64) -> bool {
        self.deletes.iter().any(|(t, i, _)| t == table && *i == id)
    }

    /// The values a previously planned action will leave in this
    /// row, if any (v2.06 composition: several FKs between the same
    /// parent/child pair can act on one row — PG applies each RI
    /// action to the row's current image, so later actions must
    /// start from the planned image, not the table image).
    pub(crate) fn planned_values(&self, table: &str, id: u64) -> Option<Row> {
        self.updates
            .iter()
            .find(|(t, i, _, _)| t == table && *i == id)
            .map(|(_, _, _, v)| v.clone())
    }

    /// Insert or replace the planned update for a row.
    pub(crate) fn upsert_update(&mut self, table: &str, id: u64, xmax: u64, values: Row) {
        if let Some(e) = self
            .updates
            .iter_mut()
            .find(|(t, i, _, _)| t == table && *i == id)
        {
            e.3 = values;
        } else {
            self.updates.push((table.to_string(), id, xmax, values));
        }
    }

    /// A planned delete subsumes any planned update of the row.
    pub(crate) fn supersede_with_delete(&mut self, table: &str, id: u64, xmax: u64) {
        self.updates
            .retain(|(t, i, _, _)| !(t == table && *i == id));
        if !self.is_planned_delete(table, id) {
            self.deletes.push((table.to_string(), id, xmax));
        }
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
        // v2.06: NOT ENFORCED FKs never act. A currently-deferred FK
        // queues one parent entry per changed row; the action is
        // planned at fire time against the row's final image.
        if !fk.enforced {
            continue;
        }
        if fk_mode_deferred(eng, session, &child_table, &fk) {
            for (pid, old_values, _) in changed {
                queue_parent_fk(
                    eng,
                    session,
                    parent_table,
                    &child_table,
                    &fk.name,
                    *pid,
                    old_values.clone(),
                );
            }
            continue;
        }
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
                if nk.len() == old_key.len()
                    && nk.iter().zip(old_key.iter()).all(|(a, b)| fk_key_eq(a, b))
                {
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
                            .all(|(&ci, kv)| fk_key_eq(&r.values[ci], kv))
                    })
                    .map(|r| (r.id, r.xmax, r.values.clone()))
                    .collect()
            };
            for (cid, cxmax, cvalues) in refs {
                if out.is_planned_delete(&child_table, cid) {
                    continue; // a planned delete subsumes later actions
                }
                // v2.06 composition: an earlier FK action on this
                // row (several FKs can join the same parent/child
                // pair — e.g. tasks.owner/worker/checked_by) leaves
                // a planned image; this action applies to THAT
                // image, not the table image. The pre-fix blanket
                // `contains` skip applied only the first FK's action
                // and left the row referencing the dead key.
                let base: Vec<Value> = out
                    .planned_values(&child_table, cid)
                    .map(|r| r.to_vec())
                    .unwrap_or_else(|| cvalues.to_vec());
                let base_row = Row::new(base.clone());
                match act {
                    FkAction::Restrict => {
                        // If a planned action already moved this
                        // row off the old key, it no longer restricts.
                        let still_refs = child_pos
                            .iter()
                            .zip(old_key.iter())
                            .all(|(&ci, kv)| fk_key_eq(&base[ci], kv));
                        if !still_refs {
                            continue;
                        }
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
                            let mut nv = base.to_vec();
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
                            // This FK is prevalidated: the parent's
                            // pending row carries the new key.
                            check_fk_child_row_pv(
                                eng,
                                snap,
                                own,
                                session,
                                &child_meta,
                                &child_table,
                                &nv,
                                &[],
                                Some(cid),
                                Some((parent_table, nk.as_slice())),
                            )?;
                            let nv = Row::new(nv);
                            out.upsert_update(&child_table, cid, cxmax, nv.clone());
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
                                &[(cid, base_row.clone(), Some(nv))],
                                depth + 1,
                                out,
                            )?;
                        } else {
                            // ON DELETE CASCADE.
                            out.supersede_with_delete(&child_table, cid, cxmax);
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
                                &[(cid, base_row.clone(), None)],
                                depth + 1,
                                out,
                            )?;
                        }
                    }
                    FkAction::SetNull | FkAction::SetDefault => {
                        check_write_conflict(eng, cxmax, level)?;
                        check_row_lock(eng, &child_table, cid, own)?;
                        let mut nv = base.to_vec();
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
                        out.upsert_update(&child_table, cid, cxmax, nv.clone());
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
                            &[(cid, base_row.clone(), Some(nv))],
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
