// v1.78 mechanical split: moved verbatim from src/exec.rs (66049-66203).
// Zero semantic changes; map in goals/.../hidden_files/refactor-modularize/.
use super::*;

// ============================================================================
// v0.9: views.
// ============================================================================

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    query: &str,
    col_aliases: &[String],
    or_replace: bool,
    security_invoker: bool,
    check_option: bool,
    matview: bool,
    with_data: bool,
) -> Result<ExecResult, ExecError> {
    // A table by this name blocks the view (Postgres shares the namespace).
    if eng
        .db
        .find_table(name, ctx.snap, &ctx.all_xids, ctx.session)
        .is_some()
    {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    let existing = eng.db.find_view(name, ctx.snap, ctx.own).is_some();
    if existing && !or_replace {
        return Err(exec_err(
            "42P07",
            format!("relation \"{}\" already exists", name),
        ));
    }
    if existing {
        // v0.11: OR REPLACE requires ownership of the existing view.
        require_view_owner(eng, ctx, name)?;
    }
    // The query must parse, and must not reference the view itself
    // (checked by the parser) or unknown relations.
    let stmt = parse_statement(query).map_err(sql_err)?;
    let select = match stmt {
        Stmt::Select(s) => s,
        _ => {
            return Err(exec_err(
                "42601",
                "CREATE VIEW query must be a SELECT".to_string(),
            ));
        }
    };
    // v1.96: USAGE on cast targets in the view query (PG19).
    check_select_cast_usages(eng, ctx, &select)?;
    // v1.97: PG19 createas.c — more column names than the query
    // produces is a syntax error ("too many column names were
    // specified", 42601). Lenient when the output shape cannot be
    // described (the scan-time checks still apply).
    if !col_aliases.is_empty() {
        if let Ok(cols) = describe_select(eng, ctx.snap, ctx.own, ctx.session, &select, &[], &[]) {
            if col_aliases.len() > cols.len() {
                return Err(exec_err(
                    "42601",
                    "too many column names were specified".to_string(),
                ));
            }
        }
    }
    let mut deps = Vec::new();
    collect_table_refs(&select, &mut deps);
    // Every referenced relation must exist (as a table or view).
    for d in &deps {
        let is_table = eng
            .db
            .find_table(d, ctx.snap, &ctx.all_xids, ctx.session)
            .is_some();
        let is_view = eng.db.find_view(d, ctx.snap, ctx.own).is_some();
        if !is_table && !is_view {
            return Err(exec_err(
                "42P01",
                format!("relation \"{}\" does not exist", d),
            ));
        }
    }
    // v1.96 finishing pass: PG19 — materialized views must not
    // use temporary objects (the matview outlives the session).
    if matview {
        for d in &deps {
            if eng.db.is_temp_table(ctx.session, d) {
                return Err(exec_err(
                    "0A000",
                    "materialized views must not use temporary objects".to_string(),
                ));
            }
        }
    }
    // v1.96 finishing pass: a materialized view created WITH DATA
    // is populated at creation in PG19 — the query runs now and
    // its errors (division by zero, bad casts, ...) fail the
    // CREATE. The engine computes matview rows lazily at scan
    // time, so this is a validation execution only.
    if matview && with_data {
        let mut lock_ids = Vec::new();
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: false,
            depth: 0,
            lock_ids: &mut lock_ids,
            ctes: Vec::new(),
            wctx: None,
            srf_vals: Vec::new(),
            priv_scopes: Vec::new(),
            priv_role_override: None,
            hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(
                std::collections::HashMap::new(),
            )),
            immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(
                std::collections::HashMap::new(),
            )),
            plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(
                std::collections::HashMap::new(),
            )),
            hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            pending_updates: None,
            write: None,
        };
        run_select(&mut q, &select, &[])?;
    }
    if existing {
        // OR REPLACE: drop the old definition first (same statement).
        drop_view_internal(eng, ctx, name)?;
    }
    let def = ViewDef {
        query: query.to_string(),
        col_aliases: col_aliases.to_vec(),
        deps,
        created_xmin: ctx.write_xid,
        dropped_xmax: 0,
        // v0.11: the creating role owns the view.
        owner: ctx.role.to_string(),
        security_invoker,
        check_option,
        acl: Vec::new(),
        matview,
        populated: with_data || !matview,
    };
    eng.db.views.entry(name.to_string()).or_default().push(def);
    note_obj_schema(eng, ctx, name, name);
    ctx.writes.push(WriteOp::CreateView {
        name: name.to_string(),
    });
    Ok(ExecResult::Command {
        tag: if matview {
            "CREATE MATERIALIZED VIEW".to_string()
        } else {
            "CREATE VIEW".to_string()
        },
    })
}

