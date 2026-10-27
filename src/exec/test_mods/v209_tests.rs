use super::*;

// v2.09: pg_trigger catalog + trigger functions + deferred
// constraint triggers. PG19 grounding: gram.y CreateTrigStmt (both
// productions), catalog/pg_trigger.h (tgtype bits, tgconstraint,
// tgdeferrable/tginitdeferred, tgparentid), commands/trigger.c
// (CreateTrigger, AfterTrigger queueing), trigger.c's
// AfterTriggerSetState for SET CONSTRAINTS name resolution.
//
// Harness shape copied from v208: each statement runs as its own
// xid on session 0 and never commits; deferred flushes are driven
// explicitly through flush_deferred_fk (the COMMIT path).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(91000);

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

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    run(eng, sql).expect_err(&format!("{sql} should fail"))
}

fn notices(eng: &mut Engine, sql: &str) -> Vec<String> {
    let (r, n) = run_full(eng, sql);
    r.unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
    n
}

/// Fire the session's deferred queue, as COMMIT would; returns the
/// notices the firing produced.
fn flush_noticed(eng: &mut Engine) -> (Result<(), ExecError>, Vec<String>) {
    let xid = NEXT_XID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    let r = flush_deferred_fk(eng, &mut ctx, None);
    (r, ctx.notices)
}

fn text_col(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).expect("select") {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Value::Text(s) => s.to_string(),
                other => panic!("not text: {other:?}"),
            })
            .collect(),
        other => panic!("not select: {other:?}"),
    }
}

const NAF: &str = "create function naf() returns trigger as $$ begin \
    raise notice 'trigger % on % % % for %: (a,b)=(%,%)', TG_NAME, TG_TABLE_NAME, TG_WHEN, TG_OP, TG_LEVEL, NEW.a, NEW.b; \
    if TG_LEVEL = 'ROW' then return NEW; end if; return null; end; $$ language plpgsql";

#[test]
fn v209_constraint_trigger_parses_and_is_cataloged() {
    let mut eng = engine();
    ok(&mut eng, "create table dt (id integer)");
    ok(
        &mut eng,
        "create function dtf() returns trigger language plpgsql as $$ begin return new; end; $$",
    );
    // The v2.08 state: this was 42601 "expected TABLE, found constraint".
    ok(
        &mut eng,
        "create constraint trigger dt_trg after insert on dt deferrable initially deferred for each row execute function dtf()",
    );
    // pg_trigger row with the constraint-trigger flags.
    match run(
        &mut eng,
        "select tgdeferrable, tginitdeferred, tgtype from pg_trigger where tgname = 'dt_trg'",
    )
    .expect("pg_trigger select")
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Bool(true));
            assert_eq!(rows[0][1], Value::Bool(true));
            // tgtype: ROW(1) + INSERT(4), AFTER = 0.
            assert_eq!(rows[0][2], Value::Int(5));
        }
        other => panic!("not select: {other:?}"),
    }
    // The synthesized contype-'t' pg_constraint row, linked by
    // pg_trigger.tgconstraint.
    match run(
        &mut eng,
        "select contype from pg_constraint where conname = 'dt_trg'",
    )
    .expect("contype select")
    {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows[0][0], Value::SingleChar(b't'))
        }
        other => panic!("not select: {other:?}"),
    }
    match run(
        &mut eng,
        "select count(*) from pg_trigger t join pg_constraint c on (t.tgconstraint = c.oid) where t.tgname = 'dt_trg'",
    )
    .expect("join select")
    {
        ExecResult::Select { rows, .. } => {
            assert!(matches!(rows[0][0], Value::Int(1) | Value::BigInt(1)))
        }
        other => panic!("not select: {other:?}"),
    }
}

#[test]
fn v209_deferred_constraint_trigger_fires_at_flush_not_at_insert() {
    let mut eng = engine();
    ok(&mut eng, "create table et (id integer)");
    ok(
        &mut eng,
        "create function exf() returns trigger language plpgsql as $$ begin perform 1/0; return new; exception when division_by_zero then raise notice 'caught'; return new; end; $$",
    );
    ok(
        &mut eng,
        "create constraint trigger ex_trg after insert on et deferrable initially deferred for each row execute function exf()",
    );
    // INITIALLY DEFERRED: nothing fires at INSERT time.
    assert!(notices(&mut eng, "insert into et values (7)").is_empty());
    // The flush (COMMIT) fires it; the EXCEPTION block catches the
    // division_by_zero and raises the notice.
    let (r, n) = flush_noticed(&mut eng);
    r.expect("flush ok");
    assert_eq!(n, vec!["caught".to_string()]);
}

#[test]
fn v209_after_trigger_tg_vars_if_and_when() {
    let mut eng = engine();
    ok(&mut eng, NAF);
    ok(&mut eng, "create table nab (a int, b text)");
    ok(
        &mut eng,
        "create trigger nab_trg after insert on nab for each row execute procedure naf()",
    );
    let n = notices(&mut eng, "insert into nab values (1, 'x')");
    assert_eq!(
        n,
        vec!["trigger nab_trg on nab AFTER INSERT for ROW: (a,b)=(1,x)".to_string()]
    );
    // WHEN gates per row.
    ok(
        &mut eng,
        "create trigger nab_when after insert on nab for each row when (new.a % 2 = 1) execute procedure naf()",
    );
    assert_eq!(
        notices(&mut eng, "insert into nab values (2, 'y')").len(),
        1
    );
    assert_eq!(
        notices(&mut eng, "insert into nab values (3, 'z')").len(),
        2
    );
}

