// v1.78 mechanical split: moved verbatim from src/exec.rs (32839-33132).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// Projection (non-aggregated queries)
// ---------------------------------------------------------------------------

/// Project one joined row to output cells + provenance. `out_ncols` is the
/// query's exact output column count (already computed by the caller via
/// `describe_select`, one per `RowDescription` field), used to size the
/// cell buffer up front for the explicit-item path below.
pub(crate) fn project_row(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
    row: QRow,
    out_ncols: usize,
    // v0.52: for DISTINCT ON's initial projection, SRFs become NULL
    // placeholders (the real expansion is deferred until after the
    // first-row-per-group filter).
    srf_placeholder: bool,
) -> Result<(Row, Vec<RowProv>), ExecError> {
    // Fast path: plain `SELECT *` moves the row through untouched — no
    // scope chain, no per-row allocation at all. Disabled when the schema
    // carries hidden columns (v0.23 merged USING/NATURAL keys): the hidden
    // originals must be filtered out of the visible row.
    if stmt.items.len() == 1
        && matches!(stmt.items[0], SelectItem::All)
        && !schema.iter().any(|c| c.hidden)
    {
        return Ok((row.cells, row.prov));
    }
    // Only expression items need a scope chain; star-only shapes don't
    // pay for one. `row` is owned, so provenance moves without cloning.
    let scopes = projection_scopes(outer, stmt, schema, &row);
    let mut cells = Vec::with_capacity(out_ncols);
    for item in &stmt.items {
        cells.extend(project_item_values(
            q,
            &scopes,
            schema,
            &row,
            item,
            false,
            srf_placeholder,
        )?);
    }
    Ok((Row::new(cells), row.prov))
}

/// v0.32: build the SELECT-list scope chain for one input row.
pub(crate) fn projection_scopes<'a>(
    outer: &'a [Scope<'a>],
    stmt: &SelectStmt,
    schema: &'a [QCol],
    row: &'a QRow,
) -> Vec<Scope<'a>> {
    if stmt
        .items
        .iter()
        .any(|i| matches!(i, SelectItem::Expr { .. }))
    {
        let mut s: Vec<Scope> = Vec::with_capacity(outer.len() + 1);
        s.extend_from_slice(outer);
        s.push(Scope {
            schema,
            row: &row.cells,
            prov: Some(&row.prov),
        });
        s
    } else {
        Vec::new()
    }
}

/// v0.32: evaluate one SELECT-list item to its column values. With
/// `expand_srf`, a top-level set-returning function call produces one value
/// per output row instead of a single scalar (PG 19 SRF-in-targetlist).
/// v0.52: with `srf_placeholder`, an SRF evaluates to NULL — used for
/// DISTINCT ON's initial projection, where the real SRF expansion is
/// deferred until after the first-row-per-group filter (the placeholder
/// is never observed; the filter keys and sort don't touch it).
pub(crate) fn project_item_values(
    q: &mut Q,
    scopes: &[Scope],
    schema: &[QCol],
    row: &QRow,
    item: &SelectItem,
    expand_srf: bool,
    srf_placeholder: bool,
) -> Result<Vec<Value>, ExecError> {
    match item {
        // v0.23: hidden columns are skipped by `*` (they stay
        // reachable via qualified refs and `qual.*`).
        SelectItem::All => Ok(schema
            .iter()
            .zip(row.cells.iter())
            .filter(|(c, _)| !c.hidden)
            .map(|(_, v)| v.clone())
            .collect()),
        SelectItem::AllOf(qual) => {
            // v0.23: expand in the qualifier's source-column order.
            let idx = qual_star_order(schema, qual);
            if idx.is_empty() {
                return Err(exec_err(
                    "42P01",
                    format!("missing FROM-clause entry for table \"{qual}\""),
                ));
            }
            Ok(idx.into_iter().map(|i| row.cells[i].clone()).collect())
        }
        SelectItem::Expr { expr, .. } => {
            if expand_srf {
                if let Expr::Func { name, args } = expr {
                    if is_srf(q.eng, name) {
                        let mut vals = Vec::with_capacity(args.len());
                        for a in args {
                            vals.push(eval_expr(q, scopes, a)?);
                        }
                        if is_builtin_srf(name) {
                            return eval_srf_vals(name, &vals);
                        }
                        // v1.09: user-defined RETURNS SETOF function.
                        return eval_user_srf_vals(q, scopes, name, &vals);
                    }
                }
            }
            // v0.52: DISTINCT ON defers SRF expansion; the initial
            // projection uses a NULL placeholder. v1.28: nested SRF calls
            // are placeholders too (their expansion is equally deferred).
            if srf_placeholder && expr_has_srf(q.eng, expr) {
                return Ok(vec![Value::Null]);
            }
            Ok(vec![eval_expr(q, scopes, expr)?])
        }
    }
}

