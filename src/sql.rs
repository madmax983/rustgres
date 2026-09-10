//! SQL tokenizer + parser for the rustgres subset.
//!
//! v0.1 statements:
//!   CREATE TABLE name (col TYPE [, ...])
//!   INSERT INTO name [(col, ...)] VALUES (v, ...), (...), ...
//!   SELECT [* | expr [, ...]] FROM name [WHERE col = lit|$N [AND ...]] [LIMIT n]
//!   SELECT expr [, ...]                      (no FROM: single row)
//!   DROP TABLE [IF EXISTS] name
//!
//! v0.2: `$N` parameter placeholders (1-based) and a tiny expression
//! language for the SELECT list: literals, column refs, params, `+`, parens.
//!
//! v0.3: transaction control statements (BEGIN/COMMIT/.../SAVEPOINT).
//!
//! v0.5: UPDATE / DELETE, VACUUM.
//!
//! v0.6: query engine.
//!   SELECT [DISTINCT] items
//!     FROM source [, ...]                     -- comma = CROSS JOIN
//!          | t [AS] alias
//!          | (SELECT ...) [AS] alias           -- derived table (alias required)
//!          | source [INNER] JOIN source ON expr
//!          | source LEFT [OUTER] JOIN source ON expr
//!     [WHERE predicate] [GROUP BY expr, ...] [HAVING predicate]
//!     [ORDER BY ...] [LIMIT n] [OFFSET n] [FOR UPDATE]
//!   Predicates: AND / OR / NOT, comparisons (= <> < <= > >=),
//!   IS [NOT] NULL, [NOT] IN (subquery), [NOT] EXISTS (subquery).
//!   Expressions: qualified refs (t.col), `t.*`, aggregates
//!   (COUNT(*)/COUNT(e)/SUM(e)/AVG(e)/MIN(e)/MAX(e)), scalar subqueries,
//!   select-list aliases ([AS] name).
//!
//! Keywords are case-insensitive; unquoted identifiers fold to lowercase.
//! String literals use single quotes with `''` as the escape for a quote.

use crate::storage::ColType;

#[derive(Debug)]
pub struct SqlError {
    pub message: String,
    /// SQLSTATE for this parse error. Syntax errors are 42601; undefined
    /// functions / wrong arity are 42883 (like Postgres' parser).
    pub code: &'static str,
}

fn err(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "42601",
    }
}

/// A parse-time 42883 (undefined function), like Postgres.
fn err_undefined(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "42883",
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String), // folded to lowercase unless double-quoted
    Number(String),
    Str(String),
    Param(u32), // $N parameter placeholder, 1-based
    LParen,
    RParen,
    Comma,
    Semi,
    Star,
    Plus,
    Minus,      // v0.7: `-` (unary and binary)
    Slash,      // v0.7: `/`
    Percent,    // v0.7: `%`
    Eq,
    Dot,  // v0.6: qualified refs (t.col)
    Lt,   // v0.6: <
    Gt,   // v0.6: >
    LtEq, // v0.6: <=
    GtEq, // v0.6: >=
    Neq,  // v0.6: <> and !=
    ColonColon, // v0.7: `::` cast
    PipePipe,   // v0.7: `||` concat
    Caret,      // v0.7: `^` exponentiation
    EOF,
}

fn tokenize(input: &str) -> Result<Vec<Token>, SqlError> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut toks = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // `--` line comment
        if c == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // `-` (minus) when not starting a `--` comment.
        if c == '-' {
            toks.push(Token::Minus);
            i += 1;
            continue;
        }
        // `::` cast operator (a lone `:` is a syntax error).
        if c == ':' {
            if i + 1 < chars.len() && chars[i + 1] == ':' {
                toks.push(Token::ColonColon);
                i += 2;
            } else {
                return Err(err("unexpected character ':'"));
            }
            continue;
        }
        // `||` concatenation (a lone `|` is a syntax error).
        if c == '|' {
            if i + 1 < chars.len() && chars[i + 1] == '|' {
                toks.push(Token::PipePipe);
                i += 2;
            } else {
                return Err(err("unexpected character '|'"));
            }
            continue;
        }
        // `/* ... */` block comment
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 >= chars.len() {
                return Err(err("unterminated block comment"));
            }
            i += 2;
            continue;
        }
        match c {
            '(' => {
                toks.push(Token::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Token::RParen);
                i += 1;
            }
            ',' => {
                toks.push(Token::Comma);
                i += 1;
            }
            ';' => {
                toks.push(Token::Semi);
                i += 1;
            }
            '*' => {
                toks.push(Token::Star);
                i += 1;
            }
            '+' => {
                toks.push(Token::Plus);
                i += 1;
            }
            '/' => {
                toks.push(Token::Slash);
                i += 1;
            }
            '%' => {
                toks.push(Token::Percent);
                i += 1;
            }
            '^' => {
                toks.push(Token::Caret);
                i += 1;
            }
            '=' => {
                toks.push(Token::Eq);
                i += 1;
            }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Token::LtEq);
                    i += 2;
                } else if i + 1 < chars.len() && chars[i + 1] == '>' {
                    toks.push(Token::Neq);
                    i += 2;
                } else {
                    toks.push(Token::Lt);
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Token::GtEq);
                    i += 2;
                } else {
                    toks.push(Token::Gt);
                    i += 1;
                }
            }
            '!' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    toks.push(Token::Neq);
                    i += 2;
                } else {
                    return Err(err(format!("unexpected character '{}'", c)));
                }
            }
            '\'' => {
                // single-quoted string, '' is an escaped quote
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(err("unterminated string literal"));
                    }
                    if chars[i] == '\'' {
                        if i + 1 < chars.len() && chars[i + 1] == '\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(chars[i]);
                        i += 1;
                    }
                }
                toks.push(Token::Str(s));
            }
            '"' => {
                // double-quoted identifier: kept verbatim (no case folding)
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(err("unterminated quoted identifier"));
                    }
                    if chars[i] == '"' {
                        if i + 1 < chars.len() && chars[i + 1] == '"' {
                            s.push('"');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(chars[i]);
                        i += 1;
                    }
                }
                toks.push(Token::Ident(s));
            }
            // `$N` parameter placeholder (only when `$` starts the token;
            // `$` inside an identifier, e.g. `a$1`, keeps the old behavior).
            '$' if i + 1 < chars.len() && chars[i + 1].is_ascii_digit() => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let raw: String = chars[start..i].iter().collect();
                let n: u32 = raw
                    .parse()
                    .map_err(|_| err(format!("bad parameter number \"${}\"", raw)))?;
                if n == 0 {
                    return Err(err("parameter number must be >= 1"));
                }
                toks.push(Token::Param(n));
            }
            _ if c.is_ascii_digit()
                || (c == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) =>
            {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i < chars.len() && chars[i] == '.' {
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                    let mut j = i + 1;
                    if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                        j += 1;
                    }
                    if j < chars.len() && chars[j].is_ascii_digit() {
                        i = j;
                        while i < chars.len() && chars[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                toks.push(Token::Number(chars[start..i].iter().collect()));
            }
            // A `.` that does not start a number is the qualifier dot.
            '.' => {
                toks.push(Token::Dot);
                i += 1;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                toks.push(Token::Ident(word.to_lowercase()));
            }
            _ => return Err(err(format!("unexpected character '{}'", c))),
        }
    }
    toks.push(Token::EOF);
    Ok(toks)
}

/// A literal value as written in SQL.
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Int(i64),
    BigInt(i64),   // v0.7: integer literals outside the int4 range
    SmallInt(i16), // v0.7: only via typed params/casts, never parsed
    Float(f64),
    /// Decimal literal text, e.g. `1.5`. Evaluates as float8 in expressions
    /// (v0.6 behavior) but INSERT coerces the exact text for numeric
    /// targets, so high-precision decimals don't round-trip through f64.
    Decimal(String),
    Real(f32), // v0.7: only via typed params/casts, never parsed
    Numeric(crate::storage::Numeric), // v0.7: only via typed params/casts
    Text(String),
    Bool(bool),
    Date(i32),        // v0.7: days since 1970-01-01
    Timestamp(i64),   // v0.7: micros since 1970-01-01 00:00:00 UTC
    Timestamptz(i64), // v0.7: micros since epoch, UTC
    Bytea(Vec<u8>),   // v0.7
    Uuid([u8; 16]),   // v0.7
    Null,
}

