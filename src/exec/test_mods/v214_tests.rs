use super::*;

// v2.14: the TRIGGER family — statement-level triggers (incl. the
// no-FOR-EACH and FOR STATEMENT spellings), transition tables
// (REFERENCING OLD/NEW TABLE) for row and statement triggers,
// INSTEAD OF triggers on views (DML routing + pg_trigger /
// pg_get_triggerdef / information_schema visibility), CREATE OR
// REPLACE TRIGGER, ALTER TABLE ... ENABLE/DISABLE [ALWAYS|REPLICA]
// TRIGGER, COMMENT ON TRIGGER, the suppress_redundant_updates
// builtin, the regproc type, information_schema.triggers, and the
// DEFAULT ... NOT NULL b_expr lookahead fix. PG19 grounding:
// trigger.c CreateTrigger / trigfire paths, gram.y TriggerForSpec,
// information_schema.sql triggers view.
// Harness shape copied from v213 (each statement its own xid,
// session 0, never commits).

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(96000);

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
        ExecResult::Dml { rows, .. } => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
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

fn notices(eng: &mut Engine, sql: &str) -> Vec<String> {
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
    execute(eng, &mut ctx, &stmt).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
    ctx.notices.clone()
}

#[test]
fn v214_statement_trigger_spellings_and_firing() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE st214 (a int)");
    ok(
        &mut eng,
        "CREATE FUNCTION sf214() RETURNS trigger AS $$ BEGIN RAISE NOTICE 'fired % %', TG_OP, TG_LEVEL; RETURN NULL; END $$ LANGUAGE plpgsql",
    );
    // No FOR EACH at all: defaults to FOR EACH STATEMENT (gram.y).
    ok(
        &mut eng,
        "CREATE TRIGGER t_before BEFORE INSERT ON st214 EXECUTE FUNCTION sf214()",
    );
    // FOR STATEMENT shorthand.
    ok(
        &mut eng,
        "CREATE TRIGGER t_after AFTER INSERT ON st214 FOR STATEMENT EXECUTE FUNCTION sf214()",
    );
    let ns = notices(&mut eng, "INSERT INTO st214 VALUES (1), (2)");
    assert_eq!(
        ns.len(),
        2,
        "statement triggers fire once per statement: {ns:?}"
    );
    // BEFORE ROW on a view and INSTEAD OF on a table are 42809.
    ok(&mut eng, "CREATE VIEW sv214 AS SELECT a FROM st214");
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE TRIGGER bad BEFORE INSERT ON sv214 FOR EACH ROW EXECUTE FUNCTION sf214()"
        ),
        "42809"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE TRIGGER bad2 INSTEAD OF INSERT ON st214 FOR EACH ROW EXECUTE FUNCTION sf214()"
        ),
        "42809"
    );
}

#[test]
fn v214_transition_tables_row_and_statement() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE tt214 (a int, b int)");
    ok(&mut eng, "CREATE TABLE log214 (n int, s int)");
    ok(
        &mut eng,
        "CREATE FUNCTION rowf214() RETURNS trigger AS $$ BEGIN INSERT INTO log214 SELECT count(*), sum(a) FROM nt214; RETURN NULL; END $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER rt214 AFTER INSERT ON tt214 REFERENCING NEW TABLE AS nt214 FOR EACH ROW EXECUTE FUNCTION rowf214()",
    );
    ok(&mut eng, "INSERT INTO tt214 VALUES (1, 1), (2, 2), (3, 3)");
    // Row trigger fires 3 times, each seeing the whole statement set.
    assert_eq!(
        rows(&mut eng, "SELECT n, s FROM log214"),
        vec![vec!["3", "6"], vec!["3", "6"], vec!["3", "6"]]
    );
    ok(
        &mut eng,
        "CREATE FUNCTION stmtf214() RETURNS trigger AS $$ BEGIN INSERT INTO log214 SELECT count(*), sum(a) FROM ot214; RETURN NULL; END $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER st214 AFTER UPDATE ON tt214 REFERENCING OLD TABLE AS ot214 FOR EACH STATEMENT EXECUTE FUNCTION stmtf214()",
    );
    ok(&mut eng, "UPDATE tt214 SET b = 9 WHERE a < 3");
    let all = rows(&mut eng, "SELECT n, s FROM log214");
    assert_eq!(all.len(), 4);
    assert_eq!(all[3], vec!["2", "3"]);
}

