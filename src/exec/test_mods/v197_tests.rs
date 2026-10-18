/// v1.97: DDL dependency rewriting — dependent view/matview
/// definition rewriting for RENAME COLUMN, ALTER VIEW RENAME
/// COLUMN, view column-name arity, ALTER TYPE attribute grammar
/// + dependency semantics (typed tables, stored composite values,
/// domain CHECK revalidation), ALTER DOMAIN ADD CONSTRAINT data
/// validation, and CTAS named-composite columns.
/// Ground truth: PG19 src/test/regress/sql/{matview,create_view,
/// alter_table,domain}.sql + expected outputs; tablecmds.c /
/// typecmds.c / createas.c.
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

fn rows(eng: &mut Engine, sql: &str) -> Vec<Vec<Option<String>>> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn cols(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message)) {
        ExecResult::Select { columns, .. } => columns.iter().map(|(n, _)| n.clone()).collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn err(eng: &mut Engine, sql: &str) -> ExecError {
    match run(eng, sql) {
        Err(e) => e,
        Ok(r) => panic!("expected error for {sql}, got {r:?}"),
    }
}

fn ok(eng: &mut Engine, sql: &str) {
    run(eng, sql).unwrap_or_else(|e| panic!("{sql} -> {} {}", e.code, e.message));
}

#[test]
fn v197_matview_rename_rewrites_dependents() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE mvtest_v (i int, j int)");
    ok(&mut eng, "INSERT INTO mvtest_v VALUES (1, 2)");
    // Too many column names: PG19 createas.c 42601.
    let e = err(
        &mut eng,
        "CREATE MATERIALIZED VIEW mvtest_mv_v (ii, jj, kk) AS SELECT i, j FROM mvtest_v",
    );
    assert_eq!(e.code, "42601");
    assert!(e.message.contains("too many column names"));
    ok(
        &mut eng,
        "CREATE MATERIALIZED VIEW mvtest_mv_v (ii, jj) AS SELECT i, j FROM mvtest_v",
    );
    ok(
        &mut eng,
        "CREATE MATERIALIZED VIEW mvtest_mv_v_2 AS SELECT i AS ii, j FROM mvtest_v",
    );
    ok(
        &mut eng,
        "CREATE MATERIALIZED VIEW mvtest_mv_v_4 AS SELECT * FROM mvtest_mv_v_2",
    );
    // The rename that v0.77-v1.96 refused with 2BP01 now rewrites.
    ok(&mut eng, "ALTER TABLE mvtest_v RENAME COLUMN i TO x");
    assert_eq!(cols(&mut eng, "SELECT * FROM mvtest_v"), vec!["x", "j"]);
    assert_eq!(
        rows(&mut eng, "SELECT * FROM mvtest_mv_v"),
        vec![vec![Some("1".into()), Some("2".into())]]
    );
    assert_eq!(
        cols(&mut eng, "SELECT * FROM mvtest_mv_v_2"),
        vec!["ii", "j"]
    );
    assert_eq!(
        cols(&mut eng, "SELECT * FROM mvtest_mv_v_4"),
        vec!["ii", "j"]
    );
    ok(&mut eng, "UPDATE mvtest_v SET j = 3 WHERE x = 1");
    assert_eq!(
        rows(&mut eng, "SELECT * FROM mvtest_mv_v_4"),
        vec![vec![Some("1".into()), Some("3".into())]]
    );
}

#[test]
fn v197_view_chain_rename_transitive() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE bt (a int, b int)");
    ok(&mut eng, "INSERT INTO bt VALUES (1, 2)");
    ok(&mut eng, "CREATE VIEW bv1 AS SELECT a, b FROM bt");
    ok(&mut eng, "CREATE VIEW bv2 AS SELECT a FROM bv1");
    ok(&mut eng, "ALTER TABLE bt RENAME COLUMN a TO aa");
    assert_eq!(cols(&mut eng, "SELECT * FROM bv1"), vec!["aa", "b"]);
    assert_eq!(cols(&mut eng, "SELECT * FROM bv2"), vec!["aa"]);
    assert_eq!(
        rows(&mut eng, "SELECT * FROM bv2"),
        vec![vec![Some("1".into())]]
    );
    // Alias-pinned output does NOT change when the base renames.
    ok(&mut eng, "CREATE VIEW bv3 AS SELECT aa AS keep FROM bt");
    ok(&mut eng, "ALTER TABLE bt RENAME COLUMN aa TO zz");
    assert_eq!(cols(&mut eng, "SELECT * FROM bv3"), vec!["keep"]);
    assert_eq!(cols(&mut eng, "SELECT * FROM bv1"), vec!["zz", "b"]);
}

