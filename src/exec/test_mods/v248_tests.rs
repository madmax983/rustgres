/// v2.48: FOREIGN_KEY REMAINDER — VALIDATE CONSTRAINT privilege +
/// NOT ENFORCED rules, ALTER CONSTRAINT parse conflicts, RI checks
/// running as the referenced table's owner, convalidated :=
/// is_enforced on enforceability flips, NO ACTION / SET DEFAULT
/// statement-final-image semantics, DETACH/DROP guards for
/// FK-referenced partition trees, ATTACH merge guards, operator
/// family DDL parsing, pg_partition_tree + regclass UNION coercion,
/// and pg_constraint referenced-side clones.
/// Ground truth: PG19 tablecmds.c / ri_triggers.c; vendored
/// foreign_key suite. Scope record: hidden_files/v248/scope.md.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run_as(eng: &mut Engine, role: &str, sql: &str) -> Result<ExecResult, ExecError> {
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
    run_as(eng, "postgres", sql)
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

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
            .collect(),
        other => panic!("expected Select for {sql} -> {other:?}"),
    }
}

#[test]
fn v248_alter_constraint_parse_conflicts() {
    // PG19 gram.y: ENFORCED + NOT ENFORCED in one ALTER CONSTRAINT
    // is a 42601 conflict; NOT VALID can never be altered to.
    let e = parse_statement("ALTER TABLE t ALTER CONSTRAINT c ENFORCED NOT ENFORCED").unwrap_err();
    assert_eq!(e.code, "42601");
    assert!(e.message.contains("conflicting constraint properties"));
    let e = parse_statement("ALTER TABLE t ALTER CONSTRAINT c NOT VALID").unwrap_err();
    assert_eq!(e.code, "42601");
    assert!(e.message.contains("cannot be altered to be NOT VALID"));
}

#[test]
fn v248_validate_not_enforced_and_privileges() {
    let mut eng = engine();
    ok(&mut eng, "CREATE USER fkowner NOLOGIN");
    ok(&mut eng, "CREATE TABLE pk (a int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TABLE fk (a int REFERENCES pk NOT ENFORCED)",
    );
    // VALIDATE CONSTRAINT on a NOT ENFORCED constraint: 55000.
    let e = err(&mut eng, "ALTER TABLE fk VALIDATE CONSTRAINT fk_a_fkey");
    assert_eq!(e.code, "55000");
    // VALIDATE by a role lacking SELECT on the FK table / the
    // referenced key columns: 42501 (RI_Initial_Check).
    ok(
        &mut eng,
        "ALTER TABLE fk ALTER CONSTRAINT fk_a_fkey ENFORCED",
    );
    let e = run_as(
        &mut eng,
        "fkowner",
        "ALTER TABLE fk VALIDATE CONSTRAINT fk_a_fkey",
    )
    .unwrap_err();
    assert_eq!(e.code, "42501");
}

#[test]
fn v248_fk_check_runs_as_pk_owner() {
    let mut eng = engine();
    ok(&mut eng, "CREATE USER pkowner NOLOGIN");
    ok(&mut eng, "CREATE TABLE pkt (a int PRIMARY KEY, b text)");
    ok(&mut eng, "ALTER TABLE pkt OWNER TO pkowner");
    ok(&mut eng, "CREATE TABLE fkt (a int REFERENCES pkt)");
    ok(&mut eng, "INSERT INTO pkt VALUES (2, 'x')");
    ok(&mut eng, "REVOKE SELECT ON pkt FROM pkowner");
    // The per-row check runs as the PK table's owner: without the
    // owner's SELECT on the key column it is 42501, whoever inserts.
    let e = err(&mut eng, "INSERT INTO fkt VALUES (2)");
    assert_eq!(e.code, "42501");
    // SELECT on the key column alone suffices while the owner
    // still holds UPDATE from the default ACL (the suite's shape).
    ok(&mut eng, "GRANT SELECT (a) ON pkt TO pkowner");
    ok(&mut eng, "INSERT INTO fkt VALUES (2)");
    // FOR KEY SHARE also requires UPDATE on some column of pkt:
    // revoking it makes the check fail again.
    ok(&mut eng, "DELETE FROM fkt");
    ok(&mut eng, "REVOKE UPDATE ON pkt FROM pkowner");
    let e = err(&mut eng, "INSERT INTO fkt VALUES (2)");
    assert_eq!(e.code, "42501");
    // UPDATE on any one column (even an unrelated one) restores it.
    ok(&mut eng, "GRANT UPDATE (b) ON pkt TO pkowner");
    ok(&mut eng, "INSERT INTO fkt VALUES (2)");
}

