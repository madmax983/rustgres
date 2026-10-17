/// v1.91: range/multirange remainder — the position operators
/// << >> &< &> -|- over ranges and multiranges (PG19 rangetypes.c
/// range_before/after/overleft/overright/adjacent and the
/// multirangetypes.c cross forms), the internal function family
/// (range_contains, multirange_contains_elem, ...), the SETOF
/// difference functions, unnest(anymultirange), cast column naming,
/// range_agg's zero-input NULL, daterange infinity canonicalization,
/// and btree index ordering over range keys.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
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

/// All rows of a SELECT, each row's cells as output text.
fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn cell(eng: &mut Engine, sql: &str) -> Option<String> {
    rows(eng, sql)
        .first()
        .and_then(|r| r.iter().next())
        .cloned()
        .flatten()
}

fn col_name(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { columns, .. } => columns[0].0.clone(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn b(eng: &mut Engine, sql: &str) -> bool {
    cell(eng, sql).as_deref() == Some("t")
}

#[test]
fn v191_position_operators_ranges() {
    let mut eng = engine();
    // << / >> (range_before / range_after): an empty range is
    // neither before nor after anything.
    assert!(b(
        &mut eng,
        "select numrange(1.0, 2.0) << numrange(3.0, 4.0)"
    ));
    // Bound ordering (range_cmp_bounds): an exclusive upper bound at
    // 2 sorts before an inclusive lower bound at 2, so adjacent
    // ranges are strictly-left; inclusive/inclusive at 2 compares
    // equal, so not strictly left.
    assert!(b(
        &mut eng,
        "select numrange(1.0, 2.0) << numrange(2.0, 4.0)"
    ));
    assert!(!b(
        &mut eng,
        "select numrange(1.0, 2.0, '[]') << numrange(2.0, 4.0)"
    ));
    assert!(b(
        &mut eng,
        "select numrange(1.0, 3.0, '[]') >> numrange(0.0, 0.5)"
    ));
    assert!(!b(
        &mut eng,
        "select 'empty'::numrange << numrange(3.0, 4.0)"
    ));
    assert!(!b(
        &mut eng,
        "select numrange(1.0, 2.0) >> 'empty'::numrange"
    ));
    // Unbounded sides: (,) is before nothing and after nothing finite.
    assert!(!b(
        &mut eng,
        "select numrange(1.0, 2.0) << numrange(null, null)"
    ));
    assert!(b(
        &mut eng,
        "select numrange(null, 2.0) << numrange(3.0, null)"
    ));
    // &< (does not extend right of): upper bounds compare with
    // range_cmp_bounds — [1,5] &< [1,5) is false (5 <= 5 but the
    // inclusive upper sorts after the exclusive one).
    assert!(b(
        &mut eng,
        "select numrange(3.0, 70.0) &< numrange(6.6, 100.0)"
    ));
    assert!(!b(
        &mut eng,
        "select numrange(1.0, 5.0, '[]') &< numrange(1.0, 5.0, '[)')"
    ));
    assert!(b(
        &mut eng,
        "select numrange(1.0, 5.0, '[)') &< numrange(1.0, 5.0, '[]')"
    ));
    // &> (does not extend left of).
    assert!(b(
        &mut eng,
        "select numrange(6.6, 100.0) &> numrange(3.0, 70.0)"
    ));
    assert!(!b(
        &mut eng,
        "select numrange(3.0, 70.0) &> numrange(6.6, 100.0)"
    ));
    // -|- adjacency: continuous ranges touching at a point covered
    // by exactly one side.
    assert!(b(
        &mut eng,
        "select numrange(2.0, 3.0) -|- numrange(3.0, 4.0)"
    ));
    assert!(!b(
        &mut eng,
        "select numrange(2.0, 3.0) -|- numrange(3.1, 4.0)"
    ));
    assert!(!b(
        &mut eng,
        "select numrange(2.0, 3.0, '[)') -|- numrange(3.0, 4.0, '(]')"
    ));
    // Discrete adjacency goes through canonical form: [1,5) meets [5,7).
    assert!(b(&mut eng, "select int4range(1, 5) -|- int4range(5, 7)"));
    assert!(!b(&mut eng, "select int4range(1, 5) -|- int4range(6, 7)"));
}

#[test]
fn v191_position_operators_cross() {
    let mut eng = engine();
    // range @> multirange and multirange <@ range (the v1.88 gaps).
    assert!(b(
        &mut eng,
        "select numrange(1, 9) @> nummultirange(numrange(1, 5), numrange(6, 9))"
    ));
    assert!(!b(
        &mut eng,
        "select numrange(1, 9) @> nummultirange(numrange(1, 5), numrange(6, 10))"
    ));
    assert!(b(
        &mut eng,
        "select nummultirange(numrange(1, 5), numrange(6, 9)) <@ numrange(1, 9)"
    ));
    // Cross position forms.
    assert!(b(
        &mut eng,
        "select numrange(1, 2) << nummultirange(numrange(3, 4))"
    ));
    assert!(b(
        &mut eng,
        "select nummultirange(numrange(3, 4)) >> numrange(1, 2)"
    ));
    assert!(b(
        &mut eng,
        "select nummultirange(numrange(1, 2)) &< numrange(3, 4)"
    ));
    assert!(!b(&mut eng, "select 'empty'::numrange &< nummultirange()"));
    // PG multirange adjacency is span-endpoint only: a range in a
    // multirange's inner gap is NOT adjacent to the multirange even
    // though it touches an inner component on both sides.
    assert!(!b(
        &mut eng,
        "select numrange(3, 4) -|- nummultirange(numrange(1, 3), numrange(4, 6))"
    ));
    assert!(b(
        &mut eng,
        "select numrange(6, 8) -|- nummultirange(numrange(1, 3), numrange(4, 6))"
    ));
    assert!(b(
        &mut eng,
        "select nummultirange(numrange(1, 2)) -|- nummultirange(numrange(2, 4))"
    ));
}

#[test]
fn v191_internal_function_family() {
    let mut eng = engine();
    assert!(b(
        &mut eng,
        "select range_contains(numrange(1, 5), numrange(2, 3))"
    ));
    assert!(b(
        &mut eng,
        "select range_contained_by(numrange(2, 3), numrange(1, 5))"
    ));
    assert!(b(
        &mut eng,
        "select range_overlaps(numrange(1, 5), numrange(4, 9))"
    ));
    assert!(b(
        &mut eng,
        "select range_before(numrange(1, 2), numrange(3, 4))"
    ));
    assert!(b(
        &mut eng,
        "select range_after(numrange(3, 4), numrange(1, 2))"
    ));
    assert!(b(
        &mut eng,
        "select range_overleft(numrange(1, 5), numrange(0, 6))"
    ));
    assert!(b(
        &mut eng,
        "select multirange_contains_elem(nummultirange(numrange(1, 5)), 4.0)"
    ));
    assert!(b(
        &mut eng,
        "select elem_contained_by_multirange(4.0, nummultirange(numrange(1, 5)))"
    ));
    assert!(b(
        &mut eng,
        "select range_overlaps_multirange(numrange(4.0, 4.2), nummultirange(numrange(4, 5)))"
    ));
    assert!(b(
        &mut eng,
        "select multirange_contains_range(nummultirange(numrange(1, 5)), numrange(2, 3))"
    ));
    // The generic multirange() constructor function.
    assert_eq!(
        cell(&mut eng, "select multirange(numrange(1, 2))").as_deref(),
        Some("{[1,2)}")
    );
}

#[test]
fn v191_minus_multi_are_setof() {
    let mut eng = engine();
    // range_minus_multi: one row per piece (PG proretset).
    assert_eq!(
        rows(
            &mut eng,
            "select range_minus_multi('empty'::numrange, numrange(2.0, 3.0))"
        ),
        Vec::<Vec<Option<String>>>::new()
    );
    assert_eq!(
        rows(
            &mut eng,
            "select range_minus_multi(numrange(1.0, 3.0), numrange(1.5, 2.0))"
        ),
        vec![
            vec![Some("[1.0,1.5)".to_string())],
            vec![Some("[2.0,3.0)".to_string())],
        ]
    );
    // multirange_minus_multi: zero rows when the difference is
    // empty, else a single multirange row.
    assert_eq!(
        rows(
            &mut eng,
            "select multirange_minus_multi(nummultirange(numrange(1, 2)), nummultirange(numrange(1, 2)))"
        ),
        Vec::<Vec<Option<String>>>::new()
    );
    assert_eq!(
        rows(
            &mut eng,
            "select multirange_minus_multi(nummultirange(numrange(1, 4)), nummultirange(numrange(2, 3)))"
        ),
        vec![vec![Some("{[1,2),[3,4)}".to_string())]]
    );
}

#[test]
fn v191_unnest_multirange() {
    let mut eng = engine();
    assert_eq!(
        rows(
            &mut eng,
            "select unnest(int4multirange(int4range('5', '6'), int4range('1', '2')))"
        ),
        vec![
            vec![Some("[1,2)".to_string())],
            vec![Some("[5,6)".to_string())],
        ]
    );
}

#[test]
fn v191_cast_column_names() {
    let mut eng = engine();
    run(
        &mut eng,
        "create type v191_textrange as range (subtype = text)",
    )
    .unwrap();
    // A bare cast to a named range type names its column after the
    // type (PG FigureColname for TypeCast). The auto-derived
    // multirange companion of v191_textrange is v191_textmultirange.
    assert_eq!(
        col_name(&mut eng, "select 'empty'::v191_textrange"),
        "v191_textrange"
    );
    assert_eq!(
        col_name(&mut eng, "select '{}'::v191_textmultirange"),
        "v191_textmultirange"
    );
    // A strength-2 inner name (the constructor call) wins over the
    // outer cast's type name.
    assert_eq!(
        col_name(
            &mut eng,
            "select v191_textrange('a', 'c')::v191_textmultirange"
        ),
        "v191_textrange"
    );
}

#[test]
fn v191_range_agg_zero_inputs_null() {
    let mut eng = engine();
    // PG range_agg_finalfn: zero non-NULL inputs -> NULL.
    assert_eq!(
        cell(
            &mut eng,
            "select range_agg(nmr) from (values ('{}'::nummultirange)) t(nmr) where false"
        ),
        None
    );
    // An empty multirange input still counts -> '{}'.
    assert_eq!(
        cell(
            &mut eng,
            "select range_agg(nmr) from (values ('{}'::nummultirange)) t(nmr)"
        )
        .as_deref(),
        Some("{}")
    );
}

#[test]
fn v191_daterange_infinity_canonical() {
    let mut eng = engine();
    // daterange_canonical skips DATE_NOT_FINITE bounds: the written
    // inclusivity survives.
    assert_eq!(
        cell(
            &mut eng,
            "select daterange('-infinity'::date, '2000-01-01'::date, '()')"
        )
        .as_deref(),
        Some("(-infinity,2000-01-01)")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select daterange('2000-01-01'::date, 'infinity'::date, '[]')"
        )
        .as_deref(),
        Some("[2000-01-01,infinity]")
    );
}

#[test]
fn v191_btree_index_orders_range_keys() {
    let mut eng = engine();
    run(&mut eng, "create table v191_ti (r int4range)").unwrap();
    run(
        &mut eng,
        "insert into v191_ti values ('[3,5)'), ('[1,2)'), ('[10,20)'), ('empty')",
    )
    .unwrap();
    run(&mut eng, "create index v191_ti_idx on v191_ti (r)").unwrap();
    // With the index present the planner may scan it for ORDER BY;
    // the keys must compare by range_cmp (empty first), not heap
    // order.
    assert_eq!(
        rows(&mut eng, "select r from v191_ti order by r"),
        vec![
            vec![Some("empty".to_string())],
            vec![Some("[1,2)".to_string())],
            vec![Some("[3,5)".to_string())],
            vec![Some("[10,20)".to_string())],
        ]
    );
}

#[test]
fn v191_companion_create_type_reaches_wal() {
    // Regression (pre-existing, found in v1.91): records_for_commit's
    // loop has a blanket `i += 1`, and the DDL arms ALSO incremented,
    // so the write op following any CreateType / DropType /
    // CreateFunction / ... op was silently dropped from the WAL.
    // For CREATE TYPE AS RANGE the dropped op was the multirange
    // companion's CreateType: companions vanished on crash recovery
    // while the range type itself survived.
    let mut eng = engine();
    let stmt = parse_statement(
        "create type tmp_f8range as range (subtype = float8, subtype_diff = float8mi)",
    )
    .unwrap();
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
    execute(&mut eng, &mut ctx, &stmt).unwrap();
    // v1.96: CREATE TYPE also emits FnTypeAcls ops (type-owner
    // tracking); the regression invariant is that BOTH CreateType
    // ops are present — count those, not the total op stream.
    let n_create_type = writes
        .iter()
        .filter(|w| matches!(w, WriteOp::CreateType { .. }))
        .count();
    assert_eq!(n_create_type, 2, "range + companion CreateType ops");
    let recs = crate::wal::records_for_commit(&eng, 9, &writes, 0).unwrap();
    assert_eq!(recs.len(), 2, "both CreateType ops must reach the WAL");
    match &recs[1] {
        crate::wal::WalRecord::CreateType { name, range, .. } => {
            assert_eq!(name, "tmp_f8multirange");
            assert_eq!(
                *range,
                Some((crate::storage::RangeElem::Float8.code(), true))
            );
        }
        other => panic!("expected companion CreateType record, got {other:?}"),
    }
}
