/// v2.23: multi-column HASH partition routing with the real PG
/// hash algorithm (hashfn.c Jenkins mix/final + hash_combine64 +
/// per-type extended hashes), the CREATE OPERATOR CLASS registry
/// (custom hash support functions drive routing), and
/// satisfies_hash_partition. Ground truth: PG19 hashfn.c,
/// partbounds.c, and the hash_part / partition_prune / update
/// expected outputs; the Jenkins constants below were pinned by an
/// independent Python transcription of hashfn.c validated against
/// the pph_arrpart placements in partition_prune.out.
use super::*;
use crate::sql::parse_statement;
use crate::storage::{PartKey, PartitionInfo};

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

fn err_code(eng: &mut Engine, sql: &str) -> String {
    match run(eng, sql) {
        Ok(_) => panic!("{sql} -> expected error, got success"),
        Err(e) => e.code.to_string(),
    }
}

fn rows_text(eng: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match run(eng, sql) {
        Ok(ExecResult::Select { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text().unwrap_or_default()).collect())
            .collect(),
        other => panic!("expected Select for {sql}, got {other:?}"),
    }
}

fn one_bool(eng: &mut Engine, sql: &str) -> bool {
    let rows = rows_text(eng, sql);
    assert_eq!(rows.len(), 1, "{sql}");
    rows[0][0] == "t" || rows[0][0] == "true"
}

#[test]
fn jenkins_pinned_vectors() {
    const SEED: u64 = 0x7A5B_2236_7996_DCFD;
    // Constants from the independent Python hashfn.c port.
    assert_eq!(super::partition::hash_bytes(b""), 2817148525);
    assert_eq!(super::partition::hash_bytes(b"hello"), 2424674345);
    assert_eq!(
        super::partition::hash_bytes_extended(b"", SEED),
        12746098489256034243
    );
    assert_eq!(
        super::partition::hash_bytes_extended(b"xxx", SEED),
        18159566888361429914
    );
    assert_eq!(
        super::partition::hash_bytes_extended(b"abcde", SEED),
        4809259092934399961
    );
    assert_eq!(
        super::partition::hash_bytes_extended(b"0123456789abc", SEED),
        9136706884384298968
    );
    assert_eq!(
        super::partition::hash_bytes_extended(b"0123456789abcdefghijklmno", 0),
        16431362363354061495
    );
    assert_eq!(
        super::partition::hash_bytes_uint32_extended(1, SEED),
        5968994663651403477
    );
    assert_eq!(
        super::partition::hash_bytes_uint32_extended(0, SEED),
        14043151463718383821
    );
}

/// Row hash against a synthetic two-column HASH key. With
/// `custom`, the key carries the part_test_* opclasses (the
/// functions must already exist in `eng`); otherwise the default
/// per-type hashes apply. NOTE: PG's hp / mchash / mcinthash /
/// hpart regress families all use the CUSTOM opclasses — their
/// published placements are custom-hash placements.
fn row_hash_ab(
    eng: &mut Engine,
    a: Option<Value>,
    b: Option<Value>,
    b_ty: ColType,
    custom: bool,
) -> u64 {
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
    let cols = vec![("a".to_string(), ColType::Int), ("b".to_string(), b_ty)];
    let (oc_a, oc_b) = if custom {
        (
            Some("part_test_int4_ops".to_string()),
            Some(if b_ty == ColType::Text {
                "part_test_text_ops".to_string()
            } else {
                "part_test_int4_ops".to_string()
            }),
        )
    } else {
        (None, None)
    };
    let pinfo = PartitionInfo {
        method: crate::storage::PartMethod::Hash,
        key: vec![
            PartKey {
                col: 0,
                expr: None,
                opclass: oc_a,
            },
            PartKey {
                col: 1,
                expr: None,
                opclass: oc_b,
            },
        ],
        bound: None,
        is_default: false,
        parent: None,
        children: vec![],
        is_partitioned: true,
    };
    let key = vec![a.unwrap_or(Value::Null), b.unwrap_or(Value::Null)];
    super::partition::partition_row_hash(eng, &mut ctx, &pinfo, &cols, &key).expect("row hash")
}

