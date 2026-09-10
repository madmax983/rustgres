//! SQL tokenizer + parser for the rustgres subset.
//!
//! v0.1 statements:
//!   CREATE TABLE name (col TYPE [, ...])
//!   INSERT INTO name [(col, ...)] VALUES (v, ...), (...), ...
//!   SELECT [* | expr [, ...]] FROM name [WHERE col = lit|$N [AND ...]] [LIMIT n]
//!   SELECT expr [, ...]                      (no FROM: single row)
//!   DROP TABLE [IF EXISTS] name
//!
//! v0.2 additions: `$N` parameter placeholders (1-based) and a tiny expression
//! language for the SELECT list: literals, column refs, params, `+`, parens.
//!
//! v0.3 additions: transaction control statements
//!   BEGIN [TRANSACTION] | START TRANSACTION
//!   COMMIT | END
//!   ROLLBACK | ABORT
//!   SAVEPOINT name
//!   ROLLBACK TO [SAVEPOINT] name
//!   RELEASE [SAVEPOINT] name
//!
//! Keywords are case-insensitive; unquoted identifiers fold to lowercase.
//! String literals use single quotes with `''` as the escape for a quote.

use crate::storage::ColType;

#[derive(Debug)]
pub struct SqlError {
    pub message: String,
}

fn err(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
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
    Eq,
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
            '=' => {
                toks.push(Token::Eq);
                i += 1;
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
    Float(f64),
    Text(String),
    Bool(bool),
    Null,
}

impl Literal {
    pub fn type_name(&self) -> &'static str {
        match self {
            Literal::Int(_) => "integer",
            Literal::Float(_) => "double precision",
            Literal::Text(_) => "text",
            Literal::Bool(_) => "boolean",
            Literal::Null => "unknown",
        }
    }

    /// Column type a bare literal projects as in `SELECT 1` (no FROM).
    pub fn col_type(&self) -> ColType {
        match self {
            Literal::Int(_) => ColType::Int,
            Literal::Float(_) => ColType::Float,
            Literal::Text(_) => ColType::Text,
            Literal::Bool(_) => ColType::Bool,
            // Postgres would say "unknown"; text is a fine stand-in.
            Literal::Null => ColType::Text,
        }
    }

    pub fn into_value(self) -> crate::storage::Value {
        use crate::storage::Value;
        match self {
            Literal::Int(i) => Value::Int(i),
            Literal::Float(f) => Value::Float(f),
            Literal::Text(s) => Value::Text(s),
            Literal::Bool(b) => Value::Bool(b),
            Literal::Null => Value::Null,
        }
    }
}

/// A SELECT-list / WHERE expression (v0.2: includes `$N` params and `+`).
#[derive(Clone, Debug)]
pub enum Expr {
    Column(String),
    Literal(Literal),
    Param(u32), // 1-based $N; substituted with a Literal before execution
    Add(Box<Expr>, Box<Expr>),
}

#[derive(Clone, Debug)]
pub enum SelectItem {
    All,
    Expr(Expr),
}

/// Right-hand side of a `WHERE col = ...` comparison.
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

