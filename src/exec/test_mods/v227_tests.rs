/// v2.27: the constraint-locality model (PG19 conislocal /
/// coninhcount / connoinherit / conbin, attislocal / attinhcount).
/// Locality is one stored bit per constraint (plus Table::nn_local /
/// Table::att_local); counts are derived from the live parent links.
/// Ground truth: PG19 tablecmds.c dropconstraint_internal /
/// RemoveInheritance / ATExecSetNotNull / ATExecDropNotNull, heap.c
/// MergeWithExistingConstraint, and the vendored expected files
/// (inherit.out, alter_table.out, create_table.out). Scope record:
/// hidden_files/v227/scope.md.
use super::*;
use crate::sql::parse_statement;

fn engine() -> Engine {
    Engine::new()
}

fn run_as(eng: &mut Engine, sql: &str, role: &str) -> Result<ExecResult, ExecError> {
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
    run_as(eng, sql, "postgres")
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

/// conislocal/coninhcount pairs for one table's pg_constraint rows,
/// as "name|t|1" strings sorted by name.
fn locality(eng: &mut Engine, table: &str) -> Vec<String> {
    rows_text(
        eng,
        &format!(
            "select conname, conislocal, coninhcount from pg_constraint \
             where conrelid = '{table}'::regclass order by conname"
        ),
    )
    .iter()
    .map(|r| r.join("|"))
    .collect()
}

#[test]
fn codec_roundtrip_locality_bits() {
    // A non-local check + non-local not-null + non-local column must
    // survive encode_constraints/decode_constraints; a pre-v2.27
    // encoding (no trailing sections) decodes as all-local.
    let mut t = Table::new(vec![("a".to_string(), ColType::Int)], 1);
    t.not_null = vec![true];
    t.nn_local = vec![false];
    t.att_local = vec![false];
    t.checks = vec![CheckDef {
        name: "c1".to_string(),
        expr: crate::sql::Expr::Cmp {
            op: crate::sql::CmpOp::Gt,
            left: Box::new(crate::sql::Expr::Column {
                table: None,
                name: "a".to_string(),
            }),
            right: Box::new(crate::sql::Expr::Literal(crate::sql::Literal::Int(0))),
        },
        not_valid: false,
        kind: crate::sql::CheckKind::Check,
        no_inherit: false,
        enforced: true,
        is_local: false,
    }];
    let enc = crate::sql::encode_constraints(&t);
    let dc = crate::sql::decode_constraints(&enc).expect("decode");
    assert_eq!(dc.checks.len(), 1);
    assert!(!dc.checks[0].is_local, "check locality lost: {enc}");
    assert_eq!(dc.nn_local, vec![false]);
    assert_eq!(dc.att_local, vec![false]);
    // All-local tables encode without the trailing sections and
    // decode back to all-local.
    let mut t2 = Table::new(vec![("a".to_string(), ColType::Int)], 1);
    t2.not_null = vec![true];
    let enc2 = crate::sql::encode_constraints(&t2);
    assert!(!enc2.contains("attlocal"), "{enc2}");
    let dc2 = crate::sql::decode_constraints(&enc2).expect("decode2");
    assert_eq!(dc2.att_local, vec![true]);
    assert_eq!(dc2.nn_local, vec![true]);
}

#[test]
fn inherits_create_locality() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table p (a int constraint pc check (a > 0), b int not null)",
    );
    ok(&mut eng, "create table c (b int not null) inherits (p)");
    // Child: pc is inherited (f/1); b's not-null merged local (t/1);
    // a's not-null is not present (p.b only). p.b not null lands on c
    // through column b merge: the named row is the synthesized one.
    let loc = locality(&mut eng, "c");
    assert!(loc.contains(&"pc|f|1".to_string()), "{loc:?}");
    assert!(loc.contains(&"c_b_not_null|t|1".to_string()), "{loc:?}");
    let ploc = locality(&mut eng, "p");
    assert!(ploc.contains(&"pc|t|0".to_string()), "{ploc:?}");
    assert!(ploc.contains(&"p_b_not_null|t|0".to_string()), "{ploc:?}");
}

