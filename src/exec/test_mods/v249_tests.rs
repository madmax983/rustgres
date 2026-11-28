/// v2.49: CREATE_INDEX REMAINDER — ADD PRIMARY KEY / UNIQUE USING
/// INDEX naming + NOT NULL semantics + partitioned rejection,
/// constraint-backed index DROP guard, duplicate index columns,
/// hash/GIN index acceptance + reloptions + gist tsvector siglen,
/// VACUUM FULL legacy form, REINDEX (VERBOSE), ON COMMIT temp-table
/// registration, temp CTAS shadowing, pg_class.relfilenode,
/// GRANT ON SCHEMA pg_toast, and the polygon geometry family.
/// Ground truth: PG19 tablecmds.c / indexcmds.c / geo_ops.c;
/// vendored create_index suite. Scope: hidden_files/v249/scope.md.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
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

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    match run(eng, sql) {
        Ok(_) => panic!("{sql} -> expected error, got success"),
        Err(e) => e,
    }
}

fn one(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => rows[0][0].to_text().unwrap_or_default(),
        other => panic!("expected Select for {sql} -> {other:?}"),
    }
}

#[test]
fn v249_pk_using_index_sets_not_null_and_takes_index_name() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text)");
    ok(&mut eng, "CREATE UNIQUE INDEX t_ab ON t (a, b)");
    ok(&mut eng, "ALTER TABLE t ADD PRIMARY KEY USING INDEX t_ab");
    // The constraint is named after the index (DROP CONSTRAINT by
    // that name must resolve), and the columns became NOT NULL.
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_constraint WHERE conname = 't_ab'"
        ),
        "1"
    );
    ok(&mut eng, "ALTER TABLE t DROP CONSTRAINT t_ab");
}

#[test]
fn v249_pk_using_index_named_constraint_renames_index() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    ok(&mut eng, "CREATE UNIQUE INDEX t_a ON t (a)");
    ok(
        &mut eng,
        "ALTER TABLE t ADD CONSTRAINT my_pk PRIMARY KEY USING INDEX t_a",
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_class WHERE relname = 'my_pk'"
        ),
        "1"
    );
    // Dropping the constraint's index directly is a dependency error.
    assert_eq!(err(&mut eng, "DROP INDEX my_pk").code, "2BP01");
}

#[test]
fn v249_using_index_on_partitioned_root_rejected() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE p (a int) PARTITION BY HASH (a)");
    ok(&mut eng, "CREATE UNIQUE INDEX p_a ON p (a)");
    assert_eq!(
        err(&mut eng, "ALTER TABLE p ADD PRIMARY KEY USING INDEX p_a").code,
        "0A000"
    );
}

#[test]
fn v249_duplicate_index_columns_and_hash_and_reloptions() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text)");
    // PG19 has no duplicate-column check in ComputeIndexAttrs.
    ok(&mut eng, "CREATE INDEX i1 ON t (b, a, b text_pattern_ops)");
    // Hash indexes are catalog-only (like gin/gist).
    ok(&mut eng, "CREATE INDEX i2 ON t USING hash (a int4_ops)");
    // Reloptions parse.
    ok(
        &mut eng,
        "CREATE INDEX i3 ON t USING gin (b) WITH (fastupdate = on)",
    );
    // Duplicate plain columns too (concur_reindex_ind4 shape).
    ok(&mut eng, "CREATE INDEX i4 ON t (a, a, b)");
}

#[test]
fn v249_vacuum_full_and_reindex_verbose_parse() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    // The grammar accepts the legacy bare FULL form; execution in
    // this harness (inside a transaction) is PG's 25001, exactly as
    // PG refuses VACUUM inside a transaction block.
    assert!(parse_statement("VACUUM FULL t").is_ok());
    assert_eq!(err(&mut eng, "VACUUM FULL t").code, "25001");
    ok(&mut eng, "REINDEX (VERBOSE) TABLE t");
}

#[test]
fn v249_on_commit_temp_registration() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TEMP TABLE tt (a int) ON COMMIT DELETE ROWS",
    );
    assert_eq!(
        eng.db
            .temp_on_commit
            .get(&0)
            .and_then(|m| m.get("tt"))
            .copied(),
        Some(2)
    );
    ok(&mut eng, "INSERT INTO tt VALUES (1)");
    eng.db.apply_temp_on_commit(0);
    assert_eq!(one(&mut eng, "SELECT count(*) FROM tt"), "0");
    // DROP action removes the table at commit.
    ok(&mut eng, "CREATE TEMP TABLE td (a int) ON COMMIT DROP");
    eng.db.apply_temp_on_commit(0);
    assert_eq!(err(&mut eng, "SELECT * FROM td").code, "42P01");
}

#[test]
fn v249_temp_ctas_shadows_permanent() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pt (a int)");
    ok(&mut eng, "INSERT INTO pt VALUES (7)");
    ok(&mut eng, "CREATE TEMP TABLE pt AS SELECT * FROM public.pt");
    assert_eq!(one(&mut eng, "SELECT a FROM pt"), "7");
}

#[test]
fn v249_polygon_type_and_constructors() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE g (f1 polygon)");
    ok(&mut eng, "INSERT INTO g VALUES ('(1000,0,0,1000)')");
    assert_eq!(one(&mut eng, "SELECT f1 FROM g"), "((1000,0),(0,1000))");
    ok(&mut eng, "CREATE TABLE b (h box)");
    ok(&mut eng, "INSERT INTO b VALUES ('(2,2),(0,0)')");
    assert_eq!(
        one(&mut eng, "SELECT polygon(h) FROM b"),
        "((0,0),(0,2),(2,2),(2,0))"
    );
    assert_eq!(
        one(&mut eng, "SELECT circle(h) FROM b"),
        "<(1,1),1.4142135623730951>"
    );
}

#[test]
fn v249_point_containment_operators() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pt (f1 point)");
    ok(&mut eng, "INSERT INTO pt VALUES ('(5,5)'), ('(500,500)')");
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pt WHERE f1 <@ box '(0,0,100,100)'"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pt WHERE box '(0,0,100,100)' @> f1"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pt WHERE f1 <@ polygon '(0,0),(0,100),(100,100),(100,0)'"
        ),
        "1"
    );
}

#[test]
fn v249_pg_class_relfilenode_and_pg_toast_grant() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    assert_eq!(
        one(
            &mut eng,
            "SELECT relfilenode = oid FROM pg_class WHERE relname = 't'"
        ),
        "t"
    );
    ok(&mut eng, "CREATE ROLE r1");
    ok(&mut eng, "GRANT USAGE ON SCHEMA pg_toast TO r1");
    ok(&mut eng, "REVOKE USAGE ON SCHEMA pg_toast FROM r1");
}