/// The two custom hash support functions from test_setup.sql.
fn create_part_fns(eng: &mut Engine) {
    ok(
        eng,
        "create function part_hashint4_noop(int4, int8) returns int8 as $$ select $1 + $2 $$ language sql strict immutable",
    );
    ok(
        eng,
        "create function part_hashtext_length(text, int8) returns int8 as $$ select length(coalesce($1, '')) $$ language sql strict immutable",
    );
    ok(
        eng,
        "create operator class part_test_int4_ops for type int4 using hash as operator 1 =, function 2 part_hashint4_noop(int4, int8)",
    );
    ok(
        eng,
        "create operator class part_test_text_ops for type text using hash as operator 1 =, function 2 part_hashtext_length(text, int8)",
    );
}

#[test]
fn hp_custom_opclass_remainders() {
    let mut eng = engine();
    create_part_fns(&mut eng);
    // partition_prune.out hp placements (modulus 4) — hp's key is
    // (a part_test_int4_ops, b part_test_text_ops).
    let cases: [(Option<Value>, Option<Value>, u64); 6] = [
        (None, None, 0),
        (Some(Value::Int(1)), None, 1),
        (Some(Value::Int(1)), Some(Value::text("xxx")), 0),
        (None, Some(Value::text("xxx")), 2),
        (Some(Value::Int(2)), Some(Value::text("xxx")), 3),
        (Some(Value::Int(1)), Some(Value::text("abcde")), 2),
    ];
    for (a, b, want) in cases {
        assert_eq!(row_hash_ab(&mut eng, a, b, ColType::Text, true) % 4, want);
    }
    // update.sql hpart placements (both keys part_test_int4_ops).
    assert_eq!(
        row_hash_ab(
            &mut eng,
            Some(Value::Int(1)),
            Some(Value::Int(1)),
            ColType::Int,
            true
        ) % 2,
        1
    );
    assert_eq!(
        row_hash_ab(
            &mut eng,
            Some(Value::Int(2)),
            Some(Value::Int(5)),
            ColType::Int,
            true
        ) % 4,
        2
    );
    assert_eq!(
        row_hash_ab(
            &mut eng,
            Some(Value::Int(3)),
            Some(Value::Int(4)),
            ColType::Int,
            true
        ) % 8,
        4
    );
    // hash_part.out mcinthash (custom ops): [0,0] -> 3, [0,1] -> 0.
    assert_eq!(
        row_hash_ab(
            &mut eng,
            Some(Value::Int(0)),
            Some(Value::Int(0)),
            ColType::Int,
            true
        ) % 4,
        3
    );
    assert_eq!(
        row_hash_ab(
            &mut eng,
            Some(Value::Int(0)),
            Some(Value::Int(1)),
            ColType::Int,
            true
        ) % 4,
        0
    );
}

#[test]
fn default_hash_remainders() {
    // Default per-type hashes; constants cross-checked against
    // the independent Python hashfn.c port (and, for int[], the
    // pph_arrpart placements in partition_prune.out).
    let mut eng = engine();
    let cases: [(Option<Value>, Option<Value>, u64); 6] = [
        (None, None, 0),
        (Some(Value::Int(1)), None, 0),
        (Some(Value::Int(1)), Some(Value::text("xxx")), 0),
        (None, Some(Value::text("xxx")), 1),
        (Some(Value::Int(2)), Some(Value::text("xxx")), 1),
        (Some(Value::Int(1)), Some(Value::text("abcde")), 3),
    ];
    for (a, b, want) in cases {
        assert_eq!(row_hash_ab(&mut eng, a, b, ColType::Text, false) % 4, want);
    }
    assert_eq!(
        row_hash_ab(
            &mut eng,
            Some(Value::Int(0)),
            Some(Value::Int(0)),
            ColType::Int,
            false
        ) % 4,
        3
    );
    assert_eq!(
        row_hash_ab(
            &mut eng,
            Some(Value::Int(0)),
            Some(Value::Int(1)),
            ColType::Int,
            false
        ) % 4,
        3
    );
}