#[test]
fn multi_parent_check_merges_to_one_row() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table p1 (f1 int constraint fpos check (f1 > 0))",
    );
    ok(
        &mut eng,
        "create table p2 (f1 int constraint fpos check (f1 > 0))",
    );
    ok(
        &mut eng,
        "create table c (f1 int constraint fpos check (f1 > 0)) inherits (p1, p2)",
    );
    // One row, merged local, inhcount 2 (PG f1_pos family).
    assert_eq!(locality(&mut eng, "c"), vec!["fpos|t|2".to_string()]);
    // Direct drop still refused (inhcount > 0 regardless of locality).
    let e = err(&mut eng, "alter table c drop constraint fpos");
    assert_eq!(e.code, "42P16");
    // Dropping one parent leaves the local copy standing.
    ok(&mut eng, "alter table p1 drop constraint fpos");
    assert_eq!(locality(&mut eng, "c"), vec!["fpos|t|1".to_string()]);
    ok(&mut eng, "alter table p2 drop constraint fpos");
    assert_eq!(locality(&mut eng, "c"), vec!["fpos|t|0".to_string()]);
    ok(&mut eng, "alter table c drop constraint fpos");
    assert!(locality(&mut eng, "c").is_empty());
}

#[test]
fn inherits_drop_guard_and_parent_drop() {
    let mut eng = engine();
    ok(&mut eng, "create table ac (aa text)");
    ok(
        &mut eng,
        "alter table ac add constraint ac_check check (aa is not null)",
    );
    ok(&mut eng, "create table bc () inherits (ac)");
    // The ADD on the parent happened before CREATE; the child copy
    // is inherited.
    assert_eq!(locality(&mut eng, "bc"), vec!["ac_check|f|1".to_string()]);
    let e = err(&mut eng, "alter table bc drop constraint ac_check");
    assert_eq!(e.code, "42P16");
    assert!(e.message.contains("cannot drop inherited constraint"));
    // Parent drop removes the purely inherited copy.
    ok(&mut eng, "alter table ac drop constraint ac_check");
    assert!(locality(&mut eng, "bc").is_empty());
}

#[test]
fn add_constraint_propagates_and_only_drop_localizes() {
    let mut eng = engine();
    ok(&mut eng, "create table p (a int)");
    ok(&mut eng, "create table c () inherits (p)");
    ok(&mut eng, "alter table p add constraint cc1 check (a > 0)");
    assert_eq!(locality(&mut eng, "c"), vec!["cc1|f|1".to_string()]);
    // ONLY drop on the parent: child copy survives, becomes local.
    ok(&mut eng, "alter table only p drop constraint cc1");
    assert_eq!(locality(&mut eng, "c"), vec!["cc1|t|0".to_string()]);
    // And is now droppable on the child.
    ok(&mut eng, "alter table c drop constraint cc1");
    assert!(locality(&mut eng, "c").is_empty());
}

#[test]
fn child_readd_merges() {
    let mut eng = engine();
    ok(&mut eng, "create table p (a int)");
    ok(&mut eng, "create table c () inherits (p)");
    ok(&mut eng, "alter table p add constraint cc check (a > 0)");
    assert_eq!(locality(&mut eng, "c"), vec!["cc|f|1".to_string()]);
    // Restating the identical definition on the child merges: the
    // copy becomes local; a later parent drop leaves it standing.
    ok(&mut eng, "alter table c add constraint cc check (a > 0)");
    assert_eq!(locality(&mut eng, "c"), vec!["cc|t|1".to_string()]);
    ok(&mut eng, "alter table p drop constraint cc");
    assert_eq!(locality(&mut eng, "c"), vec!["cc|t|0".to_string()]);
}

#[test]
fn grandchild_chain_drop() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table g1 (a int constraint gc check (a > 0))",
    );
    ok(&mut eng, "create table g2 () inherits (g1)");
    ok(&mut eng, "create table g3 () inherits (g2)");
    assert_eq!(locality(&mut eng, "g3"), vec!["gc|f|1".to_string()]);
    ok(&mut eng, "alter table g1 drop constraint gc");
    assert!(locality(&mut eng, "g2").is_empty());
    assert!(locality(&mut eng, "g3").is_empty());
}

#[test]
fn set_drop_not_null_recursion() {
    let mut eng = engine();
    ok(&mut eng, "create table p (a int not null)");
    ok(&mut eng, "create table c () inherits (p)");
    assert_eq!(
        locality(&mut eng, "c"),
        vec!["c_a_not_null|f|1".to_string()]
    );
    // Direct DROP NOT NULL on the child is refused (inherited).
    let e = err(&mut eng, "alter table c alter column a drop not null");
    assert_eq!(e.code, "42P16");
    // Parent drop clears the purely inherited copy.
    ok(&mut eng, "alter table p alter column a drop not null");
    assert!(locality(&mut eng, "c").is_empty());
    // SET NOT NULL on the parent propagates as inherited.
    ok(&mut eng, "alter table p alter column a set not null");
    assert_eq!(
        locality(&mut eng, "c"),
        vec!["c_a_not_null|f|1".to_string()]
    );
    // Enforcement reaches the child.
    let e = err(&mut eng, "insert into c values (null)");
    assert_eq!(e.code, "23502");
}

