use super::*;

// v2.16: the ALTER TABLE family — COMMENT ON for the general object
// family, ALTER INDEX, RENAME CONSTRAINT with PG19 locality rules,
// ADD/DROP ... IF [NOT] EXISTS, SET SCHEMA (tables/views/functions),
// SET LOGGED/UNLOGGED acceptance, CLUSTER ON, column SET STATISTICS
// / SET (options), CHECK ... NO INHERIT + NOT VALID validation
// semantics, unnamed-CHECK naming (<table>_<col>_check), system
// column names (42701), serial2/4/8 aliases, DELETE/UPDATE ONLY,
// and COLLATE/STORAGE column clauses. PG19 grounding: tablecmds.c
// ATExec* family, gram.y AlterTableStmt/CommentStmt, heap.c
// system column names. Harness shape copied from v214.

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(216000);

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let xid = NEXT_XID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Preserve the parser's SQLSTATE (sql_err flattens to 42601;
    // several v2.16 checks are parse-time with specific codes).
    let stmt = crate::sql::parse_statement(sql).map_err(|e| ExecError {
        code: e.code,
        message: e.message,
        detail: None,
    })?;
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
        privs_suppressed: false,
        snap: &snap,
        own: xid,
        write_xid: xid,
        all_xids: vec![xid],
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

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql)
        .map(|_| "OK".to_string())
        .unwrap_or_else(|e| e.code.to_string())
}

#[test]
fn v216_comment_on_family() {
    let mut eng = engine();
    ok(&mut eng, "create table ct (a int primary key, b int)");
    ok(&mut eng, "create index ct_b_idx on ct (b)");
    ok(&mut eng, "create view cv as select a from ct");
    ok(&mut eng, "create sequence cs");
    ok(&mut eng, "comment on table ct is 'x'");
    assert_eq!(err_code(&mut eng, "comment on table nope is 'x'"), "42P01");
    ok(&mut eng, "comment on index ct_b_idx is 'x'");
    assert_eq!(err_code(&mut eng, "comment on index nope is 'x'"), "42P01");
    ok(&mut eng, "comment on column ct.a is 'x'");
    assert_eq!(
        err_code(&mut eng, "comment on column ct.zz is 'x'"),
        "42703"
    );
    assert_eq!(
        err_code(&mut eng, "comment on column nope.a is 'x'"),
        "42P01"
    );
    ok(&mut eng, "comment on view cv is 'x'");
    ok(&mut eng, "comment on sequence cs is 'x'");
    assert_eq!(
        err_code(&mut eng, "comment on sequence nope is 'x'"),
        "42P01"
    );
    ok(&mut eng, "comment on role postgres is 'x'");
    assert_eq!(err_code(&mut eng, "comment on role nope is 'x'"), "42704");
}

#[test]
fn v216_alter_index_family() {
    let mut eng = engine();
    ok(&mut eng, "create table ait (a int, b int)");
    ok(&mut eng, "create index ait_idx on ait (a, (b + 1))");
    ok(&mut eng, "alter index ait_idx rename to ait_idx2");
    // Expression column (position 2) may take a statistics target;
    // a plain column may not (0A000); column 0 is a range error.
    ok(
        &mut eng,
        "alter index ait_idx2 alter column 2 set statistics 100",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "alter index ait_idx2 alter column 1 set statistics 100"
        ),
        "0A000"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "alter index ait_idx2 alter column 0 set statistics 100"
        ),
        "42601"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "alter index ait_idx2 alter column 9 set statistics 100"
        ),
        "42703"
    );
    ok(&mut eng, "alter index ait_idx2 set (fillfactor = 70)");
    assert_eq!(
        err_code(&mut eng, "alter index ait_idx2 set (nope = 1)"),
        "22023"
    );
    ok(&mut eng, "alter index if exists nope rename to x");
    assert_eq!(err_code(&mut eng, "alter index nope rename to x"), "42P01");
    // Renaming a unique constraint's index renames the constraint too.
    ok(&mut eng, "create table uq (a int unique)");
    ok(&mut eng, "alter index uq_a_key rename to uq_a_key2");
    ok(&mut eng, "alter table uq drop constraint uq_a_key2");
}

