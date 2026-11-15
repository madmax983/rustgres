/// v2.31: WITHOUT OVERLAPS family — temporal PRIMARY KEY / UNIQUE
/// WITHOUT OVERLAPS (with PERIOD foreign keys), exclusion-constraint
/// enforcement over the range/multirange machinery, and
/// pg_constraint / pg_get_constraintdef / pg_get_indexdef rendering
/// for the new shapes. Ground truth: PG19 gram.y, parse_utilcmd.c,
/// tablecmds.c, ri_triggers.c, ruleutils.c; expected outputs from
/// the vendored without_overlaps regression suite.
/// Scope record: hidden_files/v231/scope.md.
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

fn one(eng: &mut Engine, sql: &str) -> Vec<String> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => {
            assert_eq!(rows.len(), 1, "{sql}");
            rows[0]
                .iter()
                .map(|v| v.to_text().unwrap_or_default())
                .collect()
        }
        other => panic!("expected Select for {sql} -> {other:?}"),
    }
}

fn setup(eng: &mut Engine) {
    ok(
        eng,
        "CREATE TABLE temporal_rng (id int4range, valid_at daterange, \
         CONSTRAINT temporal_rng_pk PRIMARY KEY (id, valid_at WITHOUT OVERLAPS))",
    );
}

#[test]
fn v231_ddl_validation_errors() {
    let mut eng = engine();
    // Fewer than two columns: 42601.
    let e = err(
        &mut eng,
        "CREATE TABLE t1 (valid_at daterange, CONSTRAINT c UNIQUE (valid_at WITHOUT OVERLAPS))",
    );
    assert_eq!(e.code, "42601");
    // Missing column: 42703.
    let e = err(
        &mut eng,
        "CREATE TABLE t2 (id int, CONSTRAINT c UNIQUE (id, valid_at WITHOUT OVERLAPS))",
    );
    assert_eq!(e.code, "42703");
    // Non-range last column: 42804.
    let e = err(
        &mut eng,
        "CREATE TABLE t3 (id int4range, valid_at text, CONSTRAINT c UNIQUE (id, valid_at WITHOUT OVERLAPS))",
    );
    assert_eq!(e.code, "42804");
    assert_eq!(
        e.message,
        "column \"valid_at\" in WITHOUT OVERLAPS is not a range or multirange type"
    );
    // A domain over a range type is accepted.
    ok(&mut eng, "CREATE DOMAIN dr AS daterange");
    ok(
        &mut eng,
        "CREATE TABLE t4 (id int, valid_at dr, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS))",
    );
}

#[test]
fn v231_enforcement_and_details() {
    let mut eng = engine();
    setup(&mut eng);
    ok(
        &mut eng,
        "INSERT INTO temporal_rng VALUES ('[1,2)', '[2018-01-01,2018-01-05)')",
    );
    // Overlap: 23P01 exclusion violation with PG's DETAIL shape.
    let e = err(
        &mut eng,
        "INSERT INTO temporal_rng VALUES ('[1,2)', '[2018-01-02,2018-02-03)')",
    );
    assert_eq!(e.code, "23P01");
    assert_eq!(
        e.message,
        "conflicting key value violates exclusion constraint \"temporal_rng_pk\""
    );
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "Key (id, valid_at)=([1,2), [2018-01-02,2018-02-03)) conflicts with existing key (id, valid_at)=([1,2), [2018-01-01,2018-01-05))."
        )
    );
    // Adjacent ranges do not conflict; a different scalar key does not either.
    ok(
        &mut eng,
        "INSERT INTO temporal_rng VALUES ('[1,2)', '[2018-01-05,2018-02-01)')",
    );
    ok(
        &mut eng,
        "INSERT INTO temporal_rng VALUES ('[3,4)', '[2018-01-02,2018-02-03)')",
    );
    // Empty range: 23514, checked before any NULL bypass.
    let e = err(
        &mut eng,
        "INSERT INTO temporal_rng VALUES ('[1,2)', 'empty')",
    );
    assert_eq!(e.code, "23514");
    // UPDATE into overlap (same scalar key): 23P01.
    let e = err(
        &mut eng,
        "UPDATE temporal_rng SET valid_at = '[2018-01-03,2018-03-01)' WHERE id = '[1,2)' AND valid_at = '[2018-01-05,2018-02-01)'",
    );
    assert_eq!(e.code, "23P01");
    // UNIQUE (not PK): NULL scalar part bypasses, but empty still fails.
    ok(
        &mut eng,
        "CREATE TABLE uq (id int, valid_at daterange, CONSTRAINT uq1 UNIQUE (id, valid_at WITHOUT OVERLAPS))",
    );
    ok(
        &mut eng,
        "INSERT INTO uq VALUES (NULL, '[2020-01-01,2020-02-01)')",
    );
    let e = err(&mut eng, "INSERT INTO uq VALUES (NULL, 'empty')");
    assert_eq!(e.code, "23514");
}

