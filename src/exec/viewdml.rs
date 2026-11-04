// v2.18: auto-updatable views (PG19 rewrite/rewriteHandler.c
// rewriteTargetView). A view with no INSTEAD OF trigger for the event
// is rewritten onto its base relation when the view query is "simply
// updatable": a single base relation, no DISTINCT / GROUP BY / HAVING
// / set operations / WITH / LIMIT / OFFSET, and no aggregates, window
// functions or set-returning functions in the target list. View
// columns whose target-list entry is a simple Var of the base are
// updatable; expression / system / whole-row entries are not
// (SQL:1999 T111 mix, per view_col_is_auto_updatable).
//
// The rewrite is a syntactic translation: target columns, SET
// assignments, WHERE (ANDed with the view qual), RETURNING and the
// ON CONFLICT arbiter are renamed from view columns to base columns
// (expression columns are substituted with their target-list
// expression where a value is read), and the ordinary table DML
// executor runs on the base relation — so defaults, identity /
// generated columns, constraints, triggers, FK actions and
// partition routing all come from the one machinery. A base that is
// itself a view recurses through the same path.
//
// WITH CHECK OPTION is enforced on the FINAL base rows (PG19
// ExecWithCheckOptions runs after BEFORE triggers): when any check
// in the view chain applies, the base DML is executed with an
// internal RETURNING capture and the applicable quals are evaluated
// over the captured rows; a violation is 44000 and the statement's
// writes are undone by the normal statement-atomicity path.
use super::*;

use crate::storage::CheckOption;

/// One view output column's mapping onto the base relation.
#[derive(Clone, Debug)]
pub(crate) enum VCol {
    /// Simple Var of the base relation: updatable, maps to this
    /// base column name.
    Base(String),
    /// Any other expression: readable by substitution, not
    /// updatable.
    Subst(Expr),
    /// Not updatable and (when `Some`) readable as this base column
    /// (system columns); the string is PG's per-column reason.
    Blocked(Option<String>, &'static str),
}

impl VCol {
    pub(crate) fn reason(&self) -> Option<&'static str> {
        match self {
            VCol::Blocked(_, r) => Some(r),
            VCol::Subst(_) => Some(R_NOTCOL),
            VCol::Base(_) => None,
        }
    }
}

/// The analysis of one auto-updatable view.
#[derive(Clone, Debug)]
pub(crate) struct AutoView {
    /// Base relation name exactly as written in the view's FROM
    /// (the string the executor resolves and uses as the DML target).
    pub base: String,
    /// Alias of the base relation in the view query, if any.
    pub base_alias: Option<String>,
    /// The view's WHERE, over base-relation columns.
    pub qual: Option<Expr>,
    /// Per view output column, positionally aligned with view_cols.
    pub cols: Vec<VCol>,
    /// View output columns (name, type), CREATE VIEW aliases applied.
    pub view_cols: Vec<(String, ColType)>,
    /// The view's check-option flavor.
    pub check_option: CheckOption,
}

pub(crate) enum ViewClass {
    Auto(AutoView),
    /// PG's reason string (rewriteHandler.c gettext_noop texts).
    NotAuto(String),
}

const R_DISTINCT: &str = "Views containing DISTINCT are not automatically updatable.";
const R_GROUP: &str = "Views containing GROUP BY are not automatically updatable.";
const R_HAVING: &str = "Views containing HAVING are not automatically updatable.";
const R_SETOP: &str =
    "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.";
const R_WITH: &str = "Views containing WITH are not automatically updatable.";
const R_LIMIT: &str = "Views containing LIMIT or OFFSET are not automatically updatable.";
const R_AGG: &str = "Views that return aggregate functions are not automatically updatable.";
const R_WINDOW: &str = "Views that return window functions are not automatically updatable.";
const R_SRF: &str = "Views that return set-returning functions are not automatically updatable.";
const R_SINGLE: &str =
    "Views that do not select from a single table or view are not automatically updatable.";
const R_NOCOLS: &str = "Views that have no updatable columns are not automatically updatable.";
const R_SYSTEM: &str = "View columns that refer to system columns are not updatable.";
const R_WHOLE: &str = "View columns that return whole-row references are not updatable.";
const R_NOTCOL: &str =
    "View columns that are not columns of their base relation are not updatable.";

fn is_system_col(name: &str) -> bool {
    matches!(
        name,
        "ctid" | "xmin" | "xmax" | "cmin" | "cmax" | "tableoid"
    )
}

/// Built-in set-returning functions (a user function with
/// returns_set is detected from the catalog instead).
fn builtin_srf(name: &str) -> bool {
    matches!(
        name,
        "generate_series"
            | "generate_subscripts"
            | "unnest"
            | "regexp_split_to_table"
            | "string_to_table"
            | "json_each"
            | "jsonb_each"
            | "json_object_keys"
            | "jsonb_object_keys"
            | "json_array_elements"
            | "jsonb_array_elements"
            | "json_array_elements_text"
            | "jsonb_array_elements_text"
            | "json_populate_recordset"
            | "jsonb_populate_recordset"
            | "json_to_recordset"
            | "jsonb_to_recordset"
    )
}

fn expr_has_srf(eng: &Engine, e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |x| {
        if let Expr::Func { name, .. } = x {
            let n = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
            if builtin_srf(&n) {
                found = true;
            } else if let Some(defs) = eng.db.functions.get(&n) {
                if defs.iter().any(|d| d.returns_set) {
                    found = true;
                }
            }
        }
    });
    found
}

fn expr_has_window(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |x| {
        if matches!(x, Expr::Window { .. }) {
            found = true;
        }
    });
    found
}

