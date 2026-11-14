/// v2.29: index catalog family — pg_index / pg_indexes as real
/// queryable relations, physical partitioned-index children in
/// pg_class / pg_indexes with relkind / relhassubclass, DROP INDEX
/// propagation, adoption, ALTER INDEX ATTACH PARTITION, INCLUDE
/// columns, CONCURRENTLY rules. Ground truth: PG19 pg_index.h,
/// system_views.sql, indexcmds.c, tablecmds.c, dependency.c.
/// Scope record: hidden_files/v229/scope.md.
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

fn rows_text(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
            .collect(),
        other => panic!("expected Select for {sql} -> {other:?}"),
    }
}

fn one(eng: &mut Engine, sql: &str) -> Vec<String> {
    let r = rows_text(eng, sql);
    assert_eq!(r.len(), 1, "{sql} -> {r:?}");
    r.into_iter().next().unwrap()
}

#[test]
fn v229_pg_index_plain_unique_pk() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b text, c int)");
    ok(&mut eng, "CREATE INDEX ix_t_a ON t (a)");
    ok(&mut eng, "CREATE UNIQUE INDEX uq_t_b ON t (b)");
    // pg_index carries one row per index with PG19 indkey rendering.
    assert_eq!(
        rows_text(
            &mut eng,
            "SELECT indexrelid::regclass::text, indrelid::regclass::text, indnatts, \
             indnkeyatts, indisunique, indkey, indclass FROM pg_index \
             ORDER BY indexrelid::regclass::text"
        ),
        vec![
            vec!["ix_t_a", "t", "1", "1", "f", "1", "1978"],
            vec!["uq_t_b", "t", "1", "1", "t", "2", "3126"],
        ]
    );
    // PK backing index reports indisprimary.
    ok(&mut eng, "CREATE TABLE pkt (id int PRIMARY KEY, v text)");
    assert_eq!(
        one(
            &mut eng,
            "SELECT indisprimary, indisunique FROM pg_index WHERE indrelid = 'pkt'::regclass"
        ),
        vec!["t", "t"]
    );
    // pg_class gains index rows with relkind 'i'.
    assert_eq!(
        rows_text(
            &mut eng,
            "SELECT relname, relkind, relnamespace, relam FROM pg_class \
             WHERE relname IN ('ix_t_a', 't') ORDER BY relname"
        ),
        vec![
            vec!["ix_t_a", "i", "2200", "403"],
            vec!["t", "r", "2200", "2"],
        ]
    );
    // The unique index still enforces.
    ok(&mut eng, "INSERT INTO t VALUES (1, 'x', 1)");
    let e = err(&mut eng, "INSERT INTO t VALUES (2, 'x', 2)");
    assert_eq!(e.code, "23505");
    assert!(e.message.contains("uq_t_b"), "{}", e.message);
}

#[test]
fn v229_pg_indexes_and_get_indexdef() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b int, c int)");
    ok(
        &mut eng,
        "CREATE INDEX i1 ON t (a DESC NULLS LAST, b NULLS FIRST)",
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT indoption FROM pg_index WHERE indexrelid = 'i1'::regclass"
        ),
        vec!["1 2"]
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'i1'"
        ),
        vec!["CREATE INDEX i1 ON public.t USING btree (a DESC NULLS LAST, b NULLS FIRST)"]
    );
    assert_eq!(
        one(&mut eng, "SELECT pg_get_indexdef('i1'::regclass)"),
        vec!["CREATE INDEX i1 ON public.t USING btree (a DESC NULLS LAST, b NULLS FIRST)"]
    );
    // 3-arg form renders a single key element; unknown oid is NULL.
    assert_eq!(
        one(&mut eng, "SELECT pg_get_indexdef('i1'::regclass, 1, false)"),
        vec!["a DESC NULLS LAST"]
    );
    assert_eq!(
        rows_text(&mut eng, "SELECT pg_get_indexdef(999999)"),
        vec![vec!["".to_string()]]
    );
}

