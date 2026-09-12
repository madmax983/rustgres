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

/// A parse-time 42701 (duplicate_column), like Postgres.
fn err_duplicate(msg: impl Into<String>) -> SqlError {
    SqlError {
        message: msg.into(),
        code: "42701",
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String), // folded to lowercase unless double-quoted
    Number(String),
    Str(String),
    UStr(String), // v0.19: U&'...' raw content (UESCAPE handled by parser)
    Param(u32),   // $N parameter placeholder, 1-based
    LParen,
    RParen,
    Comma,
    Semi,
    Star,
    Plus,
    Minus,   // v0.7: `-` (unary and binary)
    Slash,   // v0.7: `/`
    Percent, // v0.7: `%`
    Eq,
    Dot,        // v0.6: qualified refs (t.col)
    Lt,         // v0.6: <
    Gt,         // v0.6: >
    LtEq,       // v0.6: <=
    GtEq,       // v0.6: >=
    Neq,        // v0.6: <> and !=
    ColonColon, // v0.7: `::` cast
    PipePipe,   // v0.7: `||` concat
    Caret,      // v0.7: `^` exponentiation
    EOF,
}

/// Parse a single-quoted string starting at `chars[*i]` (which must be `'`);
/// `''` is an escaped quote. Advances `*i` past the closing quote.
fn parse_single_quoted(chars: &[char], i: &mut usize) -> Result<String, SqlError> {
    assert_eq!(chars[*i], '\'');
    *i += 1;
    let mut s = String::new();
    loop {
        if *i >= chars.len() {
            return Err(err("unterminated string literal"));
        }
        if chars[*i] == '\'' {
            if *i + 1 < chars.len() && chars[*i + 1] == '\'' {
                s.push('\'');
                *i += 2;
            } else {
                *i += 1;
                break;
            }
        } else {
            s.push(chars[*i]);
            *i += 1;
        }
    }
    Ok(s)
}

/// Process `E'...'` escape sequences: `\n`, `\t`, `\b`, `\f`, `\r`,
/// `\\`, `\'`, `\uXXXX`, `\UXXXXXXXX`, and octal `\ooo`.
fn unescape_e_string(s: &str) -> Result<String, SqlError> {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            let c = s[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        i += 1;
        if i >= b.len() {
            return Err(err("unterminated escape sequence"));
        }
        match b[i] {
            b'n' => {
                out.push('\n');
                i += 1;
            }
            b't' => {
                out.push('\t');
                i += 1;
            }
            b'b' => {
                out.push('\x08');
                i += 1;
            }
            b'f' => {
                out.push('\x0c');
                i += 1;
            }
            b'r' => {
                out.push('\r');
                i += 1;
            }
            b'\\' => {
                out.push('\\');
                i += 1;
            }
            b'\'' => {
                out.push('\'');
                i += 1;
            }
            b'u' => {
                // \uXXXX
                if i + 4 >= b.len() {
                    return Err(err("invalid \\u escape"));
                }
                let hex = &s[i + 1..i + 5];
                let cp = u32::from_str_radix(hex, 16).map_err(|_| err("invalid \\u escape"))?;
                out.push(char::from_u32(cp).ok_or_else(|| err("invalid \\u escape"))?);
                i += 5;
            }
            b'U' => {
                // \UXXXXXXXX
                if i + 8 >= b.len() {
                    return Err(err("invalid \\U escape"));
                }
                let hex = &s[i + 1..i + 9];
                let cp = u32::from_str_radix(hex, 16).map_err(|_| err("invalid \\U escape"))?;
                out.push(char::from_u32(cp).ok_or_else(|| err("invalid \\U escape"))?);
                i += 9;
            }
            b'0'..=b'7' => {
                // Octal \ooo (up to 3 digits).
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    val = val * 8 + (b[i] - b'0') as u32;
                    i += 1;
                    n += 1;
                }
                out.push(char::from_u32(val).ok_or_else(|| err("invalid octal escape"))?);
            }
            _ => return Err(err(format!("invalid escape sequence '\\{}'", b[i] as char))),
        }
    }
    Ok(out)
}

/// Decode a `U&'...'` string with the given escape character.
/// `\\` followed by 4 hex digits = 16-bit code unit, `\\+` followed by 6
/// hex digits = 32-bit code point. `\\` followed by the escape char itself
/// is a literal escape char. Returns None on invalid escapes.
fn decode_ustr(s: &str, escape: char) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != escape {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // Escape sequence.
        i += 1;
        if i >= chars.len() {
            return None;
        }
        if chars[i] == escape {
            out.push(escape);
            i += 1;
        } else if chars[i] == '+' {
            // \+XXXXXX (6 hex digits).
            i += 1;
            if i + 6 > chars.len() {
                return None;
            }
            let hex: String = chars[i..i + 6].iter().collect();
            let cp = u32::from_str_radix(&hex, 16).ok()?;
            out.push(char::from_u32(cp)?);
            i += 6;
        } else {
            // \\XXXX (4 hex digits).
            if i + 4 > chars.len() {
                return None;
            }
            let hex: String = chars[i..i + 4].iter().collect();
            let cp = u32::from_str_radix(&hex, 16).ok()?;
            out.push(char::from_u32(cp)?);
            i += 4;
        }
    }
    Some(out)
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
        // v0.19: `U&'...'` Unicode escape string prefix (must come before
        // the identifier branch, since `&` is not an identifier char).
        if (c == 'u' || c == 'U')
            && i + 2 < chars.len()
            && chars[i + 1] == '&'
            && chars[i + 2] == '\''
        {
            i += 2; // consume `U&`, now at the quote
            let s = parse_single_quoted(&chars, &mut i)?;
            toks.push(Token::UStr(s));
            continue;
        }
        // v0.19: `E'...'` escape string prefix.
        if (c == 'e' || c == 'E') && i + 1 < chars.len() && chars[i + 1] == '\'' {
            i += 1; // consume `E`, now at the quote
            let s = parse_single_quoted(&chars, &mut i)?;
            toks.push(Token::Str(unescape_e_string(&s)?));
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
    Real(f32),                        // v0.7: only via typed params/casts, never parsed
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
            Literal::Float(_) => "double precision",
            // v0.18: decimal literals are numeric, like PostgreSQL.
            Literal::Decimal(_) => "numeric",
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
            Literal::Float(_) => ColType::Float,
            // v0.18: decimal literals are numeric, like PostgreSQL.
            Literal::Decimal(_) => ColType::Numeric,
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
            // v0.18: decimal literals are numeric, like PostgreSQL.
            // Absurd magnitudes that exceed i128 fall back to float8
            // (documented deviation: PG would keep arbitrary precision).
            Literal::Decimal(s) => match crate::storage::Numeric::parse(s.as_str()) {
                Ok(n) => Value::Numeric(n),
                Err(crate::storage::NumericParseError::Overflow) => {
                    Value::Float(s.parse().unwrap_or(f64::NAN))
                }
                Err(_) => Value::Float(f64::NAN),
            },
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
        escape: Option<Box<Expr>>,
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
    /// v0.10: `<func>(args) OVER (PARTITION BY ... ORDER BY ... frame)`.
    /// `wid` is a per-query window id assigned by the executor's pre-pass
    /// (the parser leaves it 0); identical window specs share one id.
    /// `distinct` is only meaningful for `Agg` funcs.
    Window {
        func: WindowFunc,
        args: Vec<Expr>,
        distinct: bool,
        partition_by: Vec<Expr>,
        order_by: Vec<OrderTerm>,
        frame: WindowFrame,
        wid: usize,
    },
}

/// v0.10: window function kinds. `Agg` reuses the aggregate functions as
/// windowed aggregates (`sum(x) OVER (...)`).
#[derive(Clone, Debug, PartialEq)]
pub enum WindowFunc {
    RowNumber,
    Rank,
    DenseRank,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue,
    Agg(AggFunc),
}

/// v0.10: window frame. `Default` follows the Postgres rule: with ORDER BY
/// it is `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`, otherwise
/// `ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING`.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowFrame {
    Default,
    Rows { start: FrameBound, end: FrameBound },
    Range { start: FrameBound, end: FrameBound },
}

