use super::*;

// v2.12: plpgsql cursor machinery (refcursor / OPEN / FETCH / MOVE /
// CLOSE / FOR-over-cursor), OUT/INOUT argument modes, RETURNS TABLE
// signatures, VARIADIC packing, and MERGE BEFORE-trigger
// suppression. PG19 grounding: pl_exec.c exec_stmt_open /
// exec_stmt_fetch / exec_stmt_forc, functioncmds.c identity-args
// rules, nodeModifyTable.c ExecMergeMatched trigger order.
// Harness shape copied from v211 (each statement its own xid,
// session 0, never commits).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(94000);

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
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
    execute(eng, &mut ctx, &stmt)
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

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql).unwrap() {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected select for {sql}: {other:?}"),
    }
}

fn setup_rc(eng: &mut Engine) {
    ok(eng, "CREATE TABLE rc212 (a int, b text)");
    ok(
        eng,
        "INSERT INTO rc212 VALUES (1,'one'),(2,'two'),(3,'three')",
    );
}

#[test]
fn v212_cursor_open_fetch_move_close() {
    let mut eng = engine();
    setup_rc(&mut eng);
    // Scroll cursor: forward fetch, backward fetch, absolute, close.
    ok(
        &mut eng,
        "CREATE FUNCTION fc212() RETURNS text AS $$
         DECLARE c scroll cursor for select a, b from rc212 order by a;
                 r record; s text := '';
         BEGIN
           open c;
           fetch c into r; s := s || r.a;
           fetch c into r; s := s || r.a;
           fetch prior from c into r; s := s || r.a;
           move forward 1 in c;
           fetch c into r; s := s || r.a;
           close c;
           return s;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(rows(&mut eng, "SELECT fc212()"), vec![vec!["1213"]]);
    // NO SCROLL cursor rejects a backward fetch with 55000.
    ok(
        &mut eng,
        "CREATE FUNCTION fc212b() RETURNS int AS $$
         DECLARE c cursor for select a from rc212 order by a; r record;
         BEGIN
           open c; fetch c into r; fetch prior from c into r; return r.a;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(err_code(&mut eng, "SELECT fc212b()"), "55000");
    // Fetching past the end sets FOUND false and NULLs the target.
    ok(
        &mut eng,
        "CREATE FUNCTION fc212c() RETURNS text AS $$
         DECLARE c cursor for select a from rc212 where a > 100; r record;
         BEGIN
           open c; fetch c into r;
           if found then return 'row'; else return 'none'; end if;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(rows(&mut eng, "SELECT fc212c()"), vec![vec!["none"]]);
}

#[test]
fn v212_bound_cursor_args_and_errors() {
    let mut eng = engine();
    setup_rc(&mut eng);
    ok(
        &mut eng,
        "CREATE FUNCTION fb212(int, int) RETURNS int AS $$
         DECLARE c1 cursor (p1 int, p2 int) for
           select a from rc212 where a > p1 and a < p2 order by a;
                 r record; n int := 0;
         BEGIN
           open c1(p2 := $2, p1 := $1);
           loop fetch c1 into r; exit when not found; n := n + 1; end loop;
           close c1;
           return n;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(rows(&mut eng, "SELECT fb212(0, 3)"), vec![vec!["2"]]);
    // Duplicate named argument is a CREATE-time 42601.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION fb212dup(int) RETURNS int AS $$
             DECLARE c1 cursor (p1 int) for select a from rc212 where a > p1;
             BEGIN open c1(p1 := 1, p1 := 2); return 0; END $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
    // Opening an already-open refcursor name is 42P03.
    ok(
        &mut eng,
        "CREATE FUNCTION fb212o() RETURNS int AS $$
         DECLARE rc refcursor;
         BEGIN
           rc := 'dupname';
           open rc for select a from rc212;
           open rc for select a from rc212;
           return 0;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(err_code(&mut eng, "SELECT fb212o()"), "42P03");
    // Fetch on a null cursor variable is 22004.
    ok(
        &mut eng,
        "CREATE FUNCTION fb212n() RETURNS int AS $$
         DECLARE rc refcursor; r record;
         BEGIN fetch rc into r; return 0; END $$ LANGUAGE plpgsql",
    );
    assert_eq!(err_code(&mut eng, "SELECT fb212n()"), "22004");
}

#[test]
fn v212_for_over_cursor() {
    let mut eng = engine();
    setup_rc(&mut eng);
    ok(
        &mut eng,
        "CREATE FUNCTION ff212() RETURNS text AS $$
         DECLARE c cursor (lo int) for select b from rc212 where a >= lo order by a;
                 r record; s text := '';
         BEGIN
           for r in c(2) loop s := s || r.b || ','; end loop;
           return s;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(rows(&mut eng, "SELECT ff212()"), vec![vec!["two,three,"]]);
    // FOR over a refcursor argument already holding an open cursor.
    ok(
        &mut eng,
        "CREATE FUNCTION ff212b(rc refcursor) RETURNS text AS $$
         DECLARE r record; s text := '';
         BEGIN
           for r in rc loop s := s || r.b || ','; end loop;
           return s;
         END $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE FUNCTION ff212a() RETURNS text AS $$
         DECLARE rc refcursor;
         BEGIN
           open rc for select b from rc212 order by a;
           return ff212b(rc);
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT ff212a()"),
        vec![vec!["one,two,three,"]]
    );
}

#[test]
fn v212_out_inout_and_returns_table() {
    let mut eng = engine();
    // OUT params, plpgsql, scalar context: single OUT is the value.
    ok(
        &mut eng,
        "CREATE FUNCTION fo212a(x int, OUT y int) AS $$
         BEGIN y := x * 3; END $$ LANGUAGE plpgsql",
    );
    assert_eq!(rows(&mut eng, "SELECT fo212a(5)"), vec![vec!["15"]]);
    // Two OUTs form a record in scalar context and columns in FROM.
    ok(
        &mut eng,
        "CREATE FUNCTION fo212b(x int, OUT a int, OUT b text) AS $$
         BEGIN a := x + 1; b := 'n' || x; END $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM fo212b(4)"),
        vec![vec!["5", "n4"]]
    );
    // INOUT passes through and can be reassigned.
    ok(
        &mut eng,
        "CREATE FUNCTION fo212c(INOUT v int) AS $$
         BEGIN v := v + 10; END $$ LANGUAGE plpgsql",
    );
    assert_eq!(rows(&mut eng, "SELECT fo212c(1)"), vec![vec!["11"]]);
    // SQL-language OUT function.
    ok(
        &mut eng,
        "CREATE FUNCTION fo212d(x int, OUT y int) AS $$ SELECT x * 2 $$ LANGUAGE sql",
    );
    assert_eq!(rows(&mut eng, "SELECT fo212d(21)"), vec![vec!["42"]]);
    // RETURNS TABLE with bare RETURN NEXT.
    ok(
        &mut eng,
        "CREATE FUNCTION ft212() RETURNS TABLE (a int, b text) AS $$
         BEGIN a := 1; b := 'x'; RETURN NEXT; a := 2; b := 'y'; RETURN NEXT; RETURN;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM ft212()"),
        vec![vec!["1", "x"], vec!["2", "y"]]
    );
    // OUT params on a SETOF record function with bare RETURN NEXT.
    ok(
        &mut eng,
        "CREATE FUNCTION fo212e(OUT p int, OUT q int) RETURNS SETOF record AS $$
         BEGIN p := -1; q := -2; RETURN NEXT; p := 1; q := 2; RETURN NEXT; RETURN;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM fo212e()"),
        vec![vec!["-1", "-2"], vec!["1", "2"]]
    );
    // Signature identity ignores OUT args: DROP by input list works,
    // and a CREATE with the same inputs but different OUTs replaces.
    ok(&mut eng, "DROP FUNCTION fo212a(int)");
    ok(
        &mut eng,
        "CREATE FUNCTION fo212a(x int, OUT y text) AS $$
         BEGIN y := 'v' || x; END $$ LANGUAGE plpgsql",
    );
    assert_eq!(rows(&mut eng, "SELECT fo212a(2)"), vec![vec!["v2"]]);
}

#[test]
fn v212_variadic_packing() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE FUNCTION fv212(VARIADIC v int[]) RETURNS int AS $$
         DECLARE s int := 0; i int;
         BEGIN
           for i in 1 .. coalesce(array_length(v, 1), 0) loop
             s := s + v[i];
           end loop;
           return s;
         END $$ LANGUAGE plpgsql",
    );
    assert_eq!(rows(&mut eng, "SELECT fv212(1, 2, 3)"), vec![vec!["6"]]);
    assert_eq!(rows(&mut eng, "SELECT fv212()"), vec![vec!["0"]]);
    assert_eq!(
        rows(&mut eng, "SELECT fv212(VARIADIC ARRAY[4,5])"),
        vec![vec!["9"]]
    );
    // A plain overload still wins over the variadic one.
    ok(
        &mut eng,
        "CREATE FUNCTION fv212(x int) RETURNS int AS $$ SELECT 100 $$ LANGUAGE sql",
    );
    assert_eq!(rows(&mut eng, "SELECT fv212(7)"), vec![vec!["100"]]);
    // VARIADIC must be the last input parameter (42P13).
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION fv212bad(VARIADIC v int[], z int) RETURNS int AS $$ SELECT 0 $$ LANGUAGE sql"
        ),
        "42P13"
    );
}

#[test]
fn v212_merge_trigger_suppression() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE mt212 (id int primary key, v int)");
    ok(
        &mut eng,
        "INSERT INTO mt212 VALUES (1, 10), (2, 20), (3, 30)",
    );
    ok(&mut eng, "CREATE TABLE ms212 (id int, v int)");
    ok(
        &mut eng,
        "INSERT INTO ms212 VALUES (1, 100), (2, 200), (4, 400)",
    );
    // BEFORE UPDATE trigger returning NULL suppresses id=2's update.
    ok(
        &mut eng,
        "CREATE FUNCTION mt212_bu() RETURNS trigger AS $$
         BEGIN IF NEW.id = 2 THEN RETURN NULL; END IF; RETURN NEW; END
         $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER trg_bu BEFORE UPDATE ON mt212 FOR EACH ROW EXECUTE FUNCTION mt212_bu()",
    );
    // BEFORE DELETE trigger returning NULL suppresses id=3's delete.
    ok(
        &mut eng,
        "CREATE FUNCTION mt212_bd() RETURNS trigger AS $$
         BEGIN IF OLD.id = 3 THEN RETURN NULL; END IF; RETURN OLD; END
         $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER trg_bd BEFORE DELETE ON mt212 FOR EACH ROW EXECUTE FUNCTION mt212_bd()",
    );
    // BEFORE INSERT trigger rewriting NEW (audited INSERT path).
    ok(
        &mut eng,
        "CREATE FUNCTION mt212_bi() RETURNS trigger AS $$
         BEGIN NEW.v := NEW.v + 1; RETURN NEW; END
         $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER trg_bi BEFORE INSERT ON mt212 FOR EACH ROW EXECUTE FUNCTION mt212_bi()",
    );
    ok(
        &mut eng,
        "MERGE INTO mt212 t USING ms212 s ON t.id = s.id
         WHEN MATCHED THEN UPDATE SET v = s.v
         WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)",
    );
    // id1 updated, id2 update suppressed, id4 inserted with NEW+1.
    assert_eq!(
        rows(&mut eng, "SELECT id, v FROM mt212 ORDER BY id"),
        vec![
            vec!["1", "100"],
            vec!["2", "20"],
            vec!["3", "30"],
            vec!["4", "401"]
        ]
    );
    // Delete suppression: delete id3 via MERGE is suppressed, id1 goes.
    ok(&mut eng, "CREATE TABLE ms212b (id int)");
    ok(&mut eng, "INSERT INTO ms212b VALUES (1), (3)");
    ok(
        &mut eng,
        "MERGE INTO mt212 t USING ms212b s ON t.id = s.id
         WHEN MATCHED THEN DELETE",
    );
    assert_eq!(
        rows(&mut eng, "SELECT id FROM mt212 ORDER BY id"),
        vec![vec!["2"], vec!["3"], vec!["4"]]
    );
}