#[test]
fn v229_include_columns() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE incl (c1 int, c2 int, c3 int, c4 int)",
    );
    ok(
        &mut eng,
        "CREATE UNIQUE INDEX incl_idx ON incl (c1, c2) INCLUDE (c3, c4)",
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT indnatts, indnkeyatts, indkey FROM pg_index \
             WHERE indexrelid = 'incl_idx'::regclass"
        ),
        vec!["4", "2", "1 2 3 4"]
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'incl_idx'"
        ),
        vec!["CREATE UNIQUE INDEX incl_idx ON public.incl USING btree (c1, c2) INCLUDE (c3, c4)"]
    );
    // INCLUDE columns are not keys: duplicates there are fine, key
    // duplicates are not.
    ok(&mut eng, "INSERT INTO incl VALUES (1, 1, 1, 1)");
    let e = err(&mut eng, "INSERT INTO incl VALUES (1, 1, 9, 9)");
    assert_eq!(e.code, "23505");
    ok(&mut eng, "INSERT INTO incl VALUES (1, 2, 1, 1)");
    // Constraint form: auto name carries key + INCLUDE columns, and
    // pg_get_constraintdef renders the INCLUDE clause.
    ok(
        &mut eng,
        "CREATE TABLE inclc (c1 int, c2 int, c3 int, c4 int, UNIQUE (c1, c2) INCLUDE (c3, c4))",
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT indexname FROM pg_indexes WHERE tablename = 'inclc'"
        ),
        vec!["inclc_c1_c2_c3_c4_key"]
    );
    let def = one(
        &mut eng,
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conname = 'inclc_c1_c2_c3_c4_key'",
    );
    assert_eq!(def, vec!["UNIQUE (c1, c2) INCLUDE (c3, c4)"]);
    // Unknown INCLUDE column is 42703.
    let e = err(&mut eng, "CREATE INDEX bad ON incl (c1) INCLUDE (nope)");
    assert_eq!(e.code, "42703");
}

#[test]
fn v229_partitioned_family() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE idxpart (a int, b int) PARTITION BY RANGE (a)",
    );
    // A partitioned index with no partitions yet: relhassubclass f.
    ok(&mut eng, "CREATE INDEX early ON idxpart (a)");
    assert_eq!(
        one(
            &mut eng,
            "SELECT relkind, relhassubclass FROM pg_class WHERE relname = 'early'"
        ),
        vec!["I", "f"]
    );
    ok(&mut eng, "DROP INDEX early");
    ok(
        &mut eng,
        "CREATE TABLE idxpart1 PARTITION OF idxpart FOR VALUES FROM (0) TO (100)",
    );
    ok(
        &mut eng,
        "CREATE TABLE idxpart2 PARTITION OF idxpart FOR VALUES FROM (100) TO (200)",
    );
    ok(&mut eng, "CREATE INDEX idxpart_idx ON idxpart (a)");
    assert_eq!(
        rows_text(
            &mut eng,
            "SELECT relname, relkind FROM pg_class WHERE relname LIKE 'idxpart%_idx' ORDER BY relname"
        ),
        vec![
            vec!["idxpart1_a_idx", "i"],
            vec!["idxpart2_a_idx", "i"],
            vec!["idxpart_idx", "I"],
        ]
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT relhassubclass FROM pg_class WHERE relname = 'idxpart_idx'"
        ),
        vec!["t"]
    );
    // pg_inherits carries the index links.
    assert_eq!(
        rows_text(
            &mut eng,
            "SELECT inhrelid::regclass::text FROM pg_inherits \
             WHERE inhparent = 'idxpart_idx'::regclass ORDER BY 1"
        ),
        vec![vec!["idxpart1_a_idx"], vec!["idxpart2_a_idx"]]
    );
    // ON ONLY: no children.
    ok(&mut eng, "CREATE INDEX onlyix ON ONLY idxpart (b)");
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_class WHERE relname LIKE '%onlyix%'"
        ),
        vec!["1"]
    );
    // A partition created later inherits the family's indexes.
    ok(
        &mut eng,
        "CREATE TABLE idxpart3 PARTITION OF idxpart FOR VALUES FROM (200) TO (300)",
    );
    assert_eq!(
        rows_text(
            &mut eng,
            "SELECT indexname FROM pg_indexes WHERE tablename = 'idxpart3' ORDER BY indexname"
        ),
        vec![vec!["idxpart3_a_idx"], vec!["idxpart3_b_idx"]]
    );
    // Dropping a child directly is 2BP01; dropping the parent
    // propagates to the leaves.
    let e = err(&mut eng, "DROP INDEX idxpart1_a_idx");
    assert_eq!(e.code, "2BP01");
    assert!(
        e.message
            .contains("cannot drop index idxpart1_a_idx because index idxpart_idx requires it"),
        "{}",
        e.message
    );
    ok(&mut eng, "DROP INDEX idxpart_idx");
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_class WHERE relname IN \
             ('idxpart_idx','idxpart1_a_idx','idxpart2_a_idx','idxpart3_a_idx')"
        ),
        vec!["0"]
    );
}