#[test]
fn v248_enforceability_flip_resets_convalidated() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pk (a int, b int, PRIMARY KEY (a, b))",
    );
    ok(&mut eng, "CREATE TABLE fk (a int, b int)");
    ok(&mut eng, "INSERT INTO pk VALUES (1, 2)");
    ok(&mut eng, "INSERT INTO fk VALUES (1, 2)");
    ok(
        &mut eng,
        "ALTER TABLE fk ADD CONSTRAINT fk_con FOREIGN KEY (a, b) REFERENCES pk NOT VALID NOT ENFORCED",
    );
    // Born NOT ENFORCED: never validated.
    let r = rows(
        &mut eng,
        "SELECT conenforced, convalidated FROM pg_constraint WHERE conname = 'fk_con'",
    );
    assert_eq!(r, vec![vec!["f".to_string(), "f".to_string()]]);
    ok(&mut eng, "ALTER TABLE fk ALTER CONSTRAINT fk_con ENFORCED");
    let r = rows(
        &mut eng,
        "SELECT conenforced, convalidated FROM pg_constraint WHERE conname = 'fk_con'",
    );
    assert_eq!(r, vec![vec!["t".to_string(), "t".to_string()]]);
    // PG19: any enforceability alteration sets convalidated :=
    // is_enforced, so flipping back clears it again.
    ok(
        &mut eng,
        "ALTER TABLE fk ALTER CONSTRAINT fk_con NOT ENFORCED NOT DEFERRABLE",
    );
    let r = rows(
        &mut eng,
        "SELECT condeferrable, conenforced, convalidated FROM pg_constraint WHERE conname = 'fk_con'",
    );
    assert_eq!(
        r,
        vec![vec!["f".to_string(), "f".to_string(), "f".to_string()]]
    );
}

#[test]
fn v248_no_action_and_set_default_final_image() {
    let mut eng = engine();
    // NO ACTION checks the statement's final image: {11,12} -> {12,13}
    // keeps key 13 referenced-and-present once cc holds it.
    ok(&mut eng, "CREATE TEMP TABLE pp (f1 int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TEMP TABLE cc (f1 int REFERENCES pp ON UPDATE NO ACTION ON DELETE NO ACTION)",
    );
    ok(&mut eng, "INSERT INTO pp VALUES (12), (11)");
    ok(&mut eng, "UPDATE pp SET f1 = f1 + 1");
    ok(&mut eng, "INSERT INTO cc VALUES (13)");
    ok(&mut eng, "UPDATE pp SET f1 = f1 + 1");
    let e = err(&mut eng, "UPDATE pp SET f1 = f1 + 1");
    assert_eq!(e.code, "23503");
    // ON DELETE SET DEFAULT: the default key must survive in the
    // final image; deleting the default row itself fails 23503.
    ok(&mut eng, "CREATE TEMP TABLE defp (f1 int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TEMP TABLE defc (f1 int DEFAULT 0 REFERENCES defp ON DELETE SET DEFAULT)",
    );
    ok(&mut eng, "INSERT INTO defp VALUES (0), (1), (2)");
    ok(&mut eng, "INSERT INTO defc VALUES (2)");
    ok(&mut eng, "DELETE FROM defp WHERE f1 = 2");
    let e = err(&mut eng, "DELETE FROM defp WHERE f1 = 0");
    assert_eq!(e.code, "23503");
}

#[test]
fn v248_detach_and_drop_guards() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pk (a int PRIMARY KEY) PARTITION BY LIST (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pk1 PARTITION OF pk FOR VALUES IN (1)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pk2 PARTITION OF pk FOR VALUES IN (2)",
    );
    ok(&mut eng, "CREATE TABLE fk (a int REFERENCES pk)");
    ok(&mut eng, "INSERT INTO pk VALUES (1)");
    ok(&mut eng, "INSERT INTO fk VALUES (1)");
    // DETACH of a partition whose keys are still referenced: 23503.
    let e = err(&mut eng, "ALTER TABLE pk DETACH PARTITION pk1");
    assert_eq!(e.code, "23503");
    // DROP of any partition of a referenced root: 2BP01.
    let e = err(&mut eng, "DROP TABLE pk2");
    assert_eq!(e.code, "2BP01");
    ok(&mut eng, "DELETE FROM fk");
    ok(&mut eng, "ALTER TABLE pk DETACH PARTITION pk1");
    let e = err(&mut eng, "DROP TABLE pk2");
    assert_eq!(e.code, "2BP01");
}

