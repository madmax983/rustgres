/// v1.88: range / multirange types as first-class column types —
/// input/output with PG canonicalization, constructors, containment /
/// overlap operators, union/intersection/difference, range_agg /
/// range_intersect_agg, user-defined range types, and PG sqlstates
/// for the malformed cases (grounded in PG19 rangetypes.c /
/// multirangetypes.c).
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

/// First cell of the first row of a SELECT, as output text.
fn cell(eng: &mut Engine, sql: &str) -> Option<String> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .first()
            .and_then(|r| r.iter().next())
            .and_then(|v| v.to_text()),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql)
        .expect_err(&format!("{sql} should fail"))
        .code
        .to_string()
}

#[test]
fn discrete_canonicalization() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "select '[1,5]'::int4range").as_deref(),
        Some("[1,6)")
    );
    assert_eq!(
        cell(&mut eng, "select '(1,5)'::int4range").as_deref(),
        Some("[2,5)")
    );
    assert_eq!(
        cell(&mut eng, "select '(1,5]'::int8range").as_deref(),
        Some("[2,6)")
    );
    assert_eq!(
        cell(&mut eng, "select '[2020-01-01,2020-01-02]'::daterange").as_deref(),
        Some("[2020-01-01,2020-01-03)")
    );
    // Adjacent-after-canonicalization collapses to empty for discrete.
    assert_eq!(
        cell(&mut eng, "select '[1,1)'::int4range").as_deref(),
        Some("empty")
    );
    assert_eq!(
        cell(&mut eng, "select 'empty'::numrange").as_deref(),
        Some("empty")
    );
}

#[test]
fn continuous_bounds_kept() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "select '[1.5,2.5]'::numrange").as_deref(),
        Some("[1.5,2.5]")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select '[2020-01-01 10:00,2020-01-01 12:00)'::tsrange"
        )
        .as_deref(),
        Some("[\"2020-01-01 10:00:00\",\"2020-01-01 12:00:00\")")
    );
    assert_eq!(
        cell(&mut eng, "select '(,)'::numrange").as_deref(),
        Some("(,)")
    );
}

#[test]
fn constructors_and_flags() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "select int4range(1, 5)").as_deref(),
        Some("[1,5)")
    );
    assert_eq!(
        cell(&mut eng, "select int4range(1, 5, '[]')").as_deref(),
        Some("[1,6)")
    );
    assert_eq!(
        cell(&mut eng, "select int4range(1, 5, '()')").as_deref(),
        Some("[2,5)")
    );
    assert_eq!(
        cell(&mut eng, "select int4range(null, 5)").as_deref(),
        Some("(,5)")
    );
    assert_eq!(
        cell(&mut eng, "select daterange('2020-01-01', '2020-01-03')").as_deref(),
        Some("[2020-01-01,2020-01-03)")
    );
    assert_eq!(err_code(&mut eng, "select int4range(1, 5, 'xx')"), "42601");
    // Multirange constructors merge overlapping members.
    assert_eq!(
        cell(
            &mut eng,
            "select int4multirange(int4range(1,3), int4range(2,5))"
        )
        .as_deref(),
        Some("{[1,5)}")
    );
    assert_eq!(
        cell(&mut eng, "select int4multirange()").as_deref(),
        Some("{}")
    );
}

#[test]
fn containment_and_overlap() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "select '[1,5)'::int4range @> 3").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "select '[1,5)'::int4range @> 5").as_deref(),
        Some("f")
    );
    assert_eq!(
        cell(&mut eng, "select 3 <@ '[1,5)'::int4range").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "select '[1,5)'::int4range @> '[2,3)'::int4range").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "select '[1,5)'::int4range && '[5,9)'::int4range").as_deref(),
        Some("f")
    );
    assert_eq!(
        cell(&mut eng, "select '[1,5)'::int4range && '[4,9)'::int4range").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "select '{[1,3),[8,9)}'::int4multirange @> 8").as_deref(),
        Some("t")
    );
}

#[test]
fn set_operations() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "select '[1,5)'::int4range + '[5,9)'::int4range").as_deref(),
        Some("[1,9)")
    );
    assert_eq!(
        cell(&mut eng, "select '[1,9)'::int4range * '[3,20)'::int4range").as_deref(),
        Some("[3,9)")
    );
    assert_eq!(
        cell(&mut eng, "select '[1,9)'::int4range - '[9,20)'::int4range").as_deref(),
        Some("[1,9)")
    );
    // Splitting difference is a 22000 for plain ranges...
    assert_eq!(
        err_code(&mut eng, "select '[1,9)'::int4range - '[3,5)'::int4range"),
        "22000"
    );
    // ...but fine against / into multiranges.
    assert_eq!(
        cell(
            &mut eng,
            "select '[1,9)'::int4range - '{[3,5)}'::int4multirange"
        )
        .as_deref(),
        Some("{[1,3),[5,9)}")
    );
    // Non-contiguous union is a 22000.
    assert_eq!(
        err_code(&mut eng, "select '[1,3)'::int4range + '[5,7)'::int4range"),
        "22000"
    );
}

