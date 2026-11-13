// v1.78 mechanical split: moved verbatim from src/exec.rs (841-969).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

pub(crate) fn exec_create(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    def: &TableDef,
    temp: bool,
) -> Result<ExecResult, ExecError> {
    // v2.14 PG19: CREATE TABLE ... PARTITION OF on a relation under
    // an active DML statement in this session is an error (the
    // tab_part_create case: the partition is created from a
    // statement trigger body of an INSERT into the parent).
    if let Some(parent) = def.partition.as_ref().and_then(|p| p.parent.as_ref()) {
        if crate::exec::partition::active_dml_contains(parent) {
            return Err(exec_err(
                "55000",
                format!(
                    "cannot CREATE TABLE .. PARTITION OF \\\"{}\\\" because it is being used by active queries in this session",
                    parent
                ),
            ));
        }
    }
    // v2.03: generation expressions are validated at CREATE (PG19
    // cookDefault): immutable, own non-generated columns only.
    // v2.16: with INHERITS, PG validates against the MERGED column
    // set — a child's own generation expression may reference
    // inherited columns (the gtest1_y conflict-resolution form).
    if def.inherits.is_empty() {
        validate_generated_columns(eng, def)?;
    } else {
        let mut ext = def.clone();
        for parent_name in &def.inherits {
            if let Some(parent) =
                eng.db
                    .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
            {
                for (i, (cn, ct)) in parent.columns.iter().enumerate() {
                    if !ext.columns.iter().any(|(n, _)| n.eq_ignore_ascii_case(cn)) {
                        ext.columns.push((cn.clone(), ct.clone()));
                        ext.gen_cols
                            .push(parent.gen_cols.get(i).cloned().unwrap_or(None));
                    }
                }
            }
        }
        validate_generated_columns(eng, &ext)?;
    }
    // v1.96 finishing pass: PG19 schema guards for CREATE TABLE:
    // system catalog tables need allow_system_table_mods, and a
    // TEMP table cannot be explicitly placed in a non-temp schema.
    if let Some((schema, rest)) = name.split_once('.') {
        if schema == "pg_catalog" {
            return Err(exec_err(
                "42501",
                format!("permission denied to create \"pg_catalog.{}\"", rest),
            ));
        }
        if temp && schema != "pg_temp" {
            return Err(exec_err(
                "42P16",
                "cannot create temporary relation in non-temporary schema".to_string(),
            ));
        }
        // v2.16: PG19 namespace.c — an UNLOGGED relation in a
        // temporary schema is rejected (42P16 "only temporary
        // relations may be created in temporary schemas"): its
        // persistence is neither temp nor permanent-in-place.
        // (A plain CREATE TABLE pg_temp.x is implicitly temp in
        // PG and passes in this engine as a literal-named table.)
        if !temp && schema == "pg_temp" && def.unlogged {
            return Err(exec_err(
                "42P16",
                "only temporary relations may be created in temporary schemas".to_string(),
            ));
        }
    }
    // v2.16: PG19 DefineRelation — partitioned tables cannot be
    // unlogged (0A000).
    if def.unlogged && def.partition.as_ref().is_some_and(|p| p.parent.is_none()) {
        return Err(exec_err(
            "0A000",
            "partitioned tables cannot be unlogged".to_string(),
        ));
    }
    // v1.96: typed tables (`CREATE TABLE name OF type`) — expand the
    // composite type's fields into the column list (USAGE on the
    // type is required, PG19 aclchk).
    let was_typed_table = def.of_type.is_some();
    let typed_of = def.of_type.clone();
    let expanded;
    let def = if let Some(tname) = &def.of_type {
        check_type_usage(eng, ctx, tname)?;
        let st = match eng.db.types.get(tname) {
            Some(st) => st,
            None => {
                return Err(exec_err(
                    "42704",
                    format!("type \"{}\" does not exist", tname),
                ));
            }
        };
        let fields = match &st.composite {
            Some(f) => f.clone(),
            None => {
                return Err(exec_err(
                    "42809",
                    format!("type \"{}\" is not a composite type", tname),
                ));
            }
        };
        // v2.28: PG19 MergeAttributes validation of the OF element
        // list (typed_table personsx/persons4, generated_stored
        // gtest28): an element must name a field of the type (42703),
        // may name it only once (42701), and may not carry a
        // generation expression (0A000 — generated columns are not
        // supported on typed tables). Identity elements are already
        // rejected at parse; the arm is kept for totality. Before
        // this, the list was consumed at parse and discarded, so
        // these statements silently SUCCEEDED.
        {
            let mut seen = std::collections::HashSet::new();
            for it in &def.typed_elements {
                if let crate::sql::TableItem::Col(c) = it {
                    if !seen.insert(c.name.as_str()) {
                        return Err(exec_err(
                            "42701",
                            format!("column \"{}\" specified more than once", c.name),
                        ));
                    }
                    if !fields.iter().any(|(fname, _, _)| fname == &c.name) {
                        return Err(exec_err(
                            "42703",
                            format!("column \"{}\" does not exist", c.name),
                        ));
                    }
                    for k in &c.cons {
                        match k {
                            crate::sql::ColCon::Generated { .. } => {
                                return Err(exec_err(
                                    "0A000",
                                    "generated columns are not supported on typed tables"
                                        .to_string(),
                                ));
                            }
                            crate::sql::ColCon::Identity { .. } => {
                                return Err(exec_err(
                                    "0A000",
                                    "identity columns are not supported on typed tables"
                                        .to_string(),
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        let mut d = def.clone();
        d.of_type = None;
        for (fname, fty, named) in &fields {
            d.columns.push((fname.clone(), *fty));
            let mut comp = None;
            let mut dom = None;
            if let Some(n) = named {
                match eng.db.types.get(n) {
                    Some(s2) if s2.domain.is_some() => dom = Some(n.clone()),
                    _ => comp = Some(n.clone()),
                }
            }
            d.composite_types.push(comp);
            d.domain_types.push(dom);
            d.bit_typmods.push(None);
            d.domain_elem.push(false);
            d.not_null.push(false);
            d.defaults.push(None);
            d.serial.push(None);
            d.compression.push(None);
            d.storage.push(None);
        }
        expanded = d;
        &expanded
    } else {
        def
    };
    // v0.70: use the effective definition (PARTITION OF inherits the
    // parent's constraints into the cloned def) for the backing indexes,
    // so inherited PRIMARY KEY / UNIQUE constraints get their indexes.
    // v2.16: PG19 AddRelationNewConstraints — a local CHECK whose
    // name matches an INHERITED constraint: identical definitions
    // merge, except a NO INHERIT local def is 42P17; a different
    // definition is 42710.
    if !def.inherits.is_empty() {
        for lc in def.checks.iter().filter(|c| !c.name.is_empty()) {
            for parent_name in &def.inherits {
                let Some(parent) =
                    eng.db
                        .find_table(parent_name, ctx.snap, &ctx.all_xids, ctx.session)
                else {
                    continue;
                };
                for pc in parent.checks.iter().filter(|c| c.name == lc.name) {
                    if format!("{:?}", pc.expr) == format!("{:?}", lc.expr) {
                        if lc.no_inherit {
                            return Err(exec_err(
                                "42P17",
                                format!(
                                    "constraint \"{}\" conflicts with inherited constraint on relation \"{}\"",
                                    lc.name, name
                                ),
                            ));
                        }
                    } else {
                        return Err(exec_err(
                            "42710",
                            format!(
                                "constraint \"{}\" for relation \"{}\" already exists",
                                lc.name, name
                            ),
                        ));
                    }
                }
            }
        }
    }
    let eff_def = create_table_from_def(eng, ctx, name, def, temp)?;
    // v2.16: PG19 StoreRelCheck — NO INHERIT constraints cannot
    // land on a partitioned table (42P16), whether written inline
    // or copied in via LIKE. Checked on the created table (the
    // statement abort rolls the creation back).
    if let Some(created) = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
    {
        if created.partition.as_ref().is_some_and(|p| p.is_partitioned)
            && created.checks.iter().any(|c| c.no_inherit)
        {
            return Err(exec_err(
                "42P16",
                format!(
                    "cannot add NO INHERIT constraint to partitioned table \"{}\"",
                    name
                ),
            ));
        }
    }
    if let Some(tname) = &typed_of {
        eng.db.typed_tables.insert(name.to_string(), tname.clone());
    }
    // v1.96: PG19 type-USAGE checks (aclchk at parse time): column
    // types (incl. LIKE-copied ones), CHECK/DEFAULT cast targets and
    // PARTITION BY expression cast targets all need USAGE on the
    // named type/domain for the creating role. An error here aborts
    // the statement, rolling the just-created table back.
    if !was_typed_table {
        for n in eff_def
            .composite_types
            .iter()
            .chain(eff_def.domain_types.iter())
            .flatten()
        {
            check_type_usage(eng, ctx, n)?;
        }
        for c in &eff_def.checks {
            check_expr_cast_usages(eng, ctx, &c.expr)?;
        }
        // v2.10: CHECK column references are validated against the
        // MERGED column list here (the parser defers the check when
        // INHERITS is present, since parent columns are only known
        // at exec; PG validates against the merged list at CREATE).
        for c in &eff_def.checks {
            let mut refs = Vec::new();
            collect_col_refs(&c.expr, &mut refs);
            for (qual, r) in refs {
                if qual.is_none() && !eff_def.columns.iter().any(|(n, _)| n == &r) {
                    return Err(exec_err(
                        "42703",
                        format!("column \"{}\" does not exist", r),
                    ));
                }
            }
        }
        for d in eff_def.defaults.iter().flatten() {
            if let crate::sql::DefaultExpr::Expr(e) = d {
                check_expr_cast_usages(eng, ctx, e)?;
                // v2.15: PG19's dependency recording for stored
                // expressions rejects constants of type regrole /
                // regdatabase (dependency.c find_expr_references,
                // 0A000) — e.g. DEFAULT 'x'::regrole.
                let mut bad: Option<String> = None;
                crate::exec::eval::walk_expr(e, &mut |x| {
                    if let crate::sql::Expr::CastNamed { expr, name } = x {
                        let n = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
                        if (n == "regrole" || n == "regdatabase")
                            && matches!(expr.as_ref(), crate::sql::Expr::Literal(_))
                        {
                            bad = Some(n);
                        }
                    }
                });
                if let Some(n) = bad {
                    return Err(exec_err(
                        "0A000",
                        format!("constant of the type {} cannot be used here", n),
                    ));
                }
            }
        }
        if let Some(p) = &eff_def.partition {
            for k in &p.keys {
                if let crate::sql::PartitionKeyDef::Expr(e, _) = k {
                    check_expr_cast_usages(eng, ctx, e)?;
                }
            }
        }
    }
    // v1.96: ALTER DEFAULT PRIVILEGES for the creating role.
    if !temp {
        apply_default_acl_for_new_table(eng, ctx, name)?;
    }
    // Backing unique indexes for PRIMARY KEY / UNIQUE constraints. The
    // table is empty, so no duplicate check is needed. (v0.22: temp
    // tables get none — the global index map cannot represent them.)
    if !temp {
        if let Some(pk) = &eff_def.pkey {
            create_constraint_index(eng, ctx, name, &pk.name, &pk.cols, true, false)?;
        }
        for u in &eff_def.uniques {
            create_constraint_index(eng, ctx, name, &u.name, &u.cols, true, u.nulls_not_distinct)?;
        }
    }
    // v2.16: record UNLOGGED creations for the SET LOGGED /
    // SET UNLOGGED prep checks.
    if def.unlogged && !temp {
        eng.db.unlogged_tables.insert(name.to_string());
    }
    Ok(ExecResult::Command {
        tag: "CREATE TABLE".to_string(),
    })
}

/// v0.48: the table-creation core shared by CREATE TABLE and
/// CREATE TABLE AS (PG19 heap_create / create_ctas_internal): temp
/// tables land in the session-local map, permanent ones in the
/// catalog, TOAST backing is created eagerly. FK validation runs here
/// (a CTAS definition never carries constraints, so it is a no-op
/// there); backing unique indexes stay with plain CREATE TABLE above.
/// v0.65: create the backing sequence for a serial column, PG-style
/// (`<table>_<column>_seq`, suffixed on collision like ChooseRelationName).
/// Returns the sequence name. PG19 sequences are NO CYCLE, START 1,
/// MINVALUE 1, and MAXVALUE follows the sequence data type (PG19
/// sequence.c init_params, via the AS clause generateSerialExtraStmts
/// injects): smallserial -> 32767, serial -> 2147483647,
/// bigserial -> 9223372036854775807. The create is a transactional
/// write op, so ROLLBACK drops the sequence with the table.
/// v1.99: ALTER DEFAULT PRIVILEGES for the creating role,
/// materialized into a freshly created table's ACL (PG19 applies
/// the default ACL at relation creation). Shared by CREATE TABLE
/// (v1.96) and CREATE TABLE AS — the CTAS path skipped it, so a
/// default-privileges REVOKE (e.g. of the owner's INSERT) was not
/// honored for CTAS-created tables.
pub(crate) fn apply_default_acl_for_new_table(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
) -> Result<(), ExecError> {
    let defaults = default_acl_entries_for(&eng.db, ctx.role, 'r', name);
    if defaults.is_empty() {
        return Ok(());
    }
    if let Some(t) = eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .cloned()
    {
        let mut next = t;
        for e in defaults {
            let g = if e.grantor.is_empty() {
                ctx.role.to_string()
            } else {
                e.grantor.clone()
            };
            let _ = apply_acl_delta(
                &mut next.acl,
                &[e.role.clone()],
                &[(g, e.privs)],
                false,
                false,
                e.grant_option != 0,
                false,
                false,
                &ctx.role.to_string(),
            );
        }
        alter_swap(eng, ctx, name, None, next, None)?;
    }
    Ok(())
}

pub(crate) fn create_serial_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    col: &str,
    kind: SerialKind,
    temp_session: Option<u64>,
) -> Result<String, ExecError> {
    let base = format!("{}_{}_seq", table, col);
    let mut name = base.clone();
    let mut n = 1u32;
    while eng.db.find_sequence(&name, ctx.snap, ctx.own).is_some()
        || eng
            .db
            .find_table(&name, ctx.snap, &ctx.all_xids, ctx.session)
            .is_some()
        || eng.db.find_view(&name, ctx.snap, ctx.own).is_some()
    {
        name = format!("{}{}", base, n);
        n += 1;
    }
    let mut seq = Sequence::new(
        name.clone(),
        // v0.99: serial kinds are typed sequences (PG19).
        match kind {
            SerialKind::SmallSerial => crate::sql::SeqType::SmallInt,
            SerialKind::Serial => crate::sql::SeqType::Integer,
            SerialKind::BigSerial => crate::sql::SeqType::BigInt,
        },
        1, // start
        1, // increment
        1, // min_value
        match kind {
            SerialKind::SmallSerial => i64::from(i16::MAX),
            SerialKind::Serial => i64::from(i32::MAX),
            SerialKind::BigSerial => i64::MAX,
        },
        false, // cycle
        1,     // v0.98: cache (Postgres default)
        ctx.own,
    );
    // v0.65: explicit serial ownership (PG's DEPENDENCY_AUTO via OWNED
    // BY). DROP TABLE consults this, never the nextval default text.
    // temp_session isolates temp-table sequences by session.
    seq.owned_by = Some((table.to_string(), col.to_string(), temp_session));
    // v0.11: the creating role owns the sequence.
    seq.owner = ctx.role.to_string();
    eng.db.sequences.entry(name.clone()).or_default().push(seq);
    ctx.writes
        .push(WriteOp::CreateSequence { name: name.clone() });
    note_obj_schema(eng, ctx, &name, &name);
    Ok(name)
}

/// v2.28: create the backing sequence for an identity column
/// (PG19 generateSerialExtraStmts with for_identity): name
/// `<table>_<column>_seq` with ChooseRelationName suffixing, typed by
/// the column type, parameters from the identity sequence options via
/// `sequence_params`. Owned by the column like a serial sequence, so
/// DROP TABLE / DROP IDENTITY cleanup rides `owned_seqs_of`.
pub(crate) fn create_identity_sequence(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    col: &str,
    col_type: ColType,
    opts: &crate::sql::SequenceOpts,
    temp_session: Option<u64>,
) -> Result<String, ExecError> {
    let base = format!("{}_{}_seq", table, col);
    let mut name = base.clone();
    let mut n = 1u32;
    while eng.db.find_sequence(&name, ctx.snap, ctx.own).is_some()
        || eng
            .db
            .find_table(&name, ctx.snap, &ctx.all_xids, ctx.session)
            .is_some()
        || eng.db.find_view(&name, ctx.snap, ctx.own).is_some()
    {
        name = format!("{}{}", base, n);
        n += 1;
    }
    let mut o = opts.clone();
    o.seq_type = Some(match col_type {
        ColType::SmallInt => crate::sql::SeqType::SmallInt,
        ColType::BigInt => crate::sql::SeqType::BigInt,
        _ => crate::sql::SeqType::Integer,
    });
    let (seq_type, start, increment, min_value, max_value, cycle, cache, _) =
        sequence_params(&o, None)?;
    let mut seq = Sequence::new(
        name.clone(),
        seq_type,
        start,
        increment,
        min_value,
        max_value,
        cycle,
        cache,
        ctx.own,
    );
    seq.owned_by = Some((table.to_string(), col.to_string(), temp_session));
    seq.owner = ctx.role.to_string();
    eng.db.sequences.entry(name.clone()).or_default().push(seq);
    ctx.writes
        .push(WriteOp::CreateSequence { name: name.clone() });
    note_obj_schema(eng, ctx, &name, &name);
    Ok(name)
}

/// v2.28: wire identity defaults into a freshly built table: for
/// every identity column whose def carries sequence options (a
/// locally declared identity — partition children share the parent's
/// sequence and carry `identity_opts == None`, so they are skipped),
/// create the backing sequence and set `DEFAULT nextval('<seq>')`.
pub(crate) fn wire_identity_defaults(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    t: &mut Table,
    def: &TableDef,
    temp_session: Option<u64>,
) -> Result<(), ExecError> {
    for i in 0..t.columns.len() {
        if def.identity.get(i).and_then(|k| *k).is_none() {
            continue;
        }
        let Some(opts) = def.identity_opts.get(i).and_then(|o| o.clone()) else {
            continue;
        };
        let (cname, ctype) = t.columns[i].clone();
        let seq_name =
            create_identity_sequence(eng, ctx, table, &cname, ctype, &opts, temp_session)?;
        t.defaults[i] = Some(DefaultExpr::Nextval(seq_name));
    }
    Ok(())
}

/// v0.65: wire serial defaults into a freshly built table: for every
/// serial column, set `DEFAULT nextval('<seq>')`. An explicit DEFAULT
/// on a serial column is PG19's 42601 ("multiple default values
/// specified", parse_utilcmd.c transformColumnDefinition) — checked up
/// front so no sequence is created before the error.
pub(crate) fn wire_serial_defaults(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    table: &str,
    t: &mut Table,
    serial: &[Option<SerialKind>],
    temp_session: Option<u64>,
) -> Result<(), ExecError> {
    for (i, kind) in serial.iter().enumerate() {
        if kind.is_some() && t.defaults[i].is_some() {
            return Err(exec_err(
                "42601",
                format!(
                    "multiple default values specified for column \"{}\" of table \"{}\"",
                    t.columns[i].0, table
                ),
            ));
        }
    }
    for (i, kind) in serial.iter().enumerate() {
        let Some(kind) = kind else { continue };
        let cname = t.columns[i].0.clone();
        let seq_name = create_serial_sequence(eng, ctx, table, &cname, *kind, temp_session)?;
        if t.defaults[i].is_none() {
            t.defaults[i] = Some(DefaultExpr::Nextval(seq_name));
        }
    }
    Ok(())
}