#[test]
fn v231_catalog_rendering() {
    let mut eng = engine();
    setup(&mut eng);
    assert_eq!(
        one(
            &mut eng,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'temporal_rng_pk'"
        ),
        vec!["PRIMARY KEY (id, valid_at WITHOUT OVERLAPS)".to_string()]
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT pg_get_indexdef(conindid, 0, true) FROM pg_constraint WHERE conname = 'temporal_rng_pk'"
        ),
        vec![
            "CREATE UNIQUE INDEX temporal_rng_pk ON temporal_rng USING gist (id, valid_at)"
                .to_string()
        ]
    );
    // pg_index flags: exclusion + unique; pg_class relam is gist (783).
    assert_eq!(
        one(
            &mut eng,
            "SELECT indisexclusion::text || '/' || indisunique::text FROM pg_index WHERE indexrelid = (SELECT conindid FROM pg_constraint WHERE conname = 'temporal_rng_pk')"
        ),
        vec!["true/true".to_string()]
    );
}

#[test]
fn v231_fk_period() {
    let mut eng = engine();
    setup(&mut eng);
    ok(
        &mut eng,
        "INSERT INTO temporal_rng VALUES ('[1,2)', '[2018-01-01,2018-01-03)'), ('[1,2)', '[2018-01-03,2018-01-05)')",
    );
    ok(
        &mut eng,
        "CREATE TABLE fk1 (parent_id int4range, valid_at daterange, \
         CONSTRAINT fk1 FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES temporal_rng (id, PERIOD valid_at))",
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'fk1'"
        ),
        vec![
            "FOREIGN KEY (parent_id, PERIOD valid_at) REFERENCES temporal_rng(id, PERIOD valid_at)"
                .to_string()
        ]
    );
    // Joint coverage by two adjacent referenced rows counts.
    ok(
        &mut eng,
        "INSERT INTO fk1 VALUES ('[1,2)', '[2018-01-02,2018-01-04)')",
    );
    // Uncovered: 23503 with PG's DETAIL.
    let e = err(
        &mut eng,
        "INSERT INTO fk1 VALUES ('[1,2)', '[2018-01-02,2018-03-01)')",
    );
    assert_eq!(e.code, "23503");
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "Key (parent_id, valid_at)=([1,2), [2018-01-02,2018-03-01)) is not present in table \"temporal_rng\"."
        )
    );
    // Referenced-side delete that breaks coverage: 23503.
    let e = err(
        &mut eng,
        "DELETE FROM temporal_rng WHERE valid_at = '[2018-01-03,2018-01-05)'",
    );
    assert_eq!(e.code, "23503");
    // PERIOD mismatch family.
    let e = err(
        &mut eng,
        "CREATE TABLE b1 (p int4range, v daterange, FOREIGN KEY (p, PERIOD v) REFERENCES temporal_rng (id))",
    );
    assert_eq!(e.code, "42830");
    assert_eq!(
        e.message,
        "foreign key uses PERIOD on the referencing table but not the referenced table"
    );
    let e = err(
        &mut eng,
        "CREATE TABLE b2 (p int4range, v daterange, FOREIGN KEY (p) REFERENCES temporal_rng)",
    );
    assert_eq!(
        e.message,
        "foreign key must use PERIOD when referencing a primary key using WITHOUT OVERLAPS"
    );
    // Non-NO-ACTION is 0A000.
    let e = err(
        &mut eng,
        "CREATE TABLE b3 (p int4range, v daterange, FOREIGN KEY (p, PERIOD v) REFERENCES temporal_rng (id, PERIOD valid_at) ON DELETE CASCADE)",
    );
    assert_eq!(e.code, "0A000");
    // PERIOD type mismatch: 42804 with PG's DETAIL.
    let e = err(
        &mut eng,
        "CREATE TABLE b4 (p int4range, v tsrange, FOREIGN KEY (p, PERIOD v) REFERENCES temporal_rng (id, PERIOD valid_at))",
    );
    assert_eq!(e.code, "42804");
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "Key columns \"v\" of the referencing table and \"valid_at\" of the referenced table are of incompatible types: tsrange and daterange."
        )
    );
    // Implicit referenced list resolves the PERIOD column.
    ok(
        &mut eng,
        "CREATE TABLE b5 (p int4range, v daterange, CONSTRAINT f5 FOREIGN KEY (p, PERIOD v) REFERENCES temporal_rng)",
    );
    ok(
        &mut eng,
        "INSERT INTO b5 VALUES ('[1,2)', '[2018-01-01,2018-01-02)')",
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'f5'"
        ),
        vec!["FOREIGN KEY (p, PERIOD v) REFERENCES temporal_rng(id, PERIOD valid_at)".to_string()]
    );
}