#[test]
fn v214_enable_disable_trigger() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE ed214 (a int)");
    ok(&mut eng, "CREATE TABLE cnt214 (n int)");
    ok(
        &mut eng,
        "CREATE FUNCTION cf214() RETURNS trigger AS $$ BEGIN UPDATE cnt214 SET n = n + 1; RETURN NEW; END $$ LANGUAGE plpgsql",
    );
    ok(&mut eng, "INSERT INTO cnt214 VALUES (0)");
    ok(
        &mut eng,
        "CREATE TRIGGER ct214 BEFORE INSERT ON ed214 FOR EACH ROW EXECUTE FUNCTION cf214()",
    );
    ok(&mut eng, "INSERT INTO ed214 VALUES (1)");
    assert_eq!(rows(&mut eng, "SELECT n FROM cnt214"), vec![vec!["1"]]);
    ok(&mut eng, "ALTER TABLE ed214 DISABLE TRIGGER ct214");
    assert_eq!(
        rows(
            &mut eng,
            "SELECT tgenabled FROM pg_trigger WHERE tgname = 'ct214'"
        ),
        vec![vec!["D"]]
    );
    ok(&mut eng, "INSERT INTO ed214 VALUES (2)");
    assert_eq!(rows(&mut eng, "SELECT n FROM cnt214"), vec![vec!["1"]]);
    ok(&mut eng, "ALTER TABLE ed214 ENABLE ALWAYS TRIGGER ct214");
    assert_eq!(
        rows(
            &mut eng,
            "SELECT tgenabled FROM pg_trigger WHERE tgname = 'ct214'"
        ),
        vec![vec!["A"]]
    );
    ok(&mut eng, "INSERT INTO ed214 VALUES (3)");
    assert_eq!(rows(&mut eng, "SELECT n FROM cnt214"), vec![vec!["2"]]);
    assert_eq!(
        err_code(&mut eng, "ALTER TABLE ed214 DISABLE TRIGGER nope214"),
        "42704"
    );
    ok(&mut eng, "ALTER TABLE ed214 DISABLE TRIGGER ALL");
    ok(&mut eng, "INSERT INTO ed214 VALUES (4)");
    assert_eq!(rows(&mut eng, "SELECT n FROM cnt214"), vec![vec!["2"]]);
}

#[test]
fn v214_or_replace_comment_and_def() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE or214 (a int)");
    ok(
        &mut eng,
        "CREATE FUNCTION f1214() RETURNS trigger AS $$ BEGIN RETURN NEW; END $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE FUNCTION f2214() RETURNS trigger AS $$ BEGIN RETURN NULL; END $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER tg214 BEFORE INSERT ON or214 FOR EACH ROW EXECUTE FUNCTION f1214()",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE TRIGGER tg214 BEFORE INSERT ON or214 FOR EACH ROW EXECUTE FUNCTION f2214()"
        ),
        "42723" // pre-existing engine code for a duplicate trigger
    );
    ok(
        &mut eng,
        "CREATE OR REPLACE TRIGGER tg214 BEFORE INSERT ON or214 FOR EACH ROW WHEN (NEW.a > 0) EXECUTE FUNCTION f2214()",
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = 'tg214'"
        ),
        vec![vec![
            "CREATE TRIGGER tg214 BEFORE INSERT ON or214 FOR EACH ROW WHEN (new.a > 0) EXECUTE FUNCTION f2214()"
                .to_string()
        ]]
    );
    ok(&mut eng, "COMMENT ON TRIGGER tg214 ON or214 IS 'hello'");
    assert_eq!(
        err_code(&mut eng, "COMMENT ON TRIGGER nope214 ON or214 IS 'x'"),
        "42704"
    );
}

#[test]
fn v214_instead_of_view_dml() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE base214 (a int, b int)");
    ok(&mut eng, "CREATE VIEW v214 AS SELECT a, b FROM base214");
    ok(
        &mut eng,
        "CREATE FUNCTION vf214() RETURNS trigger AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO base214 VALUES (NEW.a, NEW.b); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE base214 SET b = NEW.b WHERE a = OLD.a; RETURN NEW; ELSE DELETE FROM base214 WHERE a = OLD.a; RETURN OLD; END IF; END $$ LANGUAGE plpgsql",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER vi214 INSTEAD OF INSERT ON v214 FOR EACH ROW EXECUTE FUNCTION vf214()",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER vu214 INSTEAD OF UPDATE ON v214 FOR EACH ROW EXECUTE FUNCTION vf214()",
    );
    ok(
        &mut eng,
        "CREATE TRIGGER vd214 INSTEAD OF DELETE ON v214 FOR EACH ROW EXECUTE FUNCTION vf214()",
    );
    assert_eq!(
        rows(&mut eng, "INSERT INTO v214 VALUES (1, 10) RETURNING a, b"),
        vec![vec!["1", "10"]]
    );
    assert_eq!(
        rows(&mut eng, "SELECT a, b FROM base214"),
        vec![vec!["1", "10"]]
    );
    assert_eq!(
        rows(&mut eng, "UPDATE v214 SET b = 11 WHERE a = 1 RETURNING b"),
        vec![vec!["11"]]
    );
    assert_eq!(
        rows(&mut eng, "DELETE FROM v214 WHERE a = 1 RETURNING a"),
        vec![vec!["1"]]
    );
    assert!(rows(&mut eng, "SELECT a FROM base214").is_empty());
    // INSTEAD OF must be FOR EACH ROW.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE TRIGGER bad214 INSTEAD OF INSERT ON v214 FOR EACH STATEMENT EXECUTE FUNCTION vf214()"
        ),
        "0A000"
    );
    // View triggers appear in pg_trigger and information_schema.
    assert_eq!(
        rows(
            &mut eng,
            "SELECT tgname, tgtype FROM pg_trigger WHERE tgname LIKE 'v_214' ORDER BY tgname"
        ),
        vec![
            vec!["vd214", "73"],
            vec!["vi214", "69"],
            vec!["vu214", "81"]
        ]
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT trigger_name, action_timing FROM information_schema.triggers WHERE event_object_table = 'v214' AND event_manipulation = 'INSERT'"
        ),
        vec![vec!["vi214", "INSTEAD OF"]]
    );
}

