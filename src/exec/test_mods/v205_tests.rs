use super::*;

// v2.05: REPLICA IDENTITY USING INDEX ON PARTITIONED TABLES +
// DROP COLUMN DEPENDENCY FAMILY (+ publication permissions /
// GRANT ON DATABASE). PG19 grounding: tablecmds.c
// ATExecReplicaIdentity / ATExecDropColumn, catalog/dependency.c
// AUTO vs NORMAL dependents, publicationcmds.c permission model.

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(31000);

fn run_as(eng: &mut Engine, role: &str, sql: &str) -> Result<ExecResult, ExecError> {
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

fn ok_as(eng: &mut Engine, role: &str, sql: &str) {
    run_as(eng, role, sql)
        .unwrap_or_else(|e| panic!("{sql} as {role} -> {} {}", e.code, e.message));
}

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    run(eng, sql).expect_err(&format!("{sql} should fail"))
}

fn err_as(eng: &mut Engine, role: &str, sql: &str) -> ExecError {
    run_as(eng, role, sql).expect_err(&format!("{sql} as {role} should fail"))
}

#[test]
fn v205_ri_using_index_on_partition_of_child() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE p8 (a int, b text, c text) PARTITION BY HASH (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE p8_0 PARTITION OF p8 FOR VALUES WITH (modulus 2, remainder 0)",
    );
    ok(&mut eng, "ALTER TABLE p8_0 ADD PRIMARY KEY (a)");
    // The backing index must be findable under the constraint's own
    // name (was: double-prefixed p8_0_p8_0_pkey -> 42704).
    ok(
        &mut eng,
        "ALTER TABLE p8_0 REPLICA IDENTITY USING INDEX p8_0_pkey",
    );
}

#[test]
fn v205_ri_using_index_after_alter_add_pk_and_null_rule() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE s0 (a int, b text)");
    ok(&mut eng, "ALTER TABLE s0 ADD PRIMARY KEY (a)");
    // ADD PRIMARY KEY implies NOT NULL, so the RI nullable check
    // passes (was 42809).
    ok(
        &mut eng,
        "ALTER TABLE s0 REPLICA IDENTITY USING INDEX s0_pkey",
    );
    // A PK over existing NULLs fails 23502.
    ok(&mut eng, "CREATE TABLE s1 (a int, b text)");
    ok(&mut eng, "INSERT INTO s1 VALUES (NULL, 'x')");
    let e = err(&mut eng, "ALTER TABLE s1 ADD PRIMARY KEY (a)");
    assert_eq!(e.code, "23502", "{}", e.message);
}

#[test]
fn v205_drop_column_publication_dependency() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t5 (a int PRIMARY KEY, b text, c text, d text)",
    );
    ok(&mut eng, "CREATE PUBLICATION pub5 FOR TABLE t5 (a, c)");
    let e = err(&mut eng, "ALTER TABLE t5 DROP COLUMN c");
    assert_eq!(e.code, "2BP01", "{}", e.message);
    // CASCADE strips the column from the publication's list.
    ok(&mut eng, "ALTER TABLE t5 DROP COLUMN c CASCADE");
    let p = eng.db.publications.get("pub5").expect("pub survives");
    assert_eq!(
        p.table_specs.get("t5").and_then(|s| s.cols.clone()),
        Some(vec!["a".to_string()])
    );
}

#[test]
fn v205_drop_column_row_filter_dependency() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE tf (a int, b int)");
    ok(
        &mut eng,
        "CREATE PUBLICATION pubf FOR TABLE tf WHERE (b > 1)",
    );
    let e = err(&mut eng, "ALTER TABLE tf DROP COLUMN b");
    assert_eq!(e.code, "2BP01", "{}", e.message);
    // CASCADE removes the table entry from the publication.
    ok(&mut eng, "ALTER TABLE tf DROP COLUMN b CASCADE");
    let p = eng.db.publications.get("pubf").expect("pub survives");
    assert!(!p.table_specs.contains_key("tf"));
}

#[test]
fn v205_drop_column_own_objects_are_auto() {
    let mut eng = engine();
    // Own CHECK + plain index ride along without CASCADE (PG19
    // DEPENDENCY_AUTO; alter_table atacc1 / old_system_table).
    ok(
        &mut eng,
        "CREATE TABLE ac (id serial primary key, value int check (value < 10))",
    );
    ok(&mut eng, "ALTER TABLE ac DROP COLUMN value");
    ok(&mut eng, "CREATE TABLE os (id int, othercol text)");
    ok(&mut eng, "CREATE INDEX os__othercol ON os (othercol)");
    ok(&mut eng, "ALTER TABLE os DROP COLUMN othercol");
}

