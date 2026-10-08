
use super::*;

fn engine() -> Engine {
    Engine::new()
}

/// Run one statement, returning the result plus the notices the
/// statement produced (the v1.02 statement-scoped sink drained
/// into `StmtCtx.notices` by `execute()`).
fn run_n(eng: &mut Engine, sql: &str) -> (Result<ExecResult, ExecError>, Vec<String>) {
    let stmt = crate::sql::parse_statement(sql)
        .map_err(|e| exec_err(e.code, e.message))
        .unwrap();
    let snap = eng.take_snapshot();
    let mut writes = Vec::new();
    let mut ctx = StmtCtx {
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
    let r = execute(eng, &mut ctx, &stmt);
    let notices = std::mem::take(&mut ctx.notices);
    (r, notices)
}

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Select { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| v.to_text().unwrap_or("NULL".to_string()))
                    .collect()
            })
            .collect(),
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn err_of(r: Result<ExecResult, ExecError>) -> (String, String) {
    match r {
        Ok(_) => panic!("expected error"),
        Err(e) => (e.code.to_string(), e.message),
    }
}

/// v1.02: the verbatim subselect.sql tattle() shape — RAISE NOTICE
/// with %-format args, then RETURN. Notices land in
/// StmtCtx.notices; the boolean result is correct.
#[test]
fn tattle_raise_notice() {
    let mut eng = engine();
    run_n(
        &mut eng,
        "create function tattle102(x int, y int) returns bool volatile language plpgsql as $$
begin
  raise notice 'x = %, y = %', x, y;
  return x > y;
end$$;",
    )
    .0
    .unwrap();
    let (r, notices) = run_n(&mut eng, "select tattle102(9, 8)");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["t"]]);
    assert_eq!(notices, vec!["x = 9, y = 8".to_string()]);
    let (r, notices) = run_n(&mut eng, "select tattle102(1, 8)");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["f"]]);
    assert_eq!(notices, vec!["x = 1, y = 8".to_string()]);
}

/// v1.02: %-format edge cases — `%%` escape, missing args (kept
/// literally), extra args (ignored), no-arg format, NULL arg.
/// PG19 would raise on arity mismatch (pl_gram.y
/// `check_raise_parameters`); the shared evaluator stays total by
/// design (documented deviation, same as the v1.00 trigger
/// bodies). PG renders NULL params as `<NULL>`; the shared
/// text-cast renders them empty (same deviation).
#[test]
fn raise_notice_format_edges() {
    let mut eng = engine();
    run_n(
        &mut eng,
        "create function fmt102(x int, y int) returns int volatile language plpgsql as $$
begin
  raise notice '100%% sure: %', x;
  raise notice 'missing: % and %', x;
  raise notice 'extra: %', x, y;
  raise notice 'plain';
  raise notice 'null: %', null;
  return x;
end$$;",
    )
    .0
    .unwrap();
    let (r, notices) = run_n(&mut eng, "select fmt102(5, 7)");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["5"]]);
    assert_eq!(
        notices,
        vec![
            "100% sure: 5".to_string(),
            "missing: 5 and %".to_string(),
            "extra: 5".to_string(),
            "plain".to_string(),
            "null: ".to_string(),
        ]
    );
    // Direct evaluator checks (same shared fn the trigger path uses).
    assert_eq!(
        format_raise_message("a=% b='%%'", &[Value::Int(1)]),
        "a=1 b='%'".to_string()
    );
    assert_eq!(
        format_raise_message("% %", &[Value::Bool(true)]),
        "true %".to_string()
    );
}

/// v1.02: RAISE EXCEPTION aborts the call with the rendered
/// message and P0001 (PG19's default errcode when no
/// condition/SQLSTATE is given), and is trappable by the v1.01
/// WHEN handlers (by name and by OTHERS).
#[test]
fn raise_exception_aborts() {
    let mut eng = engine();
    run_n(
        &mut eng,
        "create function boom102() returns int volatile language plpgsql as $$
begin
  raise exception 'kaput %', 42;
  return 1;
end$$;",
    )
    .0
    .unwrap();
    let (r, notices) = run_n(&mut eng, "select boom102()");
    let (code, message) = err_of(r);
    assert_eq!(code, "P0001");
    assert_eq!(message, "kaput 42");
    assert!(notices.is_empty());
    // Trappable by condition name and by OTHERS.
    run_n(
        &mut eng,
        "create function trap102() returns int volatile language plpgsql as $$
begin
  raise exception 'nope';
  return 1;
exception when raise_exception then return -1;
end$$;",
    )
    .0
    .unwrap();
    let (r, _) = run_n(&mut eng, "select trap102()");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["-1"]]);
    run_n(
        &mut eng,
        "create function trap102b() returns int volatile language plpgsql as $$
begin
  raise exception 'nope';
  return 1;
exception when others then return -2;
end$$;",
    )
    .0
    .unwrap();
    let (r, _) = run_n(&mut eng, "select trap102b()");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["-2"]]);
}