#[test]
fn v231_on_conflict() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t (id int, valid_at daterange, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS))",
    );
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1, '[2020-01-01,2020-01-10)')",
    );
    // Bare DO NOTHING absorbs the exclusion conflict.
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1, '[2020-01-05,2020-02-01)') ON CONFLICT DO NOTHING",
    );
    // ON CONSTRAINT <wo> DO NOTHING absorbs it too.
    ok(
        &mut eng,
        "INSERT INTO t VALUES (1, '[2020-01-05,2020-02-01)') ON CONFLICT ON CONSTRAINT t_pkey DO NOTHING",
    );
    // DO UPDATE via an exclusion constraint: 0A000.
    let e = err(
        &mut eng,
        "INSERT INTO t VALUES (1, '[2020-01-05,2020-02-01)') ON CONFLICT ON CONSTRAINT t_pkey DO UPDATE SET id = 2",
    );
    assert_eq!(e.code, "0A000");
    // Column-list inference never matches an exclusion constraint: 42P10.
    let e = err(
        &mut eng,
        "INSERT INTO t VALUES (1, '[2020-01-05,2020-02-01)') ON CONFLICT (id, valid_at) DO NOTHING",
    );
    assert_eq!(e.code, "42P10");
    // Only the original row survives.
    assert_eq!(
        one(&mut eng, "SELECT count(*)::text FROM t"),
        vec!["1".to_string()]
    );
}

