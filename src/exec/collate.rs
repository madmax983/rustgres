//! v2.21: the collation subsystem — collation derivation and conflict
//! rules, grounded in PG19 `parse_collate.c`, `parse_cte.c`,
//! `commands/view.c`, `commands/collationcmds.c` and `like.c` /
//! `regc_pg_locale.c` / `catalog/index.c` for the nondeterministic
//! restrictions.
//!
//! Model: every query level gets a derivation environment
//! ([`CollEnv`]) built from its FROM clause (base tables carry their
//! stored per-column collations, domains contribute theirs, views
//! carry theirs, derived tables / CTEs / VALUES carry computed output
//! collations). [`derive`] walks an expression mirroring
//! `assign_expr_collations`: COLLATE is EXPLICIT, column references
//! and type coercions are IMPLICIT, unknown-typed literals carry no
//! collation. Conflicts between two different non-default IMPLICIT
//! collations are deferred (strength CONFLICT) and error only at a
//! consumption point (comparison -> 42P22, sort/group/distinct ->
//! 42P21), exactly like PG; two different EXPLICIT collations error
//! immediately (42P21) wherever they meet.
//!
//! The pass is a *validation* pass: it runs at the top of
//! `run_select` for every query level and never changes evaluation.
//! The one semantic gap versus PG is function bodies: argument
//! collations are not propagated into SQL/plpgsql function bodies
//! (held back, see the v2.21 scope note).
//!
//! Storage note: column / domain / view collations and the CREATE
//! COLLATION registry are runtime-only in v2.21 (not WAL-logged),
//! the same durability class as user-defined casts.

use super::describe::describe_select;
use super::*;
use crate::sql::col_type_collatable;
use crate::sql::{CteBody, Expr, FromItem, Literal, SelectItem, SelectStmt};
use crate::storage::Snapshot;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Collation assignment state (parse_collate.c CollatingState)
// ---------------------------------------------------------------------------

/// Strength of a collation assignment, weakest first. The ordering
/// is load-bearing: merge keeps the stronger side.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum CollStrength {
    /// No collation (non-collatable type or unknown-typed input).
    None,
    /// Derived from context (column, coercion, domain).
    Implicit,
    /// Two irreconcilable implicit collations met; the error is
    /// deferred to the consumption point, as in PG.
    Conflict,
    /// An explicit COLLATE clause.
    Explicit,
}

#[derive(Clone, Debug)]
pub(crate) struct CollState {
    /// The collation name; `None` at Implicit strength means the
    /// default collation. At Conflict strength this is the first of
    /// the two conflicting names (`other` is the second).
    pub name: Option<String>,
    pub strength: CollStrength,
    pub other: Option<String>,
}

impl CollState {
    pub(crate) fn none() -> CollState {
        CollState {
            name: None,
            strength: CollStrength::None,
            other: None,
        }
    }
    pub(crate) fn implicit(name: Option<String>) -> CollState {
        CollState {
            name,
            strength: CollStrength::Implicit,
            other: None,
        }
    }
    pub(crate) fn explicit(name: Option<String>) -> CollState {
        CollState {
            name,
            strength: CollStrength::Explicit,
            other: None,
        }
    }
    fn conflict(a: Option<String>, b: Option<String>) -> CollState {
        CollState {
            name: a,
            strength: CollStrength::Conflict,
            other: b,
        }
    }
    /// PG: an implicit *default* collation loses to any non-default
    /// implicit collation instead of conflicting with it.
    fn is_default(&self) -> bool {
        self.strength == CollStrength::Implicit && self.name.is_none()
    }
    /// Name as PG prints it in error messages.
    pub(crate) fn display(name: &Option<String>) -> String {
        name.clone().unwrap_or_else(|| "default".to_string())
    }
}

/// Normalize a COLLATE name: the default collation is represented
/// internally as `None`.
fn normalize_name(name: &str) -> Option<String> {
    if name.eq_ignore_ascii_case("default") {
        None
    } else {
        Some(name.to_string())
    }
}

fn err_explicit_conflict(a: &Option<String>, b: &Option<String>) -> ExecError {
    exec_err(
        "42P21",
        format!(
            "collation mismatch between explicit collations \"{}\" and \"{}\"",
            CollState::display(a),
            CollState::display(b)
        ),
    )
}

fn err_implicit_conflict(a: &Option<String>, b: &Option<String>) -> ExecError {
    exec_err(
        "42P21",
        format!(
            "collation mismatch between implicit collations \"{}\" and \"{}\"",
            CollState::display(a),
            CollState::display(b)
        ),
    )
}

/// `merge_collation_state`: combine two assignment states. Errors
/// only on the explicit/explicit mismatch, like PG.
pub(crate) fn merge_coll(a: &CollState, b: &CollState) -> Result<CollState, ExecError> {
    if b.strength > a.strength {
        // PG: when the stronger side is a deferred conflict, the
        // weaker side's collation joins the conflict pair (used only
        // for messages); approximate by keeping b as-is.
        return Ok(b.clone());
    }
    if a.strength > b.strength {
        return Ok(a.clone());
    }
    match a.strength {
        CollStrength::None | CollStrength::Conflict => Ok(a.clone()),
        CollStrength::Implicit => {
            if a.name == b.name {
                Ok(a.clone())
            } else if a.is_default() {
                Ok(b.clone())
            } else if b.is_default() {
                Ok(a.clone())
            } else {
                Ok(CollState::conflict(a.name.clone(), b.name.clone()))
            }
        }
        CollStrength::Explicit => {
            if a.name == b.name {
                Ok(a.clone())
            } else {
                Err(err_explicit_conflict(&a.name, &b.name))
            }
        }
    }
}

/// Consumption point for comparison-like operations: a deferred
/// conflict becomes 42P22 `could not determine which collation to
/// use for <what>`.
pub(crate) fn require_determined(st: &CollState, what: &str) -> Result<(), ExecError> {
    if st.strength == CollStrength::Conflict {
        return Err(exec_err(
            "42P22",
            format!("could not determine which collation to use for {}", what),
        ));
    }
    Ok(())
}

/// Consumption point for sort-group targets (ORDER BY / GROUP BY /
/// DISTINCT / set-op resolution): a deferred conflict becomes the
/// 42P21 implicit-mismatch error.
pub(crate) fn require_sort_group(st: &CollState) -> Result<(), ExecError> {
    if st.strength == CollStrength::Conflict {
        return Err(err_implicit_conflict(&st.name, &st.other));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Collation name services
// ---------------------------------------------------------------------------

/// ISO 639-1 two-letter language codes (plus a few 639-2 codes the
/// regression suites use). Used to decide whether an ICU-style or
/// libc-style collation name is plausible: PG resolves names against
/// the OS/ICU catalogs, which this engine does not carry, so names
/// are validated by shape. `foo`, `foo-x-icu` and `nonsense` are
/// rejected because their language subtags are not real languages;
/// `ja_JP.eucjp` is rejected because its encoding is not UTF8.
const LANGUAGE_CODES: &[&str] = &[
    "aa", "ab", "ae", "af", "ak", "am", "an", "ar", "as", "av", "ay", "az", "ba", "be", "bg", "bh",
    "bi", "bm", "bn", "bo", "br", "bs", "ca", "ce", "ch", "co", "cr", "cs", "cu", "cv", "cy", "da",
    "de", "dv", "dz", "ee", "el", "en", "eo", "es", "et", "eu", "fa", "ff", "fi", "fj", "fo", "fr",
    "fy", "ga", "gd", "gl", "gn", "gu", "gv", "ha", "he", "hi", "ho", "hr", "ht", "hu", "hy", "ia",
    "id", "ie", "ig", "ii", "ik", "io", "is", "it", "iu", "ja", "jv", "ka", "kg", "ki", "kj", "kk",
    "kl", "km", "kn", "ko", "kr", "ks", "ku", "kv", "kw", "ky", "la", "lb", "lg", "li", "ln", "lo",
    "lt", "lu", "lv", "mg", "mh", "mi", "mk", "ml", "mn", "mr", "ms", "mt", "my", "na", "nb", "nd",
    "ne", "ng", "nl", "nn", "no", "nr", "nv", "ny", "oc", "oj", "om", "or", "os", "pa", "pi", "pl",
    "ps", "pt", "qu", "rm", "rn", "ro", "ru", "rw", "sa", "sc", "sd", "se", "sg", "si", "sk", "sl",
    "sm", "sn", "so", "sq", "sr", "ss", "st", "su", "sv", "sw", "ta", "te", "tg", "th", "ti", "tk",
    "tl", "tn", "to", "tr", "ts", "tt", "ty", "ug", "uk", "ur", "uz", "ve", "vi", "vo", "wa", "wo",
    "xh", "yi", "yo", "za", "zh", "zu", "fil", "ceb", "hmn", "haw", "und",
];

fn is_language_code(s: &str) -> bool {
    LANGUAGE_CODES.contains(&s)
}

/// Names that always exist and are deterministic, independent of
/// the CREATE COLLATION registry.
fn is_builtin_collation(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "default"
            | "c"
            | "posix"
            | "c.utf-8"
            | "c.utf8"
            | "pg_c_utf8"
            | "pg_unicode_fast"
            | "ucs_basic"
            | "unicode"
    )
}

/// Pragmatic shape check for names PG would resolve against the
/// libc or ICU catalogs: BCP-47-style tags with a real language
/// subtag (optionally suffixed `-x-icu`), and libc
/// `lang[_TERRITORY][.UTF-8]` names.
fn plausible_catalog_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if let Some(base) = lower.strip_suffix("-x-icu") {
        // ICU form: hyphenated tag, first subtag a real language
        // (or the special `root` tag).
        if base == "root" {
            return true;
        }
        let mut parts = base.split('-');
        let lang = parts.next().unwrap_or("");
        if !is_language_code(lang) {
            return false;
        }
        return parts.all(|p| {
            !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()) && p.len() <= 8
        });
    }
    if lower.contains('-') {
        // A hyphenated tag without the ICU suffix (e.g. a bare
        // BCP-47 locale used as an ICU collation name).
        let mut parts = lower.split('-');
        let lang = parts.next().unwrap_or("");
        return is_language_code(lang)
            && parts.all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()));
    }
    // libc form: lang[_CC][.enc][@modifier]; only UTF8 encodings
    // exist in this UTF8 database.
    let (main, _modifier) = match lower.split_once('@') {
        Some((m, _)) => (m, ()),
        None => (lower.as_str(), ()),
    };
    let (langpart, enc) = match main.split_once('.') {
        Some((l, e)) => (l, Some(e)),
        None => (main, None),
    };
    if let Some(e) = enc {
        if !matches!(e, "utf8" | "utf-8") {
            return false;
        }
    }
    let mut segs = langpart.split('_');
    let lang = segs.next().unwrap_or("");
    if !is_language_code(lang) {
        return false;
    }
    match segs.next() {
        None => true,
        Some(cc) => {
            cc.len() == 2 && cc.chars().all(|c| c.is_ascii_alphabetic()) && segs.next().is_none()
        }
    }
}