#[test]
fn v197_alter_view_rename_column() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE at (a int, b int)");
    ok(&mut eng, "CREATE VIEW av1 AS SELECT a, b FROM at");
    ok(&mut eng, "CREATE VIEW av2 AS SELECT a FROM av1");
    ok(&mut eng, "ALTER VIEW av1 RENAME COLUMN a TO x");
    assert_eq!(cols(&mut eng, "SELECT * FROM av1"), vec!["x", "b"]);
    assert_eq!(cols(&mut eng, "SELECT * FROM av2"), vec!["x"]);
    // ALTER TABLE on a view name works too (PG dispatches by kind).
    ok(&mut eng, "ALTER TABLE av1 RENAME COLUMN b TO y");
    assert_eq!(cols(&mut eng, "SELECT * FROM av1"), vec!["x", "y"]);
    let e = err(&mut eng, "ALTER VIEW av1 RENAME COLUMN nosuch TO z");
    assert_eq!(e.code, "42703");
    let e = err(&mut eng, "ALTER VIEW av1 RENAME COLUMN x TO y");
    assert_eq!(e.code, "42701");
}

#[test]
fn v197_rename_guards_and_forms() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE p1 (a int, b int)");
    ok(&mut eng, "CREATE TABLE c1 (c int) INHERITS (p1)");
    // ONLY with descendants: PG19 renameatt 42P16.
    let e = err(&mut eng, "ALTER TABLE ONLY p1 RENAME COLUMN a TO d");
    assert_eq!(e.code, "42P16");
    // Plain rename propagates to the child.
    ok(&mut eng, "ALTER TABLE p1 RENAME COLUMN a TO d");
    assert_eq!(cols(&mut eng, "SELECT * FROM c1"), vec!["d", "b", "c"]);
    // Renaming an inherited column in the child: 42P16.
    let e = err(&mut eng, "ALTER TABLE c1 RENAME COLUMN d TO z");
    assert_eq!(e.code, "42P16");
    // IF EXISTS on a missing table: quiet success.
    ok(
        &mut eng,
        "ALTER TABLE IF EXISTS nosuch_tab RENAME COLUMN a TO d",
    );
    // COLUMN keyword optional (create_view tt3 case).
    ok(&mut eng, "CREATE TABLE tt3 (c int)");
    ok(&mut eng, "ALTER TABLE tt3 RENAME c TO d");
    assert_eq!(cols(&mut eng, "SELECT * FROM tt3"), vec!["d"]);
    // Foreign keys follow the referenced rename.
    ok(&mut eng, "CREATE TABLE fk_p (id int PRIMARY KEY)");
    ok(&mut eng, "CREATE TABLE fk_c (pid int REFERENCES fk_p(id))");
    ok(&mut eng, "ALTER TABLE fk_p RENAME COLUMN id TO pid");
    ok(&mut eng, "INSERT INTO fk_p VALUES (7)");
    ok(&mut eng, "INSERT INTO fk_c VALUES (7)");
    let e = err(&mut eng, "INSERT INTO fk_c VALUES (9)");
    assert_eq!(e.code, "23503");
}

#[test]
fn v197_alter_type_grammar() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TYPE test_type AS (a int, b text, c bool)");
    ok(
        &mut eng,
        "ALTER TYPE test_type ALTER ATTRIBUTE b SET DATA TYPE varchar",
    );
    ok(&mut eng, "ALTER TYPE test_type DROP ATTRIBUTE c");
    let e = err(&mut eng, "ALTER TYPE test_type DROP ATTRIBUTE c");
    assert_eq!(e.code, "42703");
    ok(&mut eng, "ALTER TYPE test_type DROP ATTRIBUTE IF EXISTS c");
    ok(
        &mut eng,
        "ALTER TYPE test_type DROP ATTRIBUTE a, ADD ATTRIBUTE d boolean",
    );
    ok(&mut eng, "ALTER TYPE test_type RENAME ATTRIBUTE d TO dd");
    assert_eq!(
        rows(&mut eng, "SELECT '(1,true)'::test_type"),
        vec![vec![Some("(1,t)".into())]]
    );
    let e = err(&mut eng, "ALTER TYPE test_type ADD ATTRIBUTE dd int");
    assert_eq!(e.code, "42701");
    let e = err(&mut eng, "ALTER TYPE nosuch_type ADD ATTRIBUTE x int");
    assert_eq!(e.code, "42704");
}

