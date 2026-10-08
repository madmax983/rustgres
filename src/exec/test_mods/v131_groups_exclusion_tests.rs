// ---------------------------------------------------------------------------
// v1.31: PG19 GROUPS frame mode + frame exclusion — unit tests
// ---------------------------------------------------------------------------
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    // Preserve the parser's SQLSTATE (e.g. 42601), like the main
    // test harness does.
    let stmt = crate::sql::parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        snap: &snap,
        own: 9,
        write_xid: 9,
        all_xids: vec![9],
        level: IsolationLevel::ReadCommitted,
        writes: &mut writes,
        session: 0,
        role: "postgres",
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    execute(eng, &mut ctx, &stmt)
}

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } | ExecResult::Explain { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn err_of(eng: &mut Engine, sql: &str) -> ExecError {
    run(eng, sql).unwrap_err()
}

/// v1.31: GROUPS frame mode + frame exclusion (PG19 window
/// `opt_frame_clause`: `GROUPS frame_extent
/// opt_window_exclusion_clause`). Peer groups by g: {10,20},
/// {30,40,50}.
fn groups_engine() -> Engine {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE gvals(g int, v int)").unwrap();
    run(
        &mut eng,
        "INSERT INTO gvals VALUES (1,10),(1,20),(2,30),(2,40),(2,50)",
    )
    .unwrap();
    eng
}

fn groups_q(eng: &mut Engine, sel: &str) -> Vec<Vec<String>> {
    rows_of(run(eng, &format!("SELECT v, {sel} FROM gvals ORDER BY v")).unwrap())
}

fn vv(pairs: &[(i32, &str)]) -> Vec<Vec<String>> {
    pairs
        .iter()
        .map(|(v, s)| vec![v.to_string(), s.to_string()])
        .collect()
}

#[test]
fn v131_groups_current_row_is_peer_group() {
    let mut eng = groups_engine();
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN CURRENT ROW AND CURRENT ROW)"
        ),
        vv(&[
            (10, "30"),
            (20, "30"),
            (30, "120"),
            (40, "120"),
            (50, "120")
        ])
    );
    // 0 PRECEDING .. 0 FOLLOWING hits the same single group.
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN 0 PRECEDING AND 0 FOLLOWING)"
        ),
        vv(&[
            (10, "30"),
            (20, "30"),
            (30, "120"),
            (40, "120"),
            (50, "120")
        ])
    );
}

#[test]
fn v131_groups_offset_counts_groups_not_rows() {
    let mut eng = groups_engine();
    // ±1 group from anywhere reaches both groups: whole partition.
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING)"
        ),
        vv(&[
            (10, "150"),
            (20, "150"),
            (30, "150"),
            (40, "150"),
            (50, "150")
        ])
    );
    // 1 PRECEDING .. CURRENT ROW: group 0 sees only itself (clamped),
    // group 1 reaches back to group 0.
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW)"
        ),
        vv(&[
            (10, "30"),
            (20, "30"),
            (30, "150"),
            (40, "150"),
            (50, "150")
        ])
    );
}

#[test]
fn v131_groups_requires_order_by() {
    let mut eng = groups_engine();
    // PG19 parse_clause.c, verbatim: 42P20.
    let err = err_of(
        &mut eng,
        "SELECT sum(v) OVER (GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM gvals",
    );
    assert_eq!(err.code, "42P20");
    let err = err_of(
        &mut eng,
        "SELECT sum(v) OVER (PARTITION BY g GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM gvals",
    );
    assert_eq!(err.code, "42P20");
}

#[test]
fn v131_groups_empty_frame() {
    let mut eng = groups_engine();
    // 2 FOLLOWING from group 0 / 1 runs past the last group: empty.
    assert_eq!(
        groups_q(
            &mut eng,
            "count(*) OVER (ORDER BY g GROUPS BETWEEN 2 FOLLOWING AND 3 FOLLOWING)"
        ),
        vv(&[(10, "0"), (20, "0"), (30, "0"), (40, "0"), (50, "0")])
    );
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN 2 FOLLOWING AND 3 FOLLOWING)"
        ),
        vv(&[
            (10, "NULL"),
            (20, "NULL"),
            (30, "NULL"),
            (40, "NULL"),
            (50, "NULL")
        ])
    );
}

#[test]
fn v131_exclude_current_row() {
    let mut eng = groups_engine();
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN UNBOUNDED PRECEDING \
                 AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW)"
        ),
        vv(&[
            (10, "140"),
            (20, "130"),
            (30, "120"),
            (40, "110"),
            (50, "100")
        ])
    );
}

#[test]
fn v131_exclude_group() {
    let mut eng = groups_engine();
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN UNBOUNDED PRECEDING \
                 AND UNBOUNDED FOLLOWING EXCLUDE GROUP)"
        ),
        vv(&[(10, "120"), (20, "120"), (30, "30"), (40, "30"), (50, "30")])
    );
}

#[test]
fn v131_exclude_ties() {
    let mut eng = groups_engine();
    // Frame is the whole partition (±1 group); each row drops its
    // peers but keeps itself.
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES)"
        ),
        vv(&[(10, "130"), (20, "140"), (30, "60"), (40, "70"), (50, "80")])
    );
    // Explicit frame (PG, like rustgres, rejects EXCLUDE without a
    // frame clause): peers of the current row inside the ROWS frame
    // are dropped, nothing else.
    assert_eq!(
        groups_q(
            &mut eng,
            "count(*) OVER (ORDER BY g ROWS BETWEEN UNBOUNDED PRECEDING \
                 AND CURRENT ROW EXCLUDE TIES)"
        ),
        vv(&[(10, "1"), (20, "1"), (30, "3"), (40, "3"), (50, "3")])
    );
}