/// Does this collation name resolve in this database?
pub(crate) fn collation_known(db: &crate::storage::Database, name: &str) -> bool {
    is_builtin_collation(name) || db.collations.contains_key(name) || plausible_catalog_name(name)
}

/// 42704 if the name does not resolve.
pub(crate) fn validate_collation_name(
    db: &crate::storage::Database,
    name: &str,
) -> Result<(), ExecError> {
    if collation_known(db, name) {
        Ok(())
    } else {
        Err(exec_err(
            "42704",
            format!(
                "collation \"{}\" for encoding \"UTF8\" does not exist",
                name
            ),
        ))
    }
}

/// Is the named collation nondeterministic? Unregistered catalog
/// names are deterministic in PG unless created otherwise; only the
/// CREATE COLLATION registry carries the flag here.
pub(crate) fn collation_is_nondeterministic(
    db: &crate::storage::Database,
    name: &Option<String>,
) -> bool {
    match name {
        None => false,
        Some(n) => db
            .collations
            .get(n)
            .map(|d| !d.deterministic)
            .unwrap_or(false),
    }
}

/// The collation declared on a domain (following domain-over-domain
/// chains, outermost first, as CoerceToDomain applies them).
pub(crate) fn domain_collation(db: &crate::storage::Database, dname: &str) -> Option<String> {
    let mut cur = dname.to_string();
    for _ in 0..16 {
        let st = db.types.get(&cur)?;
        let dom = st.domain.as_ref()?;
        if let Some(c) = &dom.collation {
            return Some(c.clone());
        }
        match &dom.base_domain {
            Some(inner) => cur = inner.clone(),
            None => return None,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Derivation context and environment
// ---------------------------------------------------------------------------

/// Everything the derivation pass needs from the execution context.
pub(crate) struct CollCtx<'a> {
    pub eng: &'a Engine,
    pub snap: &'a Snapshot,
    pub own: u64,
    pub session: u64,
}

impl<'a> CollCtx<'a> {
    pub(crate) fn from_stmt(eng: &'a Engine, ctx: &'a StmtCtx) -> CollCtx<'a> {
        CollCtx {
            eng,
            snap: &ctx.snap,
            own: ctx.own,
            session: ctx.session,
        }
    }
    fn db(&self) -> &crate::storage::Database {
        &self.eng.db
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EnvCol {
    pub name: String,
    pub ty: ColType,
    pub state: CollState,
}

#[derive(Clone, Debug)]
pub(crate) struct QualCols {
    pub qual: String,
    pub cols: Vec<EnvCol>,
}

/// One query level's column environment, chained to the enclosing
/// level's environment for correlated references.
pub(crate) struct CollEnv {
    pub quals: Vec<QualCols>,
    pub parent: Option<Rc<CollEnv>>,
}

impl CollEnv {
    pub(crate) fn empty() -> CollEnv {
        CollEnv {
            quals: Vec::new(),
            parent: None,
        }
    }
    /// Resolve a column reference. Qualified references search only
    /// the named range; unqualified references take the first match
    /// in scope order (this engine's resolver is likewise
    /// first-match for the shapes that reach execution).
    pub(crate) fn resolve(&self, qual: Option<&str>, name: &str) -> Option<EnvCol> {
        if let Some(q) = qual {
            for qc in &self.quals {
                if qc.qual == q {
                    if let Some(c) = qc.cols.iter().find(|c| c.name == name) {
                        return Some(c.clone());
                    }
                }
            }
            return self.parent.as_ref().and_then(|p| p.resolve(qual, name));
        }
        for qc in &self.quals {
            if let Some(c) = qc.cols.iter().find(|c| c.name == name) {
                return Some(c.clone());
            }
        }
        self.parent.as_ref().and_then(|p| p.resolve(qual, name))
    }
}

/// The assignment state of a plain column with the given stored
/// collation: collatable columns carry an implicit collation (the
/// stored one, or the default), other columns carry none.
fn column_state(ty: &ColType, stored: Option<String>) -> CollState {
    if col_type_collatable(ty, None) {
        CollState::implicit(stored)
    } else {
        CollState::none()
    }
}

// ---------------------------------------------------------------------------
// Environments and output collations
// ---------------------------------------------------------------------------

/// Recursion guard for self-referential environment computation
/// (recursive CTEs are cut off structurally before this matters).
const MAX_ENV_DEPTH: usize = 10;

/// Precomputed output columns of each CTE visible at a level.
type CteOutputs = Vec<(String, Vec<EnvCol>)>;

/// Output columns (name, type, assignment state) of a SELECT
/// statement, computed without executing it. For a set-operation
/// carrier the branch states are folded position-wise: ALL links
/// defer conflicts, DISTINCT links resolve eagerly and error on an
/// implicit conflict (PG `transformSetOperationTree`).
pub(crate) fn stmt_output_states(
    ctx: &CollCtx,
    stmt: &SelectStmt,
    parent: Option<&CollEnv>,
    depth: usize,
) -> Result<Vec<EnvCol>, ExecError> {
    if depth > MAX_ENV_DEPTH {
        return Ok(Vec::new());
    }
    if let Some(root) = &stmt.set_op {
        let mut acc = stmt_output_states(ctx, &root.left, parent, depth + 1)?;
        for b in &root.chain {
            let bs = stmt_output_states(ctx, &b.right, parent, depth + 1)?;
            if bs.len() != acc.len() {
                return Ok(acc);
            }
            for (a, bcol) in acc.iter_mut().zip(bs.iter()) {
                let merged = merge_coll(&a.state, &bcol.state)?;
                if !b.all && merged.strength == CollStrength::Conflict {
                    return Err(err_implicit_conflict(&merged.name, &merged.other));
                }
                a.state = merged;
            }
        }
        return Ok(acc);
    }
    let env = build_env(ctx, stmt, parent, None, depth);
    let mut out: Vec<EnvCol> = Vec::new();
    for item in &stmt.items {
        match item {
            SelectItem::All => {
                for qc in &env.quals {
                    out.extend(qc.cols.iter().cloned());
                }
            }
            SelectItem::AllOf(qual) => {
                if let Some(qc) = env.quals.iter().find(|q| &q.qual == qual) {
                    out.extend(qc.cols.iter().cloned());
                }
            }
            SelectItem::Expr { expr, alias } => {
                let state = derive(ctx, &env, expr)?;
                let name = alias.clone().unwrap_or_else(|| match expr {
                    Expr::Column { name, .. } => name.clone(),
                    _ => "?column?".to_string(),
                });
                out.push(EnvCol {
                    name,
                    ty: lite_type(ctx, &env, expr),
                    state,
                });
            }
        }
    }
    Ok(out)
}

/// Build the derivation environment for one query level.
fn build_env(
    ctx: &CollCtx,
    stmt: &SelectStmt,
    parent: Option<&CollEnv>,
    override_cols: Option<(&str, Vec<EnvCol>)>,
    depth: usize,
) -> CollEnv {
    let mut env = CollEnv {
        quals: Vec::new(),
        parent: parent.map(|p| Rc::new(clone_env_shallow(p))),
    };
    // Precompute CTE outputs in declaration order; each body sees
    // the earlier CTEs (through the extra map consulted by
    // `from_item_cols`).
    let mut cte_outs: CteOutputs = Vec::new();
    for def in &stmt.with {
        let cols = cte_output_cols(ctx, def, &cte_outs, parent, depth);
        cte_outs.push((def.name.clone(), cols));
    }
    for item in &stmt.from {
        collect_from_item(
            ctx,
            item,
            &mut env,
            &cte_outs,
            override_cols.as_ref(),
            depth,
        );
    }
    env
}

fn clone_env_shallow(e: &CollEnv) -> CollEnv {
    CollEnv {
        quals: e.quals.clone(),
        parent: e.parent.clone(),
    }
}

/// Output columns of one CTE definition. A recursive definition's
/// outputs come from its non-recursive term (PG resolves the
/// recursive term against them, and the conflict check compares
/// the two).
fn cte_output_cols(
    ctx: &CollCtx,
    def: &crate::sql::CteDef,
    earlier: &CteOutputs,
    parent: Option<&CollEnv>,
    depth: usize,
) -> Vec<EnvCol> {
    if depth > MAX_ENV_DEPTH {
        return Vec::new();
    }
    match &def.body {
        CteBody::Simple(q) => {
            // Sibling CTEs are visible inside the body by name:
            // present them through a synthetic parent environment.
            let synth = CollEnv {
                quals: earlier
                    .iter()
                    .map(|(n, cols)| QualCols {
                        qual: n.clone(),
                        cols: cols.clone(),
                    })
                    .collect(),
                parent: parent.map(|p| Rc::new(clone_env_shallow(p))),
            };
            let mut cols = stmt_output_states(ctx, q, Some(&synth), depth + 1).unwrap_or_default();
            apply_col_aliases(&mut cols, &def.col_aliases);
            cols
        }
        CteBody::Union { left, .. } => {
            let synth = CollEnv {
                quals: earlier
                    .iter()
                    .map(|(n, cols)| QualCols {
                        qual: n.clone(),
                        cols: cols.clone(),
                    })
                    .collect(),
                parent: parent.map(|p| Rc::new(clone_env_shallow(p))),
            };
            let mut cols =
                stmt_output_states(ctx, left, Some(&synth), depth + 1).unwrap_or_default();
            apply_col_aliases(&mut cols, &def.col_aliases);
            cols
        }
        CteBody::Dml(_) => Vec::new(),
    }
}

fn apply_col_aliases(cols: &mut [EnvCol], aliases: &[String]) {
    if aliases.is_empty() {
        return;
    }
    for (c, a) in cols.iter_mut().zip(aliases.iter()) {
        c.name = a.clone();
    }
}

/// Append one FROM item's range variables to the environment.
#[allow(clippy::too_many_arguments)]
fn collect_from_item(
    ctx: &CollCtx,
    item: &FromItem,
    env: &mut CollEnv,
    cte_outs: &CteOutputs,
    override_cols: Option<&(&str, Vec<EnvCol>)>,
    depth: usize,
) {
    match item {
        FromItem::Table {
            name,
            alias,
            col_aliases,
            ..
        } => {
            let qual = alias.clone().unwrap_or_else(|| base_name(name));
            let mut cols = table_like_cols(ctx, name, cte_outs, override_cols);
            apply_col_aliases(&mut cols, col_aliases);
            env.quals.push(QualCols { qual, cols });
        }
        FromItem::Derived {
            sub,
            alias,
            col_aliases,
            ..
        } => {
            // LATERAL subqueries see the ranges built so far.
            let so_far = clone_env_shallow(env);
            let mut cols =
                stmt_output_states(ctx, sub, Some(&so_far), depth + 1).unwrap_or_default();
            apply_col_aliases(&mut cols, col_aliases);
            env.quals.push(QualCols {
                qual: alias.clone(),
                cols,
            });
        }
        FromItem::Values {
            rows,
            alias,
            col_aliases,
            ..
        } => {
            let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            let mut cols: Vec<EnvCol> = Vec::new();
            for i in 0..ncols {
                let mut st = CollState::none();
                let mut ty = ColType::Text;
                for (ri, row) in rows.iter().enumerate() {
                    if let Some(cell) = row.get(i) {
                        if let Ok(s) = derive(ctx, env, cell) {
                            if let Ok(m) = merge_coll(&st, &s) {
                                st = m;
                            }
                        }
                        if ri == 0 {
                            ty = lite_type(ctx, env, cell);
                        }
                    }
                }
                let name = col_aliases
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("column{}", i + 1));
                cols.push(EnvCol {
                    name,
                    ty,
                    state: st,
                });
            }
            env.quals.push(QualCols {
                qual: alias.clone(),
                cols,
            });
        }
        FromItem::Function {
            name,
            alias,
            col_aliases,
            col_defs,
            ..
        } => {
            let qual = alias.clone().unwrap_or_else(|| base_name(name));
            let cols: Vec<EnvCol> = if !col_defs.is_empty() {
                col_defs
                    .iter()
                    .map(|(n, t, _)| EnvCol {
                        name: n.clone(),
                        ty: t.clone(),
                        state: column_state(t, None),
                    })
                    .collect()
            } else if !col_aliases.is_empty() {
                col_aliases
                    .iter()
                    .map(|n| EnvCol {
                        name: n.clone(),
                        ty: ColType::Text,
                        state: CollState::implicit(None),
                    })
                    .collect()
            } else {
                vec![EnvCol {
                    name: qual.clone(),
                    ty: ColType::Text,
                    state: CollState::implicit(None),
                }]
            };
            env.quals.push(QualCols { qual, cols });
        }
        FromItem::Join {
            left,
            right,
            alias,
            col_aliases,
            ..
        } => {
            let before = env.quals.len();
            collect_from_item(ctx, left, env, cte_outs, override_cols, depth);
            collect_from_item(ctx, right, env, cte_outs, override_cols, depth);
            if let Some(a) = alias {
                // A parenthesized-join alias hides the inner ranges.
                let mut cols: Vec<EnvCol> = Vec::new();
                for qc in env.quals.drain(before..) {
                    cols.extend(qc.cols);
                }
                apply_col_aliases(&mut cols, col_aliases);
                env.quals.push(QualCols {
                    qual: a.clone(),
                    cols,
                });
            }
        }
    }
}

/// Columns exposed by a table-like FROM reference: an explicit
/// override (the recursive-CTE working table), a CTE, a view, or a
/// base table, in that precedence order (PG: a CTE shadows a table
/// of the same name).
/// Does `view`'s dependency closure reach the view named `target`
/// (i.e. is the definition cyclic)? Bounded DFS over stored deps.
fn view_deps_reach(
    db: &crate::storage::Database,
    ctx: &CollCtx,
    view: &crate::storage::ViewDef,
    target: &str,
) -> bool {
    let mut visited: Vec<String> = Vec::new();
    let mut stack: Vec<String> = view.deps.clone();
    while let Some(n) = stack.pop() {
        let b = base_name(&n).to_string();
        if b == target {
            return true;
        }
        if visited.contains(&b) {
            continue;
        }
        visited.push(b);
        if visited.len() > 128 {
            return false;
        }
        if let Some(v) = db.find_view(&n, ctx.snap, ctx.own) {
            stack.extend(v.deps.iter().cloned());
        }
    }
    false
}

fn table_like_cols(
    ctx: &CollCtx,
    name: &str,
    cte_outs: &CteOutputs,
    override_cols: Option<&(&str, Vec<EnvCol>)>,
) -> Vec<EnvCol> {
    let base = base_name(name);
    if let Some((n, cols)) = override_cols {
        if *n == base {
            return cols.clone();
        }
    }
    if let Some((_, cols)) = cte_outs.iter().find(|(n, _)| n == &base) {
        return cols.clone();
    }
    let db = ctx.db();
    if let Some(view) = db.find_view(name, ctx.snap, ctx.own) {
        let view = view.clone();
        // Cyclic view definitions (lock.sql: lock_view2 replaced to
        // select from lock_view3, which selects from lock_view2) make
        // describe_select's expansion spin forever. When the view's
        // dependency closure reaches itself, synthesize the columns
        // from the stored aliases/collations instead of describing
        // (PG keeps the tuple descriptor from creation time; the
        // derivation states over plain columns are unaffected).
        if view_deps_reach(db, ctx, &view, &base) {
            let n = view.col_aliases.len().max(view.col_collations.len());
            return (0..n)
                .map(|i| EnvCol {
                    name: view
                        .col_aliases
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| "?column?".to_string()),
                    state: column_state(
                        &ColType::Text,
                        view.col_collations.get(i).cloned().unwrap_or(None),
                    ),
                    ty: ColType::Text,
                })
                .collect();
        }
        let mut cols: Vec<EnvCol> = Vec::new();
        if let Ok(Stmt::Select(sel)) = crate::sql::parse_statement(&view.query) {
            if let Ok(desc) =
                describe_select(ctx.eng, ctx.snap, ctx.own, ctx.session, &sel, &[], &[])
            {
                for (i, (cn, ty)) in desc.into_iter().enumerate() {
                    let stored = view.col_collations.get(i).cloned().unwrap_or(None);
                    cols.push(EnvCol {
                        name: view.col_aliases.get(i).cloned().unwrap_or(cn),
                        state: column_state(&ty, stored),
                        ty,
                    });
                }
            }
        }
        return cols;
    }
    if let Some(t) = db.find_table(name, ctx.snap, &[ctx.own], ctx.session) {
        return t
            .columns
            .iter()
            .enumerate()
            .map(|(i, (cn, ty))| {
                let stored = t.col_collation(i).map(|s| s.to_string()).or_else(|| {
                    t.domain_types
                        .get(i)
                        .cloned()
                        .flatten()
                        .and_then(|d| domain_collation(db, &d))
                });
                EnvCol {
                    name: cn.clone(),
                    ty: ty.clone(),
                    state: column_state(ty, stored),
                }
            })
            .collect();
    }
    Vec::new()
}

/// The relation name a FROM reference resolves by: schema
/// qualification is not part of the stored key.
fn base_name(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_string()
}

/// A light static type for an expression, used where output types
/// are needed without executing or describing. Falls back to Text
/// (collatable) when unknown — the safe direction for collation
/// purposes.
pub(crate) fn lite_type(ctx: &CollCtx, env: &CollEnv, e: &Expr) -> ColType {
    match e {
        Expr::Column { table, name } => env
            .resolve(table.as_deref(), name)
            .map(|c| c.ty)
            .unwrap_or(ColType::Text),
        Expr::ResolvedCol { .. } => ColType::Text,
        Expr::Literal(v) => match v {
            Literal::Int(_) | Literal::BigInt(_) | Literal::SmallInt(_) => ColType::Int,
            Literal::Float(_) | Literal::Real(_) | Literal::Decimal(_) => ColType::Float,
            Literal::Bool(_) => ColType::Bool,
            Literal::Date(_) => ColType::Date,
            Literal::Timestamp(_) | Literal::Timestamptz(_) => ColType::Timestamp,
            Literal::Interval(_) => ColType::Interval,
            Literal::Bytea(_) => ColType::Bytea,
            _ => ColType::Text,
        },
        Expr::Cast { to, .. } => to.clone(),
        Expr::CastNamed { name, .. } => crate::sql::coltype_by_name(name)
            .ok()
            .or_else(|| {
                ctx.db()
                    .types
                    .get(name)
                    .and_then(|t| t.domain.as_ref())
                    .map(|d| d.base)
            })
            .unwrap_or(ColType::Text),
        Expr::Collate { expr, .. } => lite_type(ctx, env, expr),
        Expr::Case { whens, else_, .. } => whens
            .first()
            .map(|(_, r)| lite_type(ctx, env, r))
            .or_else(|| else_.as_ref().map(|e| lite_type(ctx, env, e)))
            .unwrap_or(ColType::Text),
        Expr::ScalarSub(sel) => stmt_output_states(ctx, sel, Some(env), 9)
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|c| c.ty)
            .unwrap_or(ColType::Text),
        _ => ColType::Text,
    }
}