/// v0.10: one end of a window frame.
#[derive(Clone, Debug, PartialEq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(u64),
    CurrentRow,
    Following(u64),
    UnboundedFollowing,
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
        /// v0.20: `FROM tbl [AS] x (a, b, c)` — column aliases rename the
        /// table's output columns positionally.
        col_aliases: Vec<String>,
    },
    /// `(SELECT ...) [AS] alias` — the alias is required, like Postgres.
    Derived { sub: Box<SelectStmt>, alias: String },
    /// v0.14: `(VALUES (e, ...) [, ...]) [AS] alias` — PG names the
    /// columns `column1`, `column2`, ... when no column aliases are given.
    Values { rows: Vec<Vec<Expr>>, alias: String },
    Join {
        left: Box<FromItem>,
        kind: JoinKind,
        right: Box<FromItem>,
        /// None for CROSS JOIN.
        on: Option<Expr>,
        /// v0.20: `USING (cols)` — stored separately so the executor can
        /// build the equijoin condition and project the merged columns.
        using: Vec<String>,
        /// v0.20: `NATURAL` — resolved to USING on common columns at
        /// execution time (schemas not known at parse time).
        natural: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

/// A full SELECT statement (v0.6).
#[derive(Clone, Debug, PartialEq)]
pub struct SelectStmt {
    /// v0.10: `WITH [RECURSIVE] ...` CTEs, in definition order.
    pub with: Vec<CteDef>,
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

/// v0.10: one Common Table Expression.
#[derive(Clone, Debug, PartialEq)]
pub struct CteDef {
    pub name: String,
    pub col_aliases: Vec<String>,
    pub body: CteBody,
    pub recursive: bool,
}

/// v0.10: a CTE body. Plain CTEs hold one SELECT; recursive CTEs hold the
/// `non_recursive UNION [ALL] recursive` pair (UNION elsewhere is not
/// supported in v0.10).
#[derive(Clone, Debug, PartialEq)]
pub enum CteBody {
    Simple(SelectStmt),
    Union {
        left: Box<SelectStmt>,
        right: Box<SelectStmt>,
        all: bool,
    },
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
    /// v0.9: the DEFAULT keyword in INSERT VALUES.
    Default,
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

/// v0.9: column DEFAULT. Stored parsed in memory; serialized to WAL /
/// checkpoints through the s-expression encoding (`encode_expr`).
#[derive(Clone, Debug, PartialEq)]
pub enum DefaultExpr {
    /// `DEFAULT <literal>`.
    Lit(Literal),
    /// `DEFAULT nextval('seq')` — recognized specially so the sequence
    /// dependency is visible and survives rewrites.
    Nextval(String),
    /// Any other default expression.
    Expr(Expr),
}

/// v0.9: a CHECK constraint (name + parsed expression).
#[derive(Clone, Debug, PartialEq)]
pub struct CheckDef {
    pub name: String,
    pub expr: Expr,
}

/// v0.9: a PRIMARY KEY or UNIQUE constraint over column names.
#[derive(Clone, Debug, PartialEq)]
pub struct UniqueDef {
    pub name: String,
    pub cols: Vec<String>,
}

/// v0.9: referential actions for foreign keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FkAction {
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

/// v0.9: a FOREIGN KEY constraint (column names; resolved at execution).
#[derive(Clone, Debug, PartialEq)]
pub struct FkDef {
    pub name: String,
    /// Child column names.
    pub cols: Vec<String>,
    pub ref_table: String,
    /// Parent column names (empty = parent's primary key, resolved later).
    pub ref_cols: Vec<String>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

/// v0.9: the full schema of a CREATE TABLE: columns plus constraints.
#[derive(Clone, Debug, PartialEq)]
pub struct TableDef {
    pub columns: Vec<(String, ColType)>,
    pub not_null: Vec<bool>,
    pub defaults: Vec<Option<DefaultExpr>>,
    pub checks: Vec<CheckDef>,
    pub uniques: Vec<UniqueDef>,
    pub pkey: Option<UniqueDef>,
    pub fks: Vec<FkDef>,
}

/// v0.9: ALTER TABLE actions.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterAction {
    AddColumn {
        name: String,
        col_type: ColType,
        not_null: bool,
        default: Option<DefaultExpr>,
        checks: Vec<CheckDef>,
        uniques: Vec<UniqueDef>,
        pkey: Option<UniqueDef>,
        fks: Vec<FkDef>,
    },
    DropColumn {
        name: String,
        cascade: bool,
    },
    AddConstraint {
        check: Option<CheckDef>,
        unique: Option<UniqueDef>,
        pkey: Option<UniqueDef>,
        fk: Option<FkDef>,
    },
    DropConstraint {
        name: String,
        cascade: bool,
    },
    AlterColumnSetDefault {
        name: String,
        default: DefaultExpr,
    },
    AlterColumnDropDefault {
        name: String,
    },
    RenameColumn {
        old: String,
        new: String,
    },
    RenameTo {
        new_name: String,
    },
    /// v0.11: ALTER TABLE name OWNER TO role.
    OwnerTo {
        new_owner: String,
    },
}

/// v0.9: CREATE / ALTER SEQUENCE options. `None` = keep current value
/// (ALTER) or the Postgres default (CREATE).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SequenceOpts {
    pub start: Option<i64>,
    pub increment: Option<i64>,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    pub cycle: Option<bool>,
    pub restart: Option<i64>,
}

impl SequenceOpts {
    /// `NO MINVALUE` / `NO MAXVALUE` sentinel: Postgres uses 1 /
    /// 2^63-1 for ascending sequences.
    pub fn no_minvalue() -> i64 {
        1
    }
    pub fn no_maxvalue() -> i64 {
        i64::MAX
    }
    /// Bare `RESTART` (no WITH value) resets to the sequence's start.
    pub const RESTART_SENTINEL: i64 = i64::MIN;
}

// ---------------------------------------------------------------------------
// v0.9: parser-internal intermediate forms for CREATE TABLE.
// ---------------------------------------------------------------------------

/// One item inside `CREATE TABLE (...)`.
enum TableItem {
    Col(ParsedColDef),
    TableCon(ParsedTableCon),
}

struct ParsedColDef {
    name: String,
    col_type: ColType,
    cons: Vec<ColCon>,
}

enum ColCon {
    NotNull,
    Null,
    Unique(Option<String>),
    PKey(Option<String>),
    Default(DefaultExpr),
    Check(Option<String>, Expr),
    References {
        name: Option<String>,
        tail: ParsedFkTail,
    },
}

enum ParsedTableCon {
    PKey(Option<String>, Vec<String>),
    Unique(Option<String>, Vec<String>),
    Check(Option<String>, Expr),
    Fk {
        name: Option<String>,
        cols: Vec<String>,
        tail: ParsedFkTail,
    },
}

struct ParsedFkTail {
    ref_table: String,
    ref_cols: Vec<String>,
    on_delete: FkAction,
    on_update: FkAction,
}

/// Classify a DEFAULT expression into its stored form.
fn classify_default(e: Expr) -> Result<DefaultExpr, SqlError> {
    match e {
        Expr::Literal(l) => Ok(DefaultExpr::Lit(l)),
        Expr::Func { name, args } if name == "nextval" && args.len() == 1 => {
            match args.into_iter().next() {
                Some(Expr::Literal(Literal::Text(s))) => Ok(DefaultExpr::Nextval(s)),
                _ => Err(err(
                    "nextval() in DEFAULT requires a sequence name string literal".to_string(),
                )),
            }
        }
        other => Ok(DefaultExpr::Expr(other)),
    }
}

impl TableDef {
    fn empty() -> Self {
        TableDef {
            columns: Vec::new(),
            not_null: Vec::new(),
            defaults: Vec::new(),
            checks: Vec::new(),
            uniques: Vec::new(),
            pkey: None,
            fks: Vec::new(),
        }
    }
}

/// Resolve a parsed CREATE TABLE item list into a `TableDef`, assigning
/// Postgres-style automatic constraint names.

fn def_col_exists(def: &TableDef, n: &str) -> bool {
    def.columns.iter().any(|(c, _)| c == n)
}

fn def_constraint_name_exists(def: &TableDef, cname: &str) -> bool {
    def.uniques.iter().any(|u| u.name == cname)
        || def.checks.iter().any(|c| c.name == cname)
        || def.fks.iter().any(|f| f.name == cname)
        || def.pkey.as_ref().map(|p| p.name == cname).unwrap_or(false)
}

fn def_add_pkey(
    table: &str,
    def: &mut TableDef,
    name: Option<String>,
    cols: &[String],
) -> Result<(), SqlError> {
    for c in cols {
        if !def_col_exists(def, c) {
            return Err(err(format!("column \"{}\" does not exist", c)));
        }
    }
    if def.pkey.is_some() {
        return Err(err("multiple primary keys for table".to_string()));
    }
    let cname = name.unwrap_or_else(|| format!("{}_pkey", table));
    if def_constraint_name_exists(def, &cname) {
        return Err(err(format!("constraint \"{}\" already exists", cname)));
    }
    for c in cols {
        let i = def.columns.iter().position(|(n, _)| n == c).unwrap();
        def.not_null[i] = true;
    }
    def.pkey = Some(UniqueDef {
        name: cname,
        cols: cols.to_vec(),
    });
    Ok(())
}

fn def_add_unique(
    table: &str,
    def: &mut TableDef,
    name: Option<String>,
    cols: &[String],
    col: &str,
) -> Result<(), SqlError> {
    for c in cols {
        if !def_col_exists(def, c) {
            return Err(err(format!("column \"{}\" does not exist", c)));
        }
    }
    let cname = name.unwrap_or_else(|| format!("{}_{}_key", table, col));
    if def_constraint_name_exists(def, &cname) {
        return Err(err(format!("constraint \"{}\" already exists", cname)));
    }
    def.uniques.push(UniqueDef {
        name: cname,
        cols: cols.to_vec(),
    });
    Ok(())
}

fn def_add_check(
    table: &str,
    def: &mut TableDef,
    name: Option<String>,
    e: Expr,
    col: &str,
) -> Result<(), SqlError> {
    let cname = name.unwrap_or_else(|| format!("{}_{}_check", table, col));
    if def_constraint_name_exists(def, &cname) {
        return Err(err(format!("constraint \"{}\" already exists", cname)));
    }
    // CHECK expressions may only reference this table's columns.
    let mut refs = Vec::new();
    collect_col_refs(&e, &mut refs);
    for (qual, r) in refs {
        if let Some(q) = qual {
            return Err(err(format!(
                "qualified column reference \"{}.{}\" not allowed in CHECK",
                q, r
            )));
        }
        if !def_col_exists(def, &r) {
            return Err(err(format!("column \"{}\" does not exist", r)));
        }
    }
    def.checks.push(CheckDef {
        name: cname,
        expr: e,
    });
    Ok(())
}

fn def_add_fk(
    table: &str,
    def: &mut TableDef,
    name: Option<String>,
    cols: &[String],
    tail: ParsedFkTail,
) -> Result<(), SqlError> {
    for c in cols {
        if !def_col_exists(def, c) {
            return Err(err(format!("column \"{}\" does not exist", c)));
        }
    }
    let cname = name.unwrap_or_else(|| format!("{}_{}_fkey", table, cols[0]));
    if def_constraint_name_exists(def, &cname) {
        return Err(err(format!("constraint \"{}\" already exists", cname)));
    }
    def.fks.push(FkDef {
        name: cname,
        cols: cols.to_vec(),
        ref_table: tail.ref_table,
        ref_cols: tail.ref_cols,
        on_delete: tail.on_delete,
        on_update: tail.on_update,
    });
    Ok(())
}

fn build_table_def(table: &str, items: Vec<TableItem>) -> Result<TableDef, SqlError> {
    let mut def = TableDef::empty();
    // Pass 1: columns.
    for item in &items {
        if let TableItem::Col(c) = item {
            if def.columns.iter().any(|(n, _)| n == &c.name) {
                // v0.12: PostgreSQL reports duplicate_column (42701) here,
                // not a syntax error.
                return Err(err_duplicate(format!(
                    "column \"{}\" specified more than once",
                    c.name
                )));
            }
            def.columns.push((c.name.clone(), c.col_type.clone()));
            def.not_null.push(false);
            def.defaults.push(None);
        }
    }
    if def.columns.is_empty() {
        return Err(err(
            "syntax error: table must have at least one column".to_string()
        ));
    }
    // Pass 2: constraints.
    for item in &items {
        match item {
            TableItem::Col(c) => {
                let i = def.columns.iter().position(|(n, _)| n == &c.name).unwrap();
                for con in &c.cons {
                    match con {
                        ColCon::NotNull => def.not_null[i] = true,
                        ColCon::Null => def.not_null[i] = false,
                        ColCon::Unique(n) => def_add_unique(
                            table,
                            &mut def,
                            n.clone(),
                            std::slice::from_ref(&c.name),
                            &c.name,
                        )?,
                        ColCon::PKey(n) => {
                            def_add_pkey(table, &mut def, n.clone(), std::slice::from_ref(&c.name))?
                        }
                        ColCon::Default(d) => def.defaults[i] = Some(d.clone()),
                        ColCon::Check(n, e) => {
                            def_add_check(table, &mut def, n.clone(), e.clone(), &c.name)?
                        }
                        ColCon::References { name: n, tail } => def_add_fk(
                            table,
                            &mut def,
                            n.clone(),
                            std::slice::from_ref(&c.name),
                            ParsedFkTail {
                                ref_table: tail.ref_table.clone(),
                                ref_cols: tail.ref_cols.clone(),
                                on_delete: tail.on_delete,
                                on_update: tail.on_update,
                            },
                        )?,
                    }
                }
            }
            TableItem::TableCon(tc) => match tc {
                ParsedTableCon::PKey(n, cols) => def_add_pkey(table, &mut def, n.clone(), cols)?,
                ParsedTableCon::Unique(n, cols) => {
                    let first = cols[0].clone();
                    def_add_unique(table, &mut def, n.clone(), cols, &first)?
                }
                ParsedTableCon::Check(n, e) => {
                    def_add_check(table, &mut def, n.clone(), e.clone(), table)?
                }
                ParsedTableCon::Fk {
                    name: n,
                    cols,
                    tail,
                } => def_add_fk(
                    table,
                    &mut def,
                    n.clone(),
                    cols,
                    ParsedFkTail {
                        ref_table: tail.ref_table.clone(),
                        ref_cols: tail.ref_cols.clone(),
                        on_delete: tail.on_delete,
                        on_update: tail.on_update,
                    },
                )?,
            },
        }
    }
    Ok(def)
}

/// Collect `(qualifier, name)` of every column reference in an expression.
pub(crate) fn collect_col_refs(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        Expr::Column { table, name } => out.push((table.clone(), name.clone())),
        Expr::Arith { left, right, .. } => {
            collect_col_refs(left, out);
            collect_col_refs(right, out);
        }
        Expr::Cast { expr, .. } => collect_col_refs(expr, out),
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            collect_col_refs(a, out);
            collect_col_refs(b, out);
        }
        Expr::Not(a) => collect_col_refs(a, out),
        Expr::Like { expr, pattern, .. } => {
            collect_col_refs(expr, out);
            collect_col_refs(pattern, out);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_col_refs(expr, out);
            collect_col_refs(low, out);
            collect_col_refs(high, out);
        }
        Expr::IsBool { expr, .. } | Expr::IsNull { expr, .. } => collect_col_refs(expr, out),
        Expr::Extract { from, .. } => collect_col_refs(from, out),
        Expr::Cmp { left, right, .. } => {
            collect_col_refs(left, out);
            collect_col_refs(right, out);
        }
        Expr::Func { args, .. } => {
            for a in args {
                collect_col_refs(a, out);
            }
        }
        Expr::Literal(_)
        | Expr::Param(_)
        | Expr::Agg { .. }
        | Expr::ScalarSub(_)
        | Expr::InSub { .. }
        | Expr::Exists { .. }
        | Expr::ResolvedCol { .. } => {}
        // v0.10: window functions — collect from args, PARTITION BY and
        // ORDER BY.
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                collect_col_refs(a, out);
            }
            for p in partition_by {
                collect_col_refs(p, out);
            }
            for o in order_by {
                collect_col_refs(&o.expr, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// v0.11: privileges and grant targets
// ---------------------------------------------------------------------------

/// v0.11: a GRANT/REVOKE privilege, optionally restricted to a column
/// list (`GRANT SELECT (a, b) ON t TO r`). An empty `columns` means the
/// privilege applies to the whole object.
#[derive(Clone, Debug, PartialEq)]
pub struct PrivSpec {
    pub priv_: Privilege,
    pub columns: Vec<String>,
}

/// A single privilege name from GRANT / REVOKE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
    All,
    /// USAGE (sequences).
    Usage,
    /// CONNECT (database).
    Connect,
}

impl Privilege {
    /// Bitmask for table/sequence ACLs. CONNECT has no per-object bit.
    pub fn bits(self) -> u32 {
        match self {
            Privilege::Select => crate::storage::PRIV_SELECT,
            Privilege::Insert => crate::storage::PRIV_INSERT,
            Privilege::Update => crate::storage::PRIV_UPDATE,
            Privilege::Delete => crate::storage::PRIV_DELETE,
            Privilege::Truncate => crate::storage::PRIV_TRUNCATE,
            Privilege::References => crate::storage::PRIV_REFERENCES,
            Privilege::Trigger => crate::storage::PRIV_TRIGGER,
            Privilege::All => crate::storage::PRIV_ALL_TABLE,
            Privilege::Usage => crate::storage::PRIV_USAGE,
            Privilege::Connect => crate::storage::PRIV_CONNECT,
        }
    }
}

/// What a GRANT / REVOKE applies to.
#[derive(Clone, Debug)]
pub enum GrantObject {
    Table(String),
    Sequence(String),
    Database,
}

/// v0.16: FETCH / MOVE direction. Counts are row counts; `None` means ALL.
#[derive(Clone, Debug)]
pub enum FetchDir {
    /// `NEXT`, bare `FETCH`, or `FORWARD [n]`.
    Forward(Option<i64>),
    /// `PRIOR` or `BACKWARD [n]`.
    Backward(Option<i64>),
    Absolute(i64),
    Relative(i64),
    First,
    Last,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    CreateTable {
        name: String,
        def: TableDef,
    },
    // --- v0.9: ALTER TABLE ---
    AlterTable {
        name: String,
        action: AlterAction,
    },
    // --- v0.9: views ---
    CreateView {
        name: String,
        query: String,
        col_aliases: Vec<String>,
        or_replace: bool,
    },
    DropView {
        names: Vec<String>,
        if_exists: bool,
        cascade: bool,
    },
    // --- v0.9: sequences ---
    CreateSequence {
        name: String,
        if_not_exists: bool,
        opts: SequenceOpts,
    },
    AlterSequence {
        name: String,
        opts: SequenceOpts,
    },
    DropSequence {
        names: Vec<String>,
        if_exists: bool,
    },
    // --- v0.11: roles and privileges ---
    CreateRole {
        name: String,
        login: bool,
        superuser: bool,
        /// Cleartext password from PASSWORD '...'; hashed into a SCRAM
        /// verifier at execution. None = no password (trust-only).
        password: Option<String>,
        /// CONNECTION LIMIT n; None = unlimited (-1).
        connlimit: Option<i32>,
        /// VALID UNTIL 'timestamp'; None = never expires.
        valid_until: Option<String>,
    },
    AlterRole {
        name: String,
        login: Option<bool>,
        superuser: Option<bool>,
        /// Some(Some(pw)) = set password, Some(None) = PASSWORD NULL
        /// (remove), None = leave unchanged.
        password: Option<Option<String>>,
        connlimit: Option<i32>,
        /// Some(ts) = set expiry, None = leave unchanged. There is no
        /// way to clear an expiry except VALID UNTIL 'infinity'.
        valid_until: Option<String>,
    },
    DropRole {
        names: Vec<String>,
        if_exists: bool,
    },
    Grant {
        privs: Vec<PrivSpec>,
        object: GrantObject,
        grantees: Vec<String>,
    },
    Revoke {
        privs: Vec<PrivSpec>,
        object: GrantObject,
        grantees: Vec<String>,
    },
    /// GRANT role [, ...] TO role [, ...] — role membership.
    GrantRole {
        roles: Vec<String>,
        grantees: Vec<String>,
    },
    /// REVOKE role [, ...] FROM role [, ...] — remove membership.
    RevokeRole {
        roles: Vec<String>,
        grantees: Vec<String>,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<InsertValue>>,
        /// v0.10: `INSERT INTO ... SELECT ...` source (mutually exclusive
        /// with `rows`).
        select: Option<SelectStmt>,
        /// v0.10: `ON CONFLICT ...`.
        on_conflict: Option<OnConflict>,
        /// v0.10: `RETURNING ...`.
        returning: Vec<SelectItem>,
        /// v0.10: `WITH ...` CTEs visible to the statement.
        with: Vec<CteDef>,
    },
    Select(SelectStmt),
    DropTable {
        if_exists: bool,
        names: Vec<String>,
        cascade: bool,
    },
    // --- v0.5: UPDATE / DELETE with MVCC semantics
    Update {
        table: String,
        /// (column, expression) assignments.
        sets: Vec<(String, Expr)>,
        where_: Vec<WhereCond>,
        /// v0.10: `RETURNING ...`.
        returning: Vec<SelectItem>,
        /// v0.10: `WITH ...` CTEs visible to the statement.
        with: Vec<CteDef>,
    },
    Delete {
        table: String,
        where_: Vec<WhereCond>,
        /// v0.10: `RETURNING ...`.
        returning: Vec<SelectItem>,
        /// v0.10: `WITH ...` CTEs visible to the statement.
        with: Vec<CteDef>,
    },
    // --- v0.16: TRUNCATE ----------------------------------------------------
    Truncate {
        tables: Vec<String>,
        /// `RESTART IDENTITY`: reset sequences owned by the truncated
        /// tables (without it, sequences are untouched).
        restart_identity: bool,
        /// `CASCADE`: skip the referenced-by-foreign-key check (without
        /// it, truncating an FK-referenced table is an error, like PG).
        cascade: bool,
    },
    // --- v0.16: SQL cursors --------------------------------------------------
    Declare {
        name: String,
        query: SelectStmt,
        with_hold: bool,
    },
    Fetch {
        name: String,
        dir: FetchDir,
    },
    Close {
        /// None = `CLOSE ALL`.
        name: Option<String>,
    },
    Move {
        name: String,
        dir: FetchDir,
    },
    // --- v0.10: COPY -------------------------------------------------------
    Copy {
        table: String,
        columns: Option<Vec<String>>,
        /// true = `TO STDOUT`, false = `FROM STDIN`.
        to_stdout: bool,
        options: CopyOptions,
    },
    // --- v0.3: transaction control (handled by the session, not the executor)
    Begin {
        level: Option<IsolationLevel>,
        /// v0.15: READ ONLY / READ WRITE mode (None = not specified).
        /// Parsed for PostgreSQL syntax compatibility; not yet enforced.
        read_only: Option<bool>,
        /// v0.15: [NOT] DEFERRABLE mode (None = not specified).
        /// Parsed for PostgreSQL syntax compatibility; not yet enforced.
        deferrable: Option<bool>,
    },
    Commit {
        /// v0.15: COMMIT AND CHAIN — commit, then immediately start a new
        /// transaction with the same characteristics (SQL standard).
        chain: bool,
    },
    Rollback {
        /// v0.15: ROLLBACK AND CHAIN.
        chain: bool,
    },
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
        // v0.14: `VACUUM ANALYZE` also collects planner statistics.
        analyze: bool,
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
    // --- v0.17: transaction characteristics / GUCs -------------------------
    /// `SET TRANSACTION mode [, ...]` — set characteristics of the
    /// current transaction (no-op with no transaction, like PG's
    /// WARNING-less path; we have no NOTICE channel).
    SetTransaction {
        level: Option<IsolationLevel>,
        read_only: Option<bool>,
        deferrable: Option<bool>,
    },
    /// `SET SESSION CHARACTERISTICS AS TRANSACTION mode [, ...]` —
    /// defaults for subsequent transactions of this session.
    SetSessionCharacteristics {
        level: Option<IsolationLevel>,
        read_only: Option<bool>,
        deferrable: Option<bool>,
    },
    /// `SET name = value` / `SET name TO value` — session GUC.
    /// Only `default_transaction_read_only` is honored; anything else
    /// is 42704 (the GUC surface is intentionally small, documented).
    Set {
        name: String,
        value: SetValue,
    },
    /// `SHOW name`.
    Show {
        name: String,
    },
    /// `RESET name` / `RESET ALL`.
    Reset {
        name: String,
    },
}

/// v0.10: `INSERT ... ON CONFLICT ...`.
#[derive(Clone, Debug, PartialEq)]
pub struct OnConflict {
    pub arbiter: ConflictArbiter,
    pub action: ConflictAction,
}

/// v0.10: conflict arbiter. `None` = no arbiter (`ON CONFLICT DO NOTHING`
/// catches any unique violation).
#[derive(Clone, Debug, PartialEq)]
pub enum ConflictArbiter {
    None,
    Columns(Vec<String>),
    Constraint(String),
}

/// v0.10: `ON CONFLICT` action.
#[derive(Clone, Debug, PartialEq)]
pub enum ConflictAction {
    DoNothing,
    DoUpdate {
        sets: Vec<(String, Expr)>,
        where_: Option<Expr>,
    },
}

/// v0.10: `COPY` format options.
#[derive(Clone, Debug, PartialEq)]
pub struct CopyOptions {
    pub format: CopyFormat,
    pub delimiter: u8,
    pub null: String,
    pub header: bool,
    pub quote: u8,
    pub escape: u8,
}

/// v0.10: `COPY` data format (BINARY is not supported).
#[derive(Clone, Debug, PartialEq)]
pub enum CopyFormat {
    Text,
    Csv,
}

impl Default for CopyOptions {
    fn default() -> Self {
        CopyOptions {
            format: CopyFormat::Text,
            delimiter: b'\t',
            null: "\\N".to_string(),
            header: false,
            quote: b'"',
            escape: b'"',
        }
    }
}

/// v0.17: value of a `SET name = value` statement.
#[derive(Clone, Debug, PartialEq)]
pub enum SetValue {
    /// `DEFAULT` — reset to the built-in default.
    Default,
    /// String literal, identifier, or number, as written (idents are
    /// already case-folded by the tokenizer).
    Str(String),
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
            Stmt::Insert {
                rows,
                select,
                on_conflict,
                returning,
                with,
                ..
            } => {
                let mut m = 0;
                for row in rows {
                    for v in row {
                        if let InsertValue::Param(n) = v {
                            m = m.max(*n as usize);
                        }
                    }
                }
                if let Some(sel) = select {
                    m = m.max(max_param_select(sel));
                }
                if let Some(oc) = on_conflict {
                    m = m.max(max_param_on_conflict(oc));
                }
                m = m.max(max_param_returning(returning));
                m = m.max(max_param_ctes(with));
                m
            }
            Stmt::Select(sel) => max_param_select(sel),
            Stmt::Explain { stmt } => stmt.max_param(),
            Stmt::Update {
                sets,
                where_,
                returning,
                with,
                ..
            } => {
                let mut m = 0;
                for (_, e) in sets {
                    m = m.max(max_param_expr(e));
                }
                for w in where_ {
                    if let WhereRhs::Param(n) = w.rhs {
                        m = m.max(n as usize);
                    }
                }
                m = m.max(max_param_returning(returning));
                m = m.max(max_param_ctes(with));
                m
            }
            Stmt::Delete {
                where_,
                returning,
                with,
                ..
            } => {
                let mut m = 0;
                for w in where_ {
                    if let WhereRhs::Param(n) = w.rhs {
                        m = m.max(n as usize);
                    }
                }
                m = m.max(max_param_returning(returning));
                m = m.max(max_param_ctes(with));
                m
            }
            _ => 0,
        }
    }
}

/// v0.10: params inside CTE definitions.
fn max_param_ctes(ctes: &[CteDef]) -> usize {
    let mut m = 0;
    for c in ctes {
        match &c.body {
            CteBody::Simple(s) => m = m.max(max_param_select(s)),
            CteBody::Union { left, right, .. } => {
                m = m.max(max_param_select(left).max(max_param_select(right)));
            }
        }
    }
    m
}

/// v0.10: params inside `ON CONFLICT ... DO UPDATE`.
fn max_param_on_conflict(oc: &OnConflict) -> usize {
    match &oc.action {
        ConflictAction::DoNothing => 0,
        ConflictAction::DoUpdate { sets, where_ } => {
            let mut m = 0;
            for (_, e) in sets {
                m = m.max(max_param_expr(e));
            }
            if let Some(w) = where_ {
                m = m.max(max_param_expr(w));
            }
            m
        }
    }
}

/// v0.10: params inside `RETURNING`.
fn max_param_returning(items: &[SelectItem]) -> usize {
    let mut m = 0;
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            m = m.max(max_param_expr(expr));
        }
    }
    m
}

fn max_param_select(s: &SelectStmt) -> usize {
    let mut m = max_param_ctes(&s.with);
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
        FromItem::Values { rows, .. } => rows
            .iter()
            .flat_map(|r| r.iter())
            .map(max_param_expr)
            .max()
            .unwrap_or(0),
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
        Expr::Like { expr, pattern, .. } => max_param_expr(expr).max(max_param_expr(pattern)),
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
        // v0.10: window functions.
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => args
            .iter()
            .map(max_param_expr)
            .max()
            .unwrap_or(0)
            .max(partition_by.iter().map(max_param_expr).max().unwrap_or(0))
            .max(
                order_by
                    .iter()
                    .map(|o| max_param_expr(&o.expr))
                    .max()
                    .unwrap_or(0),
            ),
    }
}

/// Parse a single statement. A single trailing semicolon is stripped.
pub fn parse_statement(input: &str) -> Result<Stmt, SqlError> {
    let mut text = input.trim();
    if let Some(stripped) = text.strip_suffix(';') {
        text = stripped.trim_end();
    }
    // v0.9: CREATE VIEW needs the raw query text for the catalog, so it is
    // split off before tokenizing (tokens carry no spans).
    if let Some(view_stmt) = try_split_create_view(text) {
        return view_stmt;
    }
    parse_statement_inner(text)
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
            | "close" // v0.16: cursors
            | "commit"
            | "create"
            | "cross"
            | "declare" // v0.16: cursors
            | "delete"
            | "desc"
            | "distinct"
            | "drop"
            | "end"
            | "exists"
            | "fetch" // v0.16: cursors
            | "for"
            | "from"
            | "full" // v0.20.1: RIGHT/FULL JOIN (was eaten as a table alias)
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
            | "move" // v0.16: cursors
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
            | "returning" // v0.10: RETURNING clause
            | "right" // v0.20.1: RIGHT JOIN (was eaten as a table alias)
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
            | "union" // v0.10: recursive CTEs
            | "update"
            | "vacuum"
            | "values"
            | "verbose"
            | "where"
            | "using" // v0.20: JOIN ... USING
            | "natural" // v0.20: NATURAL JOIN
    )
}

