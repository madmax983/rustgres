/// v2.25: per-leaf unique enforcement for INSERT...SELECT
/// (indexing.sql idxpart) + numeric/float assignment coercion.
/// (1) A UNIQUE INDEX on a partitioned parent has a child index on
/// every leaf in PG19 (DefineIndex) and ExecInsert enforces the
/// leaf's indexes (nodeModifyTable.c / execIndexing.c) — 23505 names
/// the leaf's child index. (2) INSERT...SELECT applies the assignment
/// cast to source rows, and numeric/float -> integer is an assignment
/// cast in PG (pg_cast.dat castcontext 'a'; numeric.c numeric_int4
/// rounds, float.c dtoi4 applies rint()). Ground truth:
/// src/test/regress/sql/indexing.sql, "Test that unique constraints
/// are working".
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

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    match run(eng, sql) {
        Ok(_) => panic!("{sql} -> expected error, got success"),
        Err(e) => e,
    }
}

fn setup_idxpart(eng: &mut Engine) {
    ok(
        eng,
        "create table idxpart (a int, b text, primary key (a, b)) partition by range (a)",
    );
    ok(
        eng,
        "create table idxpart1 partition of idxpart for values from (0) to (100000)",
    );
    ok(
        eng,
        "create table idxpart2 partition of idxpart for values from (100000) to (1000000)",
    );
    ok(eng, "create unique index on idxpart (a)");
}

#[test]
fn partitioned_unique_index_enforced_on_values_and_select() {
    let mut eng = engine();
    setup_idxpart(&mut eng);
    ok(&mut eng, "insert into idxpart values (100, 'x')");
    // VALUES into the parent, same (a), different (b): the PK (a,b)
    // cannot catch it; the parent unique index's child must.
    let e = err(&mut eng, "insert into idxpart values (100, 'other')");
    assert_eq!(e.code, "23505");
    assert!(e.message.contains("idxpart1_a_idx"), "{}", e.message);
    // Direct leaf insert.
    let e = err(&mut eng, "insert into idxpart1 values (100, 'other')");
    assert_eq!(e.code, "23505");
    assert!(e.message.contains("idxpart1_a_idx"), "{}", e.message);
    // INSERT...SELECT into the parent.
    let e = err(&mut eng, "insert into idxpart select 100, 'other'");
    assert_eq!(e.code, "23505");
    assert!(e.message.contains("idxpart1_a_idx"), "{}", e.message);
    // INSERT...SELECT direct into the leaf.
    let e = err(&mut eng, "insert into idxpart1 select 100, 'other'");
    assert_eq!(e.code, "23505");
    // In-batch duplicate via SELECT (two rows, same key, empty leaf range).
    let e = err(
        &mut eng,
        "insert into idxpart select g, 'x' from (select 500 as g union all select 500) s",
    );
    assert_eq!(e.code, "23505");
    // A non-conflicting row still lands, in the right leaf.
    ok(&mut eng, "insert into idxpart select 200000, 'y'");
    let e = err(&mut eng, "insert into idxpart values (200000, 'other')");
    assert_eq!(e.code, "23505");
    assert!(e.message.contains("idxpart2_a_idx"), "{}", e.message);
}

#[test]
fn partitioned_unique_index_null_keys_never_conflict() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table np (a int, b int) partition by range (b)",
    );
    ok(
        &mut eng,
        "create table np1 partition of np for values from (0) to (100)",
    );
    ok(&mut eng, "create unique index on np (a)");
    ok(&mut eng, "insert into np values (null, 1), (null, 2)");
    ok(&mut eng, "insert into np select null, 3");
}

#[test]
fn create_unique_index_on_partitioned_validates_existing_rows() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table cp (a int, b text) partition by range (a)",
    );
    ok(
        &mut eng,
        "create table cp1 partition of cp for values from (0) to (100)",
    );
    ok(&mut eng, "insert into cp values (5, 'x'), (5, 'y')");
    let e = err(&mut eng, "create unique index on cp (a)");
    assert_eq!(e.code, "23505");
    assert!(e.message.contains("cp1_a_idx"), "{}", e.message);
    // A non-unique index over the same data is fine.
    ok(&mut eng, "create index on cp (a)");
}

#[test]
fn attach_partition_validates_ancestor_unique_index() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table ap (a int, b text) partition by range (a)",
    );
    ok(
        &mut eng,
        "create table ap1 partition of ap for values from (0) to (100)",
    );
    ok(&mut eng, "create unique index on ap (a)");
    ok(&mut eng, "create table apx (a int, b text)");
    ok(&mut eng, "insert into apx values (107, 'x'), (107, 'y')");
    let e = err(
        &mut eng,
        "alter table ap attach partition apx for values from (100) to (200)",
    );
    assert_eq!(e.code, "23505");
    // Clean data attaches and is then enforced.
    ok(&mut eng, "delete from apx where b = 'y'");
    ok(
        &mut eng,
        "alter table ap attach partition apx for values from (100) to (200)",
    );
    let e = err(&mut eng, "insert into ap values (107, 'z')");
    assert_eq!(e.code, "23505");
    assert!(e.message.contains("apx_a_idx"), "{}", e.message);
}