pub(crate) fn exec_drop_view(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        // v0.11: only the owner (or a superuser) may drop a view.
        require_view_owner(eng, ctx, name)?;
        let exists = eng.db.find_view(name, ctx.snap, ctx.own).is_some();
        if !exists {
            if if_exists {
                continue;
            }
            return Err(exec_err(
                "42P01",
                format!("view \"{}\" does not exist", name),
            ));
        }
        // Other views depending on this one.
        let mut dependents: Vec<String> = Vec::new();
        for (vname, vs) in &eng.db.views {
            if vname == name {
                continue;
            }
            if vs.iter().any(|v| {
                crate::storage::view_visible(v, ctx.snap, ctx.own)
                    && v.deps.iter().any(|d| d == name)
            }) {
                dependents.push(vname.clone());
            }
        }
        if !dependents.is_empty() && !cascade {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop view {} because view {} depends on it",
                    name, dependents[0]
                ),
            ));
        }
        for dep in &dependents {
            drop_view_internal(eng, ctx, dep)?;
        }
        drop_view_internal(eng, ctx, name)?;
    }
    Ok(ExecResult::Command {
        tag: "DROP VIEW".to_string(),
    })
}

/// Drop a view by name, logging WriteOp::DropView. Used by DROP VIEW and
/// by CASCADE paths from DROP TABLE / DROP COLUMN.
pub(crate) fn drop_view_internal(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
) -> Result<(), ExecError> {
    let snapshot = eng
        .db
        .find_view(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("view \"{}\" does not exist", name)))?
        .clone();
    eng.db
        .find_view_mut(name, ctx.snap, ctx.own)
        .expect("view still present")
        .dropped_xmax = ctx.own;
    ctx.writes.push(WriteOp::DropView {
        name: name.to_string(),
        view: snapshot,
    });
    forget_obj_schema(eng, ctx, name);
    Ok(())
}

/// v1.96: REFRESH MATERIALIZED VIEW — re-materialize (the engine
/// evaluates views at read time, so this just flips `populated`).
pub(crate) fn exec_refresh_matview(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    with_data: bool,
    concurrently: bool,
) -> Result<ExecResult, ExecError> {
    let v = eng
        .db
        .find_view(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("relation \"{}\" does not exist", name)))?
        .clone();
    if !v.matview {
        return Err(exec_err(
            "42809",
            format!("\"{}\" is not a materialized view", name),
        ));
    }
    // v1.96 finishing pass: PG19 REFRESH option guards
    // (matview.c): CONCURRENTLY conflicts with WITH NO DATA, and
    // requires a unique, non-partial index on the matview.
    if concurrently && !with_data {
        return Err(exec_err(
            "0A000",
            "REFRESH options CONCURRENTLY and WITH NO DATA cannot be used together".to_string(),
        ));
    }
    if concurrently {
        let has_unique = eng.db.indexes.values().any(|ix| {
            ix.def.table == name
                && ix.def.unique
                && ix.def.predicate.is_none()
                && ix.def.exprs.iter().all(|e| e.is_none())
                && crate::storage::index_visible(&ix.def, ctx.snap, &[ctx.own])
        });
        if !has_unique {
            return Err(exec_err(
                "0A000",
                format!("cannot refresh materialized view \"{}\" concurrently", name),
            ));
        }
    }
    let closure = crate::storage::role_closure(&eng.db, ctx.role, ctx.snap, ctx.own);
    let ok = v.owner == ctx.role
        || crate::storage::is_superuser_snap(&eng.db, ctx.role, ctx.snap, ctx.own)
        || closure.iter().any(|r| r == "pg_maintain")
        // v1.96: an explicit MAINTAIN grant on the matview itself.
        || v.acl.iter().any(|e| {
            e.privs & 512 != 0
                && (e.role == "public" || closure.iter().any(|n| n == &e.role))
        });
    if !ok {
        return Err(exec_err(
            "42501",
            format!(
                "permission denied: must be owner of materialized view \"{}\"",
                name
            ),
        ));
    }
    // v1.96 finishing pass: PG19 REFRESH WITH DATA actually
    // populates the matview — the query runs now and its errors
    // fail the REFRESH. The engine computes matview rows lazily
    // at scan time, so this is a validation execution only.
    if with_data {
        let stmt = parse_statement(&v.query).map_err(sql_err)?;
        let select = match stmt {
            Stmt::Select(s) => s,
            _ => {
                return Err(exec_err(
                    "42601",
                    "materialized view query must be a SELECT".to_string(),
                ));
            }
        };
        let mut lock_ids: Vec<(String, u64)> = Vec::new();
        let mut q = Q {
            eng: &mut *eng,
            snap: ctx.snap,
            own: ctx.own,
            all_xids: ctx.all_xids.clone(),
            session: ctx.session,
            role: ctx.role,
            read_only: false,
            depth: 0,
            lock_ids: &mut lock_ids,
            ctes: Vec::new(),
            wctx: None,
            srf_vals: Vec::new(),
            priv_scopes: Vec::new(),
            priv_role_override: None,
            hashed_exists: std::rc::Rc::new(std::cell::RefCell::new(
                std::collections::HashMap::new(),
            )),
            immutable_fn_cache: std::rc::Rc::new(std::cell::RefCell::new(
                std::collections::HashMap::new(),
            )),
            plan_fold_memo: std::rc::Rc::new(std::cell::RefCell::new(
                std::collections::HashMap::new(),
            )),
            hashed_in: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            pending_updates: None,
            write: None,
        };
        run_select(&mut q, &select, &[])?;
    }
    let prev_acl = v.acl.clone();
    let prev_populated = v.populated;
    let live = eng
        .db
        .find_view_mut(name, ctx.snap, ctx.own)
        .expect("visible");
    live.populated = with_data;
    ctx.writes.push(WriteOp::ViewState {
        name: name.to_string(),
        prev_acl,
        prev_populated,
        prev_owner: String::new(),
    });
    Ok(ExecResult::Command {
        tag: "REFRESH MATERIALIZED VIEW".to_string(),
    })
}

