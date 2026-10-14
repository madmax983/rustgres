//! v1.85: the jsonpath engine — parser, `jsonpath_out` renderer, and
//! evaluator over `JsonbVal`, grounded in PG19
//! `src/backend/utils/adt/jsonpath_gram.y` (grammar),
//! `jsonpath_scan.l` (scanner), `jsonpath.c` (`printJsonPathItem`,
//! `operationPriority`), and `jsonpath_exec.c` (lax/strict execution,
//! `JsonPathExecResult` error taxonomy with the SQL/JSON 2203x codes).
//!
//! A parsed path is a tree of `JpExpr`: accessor chains nest
//! (Key(Key(Root))) where PG has a flat item list; evaluation is
//! sequence-based like PG's `executeItem` (each node maps an input
//! sequence to an output sequence). Predicate nodes live in the same
//! tree (PG's grammar has one tree for exprs and predicates);
//! `eval_pred` gives them three-valued semantics.

use super::json::{JsonbVal, jsonb_escape_string};
use super::*;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct JpPath {
    pub lax: bool,
    pub expr: JpExpr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JpArith {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl JpArith {
    pub fn sql(self) -> &'static str {
        match self {
            JpArith::Add => "+",
            JpArith::Sub => "-",
            JpArith::Mul => "*",
            JpArith::Div => "/",
            JpArith::Mod => "%",
        }
    }
    fn priority(self) -> u8 {
        match self {
            JpArith::Add | JpArith::Sub => 3,
            JpArith::Mul | JpArith::Div | JpArith::Mod => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JpCmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl JpCmp {
    pub fn sql(self) -> &'static str {
        match self {
            JpCmp::Eq => "==",
            JpCmp::Ne => "!=",
            JpCmp::Lt => "<",
            JpCmp::Le => "<=",
            JpCmp::Gt => ">",
            JpCmp::Ge => ">=",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum JpMethod {
    Abs,
    Size,
    Type,
    Floor,
    Double,
    Ceiling,
    KeyValue,
    Bigint,
    Boolean,
    Date,
    Datetime(Option<String>),
    Decimal(Option<i64>, Option<i64>),
    Integer,
    Number,
    StringM,
    Time(Option<i64>),
    TimeTz(Option<i64>),
    Timestamp(Option<i64>),
    TimestampTz(Option<i64>),
    Lower,
    Upper,
    Initcap,
    Replace(String, String),
    SplitPart(String, i64),
    Ltrim(Option<String>),
    Rtrim(Option<String>),
    Btrim(Option<String>),
}

impl JpMethod {
    /// The `.name()` spelling used in error messages and rendering.
    pub fn name(&self) -> &'static str {
        match self {
            JpMethod::Abs => "abs",
            JpMethod::Size => "size",
            JpMethod::Type => "type",
            JpMethod::Floor => "floor",
            JpMethod::Double => "double",
            JpMethod::Ceiling => "ceiling",
            JpMethod::KeyValue => "keyvalue",
            JpMethod::Bigint => "bigint",
            JpMethod::Boolean => "boolean",
            JpMethod::Date => "date",
            JpMethod::Datetime(_) => "datetime",
            JpMethod::Decimal(..) => "decimal",
            JpMethod::Integer => "integer",
            JpMethod::Number => "number",
            JpMethod::StringM => "string",
            JpMethod::Time(_) => "time",
            JpMethod::TimeTz(_) => "time_tz",
            JpMethod::Timestamp(_) => "timestamp",
            JpMethod::TimestampTz(_) => "timestamp_tz",
            JpMethod::Lower => "lower",
            JpMethod::Upper => "upper",
            JpMethod::Initcap => "initcap",
            JpMethod::Replace(..) => "replace",
            JpMethod::SplitPart(..) => "split_part",
            JpMethod::Ltrim(_) => "ltrim",
            JpMethod::Rtrim(_) => "rtrim",
            JpMethod::Btrim(_) => "btrim",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum JpExpr {
    Root,
    Current,
    Last,
    Var(String),
    Str(String),
    Num(Numeric),
    Bool(bool),
    Null,
    Key(Box<JpExpr>, String),
    AnyKey(Box<JpExpr>),
    AnyArray(Box<JpExpr>),
    /// Subscript list: (from, Option<to>) per bracket element.
    Subscript(Box<JpExpr>, Vec<(JpExpr, Option<JpExpr>)>),
    /// `**{first to last}`; last == -1 means unbounded (PG's PG_UINT32_MAX).
    Any(Box<JpExpr>, i64, i64),
    Filter(Box<JpExpr>, Box<JpExpr>),
    Method(Box<JpExpr>, JpMethod),
    Arith(JpArith, Box<JpExpr>, Box<JpExpr>),
    UnaryPlus(Box<JpExpr>),
    UnaryMinus(Box<JpExpr>),
    // Predicate nodes.
    Cmp(JpCmp, Box<JpExpr>, Box<JpExpr>),
    And(Box<JpExpr>, Box<JpExpr>),
    Or(Box<JpExpr>, Box<JpExpr>),
    Not(Box<JpExpr>),
    IsUnknown(Box<JpExpr>),
    Exists(Box<JpExpr>),
    StartsWith(Box<JpExpr>, Box<JpExpr>),
    LikeRegex(Box<JpExpr>, String, Option<String>),
}

impl JpExpr {
    pub fn is_predicate(&self) -> bool {
        matches!(
            self,
            JpExpr::Cmp(..)
                | JpExpr::And(..)
                | JpExpr::Or(..)
                | JpExpr::Not(_)
                | JpExpr::IsUnknown(_)
                | JpExpr::Exists(_)
                | JpExpr::StartsWith(..)
                | JpExpr::LikeRegex(..)
        )
    }
    /// printJsonPathItem's operationPriority (jsonpath.c).
    fn priority(&self) -> u8 {
        match self {
            JpExpr::Or(..) => 0,
            JpExpr::And(..) => 1,
            JpExpr::Cmp(..) | JpExpr::StartsWith(..) => 2,
            JpExpr::Arith(op, ..) => op.priority(),
            JpExpr::UnaryPlus(_) | JpExpr::UnaryMinus(_) => 5,
            JpExpr::LikeRegex(..) => 2,
            _ => 6,
        }
    }
}

// ---------------------------------------------------------------------------
// Scanner (jsonpath_scan.l)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum JpTok {
    Dollar,
    At,
    Dot,
    Comma,
    LBracket,
    RBracket,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Question,
    Star,
    StarStar,
    Plus,
    Minus,
    Slash,
    Percent,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Bang,
    Str(String),
    Ident(String),
    Var(String),
    Num(String),
    Eof,
}

struct JpLexer {
    chars: Vec<char>,
    pos: usize,
}

impl JpLexer {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }
    fn err<T>(&self, near: &str) -> Result<T, String> {
        Err(format!(
            "syntax error at or near {:?} of jsonpath input",
            near
        ))
    }

    /// jsonpath_scan.l string literal: escapes \b \f \n \r \t \v \\ \"
    /// \/ \xHH \uXXXX \u{H...}; any other escaped char is itself.
    fn lex_string(&mut self) -> Result<String, String> {
        // self.chars[pos] == '"'
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = match self.peek() {
                None => return Err("syntax error at end of jsonpath input".to_string()),
                Some(c) => c,
            };
            self.pos += 1;
            match c {
                '"' => return Ok(out),
                '\\' => {
                    let e = match self.peek() {
                        None => return Err("syntax error at end of jsonpath input".to_string()),
                        Some(c) => c,
                    };
                    self.pos += 1;
                    match e {
                        'b' => out.push('\u{0008}'),
                        'f' => out.push('\u{000C}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'v' => out.push('\u{000B}'),
                        'x' => {
                            let h: String = (0..2)
                                .filter_map(|_| {
                                    let c = self.peek()?;
                                    self.pos += 1;
                                    Some(c)
                                })
                                .collect();
                            let v = u32::from_str_radix(&h, 16)
                                .map_err(|_| "invalid \\x escape".to_string())?;
                            out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                        }
                        'u' => {
                            if self.peek() == Some('{') {
                                self.pos += 1;
                                let mut h = String::new();
                                while let Some(c) = self.peek() {
                                    if c == '}' {
                                        break;
                                    }
                                    h.push(c);
                                    self.pos += 1;
                                }
                                if self.peek() != Some('}') {
                                    return Err("syntax error at end of jsonpath input".to_string());
                                }
                                self.pos += 1;
                                let v = u32::from_str_radix(&h, 16)
                                    .map_err(|_| "invalid \\u escape".to_string())?;
                                // PG combines surrogate pairs in \u{...}
                                // too; a lone surrogate is an error there,
                                // approximated here by replacement.
                                out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                            } else {
                                let h: String = (0..4)
                                    .filter_map(|_| {
                                        let c = self.peek()?;
                                        self.pos += 1;
                                        Some(c)
                                    })
                                    .collect();
                                let mut v = u32::from_str_radix(&h, 16)
                                    .map_err(|_| "invalid \\u escape".to_string())?;
                                if v == 0 {
                                    // PG: \u0000 is an error in
                                    // jsonpath strings.
                                    return Err(
                                        "\\u0000 cannot be used in jsonpath string".to_string()
                                    );
                                }
                                if (0xD800..0xDC00).contains(&v) {
                                    // A following \uXXXX low surrogate
                                    // is required; PG errors otherwise.
                                    let mut paired = false;
                                    if self.peek() == Some('\\') && self.peek2() == Some('u') {
                                        let save = self.pos;
                                        self.pos += 2;
                                        let h2: String = (0..4)
                                            .filter_map(|_| {
                                                let c = self.peek()?;
                                                self.pos += 1;
                                                Some(c)
                                            })
                                            .collect();
                                        if let Ok(lo) = u32::from_str_radix(&h2, 16) {
                                            if (0xDC00..0xE000).contains(&lo) {
                                                v = 0x10000 + ((v - 0xD800) << 10) + (lo - 0xDC00);
                                                paired = true;
                                            }
                                        }
                                        if !paired {
                                            self.pos = save;
                                        }
                                    }
                                    if !paired {
                                        return Err("unpaired unicode high surrogate".to_string());
                                    }
                                } else if (0xDC00..0xE000).contains(&v) {
                                    return Err("unpaired unicode low surrogate".to_string());
                                }
                                out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                            }
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn lex_number(&mut self) -> Result<String, String> {
        let start = self.pos;
        // Based integers (0x/0o/0b) per PG16+ scanner rules.
        if self.peek() == Some('0')
            && matches!(self.peek2(), Some('x' | 'X' | 'o' | 'O' | 'b' | 'B'))
        {
            self.pos += 2;
            let radix = match self.chars[self.pos - 1] {
                'x' | 'X' => 16,
                'o' | 'O' => 8,
                _ => 2,
            };
            while let Some(c) = self.peek() {
                if c.is_digit(radix) || c == '_' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            return Ok(self.chars[start..self.pos].iter().collect());
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == '_' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.pos += 1;
            }
            if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == '_' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            } else {
                self.pos = save;
            }
        }
        Ok(self.chars[start..self.pos].iter().collect())
    }

    fn ident_start(c: char) -> bool {
        c.is_alphabetic() || c == '_' || (c as u32) >= 0x80
    }
    fn ident_cont(c: char) -> bool {
        c.is_alphanumeric() || c == '_' || c == '$' || (c as u32) >= 0x80
    }

    fn next_tok(&mut self) -> Result<JpTok, String> {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
        let c = match self.peek() {
            None => return Ok(JpTok::Eof),
            Some(c) => c,
        };
        let two = |a: char, b: char| self.peek() == Some(a) && self.peek2() == Some(b);
        match c {
            '$' => {
                self.pos += 1;
                match self.peek() {
                    Some('"') => {
                        let name = self.lex_string()?;
                        Ok(JpTok::Var(name))
                    }
                    Some(c2) if Self::ident_start(c2) => {
                        let start = self.pos;
                        while let Some(c2) = self.peek() {
                            if Self::ident_cont(c2) {
                                self.pos += 1;
                            } else {
                                break;
                            }
                        }
                        Ok(JpTok::Var(self.chars[start..self.pos].iter().collect()))
                    }
                    _ => Ok(JpTok::Dollar),
                }
            }
            '@' => {
                self.pos += 1;
                Ok(JpTok::At)
            }
            '.' => {
                // `.5` is a numeric literal.
                if matches!(self.peek2(), Some(c2) if c2.is_ascii_digit()) {
                    Ok(JpTok::Num(self.lex_number()?))
                } else {
                    self.pos += 1;
                    Ok(JpTok::Dot)
                }
            }
            ',' => {
                self.pos += 1;
                Ok(JpTok::Comma)
            }
            '[' => {
                self.pos += 1;
                Ok(JpTok::LBracket)
            }
            ']' => {
                self.pos += 1;
                Ok(JpTok::RBracket)
            }
            '(' => {
                self.pos += 1;
                Ok(JpTok::LParen)
            }
            ')' => {
                self.pos += 1;
                Ok(JpTok::RParen)
            }
            '{' => {
                self.pos += 1;
                Ok(JpTok::LBrace)
            }
            '}' => {
                self.pos += 1;
                Ok(JpTok::RBrace)
            }
            '?' => {
                self.pos += 1;
                Ok(JpTok::Question)
            }
            '*' => {
                if two('*', '*') {
                    self.pos += 2;
                    Ok(JpTok::StarStar)
                } else {
                    self.pos += 1;
                    Ok(JpTok::Star)
                }
            }
            '+' => {
                self.pos += 1;
                Ok(JpTok::Plus)
            }
            '-' => {
                self.pos += 1;
                Ok(JpTok::Minus)
            }
            '/' => {
                self.pos += 1;
                Ok(JpTok::Slash)
            }
            '%' => {
                self.pos += 1;
                Ok(JpTok::Percent)
            }
            '=' => {
                if two('=', '=') {
                    self.pos += 2;
                    Ok(JpTok::EqEq)
                } else {
                    self.err("=")
                }
            }
            '!' => {
                if two('!', '=') {
                    self.pos += 2;
                    Ok(JpTok::Ne)
                } else {
                    self.pos += 1;
                    Ok(JpTok::Bang)
                }
            }
            '<' => {
                if two('<', '=') {
                    self.pos += 2;
                    Ok(JpTok::Le)
                } else {
                    self.pos += 1;
                    Ok(JpTok::Lt)
                }
            }
            '>' => {
                if two('>', '=') {
                    self.pos += 2;
                    Ok(JpTok::Ge)
                } else {
                    self.pos += 1;
                    Ok(JpTok::Gt)
                }
            }
            '&' => {
                if two('&', '&') {
                    self.pos += 2;
                    Ok(JpTok::AndAnd)
                } else {
                    self.err("&")
                }
            }
            '|' => {
                if two('|', '|') {
                    self.pos += 2;
                    Ok(JpTok::OrOr)
                } else {
                    self.err("|")
                }
            }
            '"' => Ok(JpTok::Str(self.lex_string()?)),
            c if c.is_ascii_digit() => Ok(JpTok::Num(self.lex_number()?)),
            c if Self::ident_start(c) => {
                let start = self.pos;
                while let Some(c2) = self.peek() {
                    if Self::ident_cont(c2) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                Ok(JpTok::Ident(self.chars[start..self.pos].iter().collect()))
            }
            other => self.err(&other.to_string()),
        }
    }
}

fn tokenize_jsonpath(text: &str) -> Result<Vec<JpTok>, String> {
    let mut lx = JpLexer {
        chars: text.chars().collect(),
        pos: 0,
    };
    let mut out = Vec::new();
    loop {
        let t = lx.next_tok()?;
        let done = t == JpTok::Eof;
        out.push(t);
        if done {
            return Ok(out);
        }
    }
}

// ---------------------------------------------------------------------------
// Parser (jsonpath_gram.y)
// ---------------------------------------------------------------------------

struct JpParser {
    toks: Vec<JpTok>,
    pos: usize,
    /// True while parsing a predicate context (filter / exists /
    /// subscript index), the only places `@` may appear (PG's
    /// grammar produces CURRENT only there).
    in_pred: bool,
}

type PResult<T> = Result<T, String>;

impl JpParser {
    fn peek(&self) -> &JpTok {
        &self.toks[self.pos]
    }
    fn peek_at(&self, n: usize) -> &JpTok {
        self.toks.get(self.pos + n).unwrap_or(&JpTok::Eof)
    }
    fn next(&mut self) -> JpTok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn err<T>(&self) -> PResult<T> {
        Err(match self.peek() {
            JpTok::Eof => "syntax error at end of jsonpath input".to_string(),
            JpTok::Str(s) => format!("syntax error at or near {:?} of jsonpath input", s),
            JpTok::Ident(s) | JpTok::Var(s) | JpTok::Num(s) => {
                format!("syntax error at or near {:?} of jsonpath input", s)
            }
            t => format!(
                "syntax error at or near {:?} of jsonpath input",
                tok_text(t)
            ),
        })
    }
    fn expect(&mut self, t: &JpTok) -> PResult<()> {
        if self.peek() == t {
            self.next();
            Ok(())
        } else {
            self.err()
        }
    }
    fn eat_ident(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), JpTok::Ident(s) if s.eq_ignore_ascii_case(kw)) {
            self.next();
            true
        } else {
            false
        }
    }
    fn peek_ident(&self, kw: &str) -> bool {
        matches!(self.peek(), JpTok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn parse_path(&mut self) -> PResult<JpPath> {
        let mut lax = true;
        if self.eat_ident("strict") {
            lax = false;
        } else {
            self.eat_ident("lax");
        }
        if *self.peek() == JpTok::Eof {
            // PG: an empty (or mode-only) input is invalid.
            return Err("syntax error at end of jsonpath input".to_string());
        }
        let expr = self.parse_pred()?;
        if *self.peek() != JpTok::Eof {
            return self.err();
        }
        // PG validates `last` placement at parse time (flattenJsonPathParseItem
        // via makeItem checks in jsonpath.c): LAST only in array subscripts.
        validate_last(&expr, false)?;
        Ok(JpPath { lax, expr })
    }

    /// predicate := pred_or; pred_or := pred_and (|| pred_and)*
    fn parse_pred(&mut self) -> PResult<JpExpr> {
        let mut left = self.parse_pred_and()?;
        loop {
            if *self.peek() == JpTok::OrOr || self.peek_ident("or") {
                self.next();
                let right = self.parse_pred_and()?;
                left = JpExpr::Or(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_pred_and(&mut self) -> PResult<JpExpr> {
        let mut left = self.parse_pred_not()?;
        loop {
            if *self.peek() == JpTok::AndAnd || self.peek_ident("and") {
                self.next();
                let right = self.parse_pred_not()?;
                left = JpExpr::And(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    /// NOT applies only to a delimited predicate (gram.y).
    fn parse_pred_not(&mut self) -> PResult<JpExpr> {
        if *self.peek() == JpTok::Bang {
            self.next();
            let inner = self.parse_delimited_pred()?;
            return Ok(JpExpr::Not(Box::new(inner)));
        }
        self.parse_pred_atom()
    }

    fn parse_delimited_pred(&mut self) -> PResult<JpExpr> {
        if *self.peek() == JpTok::LParen {
            self.next();
            let inner = self.parse_pred()?;
            self.expect(&JpTok::RParen)?;
            return Ok(inner);
        }
        if self.peek_ident("exists") && *self.peek_at(1) == JpTok::LParen {
            self.next();
            self.next();
            let save = self.in_pred;
            self.in_pred = true;
            let e = self.parse_expr();
            self.in_pred = save;
            let e = e?;
            self.expect(&JpTok::RParen)?;
            return Ok(JpExpr::Exists(Box::new(e)));
        }
        self.err()
    }

    fn parse_pred_atom(&mut self) -> PResult<JpExpr> {
        // EXISTS ( expr )
        if self.peek_ident("exists") && *self.peek_at(1) == JpTok::LParen {
            self.next();
            self.next();
            let save = self.in_pred;
            self.in_pred = true;
            let e = self.parse_expr();
            self.in_pred = save;
            let e = e?;
            self.expect(&JpTok::RParen)?;
            return Ok(JpExpr::Exists(Box::new(e)));
        }
        let left = self.parse_expr()?;
        // ( predicate ) IS UNKNOWN — the parenthesized case arrives here
        // with `left` already a predicate node.
        if self.peek_ident("is") {
            // Only `is unknown` (gram.y); peek ahead before committing.
            if matches!(self.peek_at(1), JpTok::Ident(s) if s.eq_ignore_ascii_case("unknown")) {
                self.next();
                self.next();
                return Ok(JpExpr::IsUnknown(Box::new(left)));
            }
        }
        // expr comp_op expr
        let cmp = match self.peek() {
            JpTok::EqEq => Some(JpCmp::Eq),
            JpTok::Ne => Some(JpCmp::Ne),
            JpTok::Lt => Some(JpCmp::Lt),
            JpTok::Le => Some(JpCmp::Le),
            JpTok::Gt => Some(JpCmp::Gt),
            JpTok::Ge => Some(JpCmp::Ge),
            _ => None,
        };
        if let Some(op) = cmp {
            self.next();
            let right = self.parse_expr()?;
            return Ok(JpExpr::Cmp(op, Box::new(left), Box::new(right)));
        }
        // expr STARTS WITH starts_with_initial
        if self.peek_ident("starts") {
            let save = self.pos;
            self.next();
            if self.eat_ident("with") {
                let init = self.parse_starts_initial()?;
                return Ok(JpExpr::StartsWith(Box::new(left), Box::new(init)));
            }
            self.pos = save;
        }
        // expr LIKE_REGEX string [FLAG string]
        if self.peek_ident("like_regex") {
            self.next();
            let pat = match self.next() {
                JpTok::Str(s) => s,
                _ => return self.err(),
            };
            let flags = if self.eat_ident("flag") {
                match self.next() {
                    JpTok::Str(s) => Some(s),
                    _ => return self.err(),
                }
            } else {
                None
            };
            // PG compiles the pattern and validates flags at parse time.
            if let Some(f) = &flags {
                let mut seen = String::new();
                for c in f.chars() {
                    if !"ismxq".contains(c) || seen.contains(c) {
                        return Err(format!(
                            "invalid parameter for jsonpath item method .like_regex: \"{}\"",
                            f
                        ));
                    }
                    seen.push(c);
                }
            }
            like_regex_compile(&pat, flags.as_deref())
                .map_err(|e| format!("invalid regular expression: {}", e.message))?;
            return Ok(JpExpr::LikeRegex(Box::new(left), pat, flags));
        }
        Ok(left)
    }

    fn parse_starts_initial(&mut self) -> PResult<JpExpr> {
        match self.next() {
            JpTok::Str(s) => Ok(JpExpr::Str(s)),
            JpTok::Var(v) => Ok(JpExpr::Var(v)),
            _ => self.err(),
        }
    }

    /// expr := arith over accessor chains.
    fn parse_expr(&mut self) -> PResult<JpExpr> {
        self.parse_arith()
    }

    fn parse_arith(&mut self) -> PResult<JpExpr> {
        let mut left = self.parse_term()?;
        loop {
            let op = match self.peek() {
                JpTok::Plus => JpArith::Add,
                JpTok::Minus => JpArith::Sub,
                _ => break,
            };
            self.next();
            let right = self.parse_term()?;
            left = JpExpr::Arith(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> PResult<JpExpr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                JpTok::Star => JpArith::Mul,
                JpTok::Slash => JpArith::Div,
                JpTok::Percent => JpArith::Mod,
                _ => break,
            };
            self.next();
            let right = self.parse_unary()?;
            left = JpExpr::Arith(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> PResult<JpExpr> {
        match self.peek() {
            JpTok::Plus => {
                self.next();
                Ok(JpExpr::UnaryPlus(Box::new(self.parse_unary()?)))
            }
            JpTok::Minus => {
                self.next();
                Ok(JpExpr::UnaryMinus(Box::new(self.parse_unary()?)))
            }
            _ => self.parse_accessor(),
        }
    }

    /// accessor_expr: a primary followed by accessor ops; a leading
    /// '(' expr-or-predicate ')' may also serve as the base.
    fn parse_accessor(&mut self) -> PResult<JpExpr> {
        let mut base = if *self.peek() == JpTok::LParen {
            self.next();
            let inner = self.parse_pred()?;
            self.expect(&JpTok::RParen)?;
            inner
        } else {
            self.parse_primary()?
        };
        loop {
            let (next, progressed) = self.parse_accessor_op(base)?;
            base = next;
            if !progressed {
                return Ok(base);
            }
        }
    }

    /// One accessor op applied to `base`; Ok(None) when the next token
    /// starts no accessor op.
    fn parse_accessor_op(&mut self, base: JpExpr) -> PResult<(JpExpr, bool)> {
        match self.peek().clone() {
            JpTok::Dot => {
                self.next();
                match self.peek().clone() {
                    JpTok::Star => {
                        self.next();
                        Ok((JpExpr::AnyKey(Box::new(base)), true))
                    }
                    JpTok::StarStar => {
                        self.next();
                        let (first, last) = self.parse_any_bounds()?;
                        Ok((JpExpr::Any(Box::new(base), first, last), true))
                    }
                    JpTok::Str(name) => {
                        self.next();
                        Ok((JpExpr::Key(Box::new(base), name), true))
                    }
                    JpTok::Ident(name) => {
                        // A method call when the name is a method and
                        // '(' follows; otherwise a plain key (gram.y's
                        // key_name includes every method keyword).
                        if is_method_name(&name) && *self.peek_at(1) == JpTok::LParen {
                            self.next(); // method name
                            self.next(); // '('
                            let m = self.parse_method_args(&name)?;
                            Ok((JpExpr::Method(Box::new(base), m), true))
                        } else {
                            self.next();
                            Ok((JpExpr::Key(Box::new(base), name), true))
                        }
                    }
                    _ => self.err(),
                }
            }
            JpTok::LBracket => {
                self.next();
                if *self.peek() == JpTok::Star {
                    self.next();
                    self.expect(&JpTok::RBracket)?;
                    return Ok((JpExpr::AnyArray(Box::new(base)), true));
                }
                let mut idxs = Vec::new();
                let save_pred = self.in_pred;
                self.in_pred = true;
                loop {
                    let from = self.parse_expr()?;
                    let to = if self.eat_ident("to") {
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    idxs.push((from, to));
                    if *self.peek() == JpTok::Comma {
                        self.next();
                        continue;
                    }
                    break;
                }
                self.in_pred = save_pred;
                self.expect(&JpTok::RBracket)?;
                Ok((JpExpr::Subscript(Box::new(base), idxs), true))
            }
            JpTok::Question => {
                self.next();
                self.expect(&JpTok::LParen)?;
                let save = self.in_pred;
                self.in_pred = true;
                let pred = self.parse_pred();
                self.in_pred = save;
                let pred = pred?;
                self.expect(&JpTok::RParen)?;
                Ok((JpExpr::Filter(Box::new(base), Box::new(pred)), true))
            }
            _ => Ok((base, false)),
        }
    }

    /// `**` bounds: `{n}`, `{n to m}`, `{last}`, `{n to last}`, or bare.
    fn parse_any_bounds(&mut self) -> PResult<(i64, i64)> {
        if *self.peek() != JpTok::LBrace {
            return Ok((0, -1));
        }
        self.next();
        let first = self.parse_any_level()?;
        let last = if self.eat_ident("to") {
            self.parse_any_level()?
        } else {
            first
        };
        self.expect(&JpTok::RBrace)?;
        Ok((first, last))
    }

    fn parse_any_level(&mut self) -> PResult<i64> {
        if self.eat_ident("last") {
            return Ok(-1);
        }
        match self.next() {
            JpTok::Num(s) => {
                let v: i64 = s
                    .parse()
                    .map_err(|_| "syntax error in jsonpath input".to_string())?;
                if v < 0 {
                    return Err("syntax error in jsonpath input".to_string());
                }
                Ok(v)
            }
            _ => self.err(),
        }
    }

    fn parse_primary(&mut self) -> PResult<JpExpr> {
        match self.next() {
            JpTok::Dollar => Ok(JpExpr::Root),
            JpTok::At => {
                if self.in_pred {
                    Ok(JpExpr::Current)
                } else {
                    Err("syntax error at or near \"@\" of jsonpath input".to_string())
                }
            }
            JpTok::Var(v) => Ok(JpExpr::Var(v)),
            JpTok::Str(s) => Ok(JpExpr::Str(s)),
            JpTok::Num(s) => Ok(JpExpr::Num(parse_jp_numeric(&s)?)),
            JpTok::Ident(s) => {
                let low = s.to_ascii_lowercase();
                match low.as_str() {
                    "true" => Ok(JpExpr::Bool(true)),
                    "false" => Ok(JpExpr::Bool(false)),
                    "null" => Ok(JpExpr::Null),
                    "last" => Ok(JpExpr::Last),
                    _ => Err(format!("syntax error at or near {:?} of jsonpath input", s)),
                }
            }
            t => Err(match t {
                JpTok::Eof => "syntax error at end of jsonpath input".to_string(),
                other => format!(
                    "syntax error at or near {:?} of jsonpath input",
                    tok_text(&other)
                ),
            }),
        }
    }

    /// Method arguments after '(' (gram.y productions per method).
    fn parse_method_args(&mut self, name: &str) -> PResult<JpMethod> {
        let m = match name {
            "abs" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Abs
            }
            "size" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Size
            }
            "type" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Type
            }
            "floor" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Floor
            }
            "double" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Double
            }
            "ceiling" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Ceiling
            }
            "keyvalue" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::KeyValue
            }
            "bigint" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Bigint
            }
            "boolean" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Boolean
            }
            "date" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Date
            }
            "integer" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Integer
            }
            "number" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Number
            }
            "string" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::StringM
            }
            "lower" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Lower
            }
            "upper" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Upper
            }
            "initcap" => {
                self.expect(&JpTok::RParen)?;
                JpMethod::Initcap
            }
            "datetime" => {
                let arg = self.parse_opt_str_arg()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::Datetime(arg)
            }
            "decimal" => {
                let mut ints = Vec::new();
                if *self.peek() != JpTok::RParen {
                    loop {
                        ints.push(self.parse_int_elem()?);
                        if *self.peek() == JpTok::Comma {
                            self.next();
                            continue;
                        }
                        break;
                    }
                }
                self.expect(&JpTok::RParen)?;
                if ints.len() > 2 {
                    return Err(
                        ".decimal() can only have an optional precision[,scale].".to_string()
                    );
                }
                JpMethod::Decimal(ints.first().copied(), ints.get(1).copied())
            }
            "time" => {
                let p = self.parse_opt_uint_arg()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::Time(p)
            }
            "time_tz" => {
                let p = self.parse_opt_uint_arg()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::TimeTz(p)
            }
            "timestamp" => {
                let p = self.parse_opt_uint_arg()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::Timestamp(p)
            }
            "timestamp_tz" => {
                let p = self.parse_opt_uint_arg()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::TimestampTz(p)
            }
            "replace" => {
                let a = self.parse_str_elem()?;
                self.expect(&JpTok::Comma)?;
                let b = self.parse_str_elem()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::Replace(a, b)
            }
            "split_part" => {
                let a = self.parse_str_elem()?;
                self.expect(&JpTok::Comma)?;
                let n = self.parse_int_elem()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::SplitPart(a, n)
            }
            "ltrim" => {
                let a = self.parse_opt_str_arg()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::Ltrim(a)
            }
            "rtrim" => {
                let a = self.parse_opt_str_arg()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::Rtrim(a)
            }
            "btrim" => {
                let a = self.parse_opt_str_arg()?;
                self.expect(&JpTok::RParen)?;
                JpMethod::Btrim(a)
            }
            _ => return Err(format!("unknown jsonpath method {:?}", name)),
        };
        Ok(m)
    }

    fn parse_str_elem(&mut self) -> PResult<String> {
        match self.next() {
            JpTok::Str(s) => Ok(s),
            _ => self.err(),
        }
    }
    fn parse_opt_str_arg(&mut self) -> PResult<Option<String>> {
        if *self.peek() == JpTok::RParen {
            return Ok(None);
        }
        Ok(Some(self.parse_str_elem()?))
    }
    fn parse_int_elem(&mut self) -> PResult<i64> {
        let neg = match self.peek() {
            JpTok::Plus => {
                self.next();
                false
            }
            JpTok::Minus => {
                self.next();
                true
            }
            _ => false,
        };
        match self.next() {
            JpTok::Num(s) => {
                let v: i64 = s
                    .parse()
                    .map_err(|_| "syntax error in jsonpath input".to_string())?;
                Ok(if neg { -v } else { v })
            }
            _ => self.err(),
        }
    }
    fn parse_opt_uint_arg(&mut self) -> PResult<Option<i64>> {
        if *self.peek() == JpTok::RParen {
            return Ok(None);
        }
        match self.next() {
            JpTok::Num(s) => {
                let v: i64 = s
                    .parse()
                    .map_err(|_| "syntax error in jsonpath input".to_string())?;
                Ok(Some(v))
            }
            _ => self.err(),
        }
    }
}

fn tok_text(t: &JpTok) -> String {
    match t {
        JpTok::Dollar => "$".into(),
        JpTok::At => "@".into(),
        JpTok::Dot => ".".into(),
        JpTok::Comma => ",".into(),
        JpTok::LBracket => "[".into(),
        JpTok::RBracket => "]".into(),
        JpTok::LParen => "(".into(),
        JpTok::RParen => ")".into(),
        JpTok::LBrace => "{".into(),
        JpTok::RBrace => "}".into(),
        JpTok::Question => "?".into(),
        JpTok::Star => "*".into(),
        JpTok::StarStar => "**".into(),
        JpTok::Plus => "+".into(),
        JpTok::Minus => "-".into(),
        JpTok::Slash => "/".into(),
        JpTok::Percent => "%".into(),
        JpTok::EqEq => "==".into(),
        JpTok::Ne => "!=".into(),
        JpTok::Lt => "<".into(),
        JpTok::Le => "<=".into(),
        JpTok::Gt => ">".into(),
        JpTok::Ge => ">=".into(),
        JpTok::AndAnd => "&&".into(),
        JpTok::OrOr => "||".into(),
        JpTok::Bang => "!".into(),
        JpTok::Str(s) => s.clone(),
        JpTok::Ident(s) => s.clone(),
        JpTok::Var(s) => s.clone(),
        JpTok::Num(s) => s.clone(),
        JpTok::Eof => "end".into(),
    }
}

fn is_method_name(name: &str) -> bool {
    matches!(
        name,
        "abs"
            | "size"
            | "type"
            | "floor"
            | "double"
            | "ceiling"
            | "keyvalue"
            | "bigint"
            | "boolean"
            | "date"
            | "datetime"
            | "decimal"
            | "integer"
            | "number"
            | "string"
            | "time"
            | "time_tz"
            | "timestamp"
            | "timestamp_tz"
            | "lower"
            | "upper"
            | "initcap"
            | "replace"
            | "split_part"
            | "ltrim"
            | "rtrim"
            | "btrim"
    )
}

/// PG scanner rules for numeric literals (scan.l via
/// jsonpath_scan.l): no leading zeros in the integer part, and
/// underscores only between digits (never right after a base
/// prefix, never leading/trailing/doubled).
fn valid_jp_number(text: &str) -> bool {
    let t = text;
    let (based, body) = if t.len() > 2 && (&t[..2] == "0x" || &t[..2] == "0X") {
        (16u32, &t[2..])
    } else if t.len() > 2 && (&t[..2] == "0o" || &t[..2] == "0O") {
        (8, &t[2..])
    } else if t.len() > 2 && (&t[..2] == "0b" || &t[..2] == "0B") {
        (2, &t[2..])
    } else {
        (10, t)
    };
    if based != 10 {
        if body.is_empty() || body.starts_with('_') {
            return false;
        }
        let digits: String = body.chars().filter(|c| *c != '_').collect();
        if digits.is_empty() || digits.chars().any(|c| c.to_digit(based).is_none()) {
            return false;
        }
    } else {
        let int_digits: String = body
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '_')
            .filter(|c| *c != '_')
            .collect();
        if int_digits.len() > 1 && int_digits.starts_with('0') {
            return false;
        }
    }
    let chars: Vec<char> = t.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '_' {
            let ok = |o: Option<&char>| o.map(|c| c.to_digit(based).is_some()).unwrap_or(false);
            let prev = if i == 0 { None } else { chars.get(i - 1) };
            if !ok(prev) || !ok(chars.get(i + 1)) {
                return false;
            }
        }
    }
    true
}

/// PG's numeric_in for a jsonpath numeric token (based integers and
/// underscore forms ride our Numeric::parse, like PG19's scanner rules).
fn parse_jp_numeric(s: &str) -> PResult<Numeric> {
    if !valid_jp_number(s) {
        return Err(format!("syntax error at or near {:?} of jsonpath input", s));
    }
    Numeric::parse(s).map_err(|e| match e {
        crate::storage::NumericParseError::Overflow => "value overflows numeric format".to_string(),
        _ => format!("syntax error at or near {:?} of jsonpath input", s),
    })
}

/// `last` may appear only inside an array subscript's index expressions
/// (PG raises this from jsonpath parse/flatten).
fn validate_last(e: &JpExpr, in_subscript: bool) -> PResult<()> {
    match e {
        JpExpr::Last => {
            if in_subscript {
                Ok(())
            } else {
                Err("LAST is allowed only in array subscripts".to_string())
            }
        }
        JpExpr::Subscript(base, idxs) => {
            validate_last(base, in_subscript)?;
            for (from, to) in idxs {
                validate_last(from, true)?;
                if let Some(to) = to {
                    validate_last(to, true)?;
                }
            }
            Ok(())
        }
        JpExpr::Key(b, _)
        | JpExpr::AnyKey(b)
        | JpExpr::AnyArray(b)
        | JpExpr::Any(b, ..)
        | JpExpr::Method(b, _)
        | JpExpr::UnaryPlus(b)
        | JpExpr::UnaryMinus(b)
        | JpExpr::Not(b)
        | JpExpr::IsUnknown(b)
        | JpExpr::Exists(b) => validate_last(b, in_subscript),
        JpExpr::Filter(b, p) => {
            validate_last(b, in_subscript)?;
            validate_last(p, in_subscript)
        }
        JpExpr::Arith(_, a, b)
        | JpExpr::Cmp(_, a, b)
        | JpExpr::And(a, b)
        | JpExpr::Or(a, b)
        | JpExpr::StartsWith(a, b) => {
            validate_last(a, in_subscript)?;
            validate_last(b, in_subscript)
        }
        JpExpr::LikeRegex(a, ..) => validate_last(a, in_subscript),
        _ => Ok(()),
    }
}

/// Parse jsonpath input text (PG's `jsonpath_in`): syntax errors are
/// 22P02 `invalid input syntax for type jsonpath: "<text>"` with the
/// scanner/grammar complaint as DETAIL.
pub(crate) fn parse_jsonpath(text: &str) -> Result<JpPath, ExecError> {
    let toks = tokenize_jsonpath(text).map_err(|detail| {
        exec_err_detail(
            "22P02",
            format!("invalid input syntax for type jsonpath: {:?}", text),
            detail,
        )
    })?;
    let mut p = JpParser {
        toks,
        pos: 0,
        in_pred: false,
    };
    p.parse_path().map_err(|detail| {
        // PG distinguishes a bare complaint (used directly as the
        // message for the LAST-placement error) from syntax details.
        if detail.starts_with("LAST is allowed") {
            exec_err("42601", detail)
        } else {
            exec_err_detail(
                "22P02",
                format!("invalid input syntax for type jsonpath: {:?}", text),
                detail,
            )
        }
    })
}

// ---------------------------------------------------------------------------
// Renderer (jsonpath.c printJsonPathItem)
// ---------------------------------------------------------------------------

/// PG's `jsonpath_out`: `strict ` prefix when not lax, then the item
/// tree printed with printBracketes=true at the root.
pub(crate) fn render_jsonpath(p: &JpPath) -> String {
    let mut out = String::new();
    if !p.lax {
        out.push_str("strict ");
    }
    print_expr(&p.expr, true, &mut out);
    out
}

/// Escape a string the way json.c's escape_json_with_len does (same
/// escape set as jsonb strings), wrapped in double quotes.
fn print_jp_string(s: &str, out: &mut String) {
    jsonb_escape_string(s, out);
}

fn print_expr(e: &JpExpr, brackets: bool, out: &mut String) {
    match e {
        JpExpr::Root => out.push('$'),
        JpExpr::Current => out.push('@'),
        JpExpr::Last => out.push_str("last"),
        JpExpr::Var(name) => {
            out.push('$');
            print_jp_string(name, out);
        }
        JpExpr::Str(s) => print_jp_string(s, out),
        JpExpr::Num(n) => {
            // jpiNumeric prints parens around itself when it heads an
            // accessor chain — handled by the chain cases below, which
            // pass brackets=true for scalar heads. Here: bare.
            out.push_str(&n.to_text());
        }
        JpExpr::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JpExpr::Null => out.push_str("null"),
        JpExpr::Key(base, name) => {
            print_chain_base(base, out);
            out.push('.');
            print_jp_string(name, out);
        }
        JpExpr::AnyKey(base) => {
            print_chain_base(base, out);
            out.push_str(".*");
        }
        JpExpr::AnyArray(base) => {
            print_chain_base(base, out);
            out.push_str("[*]");
        }
        JpExpr::Subscript(base, idxs) => {
            print_chain_base(base, out);
            out.push('[');
            for (i, (from, to)) in idxs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                print_expr(from, false, out);
                if let Some(to) = to {
                    out.push_str(" to ");
                    print_expr(to, false, out);
                }
            }
            out.push(']');
        }
        JpExpr::Any(base, first, last) => {
            print_chain_base(base, out);
            out.push_str(".**");
            if !(*first == 0 && *last == -1) {
                out.push('{');
                print_any_level(*first, out);
                if first != last {
                    out.push_str(" to ");
                    print_any_level(*last, out);
                }
                out.push('}');
            }
        }
        JpExpr::Filter(base, pred) => {
            print_chain_base(base, out);
            out.push_str("?(");
            print_expr(pred, false, out);
            out.push(')');
        }
        JpExpr::Method(base, m) => {
            print_chain_base(base, out);
            print_method(m, out);
        }
        JpExpr::Arith(op, l, r) => {
            if brackets {
                out.push('(');
            }
            print_expr(l, l.priority() <= op.priority(), out);
            out.push(' ');
            out.push_str(op.sql());
            out.push(' ');
            print_expr(r, r.priority() <= op.priority(), out);
            if brackets {
                out.push(')');
            }
        }
        JpExpr::Cmp(op, l, r) => {
            if brackets {
                out.push('(');
            }
            print_expr(l, l.priority() <= 2, out);
            out.push(' ');
            out.push_str(op.sql());
            out.push(' ');
            print_expr(r, r.priority() <= 2, out);
            if brackets {
                out.push(')');
            }
        }
        JpExpr::And(l, r) => {
            if brackets {
                out.push('(');
            }
            print_expr(l, l.priority() <= 1, out);
            out.push_str(" && ");
            print_expr(r, r.priority() <= 1, out);
            if brackets {
                out.push(')');
            }
        }
        JpExpr::Or(l, r) => {
            if brackets {
                out.push('(');
            }
            print_expr(l, l.priority() <= 0, out);
            out.push_str(" || ");
            print_expr(r, r.priority() <= 0, out);
            if brackets {
                out.push(')');
            }
        }
        JpExpr::StartsWith(l, r) => {
            if brackets {
                out.push('(');
            }
            print_expr(l, l.priority() <= 2, out);
            out.push_str(" starts with ");
            print_expr(r, r.priority() <= 2, out);
            if brackets {
                out.push(')');
            }
        }
        JpExpr::LikeRegex(l, pat, flags) => {
            if brackets {
                out.push('(');
            }
            print_expr(l, l.priority() <= 2, out);
            out.push_str(" like_regex ");
            print_jp_string(pat, out);
            if let Some(f) = flags {
                out.push_str(" flag ");
                print_jp_string(f, out);
            }
            if brackets {
                out.push(')');
            }
        }
        JpExpr::Not(inner) => {
            out.push_str("!(");
            print_expr(inner, false, out);
            out.push(')');
        }
        JpExpr::IsUnknown(inner) => {
            out.push('(');
            print_expr(inner, false, out);
            out.push_str(") is unknown");
        }
        JpExpr::Exists(inner) => {
            out.push_str("exists (");
            print_expr(inner, false, out);
            out.push(')');
        }
        JpExpr::UnaryPlus(inner) => {
            if brackets {
                out.push('(');
            }
            out.push('+');
            print_expr(inner, inner.priority() <= 5, out);
            if brackets {
                out.push(')');
            }
        }
        JpExpr::UnaryMinus(inner) => {
            if brackets {
                out.push('(');
            }
            out.push('-');
            print_expr(inner, inner.priority() <= 5, out);
            if brackets {
                out.push(')');
            }
        }
    }
}

/// Print the base of an accessor chain. A scalar literal heading a
/// chain prints parenthesized when it is a numeric (jsonpath.c's
/// jpiNumeric hasNext rule); a binary/unary base prints via its own
/// bracket rule with printBracketes=true like PG's operand printing
/// (e.g. `($ + 1)` when an arithmetic expr heads a chain — PG's list
/// model prints the operand chain with the brackets the caller set).
fn print_chain_base(base: &JpExpr, out: &mut String) {
    match base {
        JpExpr::Num(n) => {
            out.push('(');
            out.push_str(&n.to_text());
            out.push(')');
        }
        JpExpr::Arith(..)
        | JpExpr::UnaryPlus(_)
        | JpExpr::UnaryMinus(_)
        | JpExpr::Cmp(..)
        | JpExpr::And(..)
        | JpExpr::Or(..)
        | JpExpr::StartsWith(..)
        | JpExpr::LikeRegex(..) => print_expr(base, true, out),
        other => print_expr(other, false, out),
    }
}

fn print_any_level(v: i64, out: &mut String) {
    if v == -1 {
        out.push_str("last");
    } else {
        out.push_str(&v.to_string());
    }
}

fn print_method(m: &JpMethod, out: &mut String) {
    match m {
        JpMethod::Datetime(arg) => {
            out.push_str(".datetime(");
            if let Some(a) = arg {
                print_jp_string(a, out);
            }
            out.push(')');
        }
        JpMethod::Decimal(p, s) => {
            out.push_str(".decimal(");
            if let Some(p) = p {
                out.push_str(&p.to_string());
            }
            if let Some(s) = s {
                out.push(',');
                out.push_str(&s.to_string());
            }
            out.push(')');
        }
        JpMethod::Time(p)
        | JpMethod::TimeTz(p)
        | JpMethod::Timestamp(p)
        | JpMethod::TimestampTz(p) => {
            out.push('.');
            out.push_str(m.name());
            out.push('(');
            if let Some(p) = p {
                out.push_str(&p.to_string());
            }
            out.push(')');
        }
        JpMethod::Replace(a, b) => {
            out.push_str(".replace(");
            print_jp_string(a, out);
            out.push(',');
            print_jp_string(b, out);
            out.push(')');
        }
        JpMethod::SplitPart(a, n) => {
            out.push_str(".split_part(");
            print_jp_string(a, out);
            out.push(',');
            out.push_str(&n.to_string());
            out.push(')');
        }
        JpMethod::Ltrim(a) | JpMethod::Rtrim(a) | JpMethod::Btrim(a) => {
            out.push('.');
            out.push_str(m.name());
            out.push('(');
            if let Some(a) = a {
                print_jp_string(a, out);
            }
            out.push(')');
        }
        other => {
            out.push('.');
            out.push_str(other.name());
            out.push_str("()");
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluator (jsonpath_exec.c)
// ---------------------------------------------------------------------------

/// Three-valued predicate result (PG's JsonPathBool).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    fn and(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::False, _) | (_, Tri::False) => Tri::False,
            (Tri::True, Tri::True) => Tri::True,
            _ => Tri::Unknown,
        }
    }
    fn or(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::True, _) | (_, Tri::True) => Tri::True,
            (Tri::False, Tri::False) => Tri::False,
            _ => Tri::Unknown,
        }
    }
    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

pub(crate) struct JpCtx<'a> {
    pub root: &'a JsonbVal,
    pub vars: Option<&'a JsonbVal>,
    pub lax: bool,
}

fn jperr(code: &'static str, msg: impl Into<String>) -> ExecError {
    exec_err(code, msg)
}

/// Structural-error helper: in lax mode PG ignores structural errors
/// (jspIgnoreStructuralErrors) and the item simply yields nothing.
fn structural<T>(
    ctx: &JpCtx,
    code: &'static str,
    msg: impl Into<String>,
) -> Result<Vec<T>, ExecError> {
    if ctx.lax {
        Ok(Vec::new())
    } else {
        Err(jperr(code, msg))
    }
}

type EResult = Result<Vec<JsonbVal>, ExecError>;

/// Evaluate an expression node against an input sequence.
/// `last` is the array-subscript `last` value (element count - 1 of
/// the array whose subscript is being evaluated), when in scope.
fn eval(e: &JpExpr, ctx: &JpCtx, input: &[JsonbVal], last: Option<i64>) -> EResult {
    match e {
        JpExpr::Root => Ok(vec![ctx.root.clone()]),
        JpExpr::Current => Ok(input.to_vec()),
        JpExpr::Last => match last {
            Some(l) => Ok(vec![JsonbVal::Number(Numeric::from_i64(l))]),
            None => Err(jperr("42601", "LAST is allowed only in array subscripts")),
        },
        JpExpr::Var(name) => {
            let vars = match ctx.vars {
                Some(v) => v,
                None => {
                    return Err(jperr(
                        "42704",
                        format!("could not find jsonpath variable \"{}\"", name),
                    ));
                }
            };
            match vars.field(name) {
                Some(v) => Ok(vec![v.clone()]),
                None => Err(jperr(
                    "42704",
                    format!("could not find jsonpath variable \"{}\"", name),
                )),
            }
        }
        JpExpr::Str(s) => Ok(vec![JsonbVal::Str(s.clone())]),
        JpExpr::Num(n) => Ok(vec![JsonbVal::Number(n.clone())]),
        JpExpr::Bool(b) => Ok(vec![JsonbVal::Bool(*b)]),
        JpExpr::Null => Ok(vec![JsonbVal::Null]),
        JpExpr::Key(base, name) => {
            let seq = eval(base, ctx, input, last)?;
            let mut out = Vec::new();
            for item in &seq {
                key_access(ctx, item, name, &mut out)?;
            }
            Ok(out)
        }
        JpExpr::AnyKey(base) => {
            let seq = eval(base, ctx, input, last)?;
            let mut out = Vec::new();
            for item in &seq {
                anykey_access(ctx, item, &mut out)?;
            }
            Ok(out)
        }
        JpExpr::AnyArray(base) => {
            let seq = eval(base, ctx, input, last)?;
            let mut out = Vec::new();
            for item in &seq {
                match item {
                    JsonbVal::Array(elems) => out.extend(elems.iter().cloned()),
                    _ => {
                        if ctx.lax {
                            out.push(item.clone());
                        } else {
                            return Err(jperr(
                                "22039",
                                "jsonpath wildcard array accessor can only be applied to an array",
                            ));
                        }
                    }
                }
            }
            Ok(out)
        }
        JpExpr::Subscript(base, idxs) => eval_subscript(base, idxs, ctx, input, last),
        JpExpr::Any(base, first, lastb) => {
            let seq = eval(base, ctx, input, last)?;
            let mut out = Vec::new();
            for item in &seq {
                any_collect(item, 0, *first, *lastb, &mut out);
            }
            Ok(out)
        }
        JpExpr::Filter(base, pred) => {
            let seq = eval(base, ctx, input, last)?;
            let mut out = Vec::new();
            for item in &seq {
                if let JsonbVal::Array(elems) = item {
                    if ctx.lax {
                        for el in elems {
                            if eval_pred(pred, ctx, std::slice::from_ref(el), last, true)?
                                == Tri::True
                            {
                                out.push(el.clone());
                            }
                        }
                        continue;
                    }
                }
                if eval_pred(pred, ctx, std::slice::from_ref(item), last, true)? == Tri::True {
                    out.push(item.clone());
                }
            }
            Ok(out)
        }
        JpExpr::Method(base, m) => {
            check_method_precision(m)?;
            let seq = eval(base, ctx, input, last)?;
            let mut out = Vec::new();
            for item in &seq {
                if let JsonbVal::Array(elems) = item {
                    if ctx.lax && !matches!(m, JpMethod::Size | JpMethod::Type) {
                        // PG unwraps arrays for most item methods in lax
                        // mode (executeItemOptUnwrapTarget); .size()/
                        // .type() apply to the array itself.
                        for el in elems {
                            apply_method(ctx, el, m, &mut out)?;
                        }
                        continue;
                    }
                }
                apply_method(ctx, item, m, &mut out)?;
            }
            Ok(out)
        }
        JpExpr::Arith(op, l, r) => {
            let mut out = Vec::new();
            for item in input {
                let lv = eval(l, ctx, std::slice::from_ref(item), last)?;
                let rv = eval(r, ctx, std::slice::from_ref(item), last)?;
                let ln = single_numeric(&lv, "left", *op)?;
                let rn = single_numeric(&rv, "right", *op)?;
                out.push(JsonbVal::Number(arith_numeric(*op, &ln, &rn)?));
            }
            Ok(out)
        }
        JpExpr::UnaryPlus(inner) | JpExpr::UnaryMinus(inner) => {
            let neg = matches!(e, JpExpr::UnaryMinus(_));
            let mut out = Vec::new();
            for item in input {
                let v = eval(inner, ctx, std::slice::from_ref(item), last)?;
                let n = single_numeric_unary(&v, neg)?;
                out.push(JsonbVal::Number(if neg { n.neg() } else { n }));
            }
            Ok(out)
        }
        // Predicate nodes in expression position produce boolean items
        // (True/False); Unknown produces no item (PG's jperUnknown in
        // value position yields an empty sequence).
        _ if e.is_predicate() => {
            let mut out = Vec::new();
            for item in input {
                match eval_pred(e, ctx, std::slice::from_ref(item), last, false)? {
                    Tri::True => out.push(JsonbVal::Bool(true)),
                    Tri::False => out.push(JsonbVal::Bool(false)),
                    Tri::Unknown => {}
                }
            }
            Ok(out)
        }
        _ => unreachable!("non-predicate node expected"),
    }
}

/// `.key` on one item (jsonpath_exec.c jpiKey), with lax array unwrap.
fn key_access(
    ctx: &JpCtx,
    item: &JsonbVal,
    name: &str,
    out: &mut Vec<JsonbVal>,
) -> Result<(), ExecError> {
    match item {
        JsonbVal::Object(_) => match item.field(name) {
            Some(v) => {
                out.push(v.clone());
                Ok(())
            }
            None => structural::<()>(
                ctx,
                "2203A",
                format!("JSON object does not contain key \"{}\"", name),
            )
            .map(|_| ()),
        },
        JsonbVal::Array(elems) => {
            if ctx.lax {
                for el in elems {
                    key_access(ctx, el, name, out)?;
                }
                Ok(())
            } else {
                Err(jperr(
                    "2203A",
                    "jsonpath member accessor can only be applied to an object",
                ))
            }
        }
        _ => structural::<()>(
            ctx,
            "2203A",
            "jsonpath member accessor can only be applied to an object",
        )
        .map(|_| ()),
    }
}

fn anykey_access(ctx: &JpCtx, item: &JsonbVal, out: &mut Vec<JsonbVal>) -> Result<(), ExecError> {
    match item {
        JsonbVal::Object(pairs) => {
            out.extend(pairs.iter().map(|(_, v)| v.clone()));
            Ok(())
        }
        JsonbVal::Array(elems) => {
            if ctx.lax {
                for el in elems {
                    anykey_access(ctx, el, out)?;
                }
                Ok(())
            } else {
                Err(jperr(
                    "2203C",
                    "jsonpath wildcard member accessor can only be applied to an object",
                ))
            }
        }
        _ => structural::<()>(
            ctx,
            "2203C",
            "jsonpath wildcard member accessor can only be applied to an object",
        )
        .map(|_| ()),
    }
}

/// Recursive `**` collection: pre-order DFS (the item itself at depth
/// 0, then children), keeping items whose depth is in [first, last]
/// (last < 0 = unbounded). Children of arrays are elements, of objects
/// are values (PG's JsonbIterator order).
fn any_collect(v: &JsonbVal, depth: i64, first: i64, last: i64, out: &mut Vec<JsonbVal>) {
    if depth >= first && (last < 0 || depth <= last) {
        out.push(v.clone());
    }
    if last >= 0 && depth >= last {
        return;
    }
    match v {
        JsonbVal::Array(elems) => {
            for el in elems {
                any_collect(el, depth + 1, first, last, out);
            }
        }
        JsonbVal::Object(pairs) => {
            for (_, val) in pairs {
                any_collect(val, depth + 1, first, last, out);
            }
        }
        _ => {}
    }
}

/// One operand of a jsonpath arithmetic operator must be exactly one
/// numeric item (jsonpath_exec.c executeArithmetic).
fn single_numeric(seq: &[JsonbVal], side: &str, op: JpArith) -> Result<Numeric, ExecError> {
    match seq {
        [JsonbVal::Number(n)] => Ok(n.clone()),
        _ => Err(jperr(
            "22038",
            format!(
                "{} operand of jsonpath operator {} is not a single numeric value",
                side,
                op.sql()
            ),
        )),
    }
}

fn single_numeric_unary(seq: &[JsonbVal], neg: bool) -> Result<Numeric, ExecError> {
    match seq {
        [JsonbVal::Number(n)] => Ok(n.clone()),
        _ => Err(jperr(
            "22038",
            format!(
                "operand of jsonpath operator {} is not a single numeric value",
                if neg { "-" } else { "+" }
            ),
        )),
    }
}

fn arith_numeric(op: JpArith, l: &Numeric, r: &Numeric) -> Result<Numeric, ExecError> {
    let overflow = || jperr("22003", "value overflows numeric format");
    match op {
        JpArith::Add => l.checked_add(r).ok_or_else(overflow),
        JpArith::Sub => l.checked_sub(r).ok_or_else(overflow),
        JpArith::Mul => l.checked_mul(r).ok_or_else(overflow),
        JpArith::Div => {
            if r.is_zero() {
                return Err(jperr("22012", "division by zero"));
            }
            // PG numeric_div with its selected rscale (jsonpath items
            // render at the quotient's natural scale).
            l.div_at_scale(r, 16.max(l.dscale + r.dscale + 4))
                .ok_or_else(overflow)
        }
        JpArith::Mod => {
            if r.is_zero() {
                return Err(jperr("22012", "division by zero"));
            }
            l.checked_rem(r).ok_or_else(overflow)
        }
    }
}

/// Array subscript access (jpiSubscript): index expressions evaluate
/// with @ = the array and `last` = its final index. In strict mode the
/// base must be an array; in lax mode non-array bases unwrap one
/// level (an array's elements are each subscripted).
fn eval_subscript(
    base: &JpExpr,
    idxs: &[(JpExpr, Option<JpExpr>)],
    ctx: &JpCtx,
    input: &[JsonbVal],
    last: Option<i64>,
) -> EResult {
    let seq = eval(base, ctx, input, last)?;
    let mut out = Vec::new();
    for item in &seq {
        subscript_item(ctx, item, idxs, &mut out)?;
    }
    Ok(out)
}

fn subscript_item(
    ctx: &JpCtx,
    item: &JsonbVal,
    idxs: &[(JpExpr, Option<JpExpr>)],
    out: &mut Vec<JsonbVal>,
) -> Result<(), ExecError> {
    match item {
        JsonbVal::Array(elems) => {
            let arr_last = elems.len() as i64 - 1;
            for (from_e, to_e) in idxs {
                let froms = eval_index_expr(ctx, from_e, item, arr_last)?;
                let tos = match to_e {
                    Some(t) => eval_index_expr(ctx, t, item, arr_last)?,
                    None => Vec::new(),
                };
                for f in &froms {
                    let fi = numeric_to_index(ctx, f)?;
                    if let Some(fi) = fi {
                        if to_e.is_some() {
                            for t in &tos {
                                if let Some(ti) = numeric_to_index(ctx, t)? {
                                    let mut i = fi;
                                    while i <= ti {
                                        push_index(ctx, elems, i, out)?;
                                        i += 1;
                                    }
                                }
                            }
                        } else {
                            push_index(ctx, elems, fi, out)?;
                        }
                    }
                }
            }
            Ok(())
        }
        JsonbVal::Object(_)
        | JsonbVal::Str(_)
        | JsonbVal::Number(_)
        | JsonbVal::Bool(_)
        | JsonbVal::Null => {
            if ctx.lax {
                // lax: wrap the scalar and subscript that (PG
                // jspAutoWrap: a non-array is treated as [item]).
                subscript_item(ctx, &JsonbVal::Array(vec![item.clone()]), idxs, out)
            } else {
                Err(jperr(
                    "22039",
                    "jsonpath array accessor can only be applied to an array",
                ))
            }
        }
    }
}

fn eval_index_expr(
    ctx: &JpCtx,
    e: &JpExpr,
    array_item: &JsonbVal,
    arr_last: i64,
) -> Result<Vec<Numeric>, ExecError> {
    let vals = eval(e, ctx, std::slice::from_ref(array_item), Some(arr_last))?;
    for v in &vals {
        if !matches!(v, JsonbVal::Number(_)) {
            return Err(jperr(
                "22038",
                "jsonpath array subscript is not a single numeric value",
            ));
        }
    }
    match vals.as_slice() {
        [JsonbVal::Number(n)] => Ok(vec![n.clone()]),
        _ => Err(jperr(
            "22038",
            "jsonpath array subscript is not a single numeric value",
        )),
    }
}

/// Convert a numeric subscript value to an index (PG rounds numeric
/// to int4 via rint). Out-of-integer-range is a hard error in both
/// modes.
fn numeric_to_index(ctx: &JpCtx, n: &Numeric) -> Result<Option<i64>, ExecError> {
    let r = n.round_to(0).unwrap_or_else(|| n.clone());
    let f = r.to_f64();
    if !f.is_finite() || f.abs() > 2_147_483_647.0 {
        return Err(jperr(
            "22003",
            "jsonpath array subscript is out of integer range",
        ));
    }
    let _ = ctx;
    Ok(Some(f as i64))
}

fn push_index(
    ctx: &JpCtx,
    elems: &[JsonbVal],
    i: i64,
    out: &mut Vec<JsonbVal>,
) -> Result<(), ExecError> {
    if i < 0 || i as usize >= elems.len() {
        return structural::<()>(ctx, "22033", "jsonpath array subscript is out of bounds")
            .map(|_| ());
    }
    out.push(elems[i as usize].clone());
    Ok(())
}

// ---------------------------------------------------------------------------
// Predicates
// ---------------------------------------------------------------------------

/// Evaluate a predicate node against one context item.
/// `suppress` is true inside filters and exists(): errors raised by
/// operand evaluation become Unknown (jsonpath_exec.c's predicate
/// error-suppression, PG commit-era semantic confirmed by the
/// '$[*]? (2 / @ > 0)' corpus case).
fn eval_pred(
    e: &JpExpr,
    ctx: &JpCtx,
    input: &[JsonbVal],
    last: Option<i64>,
    suppress: bool,
) -> Result<Tri, ExecError> {
    match e {
        JpExpr::And(l, r) => {
            let a = eval_pred(l, ctx, input, last, suppress)?;
            if a == Tri::False {
                return Ok(Tri::False);
            }
            let b = eval_pred(r, ctx, input, last, suppress)?;
            Ok(a.and(b))
        }
        JpExpr::Or(l, r) => {
            let a = eval_pred(l, ctx, input, last, suppress)?;
            if a == Tri::True {
                return Ok(Tri::True);
            }
            let b = eval_pred(r, ctx, input, last, suppress)?;
            Ok(a.or(b))
        }
        JpExpr::Not(inner) => Ok(eval_pred(inner, ctx, input, last, suppress)?.not()),
        JpExpr::IsUnknown(inner) => match eval_pred(inner, ctx, input, last, true) {
            Ok(Tri::Unknown) => Ok(Tri::True),
            Ok(_) => Ok(Tri::False),
            Err(_) => Ok(Tri::True),
        },
        JpExpr::Exists(inner) => {
            // exists(): evaluation failures mean "no items" — except
            // undefined variables, which PG raises with ereport.
            match eval(inner, ctx, input, last) {
                Ok(seq) => Ok(if seq.is_empty() {
                    Tri::False
                } else {
                    Tri::True
                }),
                Err(e) if e.code == "42704" => Err(e),
                Err(_) => Ok(Tri::False),
            }
        }
        JpExpr::Cmp(op, l, r) => {
            let lv = operand_seq(l, ctx, input, last, suppress)?;
            let rv = operand_seq(r, ctx, input, last, suppress)?;
            match (lv, rv) {
                (Some(lv), Some(rv)) => Ok(compare_seqs(*op, &lv, &rv)),
                _ => Ok(Tri::Unknown),
            }
        }
        JpExpr::StartsWith(l, r) => {
            let lv = operand_seq(l, ctx, input, last, suppress)?;
            let rv = operand_seq(r, ctx, input, last, suppress)?;
            match (lv, rv) {
                (Some(lv), Some(rv)) => {
                    let mut unknown = false;
                    for a in &lv {
                        for b in &rv {
                            match (a, b) {
                                (JsonbVal::Str(s), JsonbVal::Str(p)) => {
                                    if s.starts_with(p.as_str()) {
                                        return Ok(Tri::True);
                                    }
                                }
                                _ => unknown = true,
                            }
                        }
                    }
                    Ok(if unknown { Tri::Unknown } else { Tri::False })
                }
                _ => Ok(Tri::Unknown),
            }
        }
        JpExpr::LikeRegex(l, pat, flags) => {
            let lv = operand_seq(l, ctx, input, last, suppress)?;
            match lv {
                Some(lv) => {
                    let re = match like_regex_compile(pat, flags.as_deref()) {
                        Ok(r) => r,
                        Err(e) => {
                            return if suppress { Ok(Tri::Unknown) } else { Err(e) };
                        }
                    };
                    let mut unknown = false;
                    for a in &lv {
                        match a {
                            JsonbVal::Str(s) => {
                                let chars: Vec<char> = s.chars().collect();
                                if re.is_match(&chars) {
                                    return Ok(Tri::True);
                                }
                            }
                            _ => unknown = true,
                        }
                    }
                    Ok(if unknown { Tri::Unknown } else { Tri::False })
                }
                None => Ok(Tri::Unknown),
            }
        }
        // A non-predicate expression in predicate position: true iff
        // it yields a boolean true item (PG: the item sequence is
        // interpreted loosely — true value => true, anything else or
        // empty => unknown/false per executePredicate's default).
        other => match eval(other, ctx, input, last) {
            Ok(seq) => {
                let mut saw = false;
                for v in &seq {
                    saw = true;
                    if let JsonbVal::Bool(b) = v {
                        if *b {
                            return Ok(Tri::True);
                        }
                    }
                }
                Ok(if saw { Tri::False } else { Tri::Unknown })
            }
            Err(e) => {
                if suppress {
                    Ok(Tri::Unknown)
                } else {
                    Err(e)
                }
            }
        },
    }
}

/// Evaluate an operand of a comparison; in suppress mode an error
/// becomes None (= Unknown at the comparison level).
fn operand_seq(
    e: &JpExpr,
    ctx: &JpCtx,
    input: &[JsonbVal],
    last: Option<i64>,
    suppress: bool,
) -> Result<Option<Vec<JsonbVal>>, ExecError> {
    match eval(e, ctx, input, last) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            if suppress && e.code != "42704" {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// Pairwise comparison of two item sequences (executeComparison):
/// any true pair wins; else any incomparable pair makes it unknown.
fn compare_seqs(op: JpCmp, lv: &[JsonbVal], rv: &[JsonbVal]) -> Tri {
    if lv.is_empty() || rv.is_empty() {
        return Tri::Unknown;
    }
    let mut unknown = false;
    for a in lv {
        for b in rv {
            match compare_pair(op, a, b) {
                Some(true) => return Tri::True,
                Some(false) => {}
                None => unknown = true,
            }
        }
    }
    if unknown { Tri::Unknown } else { Tri::False }
}

/// Compare one pair of items; None = incomparable (different types,
/// or unordered kinds like arrays/objects — except ==/!= on
/// containers, which PG supports via jsonb equality).
fn compare_pair(op: JpCmp, a: &JsonbVal, b: &JsonbVal) -> Option<bool> {
    let ord: Option<Ordering> = match (a, b) {
        (JsonbVal::Null, JsonbVal::Null) => Some(Ordering::Equal),
        (JsonbVal::Bool(x), JsonbVal::Bool(y)) => Some(x.cmp(y)),
        (JsonbVal::Number(x), JsonbVal::Number(y)) => Some(x.cmp(y)),
        (JsonbVal::Str(x), JsonbVal::Str(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (JsonbVal::Array(_), JsonbVal::Array(_))
        | (JsonbVal::Object(_), JsonbVal::Object(_))
        | (JsonbVal::Array(_), JsonbVal::Object(_))
        | (JsonbVal::Object(_), JsonbVal::Array(_)) => match op {
            JpCmp::Eq => return Some(jsonb_cmp(a, b) == Ordering::Equal),
            JpCmp::Ne => return Some(jsonb_cmp(a, b) != Ordering::Equal),
            _ => return None,
        },
        _ => return None,
    };
    let ord = ord?;
    Some(match op {
        JpCmp::Eq => ord == Ordering::Equal,
        JpCmp::Ne => ord != Ordering::Equal,
        JpCmp::Lt => ord == Ordering::Less,
        JpCmp::Le => ord != Ordering::Greater,
        JpCmp::Gt => ord == Ordering::Greater,
        JpCmp::Ge => ord != Ordering::Less,
    })
}

/// Compile a jsonpath like_regex pattern (XQuery regex flavor) onto
/// the crate regex engine. Flags: i (case-insensitive), m (^/$
/// multiline), s (. matches newline), x (extended / whitespace-
/// ignoring), q (quote all chars literally).
fn like_regex_compile(
    pat: &str,
    flags: Option<&str>,
) -> Result<std::rc::Rc<crate::regex::Compiled>, ExecError> {
    let flags = flags.unwrap_or("");
    let pattern: String = if flags.contains('q') {
        // q: every character is literal.
        pat.chars()
            .flat_map(|c| match c {
                '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^'
                | '$' | '#' | '&' | '-' | '~' => vec!['\\', c],
                c => vec![c],
            })
            .collect()
    } else {
        pat.to_string()
    };
    let opts = crate::regex::RegexOptions {
        case_insensitive: flags.contains('i'),
        // XQuery m: ^/$ match at newlines (and '.' stops at \n,
        // which the engine bundles); s: '.' also matches \n.
        newline_sensitive: flags.contains('m') && !flags.contains('s'),
        expanded: flags.contains('x') && !flags.contains('q'),
    };
    crate::regex::compile_opts(&pattern, opts)
        .map_err(|e| jperr("2201B", format!("invalid regular expression: {}", e)))
}

// ---------------------------------------------------------------------------
// Item methods
// ---------------------------------------------------------------------------

fn method_type_err(name: &str, what: &str) -> ExecError {
    jperr(
        "22036",
        format!(
            "jsonpath item method .{}() can only be applied to {}",
            name, what
        ),
    )
}

fn apply_method(
    ctx: &JpCtx,
    item: &JsonbVal,
    m: &JpMethod,
    out: &mut Vec<JsonbVal>,
) -> Result<(), ExecError> {
    match m {
        JpMethod::Type => {
            let t = match item {
                JsonbVal::Null => "null",
                JsonbVal::Bool(_) => "boolean",
                JsonbVal::Number(_) => "number",
                JsonbVal::Str(_) => "string",
                JsonbVal::Array(_) => "array",
                JsonbVal::Object(_) => "object",
            };
            out.push(JsonbVal::Str(t.to_string()));
        }
        JpMethod::Size => match item {
            JsonbVal::Array(e) => {
                out.push(JsonbVal::Number(Numeric::from_i64(e.len() as i64)));
            }
            JsonbVal::Object(p) => {
                out.push(JsonbVal::Number(Numeric::from_i64(p.len() as i64)));
            }
            _ => {
                if ctx.lax {
                    out.push(JsonbVal::Number(Numeric::from_i64(1)));
                } else {
                    return Err(jperr(
                        "22039",
                        "jsonpath item method .size() can only be applied to an array",
                    ));
                }
            }
        },
        JpMethod::Abs | JpMethod::Floor | JpMethod::Ceiling => match item {
            JsonbVal::Number(n) => {
                let v = match m {
                    JpMethod::Abs => n.abs(),
                    JpMethod::Floor => n.floor().unwrap_or_else(|| n.clone()),
                    _ => n.ceil().unwrap_or_else(|| n.clone()),
                };
                out.push(JsonbVal::Number(v));
            }
            _ => return Err(method_type_err(m.name(), "a numeric value")),
        },
        JpMethod::Double => {
            let f = conversion_number(item, m)?;
            if !f.is_finite() {
                return Err(jperr(
                    "22036",
                    format!(
                        "argument \"{}\" of jsonpath item method .double() is invalid for type double precision",
                        item_scalar_text(item)
                    ),
                ));
            }
            out.push(JsonbVal::Number(
                Numeric::from_f64(f)
                    .map_err(|_| jperr("22003", "value overflows numeric format"))?,
            ));
        }
        JpMethod::Number => {
            let n = conversion_numeric(item, m)?;
            out.push(JsonbVal::Number(n));
        }
        JpMethod::Bigint | JpMethod::Integer => {
            if let JsonbVal::Str(txt) = item {
                if let Ok(n) = Numeric::parse(txt.trim()) {
                    if n.round_to(0).map(|r| r != n).unwrap_or(false) {
                        return Err(jperr(
                            "22036",
                            format!(
                                "argument \"{}\" of jsonpath item method .{}() is invalid for type {}",
                                txt,
                                m.name(),
                                conversion_type_name(m)
                            ),
                        ));
                    }
                }
            }
            let n = conversion_numeric(item, m)?;
            let r = n.round_to(0).unwrap_or_else(|| n.clone());
            let (lo, hi, tname) = if matches!(m, JpMethod::Bigint) {
                (i64::MIN as f64, i64::MAX as f64, "bigint")
            } else {
                (i32::MIN as f64, i32::MAX as f64, "integer")
            };
            let f = r.to_f64();
            if !f.is_finite() || f < lo || f > hi {
                return Err(jperr(
                    "22003",
                    format!(
                        "value \"{}\" is out of range for type {}",
                        r.to_text(),
                        tname
                    ),
                ));
            }
            out.push(JsonbVal::Number(r));
        }
        JpMethod::Decimal(p, s) => {
            if let Some(pv) = p {
                if pv
                    .checked_abs()
                    .map(|a| a > i32::MAX as i64)
                    .unwrap_or(true)
                {
                    return Err(jperr(
                        "22036",
                        "precision of jsonpath item method .decimal() is out of range for type integer",
                    ));
                }
            }
            if let Some(sv) = s {
                if sv
                    .checked_abs()
                    .map(|a| a > i32::MAX as i64)
                    .unwrap_or(true)
                {
                    return Err(jperr(
                        "22036",
                        "scale of jsonpath item method .decimal() is out of range for type integer",
                    ));
                }
            }
            if let Some(pv) = p {
                if !(1..=1000).contains(pv) {
                    return Err(jperr(
                        "22023",
                        format!("NUMERIC precision {} must be between 1 and 1000", pv),
                    ));
                }
            }
            let scale = s.unwrap_or(0);
            if !(-1000..=1000).contains(&scale) {
                return Err(jperr(
                    "22023",
                    format!("NUMERIC scale {} must be between -1000 and 1000", scale),
                ));
            }
            let n = conversion_numeric(item, m)?;
            let n = n.round_to(scale as i32).unwrap_or_else(|| n.clone());
            if let Some(p) = p {
                // precision: total significant digits must fit.
                let txt = n.to_text();
                let int_digits = txt
                    .trim_start_matches('-')
                    .split('.')
                    .next()
                    .map(|s| s.trim_start_matches('0').len().max(1))
                    .unwrap_or(1) as i64;
                if int_digits + scale.max(0) as i64 > *p as i64 {
                    return Err(jperr(
                        "22003",
                        format!(
                            "argument \"{}\" of jsonpath item method .decimal() is invalid for type numeric({}, {})",
                            item_scalar_text(item),
                            p,
                            scale
                        ),
                    ));
                }
            }
            out.push(JsonbVal::Number(n));
        }
        JpMethod::Boolean => match item {
            JsonbVal::Bool(b) => out.push(JsonbVal::Bool(*b)),
            JsonbVal::Number(n) => {
                if n.is_zero() {
                    out.push(JsonbVal::Bool(false));
                } else if *n == Numeric::from_i64(1) {
                    out.push(JsonbVal::Bool(true));
                } else {
                    return Err(bool_arg_err(item));
                }
            }
            JsonbVal::Str(s) => match parse_pg_bool(s) {
                Some(b) => out.push(JsonbVal::Bool(b)),
                None => return Err(bool_arg_err(item)),
            },
            _ => {
                return Err(jperr(
                    "2203B",
                    "jsonpath item method .boolean() can only be applied to a boolean, string, or numeric value",
                ));
            }
        },
        JpMethod::StringM => match item {
            JsonbVal::Str(s) => out.push(JsonbVal::Str(s.clone())),
            JsonbVal::Bool(b) => {
                out.push(JsonbVal::Str(if *b { "true" } else { "false" }.to_string()))
            }
            JsonbVal::Number(n) => out.push(JsonbVal::Str(n.to_text())),
            _ => {
                return Err(jperr(
                    "2203F",
                    "jsonpath item method .string() can only be applied to a boolean, string, numeric, or datetime value",
                ));
            }
        },
        JpMethod::KeyValue => match item {
            JsonbVal::Object(pairs) => {
                // PG's `id` is the object's offset in the binary
                // jsonb datum; we approximate root containers with 0
                // and nested ones with a simulated layout offset —
                // computed by the caller when available; here the
                // item itself is treated as its own root (id 0),
                // which matches PG for top-level objects.
                for (k, v) in pairs {
                    out.push(JsonbVal::Object(vec![
                        ("id".to_string(), JsonbVal::Number(Numeric::from_i64(0))),
                        ("key".to_string(), JsonbVal::Str(k.clone())),
                        ("value".to_string(), v.clone()),
                    ]));
                }
            }
            _ => {
                return Err(jperr(
                    "2203C",
                    "jsonpath item method .keyvalue() can only be applied to an object",
                ));
            }
        },
        JpMethod::Lower | JpMethod::Upper | JpMethod::Initcap => match item {
            JsonbVal::Str(s) => {
                let v = match m {
                    JpMethod::Lower => s.to_lowercase(),
                    JpMethod::Upper => s.to_uppercase(),
                    _ => initcap_str(s),
                };
                out.push(JsonbVal::Str(v));
            }
            _ => {
                return Err(jperr(
                    "2203F",
                    format!(
                        "jsonpath item method .{}() can only be applied to a string",
                        m.name()
                    ),
                ));
            }
        },
        JpMethod::Ltrim(chars) | JpMethod::Rtrim(chars) | JpMethod::Btrim(chars) => match item {
            JsonbVal::Str(s) => {
                let set: Vec<char> = chars.as_deref().unwrap_or(" ").chars().collect();
                let v: String = match m {
                    JpMethod::Ltrim(_) => s.trim_start_matches(&set[..]).to_string(),
                    JpMethod::Rtrim(_) => s.trim_end_matches(&set[..]).to_string(),
                    _ => s
                        .trim_start_matches(&set[..])
                        .trim_end_matches(&set[..])
                        .to_string(),
                };
                out.push(JsonbVal::Str(v));
            }
            _ => {
                return Err(jperr(
                    "2203F",
                    format!(
                        "jsonpath item method .{}() can only be applied to a string",
                        m.name()
                    ),
                ));
            }
        },
        JpMethod::Replace(a, b) => match item {
            JsonbVal::Str(s) => {
                out.push(JsonbVal::Str(s.replace(a.as_str(), b.as_str())));
            }
            _ => {
                return Err(jperr(
                    "2203F",
                    "jsonpath item method .replace() can only be applied to a string",
                ));
            }
        },
        JpMethod::SplitPart(sep, n) => match item {
            JsonbVal::Str(s) => {
                if n.checked_abs().map(|a| a > i32::MAX as i64).unwrap_or(true) {
                    return Err(jperr(
                        "22023",
                        "field position of jsonpath item method .split_part() is out of range for type integer",
                    ));
                }
                if *n == 0 {
                    return Err(jperr(
                        "22023",
                        "field position of jsonpath item method .split_part() must not be zero",
                    ));
                }
                let parts: Vec<&str> = if sep.is_empty() {
                    vec![s.as_str()]
                } else {
                    s.split(sep.as_str()).collect()
                };
                let idx = if *n > 0 {
                    (*n - 1) as usize
                } else {
                    (parts.len() as i64 + *n) as usize
                };
                let v = parts.get(idx).copied().unwrap_or("");
                out.push(JsonbVal::Str(v.to_string()));
            }
            _ => {
                return Err(jperr(
                    "2203F",
                    "jsonpath item method .split_part() can only be applied to a string",
                ));
            }
        },
        JpMethod::Date
        | JpMethod::Time(_)
        | JpMethod::TimeTz(_)
        | JpMethod::Timestamp(_)
        | JpMethod::TimestampTz(_)
        | JpMethod::Datetime(_) => {
            let txt = datetime_string_arg(item, m)?;
            out.push(JsonbVal::Str(eval_datetime_method(m, &txt)?));
        }
    }
    Ok(())
}

fn item_scalar_text(item: &JsonbVal) -> String {
    match item {
        JsonbVal::Str(s) => s.clone(),
        JsonbVal::Number(n) => n.to_text(),
        JsonbVal::Bool(b) => b.to_string(),
        other => {
            let mut s = String::new();
            jsonb_render(other, &mut s);
            s
        }
    }
}

fn bool_arg_err(item: &JsonbVal) -> ExecError {
    jperr(
        "22036",
        format!(
            "argument \"{}\" of jsonpath item method .boolean() is invalid for type boolean",
            item_scalar_text(item)
        ),
    )
}

/// PG's .number()/.bigint()/... string conversion: the string must
/// parse as a number; a numeric item passes through. Errors use the
/// NON_NUMERIC_SQL_JSON_ITEM template from jsonpath_exec.c.
fn conversion_numeric(item: &JsonbVal, m: &JpMethod) -> Result<Numeric, ExecError> {
    match item {
        JsonbVal::Number(n) => Ok(n.clone()),
        JsonbVal::Str(s) => {
            let parsed = Numeric::parse(s.trim());
            match parsed {
                Ok(n) => {
                    if !n.to_f64().is_finite() {
                        return Err(jperr(
                            "22036",
                            format!(
                                "NaN or Infinity is not allowed for jsonpath item method.{}()",
                                m.name()
                            ),
                        ));
                    }
                    Ok(n)
                }
                Err(_) => Err(jperr(
                    "22036",
                    format!(
                        "argument \"{}\" of jsonpath item method .{}() is invalid for type {}",
                        s,
                        m.name(),
                        conversion_type_name(m)
                    ),
                )),
            }
        }
        _ => Err(jperr(
            "2203B",
            format!(
                "jsonpath item method .{}() can only be applied to a string or numeric value",
                m.name()
            ),
        )),
    }
}

fn conversion_number(item: &JsonbVal, m: &JpMethod) -> Result<f64, ExecError> {
    Ok(conversion_numeric(item, m)?.to_f64())
}

fn conversion_type_name(m: &JpMethod) -> &'static str {
    match m {
        JpMethod::Bigint => "bigint",
        JpMethod::Integer => "integer",
        JpMethod::Double => "double precision",
        JpMethod::Decimal(..) => "numeric",
        _ => "numeric",
    }
}

fn datetime_string_arg(item: &JsonbVal, m: &JpMethod) -> Result<String, ExecError> {
    match item {
        JsonbVal::Str(s) => Ok(s.clone()),
        _ => Err(jperr(
            "2203F",
            format!(
                "jsonpath item method .{}() can only be applied to a string",
                m.name()
            ),
        )),
    }
}

fn format_time_of_day(tod_micros: i64) -> String {
    let secs = tod_micros / 1_000_000;
    let micros = tod_micros % 1_000_000;
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if micros == 0 {
        format!("{:02}:{:02}:{:02}", h, mi, s)
    } else {
        let frac = format!("{:06}", micros);
        format!("{:02}:{:02}:{:02}.{}", h, mi, s, frac.trim_end_matches('0'))
    }
}

/// PG's initcap: uppercase the first char of each word, lowercase
/// the rest (word = run of alphanumeric chars).
fn initcap_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if at_word_start {
                out.extend(c.to_uppercase());
                at_word_start = false;
            } else {
                out.extend(c.to_lowercase());
            }
        } else {
            out.push(c);
            at_word_start = true;
        }
    }
    out
}

/// PG boolean input (boolin): the accepted spellings.
fn parse_pg_bool(s: &str) -> Option<bool> {
    let t = s.trim().to_ascii_lowercase();
    match t.as_str() {
        "t" | "true" | "y" | "yes" | "on" | "1" => Some(true),
        "f" | "false" | "n" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public entry points (jsonfuncs.c wrappers)
// ---------------------------------------------------------------------------

/// Build an execution context, validating the vars argument the way
/// PG's jsonb_path_* wrappers do.
pub(crate) fn jp_context<'a>(
    root: &'a JsonbVal,
    path: &JpPath,
    vars: Option<&'a JsonbVal>,
) -> Result<JpCtx<'a>, ExecError> {
    if let Some(v) = vars {
        if !matches!(v, JsonbVal::Object(_)) {
            return Err(jperr("22023", "\"vars\" argument is not an object"));
        }
    }
    Ok(JpCtx {
        root,
        vars,
        lax: path.lax,
    })
}

/// jsonb_path_query: the full result sequence.
pub(crate) fn jp_query_seq(
    root: &JsonbVal,
    path: &JpPath,
    vars: Option<&JsonbVal>,
) -> Result<Vec<JsonbVal>, ExecError> {
    let ctx = jp_context(root, path, vars)?;
    if path.expr.is_predicate() {
        let mut out = Vec::new();
        for item in std::slice::from_ref(root) {
            match eval_pred(&path.expr, &ctx, std::slice::from_ref(item), None, false)? {
                Tri::True => out.push(JsonbVal::Bool(true)),
                Tri::False => out.push(JsonbVal::Bool(false)),
                Tri::Unknown => {}
            }
        }
        Ok(out)
    } else {
        eval(&path.expr, &ctx, std::slice::from_ref(root), None)
    }
}

/// jsonb_path_exists: predicate paths test the predicate; other
/// paths test non-emptiness.
pub(crate) fn jp_exists(
    root: &JsonbVal,
    path: &JpPath,
    vars: Option<&JsonbVal>,
) -> Result<bool, ExecError> {
    let ctx = jp_context(root, path, vars)?;
    if path.expr.is_predicate() {
        Ok(eval_pred(&path.expr, &ctx, std::slice::from_ref(root), None, false)? == Tri::True)
    } else {
        Ok(!eval(&path.expr, &ctx, std::slice::from_ref(root), None)?.is_empty())
    }
}

/// jsonb_path_match: the path must produce a single boolean item
/// (or a single null item, which maps to SQL NULL — returns None).
pub(crate) fn jp_match(
    root: &JsonbVal,
    path: &JpPath,
    vars: Option<&JsonbVal>,
) -> Result<Option<bool>, ExecError> {
    let ctx = jp_context(root, path, vars)?;
    let seq = if path.expr.is_predicate() {
        match eval_pred(&path.expr, &ctx, std::slice::from_ref(root), None, false)? {
            Tri::True => vec![JsonbVal::Bool(true)],
            Tri::False => vec![JsonbVal::Bool(false)],
            Tri::Unknown => Vec::new(),
        }
    } else {
        eval(&path.expr, &ctx, std::slice::from_ref(root), None)?
    };
    match seq.as_slice() {
        [JsonbVal::Bool(b)] => Ok(Some(*b)),
        [JsonbVal::Null] => Ok(None),
        _ => Err(jperr("22038", "single boolean result is expected")),
    }
}

// ---------------------------------------------------------------------------
// Datetime item methods (jsonpath_exec.c executeDatetimeMethod).
// Datetime results ride as JsonbVal::Str of PG's ISO rendering (the
// crate has no datetime JsonbVal variant); the input string is
// classified into PG's datetime kinds (date/time/timetz/timestamp/
// timestamptz) and PG's conversion matrix applied: same-family
// conversions succeed, cross time-zone conversions raise the
// "cannot convert ... without time zone usage" error, and inputs of
// the wrong shape raise "<kind> format is not recognized".
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DtKind {
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

impl DtKind {
    fn name(self) -> &'static str {
        match self {
            DtKind::Date => "date",
            DtKind::Time => "time",
            DtKind::TimeTz => "timetz",
            DtKind::Timestamp => "timestamp",
            DtKind::TimestampTz => "timestamptz",
        }
    }
}

/// Run a crate datetime parser without letting its panics (overflow
/// on absurd years in debug builds) take the server down.
fn dt_catch<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err("datetime value out of range".to_string()),
    }
}

/// Does the time portion of `s` carry a zone offset (+hh:mm / -hh:mm
/// / Z)? The date part's own '-' separators don't count: only the
/// part after the date/time separator is inspected.
fn dt_has_offset(s: &str) -> bool {
    let t = s.trim();
    let time_part = match t.find(['T', ' ']) {
        Some(i) => &t[i + 1..],
        None => t,
    };
    time_part.ends_with(['Z', 'z'])
        || time_part.contains('+')
        || time_part.matches('-').count() > 0
            && time_part.contains(':')
            && time_part
                .rfind('-')
                .map(|i| i > time_part.find(':').unwrap_or(0))
                .unwrap_or(false)
}

/// Classify a datetime string the way PG's datetime parser does.
fn dt_classify(s: &str) -> Option<DtKind> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if dt_catch(|| crate::datetime::parse_date(t)).is_ok() && !t.contains(':') {
        return Some(DtKind::Date);
    }
    let has_time_sep = t.contains(['T', ' ']) && t.contains(':');
    if has_time_sep {
        return Some(if dt_has_offset(t) {
            DtKind::TimestampTz
        } else {
            DtKind::Timestamp
        });
    }
    if t.contains(':') {
        return Some(if dt_has_offset(t) {
            DtKind::TimeTz
        } else {
            DtKind::Time
        });
    }
    None
}

/// Parse "HH:MM:SS[.ffffff]" into micros-of-day.
fn dt_parse_tod(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let mut parts = t.split(':');
    let h: i64 = parts
        .next()
        .ok_or("bad time")?
        .parse()
        .map_err(|_| "bad time")?;
    let mi: i64 = parts
        .next()
        .ok_or("bad time")?
        .parse()
        .map_err(|_| "bad time")?;
    let sec_part = parts.next().ok_or("bad time")?;
    let (sec, frac) = match sec_part.split_once('.') {
        Some((a, b)) => (a, b),
        None => (sec_part, ""),
    };
    let sec: i64 = sec.parse().map_err(|_| "bad time")?;
    let mut micros: i64 = 0;
    if !frac.is_empty() {
        let mut f = frac.to_string();
        while f.len() < 6 {
            f.push('0');
        }
        f.truncate(6);
        micros = f.parse().map_err(|_| "bad time")?;
    }
    if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..60).contains(&sec) {
        return Err("time out of range".to_string());
    }
    Ok(((h * 60 + mi) * 60 + sec) * 1_000_000 + micros)
}