#[test]
fn numeric_and_float_assignment_casts_to_int() {
    let mut eng = engine();
    ok(&mut eng, "create table ct (a int, b smallint, c bigint)");
    // numeric -> int rounds half away from zero (numeric_int4).
    ok(
        &mut eng,
        "insert into ct values (2.5::numeric, 3.5::numeric, -2.5::numeric)",
    );
    // float -> int applies rint() (half to even): 2.5 -> 2, 3.5 -> 4.
    // (Via INSERT...SELECT, whose rows are evaluated expressions; the
    // VALUES builder folds `2.5::float8` to a decimal literal first —
    // a pre-existing folding gap, see scope.md hold-backs.)
    ok(
        &mut eng,
        "insert into ct select 2.5::float8, 3.5::float8, 1e10",
    );
    // The indexing.sql shape: 2^16 is numeric.
    ok(&mut eng, "insert into ct values (2^16, 2^7, 2^40)");
    match run(&mut eng, "select a, b, c from ct order by a") {
        Ok(ExecResult::Select { rows, .. }) => {
            let got: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
                .collect();
            assert_eq!(
                got,
                vec![
                    vec!["2", "4", "10000000000"],
                    vec!["3", "4", "-3"],
                    vec!["65536", "128", "1099511627776"],
                ]
            );
        }
        other => panic!("select -> {:?}", other.map(|_| ())),
    }
    // Overflow is 22003, NaN numeric is 0A000, NaN float is 22003.
    let e = err(
        &mut eng,
        "insert into ct values (2147483648::numeric, 1, 1)",
    );
    assert_eq!(e.code, "22003");
    let e = err(&mut eng, "insert into ct select 'NaN'::numeric, 1, 1");
    assert_eq!(e.code, "0A000");
    let e = err(&mut eng, "insert into ct select 'Infinity'::numeric, 1, 1");
    assert_eq!(e.code, "0A000");
    let e = err(&mut eng, "insert into ct select 'NaN'::float8, 1, 1");
    assert_eq!(e.code, "22003");
    // INSERT...SELECT applies the same assignment cast (was: raw
    // numeric stored into the integer column).
    ok(&mut eng, "create table cs (a int)");
    ok(&mut eng, "insert into cs select 2^10");
    match run(&mut eng, "select pg_typeof(a) from cs") {
        Ok(ExecResult::Select { rows, .. }) => {
            assert_eq!(rows[0][0].to_text().as_deref(), Some("integer"));
        }
        other => panic!("select -> {:?}", other.map(|_| ())),
    }
}

#[test]
fn insert_select_assignment_casts_text_oid_datetime() {
    // PG assignment context: any type -> text via its output
    // function, int -> oid, timestamp -> timestamptz (all
    // castcontext 'a' / CoerceViaIO). These fired 42804 when the
    // INSERT...SELECT builder started applying assignment coercion
    // without them (v2.25 schedule run 1: largeobject, stats_ext,
    // collate, horology clusters).
    let mut eng = engine();
    ok(&mut eng, "create table tc (a text, b oid, c timestamptz)");
    ok(
        &mut eng,
        "insert into tc select 42, 1234, timestamp '2026-01-02 03:04:05'",
    );
    match run(&mut eng, "select a, b, pg_typeof(c) from tc") {
        Ok(ExecResult::Select { rows, .. }) => {
            assert_eq!(rows[0][0].to_text().as_deref(), Some("42"));
            assert_eq!(rows[0][1].to_text().as_deref(), Some("1234"));
            assert_eq!(
                rows[0][2].to_text().as_deref(),
                Some("timestamp with time zone")
            );
        }
        other => panic!("select -> {:?}", other.map(|_| ())),
    }
}

#[test]
fn recursive_cte_wider_recursive_term_is_42804() {
    // PG19 parse_cte.c: the non-recursive term's type is the overall
    // type; a recursive term resolving WIDER (numeric over an integer
    // seed) errors instead of narrowing through an assignment cast.
    let mut eng = engine();
    let e = err(
        &mut eng,
        "with recursive foo(i) as (select i from (values(1),(2)) t(i) \
         union all select (i+1)::numeric(10,0) from foo where i < 10) \
         select * from foo",
    );
    assert_eq!(e.code, "42804");
    // The narrowing direction (seed numeric, term int) resolves to
    // the seed type and runs.
    ok(
        &mut eng,
        "with recursive bar(i) as (select 1::numeric union all \
         select i+1 from bar where i < 5) select * from bar",
    );
}

#[test]
fn partitioned_parent_order_by_sorts_globally() {
    // v2.25: plan_order_scan must not certify ORDER BY from a
    // partitioned parent's index — its tree covers no leaf rows.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table opq (a int, b text, primary key (a, b)) partition by range (a)",
    );
    ok(
        &mut eng,
        "create table opq1 partition of opq for values from (0) to (100)",
    );
    ok(
        &mut eng,
        "create table opq2 partition of opq for values from (100) to (200)",
    );
    ok(&mut eng, "create unique index on opq (a)");
    ok(
        &mut eng,
        "insert into opq values (150,'x'),(50,'y'),(120,'z'),(10,'w')",
    );
    match run(&mut eng, "select a from opq order by a") {
        Ok(ExecResult::Select { rows, .. }) => {
            let got: Vec<String> = rows
                .iter()
                .map(|r| r[0].to_text().unwrap_or_default())
                .collect();
            assert_eq!(got, vec!["10", "50", "120", "150"]);
        }
        other => panic!("select -> {:?}", other.map(|_| ())),
    }
}
