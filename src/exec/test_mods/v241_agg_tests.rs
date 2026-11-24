/// v2.41: AGGREGATES REMAINDER — aggregate function surface
/// (bool_or / any_value / regr family), PG19 Youngs-Cramer float
/// accumulation, user-defined aggregates (validation, composite
/// plpgsql state, ordered-set WITHIN GROUP, ORDER BY/USING keys),
/// hypothetical-set literal coercion, float4->numeric input fidelity.
/// Ground truth: PG19 aggregates.sql / create_aggregate.sql.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run_as(eng: &mut Engine, sql: &str, role: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        privs_suppressed: false,
        snap: &snap,
        own: 9,
        write_xid: 9,
        all_xids: vec![9],
        level: IsolationLevel::ReadCommitted,
        writes: &mut writes,
        session: 0,
        role,
        read_only: false,
        default_toast_compression: crate::storage::ToastCompression::Pglz,
        notices: Vec::new(),
    };
    execute(eng, &mut ctx, &stmt)
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_as(eng, sql, "postgres")
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

fn one(eng: &mut Engine, sql: &str) -> String {
    let rows = rows_of(run(eng, sql).unwrap());
    assert_eq!(rows.len(), 1, "{sql}");
    rows.into_iter().next().unwrap().into_iter().next().unwrap()
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql).unwrap_err().code.to_string()
}

#[test]
fn bool_or_and_any_value() {
    let mut eng = engine();
    assert_eq!(
        one(
            &mut eng,
            "select bool_or(b) from (values(true),(false)) t(b)"
        ),
        "t"
    );
    assert_eq!(
        one(
            &mut eng,
            "select bool_or(b) from (values(false),(false)) t(b)"
        ),
        "f"
    );
    assert_eq!(
        one(&mut eng, "select any_value(x) from (values(7),(9)) t(x)"),
        "7"
    );
}

#[test]
fn regr_family_surface() {
    let mut eng = engine();
    // Points on y = 2x + 1: slope 2, intercept 1, r2 1, sxx 5, syy 20, sxy 10.
    let setup = "create table pts(x float8, y float8); \
                 insert into pts values (1,3),(2,5),(3,7),(4,9)";
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            run(&mut eng, s).unwrap();
        }
    }
    assert_eq!(one(&mut eng, "select regr_count(y, x) from pts"), "4");
    assert_eq!(one(&mut eng, "select regr_slope(y, x) from pts"), "2");
    assert_eq!(one(&mut eng, "select regr_intercept(y, x) from pts"), "1");
    assert_eq!(one(&mut eng, "select regr_r2(y, x) from pts"), "1");
    assert_eq!(one(&mut eng, "select regr_sxx(y, x) from pts"), "5");
    assert_eq!(one(&mut eng, "select regr_syy(y, x) from pts"), "20");
    assert_eq!(one(&mut eng, "select regr_sxy(y, x) from pts"), "10");
    assert_eq!(one(&mut eng, "select corr(y, x) from pts"), "1");
    // NULL pairs are skipped pairwise.
    assert_eq!(
        one(
            &mut eng,
            "select regr_count(y, x) from (values (1.0, null), (3.0, 1.0)) t(y, x)"
        ),
        "1"
    );
    // REL_19_STABLE: an Inf/NaN anywhere poisons the dependent sums.
    assert_eq!(
        one(
            &mut eng,
            "select covar_pop(y, x) from (values ('NaN'::float8, 1.0), (2.0, 2.0)) t(y, x)"
        ),
        "NaN"
    );
}

#[test]
fn float_variance_large_offsets() {
    let mut eng = engine();
    // Youngs-Cramer: the naive sum-of-squares algorithm loses this.
    assert_eq!(
        one(
            &mut eng,
            "select var_pop(x) from (values (1000000001::float8), (1000000002::float8), \
             (1000000003::float8), (1000000004::float8)) t(x)"
        ),
        "1.25"
    );
    assert_eq!(
        one(
            &mut eng,
            "select var_samp(x) from (values (1000000001::float8), (1000000002::float8), \
             (1000000003::float8), (1000000004::float8)) t(x)"
        ),
        "1.6666666666666667"
    );
}

#[test]
fn numeric_variance_uses_float4_shortest_text() {
    let mut eng = engine();
    // aggtest.b is float4 in the regress suite; PG converts float4 to
    // numeric via the f32's own shortest round-trip text (7.8), not
    // the widened f64 expansion (7.800000190734863).
    run(&mut eng, "create table aggtest(a int, b float4)").unwrap();
    run(
        &mut eng,
        "insert into aggtest values (56, 7.8), (100, 99.097), (0, 0.09561), (42, 324.78)",
    )
    .unwrap();
    assert_eq!(
        one(&mut eng, "select var_pop(b::numeric) from aggtest"),
        "17189.054065929769"
    );
    assert_eq!(
        one(&mut eng, "select stddev_pop(b::numeric) from aggtest"),
        "131.107032862199"
    );
}