#[test]
fn v197_alter_type_column_use_rules() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TYPE test_type1 AS (a int, b text)");
    ok(&mut eng, "CREATE TABLE test_tbl1 (x int, y test_type1)");
    // ALTER ATTRIBUTE over a plain column use: 0A000 (PG19
    // find_composite_type_dependencies), not the old 2BP01.
    let e = err(
        &mut eng,
        "ALTER TYPE test_type1 ALTER ATTRIBUTE b TYPE varchar",
    );
    assert_eq!(e.code, "0A000");
    assert!(e.message.contains("column \"test_tbl1.y\" uses it"));
    // ADD ATTRIBUTE is allowed; stored records gain a NULL field.
    ok(&mut eng, "INSERT INTO test_tbl1 VALUES (1, (2,'x'))");
    ok(&mut eng, "ALTER TYPE test_type1 ADD ATTRIBUTE c int");
    assert_eq!(
        rows(&mut eng, "SELECT y FROM test_tbl1"),
        vec![vec![Some("(2,x,)".into())]]
    );
    // Index-expression use blocks with the "<index>.row" culprit
    // (alter_table.sql's second phase: plain int/text columns, the
    // type appears only inside the index expression).
    let mut eng2 = engine();
    ok(&mut eng2, "CREATE TYPE test_type1 AS (a int, b text)");
    ok(&mut eng2, "CREATE TABLE test_tbl1 (x int, y text)");
    ok(
        &mut eng2,
        "CREATE INDEX test_tbl1_idx ON test_tbl1 ((row(x,y)::test_type1))",
    );
    let e = err(
        &mut eng2,
        "ALTER TYPE test_type1 ALTER ATTRIBUTE b TYPE varchar",
    );
    assert_eq!(e.code, "0A000");
    assert!(e.message.contains("test_tbl1_idx.row"));
}

#[test]
fn v197_alter_type_typed_table_cascade() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TYPE test_type2 AS (a int, b text)");
    ok(&mut eng, "CREATE TABLE test_tbl2 OF test_type2");
    ok(&mut eng, "CREATE TABLE test_tbl2_subclass OF test_type2");
    // typed_tables is a runtime map keyed by CREATE TABLE OF;
    // subclass via INHERITS in PG — here OF again is enough to
    // exercise propagation through rename_descendants when the
    // subclass inherits. Use INHERITS for the descendant shape:
    ok(&mut eng, "DROP TABLE test_tbl2_subclass");
    ok(
        &mut eng,
        "CREATE TABLE test_tbl2_subclass () INHERITS (test_tbl2)",
    );
    ok(&mut eng, "INSERT INTO test_tbl2 VALUES (1, 'x')");
    // RESTRICT (default) refuses: 2BP01 typed-table message.
    let e = err(&mut eng, "ALTER TYPE test_type2 ADD ATTRIBUTE c text");
    assert_eq!(e.code, "2BP01");
    assert!(e.message.contains("it is the type of a typed table"));
    // CASCADE propagates to the typed table and its inheritor.
    ok(
        &mut eng,
        "ALTER TYPE test_type2 ADD ATTRIBUTE c text CASCADE",
    );
    assert_eq!(
        cols(&mut eng, "SELECT * FROM test_tbl2"),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        cols(&mut eng, "SELECT * FROM test_tbl2_subclass"),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM test_tbl2"),
        vec![vec![Some("1".into()), Some("x".into()), None]]
    );
    let e = err(
        &mut eng,
        "ALTER TYPE test_type2 ALTER ATTRIBUTE b TYPE varchar",
    );
    assert_eq!(e.code, "2BP01");
    ok(
        &mut eng,
        "ALTER TYPE test_type2 ALTER ATTRIBUTE b TYPE varchar CASCADE",
    );
    ok(&mut eng, "ALTER TYPE test_type2 DROP ATTRIBUTE b CASCADE");
    assert_eq!(cols(&mut eng, "SELECT * FROM test_tbl2"), vec!["a", "c"]);
    let e = err(&mut eng, "ALTER TYPE test_type2 RENAME ATTRIBUTE a TO aa");
    assert_eq!(e.code, "2BP01");
    ok(
        &mut eng,
        "ALTER TYPE test_type2 RENAME ATTRIBUTE a TO aa CASCADE",
    );
    assert_eq!(cols(&mut eng, "SELECT * FROM test_tbl2"), vec!["aa", "c"]);
    assert_eq!(
        cols(&mut eng, "SELECT * FROM test_tbl2_subclass"),
        vec!["aa", "c"]
    );
}