#[test]
fn partition_of_locality() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table parted (a int, b int not null, constraint check_a check (a > 0)) partition by list (a)",
    );
    ok(
        &mut eng,
        "create table part_b partition of parted (b not null) for values in (2)",
    );
    // check_a inherited f/1; the element NOT NULL is local t/1.
    let loc = locality(&mut eng, "part_b");
    assert!(loc.contains(&"check_a|f|1".to_string()), "{loc:?}");
    assert!(
        loc.contains(&"part_b_b_not_null|t|1".to_string()),
        "{loc:?}"
    );
    // A later parent ADD merges into the partition copy, which
    // becomes non-local; the parent drop then removes it.
    ok(
        &mut eng,
        "alter table parted add constraint check_b check (b > 0)",
    );
    let loc = locality(&mut eng, "part_b");
    assert!(loc.contains(&"check_b|f|1".to_string()), "{loc:?}");
    ok(&mut eng, "alter table parted drop constraint check_b");
    let loc = locality(&mut eng, "part_b");
    assert!(!loc.iter().any(|r| r.starts_with("check_b|")), "{loc:?}");
    // The element not-null survives parent-side changes.
    assert!(
        loc.contains(&"part_b_b_not_null|t|1".to_string()),
        "{loc:?}"
    );
}

#[test]
fn partition_element_merge_forces_nonlocal() {
    // A PARTITION OF element constraint identical to a constraint the
    // parent gains LATER merges as non-local on the partition
    // (heap.c MergeWithExistingConstraint, relispartition case), so
    // the parent drop removes it — the create_table.out part_b
    // check_b sequence.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table parted (a text, b int not null default 0, constraint check_a check (length(a) > 0)) partition by list (a)",
    );
    ok(
        &mut eng,
        "create table part_b partition of parted (b not null default 1, constraint check_a check (length(a) > 0), constraint check_b check (b >= 0)) for values in ('b')",
    );
    let loc = locality(&mut eng, "part_b");
    assert!(loc.contains(&"check_b|t|0".to_string()), "{loc:?}");
    ok(
        &mut eng,
        "alter table parted add constraint check_b check (b >= 0)",
    );
    let loc = locality(&mut eng, "part_b");
    assert!(loc.contains(&"check_b|f|1".to_string()), "{loc:?}");
    let e = err(&mut eng, "alter table part_b drop constraint check_b");
    assert_eq!(e.code, "42P16");
    ok(&mut eng, "alter table parted drop constraint check_a");
    ok(&mut eng, "alter table parted drop constraint check_b");
    assert_eq!(
        locality(&mut eng, "part_b"),
        vec!["part_b_b_not_null|t|1".to_string()]
    );
}

#[test]
fn no_inherit_localizes() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table ip (a int constraint icheck check (a > 0))",
    );
    ok(&mut eng, "create table ic () inherits (ip)");
    assert_eq!(locality(&mut eng, "ic"), vec!["icheck|f|1".to_string()]);
    ok(&mut eng, "alter table ic no inherit ip");
    // PG19 RemoveInheritance: the copy survives, marked local, and
    // stays enforced (it does NOT vanish).
    assert_eq!(locality(&mut eng, "ic"), vec!["icheck|t|0".to_string()]);
    let e = err(&mut eng, "insert into ic values (-1)");
    assert_eq!(e.code, "23514");
    ok(&mut eng, "alter table ic drop constraint icheck");
    ok(&mut eng, "insert into ic values (-1)");
}

#[test]
fn add_column_inheritance_locality() {
    let mut eng = engine();
    ok(&mut eng, "create table depth0 ()");
    ok(&mut eng, "create table depth1 (c text) inherits (depth0)");
    ok(&mut eng, "create table depth2 () inherits (depth1)");
    ok(&mut eng, "alter table depth0 add column c text");
    // depth1 merges (stays local, inhcount 1); depth2 gains the
    // column as inherited (f/1) — alter_table.out's depth family.
    let rows = rows_text(
        &mut eng,
        "select attrelid::regclass, attname, attinhcount, attislocal from pg_attribute \
         where attnum > 0 and attname = 'c' order by attrelid::regclass::text",
    );
    let flat: Vec<String> = rows.iter().map(|r| r.join("|")).collect();
    assert!(flat.contains(&"depth0|c|0|t".to_string()), "{flat:?}");
    assert!(flat.contains(&"depth1|c|1|t".to_string()), "{flat:?}");
    assert!(flat.contains(&"depth2|c|1|f".to_string()), "{flat:?}");
}