#[test]
fn create_aggregate_validation() {
    let mut eng = engine();
    run(
        &mut eng,
        "create function least_accum(int8, int8) returns int8 language sql as 'select least($1, $2)'",
    )
    .unwrap();
    // sfunc signature cannot serve (stype, arg) without coercion.
    assert_eq!(
        err_code(
            &mut eng,
            "create aggregate least_agg(int4) (stype = int8, sfunc = least_accum)"
        ),
        "42P13"
    );
    // Duplicate aggregate with the same signature is 42723.
    run(
        &mut eng,
        "create aggregate cnt_star(*) (stype = int8, sfunc = int8inc_any, initcond = '0')",
    )
    .unwrap();
    assert_eq!(
        err_code(
            &mut eng,
            "create aggregate cnt_star(*) (stype = int8, sfunc = int8inc_any, initcond = '0')"
        ),
        "42723"
    );
    // Serialization functions must be specified in pairs.
    assert_eq!(
        err_code(
            &mut eng,
            "create aggregate badser(numeric) (stype = internal, sfunc = numeric_avg_accum, \
             serialfunc = numeric_avg_serialize)"
        ),
        "42P13"
    );
    // Moving-window strictness must match.
    run(
        &mut eng,
        "create function float8mi_n(float8, float8) returns float8 as $$ select $1 - $2 $$ language sql",
    )
    .unwrap();
    assert_eq!(
        err_code(
            &mut eng,
            "create aggregate invalidsumdouble(float8) (stype = float8, sfunc = float8pl, \
             mstype = float8, msfunc = float8pl, minvfunc = float8mi_n)"
        ),
        "42P13"
    );
}

#[test]
fn user_agg_plpgsql_composite_state() {
    let mut eng = engine();
    // aggregates.sql's my_avg: a plpgsql transition over a composite
    // state. Exercises bare-record RETURN and field assignment.
    run(
        &mut eng,
        "create type avg_state as (total bigint, count bigint)",
    )
    .unwrap();
    run(
        &mut eng,
        "create function avg_transfn(state avg_state, n int) returns avg_state as $$ \
         declare new_state avg_state; \
         begin if state is null then \
           if n is not null then new_state.total := n; new_state.count := 1; \
             return new_state; end if; return null; \
           elsif n is not null then state.total := state.total + n; \
             state.count := state.count + 1; return state; end if; \
           return null; end $$ language plpgsql",
    )
    .unwrap();
    run(
        &mut eng,
        "create function avg_finalfn(state avg_state) returns int4 as $$ \
         begin if state is null then return null; \
         else return state.total / state.count; end if; end $$ language plpgsql",
    )
    .unwrap();
    run(
        &mut eng,
        "create aggregate my_avg(int4) (stype = avg_state, sfunc = avg_transfn, \
         finalfunc = avg_finalfn)",
    )
    .unwrap();
    assert_eq!(
        one(&mut eng, "select my_avg(one) from (values(1),(3)) t(one)"),
        "2"
    );
}

#[test]
fn user_agg_order_by_and_using() {
    let mut eng = engine();
    run(&mut eng, "create type aggtype as (a int, b int, c text)").unwrap();
    run(
        &mut eng,
        "create function aggf_trans(aggstate aggtype[], a int, b int, c text) \
         returns aggtype[] as $$ select $1 || row($2,$3,$4)::aggtype $$ language sql",
    )
    .unwrap();
    run(
        &mut eng,
        "create aggregate aggfns(integer, integer, text) \
         (stype = aggtype[], sfunc = aggf_trans, initcond = '{}')",
    )
    .unwrap();
    // ORDER BY inside the aggregate call sorts the transition input.
    assert_eq!(
        one(
            &mut eng,
            "select aggfns(distinct a, b, c order by b) from \
             (values (1,3,'foo'),(0,null,null),(2,2,'bar')) t(a,b,c)"
        ),
        "{\"(2,2,bar)\",\"(1,3,foo)\",\"(0,,)\"}"
    );
}

#[test]
fn user_ordered_set_within_group() {
    let mut eng = engine();
    run(
        &mut eng,
        "create aggregate test_rank(variadic \"any\" order by variadic \"any\") \
         (stype = internal, sfunc = ordered_set_transition_multi, \
         finalfunc = rank_final, finalfunc_extra = true, hypothetical)",
    )
    .unwrap();
    assert_eq!(
        one(
            &mut eng,
            "select test_rank(30) within group (order by x) \
             from (values (10),(20),(30),(40)) v(x)"
        ),
        "3"
    );
    // A plain aggregate used with WITHIN GROUP is 42809.
    assert_eq!(
        err_code(
            &mut eng,
            "select sum(x) within group (order by x) from (values (1)) v(x)"
        ),
        "42809"
    );
}

#[test]
fn hypothetical_literal_coercion() {
    let mut eng = engine();
    // Untyped literals adopt the sort column's type.
    assert_eq!(
        one(
            &mut eng,
            "select rank('3') within group (order by x) from generate_series(1,5) x"
        ),
        "3"
    );
    // ... and fail with the target type's input error when invalid.
    assert_eq!(
        err_code(
            &mut eng,
            "select rank('fred') within group (order by x) from generate_series(1,5) x"
        ),
        "22P02"
    );
    // varchar unifies with text sort columns.
    assert_eq!(
        one(
            &mut eng,
            "select rank('adam'::varchar) within group (order by x) \
             from (values ('fred'),('jim')) v(x)"
        ),
        "1"
    );
}

#[test]
fn builtin_transition_functions_as_scalars() {
    let mut eng = engine();
    // CREATE AGGREGATE's support functions are callable directly.
    assert_eq!(one(&mut eng, "select int8inc(41::int8)"), "42");
    assert_eq!(
        rows_of(run(&mut eng, "select numeric_avg(numeric_avg_accum(null, 2.0))").unwrap()).len(),
        1
    );
}