/// v0.32: project one input row with SRF-in-targetlist expansion (PG 19).
/// The row fans out to max(SRF widths) output rows; SRFs narrower than the
/// max pad with NULL; an all-empty SRF set yields zero rows. Plain columns
/// repeat their value on every fanned-out row.
/// v1.28: PG19 ProjectSet on the plain path — SRF calls nested anywhere in
/// the target list (not just top-level items) fan out, mirroring
/// `project_group_expanded` (`nodeProjectSet.c` `ExecProjectSRF`: the
/// planner lifts nested SRFs via `split_pathtarget_at_srfs`). Each fanned
/// row is returned with its SRF bindings so callers can rebind them for
/// ORDER BY terms that name an SRF call textually (PG evaluates ORDER BY
/// after the ProjectSet).
pub(crate) fn project_row_expanded(
    q: &mut Q,
    outer: &[Scope],
    stmt: &SelectStmt,
    schema: &[QCol],
    row: QRow,
    out_ncols: usize,
    nested: bool,
) -> Result<Vec<(Row, Vec<RowProv>, Vec<(Expr, Value)>)>, ExecError> {
    let scopes = projection_scopes(outer, stmt, schema, &row);
    if !nested {
        // Fast path: every SRF is a whole top-level select item.
        // (values, pad_with_null): SRF columns pad, plain columns repeat.
        let mut cols: Vec<(Vec<Value>, bool)> = Vec::with_capacity(out_ncols);
        for item in &stmt.items {
            let vals = project_item_values(q, &scopes, schema, &row, item, true, false)?;
            let srf_col = matches!(
                item,
                SelectItem::Expr {
                    expr: Expr::Func { name, .. },
                    ..
                } if is_srf(q.eng, name)
            );
            if srf_col {
                cols.push((vals, true));
            } else {
                for v in vals {
                    cols.push((vec![v], false));
                }
            }
        }
        // v0.47: the fan-out width comes from the SRF columns only (PG19
        // nodeProjectSet.c `ExecProjectSRF`: a row is produced only when at
        // least one SRF yields a value — `hasresult`). Plain columns repeat
        // and never create rows on their own, so an all-empty SRF set yields
        // zero rows even next to plain columns.
        let width = cols
            .iter()
            .filter(|(_, pad)| *pad)
            .map(|(v, _)| v.len())
            .max()
            .unwrap_or(0);
        let mut out = Vec::with_capacity(width);
        for k in 0..width {
            let mut cells = Vec::with_capacity(cols.len());
            for (vals, pad) in &cols {
                if *pad {
                    cells.push(vals.get(k).cloned().unwrap_or(Value::Null));
                } else {
                    cells.push(vals[0].clone());
                }
            }
            out.push((Row::new(cells), row.prov.clone(), Vec::new()));
        }
        return Ok(out);
    }
    // General path: at least one SRF is nested inside a larger expression
    // (this shape raised 42883 before v1.28). Collect every SRF occurrence
    // across the target list — a whole top-level SRF item is one slot, any
    // other item is scanned for nested calls — then fan out exactly like
    // the grouped path: SRF args evaluate per input row, multiple SRFs zip
    // with NULL padding, an all-empty set drops the row.
    let saved_srf = std::mem::take(&mut q.srf_vals);
    let mut slots: Vec<SrfSlot> = Vec::new();
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            if let Expr::Func { name, args } = expr {
                if is_srf(q.eng, name) {
                    slots.push(SrfSlot {
                        name: name.clone(),
                        args: args.clone(),
                        call: expr.clone(),
                    });
                    continue;
                }
            }
            collect_target_srfs(q.eng, expr, &[], &mut slots);
        }
    }
    let mut fans: Vec<Vec<Value>> = Vec::with_capacity(slots.len());
    for slot in &slots {
        let mut avals = Vec::with_capacity(slot.args.len());
        for a in &slot.args {
            avals.push(eval_expr(q, &scopes, a)?);
        }
        fans.push(if is_builtin_srf(&slot.name) {
            eval_srf_vals(&slot.name, &avals)?
        } else {
            eval_user_srf_vals(q, &scopes, &slot.name, &avals)?
        });
    }
    // The fan-out width comes from the SRF calls only — a row is produced
    // only when at least one SRF yields a value (PG19 `hasresult`). Plain
    // columns repeat and never create rows on their own.
    let width = fans.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = Vec::with_capacity(width);
    for k in 0..width {
        // Bind this row's SRF values (NULL-padded past exhaustion); each
        // select item then evaluates with nested SRF calls intercepted to
        // their fanned values (the v1.28 `eval_expr` Func arm).
        q.srf_vals = slots
            .iter()
            .zip(&fans)
            .map(|(s, fan)| (s.call.clone(), fan.get(k).cloned().unwrap_or(Value::Null)))
            .collect();
        let mut cells = Vec::with_capacity(out_ncols);
        for item in &stmt.items {
            cells.extend(project_item_values(
                q, &scopes, schema, &row, item, false, false,
            )?);
        }
        out.push((Row::new(cells), row.prov.clone(), q.srf_vals.clone()));
    }
    q.srf_vals = saved_srf;
    Ok(out)
}

/// v1.28: does this expression contain any SRF call at all (a whole
/// top-level call or one nested inside a larger expression)? Used for the
/// DISTINCT ON initial-projection placeholder, which NULLs SRF-bearing
/// items until the deferred expansion runs.
pub(crate) fn expr_has_srf(eng: &Engine, e: &Expr) -> bool {
    let mut slots = Vec::new();
    collect_target_srfs(eng, e, &[], &mut slots);
    !slots.is_empty()
}

/// v1.28: does this select item contain an SRF call nested inside a larger
/// expression (as opposed to a whole top-level SRF item, which takes the
/// v0.32 fast path)? Drives the general ProjectSet path in
/// `project_row_expanded` and the `expand_srf` trigger.
pub(crate) fn item_has_nested_srf(eng: &Engine, item: &SelectItem) -> bool {
    match item {
        SelectItem::Expr { expr, .. } => {
            if let Expr::Func { name, .. } = expr {
                if is_srf(eng, name) {
                    return false;
                }
            }
            let mut slots = Vec::new();
            collect_target_srfs(eng, expr, &[], &mut slots);
            !slots.is_empty()
        }
        _ => false,
    }
}