/// v0.10: output column count of a SELECT, when statically known
/// (`SELECT *` makes it unknown).
fn cte_width(s: &SelectStmt) -> Option<usize> {
    let mut n = 0;
    for item in &s.items {
        match item {
            SelectItem::Expr { .. } => n += 1,
            SelectItem::All | SelectItem::AllOf(_) => return None,
        }
    }
    Some(n)
}

/// v0.10: the parsed contents of `OVER (...)`, before conversion into
/// `Expr::Window`.
struct WindowSpec {
    partition_by: Vec<Expr>,
    order_by: Vec<OrderTerm>,
    frame: WindowFrame,
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// v0.14: counter for auto-generated `unnamed_subquery[_N]` aliases,
    /// matching what PostgreSQL reports for alias-less FROM subqueries.
    unnamed_seq: usize,
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
            // --- v0.11: GRANT / REVOKE
            "grant" => self.parse_grant(),
            "revoke" => self.parse_revoke(),
            // --- v0.5: UPDATE / DELETE
            "update" => self.parse_update(),
            "delete" => self.parse_delete(),
            // --- v0.16: TRUNCATE / cursors
            "truncate" => self.parse_truncate(),
            "declare" => self.parse_declare(),
            "fetch" => self.parse_fetch(),
            "close" => self.parse_close(),
            "move" => self.parse_move(),
            // --- v0.5: VACUUM
            "vacuum" => self.parse_vacuum(),
            // --- v0.3: transaction control
            "begin" => {
                // BEGIN [WORK | TRANSACTION] [transaction_mode [, ...]]
                let _ = self.eat_keyword("transaction") || self.eat_keyword("work");
                self.parse_begin_rest()
            }
            "start" => {
                self.expect_keyword("transaction")?;
                self.parse_begin_rest()
            }
            "commit" | "end" => {
                // COMMIT [WORK | TRANSACTION] [AND [NO] CHAIN]
                // (END is COMMIT's alias and takes the same clauses.)
                let _ = self.eat_keyword("transaction") || self.eat_keyword("work");
                let chain = if self.eat_keyword("and") {
                    let no = self.eat_keyword("no");
                    self.expect_keyword("chain")?;
                    !no
                } else {
                    false
                };
                Ok(Stmt::Commit { chain })
            }
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
                    // ROLLBACK [WORK | TRANSACTION] [AND [NO] CHAIN]
                    // (ABORT is ROLLBACK's alias; accept the same clauses.)
                    let _ = self.eat_keyword("transaction") || self.eat_keyword("work");
                    let chain = if self.eat_keyword("and") {
                        let no = self.eat_keyword("no");
                        self.expect_keyword("chain")?;
                        !no
                    } else {
                        false
                    };
                    Ok(Stmt::Rollback { chain })
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
            // --- v0.9: ALTER TABLE / ALTER SEQUENCE
            "alter" => match self.peek() {
                Token::Ident(ref s) if s == "table" => self.parse_alter(),
                Token::Ident(ref s) if s == "sequence" => self.parse_alter_sequence(),
                Token::Ident(ref s) if s == "role" || s == "user" || s == "group" => {
                    self.parse_alter_role()
                }
                _ => Err(err(
                    "syntax error: expected TABLE, SEQUENCE or ROLE after ALTER".to_string(),
                )),
            },
            // --- v0.17: SET / SHOW / RESET
            "set" => self.parse_set(),
            "show" => Ok(Stmt::Show {
                name: self.expect_ident()?,
            }),
            "reset" => {
                // RESET name | RESET ALL
                let name = self.expect_ident()?;
                Ok(Stmt::Reset { name })
            }
            // --- v0.10: WITH [RECURSIVE] ... / COPY
            "with" => self.parse_with(),
            "copy" => self.parse_copy(),
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
            // v0.14: PostgreSQL internal alias names (pg_regress conformance).
            "int4" => Ok(ColType::Int),
            "bigint" | "int8" => Ok(ColType::BigInt),
            "smallint" | "int2" => Ok(ColType::SmallInt),
            // v0.14: `serial` is accepted as an integer alias for casts and
            // column definitions. Unlike PostgreSQL we do not auto-create a
            // backing sequence or DEFAULT nextval(); documented in README.
            "serial" => Ok(ColType::Int),
            // v0.14: character type aliases. Length modifiers are parsed and
            // ignored; values are stored as text (no blank-padding or
            // truncation enforcement), documented in README.
            "varchar" => {
                self.eat_optional_typmod();
                Ok(ColType::Text)
            }
            "char" | "bpchar" => {
                self.eat_optional_typmod();
                Ok(ColType::Text)
            }
            "character" => {
                // `character varying(n)` or plain `character(n)`.
                self.eat_keyword("varying");
                self.eat_optional_typmod();
                Ok(ColType::Text)
            }
            // v0.14: `name` (PostgreSQL internal identifier type) behaves
            // like text here; the 63-byte truncation is not enforced.
            "name" => Ok(ColType::Text),
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

    /// Consume an optional `(n)` or `(n, m)` length/precision modifier,
    /// ignoring the values (v0.14: character-type typmods are parsed but
    /// not enforced).
    fn eat_optional_typmod(&mut self) {
        if self.peek() != Token::LParen {
            return;
        }
        self.next(); // '('
        let mut depth = 1usize;
        while depth > 0 {
            match self.next() {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                Token::EOF => break,
                _ => {}
            }
        }
    }

    /// First word of a type name (for typed-literal lookahead like
    /// `DATE '2026-01-01'`).
    fn is_type_start(name: &str) -> bool {
        matches!(
            name,
            "int"
                | "integer"
                | "int4"
                | "bigint"
                | "int8"
                | "smallint"
                | "int2"
                | "serial"
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
                | "varchar"
                | "char"
                | "character"
                | "bpchar"
                | "name"
                | "date"
                | "timestamp"
                | "timestamptz"
                | "bytea"
                | "uuid"
        )
    }