#[test]
fn v229_partitioned_unique_needs_partition_key() {
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE p (a int, b int) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (100)",
    );
    let e = err(&mut eng, "CREATE UNIQUE INDEX bad ON p (b)");
    assert_eq!(e.code, "0A000");
    assert!(
        e.message.contains(
            "UNIQUE constraint on partitioned table must include all partitioning columns"
        ),
        "{}",
        e.message
    );
    ok(&mut eng, "CREATE UNIQUE INDEX good ON p (a, b)");
    ok(&mut eng, "INSERT INTO p VALUES (10, 5)");
    let e = err(&mut eng, "INSERT INTO p VALUES (10, 5)");
    assert_eq!(e.code, "23505");
    assert!(e.message.contains("p1_a_b_idx"), "{}", e.message);
    // Same b in another leaf is fine.
    ok(
        &mut eng,
        "CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (100) TO (200)",
    );
    ok(&mut eng, "INSERT INTO p VALUES (150, 5)");
}

#[test]
fn v229_adoption_and_attach() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE adp (a int) PARTITION BY RANGE (a)");
    ok(
        &mut eng,
        "CREATE TABLE adp1 PARTITION OF adp FOR VALUES FROM (0) TO (10)",
    );
    ok(&mut eng, "CREATE INDEX adp1_myix ON adp1 (a)");
    ok(&mut eng, "CREATE INDEX adp_ix ON adp (a)");
    // The pre-existing equivalent index is adopted, not duplicated.
    assert_eq!(
        rows_text(
            &mut eng,
            "SELECT indexname FROM pg_indexes WHERE tablename = 'adp1'"
        ),
        vec![vec!["adp1_myix"]]
    );
    assert_eq!(
        one(
            &mut eng,
            "SELECT inhparent::regclass::text FROM pg_inherits \
             WHERE inhrelid = 'adp1_myix'::regclass"
        ),
        vec!["adp_ix"]
    );
    ok(&mut eng, "DROP INDEX adp_ix");
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_class WHERE relname = 'adp1_myix'"
        ),
        vec!["0"]
    );
    // ALTER INDEX ATTACH PARTITION links an existing leaf index.
    ok(&mut eng, "CREATE TABLE atp (a int) PARTITION BY RANGE (a)");
    ok(&mut eng, "CREATE INDEX atp_ix ON atp (a)");
    ok(&mut eng, "CREATE TABLE at1 (a int)");
    ok(&mut eng, "CREATE INDEX at1_ix ON at1 (a)");
    ok(
        &mut eng,
        "ALTER TABLE atp ATTACH PARTITION at1 FOR VALUES FROM (0) TO (10)",
    );
    ok(&mut eng, "ALTER INDEX atp_ix ATTACH PARTITION at1_ix");
    assert_eq!(
        one(
            &mut eng,
            "SELECT inhparent::regclass::text FROM pg_inherits \
             WHERE inhrelid = 'at1_ix'::regclass"
        ),
        vec!["atp_ix"]
    );
    // An index on a table that is not a partition of the parent
    // table is 55000.
    ok(&mut eng, "CREATE TABLE at2 (a int)");
    ok(&mut eng, "CREATE INDEX at2_ix ON at2 (a)");
    let e = err(&mut eng, "ALTER INDEX atp_ix ATTACH PARTITION at2_ix");
    assert_eq!(e.code, "55000");
}

