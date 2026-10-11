// v1.78 mechanical split: moved verbatim from src/wal.rs (3103-4752).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ---------------------------------------------------------------------------
// Applying records during recovery
// ---------------------------------------------------------------------------

/// Find the table version that a committed DML record targets: the live
/// (not dropped) version with `name`. Returns `None` when there is no
/// live version — possible if a concurrent transaction's DDL won a race
/// after the record's own commit validation (see `records_for_commit`);
/// the caller skips the record with a warning rather than failing
/// recovery over it.
pub(crate) fn live_table<'e>(eng: &'e mut Engine, name: &str) -> Option<&'e mut Table> {
    eng.db
        .tables
        .get_mut(name)
        .and_then(|vs| vs.iter_mut().find(|t| t.dropped_xmax == 0))
}

/// v1.80: adapt a record from an `RGSWAL20` log to current semantics.
/// Those logs never carried partition metadata, so an `AlterTable` on a
/// partitioned table (ATTACH, a link from PARTITION OF, any ALTER) decodes
/// with `partition: None`. Applied as-is it would de-partition the table.
/// Carry the live version's metadata forward instead. That metadata came
/// from the checkpoint image, or from an earlier record in the same log.
/// `CreateTable` cannot be repaired this way: v1.79 lost that metadata at
/// write time.
pub(crate) fn legacy_fill_partition(eng: &Engine, r: &WalRecord) -> Option<WalRecord> {
    let WalRecord::AlterTable {
        name,
        partition: None,
        ..
    } = r
    else {
        return None;
    };
    let live = eng
        .db
        .tables
        .get(name)?
        .iter()
        .find(|v| v.dropped_xmax == 0)?
        .partition
        .clone()?;
    let mut out = r.clone();
    if let WalRecord::AlterTable { partition, .. } = &mut out {
        *partition = Some(live);
    }
    Some(out)
}