/// The mchash setup from update.sql / hash_part.sql: custom
/// opclasses whose hash functions are (value + seed) and
/// length(coalesce(value, '')).
fn setup_mchash(eng: &mut Engine) {
    create_part_fns(eng);
    ok(
        eng,
        "create table mchash (a int4, b text) partition by hash (a part_test_int4_ops, b part_test_text_ops)",
    );
    for r in 0..4 {
        ok(
            eng,
            &format!(
                "create table mchash{} partition of mchash for values with (modulus 4, remainder {})",
                r + 1,
                r
            ),
        );
    }
}

#[test]
fn custom_opclass_routing_full_stack() {
    let mut eng = engine();
    setup_mchash(&mut eng);
    // hash_a = 0 + SEED, hash_b = length('') = 0 -> remainder 2.
    ok(&mut eng, "insert into mchash values (0, '')");
    assert_eq!(
        rows_text(&mut eng, "select count(*) from mchash3"),
        vec![vec!["1".to_string()]]
    );
    for leaf in ["mchash1", "mchash2", "mchash4"] {
        assert_eq!(
            rows_text(&mut eng, &format!("select count(*) from {leaf}")),
            vec![vec!["0".to_string()]],
            "{leaf} must stay empty"
        );
    }
    // Direct insert into the wrong leaf is a bound violation.
    assert_eq!(
        err_code(&mut eng, "insert into mchash1 values (0, '')"),
        "23514"
    );
    // Direct insert into the right leaf works.
    ok(&mut eng, "insert into mchash3 values (0, '')");
    // satisfies_hash_partition sees the same routing.
    assert!(one_bool(
        &mut eng,
        "select satisfies_hash_partition('mchash'::regclass, 4, 2, 0, '')"
    ));
    assert!(!one_bool(
        &mut eng,
        "select satisfies_hash_partition('mchash'::regclass, 4, 1, 0, '')"
    ));
}

#[test]
fn opclass_registry_ddl() {
    let mut eng = engine();
    setup_mchash(&mut eng);
    // Duplicate (name, method) -> 42710.
    assert_eq!(
        err_code(
            &mut eng,
            "create operator class part_test_int4_ops for type int4 using hash as operator 1 =, function 2 part_hashint4_noop(int4, int8)"
        ),
        "42710"
    );
    // Unknown access method -> 42704.
    assert_eq!(
        err_code(
            &mut eng,
            "create operator class bad_ops for type int4 using nosucham as operator 1 ="
        ),
        "42704"
    );
    // Missing hash function -> 42883.
    assert_eq!(
        err_code(
            &mut eng,
            "create operator class bad2_ops for type int4 using hash as operator 1 =, function 2 no_such_fn(int4, int8)"
        ),
        "42883"
    );
    // A partition key naming an unknown opclass -> 42704.
    assert_eq!(
        err_code(
            &mut eng,
            "create table badt (a int4) partition by hash (a nosuch_ops)"
        ),
        "42704"
    );
    // An opclass for the wrong type -> 42883.
    assert_eq!(
        err_code(
            &mut eng,
            "create table badt2 (a text) partition by hash (a part_test_int4_ops)"
        ),
        "42883"
    );
    // DROP: missing -> 42704, IF EXISTS -> notice only.
    assert_eq!(
        err_code(&mut eng, "drop operator class nosuch_ops using hash"),
        "42704"
    );
    ok(
        &mut eng,
        "drop operator class if exists nosuch_ops using hash",
    );
    // ALTER ... SET SCHEMA moves the registration.
    ok(&mut eng, "create schema alter2");
    ok(
        &mut eng,
        "alter operator class part_test_text_ops using hash set schema alter2",
    );
    ok(
        &mut eng,
        "drop operator class alter2.part_test_text_ops using hash",
    );
    // ... and the int4 class is still usable afterwards.
    ok(
        &mut eng,
        "create table still_ok (a int4) partition by hash (a part_test_int4_ops)",
    );
}

