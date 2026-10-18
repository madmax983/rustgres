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
    let eff_def = create_table_from_def(eng, ctx, name, def, temp)?;
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
        for d in eff_def.defaults.iter().flatten() {
            if let crate::sql::DefaultExpr::Expr(e) = d {
                check_expr_cast_usages(eng, ctx, e)?;
            }
        }
        if let Some(p) = &eff_def.partition {
            for k in &p.keys {
                if let crate::sql::PartitionKeyDef::Expr(e) = k {
                    check_expr_cast_usages(eng, ctx, e)?;
                }
            }
        }
    }
    // v1.96: ALTER DEFAULT PRIVILEGES for the creating role.
    if !temp {
        let defaults = default_acl_entries_for(&eng.db, ctx.role, 'r', name);
        if !defaults.is_empty() {
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
        }
    }
    // Backing unique indexes for PRIMARY KEY / UNIQUE constraints. The
    // table is empty, so no duplicate check is needed. (v0.22: temp
    // tables get none — the global index map cannot represent them.)
    if !temp {
        if let Some(pk) = &eff_def.pkey {
            create_constraint_index(eng, ctx, name, &pk.name, &pk.cols, true)?;
        }
        for u in &eff_def.uniques {
            create_constraint_index(eng, ctx, name, &u.name, &u.cols, true)?;
        }
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