// ============================================================================
// v1.97: dependent view definition rewriting for RENAME COLUMN.
//
// PG19 stores view query trees with attnum-based Vars, so renaming a
// column automatically rewrites every dependent view (ruleutils
// deparses the new name). This engine stores raw query TEXT, so the
// equivalent is a token-level rewrite of the stored text: qualified
// references (`qual.old`, `schema.qual.old`) whose qualifier names
// the renamed relation (or one of its FROM aliases), plus bare
// references when no other FROM relation of the view exposes a
// column with the same name (a valid view cannot bind a bare name
// ambiguously in one scope). The rewrite is verified by re-parsing;
// a dependent view whose references the rewrite provably did not
// cover keeps the pre-v1.97 2BP01 refusal rather than breaking
// silently. Documented narrowings (scope.md): FROM column-alias
// lists on the renamed relation, JOIN ... USING (old), and CTE
// shadowing of the relation name are treated as unrewritable.
// ============================================================================

/// Output column names of a view (query-derived, overlaid with the
/// view's explicit column aliases), via the describe machinery —
/// no rows are executed.
pub(crate) fn view_output_names(
    eng: &Engine,
    ctx: &StmtCtx,
    view: &crate::storage::ViewDef,
) -> Result<Vec<String>, ExecError> {
    let stmt = parse_statement(&view.query).map_err(sql_err)?;
    let select = match stmt {
        Stmt::Select(s) => s,
        _ => return Err(exec_err("42601", "view query must be a SELECT".to_string())),
    };
    let cols = describe_select(eng, &ctx.snap, ctx.own, ctx.session, &select, &[], &[])?;
    Ok(cols
        .into_iter()
        .enumerate()
        .map(|(i, (cn, _))| view.col_aliases.get(i).cloned().unwrap_or(cn))
        .collect())
}

/// Replace a view's definition in place (new query text and/or new
/// column aliases), transactionally: the old version is dropped via
/// the standard DropView write op and the new one pushed with a
/// CreateView write op — the same pattern CREATE OR REPLACE VIEW
/// uses, so WAL replay and rollback see an ordinary replace.
pub(crate) fn replace_view_def(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    query: Option<String>,
    col_aliases: Option<Vec<String>>,
) -> Result<(), ExecError> {
    let old = eng
        .db
        .find_view(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("view \"{}\" does not exist", name)))?
        .clone();
    drop_view_internal(eng, ctx, name)?;
    let mut def = old;
    if let Some(q) = query {
        def.query = q;
    }
    if let Some(a) = col_aliases {
        def.col_aliases = a;
    }
    def.created_xmin = ctx.write_xid;
    def.dropped_xmax = 0;
    eng.db.views.entry(name.to_string()).or_default().push(def);
    ctx.writes.push(WriteOp::CreateView {
        name: name.to_string(),
    });
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
enum SqlTokKind {
    Ident,
    QIdent,
    Other,
    Punct,
}