impl Literal {
    pub fn type_name(&self) -> &'static str {
        match self {
            Literal::Int(_) => "integer",
            Literal::BigInt(_) => "bigint",
            Literal::SmallInt(_) => "smallint",
            Literal::Float(_) | Literal::Decimal(_) => "double precision",
            Literal::Real(_) => "real",
            Literal::Numeric(_) => "numeric",
            Literal::Text(_) => "text",
            Literal::Bool(_) => "boolean",
            Literal::Date(_) => "date",
            Literal::Timestamp(_) => "timestamp without time zone",
            Literal::Timestamptz(_) => "timestamp with time zone",
            Literal::Bytea(_) => "bytea",
            Literal::Uuid(_) => "uuid",
            Literal::Null => "unknown",
        }
    }

    /// Column type a bare literal projects as in `SELECT 1` (no FROM).
    pub fn col_type(&self) -> ColType {
        match self {
            Literal::Int(_) => ColType::Int,
            Literal::BigInt(_) => ColType::BigInt,
            Literal::SmallInt(_) => ColType::SmallInt,
            Literal::Float(_) | Literal::Decimal(_) => ColType::Float,
            Literal::Real(_) => ColType::Float4,
            Literal::Numeric(_) => ColType::Numeric,
            Literal::Text(_) => ColType::Text,
            Literal::Bool(_) => ColType::Bool,
            Literal::Date(_) => ColType::Date,
            Literal::Timestamp(_) => ColType::Timestamp,
            Literal::Timestamptz(_) => ColType::Timestamptz,
            Literal::Bytea(_) => ColType::Bytea,
            Literal::Uuid(_) => ColType::Uuid,
            // Postgres would say "unknown"; text is a fine stand-in.
            Literal::Null => ColType::Text,
        }
    }

    pub fn into_value(self) -> crate::storage::Value {
        use crate::storage::Value;
        match self {
            Literal::Int(i) => Value::Int(i),
            Literal::BigInt(i) => Value::BigInt(i),
            Literal::SmallInt(i) => Value::SmallInt(i),
            Literal::Float(f) => Value::Float(f),
            Literal::Decimal(s) => Value::Float(s.parse().unwrap_or(f64::NAN)),
            Literal::Real(f) => Value::Float4(f),
            Literal::Numeric(n) => Value::Numeric(n),
            Literal::Text(s) => Value::Text(s),
            Literal::Bool(b) => Value::Bool(b),
            Literal::Date(d) => Value::Date(d),
            Literal::Timestamp(m) => Value::Timestamp(m),
            Literal::Timestamptz(m) => Value::Timestamptz(m),
            Literal::Bytea(b) => Value::Bytea(b),
            Literal::Uuid(u) => Value::Uuid(u),
            Literal::Null => Value::Null,
        }
    }
}

/// Comparison operators (v0.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn sql(&self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// Aggregate functions (v0.6; v0.7 adds StringAgg).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    StringAgg, // v0.7: string_agg(x, delim)
}

impl AggFunc {
    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
            AggFunc::StringAgg => "string_agg",
        }
    }
}

/// Binary arithmetic operators (v0.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow, // v0.7: `^` exponentiation
}

impl ArithOp {
    pub fn sql(&self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Mod => "%",
            ArithOp::Pow => "^",
        }
    }
}