    fn parse_create(&mut self) -> Result<Stmt, SqlError> {
        // v0.11: CREATE ROLE / USER / GROUP
        if matches!(self.peek(), Token::Ident(ref s) if s == "role" || s == "user" || s == "group")
        {
            return self.parse_create_role();
        }
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
            let name = if matches!(self.peek(), Token::Ident(ref s) if s == "on") {
                // v0.14: the index name may be omitted
                // (`CREATE INDEX ON t (a, b)`); PostgreSQL auto-names it
                // <table>_<columns>_idx. Filled in after parsing columns.
                String::new()
            } else {
                self.expect_ident()?
            };
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
                return Err(err(
                    "syntax error: index requires at least one column".to_string()
                ));
            }
            let name = if name.is_empty() {
                format!("{}_{}_idx", table, columns.join("_"))
            } else {
                name
            };
            return Ok(Stmt::CreateIndex {
                name,
                table,
                columns,
                unique,
                if_not_exists,
            });
        }
        // v0.9: CREATE SEQUENCE (CREATE VIEW is intercepted before
        // tokenizing so the raw query text survives).
        if matches!(self.peek(), Token::Ident(ref s) if s == "sequence") {
            return self.parse_create_sequence();
        }
        // v0.14: CREATE [ { TEMPORARY | TEMP } | { GLOBAL | LOCAL } ] TABLE.
        // TEMPORARY is accepted for pg_regress conformance but currently
        // behaves as a regular persistent table: per-session scoping and
        // ON COMMIT behavior are not implemented (documented in README).
        if self.eat_keyword("temporary") || self.eat_keyword("temp") {
            // TEMP accepted, TEMPORARY semantics not implemented
        } else {
            // GLOBAL/LOCAL are noise words in PostgreSQL (SQL standard
            // scoping); accept and ignore them as well.
            let _ = self.eat_keyword("global") || self.eat_keyword("local");
            let _ = self.eat_keyword("temporary") || self.eat_keyword("temp");
        }
        self.expect_keyword("table")?;
        let name = self.expect_ident()?;
        self.expect(Token::LParen, "'('")?;
        let mut items: Vec<TableItem> = Vec::new();
        loop {
            if self.is_table_constraint_start() {
                items.push(TableItem::TableCon(self.parse_table_constraint()?));
            } else {
                items.push(TableItem::Col(self.parse_column_def()?));
            }
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
        let def = build_table_def(&name, items)?;
        Ok(Stmt::CreateTable { name, def })
    }

    /// True when the next tokens start a table-level constraint rather
    /// than a column definition.
    fn is_table_constraint_start(&mut self) -> bool {
        matches!(self.peek(), Token::Ident(ref s)
            if s == "constraint" || s == "primary" || s == "unique"
                || s == "check" || s == "foreign")
    }

    /// `name type [column constraints...]`.
    fn parse_column_def(&mut self) -> Result<ParsedColDef, SqlError> {
        let name = self.expect_ident()?;
        let col_type = self.parse_col_type()?;
        let mut cons = Vec::new();
        loop {
            let cname = if self.eat_keyword("constraint") {
                Some(self.expect_ident()?)
            } else {
                None
            };
            if self.eat_keyword("not") {
                self.expect_keyword("null")?;
                cons.push(ColCon::NotNull);
            } else if self.eat_keyword("null") {
                cons.push(ColCon::Null);
            } else if self.eat_keyword("unique") {
                cons.push(ColCon::Unique(cname));
            } else if self.eat_keyword("primary") {
                self.expect_keyword("key")?;
                cons.push(ColCon::PKey(cname));
            } else if self.eat_keyword("default") {
                let e = self.parse_or()?;
                validate_constraint_expr(&e, "DEFAULT")?;
                cons.push(ColCon::Default(classify_default(e)?));
            } else if self.eat_keyword("check") {
                self.expect(Token::LParen, "'('")?;
                let e = self.parse_or()?;
                self.expect(Token::RParen, "')'")?;
                validate_constraint_expr(&e, "CHECK")?;
                cons.push(ColCon::Check(cname, e));
            } else if self.eat_keyword("references") {
                let tail = self.parse_fk_tail()?;
                cons.push(ColCon::References { name: cname, tail });
            } else {
                if cname.is_some() {
                    return Err(err(
                        "syntax error: expected constraint type after CONSTRAINT name".to_string(),
                    ));
                }
                break;
            }
        }
        Ok(ParsedColDef {
            name,
            col_type,
            cons,
        })
    }

    /// Parse a table-level constraint: `[CONSTRAINT name] PRIMARY KEY (cols)
    /// | UNIQUE (cols) | CHECK (expr) | FOREIGN KEY (cols) REFERENCES ...`.
    fn parse_table_constraint(&mut self) -> Result<ParsedTableCon, SqlError> {
        let cname = if self.eat_keyword("constraint") {
            Some(self.expect_ident()?)
        } else {
            None
        };
        if self.eat_keyword("primary") {
            self.expect_keyword("key")?;
            let cols = self.parse_col_name_list()?;
            Ok(ParsedTableCon::PKey(cname, cols))
        } else if self.eat_keyword("unique") {
            let cols = self.parse_col_name_list()?;
            Ok(ParsedTableCon::Unique(cname, cols))
        } else if self.eat_keyword("check") {
            self.expect(Token::LParen, "'('")?;
            let e = self.parse_or()?;
            self.expect(Token::RParen, "')'")?;
            validate_constraint_expr(&e, "CHECK")?;
            Ok(ParsedTableCon::Check(cname, e))
        } else if self.eat_keyword("foreign") {
            self.expect_keyword("key")?;
            let cols = self.parse_col_name_list()?;
            self.expect_keyword("references")?;
            let tail = self.parse_fk_tail()?;
            Ok(ParsedTableCon::Fk {
                name: cname,
                cols,
                tail,
            })
        } else {
            Err(err(
                "syntax error: expected PRIMARY KEY, UNIQUE, CHECK or FOREIGN KEY".to_string(),
            ))
        }
    }

    fn parse_col_name_list(&mut self) -> Result<Vec<String>, SqlError> {
        self.expect(Token::LParen, "'('")?;
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
        if cols.is_empty() {
            return Err(err("syntax error: empty column list".to_string()));
        }
        Ok(cols)
    }

    /// `reftable [(refcol [, ...])] [ON DELETE action] [ON UPDATE action]`,
    /// after the REFERENCES keyword.
    fn parse_fk_tail(&mut self) -> Result<ParsedFkTail, SqlError> {
        let ref_table = self.expect_ident()?;
        let mut ref_cols = Vec::new();
        if self.peek() == Token::LParen {
            ref_cols = self.parse_col_name_list()?;
        }
        let mut on_delete = FkAction::Restrict;
        let mut on_update = FkAction::Restrict;
        loop {
            if self.eat_keyword("on") {
                if self.eat_keyword("delete") {
                    on_delete = self.parse_fk_action()?;
                } else if self.eat_keyword("update") {
                    on_update = self.parse_fk_action()?;
                } else {
                    return Err(err(
                        "syntax error: expected DELETE or UPDATE after ON".to_string()
                    ));
                }
            } else {
                break;
            }
        }
        Ok(ParsedFkTail {
            ref_table,
            ref_cols,
            on_delete,
            on_update,
        })
    }

    fn parse_fk_action(&mut self) -> Result<FkAction, SqlError> {
        if self.eat_keyword("cascade") {
            Ok(FkAction::Cascade)
        } else if self.eat_keyword("restrict") {
            Ok(FkAction::Restrict)
        } else if self.eat_keyword("set") {
            if self.eat_keyword("null") {
                Ok(FkAction::SetNull)
            } else if self.eat_keyword("default") {
                Ok(FkAction::SetDefault)
            } else {
                Err(err(
                    "syntax error: expected NULL or DEFAULT after SET".to_string()
                ))
            }
        } else if self.eat_keyword("no") {
            self.expect_keyword("action")?;
            Ok(FkAction::Restrict)
        } else {
            Err(err(
                "syntax error: expected CASCADE, RESTRICT, SET NULL, SET DEFAULT or NO ACTION"
                    .to_string(),
            ))
        }
    }

    /// ALTER TABLE name <action>.
    fn parse_alter(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("table")?;
        let name = self.expect_ident()?;
        let action = self.parse_alter_action()?;
        Ok(Stmt::AlterTable { name, action })
    }

    fn parse_alter_action(&mut self) -> Result<AlterAction, SqlError> {
        // v0.11: ALTER TABLE name OWNER TO role
        if self.eat_keyword("owner") {
            self.expect_keyword("to")?;
            return Ok(AlterAction::OwnerTo {
                new_owner: self.expect_ident()?,
            });
        }
        if self.eat_keyword("add") {
            if self.is_table_constraint_start() {
                return self.parse_alter_add_constraint();
            }
            self.eat_keyword("column");
            let col = self.parse_column_def()?;
            let mut not_null = false;
            let mut default = None;
            let mut checks = Vec::new();
            let mut uniques = Vec::new();
            let mut pkey = None;
            let mut fks = Vec::new();
            for con in col.cons {
                match con {
                    ColCon::NotNull => not_null = true,
                    ColCon::Null => not_null = false,
                    ColCon::Unique(n) => uniques.push(UniqueDef {
                        name: n.unwrap_or_else(|| format!("{}_key", col.name)),
                        cols: vec![col.name.clone()],
                    }),
                    ColCon::PKey(n) => {
                        pkey = Some(UniqueDef {
                            name: n.unwrap_or_else(|| format!("{}_pkey", col.name)),
                            cols: vec![col.name.clone()],
                        });
                        not_null = true;
                    }
                    ColCon::Default(d) => default = Some(d),
                    ColCon::Check(n, e) => checks.push(CheckDef {
                        name: n.unwrap_or_else(|| format!("{}_check", col.name)),
                        expr: e,
                    }),
                    ColCon::References { name: n, tail } => fks.push(FkDef {
                        name: n.unwrap_or_else(|| format!("{}_fkey", col.name)),
                        cols: vec![col.name.clone()],
                        ref_table: tail.ref_table,
                        ref_cols: tail.ref_cols,
                        on_delete: tail.on_delete,
                        on_update: tail.on_update,
                    }),
                }
            }
            return Ok(AlterAction::AddColumn {
                name: col.name,
                col_type: col.col_type,
                not_null,
                default,
                checks,
                uniques,
                pkey,
                fks,
            });
        }
        if self.eat_keyword("drop") {
            if self.eat_keyword("constraint") {
                let cname = self.expect_ident()?;
                let cascade = self.parse_cascade_opt()?;
                return Ok(AlterAction::DropConstraint {
                    name: cname,
                    cascade,
                });
            }
            self.eat_keyword("column");
            let cname = self.expect_ident()?;
            let cascade = self.parse_cascade_opt()?;
            return Ok(AlterAction::DropColumn {
                name: cname,
                cascade,
            });
        }
        if self.eat_keyword("alter") {
            self.eat_keyword("column");
            let cname = self.expect_ident()?;
            if self.eat_keyword("set") {
                self.expect_keyword("default")?;
                let e = self.parse_or()?;
                validate_constraint_expr(&e, "DEFAULT")?;
                return Ok(AlterAction::AlterColumnSetDefault {
                    name: cname,
                    default: classify_default(e)?,
                });
            }
            if self.eat_keyword("drop") {
                self.expect_keyword("default")?;
                return Ok(AlterAction::AlterColumnDropDefault { name: cname });
            }
            return Err(err(
                "syntax error: expected SET DEFAULT or DROP DEFAULT".to_string()
            ));
        }
        if self.eat_keyword("rename") {
            if self.eat_keyword("column") {
                let old = self.expect_ident()?;
                self.expect_keyword("to")?;
                let new = self.expect_ident()?;
                return Ok(AlterAction::RenameColumn { old, new });
            }
            self.expect_keyword("to")?;
            let new_name = self.expect_ident()?;
            return Ok(AlterAction::RenameTo { new_name });
        }
        Err(err(
            "syntax error: expected ADD, DROP, ALTER or RENAME".to_string()
        ))
    }

    /// `ADD [CONSTRAINT name] PRIMARY KEY ... | UNIQUE ... | CHECK ... |
    /// FOREIGN KEY ...` (table-constraint form).
    fn parse_alter_add_constraint(&mut self) -> Result<AlterAction, SqlError> {
        match self.parse_table_constraint()? {
            ParsedTableCon::PKey(name, cols) => Ok(AlterAction::AddConstraint {
                pkey: Some(UniqueDef {
                    name: name.unwrap_or_default(),
                    cols,
                }),
                check: None,
                unique: None,
                fk: None,
            }),
            ParsedTableCon::Unique(name, cols) => Ok(AlterAction::AddConstraint {
                unique: Some(UniqueDef {
                    name: name.unwrap_or_default(),
                    cols,
                }),
                check: None,
                pkey: None,
                fk: None,
            }),
            ParsedTableCon::Check(name, e) => Ok(AlterAction::AddConstraint {
                check: Some(CheckDef {
                    name: name.unwrap_or_default(),
                    expr: e,
                }),
                unique: None,
                pkey: None,
                fk: None,
            }),
            ParsedTableCon::Fk { name, cols, tail } => Ok(AlterAction::AddConstraint {
                fk: Some(FkDef {
                    name: name.unwrap_or_default(),
                    cols,
                    ref_table: tail.ref_table,
                    ref_cols: tail.ref_cols,
                    on_delete: tail.on_delete,
                    on_update: tail.on_update,
                }),
                check: None,
                unique: None,
                pkey: None,
            }),
        }
    }

    fn parse_cascade_opt(&mut self) -> Result<bool, SqlError> {
        if self.eat_keyword("cascade") {
            Ok(true)
        } else {
            // RESTRICT is the default; consume it if present.
            self.eat_keyword("restrict");
            Ok(false)
        }
    }

    /// CREATE SEQUENCE name [options...].
    fn parse_create_sequence(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("sequence")?;
        let if_not_exists = if self.eat_keyword("if") {
            self.expect_keyword("not")?;
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let name = self.expect_ident()?;
        let opts = self.parse_sequence_opts()?;
        Ok(Stmt::CreateSequence {
            name,
            if_not_exists,
            opts,
        })
    }

    /// ALTER SEQUENCE name [options...] — all options optional.
    fn parse_alter_sequence(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("sequence")?;
        if self.eat_keyword("if") {
            self.expect_keyword("exists")?;
        }
        let name = self.expect_ident()?;
        let opts = self.parse_sequence_opts()?;
        Ok(Stmt::AlterSequence { name, opts })
    }

    fn parse_sequence_opts(&mut self) -> Result<SequenceOpts, SqlError> {
        let mut opts = SequenceOpts::default();
        loop {
            if self.eat_keyword("start") {
                if self.eat_keyword("with") {
                    opts.start = Some(self.parse_seq_int("START")?);
                } else {
                    return Err(err("syntax error: expected WITH after START".to_string()));
                }
            } else if self.eat_keyword("increment") {
                if self.eat_keyword("by") {
                    opts.increment = Some(self.parse_seq_int("INCREMENT")?);
                } else {
                    return Err(err("syntax error: expected BY after INCREMENT".to_string()));
                }
            } else if self.eat_keyword("minvalue") {
                opts.min_value = Some(self.parse_seq_int("MINVALUE")?);
            } else if self.eat_keyword("no") {
                if self.eat_keyword("minvalue") {
                    opts.min_value = Some(SequenceOpts::no_minvalue());
                } else if self.eat_keyword("maxvalue") {
                    opts.max_value = Some(SequenceOpts::no_maxvalue());
                } else if self.eat_keyword("cycle") {
                    opts.cycle = Some(false);
                } else {
                    return Err(err(
                        "syntax error: expected MINVALUE, MAXVALUE or CYCLE after NO".to_string(),
                    ));
                }
            } else if self.eat_keyword("maxvalue") {
                opts.max_value = Some(self.parse_seq_int("MAXVALUE")?);
            } else if self.eat_keyword("cycle") {
                opts.cycle = Some(true);
            } else if self.eat_keyword("restart") {
                if self.eat_keyword("with") {
                    opts.restart = Some(self.parse_seq_int("RESTART")?);
                } else {
                    opts.restart = Some(SequenceOpts::RESTART_SENTINEL);
                }
            } else {
                break;
            }
        }
        Ok(opts)
    }

    fn parse_seq_int(&mut self, what: &str) -> Result<i64, SqlError> {
        let neg = matches!(self.peek(), Token::Minus);
        if neg {
            self.next();
        }
        match self.next() {
            Token::Number(raw) => {
                let v: i64 = raw
                    .parse()
                    .map_err(|_| err(format!("invalid {} value: {}", what, raw)))?;
                Ok(if neg { -v } else { v })
            }
            other => Err(err(format!(
                "syntax error: expected integer for {}, found {:?}",
                what, other
            ))),
        }
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
            Token::UStr(raw) => {
                // v0.19: U&'...' with optional UESCAPE 'c' clause.
                let escape = if self.eat_keyword("uescape") {
                    match self.next() {
                        Token::Str(e) => {
                            let mut ch = e.chars();
                            match (ch.next(), ch.next()) {
                                (Some(c), None) => c,
                                _ => {
                                    return Err(err(
                                        "UESCAPE string must be empty or one character",
                                    ));
                                }
                            }
                        }
                        other => {
                            return Err(err(format!(
                                "syntax error: expected string literal for UESCAPE, found {:?}",
                                other
                            )));
                        }
                    }
                } else {
                    '\\'
                };
                match decode_ustr(&raw, escape) {
                    Some(t) => Ok(Literal::Text(t)),
                    None => Err(err("invalid Unicode escape")),
                }
            }
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
        // v0.10: `INSERT INTO ... SELECT ...` (or VALUES).
        let (rows, select) = if self.eat_keyword("select") {
            let sel = self.parse_select_rest()?;
            (Vec::new(), Some(sel))
        } else {
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
            (rows, None)
        };
        // v0.10: `ON CONFLICT ...`.
        let on_conflict = self.parse_on_conflict()?;
        // v0.10: `RETURNING ...`.
        let returning = self.parse_returning()?;
        Ok(Stmt::Insert {
            table,
            columns,
            rows,
            select,
            on_conflict,
            returning,
            with: Vec::new(),
        })
    }

    /// v0.10: `RETURNING * | expr [, ...]` after INSERT/UPDATE/DELETE.
    /// Empty when the keyword is absent.
    fn parse_returning(&mut self) -> Result<Vec<SelectItem>, SqlError> {
        if !self.eat_keyword("returning") {
            return Ok(Vec::new());
        }
        let mut items = Vec::new();
        loop {
            if self.peek() == Token::Star {
                self.next();
                items.push(SelectItem::All);
            } else if let Token::Ident(q) = self.peek() {
                if self.peek2() == Token::Dot && self.peek3() == Token::Star {
                    self.next();
                    self.next();
                    self.next();
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
            return Err(err("syntax error: RETURNING requires a select list"));
        }
        Ok(items)
    }

    /// v0.10: `ON CONFLICT [ ( cols ) | ON CONSTRAINT name ] DO NOTHING |
    /// DO UPDATE SET ... [WHERE ...]`. Returns None when absent.
    fn parse_on_conflict(&mut self) -> Result<Option<OnConflict>, SqlError> {
        if !self.eat_keyword("on") {
            return Ok(None);
        }
        self.expect_keyword("conflict")?;
        let arbiter = if self.eat_keyword("on") {
            self.expect_keyword("constraint")?;
            ConflictArbiter::Constraint(self.expect_ident()?)
        } else if self.peek() == Token::LParen {
            self.next();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            self.expect(Token::RParen, "')'")?;
            if cols.is_empty() {
                return Err(err(
                    "syntax error: ON CONFLICT arbiter requires at least one column".to_string(),
                ));
            }
            ConflictArbiter::Columns(cols)
        } else {
            ConflictArbiter::None
        };
        // Optional index_predicate (`WHERE ...`) on the arbiter is not
        // supported in v0.10.
        if self.eat_keyword("where") {
            return Err(SqlError {
                message: "ON CONFLICT with a WHERE arbiter predicate is not supported yet"
                    .to_string(),
                code: "0A000",
            });
        }
        let action = if self.eat_keyword("do") {
            if self.eat_keyword("nothing") {
                ConflictAction::DoNothing
            } else if self.eat_keyword("update") {
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
                    return Err(err(
                        "syntax error: ON CONFLICT DO UPDATE requires at least one assignment"
                            .to_string(),
                    ));
                }
                let where_ = if self.eat_keyword("where") {
                    Some(self.parse_or()?)
                } else {
                    None
                };
                ConflictAction::DoUpdate { sets, where_ }
            } else {
                return Err(err(format!(
                    "syntax error: expected NOTHING or UPDATE after ON CONFLICT DO, found {:?}",
                    self.peek()
                )));
            }
        } else {
            return Err(err(format!(
                "syntax error: expected DO after ON CONFLICT, found {:?}",
                self.peek()
            )));
        };
        Ok(Some(OnConflict { arbiter, action }))
    }

    /// v0.10: `WITH [RECURSIVE] name [(cols)] AS (select) [, ...]` followed
    /// by SELECT / INSERT / UPDATE / DELETE.
    fn parse_with(&mut self) -> Result<Stmt, SqlError> {
        let recursive = self.eat_keyword("recursive");
        let mut ctes = Vec::new();
        loop {
            let name = self.expect_ident()?;
            let col_aliases = if self.peek() == Token::LParen {
                self.next();
                let mut aliases = Vec::new();
                loop {
                    aliases.push(self.expect_ident()?);
                    if self.peek() == Token::Comma {
                        self.next();
                        continue;
                    }
                    break;
                }
                self.expect(Token::RParen, "')'")?;
                aliases
            } else {
                Vec::new()
            };
            self.expect_keyword("as")?;
            self.expect(Token::LParen, "'('")?;
            let body = self.parse_cte_body(recursive)?;
            self.expect(Token::RParen, "')'")?;
            if ctes.iter().any(|c: &CteDef| c.name == name) {
                return Err(err(format!("duplicate CTE name \"{}\"", name)));
            }
            ctes.push(CteDef {
                name,
                col_aliases,
                body,
                recursive,
            });
            if self.peek() == Token::Comma {
                self.next();
                continue;
            }
            break;
        }
        if ctes.is_empty() {
            return Err(err(
                "syntax error: WITH requires at least one CTE".to_string()
            ));
        }
        let kw = match self.next() {
            Token::Ident(kw) => kw,
            other => {
                return Err(err(format!(
                    "syntax error: expected SELECT, INSERT, UPDATE or DELETE after WITH, found {:?}",
                    other
                )));
            }
        };
        match kw.as_str() {
            "select" => {
                let mut sel = self.parse_select_rest()?;
                sel.with = ctes;
                Ok(Stmt::Select(sel))
            }
            "insert" => {
                let mut stmt = self.parse_insert()?;
                match &mut stmt {
                    Stmt::Insert { with, .. } => *with = ctes,
                    _ => unreachable!(),
                }
                Ok(stmt)
            }
            "update" => {
                let mut stmt = self.parse_update()?;
                match &mut stmt {
                    Stmt::Update { with, .. } => *with = ctes,
                    _ => unreachable!(),
                }
                Ok(stmt)
            }
            "delete" => {
                let mut stmt = self.parse_delete()?;
                match &mut stmt {
                    Stmt::Delete { with, .. } => *with = ctes,
                    _ => unreachable!(),
                }
                Ok(stmt)
            }
            _ => Err(err(format!(
                "syntax error: expected SELECT, INSERT, UPDATE or DELETE after WITH, found \"{}\"",
                kw
            ))),
        }
    }

    /// v0.10: one CTE body. A recursive CTE may be
    /// `non_recursive UNION [ALL] recursive`.
    fn parse_cte_body(&mut self, recursive: bool) -> Result<CteBody, SqlError> {
        // The body must start with SELECT.
        match self.next() {
            Token::Ident(kw) if kw == "select" => {}
            other => {
                return Err(err(format!(
                    "syntax error: expected SELECT in CTE body, found {:?}",
                    other
                )));
            }
        }
        let first = self.parse_select_rest()?;
        if self.eat_keyword("union") {
            if !recursive {
                return Err(SqlError {
                    message: "UNION in a CTE body requires WITH RECURSIVE".to_string(),
                    code: "0A000",
                });
            }
            let all = if self.eat_keyword("all") {
                true
            } else {
                self.eat_keyword("distinct");
                false
            };
            match self.next() {
                Token::Ident(kw) if kw == "select" => {}
                other => {
                    return Err(err(format!(
                        "syntax error: expected SELECT after UNION in recursive CTE, found {:?}",
                        other
                    )));
                }
            }
            let second = self.parse_select_rest()?;
            // Both sides must produce the same number of columns (when
            // statically known).
            match (cte_width(&first), cte_width(&second)) {
                (Some(n1), Some(n2)) if n1 != n2 => {
                    return Err(err(format!(
                        "recursive CTE: non-recursive and recursive terms have different column counts ({} vs {})",
                        n1, n2
                    )));
                }
                _ => {}
            }
            Ok(CteBody::Union {
                left: Box::new(first),
                right: Box::new(second),
                all,
            })
        } else {
            Ok(CteBody::Simple(first))
        }
    }

    /// v0.10: `COPY table [(cols)] FROM STDIN | TO STDOUT [WITH (...)]`.
    fn parse_copy(&mut self) -> Result<Stmt, SqlError> {
        let table = self.expect_ident()?;
        let columns = if self.peek() == Token::LParen {
            self.next();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            self.expect(Token::RParen, "')'")?;
            Some(cols)
        } else {
            None
        };
        let to_stdout = if self.eat_keyword("from") {
            // `FROM STDIN` (a filename is not supported).
            if self.eat_keyword("stdin") {
                false
            } else {
                return Err(SqlError {
                    message: "COPY FROM a file is not supported; use COPY FROM STDIN".to_string(),
                    code: "0A000",
                });
            }
        } else if self.eat_keyword("to") {
            if self.eat_keyword("stdout") {
                true
            } else {
                return Err(SqlError {
                    message: "COPY TO a file is not supported; use COPY TO STDOUT".to_string(),
                    code: "0A000",
                });
            }
        } else {
            return Err(err(format!(
                "syntax error: expected FROM or TO after COPY table, found {:?}",
                self.peek()
            )));
        };
        let mut options = CopyOptions::default();
        let mut delimiter_set = false;
        let mut null_set = false;
        if self.eat_keyword("with") {
            self.expect(Token::LParen, "'('")?;
            loop {
                let opt = self.expect_ident()?;
                match opt.as_str() {
                    "format" => {
                        let fmt = self.expect_ident()?;
                        options.format = match fmt.as_str() {
                            "text" => CopyFormat::Text,
                            "csv" => CopyFormat::Csv,
                            "binary" => {
                                return Err(SqlError {
                                    message: "COPY FORMAT BINARY is not supported".to_string(),
                                    code: "0A000",
                                });
                            }
                            _ => {
                                return Err(err(format!(
                                    "syntax error: unknown COPY format \"{}\"",
                                    fmt
                                )));
                            }
                        };
                    }
                    "delimiter" => {
                        options.delimiter = self.parse_copy_char("DELIMITER")?;
                        delimiter_set = true;
                    }
                    "null" => {
                        options.null = self.parse_copy_string("NULL")?;
                        null_set = true;
                    }
                    "header" => {
                        // `HEADER` or `HEADER true|false`.
                        options.header = match self.peek() {
                            Token::Ident(v) if v == "true" => {
                                self.next();
                                true
                            }
                            Token::Ident(v) if v == "false" => {
                                self.next();
                                false
                            }
                            _ => true,
                        };
                    }
                    "quote" => {
                        options.quote = self.parse_copy_char("QUOTE")?;
                    }
                    "escape" => {
                        options.escape = self.parse_copy_char("ESCAPE")?;
                    }
                    _ => {
                        return Err(err(format!(
                            "syntax error: unknown COPY option \"{}\"",
                            opt
                        )));
                    }
                }
                if self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
            self.expect(Token::RParen, "')'")?;
        }
        // v0.10: CSV defaults (like Postgres): comma delimiter and
        // empty-string NULL unless explicitly set.
        if options.format == CopyFormat::Csv {
            if !delimiter_set {
                options.delimiter = b',';
            }
            if !null_set {
                options.null = String::new();
            }
        }
        Ok(Stmt::Copy {
            table,
            columns,
            to_stdout,
            options,
        })
    }

    /// v0.10: one single-character COPY option value: `'c'` or `E'c'`.
    fn parse_copy_char(&mut self, what: &str) -> Result<u8, SqlError> {
        let s = self.parse_copy_string(what)?;
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                if encoded.len() == 1 {
                    Ok(encoded.as_bytes()[0])
                } else {
                    Err(err(format!(
                        "syntax error: COPY {} must be a single ASCII character",
                        what
                    )))
                }
            }
            _ => Err(err(format!(
                "syntax error: COPY {} must be a single character",
                what
            ))),
        }
    }

    /// v0.10: a string literal for a COPY option (plain or E''-escaped).
    fn parse_copy_string(&mut self, what: &str) -> Result<String, SqlError> {
        match self.next() {
            Token::Str(s) => Ok(s),
            other => Err(err(format!(
                "syntax error: expected string literal for COPY {}, found {:?}",
                what, other
            ))),
        }
    }

    /// One INSERT value: a literal (with optional unary `+`/`-`), a
    /// `$N` parameter placeholder, or the DEFAULT keyword (v0.9).
    fn parse_insert_value(&mut self) -> Result<InsertValue, SqlError> {
        if self.eat_keyword("default") {
            return Ok(InsertValue::Default);
        }
        // v0.14: typed literals in VALUES, e.g. `bool 't'`, `date '2026-01-01'`
        // (pg_regress conformance). The text is stored as-is; the column
        // coercion applies the target type's input function, matching
        // PostgreSQL assignment semantics (including 22P02 on bad input).
        if let Token::Ident(name) = self.peek() {
            if Self::is_type_start(&name) {
                let save = self.pos;
                self.next(); // consume the type name
                if self.parse_type_name_rest(name).is_ok() {
                    if let Token::Str(s) = self.next() {
                        return Ok(InsertValue::Lit(Literal::Text(s)));
                    }
                }
                self.pos = save;
            }
        }
        let value = match self.peek() {
            Token::Param(n) => {
                self.next();
                InsertValue::Param(n)
            }
            Token::Minus | Token::Plus => {
                let neg = self.peek() == Token::Minus;
                self.next();
                let lit = self.parse_literal()?;
                InsertValue::Lit(match (neg, lit) {
                    (true, Literal::Int(i)) => Literal::Int(-i),
                    (true, Literal::BigInt(i)) => Literal::BigInt(-i),
                    // `-9223372036854775808`: the digits alone overflow
                    // i64; recover the exact i64::MIN.
                    (true, Literal::Decimal(s)) if s == "9223372036854775808" => {
                        Literal::BigInt(i64::MIN)
                    }
                    (true, Literal::Decimal(s)) => Literal::Decimal(format!("-{}", s)),
                    (true, Literal::Float(f)) => Literal::Float(-f),
                    (_, l) => l,
                })
            }
            _ => InsertValue::Lit(self.parse_literal()?),
        };
        // v0.14: `::type` cast suffix on a VALUES literal, e.g.
        // `1::int`, `'x'::text` (pg_regress conformance). The type name
        // is validated; the literal itself flows through normal column
        // assignment coercion (the casts in the conformance tests are
        // no-ops for their target columns).
        if self.peek() == Token::ColonColon {
            self.next();
            let _ = self.parse_type_name()?;
        }
        Ok(value)
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
            let escape = if self.eat_keyword("escape") {
                Some(Box::new(self.parse_concat()?))
            } else {
                None
            };
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                not: neg,
                ilike: false,
                escape,
            });
        }
        // v0.19: [NOT] SIMILAR TO pattern [ESCAPE 'c'].
        // Desugars to similar_to(expr, pattern [, escape]).
        if self.eat_keyword("similar") {
            self.expect_keyword("to")?;
            let pattern = self.parse_concat()?;
            let mut args = vec![left, pattern];
            if self.eat_keyword("escape") {
                args.push(self.parse_concat()?);
            }
            let e = Expr::Func {
                name: "similar_to".to_string(),
                args,
            };
            if neg {
                return Ok(Expr::Not(Box::new(e)));
            } else {
                return Ok(e);
            }
        }
        if self.eat_keyword("ilike") {
            let pattern = self.parse_concat()?;
            let escape = if self.eat_keyword("escape") {
                Some(Box::new(self.parse_concat()?))
            } else {
                None
            };
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                not: neg,
                ilike: true,
                escape,
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
            Token::Number(_) | Token::Str(_) | Token::UStr(_) => {
                // v0.19: adjacent string literals concatenate
                // ('a' 'b' -> 'ab', per SQL standard).
                let mut lit = self.parse_literal()?;
                loop {
                    let is_str = matches!(self.peek(), Token::Str(_) | Token::UStr(_));
                    if !is_str {
                        break;
                    }
                    // Only concatenate text literals.
                    let next = self.parse_literal()?;
                    match (&mut lit, next) {
                        (Literal::Text(a), Literal::Text(b)) => a.push_str(&b),
                        _ => {
                            return Err(err(
                                "syntax error: adjacent literals must both be strings",
                            ));
                        }
                    }
                }
                Ok(Expr::Literal(lit))
            }
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
        // v0.14: function-style cast — PostgreSQL treats `typename(expr)`
        // as a cast when the name is a type and there is exactly one
        // argument (e.g. `float8(count(*))`).
        if Self::is_type_start(&name) {
            let save = self.pos;
            if let Ok(to) = self.parse_type_name_rest(name.clone()) {
                if self.peek() == Token::LParen {
                    self.next();
                    if let Ok(expr) = self.parse_or() {
                        if self.peek() == Token::RParen {
                            self.next();
                            return Ok(Expr::Cast {
                                expr: Box::new(expr),
                                to,
                            });
                        }
                    }
                }
            }
            self.pos = save;
        }
        match name.as_str() {
            "extract" => return self.parse_extract(),
            "trim" => return self.parse_trim(),
            "position" => return self.parse_position(),
            "substring" => return self.parse_substring(),
            "overlay" => return self.parse_overlay(),
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
            let mut args = Vec::new();
            if let Some(a) = arg {
                args.push(*a);
            }
            if let Some(a) = arg2 {
                args.push(*a);
            }
            let mut expr = Expr::Agg {
                func: func.clone(),
                arg: args.first().cloned().map(Box::new),
                distinct,
                arg2: args.get(1).cloned().map(Box::new),
            };
            // v0.10: `<agg>(...) OVER (...)` — windowed aggregate.
            if self.eat_keyword("over") {
                let spec = self.parse_window_spec()?;
                expr = Expr::Window {
                    func: WindowFunc::Agg(func),
                    args,
                    distinct,
                    partition_by: spec.partition_by,
                    order_by: spec.order_by,
                    frame: spec.frame,
                    wid: 0,
                };
            }
            return Ok(expr);
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
            // v0.10: `<func>(...) OVER (...)` — window function.
            if self.eat_keyword("over") {
                let func = match name.as_str() {
                    "row_number" => WindowFunc::RowNumber,
                    "rank" => WindowFunc::Rank,
                    "dense_rank" => WindowFunc::DenseRank,
                    "ntile" => WindowFunc::Ntile,
                    "lag" => WindowFunc::Lag,
                    "lead" => WindowFunc::Lead,
                    "first_value" => WindowFunc::FirstValue,
                    "last_value" => WindowFunc::LastValue,
                    "nth_value" => WindowFunc::NthValue,
                    _ => {
                        return Err(SqlError {
                            message: format!(
                                "OVER specified, but {} is not a window function",
                                name
                            ),
                            code: "42883",
                        });
                    }
                };
                let spec = self.parse_window_spec()?;
                return Ok(Expr::Window {
                    func,
                    args,
                    distinct: false,
                    partition_by: spec.partition_by,
                    order_by: spec.order_by,
                    frame: spec.frame,
                    wid: 0,
                });
            }
            return Ok(Expr::Func { name, args });
        }
        Err(err(format!(
            "syntax error: expected '(', found {:?}",
            self.peek()
        )))
    }

    /// v0.10: the parenthesized part of `OVER (...)`: optional PARTITION BY,
    /// optional ORDER BY, optional frame clause.
    fn parse_window_spec(&mut self) -> Result<WindowSpec, SqlError> {
        self.expect(Token::LParen, "'('")?;
        let mut partition_by = Vec::new();
        if self.eat_keyword("partition") {
            self.expect_keyword("by")?;
            loop {
                partition_by.push(self.parse_or()?);
                if self.peek() == Token::Comma {
                    self.next();
                    continue;
                }
                break;
            }
        }
        let mut order_by = Vec::new();
        if self.eat_keyword("order") {
            self.expect_keyword("by")?;
            loop {
                let expr = self.parse_or()?;
                let desc = if self.eat_keyword("desc") {
                    true
                } else {
                    self.eat_keyword("asc");
                    false
                };
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
                order_by.push(OrderTerm {
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
        }
        let frame = self.parse_window_frame()?;
        self.expect(Token::RParen, "')'")?;
        Ok(WindowSpec {
            partition_by,
            order_by,
            frame,
        })
    }

    /// v0.10: `[ROWS|RANGE] BETWEEN <bound> AND <bound>`,
    /// `[ROWS|RANGE] <bound>`, or absent (Default).
    fn parse_window_frame(&mut self) -> Result<WindowFrame, SqlError> {
        let mode = if self.eat_keyword("rows") {
            Some(false)
        } else if self.eat_keyword("range") {
            Some(true)
        } else {
            None
        };
        let mode = match mode {
            Some(m) => m,
            None => return Ok(WindowFrame::Default),
        };
        let (start, end) = if self.eat_keyword("between") {
            let s = self.parse_frame_bound()?;
            self.expect_keyword("and")?;
            let e = self.parse_frame_bound()?;
            (s, e)
        } else {
            // `<bound>` alone = `BETWEEN <bound> AND CURRENT ROW`.
            (self.parse_frame_bound()?, FrameBound::CurrentRow)
        };
        // Frame sanity: start must not come after end.
        let rank = |b: &FrameBound| match b {
            FrameBound::UnboundedPreceding => 0,
            FrameBound::Preceding(_) => 1,
            FrameBound::CurrentRow => 2,
            FrameBound::Following(_) => 3,
            FrameBound::UnboundedFollowing => 4,
        };
        if rank(&start) > rank(&end) {
            return Err(err(
                "syntax error: frame starting from following row cannot end with current row"
                    .to_string(),
            ));
        }
        if mode {
            Ok(WindowFrame::Range { start, end })
        } else {
            Ok(WindowFrame::Rows { start, end })
        }
    }

    /// v0.10: one frame bound: `UNBOUNDED PRECEDING|FOLLOWING`,
    /// `CURRENT ROW`, or `<n> PRECEDING|FOLLOWING`.
    fn parse_frame_bound(&mut self) -> Result<FrameBound, SqlError> {
        if self.eat_keyword("unbounded") {
            if self.eat_keyword("preceding") {
                return Ok(FrameBound::UnboundedPreceding);
            }
            if self.eat_keyword("following") {
                return Ok(FrameBound::UnboundedFollowing);
            }
            return Err(err(format!(
                "syntax error: expected PRECEDING or FOLLOWING after UNBOUNDED, found {:?}",
                self.peek()
            )));
        }
        if self.eat_keyword("current") {
            self.expect_keyword("row")?;
            return Ok(FrameBound::CurrentRow);
        }
        match self.next() {
            Token::Number(n) => {
                let n: u64 = n
                    .parse()
                    .map_err(|_| err(format!("syntax error: bad frame offset \"{}\"", n)))?;
                if self.eat_keyword("preceding") {
                    Ok(FrameBound::Preceding(n))
                } else if self.eat_keyword("following") {
                    Ok(FrameBound::Following(n))
                } else {
                    Err(err(format!(
                        "syntax error: expected PRECEDING or FOLLOWING after frame offset, found {:?}",
                        self.peek()
                    )))
                }
            }
            other => Err(err(format!(
                "syntax error: expected frame bound, found {:?}",
                other
            ))),
        }
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
                args: vec![Expr::Literal(Literal::Text(spec.to_string())), first, s],
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

    /// `overlay(str placing replacement from start [for len])`.
    /// Desugars to `overlay(str, replacement, start, len)`; omitted FOR
    /// defaults to the replacement length (Postgres behavior).
    fn parse_overlay(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LParen, "'('")?;
        let s = self.parse_or()?;
        let (replacement, start, len) = if self.eat_keyword("placing") {
            let r = self.parse_or()?;
            self.expect_keyword("from")?;
            let st = self.parse_or()?;
            let ln = if self.eat_keyword("for") {
                Some(self.parse_or()?)
            } else {
                None
            };
            (r, st, ln)
        } else {
            let r = self.parse_or()?;
            self.expect(Token::Comma, "','")?;
            let st = self.parse_or()?;
            let ln = if self.peek() == Token::Comma {
                self.next();
                Some(self.parse_or()?)
            } else {
                None
            };
            (r, st, ln)
        };
        self.expect(Token::RParen, "')'")?;
        // Default FOR = length(replacement); encode as a Func call.
        let mut args = vec![s, replacement, start];
        if let Some(l) = len {
            args.push(l);
        } else {
            // Use a sentinel: overlay() with 3 args means "default length".
            // We handle this in eval by computing length(replacement).
        }
        Ok(Expr::Func {
            name: "overlay".to_string(),
            args,
        })
    }

    /// `substring(str from start [for len])`, `substring(str, start [, len])`,
    /// `substring(str from pattern)` (POSIX regex), `substring(str from pattern
    /// for escape)` (SQL99 SIMILAR), or `substring(str similar pattern
    /// [escape 'c'])` (SIMILAR).
    fn parse_substring(&mut self) -> Result<Expr, SqlError> {
        self.expect(Token::LParen, "'('")?;
        let s = self.parse_or()?;
        // v0.19: SUBSTRING(s SIMILAR pat [ESCAPE 'c']).
        if self.eat_keyword("similar") {
            let pat = self.parse_or()?;
            let mut args = vec![s, pat];
            if self.eat_keyword("escape") {
                args.push(self.parse_or()?);
            }
            self.expect(Token::RParen, "')'")?;
            return Ok(Expr::Func {
                name: "substring_similar".to_string(),
                args,
            });
        }
        if self.eat_keyword("from") {
            let pat_or_start = self.parse_or()?;
            if self.eat_keyword("for") {
                let for_arg = self.parse_or()?;
                self.expect(Token::RParen, "')'")?;
                // Distinguish integer (start, len) from text (pattern, escape).
                // If both are integer literals, it's substring(s, start, len).
                // Otherwise, it's the SQL99 SIMILAR form.
                let is_int = |e: &Expr| matches!(e, Expr::Literal(crate::sql::Literal::Int(_)));
                if is_int(&pat_or_start) && is_int(&for_arg) {
                    return Ok(Expr::Func {
                        name: "substring".to_string(),
                        args: vec![s, pat_or_start, for_arg],
                    });
                } else {
                    return Ok(Expr::Func {
                        name: "substring_similar".to_string(),
                        args: vec![s, pat_or_start, for_arg],
                    });
                }
            }
            self.expect(Token::RParen, "')'")?;
            // SUBSTRING(s FROM pat): could be POSIX regex or (start) integer.
            // We dispatch based on the type at runtime: if the second arg is
            // text, treat as regex; if integer, treat as start position.
            // For now, use a special function name and let eval decide.
            return Ok(Expr::Func {
                name: "substring_from".to_string(),
                args: vec![s, pat_or_start],
            });
        }
        // Comma form: substring(s, start [, len]).
        self.expect(Token::Comma, "','")?;
        let start = self.parse_or()?;
        let mut args = vec![s, start];
        if self.peek() == Token::Comma {
            self.next();
            args.push(self.parse_or()?);
        }
        self.expect(Token::RParen, "')'")?;
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
        let (level, read_only, deferrable) = self.parse_txn_modes()?;
        Ok(Stmt::Begin {
            level,
            read_only,
            deferrable,
        })
    }

    /// v0.17: shared `transaction_mode [, ...]` list used by BEGIN,
    /// START TRANSACTION, SET TRANSACTION, and SET SESSION
    /// CHARACTERISTICS. Returns (level, read_only, deferrable).
    fn parse_txn_modes(
        &mut self,
    ) -> Result<(Option<IsolationLevel>, Option<bool>, Option<bool>), SqlError> {
        // transaction_mode :=
        //     ISOLATION LEVEL { SERIALIZABLE | REPEATABLE READ | READ COMMITTED | READ UNCOMMITTED }
        //   | READ WRITE | READ ONLY
        //   | [NOT] DEFERRABLE
        let mut level: Option<IsolationLevel> = None;
        let mut read_only: Option<bool> = None;
        let mut deferrable: Option<bool> = None;
        loop {
            if self.eat_keyword("isolation") {
                self.expect_keyword("level")?;
                if level.is_some() {
                    return Err(err(
                        "syntax error: multiple ISOLATION LEVEL clauses".to_string()
                    ));
                }
                level = Some(self.parse_isolation_level()?);
            } else if self.eat_keyword("read") {
                let w = self.expect_ident()?;
                match w.as_str() {
                    "write" => read_only = Some(false),
                    "only" => read_only = Some(true),
                    _ => {
                        return Err(err(format!(
                            "syntax error: expected WRITE or ONLY, found {:?}",
                            w
                        )));
                    }
                }
            } else if self.eat_keyword("not") {
                self.expect_keyword("deferrable")?;
                deferrable = Some(false);
            } else if self.eat_keyword("deferrable") {
                deferrable = Some(true);
            } else {
                break;
            }
            // Modes are comma-separated; a missing comma ends the list.
            if self.peek() == Token::Comma {
                self.next();
            } else {
                break;
            }
        }
        Ok((level, read_only, deferrable))
    }

    /// v0.17: `SET TRANSACTION ...`, `SET SESSION CHARACTERISTICS ...`,
    /// or `SET name = value` / `SET name TO value`.
    fn parse_set(&mut self) -> Result<Stmt, SqlError> {
        if self.eat_keyword("transaction") {
            // SET TRANSACTION transaction_mode [, ...]
            let (level, read_only, deferrable) = self.parse_txn_modes()?;
            return Ok(Stmt::SetTransaction {
                level,
                read_only,
                deferrable,
            });
        }
        if self.eat_keyword("session") {
            // SET SESSION CHARACTERISTICS AS TRANSACTION mode [, ...]
            self.expect_keyword("characteristics")?;
            self.expect_keyword("as")?;
            self.expect_keyword("transaction")?;
            let (level, read_only, deferrable) = self.parse_txn_modes()?;
            return Ok(Stmt::SetSessionCharacteristics {
                level,
                read_only,
                deferrable,
            });
        }
        // SET name = value | SET name TO value | SET name TO DEFAULT
        let name = self.expect_ident()?;
        if self.eat_keyword("to") {
            // consumed TO
        } else if self.peek() == Token::Eq {
            self.next();
        } else {
            return Err(err(format!(
                "syntax error: expected TO or '=' after SET {}, found {:?}",
                name,
                self.peek()
            )));
        }
        let value = match self.next() {
            Token::Str(s) => SetValue::Str(s),
            Token::Number(num) => {
                self.next();
                SetValue::Str(num)
            }
            Token::Ident(kw) => {
                // A bare keyword value (ON, OFF, TRUE, ...): fold to the
                // lowercased keyword text. DEFAULT resets the parameter.
                if kw == "default" {
                    SetValue::Default
                } else {
                    SetValue::Str(kw.to_ascii_lowercase())
                }
            }
            other => {
                return Err(err(format!(
                    "syntax error: unexpected SET value {:?}",
                    other
                )));
            }
        };
        Ok(Stmt::Set { name, value })
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
        let returning = self.parse_returning()?;
        Ok(Stmt::Update {
            table,
            sets,
            where_,
            returning,
            with: Vec::new(),
        })
    }

    fn parse_delete(&mut self) -> Result<Stmt, SqlError> {
        self.expect_keyword("from")?;
        let table = self.expect_ident()?;
        let where_ = self.parse_where_opt()?;
        let returning = self.parse_returning()?;
        Ok(Stmt::Delete {
            table,
            where_,
            returning,
            with: Vec::new(),
        })
    }

    // --- v0.16: TRUNCATE / SQL cursors -----------------------------------
    fn parse_truncate(&mut self) -> Result<Stmt, SqlError> {
        // TRUNCATE [ TABLE ] name [, ...]
        //   [ RESTART IDENTITY | CONTINUE IDENTITY ] [ CASCADE | RESTRICT ]
        let _ = self.eat_keyword("table");
        let mut tables = vec![self.expect_ident()?];
        while self.peek() == Token::Comma {
            self.next();
            tables.push(self.expect_ident()?);
        }
        let restart_identity = if self.eat_keyword("restart") {
            self.expect_keyword("identity")?;
            true
        } else {
            let _ = self.eat_keyword("continue");
            let _ = self.eat_keyword("identity");
            false
        };
        let cascade = if self.eat_keyword("cascade") {
            true
        } else {
            let _ = self.eat_keyword("restrict");
            false
        };
        Ok(Stmt::Truncate {
            tables,
            restart_identity,
            cascade,
        })
    }

    fn parse_declare(&mut self) -> Result<Stmt, SqlError> {
        // DECLARE name [BINARY] [INSENSITIVE] [SCROLL | NO SCROLL]
        //   CURSOR [WITH HOLD | WITHOUT HOLD] FOR select
        let name = self.expect_ident()?;
        let _ = self.eat_keyword("binary");
        let _ = self.eat_keyword("insensitive");
        if self.eat_keyword("no") {
            self.expect_keyword("scroll")?;
        } else {
            let _ = self.eat_keyword("scroll");
        }
        self.expect_keyword("cursor")?;
        let with_hold = if self.eat_keyword("with") {
            self.expect_keyword("hold")?;
            true
        } else if self.eat_keyword("without") {
            self.expect_keyword("hold")?;
            false
        } else {
            false
        };
        self.expect_keyword("for")?;
        // `parse_select_rest` expects the SELECT keyword already
        // consumed (like `parse_top` does for a top-level SELECT).
        self.expect_keyword("select")?;
        let query = self.parse_select_rest()?;
        Ok(Stmt::Declare {
            name,
            query,
            with_hold,
        })
    }

    /// An optional `-` followed by an integer literal, for FETCH counts.
    fn parse_fetch_count(&mut self) -> Result<i64, SqlError> {
        let neg = self.peek() == Token::Minus && {
            self.next();
            true
        };
        match self.next() {
            Token::Number(s) => {
                let n: i64 = s
                    .parse()
                    .map_err(|_| err(format!("syntax error: invalid FETCH count \"{}\"", s)))?;
                Ok(if neg { -n } else { n })
            }
            other => Err(err(format!(
                "syntax error: expected FETCH count, found {:?}",
                other
            ))),
        }
    }

    fn try_parse_fetch_count(&mut self) -> Option<i64> {
        // Peek for [Minus] Number without consuming on mismatch.
        let save = self.pos;
        if self.peek() == Token::Minus {
            self.next();
        }
        let is_num = matches!(self.peek(), Token::Number(_));
        self.pos = save;
        if !is_num {
            return None;
        }
        self.parse_fetch_count().ok()
    }

    /// FETCH / MOVE direction. With no direction word this is NEXT (one
    /// row), like Postgres.
    fn parse_fetch_dir(&mut self) -> Result<FetchDir, SqlError> {
        if self.eat_keyword("next") {
            return Ok(FetchDir::Forward(Some(1)));
        }
        if self.eat_keyword("prior") {
            return Ok(FetchDir::Backward(Some(1)));
        }
        if self.eat_keyword("first") {
            return Ok(FetchDir::First);
        }
        if self.eat_keyword("last") {
            return Ok(FetchDir::Last);
        }
        if self.eat_keyword("absolute") {
            return Ok(FetchDir::Absolute(self.parse_fetch_count()?));
        }
        if self.eat_keyword("relative") {
            return Ok(FetchDir::Relative(self.parse_fetch_count()?));
        }
        if self.eat_keyword("forward") {
            if self.eat_keyword("all") {
                return Ok(FetchDir::Forward(None));
            }
            if let Some(n) = self.try_parse_fetch_count() {
                return Ok(FetchDir::Forward(Some(n)));
            }
            return Ok(FetchDir::Forward(Some(1)));
        }
        if self.eat_keyword("backward") {
            if self.eat_keyword("all") {
                return Ok(FetchDir::Backward(None));
            }
            if let Some(n) = self.try_parse_fetch_count() {
                return Ok(FetchDir::Backward(Some(n)));
            }
            return Ok(FetchDir::Backward(Some(1)));
        }
        if self.eat_keyword("all") {
            return Ok(FetchDir::Forward(None));
        }
        if let Some(n) = self.try_parse_fetch_count() {
            return Ok(FetchDir::Forward(Some(n)));
        }
        Ok(FetchDir::Forward(Some(1)))
    }

    fn parse_fetch(&mut self) -> Result<Stmt, SqlError> {
        // FETCH [ direction ] { FROM | IN } name
        let dir = self.parse_fetch_dir()?;
        if !(self.eat_keyword("from") || self.eat_keyword("in")) {
            return Err(err(format!(
                "syntax error: expected FROM or IN in FETCH, found {:?}",
                self.peek()
            )));
        }
        let name = self.expect_ident()?;
        Ok(Stmt::Fetch { name, dir })
    }

    fn parse_move(&mut self) -> Result<Stmt, SqlError> {
        // MOVE [ direction ] { FROM | IN } name
        let dir = self.parse_fetch_dir()?;
        if !(self.eat_keyword("from") || self.eat_keyword("in")) {
            return Err(err(format!(
                "syntax error: expected FROM or IN in MOVE, found {:?}",
                self.peek()
            )));
        }
        let name = self.expect_ident()?;
        Ok(Stmt::Move { name, dir })
    }

    fn parse_close(&mut self) -> Result<Stmt, SqlError> {
        // CLOSE name | CLOSE ALL
        if self.eat_keyword("all") {
            return Ok(Stmt::Close { name: None });
        }
        let name = self.expect_ident()?;
        Ok(Stmt::Close { name: Some(name) })
    }

    fn parse_vacuum(&mut self) -> Result<Stmt, SqlError> {
        let verbose = self.eat_keyword("verbose");
        // v0.14: `VACUUM ANALYZE` (pg_regress conformance).
        let analyze = self.eat_keyword("analyze");
        let table = match self.peek() {
            Token::Ident(_) => Some(self.expect_ident()?),
            _ => None,
        };
        Ok(Stmt::Vacuum {
            table,
            verbose,
            analyze,
        })
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
            with: Vec::new(),
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
                using: Vec::new(),
                natural: false,
            };
        }
        Ok(vec![acc])
    }

    fn parse_join_chain(&mut self) -> Result<FromItem, SqlError> {
        let mut left = self.parse_from_primary()?;
        loop {
            // v0.20: NATURAL [kind] JOIN — desugared at execution time.
            let natural = self.eat_keyword("natural");
            let kind = if self.eat_keyword("join") {
                JoinKind::Inner
            } else if self.eat_keyword("inner") {
                self.expect_keyword("join")?;
                JoinKind::Inner
            } else if self.eat_keyword("left") {
                self.eat_keyword("outer");
                self.expect_keyword("join")?;
                JoinKind::Left
            } else if self.eat_keyword("right") {
                self.eat_keyword("outer");
                self.expect_keyword("join")?;
                JoinKind::Right
            } else if self.eat_keyword("full") {
                self.eat_keyword("outer");
                self.expect_keyword("join")?;
                JoinKind::Full
            } else if self.eat_keyword("cross") {
                self.expect_keyword("join")?;
                JoinKind::Cross
            } else {
                if natural {
                    return Err(err("NATURAL must be followed by a join type"));
                }
                break;
            };
            let right = self.parse_from_primary()?;
            let (on, using) = match kind {
                JoinKind::Cross => (None, Vec::new()),
                // v0.20: NATURAL has no ON/USING — condition is implicit.
                _ if natural => (None, Vec::new()),
                _ => {
                    if self.eat_keyword("using") {
                        self.expect(Token::LParen, "'('")?;
                        let mut cols = Vec::new();
                        loop {
                            cols.push(self.expect_ident()?);
                            if self.peek() == Token::Comma {
                                self.next();
                                continue;
                            }
                            break;
                        }
                        self.expect(Token::RParen, "')'")?;
                        (None, cols)
                    } else {
                        self.expect_keyword("on")?;
                        (Some(self.parse_or()?), Vec::new())
                    }
                }
            };
            left = FromItem::Join {
                left: Box::new(left),
                kind,
                right: Box::new(right),
                on,
                using,
                natural,
            };
        }
        Ok(left)
    }

    fn parse_from_primary(&mut self) -> Result<FromItem, SqlError> {
        if self.peek() == Token::LParen {
            self.next();
            // v0.14: PostgreSQL allows redundant parens: FROM ((SELECT ...)).
            // Only consume an extra '(' when it opens a subquery or VALUES —
            // never a VALUES row tuple like (1, 2).
            let mut extra = 0;
            while self.peek() == Token::LParen
                && matches!(self.peek2(), Token::Ident(ref s) if s == "select" || s == "values")
            {
                self.next();
                extra += 1;
            }
            let mut item = if matches!(self.peek(), Token::Ident(ref s) if s == "values") {
                self.next();
                let mut rows: Vec<Vec<Expr>> = Vec::new();
                loop {
                    self.expect(Token::LParen, "'('")?;
                    let mut row = Vec::new();
                    loop {
                        row.push(self.parse_or()?);
                        match self.next() {
                            Token::Comma => continue,
                            Token::RParen => break,
                            other => {
                                return Err(err(format!(
                                    "syntax error: expected ',' or ')' in VALUES row, found {:?}",
                                    other
                                )));
                            }
                        }
                    }
                    rows.push(row);
                    if self.peek() == Token::Comma {
                        self.next();
                    } else {
                        break;
                    }
                }
                if rows.is_empty() {
                    return Err(err(
                        "syntax error: VALUES requires at least one row".to_string()
                    ));
                }
                FromItem::Values {
                    rows,
                    alias: String::new(),
                }
            } else {
                let sub = self.parse_subquery()?;
                FromItem::Derived {
                    sub: Box::new(sub),
                    alias: String::new(),
                }
            };
            for _ in 0..=extra {
                self.expect(Token::RParen, "')'")?;
            }
            // The alias follows the closing parens: FROM ((SELECT 1 AS x)) ss.
            let alias = self.parse_derived_alias()?;
            match &mut item {
                FromItem::Values { alias: a, .. } | FromItem::Derived { alias: a, .. } => {
                    *a = alias;
                }
                _ => unreachable!("parse_from_primary paren branch"),
            }
            Ok(item)
        } else {
            let name = self.expect_ident()?;
            // v0.9: schema-qualified names, so the information_schema
            // catalog views are reachable (`FROM information_schema.tables`).
            let name = if self.peek() == Token::Dot {
                self.next();
                format!("{}.{}", name, self.expect_ident()?)
            } else {
                name
            };
            let alias = self.parse_alias_opt()?;
            // v0.20: `FROM tbl [AS] x (a, b, c)` — optional column aliases.
            let col_aliases = if self.peek() == Token::LParen {
                self.next();
                let mut cols = Vec::new();
                loop {
                    cols.push(self.expect_ident()?);
                    if self.peek() == Token::Comma {
                        self.next();
                        continue;
                    }
                    break;
                }
                self.expect(Token::RParen, "')'")?;
                cols
            } else {
                Vec::new()
            };
            Ok(FromItem::Table {
                name,
                alias,
                col_aliases,
            })
        }
    }

    /// Alias for a parenthesized FROM item. PostgreSQL auto-generates
    /// `unnamed_subquery[_N]` when no alias is given (v0.14).
    fn parse_derived_alias(&mut self) -> Result<String, SqlError> {
        if self.eat_keyword("as") {
            return self.expect_ident();
        }
        match self.peek() {
            Token::Ident(s) if !is_reserved(&s) => {
                self.next();
                Ok(s)
            }
            _ => {
                let n = self.unnamed_seq;
                self.unnamed_seq += 1;
                Ok(if n == 0 {
                    "unnamed_subquery".to_string()
                } else {
                    format!("unnamed_subquery_{}", n)
                })
            }
        }
    }

    fn parse_drop(&mut self) -> Result<Stmt, SqlError> {
        // v0.11: DROP ROLE / USER / GROUP
        if matches!(self.peek(), Token::Ident(ref s) if s == "role" || s == "user" || s == "group")
        {
            return self.parse_drop_role();
        }
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
        // v0.9: DROP VIEW [IF EXISTS] name [, ...] [CASCADE | RESTRICT]
        if self.eat_keyword("view") {
            let if_exists = if self.eat_keyword("if") {
                self.expect_keyword("exists")?;
                true
            } else {
                false
            };
            let mut names = Vec::new();
            loop {
                names.push(self.expect_ident()?);
                if !matches!(self.peek(), Token::Comma) {
                    break;
                }
                self.next();
            }
            let cascade = self.parse_cascade_opt()?;
            return Ok(Stmt::DropView {
                names,
                if_exists,
                cascade,
            });
        }
        // v0.9: DROP SEQUENCE [IF EXISTS] name [, ...] [CASCADE | RESTRICT]
        if self.eat_keyword("sequence") {
            let if_exists = if self.eat_keyword("if") {
                self.expect_keyword("exists")?;
                true
            } else {
                false
            };
            let mut names = Vec::new();
            loop {
                names.push(self.expect_ident()?);
                if !matches!(self.peek(), Token::Comma) {
                    break;
                }
                self.next();
            }
            // RESTRICT/CASCADE accepted but sequences have no dependents
            // tracked in v0.9 (documented).
            self.parse_cascade_opt()?;
            return Ok(Stmt::DropSequence { names, if_exists });
        }
        self.expect_keyword("table")?;
        let if_exists = if self.eat_keyword("if") {
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        // v0.9: DROP TABLE [IF EXISTS] name [, ...] [CASCADE | RESTRICT]
        let mut names = Vec::new();
        loop {
            names.push(self.expect_ident()?);
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        let cascade = self.parse_cascade_opt()?;
        Ok(Stmt::DropTable {
            if_exists,
            names,
            cascade,
        })
    }

    // ------------------------------------------------------------------
    // v0.11: roles and privileges
    // ------------------------------------------------------------------

    /// Consume ROLE | USER | GROUP and return the canonical kind string.
    fn parse_role_kind(&mut self) -> Result<&'static str, SqlError> {
        match self.next() {
            Token::Ident(s) if s == "role" => Ok("role"),
            Token::Ident(s) if s == "user" => Ok("user"),
            Token::Ident(s) if s == "group" => Ok("group"),
            other => Err(err(format!(
                "syntax error: expected ROLE, USER or GROUP, found {:?}",
                other
            ))),
        }
    }

    /// CREATE ROLE name [LOGIN|NOLOGIN] [SUPERUSER|NOSUPERUSER]
    ///   [PASSWORD 'pw' | PASSWORD NULL] [CONNECTION LIMIT n]
    /// CREATE USER = LOGIN; CREATE GROUP = NOLOGIN.
    fn parse_create_role(&mut self) -> Result<Stmt, SqlError> {
        let kind = self.parse_role_kind()?;
        let name = self.expect_ident()?;
        let mut login = kind == "user";
        let mut superuser = false;
        let mut password: Option<String> = None;
        let mut connlimit: Option<i32> = None;
        let mut valid_until: Option<String> = None;
        loop {
            if self.eat_keyword("login") {
                login = true;
            } else if self.eat_keyword("nologin") {
                login = false;
            } else if self.eat_keyword("superuser") {
                superuser = true;
            } else if self.eat_keyword("nosuperuser") {
                superuser = false;
            } else if self.eat_keyword("password") {
                match self.next() {
                    Token::Str(s) => password = Some(s),
                    Token::Ident(s) if s == "null" => password = None,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected password string or NULL, found {:?}",
                            other
                        )));
                    }
                }
            } else if self.eat_keyword("connection") {
                self.expect_keyword("limit")?;
                let neg = matches!(self.peek(), Token::Minus);
                if neg {
                    self.next();
                }
                let n = match self.next() {
                    Token::Number(s) => s
                        .parse::<i32>()
                        .map_err(|_| err("syntax error: bad CONNECTION LIMIT value".to_string()))?,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected number for CONNECTION LIMIT, found {:?}",
                            other
                        )));
                    }
                };
                connlimit = Some(if neg { -n } else { n });
            } else if self.eat_keyword("valid") {
                self.expect_keyword("until")?;
                match self.next() {
                    Token::Str(s) => valid_until = Some(s),
                    other => {
                        return Err(err(format!(
                            "syntax error: expected timestamp string for VALID UNTIL, found {:?}",
                            other
                        )));
                    }
                }
            } else {
                break;
            }
        }
        Ok(Stmt::CreateRole {
            name,
            login,
            superuser,
            password,
            connlimit,
            valid_until,
        })
    }

    /// ALTER ROLE name [LOGIN|NOLOGIN] [SUPERUSER|NOSUPERUSER]
    ///   [PASSWORD 'pw' | PASSWORD NULL] [CONNECTION LIMIT n]
    fn parse_alter_role(&mut self) -> Result<Stmt, SqlError> {
        let _kind = self.parse_role_kind()?;
        let name = self.expect_ident()?;
        let mut login: Option<bool> = None;
        let mut superuser: Option<bool> = None;
        let mut password: Option<Option<String>> = None;
        let mut connlimit: Option<i32> = None;
        let mut valid_until: Option<String> = None;
        loop {
            if self.eat_keyword("login") {
                login = Some(true);
            } else if self.eat_keyword("nologin") {
                login = Some(false);
            } else if self.eat_keyword("superuser") {
                superuser = Some(true);
            } else if self.eat_keyword("nosuperuser") {
                superuser = Some(false);
            } else if self.eat_keyword("password") {
                match self.next() {
                    Token::Str(s) => password = Some(Some(s)),
                    Token::Ident(s) if s == "null" => password = Some(None),
                    other => {
                        return Err(err(format!(
                            "syntax error: expected password string or NULL, found {:?}",
                            other
                        )));
                    }
                }
            } else if self.eat_keyword("connection") {
                self.expect_keyword("limit")?;
                let neg = matches!(self.peek(), Token::Minus);
                if neg {
                    self.next();
                }
                let n = match self.next() {
                    Token::Number(s) => s
                        .parse::<i32>()
                        .map_err(|_| err("syntax error: bad CONNECTION LIMIT value".to_string()))?,
                    other => {
                        return Err(err(format!(
                            "syntax error: expected number for CONNECTION LIMIT, found {:?}",
                            other
                        )));
                    }
                };
                connlimit = Some(if neg { -n } else { n });
            } else if self.eat_keyword("valid") {
                self.expect_keyword("until")?;
                match self.next() {
                    Token::Str(s) => valid_until = Some(s),
                    other => {
                        return Err(err(format!(
                            "syntax error: expected timestamp string for VALID UNTIL, found {:?}",
                            other
                        )));
                    }
                }
            } else {
                break;
            }
        }
        if login.is_none()
            && superuser.is_none()
            && password.is_none()
            && connlimit.is_none()
            && valid_until.is_none()
        {
            return Err(err(
                "syntax error: ALTER ROLE requires at least one option".to_string()
            ));
        }
        Ok(Stmt::AlterRole {
            name,
            login,
            superuser,
            password,
            connlimit,
            valid_until,
        })
    }

    /// DROP ROLE [IF EXISTS] name [, ...]
    fn parse_drop_role(&mut self) -> Result<Stmt, SqlError> {
        let _kind = self.parse_role_kind()?;
        let if_exists = if self.eat_keyword("if") {
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let mut names = Vec::new();
        loop {
            names.push(self.expect_ident()?);
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        Ok(Stmt::DropRole { names, if_exists })
    }

    /// Parse a comma-separated privilege list: SELECT, INSERT, UPDATE,
    /// DELETE, TRUNCATE, REFERENCES, TRIGGER, USAGE, CONNECT, ALL
    /// [PRIVILEGES].
    fn parse_privilege_list(&mut self) -> Result<Vec<PrivSpec>, SqlError> {
        let mut privs = Vec::new();
        loop {
            let p = if self.eat_keyword("select") {
                Privilege::Select
            } else if self.eat_keyword("insert") {
                Privilege::Insert
            } else if self.eat_keyword("update") {
                Privilege::Update
            } else if self.eat_keyword("delete") {
                Privilege::Delete
            } else if self.eat_keyword("truncate") {
                Privilege::Truncate
            } else if self.eat_keyword("references") {
                Privilege::References
            } else if self.eat_keyword("trigger") {
                Privilege::Trigger
            } else if self.eat_keyword("usage") {
                Privilege::Usage
            } else if self.eat_keyword("connect") {
                Privilege::Connect
            } else if self.eat_keyword("all") {
                self.eat_keyword("privileges");
                Privilege::All
            } else {
                return Err(err(format!(
                    "syntax error: expected privilege name, found {:?}",
                    self.peek()
                )));
            };
            // v0.11: optional column list, e.g. SELECT (a, b).
            let mut columns = Vec::new();
            if matches!(self.peek(), Token::LParen) {
                match p {
                    Privilege::Select
                    | Privilege::Insert
                    | Privilege::Update
                    | Privilege::References => {}
                    _ => {
                        return Err(err(format!(
                            "syntax error: privilege {:?} does not accept a column list",
                            p
                        )));
                    }
                }
                self.next(); // (
                loop {
                    match self.next() {
                        Token::Ident(n) => columns.push(n),
                        other => {
                            return Err(err(format!(
                                "syntax error: expected column name, found {:?}",
                                other
                            )));
                        }
                    }
                    if matches!(self.peek(), Token::Comma) {
                        self.next();
                        continue;
                    }
                    break;
                }
                match self.next() {
                    Token::RParen => {}
                    other => {
                        return Err(err(format!(
                            "syntax error: expected ')', found {:?}",
                            other
                        )));
                    }
                }
                if columns.is_empty() {
                    return Err(err("syntax error: empty column list".to_string()));
                }
            }
            privs.push(PrivSpec { priv_: p, columns });
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        Ok(privs)
    }

    /// GRANT privs ON [TABLE] name [, ...] | SEQUENCE ... | DATABASE TO
    /// role [, ...].
    /// True when the upcoming GRANT/REVOKE names an object (`privs ON
    /// ... TO/FROM ...`) rather than granting role membership
    /// (`role [, ...] TO/FROM ...`). Scans ahead for a top-level ON
    /// before a top-level TO/FROM.
    fn grant_has_object(&self) -> bool {
        let mut depth = 0i32;
        for tok in self.tokens[self.pos..].iter() {
            match tok {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                Token::Ident(w) if depth == 0 => {
                    if w.eq_ignore_ascii_case("on") {
                        return true;
                    }
                    if w.eq_ignore_ascii_case("to") || w.eq_ignore_ascii_case("from") {
                        return false;
                    }
                }
                Token::EOF => break,
                _ => {}
            }
        }
        false
    }

    fn parse_grant(&mut self) -> Result<Stmt, SqlError> {
        if !self.grant_has_object() {
            // GRANT role [, ...] TO role [, ...]: role membership.
            let roles = self.parse_grantee_list()?;
            self.expect_keyword("to")?;
            let grantees = self.parse_grantee_list()?;
            // WITH ADMIN OPTION is parsed and ignored (documented).
            if self.eat_keyword("with") {
                self.expect_keyword("admin")?;
                self.expect_keyword("option")?;
            }
            return Ok(Stmt::GrantRole { roles, grantees });
        }
        let privs = self.parse_privilege_list()?;
        self.expect_keyword("on")?;
        let object = self.parse_grant_object()?;
        self.expect_keyword("to")?;
        let grantees = self.parse_grantee_list()?;
        // GRANT OPTION is parsed and ignored (documented).
        if self.eat_keyword("with") {
            self.expect_keyword("grant")?;
            self.expect_keyword("option")?;
        }
        Ok(Stmt::Grant {
            privs,
            object,
            grantees,
        })
    }

    /// REVOKE privs ON ... FROM role [, ...], or REVOKE role [, ...]
    /// FROM role [, ...] for membership.
    fn parse_revoke(&mut self) -> Result<Stmt, SqlError> {
        if !self.grant_has_object() {
            let roles = self.parse_grantee_list()?;
            self.expect_keyword("from")?;
            let grantees = self.parse_grantee_list()?;
            return Ok(Stmt::RevokeRole { roles, grantees });
        }
        let privs = self.parse_privilege_list()?;
        self.expect_keyword("on")?;
        let object = self.parse_grant_object()?;
        self.expect_keyword("from")?;
        let grantees = self.parse_grantee_list()?;
        Ok(Stmt::Revoke {
            privs,
            object,
            grantees,
        })
    }

    fn parse_grant_object(&mut self) -> Result<GrantObject, SqlError> {
        // Optional TABLE keyword (Postgres allows omitting it).
        let is_table_kw = self.eat_keyword("table");
        if self.eat_keyword("sequence") {
            return Ok(GrantObject::Sequence(self.expect_ident()?));
        }
        if self.eat_keyword("database") {
            // `GRANT ... ON DATABASE` without a name targets this database.
            return Ok(GrantObject::Database);
        }
        // TABLE name [, ...]: only the first name is used (v0.11 has a
        // single-database, single-schema catalog; multi-name lists are
        // accepted and applied to each).
        let mut names = Vec::new();
        loop {
            names.push(self.expect_ident()?);
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        let _ = is_table_kw;
        // Encode multi-name lists by chaining: keep the first as the
        // object and stash the rest via repeated execution is complex;
        // instead we only support one name per statement in v0.11.
        if names.len() > 1 {
            return Err(err(
                "only one object name per GRANT/REVOKE is supported".to_string()
            ));
        }
        Ok(GrantObject::Table(names.into_iter().next().unwrap()))
    }

    fn parse_grantee_list(&mut self) -> Result<Vec<String>, SqlError> {
        let mut out = Vec::new();
        loop {
            out.push(self.expect_ident()?);
            if !matches!(self.peek(), Token::Comma) {
                break;
            }
            self.next();
        }
        Ok(out)
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
        | "substring" | "substr" | "trim" | "position" | "replace" | "split_part"
        | "concat" | "concat_ws" | "to_hex" | "to_oct" | "to_bin"
        | "left" | "right" | "reverse"
        // math
        | "abs" | "round" | "floor" | "ceil" | "ceiling" | "sqrt" | "power" | "mod"
        | "sign"
        // date/time
        | "now" | "current_date" | "current_timestamp" | "date_trunc"
        // conditional
        | "coalesce" | "nullif" | "greatest" | "least"
        // v0.9: sequence functions
        | "nextval" | "currval" | "setval"
        // v0.14: PostgreSQL internal operator-function aliases
        | "booleq" | "boolne" | "int4eq" | "texteq"
    )
}

/// Arity check for built-in functions. Wrong argument counts raise
/// "function does not exist" (SQLSTATE 42883), like Postgres.
pub fn check_builtin_arity(name: &str, n: usize) -> Result<(), SqlError> {
    let ok = match name {
        "upper" | "lower" | "length" | "char_length" | "character_length" | "abs" | "floor"
        | "ceil" | "ceiling" | "sqrt" => n == 1,
        "now" | "current_date" | "current_timestamp" => n == 0,
        "substring" | "substr" => n == 2 || n == 3,
        "trim" => n == 1 || n == 3,
        "position" | "power" | "mod" | "nullif" | "date_trunc" => n == 2,
        "replace" | "split_part" => n == 3,
        "round" => n == 1 || n == 2,
        // v0.16: concat() with no args is '' (Postgres); concat_ws(sep, ...)
        // needs >= 1 (a lone separator yields the empty string).
        "concat" => true,
        "concat_ws" => n >= 1,
        "to_hex" | "to_oct" | "to_bin" | "sign" | "reverse" => n == 1,
        "left" | "right" => n == 2,
        "coalesce" | "greatest" | "least" => n >= 1,
        // v0.9: setval(name, value [, is_called])
        "nextval" | "currval" => n == 1,
        "setval" => n == 2 || n == 3,
        // v0.14: PostgreSQL internal operator-function aliases
        "booleq" | "boolne" | "int4eq" | "texteq" => n == 2,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(err_undefined(format!("function {}() does not exist", name)))
    }
}

// ============================================================================
// v0.9: CREATE VIEW raw-text split, s-expression codec for CHECK / DEFAULT
// expressions, and constraint-expression validation.
// ============================================================================

/// Match a keyword case-insensitively at the start of `s`, requiring a
/// word boundary after it. Returns the remainder on success.
fn match_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    if s.len() < kw.len() {
        return None;
    }
    if !s[..kw.len()].eq_ignore_ascii_case(kw) {
        return None;
    }
    let rest = &s[kw.len()..];
    match rest.chars().next() {
        None => Some(rest),
        Some(c) if c.is_alphanumeric() || c == '_' => None,
        Some(_) => Some(rest),
    }
}

/// Skip whitespace and `--` / `/* */` comments.
fn skip_ws_comments(mut s: &str) -> &str {
    loop {
        let t = s.trim_start();
        if let Some(r) = t.strip_prefix("--") {
            match r.find('\n') {
                Some(i) => s = &r[i..],
                None => return "",
            }
        } else if let Some(r) = t.strip_prefix("/*") {
            match r.find("*/") {
                Some(i) => s = &r[i + 2..],
                None => return "",
            }
        } else {
            return t;
        }
    }
}

/// Parse one identifier (bare or double-quoted) at the start of `s`.
fn split_ident(s: &str) -> Option<(String, &str)> {
    let s = skip_ws_comments(s);
    if let Some(r) = s.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = r.char_indices();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if r[i + 1..].starts_with('"') {
                    name.push('"');
                    chars.next();
                } else {
                    return Some((name, &r[i + 1..]));
                }
            } else {
                name.push(c);
            }
        }
        return None;
    }
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() || c == '_' || c == '$' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let word = &s[..end];
    if word.chars().next().unwrap().is_ascii_digit() {
        return None;
    }
    Some((word.to_lowercase(), &s[end..]))
}