#[derive(Clone, Debug)]
struct SqlTok {
    start: usize,
    end: usize,
    kind: SqlTokKind,
    /// Normalized (lowercased) text for Ident; raw inner text for
    /// QIdent; the literal characters for Punct.
    text: String,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}
fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Tokenize SQL text into significant tokens (whitespace and
/// comments are skipped; string/dollar-quoted literals become
/// opaque Other tokens). Byte ranges refer to the original text so
/// replacements can be spliced without disturbing anything else.
fn scan_sql_tokens(text: &str) -> Vec<SqlTok> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        // Whitespace.
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Line comment.
        if c == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment (PG nests).
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // String literal ('' doubling; backslash escapes when the
        // quote is E-prefixed).
        if c == b'\'' {
            let e_prefixed = i > 0
                && (b[i - 1] == b'e' || b[i - 1] == b'E')
                && (i < 2 || !is_ident_char(b[i - 2]));
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && e_prefixed {
                    i += 2;
                    continue;
                }
                if b[i] == b'\'' {
                    if i + 1 < b.len() && b[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(SqlTok {
                start,
                end: i,
                kind: SqlTokKind::Other,
                text: String::new(),
            });
            continue;
        }
        // Dollar-quoted string: $tag$ ... $tag$.
        if c == b'$' {
            let mut j = i + 1;
            while j < b.len() && (is_ident_char(b[j]) && b[j] != b'$') {
                j += 1;
            }
            if j < b.len() && b[j] == b'$' && (j == i + 1 || is_ident_start(b[i + 1])) {
                let delim = &text[i..=j];
                let start = i;
                i = j + 1;
                match text[i..].find(delim) {
                    Some(pos) => i += pos + delim.len(),
                    None => i = b.len(),
                }
                out.push(SqlTok {
                    start,
                    end: i,
                    kind: SqlTokKind::Other,
                    text: String::new(),
                });
                continue;
            }
            // Not a dollar quote: fall through to punctuation.
            out.push(SqlTok {
                start: i,
                end: i + 1,
                kind: SqlTokKind::Punct,
                text: "$".to_string(),
            });
            i += 1;
            continue;
        }
        // Quoted identifier.
        if c == b'"' {
            let start = i;
            i += 1;
            let mut inner = String::new();
            while i < b.len() {
                if b[i] == b'"' {
                    if i + 1 < b.len() && b[i + 1] == b'"' {
                        inner.push('"');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                inner.push(b[i] as char);
                i += 1;
            }
            out.push(SqlTok {
                start,
                end: i,
                kind: SqlTokKind::QIdent,
                text: inner,
            });
            continue;
        }
        // Bare identifier / keyword.
        if is_ident_start(c) {
            let start = i;
            while i < b.len() && is_ident_char(b[i]) {
                i += 1;
            }
            out.push(SqlTok {
                start,
                end: i,
                kind: SqlTokKind::Ident,
                text: text[start..i].to_ascii_lowercase(),
            });
            continue;
        }
        // Number or other run: consume to the next delimiter-ish byte
        // as one opaque token (numbers never match a column name).
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            out.push(SqlTok {
                start,
                end: i,
                kind: SqlTokKind::Other,
                text: String::new(),
            });
            continue;
        }
        // Single punctuation byte.
        out.push(SqlTok {
            start: i,
            end: i + 1,
            kind: SqlTokKind::Punct,
            text: (c as char).to_string(),
        });
        i += 1;
    }
    out
}

const RENAME_BARE_STOPWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "group",
    "order",
    "limit",
    "offset",
    "union",
    "intersect",
    "except",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "on",
    "as",
    "and",
    "or",
    "not",
    "null",
    "true",
    "false",
    "case",
    "when",
    "then",
    "else",
    "end",
    "in",
    "is",
    "like",
    "between",
    "exists",
    "distinct",
    "all",
    "any",
    "some",
    "asc",
    "desc",
    "by",
    "having",
    "using",
    "values",
    "into",
    "insert",
    "update",
    "delete",
    "set",
    "table",
    "view",
    "with",
    "recursive",
    "lateral",
    "only",
    "returning",
    "window",
    "fetch",
    "first",
    "next",
    "rows",
    "row",
    "ties",
    "percent",
    "collate",
];

fn ident_replacement(new: &str, was_quoted: bool) -> String {
    let simple = !new.is_empty()
        && new
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !new.as_bytes()[0].is_ascii_digit();
    if was_quoted || !simple {
        format!("\"{}\"", new.replace('"', "\"\""))
    } else {
        new.to_string()
    }
}

/// Splice `old` -> `new` column references for relation `rel` into
/// `text`. `qualifiers` are the names that may qualify a reference
/// to `rel` (its name and FROM aliases). When `bare_ok` is false,
/// only qualified references are rewritten. Returns the new text
/// (unchanged when nothing matched).
fn rewrite_query_text(
    text: &str,
    old: &str,
    new: &str,
    qualifiers: &[String],
    bare_ok: bool,
) -> String {
    let toks = scan_sql_tokens(text);
    let is_old = |t: &SqlTok| -> bool {
        (t.kind == SqlTokKind::Ident && t.text == old)
            || (t.kind == SqlTokKind::QIdent && t.text == old)
    };
    let tok_is = |idx: usize, s: &str| -> bool {
        toks.get(idx).is_some_and(|t| {
            (t.kind == SqlTokKind::Ident && t.text == s)
                || (t.kind == SqlTokKind::Punct && t.text == s)
        })
    };
    let mut repl: Vec<(usize, usize, String)> = Vec::new();
    for i in 0..toks.len() {
        if !is_old(&toks[i]) {
            continue;
        }
        let prev_dot = i >= 1 && tok_is(i - 1, ".");
        if prev_dot {
            // Qualified reference: the qualifier is the token before
            // the dot (or the middle token of schema.qual.col).
            let qual = if i >= 4 && tok_is(i - 3, ".") {
                Some(toks[i - 2].text.clone())
            } else if i >= 2 {
                Some(toks[i - 2].text.clone())
            } else {
                None
            };
            match qual {
                Some(q) if qualifiers.iter().any(|x| x == &q) => {
                    repl.push((
                        toks[i].start,
                        toks[i].end,
                        ident_replacement(new, toks[i].kind == SqlTokKind::QIdent),
                    ));
                }
                _ => {}
            }
            continue;
        }
        // A name followed by '.' is a qualifier itself, not a column.
        if i + 1 < toks.len() && tok_is(i + 1, ".") {
            continue;
        }
        if !bare_ok {
            continue;
        }
        if RENAME_BARE_STOPWORDS.contains(&old) {
            continue;
        }
        // Alias definition after AS.
        if i >= 1 && tok_is(i - 1, "as") {
            continue;
        }
        // Function call: name immediately followed by '('.
        if i + 1 < toks.len() && tok_is(i + 1, "(") {
            continue;
        }
        // Table-name position after FROM / JOIN / INTO / UPDATE.
        if i >= 1
            && (tok_is(i - 1, "from")
                || tok_is(i - 1, "join")
                || tok_is(i - 1, "into")
                || tok_is(i - 1, "update"))
        {
            continue;
        }
        repl.push((
            toks[i].start,
            toks[i].end,
            ident_replacement(new, toks[i].kind == SqlTokKind::QIdent),
        ));
    }
    if repl.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    for (s, e, r) in repl {
        out.push_str(&text[pos..s]);
        out.push_str(&r);
        pos = e;
    }
    out.push_str(&text[pos..]);
    out
}

/// Collect (relation, alias) pairs for every FROM item in a select,
/// recursing into joins, derived subqueries and CTE bodies.
fn collect_from_pairs(sel: &SelectStmt, out: &mut Vec<(String, Option<String>)>) {
    fn walk_from(item: &FromItem, out: &mut Vec<(String, Option<String>)>) {
        match item {
            FromItem::Table { name, alias, .. } => {
                let base = name.rsplit('.').next().unwrap_or(name).to_string();
                out.push((base, alias.clone()));
            }
            FromItem::Derived { sub, .. } => collect_from_pairs(sub, out),
            FromItem::Join { left, right, .. } => {
                walk_from(left, out);
                walk_from(right, out);
            }
            _ => {}
        }
    }
    for cte in &sel.with {
        match &cte.body {
            crate::sql::CteBody::Simple(s) => collect_from_pairs(s, out),
            crate::sql::CteBody::Union { left, right, .. } => {
                collect_from_pairs(left, out);
                collect_from_pairs(right, out);
            }
            crate::sql::CteBody::Dml(_) => {}
        }
    }
    if let Some(root) = &sel.set_op {
        collect_from_pairs(&root.left, out);
        for b in &root.chain {
            collect_from_pairs(&b.right, out);
        }
        return;
    }
    for item in &sel.from {
        walk_from(item, out);
    }
}

/// Whether any FROM item for `rel` carries an explicit column-alias
/// list (references then use the alias names, not the base names).
fn from_has_col_aliases(sel: &SelectStmt, rel: &str) -> bool {
    fn walk(item: &FromItem, rel: &str) -> bool {
        match item {
            FromItem::Table {
                name, col_aliases, ..
            } => name.rsplit('.').next().unwrap_or(name) == rel && !col_aliases.is_empty(),
            FromItem::Derived { sub, .. } => from_has_col_aliases(sub, rel),
            FromItem::Join { left, right, .. } => walk(left, rel) || walk(right, rel),
            _ => false,
        }
    }
    if let Some(root) = &sel.set_op {
        return from_has_col_aliases(&root.left, rel)
            || root
                .chain
                .iter()
                .any(|b| from_has_col_aliases(&b.right, rel));
    }
    sel.from.iter().any(|i| walk(i, rel))
}

/// Collect every column reference (qualifier, name) in a select,
/// recursing into CTE bodies, derived subqueries, join conditions
/// and set-operation branches.
fn select_col_refs(sel: &SelectStmt, out: &mut Vec<(Option<String>, String)>) {
    for cte in &sel.with {
        match &cte.body {
            crate::sql::CteBody::Simple(s) => select_col_refs(s, out),
            crate::sql::CteBody::Union { left, right, .. } => {
                select_col_refs(left, out);
                select_col_refs(right, out);
            }
            crate::sql::CteBody::Dml(_) => {}
        }
    }
    if let Some(root) = &sel.set_op {
        select_col_refs(&root.left, out);
        for b in &root.chain {
            select_col_refs(&b.right, out);
        }
        for t in &root.order_by {
            crate::sql::collect_col_refs(&t.expr, out);
        }
        return;
    }
    fn walk_from(item: &FromItem, out: &mut Vec<(Option<String>, String)>) {
        match item {
            FromItem::Derived { sub, .. } => select_col_refs(sub, out),
            FromItem::Join {
                left, right, on, ..
            } => {
                walk_from(left, out);
                walk_from(right, out);
                if let Some(e) = on {
                    crate::sql::collect_col_refs(e, out);
                }
            }
            FromItem::Function { args, .. } => {
                for a in args {
                    crate::sql::collect_col_refs(a, out);
                }
            }
            FromItem::Values { rows, .. } => {
                for r in rows {
                    for e in r {
                        crate::sql::collect_col_refs(e, out);
                    }
                }
            }
            FromItem::Table { .. } => {}
        }
    }
    for item in &sel.items {
        if let SelectItem::Expr { expr, .. } = item {
            crate::sql::collect_col_refs(expr, out);
        }
    }
    for item in &sel.from {
        walk_from(item, out);
    }
    if let Some(w) = &sel.where_ {
        crate::sql::collect_col_refs(w, out);
    }
    for g in &sel.group_by {
        for e in g {
            crate::sql::collect_col_refs(e, out);
        }
    }
    if let Some(h) = &sel.having {
        crate::sql::collect_col_refs(h, out);
    }
    for t in &sel.order_by {
        crate::sql::collect_col_refs(&t.expr, out);
    }
    for e in &sel.distinct_on {
        crate::sql::collect_col_refs(e, out);
    }
}

/// Does renaming `rel.old` -> `new` change this select's output
/// column name for that column? True when an unaliased target item
/// is the column itself, or a star expansion covers the relation,
/// and the view has no explicit column alias pinning that position
/// (checked by the caller against the view's col_aliases).
fn select_output_renamed(sel: &SelectStmt, qualifiers: &[String], old: &str) -> bool {
    let items_owner: &SelectStmt = match &sel.set_op {
        Some(root) => &root.left,
        None => sel,
    };
    for item in &items_owner.items {
        match item {
            SelectItem::All => return true,
            SelectItem::AllOf(q) => {
                if qualifiers.iter().any(|x| x == q) {
                    return true;
                }
            }
            SelectItem::Expr { expr, alias } => {
                if alias.is_some() {
                    continue;
                }
                if let Expr::Column { table, name } = expr {
                    if name == old {
                        match table {
                            None => return true,
                            Some(q) => {
                                if qualifiers.iter().any(|x| x == q) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

struct ViewRenamePlan {
    new_query: Option<String>,
    output_renamed: bool,
    /// The view references rel.old in a way the rewrite could not
    /// cover; the caller must refuse the rename (2BP01).
    unrewritable: bool,
}

fn plan_view_rename(
    eng: &Engine,
    ctx: &StmtCtx,
    view: &crate::storage::ViewDef,
    rel: &str,
    old: &str,
    new: &str,
) -> ViewRenamePlan {
    let fail = ViewRenamePlan {
        new_query: None,
        output_renamed: false,
        unrewritable: true,
    };
    let stmt = match parse_statement(&view.query) {
        Ok(s) => s,
        Err(_) => return fail,
    };
    let sel = match stmt {
        Stmt::Select(s) => s,
        _ => return fail,
    };
    // Qualifiers that may reference rel in this query.
    let mut pairs = Vec::new();
    collect_from_pairs(&sel, &mut pairs);
    let mut qualifiers: Vec<String> = Vec::new();
    for (t, a) in &pairs {
        if t == rel {
            qualifiers.push(a.clone().unwrap_or_else(|| t.clone()));
            if a.is_some() {
                // The bare table name does not qualify once aliased
                // in PG; keep only the alias. (The name was pushed
                // above only when there is no alias.)
            }
        }
    }
    if qualifiers.is_empty() {
        // rel is named in deps but not in the FROM walk (e.g. only
        // inside an expression subquery): be conservative — if the
        // query references `old` at all, treat as unrewritable.
        let mut refs = Vec::new();
        select_col_refs(&sel, &mut refs);
        if refs.iter().any(|(_, n)| n == old) {
            return fail;
        }
        return ViewRenamePlan {
            new_query: None,
            output_renamed: false,
            unrewritable: false,
        };
    }
    // JOIN ... USING (old) over rel cannot be expressed after a
    // one-sided rename in the text model.
    fn using_has_old(sel: &SelectStmt, rel: &str, old: &str) -> bool {
        fn subtree_has_rel(item: &FromItem, rel: &str) -> bool {
            match item {
                FromItem::Table { name, .. } => name.rsplit('.').next().unwrap_or(name) == rel,
                FromItem::Join { left, right, .. } => {
                    subtree_has_rel(left, rel) || subtree_has_rel(right, rel)
                }
                _ => false,
            }
        }
        fn walk(item: &FromItem, rel: &str, old: &str) -> bool {
            match item {
                FromItem::Join {
                    left, right, using, ..
                } => {
                    if using.iter().any(|u| u == old)
                        && (subtree_has_rel(left, rel) || subtree_has_rel(right, rel))
                    {
                        return true;
                    }
                    walk(left, rel, old) || walk(right, rel, old)
                }
                FromItem::Derived { sub, .. } => using_has_old(sub, rel, old),
                _ => false,
            }
        }
        if let Some(root) = &sel.set_op {
            return using_has_old(&root.left, rel, old)
                || root.chain.iter().any(|b| using_has_old(&b.right, rel, old));
        }
        sel.from.iter().any(|i| walk(i, rel, old))
    }
    // References to rel.old: qualified by one of rel's qualifiers,
    // or bare (decided below).
    let mut refs = Vec::new();
    select_col_refs(&sel, &mut refs);
    let qualified_ref = refs.iter().any(|(q, n)| {
        n == old
            && q.as_ref()
                .is_some_and(|q| qualifiers.iter().any(|x| x == q))
    });
    let bare_ref = refs.iter().any(|(q, n)| n == old && q.is_none());
    if !qualified_ref && !bare_ref {
        // No reference to the renamed column (a star expansion is
        // dynamic and needs no text change, but does rename the
        // output).
        return ViewRenamePlan {
            new_query: None,
            output_renamed: !view.col_aliases.iter().any(|a| a == old)
                && select_output_renamed(&sel, &qualifiers, old),
            unrewritable: false,
        };
    }
    if from_has_col_aliases(&sel, rel) || using_has_old(&sel, rel, old) {
        return fail;
    }
    // Bare references are safe only when no OTHER relation in the
    // view exposes a column named `old`.
    let bare_ok = if bare_ref {
        let mut ok = true;
        for d in &view.deps {
            if d == rel {
                continue;
            }
            if let Some(t) = eng.db.find_table(d, ctx.snap, &ctx.all_xids, ctx.session) {
                if t.columns.iter().any(|(cn, _)| cn == old) {
                    ok = false;
                }
            } else if let Some(dv) = eng.db.find_view(d, ctx.snap, ctx.own) {
                if let Ok(names) = view_output_names(eng, ctx, dv) {
                    if names.iter().any(|n| n == old) {
                        ok = false;
                    }
                } else {
                    ok = false;
                }
            } else {
                ok = false;
            }
        }
        ok
    } else {
        false
    };
    if bare_ref && !bare_ok && !qualified_ref {
        return fail;
    }
    let new_text = rewrite_query_text(&view.query, old, new, &qualifiers, bare_ok);
    if new_text == view.query {
        return fail;
    }
    // Verify by re-parsing: no qualified reference to rel.old may
    // remain; bare references may remain only when they provably
    // belong to another relation (bare_ok false but a qualified
    // reference was rewritten).
    let verify = parse_statement(&new_text);
    if let Ok(Stmt::Select(sel2)) = verify {
        let mut refs2 = Vec::new();
        select_col_refs(&sel2, &mut refs2);
        let qualified_left = refs2.iter().any(|(q, n)| {
            n == old
                && q.as_ref()
                    .is_some_and(|q| qualifiers.iter().any(|x| x == q))
        });
        if qualified_left {
            return fail;
        }
        let bare_left = refs2.iter().any(|(q, n)| n == old && q.is_none());
        if bare_left && bare_ok {
            return fail;
        }
    } else {
        return fail;
    }
    ViewRenamePlan {
        new_query: Some(new_text),
        output_renamed: !view.col_aliases.iter().any(|a| a == old)
            && select_output_renamed(&sel, &qualifiers, old),
        unrewritable: false,
    }
}

/// Rewrite every dependent view (transitively) for a column rename
/// `rel.old` -> `rel.new`. Errors 2BP01 with the historical message
/// when a dependent view cannot be rewritten safely.
pub(crate) fn rewrite_views_for_rename(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    rel: &str,
    old: &str,
    new: &str,
) -> Result<(), ExecError> {
    let mut queue: Vec<(String, String, String)> =
        vec![(rel.to_string(), old.to_string(), new.to_string())];
    let mut processed: Vec<(String, String)> = Vec::new();
    while let Some((r, o, n)) = queue.pop() {
        // Snapshot dependent view names first (rewriting mutates
        // the view map).
        let dependents: Vec<String> = eng
            .db
            .views
            .iter()
            .filter(|(vname, vs)| {
                vname.as_str() != r
                    && vs.iter().any(|v| {
                        crate::storage::view_visible(v, ctx.snap, ctx.own)
                            && v.deps.iter().any(|d| d == &r)
                    })
            })
            .map(|(vname, _)| vname.clone())
            .collect();
        for vname in dependents {
            if processed.contains(&(vname.clone(), o.clone())) {
                continue;
            }
            processed.push((vname.clone(), o.clone()));
            let view = match eng.db.find_view(&vname, ctx.snap, ctx.own) {
                Some(v) => v.clone(),
                None => continue,
            };
            let plan = plan_view_rename(eng, ctx, &view, &r, &o, &n);
            if plan.unrewritable {
                return Err(exec_err(
                    "2BP01",
                    format!(
                        "cannot rename column {} because view {} depends on table {}",
                        o, vname, r
                    ),
                ));
            }
            if let Some(nq) = plan.new_query {
                replace_view_def(eng, ctx, &vname, Some(nq), None)?;
            }
            if plan.output_renamed {
                queue.push((vname.clone(), o.clone(), n.clone()));
            }
        }
    }
    Ok(())
}

/// v1.97: ALTER VIEW / ALTER MATERIALIZED VIEW name RENAME COLUMN
/// (also reached from ALTER TABLE addressed at a view). The view's
/// output names are materialized into its column-alias list with
/// the one entry renamed; dependent views are rewritten as for a
/// table column rename.
pub(crate) fn exec_alter_view_rename_column(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    old: &str,
    new: &str,
) -> Result<ExecResult, ExecError> {
    exec_alter_view_rename_column_tag(eng, ctx, name, old, new, "ALTER VIEW")
}

pub(crate) fn exec_alter_view_rename_column_tag(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    old: &str,
    new: &str,
    tag: &str,
) -> Result<ExecResult, ExecError> {
    let view = eng
        .db
        .find_view(name, ctx.snap, ctx.own)
        .ok_or_else(|| exec_err("42P01", format!("view \"{}\" does not exist", name)))?
        .clone();
    require_view_owner(eng, ctx, name)?;
    let names = view_output_names(eng, ctx, &view)?;
    let idx = names.iter().position(|n| n == old).ok_or_else(|| {
        exec_err(
            "42703",
            format!("column \"{}\" of relation \"{}\" does not exist", old, name),
        )
    })?;
    if names.iter().any(|n| n == new) {
        return Err(exec_err(
            "42701",
            format!("column \"{}\" of relation \"{}\" already exists", new, name),
        ));
    }
    let mut aliases = names;
    aliases[idx] = new.to_string();
    // Rewrite dependents first: if one cannot be rewritten the
    // statement fails before anything is replaced (the transaction
    // would roll back anyway, but fail-fast keeps the error path
    // identical to the table case).
    rewrite_views_for_rename(eng, ctx, name, old, new)?;
    replace_view_def(eng, ctx, name, None, Some(aliases))?;
    Ok(ExecResult::Command {
        tag: tag.to_string(),
    })
}
