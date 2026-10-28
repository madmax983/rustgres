use super::*;

// v2.11: general plpgsql execution — IF/ELSIF/ELSE, SELECT INTO
// (scalar + record), DML statements through the statement executor
// with variable binding, FOR range with auto-declared variables,
// nested blocks with EXCEPTION write rollback, EXECUTE INTO/USING,
// RETURN QUERY, GET DIAGNOSTICS. PG19 grounding: pl_exec.c
// exec_stmt_if / exec_stmt_execsql (STRICT: P0002 no_data_found,
// P0003 too_many_rows) / exec_stmt_block subtransactions /
// exec_stmt_fori / exec_stmt_getdiag. Harness shape copied from
// v210 (each statement its own xid, session 0, never commits).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(93000);

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

fn err_code(eng: &mut Engine, sql: &str) -> &'static str {
    match run(eng, sql) {
        Ok(_) => panic!("expected error for {sql}"),
        Err(e) => e.code,
    }
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

fn one_text(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql).unwrap() {
        ExecResult::Select { rows, .. } => match &rows[0][0] {
            Value::Text(s) => s.to_string(),
            Value::BpChar(s) => s.to_string(),
            Value::Null => "NULL".to_string(),
            other => panic!("not text: {other:?}"),
        },
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn v211_if_elsif_else() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create function grade211(x int) returns text language plpgsql as $$
begin
  if x >= 90 then
    return 'A';
  elsif x >= 80 then
    return 'B';
  elsif x >= 70 then
    return 'C';
  else
    return 'F';
  end if;
end; $$",
    );
    assert_eq!(one_text(&mut eng, "select grade211(95)"), "A");
    assert_eq!(one_text(&mut eng, "select grade211(85)"), "B");
    assert_eq!(one_text(&mut eng, "select grade211(75)"), "C");
    assert_eq!(one_text(&mut eng, "select grade211(10)"), "F");
    // Nested IF and the IN condition form used by the hub helpers.
    ok(
        &mut eng,
        "create function cls211(link text) returns text language plpgsql as $$
begin
  if link in ('PLWS', 'WSPL') then
    if link = 'PLWS' then
      return 'direct';
    end if;
    return 'link';
  end if;
  return 'other';
end; $$",
    );
    assert_eq!(one_text(&mut eng, "select cls211('PLWS')"), "direct");
    assert_eq!(one_text(&mut eng, "select cls211('WSPL')"), "link");
    assert_eq!(one_text(&mut eng, "select cls211('XXXX')"), "other");
}

#[test]
fn v211_recursion_uppercase_declare() {
    // The plpgsql.sql recursion_test shape verbatim: uppercase
    // DECLARE/BEGIN, bare `=` assignment, IF/ELSE, self-recursion.
    // (At the v2.10 baseline this CREATE failed with "DECLARE
    // section without BEGIN".)
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE FUNCTION recursion_test211(int,int) RETURNS text AS '
DECLARE rslt text;
BEGIN
    IF $1 <= 0 THEN
        rslt = CAST($2 AS TEXT);
    ELSE
        rslt = CAST($1 AS TEXT) || '','' || recursion_test211($1 - 1, $2);
    END IF;
    RETURN rslt;
END;' LANGUAGE plpgsql",
    );
    assert_eq!(
        one_text(&mut eng, "select recursion_test211(4,3)"),
        "4,3,2,1,3"
    );
}

#[test]
fn v211_select_into_scalar_strict_found() {
    let mut eng = engine();
    ok(&mut eng, "create table acct211(id int, bal int)");
    ok(&mut eng, "insert into acct211 values (1, 100), (2, 200)");
    ok(
        &mut eng,
        "create function bal211(x int) returns int language plpgsql as $$
declare b int;
begin
  select into b bal from acct211 where id = x;
  if not found then
    return -1;
  end if;
  return b;
end; $$",
    );
    assert_eq!(one_int(&mut eng, "select bal211(2)"), 200);
    assert_eq!(one_int(&mut eng, "select bal211(9)"), -1);
    // STRICT: no rows -> P0002, more than one row -> P0003.
    ok(
        &mut eng,
        "create function balstrict211(x int) returns int language plpgsql as $$
declare b int;
begin
  select into strict b bal from acct211 where id = x;
  return b;
end; $$",
    );
    assert_eq!(one_int(&mut eng, "select balstrict211(1)"), 100);
    assert_eq!(err_code(&mut eng, "select balstrict211(9)"), "P0002");
    ok(
        &mut eng,
        "create function allbal211() returns int language plpgsql as $$
declare b int;
begin
  select into strict b bal from acct211;
  return b;
end; $$",
    );
    assert_eq!(err_code(&mut eng, "select allbal211()"), "P0003");
}