#[test]
fn v248_attach_merge_guards() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pk (a int, b int, PRIMARY KEY (a, b))",
    );
    ok(
        &mut eng,
        "CREATE TABLE fk (a int, b int) PARTITION BY LIST (a)",
    );
    ok(
        &mut eng,
        "ALTER TABLE fk ADD FOREIGN KEY (a, b) REFERENCES pk ON UPDATE CASCADE ON DELETE CASCADE",
    );
    ok(
        &mut eng,
        "CREATE TABLE fk_2 (b int, a int, CONSTRAINT fk_part_con FOREIGN KEY (a, b) REFERENCES pk ON UPDATE CASCADE ON DELETE CASCADE NOT ENFORCED)",
    );
    // Shape-matching child FK with different enforceability: 42P16.
    let e = err(
        &mut eng,
        "ALTER TABLE fk ATTACH PARTITION fk_2 FOR VALUES IN (1500, 1502)",
    );
    assert_eq!(e.code, "42P16");
    // Attaching the referenced table itself under its referencer:
    // 0A000.
    ok(&mut eng, "CREATE TABLE pk6 (a int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TABLE fk6 (a int REFERENCES pk6) PARTITION BY LIST (a)",
    );
    let e = err(
        &mut eng,
        "ALTER TABLE fk6 ATTACH PARTITION pk6 FOR VALUES IN (1)",
    );
    assert_eq!(e.code, "0A000");
}

#[test]
fn v248_operator_family_ddl_parses() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE OPERATOR CLASS int_ops FOR TYPE integer USING btree FAMILY fam AS OPERATOR 1 <(integer,integer), OPERATOR 2 <=(integer,integer)",
    );
    ok(
        &mut eng,
        "ALTER OPERATOR FAMILY fam USING btree ADD OPERATOR 1 <(integer,bigint)",
    );
    ok(
        &mut eng,
        "ALTER OPERATOR FAMILY fam USING btree ADD OPERATOR 3 <(integer,bigint)",
    );
    // DROP items carry no operator name in PG19's grammar.
    ok(
        &mut eng,
        "ALTER OPERATOR FAMILY fam USING btree DROP OPERATOR 3 (integer,bigint)",
    );
    // pg_proc cross-type equality is a valid operator procedure.
    ok(
        &mut eng,
        "CREATE OPERATOR =#= (LEFTARG = integer, RIGHTARG = bigint, FUNCTION = int48eq)",
    );
    ok(
        &mut eng,
        "CREATE OPERATOR CLASS ri_ops FOR TYPE int4 USING btree AS OPERATOR 1 pg_catalog.<, OPERATOR 2 pg_catalog.<=",
    );
}