/// v1.02: unsupported RAISE levels (DEBUG/LOG/INFO/WARNING) are an
/// honest 0A000 at parse, like the v1.00 trigger bodies.
#[test]
fn raise_unsupported_level_is_0a000() {
    let mut eng = engine();
    for level in ["debug", "log", "info", "warning"] {
        let (r, _) = run_n(
            &mut eng,
            &format!(
                "create function lvl102() returns int as 'begin raise {level} ''x''; return 1; end' language plpgsql"
            ),
        );
        let (code, _) = err_of(r);
        assert_eq!(code, "0A000", "level {level}");
    }
}

/// v1.02: RAISE parse errors — missing format literal and garbage
/// after the format string are 42601.
#[test]
fn raise_parse_errors() {
    let mut eng = engine();
    let (r, _) = run_n(
        &mut eng,
        "create function rp102a() returns int as 'begin raise notice; return 1; end' language plpgsql",
    );
    assert_eq!(err_of(r).0, "42601");
    let (r, _) = run_n(
        &mut eng,
        "create function rp102b() returns int as 'begin raise notice ''x'' oops; return 1; end' language plpgsql",
    );
    assert_eq!(err_of(r).0, "42601");
}

/// v1.02: named arguments in RAISE args rewrite to `$n` at CREATE
/// (and would on WAL-replay rebuild), so `x`/`y` resolve to the
/// call's parameters, not to columns.
#[test]
fn raise_named_arg_rewrite() {
    let mut eng = engine();
    run_n(
        &mut eng,
        "create function narr102(alpha int, beta int) returns int volatile language plpgsql as $$
begin
  raise notice '% + % = %', alpha, beta, alpha + beta;
  return alpha + beta;
end$$;",
    )
    .0
    .unwrap();
    let (r, notices) = run_n(&mut eng, "select narr102(20, 22)");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["42"]]);
    assert_eq!(notices, vec!["20 + 22 = 42".to_string()]);
    // The rewrite is visible in the stored parse (Param, not Column).
    let fns = eng.db.functions.get("narr102").unwrap();
    let fdef = fns
        .iter()
        .find(|f| f.arg_types == vec!["int", "int"])
        .unwrap();
    let pb = fdef.plpgsql.as_ref().unwrap();
    match &pb.stmts[0] {
        crate::sql::PlpgsqlStmt::Raise { args, .. } => {
            assert!(matches!(args[0], crate::sql::Expr::Param(1)));
            assert!(matches!(args[2], crate::sql::Expr::Arith { .. }));
        }
        s => panic!("expected Raise, got {s:?}"),
    }
}

/// v1.02: named args inside CASE / array / subscript forms rewrite
/// AND substitute coherently (the substitutor descends through
/// the same forms the rewriter does).
#[test]
fn raise_rewrite_subst_coherent() {
    let mut eng = engine();
    run_n(
            &mut eng,
            "create function coh102(x int) returns int volatile language plpgsql as $$\nbegin\n  raise notice 'c=%', case when x > 0 then x else 0 - x end;\n  return (array[x, x + 1])[2];\nend$$;",
        )
        .0
        .unwrap();
    let (r, notices) = run_n(&mut eng, "select coh102(5)");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["6"]]);
    assert_eq!(notices, vec!["c=5".to_string()]);
    let (r, notices) = run_n(&mut eng, "select coh102(-3)");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["-2"]]);
    assert_eq!(notices, vec!["c=3".to_string()]);
}

/// v1.02: multiple RAISE NOTICE calls in one statement accumulate
/// in order.
#[test]
fn raise_multiple_notices_accumulate() {
    let mut eng = engine();
    run_n(
        &mut eng,
        "create function multi102(x int) returns int volatile language plpgsql as $$
begin
  raise notice 'first %', x;
  raise notice 'second %', x + 1;
  return x;
end$$;",
    )
    .0
    .unwrap();
    let (r, notices) = run_n(&mut eng, "select multi102(1)");
    assert_eq!(rows_of(r.unwrap()), vec![vec!["1"]]);
    assert_eq!(notices, vec!["first 1".to_string(), "second 2".to_string()]);
}

/// v1.02: a notice raised before a later error is still delivered
/// (PG19 emits notices as generated, even when the statement
/// later fails).
#[test]
fn raise_notice_survives_later_error() {
    let mut eng = engine();
    run_n(
        &mut eng,
        "create function ne102() returns int volatile language plpgsql as $$
begin
  raise notice 'before the fall';
  return 1/0;
end$$;",
    )
    .0
    .unwrap();
    let (r, notices) = run_n(&mut eng, "select ne102()");
    assert_eq!(err_of(r).0, "22012");
    assert_eq!(notices, vec!["before the fall".to_string()]);
}
