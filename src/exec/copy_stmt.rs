// v1.78 mechanical split: moved verbatim from src/exec.rs (27424-27569).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

/// v0.10: COPY support — data layer for the server's COPY protocol.
// ---------------------------------------------------------------------------

/// v0.10: resolve the column count for a COPY target (explicit column
/// list, or the table's full width).
pub fn copy_ncols(
    eng: &Engine,
    snap: &Snapshot,
    own: u64,
    session: u64,
    table: &str,
    columns: &Option<Vec<String>>,
) -> Result<usize, ExecError> {
    if let Some(cols) = columns {
        // Validate the names while we're at it.
        let t = eng
            .db
            .find_table(table, snap, &[own], session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        let meta = TableMeta::of(t);
        for n in cols {
            if !meta.columns.iter().any(|(c, _)| c == n) {
                return Err(exec_err(
                    "42703",
                    format!("column \"{}\" of relation \"{}\" does not exist", n, table),
                ));
            }
        }
        Ok(cols.len())
    } else {
        let t = eng
            .db
            .find_table(table, snap, &[own], session)
            .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", table)))?;
        Ok(TableMeta::of(t).columns.len())
    }
}

/// v0.10: COPY TO STDOUT — run `SELECT <cols> FROM <table>` and return
/// the (name, type) columns and the visible rows.
pub fn copy_to_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    columns: &Option<Vec<String>>,
) -> Result<(Vec<(String, ColType)>, Vec<Row>), ExecError> {
    // Validate the table/columns first (clean 42P01/42703 errors).
    copy_ncols(eng, ctx.snap, ctx.own, ctx.session, table, columns)?;
    let col_list = match columns {
        Some(cols) => cols
            .iter()
            .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", "),
        None => "*".to_string(),
    };
    let sql = format!(
        "SELECT {} FROM \"{}\"",
        col_list,
        table.replace('"', "\"\"")
    );
    let stmt = crate::sql::parse_statement(&sql).map_err(|e| ExecError {
        detail: None,
        code: e.code,
        message: e.message,
    })?;
    match execute(eng, ctx, &stmt)? {
        ExecResult::Select { columns, rows } => Ok((columns, rows)),
        _ => Err(ExecError {
            detail: None,
            code: "XX000",
            message: "internal error: COPY TO did not return rows".to_string(),
        }),
    }
}

/// v0.10: COPY FROM STDIN — insert pre-parsed rows. Returns the row count.
pub fn copy_from_rows(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    columns: &Option<Vec<String>>,
    rows: Vec<Vec<crate::copy::CopyField>>,
) -> Result<u64, ExecError> {
    use crate::copy::CopyField;
    use crate::sql::{InsertTarget, Literal};
    let insert_rows: Vec<Vec<InsertValue>> = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|f| match f {
                    CopyField::Text(s) => InsertValue::Lit(Literal::Text(s.into())),
                    CopyField::Null => InsertValue::Lit(Literal::Null),
                })
                .collect()
        })
        .collect();
    let n = insert_rows.len() as u64;
    // v0.84: COPY's column list is plain names (PG has no indirection
    // in COPY) — wrap as whole-column InsertTargets.
    let targets: Option<Vec<InsertTarget>> = columns.as_ref().map(|cs| {
        cs.iter()
            .map(|name| InsertTarget {
                name: name.clone(),
                indirection: Vec::new(),
            })
            .collect()
    });
    // Reuse the full INSERT path: coercion, defaults, constraints,
    // unique indexes, foreign keys, WAL — atomically.
    let _ = exec_insert(
        eng,
        ctx,
        table,
        &targets,
        &insert_rows,
        &None,
        &[],
        &None,
        &[],
    )?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// v0.10: Window functions
// ---------------------------------------------------------------------------

/// v0.10: an executable window specification, deduplicated across the
/// query level. `wid` indexes the precomputed values in the query
/// context (`Q.wctx`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExecWindow {
    pub(crate) func: WindowFunc,
    pub(crate) args: Vec<Expr>,
    pub(crate) distinct: bool,
    pub(crate) partition_by: Vec<Expr>,
    pub(crate) order_by: Vec<OrderTerm>,
    pub(crate) frame: WindowFrame,
    /// v1.29: FILTER on a windowed aggregate (PG19 `wfunc->aggfilter`).
    pub(crate) filter: Option<Expr>,
    /// v1.31: PG19 `opt_window_exclusion_clause`; participates in
    /// window identity like `filter`.
    pub(crate) exclusion: FrameExclusion,
}