/// One `ORDER BY` sort key: expression + direction.
#[derive(Clone, Debug)]
pub struct OrderTerm {
    pub expr: Expr,
    pub desc: bool,
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
    Select {
        items: Vec<SelectItem>,
        table: Option<String>,
        where_: Vec<WhereCond>,
        order_by: Vec<OrderTerm>,
        limit: Option<i64>,
    },
    DropTable { if_exists: bool, name: String },
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
    Savepoint { name: String },
    RollbackTo { name: String },
    Release { name: String },
    // --- v0.4: checkpoint (handled by the session, not the executor)
    Checkpoint,
    // --- v0.5: vacuum (handled by the session, not the executor)
    Vacuum {
        table: Option<String>,
        verbose: bool,
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
            Stmt::Select {
                items,
                where_,
                order_by,
                ..
            } => {
                let mut m = 0;
                for item in items {
                    if let SelectItem::Expr(e) = item {
                        m = m.max(max_param_expr(e));
                    }
                }
                for w in where_ {
                    if let WhereRhs::Param(n) = w.rhs {
                        m = m.max(n as usize);
                    }
                }
                for o in order_by {
                    m = m.max(max_param_expr(&o.expr));
                }
                m
            }
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

fn max_param_expr(e: &Expr) -> usize {
    match e {
        Expr::Param(n) => *n as usize,
        Expr::Add(a, b) => max_param_expr(a).max(max_param_expr(b)),
        _ => 0,
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

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Token {
        self.tokens.get(self.pos).cloned().unwrap_or(Token::EOF)
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
        match kw.as_str() {
            "create" => self.parse_create(),
            "insert" => self.parse_insert(),
            "select" => self.parse_select(),
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
            _ => Err(err(format!("syntax error at or near \"{}\"", kw))),
        }
    }

    fn parse_col_type(&mut self) -> Result<ColType, SqlError> {
        let name = self.expect_ident()?;
        match name.as_str() {
            "int" | "integer" => Ok(ColType::Int),
            "text" => Ok(ColType::Text),
            "bool" | "boolean" => Ok(ColType::Bool),
            "real" | "float" | "float8" | "double" => Ok(ColType::Float),
            _ => Err(err(format!("syntax error: unknown type \"{}\"", name))),
        }
    }

    fn parse_create(&mut self) -> Result<Stmt, SqlError> {
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
                    )))
                }
            }
        }
        Ok(Stmt::CreateTable { name, columns })
    }

    fn parse_literal(&mut self) -> Result<Literal, SqlError> {
        match self.next() {
            Token::Number(raw) => {
                if let Ok(i) = raw.parse::<i64>() {
                    Ok(Literal::Int(i))
                } else if let Ok(f) = raw.parse::<f64>() {
                    Ok(Literal::Float(f))
                } else {
                    Err(err(format!("syntax error: bad numeric literal \"{}\"", raw)))
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
                        )))
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
                        )))
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

    /// One INSERT value: a literal, or a `$N` parameter placeholder.
    fn parse_insert_value(&mut self) -> Result<InsertValue, SqlError> {
        match self.peek() {
            Token::Param(n) => {
                self.next();
                Ok(InsertValue::Param(n))
            }
            _ => Ok(InsertValue::Lit(self.parse_literal()?)),
        }
    }

    /// expr := primary (`+` primary)*
    fn parse_expr(&mut self) -> Result<Expr, SqlError> {
        let mut left = self.parse_primary()?;
        while self.peek() == Token::Plus {
            self.next();
            let right = self.parse_primary()?;
            left = Expr::Add(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_primary(&mut self) -> Result<Expr, SqlError> {
        match self.peek() {
            Token::LParen => {
                self.next();
                let e = self.parse_expr()?;
                self.expect(Token::RParen, "')'")?;
                Ok(e)
            }
            Token::Param(n) => {
                self.next();
                Ok(Expr::Param(n))
            }
            Token::Number(_) | Token::Str(_) => Ok(Expr::Literal(self.parse_literal()?)),
            Token::Ident(ref s) if s == "true" || s == "false" || s == "null" => {
                Ok(Expr::Literal(self.parse_literal()?))
            }
            Token::Ident(_) => Ok(Expr::Column(self.expect_ident()?)),
            other => Err(err(format!(
                "syntax error: expected expression, found {:?}",
                other
            ))),
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
            _ => Err(err(format!("syntax error: unknown isolation level \"{}\"", w))),
        }
    }

    /// Shared `WHERE col = lit|$N [AND ...]` tail for SELECT/UPDATE/DELETE.
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
            let expr = self.parse_expr()?;
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
        Ok(Stmt::Update { table, sets, where_ })
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

    fn parse_select(&mut self) -> Result<Stmt, SqlError> {
        let mut items = Vec::new();
        if self.peek() == Token::Star {
            self.next();
            items.push(SelectItem::All);
        } else {
            loop {
                items.push(SelectItem::Expr(self.parse_expr()?));
                if self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
        }
        let table = if self.eat_keyword("from") {
            Some(self.expect_ident()?)
        } else {
            None
        };
        let where_ = self.parse_where_opt()?;
        let order_by = if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            let mut terms = Vec::new();
            loop {
                let expr = self.parse_expr()?;
                let desc = if self.eat_keyword("desc") {
                    true
                } else {
                    // ASC is the default; an explicit ASC is just consumed.
                    self.eat_keyword("asc");
                    false
                };
                terms.push(OrderTerm { expr, desc });
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
        let limit = if self.eat_keyword("limit") {
            match self.next() {
                Token::Number(raw) => match raw.parse::<i64>() {
                    Ok(n) => Some(n),
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
        } else {
            None
        };
        // `*` needs a FROM; bare literals only make sense without one.
        let has_from = table.is_some();
        for item in &items {
            match item {
                SelectItem::All if !has_from => {
                    return Err(err("syntax error: SELECT * requires FROM"));
                }
                // A bare literal in the select list with FROM stays an error,
                // exactly like v0.1 (params and `+` expressions are fine).
                SelectItem::Expr(Expr::Literal(_)) if has_from => {
                    return Err(err(
                        "syntax error: literals not supported in select list with FROM",
                    ));
                }
                _ => {}
            }
        }
        Ok(Stmt::Select {
            items,
            table,
            where_,
            order_by,
            limit,
        })
    }

    fn parse_drop(&mut self) -> Result<Stmt, SqlError> {
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
