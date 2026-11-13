/// v2.26: PARTITION OF column-override grammar (PG19 gram.y
/// OptTypedTableElementList: `ColId [WITH OPTIONS] ColQualList` |
/// TableConstraint — no type names) merged per tablecmds.c
/// MergeAttributes' is_partition block. Ground truth:
/// src/test/regress/sql/{indexing,create_table,alter_table,
/// generated_virtual,generated_stored,identity,event_trigger}.sql
/// and hidden_files/v226/scope.md.
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

fn rows_text(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

#[test]
fn bare_column_list_elements() {
    let mut eng = engine();
    ok(&mut eng, "create table hp (i int) partition by hash (i)");
    ok(
        &mut eng,
        "create table hp0 partition of hp (i) for values with (modulus 2, remainder 0)",
    );
    ok(
        &mut eng,
        "create table hp1 partition of hp (i) for values with (modulus 2, remainder 1)",
    );
    ok(&mut eng, "insert into hp values (1), (2), (3)");
    assert_eq!(rows_text(&mut eng, "select i from hp order by i").len(), 3);
    // Unknown element column: 42703 (MergeAttributes).
    let e = err(
        &mut eng,
        "create table hpx partition of hp (nosuch) for values with (modulus 2, remainder 0)",
    );
    assert_eq!(e.code, "42703");
    // Same column twice: 42701.
    let e = err(
        &mut eng,
        "create table hpy partition of hp (i, i) for values with (modulus 2, remainder 0)",
    );
    assert_eq!(e.code, "42701");
}

#[test]
fn merged_default_notnull_and_table_constraint_elements() {
    // create_table.sql part_b, reduced to its scored core.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table parted (a text, b int) partition by list (a)",
    );
    // The triple-b form is PG's 42701 (checked before any merge).
    let e = err(
        &mut eng,
        "create table part_b partition of parted (b not null, b default 1, b check (b >= 0), constraint check_a check (length(a) > 0)) for values in ('b')",
    );
    assert_eq!(e.code, "42701");
    ok(
        &mut eng,
        "create table part_b partition of parted (b not null default 1, constraint check_a check (length(a) > 0), constraint check_b check (b >= 0)) for values in ('b')",
    );
    // Leaf-local NOT NULL judges rows routed through the parent, and
    // the error names the leaf (PG19 ExecInsert).
    let e = err(&mut eng, "insert into parted values ('b', null)");
    assert_eq!(e.code, "23502");
    assert!(e.message.contains("\"part_b\""), "{}", e.message);
    // Leaf-local CHECK likewise.
    let e = err(&mut eng, "insert into parted values ('b', -5)");
    assert_eq!(e.code, "23514");
    assert!(e.message.contains("check_b"), "{}", e.message);
    // The element DEFAULT applies to direct leaf inserts.
    ok(&mut eng, "insert into part_b (a) values ('b')");
    assert_eq!(
        rows_text(&mut eng, "select b from part_b"),
        vec![vec!["1".to_string()]]
    );
}

#[test]
fn with_options_default_and_subpartition() {
    // create_table.sql part_c shape.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table parted (a text, b int) partition by list (a)",
    );
    ok(
        &mut eng,
        "create table part_c partition of parted (b with options not null default 0) for values in ('c') partition by range ((b))",
    );
    ok(
        &mut eng,
        "create table part_c_1_10 partition of part_c for values from (1) to (10)",
    );
    ok(&mut eng, "insert into parted values ('c', 5)");
    assert_eq!(rows_text(&mut eng, "select b from part_c_1_10").len(), 1);
}

#[test]
fn generated_override_family() {
    // generated_stored/virtual gtest_child forms.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table gp (f1 date not null, f2 bigint, f3 bigint generated always as (f2 * 2) stored) partition by range (f1)",
    );
    ok(
        &mut eng,
        "create table gchild partition of gp for values from ('2016-07-01') to ('2016-08-01')",
    );
    // Same kind, different expression: legal override; the routed
    // insert computes with the LEAF's expression (66, not 6).
    ok(
        &mut eng,
        "create table gchild2 partition of gp (f3 with options generated always as (f2 * 22) stored) for values from ('2016-08-01') to ('2016-09-01')",
    );
    ok(&mut eng, "insert into gp values ('2016-08-15', 3)");
    assert_eq!(
        rows_text(&mut eng, "select f3 from gchild2"),
        vec![vec!["66".to_string()]]
    );
    // DEFAULT over a generated parent column: 42P16.
    let e = err(
        &mut eng,
        "create table gchild3 partition of gp (f3 default 42) for values from ('2016-09-01') to ('2016-10-01')",
    );
    assert_eq!(e.code, "42P16");
    // Kind mismatch: 42P16.
    let e = err(
        &mut eng,
        "create table gchild3 partition of gp (f3 generated always as (f2 * 2) virtual) for values from ('2016-09-01') to ('2016-10-01')",
    );
    assert_eq!(e.code, "42P16");
    // Generation expression over a plain parent column: 42P16.
    ok(
        &mut eng,
        "create table gp2 (f1 date not null, f2 bigint, f3 bigint) partition by range (f1)",
    );
    let e = err(
        &mut eng,
        "create table gp2c partition of gp2 (f3 with options generated always as (f2 * 2) virtual) for values from ('2016-07-01') to ('2016-08-01')",
    );
    assert_eq!(e.code, "42P16");
}