/// Skip a balanced parenthesized group; `s` must start with `(`.
/// Returns the text after the closing paren.
fn skip_balanced_parens(s: &str) -> Option<&str> {
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    if first != '(' {
        return None;
    }
    let mut depth = 1;
    let mut in_str = false;
    let mut in_ident = false;
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 1;
                } else {
                    in_str = false;
                }
            }
        } else if in_ident {
            if c == b'"' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 1;
                } else {
                    in_ident = false;
                }
            }
        } else if c == b'\'' {
            in_str = true;
        } else if c == b'"' {
            in_ident = true;
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(&s[i + 1..]);
            }
        }
        i += 1;
    }
    None
}

/// v0.9: split `CREATE [OR REPLACE] VIEW name [(cols)] AS <query>` off the
/// raw statement text. Tokens carry no spans, so the view definition is
/// captured from the source before tokenizing. Returns `None` when the
/// statement is not a CREATE VIEW.
fn try_split_create_view(text: &str) -> Option<Result<Stmt, SqlError>> {
    let mut rest = match_kw(text.trim_start(), "create")?;
    rest = skip_ws_comments(rest);
    let mut or_replace = false;
    if let Some(r) = match_kw(rest, "or") {
        let r2 = skip_ws_comments(r);
        if let Some(r3) = match_kw(r2, "replace") {
            or_replace = true;
            rest = skip_ws_comments(r3);
        }
    }
    // v0.9: TEMP views are accepted as regular views (documented: no
    // session-local temp namespace yet).
    if let Some(r) = match_kw(rest, "temporary") {
        rest = skip_ws_comments(r);
    } else if let Some(r) = match_kw(rest, "temp") {
        rest = skip_ws_comments(r);
    }
    rest = match_kw(rest, "view")?;
    rest = skip_ws_comments(rest);
    // Postgres does not allow IF NOT EXISTS on CREATE VIEW.
    let (name, r) = split_ident(rest)?;
    rest = skip_ws_comments(r);
    // Optional column alias list.
    let mut col_aliases = Vec::new();
    if rest.starts_with('(') {
        let inner_end = rest.find(')')?; // aliases are plain idents; no nesting
        let inner = &rest[1..inner_end];
        for part in inner.split(',') {
            let (a, r) = split_ident(part)?;
            if !skip_ws_comments(r).is_empty() {
                return Some(Err(err(
                    "syntax error in view column alias list".to_string()
                )));
            }
            col_aliases.push(a);
        }
        rest = skip_balanced_parens(rest)?;
        rest = skip_ws_comments(rest);
    }
    rest = match_kw(rest, "as")?;
    let query = skip_ws_comments(rest).trim().to_string();
    if query.is_empty() {
        return Some(Err(
            err("syntax error: expected query after AS".to_string()),
        ));
    }
    // The view query must parse as a SELECT (this also validates it now).
    let parsed = match parse_statement_inner(&query) {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };
    let sel = match parsed {
        Stmt::Select(s) => s,
        _ => {
            return Some(Err(err(
                "syntax error: view query must be a SELECT".to_string()
            )));
        }
    };
    if sel.for_update {
        return Some(Err(err(
            "SELECT FOR UPDATE is not allowed in a view".to_string()
        )));
    }
    let mut deps = Vec::new();
    collect_table_refs(&sel, &mut deps);
    deps.sort();
    deps.dedup();
    if deps.iter().any(|d| d == &name) {
        return Some(Err(err(format!(
            "view \"{}\" cannot depend on itself",
            name
        ))));
    }
    Some(Ok(Stmt::CreateView {
        name,
        query,
        col_aliases,
        or_replace,
    }))
}