#[test]
fn hash_parent_shape_errors() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table hp (a int, b text) partition by hash (a, b)",
    );
    // DEFAULT partition on a HASH parent -> 42P16.
    assert_eq!(
        err_code(&mut eng, "create table hp_def partition of hp default"),
        "42P16"
    );
    // Bad modulus / remainder -> 42P16 with PG's messages.
    assert_eq!(
        err_code(
            &mut eng,
            "create table hp_x partition of hp for values with (modulus 0, remainder 0)"
        ),
        "42P16"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "create table hp_y partition of hp for values with (modulus 4, remainder 4)"
        ),
        "42P16"
    );
    ok(
        &mut eng,
        "create table hp4 partition of hp for values with (modulus 4, remainder 0)",
    );
    // Modulus 3 is not a factor of (nor divisible by) 4 -> 42P17.
    assert_eq!(
        err_code(
            &mut eng,
            "create table hp3 partition of hp for values with (modulus 3, remainder 0)"
        ),
        "42P17"
    );
    // Same remainder class under a compatible modulus still
    // overlaps -> 23514. (Residue 4 mod 8 is a SUBSET of residue
    // 0 mod 4, so it overlaps too; residue 1 mod 8 is disjoint.)
    assert_eq!(
        err_code(
            &mut eng,
            "create table hp8 partition of hp for values with (modulus 8, remainder 0)"
        ),
        "23514"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "create table hp8c partition of hp for values with (modulus 8, remainder 4)"
        ),
        "23514"
    );
    ok(
        &mut eng,
        "create table hp8b partition of hp for values with (modulus 8, remainder 1)",
    );
}

#[test]
fn default_hash_multicol_routing_and_moves() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table hp (a int, b text) partition by hash (a, b)",
    );
    for r in 0..4 {
        ok(
            &mut eng,
            &format!(
                "create table hp_r{} partition of hp for values with (modulus 4, remainder {})",
                r, r
            ),
        );
    }
    // The same row shapes as partition_prune's hp, but routed by
    // the DEFAULT hashes here (hp in the regress suite uses the
    // custom opclasses; see hp_custom_opclass_remainders).
    // Default remainders (mod 4): 0, 0, 0, 1, 1, 3.
    ok(&mut eng, "insert into hp values (null, null)"); // -> 0
    ok(&mut eng, "insert into hp values (1, null)"); // -> 0
    ok(&mut eng, "insert into hp values (1, 'xxx')"); // -> 0
    ok(&mut eng, "insert into hp values (null, 'xxx')"); // -> 1
    ok(&mut eng, "insert into hp values (2, 'xxx')"); // -> 1
    ok(&mut eng, "insert into hp values (1, 'abcde')"); // -> 3
    assert_eq!(
        rows_text(&mut eng, "select count(*) from hp_r0"),
        vec![vec!["3".to_string()]]
    );
    assert_eq!(
        rows_text(&mut eng, "select count(*) from hp_r1"),
        vec![vec!["2".to_string()]]
    );
    assert_eq!(
        rows_text(&mut eng, "select count(*) from hp_r2"),
        vec![vec!["0".to_string()]]
    );
    assert_eq!(
        rows_text(&mut eng, "select count(*) from hp_r3"),
        vec![vec!["1".to_string()]]
    );
    // An UPDATE that moves a row out of its leaf's bound fails —
    // update.sql's hpart family (custom opclasses both keys).
    create_part_fns(&mut eng);
    ok(
        &mut eng,
        "create table hpart (a int, b int) partition by hash (a part_test_int4_ops, b part_test_int4_ops)",
    );
    ok(
        &mut eng,
        "create table hpart1 partition of hpart for values with (modulus 2, remainder 1)",
    );
    ok(
        &mut eng,
        "create table hpart2 partition of hpart for values with (modulus 2, remainder 0)",
    );
    ok(&mut eng, "insert into hpart1 values (1, 1)");
    assert_eq!(
        err_code(&mut eng, "update hpart1 set a = 3, b = 4"),
        "23514"
    );
    // An UPDATE through the parent that stays routable works:
    // (2, 5) hashes to remainder 0 at modulus 2.
    ok(&mut eng, "update hpart set a = 2, b = 5 where a = 1");
    assert_eq!(
        rows_text(&mut eng, "select count(*) from hpart2"),
        vec![vec!["1".to_string()]]
    );
}