#[test]
fn v205_drop_column_generated_dependency_blocks() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE gt (a int, b int GENERATED ALWAYS AS (a + 1) STORED)",
    );
    let e = err(&mut eng, "ALTER TABLE gt DROP COLUMN a");
    assert_eq!(e.code, "2BP01", "{}", e.message);
}

#[test]
fn v205_drop_table_set_with_internal_fk() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE ck1 (id int PRIMARY KEY, t text)");
    ok(&mut eng, "CREATE TABLE ck2 (id int REFERENCES ck1, t text)");
    // Both members of the DROP list: the FK between them does not
    // block (PG19 performMultipleDeletions).
    ok(&mut eng, "DROP TABLE ck1, ck2");
    // A lone drop of the referenced table still blocks.
    ok(&mut eng, "CREATE TABLE ck3 (id int PRIMARY KEY, t text)");
    ok(&mut eng, "CREATE TABLE ck4 (id int REFERENCES ck3, t text)");
    let e = err(&mut eng, "DROP TABLE ck3");
    assert_eq!(e.code, "2BP01", "{}", e.message);
}

#[test]
fn v205_partitioned_update_checks_all_leaves() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE q8 (a int, b text, c text) PARTITION BY HASH (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE q8_0 PARTITION OF q8 FOR VALUES WITH (modulus 2, remainder 0)",
    );
    ok(&mut eng, "ALTER TABLE q8_0 ADD PRIMARY KEY (a)");
    ok(
        &mut eng,
        "ALTER TABLE q8_0 REPLICA IDENTITY USING INDEX q8_0_pkey",
    );
    ok(
        &mut eng,
        "CREATE TABLE q8_1 PARTITION OF q8 FOR VALUES WITH (modulus 2, remainder 1)",
    );
    ok(&mut eng, "ALTER TABLE q8_1 ADD PRIMARY KEY (b)");
    ok(
        &mut eng,
        "ALTER TABLE q8_1 REPLICA IDENTITY USING INDEX q8_1_pkey",
    );
    ok(
        &mut eng,
        "CREATE PUBLICATION qpub FOR TABLE q8 (a, b) WITH (publish_via_partition_root = 'true')",
    );
    // Empty leaves: PG still errors when the column list does not
    // cover a leaf's replica identity (init-time check).
    ok(&mut eng, "UPDATE q8 SET a = 1");
    ok(&mut eng, "ALTER PUBLICATION qpub DROP TABLE q8");
    ok(&mut eng, "ALTER PUBLICATION qpub ADD TABLE q8 (a, c)");
    let e = err(&mut eng, "UPDATE q8 SET a = 1");
    assert_eq!(e.code, "42P10", "{}", e.message);
}

#[test]
fn v205_grant_on_database_and_publication_permissions() {
    let mut eng = engine();
    ok(&mut eng, "CREATE ROLE pub_u2");
    ok(&mut eng, "CREATE TABLE pt1 (a int)");
    // Named DATABASE form parses and executes (was 42601).
    ok(&mut eng, "GRANT CREATE ON DATABASE postgres TO pub_u2");
    ok_as(&mut eng, "pub_u2", "CREATE PUBLICATION pu2");
    ok(&mut eng, "REVOKE CREATE ON DATABASE postgres FROM pub_u2");
    // Without CREATE on the database, CREATE PUBLICATION is 42501.
    ok(&mut eng, "CREATE ROLE pub_u3");
    let e = err_as(&mut eng, "pub_u3", "CREATE PUBLICATION pu3");
    assert_eq!(e.code, "42501", "{}", e.message);
    // FOR TABLES IN SCHEMA is superuser-only even with CREATE.
    ok(&mut eng, "GRANT CREATE ON DATABASE postgres TO pub_u3");
    ok(&mut eng, "CREATE SCHEMA pub_test");
    let e = err_as(
        &mut eng,
        "pub_u3",
        "CREATE PUBLICATION pu3 FOR TABLES IN SCHEMA pub_test",
    );
    assert_eq!(e.code, "42501", "{}", e.message);
    // ALTER / DROP by a non-owner is 42501; the owner passes.
    let e = err_as(&mut eng, "pub_u3", "ALTER PUBLICATION pu2 RENAME TO pu2x");
    assert_eq!(e.code, "42501", "{}", e.message);
    let e = err_as(&mut eng, "pub_u3", "DROP PUBLICATION pu2");
    assert_eq!(e.code, "42501", "{}", e.message);
    ok_as(&mut eng, "pub_u2", "DROP PUBLICATION pu2");
    // ADD TABLE requires table ownership (or membership in the
    // table owner's role).
    ok(&mut eng, "CREATE PUBLICATION pown");
    let e = err_as(&mut eng, "pub_u3", "ALTER PUBLICATION pown ADD TABLE pt1");
    // pown is owned by postgres, so even the publication-owner gate
    // fires first — still 42501.
    assert_eq!(e.code, "42501", "{}", e.message);
}