/// Collect every plain table name referenced by a SELECT (through joins
/// and derived tables), for view dependency tracking.
pub fn collect_table_refs(sel: &SelectStmt, out: &mut Vec<String>) {
    fn from_item(fi: &FromItem, out: &mut Vec<String>) {
        match fi {
            FromItem::Table { name, .. } => out.push(name.clone()),
            FromItem::Derived { sub, .. } => collect_table_refs(sub, out),
            FromItem::Values { .. } => {}
            FromItem::Join { left, right, .. } => {
                from_item(left, out);
                from_item(right, out);
            }
        }
    }
    for fi in &sel.from {
        from_item(fi, out);
    }
}

/// Parse one statement from already-trimmed text (no view interception).
fn parse_statement_inner(text: &str) -> Result<Stmt, SqlError> {
    let tokens = tokenize(text)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        unnamed_seq: 0,
    };
    let stmt = p.parse_top()?;
    match p.next() {
        Token::EOF => Ok(stmt),
        other => Err(err(format!("syntax error: unexpected {:?}", other))),
    }
}

/// Walk an expression, rejecting anything a CHECK / DEFAULT expression
/// may not contain: aggregates, subqueries, window-less set functions
/// are fine, but no sub-selects, no aggregates, no `Param` placeholders,
/// and no volatile sequence calls other than the recognized nextval form.
pub fn validate_constraint_expr(e: &Expr, what: &str) -> Result<(), SqlError> {
    match e {
        Expr::Agg { .. } => Err(err(format!("cannot use aggregate in {} constraint", what))),
        Expr::ScalarSub(_) | Expr::InSub { .. } | Expr::Exists { .. } => {
            Err(err(format!("cannot use subquery in {} constraint", what)))
        }
        Expr::Param(_) => Err(err(format!("cannot use parameter in {} constraint", what))),
        Expr::ResolvedCol { .. } => Err(err(format!("invalid expression in {}", what))),
        Expr::Column { .. } | Expr::Literal(_) => Ok(()),
        Expr::Arith { left, right, .. } => {
            validate_constraint_expr(left, what)?;
            validate_constraint_expr(right, what)
        }
        Expr::Cast { expr, .. } => validate_constraint_expr(expr, what),
        Expr::Concat(a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            validate_constraint_expr(a, what)?;
            validate_constraint_expr(b, what)
        }
        Expr::Not(a) => validate_constraint_expr(a, what),
        Expr::Like { expr, pattern, .. } => {
            validate_constraint_expr(expr, what)?;
            validate_constraint_expr(pattern, what)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_constraint_expr(expr, what)?;
            validate_constraint_expr(low, what)?;
            validate_constraint_expr(high, what)
        }
        Expr::IsBool { expr, .. } | Expr::IsNull { expr, .. } => {
            validate_constraint_expr(expr, what)
        }
        Expr::Extract { from, .. } => validate_constraint_expr(from, what),
        Expr::Cmp { left, right, .. } => {
            validate_constraint_expr(left, what)?;
            validate_constraint_expr(right, what)
        }
        Expr::Func { name, args } => {
            // v0.9: nextval is allowed in DEFAULT (Postgres auto-increment),
            // but no sequence functions in CHECK (must be immutable).
            if name == "nextval" || name == "currval" || name == "setval" {
                if what != "DEFAULT" || name != "nextval" {
                    return Err(err(format!(
                        "cannot use sequence function in {} constraint",
                        what
                    )));
                }
            }
            for a in args {
                validate_constraint_expr(a, what)?;
            }
            Ok(())
        }
        // v0.10: windows are never valid in constraints.
        Expr::Window { .. } => Err(err(format!(
            "cannot use window function in {} constraint",
            what
        ))),
    }
}