#[test]
fn v211_select_into_record_fields() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table pslot211(slotname text, backlink text)",
    );
    ok(
        &mut eng,
        "insert into pslot211 values ('PS.1', 'PL.9'), ('PS.2', '')",
    );
    ok(
        &mut eng,
        "create function bl211(n text) returns text language plpgsql as $$
declare rec record;
begin
  select into rec * from pslot211 where slotname = n;
  if not found then
    raise exception '% does not exist', n;
  end if;
  return rec.backlink;
end; $$",
    );
    assert_eq!(one_text(&mut eng, "select bl211('PS.1')"), "PL.9");
    assert_eq!(err_code(&mut eng, "select bl211('PS.9')"), "P0001");
    // Record field in an UPDATE ... SET comparison path (the hub
    // helper shape: only write when the field differs).
    ok(
        &mut eng,
        "create function setbl211(n text, target text) returns int language plpgsql as $$
declare rec record;
begin
  select into rec * from pslot211 where slotname = n;
  if rec.backlink != target then
    update pslot211 set backlink = target where slotname = n;
  end if;
  return 0;
end; $$",
    );
    ok(&mut eng, "select setbl211('PS.2', 'WS.3')");
    assert_eq!(
        one_text(
            &mut eng,
            "select backlink from pslot211 where slotname = 'PS.2'"
        ),
        "WS.3"
    );
}

#[test]
fn v211_dml_and_diagnostics() {
    let mut eng = engine();
    ok(&mut eng, "create table cnt211(id int, n int)");
    ok(&mut eng, "insert into cnt211 values (1, 0), (2, 0), (3, 0)");
    ok(
        &mut eng,
        "create function bump211(delta int) returns int language plpgsql as $$
declare rc int;
begin
  update cnt211 set n = n + delta where id >= 2;
  get diagnostics rc = row_count;
  return rc;
end; $$",
    );
    assert_eq!(one_int(&mut eng, "select bump211(5)"), 2);
    assert_eq!(one_int(&mut eng, "select n from cnt211 where id = 3"), 5);
    // INSERT with variable binding + FOUND after a DELETE that
    // matched nothing.
    ok(
        &mut eng,
        "create function addel211(x int) returns int language plpgsql as $$
begin
  insert into cnt211 values (x, 1);
  delete from cnt211 where id = 999;
  if found then
    return 1;
  end if;
  return 0;
end; $$",
    );
    assert_eq!(one_int(&mut eng, "select addel211(7)"), 0);
    assert_eq!(
        one_int(&mut eng, "select count(*) from cnt211 where id = 7"),
        1
    );
    // UPDATE ... RETURNING INTO.
    ok(
        &mut eng,
        "create function bump1_211(x int) returns int language plpgsql as $$
declare newn int;
begin
  update cnt211 set n = n + 10 where id = x returning n into newn;
  return newn;
end; $$",
    );
    assert_eq!(one_int(&mut eng, "select bump1_211(1)"), 10);
}