/// The statically-known type of an expression, when PG would know
/// it at parse time (used for the 42804 "collations are not
/// supported by type X" checks). Text literals are unknown-typed in
/// PG and return None here.
fn known_type(ctx: &CollCtx, env: &CollEnv, e: &Expr) -> Option<ColType> {
    match e {
        Expr::Column { table, name } => env.resolve(table.as_deref(), name).map(|c| c.ty),
        Expr::ResolvedCol { .. } => None,
        Expr::Literal(v) => match v {
            Literal::Int(_) | Literal::BigInt(_) | Literal::SmallInt(_) => Some(ColType::Int),
            Literal::Float(_) | Literal::Real(_) | Literal::Decimal(_) => Some(ColType::Float),
            Literal::Bool(_) => Some(ColType::Bool),
            Literal::Date(_) => Some(ColType::Date),
            Literal::Timestamp(_) | Literal::Timestamptz(_) => Some(ColType::Timestamp),
            Literal::Interval(_) => Some(ColType::Interval),
            Literal::Bytea(_) => Some(ColType::Bytea),
            _ => None,
        },
        Expr::Cast { to, .. } => Some(to.clone()),
        Expr::CastNamed { name, .. } => crate::sql::coltype_by_name(name).ok().or_else(|| {
            ctx.db()
                .types
                .get(name)
                .and_then(|t| t.domain.as_ref())
                .map(|d| d.base)
        }),
        Expr::Collate { expr, .. } => known_type(ctx, env, expr),
        Expr::ScalarSub(sel) => stmt_output_states(ctx, sel, Some(env), 0)
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|c| c.ty),
        _ => None,
    }
}

