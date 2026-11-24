// ---------------------------------------------------------------------------
// v2.40: arrays remainder — the array function surface (array_sort /
// array_fill / array_position family / string_to_table /
// generate_subscripts), ANY/ALL typing, array containment and
// ordering, literal strictness, and slice-form subscript assignment.
// ---------------------------------------------------------------------------
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = crate::sql::parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
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

fn one(eng: &mut Engine, sql: &str) -> String {
    let rows = rows_of(run(eng, sql).unwrap());
    assert_eq!(rows.len(), 1, "{sql}");
    rows.into_iter().next().unwrap().into_iter().next().unwrap()
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql).unwrap_err().code.to_string()
}

#[test]
fn array_function_surface() {
    let mut eng = engine();
    assert_eq!(one(&mut eng, "select array_sort(array[3,1,2])"), "{1,2,3}");
    assert_eq!(
        one(&mut eng, "select array_sort(array[1,null,2], true)"),
        "{NULL,2,1}"
    );
    assert_eq!(
        one(&mut eng, "select array_fill(7, array[2,3], array[5,10])"),
        "[5:6][10:12]={{7,7,7},{7,7,7}}"
    );
    assert_eq!(one(&mut eng, "select array_fill(42, '{}')"), "{}");
    assert_eq!(
        err_code(&mut eng, "select array_fill(1, null, array[2,2])"),
        "22004"
    );
    assert_eq!(
        err_code(&mut eng, "select array_fill(1, array[1,2,null])"),
        "22004"
    );
    assert_eq!(
        err_code(&mut eng, "select array_fill(1, array[[1,2],[3,4]])"),
        "2202E"
    );
    assert_eq!(
        one(&mut eng, "select array_position(array[10,20,30,20], 20, 3)"),
        "4"
    );
    assert_eq!(
        one(&mut eng, "select array_positions(array[10,20,30,20], 20)"),
        "{2,4}"
    );
    assert_eq!(
        err_code(&mut eng, "select array_position(array[[1,2],[3,4]], 3)"),
        "0A000"
    );
    assert_eq!(
        one(&mut eng, "select array_remove(array[1,2,3,2], 2)"),
        "{1,3}"
    );
    assert_eq!(
        err_code(&mut eng, "select array_remove(array[[1,2],[3,4]], 2)"),
        "0A000"
    );
    assert_eq!(
        one(&mut eng, "select array_replace(array[1,2,3], 2, 9)"),
        "{1,9,3}"
    );
    assert_eq!(
        one(&mut eng, "select array_reverse(array[1,2,3])"),
        "{3,2,1}"
    );
    assert_eq!(one(&mut eng, "select trim_array(array[1,2,3], 1)"), "{1,2}");
    assert_eq!(
        err_code(&mut eng, "select trim_array(array[1,2,3], 10)"),
        "2202E"
    );
    assert_eq!(
        one(
            &mut eng,
            "select array_to_string(array[1,null,3], ',', '*')"
        ),
        "1,*,3"
    );
    assert_eq!(
        one(&mut eng, "select array_cat(array[1,2], array[[3,4]])"),
        "{{1,2},{3,4}}"
    );
    assert_eq!(
        one(&mut eng, "select array_append(array[1,2], 3)"),
        "{1,2,3}"
    );
    assert_eq!(
        one(&mut eng, "select array_prepend(0, array[1,2])"),
        "{0,1,2}"
    );
    // array_sort on xid: >1 first-dim elements need a comparison
    // function (42883); a single element short-circuits like PG.
    assert_eq!(
        err_code(&mut eng, "select array_sort('{1,2}'::xid[])"),
        "42883"
    );
    assert_eq!(one(&mut eng, "select array_sort('{1}'::xid[])"), "{1}");
}

#[test]
fn srf_in_from_and_targetlist() {
    let mut eng = engine();
    let rows = rows_of(run(&mut eng, "select * from string_to_table('a|b|c', '|')").unwrap());
    assert_eq!(rows, vec![vec!["a"], vec!["b"], vec!["c"]]);
    let rows = rows_of(
        run(
            &mut eng,
            "select * from generate_subscripts(array[10,20,30], 1)",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["1"], vec!["2"], vec!["3"]]);
    let rows = rows_of(run(&mut eng, "select string_to_table('x,y', ',')").unwrap());
    assert_eq!(rows, vec![vec!["x"], vec!["y"]]);
    // A polymorphic SQL function over generate_subscripts (the
    // arrays.sql unnest1 shape).
    run(
        &mut eng,
        "create function unnest1(anyarray) returns setof anyelement as \
         $$ select $1[s] from generate_subscripts($1,1) g(s); $$ language sql",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "select * from unnest1(array[1,2,3])").unwrap());
    assert_eq!(rows, vec![vec!["1"], vec!["2"], vec!["3"]]);
}