/// Extract the zone offset (seconds) from a datetime string.
fn dt_offset_seconds(s: &str) -> i64 {
    let t = s.trim();
    if t.ends_with(['Z', 'z']) {
        return 0;
    }
    let time_part = match t.find(['T', ' ']) {
        Some(i) => &t[i + 1..],
        None => t,
    };
    for (i, c) in time_part.char_indices() {
        if (c == '+' || c == '-') && i > 0 {
            let off = &time_part[i..];
            let sign = if c == '-' { -1 } else { 1 };
            let body = &off[1..];
            let (hh, mm) = match body.split_once(':') {
                Some((a, b)) => (
                    a.parse::<i64>().unwrap_or(0),
                    b.trim_end_matches(|ch: char| ch.is_alphabetic())
                        .parse::<i64>()
                        .unwrap_or(0),
                ),
                None => (body.parse::<i64>().unwrap_or(0), 0),
            };
            return sign * (hh * 3600 + mm * 60);
        }
    }
    0
}

fn dt_format_offset(secs: i64) -> String {
    let sign = if secs < 0 { "-" } else { "+" };
    let a = secs.abs();
    let (h, m) = (a / 3600, (a % 3600) / 60);
    if m == 0 {
        format!("{}{:02}", sign, h)
    } else {
        format!("{}{:02}:{:02}", sign, h, m)
    }
}