#[test]
fn v229_expression_index_build_evaluates_rows() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int, b int)");
    ok(&mut eng, "INSERT INTO t VALUES (1, 0)");
    // PG builds the index over existing rows: (a/b) with b = 0 is
    // 22012, for plain and CONCURRENTLY creates alike (indexing.sql).
    let e = err(&mut eng, "CREATE INDEX bad ON t ((a/b))");
    assert_eq!(e.code, "22012");
    let e = err(&mut eng, "CREATE INDEX CONCURRENTLY bad2 ON t ((a/b))");
    assert_eq!(e.code, "22012");
    // No index was left behind by the failed builds.
    assert_eq!(
        one(
            &mut eng,
            "SELECT count(*) FROM pg_class WHERE relname IN ('bad', 'bad2')"
        ),
        vec!["0"]
    );
    ok(&mut eng, "DELETE FROM t");
    ok(&mut eng, "CREATE INDEX good ON t ((a/b))");
    // Capability errors are NOT build errors in the catalog-only
    // expression-index model: an unresolvable function in the
    // expression does not abort the CREATE (privileges.sql's
    // sro_ifun shape; PG builds it because the function exists
    // there — here it stays catalog-only like every other
    // expression index).
    ok(&mut eng, "CREATE INDEX udf_ix ON t ((no_such_fn(a)))");
}

#[test]
fn v229_indisvalid_and_cluster() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE cp (a int) PARTITION BY RANGE (a)");
    ok(
        &mut eng,
        "CREATE TABLE cp1 PARTITION OF cp FOR VALUES FROM (0) TO (10)",
    );
    // An ON ONLY partitioned index starts invalid: no partition
    // carries a linked child yet.
    ok(&mut eng, "CREATE INDEX cp_only ON ONLY cp (a)");
    assert_eq!(
        one(
            &mut eng,
            "SELECT indisvalid FROM pg_index WHERE indexrelid = 'cp_only'::regclass"
        ),
        vec!["f"]
    );
    // CLUSTER on an invalid index is 0A000 (PG19 repack.c).
    let e = err(&mut eng, "CLUSTER cp USING cp_only");
    assert_eq!(e.code, "0A000");
    assert!(
        e.message
            .contains("cannot cluster on invalid index \"cp_only\""),
        "{}",
        e.message
    );
    // A plain create propagates children and is valid immediately.
    ok(&mut eng, "CREATE INDEX cp_ix ON cp (a)");
    assert_eq!(
        one(
            &mut eng,
            "SELECT indisvalid FROM pg_index WHERE indexrelid = 'cp_ix'::regclass"
        ),
        vec!["t"]
    );
    ok(&mut eng, "CLUSTER cp USING cp_ix");
}

#[test]
fn v229_on_conflict_prefers_constraint_arbiter() {
    // partition_info.sql's shape: a standalone ON ONLY unique index
    // on an intermediate partitioned table must not shadow the
    // PRIMARY KEY arbiter (it has no per-leaf backing index).
    let mut eng = engine();
    ok(
        &mut eng,
        "CREATE TABLE pt (a int PRIMARY KEY) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE p1 PARTITION OF pt FOR VALUES FROM (1) TO (2) PARTITION BY RANGE (a)",
    );
    ok(
        &mut eng,
        "CREATE TABLE p1_1 PARTITION OF p1 FOR VALUES FROM (1) TO (2)",
    );
    ok(&mut eng, "CREATE UNIQUE INDEX ON ONLY p1 (a)");
    ok(
        &mut eng,
        "INSERT INTO p1 VALUES (1) ON CONFLICT (a) DO NOTHING",
    );
    // The PK arbiter enforces: a second identical insert is a
    // no-op, not a duplicate row.
    ok(
        &mut eng,
        "INSERT INTO p1 VALUES (1) ON CONFLICT (a) DO NOTHING",
    );
    assert_eq!(one(&mut eng, "SELECT count(*) FROM pt"), vec!["1"]);
}

#[test]
fn v229_concurrently_rules() {
    let mut eng = engine();
    ok(&mut eng, "CREATE TABLE t (a int)");
    ok(&mut eng, "CREATE INDEX CONCURRENTLY ci ON t (a)");
    ok(&mut eng, "CREATE TABLE p (a int) PARTITION BY RANGE (a)");
    ok(
        &mut eng,
        "CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10)",
    );
    ok(&mut eng, "CREATE INDEX pix ON p (a)");
    let e = err(&mut eng, "CREATE INDEX CONCURRENTLY bad ON p (a)");
    assert_eq!(e.code, "0A000");
    assert!(
        e.message
            .contains("cannot create index on partitioned table \"p\" concurrently"),
        "{}",
        e.message
    );
    let e = err(&mut eng, "DROP INDEX CONCURRENTLY pix");
    assert_eq!(e.code, "0A000");
    assert!(
        e.message
            .contains("cannot drop partitioned index \"pix\" concurrently"),
        "{}",
        e.message
    );
    ok(&mut eng, "DROP INDEX pix");
}