/// view_columns_of without a StmtCtx (the information_schema
/// scans run from catalog contexts that only carry the pieces).
pub(crate) fn view_columns_of_db(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    view: &str,
) -> Result<Vec<(String, ColType)>, ExecError> {
    let v = eng
        .db
        .find_view(view, snap, own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", view)))?
        .clone();
    let stmt = parse_statement(&v.query).map_err(sql_err)?;
    let sel = match stmt {
        Stmt::Select(s) => s,
        _ => return Err(exec_err("XX000", "view query is not a SELECT".to_string())),
    };
    let mut cols = describe_select(eng, snap, own, session, &sel, &[], &[])?;
    for (i, a) in v.col_aliases.iter().enumerate() {
        if i < cols.len() {
            cols[i].0 = a.clone();
        }
    }
    Ok(cols)
}

/// Analyze a view for auto-updatability (PG19
/// view_query_is_auto_updatable + view_cols_are_auto_updatable).
/// The view must exist (the caller has already established that).
pub(crate) fn classify_view(
    eng: &Engine,
    ctx: &StmtCtx,
    view: &str,
) -> Result<ViewClass, ExecError> {
    classify_view_x(eng, ctx.snap, ctx.own, ctx.session, &ctx.all_xids, view)
}

pub(crate) fn classify_view_x(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    all_xids: &[u64],
    view: &str,
) -> Result<ViewClass, ExecError> {
    classify_view_xd(eng, snap, own, session, all_xids, view, 0)
}

fn classify_view_xd(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    all_xids: &[u64],
    view: &str,
    depth: u32,
) -> Result<ViewClass, ExecError> {
    if depth > 32 {
        return Ok(ViewClass::NotAuto(R_SINGLE.to_string()));
    }
    let v = eng
        .db
        .find_view(view, snap, own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", view)))?
        .clone();
    let stmt = parse_statement(&v.query).map_err(sql_err)?;
    let sel = match stmt {
        Stmt::Select(s) => s,
        _ => return Ok(ViewClass::NotAuto(R_SINGLE.to_string())),
    };
    if sel.set_op.is_some() {
        return Ok(ViewClass::NotAuto(R_SETOP.to_string()));
    }
    if !sel.with.is_empty() {
        return Ok(ViewClass::NotAuto(R_WITH.to_string()));
    }
    if sel.distinct || !sel.distinct_on.is_empty() {
        return Ok(ViewClass::NotAuto(R_DISTINCT.to_string()));
    }
    if !sel.group_by.is_empty() {
        return Ok(ViewClass::NotAuto(R_GROUP.to_string()));
    }
    if sel.having.is_some() {
        return Ok(ViewClass::NotAuto(R_HAVING.to_string()));
    }
    if sel.limit.is_some() || sel.offset.is_some() {
        return Ok(ViewClass::NotAuto(R_LIMIT.to_string()));
    }
    for item in &sel.items {
        if let SelectItem::Expr { expr, .. } = item {
            if contains_agg(expr) {
                return Ok(ViewClass::NotAuto(R_AGG.to_string()));
            }
            if expr_has_window(expr) {
                return Ok(ViewClass::NotAuto(R_WINDOW.to_string()));
            }
            if expr_has_srf(eng, expr) {
                return Ok(ViewClass::NotAuto(R_SRF.to_string()));
            }
        }
    }
    if sel.from.len() != 1 {
        return Ok(ViewClass::NotAuto(R_SINGLE.to_string()));
    }
    let (base, base_alias, base_col_aliases) = match &sel.from[0] {
        FromItem::Table {
            name,
            alias,
            col_aliases,
            ..
        } => (name.clone(), alias.clone(), col_aliases.clone()),
        _ => return Ok(ViewClass::NotAuto(R_SINGLE.to_string())),
    };
    // The base relation's columns: `visible` = names as the view
    // query sees them (FROM column aliases applied), `real` = names
    // DML on the base must use, positionally aligned.
    let (visible_cols, real_cols): (Vec<String>, Vec<String>) =
        if let Some(t) = eng.db.find_table(&base, snap, all_xids, session) {
            let real: Vec<String> = t.columns.iter().map(|(n, _)| n.clone()).collect();
            let mut visible = real.clone();
            for (i, a) in base_col_aliases.iter().enumerate() {
                if i < visible.len() {
                    visible[i] = a.clone();
                }
            }
            (visible, real)
        } else if eng.db.find_view(&base, snap, own).is_some() {
            let cols = view_columns_of_db(eng, snap, own, session, &base)?;
            let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
            (names.clone(), names)
        } else {
            // A sequence or a missing relation: a valid view shape
            // (sequences) but not a DML target.
            return Ok(ViewClass::NotAuto(R_SINGLE.to_string()));
        };
    let view_cols = view_columns_of_db(eng, snap, own, session, view)?;
    // v2.18: when the base is itself a view, outer columns inherit
    // the INNER view's per-column classification (a Var of an inner
    // expression column is itself non-updatable). If the inner view
    // is not auto-updatable at all and has no INSTEAD OF triggers to
    // route through, the outer view is not updatable either (PG
    // checks the expanded query tree).
    let base_is_view = eng.db.find_table(&base, snap, all_xids, session).is_none()
        && eng.db.find_view(&base, snap, own).is_some();
    let inner_kinds: Option<Vec<VCol>> = if base_is_view {
        match classify_view_xd(eng, snap, own, session, all_xids, &base, depth + 1)? {
            ViewClass::Auto(inner_av) => Some(inner_av.cols),
            ViewClass::NotAuto(r) => {
                let inner_v = eng.db.find_view(&base, snap, own).unwrap().clone();
                if inner_v.triggers.is_empty() {
                    return Ok(ViewClass::NotAuto(r));
                }
                None
            }
        }
    } else {
        None
    };
    // The KIND comes from the inner classification; the NAME is
    // always this view's base-relation column at `pos` (VCol::Base
    // names a column of av.base, i.e. the inner view here — the
    // inner execution maps it further down).
    let map_inner = |pos: usize, real: &str, subst_expr: Expr| -> VCol {
        match &inner_kinds {
            Some(kinds) => match kinds.get(pos) {
                Some(VCol::Base(_)) => VCol::Base(real.to_string()),
                Some(VCol::Subst(_)) => VCol::Subst(subst_expr),
                Some(VCol::Blocked(obn, r)) => {
                    VCol::Blocked(obn.as_ref().map(|_| real.to_string()), r)
                }
                None => VCol::Base(real.to_string()),
            },
            None => VCol::Base(real.to_string()),
        }
    };
    let mut cols: Vec<VCol> = Vec::new();
    for item in &sel.items {
        match item {
            SelectItem::All => {
                for (pos, real) in real_cols.iter().enumerate() {
                    let e = Expr::Column {
                        table: None,
                        name: real.clone(),
                    };
                    cols.push(map_inner(pos, real, e));
                }
            }
            SelectItem::AllOf(q) => {
                let bq = base.rsplit('.').next().unwrap_or(&base);
                if q == &base || Some(q.as_str()) == base_alias.as_deref() || q == bq {
                    for (pos, real) in real_cols.iter().enumerate() {
                        let e = Expr::Column {
                            table: None,
                            name: real.clone(),
                        };
                        cols.push(map_inner(pos, real, e));
                    }
                } else {
                    return Ok(ViewClass::NotAuto(R_SINGLE.to_string()));
                }
            }
            SelectItem::Expr { expr, .. } => match expr {
                Expr::Column { table, name } => {
                    let qual_ok = match table {
                        None => true,
                        Some(q) => {
                            q == &base
                                || Some(q.as_str()) == base_alias.as_deref()
                                || q == base.rsplit('.').next().unwrap_or(&base)
                        }
                    };
                    if !qual_ok {
                        cols.push(VCol::Blocked(None, R_NOTCOL));
                    } else if is_system_col(name) {
                        cols.push(VCol::Blocked(Some(name.clone()), R_SYSTEM));
                    } else if let Some(pos) = visible_cols.iter().position(|c| c == name) {
                        let e = Expr::Column {
                            table: table.clone(),
                            name: name.clone(),
                        };
                        cols.push(map_inner(pos, &real_cols[pos], e));
                    } else {
                        cols.push(VCol::Blocked(None, R_NOTCOL));
                    }
                }
                Expr::WholeRow { .. } => cols.push(VCol::Blocked(None, R_WHOLE)),
                other => cols.push(VCol::Subst(other.clone())),
            },
        }
    }
    if cols.len() != view_cols.len() {
        // describe_select and the item walk disagree (exotic query
        // shapes): refuse rather than guess a mapping.
        return Ok(ViewClass::NotAuto(R_SINGLE.to_string()));
    }
    Ok(ViewClass::Auto(AutoView {
        base,
        base_alias,
        qual: sel.where_.clone(),
        cols,
        view_cols,
        check_option: v.check_option,
    }))
}

/// PG19 error_view_not_updatable: 55000 with the reason as detail.
pub(crate) fn err_view_not_updatable(view: &str, event: &str, reason: &str) -> ExecError {
    let base = view.rsplit('.').next().unwrap_or(view);
    let msg = match event {
        "INSERT" => format!("cannot insert into view \"{}\"", base),
        "UPDATE" => format!("cannot update view \"{}\"", base),
        "DELETE" => format!("cannot delete from view \"{}\"", base),
        _ => format!("cannot merge into view \"{}\"", base),
    };
    // The runner compares SQLSTATE + message; PG's detail travels in
    // the detail field, which this engine folds into the message of
    // some errors — keep the primary message exact and append the
    // reason after a newline-free separator only via detail slot:
    let mut e = exec_err("55000", msg);
    e.detail = Some(reason.to_string());
    e
}

/// PG19 rewriteTargetView column error: 0A000 "cannot <verb> into
/// column ... of view ..." — MERGE uses "merge into column".
pub(crate) fn err_view_col_not_updatable(
    view: &str,
    event: &str,
    col: &str,
    reason: &str,
) -> ExecError {
    let base = view.rsplit('.').next().unwrap_or(view);
    let msg = match event {
        "INSERT" => format!("cannot insert into column \"{}\" of view \"{}\"", col, base),
        "UPDATE" => format!("cannot update column \"{}\" of view \"{}\"", col, base),
        _ => format!("cannot merge into column \"{}\" of view \"{}\"", col, base),
    };
    let mut e = exec_err("0A000", msg);
    e.detail = Some(reason.to_string());
    e
}

/// Does this view have at least one updatable column?
pub(crate) fn av_has_updatable(av: &AutoView) -> bool {
    av.cols.iter().any(|c| matches!(c, VCol::Base(_)))
}

/// PG19 relation_is_updatable (include_triggers = false), as used by
/// information_schema: returns (insert, update, delete) event support.
/// `cols`, when given, restricts the columns considered (the
/// include_cols bitmapset): INSERT/UPDATE then require an updatable
/// column within the set, mapped through each view hop. Recursion is
/// depth-capped (PG uses a reloid stack; cycles cannot be created
/// through CREATE VIEW's dependency checks in practice).
pub(crate) fn view_updatable_events(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    view: &str,
) -> (bool, bool, bool) {
    view_events_inner(eng, snap, own, session, view, None, 0)
}

fn view_events_inner(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    view: &str,
    cols: Option<&[String]>,
    depth: u32,
) -> (bool, bool, bool) {
    if depth > 16 {
        return (false, false, false);
    }
    let av = match classify_view_x(eng, snap, own, session, &[own], view) {
        Ok(ViewClass::Auto(av)) => av,
        _ => return (false, false, false),
    };
    // Locally updatable columns, restricted to `cols` when given.
    let local: Vec<String> = av
        .view_cols
        .iter()
        .enumerate()
        .filter(|(i, _)| matches!(av.cols[*i], VCol::Base(_)))
        .map(|(_, (n, _))| n.clone())
        .filter(|n| cols.map(|cs| cs.iter().any(|c| c == n)).unwrap_or(true))
        .collect();
    if eng.db.find_table(&av.base, snap, &[own], session).is_some() {
        return (!local.is_empty(), !local.is_empty(), true);
    }
    if eng.db.find_view(&av.base, snap, own).is_some() {
        // Map the surviving local columns to inner view column names.
        let mapped: Vec<String> = av
            .view_cols
            .iter()
            .enumerate()
            .filter(|(_, (n, _))| local.iter().any(|l| l == n))
            .filter_map(|(i, _)| match &av.cols[i] {
                VCol::Base(b) => Some(b.clone()),
                _ => None,
            })
            .collect();
        return view_events_inner(eng, snap, own, session, &av.base, Some(&mapped), depth + 1);
    }
    (false, false, false)
}

/// PG19 pg_column_is_updatable (include_triggers = false): a view
/// column is updatable iff it is a simple Var of the base at every
/// hop down to a table.
pub(crate) fn view_col_updatable(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    view: &str,
    col: &str,
) -> bool {
    fn inner(
        eng: &Engine,
        snap: &Snapshot,
        own: u64,
        session: u64,
        view: &str,
        col: &str,
        depth: u32,
    ) -> bool {
        if depth > 16 {
            return false;
        }
        let av = match classify_view_x(eng, snap, own, session, &[own], view) {
            Ok(ViewClass::Auto(av)) => av,
            _ => return false,
        };
        let Some(pos) = av.view_cols.iter().position(|(n, _)| n == col) else {
            return false;
        };
        match &av.cols[pos] {
            VCol::Base(b) => {
                if eng.db.find_table(&av.base, snap, &[own], session).is_some() {
                    true
                } else if eng.db.find_view(&av.base, snap, own).is_some() {
                    inner(eng, snap, own, session, &av.base, b, depth + 1)
                } else {
                    false
                }
            }
            _ => false,
        }
    }
    inner(eng, snap, own, session, view, col, 0)
}

// ===========================================================================
// Expression renaming: view space -> base space.
// ===========================================================================

/// Rebuild `e` with `f` applied to every direct child expression.
/// Subquery bodies (ScalarSub / InSub.sub / Exists / Quantified.sub /
/// ArraySubquery / JsonArraySubq) are a different scope and are NOT
/// descended into: correlated references to the view inside a
/// subquery are a documented hold-back.
fn map_children(
    e: &Expr,
    f: &mut impl FnMut(&Expr) -> Result<Expr, ExecError>,
) -> Result<Expr, ExecError> {
    fn map_terms(
        terms: &[OrderTerm],
        f: &mut impl FnMut(&Expr) -> Result<Expr, ExecError>,
    ) -> Result<Vec<OrderTerm>, ExecError> {
        terms
            .iter()
            .map(|t| {
                Ok(OrderTerm {
                    expr: f(&t.expr)?,
                    desc: t.desc,
                    nulls_first: t.nulls_first,
                })
            })
            .collect()
    }
    fn opt(
        o: &Option<Box<Expr>>,
        f: &mut impl FnMut(&Expr) -> Result<Expr, ExecError>,
    ) -> Result<Option<Box<Expr>>, ExecError> {
        Ok(match o {
            Some(x) => Some(Box::new(f(x)?)),
            None => None,
        })
    }
    Ok(match e {
        Expr::Column { .. }
        | Expr::ResolvedCol { .. }
        | Expr::Literal(_)
        | Expr::Param(_)
        | Expr::WholeRow { .. }
        | Expr::ScalarSub(_)
        | Expr::ArraySubquery(_)
        | Expr::JsonArraySubq { .. }
        | Expr::Exists { .. } => e.clone(),
        Expr::Arith { op, left, right } => Expr::Arith {
            op: *op,
            left: Box::new(f(left)?),
            right: Box::new(f(right)?),
        },
        Expr::Cast { expr, to, written } => Expr::Cast {
            expr: Box::new(f(expr)?),
            to: *to,
            written: written.clone(),
        },
        Expr::CastNamed { expr, name } => Expr::CastNamed {
            expr: Box::new(f(expr)?),
            name: name.clone(),
        },
        Expr::AtTimeZone { expr, zone } => Expr::AtTimeZone {
            expr: Box::new(f(expr)?),
            zone: opt(zone, f)?,
        },
        Expr::Row(elems) => Expr::Row(elems.iter().map(|x| f(x)).collect::<Result<Vec<_>, _>>()?),
        Expr::FieldAccess { expr, field } => Expr::FieldAccess {
            expr: Box::new(f(expr)?),
            field: field.clone(),
        },
        Expr::Concat(l, r) => Expr::Concat(Box::new(f(l)?), Box::new(f(r)?)),
        Expr::JsonOp { op, left, right } => Expr::JsonOp {
            op: *op,
            left: Box::new(f(left)?),
            right: Box::new(f(right)?),
        },
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => Expr::Like {
            expr: Box::new(f(expr)?),
            pattern: Box::new(f(pattern)?),
            not: *not,
            ilike: *ilike,
            escape: opt(escape, f)?,
        },
        Expr::Regex {
            expr,
            pattern,
            not,
            case_insensitive,
        } => Expr::Regex {
            expr: Box::new(f(expr)?),
            pattern: Box::new(f(pattern)?),
            not: *not,
            case_insensitive: *case_insensitive,
        },
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => Expr::Between {
            expr: Box::new(f(expr)?),
            low: Box::new(f(low)?),
            high: Box::new(f(high)?),
            neg: *neg,
        },
        Expr::IsBool { expr, neg, val } => Expr::IsBool {
            expr: Box::new(f(expr)?),
            neg: *neg,
            val: *val,
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args.iter().map(|x| f(x)).collect::<Result<Vec<_>, _>>()?,
        },
        Expr::NamedArg { name, expr } => Expr::NamedArg {
            name: name.clone(),
            expr: Box::new(f(expr)?),
        },
        Expr::Extract { field, from } => Expr::Extract {
            field: field.clone(),
            from: Box::new(f(from)?),
        },
        Expr::Cmp { op, left, right } => Expr::Cmp {
            op: *op,
            left: Box::new(f(left)?),
            right: Box::new(f(right)?),
        },
        Expr::And(l, r) => Expr::And(Box::new(f(l)?), Box::new(f(r)?)),
        Expr::Or(l, r) => Expr::Or(Box::new(f(l)?), Box::new(f(r)?)),
        Expr::Not(x) => Expr::Not(Box::new(f(x)?)),
        Expr::BitNot(x) => Expr::BitNot(Box::new(f(x)?)),
        Expr::Neg(x) => Expr::Neg(Box::new(f(x)?)),
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: opt(operand, f)?,
            whens: whens
                .iter()
                .map(|(a, b)| Ok((Box::new(f(a)?), Box::new(f(b)?))))
                .collect::<Result<Vec<_>, ExecError>>()?,
            else_: opt(else_, f)?,
        },
        Expr::IsNull { expr, neg } => Expr::IsNull {
            expr: Box::new(f(expr)?),
            neg: *neg,
        },
        Expr::IsDistinctFrom { left, right, neg } => Expr::IsDistinctFrom {
            left: Box::new(f(left)?),
            right: Box::new(f(right)?),
            neg: *neg,
        },
        Expr::Agg {
            func,
            arg,
            distinct,
            arg2,
            agg_order_by,
            filter,
        } => Expr::Agg {
            func: *func,
            arg: opt(arg, f)?,
            distinct: *distinct,
            arg2: opt(arg2, f)?,
            agg_order_by: map_terms(agg_order_by, f)?,
            filter: opt(filter, f)?,
        },
        Expr::WithinGroup {
            func,
            direct_args,
            within_order_by,
            filter,
        } => Expr::WithinGroup {
            func: *func,
            direct_args: direct_args
                .iter()
                .map(|x| f(x))
                .collect::<Result<Vec<_>, _>>()?,
            within_order_by: map_terms(within_order_by, f)?,
            filter: opt(filter, f)?,
        },
        Expr::ArrayCtor { elems, nested } => Expr::ArrayCtor {
            elems: elems.iter().map(|x| f(x)).collect::<Result<Vec<_>, _>>()?,
            nested: *nested,
        },
        Expr::Subscript { array, indices } => Expr::Subscript {
            array: Box::new(f(array)?),
            indices: indices
                .iter()
                .map(|x| f(x))
                .collect::<Result<Vec<_>, _>>()?,
        },
        Expr::Slice { array, bounds } => Expr::Slice {
            array: Box::new(f(array)?),
            bounds: bounds
                .iter()
                .map(|(lo, hi)| Ok((opt(lo, f)?, opt(hi, f)?)))
                .collect::<Result<Vec<_>, ExecError>>()?,
        },
        Expr::InSub { expr, sub, neg } => Expr::InSub {
            expr: Box::new(f(expr)?),
            sub: sub.clone(),
            neg: *neg,
        },
        Expr::Quantified {
            left,
            op,
            quant,
            sub,
        } => Expr::Quantified {
            left: Box::new(f(left)?),
            op: op.clone(),
            quant: *quant,
            sub: sub.clone(),
        },
        Expr::UserOp { op, left, right } => Expr::UserOp {
            op: op.clone(),
            left: Box::new(f(left)?),
            right: Box::new(f(right)?),
        },
        Expr::Window {
            func,
            args,
            distinct,
            partition_by,
            order_by,
            frame,
            wid,
            filter,
            exclusion,
        } => Expr::Window {
            func: func.clone(),
            args: args.iter().map(|x| f(x)).collect::<Result<Vec<_>, _>>()?,
            distinct: *distinct,
            partition_by: partition_by
                .iter()
                .map(|x| f(x))
                .collect::<Result<Vec<_>, _>>()?,
            order_by: map_terms(order_by, f)?,
            frame: frame.clone(),
            wid: *wid,
            filter: opt(filter, f)?,
            exclusion: exclusion.clone(),
        },
    })
}

/// Rename an expression written in BASE space (a Subst target-list
/// entry, or the view qual): only qualifier normalization — base
/// alias / base name become `target_qual`; with `oldnew` set (inside
/// RETURNING old./new. expansion) every base reference is forced
/// into that qualifier.
fn rename_base_expr(e: &Expr, av: &AutoView, target_qual: &str, oldnew: Option<&str>) -> Expr {
    match e {
        Expr::Column { table, name } => {
            let new_table = if let Some(q) = oldnew {
                Some(q.to_string())
            } else {
                match table {
                    None => None,
                    Some(q)
                        if q == &av.base
                            || Some(q.as_str()) == av.base_alias.as_deref()
                            || q == av.base.rsplit('.').next().unwrap_or(&av.base) =>
                    {
                        Some(target_qual.to_string())
                    }
                    Some(q) => Some(q.clone()),
                }
            };
            Expr::Column {
                table: new_table,
                name: name.clone(),
            }
        }
        Expr::WholeRow { qual } => {
            if let Some(q) = oldnew {
                Expr::WholeRow {
                    qual: q.to_string(),
                }
            } else if qual == &av.base
                || Some(qual.as_str()) == av.base_alias.as_deref()
                || qual == av.base.rsplit('.').next().unwrap_or(&av.base)
            {
                Expr::WholeRow {
                    qual: target_qual.to_string(),
                }
            } else {
                e.clone()
            }
        }
        _ => map_children(e, &mut |c| Ok(rename_base_expr(c, av, target_qual, oldnew)))
            .unwrap_or_else(|_| e.clone()),
    }
}

/// Context for renaming user expressions (WHERE / SET / RETURNING /
/// ON CONFLICT) from view space to base space.
pub(crate) struct Ren<'a> {
    pub view: &'a str,
    pub alias: Option<&'a str>,
    pub av: &'a AutoView,
    /// Qualifier the base executor will use for the target scope.
    pub target_qual: String,
    /// Rename unqualified column references that match view columns
    /// (true for UPDATE/DELETE/INSERT paths; false for MERGE, where
    /// unqualified references may belong to the source).
    pub unqualified: bool,
}

