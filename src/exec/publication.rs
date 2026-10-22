// v2.02: publications (minimal catalog) + the replica-identity guard
// (PG19 commands/publicationcmds.c, executor/execReplication.c
// CheckCmdReplicaIdentity, called from execMain.c
// CheckValidResultRel).
//
// The catalog lives on `Database::publications` — not WAL-logged
// and not checkpointed (definitions do not survive restart), but
// transactional via the statement undo log (`WriteOp::*Publication`),
// which object_address.sql observably relies on. Its only consumer
// in this milestone is the guard below (plus the DDL surface
// itself); pg_publication catalog views are not implemented.
//
// Guard semantics (DEFAULT replica identity only): a table publishes
// updates/deletes when some publication covers it — FOR ALL TABLES
// (minus EXCEPT), an explicit FOR TABLE entry, a TABLES IN SCHEMA
// entry for the table's schema, or an ancestor partitioned table so
// covered — and that publication's publish actions include the
// command. A covered UPDATE/DELETE against a table with no PRIMARY
// KEY (the decidable DEFAULT replica identity; REPLICA IDENTITY
// INDEX/FULL syntax stays 42601) errors 55000 exactly as PG19.
// Partitioned parents are skipped (PG skips partitioned relkind);
// leaves are checked individually by the callers.
use super::*;
use crate::storage::Publication;

fn base_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// PG19 pg_publication.c check_publication_add_relation, shared by
/// FOR TABLE entries and FOR ALL TABLES EXCEPT entries: the relation
/// must exist (42P01); EXCEPT entries additionally must not be an
/// individual partition (22023 — this is what keeps publication.sql's
/// testpub9 from ever existing upstream); catalog and temporary
/// relations are rejected (22023) in both roles.
fn validate_pub_relation(
    eng: &Engine,
    ctx: &StmtCtx,
    t: &str,
    except: bool,
) -> Result<(), ExecError> {
    let base = base_name(t);
    let msg = if except {
        format!(
            "cannot specify relation \"{}\" in the publication EXCEPT clause",
            t
        )
    } else {
        format!("cannot add relation \"{}\" to publication", base)
    };
    if eng
        .db
        .temp_tables
        .get(&ctx.session)
        .is_some_and(|m| m.contains_key(base))
    {
        return Err(exec_err("22023", msg));
    }
    if let Some(tbl) = eng.db.find_table(t, ctx.snap, &ctx.all_xids, ctx.session) {
        if eng
            .db
            .obj_schema
            .get(base)
            .is_some_and(|s| s.eq_ignore_ascii_case("pg_catalog"))
        {
            return Err(exec_err("22023", msg));
        }
        if except
            && tbl
                .partition
                .as_ref()
                .is_some_and(|pi| pi.parent.is_some() && !pi.is_partitioned)
        {
            return Err(exec_err("22023", msg));
        }
        return Ok(());
    }
    if eng.db.find_view(t, ctx.snap, ctx.own).is_some() {
        return Err(exec_err("22023", msg));
    }
    Err(exec_err(
        "42P01",
        format!("relation \"{}\" does not exist", t),
    ))
}

/// PG19 publicationcmds.c validation for a FOR TABLE entry.
fn validate_pub_table(eng: &Engine, ctx: &StmtCtx, t: &str) -> Result<(), ExecError> {
    validate_pub_relation(eng, ctx, t, false)
}

/// PG19: EXCEPT entries get the same validation, with the
/// individual-partition rejection on top.
fn validate_pub_except(eng: &Engine, ctx: &StmtCtx, tables: &[String]) -> Result<(), ExecError> {
    for t in tables {
        validate_pub_relation(eng, ctx, t, true)?;
    }
    Ok(())
}

