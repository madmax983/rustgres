// v2.36: FOREIGN DATA (FDW) family — CREATE/ALTER/DROP FOREIGN
// DATA WRAPPER, SERVER, USER MAPPING, FOREIGN TABLE; the option
// merge/validation machinery; the pg_foreign_* catalogs and the
// information_schema foreign views; has_*_privilege; the partition
// and trigger guards the family exposed.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = parse_statement(sql).expect("parses");
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

fn err_code(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql) {
        Ok(_) => panic!("{sql} unexpectedly succeeded"),
        Err(e) => e.code.to_string(),
    }
}

fn rows_text(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) | Ok(ExecResult::Explain { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
            .collect(),
        Ok(other) => panic!("{sql} -> {other:?}"),
        Err(e) => panic!("{sql} -> {} {}", e.code, e.message),
    }
}

fn setup(eng: &mut Engine) {
    ok(eng, "CREATE FOREIGN DATA WRAPPER dummy");
    ok(eng, "CREATE SERVER s0 FOREIGN DATA WRAPPER dummy");
}

#[test]
fn fdw_server_mapping_crud() {
    let mut eng = engine();
    setup(&mut eng);
    // Duplicate names are 42710; missing objects are 42704.
    assert_eq!(
        err_code(&mut eng, "CREATE FOREIGN DATA WRAPPER dummy"),
        "42710"
    );
    assert_eq!(
        err_code(&mut eng, "CREATE SERVER s0 FOREIGN DATA WRAPPER dummy"),
        "42710"
    );
    assert_eq!(
        err_code(&mut eng, "CREATE SERVER sx FOREIGN DATA WRAPPER nope"),
        "42704"
    );
    // Foreign table registers and shows in pg_foreign_table.
    ok(
        &mut eng,
        "CREATE FOREIGN TABLE ft (a int, b text) SERVER s0 OPTIONS (filename 'x')",
    );
    let rows = rows_text(
        &mut eng,
        "SELECT ftserver::regclass::text FROM pg_foreign_table",
    );
    assert_eq!(rows.len(), 1);
    // User mappings: create, duplicate -> 42710, drop, missing -> 42704.
    ok(
        &mut eng,
        "CREATE USER MAPPING FOR postgres SERVER s0 OPTIONS (user 'u')",
    );
    assert_eq!(
        err_code(&mut eng, "CREATE USER MAPPING FOR postgres SERVER s0"),
        "42710"
    );
    ok(&mut eng, "DROP USER MAPPING FOR postgres SERVER s0");
    assert_eq!(
        err_code(&mut eng, "DROP USER MAPPING FOR postgres SERVER s0"),
        "42704"
    );
    // DROP SERVER is blocked by the foreign table (2BP01), then
    // CASCADE takes the table with it.
    assert_eq!(err_code(&mut eng, "DROP SERVER s0"), "2BP01");
    ok(&mut eng, "DROP SERVER s0 CASCADE");
    assert!(rows_text(&mut eng, "SELECT * FROM pg_foreign_table").is_empty());
    ok(&mut eng, "DROP FOREIGN DATA WRAPPER dummy");
}

#[test]
fn fdw_option_merge_and_validation() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE FOREIGN DATA WRAPPER pg_fdw VALIDATOR postgresql_fdw_validator",
    );
    // postgresql_fdw server options are libpq keys; a bogus key is
    // 42710-style invalid (PG uses 22023 for the validator reject).
    ok(
        &mut eng,
        "CREATE SERVER ps FOREIGN DATA WRAPPER pg_fdw OPTIONS (host 'h', dbname 'd')",
    );
    let bad = run(
        &mut eng,
        "CREATE SERVER ps2 FOREIGN DATA WRAPPER pg_fdw OPTIONS (bogus '1')",
    );
    assert!(bad.is_err());
    // ADD of an existing option errors; SET replaces; DROP removes.
    ok(
        &mut eng,
        "ALTER SERVER ps OPTIONS (SET host 'h2', DROP dbname)",
    );
    assert_eq!(
        err_code(&mut eng, "ALTER SERVER ps OPTIONS (ADD host 'x')"),
        "42710"
    );
    let rows = rows_text(
        &mut eng,
        "SELECT srvoptions::text FROM pg_foreign_server WHERE srvname = 'ps'",
    );
    assert!(rows[0][0].contains("h2"), "got {rows:?}");
    // Duplicate option in one list is 42710.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE SERVER ps3 FOREIGN DATA WRAPPER pg_fdw OPTIONS (host 'a', host 'b')"
        ),
        "42710"
    );
}