#[test]
fn v205_set_drop_expression() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE g29 (a int, b int GENERATED ALWAYS AS (a * 2) STORED)",
    );
    // SET EXPRESSION on a non-generated column: 55000 (PG19
    // ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE).
    let e = err(
        &mut eng,
        "ALTER TABLE g29 ALTER COLUMN a SET EXPRESSION AS (a * 3)",
    );
    assert_eq!(e.code, "55000");
    ok(
        &mut eng,
        "ALTER TABLE g29 ALTER COLUMN b SET EXPRESSION AS (a * 3)",
    );
    ok(&mut eng, "INSERT INTO g29 (a) VALUES (3)");
    // DROP EXPRESSION: b becomes a plain column keeping its value,
    // and the generated dependency on a disappears.
    ok(&mut eng, "ALTER TABLE g29 ALTER COLUMN b DROP EXPRESSION");
    ok(&mut eng, "INSERT INTO g29 (a, b) VALUES (6, 66)");
    ok(&mut eng, "ALTER TABLE g29 DROP COLUMN a");
    // DROP EXPRESSION IF EXISTS on a plain column is a no-op;
    // without IF EXISTS it is 55000.
    ok(
        &mut eng,
        "ALTER TABLE g29 ALTER COLUMN b DROP EXPRESSION IF EXISTS",
    );
    let e = err(&mut eng, "ALTER TABLE g29 ALTER COLUMN b DROP EXPRESSION");
    assert_eq!(e.code, "55000");
}

#[test]
fn v205_drop_column_ignores_stale_index() {
    // An index dropped with an earlier incarnation of the table
    // lingers in the index map (MVCC); DROP COLUMN on a recreated
    // same-named table must not try to drop it again (42P01).
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t1 (col1 int) PARTITION BY RANGE (col1)",
    );
    ok(&mut eng, "CREATE INDEX ON t1 (col1)");
    ok(&mut eng, "CREATE TABLE other (x int)");
    ok(&mut eng, "DROP TABLE t1, other");
    ok(
        &mut eng,
        "CREATE TABLE t1 (col1 int, a int, col2 int, b int) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "ALTER TABLE t1 DROP COLUMN col1, DROP COLUMN col2",
    );
}

#[test]
fn v205_auto_constraint_names_suffix_and_truncate() {
    // PG19 ChooseConstraintName: repeated auto names get numeric
    // suffixes; bases truncate at 63 bytes (foreign_key.out).
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE pk1 (a int PRIMARY KEY)");
    ok(
        &mut eng,
        "CREATE TABLE f3 (a int, FOREIGN KEY (a) REFERENCES pk1, FOREIGN KEY (a) REFERENCES pk1)",
    );
    let t = eng.db.tables.get("f3").expect("f3 exists").last().unwrap();
    let names: Vec<&str> = t.fks.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["f3_a_fkey", "f3_a_fkey1"]);
    ok(
        &mut eng,
        "CREATE TABLE pk2 (a int, b int, PRIMARY KEY (a, b))",
    );
    ok(
        &mut eng,
        "CREATE TABLE fktable2 (a int, very_very_long_column_name_to_exceed_63_characters int, FOREIGN KEY (a, very_very_long_column_name_to_exceed_63_characters) REFERENCES pk2, FOREIGN KEY (a, very_very_long_column_name_to_exceed_63_characters) REFERENCES pk2)",
    );
    let t = eng
        .db
        .tables
        .get("fktable2")
        .expect("fktable2 exists")
        .last()
        .unwrap();
    let names: Vec<&str> = t.fks.iter().map(|f| f.name.as_str()).collect();
    // Exactly the names PG19's foreign_key.out expects.
    assert_eq!(
        names,
        vec![
            "fktable2_a_very_very_long_column_name_to_exceed_63_charact_fkey",
            "fktable2_a_very_very_long_column_name_to_exceed_63_charac_fkey1",
        ]
    );
    assert!(names.iter().all(|n| n.len() <= 63));
}