/// PG19: a TABLES IN SCHEMA entry must name an existing schema;
/// system schemas cannot be added (check_publication_add_schema).
fn validate_pub_schemas(eng: &Engine, schemas: &[String]) -> Result<(), ExecError> {
    for s in schemas {
        if s.eq_ignore_ascii_case("pg_catalog") || s.eq_ignore_ascii_case("pg_toast") {
            return Err(exec_err(
                "22023",
                format!("cannot add schema \"{}\" to publication", s),
            ));
        }
        if !eng.db.schemas.iter().any(|x| x.eq_ignore_ascii_case(s)) {
            return Err(exec_err(
                "42P01",
                format!("schema \"{}\" does not exist", s),
            ));
        }
    }
    Ok(())
}

/// PG19: an unquoted CURRENT_SCHEMA entry in a publication spec is
/// resolved to the current schema at DDL time (the first existing
/// schema on search_path, $user resolving to the role); when no
/// schema has been selected that is an error. (The parser folds the
/// unquoted keyword to lowercase "current_schema"; a quoted
/// "CURRENT_SCHEMA" arrives verbatim and stays a literal name.)
/// Storing the literal instead would make the entry match every
/// schema in the replica-identity guard.
fn resolve_pub_schemas(
    eng: &Engine,
    ctx: &StmtCtx,
    schemas: &[String],
) -> Result<Vec<String>, ExecError> {
    let mut out: Vec<String> = Vec::new();
    for s in schemas {
        let resolved = if s == "current_schema" {
            let sp = crate::exec::installed_guc("search_path").unwrap_or_default();
            let mut found: Option<String> = None;
            for raw in sp.split(',') {
                let e = raw.trim().trim_matches('"');
                if e.is_empty() {
                    continue;
                }
                let cand = if e == "$user" {
                    ctx.role.to_string()
                } else {
                    e.to_string()
                };
                if eng.db.schemas.iter().any(|x| x == &cand) {
                    found = Some(cand);
                    break;
                }
            }
            match found {
                Some(f) => f,
                None => {
                    return Err(exec_err(
                        "22023",
                        "no schema has been selected for CURRENT_SCHEMA".to_string(),
                    ));
                }
            }
        } else {
            s.clone()
        };
        if !out.iter().any(|x| x.eq_ignore_ascii_case(&resolved)) {
            out.push(resolved);
        }
    }
    Ok(out)
}

/// Does publication `p` cover `table` (a storage key, unqualified) for
/// the schema attribution `obj_schema` records?
fn pub_covers(eng: &Engine, p: &Publication, table: &str) -> bool {
    if p.all_tables
        && !p
            .except_tables
            .iter()
            .any(|e| base_name(e).eq_ignore_ascii_case(table))
    {
        return true;
    }
    if p.tables
        .iter()
        .any(|t| base_name(t).eq_ignore_ascii_case(table))
    {
        return true;
    }
    if !p.schemas.is_empty() {
        let schema = eng
            .db
            .obj_schema
            .get(table)
            .map(|s| s.as_str())
            .unwrap_or("public");
        if p.schemas.iter().any(|s| s.eq_ignore_ascii_case(schema)) {
            return true;
        }
    }
    false
}

/// Publish actions covering `table` anywhere in the catalog, walking
/// partition ancestors: returns (publishes_update, publishes_delete).
fn publish_actions_for(eng: &Engine, ctx: &StmtCtx, table: &str) -> (bool, bool) {
    let mut upd = false;
    let mut del = false;
    // The table itself, then each partitioned ancestor in turn.
    let mut cur: Option<String> = Some(table.to_string());
    let mut guard = 0;
    while let Some(name) = cur {
        guard += 1;
        if guard > 16 {
            break;
        }
        for p in eng.db.publications.values() {
            if pub_covers(eng, p, base_name(&name)) {
                upd |= p.pubupdate;
                del |= p.pubdelete;
            }
        }
        cur = eng
            .db
            .find_table(&name, ctx.snap, &ctx.all_xids, ctx.session)
            .and_then(|t| t.partition.as_ref())
            .and_then(|pi| pi.parent.clone());
    }
    (upd, del)
}

