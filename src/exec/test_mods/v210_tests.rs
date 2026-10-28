use super::*;

// v2.10: trigger-body DML + pg_trigger_depth. PG19 grounding:
// commands/trigger.c pg_trigger_depth() + ExecCallTriggerFunc depth
// counter, plpgsql SPI execution of body statements in the caller's
// transaction, EXCEPTION blocks as subtransactions, FOUND semantics,
// ExecBRUpdateTriggers / ExecBRDeleteTriggers for the BEFORE hooks.
// Harness shape copied from v209 (each statement its own xid,
// session 0, never commits).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(92000);

fn run_full(eng: &mut Engine, sql: &str) -> (Result<ExecResult, ExecError>, Vec<String>) {
    let xid = NEXT_XID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stmt = crate::sql::parse_statement(sql).expect("parses");
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
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
    let r = execute(eng, &mut ctx, &stmt);
    (r, ctx.notices)
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_full(eng, sql).0
}

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

fn notices(eng: &mut Engine, sql: &str) -> Vec<String> {
    let (r, n) = run_full(eng, sql);
    r.unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
    n
}

fn one_int(eng: &mut Engine, sql: &str) -> i64 {
    match run(eng, sql).unwrap() {
        ExecResult::Select { rows, .. } => match &rows[0][0] {
            Value::Int(i) => *i,
            Value::BigInt(i) => *i,
            other => panic!("not an int: {other:?}"),
        },
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn v210_body_dml_update_with_new_binding() {
    let mut eng = engine();
    ok(&mut eng, "create table parent(aid int, bcnt int)");
    ok(&mut eng, "create table child(aid int)");
    ok(
        &mut eng,
        "create function ci() returns trigger language plpgsql as $$ begin update parent set bcnt = bcnt + 1 where aid = new.aid; return new; end; $$",
    );
    ok(
        &mut eng,
        "create trigger ci_t after insert on child for each row execute function ci()",
    );
    ok(&mut eng, "insert into parent values (1, 0)");
    ok(&mut eng, "insert into child values (1)");
    ok(&mut eng, "insert into child values (1)");
    assert_eq!(
        one_int(&mut eng, "select bcnt from parent where aid = 1"),
        2
    );
}

#[test]
fn v210_declare_select_into_execute_and_found() {
    let mut eng = engine();
    ok(&mut eng, "create table src(id int)");
    ok(&mut eng, "create table dst(id int, n int)");
    ok(
        &mut eng,
        "create function dx() returns trigger language plpgsql as $$ declare n int; begin select count(*) into n from src; execute 'insert into dst values (' || new.id || ', ' || n || ')'; return new; end; $$",
    );
    ok(
        &mut eng,
        "create trigger dx_t after insert on src for each row execute function dx()",
    );
    ok(&mut eng, "insert into src values (7)");
    assert_eq!(one_int(&mut eng, "select n from dst where id = 7"), 1);
    // FOUND after a DELETE that matched nothing.
    ok(
        &mut eng,
        "create function fd() returns trigger language plpgsql as $$ begin delete from src where id = old.id + 1000; if not found then raise notice 'none'; end if; return old; end; $$",
    );
    ok(
        &mut eng,
        "create trigger fd_t after delete on dst for each row execute function fd()",
    );
    let n = notices(&mut eng, "delete from dst where id = 7");
    assert!(n.iter().any(|m| m == "none"), "notices: {n:?}");
}

#[test]
fn v210_trigger_depth_and_exception_rollback() {
    let mut eng = engine();
    ok(&mut eng, "create table da(id int)");
    ok(&mut eng, "create table db(id int)");
    ok(&mut eng, "create table dc(id int)");
    ok(
        &mut eng,
        "create function da_tf() returns trigger language plpgsql as $$ begin raise notice 'da %', pg_trigger_depth(); insert into db values (new.id); return new; end; $$",
    );
    ok(
        &mut eng,
        "create function db_tf() returns trigger language plpgsql as $$ begin raise notice 'db %', pg_trigger_depth(); begin execute 'insert into dc values (' || new.id || ')'; exception when others then raise notice 'caught'; end; return new; end; $$",
    );
    ok(
        &mut eng,
        "create function dc_tf() returns trigger language plpgsql as $$ begin if new.id = 1 then raise exception sqlstate 'U9999'; end if; return new; end; $$",
    );
    ok(
        &mut eng,
        "create trigger da_tr before insert on da for each row execute function da_tf()",
    );
    ok(
        &mut eng,
        "create trigger db_tr before insert on db for each row execute function db_tf()",
    );
    ok(
        &mut eng,
        "create trigger dc_tr before insert on dc for each row execute function dc_tf()",
    );
    let n = notices(&mut eng, "insert into da values (1)");
    assert!(n.iter().any(|m| m == "da 1"), "notices: {n:?}");
    assert!(n.iter().any(|m| m == "db 2"), "notices: {n:?}");
    assert!(n.iter().any(|m| m == "caught"), "notices: {n:?}");
    // The failed dc insert was rolled back by the exception block.
    assert_eq!(one_int(&mut eng, "select count(*) from dc"), 0);
    assert_eq!(one_int(&mut eng, "select pg_trigger_depth()"), 0);
    ok(&mut eng, "insert into da values (2)");
    assert_eq!(one_int(&mut eng, "select count(*) from dc"), 1);
}

#[test]
fn v210_before_update_and_delete_hooks() {
    let mut eng = engine();
    ok(&mut eng, "create table t1(id int, descr text)");
    ok(
        &mut eng,
        "create function bu() returns trigger language plpgsql as $$ begin new.descr = 'updated in trigger'; return new; end; $$",
    );
    ok(
        &mut eng,
        "create trigger bu_t before update on t1 for each row execute function bu()",
    );
    ok(&mut eng, "insert into t1 values (1, 'orig')");
    ok(&mut eng, "update t1 set descr = 'no no' where id = 1");
    match run(&mut eng, "select descr from t1").unwrap() {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::text("updated in trigger".to_string()))
        }
        other => panic!("not a select: {other:?}"),
    }
    ok(
        &mut eng,
        "create function bd() returns trigger language plpgsql as $$ begin if old.id = 2 then return null; end if; return old; end; $$",
    );
    ok(
        &mut eng,
        "create trigger bd_t before delete on t1 for each row execute function bd()",
    );
    ok(&mut eng, "insert into t1 values (2, 'keep')");
    ok(&mut eng, "delete from t1");
    assert_eq!(one_int(&mut eng, "select count(*) from t1"), 1);
    assert_eq!(one_int(&mut eng, "select id from t1"), 2);
}

#[test]
fn v210_for_range_and_query_loops() {
    let mut eng = engine();
    ok(&mut eng, "create table log(x text)");
    ok(&mut eng, "create table t1(id int, descr text)");
    ok(
        &mut eng,
        "create function fl() returns trigger language plpgsql as $$ declare r record; begin for i in 1..2 loop insert into log values ('i=' || i); end loop; for r in select descr from t1 loop insert into log values ('d:' || r.descr); end loop; return new; end; $$",
    );
    ok(
        &mut eng,
        "create trigger fl_t after insert on t1 for each row execute function fl()",
    );
    ok(&mut eng, "insert into t1 values (1, 'one')");
    assert_eq!(one_int(&mut eng, "select count(*) from log"), 3);
}