#[test]
fn identity_element_is_0a000_on_partitions() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table pit (f1 date not null, f2 text, f3 bigint) partition by range (f1)",
    );
    let e = err(
        &mut eng,
        "create table pit_pfail partition of pit (f3 with options generated always as identity) for values from ('2016-11-01') to ('2016-12-01')",
    );
    assert_eq!(e.code, "0A000");
    assert!(
        e.message
            .contains("identity columns are not supported on partitions"),
        "{}",
        e.message
    );
    // v2.28 supersedes the v2.26 standing rejection: plain CREATE
    // TABLE identity is now a real feature. The PARTITION OF element
    // rejection above is unchanged.
    ok(
        &mut eng,
        "create table plain_id (f3 bigint generated always as identity)",
    );
    ok(&mut eng, "insert into plain_id default values");
    assert_eq!(
        rows_text(&mut eng, "select f3 from plain_id"),
        vec![vec!["1"]]
    );
}

#[test]
fn primary_key_element_and_table_constraint_only_list() {
    let mut eng = engine();
    // indexing.sql idxpart1pk shape.
    ok(
        &mut eng,
        "create table ipk (a int, b text) partition by range (a)",
    );
    ok(
        &mut eng,
        "create table ipk1 partition of ipk (a primary key) for values from (0) to (100)",
    );
    ok(&mut eng, "insert into ipk1 values (1, 'x')");
    let e = err(&mut eng, "insert into ipk1 values (1, 'y')");
    assert_eq!(e.code, "23505");
    let e = err(&mut eng, "insert into ipk1 values (null, 'z')");
    assert_eq!(e.code, "23502");
    // alter_table.sql quuux_default1 shape: the list holds ONLY a
    // table constraint, enforced on rows routed through two levels.
    ok(
        &mut eng,
        "create table quuux (a int, b text) partition by list (a)",
    );
    ok(
        &mut eng,
        "create table quuux_default partition of quuux default partition by list (b)",
    );
    ok(
        &mut eng,
        "create table quuux_default1 partition of quuux_default (constraint check_1 check (a is not null and a = 1)) for values in ('b')",
    );
    let e = err(&mut eng, "insert into quuux values (2, 'b')");
    assert_eq!(e.code, "23514");
    assert!(e.message.contains("check_1"), "{}", e.message);
    ok(&mut eng, "insert into quuux values (1, 'b')");
}

#[test]
fn inherited_check_merges_instead_of_duplicating() {
    // part_b's check_a is identical to the parent's: PG merges
    // (NOTICE) rather than erroring or duplicating.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table pp (a text, b int, constraint check_a check (length(a) > 0)) partition by list (a)",
    );
    ok(
        &mut eng,
        "create table pp_b partition of pp (constraint check_a check (length(a) > 0)) for values in ('b')",
    );
    // One copy, enforced once, on the leaf.
    let e = err(&mut eng, "insert into pp values ('', 1)");
    assert_eq!(e.code, "23514");
    // Same name, different expression: 42710.
    ok(
        &mut eng,
        "create table pp2 (a text, b int, constraint cc check (b > 0)) partition by list (a)",
    );
    let e = err(
        &mut eng,
        "create table pp2_b partition of pp2 (constraint cc check (b > 5)) for values in ('b')",
    );
    assert_eq!(e.code, "42710");
}

#[test]
fn drop_inherited_constraint_on_partition_child() {
    // PG19 tablecmds.c ATExecDropConstraint: coninhcount > 0 &&
    // !recursing -> 42P16 "cannot drop inherited constraint".
    // Merged element checks stay inherited; purely local element
    // constraints drop; a drop on the parent recurses.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table dp (a int, b int, constraint check_a check (a > 0), constraint check_b check (b > 0)) partition by range (a)",
    );
    ok(
        &mut eng,
        "create table dp_b partition of dp (b not null default 1, constraint check_a check (a > 0), constraint check_b check (b > 0)) for values from (0) to (100)",
    );
    let e = err(&mut eng, "alter table dp_b drop constraint check_a");
    assert_eq!(e.code, "42P16");
    assert!(e.message.contains("cannot drop inherited constraint"));
    let e = err(&mut eng, "alter table dp_b drop constraint check_b");
    assert_eq!(e.code, "42P16");
    // A purely local element constraint is droppable on the child.
    ok(
        &mut eng,
        "create table dp_c partition of dp (constraint my_local check (b < 1000)) for values from (100) to (200)",
    );
    ok(&mut eng, "alter table dp_c drop constraint my_local");
    // Dropping on the parent recurses into the children: a = 0 is
    // inside dp_b's bound but violates check_a (a > 0).
    let e = err(&mut eng, "insert into dp values (0, 1)");
    assert_eq!(e.code, "23514");
    ok(&mut eng, "alter table dp drop constraint check_a");
    ok(&mut eng, "insert into dp values (0, 1)");
    let e = err(&mut eng, "insert into dp_b values (5, -1)");
    assert_eq!(e.code, "23514");
    // FK copies are renamed per child; the guard matches by shape.
    ok(&mut eng, "create table dppk (a int primary key)");
    ok(&mut eng, "create table dfp (a int) partition by list (a)");
    ok(
        &mut eng,
        "create table dfp_1 partition of dfp (foreign key (a) references dppk) for values in (1)",
    );
    ok(
        &mut eng,
        "alter table dfp add foreign key (a) references dppk",
    );
    let e = err(&mut eng, "alter table dfp_1 drop constraint dfp_1_a_fkey");
    assert_eq!(e.code, "42P16");
    ok(&mut eng, "alter table dfp drop constraint dfp_a_fkey");
}

#[test]
fn temp_partition_of_with_override() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create temp table tp (a int, b text) partition by range (a)",
    );
    ok(
        &mut eng,
        "create temp table tp1 partition of tp (b default 'dflt') for values from (0) to (10)",
    );
    ok(&mut eng, "insert into tp1 (a) values (5)");
    assert_eq!(
        rows_text(&mut eng, "select b from tp1"),
        vec![vec!["dflt".to_string()]]
    );
}