/// 42804 when an expression of statically-known non-collatable type
/// is given a collation (explicit COLLATE or `collation for`).
fn require_collatable_operand(ctx: &CollCtx, env: &CollEnv, e: &Expr) -> Result<(), ExecError> {
    if let Some(t) = known_type(ctx, env, e) {
        if !col_type_collatable(&t, None) {
            return Err(exec_err(
                "42804",
                format!(
                    "collations are not supported by type {}",
                    crate::sql::col_type_pg_name(&t, None)
                ),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Derivation (assign_expr_collations)
// ---------------------------------------------------------------------------

/// Derive the collation assignment state of an expression,
/// raising the errors PG raises during its collation walk:
/// unknown collation names (42704), COLLATE on a non-collatable
/// type (42804), explicit/explicit conflicts (42P21, immediately),
/// and deferred implicit conflicts at consumption points (42P22 /
/// 42P21 depending on the consumer).
pub(crate) fn derive(ctx: &CollCtx, env: &CollEnv, e: &Expr) -> Result<CollState, ExecError> {
    let db = ctx.db();
    match e {
        Expr::Column { table, name } => Ok(env
            .resolve(table.as_deref(), name)
            .map(|c| c.state)
            .unwrap_or_else(CollState::none)),
        Expr::Literal(_) | Expr::Param(_) | Expr::ResolvedCol { .. } | Expr::WholeRow { .. } => {
            Ok(CollState::none())
        }
        Expr::Collate { expr, name } => {
            validate_collation_name(db, name)?;
            require_collatable_operand(ctx, env, expr)?;
            derive(ctx, env, expr)?;
            Ok(CollState::explicit(normalize_name(name)))
        }
        Expr::CollationFor { expr } => {
            require_collatable_operand(ctx, env, expr)?;
            derive(ctx, env, expr)?;
            Ok(CollState::none())
        }
        Expr::Cast { expr, to, .. } => {
            let s = derive(ctx, env, expr)?;
            Ok(if col_type_collatable(to, None) {
                if s.strength == CollStrength::None {
                    CollState::implicit(None)
                } else {
                    s
                }
            } else {
                CollState::none()
            })
        }
        Expr::CastNamed { name, expr, .. } => {
            let s = derive(ctx, env, expr)?;
            // CoerceToDomain: a domain with its own collation wins.
            if let Some(dc) = domain_collation(db, name) {
                return Ok(CollState::implicit(Some(dc)));
            }
            let base = crate::sql::coltype_by_name(name).ok().or_else(|| {
                db.types
                    .get(name)
                    .and_then(|t| t.domain.as_ref())
                    .map(|d| d.base)
            });
            Ok(match base {
                Some(t) if col_type_collatable(&t, None) => {
                    if s.strength == CollStrength::None {
                        CollState::implicit(None)
                    } else {
                        s
                    }
                }
                Some(_) => CollState::none(),
                None => s,
            })
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(op) = operand {
                derive(ctx, env, op)?;
            }
            let mut acc = CollState::none();
            for (cond, res) in whens {
                derive(ctx, env, cond)?;
                acc = merge_coll(&acc, &derive(ctx, env, res)?)?;
            }
            if let Some(el) = else_ {
                acc = merge_coll(&acc, &derive(ctx, env, el)?)?;
            }
            Ok(acc)
        }
        Expr::Cmp { left, right, .. } => {
            let m = merge_coll(&derive(ctx, env, left)?, &derive(ctx, env, right)?)?;
            require_determined(&m, "string comparison")?;
            Ok(CollState::none())
        }
        Expr::IsDistinctFrom { left, right, .. } => {
            let m = merge_coll(&derive(ctx, env, left)?, &derive(ctx, env, right)?)?;
            require_determined(&m, "string comparison")?;
            Ok(CollState::none())
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            let mut m = derive(ctx, env, expr)?;
            m = merge_coll(&m, &derive(ctx, env, low)?)?;
            m = merge_coll(&m, &derive(ctx, env, high)?)?;
            require_determined(&m, "string comparison")?;
            Ok(CollState::none())
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ilike,
            ..
        } => {
            let m = merge_coll(&derive(ctx, env, expr)?, &derive(ctx, env, pattern)?)?;
            if let Some(esc) = escape {
                derive(ctx, env, esc)?;
            }
            require_determined(&m, if *ilike { "ILIKE" } else { "LIKE" })?;
            if *ilike && collation_is_nondeterministic(db, &m.name) {
                return Err(exec_err(
                    "0A000",
                    "nondeterministic collations are not supported for ILIKE".to_string(),
                ));
            }
            Ok(CollState::none())
        }
        Expr::Regex { expr, pattern, .. } => {
            let m = merge_coll(&derive(ctx, env, expr)?, &derive(ctx, env, pattern)?)?;
            require_determined(&m, "regular expression")?;
            if collation_is_nondeterministic(db, &m.name) {
                return Err(exec_err(
                    "0A000",
                    "nondeterministic collations are not supported for regular expressions"
                        .to_string(),
                ));
            }
            Ok(CollState::none())
        }
        Expr::Concat(a, b) => {
            let m = merge_coll(&derive(ctx, env, a)?, &derive(ctx, env, b)?)?;
            Ok(if m.strength == CollStrength::None {
                CollState::implicit(None)
            } else {
                m
            })
        }
        Expr::Arith { left, right, .. } => {
            // Merging surfaces explicit conflicts among the operands
            // even though the (numeric) result carries no collation.
            merge_coll(&derive(ctx, env, left)?, &derive(ctx, env, right)?)?;
            Ok(CollState::none())
        }
        Expr::InSub { expr, sub, .. } => {
            let sub_states = stmt_output_states(ctx, sub, Some(env), 0)?;
            if let Expr::Row(elems) = &**expr {
                if sub_states.len() == elems.len() && elems.len() > 1 {
                    for (el, ss) in elems.iter().zip(sub_states.iter()) {
                        let m = merge_coll(&derive(ctx, env, el)?, &ss.state)?;
                        require_determined(&m, "string hashing")?;
                    }
                    return Ok(CollState::none());
                }
            }
            let first = sub_states
                .first()
                .map(|c| c.state.clone())
                .unwrap_or_else(CollState::none);
            let m = merge_coll(&derive(ctx, env, expr)?, &first)?;
            require_determined(&m, "string hashing")?;
            Ok(CollState::none())
        }
        Expr::Quantified { left, sub, .. } => {
            let sub_states = stmt_output_states(ctx, sub, Some(env), 0)?;
            let first = sub_states
                .first()
                .map(|c| c.state.clone())
                .unwrap_or_else(CollState::none);
            let m = merge_coll(&derive(ctx, env, left)?, &first)?;
            require_determined(&m, "string comparison")?;
            Ok(CollState::none())
        }
        Expr::ScalarSub(sub) => {
            let states = stmt_output_states(ctx, sub, Some(env), 0)?;
            Ok(states
                .first()
                .map(|c| c.state.clone())
                .unwrap_or_else(CollState::none))
        }
        Expr::ArraySubquery(sub) => {
            stmt_output_states(ctx, sub, Some(env), 0)?;
            Ok(CollState::none())
        }
        Expr::Exists { sub, .. } => {
            stmt_output_states(ctx, sub, Some(env), 0)?;
            Ok(CollState::none())
        }
        Expr::Row(elems) => {
            // Row elements are independent: no cross-element merge.
            for el in elems {
                derive(ctx, env, el)?;
            }
            Ok(CollState::none())
        }
        Expr::ArrayCtor { elems, .. } => {
            let mut acc = CollState::none();
            for el in elems {
                acc = merge_coll(&acc, &derive(ctx, env, el)?)?;
            }
            Ok(acc)
        }
        Expr::Subscript { array, indices } => {
            let s = derive(ctx, env, array)?;
            for i in indices {
                derive(ctx, env, i)?;
            }
            Ok(s)
        }
        Expr::Slice { array, bounds } => {
            let s = derive(ctx, env, array)?;
            for (lo, hi) in bounds {
                if let Some(b) = lo {
                    derive(ctx, env, b)?;
                }
                if let Some(b) = hi {
                    derive(ctx, env, b)?;
                }
            }
            Ok(s)
        }
        Expr::Agg {
            func,
            arg,
            arg2,
            agg_order_by,
            filter,
            ..
        } => {
            let mut acc = CollState::none();
            if let Some(a) = arg {
                acc = merge_coll(&acc, &derive(ctx, env, a)?)?;
            }
            if let Some(a) = arg2 {
                acc = merge_coll(&acc, &derive(ctx, env, a)?)?;
            }
            // Aggregate ORDER BY terms are sort-group targets of
            // their own: a deferred conflict there errors 42P21,
            // and they do not feed the aggregate's own collation.
            for t in agg_order_by {
                let st = derive(ctx, env, &t.expr)?;
                require_sort_group(&st)?;
            }
            if let Some(f) = filter {
                derive(ctx, env, f)?;
            }
            // Only aggregates whose result type is the input type
            // (or an array of it) carry the input collation onward;
            // PG assigns no collation to a non-collatable result.
            Ok(match func {
                crate::sql::AggFunc::Min
                | crate::sql::AggFunc::Max
                | crate::sql::AggFunc::StringAgg
                | crate::sql::AggFunc::ArrayAgg => acc,
                _ => CollState::none(),
            })
        }
        Expr::WithinGroup {
            direct_args,
            within_order_by,
            filter,
            ..
        } => {
            // Hypothetical-set: each direct argument is unified
            // with its aggregated counterpart in a pair context; a
            // pair conflict errors immediately (42P21).
            let mut acc = CollState::none();
            for (i, d) in direct_args.iter().enumerate() {
                let ds = derive(ctx, env, d)?;
                if let Some(t) = within_order_by.get(i) {
                    let os = derive(ctx, env, &t.expr)?;
                    let pair = merge_coll(&ds, &os)?;
                    if pair.strength == CollStrength::Conflict {
                        return Err(err_implicit_conflict(&pair.name, &pair.other));
                    }
                    acc = merge_coll(&acc, &pair)?;
                } else {
                    acc = merge_coll(&acc, &ds)?;
                }
            }
            for t in within_order_by.iter().skip(direct_args.len()) {
                derive(ctx, env, &t.expr)?;
            }
            if let Some(f) = filter {
                derive(ctx, env, f)?;
            }
            Ok(acc)
        }
        Expr::Window {
            func,
            args,
            partition_by,
            order_by,
            filter,
            ..
        } => {
            let mut acc = CollState::none();
            for a in args {
                acc = merge_coll(&acc, &derive(ctx, env, a)?)?;
            }
            for p in partition_by {
                let st = derive(ctx, env, p)?;
                require_sort_group(&st)?;
            }
            for t in order_by {
                let st = derive(ctx, env, &t.expr)?;
                require_sort_group(&st)?;
            }
            if let Some(f) = filter {
                derive(ctx, env, f)?;
            }
            // Value-carrying window functions propagate their
            // argument's collation; ranking/aggregate-typed ones
            // with non-collatable results do not.
            Ok(match func {
                crate::sql::WindowFunc::Lag
                | crate::sql::WindowFunc::Lead
                | crate::sql::WindowFunc::FirstValue
                | crate::sql::WindowFunc::LastValue
                | crate::sql::WindowFunc::NthValue => acc,
                crate::sql::WindowFunc::Agg(
                    crate::sql::AggFunc::Min
                    | crate::sql::AggFunc::Max
                    | crate::sql::AggFunc::StringAgg
                    | crate::sql::AggFunc::ArrayAgg,
                ) => acc,
                _ => CollState::none(),
            })
        }
        Expr::Func { name, args } => {
            // SIMILAR TO desugars to a function call; PG applies
            // the regular-expression nondeterminism rule to it.
            if name == "similar_to" {
                let mut acc = CollState::none();
                for a in args {
                    acc = merge_coll(&acc, &derive(ctx, env, a)?)?;
                }
                if collation_is_nondeterministic(db, &acc.name) {
                    return Err(exec_err(
                        "0A000",
                        "nondeterministic collations are not supported for regular expressions"
                            .to_string(),
                    ));
                }
                return Ok(CollState::none());
            }
            let mut acc = CollState::none();
            for a in args {
                acc = merge_coll(&acc, &derive(ctx, env, a)?)?;
            }
            // The merged argument state propagates only when the
            // function's result is text-carrying; PG assigns no
            // collation to a non-collatable result (e.g. length()
            // over differently-collated inputs compares fine).
            const TEXTY: &[&str] = &[
                "lower",
                "upper",
                "concat",
                "concat_ws",
                "substring",
                "substr",
                "replace",
                "translate",
                "trim",
                "ltrim",
                "rtrim",
                "btrim",
                "left",
                "right",
                "lpad",
                "rpad",
                "repeat",
                "reverse",
                "initcap",
                "format",
                "split_part",
                "chr",
                "md5",
                "encode",
                "quote_literal",
                "quote_ident",
                "regexp_replace",
                "regexp_substr",
                "to_char",
                "overlay",
                "convert",
                "pg_collation_for",
                "collation_for",
            ];
            if TEXTY.contains(&name.as_str()) {
                Ok(acc)
            } else {
                Ok(CollState::none())
            }
        }
        Expr::UserOp { left, right, .. } => {
            merge_coll(&derive(ctx, env, left)?, &derive(ctx, env, right)?)?;
            Ok(CollState::none())
        }
        Expr::JsonArraySubq { query, .. } => {
            stmt_output_states(ctx, query, Some(env), 0)?;
            Ok(CollState::none())
        }
        _ => {
            // Generic node: merge all direct children (the
            // parse_collate default case). The node's own state is
            // the merged state; consumption points downstream
            // decide whether a conflict matters.
            let mut acc = CollState::none();
            let mut kids: Vec<&Expr> = Vec::new();
            collect_children(e, &mut kids);
            for k in kids {
                acc = merge_coll(&acc, &derive(ctx, env, k)?)?;
            }
            Ok(acc)
        }
    }
}

/// Direct child expressions of a node, for the generic derivation
/// fallback. Subqueries are deliberately excluded (they are
/// separate levels, reached through their own arms above).
fn collect_children<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Not(x) | Expr::Neg(x) | Expr::BitNot(x) | Expr::IsNull { expr: x, .. } => out.push(x),
        Expr::And(a, b) | Expr::Or(a, b) => {
            out.push(a);
            out.push(b);
        }
        Expr::FieldAccess { expr, .. } => out.push(expr),
        Expr::NamedArg { expr, .. } => out.push(expr),
        Expr::Extract { from, .. } => out.push(from),
        Expr::AtTimeZone { expr, zone } => {
            out.push(expr);
            if let Some(z) = zone {
                out.push(z);
            }
        }
        Expr::JsonOp { left, right, .. } => {
            out.push(left);
            out.push(right);
        }
        Expr::IsBool { expr, .. } => out.push(expr),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Statement-level validation (the run_select hook)
// ---------------------------------------------------------------------------

/// Validate one query level: build its environment and derive
/// every expression the level owns, applying the sort-group rule
/// to ORDER BY / GROUP BY / DISTINCT terms.
pub(crate) fn validate_select_collations(
    ctx: &CollCtx,
    stmt: &SelectStmt,
    outer: &[Scope],
) -> Result<(), ExecError> {
    let parent = outer_env(ctx, outer);
    if let Some(root) = &stmt.set_op {
        // Branches validate through their own run_select calls;
        // here only the folded output states and the root ORDER
        // BY (which resolves against the folded outputs) remain.
        let outs = stmt_output_states(ctx, stmt, parent.as_ref(), 0)?;
        if !root.order_by.is_empty() {
            let pseudo = CollEnv {
                quals: vec![QualCols {
                    qual: String::new(),
                    cols: outs.clone(),
                }],
                parent: None,
            };
            for t in &root.order_by {
                // An ordinal or bare output-column reference
                // resolves to the folded output state itself.
                let st = match &t.expr {
                    Expr::Literal(Literal::Int(n)) if *n >= 1 && (*n as usize) <= outs.len() => {
                        outs[*n as usize - 1].state.clone()
                    }
                    Expr::Column { table: None, name } if outs.iter().any(|c| &c.name == name) => {
                        outs.iter()
                            .find(|c| &c.name == name)
                            .map(|c| c.state.clone())
                            .unwrap_or_else(CollState::none)
                    }
                    _ => derive(ctx, &pseudo, &t.expr)?,
                };
                require_determined(&st, "string comparison")?;
            }
        }
        return Ok(());
    }
    let env = build_env(ctx, stmt, parent.as_ref(), None, 0);
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            let st = derive(ctx, &env, expr)?;
            if stmt.distinct {
                require_sort_group(&st)?;
            }
        }
    }
    if let Some(w) = &stmt.where_ {
        derive(ctx, &env, w)?;
    }
    if let Some(h) = &stmt.having {
        derive(ctx, &env, h)?;
    }
    for set in &stmt.group_by {
        for g in set {
            let st = derive(ctx, &env, g)?;
            require_sort_group(&st)?;
        }
    }
    for t in &stmt.order_by {
        let st = derive(ctx, &env, &t.expr)?;
        require_sort_group(&st)?;
    }
    for t in &stmt.distinct_on {
        let st = derive(ctx, &env, t)?;
        require_sort_group(&st)?;
    }
    // FROM-owned expressions: join conditions and table-function
    // arguments belong to this level.
    fn walk_from(ctx: &CollCtx, env: &CollEnv, item: &FromItem) -> Result<(), ExecError> {
        match item {
            FromItem::Join {
                left, right, on, ..
            } => {
                walk_from(ctx, env, left)?;
                walk_from(ctx, env, right)?;
                if let Some(on) = on {
                    derive(ctx, env, on)?;
                }
                Ok(())
            }
            FromItem::Function { args, .. } => {
                for a in args {
                    derive(ctx, env, a)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    for item in &stmt.from {
        walk_from(ctx, &env, item)?;
    }
    Ok(())
}

/// Parent environment from the enclosing query's scopes (used for
/// correlated subqueries): each outer range variable's columns get
/// the states their underlying table/view carries when the range
/// name resolves to one, else type-only states.
fn outer_env(ctx: &CollCtx, outer: &[Scope]) -> Option<CollEnv> {
    if outer.is_empty() {
        return None;
    }
    let mut quals: Vec<QualCols> = Vec::new();
    for sc in outer {
        for c in sc.schema.iter() {
            let entry = quals.iter_mut().find(|q: &&mut QualCols| q.qual == c.qual);
            match entry {
                Some(q) => q.cols.push(EnvCol {
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                    state: column_state(&c.ty, outer_col_stored(ctx, c)),
                }),
                None => quals.push(QualCols {
                    qual: c.qual.clone(),
                    cols: vec![EnvCol {
                        name: c.name.clone(),
                        ty: c.ty.clone(),
                        state: column_state(&c.ty, outer_col_stored(ctx, c)),
                    }],
                }),
            }
        }
    }
    Some(CollEnv {
        quals,
        parent: None,
    })
}

fn outer_col_stored(ctx: &CollCtx, c: &QCol) -> Option<String> {
    let db = ctx.db();
    if let Some(t) = db.find_table(&c.qual, ctx.snap, &[ctx.own], ctx.session) {
        if let Some((i, _)) = t
            .columns
            .iter()
            .enumerate()
            .find(|(_, (n, _))| n == &c.name)
        {
            if let Some(s) = t.col_collation(i) {
                return Some(s.to_string());
            }
            return t
                .domain_types
                .get(i)
                .cloned()
                .flatten()
                .and_then(|d| domain_collation(db, &d));
        }
    }
    if let Some(v) = db.find_view(&c.qual, ctx.snap, ctx.own) {
        if let Some(pos) = view_col_pos(ctx, v, &c.qual, &c.name) {
            return v.col_collations.get(pos).cloned().unwrap_or(None);
        }
    }
    None
}

fn view_col_pos(
    ctx: &CollCtx,
    v: &crate::storage::ViewDef,
    view_name: &str,
    name: &str,
) -> Option<usize> {
    if view_deps_reach(ctx.db(), ctx, v, &base_name(view_name)) {
        return v.col_aliases.iter().position(|a| a == name);
    }
    if let Ok(Stmt::Select(sel)) = crate::sql::parse_statement(&v.query) {
        if let Ok(desc) = describe_select(ctx.eng, ctx.snap, ctx.own, ctx.session, &sel, &[], &[]) {
            return desc.iter().position(|(n, _)| n == name);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Consumers: collation for / CTAS / views / recursive CTEs / indexes
// ---------------------------------------------------------------------------

/// `collation for (expr)` evaluation: the derived collation name
/// as a text value, NULL when the operand has no collation, 42804
/// for a statically-known non-collatable operand type.
pub(crate) fn eval_collation_for(
    q: &mut Q,
    scopes: &[Scope],
    expr: &Expr,
) -> Result<Value, ExecError> {
    let ctx = CollCtx {
        eng: q.eng,
        snap: q.snap,
        own: q.own,
        session: q.session,
    };
    let env = outer_env(&ctx, scopes).unwrap_or_else(CollEnv::empty);
    require_collatable_operand(&ctx, &env, expr)?;
    let st = derive(&ctx, &env, expr)?;
    Ok(match st.strength {
        CollStrength::None | CollStrength::Conflict => Value::Null,
        _ => Value::text(CollState::display(&st.name)),
    })
}

/// Output collations for CREATE TABLE AS: a position whose folded
/// state is a deferred conflict on a collatable type is the
/// createas.c error; otherwise the stored name (None = default).
pub(crate) fn ctas_output_collations(
    ctx: &CollCtx,
    select: &SelectStmt,
    columns: &[(String, ColType)],
) -> Result<Vec<Option<String>>, ExecError> {
    let outs = stmt_output_states(ctx, select, None, 0)?;
    let mut res = Vec::with_capacity(columns.len());
    for (i, (cn, ty)) in columns.iter().enumerate() {
        let st = outs.get(i).map(|c| c.state.clone());
        match st {
            Some(s) if s.strength == CollStrength::Conflict => {
                if col_type_collatable(ty, None) {
                    return Err(exec_err(
                        "42P22",
                        format!(
                            "no collation was derived for column \"{}\" with collatable type {}",
                            cn,
                            crate::sql::col_type_pg_name(ty, None)
                        ),
                    ));
                }
                res.push(None);
            }
            Some(s) => res.push(match s.strength {
                CollStrength::None => None,
                _ => s.name,
            }),
            None => res.push(None),
        }
    }
    Ok(res)
}

/// The parse_cte.c recursive-term check: the recursive term's
/// output collations (computed against the non-recursive term's
/// outputs) must not contradict the non-recursive term's.
pub(crate) fn check_recursive_cte(
    ctx: &CollCtx,
    name: &str,
    left: &SelectStmt,
    right: &SelectStmt,
    seed_schema: &[QCol],
) -> Result<(), ExecError> {
    let left_outs = stmt_output_states(ctx, left, None, 0)?;
    let seed_cols: Vec<EnvCol> = seed_schema
        .iter()
        .enumerate()
        .map(|(i, c)| EnvCol {
            name: c.name.clone(),
            ty: c.ty.clone(),
            state: left_outs
                .get(i)
                .map(|o| o.state.clone())
                .unwrap_or_else(CollState::none),
        })
        .collect();
    // The right term's outputs are computed against an
    // environment where the CTE name carries the seed outputs.
    let env = build_env(ctx, right, None, Some((name, seed_cols)), 0);
    let mut right_states: Vec<CollState> = Vec::new();
    if right.set_op.is_some() {
        // Set-op right terms fold through their own machinery;
        // the common recursive shape is a plain SELECT (handled
        // below). The override does not thread into the fold, so
        // these states are best-effort.
        let outs = stmt_output_states(ctx, right, None, 0)?;
        right_states = outs.iter().map(|c| c.state.clone()).collect();
    } else {
        for item in &right.items {
            match item {
                SelectItem::Expr { expr, .. } => {
                    right_states.push(derive(ctx, &env, expr)?);
                }
                SelectItem::All => {
                    for qc in &env.quals {
                        right_states.extend(qc.cols.iter().map(|c| c.state.clone()));
                    }
                }
                SelectItem::AllOf(qual) => {
                    if let Some(qc) = env.quals.iter().find(|q| &q.qual == qual) {
                        right_states.extend(qc.cols.iter().map(|c| c.state.clone()));
                    }
                }
            }
        }
    }
    for (i, (l, r)) in left_outs.iter().zip(right_states.iter()).enumerate() {
        let (ln, rn) = (&l.state.name, &r.name);
        let l_known = matches!(
            l.state.strength,
            CollStrength::Implicit | CollStrength::Explicit
        );
        let r_known = matches!(r.strength, CollStrength::Implicit | CollStrength::Explicit);
        if l_known && r_known {
            let lname = normalize_name(&CollState::display(ln));
            let rname = normalize_name(&CollState::display(rn));
            if lname != rname {
                return Err(exec_err(
                    "42P21",
                    format!(
                        "recursive query \"{}\" column {} has collation \"{}\" in non-recursive term but collation \"{}\" overall",
                        name,
                        i + 1,
                        CollState::display(ln),
                        CollState::display(rn)
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// CREATE INDEX collation validation: element COLLATE names must
/// resolve (42704); expression elements are derived against the
/// table environment (COLLATE on a non-collatable column is 42804,
/// unknown names 42704); a pattern operator class over a
/// nondeterministic element collation is rejected (PG
/// catalog/index.c).
pub(crate) fn validate_index_collations(
    ctx: &CollCtx,
    table_name: &str,
    table: &crate::storage::Table,
    columns: &[crate::sql::IndexColSpec],
) -> Result<(), ExecError> {
    let db = ctx.db();
    let env = CollEnv {
        quals: vec![QualCols {
            qual: table_name.to_string(),
            cols: table
                .columns
                .iter()
                .enumerate()
                .map(|(i, (cn, ty))| {
                    let stored = table.col_collation(i).map(|s| s.to_string()).or_else(|| {
                        table
                            .domain_types
                            .get(i)
                            .cloned()
                            .flatten()
                            .and_then(|d| domain_collation(db, &d))
                    });
                    EnvCol {
                        name: cn.clone(),
                        ty: ty.clone(),
                        state: column_state(ty, stored),
                    }
                })
                .collect(),
        }],
        parent: None,
    };
    for spec in columns {
        if let Some(cn) = &spec.collate {
            validate_collation_name(db, cn)?;
        }
        let mut elem_state: Option<CollState> = None;
        if let Some(expr_text) = &spec.expr {
            if let Ok(e) = crate::sql::parse_expr_str(expr_text) {
                elem_state = Some(derive(ctx, &env, &e)?);
                // An element-level COLLATE applies to the element
                // expression's result type (this also covers the
                // parenthesized `((a COLLATE "POSIX"))` shape and
                // a bare column whose collation the parser split
                // into the spec).
                if spec.collate.is_some() {
                    require_collatable_operand(ctx, &env, &e)?;
                }
            }
        } else if let Some(col_name) = &spec.name {
            if let Some(c) = env.quals[0].cols.iter().find(|c| &c.name == col_name) {
                elem_state = Some(c.state.clone());
            }
        }
        if let Some(oc) = &spec.opclass {
            let oc_base = oc.rsplit('.').next().unwrap_or(oc);
            if matches!(
                oc_base,
                "text_pattern_ops" | "bpchar_pattern_ops" | "varchar_pattern_ops"
            ) {
                // The element's effective collation: its explicit
                // COLLATE if present, else the derived state.
                let eff = match &spec.collate {
                    Some(cn) => Some(cn.clone()),
                    None => elem_state.as_ref().and_then(|s| s.name.clone()),
                };
                if collation_is_nondeterministic(db, &eff) {
                    return Err(exec_err(
                        "42P22",
                        format!(
                            "nondeterministic collations are not supported for operator class \"{}\"",
                            oc_base
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CREATE / DROP / ALTER COLLATION (collationcmds.c)
// ---------------------------------------------------------------------------

/// Validate an ICU locale string the way PG's
/// `pg_newlocale_from_collation` + ICU validation does at CREATE
/// COLLATION time. Returns the error message when invalid.
fn icu_locale_error(loc: &str) -> Option<String> {
    if let Some(kw) = loc.strip_prefix('@') {
        // Keyword form: @key=value;key=value — every keyword must
        // be a real ICU collation keyword.
        const KEYWORDS: &[&str] = &[
            "colstrength",
            "colcaselevel",
            "colnumeric",
            "colalternate",
            "colnormalization",
            "colcasefirst",
            "colbackwards",
            "colhiraganaquaternarymode",
            "ks",
            "kf",
            "kn",
            "ka",
            "kc",
            "kb",
            "kh",
            "kk",
        ];
        for part in kw.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let key = part
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !KEYWORDS.contains(&key.as_str()) || !part.contains('=') {
                return Some(format!(
                    "could not convert locale name \"{}\" to language tag: U_ILLEGAL_ARGUMENT_ERROR",
                    loc
                ));
            }
            // Boolean-valued keywords reject non-boolean values at
            // collator-open time in PG (testcoll_error1:
            // '@colNumeric=lower' -> U_ILLEGAL_ARGUMENT_ERROR).
            let value = part
                .split('=')
                .nth(1)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if matches!(
                key.as_str(),
                "colnumeric" | "kn" | "colbackwards" | "kb" | "colcaselevel" | "kc"
            ) && !matches!(value.as_str(), "true" | "false" | "yes" | "no")
            {
                return Some(format!(
                    "could not open collator for locale \"{}\": U_ILLEGAL_ARGUMENT_ERROR",
                    loc
                ));
            }
        }
        return None;
    }
    // Language part: everything before '-'/'_' separators and before
    // any '@' keyword section ('und@colCaseLevel=true' -> 'und').
    let lang = loc
        .split('@')
        .next()
        .unwrap_or("")
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if lang == "root" || lang.is_empty() || is_language_code(&lang) {
        None
    } else {
        Some(format!(
            "ICU locale \"{}\" has unknown language \"{}\"",
            loc,
            loc.split(['-', '_']).next().unwrap_or("")
        ))
    }
}

/// The hard (never GUC-gated) part of ICU keyword validation: a
/// keyword part without '=', or a known boolean-valued keyword
/// (colNumeric/colBackwards/colCaseLevel and short forms) carrying a
/// non-boolean value — PG fails these at collator-open time during
/// CREATE COLLATION regardless of icu_validation_level
/// (collate.icu.utf8 testcoll_error1: '@colNumeric=lower').
fn icu_keyword_hard_error(loc: &str) -> Option<String> {
    let kw = loc.strip_prefix('@')?;
    for part in kw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !part.contains('=') {
            return Some(format!(
                "could not convert locale name \"{}\" to language tag: U_ILLEGAL_ARGUMENT_ERROR",
                loc
            ));
        }
        let key = part
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let value = part
            .split('=')
            .nth(1)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "colnumeric" | "kn" | "colbackwards" | "kb" | "colcaselevel" | "kc"
        ) && !matches!(value.as_str(), "true" | "false" | "yes" | "no")
        {
            return Some(format!(
                "could not open collator for locale \"{}\": U_ILLEGAL_ARGUMENT_ERROR",
                loc
            ));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_create_collation(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    if_not_exists: bool,
    from: Option<&str>,
    provider: Option<&str>,
    locale: Option<&str>,
    lc_collate: Option<&str>,
    lc_ctype: Option<&str>,
    deterministic: Option<bool>,
) -> Result<ExecResult, ExecError> {
    let exists = eng.db.collations.contains_key(name) || is_builtin_collation(name);
    if exists {
        if if_not_exists {
            return Ok(ExecResult::Command {
                tag: "CREATE COLLATION".to_string(),
            });
        }
        return Err(exec_err(
            "42P06",
            format!("collation \"{}\" already exists", name),
        ));
    }
    let def = if let Some(src) = from {
        if src.eq_ignore_ascii_case("default") {
            return Err(exec_err(
                "22023",
                "collation \"default\" cannot be copied".to_string(),
            ));
        }
        validate_collation_name(&eng.db, src)?;
        match eng.db.collations.get(src) {
            Some(d) => d.clone(),
            None => crate::storage::CollationDef {
                deterministic: true,
                provider: 'c',
                locale: None,
                owner: ctx.role.to_string(),
            },
        }
    } else {
        let prov = provider
            .map(|p| p.to_ascii_lowercase())
            .unwrap_or_else(|| "libc".to_string());
        let (pchar, ploc) = match prov.as_str() {
            "builtin" | "b" => {
                let loc = locale.ok_or_else(|| {
                    exec_err(
                        "22023",
                        "parameter \"locale\" must be specified".to_string(),
                    )
                })?;
                let ok = matches!(loc, "C" | "C.UTF-8" | "PG_C_UTF8" | "PG_UNICODE_FAST")
                    || loc.eq_ignore_ascii_case("c.utf8");
                if !ok {
                    return Err(exec_err(
                        "22023",
                        format!("invalid locale name \"{}\" for builtin provider", loc),
                    ));
                }
                ('b', Some(loc.to_string()))
            }
            "icu" | "i" => {
                let loc = locale.ok_or_else(|| {
                    exec_err(
                        "22023",
                        "parameter \"locale\" must be specified".to_string(),
                    )
                })?;
                // Keyword-form locales ('@key=value') fail at
                // collator-open time in PG regardless of
                // icu_validation_level when a KNOWN boolean keyword
                // carries a non-boolean value (testcoll_error1) or a
                // keyword part is malformed. Unknown keywords and
                // unknown languages stay on the gated path below.
                if loc.starts_with('@') {
                    if let Some(msg) = icu_keyword_hard_error(loc) {
                        return Err(exec_err("22023", msg));
                    }
                }
                if let Some(msg) = icu_locale_error(loc) {
                    let level = super::installed_guc("icu_validation_level")
                        .unwrap_or_else(|| "warning".to_string());
                    if level == "error" {
                        return Err(exec_err("22023", msg));
                    }
                    // warning/disabled: the CREATE succeeds (the
                    // warning itself is not modeled).
                }
                ('i', Some(loc.to_string()))
            }
            "libc" | "c" => {
                // PG: lc_collate and lc_ctype must be specified
                // together for the libc provider (collate.linux.utf8
                // test3: "parameter \"lc_ctype\" must be specified").
                if locale.is_none() {
                    match (lc_collate.is_some(), lc_ctype.is_some()) {
                        (true, false) => {
                            return Err(exec_err(
                                "22023",
                                "parameter \"lc_ctype\" must be specified".to_string(),
                            ));
                        }
                        (false, true) => {
                            return Err(exec_err(
                                "22023",
                                "parameter \"lc_collate\" must be specified".to_string(),
                            ));
                        }
                        (false, false) => {
                            return Err(exec_err(
                                "22023",
                                "parameter \"locale\" must be specified".to_string(),
                            ));
                        }
                        (true, true) => {}
                    }
                }
                let loc = locale.or(lc_collate).ok_or_else(|| {
                    exec_err(
                        "22023",
                        "parameter \"locale\" must be specified".to_string(),
                    )
                })?;
                // The OS must know the locale: mirror PG's "could not
                // create locale" with a language-shape check (a bare
                // bogus word like 'nonsense' has no valid ISO language
                // subtag and no libc locale structure).
                let lang = loc
                    .split(['_', '.', '@'])
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let c_family = loc == "C"
                    || loc.starts_with("C.")
                    || loc.eq_ignore_ascii_case("posix")
                    || loc.starts_with("POSIX");
                if !c_family && !is_language_code(&lang) {
                    return Err(exec_err(
                        "22023",
                        format!(
                            "could not create locale \"{}\": No such file or directory",
                            loc
                        ),
                    ));
                }
                ('c', Some(loc.to_string()))
            }
            other => {
                return Err(exec_err(
                    "22023",
                    format!("unrecognized collation provider: {}", other),
                ));
            }
        };
        let _ = lc_ctype;
        crate::storage::CollationDef {
            deterministic: deterministic.unwrap_or(true),
            provider: pchar,
            locale: ploc,
            owner: ctx.role.to_string(),
        }
    };
    eng.db.collations.insert(name.to_string(), def);
    Ok(ExecResult::Command {
        tag: "CREATE COLLATION".to_string(),
    })
}

pub(crate) fn exec_drop_collation(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecResult, ExecError> {
    for name in names {
        if eng.db.collations.contains_key(name) {
            if !cascade && collation_in_use(&eng.db, name) {
                return Err(exec_err(
                    "2BP01",
                    format!(
                        "cannot drop collation {} because other objects depend on it",
                        name
                    ),
                ));
            }
            // CASCADE note: PG drops the dependent objects; this
            // engine only removes the registry entry (the stored
            // per-column names remain and resolve as catalog
            // names). Documented v2.21 narrowing.
            eng.db.collations.remove(name);
            continue;
        }
        if is_builtin_collation(name) {
            return Err(exec_err(
                "2BP01",
                format!(
                    "cannot drop collation {} because other objects depend on it",
                    name
                ),
            ));
        }
        if collation_known(&eng.db, name) {
            // An imported catalog collation: dropping it succeeds
            // (nothing is stored for it here).
            continue;
        }
        if if_exists {
            continue;
        }
        return Err(exec_err(
            "42704",
            format!("collation \"{}\" does not exist", name),
        ));
    }
    let _ = ctx;
    Ok(ExecResult::Command {
        tag: "DROP COLLATION".to_string(),
    })
}

fn collation_in_use(db: &crate::storage::Database, name: &str) -> bool {
    for versions in db.tables.values() {
        for t in versions {
            if t.col_collations.iter().any(|c| c.as_deref() == Some(name)) {
                return true;
            }
        }
    }
    for st in db.types.values() {
        if let Some(d) = &st.domain {
            if d.collation.as_deref() == Some(name) {
                return true;
            }
        }
    }
    for versions in db.views.values() {
        for v in versions {
            if v.col_collations.iter().any(|c| c.as_deref() == Some(name)) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn exec_alter_collation(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
    rename_to: Option<&str>,
    owner_to: Option<&str>,
    set_schema: Option<&str>,
    refresh_version: bool,
) -> Result<ExecResult, ExecError> {
    if !eng.db.collations.contains_key(name) {
        if !collation_known(&eng.db, name) {
            return Err(exec_err(
                "42704",
                format!("collation \"{}\" does not exist", name),
            ));
        }
        // Materialize an imported collation so the ALTER sticks.
        eng.db.collations.insert(
            name.to_string(),
            crate::storage::CollationDef {
                deterministic: true,
                provider: 'c',
                locale: None,
                owner: ctx.role.to_string(),
            },
        );
    }
    if let Some(new) = rename_to {
        if eng.db.collations.contains_key(new) || is_builtin_collation(new) {
            return Err(exec_err(
                "42P06",
                format!("collation \"{}\" already exists", new),
            ));
        }
        let def = eng.db.collations.remove(name).unwrap();
        eng.db.collations.insert(new.to_string(), def);
    }
    if let Some(owner) = owner_to {
        if eng.db.find_role(owner, ctx.snap, ctx.own).is_none() {
            return Err(exec_err(
                "42704",
                format!("role \"{}\" does not exist", owner),
            ));
        }
        if let Some(def) = eng.db.collations.get_mut(name) {
            def.owner = owner.to_string();
        }
    }
    // SET SCHEMA is a no-op in the single-namespace registry;
    // REFRESH VERSION has no version string to recompute.
    let _ = (set_schema, refresh_version);
    Ok(ExecResult::Command {
        tag: "ALTER COLLATION".to_string(),
    })
}

pub(crate) fn exec_alter_database_refresh_collation(
    eng: &mut Engine,
    ctx: &mut StmtCtx,
    name: &str,
) -> Result<ExecResult, ExecError> {
    let _ = (eng, ctx);
    if name != "postgres" {
        return Err(exec_err(
            "42704",
            format!("database \"{}\" does not exist", name),
        ));
    }
    Ok(ExecResult::Command {
        tag: "ALTER DATABASE".to_string(),
    })
}