impl Ren<'_> {
    fn view_col(&self, name: &str) -> Option<(usize, &VCol)> {
        self.av
            .view_cols
            .iter()
            .position(|(n, _)| n == name)
            .map(|i| (i, &self.av.cols[i]))
    }
    fn is_view_qual(&self, q: &str) -> bool {
        q == self.view
            || Some(q) == self.alias
            || q == self.view.rsplit('.').next().unwrap_or(self.view)
    }
    /// A view-space column reference -> base space.
    fn col(&self, table: &Option<String>, name: &str) -> Result<Expr, ExecError> {
        if let Some(q) = table {
            if q == "old" || q == "new" {
                // RETURNING old.col / new.col.
                return match self.view_col(name) {
                    Some((_, VCol::Base(b))) => Ok(Expr::Column {
                        table: Some(q.clone()),
                        name: b.clone(),
                    }),
                    Some((_, VCol::Subst(se))) => Ok(rename_base_expr(
                        se,
                        self.av,
                        &self.target_qual,
                        Some(q.as_str()),
                    )),
                    Some((_, VCol::Blocked(Some(bn), _))) => Ok(Expr::Column {
                        table: Some(q.clone()),
                        name: bn.clone(),
                    }),
                    _ => Err(exec_err(
                        "42703",
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            name, self.view
                        ),
                    )),
                };
            }
            if q == "excluded" {
                // ON CONFLICT's proposed-row reference. An expression
                // column reads as its defining expression evaluated
                // over the proposed row's base columns (PG semantics:
                // excluded.<viewcol> for a non-simple column is the
                // expression over EXCLUDED's underlying values).
                return match self.view_col(name) {
                    Some((_, VCol::Base(b))) => Ok(Expr::Column {
                        table: Some("excluded".to_string()),
                        name: b.clone(),
                    }),
                    Some((_, VCol::Subst(se))) => {
                        Ok(rename_base_expr(se, self.av, "excluded", None))
                    }
                    Some((_, VCol::Blocked(Some(bn), _))) => Ok(Expr::Column {
                        table: Some("excluded".to_string()),
                        name: bn.clone(),
                    }),
                    _ => Ok(Expr::Column {
                        table: table.clone(),
                        name: name.to_string(),
                    }),
                };
            }
            if !self.is_view_qual(q) {
                return Ok(Expr::Column {
                    table: table.clone(),
                    name: name.to_string(),
                });
            }
            // Qualified by the view name/alias: fall through to the
            // view-column resolution below.
        } else if !self.unqualified {
            return Ok(Expr::Column {
                table: None,
                name: name.to_string(),
            });
        } else if (name == "old" || name == "new") && self.view_col(name).is_none() {
            // Bare `old` / `new` in RETURNING: PG's whole-row forms;
            // the base executor resolves them.
            return Ok(Expr::Column {
                table: None,
                name: name.to_string(),
            });
        }
        match self.view_col(name) {
            Some((_, VCol::Base(b))) => Ok(Expr::Column {
                table: Some(self.target_qual.clone()),
                name: b.clone(),
            }),
            Some((_, VCol::Subst(se))) => {
                Ok(rename_base_expr(se, self.av, &self.target_qual, None))
            }
            Some((_, VCol::Blocked(Some(bn), _))) => Ok(Expr::Column {
                table: Some(self.target_qual.clone()),
                name: bn.clone(),
            }),
            _ => Ok(Expr::Column {
                table: table.clone(),
                name: name.to_string(),
            }),
        }
    }
    pub(crate) fn expr(&self, e: &Expr) -> Result<Expr, ExecError> {
        match e {
            Expr::Column { table, name } => self.col(table, name),
            Expr::WholeRow { qual } => {
                if self.is_view_qual(qual) {
                    Ok(Expr::WholeRow {
                        qual: self.target_qual.clone(),
                    })
                } else {
                    Ok(e.clone())
                }
            }
            _ => map_children(e, &mut |c| self.expr(c)),
        }
    }
}

// ===========================================================================
// Check-option collection + capture-mode RETURNING projection.
// ===========================================================================

/// A check to enforce on final rows, in `av.base` space.
struct PendingCheck {
    owner: String,
    qual: Expr,
}

/// Invert one rename step: an expression over view `w`'s BASE
/// columns -> the same expression over `w`'s OUTPUT columns.
/// Returns None when a referenced column is not a simple Var of
/// w's base (the value is not recoverable from w's output row).
fn invert_rename(e: &Expr, w: &AutoView) -> Option<Expr> {
    match e {
        Expr::Column { name, .. } => {
            let pos = w
                .cols
                .iter()
                .position(|c| matches!(c, VCol::Base(b) if b == name))?;
            Some(Expr::Column {
                table: None,
                name: w.view_cols[pos].0.clone(),
            })
        }
        Expr::WholeRow { .. } => None,
        _ => {
            let mut failed = false;
            let out = map_children(e, &mut |c| match invert_rename(c, w) {
                Some(x) => Ok(x),
                None => {
                    failed = true;
                    Ok(c.clone())
                }
            })
            .ok()?;
            if failed { None } else { Some(out) }
        }
    }
}