/// PG19 CheckCmdReplicaIdentity (DEFAULT-identity core): error 55000
/// when `table` publishes the command and has no replica identity
/// (no PRIMARY KEY). Partitioned parents are skipped here — callers
/// check the leaves a statement actually touches.
pub(crate) fn check_replica_identity(
    eng: &Engine,
    ctx: &StmtCtx,
    t: &Table,
    table: &str,
    is_update: bool,
) -> Result<(), ExecError> {
    if t.partition.as_ref().is_some_and(|pi| pi.is_partitioned) {
        return Ok(());
    }
    if t.pkey.is_some() {
        return Ok(());
    }
    if eng.db.publications.is_empty() {
        return Ok(());
    }
    // PG19: temp relations are never publishable (see
    // check_replica_identity_for). Name resolution prefers the
    // session's temp table, so a same-named temp table means the
    // statement targets it, never the permanent namesake.
    if eng
        .db
        .temp_tables
        .get(&ctx.session)
        .is_some_and(|m| m.contains_key(base_name(table)))
    {
        return Ok(());
    }
    let (upd, del) = publish_actions_for(eng, ctx, table);
    let base = base_name(table);
    if is_update && upd {
        return Err(exec_err(
            "55000",
            format!(
                "cannot update table \"{}\" because it does not have a replica identity and publishes updates",
                base
            ),
        ));
    }
    if !is_update && del {
        return Err(exec_err(
            "55000",
            format!(
                "cannot delete from table \"{}\" because it does not have a replica identity and publishes deletes",
                base
            ),
        ));
    }
    Ok(())
}