/// Apply one record during recovery. Records replay in commit order, so
/// the version chains — and therefore visibility — are rebuilt exactly.
pub fn apply_record(eng: &mut Engine, r: &WalRecord) -> Result<(), String> {
    // Fold the record's xids / row ids into the counters first, so they
    // resume above every id the log ever used even when the record below
    // is skipped as a no-op.
    match r {
        WalRecord::CreateTable { xmin, .. }
        | WalRecord::DropTable { xmax: xmin, .. }
        | WalRecord::CreateIndex { xmin, .. }
        | WalRecord::DropIndex { xmax: xmin, .. }
        | WalRecord::AlterTable { xmin, .. }
        | WalRecord::CreateView { xmin, .. }
        | WalRecord::DropView { xmax: xmin, .. }
        | WalRecord::CreateSequence { xmin, .. }
        | WalRecord::DropSequence { xmax: xmin, .. }
        | WalRecord::AlterSequence { xmin, .. }
        // v1.05: the lazy reltoastrelid link carries its xid too.
        | WalRecord::SetToastRelid { xmin, .. } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
        }
        WalRecord::InsertRows { rows, .. } => {
            for row in rows {
                if row.xmin >= eng.txns.next_xid {
                    eng.txns.next_xid = row.xmin + 1;
                }
                if row.id >= eng.txns.next_row_id {
                    eng.txns.next_row_id = row.id + 1;
                }
            }
        }
        WalRecord::DeleteRows { xmax, .. } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
        }
        // v0.13: UPDATE carries old/new row ids — fold them into the
        // row-id counter so it resumes above every id the log used.
        WalRecord::UpdateRows {
            table: _,
            old,
            new,
            xmax,
        } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            for r in old.iter().chain(new.iter()) {
                if r.id >= eng.txns.next_row_id {
                    eng.txns.next_row_id = r.id + 1;
                }
            }
        }
        // v0.13: replication slots carry no xid (non-transactional).
        WalRecord::ReplSlotCreate { .. }
        | WalRecord::ReplSlotDrop { .. }
        | WalRecord::ReplSlotFlush { .. } => {}
        // v0.9: sequence advances carry no xid (non-transactional).
        WalRecord::SeqAdvance { .. } => {}
        // v0.11: roles.
        WalRecord::CreateRole { name, role, xmin } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            let r = crate::storage::Role {
                name: name.clone(),
                password: role.password.clone().map(WalVerifier::into_verifier),
                can_login: role.can_login,
                superuser: role.superuser,
                connlimit: role.connlimit,
                memberships: role
                    .memberships
                    .iter()
                    .map(|m| crate::storage::RoleMembership {
                        role: m.role.clone(),
                        grantor: m.grantor.clone(),
                    })
                    .collect(),
                valid_until: role.valid_until.clone(),
                created_xmin: *xmin,
                dropped_xmax: 0,
            };
            eng.db.roles.entry(name.clone()).or_default().push(r);
        }
        WalRecord::DropRole { name, xmax } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            match eng
                .db
                .roles
                .get_mut(name)
                .and_then(|vs| vs.iter_mut().find(|v| v.dropped_xmax == 0))
            {
                Some(r) => r.dropped_xmax = *xmax,
                None => eprintln!(
                    "WAL replay: skipping DropRole \"{}\": no live role version",
                    name
                ),
            }
        }
        WalRecord::AlterRole { name, role, xmin } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            let versions = eng.db.roles.entry(name.clone()).or_default();
            if let Some(prev) = versions.iter_mut().find(|v| v.dropped_xmax == 0) {
                prev.dropped_xmax = *xmin;
            }
            let r = crate::storage::Role {
                name: name.clone(),
                password: role.password.clone().map(WalVerifier::into_verifier),
                can_login: role.can_login,
                superuser: role.superuser,
                connlimit: role.connlimit,
                memberships: role
                    .memberships
                    .iter()
                    .map(|m| crate::storage::RoleMembership {
                        role: m.role.clone(),
                        grantor: m.grantor.clone(),
                    })
                    .collect(),
                valid_until: role.valid_until.clone(),
                created_xmin: *xmin,
                dropped_xmax: 0,
            };
            versions.push(r);
        }
        WalRecord::DbAcl { acl, xmin } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            eng.db.db_acl = acl.iter().cloned().map(WalAcl::into_entry).collect();
        }
        // v0.82: type DDL replay — rebuild the catalog entry exactly.
        WalRecord::CreateType {
            name,
            like_base,
            composite,
            domain,
            xmin,
        } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            // v0.85: restore the domain definition from the s-expr
            // strings. A corrupt record fails replay loudly rather than
            // silently dropping constraints.
            let domain = match domain {
                Some(d) => Some(crate::storage::DomainDef {
                    base: d.base.clone(),
                    base_named: d.base_named.clone(),
                    base_domain: d.base_domain.clone(),
                    checks: crate::sql::decode_domain_checks(&d.checks)
                        .map_err(|e| format!("corrupt domain checks in WAL: {}", e))?,
                    not_null: d.not_null,
                    default: crate::sql::decode_domain_default(&d.default)
                        .map_err(|e| format!("corrupt domain default in WAL: {}", e))?,
                }),
                None => None,
            };
            eng.db.types.insert(
                name.clone(),
                crate::storage::ShellType {
                    like_base: like_base.clone(),
                    composite: composite.clone(),
                    domain,
                },
            );
        }
        WalRecord::DropType { name, xmax } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            eng.db.types.remove(name);
        }
        // v0.86: function/operator DDL replay — rebuild the catalog
        // entries exactly. The body is re-parsed; a corrupt body fails
        // replay loudly.
        WalRecord::CreateFunction {
            name,
            arg_names,
            arg_types,
            ret_type,
            returns_set,
            lang,
            body,
            volatility,
            strict,
            xmin,
        } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            let lang = match lang {
                0 => crate::sql::FuncLang::Sql,
                1 => crate::sql::FuncLang::Internal,
                2 => crate::sql::FuncLang::Plpgsql,
                _ => return Err(format!("corrupt function language in WAL: {}", lang)),
            };
            let volatility = match volatility {
                0 => crate::sql::FuncVolatility::Volatile,
                1 => crate::sql::FuncVolatility::Stable,
                2 => crate::sql::FuncVolatility::Immutable,
                _ => {
                    return Err(format!(
                        "corrupt function volatility in WAL: {}",
                        volatility
                    ));
                }
            };
            let (parsed, plpgsql) =
                crate::exec::rebuild_function_bodies(lang, arg_names, body, *returns_set);
            // v0.87: replay appends to the overload list (replacing any
            // existing overload with the same signature).
            {
                let def = crate::storage::FuncDef {
                    name: name.clone(),
                    arg_names: arg_names.clone(),
                    arg_types: arg_types.clone(),
                    ret_type: ret_type.clone(),
                    returns_set: *returns_set,
                    lang,
                    body: body.clone(),
                    parsed,
                    // v1.01: multi-statement plpgsql bodies.
                    plpgsql,
                    volatility,
                    strict: *strict,
                };
                let overloads = eng.db.functions.entry(name.clone()).or_default();
                if let Some(slot) = overloads.iter_mut().find(|f| f.arg_types == def.arg_types) {
                    *slot = def;
                } else {
                    overloads.push(def);
                }
            }
        }
        WalRecord::DropFunction {
            name,
            arg_types,
            xmax,
        } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            // v0.87: remove the specific overload by signature.
            if let Some(overloads) = eng.db.functions.get_mut(name) {
                overloads.retain(|f| &f.arg_types != arg_types);
                if overloads.is_empty() {
                    eng.db.functions.remove(name);
                }
            }
        }
        WalRecord::CreateOperator { name, defs, xmin } => {
            if *xmin >= eng.txns.next_xid {
                eng.txns.next_xid = *xmin + 1;
            }
            eng.db.operators.insert(
                name.clone(),
                defs.iter()
                    .map(|d| crate::storage::OperDef {
                        name: name.clone(),
                        procedure: d.procedure.clone(),
                        leftarg: d.leftarg.clone(),
                        rightarg: d.rightarg.clone(),
                        commutator: d.commutator.clone(),
                        negator: d.negator.clone(),
                        hashes: d.hashes,
                        merges: d.merges,
                    })
                    .collect(),
            );
        }
        WalRecord::DropOperator { name, xmax } => {
            if *xmax >= eng.txns.next_xid {
                eng.txns.next_xid = *xmax + 1;
            }
            eng.db.operators.remove(name);
        }
    }
    match r {
        // v0.11: role records are applied by the match above; nothing left
        // to do here. v0.82: type records likewise.
        WalRecord::CreateRole { .. }
        | WalRecord::DropRole { .. }
        | WalRecord::AlterRole { .. }
        | WalRecord::DbAcl { .. }
        | WalRecord::CreateType { .. }
        | WalRecord::DropType { .. }
        // v0.86: function/operator records likewise.
        | WalRecord::CreateFunction { .. }
        | WalRecord::DropFunction { .. }
        | WalRecord::CreateOperator { .. }
        | WalRecord::DropOperator { .. } => {}
        // v0.13: replication slot records are applied here.
        WalRecord::ReplSlotCreate {
            name,
            plugin,
            slot_type,
            restart_lsn,
        } => {
            eng.repl_slots.insert(
                name.clone(),
                crate::storage::ReplSlot {
                    name: name.clone(),
                    plugin: plugin.clone(),
                    slot_type: slot_type.clone(),
                    restart_lsn: *restart_lsn,
                    confirmed_flush_lsn: *restart_lsn,
                    active: false,
                },
            );
        }
        WalRecord::ReplSlotDrop { name } => {
            eng.repl_slots.remove(name);
        }
        WalRecord::ReplSlotFlush {
            name,
            restart_lsn,
            confirmed_flush_lsn,
        } => {
            if let Some(s) = eng.repl_slots.get_mut(name) {
                s.confirmed_flush_lsn = s.confirmed_flush_lsn.max(*confirmed_flush_lsn);
                s.restart_lsn = s.restart_lsn.max(*restart_lsn);
            }
        }
        WalRecord::CreateTable {
            name,
            columns,
            constraints,
            owner,
            acl,
            col_acl,
            oid,
            toast_relid,
            col_compression,
            composite_types,
            domain_types,
            domain_elem,
            inherits,
            attnums,
            next_attnum,
            fillfactor,
            partition,
            xmin,
        } => {
            let mut t = Table::new(columns.clone(), *xmin);
            // v1.80: partition metadata (None on RGSWAL20 logs).
            t.partition = partition.clone();
            // v1.41: restore attnums, the next-attnum counter, and
            // fillfactor.
            t.attnums = attnums.clone();
            t.next_attnum = *next_attnum;
            t.fillfactor = *fillfactor;
            // v0.11
            t.owner = owner.clone();
            t.acl = acl.iter().cloned().map(WalAcl::into_entry).collect();
            t.col_acl = col_acl.iter().cloned().map(WalColAcl::into_entry).collect();
            // v0.37: restore the table OID and its toast table link.
            t.oid = *oid;
            t.toast_relid = *toast_relid;
            // v0.96: restore inheritance parent links.
            t.inherits = inherits.clone();
            // v0.85: restore composite/domain type use (empty vecs on old
            // records mean "no domain use").
            t.composite_types = composite_types.clone();
            t.domain_types = domain_types.clone();
            t.domain_elem = domain_elem.clone();
            // v0.41: restore per-column compression methods (0 = default).
            t.col_compression = col_compression
                .iter()
                .map(|&code| {
                    if code == 0 {
                        Ok(None)
                    } else {
                        crate::storage::ToastCompression::from_code(code)
                            .map(Some)
                            .ok_or_else(|| {
                                format!("WAL replay: unknown toast compression code {}", code)
                            })
                    }
                })
                .collect::<Result<Vec<_>, String>>()?;
            // v0.37: advance the OID counter past every restored OID, or
            // the next CREATE after replay would hand out a colliding OID.
            eng.db.next_oid = eng
                .db
                .next_oid
                .max(oid.wrapping_add(1))
                .max(toast_relid.wrapping_add(1));
            match crate::sql::decode_constraints(constraints) {
                Ok(dc) => {
                    t.not_null = dc.not_null;
                    t.defaults = dc.defaults;
                    t.checks = dc.checks;
                    t.uniques = dc.uniques;
                    t.pkey = dc.pkey;
                    t.fks = dc.fks;
                }
                Err(e) => {
                    return Err(format!(
                        "WAL replay: bad constraints for table \"{}\": {}",
                        name, e
                    ));
                }
            }
            eng.db.tables.entry(name.clone()).or_default().push(t);
        }
        WalRecord::InsertRows { table, rows } => {
            // Collect the actually-inserted (id, values) first: the index
            // maintenance below needs `eng` mutably, which conflicts with
            // the table borrow.
            let inserted: Vec<(u64, Row)> = {
                let Some(t) = live_table(eng, table) else {
                    eprintln!(
                        "WAL replay: skipping InsertRows for \"{}\": no live table version",
                        table
                    );
                    return Ok(());
                };
                let mut out = Vec::new();
                for row in rows {
                    // v1.81: O(1) via row_index — the old linear scan made
                    // crash recovery quadratic in table size, the same bug
                    // v0.22 fixed for DeleteRows/UpdateRows below but missed
                    // here (see benches/BASELINE.md, 2026-10-11).
                    if t.row_pos(row.id).is_some() {
                        eprintln!(
                            "WAL replay: skipping duplicate row id {} in table \"{}\"",
                            row.id, table
                        );
                        continue;
                    }
                    // v1.80: live execution never stores a row whose width
                    // differs from its version's columns. In replay such a
                    // row can only be a v1.79 mis-logged intermediate
                    // rewrite of a multi-action ALTER, whose AlterTable
                    // record carried the final shape. A later AlterTable in
                    // the same batch supersedes that version and re-inserts
                    // the rows at the right width, so skip it rather than
                    // build a version no checkpoint could load back.
                    if row.values.len() != t.columns.len() {
                        eprintln!(
                            "WAL replay: skipping row id {} in table \"{}\": {} values for {} columns",
                            row.id,
                            table,
                            row.values.len(),
                            t.columns.len()
                        );
                        continue;
                    }
                    // v0.39: restore the per-cell toast flags (they were
                    // WAL-logged per row) and rebuild the table's
                    // toast_info provenance from the record's metadata.
                    let mut rv = RowVersion::plain(row.id, row.values.clone(), row.xmin);
                    if row.toast.len() == row.values.len() {
                        rv.toast = row.toast.clone();
                    }
                    for (vid, compressed) in &row.toast_meta {
                        // v0.41: the byte is a method code (0 = plain).
                        let info = decode_toast_method(*compressed)?;
                        t.toast_info.insert(*vid, info);
                        if t.next_value_id <= *vid {
                            t.next_value_id = vid + 1;
                        }
                    }
                    t.push_version(rv);
                    out.push((row.id, row.values.clone()));
                }
                out
            };
            for (id, values) in &inserted {
                eng.db
                    .index_insert_row_replay(table, *id, values);
            }
        }
        WalRecord::DropTable { name, xmax } => match live_table(eng, name) {
            Some(t) => t.dropped_xmax = *xmax,
            None => eprintln!(
                "WAL replay: skipping DropTable \"{}\": no live table version",
                name
            ),
        },
        WalRecord::DeleteRows {
            table,
            ids,
            old_rows: _,
            xmax,
        } => {
            let Some(t) = live_table(eng, table) else {
                eprintln!(
                    "WAL replay: skipping DeleteRows for \"{}\": no live table version",
                    table
                );
                return Ok(());
            };
            for id in ids {
                // v0.22: O(1) via row_index — the old linear scan made
                // crash recovery quadratic in version count (a 10 MB WAL
                // replayed in ~53 s; now ~1 s).
                match t.row_pos(*id).and_then(|p| t.rows.get_mut(p)) {
                    Some(r) => r.xmax = *xmax,
                    None => eprintln!(
                        "WAL replay: skipping DELETE of missing row id {} in table \"{}\"",
                        id, table
                    ),
                }
            }
            // No index maintenance: the entries stay (visibility filters
            // them), exactly as in live execution.
        }
        // v0.13: UPDATE replays as delete-old + insert-new — the same
        // version-chain surgery live execution performs.
        WalRecord::UpdateRows {
            table,
            old,
            new,
            xmax,
        } => {
            let Some(t) = live_table(eng, table) else {
                eprintln!(
                    "WAL replay: skipping UpdateRows for \"{}\": no live table version",
                    table
                );
                return Ok(());
            };
            for r in old {
                // v0.22: O(1) via row_index — see DeleteRows above.
                match t.row_pos(r.id).and_then(|p| t.rows.get_mut(p)) {
                    Some(v) => v.xmax = *xmax,
                    None => eprintln!(
                        "WAL replay: skipping UPDATE of missing row id {} in table \"{}\"",
                        r.id, table
                    ),
                }
            }
            for r in new {
                // v0.39: restore toast flags and metadata, like InsertRows.
                let mut rv = RowVersion::plain(r.id, r.values.clone(), *xmax);
                if r.toast.len() == r.values.len() {
                    rv.toast = r.toast.clone();
                }
                for (vid, compressed) in &r.toast_meta {
                    // v0.41: the byte is a method code (0 = plain).
                    let info = decode_toast_method(*compressed)?;
                    t.toast_info.insert(*vid, info);
                    if t.next_value_id <= *vid {
                        t.next_value_id = vid + 1;
                    }
                }
                t.push_version(rv);
            }
            // Index the new versions, like live execution's
            // index_insert_row (replay variant): recovery rebuilds indexes only for the
            // checkpoint image; replayed rows need their entries.
            let indexed: Vec<(u64, Row)> = new.iter().map(|r| (r.id, r.values.clone())).collect();
            // `t`'s borrow ends at its last use above; eng.db is free again.
            for (id, values) in indexed {
                eng.db
                    .index_insert_row_replay(table, id, &values);
            }
        }
        // v0.13: replication slot metadata is applied by the match
        // above (like roles); nothing left to do here.
        WalRecord::CreateIndex {
            name,
            table,
            columns,
            unique,
            internal,
            desc,
            nulls_first,
            exprs,
            predicate,
            xmin,
        } => {
            // Rebuild the index from the table's current rows: at recovery
            // every row present comes from a committed batch, so indexing
            // all versions is correct (visibility filters at scan time).
            // v0.88: expression / partial indexes are catalog-only (never
            // built); their definitions still round-trip for fidelity.
            let built: Option<Index> = (|| {
                let t = live_table(eng, table)?;
                let n = columns.len();
                let mut cols = Vec::with_capacity(n);
                let mut any_expr = false;
                for (i, c) in columns.iter().enumerate() {
                    let is_expr = exprs.get(i).and_then(|e| e.as_ref()).is_some();
                    if is_expr {
                        any_expr = true;
                        cols.push(usize::MAX);
                    } else {
                        cols.push(t.column_index(c)?);
                    }
                }
                let pad_bool = |v: &[bool]| {
                    let mut out = v.to_vec();
                    out.resize(n, false);
                    out
                };
                let mut ex = exprs.clone();
                ex.resize(n, None);
                let planner_usable = !any_expr && predicate.is_none();
                let mut ix = Index::new(IndexDef {
                    name: name.clone(),
                    table: table.clone(),
                    cols,
                    col_names: columns.clone(),
                    unique: *unique,
                    internal: *internal,
                    created_xmin: *xmin,
                    dropped_xmax: 0,
                    desc: pad_bool(desc),
                    nulls_first: pad_bool(nulls_first),
                    exprs: ex,
                    predicate: predicate.clone(),
                    planner_usable,
                });
                if planner_usable {
                    for r in &t.rows {
                        let key = ix.key_for(&r.values);
                        ix.insert(key, r.id);
                    }
                }
                Some(ix)
            })();
            match built {
                Some(ix) => {
                    eng.db.indexes.insert(name.clone(), ix);
                }
                None => eprintln!(
                    "WAL replay: skipping CreateIndex \"{}\": no live table version \
                     or unknown column",
                    name
                ),
            }
        }
        WalRecord::DropIndex { name, xmax } => match eng.db.indexes.get_mut(name) {
            Some(ix) => ix.def.dropped_xmax = *xmax,
            None => eprintln!("WAL replay: skipping DropIndex \"{}\": no such index", name),
        },
        WalRecord::AlterTable {
            name,
            columns,
            constraints,
            copy_rows,
            owner,
            acl,
            col_acl,
            oid,
            toast_relid,
            toast_target,
            col_storage,
            col_compression,
            composite_types,
            domain_types,
            domain_elem,
            inherits,
            attnums,
            next_attnum,
            fillfactor,
            next_value_id,
            toast_info,
            partition,
            xmin,
        } => {
            let versions = eng.db.tables.entry(name.clone()).or_default();
            // The previous live version is superseded.
            if let Some(prev) = versions.iter_mut().find(|v| v.dropped_xmax == 0) {
                prev.dropped_xmax = *xmin;
            }
            let mut t = Table::new(columns.clone(), *xmin);
            // v1.80: partition metadata (ATTACH, PARTITION OF, DROP COLUMN
            // key shifts). RGSWAL20 records never carried it; open() fills
            // it in from the live version before replay (see
            // `legacy_fill_partition`), so here None means "not partitioned".
            t.partition = partition.clone();
            // v1.41: restore attnums, the next-attnum counter, and
            // fillfactor.
            t.attnums = attnums.clone();
            t.next_attnum = *next_attnum;
            t.fillfactor = *fillfactor;
            match crate::sql::decode_constraints(constraints) {
                Ok(dc) => {
                    t.not_null = dc.not_null;
                    t.defaults = dc.defaults;
                    t.checks = dc.checks;
                    t.uniques = dc.uniques;
                    t.pkey = dc.pkey;
                    t.fks = dc.fks;
                }
                Err(e) => {
                    return Err(format!(
                        "WAL replay: bad constraints for table \"{}\": {}",
                        name, e
                    ));
                }
            }
            // Pure-metadata alters carry the rows forward; ADD/DROP COLUMN
            // rewrites them via InsertRows records.
            if *copy_rows {
                if let Some(prev) = versions.iter().find(|v| v.dropped_xmax == *xmin) {
                    t.rows = prev.rows.clone();
                }
            }
            // v0.11
            t.owner = owner.clone();
            t.acl = acl.iter().cloned().map(WalAcl::into_entry).collect();
            t.col_acl = col_acl.iter().cloned().map(WalColAcl::into_entry).collect();
            // v0.37: TOAST metadata.
            t.oid = *oid;
            t.toast_relid = *toast_relid;
            // v0.37: keep the OID counter ahead of restored OIDs (see
            // the CreateTable replay above).
            eng.db.next_oid = eng
                .db
                .next_oid
                .max(oid.wrapping_add(1))
                .max(toast_relid.wrapping_add(1));
            t.toast_target = *toast_target;
            t.col_storage = col_storage.clone();
            // v0.85: restore composite/domain type use (empty vecs on old
            // records mean "no domain use").
            t.composite_types = composite_types.clone();
            t.domain_types = domain_types.clone();
            t.domain_elem = domain_elem.clone();
            // v0.96: restore inheritance parent links.
            t.inherits = inherits.clone();
            // v0.41: per-column compression methods (0 = default).
            t.col_compression = col_compression
                .iter()
                .map(|&code| {
                    if code == 0 {
                        Ok(None)
                    } else {
                        crate::storage::ToastCompression::from_code(code)
                            .map(Some)
                            .ok_or_else(|| {
                                format!("WAL replay: unknown toast compression code {}", code)
                            })
                    }
                })
                .collect::<Result<Vec<_>, String>>()?;
            t.next_value_id = (*next_value_id).max(1);
            t.toast_info = toast_info
                .iter()
                .map(|(k, c)| {
                    // v0.41: the byte is a method code (0 = plain).
                    decode_toast_method(*c).map(|info| (*k, info))
                })
                .collect::<Result<_, _>>()?;
            versions.push(t);
        }
        WalRecord::CreateView {
            name,
            query,
            col_aliases,
            deps,
            owner,
            xmin,
        } => {
            eng.db
                .views
                .entry(name.clone())
                .or_default()
                .push(crate::storage::ViewDef {
                    query: query.clone(),
                    col_aliases: col_aliases.clone(),
                    deps: deps.clone(),
                    created_xmin: *xmin,
                    dropped_xmax: 0,
                    // v0.11: owner comes from the record.
                    owner: owner.clone(),
                });
        }
        WalRecord::DropView { name, xmax } => {
            match eng
                .db
                .views
                .get_mut(name)
                .and_then(|vs| vs.iter_mut().find(|v| v.dropped_xmax == 0))
            {
                Some(v) => v.dropped_xmax = *xmax,
                None => eprintln!("WAL replay: skipping DropView \"{}\": no live view", name),
            }
        }
        WalRecord::CreateSequence { name, seq, xmin } => {
            eng.db
                .sequences
                .entry(name.clone())
                .or_default()
                .push(seq.clone().into_sequence(*xmin));
        }
        WalRecord::DropSequence { name, xmax } => {
            match eng
                .db
                .sequences
                .get_mut(name)
                .and_then(|vs| vs.iter_mut().find(|v| v.dropped_xmax == 0))
            {
                Some(s) => s.dropped_xmax = *xmax,
                None => eprintln!(
                    "WAL replay: skipping DropSequence \"{}\": no live sequence",
                    name
                ),
            }
        }
        WalRecord::AlterSequence { name, seq, xmin } => {
            let versions = eng.db.sequences.entry(name.clone()).or_default();
            let mut s = seq.clone().into_sequence(*xmin);
            // Carry the current value forward: ALTER SEQUENCE changes the
            // parameters, not the position.
            if let Some(prev) = versions.iter().find(|v| v.dropped_xmax == 0) {
                s.current = prev.current;
            }
            versions.push(s);
        }
        WalRecord::SeqAdvance {
            name,
            last_value,
            is_called,
        } => {
            match eng
                .db
                .sequences
                .get_mut(name)
                .and_then(|vs| vs.iter_mut().find(|v| v.dropped_xmax == 0))
            {
                Some(s) => {
                    s.current = Some(*last_value);
                    s.is_called = *is_called;
                }
                None => eprintln!(
                    "WAL replay: skipping SeqAdvance \"{}\": no live sequence",
                    name
                ),
            }
        }
        // v1.05: replay the lazy reltoastrelid link onto the table's
        // live version. The toast table itself was created by its own
        // CreateTable record earlier in the log (op order is
        // preserved), so only the link needs restoring here.
        WalRecord::SetToastRelid {
            name, toast_relid, ..
        } => match live_table(eng, name) {
            Some(t) => {
                t.toast_relid = *toast_relid;
            }
            None => eprintln!(
                "WAL replay: skipping SetToastRelid \"{}\": no live table",
                name
            ),
        },
    }
    Ok(())
}