#[test]
fn any_all_and_like_quantifiers() {
    let mut eng = engine();
    assert_eq!(one(&mut eng, "select 33 = any('{1,2,33}')"), "t");
    assert_eq!(one(&mut eng, "select 33 = all('{1,2,33}')"), "f");
    assert_eq!(one(&mut eng, "select null::int >= all('{}')"), "t");
    assert_eq!(
        one(&mut eng, "select 'abc' like any(array['a%','b%'])"),
        "t"
    );
    assert_eq!(one(&mut eng, "select 'abc' not like all(array['a%'])"), "f");
    assert_eq!(one(&mut eng, "select 'ABC' ilike any(array['a%'])"), "t");
    // The result column is ?column?, not the hidden builtin's name.
    let r = run(&mut eng, "select 1 = any('{1}')").unwrap();
    match r {
        ExecResult::Select { columns, .. } => assert_eq!(columns[0].0, "?column?"),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

#[test]
fn containment_ordering_equality() {
    let mut eng = engine();
    assert_eq!(one(&mut eng, "select array[1,2,3] @> '{2}'"), "t");
    assert_eq!(one(&mut eng, "select array[1,2,3] <@ array[1,2,3,4]"), "t");
    assert_eq!(one(&mut eng, "select array[1,2] && array[3,4]"), "f");
    // PG19 array_contain_compare: NULL elements never match.
    assert_eq!(one(&mut eng, "select array[1,null] @> '{null}'"), "f");
    assert_eq!(one(&mut eng, "select array[1,null] && '{null}'"), "f");
    // PG19 array_eq: NULL = NULL, and lower bounds participate.
    assert_eq!(one(&mut eng, "select array[1,null] = '{1,null}'"), "t");
    assert_eq!(
        one(
            &mut eng,
            "select '[0:1]={1,2}'::int[] = '[1:2]={1,2}'::int[]"
        ),
        "f"
    );
    // array_cmp ordering (min/max, ORDER BY, operators).
    assert_eq!(one(&mut eng, "select array[1,2] < array[1,3]"), "t");
    run(&mut eng, "create temp table mx (x int[])").unwrap();
    run(&mut eng, "insert into mx values (array[1,2]), (array[1,3])").unwrap();
    assert_eq!(one(&mut eng, "select max(x) from mx"), "{1,3}");
    // width_bucket array form + its 42883/2202E edges.
    assert_eq!(
        one(&mut eng, "select width_bucket(5, array[1.0,3.0,9.0])"),
        "2"
    );
    assert_eq!(
        err_code(&mut eng, "select width_bucket('5'::text, array[3,4])"),
        "42883"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "select width_bucket(5, array[array[1,2],array[3,4]])"
        ),
        "2202E"
    );
}

#[test]
fn literal_strictness() {
    let mut eng = engine();
    assert_eq!(err_code(&mut eng, "select '{foo,,bar}'::text[]"), "22P02");
    assert_eq!(err_code(&mut eng, "select '{1,}'::int[]"), "22P02");
    assert_eq!(err_code(&mut eng, "select '{a\"b\"}'::text[]"), "22P02");
    assert_eq!(
        err_code(&mut eng, "select '[2147483646:2147483647]={1,2}'::int[]"),
        "54000"
    );
    assert_eq!(one(&mut eng, "select '[2]={1,7}'::int[]"), "{1,7}");
    // An escaped NULL spelling is the string "null", not SQL NULL.
    assert_eq!(
        one(&mut eng, r#"select '{null,n\ull,"null"}'::text[]"#),
        "{NULL,\"null\",\"null\"}"
    );
    // A cast drives hard per-element coercion of a constructor.
    assert_eq!(
        one(&mut eng, "select array[1, 3, 'NaN']::float8[]"),
        "{1,3,NaN}"
    );
    // Nested constructors stack dimensions regardless of spelling.
    assert_eq!(
        one(&mut eng, "select array[array['hello'],array['world']]"),
        "{{hello},{world}}"
    );
    // Slice fetch with a NULL bound is NULL as a whole.
    assert_eq!(
        one(&mut eng, "select ('[0:4]={0,1,2,3,4}'::int[])[1:null]"),
        "NULL"
    );
}

#[test]
fn slice_assignment_forms() {
    let mut eng = engine();
    run(&mut eng, "create temp table st (a int[], b int[][])").unwrap();
    run(
        &mut eng,
        "insert into st (a[1:3], b) values ('{7,8,9}', '{{1,2},{3,4}}')",
    )
    .unwrap();
    assert_eq!(one(&mut eng, "select a from st"), "{7,8,9}");
    run(&mut eng, "update st set a[2:3] = '{80,90}'").unwrap();
    assert_eq!(one(&mut eng, "select a from st"), "{7,80,90}");
    run(&mut eng, "update st set a[:2] = '{11,12}'").unwrap();
    assert_eq!(one(&mut eng, "select a from st"), "{11,12,90}");
    run(&mut eng, "update st set a[2:] = '{22,23}'").unwrap();
    assert_eq!(one(&mut eng, "select a from st"), "{11,22,23}");
    // Multiple subscripted assignments to one column compose.
    run(
        &mut eng,
        "update st set b[1:1][1:1] = '{101}', b[2:2][2:2] = '{104}'",
    )
    .unwrap();
    assert_eq!(one(&mut eng, "select b from st"), "{{101,2},{3,104}}");
    // A NULL provided bound in an assignment is 22004.
    assert_eq!(
        err_code(&mut eng, "update st set a[1:null] = '{1}'"),
        "22004"
    );
    // The keyword array-bound type form.
    run(&mut eng, "create temp table kt (x integer array[4])").unwrap();
    run(&mut eng, "insert into kt values (array[1,2])").unwrap();
    assert_eq!(one(&mut eng, "select x from kt"), "{1,2}");
}

#[test]
fn upsert_subscript_targets() {
    let mut eng = engine();
    run(
        &mut eng,
        "create temp table pk (pk int primary key, f1 int[])",
    )
    .unwrap();
    run(&mut eng, "insert into pk values (1, '{1,2,3}')").unwrap();
    run(
        &mut eng,
        "insert into pk (pk, f1[1:2]) values (1, '{6,7}') \
         on conflict (pk) do update set f1[1:2] = excluded.f1[1:2], f1[3] = 7",
    )
    .unwrap();
    assert_eq!(one(&mut eng, "select f1 from pk"), "{6,7,7}");
}