#[test]
fn satisfies_hash_partition_shapes() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table mc2 (a int, b int) partition by hash (a, b)",
    );
    for r in 0..4 {
        ok(
            &mut eng,
            &format!(
                "create table mc2_{} partition of mc2 for values with (modulus 4, remainder {})",
                r, r
            ),
        );
    }
    // Default hashes: [0,0] -> 3, [0,1] -> 3 (mod 4).
    assert!(one_bool(
        &mut eng,
        "select satisfies_hash_partition('mc2'::regclass, 4, 3, 0, 0)"
    ));
    assert!(!one_bool(
        &mut eng,
        "select satisfies_hash_partition('mc2'::regclass, 4, 0, 0, 0)"
    ));
    assert!(one_bool(
        &mut eng,
        "select satisfies_hash_partition('mc2'::regclass, 4, 3, 0, 1)"
    ));
    assert!(!one_bool(
        &mut eng,
        "select satisfies_hash_partition('mc2'::regclass, 4, 0, 0, 1)"
    ));
    // NULL among the first three -> false, ahead of range errors.
    assert!(!one_bool(
        &mut eng,
        "select satisfies_hash_partition(null, 0, 0, 0, 0)"
    ));
    assert!(!one_bool(
        &mut eng,
        "select satisfies_hash_partition('mc2'::regclass, null, 0, 0, 0)"
    ));
    // Range errors (22023) precede the parent lookup.
    assert_eq!(
        err_code(&mut eng, "select satisfies_hash_partition(0, 8, 8, 1)"),
        "22023"
    );
    assert_eq!(
        err_code(
            &mut eng,
            "select satisfies_hash_partition('mc2'::regclass, 0, 0, 0, 0)"
        ),
        "22023"
    );
    // OID 0 -> could not open relation (XX000).
    assert_eq!(
        err_code(&mut eng, "select satisfies_hash_partition(0, 1, 0, 1)"),
        "XX000"
    );
    // Wrong key count -> 22023.
    assert_eq!(
        err_code(
            &mut eng,
            "select satisfies_hash_partition('mc2'::regclass, 4, 0, 1)"
        ),
        "22023"
    );
    // A RANGE parent is not a hash partitioned table.
    ok(&mut eng, "create table rp (a int) partition by range (a)");
    assert_eq!(
        err_code(
            &mut eng,
            "select satisfies_hash_partition('rp'::regclass, 4, 0, 1)"
        ),
        "22023"
    );
    // VARIADIC form.
    assert!(one_bool(
        &mut eng,
        "select satisfies_hash_partition('mc2'::regclass, 4, 3, variadic array[0, 0])"
    ));
    assert!(!one_bool(
        &mut eng,
        "select satisfies_hash_partition('mc2'::regclass, 4, 3, variadic null::int[])"
    ));
    // VARIADIC element type mismatch -> 22023.
    assert_eq!(
        err_code(
            &mut eng,
            "select satisfies_hash_partition('mc2'::regclass, 4, 0, variadic array[now(), now()])"
        ),
        "22023"
    );
    // VARIADIC wrong length -> 22023.
    assert_eq!(
        err_code(
            &mut eng,
            "select satisfies_hash_partition('mc2'::regclass, 4, 0, variadic array[1])"
        ),
        "22023"
    );
}