// ---------------------------------------------------------------------------
// S-expression codec for CHECK / DEFAULT expressions (WAL + checkpoints).
// ---------------------------------------------------------------------------

fn sexpr_escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
}

fn encode_literal(lit: &Literal, out: &mut String) {
    out.push_str("(lit ");
    match lit {
        Literal::Int(i) => out.push_str(&format!("int {}", i)),
        Literal::BigInt(i) => out.push_str(&format!("bigint {}", i)),
        Literal::SmallInt(i) => out.push_str(&format!("smallint {}", i)),
        Literal::Float(f) => out.push_str(&format!("float {}", f)),
        Literal::Decimal(s) => {
            out.push_str("decimal ");
            sexpr_escape(s, out);
        }
        Literal::Real(f) => out.push_str(&format!("real {}", f)),
        Literal::Numeric(n) => {
            out.push_str(&format!("numeric {} {}", n.unscaled, n.scale));
        }
        Literal::Text(s) => {
            out.push_str("text ");
            sexpr_escape(s, out);
        }
        Literal::Bool(b) => out.push_str(&format!("bool {}", b)),
        Literal::Date(d) => out.push_str(&format!("date {}", d)),
        Literal::Timestamp(t) => out.push_str(&format!("ts {}", t)),
        Literal::Timestamptz(t) => out.push_str(&format!("tstz {}", t)),
        Literal::Bytea(b) => {
            out.push_str("bytea ");
            for byte in b {
                out.push_str(&format!("{:02x}", byte));
            }
        }
        Literal::Uuid(u) => {
            out.push_str("uuid ");
            for byte in u {
                out.push_str(&format!("{:02x}", byte));
            }
        }
        Literal::Null => out.push_str("null"),
    }
    out.push(')');
}

fn encode_expr_inner(e: &Expr, out: &mut String) {
    match e {
        Expr::Column { table, name } => {
            out.push_str("(col ");
            sexpr_escape(table.as_deref().unwrap_or(""), out);
            out.push(' ');
            sexpr_escape(name, out);
            out.push(')');
        }
        Expr::Literal(l) => encode_literal(l, out),
        Expr::Param(n) => out.push_str(&format!("(param {})", n)),
        Expr::Arith { op, left, right } => {
            let o = match op {
                ArithOp::Add => "add",
                ArithOp::Sub => "sub",
                ArithOp::Mul => "mul",
                ArithOp::Div => "div",
                ArithOp::Mod => "mod",
                ArithOp::Pow => "pow",
            };
            out.push_str(&format!("(arith {} ", o));
            encode_expr_inner(left, out);
            out.push(' ');
            encode_expr_inner(right, out);
            out.push(')');
        }
        Expr::Cast { expr, to } => {
            out.push_str(&format!("(cast {} ", to.sql_name()));
            encode_expr_inner(expr, out);
            out.push(')');
        }
        Expr::Concat(a, b) => {
            out.push_str("(concat ");
            encode_expr_inner(a, out);
            out.push(' ');
            encode_expr_inner(b, out);
            out.push(')');
        }
        Expr::Like {
            expr,
            pattern,
            not,
            ilike,
            escape,
        } => {
            out.push_str(&format!(
                "(like {} {} ",
                if *not { 1 } else { 0 },
                if *ilike { 1 } else { 0 }
            ));
            encode_expr_inner(expr, out);
            out.push(' ');
            encode_expr_inner(pattern, out);
            out.push(' ');
            match escape {
                Some(e) => encode_expr_inner(e, out),
                None => out.push_str("(null)"),
            }
            out.push(')');
        }
        Expr::Between {
            expr,
            low,
            high,
            neg,
        } => {
            out.push_str(&format!("(between {} ", if *neg { 1 } else { 0 }));
            encode_expr_inner(expr, out);
            out.push(' ');
            encode_expr_inner(low, out);
            out.push(' ');
            encode_expr_inner(high, out);
            out.push(')');
        }
        Expr::IsBool { expr, neg, val } => {
            let v = match val {
                Some(true) => 1,
                Some(false) => 0,
                None => 2,
            };
            out.push_str(&format!("(isbool {} {} ", if *neg { 1 } else { 0 }, v));
            encode_expr_inner(expr, out);
            out.push(')');
        }
        Expr::Func { name, args } => {
            out.push_str("(func ");
            sexpr_escape(name, out);
            for a in args {
                out.push(' ');
                encode_expr_inner(a, out);
            }
            out.push(')');
        }
        Expr::Extract { field, from } => {
            out.push_str("(extract ");
            sexpr_escape(field, out);
            out.push(' ');
            encode_expr_inner(from, out);
            out.push(')');
        }
        Expr::Cmp { op, left, right } => {
            let o = match op {
                CmpOp::Eq => "eq",
                CmpOp::Ne => "ne",
                CmpOp::Lt => "lt",
                CmpOp::Le => "le",
                CmpOp::Gt => "gt",
                CmpOp::Ge => "ge",
            };
            out.push_str(&format!("(cmp {} ", o));
            encode_expr_inner(left, out);
            out.push(' ');
            encode_expr_inner(right, out);
            out.push(')');
        }
        Expr::And(a, b) => {
            out.push_str("(and ");
            encode_expr_inner(a, out);
            out.push(' ');
            encode_expr_inner(b, out);
            out.push(')');
        }
        Expr::Or(a, b) => {
            out.push_str("(or ");
            encode_expr_inner(a, out);
            out.push(' ');
            encode_expr_inner(b, out);
            out.push(')');
        }
        Expr::Not(a) => {
            out.push_str("(not ");
            encode_expr_inner(a, out);
            out.push(')');
        }
        Expr::IsNull { expr, neg } => {
            out.push_str(&format!("(isnull {} ", if *neg { 1 } else { 0 }));
            encode_expr_inner(expr, out);
            out.push(')');
        }
        // Aggregates, subqueries, windows and pre-resolved columns can never
        // appear in a persisted CHECK / DEFAULT (validated at parse time).
        Expr::Agg { .. }
        | Expr::ScalarSub(_)
        | Expr::InSub { .. }
        | Expr::Exists { .. }
        | Expr::Window { .. }
        | Expr::ResolvedCol { .. } => {
            out.push_str("(invalid)");
        }
    }
}