/// Round micros-of-an-instant (or of a day) to `precision` fractional
/// digits, PG's AdjustForTypmod half-up rule.
fn dt_apply_precision(micros: i64, precision: Option<i64>) -> i64 {
    match precision {
        None => micros,
        Some(p) => {
            if p >= 6 {
                return micros;
            }
            let mult = 10i64.pow((6 - p.max(0)) as u32);
            ((micros + mult / 2) / mult) * mult
        }
    }
}

fn dt_not_recognized(kind_name: &str, input: &str) -> ExecError {
    jperr(
        "22031",
        format!("{} format is not recognized: \"{}\"", kind_name, input),
    )
}

fn dt_cannot_convert(from: DtKind, to: DtKind) -> ExecError {
    exec_err_detail(
        "22031",
        format!(
            "cannot convert value from {} to {} without time zone usage",
            from.name(),
            to.name()
        ),
        "Use *_tz() function for time zone support.",
    )
}

/// .timestamp()/.time()/... method precision must fit int4
/// (jsonpath_exec.c: 22036 "time precision ... is out of range").
fn check_method_precision(m: &JpMethod) -> Result<(), ExecError> {
    let (p, name) = match m {
        JpMethod::Time(p) => (p, "time"),
        JpMethod::TimeTz(p) => (p, "time_tz"),
        JpMethod::Timestamp(p) => (p, "timestamp"),
        JpMethod::TimestampTz(p) => (p, "timestamp_tz"),
        _ => return Ok(()),
    };
    if let Some(pv) = p {
        if pv
            .checked_abs()
            .map(|a| a > i32::MAX as i64)
            .unwrap_or(true)
        {
            return Err(jperr(
                "22036",
                format!(
                    "time precision of jsonpath item method .{}() is out of range for type integer",
                    name
                ),
            ));
        }
    }
    Ok(())
}