#[test]
fn v197_alter_type_drop_field_refs() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TYPE test_typex AS (a int, b text)");
    ok(
        &mut eng,
        "CREATE TABLE test_tblx (x int, y test_typex, CHECK ((y).a > 0))",
    );
    ok(&mut eng, "INSERT INTO test_tblx VALUES (1, (2,'t'))");
    // The CHECK references field a: RESTRICT refuses 2BP01 with
    // PG's exact message.
    let e = err(&mut eng, "ALTER TYPE test_typex DROP ATTRIBUTE a");
    assert_eq!(e.code, "2BP01");
    assert_eq!(
        e.message,
        "cannot drop column a of composite type test_typex because other objects depend on it"
    );
    // CASCADE drops the constraint, the attribute, and the stored
    // field.
    ok(&mut eng, "ALTER TYPE test_typex DROP ATTRIBUTE a CASCADE");
    assert_eq!(
        rows(&mut eng, "SELECT y FROM test_tblx"),
        vec![vec![Some("(t)".into())]]
    );
    // The CHECK is gone: a formerly-violating row now inserts.
    ok(&mut eng, "INSERT INTO test_tblx VALUES (2, (-1,'u'))");
}

#[test]
fn v197_alter_type_domain_check_revalidation() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TYPE comptype AS (r float8, i float8)");
    ok(&mut eng, "CREATE DOMAIN dcomptype AS comptype");
    ok(
        &mut eng,
        "ALTER DOMAIN dcomptype ADD CONSTRAINT c1 CHECK ((value).r > 0)",
    );
    // varchar breaks the check's operator resolution: 42883.
    let e = err(
        &mut eng,
        "ALTER TYPE comptype ALTER ATTRIBUTE r TYPE varchar",
    );
    assert_eq!(e.code, "42883");
    assert_eq!(
        e.message,
        "operator does not exist: character varying > double precision"
    );
    // bigint keeps it resolvable.
    ok(
        &mut eng,
        "ALTER TYPE comptype ALTER ATTRIBUTE r TYPE bigint",
    );
    // DROP of a field the domain check uses: 2BP01; unused field OK.
    let e = err(&mut eng, "ALTER TYPE comptype DROP ATTRIBUTE r");
    assert_eq!(e.code, "2BP01");
    ok(&mut eng, "ALTER TYPE comptype DROP ATTRIBUTE i");
}

