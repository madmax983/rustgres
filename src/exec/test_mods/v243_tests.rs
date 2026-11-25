/// v2.43: INHERITANCE REMAINDER tests — DML expansion over plain
/// inheritance children, assignment coercion into real/float4,
/// multi-column UPDATE SET, ALTER CONSTRAINT INHERIT / ENFORCED
/// semantics, constraint merge rules, inherited-column guards, and
/// ALTER COLUMN TYPE propagation into inheritance descendants.
use super::*;

fn engine() -> Engine {
    Engine::new()
}

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    let stmt = crate::sql::parse_statement(sql).map_err(|e| exec_err(e.code, e.message))?;
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

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn err_code(eng: &mut Engine, sql: &str) -> String {
    run(eng, sql).unwrap_err().code.to_string()
}

/// UPDATE and DELETE on a parent reach plain-inheritance children;
/// ONLY restricts them to the parent's own rows. Child-only columns
/// survive a parent-column UPDATE (overlay write-back).
#[test]
fn dml_expands_over_inheritance_children() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE p (a int, b text)").unwrap();
    run(&mut eng, "CREATE TABLE c1 (x text) INHERITS (p)").unwrap();
    run(&mut eng, "CREATE TABLE c2 () INHERITS (p)").unwrap();
    run(&mut eng, "INSERT INTO p VALUES (1, 'p')").unwrap();
    run(&mut eng, "INSERT INTO c1 VALUES (2, 'c', 'keep')").unwrap();
    run(&mut eng, "INSERT INTO c2 VALUES (3, 'd')").unwrap();
    run(&mut eng, "UPDATE p SET b = 'z' WHERE a >= 2").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT a, b FROM p ORDER BY a").unwrap());
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "p".to_string()],
            vec!["2".to_string(), "z".to_string()],
            vec!["3".to_string(), "z".to_string()],
        ]
    );
    // The child-only column survived the parent-driven update.
    let rows = rows_of(run(&mut eng, "SELECT x FROM c1").unwrap());
    assert_eq!(rows, vec![vec!["keep".to_string()]]);
    // ONLY limits the delete to the parent's own rows.
    run(&mut eng, "DELETE FROM ONLY p").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT count(*) FROM p").unwrap());
    assert_eq!(rows, vec![vec!["2".to_string()]]);
    run(&mut eng, "DELETE FROM p").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT count(*) FROM p").unwrap());
    assert_eq!(rows, vec![vec!["0".to_string()]]);
}

/// tableoid is usable in WHERE (row provenance reaches filters).
#[test]
fn tableoid_in_where() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE p (a int)").unwrap();
    run(&mut eng, "CREATE TABLE c () INHERITS (p)").unwrap();
    run(&mut eng, "INSERT INTO p VALUES (1)").unwrap();
    run(&mut eng, "INSERT INTO c VALUES (2)").unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT a FROM p WHERE tableoid = 'c'::regclass ORDER BY a",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["2".to_string()]]);
}

/// Assignment coercion: integer literals widen into real/float4
/// columns on INSERT and UPDATE (PG assignment cast).
#[test]
fn assignment_coercion_int_to_real() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (r real, f float4, d float8)").unwrap();
    run(&mut eng, "INSERT INTO t VALUES (1, 2, 3)").unwrap();
    run(&mut eng, "UPDATE t SET r = 4, f = 5").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT r, f, d FROM t").unwrap());
    assert_eq!(
        rows,
        vec![vec!["4".to_string(), "5".to_string(), "3".to_string()]]
    );
}

/// Multi-column UPDATE SET, both the row form and the sub-SELECT
/// form; arity mismatch is a syntax error.
#[test]
fn multi_column_update_set() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (a int, b int, c int)").unwrap();
    run(&mut eng, "INSERT INTO t VALUES (1, 2, 3)").unwrap();
    run(&mut eng, "UPDATE t SET (a, b) = (10, 20)").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT a, b, c FROM t").unwrap());
    assert_eq!(
        rows,
        vec![vec!["10".to_string(), "20".to_string(), "3".to_string()]]
    );
    run(&mut eng, "CREATE TABLE s (x int, y int)").unwrap();
    run(&mut eng, "INSERT INTO s VALUES (7, 8)").unwrap();
    run(&mut eng, "UPDATE t SET (a, b) = (SELECT x, y FROM s)").unwrap();
    let rows = rows_of(run(&mut eng, "SELECT a, b FROM t").unwrap());
    assert_eq!(rows, vec![vec!["7".to_string(), "8".to_string()]]);
    assert_eq!(
        err_code(&mut eng, "UPDATE t SET (a, b) = (1, 2, 3)"),
        "42601"
    );
    // A non-row, non-subquery source is 0A000 in PG19
    // (transformMultiAssignRef).
    assert_eq!(err_code(&mut eng, "UPDATE t SET (a, b) = (1)"), "0A000");
}