/// Derive the commit-time WAL records from a transaction's write log.
///
/// Every op is re-validated against current engine state: only effects
/// that are still "ours" (`xmin`/`xmax`/`dropped_xmax`/`created_xmin`
/// equal to our xid) are logged. A concurrent transaction may have
/// overwritten our change after we made it (last-writer-wins — there is
/// no row locking in v0.5); logging the stale op would corrupt replay,
/// so it is skipped and the concurrent change wins. An op whose target
/// vanished (table dropped by a committed concurrent transaction) is
/// skipped for the same reason.
///
/// Two commit-ordering rules keep replay total:
/// - CREATE TABLE is first-committer-wins: if another committed live
///   version of the name exists, our commit fails with a serialization
///   error (the statement-time check could not see the concurrent
///   uncommitted CREATE).
/// - Records stay in op order (inserts grouped per table), so replay
///   sees every target its record needs.
///
/// Returns `Err` on a commit-time serialization conflict (SQLSTATE 40001
/// at the call site).
pub fn records_for_commit(
    eng: &Engine,
    own: u64,
    writes: &[WriteOp],
    session: u64,
) -> Result<Vec<WalRecord>, String> {
    let mut out: Vec<WalRecord> = Vec::new();
    // v1.80: CreateTable/AlterTable ops and the table versions they push
    // pair up one-to-one, in order: op N on a table made that table's Nth
    // version owned by this transaction. Each record must describe ITS
    // version. v1.79 logged the latest one for every AlterTable, so a
    // multi-action ALTER (two DROP COLUMNs) logged the final shape for the
    // intermediate version, whose rewritten rows were wider. Replay built
    // an inconsistent version, and the next checkpoint made the data
    // directory unopenable.
    let mut own_versions: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut i = 0;
    while i < writes.len() {
        let op = &writes[i];
        match op {
            WriteOp::InsertRow { table, row_id } => {
                // v0.22: temp-table DML is never WAL-logged (PostgreSQL
                // doesn't log temp-table data either): it is
                // session-local and dies with the session. Row ids are
                // globally unique, so this test is exact.
                if eng.db.row_id_in_temp(*row_id) {
                    i += 1;
                    continue;
                }
                let Some((values, toast, table_live)) = own_row(eng, own, *row_id) else {
                    i += 1;
                    continue;
                };
                if !table_live {
                    i += 1;
                    continue; // table dropped by a committed concurrent txn
                }
                // v0.14: commit-time unique recheck. A concurrent txn may
                // have committed the same unique key after our
                // statement-time check ran; committing would create a
                // duplicate key. Fail the commit (40001 at the call site)
                // instead of corrupting the unique index.
                if let Some(cname) = eng.db.committed_unique_violation(
                    &eng.txns,
                    table,
                    &values,
                    *row_id,
                    &[own],
                    session,
                ) {
                    return Err(format!(
                        "duplicate key value violates unique constraint \"{}\" \
                         (committed by a concurrent transaction)",
                        cname
                    ));
                }
                // v0.39: the row's toast flags and value-id provenance
                // ride along so replay restores TOAST metadata.
                let toast_meta = toast_meta_for(eng, table, &toast);
                let row = WalRow {
                    id: *row_id,
                    xmin: own,
                    values,
                    toast,
                    toast_meta,
                };
                match out.last_mut() {
                    Some(WalRecord::InsertRows { table: t, rows }) if t == table => rows.push(row),
                    _ => out.push(WalRecord::InsertRows {
                        table: table.clone(),
                        rows: vec![row],
                    }),
                }
            }
            WriteOp::DeleteRow { table, row_id, .. } => {
                // v0.22: temp-table DML is never WAL-logged (see above).
                if eng.db.row_id_in_temp(*row_id) {
                    i += 1;
                    continue;
                }
                let Some(v) = eng.db.find_row_version(*row_id) else {
                    i += 1;
                    continue;
                };
                if v.xmax != own {
                    // Our delete was overwritten by a concurrent txn
                    // (last-writer-wins). If the next op is our INSERT
                    // successor (an UPDATE pair), committing would leave
                    // BOTH versions live — a duplicate row. Fail the
                    // commit instead, like a write-write conflict.
                    // (v0.13: exec now emits UpdateRow for updates, so the
                    // paired_insert case below is vestigial — kept as a
                    // safety net in case any path still emits the pair.)
                    let paired_insert =
                        matches!(writes.get(i + 1), Some(WriteOp::InsertRow { .. }));
                    if paired_insert {
                        return Err(format!(
                            "concurrent update on row id {} in table \"{}\"",
                            row_id, table
                        ));
                    }
                    i += 1;
                    continue; // pure DELETE lost; the row stays deleted
                }
                // v0.13: the old values travel with the delete so the
                // logical decoder can report them without consulting
                // live (possibly vacuumed) storage. v0.39: the old
                // toast flags ride along too (no new provenance: the
                // value ids were introduced by an earlier record).
                let old = WalRow {
                    id: *row_id,
                    xmin: v.xmin,
                    values: v.values.clone(),
                    toast: v.toast.clone(),
                    toast_meta: Vec::new(),
                };
                match out.last_mut() {
                    Some(WalRecord::DeleteRows {
                        table: t,
                        ids,
                        old_rows,
                        xmax,
                    }) if t == table && *xmax == own => {
                        ids.push(*row_id);
                        old_rows.push(old);
                    }
                    _ => out.push(WalRecord::DeleteRows {
                        table: table.clone(),
                        ids: vec![*row_id],
                        old_rows: vec![old],
                        xmax: own,
                    }),
                }
            }
            // v0.13: first-class UPDATE op (see WriteOp::UpdateRow). The
            // write-write check mirrors the old paired-insert logic: if
            // a concurrent txn deleted the row, the update fails.
            WriteOp::UpdateRow {
                table,
                old_id,
                new_id,
                old_values,
                ..
            } => {
                // v0.22: temp-table DML is never WAL-logged (see above).
                // Both versions live in the same table; one check suffices.
                if eng.db.row_id_in_temp(*old_id) {
                    i += 1;
                    continue;
                }
                let Some(v) = eng.db.find_row_version(*old_id) else {
                    i += 1;
                    continue;
                };
                if v.xmax != own {
                    return Err(format!(
                        "concurrent update on row id {} in table \"{}\"",
                        old_id, table
                    ));
                }
                let Some((new_values, new_toast, table_live)) = own_row(eng, own, *new_id) else {
                    i += 1;
                    continue;
                };
                if !table_live {
                    i += 1;
                    continue; // table dropped by a committed concurrent txn
                }
                // v0.39: old rows carry their toast flags (provenance was
                // logged when the value ids were introduced); the new row
                // carries its flags plus fresh provenance.
                let old = WalRow {
                    id: *old_id,
                    xmin: v.xmin,
                    values: old_values.clone(),
                    toast: v.toast.clone(),
                    toast_meta: Vec::new(),
                };
                let new_toast_meta = toast_meta_for(eng, table, &new_toast);
                let new = WalRow {
                    id: *new_id,
                    xmin: own,
                    values: new_values,
                    toast: new_toast,
                    toast_meta: new_toast_meta,
                };
                match out.last_mut() {
                    Some(WalRecord::UpdateRows {
                        table: t,
                        old: olds,
                        new: news,
                        xmax,
                    }) if t == table && *xmax == own => {
                        olds.push(old);
                        news.push(new);
                    }
                    _ => out.push(WalRecord::UpdateRows {
                        table: table.clone(),
                        old: vec![old],
                        new: vec![new],
                        xmax: own,
                    }),
                }
            }
            WriteOp::CreateTable { name } => {
                let nth = own_versions.entry(name.clone()).or_insert(0);
                let idx = *nth;
                *nth += 1;
                let Some(versions) = eng.db.tables.get(name) else {
                    i += 1;
                    continue;
                };
                let Some(ours) = versions.iter().filter(|t| t.created_xmin == own).nth(idx) else {
                    i += 1;
                    continue;
                };
                // First-committer-wins: a rival committed live version
                // means our CREATE lost the race. v1.80: a version WE
                // dropped earlier in this transaction is not a rival
                // (DROP TABLE t; CREATE TABLE t in one transaction, which
                // PG allows), like the CREATE VIEW check below.
                let rival = versions.iter().any(|t| {
                    t.created_xmin != own
                        && eng.xid_committed(t.created_xmin)
                        && (t.dropped_xmax == 0 || !eng.xid_committed(t.dropped_xmax))
                        && t.dropped_xmax != own
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateTable {
                    name: name.clone(),
                    columns: ours.columns.clone(),
                    constraints: crate::sql::encode_constraints(ours),
                    owner: ours.owner.clone(),
                    acl: ours.acl.iter().map(WalAcl::of).collect(),
                    col_acl: ours.col_acl.iter().map(WalColAcl::of).collect(),
                    oid: ours.oid,
                    toast_relid: ours.toast_relid,
                    // v0.41: compression method codes, 0 = default.
                    col_compression: ours
                        .col_compression
                        .iter()
                        .map(|m| m.map(|m| m.code()).unwrap_or(0))
                        .collect(),
                    // v0.85: composite/domain type use.
                    composite_types: ours.composite_types.clone(),
                    domain_types: ours.domain_types.clone(),
                    domain_elem: ours.domain_elem.clone(),
                    // v0.96: inheritance parent links.
                    inherits: ours.inherits.clone(),
                    // v1.41: attnums, next_attnum, fillfactor.
                    attnums: ours.attnums.clone(),
                    next_attnum: ours.next_attnum,
                    fillfactor: ours.fillfactor,
                    // v1.80: partition metadata (RGSWAL21).
                    partition: ours.partition.clone(),
                    xmin: own,
                });
            }
            // v1.05: the lazy reltoastrelid link. Only log when the
            // table's live version still carries the link we set
            // (conditional like the other ops); replay restores it.
            WriteOp::SetToastRelid {
                table, toast_relid, ..
            } => {
                let linked = eng
                    .db
                    .tables
                    .get(table)
                    .and_then(|vs| vs.iter().find(|t| t.dropped_xmax == 0))
                    .is_some_and(|t| t.toast_relid == *toast_relid);
                if linked {
                    out.push(WalRecord::SetToastRelid {
                        name: table.clone(),
                        toast_relid: *toast_relid,
                        xmin: own,
                    });
                }
                i += 1;
            }
            // v0.9: ALTER TABLE swaps the table version; log the latest
            // version this transaction created.
            WriteOp::AlterTable {
                name,
                rewrite_rows,
                renamed_to,
                ..
            } => {
                let nth = own_versions.entry(name.clone()).or_insert(0);
                let idx = *nth;
                *nth += 1;
                // A rename moves the table's versions (and their count)
                // under the new name; later ops address it by that name.
                if let Some(target) = renamed_to {
                    let moved = own_versions.remove(name).unwrap_or(0);
                    own_versions.insert(target.clone(), moved);
                }
                let Some(ours) = eng
                    .db
                    .tables
                    .get(name)
                    .and_then(|vs| vs.iter().filter(|t| t.created_xmin == own).nth(idx))
                else {
                    i += 1;
                    continue;
                };
                out.push(WalRecord::AlterTable {
                    name: name.clone(),
                    columns: ours.columns.clone(),
                    constraints: crate::sql::encode_constraints(ours),
                    copy_rows: !rewrite_rows,
                    owner: ours.owner.clone(),
                    acl: ours.acl.iter().map(WalAcl::of).collect(),
                    col_acl: ours.col_acl.iter().map(WalColAcl::of).collect(),
                    // v0.37: TOAST metadata.
                    oid: ours.oid,
                    toast_relid: ours.toast_relid,
                    toast_target: ours.toast_target,
                    col_storage: ours.col_storage.clone(),
                    // v0.41: compression method codes, 0 = default.
                    col_compression: ours
                        .col_compression
                        .iter()
                        .map(|m| m.map(|m| m.code()).unwrap_or(0))
                        .collect(),
                    // v0.85: composite/domain type use.
                    composite_types: ours.composite_types.clone(),
                    domain_types: ours.domain_types.clone(),
                    domain_elem: ours.domain_elem.clone(),
                    // v0.96: inheritance parent links.
                    inherits: ours.inherits.clone(),
                    // v1.41: attnums, next_attnum, fillfactor.
                    attnums: ours.attnums.clone(),
                    next_attnum: ours.next_attnum,
                    fillfactor: ours.fillfactor,
                    next_value_id: ours.next_value_id,
                    toast_info: ours
                        .toast_info
                        .iter()
                        .map(|(k, v)| (*k, if v.compressed { v.method.code() } else { 0 }))
                        .collect(),
                    // v1.80: partition metadata (RGSWAL21).
                    partition: ours.partition.clone(),
                    xmin: own,
                });
            }
            WriteOp::CreateView { name } => {
                let Some(ours) = eng
                    .db
                    .views
                    .get(name)
                    .and_then(|vs| vs.iter().find(|v| v.created_xmin == own))
                else {
                    i += 1;
                    continue;
                };
                // First-committer-wins, like CREATE TABLE. v0.9: exclude
                // views we dropped ourselves in this transaction (OR REPLACE).
                let rival = eng.db.views.get(name).is_some_and(|vs| {
                    vs.iter().any(|v| {
                        v.created_xmin != own
                            && eng.xid_committed(v.created_xmin)
                            && (v.dropped_xmax == 0 || !eng.xid_committed(v.dropped_xmax))
                            && v.dropped_xmax != own
                    })
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateView {
                    name: name.clone(),
                    query: ours.query.clone(),
                    col_aliases: ours.col_aliases.clone(),
                    deps: ours.deps.clone(),
                    owner: ours.owner.clone(),
                    xmin: own,
                });
            }
            WriteOp::DropView { name, .. } => {
                let won = eng
                    .db
                    .views
                    .get(name)
                    .is_some_and(|vs| vs.iter().any(|v| v.dropped_xmax == own));
                if !won {
                    i += 1;
                    continue;
                }
                out.push(WalRecord::DropView {
                    name: name.clone(),
                    xmax: own,
                });
            }
            WriteOp::CreateSequence { name } => {
                let Some(ours) = eng
                    .db
                    .sequences
                    .get(name)
                    .and_then(|vs| vs.iter().find(|s| s.created_xmin == own))
                else {
                    i += 1;
                    continue;
                };
                let rival = eng.db.sequences.get(name).is_some_and(|vs| {
                    vs.iter().any(|s| {
                        s.created_xmin != own
                            && eng.xid_committed(s.created_xmin)
                            && (s.dropped_xmax == 0 || !eng.xid_committed(s.dropped_xmax))
                    })
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateSequence {
                    name: name.clone(),
                    seq: WalSequence::of(ours),
                    xmin: own,
                });
            }
            WriteOp::DropSequence { name, .. } => {
                let won = eng
                    .db
                    .sequences
                    .get(name)
                    .is_some_and(|vs| vs.iter().any(|s| s.dropped_xmax == own));
                if !won {
                    i += 1;
                    continue;
                }
                out.push(WalRecord::DropSequence {
                    name: name.clone(),
                    xmax: own,
                });
            }
            WriteOp::AlterSequence { name, .. } => {
                let Some(ours) = eng
                    .db
                    .sequences
                    .get(name)
                    .and_then(|vs| vs.iter().filter(|s| s.created_xmin == own).last())
                else {
                    i += 1;
                    continue;
                };
                out.push(WalRecord::AlterSequence {
                    name: name.clone(),
                    seq: WalSequence::of(ours),
                    xmin: own,
                });
            }
            // v0.9: sequence advances are non-transactional (Postgres
            // semantics): the value at commit time is logged, and abort
            // never rolls it back.
            WriteOp::SeqAdvance { name, .. } => {
                let Some((cur, called)) = eng
                    .db
                    .sequences
                    .get(name)
                    .and_then(|vs| vs.iter().find(|s| s.dropped_xmax == 0))
                    .and_then(|s| s.current.map(|c| (c, s.is_called)))
                else {
                    i += 1;
                    continue;
                };
                out.push(WalRecord::SeqAdvance {
                    name: name.clone(),
                    last_value: cur,
                    is_called: called,
                });
            }
            // v0.11: role DDL.
            WriteOp::CreateRole { name } => {
                let Some(ours) = eng
                    .db
                    .roles
                    .get(name)
                    .and_then(|vs| vs.iter().find(|r| r.created_xmin == own))
                else {
                    i += 1;
                    continue;
                };
                // First-committer-wins, like CREATE TABLE.
                let rival = eng.db.roles.get(name).is_some_and(|vs| {
                    vs.iter().any(|r| {
                        r.created_xmin != own
                            && eng.xid_committed(r.created_xmin)
                            && (r.dropped_xmax == 0 || !eng.xid_committed(r.dropped_xmax))
                    })
                });
                if rival {
                    return Err(format!("role \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateRole {
                    name: name.clone(),
                    role: WalRole::of(ours),
                    xmin: own,
                });
            }
            WriteOp::DropRole { name, .. } => {
                let won = eng
                    .db
                    .roles
                    .get(name)
                    .is_some_and(|vs| vs.iter().any(|r| r.dropped_xmax == own));
                if !won {
                    i += 1;
                    continue;
                }
                out.push(WalRecord::DropRole {
                    name: name.clone(),
                    xmax: own,
                });
            }
            WriteOp::AlterRole { name, .. } => {
                let Some(ours) = eng
                    .db
                    .roles
                    .get(name)
                    .and_then(|vs| vs.iter().filter(|r| r.created_xmin == own).last())
                else {
                    i += 1;
                    continue;
                };
                out.push(WalRecord::AlterRole {
                    name: name.clone(),
                    role: WalRole::of(ours),
                    xmin: own,
                });
            }
            // v0.11: database ACL replaces wholesale.
            WriteOp::DbAcl { .. } => {
                out.push(WalRecord::DbAcl {
                    acl: eng.db.db_acl.iter().map(WalAcl::of).collect(),
                    xmin: own,
                });
            }
            WriteOp::DropTable { name, .. } => {
                let won = eng
                    .db
                    .tables
                    .get(name)
                    .is_some_and(|vs| vs.iter().any(|t| t.dropped_xmax == own));
                if !won {
                    i += 1;
                    continue; // overwritten by a concurrent drop; theirs wins
                }
                out.push(WalRecord::DropTable {
                    name: name.clone(),
                    xmax: own,
                });
            }
            // v0.22: temp-table DDL is session-local and never WAL-logged.
            // The temp table itself is already in `temp_tables`; the op
            // only exists for statement-atomic undo.
            WriteOp::CreateTempTable { .. }
            | WriteOp::DropTempTable { .. }
            | WriteOp::AlterTempTable { .. } => {
                i += 1;
                continue;
            }
            // v0.82: type DDL is WAL-logged (definitions survive
            // checkpoint/restart). The new definition is read from the
            // type map, which the executor updated; absent means a later
            // DROP TYPE in the same txn removed it, so only the drop is
            // logged. DROP TYPE logs the removal.
            WriteOp::CreateType { name, .. } => {
                if let Some(st) = eng.db.types.get(name) {
                    out.push(WalRecord::CreateType {
                        name: name.clone(),
                        like_base: st.like_base.clone(),
                        composite: st.composite.clone(),
                        // v0.85: domain definitions travel as s-expr
                        // strings (sql::encode_domain_checks /
                        // encode_domain_default).
                        domain: st.domain.as_ref().map(|d| WalDomain {
                            base: d.base.clone(),
                            base_named: d.base_named.clone(),
                            base_domain: d.base_domain.clone(),
                            checks: crate::sql::encode_domain_checks(&d.checks),
                            not_null: d.not_null,
                            default: crate::sql::encode_domain_default(&d.default),
                        }),
                        xmin: own,
                    });
                }
                i += 1;
            }
            WriteOp::DropType { name, .. } => {
                out.push(WalRecord::DropType {
                    name: name.clone(),
                    xmax: own,
                });
                i += 1;
            }
            // v0.86: function/operator DDL is WAL-logged (definitions
            // survive checkpoint/restart). The new definition is read
            // from the catalog maps, which the executor updated; absent
            // means a later DROP in the same txn removed it, so only
            // the drop is logged.
            // v0.87: WAL-log the specific overload that was added.
            WriteOp::CreateFunction { name, added, .. } => {
                if eng
                    .db
                    .functions
                    .get(name)
                    .is_some_and(|ovs| ovs.iter().any(|f| f.arg_types == added.arg_types))
                {
                    let f = added;
                    out.push(WalRecord::CreateFunction {
                        name: name.clone(),
                        arg_names: f.arg_names.clone(),
                        arg_types: f.arg_types.clone(),
                        ret_type: f.ret_type.clone(),
                        returns_set: f.returns_set,
                        lang: match f.lang {
                            crate::sql::FuncLang::Sql => 0,
                            crate::sql::FuncLang::Internal => 1,
                            // v0.97: bounded plpgsql (body desugared to
                            // SQL at CREATE; the lang tag round-trips so
                            // restored catalogs keep the declared
                            // language).
                            crate::sql::FuncLang::Plpgsql => 2,
                        },
                        body: f.body.clone(),
                        volatility: match f.volatility {
                            crate::sql::FuncVolatility::Volatile => 0,
                            crate::sql::FuncVolatility::Stable => 1,
                            crate::sql::FuncVolatility::Immutable => 2,
                        },
                        strict: f.strict,
                        xmin: own,
                    });
                }
                i += 1;
            }
            // v0.87: WAL-log the specific overload's signature.
            WriteOp::DropFunction { name, prev, .. } => {
                let arg_types = prev
                    .as_ref()
                    .map(|f| f.arg_types.clone())
                    .unwrap_or_default();
                out.push(WalRecord::DropFunction {
                    name: name.clone(),
                    arg_types,
                    xmax: own,
                });
                i += 1;
            }
            WriteOp::CreateOperator { name, .. } => {
                if let Some(defs) = eng.db.operators.get(name) {
                    out.push(WalRecord::CreateOperator {
                        name: name.clone(),
                        defs: defs
                            .iter()
                            .map(|d| WalOperDef {
                                procedure: d.procedure.clone(),
                                leftarg: d.leftarg.clone(),
                                rightarg: d.rightarg.clone(),
                                commutator: d.commutator.clone(),
                                negator: d.negator.clone(),
                                hashes: d.hashes,
                                merges: d.merges,
                            })
                            .collect(),
                        xmin: own,
                    });
                }
                i += 1;
            }
            WriteOp::DropOperator { name, .. } => {
                out.push(WalRecord::DropOperator {
                    name: name.clone(),
                    xmax: own,
                });
                i += 1;
            }
            // v1.38: cast DDL is deliberately NOT WAL-logged (documented
            // known gap on `Database::casts`): user-defined casts are
            // in-memory and do not survive restart.
            WriteOp::CreateCast { .. } => {
                i += 1;
            }
            WriteOp::CreateIndex { name } => {
                let Some(ix) = eng.db.indexes.get(name) else {
                    i += 1;
                    continue;
                };
                if ix.def.created_xmin != own {
                    i += 1;
                    continue;
                }
                // First-committer-wins, mirroring CREATE TABLE: a rival
                // committed live definition means our CREATE lost the race.
                let rival = eng.db.indexes.values().any(|o| {
                    o.def.name == *name
                        && o.def.created_xmin != own
                        && eng.xid_committed(o.def.created_xmin)
                        && (o.def.dropped_xmax == 0 || !eng.xid_committed(o.def.dropped_xmax))
                });
                if rival {
                    return Err(format!("relation \"{}\" already exists", name));
                }
                out.push(WalRecord::CreateIndex {
                    name: name.clone(),
                    table: ix.def.table.clone(),
                    columns: ix.def.col_names.clone(),
                    unique: ix.def.unique,
                    internal: ix.def.internal,
                    desc: ix.def.desc.clone(),
                    nulls_first: ix.def.nulls_first.clone(),
                    exprs: ix.def.exprs.clone(),
                    predicate: ix.def.predicate.clone(),
                    xmin: own,
                });
            }
            // v0.87: temp index DDL is never WAL-logged (session-local,
            // dies with the session, like temp tables).
            WriteOp::CreateTempIndex { .. } | WriteOp::DropTempIndex { .. } => {
                i += 1;
                continue;
            }
            WriteOp::DropIndex { name, .. } => {
                // v1.80: the drop is ours if the entry under this name was
                // dropped by us, OR was created by us: that means we dropped
                // the old index and re-created it under the same name (ALTER
                // TABLE ... DROP COLUMN shifts positions this way). That drop
                // must reach the WAL ahead of the row rewrite, or replay
                // indexes the rewritten rows through the stale definition.
                let won = eng
                    .db
                    .indexes
                    .get(name)
                    .is_some_and(|ix| ix.def.dropped_xmax == own || ix.def.created_xmin == own);
                if !won {
                    i += 1;
                    continue; // overwritten by a concurrent drop; theirs wins
                }
                out.push(WalRecord::DropIndex {
                    name: name.clone(),
                    xmax: own,
                });
            }
        }
        i += 1;
    }
    Ok(out)
}

/// Our uncommitted row version, plus whether its table version is still
/// live (not dropped by a committed concurrent transaction). `None` when
/// the row is gone or no longer ours.
/// v0.39: also returns the row version's per-cell toast flags, so the
/// commit path can WAL-log them (and the value-id provenance).
pub(crate) fn own_row(eng: &Engine, own: u64, row_id: u64) -> Option<(Row, Vec<u32>, bool)> {
    for versions in eng.db.tables.values() {
        for t in versions {
            if let Some(pos) = t.row_pos(row_id) {
                let v = &t.rows[pos];
                if v.xmin != own {
                    // Not our row version (e.g. an old table version with
                    // reused row ids after ALTER); keep searching.
                    continue;
                }
                let dropped = t.dropped_xmax != 0
                    && t.dropped_xmax != own
                    && eng.xid_committed(t.dropped_xmax);
                return Some((v.values.clone(), v.toast.clone(), !dropped));
            }
        }
    }
    None
}

/// v0.39: collect the (value id, compressed) provenance for the nonzero
/// toast flags of one row, from the live table's `toast_info`. Returns an
/// empty vec when the row carries no toasted cells.
/// v0.41: the byte is the compression *method* code (0 = not
/// compressed, `b'p'`/`b'l'`), so the method survives WAL replay.
pub(crate) fn toast_meta_for(eng: &Engine, table: &str, flags: &[u32]) -> Vec<(u32, u8)> {
    if flags.iter().all(|f| *f == 0) {
        return Vec::new();
    }
    let Some(t) = eng.db.tables.get(table).and_then(|v| v.last()) else {
        return Vec::new();
    };
    let mut out: Vec<(u32, u8)> = Vec::new();
    for &vid in flags {
        if vid == 0 || out.iter().any(|(k, _)| *k == vid) {
            continue;
        }
        let code = t
            .toast_info
            .get(&vid)
            .map(|i| if i.compressed { i.method.code() } else { 0 })
            .unwrap_or(0);
        out.push((vid, code));
    }
    out
}

/// v0.41: decode a WAL/checkpoint toast method byte (0 = not
/// compressed, otherwise a `ToastCompression` code). Unknown codes are
/// a loud decode error — never silently reinterpreted.
pub(crate) fn decode_toast_method(byte: u8) -> Result<crate::storage::ToastInfo, String> {
    if byte == 0 {
        return Ok(crate::storage::ToastInfo {
            compressed: false,
            method: crate::storage::ToastCompression::Pglz,
        });
    }
    match crate::storage::ToastCompression::from_code(byte) {
        Some(method) => Ok(crate::storage::ToastInfo {
            compressed: true,
            method,
        }),
        None => Err(format!("unknown toast compression code {}", byte)),
    }
}