#[test]
fn v197_alter_domain_add_constraint_validation() {
    let mut eng = engine();
    ok(&mut eng, "CREATE DOMAIN dpos AS int");
    ok(&mut eng, "CREATE TABLE dt (d1 dpos)");
    ok(&mut eng, "INSERT INTO dt VALUES (1), (2)");
    // Existing row 1 violates value > 1: 23514, PG message.
    let e = err(
        &mut eng,
        "ALTER DOMAIN dpos ADD CONSTRAINT c2 CHECK (value > 1)",
    );
    assert_eq!(e.code, "23514");
    assert_eq!(
        e.message,
        "column \"d1\" of table \"dt\" contains values that violate the new constraint"
    );
    ok(
        &mut eng,
        "ALTER DOMAIN dpos ADD CONSTRAINT c2 CHECK (value > 0)",
    );
    // NOT VALID skips validation; VALIDATE CONSTRAINT runs it.
    ok(
        &mut eng,
        "ALTER DOMAIN dpos ADD CONSTRAINT c3 CHECK (value < 2) NOT VALID",
    );
    let e = err(&mut eng, "ALTER DOMAIN dpos VALIDATE CONSTRAINT c3");
    assert_eq!(e.code, "23514");
    ok(&mut eng, "DELETE FROM dt WHERE d1 = 2");
    ok(&mut eng, "ALTER DOMAIN dpos VALIDATE CONSTRAINT c3");
    let e = err(&mut eng, "ALTER DOMAIN dpos VALIDATE CONSTRAINT nosuch");
    assert_eq!(e.code, "42704");
    // Container rule: domain as a composite attribute reaches a
    // stored column only through the composite: 0A000.
    ok(&mut eng, "CREATE DOMAIN posint AS int CHECK (value > 0)");
    ok(&mut eng, "CREATE TYPE ddtest1 AS (f1 posint)");
    ok(&mut eng, "CREATE TABLE ddtest2 (f1 ddtest1)");
    let e = err(
        &mut eng,
        "ALTER DOMAIN posint ADD CONSTRAINT c9 CHECK (value < 100)",
    );
    assert_eq!(e.code, "0A000");
    assert!(e.message.contains("column \"ddtest2.f1\" uses it"));
    // Derived (sub-domain) columns are validated directly
    // (isolated chain: dposb -> dposbsub, value 50 in the table).
    ok(&mut eng, "CREATE DOMAIN dposb AS int");
    ok(&mut eng, "CREATE DOMAIN dposbsub AS dposb");
    ok(&mut eng, "CREATE TABLE dt2 (d1 dposbsub)");
    ok(&mut eng, "INSERT INTO dt2 VALUES (50)");
    let e = err(
        &mut eng,
        "ALTER DOMAIN dposb ADD CONSTRAINT c4 CHECK (value < 10)",
    );
    assert_eq!(e.code, "23514");
    assert!(e.message.contains("\"dt2\""));
}

#[test]
fn v197_ctas_named_composite() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TYPE test_type3 AS (a int)");
    // The alter_table[408] shape: CTAS over a composite cast.
    ok(
        &mut eng,
        "CREATE TABLE test_tbl3 (c) AS SELECT '(1)'::test_type3",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM test_tbl3"),
        vec![vec![Some("(1)".into())]]
    );
    // Without the explicit column list, the column takes the
    // cast's output name and still records the composite type.
    ok(&mut eng, "CREATE TABLE t3 AS SELECT '(2)'::test_type3");
    ok(
        &mut eng,
        "ALTER TYPE test_type3 DROP ATTRIBUTE a, ADD ATTRIBUTE b int",
    );
    assert_eq!(
        rows(&mut eng, "SELECT * FROM test_tbl3"),
        vec![vec![Some("()".into())]]
    );
    // Plain column reference to a composite column carries the type.
    ok(&mut eng, "CREATE TABLE src (y test_type3)");
    ok(&mut eng, "INSERT INTO src VALUES (ROW(5)::test_type3)");
    ok(&mut eng, "CREATE TABLE t4 AS SELECT y FROM src");
    assert_eq!(
        rows(&mut eng, "SELECT * FROM t4"),
        vec![vec![Some("(5)".into())]]
    );
}

#[test]
fn v197_rename_multi_parent_guard() {
    // inherit.sql: inhts inherits b from BOTH inht1 and inhs1 —
    // renaming b through one parent fails in PG19.
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE inht1 (a int, b int)");
    ok(&mut eng, "CREATE TABLE inhs1 (b int, c int)");
    ok(
        &mut eng,
        "CREATE TABLE inhts (d int) INHERITS (inht1, inhs1)",
    );
    ok(&mut eng, "ALTER TABLE inht1 RENAME a TO aa");
    let e = err(&mut eng, "ALTER TABLE inht1 RENAME b TO bb");
    assert_eq!(e.code, "42P16");
    assert_eq!(e.message, "cannot rename inherited column \"b\"");
    assert_eq!(
        cols(&mut eng, "SELECT * FROM inhts"),
        vec!["aa", "b", "c", "d"]
    );
}