#[test]
fn v131_exclude_no_others_is_default() {
    let mut eng = groups_engine();
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE NO OTHERS)"
        ),
        vv(&[
            (10, "150"),
            (20, "150"),
            (30, "150"),
            (40, "150"),
            (50, "150")
        ])
    );
}

#[test]
fn v131_exclude_without_frame_is_syntax_error() {
    let mut eng = groups_engine();
    // PG19: exclusion is only valid after a frame extent.
    let err = err_of(
        &mut eng,
        "SELECT sum(v) OVER (ORDER BY v EXCLUDE TIES) FROM gvals",
    );
    assert_eq!(err.code, "42601");
}

#[test]
fn v131_first_last_nth_value_skip_excluded() {
    let mut eng = groups_engine();
    // PG19 WindowGetFuncArgInFrame HEAD adjustment: first
    // non-excluded row of the frame.
    assert_eq!(
        groups_q(
            &mut eng,
            "first_value(v) OVER (ORDER BY g GROUPS BETWEEN 1 PRECEDING \
                 AND 1 FOLLOWING EXCLUDE TIES)"
        ),
        vv(&[(10, "10"), (20, "20"), (30, "10"), (40, "10"), (50, "10")])
    );
    // TAIL adjustment: last non-excluded row (the final row excludes
    // itself, so it sees v=40).
    assert_eq!(
        groups_q(
            &mut eng,
            "last_value(v) OVER (ORDER BY g GROUPS BETWEEN UNBOUNDED PRECEDING \
                 AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW)"
        ),
        vv(&[(10, "50"), (20, "50"), (30, "50"), (40, "50"), (50, "40")])
    );
    // nth non-excluded row.
    assert_eq!(
        groups_q(
            &mut eng,
            "nth_value(v, 2) OVER (ORDER BY g GROUPS BETWEEN UNBOUNDED PRECEDING \
                 AND UNBOUNDED FOLLOWING EXCLUDE GROUP)"
        ),
        vv(&[(10, "40"), (20, "40"), (30, "20"), (40, "20"), (50, "20")])
    );
}

#[test]
fn v131_lead_lag_ignore_exclusion() {
    let mut eng = groups_engine();
    // PG19: lead/lag are physical navigation (WINDOW_SEEK_CURRENT);
    // exclusion does not move them.
    assert_eq!(
        groups_q(
            &mut eng,
            "lead(v) OVER (ORDER BY g GROUPS BETWEEN 1 PRECEDING \
                 AND 1 FOLLOWING EXCLUDE TIES)"
        ),
        vv(&[(10, "20"), (20, "30"), (30, "40"), (40, "50"), (50, "NULL")])
    );
}

#[test]
fn v131_exclude_composes_with_filter() {
    let mut eng = groups_engine();
    // Exclusion drops the current row first; FILTER then drops v <= 15.
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) FILTER (WHERE v > 15) OVER (ORDER BY g GROUPS BETWEEN \
                 UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW)"
        ),
        vv(&[
            (10, "140"),
            (20, "120"),
            (30, "110"),
            (40, "100"),
            (50, "90")
        ])
    );
}

#[test]
fn v131_groups_partitioned() {
    let mut eng = groups_engine();
    // Peer groups are numbered per partition. ORDER BY v makes every
    // row its own group, so ±1 group is ±1 row here.
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (PARTITION BY CASE WHEN g = 1 THEN 0 ELSE 1 END \
                 ORDER BY v GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING)"
        ),
        vv(&[(10, "30"), (20, "30"), (30, "70"), (40, "120"), (50, "90")])
    );
}

#[test]
fn v131_groups_null_order_key_is_own_group() {
    let mut eng = groups_engine();
    run(&mut eng, "INSERT INTO gvals VALUES (NULL, 5)").unwrap();
    // NULLs are peers of each other (PG: nulls are peers); the single
    // NULL row is its own group.
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ORDER BY g GROUPS BETWEEN CURRENT ROW AND CURRENT ROW)"
        ),
        vv(&[
            (5, "5"),
            (10, "30"),
            (20, "30"),
            (30, "120"),
            (40, "120"),
            (50, "120")
        ])
    );
}

#[test]
fn v131_groups_keywords_case_insensitive() {
    let mut eng = groups_engine();
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) over (order by g groups between 1 preceding and 1 following exclude ties)"
        ),
        vv(&[(10, "130"), (20, "140"), (30, "60"), (40, "70"), (50, "80")])
    );
}

#[test]
fn v131_exclude_without_order_by_all_rows_are_peers() {
    let mut eng = groups_engine();
    // PG19 row_is_in_frame: with no ORDER BY all rows are peers —
    // EXCLUDE TIES drops everything but the current row.
    assert_eq!(
        groups_q(
            &mut eng,
            "count(*) OVER (ROWS BETWEEN UNBOUNDED PRECEDING \
                 AND UNBOUNDED FOLLOWING EXCLUDE TIES)"
        ),
        vv(&[(10, "1"), (20, "1"), (30, "1"), (40, "1"), (50, "1")])
    );
    // EXCLUDE GROUP drops the whole peer group: empty frame.
    assert_eq!(
        groups_q(
            &mut eng,
            "sum(v) OVER (ROWS BETWEEN UNBOUNDED PRECEDING \
                 AND UNBOUNDED FOLLOWING EXCLUDE GROUP)"
        ),
        vv(&[
            (10, "NULL"),
            (20, "NULL"),
            (30, "NULL"),
            (40, "NULL"),
            (50, "NULL")
        ])
    );
}
