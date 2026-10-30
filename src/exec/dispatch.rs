// v1.78 mechanical split: moved verbatim from src/exec.rs (401-840).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

pub fn execute(eng: &mut Engine, ctx: &mut StmtCtx, stmt: &Stmt) -> Result<ExecResult, ExecError> {
    // v1.02: install the statement-scoped notice sink (see
    // NOTICE_SINK): `RAISE NOTICE` inside plpgsql function bodies
    // pushes here, and the accumulated messages are drained into
    // `ctx.notices` on the way out — on both success and error, like
    // PG19, which emits notices as they are generated even when the
    // statement later fails. The previous sink is save/restored so a
    // nested `execute()` (the COPY TO helper) cannot steal the
    // outer statement's notices.
    let sink = std::rc::Rc::new(RefCell::new(Vec::new()));
    let prev = NOTICE_SINK.with(|s| s.borrow_mut().replace(sink.clone()));
    let r = execute_inner(eng, ctx, stmt);
    NOTICE_SINK.with(|s| {
        *s.borrow_mut() = prev;
    });
    ctx.notices.extend(sink.borrow_mut().drain(..));
    r
}

/// v1.02: `execute` inner — the statement dispatch. Split out so the
/// notice-sink install/drain in `execute` covers every statement.
pub(crate) fn execute_inner(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    stmt: &Stmt,
) -> Result<ExecResult, ExecError> {
    match stmt {
        // v0.74: session-level PREPARE/EXECUTE/DEALLOCATE are resolved in
        // server.rs before reaching the executor.
        Stmt::Prepare { .. } | Stmt::Execute { .. } | Stmt::Deallocate { .. } => Err(exec_err(
            "XX000",
            "internal error: PREPARE/EXECUTE/DEALLOCATE reached the executor",
        )),
        Stmt::CreateTable { name, def, temp } => exec_create(eng, ctx, name, def, *temp),
        // v0.48: CREATE TABLE ... AS <query>.
        Stmt::CreateTableAs {
            name,
            col_aliases,
            select,
            temp,
            if_not_exists,
            with_data,
            execute,
        } => {
            // v1.99: server.rs resolves AS EXECUTE before dispatch; an
            // unresolved spec reaching here means a non-session caller.
            if execute.is_some() {
                return Err(exec_err(
                    "26000",
                    "prepared statement for CREATE TABLE AS EXECUTE was not resolved".to_string(),
                ));
            }
            exec_create_table_as(
                eng,
                ctx,
                name,
                col_aliases,
                select,
                *temp,
                *if_not_exists,
                *with_data,
            )
        }
        Stmt::Insert {
            table,
            columns,
            rows,
            select,
            with,
            on_conflict,
            returning,
        } => exec_insert(
            eng,
            ctx,
            table,
            columns,
            rows,
            select,
            with,
            on_conflict,
            returning,
        ),
        Stmt::Select(sel) => {
            // v1.85: SQL/JSON DEFAULT analysis-time restrictions
            // (PG transformJsonFuncExpr) — checked once here so they
            // fire even when the query yields zero rows.
            for item in &sel.items {
                if let crate::sql::SelectItem::Expr { expr, .. } = item {
                    analyze_sqljson_defaults(eng, expr)?;
                }
            }
            // FOR UPDATE locks (from every query level that asked for
            // them) are collected during the scan and acquired here, while
            // the engine lock is still held — so they are atomic with the
            // read. Never waited on: see try_row_lock.
            let mut lock_ids: Vec<(String, u64)> = Vec::new();
            let out = {
                let mut q = Q {
                    eng,
                    snap: ctx.snap,
                    own: ctx.own,
                    all_xids: ctx.all_xids.clone(),
                    session: ctx.session,
                    role: ctx.role,
                    read_only: ctx.read_only,
                    depth: 0,
                    lock_ids: &mut lock_ids,
                    ctes: Vec::new(),
                    wctx: None,
                    srf_vals: Vec::new(),
                    priv_scopes: Vec::new(),
            priv_role_override: None,
                    hashed_exists: Rc::new(RefCell::new(HashMap::new())),
                    immutable_fn_cache: Rc::new(RefCell::new(HashMap::new())),
                    plan_fold_memo: Rc::new(RefCell::new(HashMap::new())),
                    hashed_in: Rc::new(RefCell::new(Vec::new())),
                    pending_updates: None,
                    write: Some(qwrite_from_ctx(
                        &mut *ctx.writes,
                        ctx.write_xid,
                        ctx.level,
                        ctx.default_toast_compression,
                    )),
                };
                run_select(&mut q, sel, &[])?
            };
            if !lock_ids.is_empty() {
                acquire_row_locks(eng, ctx.own, &lock_ids)?;
            }
            Ok(ExecResult::Select {
                columns: out.columns,
                rows: out.rows,
            })
        }
        Stmt::DropTable {
            if_exists,
            names,
            cascade,
        } => exec_drop(eng, ctx, names, *if_exists, *cascade),
        // --- v0.9: constraints / ALTER / views / sequences
        Stmt::AlterTable {
            name,
            actions,
            if_exists,
            only,
        } => exec_alter_stmt(eng, ctx, name, actions, *if_exists, *only),
        Stmt::AlterViewRenameColumn { name, old, new } => {
            exec_alter_view_rename_column(eng, ctx, name, old, new)
        }
        Stmt::CreateView {
            name,
            query,
            col_aliases,
            or_replace,
            security_invoker,
            check_option,
            matview,
            with_data,
        } => exec_create_view(
            eng,
            ctx,
            name,
            query,
            col_aliases,
            *or_replace,
            *security_invoker,
            *check_option,
            *matview,
            *with_data,
        ),
        Stmt::AlterSchemaOwner { name, new_owner } => {
            exec_alter_schema_owner(eng, ctx, name, new_owner)
        }
        Stmt::AlterSchemaRename { name, new_name } => {
            exec_alter_schema_rename(eng, ctx, name, new_name)
        }
        Stmt::AlterDatabaseOwner { name, new_owner } => {
            exec_alter_database_owner(eng, ctx, name, new_owner)
        }
        // v2.02: publication DDL (runtime catalog; see publication.rs).
        Stmt::CreatePublication {
            name,
            all_tables,
            all_sequences,
            except_tables,
            tables,
            schemas,
            publish,
            via_partition_root,
            table_specs,
            publish_generated_columns,
        } => exec_create_publication(
            eng,
            ctx,
            name,
            *all_tables,
            *all_sequences,
            except_tables,
            tables,
            schemas,
            publish,
            via_partition_root,
            table_specs,
            publish_generated_columns,
        ),
        Stmt::DropPublication { names, if_exists } => {
            exec_drop_publication(eng, ctx, names, *if_exists)
        }
        Stmt::AlterPublication {
            name,
            action,
            all_tables,
            all_sequences,
            except_tables,
            tables,
            schemas,
            publish,
            via_partition_root,
            new_value,
            table_specs,
            publish_generated_columns,
        } => exec_alter_publication(
            eng,
            ctx,
            name,
            *action,
            *all_tables,
            *all_sequences,
            except_tables,
            tables,
            schemas,
            publish,
            via_partition_root,
            new_value,
            table_specs,
            publish_generated_columns,
        ),
        // v2.04: COMMENT ON PUBLICATION.
        Stmt::CommentPublication { name, comment } => {
            exec_comment_publication(eng, ctx, name, comment)
        }
        // v1.96: schema / maintenance / aggregate statement surface.
        Stmt::CreateSchema {
            name,
            if_not_exists,
            authorization,
        } => exec_create_schema(eng, ctx, name, *if_not_exists, authorization),
        Stmt::DropSchema {
            names,
            if_exists,
            cascade,
        } => exec_drop_schema(eng, ctx, names, *if_exists, *cascade),
        Stmt::Reindex {
            kind,
            name,
            concurrently,
        } => exec_reindex(eng, ctx, *kind, name, *concurrently),
        Stmt::ClusterStmt { name, index } => exec_cluster(eng, ctx, name, index),
        Stmt::ReassignOwned { from, to } => exec_reassign_owned(eng, ctx, from, to),
        Stmt::DropOwned { roles, cascade } => exec_drop_owned(eng, ctx, roles, *cascade),
        Stmt::RefreshMatView {
            name,
            with_data,
            concurrently,
        } => exec_refresh_matview(eng, ctx, name, *with_data, *concurrently),
        Stmt::SetConstraints { names, deferred } => {
            exec_set_constraints(eng, ctx, names, *deferred)
        }
        Stmt::CreateAggregate {
            name,
            arg_types,
            sfunc,
            stype,
            finalfunc,
            initcond,
            msfunc,
            minvfunc,
            mstype,
            parallel,
        } => exec_create_aggregate(
            eng,
            ctx,
            name,
            arg_types,
            sfunc,
            stype,
            finalfunc,
            initcond,
            msfunc,
            minvfunc,
            mstype,
            parallel,
        ),
        Stmt::DropAggregate {
            name,
            arg_types,
            if_exists,
            cascade,
        } => exec_drop_aggregate(eng, ctx, name, arg_types, *if_exists, *cascade),
        Stmt::DropView {
            names,
            if_exists,
            cascade,
        } => exec_drop_view(eng, ctx, names, *if_exists, *cascade),
        Stmt::CreateSequence {
            name,
            if_not_exists,
            opts,
        } => exec_create_sequence(eng, ctx, name, *if_not_exists, opts),
        Stmt::AlterSequence {
            name,
            if_exists,
            opts,
        } => exec_alter_sequence(eng, ctx, name, *if_exists, opts),
        Stmt::DropSequence { names, if_exists } => exec_drop_sequence(eng, ctx, names, *if_exists),
        // v0.75: CREATE STATISTICS is a validated no-op.
        Stmt::CreateStatistics => Ok(ExecResult::Command {
            tag: "CREATE STATISTICS".to_string(),
        }),
        // --- v0.22: bounded CREATE TYPE ---
        Stmt::CreateType {
            name,
            like_base,
            composite,
            range,
        } => {
            if let Some((subty, sub_named, multi_name)) = range {
                exec_create_range_type(
                    eng,
                    ctx,
                    name,
                    *subty,
                    sub_named.as_deref(),
                    multi_name.as_deref(),
                )
            } else {
                exec_create_type(eng, ctx, name, like_base.as_deref(), composite.as_deref())
            }
        }
        Stmt::DropType {
            names,
            if_exists,
            cascade,
        } => exec_drop_type(eng, ctx, names, *if_exists, *cascade),
        // --- v1.38: CREATE CAST (binary method only) ---
        Stmt::CreateCast {
            src,
            dst,
            method,
            context,
        } => exec_create_cast(eng, ctx, src, dst, *method, *context),
        Stmt::DropCast {
            src,
            dst,
            if_exists,
            ..
        } => exec_drop_cast(eng, ctx, src, dst, *if_exists),
        Stmt::AlterType { name, actions } => exec_alter_type(eng, ctx, name, actions),
        // --- v0.85: CREATE/DROP DOMAIN ---
        Stmt::CreateDomain {
            name,
            base,
            base_named,
            checks,
            not_null,
            default,
            base_bit_typmod,
        } => exec_create_domain(
            eng,
            ctx,
            name,
            base,
            base_named.as_deref(),
            checks,
            *not_null,
            default.as_ref(),
            *base_bit_typmod,
        ),
        Stmt::DropDomain { names, if_exists } => exec_drop_domain(eng, ctx, names, *if_exists),
        // --- v0.97: ALTER DOMAIN ---
        Stmt::AlterDomain { name, action } => exec_alter_domain(eng, ctx, name, action),
        // --- v1.09: ALTER FUNCTION ---
        Stmt::AlterFunction {
            name,
            arg_types,
            volatility,
            owner,
            security,
        } => exec_alter_function(
            eng,
            ctx,
            name,
            arg_types,
            *volatility,
            owner.as_deref(),
            *security,
        ),
        // --- v0.86: user-defined functions and operators ---
        Stmt::CreateFunction {
            name,
            args,
            ret_type,
            returns_set,
            lang_name,
            body,
            or_replace,
            volatility,
            strict,
            security_definer,
            procedure,
            table_cols,
        } => exec_create_function(
            eng,
            ctx,
            name,
            args,
            ret_type,
            *returns_set,
            lang_name,
            body,
            *or_replace,
            *volatility,
            *strict,
            *security_definer,
            *procedure,
            table_cols,
        ),
        Stmt::DropFunction {
            name,
            arg_types,
            arg_modes,
            if_exists,
            cascade,
            procedure,
            args_omitted,
            any_kind,
        } => exec_drop_function(
            eng,
            ctx,
            name,
            arg_types,
            arg_modes,
            *if_exists,
            *cascade,
            *procedure,
            *args_omitted,
            *any_kind,
        ),
        Stmt::CallProc { name, args } => exec_call_proc(eng, ctx, name, args),
        Stmt::DoBlock { body } => exec_do_block(eng, ctx, body),
        Stmt::DropOperator {
            name,
            leftarg,
            rightarg,
            if_exists,
            cascade,
        } => exec_drop_operator(
            eng,
            ctx,
            name,
            leftarg.as_deref(),
            rightarg.as_deref(),
            *if_exists,
            *cascade,
        ),
        Stmt::CreateOperator {
            name,
            procedure,
            leftarg,
            rightarg,
            commutator,
            negator,
            hashes,
            merges,
        } => exec_create_operator(
            eng,
            ctx,
            name,
            procedure,
            leftarg.as_deref(),
            rightarg.as_deref(),
            commutator.as_deref(),
            negator.as_deref(),
            *hashes,
            *merges,
        ),
        // --- v1.00: triggers (bounded) ---
        Stmt::CreateTrigger {
            name,
            table,
            timing,
            events,
            for_each_row,
            function,
            args,
            when,
            is_constraint,
            deferrable,
            initially_deferred,
            enforced,
            constr_rel,
            update_cols,
            has_transition,
        } => exec_create_trigger(
            eng,
            ctx,
            name,
            table,
            *timing,
            *events,
            *for_each_row,
            function,
            args,
            when.clone(),
            *is_constraint,
            *deferrable,
            *initially_deferred,
            *enforced,
            constr_rel.clone(),
            update_cols.clone(),
            *has_transition,
        ),
        Stmt::AlterTriggerRename {
            name,
            table,
            new_name,
        } => exec_alter_trigger_rename(eng, ctx, name, table, new_name),
        Stmt::DropTrigger {
            name,
            table,
            if_exists,
            cascade,
        } => exec_drop_trigger(eng, ctx, name, table, *if_exists, *cascade),
        // --- v0.11: roles and privileges ---
        Stmt::CreateRole {
            name,
            login,
            superuser,
            bypassrls,
            inherit,
            createrole,
            createdb,
            replication,
            member_of,
            members,
            password,
            connlimit,
            valid_until,
        } => exec_create_role(
            eng,
            ctx,
            name,
            *login,
            *superuser,
            *bypassrls,
            *inherit,
            *createrole,
            *createdb,
            *replication,
            member_of,
            members,
            password,
            *connlimit,
            valid_until,
        ),
        Stmt::AlterRole {
            name,
            login,
            superuser,
            bypassrls,
            inherit,
            createrole,
            createdb,
            replication,
            password,
            connlimit,
            valid_until,
        } => exec_alter_role(
            eng,
            ctx,
            name,
            *login,
            *superuser,
            *bypassrls,
            *inherit,
            *createrole,
            *createdb,
            *replication,
            password,
            *connlimit,
            valid_until,
        ),
        Stmt::DropRole { names, if_exists } => exec_drop_role(eng, ctx, names, *if_exists),
        // --- v1.95: row-level security policies ---
        Stmt::CreatePolicy {
            name,
            table,
            permissive,
            cmd,
            roles,
            using_expr,
            check_expr,
            using_sql,
            check_sql,
        } => exec_create_policy(
            eng,
            ctx,
            name,
            table,
            *permissive,
            *cmd,
            roles,
            using_expr,
            check_expr,
            using_sql,
            check_sql,
        ),
        Stmt::AlterPolicy {
            name,
            table,
            rename_to,
            roles,
            using_expr,
            check_expr,
            using_sql,
            check_sql,
            has_using,
            has_check,
        } => exec_alter_policy(
            eng,
            ctx,
            name,
            table,
            rename_to,
            roles,
            using_expr,
            check_expr,
            using_sql,
            check_sql,
            *has_using,
            *has_check,
        ),
        Stmt::DropPolicy {
            name,
            table,
            if_exists,
            cascade,
        } => {
            // Policies have no dependents; CASCADE is accepted and
            // otherwise ignored, like PG19 for this object type.
            let _ = cascade;
            exec_drop_policy(eng, ctx, name, table, *if_exists)
        }
        Stmt::Grant {
            privs,
            object,
            grantees,
            granted_by,
            grant_option,
        } => exec_grant_revoke(
            eng,
            ctx,
            privs,
            object,
            grantees,
            false,
            granted_by,
            *grant_option,
            false,
            false,
        ),
        Stmt::Revoke {
            privs,
            object,
            grantees,
            granted_by,
            grant_option_for,
            cascade: _cascade,
        } => exec_grant_revoke(
            eng,
            ctx,
            privs,
            object,
            grantees,
            true,
            granted_by,
            false,
            *grant_option_for,
            *_cascade,
        ),
        Stmt::GrantRole {
            roles,
            grantees,
            admin,
            inherit,
            set,
            granted_by,
        } => exec_grant_role(
            eng,
            ctx,
            roles,
            grantees,
            *admin,
            *inherit,
            *set,
            granted_by,
        ),
        Stmt::RevokeRole {
            roles,
            grantees,
            option_for,
            granted_by,
            cascade,
        } => exec_revoke_role(eng, ctx, roles, grantees, *option_for, granted_by, *cascade),
        Stmt::AlterGroup {
            name,
            add,
            drop,
            rename_to,
        } => exec_alter_group(eng, ctx, name, add, drop, rename_to),
        Stmt::LockTable {
            names,
            mode,
            nowait,
        } => exec_lock_table(eng, ctx, names, mode, *nowait),
        Stmt::AlterDefaultPrivileges {
            target_role,
            revoke,
            privs,
            kind,
            grantees,
            grant_option_for,
            schemas,
        } => exec_alter_default_privileges(
            eng,
            ctx,
            target_role.as_deref(),
            *revoke,
            privs,
            *kind,
            grantees,
            *grant_option_for,
            schemas,
        ),
        // --- v0.8: indexes / EXPLAIN / ANALYZE
        Stmt::CreateIndex {
            name,
            table,
            columns,
            unique,
            if_not_exists,
            predicate,
            nulls_not_distinct,
        } => exec_create_index(eng, ctx, name, table, columns, *unique, *if_not_exists, predicate.as_deref(), *nulls_not_distinct),
        Stmt::DropIndex { names, if_exists } => exec_drop_index(eng, ctx, names, *if_exists),
        Stmt::Explain {
            stmt, analyze, costs, opts, ..
        } => {
            // v1.45: only FORMAT TEXT has a renderer; PG's XML/JSON/YAML
            // formats are honestly rejected (0A000) rather than silently
            // rendering text.
            if opts.format != ExplainFormat::Text {
                return Err(exec_err(
                    "0A000",
                    "EXPLAIN (FORMAT XML/JSON/YAML) is not supported".to_string(),
                ));
            }
            if *analyze {
                exec_explain_analyze(eng, ctx, stmt, *costs)
            } else {
                // v1.46: thread VERBOSE through for `Output:` rendering.
                exec_explain(eng, ctx, stmt, *costs, opts.verbose)
            }
        }
        Stmt::Analyze { table } => exec_analyze(eng, ctx, table),
        Stmt::Update {
            table,
            alias,
            sets,
            from,
            where_,
            with,
            returning,
        } => exec_update(eng, ctx, table, alias, sets, from, where_, with, returning),
        Stmt::Delete {
            table,
            alias,
            using,
            where_,
            with,
            returning,
        } => exec_delete(eng, ctx, table, alias, using, where_, with, returning),
        Stmt::Merge {
            table,
            alias,
            source,
            on,
            whens,
            returning,
            with,
        } => exec_merge(eng, ctx, table, alias, source, on, whens, returning, with),
        // v0.16: TRUNCATE is executor-level (transactional row removal).
        Stmt::Truncate {
            tables,
            restart_identity,
            cascade,
        } => exec_truncate(eng, ctx, tables, *restart_identity, *cascade),
        // v0.16: cursor statements are session-level (server.rs owns the
        // cursor map); reaching the executor is a bug in the session
        // layer.
        Stmt::Declare { .. } | Stmt::Fetch { .. } | Stmt::Close { .. } | Stmt::Move { .. } => {
            Err(exec_err(
                "XX000",
                "internal error: cursor statement reached the statement executor",
            ))
        }
        // v0.10: COPY is handled by the server layer (it needs the raw
        // frontend messages); reaching the executor is a bug.
        Stmt::Copy { .. } => Err(exec_err(
            "XX000",
            "internal error: COPY reached the statement executor",
        )),
        // Transaction control / checkpoint / vacuum never reach the
        // executor (server.rs intercepts them); reaching here is a bug in
        // the session layer.
        Stmt::Begin { .. }
        | Stmt::Commit { .. }
        | Stmt::Rollback { .. }
        | Stmt::Savepoint { .. }
        | Stmt::RollbackTo { .. }
        | Stmt::Release { .. }
        | Stmt::Checkpoint
        | Stmt::Vacuum { .. }
        // v0.17: SET/SHOW/RESET and transaction-characteristic statements
        // are intercepted by server.rs; reaching here is a session bug.
        | Stmt::SetTransaction { .. }
        | Stmt::SetSessionCharacteristics { .. }
        | Stmt::Set { .. }
        | Stmt::Show { .. }
        | Stmt::Reset { .. } => Err(exec_err(
            "25001",
            "transaction control statements must go through the session",
        )),
    }
}

/// Take every collected FOR UPDATE lock for `own`. A row locked by
/// another *active* transaction fails the whole statement with 40001 —
/// we never block waiting for a lock (documented deviation from Postgres,
/// which would wait).
pub(crate) fn acquire_row_locks(
    eng: &mut Engine,
    own: u64,
    ids: &[(String, u64)],
) -> Result<(), ExecError> {
    for (table, id) in ids {
        if let Err(_holder) = eng.try_row_lock(*id, own) {
            return Err(exec_err(
                "40001",
                format!("could not obtain lock on row in relation \"{}\"", table),
            ));
        }
    }
    Ok(())
}