fn encode_default(d: &DefaultExpr, out: &mut String) {
    match d {
        DefaultExpr::Lit(l) => {
            out.push_str("(default-lit ");
            encode_literal(l, out);
            out.push(')');
        }
        DefaultExpr::Nextval(s) => {
            out.push_str("(default-nextval ");
            sexpr_escape(s, out);
            out.push(')');
        }
        DefaultExpr::Expr(e) => {
            out.push_str("(default-expr ");
            encode_expr_inner(e, out);
            out.push(')');
        }
    }
}

struct SexprParser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl<'a> SexprParser<'a> {
    fn new(s: &'a str) -> Self {
        SexprParser {
            chars: s.chars().peekable(),
        }
    }
    fn ws(&mut self) {
        while matches!(self.chars.peek(), Some(c) if c.is_whitespace()) {
            self.chars.next();
        }
    }
    fn atom(&mut self) -> Result<String, String> {
        self.ws();
        let mut s = String::new();
        if self.chars.peek() == Some(&'"') {
            self.chars.next();
            loop {
                match self.chars.next() {
                    None => return Err("unterminated string in expression encoding".into()),
                    Some('"') => break,
                    Some('\\') => match self.chars.next() {
                        Some('n') => s.push('\n'),
                        Some('\"') => s.push('"'),
                        Some('\\') => s.push('\\'),
                        Some(c) => {
                            s.push('\\');
                            s.push(c);
                        }
                        None => return Err("unterminated escape".into()),
                    },
                    Some(c) => s.push(c),
                }
            }
            return Ok(s);
        }
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() || c == '(' || c == ')' {
                break;
            }
            s.push(c);
            self.chars.next();
        }
        if s.is_empty() {
            return Err("expected atom in expression encoding".into());
        }
        Ok(s)
    }
    fn open(&mut self) -> Result<(), String> {
        self.ws();
        match self.chars.next() {
            Some('(') => Ok(()),
            _ => Err("expected '(' in expression encoding".into()),
        }
    }
    fn close(&mut self) -> Result<(), String> {
        self.ws();
        match self.chars.next() {
            Some(')') => Ok(()),
            _ => Err("expected ')' in expression encoding".into()),
        }
    }
    fn expr(&mut self) -> Result<Expr, String> {
        self.open()?;
        let head = self.atom()?;
        let e = match head.as_str() {
            "col" => {
                let qual = self.atom()?;
                let name = self.atom()?;
                Expr::Column {
                    table: if qual.is_empty() { None } else { Some(qual) },
                    name,
                }
            }
            "lit" => Expr::Literal(self.literal()?),
            "param" => Expr::Param(self.atom()?.parse::<u32>().map_err(|_| "bad param")?),
            "arith" => {
                let op = match self.atom()?.as_str() {
                    "add" => ArithOp::Add,
                    "sub" => ArithOp::Sub,
                    "mul" => ArithOp::Mul,
                    "div" => ArithOp::Div,
                    "mod" => ArithOp::Mod,
                    "pow" => ArithOp::Pow,
                    o => return Err(format!("bad arith op {}", o)),
                };
                let l = self.expr()?;
                let r = self.expr()?;
                Expr::Arith {
                    op,
                    left: Box::new(l),
                    right: Box::new(r),
                }
            }
            "cast" => {
                let to = coltype_by_name(&self.atom()?)?;
                let x = self.expr()?;
                Expr::Cast {
                    expr: Box::new(x),
                    to,
                }
            }
            "concat" => {
                let a = self.expr()?;
                let b = self.expr()?;
                Expr::Concat(Box::new(a), Box::new(b))
            }
            "like" => {
                let not = self.atom()? == "1";
                let ilike = self.atom()? == "1";
                let x = self.expr()?;
                let p = self.expr()?;
                let e = self.expr()?;
                let escape = match e {
                    Expr::Literal(crate::sql::Literal::Null) => None,
                    other => Some(Box::new(other)),
                };
                Expr::Like {
                    expr: Box::new(x),
                    pattern: Box::new(p),
                    not,
                    ilike,
                    escape,
                }
            }
            "between" => {
                let neg = self.atom()? == "1";
                let x = self.expr()?;
                let low = self.expr()?;
                let high = self.expr()?;
                Expr::Between {
                    expr: Box::new(x),
                    low: Box::new(low),
                    high: Box::new(high),
                    neg,
                }
            }
            "isbool" => {
                let neg = self.atom()? == "1";
                let val = match self.atom()?.as_str() {
                    "1" => Some(true),
                    "0" => Some(false),
                    _ => None,
                };
                let x = self.expr()?;
                Expr::IsBool {
                    expr: Box::new(x),
                    neg,
                    val,
                }
            }
            "func" => {
                let name = self.atom()?;
                let mut args = Vec::new();
                loop {
                    self.ws();
                    if self.chars.peek() == Some(&')') {
                        break;
                    }
                    args.push(self.expr()?);
                }
                Expr::Func { name, args }
            }
            "extract" => {
                let field = self.atom()?;
                let x = self.expr()?;
                Expr::Extract {
                    field,
                    from: Box::new(x),
                }
            }
            "cmp" => {
                let op = match self.atom()?.as_str() {
                    "eq" => CmpOp::Eq,
                    "ne" => CmpOp::Ne,
                    "lt" => CmpOp::Lt,
                    "le" => CmpOp::Le,
                    "gt" => CmpOp::Gt,
                    "ge" => CmpOp::Ge,
                    o => return Err(format!("bad cmp op {}", o)),
                };
                let l = self.expr()?;
                let r = self.expr()?;
                Expr::Cmp {
                    op,
                    left: Box::new(l),
                    right: Box::new(r),
                }
            }
            "and" => {
                let a = self.expr()?;
                let b = self.expr()?;
                Expr::And(Box::new(a), Box::new(b))
            }
            "or" => {
                let a = self.expr()?;
                let b = self.expr()?;
                Expr::Or(Box::new(a), Box::new(b))
            }
            "not" => {
                let a = self.expr()?;
                Expr::Not(Box::new(a))
            }
            "isnull" => {
                let neg = self.atom()? == "1";
                let x = self.expr()?;
                Expr::IsNull {
                    expr: Box::new(x),
                    neg,
                }
            }
            o => return Err(format!("bad expr head {}", o)),
        };
        self.close()?;
        Ok(e)
    }
    fn literal(&mut self) -> Result<Literal, String> {
        let kind = self.atom()?;
        let lit = match kind.as_str() {
            "int" => Literal::Int(self.atom()?.parse().map_err(|_| "bad int")?),
            "bigint" => Literal::BigInt(self.atom()?.parse().map_err(|_| "bad bigint")?),
            "smallint" => Literal::SmallInt(self.atom()?.parse().map_err(|_| "bad smallint")?),
            "float" => Literal::Float(self.atom()?.parse().map_err(|_| "bad float")?),
            "decimal" => Literal::Decimal(self.atom()?),
            "real" => Literal::Real(self.atom()?.parse().map_err(|_| "bad real")?),
            "numeric" => {
                let unscaled: i128 = self.atom()?.parse().map_err(|_| "bad numeric")?;
                let scale: u32 = self.atom()?.parse().map_err(|_| "bad numeric")?;
                Literal::Numeric(crate::storage::Numeric::new(unscaled, scale))
            }
            "text" => Literal::Text(self.atom()?),
            "bool" => Literal::Bool(self.atom()?.parse().map_err(|_| "bad bool")?),
            "date" => Literal::Date(self.atom()?.parse().map_err(|_| "bad date")?),
            "ts" => Literal::Timestamp(self.atom()?.parse().map_err(|_| "bad ts")?),
            "tstz" => Literal::Timestamptz(self.atom()?.parse().map_err(|_| "bad tstz")?),
            "bytea" => {
                let hex = self.atom()?;
                Literal::Bytea(hex_decode(&hex)?)
            }
            "uuid" => {
                let hex = self.atom()?;
                let b = hex_decode(&hex)?;
                if b.len() != 16 {
                    return Err("bad uuid".into());
                }
                let mut u = [0u8; 16];
                u.copy_from_slice(&b);
                Literal::Uuid(u)
            }
            "null" => Literal::Null,
            o => return Err(format!("bad literal kind {}", o)),
        };
        Ok(lit)
    }
}

fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("bad hex".into());
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).ok_or("bad hex")?;
        let lo = (bytes[i + 1] as char).to_digit(16).ok_or("bad hex")?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Ok(out)
}

fn coltype_by_name(name: &str) -> Result<ColType, String> {
    Ok(match name {
        "integer" | "int" | "int4" => ColType::Int,
        "bigint" | "int8" => ColType::BigInt,
        "smallint" | "int2" => ColType::SmallInt,
        "double precision" | "float8" | "float" => ColType::Float,
        "real" | "float4" => ColType::Float4,
        "numeric" | "decimal" => ColType::Numeric,
        "text" | "varchar" | "character varying" => ColType::Text,
        "boolean" | "bool" => ColType::Bool,
        "date" => ColType::Date,
        "timestamp" | "timestamp without time zone" => ColType::Timestamp,
        "timestamptz" | "timestamp with time zone" => ColType::Timestamptz,
        "bytea" => ColType::Bytea,
        "uuid" => ColType::Uuid,
        o => return Err(format!("bad column type {}", o)),
    })
}

fn fk_action_name(a: FkAction) -> &'static str {
    match a {
        FkAction::Restrict => "restrict",
        FkAction::Cascade => "cascade",
        FkAction::SetNull => "setnull",
        FkAction::SetDefault => "setdefault",
    }
}

fn parse_fk_action(s: &str) -> Result<FkAction, String> {
    match s {
        "restrict" => Ok(FkAction::Restrict),
        "cascade" => Ok(FkAction::Cascade),
        "setnull" => Ok(FkAction::SetNull),
        "setdefault" => Ok(FkAction::SetDefault),
        o => Err(format!("bad fk action {}", o)),
    }
}

/// v0.9: encode a table's full constraint/default metadata for WAL and
/// checkpoints. All variable-length lists align with the table's columns
/// by position (defaults/notnull) or carry their own names.
pub fn encode_constraints(t: &crate::storage::Table) -> String {
    let mut out = String::from("(constraints ");
    out.push_str("(notnull");
    for b in &t.not_null {
        out.push_str(if *b { " 1" } else { " 0" });
    }
    out.push_str(") (defaults");
    for d in &t.defaults {
        out.push(' ');
        match d {
            Some(dd) => encode_default(dd, &mut out),
            None => out.push('-'),
        }
    }
    out.push_str(") (checks");
    for c in &t.checks {
        out.push('(');
        sexpr_escape(&c.name, &mut out);
        out.push(' ');
        encode_expr_inner(&c.expr, &mut out);
        out.push(')');
    }
    out.push_str(") (uniques");
    for u in &t.uniques {
        out.push('(');
        sexpr_escape(&u.name, &mut out);
        for c in &u.cols {
            out.push(' ');
            sexpr_escape(c, &mut out);
        }
        out.push(')');
    }
    out.push(')');
    match &t.pkey {
        Some(pk) => {
            out.push_str(" (pkey ");
            sexpr_escape(&pk.name, &mut out);
            for c in &pk.cols {
                out.push(' ');
                sexpr_escape(c, &mut out);
            }
            out.push(')');
        }
        None => out.push_str(" (pkey -)"),
    }
    out.push_str(" (fks");
    for f in &t.fks {
        out.push('(');
        sexpr_escape(&f.name, &mut out);
        out.push_str(" (cols");
        for c in &f.cols {
            out.push(' ');
            sexpr_escape(c, &mut out);
        }
        out.push_str(") (ref ");
        sexpr_escape(&f.ref_table, &mut out);
        out.push_str(") (refcols");
        for c in &f.ref_cols {
            out.push(' ');
            sexpr_escape(c, &mut out);
        }
        out.push_str(") (ondel ");
        out.push_str(fk_action_name(f.on_delete));
        out.push_str(") (onupd ");
        out.push_str(fk_action_name(f.on_update));
        out.push_str("))");
    }
    out.push_str("))");
    out
}

/// Decoded v0.9 table constraint metadata (WAL replay / checkpoints).
pub struct DecodedConstraints {
    pub not_null: Vec<bool>,
    pub defaults: Vec<Option<DefaultExpr>>,
    pub checks: Vec<CheckDef>,
    pub uniques: Vec<UniqueDef>,
    pub pkey: Option<UniqueDef>,
    pub fks: Vec<FkDef>,
}

fn sexpr_is_close(p: &mut SexprParser) -> bool {
    p.ws();
    p.chars.peek() == Some(&')')
}

pub fn decode_constraints(s: &str) -> Result<DecodedConstraints, String> {
    let mut p = SexprParser::new(s);
    p.open()?;
    if p.atom()? != "constraints" {
        return Err("bad constraints head".into());
    }
    // (notnull 0 1 ...)
    p.open()?;
    if p.atom()? != "notnull" {
        return Err("bad notnull head".into());
    }
    let mut not_null = Vec::new();
    while !sexpr_is_close(&mut p) {
        not_null.push(match p.atom()?.as_str() {
            "1" => true,
            "0" => false,
            o => return Err(format!("bad notnull bit {}", o)),
        });
    }
    p.close()?;
    // (defaults ...)
    p.open()?;
    if p.atom()? != "defaults" {
        return Err("bad defaults head".into());
    }
    let mut defaults = Vec::new();
    while !sexpr_is_close(&mut p) {
        p.ws();
        if p.chars.peek() == Some(&'-') {
            p.chars.next();
            defaults.push(None);
            continue;
        }
        p.open()?;
        let head = p.atom()?;
        let d = match head.as_str() {
            "default-lit" => {
                p.open()?;
                let lit_head = p.atom()?;
                if lit_head != "lit" {
                    return Err(format!("bad default-lit head {}", lit_head));
                }
                let lit = p.literal()?;
                p.close()?;
                DefaultExpr::Lit(lit)
            }
            "default-nextval" => DefaultExpr::Nextval(p.atom()?),
            "default-expr" => DefaultExpr::Expr(p.expr()?),
            o => return Err(format!("bad default head {}", o)),
        };
        p.close()?;
        defaults.push(Some(d));
    }
    p.close()?;
    // (checks ...)
    p.open()?;
    if p.atom()? != "checks" {
        return Err("bad checks head".into());
    }
    let mut checks = Vec::new();
    while !sexpr_is_close(&mut p) {
        p.open()?;
        let name = p.atom()?;
        let expr = p.expr()?;
        p.close()?;
        checks.push(CheckDef { name, expr });
    }
    p.close()?;
    // (uniques ...)
    p.open()?;
    if p.atom()? != "uniques" {
        return Err("bad uniques head".into());
    }
    let mut uniques = Vec::new();
    while !sexpr_is_close(&mut p) {
        p.open()?;
        let name = p.atom()?;
        let mut cols = Vec::new();
        while !sexpr_is_close(&mut p) {
            cols.push(p.atom()?);
        }
        p.close()?;
        uniques.push(UniqueDef { name, cols });
    }
    p.close()?;
    // (pkey -) | (pkey name cols...)
    p.open()?;
    if p.atom()? != "pkey" {
        return Err("bad pkey head".into());
    }
    let pkey = if sexpr_is_close(&mut p) {
        None
    } else {
        let name = p.atom()?;
        let mut cols = Vec::new();
        while !sexpr_is_close(&mut p) {
            cols.push(p.atom()?);
        }
        Some(UniqueDef { name, cols })
    };
    // Careful: `(pkey -)` encodes absence as the atom "-".
    let pkey = match &pkey {
        Some(pk) if pk.name == "-" && pk.cols.is_empty() => None,
        other => other.clone(),
    };
    p.close()?;
    // (fks ...)
    p.open()?;
    if p.atom()? != "fks" {
        return Err("bad fks head".into());
    }
    let mut fks = Vec::new();
    while !sexpr_is_close(&mut p) {
        p.open()?;
        let name = p.atom()?;
        p.open()?;
        if p.atom()? != "cols" {
            return Err("bad fk cols head".into());
        }
        let mut cols = Vec::new();
        while !sexpr_is_close(&mut p) {
            cols.push(p.atom()?);
        }
        p.close()?;
        p.open()?;
        if p.atom()? != "ref" {
            return Err("bad fk ref head".into());
        }
        let ref_table = p.atom()?;
        p.close()?;
        p.open()?;
        if p.atom()? != "refcols" {
            return Err("bad fk refcols head".into());
        }
        let mut ref_cols = Vec::new();
        while !sexpr_is_close(&mut p) {
            ref_cols.push(p.atom()?);
        }
        p.close()?;
        p.open()?;
        if p.atom()? != "ondel" {
            return Err("bad fk ondel head".into());
        }
        let on_delete = parse_fk_action(&p.atom()?)?;
        p.close()?;
        p.open()?;
        if p.atom()? != "onupd" {
            return Err("bad fk onupd head".into());
        }
        let on_update = parse_fk_action(&p.atom()?)?;
        p.close()?;
        p.close()?;
        fks.push(FkDef {
            name,
            cols,
            ref_table,
            ref_cols,
            on_delete,
            on_update,
        });
    }
    p.close()?;
    p.close()?;
    p.ws();
    if p.chars.peek().is_some() {
        return Err("trailing data in constraints encoding".into());
    }
    Ok(DecodedConstraints {
        not_null,
        defaults,
        checks,
        uniques,
        pkey,
        fks,
    })
}