#[test]
fn v211_hub_helpers_end_to_end() {
    // The plpgsql.sql hub/slot helper shapes, condensed: the five
    // general-plpgsql helpers must CREATE and RUN.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table PSlot211(slotname bpchar, backlink bpchar, slotlink bpchar)",
    );
    ok(
        &mut eng,
        "create table WSlot211(slotname bpchar, backlink bpchar, slotlink bpchar)",
    );
    ok(
        &mut eng,
        "create table PLine211(slotname bpchar, backlink bpchar)",
    );
    ok(
        &mut eng,
        "create table HSlot211(slotname bpchar, hubname bpchar, slotno int, slotlink bpchar)",
    );
    ok(
        &mut eng,
        "insert into PSlot211 values ('PS.a', '', ''), ('WS.a', '', '')",
    );
    // tg_backlink_set shape: named args, char(2)/char(4) decls,
    // record SELECT INTO, NOT FOUND raise, conditional UPDATE.
    ok(
        &mut eng,
        "create function tg_backlink_set211(myname bpchar, blname bpchar) returns integer as '
declare
    mytype char(2);
    link char(4);
    rec record;
begin
    mytype := substr(myname, 1, 2);
    link := mytype || substr(blname, 1, 2);
    if link = ''PLPL'' then
        raise exception ''backlink between two phone lines does not make sense'';
    end if;
    if link in (''PLWS'', ''WSPL'') then
        raise exception ''direct link of phone line to wall slot not permitted'';
    end if;
    if mytype = ''PS'' then
        select into rec * from PSlot211 where slotname = myname;
        if not found then
            raise exception ''% does not exist'', myname;
        end if;
        if rec.backlink != blname then
            update PSlot211 set backlink = blname where slotname = myname;
        end if;
        return 0;
    end if;
    raise exception ''illegal backlink beginning with %'', mytype;
end;
' language plpgsql",
    );
    assert_eq!(
        one_int(&mut eng, "select tg_backlink_set211('PS.a', 'PL.b')"),
        0
    );
    assert_eq!(
        one_text(
            &mut eng,
            "select backlink from PSlot211 where slotname = 'PS.a'"
        ),
        "PL.b"
    );
    assert_eq!(
        err_code(&mut eng, "select tg_backlink_set211('PS.zz', 'PL.b')"),
        "P0001"
    );
    // tg_backlink_unset shape: ALIAS FOR $1/$2 declarations.
    ok(
        &mut eng,
        "create function tg_backlink_unset211(bpchar, bpchar) returns integer as '
declare
    myname alias for $1;
    blname alias for $2;
    mytype char(2);
    rec record;
begin
    mytype := substr(myname, 1, 2);
    if mytype = ''PS'' then
        select into rec * from PSlot211 where slotname = myname;
        if not found then
            return 0;
        end if;
        if rec.backlink = blname then
            update PSlot211 set backlink = '''' where slotname = myname;
        end if;
        return 0;
    end if;
    return 0;
end;
' language plpgsql",
    );
    assert_eq!(
        one_int(&mut eng, "select tg_backlink_unset211('PS.a', 'PL.b')"),
        0
    );
    assert_eq!(
        one_text(
            &mut eng,
            "select backlink from PSlot211 where slotname = 'PS.a'"
        ),
        ""
    );
    // tg_hub_adjustslots shape: FOR range with an UNDECLARED loop
    // variable over expression bounds, INSERT/DELETE in the body.
    ok(
        &mut eng,
        "create function tg_hub_adjustslots211(hname bpchar, oldnslots integer, newnslots integer) returns integer as '
begin
    if newnslots = oldnslots then
        return 0;
    end if;
    if newnslots < oldnslots then
        delete from HSlot211 where hubname = hname and slotno > newnslots;
        return 0;
    end if;
    for i in oldnslots + 1 .. newnslots loop
        insert into HSlot211 (slotname, hubname, slotno, slotlink)
            values (''HS.dummy'', hname, i, '''');
    end loop;
    return 0;
end
' language plpgsql",
    );
    assert_eq!(
        one_int(&mut eng, "select tg_hub_adjustslots211('H1', 0, 3)"),
        0
    );
    assert_eq!(one_int(&mut eng, "select count(*) from HSlot211"), 3);
    assert_eq!(
        one_int(
            &mut eng,
            "select max(slotno) from HSlot211 where hubname = 'H1'"
        ),
        3
    );
    assert_eq!(
        one_int(&mut eng, "select tg_hub_adjustslots211('H1', 3, 1)"),
        0
    );
    assert_eq!(one_int(&mut eng, "select count(*) from HSlot211"), 1);
}

#[test]
fn v211_nested_block_rollback_and_stacked_diag() {
    let mut eng = engine();
    ok(&mut eng, "create table log211(id int)");
    ok(
        &mut eng,
        "create function f211() returns text language plpgsql as $$
declare st text; msg text;
begin
  insert into log211 values (1);
  begin
    insert into log211 values (2);
    raise exception 'boom';
  exception when others then
    get stacked diagnostics st = returned_sqlstate, msg = message_text;
  end;
  insert into log211 values (3);
  return st || ':' || msg;
end; $$",
    );
    // The inner block's INSERT is rolled back (subtransaction
    // semantics); the outer writes survive.
    assert_eq!(one_text(&mut eng, "select f211()"), "P0001:boom");
    assert_eq!(one_int(&mut eng, "select count(*) from log211"), 2);
    assert_eq!(
        one_int(&mut eng, "select count(*) from log211 where id = 2"),
        0
    );
}

