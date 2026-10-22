use super::*;

// v2.02: per-action DML guards convergence — MERGE row-level security
// (SELECT visibility, UPDATE/DELETE USING as errors, WITH CHECK, the
// RETURNING visibility rule) and the publication replica-identity
// guard for MERGE / plain UPDATE / DELETE / ON CONFLICT DO UPDATE.

fn engine() -> Engine {
    let mut eng = Engine::new();
    eng.txns.next_xid = 1_000_000;
    eng
}

static NEXT_XID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(9000);

fn run(eng: &mut Engine, sql: &str) -> Result<ExecResult, ExecError> {
    run_as(eng, sql, "postgres")
}

fn run_as(eng: &mut Engine, sql: &str, role: &str) -> Result<ExecResult, ExecError> {
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

fn err_code(eng: &mut Engine, sql: &str, role: &str) -> String {
    match run_as(eng, sql, role) {
        Ok(_) => panic!("expected error for {sql}"),
        Err(e) => e.code.to_string(),
    }
}

fn doc_setup(eng: &mut Engine) {
    run(
        eng,
        "CREATE TABLE category (cid int primary key, cname text)",
    )
    .unwrap();
    run(
        eng,
        "INSERT INTO category VALUES (11, 'novel'), (33, 'science fiction'), (44, 'manga')",
    )
    .unwrap();
    run(
        eng,
        "CREATE TABLE document (did int primary key, cid int, dlevel int, dauthor text, dtitle text, dnotes text default '')",
    )
    .unwrap();
    run(
        eng,
        "INSERT INTO document VALUES (1, 11, 1, 'bob', 'novel1', ''), (3, 33, 1, 'bob', 'scifi', ''), (4, 44, 1, 'bob', 'manga', '')",
    )
    .unwrap();
    run(eng, "ALTER TABLE document ENABLE ROW LEVEL SECURITY").unwrap();
    run(eng, "CREATE POLICY p1 ON document FOR SELECT USING (true)").unwrap();
    run(
        eng,
        "CREATE POLICY p2 ON document FOR INSERT WITH CHECK (dauthor = current_user)",
    )
    .unwrap();
    run(
        eng,
        "CREATE POLICY p3 ON document FOR UPDATE USING (cid = (SELECT cid from category WHERE cname = 'novel')) WITH CHECK (dlevel > 0)",
    )
    .unwrap();
    run(
        eng,
        "CREATE POLICY p4 ON document FOR DELETE USING (cid = (SELECT cid from category WHERE cname = 'manga'))",
    )
    .unwrap();
    run(eng, "CREATE ROLE bob LOGIN").unwrap();
    run(
        eng,
        "GRANT SELECT, INSERT, UPDATE, DELETE ON document TO bob",
    )
    .unwrap();
    run(eng, "GRANT SELECT ON category TO bob").unwrap();
}

#[test]
fn v202_merge_rls_with_check_and_using() {
    let mut eng = engine();
    doc_setup(&mut eng);
    // WITH CHECK failure on the new row (dlevel = 0).
    let code = err_code(
        &mut eng,
        "MERGE INTO document d USING (SELECT 1 as sdid) s ON did = s.sdid WHEN MATCHED THEN UPDATE SET dlevel = 0",
        "bob",
    );
    assert_eq!(code, "42501");
    // UPDATE USING failure is an error, not a silent skip (did=3 is
    // 'science fiction', outside the UPDATE policy's USING qual).
    let e = run_as(
        &mut eng,
        "MERGE INTO document d USING (SELECT 3 as sdid) s ON did = s.sdid WHEN MATCHED THEN UPDATE SET dnotes = 'x'",
        "bob",
    )
    .unwrap_err();
    assert_eq!(e.code, "42501");
    assert!(
        e.message
            .contains("target row violates row-level security policy"),
        "{}",
        e.message
    );
    // DELETE USING failure likewise.
    let code = err_code(
        &mut eng,
        "MERGE INTO document d USING (SELECT 3 as sdid) s ON did = s.sdid WHEN MATCHED THEN DELETE",
        "bob",
    );
    assert_eq!(code, "42501");
    // A passing update works.
    run_as(
        &mut eng,
        "MERGE INTO document d USING (SELECT 1 as sdid) s ON did = s.sdid WHEN MATCHED THEN UPDATE SET dnotes = 'ok'",
        "bob",
    )
    .unwrap();
    // INSERT WITH CHECK failure (dauthor != current_user).
    let code = err_code(
        &mut eng,
        "MERGE INTO document d USING (SELECT 12 as sdid) s ON did = s.sdid WHEN NOT MATCHED THEN INSERT VALUES (12, 11, 1, 'dave', 'x')",
        "bob",
    );
    assert_eq!(code, "42501");
}

#[test]
fn v202_merge_rls_select_visibility() {
    let mut eng = engine();
    doc_setup(&mut eng);
    // Narrow the SELECT policy to 'novel' only.
    run(&mut eng, "DROP POLICY p1 ON document").unwrap();
    run(
        &mut eng,
        "CREATE POLICY p1 ON document FOR SELECT USING (cid = (SELECT cid from category WHERE cname = 'novel'))",
    )
    .unwrap();
    // did=3 is now invisible: the source row falls to NOT MATCHED and
    // its INSERT collides with the existing (invisible) row.
    let code = err_code(
        &mut eng,
        "MERGE INTO document d USING (SELECT 3 as sdid) s ON did = s.sdid WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (3, 11, 1, 'bob', 'dup')",
        "bob",
    );
    assert_eq!(code, "23505");
    // Updated row must stay visible (no RETURNING needed for MERGE).
    let code = err_code(
        &mut eng,
        "MERGE INTO document d USING (SELECT 1 as sdid) s ON did = s.sdid WHEN MATCHED THEN UPDATE SET cid = 33",
        "bob",
    );
    assert_eq!(code, "42501");
    // Inserting an invisible row is fine without RETURNING...
    run_as(
        &mut eng,
        "MERGE INTO document d USING (SELECT 14 as sdid) s ON did = s.sdid WHEN NOT MATCHED THEN INSERT VALUES (14, 44, 1, 'bob', 'new manga')",
        "bob",
    )
    .unwrap();
    // ...but errors with RETURNING.
    let code = err_code(
        &mut eng,
        "MERGE INTO document d USING (SELECT 15 as sdid) s ON did = s.sdid WHEN NOT MATCHED THEN INSERT VALUES (15, 44, 1, 'bob', 'new manga') RETURNING *",
        "bob",
    );
    assert_eq!(code, "42501");
}

#[test]
fn v202_publication_ri_guard_merge_and_dml() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE nori (a int, b int)").unwrap();
    run(&mut eng, "CREATE TABLE withpk (a int primary key, b int)").unwrap();
    run(&mut eng, "INSERT INTO nori VALUES (1, 1)").unwrap();
    run(&mut eng, "INSERT INTO withpk VALUES (1, 2)").unwrap();
    run(&mut eng, "CREATE PUBLICATION pub1 FOR ALL TABLES").unwrap();
    // MERGE UPDATE / DELETE against a published no-RI table: 55000.
    let e = run(
        &mut eng,
        "MERGE INTO nori USING withpk s ON s.a >= 1 WHEN MATCHED THEN UPDATE SET b = s.b",
    )
    .unwrap_err();
    assert_eq!(e.code, "55000");
    assert!(
        e.message
            .contains("cannot update table \"nori\" because it does not have a replica identity and publishes updates"),
        "{}",
        e.message
    );
    let e = run(
        &mut eng,
        "MERGE INTO nori USING withpk s ON s.a >= 1 WHEN MATCHED THEN DELETE",
    )
    .unwrap_err();
    assert_eq!(e.code, "55000");
    assert!(e.message.contains("publishes deletes"), "{}", e.message);
    // DO NOTHING + INSERT are unrestricted; a PK target is fine.
    run(
        &mut eng,
        "MERGE INTO nori USING withpk s ON s.a >= 1 WHEN MATCHED THEN DO NOTHING WHEN NOT MATCHED THEN INSERT (a, b) VALUES (0, 0)",
    )
    .unwrap();
    run(
        &mut eng,
        "MERGE INTO withpk USING nori s ON s.a >= 1 WHEN MATCHED THEN UPDATE SET b = s.b",
    )
    .unwrap();
    // Plain UPDATE / DELETE.
    assert_eq!(
        err_code(&mut eng, "UPDATE nori SET b = 5", "postgres"),
        "55000"
    );
    assert_eq!(err_code(&mut eng, "DELETE FROM nori", "postgres"), "55000");
    run(&mut eng, "UPDATE withpk SET b = 5").unwrap();
    // ON CONFLICT DO UPDATE counts as an UPDATE.
    run(&mut eng, "CREATE UNIQUE INDEX nori_a ON nori(a)").unwrap();
    assert_eq!(
        err_code(
            &mut eng,
            "INSERT INTO nori VALUES (1, 1) ON CONFLICT (a) DO UPDATE SET b = 2",
            "postgres"
        ),
        "55000"
    );
    // Dropping the publication releases the guard.
    run(&mut eng, "DROP PUBLICATION pub1").unwrap();
    run(&mut eng, "UPDATE nori SET b = 5").unwrap();
    // A publish='insert' publication does not block updates.
    run(
        &mut eng,
        "CREATE PUBLICATION pubins FOR TABLE nori WITH (publish = 'insert')",
    )
    .unwrap();
    run(&mut eng, "UPDATE nori SET b = 6").unwrap();
    run(&mut eng, "DROP PUBLICATION pubins").unwrap();
}

#[test]
fn v202_publication_ddl_errors() {
    let mut eng = engine();
    run(&mut eng, "CREATE TABLE t1 (a int)").unwrap();
    run(&mut eng, "CREATE PUBLICATION p1 FOR TABLE t1").unwrap();
    assert_eq!(
        err_code(&mut eng, "CREATE PUBLICATION p1 FOR TABLE t1", "postgres"),
        "42710"
    );
    assert_eq!(
        err_code(&mut eng, "DROP PUBLICATION nosuch", "postgres"),
        "42P01"
    );
    run(&mut eng, "DROP PUBLICATION IF EXISTS nosuch").unwrap();
    assert_eq!(
        err_code(
            &mut eng,
            "CREATE PUBLICATION p2 FOR TABLE nosuchtable",
            "postgres"
        ),
        "42P01"
    );
    run(&mut eng, "ALTER PUBLICATION p1 RENAME TO p1new").unwrap();
    run(&mut eng, "DROP PUBLICATION p1new").unwrap();
}