#[test]
fn unique_autoname_all_columns() {
    let mut eng = engine();
    ok(
        &mut eng,
        "create table tc (id int, val1 varchar, val2 int, unique(val1, val2))",
    );
    let loc = locality(&mut eng, "tc");
    assert!(loc.contains(&"tc_val1_val2_key|t|0".to_string()), "{loc:?}");
    ok(
        &mut eng,
        "alter table only tc drop constraint tc_val1_val2_key",
    );
    assert!(locality(&mut eng, "tc").is_empty());
    // ALTER-added uniques get the same all-columns name.
    ok(&mut eng, "alter table tc add unique (val1, val2)");
    let loc = locality(&mut eng, "tc");
    assert!(loc.contains(&"tc_val1_val2_key|t|0".to_string()), "{loc:?}");
}

#[test]
fn parent_add_conflicts_with_child_not_valid() {
    // inherit.sql invalid_check_con family: the child holds the
    // constraint NOT VALID; a VALID parent add must abort with the
    // NOT VALID conflict, leaving the child's local copy droppable.
    let mut eng = engine();
    ok(&mut eng, "create table icp (f1 int)");
    ok(&mut eng, "create table icc () inherits (icp)");
    ok(
        &mut eng,
        "alter table icc add constraint cc1 check (f1 > 0) not valid",
    );
    let e = err(
        &mut eng,
        "alter table icp add constraint cc1 check (f1 > 0)",
    );
    assert_eq!(e.code, "42P16");
    assert!(e.message.contains("NOT VALID"), "{}", e.message);
    // NB: the parent's rolled-back version stays visible in this
    // harness (every statement shares xid 9, so nothing is ever
    // marked aborted, and the child's derived inhcount reads 1
    // against the leaked parent version); the live server aborts
    // the statement's xid, the parent gains nothing, and the
    // child's local copy drops cleanly — probe-verified against
    // the vendored inherit.sql sequence (hidden_files/v227). The
    // merge half therefore runs on a fresh engine.
    let mut eng = engine();
    ok(&mut eng, "create table icp (f1 int)");
    ok(&mut eng, "create table icc () inherits (icp)");
    // The NOT VALID parent add merges into a VALID child copy.
    ok(
        &mut eng,
        "alter table icc add constraint cc1 check (f1 > 0)",
    );
    ok(
        &mut eng,
        "alter table icp add constraint cc1 check (f1 > 0) not valid",
    );
    let loc = locality(&mut eng, "icc");
    assert!(loc.contains(&"cc1|t|1".to_string()), "{loc:?}");
}

#[test]
fn nn_add_store_rel_not_null_rules() {
    // PG19 StoreRelNotNull: one not-null per column, found by
    // column. Adopting the synthesized name materializes the
    // record; a different name is a 55000; a NO INHERIT status
    // mismatch is a 55000 naming the existing constraint.
    let mut eng = engine();
    ok(&mut eng, "create table nnt (a int not null)");
    ok(
        &mut eng,
        "alter table nnt add constraint nnt_a_not_null not null a",
    );
    let e = err(&mut eng, "alter table nnt add constraint nn not null a");
    assert_eq!(e.code, "55000");
    assert!(
        e.message.contains("cannot create not-null"),
        "{}",
        e.message
    );
    let loc = locality(&mut eng, "nnt");
    assert!(loc.contains(&"nnt_a_not_null|t|0".to_string()), "{loc:?}");
    // ADD CONSTRAINT NOT NULL lands the attnotnull flag too: a
    // child created afterwards inherits with inhcount 1 and its
    // DROP NOT NULL is refused as inherited.
    ok(&mut eng, "create table nnp (a int)");
    ok(&mut eng, "alter table nnp add constraint nn not null a");
    ok(&mut eng, "create table nnc () inherits (nnp)");
    let loc = locality(&mut eng, "nnc");
    assert!(loc.contains(&"nn|f|1".to_string()), "{loc:?}");
    let e = err(&mut eng, "alter table nnc alter column a drop not null");
    assert_eq!(e.code, "42P16");
    let e = err(&mut eng, "alter table nnc add not null a no inherit");
    assert_eq!(e.code, "55000");
    assert!(
        e.message.contains("cannot change NO INHERIT status"),
        "{}",
        e.message
    );
    // Same-name re-add merges and makes the copy local.
    ok(&mut eng, "alter table nnc add constraint nn not null a");
    let loc = locality(&mut eng, "nnc");
    assert!(loc.contains(&"nn|t|1".to_string()), "{loc:?}");
}