/// The checks this statement must enforce at THIS view level
/// (PG19 rewriteTargetView's WithCheckOption collection): the
/// view's own qual when it carries a check option, plus — for
/// CASCADED — every underlying view's qual translated into this
/// view's base space. LOCAL relies on the recursion to enforce
/// flagged underlying views in their own space.
fn collect_checks(
    eng: &Engine,
    ctx: &StmtCtx,
    view: &str,
    av: &AutoView,
) -> Result<Vec<PendingCheck>, ExecError> {
    let mut out = Vec::new();
    if !av.check_option.present() {
        return Ok(out);
    }
    if let Some(q) = &av.qual {
        out.push(PendingCheck {
            owner: view.rsplit('.').next().unwrap_or(view).to_string(),
            qual: rename_base_expr(q, av, &av.base, None),
        });
    }
    if av.check_option == CheckOption::Cascaded {
        // Walk the chain, translating each deeper qual up into
        // av.base space one inverse rename at a time.
        let mut chain: Vec<(String, AutoView)> = Vec::new();
        let mut cur_base = av.base.clone();
        loop {
            let Some(wv) = eng.db.find_view(&cur_base, ctx.snap, ctx.own) else {
                break;
            };
            // PG19: the cascade stops at a view with INSTEAD OF
            // triggers — the row never flows through that view's
            // query, so neither its qual nor anything below it is
            // checked ("check options don't work with INSTEAD
            // triggers/rules").
            if wv
                .triggers
                .iter()
                .any(|t| t.timing == crate::sql::TriggerTiming::InsteadOf)
            {
                break;
            }
            match classify_view(eng, ctx, &cur_base)? {
                ViewClass::Auto(w) => {
                    let next = w.base.clone();
                    chain.push((cur_base.clone(), w));
                    cur_base = next;
                }
                ViewClass::NotAuto(_) => break,
            }
        }
        // chain[i] is the view whose output space is chain[i-1]'s
        // base space (av.base for i=0). The qual of chain[i] lives
        // in chain[i].base space; invert through chain[i], then
        // chain[i-1], ..., chain[0] to reach av.base space.
        for i in 0..chain.len() {
            let (wname, w) = &chain[i];
            let Some(q) = &w.qual else { continue };
            let mut expr = q.clone();
            let mut ok = true;
            for j in (0..=i).rev() {
                match invert_rename(&expr, &chain[j].1) {
                    Some(x) => expr = x,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                // The refs are now av.base's output column names,
                // unqualified.
                out.push(PendingCheck {
                    owner: wname.rsplit('.').next().unwrap_or(wname).to_string(),
                    qual: expr,
                });
            }
        }
    }
    Ok(out)
}

/// Columns of the relation `av.base` (a table's columns, or a
/// view's output columns), for capture scopes.
fn base_scope_cols(
    eng: &Engine,
    ctx: &StmtCtx,
    av: &AutoView,
) -> Result<Vec<(String, ColType)>, ExecError> {
    if let Some(t) = eng
        .db
        .find_table(&av.base, ctx.snap, &ctx.all_xids, ctx.session)
    {
        Ok(t.columns.clone())
    } else {
        view_columns_of(eng, ctx, &av.base)
    }
}

fn qcols_of(qual: &str, cols: &[(String, ColType)]) -> Vec<QCol> {
    cols.iter()
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

/// Precomputed scope for repeated `eval_scoped` calls against the same
/// (qualifier, column-list) pair: every call site here evaluates many
/// rows (and/or many expressions) against one fixed base-relation scope
/// per statement, so the `QCol`/`RowProv` vectors -- built once here --
/// are reused instead of being re-derived (re-cloning every column name)
/// on every row.
struct ScopeCtx {
    qc: Vec<QCol>,
    prov: Vec<RowProv>,
}

fn scope_ctx(qual: &str, cols: &[(String, ColType)]) -> ScopeCtx {
    ScopeCtx {
        qc: qcols_of(qual, cols),
        prov: vec![RowProv {
            qual: qual.to_string(),
            table: qual.to_string(),
            row_id: u64::MAX,
        }],
    }
}

fn eval_scoped(
    eng: &mut Engine,
    ctx: &StmtCtx,
    scope: &ScopeCtx,
    row: &[Value],
    expr: &Expr,
) -> Result<Value, ExecError> {
    eval_dml_expr(
        eng,
        ctx.snap,
        ctx.own,
        ctx.session,
        ctx.role,
        &[(&scope.qc, row)],
        Some(&scope.prov),
        expr,
        &[],
        None,
        None,
    )
}

/// Enforce the collected checks over final (new) base rows.
fn enforce_checks(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    av: &AutoView,
    base_cols: &[(String, ColType)],
    checks: &[PendingCheck],
    new_rows: &[Row],
) -> Result<(), ExecError> {
    let scope = scope_ctx(&av.base, base_cols);
    for row in new_rows {
        for chk in checks {
            let v = eval_scoped(eng, ctx, &scope, row, &chk.qual)?;
            if v != Value::Bool(true) {
                return Err(exec_err(
                    "44000",
                    format!("new row violates check option for view \"{}\"", chk.owner),
                ));
            }
        }
    }
    Ok(())
}

/// Build the view-space row for one base-space row.
fn view_row_from_base(
    eng: &mut Engine,
    ctx: &StmtCtx,
    av: &AutoView,
    base_cols: &[(String, ColType)],
    scope: &ScopeCtx,
    base_row: &[Value],
) -> Result<Vec<Value>, ExecError> {
    let mut out = Vec::with_capacity(av.cols.len());
    for c in &av.cols {
        out.push(match c {
            VCol::Base(b) | VCol::Blocked(Some(b), _) => base_cols
                .iter()
                .position(|(n, _)| n == b)
                .and_then(|i| base_row.get(i).cloned())
                .unwrap_or(Value::Null),
            VCol::Subst(e) => eval_scoped(eng, ctx, scope, base_row, e)?,
            VCol::Blocked(None, _) => Value::Null,
        });
    }
    Ok(out)
}

/// Does this RETURNING list need capture mode (star forms, whole
/// rows, or old/new references the base executor cannot evaluate)?
/// Does the RETURNING list read the OLD row (old.* / old.col /
/// bare `old`)?
fn returning_refs_old(returning: &[SelectItem]) -> bool {
    returning.iter().any(|item| match item {
        SelectItem::AllOf(q) => q == "old",
        SelectItem::Expr { expr, .. } => {
            let mut n = false;
            walk_expr(expr, &mut |x| {
                if let Expr::Column { table, name } = x {
                    if matches!(table.as_deref(), Some("old")) || (table.is_none() && name == "old")
                    {
                        n = true;
                    }
                }
                if matches!(x, Expr::WholeRow { qual } if qual == "old") {
                    n = true;
                }
            });
            n
        }
        _ => false,
    })
}

fn returning_needs_capture(returning: &[SelectItem]) -> bool {
    returning.iter().any(|item| match item {
        SelectItem::All | SelectItem::AllOf(_) => true,
        SelectItem::Expr { expr, .. } => {
            let mut need = false;
            walk_expr(expr, &mut |x| match x {
                Expr::WholeRow { .. } => need = true,
                Expr::Column { table, name } => {
                    if matches!(table.as_deref(), Some("old") | Some("new")) {
                        need = true;
                    }
                    if table.is_none() && (name == "old" || name == "new") {
                        need = true;
                    }
                }
                _ => {}
            });
            need
        }
    })
}

/// Expand star / bare-old-new forms of a RETURNING list into plain
/// Expr items over view-space scopes (qualifiers: view name for the
/// primary row, "old" / "new").
fn expand_capture_items(view: &str, av: &AutoView, returning: &[SelectItem]) -> Vec<SelectItem> {
    let col_items = |qual: &str| -> Vec<SelectItem> {
        av.view_cols
            .iter()
            .map(|(n, _)| SelectItem::Expr {
                expr: Expr::Column {
                    table: Some(qual.to_string()),
                    name: n.clone(),
                },
                alias: None,
            })
            .collect()
    };
    let mut out = Vec::new();
    for item in returning {
        match item {
            SelectItem::All => out.extend(col_items(view)),
            SelectItem::AllOf(q) if q == view || q == "old" || q == "new" => {
                out.extend(col_items(q))
            }
            SelectItem::AllOf(_) => out.push(item.clone()),
            SelectItem::Expr { expr, alias } => match expr {
                Expr::Column { table: None, name }
                    if (name == "old" || name == "new")
                        && av.view_cols.iter().all(|(n, _)| n != name) =>
                {
                    out.push(SelectItem::Expr {
                        expr: Expr::WholeRow { qual: name.clone() },
                        alias: alias.clone(),
                    })
                }
                _ => out.push(item.clone()),
            },
        }
    }
    out
}

/// Output columns for a capture-mode RETURNING.
fn capture_returning_cols(
    eng: &Engine,
    ctx: &StmtCtx,
    view: &str,
    av: &AutoView,
    expanded: &[SelectItem],
) -> Vec<(String, ColType)> {
    let primary = qcols_of(view, &av.view_cols);
    let old = qcols_of("old", &av.view_cols);
    let new = qcols_of("new", &av.view_cols);
    let refs: Vec<&[QCol]> = vec![&primary, &old, &new];
    let mut out = Vec::new();
    for item in expanded {
        if let SelectItem::Expr { expr, alias } = item {
            let ty = expr_type(eng, ctx.snap, ctx.own, ctx.session, &refs, &[], &[], expr)
                .unwrap_or(ColType::Text);
            let name = alias.clone().unwrap_or_else(|| expr_col_name(expr));
            out.push((name, ty));
        }
    }
    out
}

/// Project one capture-mode RETURNING row over view-space old/new
/// rows. `primary_is_old` for DELETE (unqualified refs read OLD).
fn project_capture_row(
    eng: &mut Engine,
    ctx: &StmtCtx,
    view: &str,
    av: &AutoView,
    old_row: &[Value],
    new_row: &[Value],
    primary_is_old: bool,
    expanded: &[SelectItem],
) -> Result<Vec<Value>, ExecError> {
    let (primary_row, _): (&[Value], ()) = if primary_is_old {
        (old_row, ())
    } else {
        (new_row, ())
    };
    let qc_primary = qcols_of(view, &av.view_cols);
    let qc_old = qcols_of("old", &av.view_cols);
    let qc_new = qcols_of("new", &av.view_cols);
    let prov = vec![
        RowProv {
            qual: view.to_string(),
            table: view.to_string(),
            row_id: u64::MAX,
        },
        RowProv {
            qual: "old".to_string(),
            table: view.to_string(),
            row_id: u64::MAX,
        },
        RowProv {
            qual: "new".to_string(),
            table: view.to_string(),
            row_id: u64::MAX,
        },
    ];
    project_returning(
        eng,
        ctx.snap,
        ctx.own,
        ctx.session,
        ctx.role,
        &[
            (&qc_primary, primary_row),
            (&qc_old, old_row),
            (&qc_new, new_row),
        ],
        &prov,
        expanded,
        &[],
        None,
    )
}

/// Translate a RETURNING list for pass-through (Mode T): every item
/// must be a plain expression; anything else returns None (caller
/// uses capture mode instead).
/// PG names a RETURNING column after the view column it reads;
/// when a rename substituted an expression (or a differently
/// named base column) for a bare column reference, pin the
/// original name as the item alias so the result keeps it.
fn pin_col_name(expr: &Expr, alias: &Option<String>) -> Option<String> {
    match (alias, expr) {
        (None, Expr::Column { name, .. }) => Some(name.clone()),
        (a, _) => a.clone(),
    }
}

fn translate_returning(ren: &Ren, returning: &[SelectItem]) -> Option<Vec<SelectItem>> {
    let mut out = Vec::with_capacity(returning.len());
    for item in returning {
        match item {
            SelectItem::Expr { expr, alias } => {
                let t_expr = ren.expr(expr).ok()?;
                out.push(SelectItem::Expr {
                    expr: t_expr,
                    alias: pin_col_name(expr, alias),
                })
            }
            _ => return None,
        }
    }
    Some(out)
}

/// The view-column index for a DML target name, or the PG error.
fn target_col(av: &AutoView, view: &str, name: &str) -> Result<usize, ExecError> {
    av.view_cols
        .iter()
        .position(|(n, _)| n == name)
        .ok_or_else(|| {
            exec_err(
                "42703",
                format!(
                    "column \"{}\" of relation \"{}\" does not exist",
                    name,
                    view.rsplit('.').next().unwrap_or(view)
                ),
            )
        })
}

/// A targeted column must be updatable: its base name, or PG's
/// 0A000 column error.
fn updatable_target(
    av: &AutoView,
    view: &str,
    event: &str,
    idx: usize,
) -> Result<String, ExecError> {
    match &av.cols[idx] {
        VCol::Base(b) => Ok(b.clone()),
        other => Err(err_view_col_not_updatable(
            view,
            event,
            &av.view_cols[idx].0,
            other.reason().unwrap_or(R_NOTCOL),
        )),
    }
}

// ---------------------------------------------------------------------
// v2.18: privilege model (PG19 aclchk.c via rewrite). The caller is
// checked against the VIEW (relation + column ACLs); each view hop's
// OWNER is then checked against the next object; the translated
// base statement itself runs with base checks suppressed
// (StmtCtx.privs_suppressed) because the chain check IS the check.
// ---------------------------------------------------------------------

fn view_privs_for(db: &Database, role: &str, v: &ViewDef, snap: &Snapshot, own: u64) -> u32 {
    if crate::storage::is_superuser_snap(db, role, snap, own) {
        return crate::storage::PRIV_ALL_TABLE;
    }
    let closure = crate::storage::role_closure(db, role, snap, own);
    let owner_item = |v: &ViewDef| {
        v.acl
            .iter()
            .find(|e| e.role == v.owner && (e.grantor.is_empty() || e.grantor == v.owner))
            .map(|e| e.privs)
            .unwrap_or(crate::storage::PRIV_ALL_TABLE)
    };
    if role == v.owner {
        return owner_item(v);
    }
    let mut privs = 0u32;
    if closure.iter().any(|n| n == &v.owner) {
        privs |= owner_item(v);
    }
    if closure.iter().any(|n| n == "pg_read_all_data") {
        privs |= crate::storage::PRIV_SELECT;
    }
    if closure.iter().any(|n| n == "pg_write_all_data") {
        privs |=
            crate::storage::PRIV_INSERT | crate::storage::PRIV_UPDATE | crate::storage::PRIV_DELETE;
    }
    for e in &v.acl {
        if e.role == "public" || closure.iter().any(|n| n == &e.role) {
            privs |= e.privs;
        }
    }
    privs
}

fn view_col_privs_for(
    db: &Database,
    role: &str,
    v: &ViewDef,
    col: &str,
    snap: &Snapshot,
    own: u64,
) -> u32 {
    let mut privs = view_privs_for(db, role, v, snap, own);
    if privs == crate::storage::PRIV_ALL_TABLE {
        return privs;
    }
    let closure = crate::storage::role_closure(db, role, snap, own);
    for e in &v.col_acl {
        if (e.role == "public" || closure.iter().any(|n| n == &e.role))
            && e.columns.iter().any(|c| c == col)
        {
            privs |= e.privs;
        }
    }
    privs
}

fn priv_err(obj_kind: &str, name: &str) -> ExecError {
    exec_err(
        "42501",
        format!("permission denied for {} \"{}\"", obj_kind, name),
    )
}

/// Collect target-side column references (unqualified, or qualified
/// by one of `quals`) from an expression.
fn collect_col_refs(e: &Expr, quals: &[&str], out: &mut Vec<String>) {
    if let Expr::Column { table, name } = e {
        let ok = match table {
            None => true,
            Some(q) => quals.iter().any(|x| *x == q),
        };
        if ok && !out.contains(name) {
            out.push(name.clone());
        }
    }
    let _ = map_children(e, &mut |c| {
        collect_col_refs(c, quals, out);
        Ok(c.clone())
    });
}

/// Position-based column lookup on an analyzed view.
fn av_col<'a>(av: &'a AutoView, name: &str) -> Option<&'a VCol> {
    av.view_cols
        .iter()
        .position(|(n, _)| n == name)
        .map(|i| &av.cols[i])
}

/// Verify PG's privilege chain for a DML statement rewritten
/// through `view_name`. `ev_priv`: the statement's event privilege
/// (PRIV_INSERT / PRIV_UPDATE / PRIV_DELETE). `set_cols`:
/// view-space columns assigned (INSERT targets / UPDATE SET
/// targets). `read_cols`: view-space columns read by WHERE / SET
/// RHS / ON / RETURNING (SELECT privilege).
#[allow(clippy::too_many_arguments)]
fn check_view_dml_privs(
    eng: &Engine,
    ctx: &StmtCtx,
    view_name: &str,
    av: &AutoView,
    ev_priv: u32,
    set_cols: &[String],
    read_cols: &[String],
) -> Result<(), ExecError> {
    check_view_dml_privs_x(
        eng, ctx, view_name, av, ev_priv, set_cols, read_cols, false, false,
    )
}