#[test]
fn attach_validates_existing_rows_against_hash_bound() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table hp (a int, b text) partition by hash (a, b)",
    );
    // (1, 'xxx') hashes to remainder 0 (default hashes):
    // attachable as the remainder-0 leaf.
    ok(&mut eng, "create table cand_ok (a int, b text)");
    ok(&mut eng, "insert into cand_ok values (1, 'xxx')");
    ok(
        &mut eng,
        "alter table hp attach partition cand_ok for values with (modulus 4, remainder 0)",
    );
    // (2, 'xxx') hashes to remainder 1 (default hashes):
    // attaching it as remainder 2 must fail validation.
    ok(&mut eng, "create table cand_bad (a int, b text)");
    ok(&mut eng, "insert into cand_bad values (2, 'xxx')");
    let code = err_code(
        &mut eng,
        "alter table hp attach partition cand_bad for values with (modulus 4, remainder 2)",
    );
    assert_eq!(code, "23514");
}

#[test]
fn operator_link_validations_and_drop_am_order() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create function myeq(int4, int4) returns boolean as $$ select $1 = $2 $$ language sql immutable",
    );
    // PG19 pg_operator.c: self-negator -> 42P13 (masked at base
    // by the === parse failure; create_operator.sql expects it).
    assert_eq!(
        err_code(
            &mut eng,
            "create operator === (leftarg = int4, rightarg = int4, procedure = myeq, negator = ===)"
        ),
        "42P13"
    );
    // Claiming the builtin `=` (its own commutator) -> 42P13.
    assert_eq!(
        err_code(
            &mut eng,
            "create operator === (leftarg = int4, rightarg = int4, procedure = myeq, commutator = =)"
        ),
        "42P13"
    );
    // DROP OPERATOR CLASS validates the access method before the
    // IF EXISTS existence check (opclasscmds.c).
    assert_eq!(
        err_code(
            &mut eng,
            "drop operator class if exists nosuch_ops using no_such_am"
        ),
        "42704"
    );
}

#[test]
fn parser_gaps_dummy_fn_and_triple_eq_operator() {
    let mut eng = engine();
    // Base-binary probe (a): a QUOTED language name failed 42601
    // ("expected identifier, found Str") — PG accepts an Sconst.
    ok(
        &mut eng,
        "create or replace function dummy_hashint4(a int4, seed int8) returns int8 as $$ begin return (a + seed); end; $$ language 'plpgsql' immutable",
    );
    // Base-binary probe (b): `===` in DDL name positions failed
    // 42601 ("expected '(', found Eq") — the lexer emits one Eq
    // token per `=` and the name parser consumed only one.
    ok(
        &mut eng,
        "create function myeq(int4, int4) returns boolean as $$ select $1 = $2 $$ language sql immutable",
    );
    ok(
        &mut eng,
        "create operator === (leftarg = int4, rightarg = int4, procedure = myeq, commutator = ===, hashes)",
    );
    ok(&mut eng, "drop operator ===(int4, int4)");
}

#[test]
fn int_array_key_hash_pph_vectors() {
    // partition_prune's pph_arrpart (int[] key, modulus 2):
    // {1} -> remainder 1 (pph_arrpart2), {1,2} -> 0, {4,5} -> 0.
    let eng = Engine::new();
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
    let cols = vec![("a".to_string(), ColType::Array(ArrayElem::Int))];
    let pinfo = PartitionInfo {
        method: crate::storage::PartMethod::Hash,
        key: vec![PartKey {
            col: 0,
            expr: None,
            opclass: None,
        }],
        bound: None,
        is_default: false,
        parent: None,
        children: vec![],
        is_partitioned: true,
    };
    let mut eng2 = eng;
    let arr = |xs: &[i32]| {
        Value::Array(Box::new(crate::storage::ArrayVal {
            elem: ArrayElem::Int,
            dims: vec![xs.len() as i32],
            lower: vec![1],
            elems: xs.iter().map(|x| Value::Int(*x as i64)).collect(),
        }))
    };
    for (xs, want) in [(&[1][..], 1u64), (&[1, 2][..], 0), (&[4, 5][..], 0)] {
        let h =
            super::partition::partition_row_hash(&mut eng2, &mut ctx, &pinfo, &cols, &[arr(xs)])
                .expect("array row hash");
        assert_eq!(h % 2, want, "array {xs:?}");
    }
}