/// INSERT ... ON CONFLICT with a target alias and a multi-column
/// DO UPDATE SET.
#[test]
fn insert_alias_on_conflict_multi_set() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t (a int PRIMARY KEY, b int, c int)").unwrap();
    run(&mut eng, "INSERT INTO t VALUES (1, 2, 3)").unwrap();
    run(
        &mut eng,
        "INSERT INTO t AS x VALUES (1, 20, 30) ON CONFLICT (a) \
         DO UPDATE SET (b, c) = (excluded.b, excluded.c)",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT a, b, c FROM t").unwrap());
    assert_eq!(
        rows,
        vec![vec!["1".to_string(), "20".to_string(), "30".to_string()]]
    );
}

/// A NO INHERIT not-null declared in a parent's CREATE does not
/// leak into children created with INHERITS.
#[test]
fn create_inherits_no_inherit_not_null_stays() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE p (a int, CONSTRAINT nn_a NOT NULL a NO INHERIT)",
    )
    .unwrap();
    run(&mut eng, "CREATE TABLE c () INHERITS (p)").unwrap();
    run(&mut eng, "INSERT INTO c VALUES (NULL)").unwrap();
    assert_eq!(err_code(&mut eng, "INSERT INTO p VALUES (NULL)"), "23502");
}

/// ALTER CONSTRAINT ... NO INHERIT / INHERIT on a not-null
/// constraint: the flip localizes / re-inherits and the catalog
/// locality follows; flipping a constraint that a child derived
/// its own NO INHERIT copy from is 55000.
#[test]
fn alter_constraint_inherit_flip_not_null() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE p (a int NOT NULL)").unwrap();
    run(&mut eng, "CREATE TABLE c () INHERITS (p)").unwrap();
    run(
        &mut eng,
        "ALTER TABLE p ALTER CONSTRAINT p_a_not_null NO INHERIT",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT conislocal, coninhcount, connoinherit FROM pg_constraint \
             WHERE conrelid = 'c'::regclass AND contype = 'n'",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![vec!["t".to_string(), "0".to_string(), "f".to_string()]]
    );
    // And back: INHERIT reattaches (child copy no longer local).
    run(
        &mut eng,
        "ALTER TABLE p ALTER CONSTRAINT p_a_not_null INHERIT",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT conislocal, coninhcount FROM pg_constraint \
             WHERE conrelid = 'c'::regclass AND contype = 'n'",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["f".to_string(), "1".to_string()]]);
}

/// NOT ENFORCED constraints read convalidated = false; an
/// ENFORCED flip validates the subtree and flips both flags.
#[test]
fn alter_constraint_enforced_flip() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE p (a int, CONSTRAINT ck CHECK (a > 0) NOT ENFORCED)",
    )
    .unwrap();
    run(&mut eng, "CREATE TABLE c () INHERITS (p)").unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT conenforced, convalidated FROM pg_constraint \
             WHERE conname = 'ck' ORDER BY conrelid::regclass::text",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["f".to_string(), "f".to_string()],
            vec!["f".to_string(), "f".to_string()],
        ]
    );
    run(&mut eng, "INSERT INTO c VALUES (-1)").unwrap();
    run(&mut eng, "ALTER TABLE p ALTER CONSTRAINT ck ENFORCED").unwrap_err();
    run(&mut eng, "DELETE FROM c").unwrap();
    run(&mut eng, "ALTER TABLE p ALTER CONSTRAINT ck ENFORCED").unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT conenforced, convalidated FROM pg_constraint \
             WHERE conname = 'ck' ORDER BY conrelid::regclass::text",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["t".to_string(), "t".to_string()],
            vec!["t".to_string(), "t".to_string()],
        ]
    );
}