#[allow(clippy::too_many_arguments)]
fn check_view_dml_privs_x(
    eng: &Engine,
    ctx: &StmtCtx,
    view_name: &str,
    av: &AutoView,
    ev_priv: u32,
    set_cols: &[String],
    read_cols: &[String],
    deep_select_all: bool,
    deep_reads: bool,
) -> Result<(), ExecError> {
    // Nested execution: an outer view translation is driving this one
    // (privs_suppressed is set only by the viewdml executors). The
    // outermost walk already checked the whole chain with the correct
    // per-hop roles; re-walking from here with the caller's role would
    // wrongly deny hops the chain's owners are entitled to.
    if ctx.privs_suppressed {
        return Ok(());
    }
    let db = &eng.db;
    let mut role = ctx.role.to_string();
    let mut cur_name = view_name.to_string();
    let mut cur_av = av.clone();
    let mut cur_set: Vec<String> = set_cols.to_vec();
    let mut cur_read: Vec<String> = read_cols.to_vec();
    // Columns the previous hop's view exposes of the CURRENT object
    // (names in the current object's namespace), for the ON
    // CONFLICT deep whole-row read rule.
    let mut deep_cols: Vec<String> = Vec::new();
    let mut first = true;
    loop {
        let v = db
            .find_view(&cur_name, ctx.snap, ctx.own)
            .ok_or_else(|| priv_err("view", &cur_name))?;
        // -- event privilege of `role` on this view.
        if ev_priv == crate::storage::PRIV_DELETE || cur_set.is_empty() {
            if view_privs_for(db, &role, v, ctx.snap, ctx.own) & ev_priv != ev_priv {
                return Err(priv_err("view", &cur_name));
            }
        } else {
            for c in &cur_set {
                if view_col_privs_for(db, &role, v, c, ctx.snap, ctx.own) & ev_priv != ev_priv {
                    return Err(exec_err(
                        "42501",
                        format!(
                            "permission denied for column \"{}\" of view \"{}\"",
                            c, cur_name
                        ),
                    ));
                }
            }
        }
        // -- read privilege: per-column on the outermost view,
        // relation-level on deeper hops (and on the base below);
        // with deep_reads (the ON CONFLICT action walk) the read
        // set is mapped through the hops and checked per-column
        // at every level, as PG checks the rewritten query.
        if deep_reads && !first {
            for c in &cur_read {
                if view_col_privs_for(db, &role, v, c, ctx.snap, ctx.own)
                    & crate::storage::PRIV_SELECT
                    != crate::storage::PRIV_SELECT
                {
                    return Err(exec_err(
                        "42501",
                        format!(
                            "permission denied for column \"{}\" of view \"{}\"",
                            c, cur_name
                        ),
                    ));
                }
            }
        } else if first {
            for c in read_cols {
                if view_col_privs_for(db, &role, v, c, ctx.snap, ctx.own)
                    & crate::storage::PRIV_SELECT
                    != crate::storage::PRIV_SELECT
                {
                    return Err(exec_err(
                        "42501",
                        format!(
                            "permission denied for column \"{}\" of view \"{}\"",
                            c, cur_name
                        ),
                    ));
                }
            }
        } else if ev_priv != crate::storage::PRIV_INSERT
            && view_privs_for(db, &role, v, ctx.snap, ctx.own) & crate::storage::PRIV_SELECT
                != crate::storage::PRIV_SELECT
        {
            return Err(priv_err("view", &cur_name));
        }
        // ON CONFLICT DO UPDATE / DO SELECT: the speculative
        // machinery reads the whole row at every hop below the
        // caller's, so each view owner needs SELECT on every column
        // the view exposes of the next object.
        if deep_select_all && !first {
            for vn in &deep_cols {
                if view_col_privs_for(db, &role, v, vn, ctx.snap, ctx.own)
                    & crate::storage::PRIV_SELECT
                    != crate::storage::PRIV_SELECT
                {
                    return Err(exec_err(
                        "42501",
                        format!(
                            "permission denied for column \"{}\" of view \"{}\"",
                            vn, cur_name
                        ),
                    ));
                }
            }
        }
        // PG19: at a security_invoker view the current role keeps
        // its own privileges into the next hop (no owner switch).
        if !v.security_invoker {
            role = v.owner.clone();
        }
        let base_is_view = db.find_view(&cur_av.base, ctx.snap, ctx.own).is_some();
        if base_is_view {
            let next = cur_av.base.clone();
            let next_av =
                match classify_view_x(eng, ctx.snap, ctx.own, ctx.session, &ctx.all_xids, &next) {
                    Ok(ViewClass::Auto(a)) => a,
                    _ => return Ok(()), // classify already gated the outer call
                };
            cur_set = cur_set
                .iter()
                .filter_map(|c| match av_col(&cur_av, c) {
                    Some(VCol::Base(b)) => Some(b.clone()),
                    _ => None,
                })
                .collect();
            cur_read = cur_read
                .iter()
                .filter_map(|c| match av_col(&cur_av, c) {
                    Some(VCol::Base(b)) => Some(b.clone()),
                    _ => None,
                })
                .collect();
            deep_cols = cur_av
                .cols
                .iter()
                .filter_map(|k| match k {
                    VCol::Base(b) => Some(b.clone()),
                    _ => None,
                })
                .collect();
            cur_name = next;
            cur_av = next_av;
            first = false;
        } else {
            let bt = cur_av.base.clone();
            let t = db
                .find_table(&bt, ctx.snap, &ctx.all_xids, ctx.session)
                .ok_or_else(|| priv_err("table", &bt))?;
            let closure = crate::storage::role_closure(db, &role, ctx.snap, ctx.own);
            if ev_priv == crate::storage::PRIV_DELETE || cur_set.is_empty() {
                let have =
                    crate::storage::table_privs_in(db, &role, t, &closure, ctx.snap, ctx.own);
                if have & ev_priv != ev_priv {
                    return Err(priv_err("table", &bt));
                }
            } else {
                for c in &cur_set {
                    let base_c = match av_col(&cur_av, c) {
                        Some(VCol::Base(b)) => b.clone(),
                        _ => continue,
                    };
                    let have = crate::storage::column_privs_in(
                        db, &role, t, &base_c, &closure, ctx.snap, ctx.own,
                    );
                    if have & ev_priv != ev_priv {
                        return Err(exec_err(
                            "42501",
                            format!(
                                "permission denied for column \"{}\" of table \"{}\"",
                                base_c, bt
                            ),
                        ));
                    }
                }
            }
            if ev_priv != crate::storage::PRIV_INSERT {
                if deep_reads {
                    for c in &cur_read {
                        let base_c = match av_col(&cur_av, c) {
                            Some(VCol::Base(b)) => b.clone(),
                            _ => continue,
                        };
                        let have = crate::storage::column_privs_in(
                            db, &role, t, &base_c, &closure, ctx.snap, ctx.own,
                        );
                        if have & crate::storage::PRIV_SELECT != crate::storage::PRIV_SELECT {
                            return Err(exec_err(
                                "42501",
                                format!(
                                    "permission denied for column \"{}\" of table \"{}\"",
                                    base_c, bt
                                ),
                            ));
                        }
                    }
                } else {
                    let have =
                        crate::storage::table_privs_in(db, &role, t, &closure, ctx.snap, ctx.own);
                    if have & crate::storage::PRIV_SELECT != crate::storage::PRIV_SELECT {
                        return Err(priv_err("table", &bt));
                    }
                }
            }
            if deep_select_all {
                let exposed: Vec<String> = cur_av
                    .cols
                    .iter()
                    .filter_map(|k| match k {
                        VCol::Base(b) => Some(b.clone()),
                        _ => None,
                    })
                    .collect();
                for bc in &exposed {
                    let have = crate::storage::column_privs_in(
                        db, &role, t, bc, &closure, ctx.snap, ctx.own,
                    );
                    if have & crate::storage::PRIV_SELECT != crate::storage::PRIV_SELECT {
                        return Err(exec_err(
                            "42501",
                            format!(
                                "permission denied for column \"{}\" of table \"{}\"",
                                bc, bt
                            ),
                        ));
                    }
                }
            }
            return Ok(());
        }
    }
}

/// Turn on base-object privilege suppression for the duration of
/// a translated base DML call (the chain check above already ran
/// PG's checks); returns the previous flag for restore.
fn suppress_privs(ctx: &mut StmtCtx) -> bool {
    std::mem::replace(&mut ctx.privs_suppressed, true)
}

// ===========================================================================
// The DML executors (translation onto the base relation).
// ===========================================================================

/// Scan the OLD base rows an UPDATE is about to touch (for
/// capture-mode RETURNING old.*), in the base executor's scan
/// order. Only for the no-FROM, no-WITH shape.
fn prescan_update_old_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    view: &str,
    av: &AutoView,
    where_t: &Option<Expr>,
) -> Result<Vec<Row>, ExecError> {
    // PG reads the base through the view OWNER's rights (the
    // rewrite's security context), not the caller's.
    let owner_override = eng
        .db
        .find_view(view, ctx.snap, ctx.own)
        .map(|v| v.owner.clone());
    let mut sel = crate::sql::empty_select();
    sel.items = vec![SelectItem::All];
    sel.from = vec![FromItem::Table {
        name: av.base.clone(),
        alias: None,
        col_aliases: Vec::new(),
        only: false,
    }];
    sel.where_ = where_t.clone();
    let mut lock_ids = Vec::new();
    let mut q = Q {
        eng: &mut *eng,
        snap: ctx.snap,
        own: ctx.own,
        all_xids: ctx.all_xids.clone(),
        session: ctx.session,
        role: ctx.role,
        read_only: true,
        depth: 0,
        lock_ids: &mut lock_ids,
        ctes: Vec::new(),
        wctx: None,
        srf_vals: Vec::new(),
        priv_scopes: Vec::new(),
        priv_role_override: owner_override,
        hashed_exists: Rc::new(RefCell::new(HashMap::new())),
        immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
        plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
        hashed_in: Rc::new(RefCell::new(Vec::new())),
        pending_updates: None,
        write: None,
    };
    let out = run_select(&mut q, &sel, &[])?;
    Ok(out.rows)
}

/// v2.18: for ON CONFLICT DO UPDATE / DO SELECT, PG's RETURNING
/// `old` is the pre-existing conflicting row; the base executor
/// only returns post rows, so pre-scan the conflicting base rows
/// (one bounded probe per proposed VALUES row: arbiter-column
/// equalities ANDed with the view qual, read through the view
/// owner's rights). One Option per proposed row; None = no
/// conflicting row (fresh insert) or an uncovered shape (the
/// caller falls back to a null old row).
#[allow(clippy::too_many_arguments)]
fn prescan_conflict_old_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    view: &str,
    av: &AutoView,
    on_conflict: &OnConflict,
    columns: &Option<Vec<InsertTarget>>,
    rows: &[Vec<InsertValue>],
) -> Vec<Option<Row>> {
    let none_vec = |n: usize| -> Vec<Option<Row>> { vec![None; n] };
    match &on_conflict.action {
        ConflictAction::DoUpdate { .. } | ConflictAction::DoSelect { .. } => {}
        ConflictAction::DoNothing => return Vec::new(),
    }
    let ConflictArbiter::Columns(elems) = &on_conflict.arbiter else {
        return Vec::new();
    };
    // Arbiter columns in view space, with their positions in the
    // target list (explicit or positional-updatable order).
    let target_names: Vec<String> = match columns {
        Some(ts) => ts.iter().map(|t| t.name.clone()).collect(),
        None => av
            .view_cols
            .iter()
            .enumerate()
            .filter(|(i, _)| matches!(av.cols[*i], VCol::Base(_)))
            .map(|(_, (n, _))| n.clone())
            .collect(),
    };
    let mut arb: Vec<(usize, String)> = Vec::new(); // (target pos, base col)
    for e in elems {
        let Some(vname) = arbiter_elem_bare_name(&e.key) else {
            return Vec::new();
        };
        let Ok(idx) = target_col(av, view, vname) else {
            return Vec::new();
        };
        let VCol::Base(bname) = &av.cols[idx] else {
            return Vec::new();
        };
        let Some(pos) = target_names.iter().position(|n| n == vname) else {
            return Vec::new();
        };
        arb.push((pos, bname.clone()));
    }
    if arb.is_empty() || rows.is_empty() || rows.len() > 256 {
        return Vec::new();
    }
    let owner_override = eng
        .db
        .find_view(view, ctx.snap, ctx.own)
        .map(|v| v.owner.clone());
    let mut out: Vec<Option<Row>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cond: Option<Expr> = av
            .qual
            .as_ref()
            .map(|q| rename_base_expr(q, av, &av.base, None));
        let mut covered = true;
        for (pos, bname) in &arb {
            let val = match row.get(*pos) {
                Some(InsertValue::Lit(l)) => Expr::Literal(l.clone()),
                Some(InsertValue::Expr(e)) => {
                    // VALUES expressions must be column-free to be
                    // reused as probe predicates.
                    let mut has_col = false;
                    walk_expr(e, &mut |x| {
                        if matches!(x, Expr::Column { .. }) {
                            has_col = true;
                        }
                    });
                    if has_col {
                        covered = false;
                        break;
                    }
                    e.clone()
                }
                _ => {
                    covered = false;
                    break;
                }
            };
            let eq = Expr::Cmp {
                op: CmpOp::Eq,
                left: Box::new(Expr::Column {
                    table: None,
                    name: bname.clone(),
                }),
                right: Box::new(val),
            };
            cond = Some(match cond {
                Some(c) => Expr::And(Box::new(c), Box::new(eq)),
                None => eq,
            });
        }
        if !covered {
            out.push(None);
            continue;
        }
        let mut sel = crate::sql::empty_select();
        sel.items = vec![SelectItem::All];
        sel.from = vec![FromItem::Table {
            name: av.base.clone(),
            alias: None,
            col_aliases: Vec::new(),
            only: false,
        }];
        sel.where_ = cond;
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: true,
            depth: 0,
            lock_ids: &mut lock_ids,
            ctes: Vec::new(),
            wctx: None,
            srf_vals: Vec::new(),
            priv_scopes: Vec::new(),
            priv_role_override: owner_override.clone(),
            hashed_exists: Rc::new(RefCell::new(HashMap::new())),
            immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
            plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
            hashed_in: Rc::new(RefCell::new(Vec::new())),
            pending_updates: None,
            write: None,
        };
        match run_select(&mut q, &sel, &[]) {
            Ok(r) => out.push(r.rows.into_iter().next()),
            Err(_) => {
                return none_vec(rows.len());
            }
        }
    }
    out
}

/// Convert a stored column default into an expression usable in
/// a VALUES row / SELECT list (PG19: view defaults are expanded
/// into the rewritten query like base defaults).
fn default_to_expr(d: &DefaultExpr) -> Expr {
    match d {
        DefaultExpr::Lit(l) => Expr::Literal(l.clone()),
        DefaultExpr::Nextval(seq) => Expr::Func {
            name: "nextval".to_string(),
            args: vec![Expr::Literal(Literal::Text(seq.as_str().into()))],
        },
        DefaultExpr::Expr(e) => e.clone(),
    }
}