#[test]
fn drop_constraint_nn_on_pk_column_errors() {
    // alter_table.sql list_parted3: the not-null of a primary-key
    // column cannot be dropped by constraint name either.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table lp3 (a int not null) partition by list (a)",
    );
    ok(
        &mut eng,
        "create table lp3_1 partition of lp3 for values in (1)",
    );
    ok(&mut eng, "alter table only lp3 add primary key (a)");
    let e = err(
        &mut eng,
        "alter table only lp3 drop constraint lp3_a_not_null",
    );
    assert_eq!(e.code, "42P16");
    assert!(e.message.contains("primary key"), "{}", e.message);
    // The constraint survives.
    let loc = locality(&mut eng, "lp3");
    assert!(loc.contains(&"lp3_a_not_null|t|0".to_string()), "{loc:?}");
}

#[test]
fn named_not_null_at_create_materializes() {
    // PG19: an explicitly named NOT NULL in CREATE TABLE (column
    // or table level) is a pg_constraint record under that name,
    // droppable by name; an unnamed one stays flag-only.
    let mut eng = engine();
    ok(
        &mut eng,
        "create table cn1 (a int, constraint nn_c1 not null a)",
    );
    let loc = locality(&mut eng, "cn1");
    assert!(loc.contains(&"nn_c1|t|0".to_string()), "{loc:?}");
    ok(&mut eng, "alter table cn1 drop constraint nn_c1");
    assert!(locality(&mut eng, "cn1").is_empty());
    ok(
        &mut eng,
        "create table cn2 (a int constraint nn_c2 not null)",
    );
    let loc = locality(&mut eng, "cn2");
    assert!(loc.contains(&"nn_c2|t|0".to_string()), "{loc:?}");
    ok(&mut eng, "alter table cn2 drop constraint nn_c2");
    assert!(locality(&mut eng, "cn2").is_empty());
}

#[test]
fn inherit_not_valid_conflict_and_attach_merge() {
    // constraints.sql 953-965 + 981-992: a VALID parent not-null
    // cannot merge into a NOT VALID child copy at INHERIT time
    // (42P17); once the parent's copy is NOT VALID too, INHERIT
    // links and the child's record counts the parent. ATTACH with
    // both sides NOT VALID merges without complaint.
    let mut eng = engine();
    ok(&mut eng, "create table it1 (a int)");
    ok(
        &mut eng,
        "alter table it1 add constraint nn_parent not null a not valid",
    );
    ok(
        &mut eng,
        "create table ic0 (a int, constraint nn_ic0 not null a)",
    );
    let e = err(&mut eng, "alter table it1 inherit ic0");
    assert_eq!(e.code, "42P17");
    ok(&mut eng, "alter table ic0 drop constraint nn_ic0");
    ok(
        &mut eng,
        "alter table ic0 add constraint nn_ic0 not null a not valid",
    );
    ok(&mut eng, "alter table it1 inherit ic0");
    let loc = locality(&mut eng, "it1");
    assert!(loc.contains(&"nn_parent|t|1".to_string()), "{loc:?}");
    // ATTACH: parent NOT VALID + child NOT VALID merges.
    ok(
        &mut eng,
        "create table ap (a int, b int) partition by list (a)",
    );
    ok(
        &mut eng,
        "alter table ap add constraint ap_con not null a not valid",
    );
    ok(&mut eng, "create table ap_3 (a int, b int)");
    ok(
        &mut eng,
        "alter table ap_3 add constraint nn3 not null a not valid",
    );
    ok(
        &mut eng,
        "alter table ap attach partition ap_3 for values in (5)",
    );
    let loc = locality(&mut eng, "ap_3");
    assert!(loc.contains(&"nn3|f|1".to_string()), "{loc:?}");
}

#[test]
fn add_column_constraint_name_collision() {
    // constraints.sql: ADD COLUMN with a constraint name the
    // table already carries errors 42710, whatever the kind.
    let mut eng = engine();
    ok(&mut eng, "create table ac1 (a int not null)");
    ok(
        &mut eng,
        "alter table ac1 add constraint ac1_a_not_null not null a",
    );
    let e = err(
        &mut eng,
        "alter table ac1 add column b int constraint ac1_a_not_null not null",
    );
    assert_eq!(e.code, "42710");
    // A fresh name works.
    ok(
        &mut eng,
        "alter table ac1 add column b int constraint ac1_b_not_null not null",
    );
}