#[test]
fn v216_rename_constraint_and_locality() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table p (a int constraint c1 check (a > 0), b int)",
    );
    ok(&mut eng, "create table c () inherits (p)");
    // Inherited copy cannot be renamed on the child (42P16).
    assert_eq!(
        err_code(&mut eng, "alter table c rename constraint c1 to c1x"),
        "42P16"
    );
    // ONLY on the parent cannot rename while children carry it.
    assert_eq!(
        err_code(&mut eng, "alter table only p rename constraint c1 to c1x"),
        "42P16"
    );
    // Plain rename propagates to the child.
    ok(&mut eng, "alter table p rename constraint c1 to c1x");
    ok(&mut eng, "alter table c drop constraint c1x");
    assert_eq!(
        err_code(&mut eng, "alter table p rename constraint nope to x"),
        "42704"
    );
    assert_eq!(
        err_code(&mut eng, "alter table p add constraint c1x check (b > 0)"),
        "42710"
    );
}

#[test]
fn v216_if_exists_forms() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table t (a int, constraint tck check (a > 0))",
    );
    ok(&mut eng, "alter table t add column if not exists b int");
    ok(&mut eng, "alter table t add column if not exists b int");
    assert_eq!(
        err_code(&mut eng, "alter table t add column b int"),
        "42701"
    );
    ok(&mut eng, "alter table t drop column if exists nope");
    ok(&mut eng, "alter table t drop constraint if exists nope");
    ok(&mut eng, "alter table t drop constraint if exists tck");
    assert_eq!(
        err_code(&mut eng, "alter table t drop constraint tck"),
        "42704"
    );
}

#[test]
fn v216_set_schema() {
    let mut eng = engine();
    ok(&mut eng, "create schema s2");
    ok(&mut eng, "create table mv (a int)");
    ok(&mut eng, "insert into mv values (1)");
    ok(&mut eng, "alter table mv set schema s2");
    // Qualified reference resolves through the schema attribution.
    ok(&mut eng, "select a from s2.mv");
    assert_eq!(
        err_code(&mut eng, "alter table mv set schema nope"),
        "3F000"
    );
    // Views move too, and functions.
    ok(&mut eng, "create view vw as select a from mv");
    ok(&mut eng, "alter table vw set schema s2");
    ok(
        &mut eng,
        "create function fn1() returns int language sql as 'select 1'",
    );
    ok(&mut eng, "alter function fn1() set schema s2");
    assert_eq!(
        err_code(&mut eng, "alter function nope() set schema s2"),
        "42883"
    );
}

#[test]
fn v216_check_no_inherit_not_valid() {
    let mut eng = engine();
    ok(&mut eng, "create table par (a int)");
    ok(&mut eng, "create table chi () inherits (par)");
    ok(&mut eng, "insert into chi values (1)");
    ok(&mut eng, "insert into par values (20)");
    // NOT VALID skips existing rows; NO INHERIT keeps the child out.
    ok(
        &mut eng,
        "alter table par add constraint ck check (a > 10) no inherit not valid",
    );
    ok(&mut eng, "alter table par validate constraint ck");
    // A violating parent row blocks validation (23514).
    ok(&mut eng, "create table par2 (a int)");
    ok(&mut eng, "insert into par2 values (2)");
    ok(
        &mut eng,
        "alter table par2 add constraint ck2 check (a > 10) not valid",
    );
    assert_eq!(
        err_code(&mut eng, "alter table par2 validate constraint ck2"),
        "23514"
    );
    // NOT ENFORCED constraints cannot be validated (55000).
    ok(
        &mut eng,
        "alter table par add constraint ck3 check (a > 0) not enforced",
    );
    assert_eq!(
        err_code(&mut eng, "alter table par validate constraint ck3"),
        "55000"
    );
    // Unnamed CHECKs are named <table>_<firstcol>_check.
    ok(&mut eng, "create table nv (d int)");
    ok(&mut eng, "alter table nv add check (d > 0) not valid");
    ok(&mut eng, "alter table nv validate constraint nv_d_check");
}

#[test]
fn v216_system_columns_and_serial_aliases() {
    let mut eng = engine();
    assert_eq!(err_code(&mut eng, "create table bad (xmin int)"), "42701");
    assert_eq!(err_code(&mut eng, "create table bad2 (ctid text)"), "42701");
    ok(&mut eng, "create table good (a int)");
    assert_eq!(
        err_code(&mut eng, "alter table good add column tableoid int"),
        "42701"
    );
    // oid is NOT a system column name in PG19 heap.c.
    ok(&mut eng, "create table withoid (oid int)");
    ok(
        &mut eng,
        "create table ser (a serial8, b serial4, c serial2)",
    );
    ok(&mut eng, "insert into ser default values");
    ok(&mut eng, "insert into ser default values");
}

