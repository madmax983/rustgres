/// v1.26: PG19 LATERAL semantic-validation parity — the two singleton
/// EXPECTED-FAILs this version targets, plus nearby legal controls.
/// Grounded in PG19 `scanNameSpaceForRefname` (42P09) and
/// `check_agglevels_and_constraints` (42803); both error codes and
/// messages verified against a live PostgreSQL 16 and the PG19 sources.
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

fn setup(eng: &mut Engine) {
    run(eng, "CREATE TABLE int8_tbl (q1 bigint, q2 bigint)").unwrap();
    run(eng, "INSERT INTO int8_tbl VALUES (1, 2)").unwrap();
    run(eng, "CREATE TABLE int4_tbl (f1 int)").unwrap();
    run(eng, "INSERT INTO int4_tbl VALUES (3)").unwrap();
    run(eng, "CREATE TABLE tenk1 (unique1 int)").unwrap();
    run(eng, "INSERT INTO tenk1 VALUES (10)").unwrap();
}

/// Corpus singleton 1: a duplicate alias `x` visible inside a nested
/// LATERAL query is 42P09 `table reference "x" is ambiguous`.
#[test]
fn nested_lateral_ambiguous_alias_is_42p09() {
    let mut eng = engine();
    setup(&mut eng);
    let err = run(
        &mut eng,
        "select * from int8_tbl x cross join (int4_tbl x cross join lateral (select x.f1) ss)",
    )
    .unwrap_err();
    assert_eq!(err.code, "42P09");
    assert_eq!(err.message, "table reference \"x\" is ambiguous");
}

/// Corpus singleton 2: an aggregate over a lateral-visible variable
/// inside a LATERAL FROM subselect is 42803.
#[test]
fn lateral_agg_over_parent_level_is_42803() {
    let mut eng = engine();
    setup(&mut eng);
    let err = run(
        &mut eng,
        "select 1 from tenk1 a, lateral (select max(a.unique1) from int4_tbl b) ss",
    )
    .unwrap_err();
    assert_eq!(err.code, "42803");
    assert_eq!(
        err.message,
        "aggregate functions are not allowed in FROM clause of their own query level"
    );
}

/// The 42P09 check is namespace-general, not tied to the corpus
/// tables: a duplicate qualifier between the immediate left row and
/// the lateral's own FROM item also errors.
#[test]
fn lateral_own_from_duplicate_alias_is_42p09() {
    let mut eng = engine();
    setup(&mut eng);
    let err = run(
        &mut eng,
        "select * from int8_tbl x cross join lateral (select x.q1 from int4_tbl x) ss",
    )
    .unwrap_err();
    assert_eq!(err.code, "42P09");
}

/// The 42803 check is level-general: an unqualified parent-level
/// column inside the aggregate errors too.
#[test]
fn lateral_agg_unqualified_parent_ref_is_42803() {
    let mut eng = engine();
    setup(&mut eng);
    // unique1 is not in int4_tbl: it resolves to the lateral-visible
    // tenk1 a -> 42803.
    let err = run(
        &mut eng,
        "select 1 from tenk1 a, lateral (select max(unique1) from int4_tbl b) ss",
    )
    .unwrap_err();
    assert_eq!(err.code, "42803");
}

/// PG19 rejects an aggregate argument referencing ANY outer-level
/// scope, not just the immediate left input: `min_varlevel >= 1`
/// walks up to the outer pstate (`EXPR_KIND_FROM_SUBSELECT`).
#[test]
fn lateral_agg_nonimmediate_outer_ref_is_42803() {
    let mut eng = engine();
    setup(&mut eng);
    // z.q1 is two FROM items back (not the immediate left row) —
    // still an outer-level variable -> 42803.
    let err = run(
        &mut eng,
        "select 1 from int8_tbl z, tenk1 a, lateral (select max(z.q1) from int4_tbl b) ss",
    )
    .unwrap_err();
    assert_eq!(err.code, "42803");
}

/// Legal controls: unambiguous lateral references and local
/// aggregates still succeed.
#[test]
fn lateral_legal_controls_still_pass() {
    let mut eng = engine();
    setup(&mut eng);
    // Single alias: resolves fine.
    run(
        &mut eng,
        "select * from int8_tbl x cross join lateral (select x.q1) ss",
    )
    .unwrap();
    // Aggregate over the lateral's own level: fine.
    run(
        &mut eng,
        "select * from int8_tbl x cross join lateral (select max(f1) from int4_tbl) ss",
    )
    .unwrap();
    // Correlated lateral without aggregates: fine.
    run(
        &mut eng,
        "select * from tenk1 a, lateral (select a.unique1 from int4_tbl b) ss",
    )
    .unwrap();
    // Top-level aggregate: no marker live, unaffected.
    run(&mut eng, "select max(q1) from int8_tbl").unwrap();
    // Scalar subquery (not LATERAL): no marker, unaffected.
    run(
        &mut eng,
        "select (select max(q1) from int8_tbl) from int4_tbl",
    )
    .unwrap();
}