/// Resolve `table` (when it exists) and run the guard on it. A
/// missing table is left to the caller's own 42P01 path.
pub(crate) fn check_replica_identity_for(
    eng: &Engine,
    ctx: &StmtCtx,
    table: &str,
    is_update: bool,
) -> Result<(), ExecError> {
    if eng.db.publications.is_empty() {
        return Ok(());
    }
    // PG19: temp relations are never publishable
    // (is_publishable_class requires permanent persistence), so the
    // guard never fires for them — combocid.sql's combocidtest is
    // the canonical case (a TEMP table updated while FOR ALL TABLES
    // publications from earlier suites are live in the database).
    if eng
        .db
        .temp_tables
        .get(&ctx.session)
        .is_some_and(|m| m.contains_key(base_name(table)))
    {
        return Ok(());
    }
    if let Some(t) = eng
        .db
        .find_table(table, ctx.snap, &ctx.all_xids, ctx.session)
    {
        let t = t.clone();
        check_replica_identity(eng, ctx, &t, table, is_update)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Publication DDL. Runtime-catalog mutations (see the module docs):
// they take effect immediately, like replication-slot DDL.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_publication(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    all_tables: bool,
    all_sequences: bool,
    except_tables: &[String],
    tables: &[String],
    schemas: &[String],
    publish: &Option<(bool, bool, bool, bool)>,
    via_partition_root: &Option<bool>,
) -> Result<ExecResult, ExecError> {
    if eng.db.publications.contains_key(name) {
        return Err(exec_err(
            "42710",
            format!("publication \"{}\" already exists", name),
        ));
    }
    // Validate named tables and schemas (publicationcmds.c).
    {
        let mut seen = HashSet::new();
        for t in tables {
            if !seen.insert(base_name(t).to_lowercase()) {
                return Err(exec_err(
                    "42710",
                    format!(
                        "relation \"{}\" is already part of publication \"{}\"",
                        base_name(t),
                        name
                    ),
                ));
            }
            validate_pub_table(eng, ctx, t)?;
        }
    }
    let schemas = &resolve_pub_schemas(eng, ctx, schemas)?;
    validate_pub_schemas(eng, schemas)?;
    if all_tables {
        validate_pub_except(eng, ctx, except_tables)?;
    }
    let (pubinsert, pubupdate, pubdelete, pubtruncate) =
        publish.unwrap_or((true, true, true, true));
    eng.db.publications.insert(
        name.to_string(),
        Publication {
            name: name.to_string(),
            owner: ctx.role.to_string(),
            all_tables,
            all_sequences,
            except_tables: except_tables.to_vec(),
            tables: tables.to_vec(),
            schemas: schemas.to_vec(),
            pubinsert,
            pubupdate,
            pubdelete,
            pubtruncate,
            via_partition_root: via_partition_root.unwrap_or(false),
        },
    );
    ctx.writes.push(crate::storage::WriteOp::CreatePublication {
        name: name.to_string(),
        prev: None,
    });
    Ok(ExecResult::Command {
        tag: "CREATE PUBLICATION".to_string(),
    })
}

pub(crate) fn exec_drop_publication(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        match eng.db.publications.remove(name) {
            Some(p) => {
                ctx.writes.push(crate::storage::WriteOp::DropPublication {
                    name: name.clone(),
                    prev: Some(p),
                });
            }
            None => {
                if if_exists {
                    ctx.notices
                        .push(format!("publication \"{}\" does not exist, skipping", name));
                } else {
                    return Err(exec_err(
                        "42P01",
                        format!("publication \"{}\" does not exist", name),
                    ));
                }
            }
        }
    }
    Ok(ExecResult::Command {
        tag: "DROP PUBLICATION".to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_alter_publication(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    action: char,
    spec_all_tables: bool,
    spec_all_sequences: bool,
    spec_except: &[String],
    tables: &[String],
    schemas: &[String],
    publish: &Option<(bool, bool, bool, bool)>,
    via_partition_root: &Option<bool>,
    new_value: &Option<String>,
) -> Result<ExecResult, ExecError> {
    if !eng.db.publications.contains_key(name) {
        return Err(exec_err(
            "42P01",
            format!("publication \"{}\" does not exist", name),
        ));
    }
    // Captured for the statement undo log (v2.02): every successful
    // mutation below records the pre-statement entry.
    let prev_pub = eng.db.publications.get(name).cloned();
    match action {
        'r' => {
            let new_name = new_value.clone().unwrap_or_default();
            if eng.db.publications.contains_key(&new_name) {
                return Err(exec_err(
                    "42710",
                    format!("publication \"{}\" already exists", new_name),
                ));
            }
            let mut p = eng.db.publications.remove(name).expect("checked above");
            p.name = new_name.clone();
            eng.db.publications.insert(new_name.clone(), p);
            ctx.writes.push(crate::storage::WriteOp::DropPublication {
                name: new_name,
                prev: None,
            });
            ctx.writes.push(crate::storage::WriteOp::CreatePublication {
                name: name.to_string(),
                prev: prev_pub,
            });
        }
        'o' => {
            if let Some(p) = eng.db.publications.get_mut(name) {
                p.owner = new_value.clone().unwrap_or_default();
            }
            ctx.writes.push(crate::storage::WriteOp::AlterPublication {
                name: name.to_string(),
                prev: prev_pub,
            });
        }
        'p' => {
            if let Some(p) = eng.db.publications.get_mut(name) {
                if let Some((i, u, d, t)) = publish {
                    p.pubinsert = *i;
                    p.pubupdate = *u;
                    p.pubdelete = *d;
                    p.pubtruncate = *t;
                }
                if let Some(v) = via_partition_root {
                    p.via_partition_root = *v;
                }
            }
            ctx.writes.push(crate::storage::WriteOp::AlterPublication {
                name: name.to_string(),
                prev: prev_pub,
            });
        }
        'a' | 'd' | 's' => {
            // CURRENT_SCHEMA entries resolve at DDL time (PG19).
            let schemas = &resolve_pub_schemas(eng, ctx, schemas)?;
            let is_all = eng.db.publications.get(name).is_some_and(|p| p.all_tables);
            // PG19: tables/schemas cannot be added to, dropped from,
            // or set on a FOR ALL TABLES publication.
            if is_all && (!tables.is_empty() || !schemas.is_empty()) {
                return Err(exec_err(
                    "55000",
                    format!("publication \"{}\" is defined as FOR ALL TABLES", name),
                ));
            }
            if action == 's' && (spec_all_tables || spec_all_sequences) {
                // SET ALL TABLES/SEQUENCES requires the publication
                // to already be FOR ALL TABLES/SEQUENCES or empty.
                let p = eng.db.publications.get(name).expect("checked above");
                let empty = !p.all_tables
                    && !p.all_sequences
                    && p.tables.is_empty()
                    && p.schemas.is_empty();
                if !p.all_tables && !p.all_sequences && !empty {
                    let what = if spec_all_tables {
                        "ALL TABLES"
                    } else {
                        "ALL SEQUENCES"
                    };
                    return Err(exec_err(
                        "55000",
                        format!(
                            "publication \"{}\" does not support {} operations",
                            name, what
                        ),
                    ));
                }
            }
            // Validate entries (PG errors on missing relations/schemas
            // for ADD and SET; DROP of a missing schema also errors,
            // DROP of a missing table is a no-op in PG only when the
            // table exists but is not a member — a missing relation
            // errors too, so validate uniformly).
            for t in tables {
                validate_pub_table(eng, ctx, t)?;
            }
            validate_pub_schemas(eng, schemas)?;
            if action == 's' && spec_all_tables {
                validate_pub_except(eng, ctx, spec_except)?;
            }
            // PG19: DROP of a non-member table/schema is an error.
            if action == 'd' {
                let p = eng.db.publications.get(name).expect("checked above");
                for t in tables {
                    if !p
                        .tables
                        .iter()
                        .any(|x| base_name(x).eq_ignore_ascii_case(base_name(t)))
                    {
                        return Err(exec_err(
                            "55000",
                            format!(
                                "relation \"{}\" is not part of the publication",
                                base_name(t)
                            ),
                        ));
                    }
                }
                for s in schemas {
                    if !p.schemas.iter().any(|x| x.eq_ignore_ascii_case(s)) {
                        return Err(exec_err(
                            "55000",
                            format!(
                                "tables from schema \"{}\" are not part of the publication",
                                s
                            ),
                        ));
                    }
                }
            }
            if let Some(p) = eng.db.publications.get_mut(name) {
                match action {
                    'a' => {
                        for t in tables {
                            if !p.tables.iter().any(|x| x == t) {
                                p.tables.push(t.clone());
                            }
                        }
                        for s in schemas {
                            if !p.schemas.iter().any(|x| x == s) {
                                p.schemas.push(s.clone());
                            }
                        }
                    }
                    'd' => {
                        p.tables.retain(|x| !tables.iter().any(|t| t == x));
                        p.schemas.retain(|x| !schemas.iter().any(|s| s == x));
                    }
                    _ => {
                        if spec_all_tables || spec_all_sequences {
                            p.all_tables = spec_all_tables;
                            p.all_sequences = spec_all_sequences;
                            p.except_tables = spec_except.to_vec();
                            p.tables.clear();
                            p.schemas.clear();
                        } else {
                            p.tables = tables.to_vec();
                            p.schemas = schemas.to_vec();
                            p.all_tables = false;
                            p.all_sequences = false;
                            p.except_tables.clear();
                        }
                    }
                }
            }
            ctx.writes.push(crate::storage::WriteOp::AlterPublication {
                name: name.to_string(),
                prev: prev_pub,
            });
        }
        _ => {}
    }
    Ok(ExecResult::Command {
        tag: "ALTER PUBLICATION".to_string(),
    })
}