#[test]
fn v231_fk_codec_roundtrip() {
    // Regression (found by the v2.31 ship schedule's kill-restart):
    // a PERIOD foreign key with NO actcols section must decode —
    // the decoder dispatches on the optional-section head.
    use crate::sql::{FkAction, FkDef, decode_constraints, encode_constraints};
    let mut t = crate::storage::Table::new(
        vec![
            (
                "p".to_string(),
                ColType::Range(crate::storage::RangeElem::Int4),
            ),
            (
                "v".to_string(),
                ColType::Range(crate::storage::RangeElem::Date),
            ),
        ],
        1,
    );
    let base = FkDef {
        name: "f1".to_string(),
        cols: vec!["p".to_string()],
        ref_table: "parent".to_string(),
        ref_cols: vec!["id".to_string()],
        on_delete: FkAction::Restrict,
        on_update: FkAction::Restrict,
        on_delete_cols: None,
        on_update_cols: None,
        match_full: false,
        deferrable: false,
        initially_deferred: false,
        enforced: true,
        not_valid: false,
        is_local: true,
        period_col: Some("v".to_string()),
        ref_period_col: Some("valid_at".to_string()),
        on_delete_no_action: true,
        on_update_no_action: true,
    };
    // period only
    t.fks.push(base.clone());
    let dec = decode_constraints(&encode_constraints(&t)).expect("period-only decode");
    assert_eq!(dec.fks[0].period_col.as_deref(), Some("v"));
    assert_eq!(dec.fks[0].ref_period_col.as_deref(), Some("valid_at"));
    assert!(dec.fks[0].on_delete_cols.is_none());
    // period + actcols
    t.fks[0].on_delete_cols = Some(vec!["v".to_string()]);
    let dec = decode_constraints(&encode_constraints(&t)).expect("period+actcols decode");
    assert_eq!(dec.fks[0].period_col.as_deref(), Some("v"));
    assert_eq!(
        dec.fks[0].on_delete_cols.as_deref(),
        Some(&["v".to_string()][..])
    );
    // actcols only
    t.fks[0].period_col = None;
    t.fks[0].ref_period_col = None;
    let dec = decode_constraints(&encode_constraints(&t)).expect("actcols-only decode");
    assert!(dec.fks[0].period_col.is_none());
    assert_eq!(
        dec.fks[0].on_delete_cols.as_deref(),
        Some(&["v".to_string()][..])
    );
}

#[test]
fn v231_alter_add_and_gist_index() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE t3 (id int4range, valid_at daterange)",
    );
    ok(
        &mut eng,
        "INSERT INTO t3 VALUES ('[1,2)', '[2018-01-01,2018-01-05)'), ('[1,2)', '[2018-01-02,2018-02-03)')",
    );
    // Existing overlap: the create form of the error (no "existing").
    let e = err(
        &mut eng,
        "ALTER TABLE t3 ADD CONSTRAINT t3_uq UNIQUE (id, valid_at WITHOUT OVERLAPS)",
    );
    assert_eq!(e.code, "23P01");
    assert_eq!(e.message, "could not create exclusion constraint \"t3_uq\"");
    assert_eq!(
        e.detail.as_deref(),
        Some(
            "Key (id, valid_at)=([1,2), [2018-01-02,2018-02-03)) conflicts with key (id, valid_at)=([1,2), [2018-01-01,2018-01-05))."
        )
    );
    ok(&mut eng, "DELETE FROM t3");
    ok(
        &mut eng,
        "ALTER TABLE t3 ADD CONSTRAINT t3_uq UNIQUE (id, valid_at WITHOUT OVERLAPS)",
    );
    // Standalone GiST index: catalog-only, renders with gist, and
    // USING INDEX conversion is PG's 42809 + DETAIL.
    ok(
        &mut eng,
        "CREATE INDEX idx_g ON t3 USING gist (id, valid_at)",
    );
    assert_eq!(
        one(&mut eng, "SELECT pg_get_indexdef('idx_g')"),
        vec!["CREATE INDEX idx_g ON public.t3 USING gist (id, valid_at)".to_string()]
    );
    let e = err(
        &mut eng,
        "ALTER TABLE t3 ADD CONSTRAINT t3_pk PRIMARY KEY USING INDEX idx_g",
    );
    assert_eq!(e.code, "42809");
    assert_eq!(e.message, "\"idx_g\" is not a unique index");
    // Note: `SET datestyle TO ISO, YMD` (comma var_list) is covered
    // by the wire probe + the vendored schedule; the unit harness
    // routes SET through the session layer (25001).
}