/// The view's per-column defaults, position-padded to the view's
/// output width (ALTER VIEW ... ALTER COLUMN c SET DEFAULT).
fn view_defaults_of(
    eng: &Engine,
    ctx: &StmtCtx,
    view: &str,
    width: usize,
) -> Vec<Option<DefaultExpr>> {
    let mut d = eng
        .db
        .find_view(view, ctx.snap, ctx.own)
        .map(|v| v.col_defaults.clone())
        .unwrap_or_default();
    d.resize(width, None);
    d
}

/// Apply a view's column defaults to an INSERT in view space
/// (before translation): explicit DEFAULT placeholders take the
/// view default, omitted columns with view defaults are appended
/// as targets, and INSERT ... DEFAULT VALUES materializes the
/// defaulted columns. Only base-mapped columns are appended (a
/// default on an expression column affects only the proposed row
/// in ON CONFLICT / RETURNING, which this pass does not model).
/// Returns None when the view sets no defaults.
#[allow(clippy::type_complexity)]
fn apply_view_defaults_insert(
    av: &AutoView,
    vdefs: &[Option<DefaultExpr>],
    columns: &Option<Vec<InsertTarget>>,
    rows: &[Vec<InsertValue>],
    select: &Option<SelectStmt>,
) -> Option<(
    Option<Vec<InsertTarget>>,
    Vec<Vec<InsertValue>>,
    Option<SelectStmt>,
)> {
    if vdefs.iter().all(|d| d.is_none()) {
        return None;
    }
    let mut cols_o = columns.clone();
    let mut rows_o: Vec<Vec<InsertValue>> = rows.to_vec();
    let mut sel_o = select.clone();
    match &mut cols_o {
        Some(targets) => {
            for (j, t) in targets.iter().enumerate() {
                if let Some(idx) = av.view_cols.iter().position(|(n, _)| n == &t.name) {
                    if let Some(d) = &vdefs[idx] {
                        if sel_o.is_none() {
                            let e = default_to_expr(d);
                            for r in rows_o.iter_mut() {
                                if j < r.len() && matches!(r[j], InsertValue::Default) {
                                    r[j] = InsertValue::Expr(e.clone());
                                }
                            }
                        }
                    }
                }
            }
            let present: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
            for (i, (vn, _)) in av.view_cols.iter().enumerate() {
                if present.iter().any(|p| p == vn) {
                    continue;
                }
                if let Some(d) = &vdefs[i] {
                    targets.push(InsertTarget {
                        name: vn.clone(),
                        indirection: Vec::new(),
                    });
                    let e = default_to_expr(d);
                    match &mut sel_o {
                        Some(sel) => sel.items.push(SelectItem::Expr {
                            expr: e,
                            alias: None,
                        }),
                        None => {
                            for r in rows_o.iter_mut() {
                                r.push(InsertValue::Expr(e.clone()));
                            }
                        }
                    }
                }
            }
        }
        None => {
            let positional = sel_o.is_some() || rows_o.iter().any(|r| !r.is_empty());
            if positional {
                // Positional values address the UPDATABLE columns
                // in view order.
                let base_idx: Vec<usize> = av
                    .cols
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| matches!(c, VCol::Base(_)))
                    .map(|(i, _)| i)
                    .collect();
                for r in rows_o.iter_mut() {
                    for (i, v) in r.iter_mut().enumerate() {
                        if matches!(v, InsertValue::Default) {
                            if let Some(&vi) = base_idx.get(i) {
                                if let Some(Some(d)) = vdefs.get(vi) {
                                    *v = InsertValue::Expr(default_to_expr(d));
                                }
                            }
                        }
                    }
                }
            } else {
                let mut targets = Vec::new();
                let mut vals = Vec::new();
                for (i, (vn, _)) in av.view_cols.iter().enumerate() {
                    if let Some(d) = &vdefs[i] {
                        targets.push(InsertTarget {
                            name: vn.clone(),
                            indirection: Vec::new(),
                        });
                        vals.push(InsertValue::Expr(default_to_expr(d)));
                    }
                }
                if !targets.is_empty() {
                    cols_o = Some(targets);
                    rows_o = vec![vals];
                }
            }
        }
    }
    Some((cols_o, rows_o, sel_o))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_insert_auto_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    view: &str,
    columns: &Option<Vec<InsertTarget>>,
    rows: &[Vec<InsertValue>],
    select: &Option<SelectStmt>,
    with: &[CteDef],
    on_conflict: &Option<OnConflict>,
    returning: &[SelectItem],
) -> Result<ExecResult, ExecError> {
    let av = match classify_view(eng, ctx, view)? {
        ViewClass::Auto(av) => av,
        ViewClass::NotAuto(r) => return Err(err_view_not_updatable(view, "INSERT", &r)),
    };
    if !av_has_updatable(&av) {
        return Err(err_view_not_updatable(view, "INSERT", R_NOCOLS));
    }
    // --- v2.18: PG privilege chain (caller vs view, owners vs base)
    {
        let set_cols: Vec<String> = match columns {
            Some(ts) => ts.iter().map(|t| t.name.clone()).collect(),
            None => Vec::new(),
        };
        let mut read_cols: Vec<String> = Vec::new();
        if let Some(oc) = on_conflict {
            match &oc.action {
                ConflictAction::DoUpdate { sets, where_ } => {
                    for (_, e) in sets {
                        collect_col_refs(e, &[view], &mut read_cols);
                    }
                    if let Some(w) = where_ {
                        collect_col_refs(w, &[view], &mut read_cols);
                    }
                }
                ConflictAction::DoSelect { where_, .. } => {
                    if let Some(w) = where_ {
                        collect_col_refs(w, &[view], &mut read_cols);
                    }
                }
                ConflictAction::DoNothing => {}
            }
        }
        for it in returning {
            if let SelectItem::Expr { expr, .. } = it {
                collect_col_refs(expr, &[view], &mut read_cols);
            }
        }
        // ON CONFLICT DO UPDATE / DO SELECT: every hop below the
        // caller's must be able to read the whole row (deep rule),
        // and DO UPDATE's SET columns need UPDATE coverage as a
        // separate walk with the UPDATE event.
        let deep_conflict = matches!(
            on_conflict.as_ref().map(|oc| &oc.action),
            Some(ConflictAction::DoUpdate { .. }) | Some(ConflictAction::DoSelect { .. })
        );
        check_view_dml_privs_x(
            eng,
            ctx,
            view,
            &av,
            crate::storage::PRIV_INSERT,
            &set_cols,
            &read_cols,
            deep_conflict,
            false,
        )?;
        if let Some(oc) = on_conflict {
            if let ConflictAction::DoSelect {
                for_update: true, ..
            } = &oc.action
            {
                let v = eng
                    .db
                    .find_view(view, ctx.snap, ctx.own)
                    .ok_or_else(|| priv_err("view", view))?;
                let rel = view_privs_for(&eng.db, ctx.role, v, ctx.snap, ctx.own)
                    & crate::storage::PRIV_UPDATE
                    == crate::storage::PRIV_UPDATE;
                let any_col = av.view_cols.iter().any(|(cn, _)| {
                    view_col_privs_for(&eng.db, ctx.role, v, cn, ctx.snap, ctx.own)
                        & crate::storage::PRIV_UPDATE
                        == crate::storage::PRIV_UPDATE
                });
                if !rel && !any_col {
                    return Err(priv_err("view", view));
                }
            }
            if let ConflictAction::DoUpdate { sets, where_ } = &oc.action {
                let uset: Vec<String> = sets.iter().map(|(n, _)| n.clone()).collect();
                let mut uread: Vec<String> = Vec::new();
                for (_, e) in sets {
                    collect_col_refs(e, &[view, "excluded"], &mut uread);
                }
                if let Some(w) = where_ {
                    collect_col_refs(w, &[view, "excluded"], &mut uread);
                }
                check_view_dml_privs_x(
                    eng,
                    ctx,
                    view,
                    &av,
                    crate::storage::PRIV_UPDATE,
                    &uset,
                    &uread,
                    false,
                    true,
                )?;
            }
        }
    }
    // --- v2.18: view column defaults (view space, pre-translation)
    let vdefs = view_defaults_of(eng, ctx, view, av.view_cols.len());
    let owned_ins = apply_view_defaults_insert(&av, &vdefs, columns, rows, select);
    let (columns, rows, select) = match &owned_ins {
        Some((c, r, s2)) => (c, &r[..], s2),
        None => (columns, rows, select),
    };
    // --- translate the target column list -------------------------
    let t_columns: Option<Vec<InsertTarget>> = match columns {
        Some(targets) => {
            let mut out = Vec::with_capacity(targets.len());
            for t in targets {
                let idx = target_col(&av, view, &t.name)?;
                let b = updatable_target(&av, view, "INSERT", idx)?;
                out.push(InsertTarget {
                    name: b,
                    indirection: t.indirection.clone(),
                });
            }
            Some(out)
        }
        None => {
            // Positional assignment touches every view column
            // (except the DEFAULT VALUES shape: one empty row).
            let positional = select.is_some() || rows.iter().any(|r| !r.is_empty());
            if positional {
                // PG19: with no explicit column list, the target
                // columns are the view's UPDATABLE columns only,
                // in view order (expression/system columns are
                // simply not part of the target list). The values
                // may be in a different order than the base's:
                // name every target explicitly.
                Some(
                    av.cols
                        .iter()
                        .filter_map(|c| match c {
                            VCol::Base(b) => Some(InsertTarget {
                                name: b.clone(),
                                indirection: Vec::new(),
                            }),
                            _ => None,
                        })
                        .collect(),
                )
            } else {
                None
            }
        }
    };
    // --- translate ON CONFLICT ------------------------------------
    let ren = Ren {
        view,
        alias: None,
        av: &av,
        target_qual: av.base.clone(),
        unqualified: true,
    };
    let t_oc = match on_conflict {
        None => None,
        Some(oc) => Some(translate_on_conflict(&ren, view, oc)?),
    };
    // --- capture decision -----------------------------------------
    let checks = collect_checks(eng, ctx, view, &av)?;
    let capture = !checks.is_empty() || returning_needs_capture(returning);
    let base_cols = base_scope_cols(eng, ctx, &av)?;
    // ON CONFLICT old rows for RETURNING old (DO UPDATE / DO
    // SELECT): pre-scan before the write, while the conflicting
    // rows are still in their pre-statement state.
    let conflict_olds: Vec<Option<Row>> = if capture && returning_refs_old(returning) {
        match on_conflict {
            Some(oc) => prescan_conflict_old_rows(eng, ctx, view, &av, oc, columns, rows),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let t_returning: Vec<SelectItem> = if capture {
        vec![SelectItem::All]
    } else {
        translate_returning(&ren, returning).unwrap_or_default()
    };
    let saved = suppress_privs(ctx);
    let res = exec_insert(
        eng,
        ctx,
        &av.base,
        &t_columns,
        rows,
        select,
        with,
        &t_oc,
        &t_returning,
    );
    ctx.privs_suppressed = saved;
    let res = res?;
    if !capture {
        return Ok(res);
    }
    let ExecResult::Dml {
        tag,
        rows: base_rows,
        ..
    } = res
    else {
        return Ok(res);
    };
    enforce_checks(eng, ctx, &av, &base_cols, &checks, &base_rows)?;
    if returning.is_empty() {
        return Ok(ExecResult::Dml {
            tag,
            columns: Vec::new(),
            rows: Vec::new(),
        });
    }
    let expanded = expand_capture_items(view, &av, returning);
    let rcols = capture_returning_cols(eng, ctx, view, &av, &expanded);
    let null_view_row = vec![Value::Null; av.view_cols.len()];
    let scope = scope_ctx(&av.base, &base_cols);
    // Positional pairing between proposed and output rows holds
    // for DO UPDATE / DO SELECT (every proposed row yields exactly
    // one output row); verify the counts before trusting it.
    let pair_olds = base_rows.len() == rows.len() && conflict_olds.len() == rows.len();
    let mut out_rows = Vec::with_capacity(base_rows.len());
    for (i, br) in base_rows.iter().enumerate() {
        let new_view = view_row_from_base(eng, ctx, &av, &base_cols, &scope, br)?;
        let old_view = if pair_olds {
            match &conflict_olds[i] {
                Some(old_base) => view_row_from_base(eng, ctx, &av, &base_cols, &scope, old_base)?,
                None => null_view_row.clone(),
            }
        } else {
            null_view_row.clone()
        };
        let vals =
            project_capture_row(eng, ctx, view, &av, &old_view, &new_view, false, &expanded)?;
        out_rows.push(Row::new(vals));
    }
    Ok(ExecResult::Dml {
        tag,
        columns: rcols,
        rows: out_rows,
    })
}

fn translate_on_conflict(ren: &Ren, view: &str, oc: &OnConflict) -> Result<OnConflict, ExecError> {
    let av = ren.av;
    let arbiter = match &oc.arbiter {
        ConflictArbiter::Columns(elems) => {
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                let mut e2 = e.clone();
                if let Some(bare) = arbiter_elem_bare_name(&e.key) {
                    let idx = target_col(av, view, bare)?;
                    e2.key = updatable_target(av, view, "INSERT", idx)?;
                }
                out.push(e2);
            }
            ConflictArbiter::Columns(out)
        }
        other => other.clone(),
    };
    let action = match &oc.action {
        ConflictAction::DoNothing => ConflictAction::DoNothing,
        ConflictAction::DoUpdate { sets, where_ } => {
            let mut t_sets = Vec::with_capacity(sets.len());
            for (n, e) in sets {
                let idx = target_col(av, view, n)?;
                let b = updatable_target(av, view, "UPDATE", idx)?;
                t_sets.push((b, ren.expr(e)?));
            }
            ConflictAction::DoUpdate {
                sets: t_sets,
                where_: match where_ {
                    Some(w) => Some(ren.expr(w)?),
                    None => None,
                },
            }
        }
        ConflictAction::DoSelect {
            where_,
            for_update,
            strength,
        } => ConflictAction::DoSelect {
            where_: match where_ {
                Some(w) => Some(ren.expr(w)?),
                None => None,
            },
            for_update: *for_update,
            strength: *strength,
        },
    };
    Ok(OnConflict { arbiter, action })
}

/// AND the view qual into a translated WHERE (PG19: the view qual
/// becomes part of the rewritten query's qual).
fn and_qual(where_t: Option<Expr>, av: &AutoView, target_qual: &str) -> Option<Expr> {
    let qual_t = av
        .qual
        .as_ref()
        .map(|q| rename_base_expr(q, av, target_qual, None));
    match (where_t, qual_t) {
        (Some(w), Some(q)) => Some(Expr::And(Box::new(w), Box::new(q))),
        (Some(w), None) => Some(w),
        (None, Some(q)) => Some(q),
        (None, None) => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_update_auto_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    view: &str,
    alias: &Option<String>,
    sets: &[(String, Expr)],
    from: &[FromItem],
    where_: &Option<Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
) -> Result<ExecResult, ExecError> {
    let av = match classify_view(eng, ctx, view)? {
        ViewClass::Auto(av) => av,
        ViewClass::NotAuto(r) => return Err(err_view_not_updatable(view, "UPDATE", &r)),
    };
    if !av_has_updatable(&av) {
        return Err(err_view_not_updatable(view, "UPDATE", R_NOCOLS));
    }
    // --- v2.18: PG privilege chain
    {
        let set_cols: Vec<String> = sets.iter().map(|(c, _)| c.clone()).collect();
        let mut read_cols: Vec<String> = Vec::new();
        let quals: Vec<&str> = std::iter::once(view).chain(alias.as_deref()).collect();
        if let Some(w) = where_ {
            collect_col_refs(w, &quals, &mut read_cols);
        }
        for (_, e) in sets {
            collect_col_refs(e, &quals, &mut read_cols);
        }
        for it in returning {
            if let SelectItem::Expr { expr, .. } = it {
                collect_col_refs(expr, &quals, &mut read_cols);
            }
        }
        check_view_dml_privs(
            eng,
            ctx,
            view,
            &av,
            crate::storage::PRIV_UPDATE,
            &set_cols,
            &read_cols,
        )?;
    }
    let target_qual = alias.clone().unwrap_or_else(|| av.base.clone());
    let ren = Ren {
        view,
        alias: alias.as_deref(),
        av: &av,
        target_qual: target_qual.clone(),
        unqualified: true,
    };
    // --- translate SET ---------------------------------------------
    let mut t_sets = Vec::with_capacity(sets.len());
    for (n, e) in sets {
        let idx = target_col(&av, view, n)?;
        let b = updatable_target(&av, view, "UPDATE", idx)?;
        t_sets.push((b, ren.expr(e)?));
    }
    let where_t0 = match where_ {
        Some(w) => Some(ren.expr(w)?),
        None => None,
    };
    let where_t = and_qual(where_t0, &av, &target_qual);
    // --- capture decision ------------------------------------------
    let checks = collect_checks(eng, ctx, view, &av)?;
    let capture = !checks.is_empty() || returning_needs_capture(returning);
    let base_cols = base_scope_cols(eng, ctx, &av)?;
    // Old rows are needed only when RETURNING reads them; the
    // pre-scan covers the no-FROM / no-WITH shape.
    let needs_old = returning.iter().any(|item| match item {
        SelectItem::AllOf(q) => q == "old",
        SelectItem::Expr { expr, .. } => {
            let mut n = false;
            walk_expr(expr, &mut |x| {
                if let Expr::Column { table, name } = x {
                    if matches!(table.as_deref(), Some("old")) || (table.is_none() && name == "old")
                    {
                        n = true;
                    }
                }
            });
            n
        }
        _ => false,
    });
    let old_base_rows: Vec<Row> = if capture && needs_old {
        if !from.is_empty() || !with.is_empty() {
            return Err(exec_err(
                "0A000",
                "RETURNING old.* on a view with UPDATE ... FROM / WITH is not supported yet"
                    .to_string(),
            ));
        }
        prescan_update_old_rows(eng, ctx, view, &av, &where_t)?
    } else {
        Vec::new()
    };
    let t_returning: Vec<SelectItem> = if capture {
        vec![SelectItem::All]
    } else {
        translate_returning(&ren, returning).unwrap_or_default()
    };
    let saved = suppress_privs(ctx);
    let res = exec_update(
        eng,
        ctx,
        &av.base,
        alias,
        &t_sets,
        from,
        &where_t,
        with,
        &t_returning,
    );
    ctx.privs_suppressed = saved;
    let res = res?;
    if !capture {
        return Ok(res);
    }
    let ExecResult::Dml {
        tag,
        rows: base_rows,
        ..
    } = res
    else {
        return Ok(res);
    };
    enforce_checks(eng, ctx, &av, &base_cols, &checks, &base_rows)?;
    if returning.is_empty() {
        return Ok(ExecResult::Dml {
            tag,
            columns: Vec::new(),
            rows: Vec::new(),
        });
    }
    if needs_old && old_base_rows.len() != base_rows.len() {
        return Err(exec_err(
            "XX000",
            format!(
                "view UPDATE capture pairing failed ({} old vs {} new)",
                old_base_rows.len(),
                base_rows.len()
            ),
        ));
    }
    let expanded = expand_capture_items(view, &av, returning);
    let rcols = capture_returning_cols(eng, ctx, view, &av, &expanded);
    let null_view_row = vec![Value::Null; av.view_cols.len()];
    let scope = scope_ctx(&av.base, &base_cols);
    let mut out_rows = Vec::with_capacity(base_rows.len());
    for (i, br) in base_rows.iter().enumerate() {
        let new_view = view_row_from_base(eng, ctx, &av, &base_cols, &scope, br)?;
        let old_view = if needs_old {
            view_row_from_base(eng, ctx, &av, &base_cols, &scope, &old_base_rows[i])?
        } else {
            null_view_row.clone()
        };
        let vals =
            project_capture_row(eng, ctx, view, &av, &old_view, &new_view, false, &expanded)?;
        out_rows.push(Row::new(vals));
    }
    Ok(ExecResult::Dml {
        tag,
        columns: rcols,
        rows: out_rows,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_delete_auto_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    view: &str,
    alias: &Option<String>,
    using: &[FromItem],
    where_: &Option<Expr>,
    with: &[CteDef],
    returning: &[SelectItem],
) -> Result<ExecResult, ExecError> {
    let av = match classify_view(eng, ctx, view)? {
        ViewClass::Auto(av) => av,
        ViewClass::NotAuto(r) => return Err(err_view_not_updatable(view, "DELETE", &r)),
    };
    // --- v2.18: PG privilege chain
    {
        let mut read_cols: Vec<String> = Vec::new();
        let quals: Vec<&str> = std::iter::once(view).chain(alias.as_deref()).collect();
        if let Some(w) = where_ {
            collect_col_refs(w, &quals, &mut read_cols);
        }
        for it in returning {
            if let SelectItem::Expr { expr, .. } = it {
                collect_col_refs(expr, &quals, &mut read_cols);
            }
        }
        check_view_dml_privs(
            eng,
            ctx,
            view,
            &av,
            crate::storage::PRIV_DELETE,
            &[],
            &read_cols,
        )?;
    }
    let target_qual = alias.clone().unwrap_or_else(|| av.base.clone());
    let ren = Ren {
        view,
        alias: alias.as_deref(),
        av: &av,
        target_qual: target_qual.clone(),
        unqualified: true,
    };
    let where_t0 = match where_ {
        Some(w) => Some(ren.expr(w)?),
        None => None,
    };
    let where_t = and_qual(where_t0, &av, &target_qual);
    // DELETE has no check-option enforcement in PG.
    let capture = returning_needs_capture(returning);
    let base_cols = base_scope_cols(eng, ctx, &av)?;
    let t_returning: Vec<SelectItem> = if capture {
        vec![SelectItem::All]
    } else {
        translate_returning(&ren, returning).unwrap_or_default()
    };
    let saved = suppress_privs(ctx);
    let res = exec_delete(
        eng,
        ctx,
        &av.base,
        alias,
        using,
        &where_t,
        with,
        &t_returning,
    );
    ctx.privs_suppressed = saved;
    let res = res?;
    if !capture {
        return Ok(res);
    }
    let ExecResult::Dml {
        tag,
        rows: base_rows,
        ..
    } = res
    else {
        return Ok(res);
    };
    if returning.is_empty() {
        return Ok(ExecResult::Dml {
            tag,
            columns: Vec::new(),
            rows: Vec::new(),
        });
    }
    let expanded = expand_capture_items(view, &av, returning);
    let rcols = capture_returning_cols(eng, ctx, view, &av, &expanded);
    let null_view_row = vec![Value::Null; av.view_cols.len()];
    let scope = scope_ctx(&av.base, &base_cols);
    let mut out_rows = Vec::with_capacity(base_rows.len());
    for br in &base_rows {
        // DELETE's RETURNING * yields the OLD (deleted) rows.
        let old_view = view_row_from_base(eng, ctx, &av, &base_cols, &scope, br)?;
        let vals = project_capture_row(
            eng,
            ctx,
            view,
            &av,
            &old_view,
            &null_view_row,
            true,
            &expanded,
        )?;
        out_rows.push(Row::new(vals));
    }
    Ok(ExecResult::Dml {
        tag,
        columns: rcols,
        rows: out_rows,
    })
}

/// Is `name` a non-materialized view (the auto path's jurisdiction)?
pub(crate) fn is_plain_view(eng: &Engine, ctx: &StmtCtx, name: &str) -> bool {
    eng.db
        .find_view(name, ctx.snap, ctx.own)
        .is_some_and(|v| !v.matview)
}

// ===========================================================================
// MERGE translation (called from exec_merge's view branch).
// ===========================================================================

/// Output column names of MERGE source FROM items (for
/// unqualified-reference disambiguation: PG resolves an
/// unqualified name against both target and source and errors
/// only on a true collision).
fn merge_src_cols(eng: &Engine, ctx: &StmtCtx, source: &[FromItem]) -> HashSet<String> {
    let mut out = HashSet::new();
    for it in source {
        match it {
            FromItem::Table {
                name, col_aliases, ..
            } => {
                if !col_aliases.is_empty() {
                    out.extend(col_aliases.iter().cloned());
                } else if let Some(t) =
                    eng.db
                        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
                {
                    out.extend(t.columns.iter().map(|(n, _)| n.clone()));
                } else if let Ok(cols) =
                    view_columns_of_db(eng, ctx.snap, ctx.own, ctx.session, name)
                {
                    out.extend(cols.into_iter().map(|(n, _)| n));
                }
            }
            FromItem::Values {
                rows, col_aliases, ..
            } => {
                if !col_aliases.is_empty() {
                    out.extend(col_aliases.iter().cloned());
                } else if let Some(r) = rows.first() {
                    for i in 0..r.len() {
                        out.insert(format!("column{}", i + 1));
                    }
                }
            }
            FromItem::Derived {
                sub, col_aliases, ..
            } => {
                if !col_aliases.is_empty() {
                    out.extend(col_aliases.iter().cloned());
                } else {
                    for item in &sub.items {
                        match item {
                            SelectItem::Expr {
                                expr,
                                alias: Some(a),
                            } => {
                                out.insert(a.clone());
                                let _ = expr;
                            }
                            SelectItem::Expr {
                                expr: Expr::Column { name, .. },
                                alias: None,
                            } => {
                                out.insert(name.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Qualify unqualified references that name VIEW columns (and do
/// not collide with a source column) so the qualified-only MERGE
/// rename can map them; anything else is left for the base
/// executor's own resolution, matching PG's ambiguity rules.
fn merge_qualify_unqualified(
    e: &Expr,
    av: &AutoView,
    qual: &str,
    src_cols: &HashSet<String>,
) -> Result<Expr, ExecError> {
    let one = |c: &Expr| -> Expr {
        match c {
            Expr::Column { table: None, name }
                if !src_cols.contains(name) && av.view_cols.iter().any(|(n, _)| n == name) =>
            {
                Expr::Column {
                    table: Some(qual.to_string()),
                    name: name.clone(),
                }
            }
            other => other.clone(),
        }
    };
    let root = one(e);
    map_children(&root, &mut |c| Ok(one(c)))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_merge_auto_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    view: &str,
    alias: &Option<String>,
    source: &[FromItem],
    on: &Expr,
    whens: &[MergeWhen],
    returning: &[SelectItem],
    with: &[CteDef],
) -> Result<ExecResult, ExecError> {
    // v2.18: a MERGE whose every action is DO NOTHING writes
    // nothing, so PG allows it on ANY view (updatability is not
    // required). Validate the join (refs in ON / source resolve
    // against the view) and count processed source rows.
    if whens
        .iter()
        .all(|w| matches!(w.action, MergeAction::DoNothing))
    {
        let mut sel = crate::sql::empty_select();
        sel.with = with.to_vec();
        sel.items = vec![SelectItem::Expr {
            expr: Expr::Literal(Literal::Int(1)),
            alias: None,
        }];
        let mut from: Vec<FromItem> = vec![FromItem::Table {
            name: view.to_string(),
            alias: alias.clone(),
            col_aliases: Vec::new(),
            only: false,
        }];
        from.extend(source.iter().cloned());
        sel.from = from;
        sel.where_ = Some(on.clone());
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: true,
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
        let out = run_select(&mut q, &sel, &[])?;
        return Ok(ExecResult::Command {
            tag: format!("MERGE {}", out.rows.len()),
        });
    }
    let av = match classify_view(eng, ctx, view)? {
        ViewClass::Auto(av) => av,
        ViewClass::NotAuto(r) => {
            // PG reports the first action (in written order) that
            // lacks a rewrite, with that action's message.
            for w in whens {
                let ev = match &w.action {
                    MergeAction::Insert { .. } => "INSERT",
                    MergeAction::Update { .. } => "UPDATE",
                    MergeAction::Delete => "DELETE",
                    MergeAction::DoNothing => continue,
                };
                return Err(err_view_not_updatable(view, ev, &r));
            }
            return Err(err_view_not_updatable(view, "INSERT", &r));
        }
    };
    let has_ins_upd = whens.iter().any(|w| {
        matches!(
            w.action,
            MergeAction::Insert { .. } | MergeAction::Update { .. }
        )
    });
    if has_ins_upd && !av_has_updatable(&av) {
        return Err(err_view_not_updatable(view, "INSERT", R_NOCOLS));
    }
    let target_qual = alias.clone().unwrap_or_else(|| av.base.clone());
    // ON / WHEN / SET expressions reference both target and source:
    // only view-QUALIFIED references are renamed (an unqualified
    // name may belong to the source).
    let ren_q = Ren {
        view,
        alias: alias.as_deref(),
        av: &av,
        target_qual: target_qual.clone(),
        unqualified: false,
    };
    // Unqualified refs: qualify the ones that name view columns
    // without colliding with source columns (PG resolution).
    let src_cols = merge_src_cols(eng, ctx, source);
    let qual_name = alias.clone().unwrap_or_else(|| view.to_string());
    let on_q = merge_qualify_unqualified(on, &av, &qual_name, &src_cols)?;
    let t_on = ren_q.expr(&on_q)?;
    // The target row set is the VIEW's rows: AND the view qual
    // (renamed to base space) into the join condition so a WHEN
    // clause can never match a base row outside the view.
    let t_on = match &av.qual {
        Some(q) => {
            // The view qual references target columns only (it is
            // part of the view definition), so qualify its refs
            // with an empty source-column set: no source collision
            // is possible.
            let qual_q =
                merge_qualify_unqualified(q, &av, &qual_name, &std::collections::HashSet::new())?;
            Expr::And(Box::new(t_on), Box::new(ren_q.expr(&qual_q)?))
        }
        None => t_on,
    };
    let mut t_whens = Vec::with_capacity(whens.len());
    for w in whens {
        let t_cond = match &w.condition {
            Some(c) => {
                let cq = merge_qualify_unqualified(c, &av, &qual_name, &src_cols)?;
                Some(ren_q.expr(&cq)?)
            }
            None => None,
        };
        let t_action = match &w.action {
            MergeAction::Update { sets } => {
                let mut t_sets = Vec::with_capacity(sets.len());
                for (n, e) in sets {
                    let idx = target_col(&av, view, n)?;
                    let b = updatable_target(&av, view, "UPDATE", idx)?;
                    let eq = merge_qualify_unqualified(e, &av, &qual_name, &src_cols)?;
                    t_sets.push((b, ren_q.expr(&eq)?));
                }
                MergeAction::Update { sets: t_sets }
            }
            MergeAction::Insert { columns, values } => {
                // v2.18: view column defaults fill omitted columns
                // (and the whole row for INSERT DEFAULT VALUES).
                let vdefs = view_defaults_of(eng, ctx, view, av.view_cols.len());
                let (columns, values) = if vdefs.iter().all(|d| d.is_none()) {
                    (columns, values)
                } else {
                    match (columns, values) {
                        (Some(names), Some(vals)) => {
                            let mut names2 = names.clone();
                            let mut vals2 = vals.clone();
                            for (i, (vn, _)) in av.view_cols.iter().enumerate() {
                                if names.iter().any(|n| n == vn) {
                                    continue;
                                }
                                if let Some(d) = &vdefs[i] {
                                    names2.push(vn.clone());
                                    vals2.push(default_to_expr(d));
                                }
                            }
                            (&Some(names2), &Some(vals2))
                        }
                        (Some(names), None) => {
                            // INSERT (cols) DEFAULT VALUES: only
                            // columns WITH view defaults are named;
                            // the rest take base defaults.
                            let mut names2 = Vec::new();
                            let mut vals2 = Vec::new();
                            for (i, (vn, _)) in av.view_cols.iter().enumerate() {
                                let listed = names.iter().any(|n| n == vn);
                                if let Some(d) = &vdefs[i] {
                                    if listed || !names.iter().any(|n| n == vn) {
                                        names2.push(vn.clone());
                                        vals2.push(default_to_expr(d));
                                    }
                                }
                            }
                            (&Some(names2), &Some(vals2))
                        }
                        (None, None) => {
                            let mut names2 = Vec::new();
                            let mut vals2 = Vec::new();
                            for (i, (vn, _)) in av.view_cols.iter().enumerate() {
                                if let Some(d) = &vdefs[i] {
                                    names2.push(vn.clone());
                                    vals2.push(default_to_expr(d));
                                }
                            }
                            if names2.is_empty() {
                                (columns, values)
                            } else {
                                (&Some(names2), &Some(vals2))
                            }
                        }
                        other => other,
                    }
                };
                let t_cols = match columns {
                    Some(names) => {
                        let mut out = Vec::with_capacity(names.len());
                        for n in names {
                            let idx = target_col(&av, view, n)?;
                            out.push(updatable_target(&av, view, "MERGE", idx)?);
                        }
                        Some(out)
                    }
                    None => {
                        // Positional MERGE insert targets the
                        // view's updatable columns only (PG19).
                        Some(
                            av.cols
                                .iter()
                                .filter_map(|c| match c {
                                    VCol::Base(b) => Some(b.clone()),
                                    _ => None,
                                })
                                .collect(),
                        )
                    }
                };
                MergeAction::Insert {
                    columns: t_cols,
                    values: values.clone(),
                }
            }
            MergeAction::Delete => MergeAction::Delete,
            MergeAction::DoNothing => MergeAction::DoNothing,
        };
        t_whens.push(MergeWhen {
            kind: w.kind,
            condition: t_cond,
            action: t_action,
        });
    }
    // --- v2.18: PG privilege chain, per action kind present
    {
        let quals: Vec<&str> = std::iter::once(view).chain(alias.as_deref()).collect();
        let mut read_cols: Vec<String> = Vec::new();
        collect_col_refs(on, &quals, &mut read_cols);
        let mut ins_cols: Vec<String> = Vec::new();
        let mut has_insert = false;
        let mut upd_cols: Vec<String> = Vec::new();
        let mut has_delete = false;
        for w in whens {
            if let Some(c) = &w.condition {
                collect_col_refs(c, &quals, &mut read_cols);
            }
            match &w.action {
                MergeAction::Insert { columns, values } => {
                    has_insert = true;
                    ins_cols = match columns {
                        Some(cs) => cs.clone(),
                        None => av
                            .view_cols
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| matches!(av.cols[*i], VCol::Base(_)))
                            .map(|(_, (n, _))| n.clone())
                            .collect(),
                    };
                    if let Some(vals) = values {
                        for e in vals {
                            collect_col_refs(e, &quals, &mut read_cols);
                        }
                    }
                }
                MergeAction::Update { sets } => {
                    for (col, e) in sets {
                        if !upd_cols.contains(col) {
                            upd_cols.push(col.clone());
                        }
                        collect_col_refs(e, &quals, &mut read_cols);
                    }
                }
                MergeAction::Delete => has_delete = true,
                MergeAction::DoNothing => {}
            }
        }
        if has_insert {
            check_view_dml_privs(
                eng,
                ctx,
                view,
                &av,
                crate::storage::PRIV_INSERT,
                &ins_cols,
                &read_cols,
            )?;
        }
        if !upd_cols.is_empty() {
            check_view_dml_privs(
                eng,
                ctx,
                view,
                &av,
                crate::storage::PRIV_UPDATE,
                &upd_cols,
                &read_cols,
            )?;
        }
        if has_delete {
            check_view_dml_privs(
                eng,
                ctx,
                view,
                &av,
                crate::storage::PRIV_DELETE,
                &[],
                &read_cols,
            )?;
        }
    }
    // RETURNING references the target only: unqualified renames on.
    let ren_r = Ren {
        view,
        alias: alias.as_deref(),
        av: &av,
        target_qual,
        unqualified: true,
    };
    // Star forms expand over the VIEW's columns (not the base's):
    // expression columns are substituted over the target/old/new
    // scope, exactly as PG's rewrite produces them.
    let expand_scope = |scope_qual: &str, oldnew: Option<&str>| -> Vec<SelectItem> {
        let mut out = Vec::with_capacity(av.view_cols.len());
        for (i, (vn, _)) in av.view_cols.iter().enumerate() {
            let expr = match &av.cols[i] {
                VCol::Base(b) => Expr::Column {
                    table: Some(scope_qual.to_string()),
                    name: b.clone(),
                },
                VCol::Subst(se) => rename_base_expr(se, &av, &ren_r.target_qual, oldnew),
                VCol::Blocked(Some(bn), _) => Expr::Column {
                    table: Some(scope_qual.to_string()),
                    name: bn.clone(),
                },
                VCol::Blocked(None, _) => Expr::Literal(Literal::Null),
            };
            // The result column carries the VIEW column's name.
            out.push(SelectItem::Expr {
                expr,
                alias: Some(vn.clone()),
            });
        }
        out
    };
    let mut t_returning = Vec::with_capacity(returning.len());
    for item in returning {
        match item {
            SelectItem::Expr { expr, alias } => t_returning.push(SelectItem::Expr {
                expr: ren_r.expr(expr)?,
                alias: pin_col_name(expr, alias),
            }),
            SelectItem::All => t_returning.extend(expand_scope(&ren_r.target_qual.clone(), None)),
            SelectItem::AllOf(q) if ren_r.is_view_qual(q) => {
                t_returning.extend(expand_scope(&ren_r.target_qual.clone(), None))
            }
            SelectItem::AllOf(q) if q == "old" || q == "new" => {
                let oq = q.clone();
                t_returning.extend(expand_scope(&oq, Some(&oq)))
            }
            other => t_returning.push(other.clone()),
        }
    }
    // WITH CHECK OPTION: when the user asked for no RETURNING,
    // capture merge_action() + the final base row per action and
    // enforce the collected quals on INSERT/UPDATE results (PG
    // checks the post-action row; DELETEs have no new row).
    let checks = collect_checks(eng, ctx, view, &av)?;
    let capture_merge = !checks.is_empty() && returning.is_empty();
    let base_cols = if capture_merge {
        base_scope_cols(eng, ctx, &av)?
    } else {
        Vec::new()
    };
    if capture_merge {
        let mut cap = vec![SelectItem::Expr {
            expr: Expr::Func {
                name: "merge_action".to_string(),
                args: Vec::new(),
            },
            alias: None,
        }];
        for (bn, _) in &base_cols {
            cap.push(SelectItem::Expr {
                expr: Expr::Column {
                    table: Some(ren_r.target_qual.clone()),
                    name: bn.clone(),
                },
                alias: Some(bn.clone()),
            });
        }
        t_returning = cap;
    }
    let saved = suppress_privs(ctx);
    let res = exec_merge(
        eng,
        ctx,
        &av.base,
        alias,
        source,
        &t_on,
        &t_whens,
        &t_returning,
        with,
    );
    ctx.privs_suppressed = saved;
    let res = res?;
    if !capture_merge {
        return Ok(res);
    }
    let ExecResult::Dml {
        tag,
        rows: cap_rows,
        ..
    } = res
    else {
        return Ok(res);
    };
    for row in &cap_rows {
        let action = row.first().and_then(|v| v.to_text());
        if action.as_deref() == Some("DELETE") || row.len() != base_cols.len() + 1 {
            continue;
        }
        let final_row = Row::new(row[1..].to_vec());
        enforce_checks(eng, ctx, &av, &base_cols, &checks, &[final_row])?;
    }
    Ok(ExecResult::Dml {
        tag,
        columns: Vec::new(),
        rows: Vec::new(),
    })
}