#[test]
fn v214_suppress_redundant_and_regproc() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE sr214 (a int, b int)");
    ok(&mut eng, "INSERT INTO sr214 VALUES (1, 1)");
    ok(
        &mut eng,
        "CREATE TRIGGER s214 BEFORE UPDATE ON sr214 FOR EACH ROW EXECUTE FUNCTION suppress_redundant_updates_trigger()",
    );
    match run(&mut eng, "UPDATE sr214 SET b = 1 WHERE a = 1").unwrap() {
        ExecResult::Dml { tag, .. } => assert_eq!(tag, "UPDATE 0"),
        other => panic!("expected Dml: {other:?}"),
    }
    match run(&mut eng, "UPDATE sr214 SET b = 2 WHERE a = 1").unwrap() {
        ExecResult::Dml { tag, .. } => assert_eq!(tag, "UPDATE 1"),
        other => panic!("expected Dml: {other:?}"),
    }
    assert_eq!(
        rows(
            &mut eng,
            "SELECT tgfoid::regproc FROM pg_trigger WHERE tgname = 's214'"
        ),
        vec![vec!["suppress_redundant_updates_trigger"]]
    );
    assert_eq!(
        rows(
            &mut eng,
            "SELECT 'suppress_redundant_updates_trigger'::regproc"
        ),
        vec![vec!["suppress_redundant_updates_trigger"]]
    );
}

#[test]
fn v214_codec_roundtrip_view_triggers() {
    // The standalone trigger codec must round-trip every field,
    // including REFERENCING names and tgenabled.
    let defs = vec![
        crate::sql::TriggerDef {
            name: "x".to_string(),
            timing: crate::sql::TriggerTiming::InsteadOf,
            events: crate::sql::trig_event::INSERT | crate::sql::trig_event::UPDATE,
            for_each_row: true,
            function: "f".to_string(),
            args: vec!["a'b".to_string()],
            when_expr: None,
            update_cols: Vec::new(),
            is_constraint: false,
            deferrable: false,
            initially_deferred: false,
            constr_rel: None,
            enforced: true,
            has_transition: true,
            old_table: Some("ot".to_string()),
            new_table: None,
            tgenabled: b'D',
        },
        crate::sql::TriggerDef {
            name: "y".to_string(),
            timing: crate::sql::TriggerTiming::Before,
            events: crate::sql::trig_event::TRUNCATE,
            for_each_row: false,
            function: "g".to_string(),
            args: Vec::new(),
            when_expr: None,
            update_cols: Vec::new(),
            is_constraint: false,
            deferrable: false,
            initially_deferred: false,
            constr_rel: None,
            enforced: true,
            has_transition: true,
            old_table: None,
            new_table: Some("nt".to_string()),
            tgenabled: b'A',
        },
    ];
    let enc = crate::sql::encode_triggers(&defs);
    let dec = crate::sql::decode_triggers(&enc).expect("decodes");
    assert_eq!(dec.len(), 2);
    assert_eq!(dec[0].name, "x");
    assert_eq!(dec[0].old_table.as_deref(), Some("ot"));
    assert_eq!(dec[0].tgenabled, b'D');
    assert_eq!(dec[0].args, vec!["a'b".to_string()]);
    assert_eq!(dec[1].new_table.as_deref(), Some("nt"));
    assert_eq!(dec[1].tgenabled, b'A');
    // Full round-trip: re-encoding the decoded defs is a fixed point.
    assert_eq!(crate::sql::encode_triggers(&dec), enc);
}

#[test]
fn v214_default_not_null_parse() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE dn214 (a int, b bool DEFAULT true NOT NULL)",
    );
    ok(&mut eng, "INSERT INTO dn214 VALUES (1)");
    assert_eq!(
        rows(&mut eng, "SELECT a, b FROM dn214"),
        vec![vec!["1", "t"]]
    );
    // Negated operators still parse.
    assert_eq!(
        rows(&mut eng, "SELECT 1 WHERE 1 NOT IN (2) AND 'a' NOT LIKE 'b'"),
        vec![vec!["1"]]
    );
}