#[test]
fn foreign_table_pk_rejected_and_scan_55000() {
    let mut eng = engine();
    setup(&mut eng);
    // PK on a foreign table is 0A000.
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FOREIGN TABLE bad (a int PRIMARY KEY) SERVER s0"
        ),
        "0A000"
    );
    ok(&mut eng, "CREATE FOREIGN TABLE ft (a int) SERVER s0");
    // Handler-less FDW: scans and DML are 55000.
    assert_eq!(err_code(&mut eng, "SELECT * FROM ft"), "55000");
    assert_eq!(err_code(&mut eng, "INSERT INTO ft VALUES (1)"), "55000");
    assert_eq!(err_code(&mut eng, "DELETE FROM ft"), "55000");
    // Indexes on foreign tables are 42809.
    assert_eq!(err_code(&mut eng, "CREATE INDEX i ON ft (a)"), "42809");
    // Constraint triggers on foreign tables are 42809; plain
    // statement triggers are allowed.
    ok(
        &mut eng,
        "CREATE FUNCTION trg() RETURNS trigger AS $$ BEGIN RETURN NULL; END $$ LANGUAGE plpgsql",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE CONSTRAINT TRIGGER ct AFTER INSERT ON ft FOR EACH ROW EXECUTE FUNCTION trg()"
        ),
        "42809"
    );
    ok(
        &mut eng,
        "CREATE TRIGGER st BEFORE INSERT ON ft FOR EACH STATEMENT EXECUTE FUNCTION trg()",
    );
}

#[test]
fn has_privilege_family() {
    let mut eng = engine();
    setup(&mut eng);
    ok(&mut eng, "CREATE ROLE alice");
    // Owner (postgres is superuser here) has USAGE; alice does not
    // until granted. Name-form missing object is 42704; OID-form
    // missing is NULL.
    let r = rows_text(
        &mut eng,
        "SELECT has_server_privilege('alice', 's0', 'USAGE')::text",
    );
    assert_eq!(r[0][0], "false");
    ok(&mut eng, "GRANT USAGE ON FOREIGN SERVER s0 TO alice");
    let r = rows_text(
        &mut eng,
        "SELECT has_server_privilege('alice', 's0', 'USAGE')::text",
    );
    assert_eq!(r[0][0], "true");
    let r = rows_text(
        &mut eng,
        "SELECT has_foreign_data_wrapper_privilege('alice', 'dummy', 'USAGE')::text",
    );
    assert_eq!(r[0][0], "false");
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT has_server_privilege('alice', 'nosuch', 'USAGE')"
        ),
        "42704"
    );
    let r = rows_text(
        &mut eng,
        "SELECT has_server_privilege('alice', 999999, 'USAGE') IS NULL",
    );
    assert_eq!(r[0][0], "t");
    // Bad privilege string is 22023 and is checked before lookup.
    assert_eq!(
        err_code(
            &mut eng,
            "SELECT has_server_privilege('alice', 'nosuch', 'SELECT')"
        ),
        "22023"
    );
}

