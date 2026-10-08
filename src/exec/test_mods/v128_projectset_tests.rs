/// v1.28: PG19 ProjectSet on the plain (non-grouped) path — SRF calls
/// nested anywhere in the target list fan out (`nodeProjectSet.c`
/// `ExecProjectSRF`; the planner lifts them via `split_pathtarget_at_srfs`).
/// Before v1.28 these shapes raised 42883; the top-level-SRF fast path is
/// unchanged. Also covers the `eval_expr` Func-arm interception (which
/// additionally fixes an SRF under a user operator on the grouped path)
/// and the ORDER BY / DISTINCT ON interactions.
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

fn s(v: &str) -> String {
    v.to_string()
}

// -- S1: plain-path nested SRF fan-out --------------------------------

/// The exact shape v1.27 deferred: a builtin SRF nested in arithmetic
/// on the plain path fans out (was 42883).
#[test]
fn nested_srf_arith() {
    let mut eng = engine();
    let r = run(&mut eng, "select generate_series(1,3)+1").unwrap();
    assert_eq!(rows_of(r), vec![vec![s("2")], vec![s("3")], vec![s("4")]]);
}

/// SRF on the left of the operator nests the same way.
#[test]
fn nested_srf_left_operand() {
    let mut eng = engine();
    let r = run(&mut eng, "select 1+generate_series(1,2)").unwrap();
    assert_eq!(rows_of(r), vec![vec![s("2")], vec![s("3")]]);
}

/// Deeper nesting: the SRF value feeds the whole expression tree.
#[test]
fn nested_srf_deep() {
    let mut eng = engine();
    let r = run(&mut eng, "select generate_series(1,3)*10+5").unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec![s("15")], vec![s("25")], vec![s("35")]]
    );
}

/// Two SRFs zip: the shorter pads with NULL past exhaustion (PG19
/// `ExecProjectSRF`).
#[test]
fn nested_srf_zip_null_pad() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "select generate_series(1,3), generate_series(10,11)+0",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec![s("1"), s("10")],
            vec![s("2"), s("11")],
            vec![s("3"), s("NULL")],
        ]
    );
}

/// A top-level SRF item and a nested SRF zip together.
#[test]
fn nested_srf_mixed_top_and_nested() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "select generate_series(1,2), generate_series(1,3)+100",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec![s("1"), s("101")],
            vec![s("2"), s("102")],
            vec![s("NULL"), s("103")],
        ]
    );
}

/// An all-empty SRF set drops the row (`hasresult` false) even next
/// to plain columns.
#[test]
fn nested_srf_empty_drops_row() {
    let mut eng = engine();
    let r = run(&mut eng, "select 'a', generate_series(1,0)+1").unwrap();
    assert_eq!(rows_of(r), Vec::<Vec<String>>::new());
}

/// Plain columns repeat their value on every fanned-out row.
#[test]
fn nested_srf_plain_col_repeats() {
    let mut eng = engine();
    let r = run(&mut eng, "select 'a' as c, generate_series(1,2)+1").unwrap();
    assert_eq!(rows_of(r), vec![vec![s("a"), s("2")], vec![s("a"), s("3")]]);
}

/// The SRF args see the input row: fan-out is per input row.
#[test]
fn nested_srf_correlated() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "select x, x+generate_series(1,2) from (values (10),(20)) t(x)",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec![s("10"), s("11")],
            vec![s("10"), s("12")],
            vec![s("20"), s("21")],
            vec![s("20"), s("22")],
        ]
    );
}

/// Fan-out works against a real table scan too.
#[test]
fn nested_srf_from_table() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t128 (f1 int)").unwrap();
    run(&mut eng, "INSERT INTO t128 VALUES (0),(5)").unwrap();
    let r = run(&mut eng, "select f1, generate_series(1,2)+f1 from t128").unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec![s("0"), s("1")],
            vec![s("0"), s("2")],
            vec![s("5"), s("6")],
            vec![s("5"), s("7")],
        ]
    );
}