/// ALTER COLUMN TYPE propagates into plain-inheritance children
/// and rewrites their rows with the USING expression.
#[test]
fn alter_type_propagates_to_inheritance_children() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a (aa text)").unwrap();
    run(&mut eng, "CREATE TABLE b (bb text) INHERITS (a)").unwrap();
    run(&mut eng, "CREATE TABLE d (dd text) INHERITS (b, a)").unwrap();
    run(&mut eng, "INSERT INTO d VALUES ('test', 'one', 'two')").unwrap();
    run(
        &mut eng,
        "ALTER TABLE a ALTER COLUMN aa TYPE int USING bit_length(aa)",
    )
    .unwrap();
    let rows = rows_of(run(&mut eng, "SELECT aa, bb, dd FROM d").unwrap());
    assert_eq!(
        rows,
        vec![vec!["32".to_string(), "one".to_string(), "two".to_string()]]
    );
}

/// The inherited-column guard is attinhcount vs numparents: a
/// diamond under one root alters fine; an unrelated second parent
/// carrying the column blocks with 42P16.
#[test]
fn inherited_column_guard_diamond_and_block() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE p1 (f1 int)").unwrap();
    run(&mut eng, "CREATE TABLE p2 (f1 int)").unwrap();
    run(&mut eng, "CREATE TABLE c () INHERITS (p1, p2)").unwrap();
    assert_eq!(
        err_code(&mut eng, "ALTER TABLE p1 ALTER COLUMN f1 TYPE text"),
        "42P16"
    );
    // Diamond under a single root is fine.
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE a (aa text)").unwrap();
    run(&mut eng, "CREATE TABLE b () INHERITS (a)").unwrap();
    run(&mut eng, "CREATE TABLE cc () INHERITS (a)").unwrap();
    run(&mut eng, "CREATE TABLE d () INHERITS (b, cc, a)").unwrap();
    run(&mut eng, "ALTER TABLE a ALTER COLUMN aa TYPE varchar(9)").unwrap();
}

/// An unnamed ADD CONSTRAINT lands on children under the name it
/// got on the parent, non-locally.
#[test]
fn unnamed_add_constraint_keeps_parent_name_on_children() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE ac (aa text)").unwrap();
    run(&mut eng, "CREATE TABLE bc (bb text) INHERITS (ac)").unwrap();
    run(&mut eng, "ALTER TABLE ac ADD CHECK (aa IS NOT NULL)").unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT conrelid::regclass::text, conname, conislocal, coninhcount \
             FROM pg_constraint WHERE contype = 'c' ORDER BY 1",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec![
                "ac".to_string(),
                "ac_aa_check".to_string(),
                "t".to_string(),
                "0".to_string()
            ],
            vec![
                "bc".to_string(),
                "ac_aa_check".to_string(),
                "f".to_string(),
                "1".to_string()
            ],
        ]
    );
}

/// An inherited not-null copy keeps the ancestor's constraint
/// name in pg_constraint.
#[test]
fn inherited_not_null_keeps_ancestor_name() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE p (f1 int NOT NULL)").unwrap();
    run(&mut eng, "CREATE TABLE c1 () INHERITS (p)").unwrap();
    run(&mut eng, "CREATE TABLE c2 () INHERITS (c1)").unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT conrelid::regclass::text, conname FROM pg_constraint \
             WHERE contype = 'n' ORDER BY 1",
        )
        .unwrap(),
    );
    assert_eq!(
        rows,
        vec![
            vec!["c1".to_string(), "p_f1_not_null".to_string()],
            vec!["c2".to_string(), "p_f1_not_null".to_string()],
            vec!["p".to_string(), "p_f1_not_null".to_string()],
        ]
    );
}

/// Merging at CREATE: a child-local ENFORCED definition upgrades
/// an inherited NOT ENFORCED copy; a child-local NOT ENFORCED
/// definition against an inherited ENFORCED copy is 42P17.
#[test]
fn create_merge_enforceability_rules() {
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE p (a int, CONSTRAINT ck CHECK (a > 0) NOT ENFORCED)",
    )
    .unwrap();
    run(
        &mut eng,
        "CREATE TABLE c (CONSTRAINT ck CHECK (a > 0) ENFORCED) INHERITS (p)",
    )
    .unwrap();
    let rows = rows_of(
        run(
            &mut eng,
            "SELECT conenforced FROM pg_constraint \
             WHERE conname = 'ck' AND conrelid = 'c'::regclass",
        )
        .unwrap(),
    );
    assert_eq!(rows, vec![vec!["t".to_string()]]);
    let mut eng = engine();
    run(
        &mut eng,
        "CREATE TABLE p (a int, CONSTRAINT ck CHECK (a > 0))",
    )
    .unwrap();
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE TABLE c (CONSTRAINT ck CHECK (a > 0) NOT ENFORCED) INHERITS (p)"
        ),
        "42P17"
    );
}