#[test]
fn rowtype_column_and_alter_guard() {
    // PG19: a relation's row type is a usable column type, and
    // while a column uses it, ALTER COLUMN TYPE on the source
    // relation is 0A000 (find_composite_type_dependencies).
    let mut eng = engine();
    setup(&mut eng);
    ok(
        &mut eng,
        "CREATE FOREIGN TABLE ft1 (c1 integer, c8 integer) SERVER s0",
    );
    ok(&mut eng, "CREATE TABLE use_ft1_column_type (x ft1)");
    assert_eq!(
        err_code(
            &mut eng,
            "ALTER FOREIGN TABLE ft1 ALTER COLUMN c8 SET DATA TYPE text"
        ),
        "0A000"
    );
    // Ordinary tables behave the same, with the plain wording.
    ok(&mut eng, "CREATE TABLE plain_t (a integer)");
    ok(&mut eng, "CREATE TABLE use_plain_type (x plain_t)");
    assert_eq!(
        err_code(
            &mut eng,
            "ALTER TABLE plain_t ALTER COLUMN a SET DATA TYPE text"
        ),
        "0A000"
    );
    // Dropping the using table releases the source.
    ok(&mut eng, "DROP TABLE use_ft1_column_type");
    ok(
        &mut eng,
        "ALTER FOREIGN TABLE ft1 ALTER COLUMN c8 SET DATA TYPE text",
    );
    // An unknown name is still 42704.
    assert_eq!(
        err_code(&mut eng, "CREATE TABLE bad_t (x nosuchtype)"),
        "42704"
    );
}

#[test]
fn info_schema_foreign_views() {
    let mut eng = engine();
    setup(&mut eng);
    ok(
        &mut eng,
        "CREATE FOREIGN TABLE ft (a int) SERVER s0 OPTIONS (k 'v')",
    );
    let rows = rows_text(
        &mut eng,
        "SELECT foreign_data_wrapper_name, foreign_data_wrapper_language FROM information_schema.foreign_data_wrappers",
    );
    assert_eq!(rows, vec![vec!["dummy".to_string(), "c".to_string()]]);
    let rows = rows_text(
        &mut eng,
        "SELECT foreign_table_name, foreign_server_name FROM information_schema.foreign_tables",
    );
    assert_eq!(rows, vec![vec!["ft".to_string(), "s0".to_string()]]);
    let rows = rows_text(
        &mut eng,
        "SELECT option_name, option_value FROM information_schema.foreign_table_options",
    );
    assert_eq!(rows, vec![vec!["k".to_string(), "v".to_string()]]);
}

#[test]
fn partition_guards_for_foreign_tables() {
    let mut eng = engine();
    setup(&mut eng);
    ok(&mut eng, "CREATE TABLE p (a int) PARTITION BY LIST (a)");
    ok(
        &mut eng,
        "CREATE FOREIGN TABLE pc PARTITION OF p FOR VALUES IN (1) SERVER s0",
    );
    // ADD COLUMN directly on a partition is 42809.
    assert_eq!(
        err_code(&mut eng, "ALTER TABLE pc ADD COLUMN b int"),
        "42809"
    );
    // A permanent relation cannot be a partition of a temp parent.
    ok(
        &mut eng,
        "CREATE TEMP TABLE tp (a int) PARTITION BY LIST (a)",
    );
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE FOREIGN TABLE xp PARTITION OF tp DEFAULT SERVER s0"
        ),
        "42809"
    );
}

#[test]
fn alter_column_type_using_foreign_descendant() {
    let mut eng = engine();
    setup(&mut eng);
    ok(&mut eng, "CREATE TABLE base (a int, b text)");
    ok(
        &mut eng,
        "CREATE FOREIGN TABLE child () INHERITS (base) SERVER s0",
    );
    // USING on the parent forces a rewrite of the foreign child:
    // 42809. Without USING it is metadata-only and succeeds.
    assert_eq!(
        err_code(
            &mut eng,
            "ALTER TABLE base ALTER COLUMN b TYPE char(10) USING '0'"
        ),
        "42809"
    );
    ok(&mut eng, "ALTER TABLE base ALTER COLUMN b TYPE char(10)");
}