#[test]
fn v209_update_of_and_from_and_set_constraints() {
    let mut eng = engine();
    ok(&mut eng, NAF);
    ok(&mut eng, "create table u (a int, b text)");
    ok(
        &mut eng,
        "create trigger u_trg after update of b on u for each row execute procedure naf()",
    );
    ok(&mut eng, "insert into u values (1, 'x')");
    // UPDATE OF b: setting a alone does not fire; setting b does.
    assert!(notices(&mut eng, "update u set a = 9 where a = 1").is_empty());
    assert_eq!(
        notices(&mut eng, "update u set b = 'w' where a = 9").len(),
        1
    );
    // FROM form on a constraint trigger + SET CONSTRAINTS by name.
    ok(&mut eng, "create table refd (id int)");
    ok(
        &mut eng,
        "create constraint trigger u_ct after update on u from refd deferrable for each row execute procedure naf()",
    );
    ok(&mut eng, "set constraints u_ct immediate");
    let e = err(&mut eng, "set constraints no_such_constraint deferred");
    assert_eq!(e.code, "42704");
    // ALTER TRIGGER RENAME + pg_get_triggerdef.
    ok(&mut eng, "alter trigger u_trg on u rename to u_trg2");
    assert_eq!(
        text_col(
            &mut eng,
            "select pg_get_triggerdef(oid) from pg_trigger where tgname = 'u_trg2'"
        ),
        vec![
            "CREATE TRIGGER u_trg2 AFTER UPDATE OF b ON u FOR EACH ROW EXECUTE FUNCTION naf()"
                .to_string()
        ]
    );
}

#[test]
fn v209_partitioned_parent_trigger_fires_and_clones_in_catalog() {
    let mut eng = engine();
    ok(&mut eng, NAF);
    ok(
        &mut eng,
        "create table parted (a int, b text) partition by list (a)",
    );
    ok(
        &mut eng,
        "create table parted1 partition of parted for values in (1)",
    );
    ok(
        &mut eng,
        "create table parted2 partition of parted for values in (2)",
    );
    ok(
        &mut eng,
        "create trigger parted_trg after insert on parted for each row execute procedure naf()",
    );
    let n = notices(&mut eng, "insert into parted values (1, 'one'), (2, 'two')");
    assert_eq!(
        n.len(),
        2,
        "parent trigger fires once per routed row: {n:?}"
    );
    // pg_trigger: parent row + one clone per leaf, clones carry
    // tgparentid = the parent row's oid.
    match run(
        &mut eng,
        "select count(*) from pg_trigger where tgname = 'parted_trg'",
    )
    .expect("count")
    {
        ExecResult::Select { rows, .. } => {
            assert!(matches!(rows[0][0], Value::Int(3) | Value::BigInt(3)))
        }
        other => panic!("not select: {other:?}"),
    }
    match run(
        &mut eng,
        "select count(*) from pg_trigger where tgname = 'parted_trg' and tgparentid <> 0",
    )
    .expect("count clones")
    {
        ExecResult::Select { rows, .. } => {
            assert!(matches!(rows[0][0], Value::Int(2) | Value::BigInt(2)))
        }
        other => panic!("not select: {other:?}"),
    }
}

#[test]
fn v209_trigger_codec_round_trip() {
    // Triggers ride the constraints codec (WAL + checkpoint). A
    // pre-v2.09 encoding has no triggers section and decodes empty.
    let mut eng = engine();
    ok(&mut eng, NAF);
    ok(&mut eng, "create table ct (a int, b text)");
    ok(
        &mut eng,
        "create constraint trigger ct_trg after update of b on ct deferrable initially deferred for each row when (new.a > 0) execute procedure naf()",
    );
    let t = eng
        .db
        .find_table("ct", &eng.take_snapshot(), &[0], 0)
        .expect("table")
        .clone();
    let enc = crate::sql::encode_constraints(&t);
    let dec = crate::sql::decode_constraints(&enc).expect("decode");
    assert_eq!(dec.triggers.len(), 1);
    let tr = &dec.triggers[0];
    assert_eq!(tr.name, "ct_trg");
    assert!(tr.is_constraint && tr.deferrable && tr.initially_deferred && tr.enforced);
    assert_eq!(tr.update_cols, vec!["b".to_string()]);
    assert!(tr.when_expr.is_some());
    // And the encoded form re-encodes byte-identically.
    let mut t2 = t.clone();
    t2.triggers = dec.triggers.clone();
    assert_eq!(crate::sql::encode_constraints(&t2), enc);
}

#[test]
fn v209_alter_function_security_definer() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create function sf() returns trigger language plpgsql as $$ begin return null; end; $$",
    );
    ok(&mut eng, "alter function sf() security definer");
    let f = eng
        .db
        .functions
        .get("sf")
        .and_then(|v| v.first())
        .expect("function");
    assert!(f.security_definer);
    ok(&mut eng, "alter function sf() security invoker");
    let f = eng
        .db
        .functions
        .get("sf")
        .and_then(|v| v.first())
        .expect("function");
    assert!(!f.security_definer);
}