#[test]
fn v211_loops_return_query_execute() {
    let mut eng = engine();
    ok(&mut eng, "create table nums211(n int)");
    // WHILE + EXIT WHEN + CONTINUE WHEN.
    ok(
        &mut eng,
        "create function fill211() returns int language plpgsql as $$
declare i int := 0; total int := 0;
begin
  while i < 10 loop
    i := i + 1;
    if i = 3 then
      continue;
    end if;
    exit when i > 6;
    total := total + i;
  end loop;
  return total;
end; $$",
    );
    assert_eq!(one_int(&mut eng, "select fill211()"), 1 + 2 + 4 + 5 + 6);
    // RETURN QUERY over a table + bare RETURN at the end.
    ok(&mut eng, "insert into nums211 values (5), (15), (25)");
    ok(
        &mut eng,
        "create function big211() returns setof int language plpgsql as $$
begin
  return query select n from nums211 where n > 10 order by n;
  return;
end; $$",
    );
    match run(&mut eng, "select * from big211()").unwrap() {
        ExecResult::Select { rows, .. } => {
            assert_eq!(rows.len(), 2);
        }
        other => panic!("not a select: {other:?}"),
    }
    // EXECUTE ... INTO with USING.
    ok(
        &mut eng,
        "create function dyn211(x int) returns int language plpgsql as $$
declare r int;
begin
  execute 'select n from nums211 where n > $1' into r using x;
  return r;
end; $$",
    );
    assert_eq!(one_int(&mut eng, "select dyn211(10)"), 15);
    // FOR over a query with a record variable.
    ok(
        &mut eng,
        "create function sum211() returns int language plpgsql as $$
declare rec record; total int := 0;
begin
  for rec in select n from nums211 loop
    total := total + rec.n;
  end loop;
  return total;
end; $$",
    );
    assert_eq!(one_int(&mut eng, "select sum211()"), 45);
}

#[test]
fn v211_labeled_block_and_shadowing() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create function lab211() returns int language plpgsql as $$
<<outer>>
declare x int := 1;
begin
  declare x int := 2;
  begin
    outer.x := outer.x + x;
  end;
  return outer.x;
end; $$",
    );
    // Inner x (2) added to outer.x (1) -> 3; the label-qualified
    // reference resolves to the outer declaration.
    assert_eq!(one_int(&mut eng, "select lab211()"), 3);
}

#[test]
fn v211_case_and_getdiag_defaults() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create function case211(x int) returns text language plpgsql as $$
begin
  case x
    when 1 then return 'one';
    when 2, 3 then return 'few';
    else return 'many';
  end case;
end; $$",
    );
    assert_eq!(one_text(&mut eng, "select case211(1)"), "one");
    assert_eq!(one_text(&mut eng, "select case211(3)"), "few");
    assert_eq!(one_text(&mut eng, "select case211(9)"), "many");
    // Searched CASE with no match and no ELSE: 20000 case_not_found.
    ok(
        &mut eng,
        "create function case211b(x int) returns text language plpgsql as $$
begin
  case
    when x > 100 then return 'big';
  end case;
  return 'small';
end; $$",
    );
    assert_eq!(one_text(&mut eng, "select case211b(200)"), "big");
    assert_eq!(err_code(&mut eng, "select case211b(5)"), "20000");
}

#[test]
fn v211_bare_return_shapes() {
    let mut eng = engine();
    // PG19 make_return_stmt: bare RETURN is legal in void and SETOF
    // functions, a CREATE-time 42601 in scalar functions.
    ok(
        &mut eng,
        "create function v211void() returns void language plpgsql as $$
begin
  return;
end; $$",
    );
    ok(&mut eng, "select v211void()");
    assert_eq!(
        err_code(
            &mut eng,
            "create function v211scalar() returns int language plpgsql as $$
begin
  return;
end; $$"
        ),
        "42601"
    );
    // A scalar function with no RETURN at all creates, but calling
    // it hits the fall-off error (2F005).
    ok(
        &mut eng,
        "create function v211falloff() returns int language plpgsql as $$
begin
  perform 2+2;
end; $$",
    );
    assert_eq!(err_code(&mut eng, "select v211falloff()"), "2F005");
}