#[test]
fn v197_drop_inherited_and_only_guards() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE dropcolumn (a int, b int, e int)");
    ok(
        &mut eng,
        "CREATE TABLE dropcolumnchild (c int) INHERITS (dropcolumn)",
    );
    // PG19 ATExecDropColumn: inherited columns refuse in the child.
    let e = err(&mut eng, "ALTER TABLE dropcolumnchild DROP COLUMN a");
    assert_eq!(e.code, "42P16");
    assert_eq!(e.message, "cannot drop inherited column \"a\"");
    let e = err(&mut eng, "ALTER TABLE ONLY dropcolumnchild DROP COLUMN b");
    assert_eq!(e.code, "42P16");
    // ONLY drop of a local parent column works (no propagation).
    ok(&mut eng, "ALTER TABLE ONLY dropcolumn DROP COLUMN e");
    assert_eq!(
        cols(&mut eng, "SELECT * FROM dropcolumnchild"),
        vec!["a", "b", "e", "c"]
    );
    // ONLY ADD COLUMN with children: 42P16 PG message.
    ok(&mut eng, "CREATE TABLE renamecolumn (a int)");
    ok(
        &mut eng,
        "CREATE TABLE renamecolumnchild (b int) INHERITS (renamecolumn)",
    );
    let e = err(&mut eng, "ALTER TABLE ONLY renamecolumn ADD COLUMN x int");
    assert_eq!(e.code, "42P16");
    assert_eq!(e.message, "column must be added to child tables too");
    ok(&mut eng, "ALTER TABLE renamecolumn ADD COLUMN x int");
}

#[test]
fn v197_only_fk_and_pk_guards() {
    let mut eng = engine();
    // PG19: ONLY for a foreign key on a partitioned table — 42809.
    ok(
        &mut eng,
        "CREATE TABLE fk_notpartitioned_pk (a int, PRIMARY KEY(a), CHECK (a > 0))",
    );
    ok(
        &mut eng,
        "CREATE TABLE fk_partitioned_fk (a int, b int) PARTITION BY RANGE(a)",
    );
    let e = err(
        &mut eng,
        "ALTER TABLE ONLY fk_partitioned_fk ADD FOREIGN KEY (a, b) REFERENCES fk_notpartitioned_pk",
    );
    assert_eq!(e.code, "42809");
    assert!(
        e.message
            .contains("cannot use ONLY for foreign key on partitioned table")
    );
    // PG19 verifyNotNullPKCompatible: a partition's NOT VALID
    // not-null constraint blocks ADD PRIMARY KEY — 55000.
    ok(&mut eng, "CREATE TABLE pp_nn (a int) PARTITION BY HASH (a)");
    ok(
        &mut eng,
        "CREATE TABLE pp_nn_1 PARTITION OF pp_nn FOR VALUES WITH (MODULUS 2, REMAINDER 0)",
    );
    ok(
        &mut eng,
        "ALTER TABLE pp_nn_1 ADD CONSTRAINT nn NOT NULL a NOT VALID",
    );
    let e = err(&mut eng, "ALTER TABLE ONLY pp_nn ADD PRIMARY KEY (a)");
    assert_eq!(e.code, "55000");
    assert_eq!(e.message, "cannot create primary key on column \"a\"");
}

#[test]
fn v197_drop_type_cascade_drops_domains() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TYPE comptype AS (r float8, i float8)");
    ok(&mut eng, "CREATE DOMAIN dcomptype AS comptype");
    ok(&mut eng, "CREATE DOMAIN dcompsub AS dcomptype");
    ok(&mut eng, "DROP TYPE comptype CASCADE");
    // Both the domain and the sub-domain are gone (PG19 cascade).
    let e = err(&mut eng, "CREATE DOMAIN dcomptype AS comptype");
    assert_eq!(e.code, "42704");
    ok(&mut eng, "CREATE TYPE comptype AS (r float8, i float8)");
    ok(&mut eng, "CREATE DOMAIN dcomptype AS comptype");
    ok(
        &mut eng,
        "ALTER DOMAIN dcomptype ADD CONSTRAINT c1 CHECK ((value).r > 0)",
    );
    let e = err(&mut eng, "SELECT row(0,1)::dcomptype");
    assert_eq!(e.code, "23514");
}