#[test]
fn v205_database_grant_chain_and_drop_role() {
    let mut eng = engine();
    ok(&mut eng, "CREATE ROLE adm205");
    ok(&mut eng, "CREATE ROLE bob205");
    // A role with no database privilege cannot grant (42501).
    let e = err_as(
        &mut eng,
        "bob205",
        "GRANT CREATE ON DATABASE postgres TO bob205",
    );
    assert_eq!(e.code, "42501");
    ok(
        &mut eng,
        "GRANT CREATE ON DATABASE postgres TO adm205 WITH GRANT OPTION",
    );
    // The grant-option holder may pass the privilege on (PG19).
    ok_as(
        &mut eng,
        "adm205",
        "GRANT CREATE ON DATABASE postgres TO bob205",
    );
    // REVOKE ... CASCADE from the owner strips the chain; after a
    // full revoke the role drops cleanly (the zero-bit ACL residue
    // is an explicit-deny marker, not a dependency).
    ok(
        &mut eng,
        "REVOKE CREATE ON DATABASE postgres FROM adm205 CASCADE",
    );
    ok(&mut eng, "DROP ROLE adm205");
    ok(&mut eng, "DROP ROLE bob205");
    // Plain grant + revoke also leaves the role droppable.
    ok(&mut eng, "CREATE ROLE adm206");
    ok(&mut eng, "GRANT CREATE ON DATABASE postgres TO adm206");
    ok(&mut eng, "REVOKE CREATE ON DATABASE postgres FROM adm206");
    ok(&mut eng, "DROP ROLE adm206");
}

#[test]
fn v205_set_expression_revalidates_not_null() {
    // PG19 gtestnn: SET EXPRESSION whose new expression yields
    // NULL for an existing row in a NOT NULL generated column
    // fails (the rewrite revalidates); a compatible one succeeds.
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE gnn (f1 int, f2 bigint, f3 bigint GENERATED ALWAYS AS (nullif(f1, 1) + nullif(f2, 10)) STORED NOT NULL)",
    );
    ok(
        &mut eng,
        "INSERT INTO gnn VALUES (2, 2, default), (3, 5, default), (14, 12, default)",
    );
    let e = err(
        &mut eng,
        "ALTER TABLE gnn ALTER COLUMN f3 SET EXPRESSION AS (nullif(f1, 2) + nullif(f2, 11))",
    );
    assert_eq!(e.code, "23502");
    // The failed SET left the old expression in place.
    ok(&mut eng, "INSERT INTO gnn VALUES (10, 11, default)");
}

#[test]
fn v205_set_expression_partitioned_parent_revalidates_leaves() {
    // PG19 gtestnn_parent is PARTITION BY RANGE with the rows in
    // the leaves: SET EXPRESSION must validate leaf rows.
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE gnp (f1 int, f2 bigint, f3 bigint GENERATED ALWAYS AS (nullif(f1, 1) + nullif(f2, 10)) STORED NOT NULL) PARTITION BY RANGE (f1)",
    );
    ok(
        &mut eng,
        "CREATE TABLE gnp_c PARTITION OF gnp FOR VALUES FROM (1) TO (5)",
    );
    ok(&mut eng, "CREATE TABLE gnp_d PARTITION OF gnp DEFAULT");
    ok(
        &mut eng,
        "INSERT INTO gnp VALUES (2, 2, default), (3, 5, default), (14, 12, default)",
    );
    // The violating rows live only in the leaves, so a 23502 here
    // proves the validation recursed. (Cross-statement rollback
    // of the parent swap is production abort semantics, covered
    // by the vendored generated_stored suite; the unit harness
    // does not model txn abort.)
    let e = err(
        &mut eng,
        "ALTER TABLE gnp ALTER COLUMN f3 SET EXPRESSION AS (nullif(f1, 2) + nullif(f2, 11))",
    );
    assert_eq!(e.code, "23502");
}