/// A SELECT-list / WHERE / ON / HAVING expression (v0.6: full predicates,
/// qualified refs, aggregates, subqueries).
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Column {
        table: Option<String>,
        name: String,
    },
    /// Pre-resolved column position: `(scope frame, column index)` within one
    /// fixed scope shape. Never produced by the parser — the executor builds
    /// it once before a hot row-pair loop (JOIN ON) so per-row evaluation
    /// skips name resolution entirely. Evaluates exactly like the `Column`
    /// it was resolved from.
    ResolvedCol {
        frame: usize,
        idx: usize,
    },
    Literal(Literal),
    Param(u32), // 1-based $N; substituted with a Literal before execution
    /// v0.7: `+ - * / %` with Postgres-ish numeric promotion.
    Arith {
        op: ArithOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// v0.7: explicit cast, `x::type` or `CAST(x AS type)`.
    Cast {
        expr: Box<Expr>,
        to: ColType,
    },
    /// v0.7: `||` string concatenation.
    Concat(Box<Expr>, Box<Expr>),
    /// v0.7: `[NOT] LIKE` / `[NOT] ILIKE`.
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        not: bool,
        ilike: bool,
    },
    /// v0.7: `[NOT] BETWEEN low AND high`.
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        neg: bool,
    },
    /// v0.7: `IS [NOT] TRUE/FALSE/UNKNOWN`. `val: None` = UNKNOWN.
    IsBool {
        expr: Box<Expr>,
        neg: bool,
        val: Option<bool>,
    },
    /// v0.7: built-in scalar function call.
    Func {
        name: String,
        args: Vec<Expr>,
    },
    /// v0.7: `extract(field FROM expr)`.
    Extract {
        field: String,
        from: Box<Expr>,
    },
    Cmp {
        op: CmpOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        neg: bool,
    },
    Agg {
        func: AggFunc,
        /// None = COUNT(*).
        arg: Option<Box<Expr>>,
        /// v0.7: DISTINCT inside the aggregate.
        distinct: bool,
        /// v0.7: second argument (string_agg's delimiter).
        arg2: Option<Box<Expr>>,
    },
    /// `(SELECT ...)` used as a value: 0 rows -> NULL, >1 row -> 21000.
    ScalarSub(Box<SelectStmt>),
    /// `[NOT] IN (subquery)`.
    InSub {
        expr: Box<Expr>,
        sub: Box<SelectStmt>,
        neg: bool,
    },
    /// `[NOT] EXISTS (subquery)`.
    Exists {
        sub: Box<SelectStmt>,
        neg: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum SelectItem {
    All,
    /// `qualifier.*`
    AllOf(String),
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

/// A FROM source (v0.6).
#[derive(Clone, Debug, PartialEq)]
pub enum FromItem {
    Table {
        name: String,
        alias: Option<String>,
    },
    /// `(SELECT ...) [AS] alias` — the alias is required, like Postgres.
    Derived {
        sub: Box<SelectStmt>,
        alias: String,
    },
    Join {
        left: Box<FromItem>,
        kind: JoinKind,
        right: Box<FromItem>,
        /// None for CROSS JOIN.
        on: Option<Expr>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Cross,
}

/// A full SELECT statement (v0.6).
#[derive(Clone, Debug, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Vec<FromItem>,
    pub where_: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderTerm>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub for_update: bool,
}

/// Right-hand side of a `WHERE col = ...` comparison (UPDATE/DELETE only;
///
/// SELECT graduated to full predicates in v0.6).
#[derive(Clone, Debug)]
pub enum WhereRhs {
    Lit(Literal),
    Param(u32),
}

#[derive(Clone, Debug)]
pub struct WhereCond {
    pub col: String,
    pub rhs: WhereRhs,
}

/// A value in an INSERT row: a literal, or a `$N` parameter placeholder
/// (v0.3: substituted with the bound value before execution).
#[derive(Clone, Debug, PartialEq)]
pub enum InsertValue {
    Lit(Literal),
    Param(u32),
}

/// One `ORDER BY` sort key: expression + direction + explicit NULL
/// placement (v0.7: `NULLS FIRST` / `NULLS LAST`).
#[derive(Clone, Debug, PartialEq)]
pub struct OrderTerm {
    pub expr: Expr,
    pub desc: bool,
    /// None = default (NULLS LAST for ASC, NULLS FIRST for DESC).
    pub nulls_first: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    CreateTable {
        name: String,
        columns: Vec<(String, ColType)>,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<InsertValue>>,
    },
    Select(SelectStmt),
    DropTable {
        if_exists: bool,
        name: String,
    },
    // --- v0.5: UPDATE / DELETE with MVCC semantics
    Update {
        table: String,
        /// (column, expression) assignments.
        sets: Vec<(String, Expr)>,
        where_: Vec<WhereCond>,
    },
    Delete {
        table: String,
        where_: Vec<WhereCond>,
    },
    // --- v0.3: transaction control (handled by the session, not the executor)
    Begin {
        level: Option<IsolationLevel>,
    },
    Commit,
    Rollback,
    Savepoint {
        name: String,
    },
    RollbackTo {
        name: String,
    },
    Release {
        name: String,
    },
    // --- v0.4: checkpoint (handled by the session, not the executor)
    Checkpoint,
    // --- v0.5: vacuum (handled by the session, not the executor)
    Vacuum {
        table: Option<String>,
        verbose: bool,
    },
    // --- v0.8: secondary indexes
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    // --- v0.8: EXPLAIN (planned, never executed)
    Explain {
        stmt: Box<Stmt>,
    },
    // --- v0.8: ANALYZE (statistics collection)
    Analyze {
        table: Option<String>,
    },
}

/// Transaction isolation level (v0.5). `READ UNCOMMITTED` is accepted and
/// treated as `READ COMMITTED`, like Postgres.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl Stmt {
    /// Highest `$N` referenced anywhere in the statement (0 = no params).
    pub fn max_param(&self) -> usize {
        match self {
            Stmt::Insert { rows, .. } => {
                let mut m = 0;
                for row in rows {
                    for v in row {
                        if let InsertValue::Param(n) = v {
                            m = m.max(*n as usize);
                        }
                    }
                }
                m
            }
            Stmt::Select(sel) => max_param_select(sel),
            Stmt::Explain { stmt } => stmt.max_param(),
            Stmt::Update { sets, where_, .. } => {
                let mut m = 0;
                for (_, e) in sets {
                    m = m.max(max_param_expr(e));
                }
                for w in where_ {
                    if let WhereRhs::Param(n) = w.rhs {
                        m = m.max(n as usize);
                    }
                }
                m
            }
            Stmt::Delete { where_, .. } => {
                let mut m = 0;
                for w in where_ {
                    if let WhereRhs::Param(n) = w.rhs {
                        m = m.max(n as usize);
                    }
                }
                m
            }
            _ => 0,
        }
    }
}

fn max_param_select(s: &SelectStmt) -> usize {
    let mut m = 0;
    for item in &s.items {
        match item {
            SelectItem::Expr { expr, .. } => m = m.max(max_param_expr(expr)),
            _ => {}
        }
    }
    for f in &s.from {
        m = m.max(max_param_from(f));
    }
    if let Some(e) = &s.where_ {
        m = m.max(max_param_expr(e));
    }
    for e in &s.group_by {
        m = m.max(max_param_expr(e));
    }
    if let Some(e) = &s.having {
        m = m.max(max_param_expr(e));
    }
    for o in &s.order_by {
        m = m.max(max_param_expr(&o.expr));
    }
    m
}

fn max_param_from(f: &FromItem) -> usize {
    match f {
        FromItem::Table { .. } => 0,
        FromItem::Derived { sub, .. } => max_param_select(sub),
        FromItem::Join {
            left, right, on, ..
        } => {
            let mut m = max_param_from(left).max(max_param_from(right));
            if let Some(e) = on {
                m = m.max(max_param_expr(e));
            }
            m
        }
    }
}

fn max_param_expr(e: &Expr) -> usize {
    match e {
        Expr::Param(n) => *n as usize,
        Expr::Column { .. } | Expr::ResolvedCol { .. } | Expr::Literal(_) => 0,
        Expr::Arith { left, right, .. } | Expr::And(left, right) | Expr::Or(left, right) => {
            max_param_expr(left).max(max_param_expr(right))
        }
        Expr::Concat(a, b) => max_param_expr(a).max(max_param_expr(b)),
        Expr::Cmp { left, right, .. } => max_param_expr(left).max(max_param_expr(right)),
        Expr::Like {
            expr, pattern, ..
        } => max_param_expr(expr).max(max_param_expr(pattern)),
        Expr::Between {
            expr, low, high, ..
        } => max_param_expr(expr)
            .max(max_param_expr(low))
            .max(max_param_expr(high)),
        Expr::Cast { expr, .. } | Expr::Not(expr) | Expr::IsNull { expr, .. } => {
            max_param_expr(expr)
        }
        Expr::IsBool { expr, .. } => max_param_expr(expr),
        Expr::Func { args, .. } => args.iter().map(max_param_expr).max().unwrap_or(0),
        Expr::Extract { from, .. } => max_param_expr(from),
        Expr::Agg { arg, arg2, .. } => arg
            .as_deref()
            .map(max_param_expr)
            .unwrap_or(0)
            .max(arg2.as_deref().map(max_param_expr).unwrap_or(0)),
        Expr::ScalarSub(s) => max_param_select(s),
        Expr::InSub { expr, sub, .. } => max_param_expr(expr).max(max_param_select(sub)),
        Expr::Exists { sub, .. } => max_param_select(sub),
    }
}

/// Parse a single statement. A single trailing semicolon is stripped.
pub fn parse_statement(input: &str) -> Result<Stmt, SqlError> {
    let mut text = input.trim();
    if let Some(stripped) = text.strip_suffix(';') {
        text = stripped.trim_end();
    }
    let tokens = tokenize(text)?;
    let mut p = Parser { tokens, pos: 0 };
    let stmt = p.parse_top()?;
    match p.next() {
        Token::EOF => Ok(stmt),
        other => Err(err(format!("syntax error: unexpected {:?}", other))),
    }
}

/// Keywords that can never be a bare (AS-less) alias or table alias.
/// (A wordier list than Postgres needs, but it keeps the grammar
/// unambiguous without much fuss.)
fn is_reserved(word: &str) -> bool {
    matches!(
        word,
        "all"
            | "and"
            | "as"
            | "asc"
            | "begin"
            | "between" // v0.7
            | "by"
            | "cast" // v0.7
            | "checkpoint"
            | "commit"
            | "create"
            | "cross"
            | "delete"
            | "desc"
            | "distinct"
            | "drop"
            | "end"
            | "exists"
            | "for"
            | "from"
            | "group"
            | "having"
            | "ilike" // v0.7
            | "in"
            | "inner"
            | "insert"
            | "into"
            | "is"
            | "isolation"
            | "join"
            | "left"
            | "level"
            | "like" // v0.7
            | "limit"
            | "not"
            | "null"
            | "nulls" // v0.7
            | "offset"
            | "on"
            | "or"
            | "order"
            | "outer"
            | "read"
            | "release"
            | "repeatable"
            | "rollback"
            | "savepoint"
            | "select"
            | "serializable"
            | "set"
            | "table"
            | "to"
            | "transaction"
            | "uncommitted"
            | "committed"
            | "update"
            | "vacuum"
            | "values"
            | "verbose"
            | "where"
    )
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Token {
        self.tokens.get(self.pos).cloned().unwrap_or(Token::EOF)
    }

    /// Token after the next one (for `qual.*` / `NOT IN` lookahead).
    fn peek2(&self) -> Token {
        self.tokens.get(self.pos + 1).cloned().unwrap_or(Token::EOF)
    }

    /// Third token (for `qual.*` vs `qual.col` disambiguation).
    fn peek3(&self) -> Token {
        self.tokens.get(self.pos + 2).cloned().unwrap_or(Token::EOF)
    }

    fn next(&mut self) -> Token {
        let t = self.peek();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        match self.peek() {
            Token::Ident(ref s) if s == kw => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), SqlError> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            Err(err(format!(
                "syntax error: expected {}, found {:?}",
                kw.to_uppercase(),
                self.peek()
            )))
        }
    }

    fn expect_ident(&mut self) -> Result<String, SqlError> {
        match self.next() {
            Token::Ident(s) => Ok(s),
            other => Err(err(format!(
                "syntax error: expected identifier, found {:?}",
                other
            ))),
        }
    }

    fn expect(&mut self, tok: Token, what: &str) -> Result<(), SqlError> {
        let got = self.next();
        if got == tok {
            Ok(())
        } else {
            Err(err(format!(
                "syntax error: expected {}, found {:?}",
                what, got
            )))
        }
    }

    fn parse_top(&mut self) -> Result<Stmt, SqlError> {
        let kw = match self.next() {
            Token::Ident(s) => s,
            other => return Err(err(format!("syntax error: unexpected {:?}", other))),
        };
        self.parse_top_kw(kw)
    }

    /// Dispatch on an already-consumed leading keyword. Split out so
    /// EXPLAIN can recurse into it for the explained statement.
    fn parse_top_kw(&mut self, kw: String) -> Result<Stmt, SqlError> {
        match kw.as_str() {
            "create" => self.parse_create(),
            "insert" => self.parse_insert(),
            "select" => Ok(Stmt::Select(self.parse_select_rest()?)),
            "drop" => self.parse_drop(),
            // --- v0.5: UPDATE / DELETE
            "update" => self.parse_update(),
            "delete" => self.parse_delete(),
            // --- v0.5: VACUUM
            "vacuum" => self.parse_vacuum(),
            // --- v0.3: transaction control
            "begin" => {
                // BEGIN [TRANSACTION] [ISOLATION LEVEL ...]
                self.eat_keyword("transaction");
                self.parse_begin_rest()
            }
            "start" => {
                self.expect_keyword("transaction")?;
                self.parse_begin_rest()
            }
            "commit" | "end" => Ok(Stmt::Commit),
            // --- v0.4: CHECKPOINT (snapshot + WAL truncation)
            "checkpoint" => Ok(Stmt::Checkpoint),
            "rollback" | "abort" => {
                if self.eat_keyword("to") {
                    // ROLLBACK TO [SAVEPOINT] name
                    self.eat_keyword("savepoint");
                    Ok(Stmt::RollbackTo {
                        name: self.expect_ident()?,
                    })
                } else {
                    Ok(Stmt::Rollback)
                }
            }
            "savepoint" => Ok(Stmt::Savepoint {
                name: self.expect_ident()?,
            }),
            "release" => {
                // RELEASE [SAVEPOINT] name
                self.eat_keyword("savepoint");
                Ok(Stmt::Release {
                    name: self.expect_ident()?,
                })
            }
            // --- v0.8: EXPLAIN / ANALYZE
            "explain" => {
                if self.eat_keyword("analyze") {
                    return Err(SqlError {
                        message: "EXPLAIN ANALYZE is not supported yet".to_string(),
                        code: "0A000",
                    });
                }
                let inner_kw = match self.next() {
                    Token::Ident(s) => s,
                    other => {
                        return Err(err(format!("syntax error: unexpected {:?}", other)));
                    }
                };
                let inner = self.parse_top_kw(inner_kw)?;
                match inner {
                    Stmt::Select(_) => Ok(Stmt::Explain {
                        stmt: Box::new(inner),
                    }),
                    _ => Err(err("EXPLAIN only supports SELECT statements".to_string())),
                }
            }
            "analyze" => {
                // ANALYZE [table]
                let table = match self.peek() {
                    Token::EOF => None,
                    Token::Ident(_) => Some(self.expect_ident()?),
                    other => {
                        return Err(err(format!("syntax error: unexpected {:?}", other)));
                    }
                };
                Ok(Stmt::Analyze { table })
            }
            _ => Err(err(format!("syntax error at or near \"{}\"", kw))),
        }
    }

    fn parse_col_type(&mut self) -> Result<ColType, SqlError> {
        self.parse_type_name()
    }

    /// A type name for CREATE TABLE / CAST / `::` (v0.7: full set).
    /// Multi-word names like `double precision` and
    /// `timestamp with time zone` are accepted; `numeric(p[,s])`
    /// precision/scale are parsed but not enforced (documented).
    fn parse_type_name(&mut self) -> Result<ColType, SqlError> {
        let name = self.expect_ident()?;
        self.parse_type_name_rest(name)
    }

    fn parse_type_name_rest(&mut self, name: String) -> Result<ColType, SqlError> {
        match name.as_str() {
            "int" | "integer" => Ok(ColType::Int),
            "bigint" | "int8" => Ok(ColType::BigInt),
            "smallint" | "int2" => Ok(ColType::SmallInt),
            "real" | "float4" => Ok(ColType::Float4),
            "float8" => Ok(ColType::Float),
            // v0.1-v0.6 spelled the float8 column type "float"/"double".
            "float" | "double" => {
                self.eat_keyword("precision");
                Ok(ColType::Float)
            }
            "numeric" | "decimal" => {
                // Optional (p[, s]); parsed and ignored.
                if self.peek() == Token::LParen {
                    self.next();
                    match self.next() {
                        Token::Number(_) => {}
                        other => {
                            return Err(err(format!(
                                "syntax error: expected numeric precision, found {:?}",
                                other
                            )));
                        }
                    }
                    if self.peek() == Token::Comma {
                        self.next();
                        match self.next() {
                            Token::Number(_) => {}
                            other => {
                                return Err(err(format!(
                                    "syntax error: expected numeric scale, found {:?}",
                                    other
                                )));
                            }
                        }
                    }
                    self.expect(Token::RParen, "')'")?;
                }
                Ok(ColType::Numeric)
            }
            "bool" | "boolean" => Ok(ColType::Bool),
            "text" => Ok(ColType::Text),
            "date" => Ok(ColType::Date),
            "timestamptz" => Ok(ColType::Timestamptz),
            "timestamp" => {
                if self.eat_keyword("with") {
                    self.expect_keyword("time")?;
                    self.expect_keyword("zone")?;
                    Ok(ColType::Timestamptz)
                } else {
                    if self.eat_keyword("without") {
                        self.expect_keyword("time")?;
                        self.expect_keyword("zone")?;
                    }
                    Ok(ColType::Timestamp)
                }
            }
            "bytea" => Ok(ColType::Bytea),
            "uuid" => Ok(ColType::Uuid),
            _ => Err(err(format!("syntax error: unknown type \"{}\"", name))),
        }
    }

    /// First word of a type name (for typed-literal lookahead like
    /// `DATE '2026-01-01'`).
    fn is_type_start(name: &str) -> bool {
        matches!(
            name,
            "int" | "integer"
                | "bigint"
                | "int8"
                | "smallint"
                | "int2"
                | "real"
                | "float4"
                | "float8"
                | "float"
                | "double"
                | "numeric"
                | "decimal"
                | "bool"
                | "boolean"
                | "text"
                | "date"
                | "timestamp"
                | "timestamptz"
                | "bytea"
                | "uuid"
        )
    }

    fn parse_create(&mut self) -> Result<Stmt, SqlError> {
        // CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON table (col [, ...])
        let unique = self.eat_keyword("unique");
        if self.eat_keyword("index") {
            let if_not_exists = if self.eat_keyword("if") {
                self.expect_keyword("not")?;
                self.expect_keyword("exists")?;
                true
            } else {
                false
            };
            let name = self.expect_ident()?;
            self.expect_keyword("on")?;
            let table = self.expect_ident()?;
            self.expect(Token::LParen, "'('")?;
            let mut columns = Vec::new();
            loop {
                columns.push(self.expect_ident()?);
                match self.next() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ',' or ')', found {:?}",
                            other
                        )));
                    }
                }
            }
            if columns.is_empty() {
                return Err(err("syntax error: index requires at least one column".to_string()));
            }
            return Ok(Stmt::CreateIndex {
                name,
                table,
                columns,
                unique,
                if_not_exists,
            });
        }
        self.expect_keyword("table")?;
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "'('")?;
        let mut columns = Vec::new();
        loop {
            let col = self.expect_ident()?;
            let typ = self.parse_col_type()?;
            columns.push((col, typ));
            match self.next() {
                Token::Comma => continue,
                Token::RParen => break,
                other => {
                    return Err(err(format!(
                        "syntax error: expected ',' or ')', found {:?}",
                        other
                    )));
                }
            }
        }
        Ok(Stmt::CreateTable { name, columns })
    }

    fn parse_literal(&mut self) -> Result<Literal, SqlError> {
        match self.next() {
            Token::Number(raw) => {
                // v0.7: integer literals outside the int4 range become
                // bigint, like Postgres.
                if let Ok(i) = raw.parse::<i64>() {
                    if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                        Ok(Literal::Int(i))
                    } else {
                        Ok(Literal::BigInt(i))
                    }
                } else if raw.parse::<f64>().is_ok() {
                    // Keep the exact text; eval treats it as float8, but
                    // INSERT into numeric uses the text exactly.
                    Ok(Literal::Decimal(raw))
                } else {
                    Err(err(format!(
                        "syntax error: bad numeric literal \"{}\"",
                        raw
                    )))
                }
            }
            Token::Str(s) => Ok(Literal::Text(s)),
            Token::Ident(s) => match s.as_str() {
                "true" => Ok(Literal::Bool(true)),
                "false" => Ok(Literal::Bool(false)),
                "null" => Ok(Literal::Null),
                _ => Err(err(format!("syntax error: unexpected \"{}\"", s))),
            },
            other => Err(err(format!(
                "syntax error: expected a literal, found {:?}",
                other
            ))),
        }
    }

    fn parse_insert(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("into")?;
        let table = self.expect_ident()?;
        let columns = if self.peek() == Token::LParen {
            self.next();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                match self.next() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ',' or ')', found {:?}",
                            other
                        )));
                    }
                }
            }
            Some(cols)
        } else {
            None
        };
        self.expect_keyword("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect(Token::LParen, "'('")?;
            let mut row = Vec::new();
            loop {
                row.push(self.parse_insert_value()?);
                match self.next() {
                    Token::Comma => continue,
                    Token::RParen => break,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ',' or ')', found {:?}",
                            other
                        )));
                    }
                }
            }
            rows.push(row);
            if self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        Ok(Stmt::Insert {
            table,
            columns,
            rows,
        })
    }

    /// One INSERT value: a literal (with optional unary `+`/`-`), or a
    /// `$N` parameter placeholder.
    fn parse_insert_value(&mut self) -> Result<InsertValue, SqlError> {
        match self.peek() {
            Token::Param(n) => {
                self.next();
                Ok(InsertValue::Param(n))
            }
            Token::Minus | Token::Plus => {
                let neg = self.peek() == Token::Minus;
                self.next();
                let lit = self.parse_literal()?;
                Ok(InsertValue::Lit(match (neg, lit) {
                    (true, Literal::Int(i)) => Literal::Int(-i),
                    (true, Literal::BigInt(i)) => Literal::BigInt(-i),
                    // `-9223372036854775808`: the digits alone overflow
                    // i64; recover the exact i64::MIN.
                    (true, Literal::Decimal(s))
                        if s == "9223372036854775808" =>
                    {
                        Literal::BigInt(i64::MIN)
                    }
                    (true, Literal::Decimal(s)) => {
                        Literal::Decimal(format!("-{}", s))
                    }
                    (true, Literal::Float(f)) => Literal::Float(-f),
                    (_, l) => l,
                }))
            }
            _ => Ok(InsertValue::Lit(self.parse_literal()?)),
        }
    }

    // --- v0.6 expression grammar ---
    //
    //   or      := and (`OR` and)*
    //   and     := not (`AND` not)*
    //   not     := `NOT` not | cmp
    //   cmp     := add (cmpop add)? (`IS` [`NOT`] `NULL`)?
    //              | add [`NOT`] `IN` `(` select `)`
    //   add     := primary (`+` primary)*
    //   primary := literal | param | column [`.' ident] | function call
    //              | `EXISTS (select)` | `(select)` | `(` or `)`

    fn parse_or(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_and()?;
        while self.eat_keyword("or") {
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_not()?;
        while self.eat_keyword("and") {
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, SqlError> {
        if self.eat_keyword("not") {
            Ok(Expr::Not(Box::new(self.parse_not()?)))
        } else {
            self.parse_cmp()
        }
    }

    fn parse_cmp(&mut self) -> Result<Expr, SqlError> {
        let left = self.parse_concat()?;
        // `[NOT] BETWEEN low AND high`, `[NOT] LIKE pat`,
        // `[NOT] ILIKE pat`, `[NOT] IN (subquery)`.
        let neg = self.eat_keyword("not");
        if self.eat_keyword("between") {
            let low = self.parse_concat()?;
            self.expect_keyword("and")?;
            let high = self.parse_concat()?;
            return Ok(Expr::Between {
                expr: Box::new(left),
                low: Box::new(low),
                high: Box::new(high),
                neg,
            });
        }
        if self.eat_keyword("like") {
            let pattern = self.parse_concat()?;
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                not: neg,
                ilike: false,
            });
        }
        if self.eat_keyword("ilike") {
            let pattern = self.parse_concat()?;
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                not: neg,
                ilike: true,
            });
        }
        if neg || self.eat_keyword("in") {
            // In the negated case the `NOT` is consumed but `IN` is
            // still pending; anything else after NOT is a syntax error.
            if neg {
                match self.peek() {
                    Token::Ident(s) if s == "in" => {
                        self.next();
                    }
                    other => {
                        return Err(err(format!(
                            "syntax error: expected BETWEEN, LIKE, ILIKE or IN after NOT, found {:?}",
                            other
                        )));
                    }
                }
            }
            self.expect(Token::LParen, "'('")?;
            let sub = self.parse_subquery()?;
            self.expect(Token::RParen, "')'")?;
            return Ok(Expr::InSub {
                expr: Box::new(left),
                sub: Box::new(sub),
                neg,
            });
        }
        let op = match self.peek() {
            Token::Eq => Some(CmpOp::Eq),
            Token::Neq => Some(CmpOp::Ne),
            Token::Lt => Some(CmpOp::Lt),
            Token::LtEq => Some(CmpOp::Le),
            Token::Gt => Some(CmpOp::Gt),
            Token::GtEq => Some(CmpOp::Ge),
            _ => None,
        };
        let mut expr = match op {
            Some(op) => {
                self.next();
                let right = self.parse_concat()?;
                Expr::Cmp {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                }
            }
            None => left,
        };
        // `IS [NOT] NULL`, `IS [NOT] TRUE/FALSE/UNKNOWN`.
        if self.eat_keyword("is") {
            let neg = self.eat_keyword("not");
            match self.peek() {
                Token::Ident(s) if s == "null" => {
                    self.next();
                    expr = Expr::IsNull {
                        expr: Box::new(expr),
                        neg,
                    };
                }
                Token::Ident(s) if s == "true" || s == "false" || s == "unknown" => {
                    let val = match s.as_str() {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    };
                    self.next();
                    expr = Expr::IsBool {
                        expr: Box::new(expr),
                        neg,
                        val,
                    };
                }
                other => {
                    return Err(err(format!(
                        "syntax error: expected NULL, TRUE, FALSE or UNKNOWN after IS, found {:?}",
                        other
                    )));
                }
            }
        }
        Ok(expr)
    }

    /// `SELECT ...` inside parentheses (the `SELECT` keyword not yet consumed).
    fn parse_subquery(&mut self) -> Result<SelectStmt, SqlError> {
        match self.next() {
            Token::Ident(s) if s == "select" => self.parse_select_rest(),
            other => Err(err(format!(
                "syntax error: expected SELECT, found {:?}",
                other
            ))),
        }
    }

    /// concat := add (`||` add)*
    fn parse_concat(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_add()?;
        while self.peek() == Token::PipePipe {
            self.next();
            let right = self.parse_add()?;
            left = Expr::Concat(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// add := mul ((`+` | `-`) mul)*
    fn parse_add(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Token::Plus => ArithOp::Add,
                Token::Minus => ArithOp::Sub,
                _ => break,
            };
            self.next();
            let right = self.parse_mul()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// mul := pow ((`*` | `/` | `%`) pow)*
    fn parse_mul(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_pow()?;
        loop {
            let op = match self.peek() {
                Token::Star => ArithOp::Mul,
                Token::Slash => ArithOp::Div,
                Token::Percent => ArithOp::Mod,
                _ => break,
            };
            self.next();
            let right = self.parse_pow()?;
            left = Expr::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// pow := cast (`^` cast)* — left-associative, like Postgres.
    /// `^` binds tighter than `*`/`/`/`%` but looser than unary minus,
    /// so `-2^2` is `(-2)^2`.
    fn parse_pow(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_cast()?;
        while self.peek() == Token::Caret {
            self.next();
            let right = self.parse_cast()?;
            left = Expr::Arith {
                op: ArithOp::Pow,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    /// cast := unary (`::` type)*
    fn parse_cast(&mut self) -> Result<Expr, SqlError> {
        let mut expr = self.parse_unary()?;
        while self.peek() == Token::ColonColon {
            self.next();
            let to = self.parse_type_name()?;
            // Fold `decimal-literal::numeric` to an exact Numeric literal
            // so high-precision decimals don't round-trip through f64.
            // (Postgres parses decimal literals as numeric in the first
            // place; v0.7 keeps float8 for expressions but not for this.)
            if let (Expr::Literal(Literal::Decimal(s)), crate::storage::ColType::Numeric) =
                (&expr, &to)
            {
                if let Ok(n) = crate::storage::Numeric::parse(s) {
                    expr = Expr::Literal(Literal::Numeric(n));
                    continue;
                }
            }
            expr = Expr::Cast {
                expr: Box::new(expr),
                to,
            };
        }
        Ok(expr)
    }

    /// unary := (`-` | `+`) unary | primary.
    /// `-x` desugars to `0 - x` (so `-NULL` is NULL and `-'2026-01-01'`
    /// fails at evaluation, like Postgres).
    fn parse_unary(&mut self) -> Result<Expr, SqlError> {
        match self.peek() {
            Token::Minus => {
                self.next();
                let inner = self.parse_unary()?;
                Ok(Expr::Arith {
                    op: ArithOp::Sub,
                    left: Box::new(Expr::Literal(Literal::Int(0))),
                    right: Box::new(inner),
                })
            }
            Token::Plus => {
                self.next();
                self.parse_unary()
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, SqlError> {
        match self.peek() {
            Token::LParen => {
                self.next();
                // `(SELECT ...)` = scalar subquery; otherwise parenthesized expr.
                match self.peek() {
                    Token::Ident(s) if s == "select" => {
                        self.next();
                        let sub = self.parse_select_rest()?;
                        self.expect(Token::RParen, "')'")?;
                        Ok(Expr::ScalarSub(Box::new(sub)))
                    }
                    _ => {
                        let e = self.parse_or()?;
                        self.expect(Token::RParen, "')'")?;
                        Ok(e)
                    }
                }
            }
            Token::Param(n) => {
                self.next();
                Ok(Expr::Param(n))
            }
            Token::Number(_) | Token::Str(_) => Ok(Expr::Literal(self.parse_literal()?)),
            Token::Ident(ref s) if s == "true" || s == "false" || s == "null" => {
                Ok(Expr::Literal(self.parse_literal()?))
            }
            Token::Ident(_) => {
                let name = self.expect_ident()?;
                // `EXISTS (SELECT ...)` — only when followed by `(` so a
                // column actually named "exists" still works elsewhere.
                if name == "exists" && self.peek() == Token::LParen {
                    self.next();
                    let sub = self.parse_subquery()?;
                    self.expect(Token::RParen, "')'")?;
                    return Ok(Expr::Exists {
                        sub: Box::new(sub),
                        neg: false,
                    });
                }
                // `CAST(x AS type)` — special form, not a function call.
                if name == "cast" && self.peek() == Token::LParen {
                    self.next();
                    let expr = self.parse_or()?;
                    self.expect_keyword("as")?;
                    let to = self.parse_type_name()?;
                    self.expect(Token::RParen, "')'")?;
                    return Ok(Expr::Cast {
                        expr: Box::new(expr),
                        to,
                    });
                }
                // Typed literal: DATE '2026-01-01', TIMESTAMP '...', etc.
                // Backtracks when no string literal follows, so columns
                // named e.g. "date" keep working.
                if Self::is_type_start(&name) {
                    let save = self.pos;
                    if let Ok(to) = self.parse_type_name_rest(name.clone()) {
                        if let Token::Str(s) = self.peek() {
                            self.next();
                            return Ok(Expr::Cast {
                                expr: Box::new(Expr::Literal(Literal::Text(s))),
                                to,
                            });
                        }
                    }
                    self.pos = save;
                }
                // Aggregate / built-in function call `name(...)`?
                if self.peek() == Token::LParen {
                    return self.parse_call(name);
                }
                // `current_date` / `current_timestamp` without parens.
                if name == "current_date" || name == "current_timestamp" {
                    return Ok(Expr::Func {
                        name,
                        args: Vec::new(),
                    });
                }
                // Qualified ref `table.column`?
                if self.peek() == Token::Dot {
                    self.next();
                    let col = self.expect_ident()?;
                    return Ok(Expr::Column {
                        table: Some(name),
                        name: col,
                    });
                }
                Ok(Expr::Column { table: None, name })
            }
            other => Err(err(format!(
                "syntax error: expected expression, found {:?}",
                other
            ))),
        }
    }

    /// `count(*)`, `count(e)`, `sum(e)`, `avg(e)`, `min(e)`, `max(e)`.
    /// Anything else followed by `(` is "function does not exist" (42883
    /// at execution type-check; here a plain syntax-level error naming it).
    /// `name(...)` — aggregates, EXTRACT/TRIM/POSITION/SUBSTRING special
    /// forms, and the v0.7 built-in function set. Anything else is
    /// "function does not exist" (SQLSTATE 42883).
    fn parse_call(&mut self, name: String) -> Result<Expr, SqlError> {
        match name.as_str() {
            "extract" => return self.parse_extract(),
            "trim" => return self.parse_trim(),
            "position" => return self.parse_position(),
            "substring" => return self.parse_substring(),
            _ => {}
        }
        let agg = match name.as_str() {
            "count" => Some(AggFunc::Count),
            "sum" => Some(AggFunc::Sum),
            "avg" => Some(AggFunc::Avg),
            "min" => Some(AggFunc::Min),
            "max" => Some(AggFunc::Max),
            "string_agg" => Some(AggFunc::StringAgg),
            _ => None,
        };
        if let Some(func) = agg {
            self.expect(Token::LParen, "'('")?;
            let distinct = self.eat_keyword("distinct");
            if distinct && matches!(func, AggFunc::Count) && self.peek() == Token::Star {
                return Err(err("syntax error: DISTINCT is not allowed with count(*)"));
            }
            let arg = if matches!(func, AggFunc::Count) && self.peek() == Token::Star {
                self.next();
                None
            } else {
                Some(Box::new(self.parse_or()?))
            };
            let arg2 = if matches!(func, AggFunc::StringAgg) {
                self.expect(Token::Comma, "','")?;
                Some(Box::new(self.parse_or()?))
            } else {
                None
            };
            self.expect(Token::RParen, "')'")?;
            return Ok(Expr::Agg {
                func,
                arg,
                distinct,
                arg2,
            });
        }
        // Any `name(` is a function call. Unknown names and wrong
        // arities are 42883 (raised here for builtins, in exec for the
        // rest) — like Postgres.
        if self.peek() == Token::LParen {
            self.next();
            let mut args = Vec::new();
            if self.peek() != Token::RParen {
                loop {
                    args.push(self.parse_or()?);
                    if self.peek() == Token::Comma {
                        self.next();
                        continue;
                    }
                    break;
                }
            }
            self.expect(Token::RParen, "')'")?;
            if is_builtin_fn(&name) {
                check_builtin_arity(&name, args.len())?;
            }
            return Ok(Expr::Func { name, args });
        }
        Err(err(format!("syntax error: expected '(', found {:?}", self.peek())))
    }

    /// `extract(field FROM expr)`.
    fn parse_extract(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LParen, "'('")?;
        let field = self.expect_ident()?;
        self.expect_keyword("from")?;
        let from = self.parse_or()?;
        self.expect(Token::RParen, "')'")?;
        Ok(Expr::Extract {
            field,
            from: Box::new(from),
        })
    }

    /// `trim([ [leading|trailing|both] [chars] from ] str)`.
    /// Encoded as Func "trim" with args [spec, chars, str] where spec is
    /// a Text literal "leading"/"trailing"/"both" and chars defaults to " ".
    fn parse_trim(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LParen, "'('")?;
        let mut spec = "both".to_string();
        if matches!(self.peek(), Token::Ident(ref s) if s == "leading" || s == "trailing" || s == "both")
        {
            if let Token::Ident(s) = self.next() {
                spec = s;
            }
        }
        if self.eat_keyword("from") {
            let s = self.parse_or()?;
            self.expect(Token::RParen, "')'")?;
            return Ok(Expr::Func {
                name: "trim".to_string(),
                args: vec![
                    Expr::Literal(Literal::Text(spec.to_string())),
                    Expr::Literal(Literal::Text(" ".to_string())),
                    s,
                ],
            });
        }
        let first = self.parse_or()?;
        if self.eat_keyword("from") {
            let s = self.parse_or()?;
            self.expect(Token::RParen, "')'")?;
            return Ok(Expr::Func {
                name: "trim".to_string(),
                args: vec![
                    Expr::Literal(Literal::Text(spec.to_string())),
                    first,
                    s,
                ],
            });
        }
        self.expect(Token::RParen, "')'")?;
        Ok(Expr::Func {
            name: "trim".to_string(),
            args: vec![
                Expr::Literal(Literal::Text("both".to_string())),
                Expr::Literal(Literal::Text(" ".to_string())),
                first,
            ],
        })
    }

    /// `position(sub in str)` or `position(sub, str)`.
    fn parse_position(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LParen, "'('")?;
        // Parse below `IN` (parse_cmp) so a trailing `in` is left for the
        // `position(x in y)` form instead of becoming an IN-subquery.
        let a = self.parse_concat()?;
        let b = if self.eat_keyword("in") {
            self.parse_or()?
        } else {
            self.expect(Token::Comma, "','")?;
            self.parse_or()?
        };
        self.expect(Token::RParen, "')'")?;
        Ok(Expr::Func {
            name: "position".to_string(),
            args: vec![a, b],
        })
    }

    /// `substring(str from start [for len])` or `substring(str, start [, len])`.
    fn parse_substring(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LParen, "'('")?;
        let s = self.parse_or()?;
        let (start, len) = if self.eat_keyword("from") {
            let start = self.parse_or()?;
            let len = if self.eat_keyword("for") {
                Some(self.parse_or()?)
            } else {
                None
            };
            (start, len)
        } else {
            self.expect(Token::Comma, "','")?;
            let start = self.parse_or()?;
            let len = if self.peek() == Token::Comma {
                self.next();
                Some(self.parse_or()?)
            } else {
                None
            };
            (start, len)
        };
        self.expect(Token::RParen, "')'")?;
        let mut args = vec![s, start];
        if let Some(len) = len {
            args.push(len);
        }
        Ok(Expr::Func {
            name: "substring".to_string(),
            args,
        })
    }

    /// Optional `[AS] alias` after a select item or table source. A bare
    /// (AS-less) alias may not be a reserved word.
    fn parse_alias_opt(&mut self) -> Result<Option<String>, SqlError> {
        if self.eat_keyword("as") {
            return Ok(Some(self.expect_ident()?));
        }
        match self.peek() {
            Token::Ident(s) if !is_reserved(&s) => {
                self.next();
                Ok(Some(s))
            }
            _ => Ok(None),
        }
    }

    /// Remainder of BEGIN / START TRANSACTION after the optional
    /// TRANSACTION keyword: `[ISOLATION LEVEL ...]`.
    fn parse_begin_rest(&mut self) -> Result<Stmt, SqlError> {
        let level = if self.eat_keyword("isolation") {
            self.expect_keyword("level")?;
            Some(self.parse_isolation_level()?)
        } else {
            None
        };
        Ok(Stmt::Begin { level })
    }

    fn parse_isolation_level(&mut self) -> Result<IsolationLevel, SqlError> {
        let w = self.expect_ident()?;
        match w.as_str() {
            "serializable" => Ok(IsolationLevel::Serializable),
            "repeatable" => {
                self.expect_keyword("read")?;
                Ok(IsolationLevel::RepeatableRead)
            }
            "read" => match self.peek() {
                // READ UNCOMMITTED is treated as READ COMMITTED, like Postgres.
                Token::Ident(s) if s == "committed" || s == "uncommitted" => {
                    self.next();
                    Ok(IsolationLevel::ReadCommitted)
                }
                other => Err(err(format!(
                    "syntax error: expected COMMITTED or UNCOMMITTED, found {:?}",
                    other
                ))),
            },
            _ => Err(err(format!(
                "syntax error: unknown isolation level \"{}\"",
                w
            ))),
        }
    }

    /// Shared `WHERE col = lit|$N [AND ...]` tail for UPDATE/DELETE
    /// (kept simple; SELECT uses full predicates since v0.6).
    fn parse_where_opt(&mut self) -> Result<Vec<WhereCond>, SqlError> {
        let mut where_ = Vec::new();
        if self.eat_keyword("where") {
            loop {
                let col = self.expect_ident()?;
                self.expect(Token::Eq, "'='")?;
                let rhs = match self.peek() {
                    Token::Param(n) => {
                        self.next();
                        WhereRhs::Param(n)
                    }
                    _ => WhereRhs::Lit(self.parse_literal()?),
                };
                where_.push(WhereCond { col, rhs });
                if self.eat_keyword("and") {
                    continue;
                }
                break;
            }
        }
        Ok(where_)
    }

    fn parse_update(&mut self) -> Result<Stmt, SqlError> {
        let table = self.expect_ident()?;
        self.expect_keyword("set")?;
        let mut sets = Vec::new();
        loop {
            let col = self.expect_ident()?;
            self.expect(Token::Eq, "'='")?;
            let expr = self.parse_or()?;
            sets.push((col, expr));
            if self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        if sets.is_empty() {
            return Err(err("syntax error: UPDATE requires at least one assignment"));
        }
        let where_ = self.parse_where_opt()?;
        Ok(Stmt::Update {
            table,
            sets,
            where_,
        })
    }

    fn parse_delete(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("from")?;
        let table = self.expect_ident()?;
        let where_ = self.parse_where_opt()?;
        Ok(Stmt::Delete { table, where_ })
    }

    fn parse_vacuum(&mut self) -> Result<Stmt, SqlError> {
        let verbose = self.eat_keyword("verbose");
        let table = match self.peek() {
            Token::Ident(_) => Some(self.expect_ident()?),
            _ => None,
        };
        Ok(Stmt::Vacuum { table, verbose })
    }

    /// The body of a SELECT after the `SELECT` keyword was consumed.
    fn parse_select_rest(&mut self) -> Result<SelectStmt, SqlError> {
        let distinct = if self.eat_keyword("distinct") {
            true
        } else {
            // ALL is the default; consume it if present.
            self.eat_keyword("all");
            false
        };
        let mut items = Vec::new();
        loop {
            // `*`
            if self.peek() == Token::Star {
                self.next();
                items.push(SelectItem::All);
            } else if let Token::Ident(q) = self.peek() {
                // `qual.*` — but only when a `*` really follows the dot;
                // `qual.col` is a normal expression.
                if self.peek2() == Token::Dot && self.peek3() == Token::Star {
                    self.next(); // qual
                    self.next(); // dot
                    self.next(); // star
                    items.push(SelectItem::AllOf(q));
                } else {
                    let expr = self.parse_or()?;
                    let alias = self.parse_alias_opt()?;
                    items.push(SelectItem::Expr { expr, alias });
                }
            } else {
                let expr = self.parse_or()?;
                let alias = self.parse_alias_opt()?;
                items.push(SelectItem::Expr { expr, alias });
            }
            if self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        if items.is_empty() {
            return Err(err("syntax error: SELECT requires a select list"));
        }
        let from = if self.eat_keyword("from") {
            self.parse_from()?
        } else {
            Vec::new()
        };
        let where_ = if self.eat_keyword("where") {
            Some(self.parse_or()?)
        } else {
            None
        };
        let group_by = if self.eat_keyword("group") {
            self.expect_keyword("by")?;
            let mut groups = Vec::new();
            loop {
                groups.push(self.parse_or()?);
                if self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            groups
        } else {
            Vec::new()
        };
        let having = if self.eat_keyword("having") {
            Some(self.parse_or()?)
        } else {
            None
        };
        let order_by = if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            let mut terms = Vec::new();
            loop {
                let expr = self.parse_or()?;
                let desc = if self.eat_keyword("desc") {
                    true
                } else {
                    // ASC is the default; an explicit ASC is just consumed.
                    self.eat_keyword("asc");
                    false
                };
                // v0.7: explicit `NULLS FIRST` / `NULLS LAST`.
                let nulls_first = if self.eat_keyword("nulls") {
                    if self.eat_keyword("first") {
                        Some(true)
                    } else if self.eat_keyword("last") {
                        Some(false)
                    } else {
                        return Err(err(format!(
                            "syntax error: expected FIRST or LAST after NULLS, found {:?}",
                            self.peek()
                        )));
                    }
                } else {
                    None
                };
                terms.push(OrderTerm {
                    expr,
                    desc,
                    nulls_first,
                });
                if self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            terms
        } else {
            Vec::new()
        };
        // LIMIT and OFFSET in either order (both accepted, like Postgres).
        let mut limit = None;
        let mut offset = None;
        loop {
            if limit.is_none() && self.eat_keyword("limit") {
                match self.next() {
                    Token::Number(raw) => match raw.parse::<i64>() {
                        Ok(n) => limit = Some(n),
                        Err(_) => {
                            return Err(err(format!("syntax error: bad LIMIT value \"{}\"", raw)));
                        }
                    },
                    other => {
                        return Err(err(format!(
                            "syntax error: expected LIMIT count, found {:?}",
                            other
                        )));
                    }
                }
            } else if offset.is_none() && self.eat_keyword("offset") {
                match self.next() {
                    Token::Number(raw) => match raw.parse::<i64>() {
                        Ok(n) => offset = Some(n),
                        Err(_) => {
                            return Err(err(format!("syntax error: bad OFFSET value \"{}\"", raw)));
                        }
                    },
                    other => {
                        return Err(err(format!(
                            "syntax error: expected OFFSET count, found {:?}",
                            other
                        )));
                    }
                }
            } else {
                break;
            }
        }
        let for_update = if self.eat_keyword("for") {
            self.expect_keyword("update")?;
            true
        } else {
            false
        };
        Ok(SelectStmt {
            distinct,
            items,
            from,
            where_,
            group_by,
            having,
            order_by,
            limit,
            offset,
            for_update,
        })
    }

    /// FROM source [, ...] — commas become CROSS JOINs; explicit JOINs
    /// bind tighter than commas.
    fn parse_from(&mut self) -> Result<Vec<FromItem>, SqlError> {
        let mut items = vec![self.parse_join_chain()?];
        while self.peek() == Token::Comma {
            self.next();
            items.push(self.parse_join_chain()?);
        }
        let mut iter = items.into_iter();
        let mut acc = iter.next().unwrap();
        for next in iter {
            acc = FromItem::Join {
                left: Box::new(acc),
                kind: JoinKind::Cross,
                right: Box::new(next),
                on: None,
            };
        }
        Ok(vec![acc])
    }

    fn parse_join_chain(&mut self) -> Result<FromItem, SqlError> {
        let mut left = self.parse_from_primary()?;
        loop {
            let kind = if self.eat_keyword("join") {
                JoinKind::Inner
            } else if self.eat_keyword("inner") {
                self.expect_keyword("join")?;
                JoinKind::Inner
            } else if self.eat_keyword("left") {
                self.eat_keyword("outer");
                self.expect_keyword("join")?;
                JoinKind::Left
            } else if self.eat_keyword("cross") {
                self.expect_keyword("join")?;
                JoinKind::Cross
            } else {
                break;
            };
            let right = self.parse_from_primary()?;
            let on = match kind {
                JoinKind::Cross => None,
                _ => {
                    self.expect_keyword("on")?;
                    Some(self.parse_or()?)
                }
            };
            left = FromItem::Join {
                left: Box::new(left),
                kind,
                right: Box::new(right),
                on,
            };
        }
        Ok(left)
    }

    fn parse_from_primary(&mut self) -> Result<FromItem, SqlError> {
        if self.peek() == Token::LParen {
            self.next();
            let sub = self.parse_subquery()?;
            self.expect(Token::RParen, "')'")?;
            // Derived tables require an alias, like Postgres.
            let alias = if self.eat_keyword("as") {
                self.expect_ident()?
            } else {
                match self.peek() {
                    Token::Ident(s) if !is_reserved(&s) => {
                        self.next();
                        s
                    }
                    _ => return Err(err("syntax error: subquery in FROM must have an alias")),
                }
            };
            Ok(FromItem::Derived {
                sub: Box::new(sub),
                alias,
            })
        } else {
            let name = self.expect_ident()?;
            let alias = self.parse_alias_opt()?;
            Ok(FromItem::Table { name, alias })
        }
    }

    fn parse_drop(&mut self) -> Result<Stmt, SqlError> {
        if self.eat_keyword("index") {
            let if_exists = if self.eat_keyword("if") {
                self.expect_keyword("exists")?;
                true
            } else {
                false
            };
            let name = self.expect_ident()?;
            return Ok(Stmt::DropIndex { name, if_exists });
        }
        self.expect_keyword("table")?;
        let if_exists = if self.eat_keyword("if") {
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let name = self.expect_ident()?;
        Ok(Stmt::DropTable { if_exists, name })
    }
}

/// Split a simple-protocol Query string into individual statements on
/// top-level `;`, ignoring semicolons inside string literals, quoted
/// identifiers, and comments. Empty segments are dropped (a Query that is
/// entirely empty is handled by the caller as EmptyQueryResponse).
pub fn split_statements(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut start = 0; // char index where the current statement begins
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // single-quoted string, '' escapes
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c == '"' {
            // double-quoted identifier, "" escapes
            i += 1;
            while i < chars.len() {
                if chars[i] == '"' {
                    if i + 1 < chars.len() && chars[i + 1] == '"' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == ';' {
            let seg: String = chars[start..i].iter().collect();
            if !seg.trim().is_empty() {
                out.push(seg);
            }
            start = i + 1;
        }
        i += 1;
    }
    let tail: String = chars[start..].iter().collect();
    if !tail.trim().is_empty() {
        out.push(tail);
    }
    out
}

// ---------------------------------------------------------------------------
// v0.7 built-in scalar functions
// ---------------------------------------------------------------------------

/// Is `name` one of the v0.7 built-in scalar functions?
pub fn is_builtin_fn(name: &str) -> bool {
    matches!(
        name,
        // string
        "upper" | "lower" | "length" | "char_length" | "character_length"
        | "substring" | "trim" | "position" | "replace" | "split_part"
        // math
        | "abs" | "round" | "floor" | "ceil" | "ceiling" | "sqrt" | "power" | "mod"
        // date/time
        | "now" | "current_date" | "current_timestamp" | "date_trunc"
        // conditional
        | "coalesce" | "nullif" | "greatest" | "least"
    )
}

/// Arity check for built-in functions. Wrong argument counts raise
/// "function does not exist" (SQLSTATE 42883), like Postgres.
pub fn check_builtin_arity(name: &str, n: usize) -> Result<(), SqlError> {
    let ok = match name {
        "upper" | "lower" | "length" | "char_length" | "character_length"
        | "abs" | "floor" | "ceil" | "ceiling" | "sqrt" => n == 1,
        "now" | "current_date" | "current_timestamp" => n == 0,
        "substring" => n == 2 || n == 3,
        "trim" => n == 1 || n == 3,
        "position" | "power" | "mod" | "nullif" | "date_trunc" => n == 2,
        "replace" | "split_part" => n == 3,
        "round" => n == 1 || n == 2,
        "coalesce" | "greatest" | "least" => n >= 1,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(err_undefined(format!("function {}() does not exist", name)))
    }
}