#[test]
fn v197_partition_cycle_and_type_guards() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE lp (a int, b char) PARTITION BY LIST (a)",
    );
    ok(&mut eng, "CREATE TABLE p5 (LIKE lp) PARTITION BY LIST (b)");
    ok(
        &mut eng,
        "ALTER TABLE lp ATTACH PARTITION p5 FOR VALUES IN (5)",
    );
    // PG19: circular partition attachment is rejected (42P16).
    let e = err(
        &mut eng,
        "ALTER TABLE p5 ATTACH PARTITION lp FOR VALUES IN ('b')",
    );
    assert_eq!(e.code, "42P16");
    assert_eq!(e.message, "circular inheritance not allowed");
    // PG19: a partition's inherited column type cannot be altered
    // directly, and the parent's column is a partition key of p5.
    let e = err(&mut eng, "ALTER TABLE p5 ALTER COLUMN b TYPE text");
    assert_eq!(e.code, "42P16");
    let e = err(&mut eng, "ALTER TABLE lp ALTER COLUMN b TYPE text");
    assert_eq!(e.code, "42P16");
    assert!(e.message.contains("partition key of relation \"p5\""));
    // ONLY ADD CONSTRAINT (check) on a partitioned parent: 42P16.
    let e = err(
        &mut eng,
        "ALTER TABLE ONLY lp ADD CONSTRAINT cb CHECK (b <> 'zz')",
    );
    assert_eq!(e.code, "42P16");
    assert_eq!(e.message, "constraint must be added to child tables too");
}

#[test]
fn v197_attach_not_valid_conflict_and_validate() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pp (a int, b int, NOT NULL a) PARTITION BY LIST (a)",
    );
    ok(&mut eng, "CREATE TABLE pp1 (a int, b int)");
    ok(
        &mut eng,
        "ALTER TABLE pp1 ADD CONSTRAINT nn1 NOT NULL a NOT VALID",
    );
    // PG19: parent's validated NOT NULL conflicts with the child's
    // NOT VALID constraint at ATTACH (42P17).
    let e = err(
        &mut eng,
        "ALTER TABLE pp ATTACH PARTITION pp1 FOR VALUES IN (NULL, 5)",
    );
    assert_eq!(e.code, "42P17");
    ok(&mut eng, "ALTER TABLE pp1 VALIDATE CONSTRAINT nn1");
    ok(
        &mut eng,
        "ALTER TABLE pp ATTACH PARTITION pp1 FOR VALUES IN (NULL, 5)",
    );
    let e = err(&mut eng, "ALTER TABLE pp1 VALIDATE CONSTRAINT nope");
    assert_eq!(e.code, "42704");
}

#[test]
fn v197_no_inherit_not_null_family() {
    let mut eng = engine();
    // PG19: NO INHERIT nn on a partitioned table is rejected.
    let e = err(
        &mut eng,
        "CREATE TABLE z1 (a int, NOT NULL a NO INHERIT) PARTITION BY LIST (a)",
    );
    assert!(e.message.contains("cannot be NO INHERIT"));
    ok(
        &mut eng,
        "CREATE TABLE z1 (a int, CONSTRAINT a_nn NOT NULL a) PARTITION BY LIST (a)",
    );
    ok(&mut eng, "DROP TABLE z1");
    // Conflicting declarations in one CREATE.
    let e = err(
        &mut eng,
        "CREATE TABLE z2 (a int primary key, b int, not null a no inherit)",
    );
    assert!(e.message.contains("conflicting NO INHERIT declaration"));
    // A NO INHERIT constraint blocks a later inheritable add and
    // ADD PRIMARY KEY (as NOT VALID ones do).
    ok(&mut eng, "CREATE TABLE z3 (a int, NOT NULL a NO INHERIT)");
    let e = err(&mut eng, "ALTER TABLE z3 ADD CONSTRAINT nna NOT NULL a");
    assert_eq!(e.code, "42P16");
    assert!(e.message.contains("cannot change NO INHERIT status"));
    ok(&mut eng, "CREATE TABLE z4 (a int) PARTITION BY HASH (a)");
    ok(
        &mut eng,
        "CREATE TABLE z4_1 PARTITION OF z4 FOR VALUES WITH (MODULUS 2, REMAINDER 0)",
    );
    ok(
        &mut eng,
        "ALTER TABLE z4_1 ADD CONSTRAINT nn NOT NULL a NO INHERIT",
    );
    let e = err(&mut eng, "ALTER TABLE ONLY z4 ADD PRIMARY KEY (a)");
    assert_eq!(e.code, "55000");
}