#[test]
fn v216_cluster_stats_and_reloptions() {
    let mut eng = engine();
    ok(&mut eng, "create table cl (a int primary key, b int)");
    ok(&mut eng, "alter table cl cluster on cl_pkey");
    ok(&mut eng, "alter table cl set without cluster");
    assert_eq!(
        err_code(&mut eng, "alter table cl cluster on nope"),
        "42P01"
    );
    ok(&mut eng, "alter table cl alter column a set statistics 500");
    assert_eq!(
        err_code(&mut eng, "alter table cl alter column zz set statistics 1"),
        "42703"
    );
    ok(
        &mut eng,
        "alter table cl alter column a set (n_distinct = 0.5)",
    );
    assert_eq!(
        err_code(&mut eng, "alter table cl alter column a set (nope = 1)"),
        "22023"
    );
    ok(&mut eng, "alter table cl reset (autovacuum_enabled)");
    ok(&mut eng, "alter table cl set unlogged");
    ok(&mut eng, "alter table cl set logged");
}

#[test]
fn v216_delete_update_only_and_column_clauses() {
    let mut eng = engine();
    ok(&mut eng, "create table dp (a int)");
    ok(&mut eng, "create table dc () inherits (dp)");
    ok(&mut eng, "insert into dp values (1)");
    ok(&mut eng, "insert into dc values (2)");
    ok(&mut eng, "delete from only dp where a = 1");
    ok(&mut eng, "update only dp set a = 5 where a = 9");
    // COLLATE / STORAGE column clauses are consumed.
    ok(
        &mut eng,
        "create table cs (a text collate \"C\", b int storage plain)",
    );
    // ALTER TABLE on a view / index renames them.
    ok(&mut eng, "create view vv as select a from dp");
    ok(&mut eng, "alter table vv rename to vv2");
    ok(&mut eng, "select a from vv2");
    ok(&mut eng, "create index dp_a_idx on dp (a)");
    ok(&mut eng, "alter table dp_a_idx rename to dp_a_idx2");
    ok(&mut eng, "drop index dp_a_idx2");
}

#[test]
fn v216_inherit_generation_conflict() {
    // PG19 MergeAttributesIntoExisting: ALTER ... INHERIT applies the
    // same generation-presence/kind rules as CREATE ... INHERITS
    // (finishing-pass fix; generated_stored gtest_normal_child).
    let mut eng = engine();
    ok(&mut eng, "create table gp (a int, b int)");
    ok(
        &mut eng,
        "create table gc (a int, b int generated always as (a * 2) stored)",
    );
    assert_eq!(err_code(&mut eng, "alter table gc inherit gp"), "42804");
    // The mirror image: parent generated, child plain.
    ok(
        &mut eng,
        "create table gp2 (a int, b int generated always as (a * 2) stored)",
    );
    ok(&mut eng, "create table gc2 (a int, b int)");
    assert_eq!(err_code(&mut eng, "alter table gc2 inherit gp2"), "42804");
    // Matching generated columns (same kind) inherit cleanly.
    ok(
        &mut eng,
        "create table gc3 (a int, b int generated always as (a * 2) stored)",
    );
    ok(&mut eng, "alter table gc3 inherit gp2");
}

#[test]
fn v216_inherits_storage_conflict() {
    // PG19 MergeChildAttribute: differing effective storage for a
    // merged column is a 42804 conflict, across parents and between
    // the child's own (or LIKE ... INCLUDING STORAGE) definition and
    // the inherited one (create_table_like inh_error1/inh_error2).
    let mut eng = engine();
    ok(&mut eng, "create table st1 (a text, b text)");
    ok(&mut eng, "alter table st1 alter column a set storage main");
    ok(&mut eng, "create table st4 (a text, c text)");
    assert_eq!(
        err_code(&mut eng, "create table bad1 () inherits (st1, st4)"),
        "42804"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "create table bad2 (like st4 including storage) inherits (st1)"
        ),
        "42804"
    );
    // Matching storage merges cleanly.
    ok(&mut eng, "create table st3 (a text, c text)");
    ok(&mut eng, "alter table st3 alter column a set storage main");
    ok(
        &mut eng,
        "create table good1 (like st3 including storage) inherits (st1)",
    );
}