/// Evaluate one datetime item method on a string input; returns PG's
/// rendered result text.
fn eval_datetime_method(m: &JpMethod, input: &str) -> Result<String, ExecError> {
    let kind = dt_classify(input);
    match m {
        JpMethod::Date => match kind {
            Some(DtKind::Date) => {
                let d = dt_catch(|| crate::datetime::parse_date(input))
                    .map_err(|_| dt_not_recognized("date", input))?;
                Ok(crate::datetime::format_date(d))
            }
            Some(DtKind::Timestamp) => {
                let t = dt_catch(|| crate::datetime::parse_timestamp(input))
                    .map_err(|_| dt_not_recognized("date", input))?;
                Ok(crate::datetime::format_date(
                    (t.div_euclid(86_400_000_000)) as i32,
                ))
            }
            Some(DtKind::TimestampTz) => Err(dt_cannot_convert(DtKind::TimestampTz, DtKind::Date)),
            _ => Err(dt_not_recognized("date", input)),
        },
        JpMethod::Time(prec) => match kind {
            Some(DtKind::Time) => {
                let tod = dt_parse_tod(input).map_err(|_| dt_not_recognized("time", input))?;
                Ok(format_time_of_day(dt_apply_precision(tod, *prec)))
            }
            Some(DtKind::Timestamp) => {
                let t = dt_catch(|| crate::datetime::parse_timestamp(input))
                    .map_err(|_| dt_not_recognized("time", input))?;
                let tod = ((t % 86_400_000_000) + 86_400_000_000) % 86_400_000_000;
                Ok(format_time_of_day(dt_apply_precision(tod, *prec)))
            }
            Some(k @ (DtKind::TimeTz | DtKind::TimestampTz)) => {
                Err(dt_cannot_convert(k, DtKind::Time))
            }
            _ => Err(dt_not_recognized("time", input)),
        },
        JpMethod::TimeTz(prec) => match kind {
            Some(DtKind::TimeTz) => {
                let tod = dt_parse_tod(input).map_err(|_| dt_not_recognized("time_tz", input))?;
                Ok(format!(
                    "{}{}",
                    format_time_of_day(dt_apply_precision(tod, *prec)),
                    dt_format_offset(dt_offset_seconds(input))
                ))
            }
            Some(DtKind::TimestampTz) => {
                let t = dt_catch(|| crate::datetime::parse_timestamp(input))
                    .map_err(|_| dt_not_recognized("time_tz", input))?;
                let tod = ((t % 86_400_000_000) + 86_400_000_000) % 86_400_000_000;
                Ok(format!(
                    "{}{}",
                    format_time_of_day(dt_apply_precision(tod, *prec)),
                    dt_format_offset(dt_offset_seconds(input))
                ))
            }
            Some(DtKind::Time) => Err(dt_cannot_convert(DtKind::Time, DtKind::TimeTz)),
            Some(DtKind::Timestamp) => Err(dt_cannot_convert(DtKind::Timestamp, DtKind::TimeTz)),
            _ => Err(dt_not_recognized("time_tz", input)),
        },
        JpMethod::Timestamp(prec) => match kind {
            Some(DtKind::Timestamp) => {
                let t = dt_catch(|| crate::datetime::parse_timestamp(input))
                    .map_err(|_| dt_not_recognized("timestamp", input))?;
                Ok(crate::datetime::format_timestamp(dt_apply_precision(
                    t, *prec,
                )))
            }
            Some(DtKind::Date) => {
                let d = dt_catch(|| crate::datetime::parse_date(input))
                    .map_err(|_| dt_not_recognized("timestamp", input))?;
                Ok(crate::datetime::format_timestamp(dt_apply_precision(
                    // v1.86: checked (extreme dates overflow i64
                    // micros; saturate — format clamps like PG's
                    // range error path in strict mode is unreachable
                    // in lax mode anyway).
                    (d as i64).checked_mul(86_400_000_000).unwrap_or(if d < 0 {
                        i64::MIN
                    } else {
                        i64::MAX
                    }),
                    *prec,
                )))
            }
            Some(DtKind::TimestampTz) => {
                Err(dt_cannot_convert(DtKind::TimestampTz, DtKind::Timestamp))
            }
            _ => Err(dt_not_recognized("timestamp", input)),
        },
        JpMethod::TimestampTz(prec) => match kind {
            Some(DtKind::TimestampTz) => {
                let t = dt_catch(|| crate::datetime::parse_timestamp(input))
                    .map_err(|_| dt_not_recognized("timestamp_tz", input))?;
                Ok(crate::datetime::format_timestamptz(dt_apply_precision(
                    t, *prec,
                )))
            }
            Some(k @ (DtKind::Date | DtKind::Timestamp)) => {
                Err(dt_cannot_convert(k, DtKind::TimestampTz))
            }
            _ => Err(dt_not_recognized("timestamp_tz", input)),
        },
        JpMethod::Datetime(fmt) => match fmt {
            Some(f) => {
                // Template form: PG's parse_datetime runs the
                // template engine in std (strict) mode.
                let parsed = dt_catch(|| {
                    crate::datetime::parse_with_format_strict(input, f)
                        .map_err(|e| format!("{:?}", e))
                })
                .map_err(|_| {
                    jperr(
                        "22031",
                        format!("datetime format is not recognized: \"{}\"", f),
                    )
                })?;
                let days = crate::datetime::days_from_civil(
                    parsed.year.unwrap_or(2000),
                    parsed.month.unwrap_or(1),
                    parsed.day.unwrap_or(1),
                );
                let hour = match (parsed.hour24, parsed.hour12, parsed.pm) {
                    (Some(h), _, _) => h as i64,
                    (None, Some(h), Some(pm)) => (h % 12) as i64 + if pm { 12 } else { 0 },
                    (None, Some(h), None) => (h % 12) as i64,
                    _ => 0,
                };
                let tod = ((hour * 60 + parsed.minute.unwrap_or(0) as i64) * 60
                    + parsed.second.unwrap_or(0) as i64)
                    * 1_000_000
                    + parsed.micros.unwrap_or(0) as i64;
                Ok(crate::datetime::format_timestamp(
                    days * 86_400_000_000 + tod,
                ))
            }
            None => match kind {
                Some(DtKind::Date) => {
                    let d = dt_catch(|| crate::datetime::parse_date(input))
                        .map_err(|_| dt_not_recognized("datetime", input))?;
                    Ok(crate::datetime::format_date(d))
                }
                Some(DtKind::Timestamp) => {
                    let t = dt_catch(|| crate::datetime::parse_timestamp(input))
                        .map_err(|_| dt_not_recognized("datetime", input))?;
                    Ok(crate::datetime::format_timestamp(t))
                }
                Some(DtKind::TimestampTz) => {
                    let t = dt_catch(|| crate::datetime::parse_timestamp(input))
                        .map_err(|_| dt_not_recognized("datetime", input))?;
                    Ok(crate::datetime::format_timestamptz(t))
                }
                Some(DtKind::Time) => {
                    let tod =
                        dt_parse_tod(input).map_err(|_| dt_not_recognized("datetime", input))?;
                    Ok(format_time_of_day(tod))
                }
                Some(DtKind::TimeTz) => {
                    let tod =
                        dt_parse_tod(input).map_err(|_| dt_not_recognized("datetime", input))?;
                    Ok(format!(
                        "{}{}",
                        format_time_of_day(tod),
                        dt_format_offset(dt_offset_seconds(input))
                    ))
                }
                None => Err(dt_not_recognized("datetime", input)),
            },
        },
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(input: &str) -> String {
        let p = parse_jsonpath(input).expect("parse");
        render_jsonpath(&p)
    }

    fn query(doc: &str, path: &str) -> Vec<String> {
        let d = parse_jsonb(doc).expect("doc");
        let p = parse_jsonpath(path).expect("path");
        jp_query_seq(&d, &p, None)
            .expect("query")
            .iter()
            .map(|v| v.render())
            .collect()
    }

    #[test]
    fn render_canonical_forms() {
        assert_eq!(roundtrip("$.a"), r#"$."a""#);
        assert_eq!(roundtrip("strict $.a"), r#"strict $."a""#);
        assert_eq!(roundtrip("lax $.a"), r#"$."a""#);
        assert_eq!(roundtrip("$.*"), "$.*");
        assert_eq!(roundtrip("$.**"), "$.**");
        assert_eq!(roundtrip("$.**{2 to 3}"), "$.**{2 to 3}");
        assert_eq!(roundtrip("$[1, 2 to 3]"), "$[1,2 to 3]");
        assert_eq!(roundtrip("$ ? (@ == 1)"), "$?(@ == 1)");
        assert_eq!(roundtrip("$ + 1"), "($ + 1)");
        assert_eq!(roundtrip("$.a.b[0].type()"), r#"$."a"."b"[0].type()"#);
        assert_eq!(roundtrip("$var"), r#"$"var""#);
        assert_eq!(roundtrip("(1).type()"), "(1).type()");
        assert_eq!(
            roundtrip("$.a ? (@ like_regex \"x.*\" flag \"i\")"),
            r#"$."a"?(@ like_regex "x.*" flag "i")"#
        );
    }

    #[test]
    fn parse_rejections() {
        for bad in [
            "'00'",
            "00",
            "0755",
            "0x_42F",
            "@ + 1",
            "\"\\u0000\"",
            "\"\\ud83dX\"",
            "$ ? (@ like_regex \"p\" flag \"ss\")",
            "$.a ? (last == 1)",
        ] {
            assert!(parse_jsonpath(bad).is_err(), "expected error for {bad}");
        }
    }

    #[test]
    fn eval_basics() {
        assert_eq!(query(r#"{"a": [1, 2, 3]}"#, "$.a[*]"), ["1", "2", "3"]);
        assert_eq!(query(r#"{"a": 1}"#, "lax $.b"), Vec::<String>::new());
        // strict missing key is an error, per PG.
        let d = parse_jsonb(r#"{"a": 1}"#).unwrap();
        let p = parse_jsonpath("strict $.b").unwrap();
        assert!(jp_query_seq(&d, &p, None).is_err());
        assert!(
            parse_jsonpath("strict $[*]")
                .map(|p| jp_query_seq(&parse_jsonb("1").unwrap(), &p, None))
                .is_ok()
        );
        assert_eq!(query("[1, 2, 3]", "lax $[last]"), ["3"]);
        assert_eq!(query(r#"{"a": {"b": 2}}"#, "$.**.b"), ["2"]);
        assert_eq!(query("[1, 2, 3]", "$[*] ? (@ > 1)"), ["2", "3"]);
        assert_eq!(query(r#"{"a": 10}"#, "$.a + 5"), ["15"]);
        assert_eq!(query(r#"{"a": "Hello"}"#, "$.a.lower()"), [r#""hello""#]);
        assert_eq!(query("[3, 1]", "$[*].abs()"), ["3", "1"]);
    }

    #[test]
    fn eval_predicates_and_vars() {
        let d = parse_jsonb(r#"{"a": 10}"#).unwrap();
        let vars = parse_jsonb(r#"{"value": 5}"#).unwrap();
        let p = parse_jsonpath("$ ? (@.a > $value)").unwrap();
        assert!(jp_exists(&d, &p, Some(&vars)).unwrap());
        // Undefined variable throws even inside a filter (PG ereport).
        let p2 = parse_jsonpath("$ ? (@.a < $missing)").unwrap();
        assert!(jp_exists(&d, &p2, Some(&vars)).is_err());
        // Division by zero inside a filter becomes unknown, not error.
        assert_eq!(query("[1, 2, 0, 3]", "$[*] ? (2 / @ > 0)"), ["1", "2", "3"]);
    }

    #[test]
    fn eval_errors() {
        let d = parse_jsonb(r#"{"a": 1}"#).unwrap();
        let p = parse_jsonpath("strict $.b").unwrap();
        assert!(jp_query_seq(&d, &p, None).is_err());
        let p = parse_jsonpath("$.a ? (@.x == 1)").unwrap();
        // strict member error surfaces from jsonb_path_exists in
        // strict mode (the accessor is inside a filter's operand,
        // but '$.a' itself is fine; @.x on a number errors are
        // suppressed in the predicate — existence just fails).
        assert!(!jp_exists(&d, &p, None).unwrap());
    }
}