#[test]
fn v212_finishing_create_identity_validation() {
    // The six CREATE/DROP identity validations recovered by the
    // finishing pass (each grounded in PG19 sources; see
    // hidden_files/v212 notes):
    // (1) pl_gram.c make_return_stmt: RETURN with an expression in
    // a function with OUT parameters is a syntax error — including
    // for single-RETURN bodies (the desugar must not bypass it).
    let mut eng = engine();
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION f1v(i int, OUT j int) RETURNS int AS $$ BEGIN RETURN i+1; END $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
    // A legal OUT-parameter body (assign + bare RETURN) still works.
    ok(
        &mut eng,
        "CREATE FUNCTION f1ok(i int, OUT j int) AS $$ BEGIN j := i + 1; END $$ LANGUAGE plpgsql",
    );
    // (2) pl_gram.c: FOR over a plain refcursor variable (no bound
    // query) must error; a bound cursor FOR stays legal.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION fbad() RETURNS void AS $$ DECLARE c refcursor; BEGIN FOR r IN c LOOP END LOOP; END $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
    // (3) pl_gram.c getdiag_target: a composite-typed argument is
    // not a scalar GET DIAGNOSTICS target.
    ok(&mut eng, "CREATE TYPE wslotv AS (a int, b int)");
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION fxv(x wslotv) RETURNS void AS $$ BEGIN GET DIAGNOSTICS x = ROW_COUNT; RETURN; END $$ LANGUAGE plpgsql"
        ),
        "42601"
    );
    // (4) functioncmds.c: duplicate parameter names error 42P13,
    // except a pure-IN name shared with a pure-OUT name.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION dpv(OUT a int, OUT a int) RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql"
        ),
        "42P13"
    );
    ok(
        &mut eng,
        "CREATE FUNCTION dpok(a int, OUT a int) AS $$ SELECT $1 $$ LANGUAGE sql",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION dtv() RETURNS TABLE(a int, a int) AS $$ SELECT 1, 2 $$ LANGUAGE sql"
        ),
        "42P13"
    );
    // (5) functioncmds.c: an explicit RETURNS must equal the
    // required result type implied by the OUT parameters.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FUNCTION rvv(IN f1 int, OUT f2 int, OUT f3 text) RETURNS int AS $$ SELECT $1 $$ LANGUAGE sql"
        ),
        "42P13"
    );
    ok(
        &mut eng,
        "CREATE FUNCTION rvok(IN f1 int, OUT f2 int, OUT f3 text) RETURNS record AS $$ BEGIN f2 := f1; f3 := 'x'; RETURN; END $$ LANGUAGE plpgsql",
    );
    // (6) The ptest10 pair: identity is the INPUT list alone, so
    // (OUT int,int,int) and (IN int,int,int) coexist; DROP matches
    // the written list as inputs OR as the full list (union), and
    // an ambiguous union is 42725.
    ok(
        &mut eng,
        "CREATE PROCEDURE ptv(OUT a int, IN b int, IN c int) LANGUAGE SQL AS $$ SELECT b - c $$",
    );
    ok(
        &mut eng,
        "CREATE PROCEDURE ptv(IN a int, IN b int, IN c int) LANGUAGE SQL AS $$ SELECT a + b + c $$",
    );
    assert_eq!(err_code(&mut eng, "DROP PROCEDURE ptv"), "42725");
    assert_eq!(
        err_code(&mut eng, "DROP PROCEDURE ptv(int, int, int)"),
        "42725"
    );
    ok(&mut eng, "DROP PROCEDURE ptv(OUT int, int, int)");
    ok(&mut eng, "DROP PROCEDURE ptv(int, int, int)");
}
