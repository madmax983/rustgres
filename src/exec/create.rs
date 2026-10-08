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
    // v0.70: use the effective definition (PARTITION OF inherits the
    // parent's constraints into the cloned def) for the backing indexes,
    // so inherited PRIMARY KEY / UNIQUE constraints get their indexes.
    let eff_def = create_table_from_def(eng, ctx, name, def, temp)?;
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