/// PG19 allows SRFs in ORDER BY (parse_func.c `EXPR_KIND_ORDER_BY` is
/// "okay"); it evaluates after the ProjectSet, so a textual SRF term
/// sees the fanned values.
#[test]
fn nested_srf_order_by_textual() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "select generate_series(1,3)+1 order by generate_series(1,3)+1 desc",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec![s("4")], vec![s("3")], vec![s("2")]]);
}

/// Ordinal ORDER BY over a nested-SRF item sorts the fanned rows.
#[test]
fn nested_srf_order_by_ordinal() {
    let mut eng = engine();
    let r = run(&mut eng, "select generate_series(1,3)+1 order by 1 desc").unwrap();
    assert_eq!(rows_of(r), vec![vec![s("4")], vec![s("3")], vec![s("2")]]);
}

/// DISTINCT ON defers the expansion until after the first-row-per-group
/// filter (PG's ProjectSet sits above Unique); nested SRF items are
/// NULL placeholders in the initial projection.
#[test]
fn nested_srf_distinct_on() {
    let mut eng = engine();
    let r = run(
        &mut eng,
        "select distinct on (x) x, generate_series(1,2)+x \
             from (values (2),(1)) t(x) order by x",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![
            vec![s("1"), s("2")],
            vec![s("1"), s("3")],
            vec![s("2"), s("3")],
            vec![s("2"), s("4")],
        ]
    );
}

/// A user-defined RETURNS SETOF function nested in an expression fans
/// out through `eval_user_srf_vals`.
#[test]
fn nested_srf_user_function() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE FUNCTION gen128(n int) RETURNS SETOF int AS $$ declare i int; \
             begin for i in select * from generate_series(1, n) loop return next i * 10; \
             end loop; end; $$ LANGUAGE plpgsql",
    )
    .unwrap();
    let r = run(&mut eng, "select gen128(2)+1").unwrap();
    assert_eq!(rows_of(r), vec![vec![s("11")], vec![s("21")]]);
}

/// The v1.28 `eval_expr` Func-arm interception also fixes the latent
/// grouped-path gap: `eval_grouped` delegates `UserOp` to `eval_expr`,
/// so an SRF under a user operator on the grouped path 42883'd before.
#[test]
fn srf_under_userop_grouped() {
    let mut eng = engine();
    run(
        &mut eng,
        "create function add128(int,int) returns int language sql as 'select $1+$2'",
    )
    .unwrap();
    run(
        &mut eng,
        "create operator ?# (procedure = add128, leftarg = int, rightarg = int)",
    )
    .unwrap();
    let r = run(
        &mut eng,
        "select x, generate_series(1,2) ?# 10 from (values (1)) t(x) group by x",
    )
    .unwrap();
    assert_eq!(
        rows_of(r),
        vec![vec![s("1"), s("11")], vec![s("1"), s("12")]]
    );
}

/// SRFs under aggregate calls keep the existing below-Agg path, which
/// this engine rejects (unchanged by v1.28).
#[test]
fn nested_srf_under_agg_still_42883() {
    let mut eng = engine();
    let err = run(
        &mut eng,
        "select sum(generate_series(1,3))+1 from (values (1)) t(x)",
    )
    .unwrap_err();
    assert_eq!(err.code, "42883");
}

/// The v0.32 top-level-SRF fast path is untouched by the general path.
#[test]
fn top_level_srf_fast_path_unchanged() {
    let mut eng = engine();
    let r = run(&mut eng, "select generate_series(1,2)").unwrap();
    assert_eq!(rows_of(r), vec![vec![s("1")], vec![s("2")]]);
    let r = run(
        &mut eng,
        "select generate_series(1,2), generate_series(5,6)",
    )
    .unwrap();
    assert_eq!(rows_of(r), vec![vec![s("1"), s("5")], vec![s("2"), s("6")]]);
}