#[test]
fn v248_operator_family_validation() {
    let mut eng = engine();
    ok(&mut eng, "CREATE OPERATOR FAMILY alt_opf4 USING btree");
    // Duplicate family: 42710.
    let e = err(&mut eng, "CREATE OPERATOR FAMILY alt_opf4 USING btree");
    assert_eq!(e.code, "42710");
    // Invalid access method is checked before family existence.
    let e = err(
        &mut eng,
        "ALTER OPERATOR FAMILY alt_opf4 USING invalid_index_method ADD OPERATOR 1 < (int4, int2)",
    );
    assert_eq!(e.code, "42704");
    // Strategy range for btree is 1..5.
    let e = err(
        &mut eng,
        "ALTER OPERATOR FAMILY alt_opf4 USING btree ADD OPERATOR 6 < (int4, int2)",
    );
    assert_eq!(e.code, "42P16");
    // ADD OPERATOR requires argument types.
    let e = err(
        &mut eng,
        "ALTER OPERATOR FAMILY alt_opf4 USING btree ADD OPERATOR 1 <",
    );
    assert_eq!(e.code, "42P16");
    // Textbook add/drop cycle works; duplicates are 42710.
    ok(
        &mut eng,
        "ALTER OPERATOR FAMILY alt_opf4 USING btree ADD OPERATOR 1 < (int4, int2), FUNCTION 1 btint42cmp(int4, int2)",
    );
    let e = err(
        &mut eng,
        "ALTER OPERATOR FAMILY alt_opf4 USING btree ADD OPERATOR 1 < (int4, int2)",
    );
    assert_eq!(e.code, "42710");
    ok(
        &mut eng,
        "ALTER OPERATOR FAMILY alt_opf4 USING btree DROP OPERATOR 1 (int4, int2), FUNCTION 1 (int4, int2)",
    );
    // Dropping an absent item: 42704.
    let e = err(
        &mut eng,
        "ALTER OPERATOR FAMILY alt_opf4 USING btree DROP OPERATOR 1 (int4, int2)",
    );
    assert_eq!(e.code, "42704");
    // DROP OPERATOR with three argument types: error.
    let e = err(
        &mut eng,
        "ALTER OPERATOR FAMILY alt_opf4 USING btree DROP OPERATOR 1 (int4, int2, int8)",
    );
    assert_eq!(e.code, "42P16");
    ok(&mut eng, "DROP OPERATOR FAMILY alt_opf4 USING btree");
    // Missing family: 42704; IF EXISTS + bad AM still errors.
    let e = err(&mut eng, "DROP OPERATOR FAMILY alt_opf4 USING btree");
    assert_eq!(e.code, "42704");
    ok(
        &mut eng,
        "DROP OPERATOR FAMILY IF EXISTS alt_opf4 USING btree",
    );
    let e = err(
        &mut eng,
        "DROP OPERATOR FAMILY IF EXISTS alt_opf4 USING no_such_am",
    );
    assert_eq!(e.code, "42704");
    // ADD/DROP requires superuser.
    ok(&mut eng, "CREATE OPERATOR FAMILY alt_opf5 USING btree");
    ok(&mut eng, "CREATE ROLE opf_user NOSUPERUSER");
    let e = run_as(
        &mut eng,
        "opf_user",
        "ALTER OPERATOR FAMILY alt_opf5 USING btree ADD OPERATOR 1 < (int4, int2)",
    )
    .unwrap_err();
    assert_eq!(e.code, "42501");
    // A class created INTO a family blocks its DROP (2BP01);
    // CASCADE drops both.
    ok(
        &mut eng,
        "CREATE OPERATOR CLASS oc_fam FOR TYPE integer USING btree FAMILY alt_opf5 AS OPERATOR 1 <",
    );
    let e = err(&mut eng, "DROP OPERATOR FAMILY alt_opf5 USING btree");
    assert_eq!(e.code, "2BP01");
    ok(
        &mut eng,
        "DROP OPERATOR FAMILY alt_opf5 USING btree CASCADE",
    );
}

#[test]
fn v248_partition_tree_union_and_ref_clones() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pk (a int, b int, PRIMARY KEY (a, b)) PARTITION BY RANGE (a, b)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pk_1 PARTITION OF pk FOR VALUES FROM (0,0) TO (1000,1000)",
    );
    ok(
        &mut eng,
        "CREATE TABLE pk_2 PARTITION OF pk FOR VALUES FROM (1000,1000) TO (2000,2000)",
    );
    ok(&mut eng, "CREATE TABLE fk (b int, a int)");
    ok(
        &mut eng,
        "ALTER TABLE fk ADD CONSTRAINT fk_ab FOREIGN KEY (a, b) REFERENCES pk NOT VALID",
    );
    // Referenced-side clones: root + one per referenced partition.
    let r = rows(
        &mut eng,
        "SELECT conname, convalidated FROM pg_constraint WHERE conrelid = 'fk'::regclass AND contype = 'f' ORDER BY conname",
    );
    assert_eq!(
        r,
        vec![
            vec!["fk_ab".to_string(), "f".to_string()],
            vec!["fk_ab_1".to_string(), "f".to_string()],
            vec!["fk_ab_2".to_string(), "f".to_string()],
        ]
    );
    // pg_partition_tree in a UNION with a regclass literal (the
    // pg_trigger IN-subquery shape) resolves names to OIDs.
    let r = rows(
        &mut eng,
        "SELECT count(*) FROM (SELECT relid FROM pg_partition_tree('pk'::regclass) UNION ALL SELECT 'pk'::regclass) u",
    );
    assert_eq!(r, vec![vec!["4".to_string()]]);
}