#[test]
fn bound_accessors() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "select lower('[1,5)'::int4range)").as_deref(),
        Some("1")
    );
    assert_eq!(
        cell(&mut eng, "select upper('[1,5)'::int4range)").as_deref(),
        Some("5")
    );
    assert_eq!(
        cell(&mut eng, "select lower_inc('(1,5)'::int4range)").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "select upper_inf('[1,)'::int4range)").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "select isempty('empty'::int4range)").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select range_merge('[1,3)'::int4range, '[5,7)'::int4range)"
        )
        .as_deref(),
        Some("[1,7)")
    );
    assert_eq!(
        cell(&mut eng, "select lower('{[2,3),[5,9)}'::int4multirange)").as_deref(),
        Some("2")
    );
}

#[test]
fn range_aggregates() {
    let mut eng = engine();
    run(&mut eng, "create table ra (r int4range)").unwrap();
    run(
        &mut eng,
        "insert into ra values ('[1,3)'), ('[5,7)'), ('[2,4)')",
    )
    .unwrap();
    assert_eq!(
        cell(&mut eng, "select range_agg(r) from ra").as_deref(),
        Some("{[1,4),[5,7)}")
    );
    assert_eq!(
        cell(&mut eng, "select range_intersect_agg(r) from ra").as_deref(),
        Some("empty")
    );
    run(&mut eng, "create table rb (r int4range)").unwrap();
    assert_eq!(
        cell(&mut eng, "select range_agg(r) from rb").as_deref(),
        Some("{}")
    );
    assert_eq!(
        cell(&mut eng, "select range_intersect_agg(r) from rb").as_deref(),
        None
    );
}

#[test]
fn range_columns_round_trip() {
    let mut eng = engine();
    run(&mut eng, "create table rc (r int4range, m nummultirange)").unwrap();
    run(&mut eng, "insert into rc values ('[1,5]', '{[1.5,2.5]}')").unwrap();
    assert_eq!(cell(&mut eng, "select r from rc").as_deref(), Some("[1,6)"));
    assert_eq!(
        cell(&mut eng, "select m from rc").as_deref(),
        Some("{[1.5,2.5]}")
    );
    assert_eq!(
        cell(&mut eng, "select count(*) from rc where r @> 4").as_deref(),
        Some("1")
    );
    assert_eq!(
        cell(&mut eng, "select pg_typeof(r) from rc").as_deref(),
        Some("int4range")
    );
}

#[test]
fn user_defined_range_types() {
    let mut eng = engine();
    run(&mut eng, "create type textrange as range (subtype=text)").unwrap();
    run(&mut eng, "create table rt (t textrange, tm textmultirange)").unwrap();
    run(&mut eng, "insert into rt values ('[a,c)', '{[a,b),[d,e)}')").unwrap();
    assert_eq!(cell(&mut eng, "select t from rt").as_deref(), Some("[a,c)"));
    assert_eq!(
        cell(&mut eng, "select pg_typeof(t) from rt").as_deref(),
        Some("textrange")
    );
    assert_eq!(
        cell(&mut eng, "select '[a,c)'::textrange @> 'b'").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "select textrange('a', 'c')").as_deref(),
        Some("[a,c)")
    );
    run(
        &mut eng,
        "create type float8range as range (subtype=float8)",
    )
    .unwrap();
    assert_eq!(
        cell(&mut eng, "select '[1.5,2.5)'::float8range").as_deref(),
        Some("[1.5,2.5)")
    );
}

#[test]
fn range_error_sqlstates() {
    let mut eng = engine();
    assert_eq!(err_code(&mut eng, "select '[5,1)'::int4range"), "22000");
    assert_eq!(
        err_code(&mut eng, "select '[1,2147483647]'::int4range"),
        "22003"
    );
    assert_eq!(err_code(&mut eng, "select 'nonsense'::int4range"), "22P02");
    assert_eq!(err_code(&mut eng, "select '[1,x)'::int4range"), "22P02");
    // Variadic constructor with a NULL member: 22004 (PG
    // multirangetypes.c). The single-argument form is a CAST in PG
    // (typename(expr)), and a cast of NULL is NULL.
    assert_eq!(
        err_code(
            &mut eng,
            "select int4multirange(null::int4range, null::int4range)"
        ),
        "22004"
    );
    assert_eq!(
        cell(&mut eng, "select int4multirange(null::int4range)").as_deref(),
        None
    );
}

#[test]
fn range_equality_and_ordering() {
    let mut eng = engine();
    assert_eq!(
        cell(&mut eng, "select '[1,5]'::int4range = '[1,6)'::int4range").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(&mut eng, "select '[1,5)'::int4range < '[1,6)'::int4range").as_deref(),
        Some("t")
    );
    assert_eq!(
        cell(
            &mut eng,
            "select '{[1,2)}'::int4multirange = '{[1,2)}'::int4multirange"
        )
        .as_deref(),
        Some("t")
    );
}
